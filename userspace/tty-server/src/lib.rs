//! `tty-server` — the terminals, separated from the syscalls: which exist, where each one's bytes
//! go, who is waiting, and the tokens one is handed on with. The binary half owns the console, the
//! IPC channels and the wait loop; what a byte does to a line is `libline`'s.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use libline::{Discipline, Key, Step};

/// Which terminals exist, where each one's bytes go, and who is waiting. No syscalls: every
/// effect comes back as an [`Act`]. Input goes to the first waiter **on its backend**, `Ctrl-C`
/// to every terminal on it.
pub mod routing {
    use super::{Discipline, Key, Step};
    use alloc::collections::VecDeque;
    use alloc::vec::Vec;

    // The real definitions, not copies: a copy of `PeerClosed` was wrong and no test could see it.
    use libkern::error::KError;
    use librsproto::{
        OP_TTY_INTERRUPT as OP_INTERRUPT, OP_TTY_READ as OP_READ, OP_TTY_READ_LINE as OP_READ_LINE,
        OP_TTY_SET_MODE as OP_SET_MODE, OP_TTY_WRITE as OP_WRITE,
    };

    /// Why a handle of `object_type` with `rights` cannot be a terminal's backend, if it cannot:
    /// the server sends on it, receives from it and waits on it beside every handle it serves,
    /// and learns what it is by a stat, which needs `INSPECT`.
    pub fn backend_refusal(object_type: u32, rights: u64) -> Option<KError> {
        use libkern::handle::{KOBJ_IPC_CHANNEL, RIGHT_RECV, RIGHT_SEND, RIGHT_WAIT};
        const NEEDED: u64 = RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT;
        if object_type != KOBJ_IPC_CHANNEL {
            Some(KError::InvalidArgument)
        } else if rights & NEEDED != NEEDED {
            Some(KError::NoAccess)
        } else {
            None
        }
    }

    /// Where a terminal's bytes come from and go to: a serial port, or a terminal emulator
    /// holding the master half of a pty. The discipline cannot tell which.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Sink {
        /// The serial console — the kernel's debug write out, the console device in.
        Console,
        /// A channel held by a terminal emulator.
        Channel(u64),
    }

    /// What an outstanding read is waiting for.
    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    pub enum ReadKind {
        /// `ReadLine`: completes when the discipline finishes a line.
        Line,
        /// `Read`: completes as soon as any byte is available, discipline not consulted.
        Raw,
    }

    /// One terminal: its channel, its editing state, its backend, and the request it owes.
    pub struct Tty {
        /// The server-side endpoint, opaque here.
        pub ch: u64,
        /// This terminal's line-editing state; the mode is per terminal.
        pub disc: Discipline,
        /// The outstanding read, if the client is waiting for input.
        pub waiting: Option<(u64, ReadKind)>,
        /// An interrupt arrived while **nobody was reading**, so the next read completes empty
        /// at once; otherwise `Ctrl-C` between prompts would wait for another keystroke.
        pub interrupt_pending: bool,
        /// Which backend this terminal belongs to.
        pub backend: u32,
    }

    /// A backend, and its input no terminal has taken yet. The queue is per backend: console
    /// bytes must not be deliverable to a terminal inside a window.
    pub struct Backend {
        /// Stable within a run, so retiring a backend renumbers no other.
        pub id: u32,
        /// Where this backend's bytes come from and go to.
        pub sink: Sink,
        pending: VecDeque<u8>,
    }

    /// An effect the caller must perform. Nothing in this module does I/O.
    #[derive(Clone, PartialEq, Eq, Debug)]
    pub enum Act {
        /// Write bytes to a sink — program output, or an echo.
        Write(Sink, Vec<u8>),
        /// Send on a terminal's channel; `request_id` 0 is unsolicited, as an interrupt is.
        Reply { ch: u64, op: u16, request_id: u64, body: Vec<u8> },
        /// Send an error reply.
        Fail { ch: u64, op: u16, request_id: u64, err: i32 },
    }

    /// The console backend's fixed id: a terminal opened with no backend named lands there.
    pub const CONSOLE: u32 = 0;

    /// Every terminal and every backend.
    pub struct Registry {
        backends: Vec<Backend>,
        ttys: Vec<Tty>,
        next_backend: u32,
    }

    impl Default for Registry {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Registry {
        /// A registry holding only the console backend.
        pub fn new() -> Registry {
            Registry {
                backends: alloc::vec![Backend { id: CONSOLE, sink: Sink::Console, pending: VecDeque::new() }],
                ttys: Vec::new(),
                next_backend: CONSOLE + 1,
            }
        }

        /// How many terminals exist.
        pub fn len(&self) -> usize {
            self.ttys.len()
        }

        /// Whether any terminal exists.
        pub fn is_empty(&self) -> bool {
            self.ttys.is_empty()
        }

        /// Every terminal's channel, for building a wait set.
        pub fn channels(&self) -> impl Iterator<Item = u64> + '_ {
            self.ttys.iter().map(|t| t.ch)
        }

        /// Every backend channel, for building a wait set. The console is not one.
        pub fn backend_channels(&self) -> impl Iterator<Item = u64> + '_ {
            self.backends.iter().filter_map(|b| match b.sink {
                Sink::Channel(h) => Some(h),
                Sink::Console => None,
            })
        }

        /// The backend whose sink is `ch`, if any.
        pub fn backend_of_channel(&self, ch: u64) -> Option<u32> {
            self.backends.iter().find(|b| b.sink == Sink::Channel(ch)).map(|b| b.id)
        }

        /// The backend terminal `ch` belongs to, if it exists — what `Tty::Token` names.
        pub fn backend_of_terminal(&self, ch: u64) -> Option<u32> {
            self.ttys.iter().find(|t| t.ch == ch).map(|t| t.backend)
        }

        /// Where terminal `ch` writes, if it exists.
        pub fn sink_of(&self, ch: u64) -> Option<Sink> {
            let t = self.ttys.iter().find(|t| t.ch == ch)?;
            self.backends.iter().find(|b| b.id == t.backend).map(|b| b.sink)
        }

        /// Add a terminal on the console backend.
        pub fn open(&mut self, ch: u64) {
            self.ttys.push(Tty {
                ch,
                disc: Discipline::new(),
                waiting: None,
                interrupt_pending: false,
                backend: CONSOLE,
            });
        }

        /// Add terminal `ch` on the same backend as terminal `of` — `Tty::OpenSibling`.
        ///
        /// Each keeps its own discipline, so a password prompt turning echo off leaves the
        /// shell's echo alone. Returns `false`, adding nothing, if `of` does not exist.
        pub fn open_sibling(&mut self, ch: u64, of: u64) -> bool {
            let Some(backend) = self.ttys.iter().find(|t| t.ch == of).map(|t| t.backend) else {
                return false;
            };
            self.ttys.push(Tty { ch, disc: Discipline::new(), waiting: None, interrupt_pending: false, backend });
            true
        }

        /// Drop terminal `ch`. Returns whether it existed.
        pub fn close(&mut self, ch: u64) -> bool {
            match self.ttys.iter().position(|t| t.ch == ch) {
                Some(i) => {
                    self.ttys.remove(i);
                    true
                }
                None => false,
            }
        }

        /// Drop terminal `ch` and return any backend channel that became unused.
        ///
        /// The caller closes it: that is what tells an emulator its terminal is gone.
        pub fn close_and_retire(&mut self, ch: u64) -> Vec<u64> {
            let Some(i) = self.ttys.iter().position(|t| t.ch == ch) else { return Vec::new() };
            let backend = self.ttys[i].backend;
            self.ttys.remove(i);
            match self.retire(backend) {
                Some(h) => alloc::vec![h],
                None => Vec::new(),
            }
        }

        /// Put terminal `ch` on the existing backend `id`; only tests call it.
        pub fn move_to(&mut self, ch: u64, id: u32) -> bool {
            if !self.backends.iter().any(|b| b.id == id) {
                return false;
            }
            match self.ttys.iter_mut().find(|t| t.ch == ch) {
                Some(t) => {
                    t.backend = id;
                    true
                }
                None => false,
            }
        }

        /// Every terminal on backend `id`, collected so the caller can close each.
        pub fn ttys_on(&self, id: u32) -> Vec<u64> {
            self.ttys.iter().filter(|t| t.backend == id).map(|t| t.ch).collect()
        }

        /// Feed `bytes` in from backend `id` and deliver what can be delivered.
        pub fn feed(&mut self, id: u32, bytes: &[u8]) -> Vec<Act> {
            if let Some(b) = self.backends.iter_mut().find(|b| b.id == id) {
                b.pending.extend(bytes.iter().copied());
            }
            self.drive(id)
        }

        /// Deliver backend `id`'s queue to whichever of *its* terminals is waiting; input with
        /// nobody waiting stays queued, as typing ahead of a prompt must.
        pub fn drive(&mut self, id: u32) -> Vec<Act> {
            let mut acts = Vec::new();
            let Some(bi) = self.backends.iter().position(|b| b.id == id) else {
                return acts;
            };
            let sink = self.backends[bi].sink;
            loop {
                if self.backends[bi].pending.is_empty() {
                    return acts;
                }
                // `Ctrl-C` is an event about the terminal, not input to it: it is taken first and
                // never queues, so it is answered while nobody is reading.
                if self.backends[bi].pending.front() == Some(&0x03) {
                    self.backends[bi].pending.pop_front();
                    self.interrupt(id, &mut acts);
                    continue;
                }
                let Some(i) = self.ttys.iter().position(|t| t.backend == id && t.waiting.is_some()) else {
                    return acts; // nobody on this backend is reading; hold the bytes
                };
                if let Some((rid, ReadKind::Raw)) = self.ttys[i].waiting {
                    // Stop at an interrupt, or one in the same chunk as earlier keystrokes would
                    // reach the client as an ordinary byte. Bytes before it are still input.
                    let q = &mut self.backends[bi].pending;
                    let stop = q.iter().position(|&b| b == 0x03).unwrap_or(q.len());
                    let n = stop.min(RAW_CHUNK);
                    if n == 0 {
                        continue; // the interrupt is at the front; the branch above handles it
                    }
                    let mut out = Vec::with_capacity(n);
                    for _ in 0..n {
                        out.push(q.pop_front().expect("non-empty"));
                    }
                    self.ttys[i].waiting = None;
                    acts.push(Act::Reply { ch: self.ttys[i].ch, op: OP_READ, request_id: rid, body: out });
                    continue;
                }
                let b = self.backends[bi].pending.pop_front().expect("non-empty");
                match self.ttys[i].disc.feed(b) {
                    Step::None => {}
                    Step::Echo(e) => acts.push(Act::Write(sink, e)),
                    Step::Line { bytes, echo } => {
                        acts.push(Act::Write(sink, echo));
                        let (rid, _) = self.ttys[i].waiting.take().expect("waiting");
                        acts.push(Act::Reply { ch: self.ttys[i].ch, op: OP_READ_LINE, request_id: rid, body: bytes });
                    }
                    // Canonical mode has no history to recall; a client that wants cursor keys
                    // reads raw.
                    Step::Key(k) => {
                        let _: Key = k;
                    }
                    Step::Eof => {
                        // Ctrl-D at an empty prompt is an *error*, not an empty line: a reader
                        // must tell "nothing this time" from "nothing ever again".
                        acts.push(Act::Write(sink, alloc::vec![b'\r', b'\n']));
                        let (rid, _) = self.ttys[i].waiting.take().expect("waiting");
                        acts.push(Act::Fail {
                            ch: self.ttys[i].ch,
                            op: OP_READ_LINE,
                            request_id: rid,
                            err: KError::PeerClosed.as_i32(),
                        });
                    }
                }
            }
        }

        /// Deliver an interrupt to every terminal on backend `id` and no other: `Ctrl-C` in one
        /// window must not interrupt a program in another.
        fn interrupt(&mut self, id: u32, acts: &mut Vec<Act>) {
            for t in self.ttys.iter_mut().filter(|t| t.backend == id) {
                acts.push(Act::Reply { ch: t.ch, op: OP_INTERRUPT, request_id: 0, body: Vec::new() });
                // An outstanding read completes empty (a raw read otherwise has at least one
                // byte), or a client at a prompt could not redraw until another key.
                match t.waiting.take() {
                    Some((rid, kind)) => {
                        let op = match kind {
                            ReadKind::Line => OP_READ_LINE,
                            ReadKind::Raw => OP_READ,
                        };
                        acts.push(Act::Reply { ch: t.ch, op, request_id: rid, body: Vec::new() });
                    }
                    // Nobody is reading *yet* — the shell is between prompts, or busy.
                    None => t.interrupt_pending = true,
                }
                t.disc.reset();
            }
        }

        /// Point terminal `ch` at a new backend whose sink is the channel `handle`.
        ///
        /// The pty's shape: an emulator hands the server the master half and keeps only the
        /// backend. Returns `(new backend id, any channel the caller must close)`, or `None`
        /// if `ch` is not a terminal.
        ///
        /// A terminal may be re-pointed: the old backend is dropped if nothing else uses it and
        /// an outstanding read is kept. Its channel comes back to the caller, because closing
        /// it is how the previous emulator learns its terminal has ended; dropping it would
        /// leak a handle and leave that emulator waiting for a `PeerClosed`.
        pub fn attach_backend(&mut self, ch: u64, handle: u64) -> Option<(u32, Option<u64>)> {
            let i = self.ttys.iter().position(|t| t.ch == ch)?;
            let old = self.ttys[i].backend;
            let id = self.next_backend;
            self.next_backend += 1;
            self.backends.push(Backend { id, sink: Sink::Channel(handle), pending: VecDeque::new() });
            self.ttys[i].backend = id;
            Some((id, self.retire(old)))
        }

        /// Drop backend `id` unless it is the console or has a terminal left, returning its
        /// channel for the caller to close.
        fn retire(&mut self, id: u32) -> Option<u64> {
            if id == CONSOLE || self.ttys.iter().any(|t| t.backend == id) {
                return None;
            }
            let orphan = self.backends.iter().find(|b| b.id == id).and_then(|b| match b.sink {
                Sink::Channel(h) => Some(h),
                Sink::Console => None,
            });
            self.backends.retain(|b| b.id != id);
            orphan
        }

        /// A `Write` request from terminal `ch`.
        pub fn write(&mut self, ch: u64, request_id: u64, body: &[u8]) -> Vec<Act> {
            let Some(sink) = self.sink_of(ch) else { return Vec::new() };
            alloc::vec![Act::Write(sink, body.to_vec()), Act::Reply { ch, op: OP_WRITE, request_id, body: Vec::new() },]
        }

        /// A `SetMode` request from terminal `ch`.
        pub fn set_mode(&mut self, ch: u64, request_id: u64, echo: bool) -> Vec<Act> {
            if let Some(t) = self.ttys.iter_mut().find(|t| t.ch == ch) {
                t.disc.set_echo(echo);
            }
            alloc::vec![Act::Reply { ch, op: OP_SET_MODE, request_id, body: Vec::new() }]
        }

        /// A `ReadLine` or `Read` request from terminal `ch`.
        pub fn read(&mut self, ch: u64, request_id: u64, kind: ReadKind) -> Vec<Act> {
            let op = match kind {
                ReadKind::Line => OP_READ_LINE,
                ReadKind::Raw => OP_READ,
            };
            let Some(i) = self.ttys.iter().position(|t| t.ch == ch) else { return Vec::new() };
            if self.ttys[i].waiting.is_some() {
                // One outstanding read per terminal: two would race for the same input.
                return alloc::vec![Act::Fail { ch, op, request_id, err: KError::WouldBlock.as_i32() }];
            }
            if self.ttys[i].interrupt_pending {
                self.ttys[i].interrupt_pending = false;
                return alloc::vec![Act::Reply { ch, op, request_id, body: Vec::new() }];
            }
            // A partial line from a canonical read must not leak into a raw one.
            if kind == ReadKind::Raw {
                self.ttys[i].disc.reset();
            }
            self.ttys[i].waiting = Some((request_id, kind));
            let backend = self.ttys[i].backend;
            self.drive(backend)
        }
    }

    /// Bytes a single raw read takes at most.
    const RAW_CHUNK: usize = 64;
}

/// One-time tokens naming a terminal's backend — `Tty::Token` and `Tty::Redeem`.
///
/// The view broker must learn a caller's terminal from this server, not the caller: a channel
/// the caller hands it could be one the caller serves itself, answering as any terminal it
/// likes. A token is 128 random bits, names one backend, and is good **once and briefly**.
pub mod tokens {
    use alloc::vec::Vec;

    /// Bytes in a token: the wire's length, not a second one.
    pub const LEN: usize = librsproto::TTY_TOKEN_LEN;

    /// How long a token lasts: enough for a slow machine (TCG under load) to redeem it.
    pub const TTL_NS: u64 = 30_000_000_000;

    /// Tokens kept at once. Each is redeemed or expires within [`TTL_NS`], so this bounds only a
    /// burst; past it the oldest is dropped, which costs its holder a password prompt.
    pub const CAPACITY: usize = 16;

    struct Entry {
        token: [u8; LEN],
        backend: u32,
        expires: u64,
    }

    /// The tokens minted and not yet redeemed or expired.
    #[derive(Default)]
    pub struct Tokens {
        live: Vec<Entry>,
    }

    /// A token from what `sys_entropy_read` answered (`r`, and the buffer), or none. Only `0`
    /// filled the buffer: a positive answer is a `PendingOperation` for an unseeded pool, which
    /// completes without writing. The serve loop does not wait for it, and a missing token only
    /// costs a password prompt.
    pub fn from_read(r: i64, buf: [u8; LEN]) -> Option<[u8; LEN]> {
        (r == 0).then_some(buf)
    }

    /// Equal bytes, compared without stopping at the first difference (constant time).
    fn same(a: &[u8; LEN], b: &[u8]) -> bool {
        b.len() == LEN && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }

    impl Tokens {
        /// No tokens.
        pub fn new() -> Tokens {
            Tokens { live: Vec::new() }
        }

        /// Keep `token` for `backend` until [`TTL_NS`] after `now`. Expired tokens go first; if
        /// the table is still full, the oldest goes.
        pub fn mint(&mut self, token: [u8; LEN], backend: u32, now: u64) {
            self.live.retain(|e| e.expires > now);
            if self.live.len() >= CAPACITY {
                self.live.remove(0);
            }
            self.live.push(Entry { token, backend, expires: now.saturating_add(TTL_NS) });
        }

        /// The backend `token` was minted for — **once**: it is gone after this. `None` for a
        /// token never minted, already redeemed, expired, or of the wrong length.
        pub fn redeem(&mut self, token: &[u8], now: u64) -> Option<u32> {
            let i = self.live.iter().position(|e| same(&e.token, token))?;
            let e = self.live.remove(i);
            (e.expires > now).then_some(e.backend)
        }

        /// Tokens held.
        pub fn len(&self) -> usize {
            self.live.len()
        }

        /// Whether none are held.
        pub fn is_empty(&self) -> bool {
            self.live.is_empty()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const T0: u64 = 1_000_000_000;

        fn token(n: u8) -> [u8; LEN] {
            [n; LEN]
        }

        /// After a `PendingOperation` the buffer is all zeros, a token anyone can name.
        #[test]
        fn only_a_completed_read_is_a_token() {
            assert_eq!(from_read(0, token(9)), Some(token(9)));
            assert_eq!(from_read(5, [0; LEN]), None, "a PO: unseeded, nothing written");
            assert_eq!(from_read(-1, [0; LEN]), None, "an error");
        }

        #[test]
        fn a_token_names_its_backend_once() {
            let mut t = Tokens::new();
            t.mint(token(1), 7, T0);
            assert_eq!(t.redeem(&token(1), T0 + 1), Some(7));
            assert_eq!(t.redeem(&token(1), T0 + 2), None, "a token is good once");
            assert!(t.is_empty());
        }

        #[test]
        fn a_token_never_minted_names_nothing() {
            let mut t = Tokens::new();
            t.mint(token(1), 7, T0);
            assert_eq!(t.redeem(&token(2), T0), None);
            // One byte off is not the token, whichever byte.
            for i in 0..LEN {
                let mut near = token(1);
                near[i] ^= 1;
                assert_eq!(t.redeem(&near, T0), None, "byte {i} differs");
            }
            assert_eq!(t.redeem(&token(1)[..LEN - 1], T0), None, "a short token");
            assert_eq!(t.redeem(&token(1), T0), Some(7), "and the real one is still there");
        }

        #[test]
        fn a_token_expires_at_its_ttl() {
            let mut t = Tokens::new();
            t.mint(token(1), 7, T0);
            assert_eq!(t.redeem(&token(1), T0 + TTL_NS - 1), Some(7), "just short of it");
            t.mint(token(2), 8, T0);
            assert_eq!(t.redeem(&token(2), T0 + TTL_NS), None, "at it");
            assert!(t.is_empty(), "an expired token is gone once tried");
        }

        #[test]
        fn a_full_table_drops_the_oldest_and_expired_go_first() {
            let mut t = Tokens::new();
            for n in 0..CAPACITY as u8 {
                t.mint(token(n), n as u32, T0);
            }
            assert_eq!(t.len(), CAPACITY);
            t.mint(token(200), 200, T0);
            assert_eq!(t.len(), CAPACITY);
            assert_eq!(t.redeem(&token(0), T0), None, "the oldest went");
            assert_eq!(t.redeem(&token(1), T0), Some(1), "the next did not");
            // Once the rest have expired, a mint makes room without dropping a live one.
            t.mint(token(201), 201, T0 + TTL_NS);
            assert_eq!(t.len(), 1);
            assert_eq!(t.redeem(&token(201), T0 + TTL_NS), Some(201));
        }

        #[test]
        fn siblings_share_a_backend_so_their_tokens_name_the_same_one() {
            use super::super::routing::Registry;
            let mut reg = Registry::new();
            reg.open(10);
            assert!(reg.open_sibling(11, 10));
            assert_eq!(reg.backend_of_terminal(10), reg.backend_of_terminal(11));
            assert_eq!(reg.backend_of_terminal(12), None, "no such terminal");
            // A terminal given a backend of its own — a window's — names a different one.
            reg.open(20);
            let (window, _) = reg.attach_backend(20, 99).expect("terminal 20 exists");
            assert_eq!(reg.backend_of_terminal(20), Some(window));
            assert_ne!(reg.backend_of_terminal(20), reg.backend_of_terminal(10));
        }
    }
}

#[cfg(test)]
mod routing_tests {
    use super::routing::*;
    use alloc::vec::Vec;
    use librsproto::{OP_TTY_INTERRUPT, OP_TTY_READ, OP_TTY_READ_LINE};

    /// A registry with `n` terminals on the console, channels 1..=n.
    fn console_ttys(n: u64) -> Registry {
        let mut r = Registry::new();
        for ch in 1..=n {
            r.open(ch);
        }
        r
    }

    /// The `Reply` acts naming `ch`, as `(op, body)`.
    fn replies_to(acts: &[Act], ch: u64) -> Vec<(u16, Vec<u8>)> {
        acts.iter()
            .filter_map(|a| match a {
                Act::Reply { ch: c, op, body, .. } if *c == ch => Some((*op, body.clone())),
                _ => None,
            })
            .collect()
    }

    /// Everything written to `sink`, concatenated.
    fn written(acts: &[Act], sink: Sink) -> Vec<u8> {
        let mut out = Vec::new();
        for a in acts {
            if let Act::Write(s, b) = a
                && *s == sink
            {
                out.extend_from_slice(b);
            }
        }
        out
    }

    #[test]
    fn a_line_goes_to_the_terminal_that_asked_for_it() {
        let mut r = console_ttys(1);
        let acts = r.read(1, 77, ReadKind::Line);
        assert!(acts.is_empty(), "nothing typed yet");
        let acts = r.feed(CONSOLE, b"hi\r");
        assert_eq!(replies_to(&acts, 1), [(OP_TTY_READ_LINE, b"hi".to_vec())]);
    }

    /// Canonical mode knows no cursor, as Linux's line discipline knows none: an editing key is
    /// dropped, so what is typed after it still goes on the end, and a password prompt behaves as
    /// it did.
    #[test]
    fn a_canonical_read_ignores_the_editing_keys() {
        let mut r = console_ttys(1);
        r.read(1, 77, ReadKind::Line);
        let acts = r.feed(CONSOLE, b"ab\x1b[D\x1b[H\x1b[3~X\r");
        assert_eq!(replies_to(&acts, 1), [(OP_TTY_READ_LINE, b"abX".to_vec())]);
    }

    #[test]
    fn input_typed_before_anyone_reads_is_kept_not_dropped() {
        let mut r = console_ttys(1);
        assert!(r.feed(CONSOLE, b"ahead\r").is_empty(), "nobody is reading yet");
        let acts = r.read(1, 5, ReadKind::Line);
        assert_eq!(replies_to(&acts, 1), [(OP_TTY_READ_LINE, b"ahead".to_vec())]);
    }

    #[test]
    fn input_from_one_backend_never_reaches_a_terminal_on_another() {
        let mut r = console_ttys(2);
        let (other, _) = r.attach_backend(2, 0xBEEF).expect("terminal 2 exists");
        assert_ne!(other, CONSOLE);

        // Terminal 2 is the only one reading, and the bytes arrive on the *console*.
        let acts = r.read(2, 9, ReadKind::Line);
        assert!(acts.is_empty());
        let acts = r.feed(CONSOLE, b"secret\r");
        assert!(replies_to(&acts, 2).is_empty(), "console input reached the window: {acts:?}");

        // ...and the same bytes on its own backend do reach it.
        let acts = r.feed(other, b"typed\r");
        assert_eq!(replies_to(&acts, 2), [(OP_TTY_READ_LINE, b"typed".to_vec())]);
    }

    #[test]
    fn an_interrupt_reaches_every_terminal_on_its_backend_and_no_others() {
        let mut r = console_ttys(2);
        let (win, _) = r.attach_backend(2, 0xBEEF).expect("terminal 2 exists");

        let acts = r.feed(CONSOLE, &[0x03]);
        assert_eq!(replies_to(&acts, 1), [(OP_TTY_INTERRUPT, Vec::new())], "the console terminal");
        assert!(replies_to(&acts, 2).is_empty(), "the window was interrupted too: {acts:?}");

        let acts = r.feed(win, &[0x03]);
        assert_eq!(replies_to(&acts, 2), [(OP_TTY_INTERRUPT, Vec::new())]);
        assert!(replies_to(&acts, 1).is_empty(), "the console terminal was interrupted too");
    }

    #[test]
    fn an_interrupt_ends_an_outstanding_read_and_one_that_has_not_started() {
        let mut r = console_ttys(1);
        r.read(1, 3, ReadKind::Line);
        let acts = r.feed(CONSOLE, &[0x03]);
        assert_eq!(
            replies_to(&acts, 1),
            [(OP_TTY_INTERRUPT, Vec::new()), (OP_TTY_READ_LINE, Vec::new())],
            "the event, then the read it ended",
        );

        // Nobody reading: the interrupt is remembered and ends the next read immediately.
        let acts = r.feed(CONSOLE, &[0x03]);
        assert_eq!(replies_to(&acts, 1), [(OP_TTY_INTERRUPT, Vec::new())], "no read to end yet");
        let acts = r.read(1, 4, ReadKind::Line);
        assert_eq!(replies_to(&acts, 1), [(OP_TTY_READ_LINE, Vec::new())], "ended before it began");

        // ...and only once. A second read waits for real input.
        let acts = r.read(1, 5, ReadKind::Line);
        assert!(acts.is_empty(), "the pending interrupt fired twice");
    }

    #[test]
    fn a_terminals_output_goes_to_its_own_backend() {
        let mut r = console_ttys(2);
        r.attach_backend(2, 0xBEEF);
        assert_eq!(r.sink_of(1), Some(Sink::Console));
        assert_eq!(r.sink_of(2), Some(Sink::Channel(0xBEEF)));

        let acts = r.write(2, 1, b"hello");
        assert_eq!(written(&acts, Sink::Channel(0xBEEF)), b"hello");
        assert_eq!(written(&acts, Sink::Console), b"", "it printed on the console as well");
    }

    #[test]
    fn an_echo_goes_to_the_backend_the_byte_came_from() {
        // The discipline generates echo, not the client; a fixed sink would show a window's
        // password on the console.
        let mut r = console_ttys(1);
        let (win, _) = r.attach_backend(1, 0xF00D).expect("terminal 1 exists");
        r.read(1, 1, ReadKind::Line);
        let acts = r.feed(win, b"ab");
        assert_eq!(written(&acts, Sink::Channel(0xF00D)), b"ab");
        assert_eq!(written(&acts, Sink::Console), b"", "the echo reached the console");
    }

    #[test]
    fn a_second_read_on_one_terminal_is_refused_rather_than_queued() {
        let mut r = console_ttys(1);
        r.read(1, 1, ReadKind::Line);
        let acts = r.read(1, 2, ReadKind::Line);
        assert!(matches!(acts.as_slice(), [Act::Fail { request_id: 2, .. }]), "expected a refusal, got {acts:?}",);
    }

    #[test]
    fn a_raw_read_stops_at_an_interrupt_in_the_same_chunk() {
        let mut r = console_ttys(1);
        r.read(1, 1, ReadKind::Raw);
        let acts = r.feed(CONSOLE, b"ab\x03cd");
        let got = replies_to(&acts, 1);
        assert_eq!(got[0], (OP_TTY_READ, b"ab".to_vec()), "the bytes before the interrupt");
        assert!(got.iter().any(|(op, _)| *op == OP_TTY_INTERRUPT), "the interrupt was swallowed as data: {got:?}",);
    }

    /// A backend is a channel end the server can send on, receive from and wait on; lacking any
    /// one of the three is refused, and so is any other object holding them all.
    #[test]
    fn a_backend_is_a_channel_it_can_send_receive_and_wait_on() {
        use libkern::error::KError;
        use libkern::handle::{
            KOBJ_IPC_CHANNEL, KOBJ_MEMORY_OBJECT, RIGHT_RECV, RIGHT_SEND, RIGHT_TRANSFER, RIGHT_WAIT,
        };
        let all = RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT;
        assert_eq!(backend_refusal(KOBJ_IPC_CHANNEL, all), None);
        assert_eq!(backend_refusal(KOBJ_IPC_CHANNEL, all | RIGHT_TRANSFER), None);
        for right in [RIGHT_SEND, RIGHT_RECV, RIGHT_WAIT] {
            assert_eq!(backend_refusal(KOBJ_IPC_CHANNEL, all & !right), Some(KError::NoAccess), "{right:#x}");
        }
        assert_eq!(backend_refusal(KOBJ_MEMORY_OBJECT, all), Some(KError::InvalidArgument));
    }

    #[test]
    fn re_pointing_a_terminal_hands_back_the_backend_it_left() {
        // Closing that channel is how the previous emulator learns its terminal has ended
        // (`rsproto-tty-ops.md` § Lifetime).
        let mut r = console_ttys(1);
        let (_first, orphan) = r.attach_backend(1, 0xAAA).expect("terminal 1 exists");
        assert_eq!(orphan, None, "there was no channel backend before this one");

        let (second, orphan) = r.attach_backend(1, 0xBBB).expect("terminal 1 exists");
        assert_eq!(orphan, Some(0xAAA), "the channel it left was not handed back");
        assert_eq!(r.backend_channels().collect::<Vec<_>>(), [0xBBB]);
        assert_eq!(r.sink_of(1), Some(Sink::Channel(0xBBB)));

        // ...and closing the terminal still hands back the one it ends on.
        assert_eq!(r.close_and_retire(1), [0xBBB]);
        let _ = second;
    }

    #[test]
    fn a_backend_with_another_terminal_left_on_it_is_not_handed_back() {
        let mut r = console_ttys(2);
        let (id, _) = r.attach_backend(1, 0xC0DE).expect("terminal 1 exists");
        r.move_to(2, id);
        assert_eq!(r.close_and_retire(1), Vec::<u64>::new(), "the channel was closed too early");
        assert_eq!(r.sink_of(2), Some(Sink::Channel(0xC0DE)));
        assert_eq!(r.close_and_retire(2), [0xC0DE], "and released when the last one goes");
    }

    #[test]
    fn closing_a_terminal_leaves_its_backends_other_terminals_alone() {
        let mut r = console_ttys(3);
        assert_eq!(r.len(), 3);
        assert!(r.close(2));
        assert_eq!(r.len(), 2);
        assert!(!r.close(2), "closing twice reported success");
        assert_eq!(r.sink_of(1), Some(Sink::Console));
        assert_eq!(r.sink_of(3), Some(Sink::Console));
    }

    /// A shell in a window (terminal 1 on backend `0xAB`) and a stage's sibling terminal (2).
    fn shell_and_stage_in_a_window() -> (Registry, u32) {
        let mut r = console_ttys(1);
        let (win, _) = r.attach_backend(1, 0xAB).expect("terminal 1 exists");
        assert!(r.open_sibling(2, 1), "terminal 1 exists");
        (r, win)
    }

    #[test]
    fn a_sibling_joins_its_terminals_window_not_the_console() {
        let (mut r, win) = shell_and_stage_in_a_window();
        assert_eq!(r.sink_of(2), Some(Sink::Channel(0xAB)));
        r.read(2, 7, ReadKind::Line);
        assert!(replies_to(&r.feed(CONSOLE, b"no\r"), 2).is_empty(), "console input reached it");
        assert_eq!(replies_to(&r.feed(win, b"pw\r"), 2), [(OP_TTY_READ_LINE, b"pw".to_vec())]);
    }

    #[test]
    fn the_stage_that_is_reading_gets_the_input_while_the_shell_waits() {
        let (mut r, win) = shell_and_stage_in_a_window();
        r.read(2, 3, ReadKind::Line);
        let acts = r.feed(win, b"yes\r");
        assert_eq!(replies_to(&acts, 2), [(OP_TTY_READ_LINE, b"yes".to_vec())]);
        assert!(replies_to(&acts, 1).is_empty(), "the shell's terminal was sent a reply");
    }

    #[test]
    fn ctrl_c_reaches_the_shell_and_the_stage_alike() {
        let (mut r, win) = shell_and_stage_in_a_window();
        let acts = r.feed(win, &[0x03]);
        assert_eq!(replies_to(&acts, 1), [(OP_TTY_INTERRUPT, Vec::new())], "the shell");
        assert_eq!(replies_to(&acts, 2), [(OP_TTY_INTERRUPT, Vec::new())], "the stage");
    }

    #[test]
    fn a_stages_echo_setting_is_its_own() {
        let (mut r, win) = shell_and_stage_in_a_window();
        r.set_mode(2, 1, false);
        r.read(2, 2, ReadKind::Line);
        let acts = r.feed(win, b"secret\r");
        assert_eq!(written(&acts, Sink::Channel(0xAB)), b"", "the password was echoed");
        r.read(1, 3, ReadKind::Line);
        let acts = r.feed(win, b"ls");
        assert_eq!(written(&acts, Sink::Channel(0xAB)), b"ls", "the shell stopped echoing");
    }

    #[test]
    fn closing_a_stages_terminal_leaves_the_shell_its_window() {
        let (mut r, _) = shell_and_stage_in_a_window();
        // Without this the test passes vacuously for a sibling that never joined the window.
        assert_eq!(r.sink_of(2), Some(Sink::Channel(0xAB)), "precondition: the stage is on it");
        assert_eq!(r.close_and_retire(2), Vec::<u64>::new(), "the window's channel was released");
        assert_eq!(r.sink_of(1), Some(Sink::Channel(0xAB)));
        assert_eq!(r.close_and_retire(1), [0xAB], "and released with the last terminal on it");
    }

    #[test]
    fn a_sibling_of_a_terminal_that_does_not_exist_is_refused() {
        let mut r = console_ttys(1);
        assert!(!r.open_sibling(2, 99));
        assert_eq!(r.len(), 1, "a terminal was added anyway");
    }
}
