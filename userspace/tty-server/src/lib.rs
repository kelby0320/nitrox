//! `tty-server` — the line discipline, separated from the syscalls.
//!
//! Everything with behaviour lives here and host-tests in milliseconds: what a byte does to
//! a line buffer, when a line is complete, what gets echoed. The binary half owns the
//! console device, the IPC channels, and the wait loop.
//!
//! **The seam is the reason this crate is split.** Line editing existed three times before
//! this server — in `eshell`, in `session-mgr`'s login, and in `nxsh`'s REPL — and the three
//! copies disagreed: two of them differed over whether to echo the CR/LF that ends a line,
//! which is why a password prompt rendered as `alicepassword:` for weeks. One implementation
//! behind a testable seam is the whole point of the exercise, so the syscalls stay out.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::vec::Vec;

/// What the caller should do after feeding one byte.
///
/// The discipline never writes anything itself — it *says* what to write. That is what keeps
/// it free of syscalls, and it is also what lets a test assert on the echo.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Step {
    /// Nothing to do.
    None,
    /// Write these bytes to the terminal (an echo, or an erase sequence).
    Echo(Vec<u8>),
    /// A line is complete. Its bytes (no terminator) are the payload; `echo` is what to
    /// write to move the cursor on, which is empty in no-echo mode.
    Line { bytes: Vec<u8>, echo: Vec<u8> },
    /// A key this discipline deliberately does not act on, reported for the caller to
    /// decide. Arrow keys are the case that matters: recalling history means replacing the
    /// line, and *what* to replace it with is the shell's business, not the terminal's.
    Key(Key),
    /// End of input — Ctrl-D at an empty line. Distinct from an empty *line*, which is
    /// what Enter on its own produces: one means "nothing this time", the other means
    /// "nothing ever again", and a reader that conflates them either exits on a stray
    /// Enter or cannot be exited at all.
    Eof,
}

/// A key with no meaning to the line discipline itself.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Key {
    Up,
    Down,
}

/// Where the escape-sequence parser is.
///
/// Arrow keys arrive as three bytes (`ESC [ A`), so recognising them needs state. Kept
/// deliberately small: this parses *cursor keys and nothing else*, and unknown sequences are
/// discarded rather than accumulated. It is the first step toward terminal input parsing,
/// and the stopping point is chosen rather than discovered — anything richer belongs with a
/// real keyboard driver, which can report modifiers a serial byte stream cannot express.
#[derive(Copy, Clone, PartialEq, Eq)]
enum EscState {
    Ground,
    Esc,
    Csi,
}

/// One terminal's editing state.
pub struct Discipline {
    line: Vec<u8>,
    echo: bool,
    max: usize,
    esc: EscState,
}

/// The longest line accepted before further printable input is refused.
///
/// Refusing beats truncating: a silently shortened password or path is a wrong value that
/// looks like a right one.
pub const LINE_MAX: usize = 1024;

impl Default for Discipline {
    fn default() -> Self {
        Self::new()
    }
}

impl Discipline {
    pub fn new() -> Self {
        Discipline { line: Vec::new(), echo: true, max: LINE_MAX, esc: EscState::Ground }
    }

    /// Whether typed characters are echoed. Off is how a password is read — and because it
    /// is *the server's* state, a client cannot forget to ask for it the way every caller of
    /// the old `read_line(..., echo: false)` had to remember.
    pub fn set_echo(&mut self, on: bool) {
        self.echo = on;
    }

    pub fn echo_on(&self) -> bool {
        self.echo
    }

    /// Discard a partially typed line — what a tty being handed to a new reader needs, so no
    /// one inherits half of somebody else's input.
    pub fn reset(&mut self) {
        self.line.clear();
        self.esc = EscState::Ground;
    }

    /// The line as typed so far.
    pub fn line(&self) -> &[u8] {
        &self.line
    }

    /// Replace the whole line, returning what to write so the display matches.
    ///
    /// This is the redraw primitive history recall needs, and the reason the line buffer
    /// stays here rather than being reimplemented by every caller that wants a history: the
    /// discipline knows what is on screen, so only it can say how to erase it.
    ///
    /// Erase-then-write, not a cursor-addressing sequence: a dumb terminal is the baseline,
    /// and `\x08 \x08` per character is what works everywhere.
    pub fn replace_line(&mut self, new: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        if self.echo {
            for _ in 0..self.line.len() {
                out.extend_from_slice(b"\x08 \x08");
            }
            out.extend_from_slice(new);
        }
        self.line.clear();
        self.line.extend_from_slice(&new[..new.len().min(self.max)]);
        out
    }

    /// Feed one byte from the device.
    pub fn feed(&mut self, b: u8) -> Step {
        // Mid-escape bytes are never text: a `[` after ESC is not a bracket the user typed.
        match self.esc {
            EscState::Ground => {}
            EscState::Esc => {
                self.esc = if b == b'[' { EscState::Csi } else { EscState::Ground };
                return Step::None;
            }
            EscState::Csi => {
                // **A CSI ends at its *final* byte, not at its first.** This consumed exactly
                // one byte after `ESC [` and called the sequence over, which is right for
                // `ESC [ A` and wrong for every sequence with a parameter: `ESC [ 3 ~` — the
                // Delete key on any real terminal — ended at the `3`, and the `~` landed in the
                // ground state as printable ASCII, so it was pushed onto the line **and
                // echoed**. Typing Delete then `list /bin` handed the shell `~list /bin`.
                //
                // Live over the serial console since this discipline was written; found while
                // reviewing `libterm`'s encoder, which produces the same four sequences
                // (PR #191 review, finding 1). The old comment claimed "the sequence cannot
                // leak into the line", which was the claim to check.
                return match b {
                    // Parameter and intermediate bytes belong to the sequence.
                    0x20..=0x3F => Step::None,
                    // `ESC` cancels and restarts, so an interrupted sequence cannot make the
                    // next one disappear.
                    0x1B => {
                        self.esc = EscState::Esc;
                        Step::None
                    }
                    // A final byte ends it.
                    0x40..=0x7E => {
                        self.esc = EscState::Ground;
                        match b {
                            b'A' => Step::Key(Key::Up),
                            b'B' => Step::Key(Key::Down),
                            // Left/right and everything else: recognised as *ended*, so the
                            // sequence cannot leak into the line, but not acted on.
                            _ => Step::None,
                        }
                    }
                    // Malformed. End the sequence rather than swallowing the rest of the
                    // stream, which is what a terminal going silent looks like.
                    _ => {
                        self.esc = EscState::Ground;
                        Step::None
                    }
                };
            }
        }
        match b {
            // ESC begins a sequence. Alone it does nothing, which is the right answer for a
            // key with no meaning here.
            0x1b => {
                self.esc = EscState::Esc;
                Step::None
            }
            // CR and LF both end a line. A serial line may send either, and which one is
            // an accident of the terminal rather than a distinction worth carrying.
            b'\r' | b'\n' => {
                let bytes = core::mem::take(&mut self.line);
                // The newline is echoed *because the user typed it*. Omitting it leaves the
                // cursor on the line they typed on, so the next prompt lands against their
                // input — exactly the `alicepassword:` bug. In no-echo mode nothing was
                // shown, so the caller supplies its own newline when it is ready.
                let echo = if self.echo { b"\r\n".to_vec() } else { Vec::new() };
                Step::Line { bytes, echo }
            }
            // Ctrl-D. At an empty line it is end-of-input, the universal convention. With
            // a partial line it does nothing: bash uses it to list completions there, and
            // silently discarding what someone typed would be worse than ignoring a key.
            0x04 => {
                if self.line.is_empty() {
                    Step::Eof
                } else {
                    Step::None
                }
            }
            // Backspace and DEL both erase: terminals disagree about which they send.
            0x08 | 0x7f => {
                if self.line.pop().is_some() && self.echo {
                    // Back up, overwrite with a space, back up again — the only way to
                    // erase on a dumb terminal.
                    Step::Echo(b"\x08 \x08".to_vec())
                } else {
                    // Nothing to erase, or nothing to show. Not an error: backspace at an
                    // empty prompt is something people do constantly.
                    Step::None
                }
            }
            // Ctrl-U: kill the line. Erase exactly what is on screen, no more — in no-echo
            // mode that is nothing.
            0x15 => {
                let n = self.line.len();
                self.line.clear();
                if self.echo && n > 0 {
                    let mut out = Vec::with_capacity(n * 3);
                    for _ in 0..n {
                        out.extend_from_slice(b"\x08 \x08");
                    }
                    Step::Echo(out)
                } else {
                    Step::None
                }
            }
            // Ctrl-W: erase the word before the cursor, plus the run of spaces before it,
            // which is what makes repeated presses walk back through a command line.
            0x17 => {
                let before = self.line.len();
                while self.line.last() == Some(&b' ') {
                    self.line.pop();
                }
                while let Some(&c) = self.line.last() {
                    if c == b' ' {
                        break;
                    }
                    self.line.pop();
                }
                let n = before - self.line.len();
                if self.echo && n > 0 {
                    let mut out = Vec::with_capacity(n * 3);
                    for _ in 0..n {
                        out.extend_from_slice(b"\x08 \x08");
                    }
                    Step::Echo(out)
                } else {
                    Step::None
                }
            }
            // Printable ASCII accumulates.
            0x20..=0x7e => {
                if self.line.len() >= self.max {
                    return Step::None; // refuse, do not truncate
                }
                self.line.push(b);
                if self.echo { Step::Echo(alloc::vec![b]) } else { Step::None }
            }
            // Everything else — control codes this discipline does not define — is dropped
            // rather than accumulated, so a stray byte cannot corrupt a command.
            _ => Step::None,
        }
    }
}

/// Which terminals exist, where each one's bytes come from and go to, and who is waiting.
///
/// **The routing half, split out for the reason the discipline was.** Until Milestone 5 Part C
/// this lived in the binary as three globals and a flat `Vec<Tty>`, where none of it could be
/// tested: the rule "input goes to the first terminal that is waiting" and the rule "`Ctrl-C`
/// goes to *every* terminal" were both single lines with no way to state them except in prose.
/// Both change in this part — from "first waiter anywhere" to "first waiter **on this
/// backend**" — and a change to an untestable rule is a change nobody can check.
///
/// Nothing here performs a syscall. Handles are opaque `u64`s and every effect comes back as an
/// [`Act`] for the caller to perform, which is the same shape as the compositor's
/// `server::dispatch` and for the same reason.
pub mod routing {
    use super::{Discipline, Key, Step};
    use alloc::collections::VecDeque;
    use alloc::vec::Vec;

    // **The real definitions, not copies.** A first draft duplicated these with a comment
    // saying the host tests must not need the wire format, and got `PeerClosed` wrong by 19 —
    // which no test could have caught, because the copy was the only thing the tests saw. Both
    // crates are `no_std` and both host-test; the compositor's library half already depends on
    // `librsproto` for exactly this reason.
    use libkern::error::KError;
    use librsproto::{
        OP_TTY_INTERRUPT as OP_INTERRUPT, OP_TTY_READ as OP_READ,
        OP_TTY_READ_LINE as OP_READ_LINE, OP_TTY_SET_MODE as OP_SET_MODE,
        OP_TTY_WRITE as OP_WRITE,
    };

    /// Where a terminal's bytes come from and go to.
    ///
    /// **The seam `console-and-tty.md` built the backend for.** One line discipline, two
    /// sources: a serial port, or a terminal emulator holding what Unix would call the master
    /// half of a pty. The discipline cannot tell the difference and must not be able to.
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
        pub disc: Discipline,
        /// The outstanding read, if the client is waiting for input.
        pub waiting: Option<(u64, ReadKind)>,
        /// An interrupt arrived while **nobody was reading**, so the next read completes empty
        /// immediately. Without this, `Ctrl-C` between prompts would only take effect once the
        /// user pressed something else — the interrupt has to be able to end a read that has
        /// not started yet.
        pub interrupt_pending: bool,
        /// Which backend this terminal belongs to.
        pub backend: u32,
    }

    /// A backend, and the input from it that no terminal has taken yet.
    ///
    /// **The queue is per backend, not global.** Bytes typed at the serial console must not be
    /// deliverable to a terminal inside a window, and a queue shared between them makes that a
    /// matter of which terminal happened to ask first.
    pub struct Backend {
        pub id: u32,
        pub sink: Sink,
        pending: VecDeque<u8>,
    }

    /// An effect the caller must perform. Nothing in this module does I/O.
    #[derive(Clone, PartialEq, Eq, Debug)]
    pub enum Act {
        /// Write bytes to a sink — program output, or an echo.
        Write(Sink, Vec<u8>),
        /// Send a message on a terminal's channel. `request_id` 0 means unsolicited, which is
        /// what an interrupt is.
        Reply { ch: u64, op: u16, request_id: u64, body: Vec<u8> },
        /// Send an error reply.
        Fail { ch: u64, op: u16, request_id: u64, err: i32 },
    }

    /// The id the console backend always has.
    ///
    /// Fixed rather than allocated, because the console exists before anything can ask for it
    /// and a terminal opened with no backend named has to land somewhere.
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
                backends: alloc::vec![Backend {
                    id: CONSOLE,
                    sink: Sink::Console,
                    pending: VecDeque::new(),
                }],
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
        /// The caller closes the returned handles. **Returned rather than closed here** for the
        /// reason nothing in this module does I/O — and closing a backend channel is not
        /// bookkeeping: it is what tells a terminal emulator its terminal is gone.
        pub fn close_and_retire(&mut self, ch: u64) -> Vec<u64> {
            let Some(i) = self.ttys.iter().position(|t| t.ch == ch) else { return Vec::new() };
            let backend = self.ttys[i].backend;
            self.ttys.remove(i);
            let orphan = (backend != CONSOLE && !self.ttys.iter().any(|t| t.backend == backend))
                .then(|| self.backends.iter().find(|b| b.id == backend).map(|b| b.sink))
                .flatten();
            self.retire(backend);
            match orphan {
                Some(Sink::Channel(h)) => alloc::vec![h],
                _ => Vec::new(),
            }
        }

        /// Every terminal on backend `id`, by channel.
        ///
        /// Collected rather than iterated, because the caller is about to close each one and
        /// cannot hold a borrow of the registry while doing it.
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

        /// Deliver whatever backend `id` has queued to whichever of *its* terminals is waiting.
        ///
        /// Input that arrives with nobody waiting stays queued rather than being dropped —
        /// typing ahead of a prompt is ordinary, and losing it would be a bug the user cannot
        /// see.
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
                // **`Ctrl-C` is taken before anything else, and never queues.** It is an event
                // about the terminal rather than input to it, so it is answered whether or not
                // anybody is reading — which is the whole point, since during an evaluation
                // nobody is.
                if self.backends[bi].pending.front() == Some(&0x03) {
                    self.backends[bi].pending.pop_front();
                    self.interrupt(id, &mut acts);
                    continue;
                }
                let Some(i) = self.ttys.iter().position(|t| t.backend == id && t.waiting.is_some())
                else {
                    return acts; // nobody on this backend is reading; hold the bytes
                };
                if let Some((rid, ReadKind::Raw)) = self.ttys[i].waiting {
                    // **Stop at an interrupt.** A raw read takes everything available in one
                    // go, which handed `Ctrl-C` to the client as an ordinary byte whenever it
                    // arrived in the same chunk as the keystrokes before it — the front-of-queue
                    // check above never saw it. Bytes typed *before* the interrupt are still
                    // input; the interrupt itself is picked up on the next turn of this loop.
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
                    acts.push(Act::Reply {
                        ch: self.ttys[i].ch,
                        op: OP_READ,
                        request_id: rid,
                        body: out,
                    });
                    continue;
                }
                let b = self.backends[bi].pending.pop_front().expect("non-empty");
                match self.ttys[i].disc.feed(b) {
                    Step::None => {}
                    Step::Echo(e) => acts.push(Act::Write(sink, e)),
                    Step::Line { bytes, echo } => {
                        acts.push(Act::Write(sink, echo));
                        let (rid, _) = self.ttys[i].waiting.take().expect("waiting");
                        acts.push(Act::Reply {
                            ch: self.ttys[i].ch,
                            op: OP_READ_LINE,
                            request_id: rid,
                            body: bytes,
                        });
                    }
                    // Canonical mode has no history to recall, so a cursor key is nothing here.
                    // A client that wants them reads raw and runs the discipline itself.
                    Step::Key(k) => {
                        let _: Key = k;
                    }
                    Step::Eof => {
                        // Ctrl-D at an empty prompt. Answered as an *error* rather than an empty
                        // line, because those are different answers and a reader that conflated
                        // them would either exit on a stray Enter or never exit at all.
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

        /// Deliver an interrupt to every terminal on backend `id`.
        ///
        /// **On that backend and no other**, which is the correction Part C makes. The flat
        /// version sent it to every terminal in the system and said so in a comment — "a session
        /// has one, and a server that guessed which was *foreground* would be inventing a
        /// concept this system does not have yet". Both halves of that were wrong: a session has
        /// as many terminals as its programs resolve, and this is not a guess about which is
        /// foreground — a backend is a *physical* grouping. Typing `Ctrl-C` in one window has no
        /// business interrupting a program in another.
        fn interrupt(&mut self, id: u32, acts: &mut Vec<Act>) {
            for t in self.ttys.iter_mut().filter(|t| t.backend == id) {
                acts.push(Act::Reply {
                    ch: t.ch,
                    op: OP_INTERRUPT,
                    request_id: 0,
                    body: Vec::new(),
                });
                // **An outstanding read is completed, empty.** Otherwise a client sitting at a
                // prompt stays blocked on a read whose byte just became an event, and could not
                // redraw until the user typed something else. A raw read otherwise only ever
                // completes with at least one byte, so an empty completion is unambiguous.
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
        /// **The pty's shape.** A terminal emulator resolves `/dev/tty` like any program, gets a
        /// terminal, and then says "this one's bytes come from and go to me" — handing the
        /// server the master half. It then passes the terminal itself to the shell it hosts and
        /// keeps only the backend, which is exactly the split Unix draws between the two ends of
        /// a pty. One line discipline serves both, which is what
        /// `console-and-tty.md` built the backend seam for.
        ///
        /// Returns the new backend's id, or `None` if `ch` is not a terminal.
        ///
        /// **A terminal may be re-pointed**, and the old backend is dropped if nothing else uses
        /// it. Re-pointing while a read is outstanding keeps the read: it is the same terminal
        /// and the same client, and failing the read would make attaching a backend a visible
        /// hiccup for a program that never asked about backends.
        pub fn attach_backend(&mut self, ch: u64, handle: u64) -> Option<u32> {
            let i = self.ttys.iter().position(|t| t.ch == ch)?;
            let old = self.ttys[i].backend;
            let id = self.next_backend;
            self.next_backend += 1;
            self.backends.push(Backend { id, sink: Sink::Channel(handle), pending: VecDeque::new() });
            self.ttys[i].backend = id;
            self.retire(old);
            Some(id)
        }

        /// Drop backend `id` if it is not the console and no terminal is left on it.
        ///
        /// The console is never retired: it exists before anything can ask for it, and a
        /// terminal opened with no backend named has to land somewhere.
        fn retire(&mut self, id: u32) {
            if id == CONSOLE || self.ttys.iter().any(|t| t.backend == id) {
                return;
            }
            self.backends.retain(|b| b.id != id);
        }

        /// A `Write` request from terminal `ch`.
        pub fn write(&mut self, ch: u64, request_id: u64, body: &[u8]) -> Vec<Act> {
            let Some(sink) = self.sink_of(ch) else { return Vec::new() };
            alloc::vec![
                Act::Write(sink, body.to_vec()),
                Act::Reply { ch, op: OP_WRITE, request_id, body: Vec::new() },
            ]
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
                // One outstanding read per terminal: two would race for the same input and there
                // is no rule that says which should win.
                return alloc::vec![Act::Fail { ch, op, request_id, err: KError::WouldBlock.as_i32() }];
            }
            if self.ttys[i].interrupt_pending {
                self.ttys[i].interrupt_pending = false;
                return alloc::vec![Act::Reply { ch, op, request_id, body: Vec::new() }];
            }
            // A partial line from a previous canonical read must not leak into a raw one, or the
            // shell's first keystroke arrives with somebody else's prefix.
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

    #[test]
    fn input_typed_before_anyone_reads_is_kept_not_dropped() {
        // Typing ahead of a prompt is ordinary, and losing it is a bug the user cannot see.
        let mut r = console_ttys(1);
        assert!(r.feed(CONSOLE, b"ahead\r").is_empty(), "nobody is reading yet");
        let acts = r.read(1, 5, ReadKind::Line);
        assert_eq!(replies_to(&acts, 1), [(OP_TTY_READ_LINE, b"ahead".to_vec())]);
    }

    #[test]
    fn input_from_one_backend_never_reaches_a_terminal_on_another() {
        // **The Part C rule.** Two windows, or a window and the serial console: bytes typed at
        // one must not be deliverable to a program in the other. Before this, one flat queue
        // and one flat `Vec` meant the recipient was whichever terminal happened to be waiting.
        let mut r = console_ttys(2);
        let other = r.attach_backend(2, 0xBEEF).expect("terminal 2 exists");
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
        // The other Part C rule. `Ctrl-C` in one window must not interrupt a program in
        // another — the flat version broadcast to every terminal in the system and justified it
        // with "a session has one", which was never true.
        let mut r = console_ttys(2);
        let win = r.attach_backend(2, 0xBEEF).expect("terminal 2 exists");

        let acts = r.feed(CONSOLE, &[0x03]);
        assert_eq!(replies_to(&acts, 1), [(OP_TTY_INTERRUPT, Vec::new())], "the console terminal");
        assert!(replies_to(&acts, 2).is_empty(), "the window was interrupted too: {acts:?}");

        let acts = r.feed(win, &[0x03]);
        assert_eq!(replies_to(&acts, 2), [(OP_TTY_INTERRUPT, Vec::new())]);
        assert!(replies_to(&acts, 1).is_empty(), "the console terminal was interrupted too");
    }

    #[test]
    fn an_interrupt_ends_an_outstanding_read_and_one_that_has_not_started() {
        // Both halves, because they are different mechanisms: a waiting read completes empty,
        // and a `Ctrl-C` between prompts has to end the *next* read rather than wait for a
        // keystroke that may never come.
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
        // The write half of the same rule: a program in a window must not print on the serial
        // console, which is what a single global `backend_write` did by construction.
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
        // Echo is the subtle one: it is generated by the *discipline*, not by the client, so a
        // version that echoed to a fixed sink would type a window's password on the console.
        let mut r = console_ttys(1);
        let win = r.attach_backend(1, 0xF00D).expect("terminal 1 exists");
        r.read(1, 1, ReadKind::Line);
        let acts = r.feed(win, b"ab");
        assert_eq!(written(&acts, Sink::Channel(0xF00D)), b"ab");
        assert_eq!(written(&acts, Sink::Console), b"", "the echo reached the console");
    }

    #[test]
    fn a_second_read_on_one_terminal_is_refused_rather_than_queued() {
        // Two outstanding reads would race for the same input and there is no rule saying
        // which should win.
        let mut r = console_ttys(1);
        r.read(1, 1, ReadKind::Line);
        let acts = r.read(1, 2, ReadKind::Line);
        assert!(
            matches!(acts.as_slice(), [Act::Fail { request_id: 2, .. }]),
            "expected a refusal, got {acts:?}",
        );
    }

    #[test]
    fn a_raw_read_stops_at_an_interrupt_in_the_same_chunk() {
        // A raw read takes everything available in one go, which handed `Ctrl-C` to the client
        // as an ordinary byte whenever it arrived in the same chunk as the keys before it.
        let mut r = console_ttys(1);
        r.read(1, 1, ReadKind::Raw);
        let acts = r.feed(CONSOLE, b"ab\x03cd");
        let got = replies_to(&acts, 1);
        assert_eq!(got[0], (OP_TTY_READ, b"ab".to_vec()), "the bytes before the interrupt");
        assert!(
            got.iter().any(|(op, _)| *op == OP_TTY_INTERRUPT),
            "the interrupt was swallowed as data: {got:?}",
        );
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a string, returning the completed lines and everything echoed.
    fn drive(d: &mut Discipline, input: &str) -> (Vec<alloc::string::String>, alloc::string::String) {
        let mut lines = Vec::new();
        let mut echoed = alloc::string::String::new();
        for b in input.bytes() {
            match d.feed(b) {
                Step::None => {}
                Step::Echo(e) => echoed.push_str(&alloc::string::String::from_utf8_lossy(&e)),
                Step::Line { bytes, echo } => {
                    lines.push(alloc::string::String::from_utf8_lossy(&bytes).into_owned());
                    echoed.push_str(&alloc::string::String::from_utf8_lossy(&echo));
                }
                // `drive` feeds ordinary input; a test that wants EOF asserts on `feed`.
                Step::Eof => panic!("unexpected end-of-input"),
                Step::Key(k) => panic!("unexpected key {k:?}"),
            }
        }
        (lines, echoed)
    }

    #[test]
    fn a_typed_line_is_returned_and_echoed() {
        let mut d = Discipline::new();
        let (lines, echoed) = drive(&mut d, "hello\n");
        assert_eq!(lines, ["hello"]);
        assert_eq!(echoed, "hello\r\n");
    }

    /// The bug this server exists to make impossible: the newline is echoed **because the
    /// user typed it**. Without it the cursor never leaves the line they typed on, so the
    /// next prompt lands against their input — `alicepassword:`.
    #[test]
    fn the_terminating_newline_is_echoed() {
        let mut d = Discipline::new();
        let (_, echoed) = drive(&mut d, "alice\n");
        assert!(echoed.ends_with("\r\n"), "echo was {echoed:?}");
    }

    /// CR and LF are the same key. Which one arrives is an accident of the terminal.
    #[test]
    fn cr_and_lf_both_end_a_line() {
        let mut d = Discipline::new();
        assert_eq!(drive(&mut d, "a\r").0, ["a"]);
        assert_eq!(drive(&mut d, "b\n").0, ["b"]);
    }

    /// No-echo is the server's state, so a password cannot leak through a caller that forgot
    /// to ask for it — which is what `read_line(..., echo: false)` depended on.
    #[test]
    fn no_echo_shows_nothing_at_all() {
        let mut d = Discipline::new();
        d.set_echo(false);
        let (lines, echoed) = drive(&mut d, "hunter2\n");
        assert_eq!(lines, ["hunter2"], "the line still reaches the caller");
        assert_eq!(echoed, "", "and nothing whatever was shown");
    }

    #[test]
    fn backspace_erases_one_character() {
        let mut d = Discipline::new();
        let (lines, echoed) = drive(&mut d, "abc\x08\n");
        assert_eq!(lines, ["ab"]);
        assert!(echoed.contains("\x08 \x08"));
    }

    /// DEL and backspace are the same key by another name.
    #[test]
    fn del_erases_like_backspace() {
        let mut d = Discipline::new();
        assert_eq!(drive(&mut d, "abc\x7f\n").0, ["ab"]);
    }

    /// Backspace at an empty prompt is something people do constantly; it must not erase
    /// the prompt itself.
    #[test]
    fn backspace_at_an_empty_line_writes_nothing() {
        let mut d = Discipline::new();
        let (_, echoed) = drive(&mut d, "\x08");
        assert_eq!(echoed, "");
    }

    #[test]
    fn ctrl_u_kills_the_line() {
        let mut d = Discipline::new();
        let (lines, _) = drive(&mut d, "throw away\x15kept\n");
        assert_eq!(lines, ["kept"]);
    }

    /// Erasing must not show more than was displayed: in no-echo mode, nothing.
    #[test]
    fn killing_a_hidden_line_erases_nothing_on_screen() {
        let mut d = Discipline::new();
        d.set_echo(false);
        let (_, echoed) = drive(&mut d, "secret\x15\n");
        assert_eq!(echoed, "");
    }

    #[test]
    fn ctrl_w_erases_a_word_and_the_spaces_before_it() {
        let mut d = Discipline::new();
        assert_eq!(drive(&mut d, "list /bin\x17\n").0, ["list "]);
        assert_eq!(drive(&mut d, "one two   \x17\n").0, ["one "]);
    }

    /// Refusing beats truncating: a silently shortened password is a wrong value that looks
    /// like a right one.
    #[test]
    fn an_over_long_line_is_refused_not_truncated() {
        let mut d = Discipline::new();
        for _ in 0..LINE_MAX {
            d.feed(b'x');
        }
        assert_eq!(d.feed(b'y'), Step::None, "further input is refused");
        match d.feed(b'\n') {
            Step::Line { bytes, .. } => {
                assert_eq!(bytes.len(), LINE_MAX);
                assert!(bytes.iter().all(|&b| b == b'x'), "no `y` sneaked in");
            }
            other => panic!("expected a line, got {other:?}"),
        }
    }

    /// A stray control byte must not become part of a command.
    /// Ctrl-D at an empty prompt is end-of-input — the convention the shell's own banner
    /// promises ("Ctrl-D or `exit` to leave").
    #[test]
    fn ctrl_d_at_an_empty_line_is_end_of_input() {
        let mut d = Discipline::new();
        assert_eq!(d.feed(0x04), Step::Eof);
    }

    /// ...but not mid-line, where discarding what someone typed would be worse than
    /// ignoring a key.
    #[test]
    fn ctrl_d_mid_line_does_nothing() {
        let mut d = Discipline::new();
        d.feed(b'a');
        assert_eq!(d.feed(0x04), Step::None);
        match d.feed(b'\n') {
            Step::Line { bytes, .. } => assert_eq!(bytes, b"a"),
            other => panic!("expected the line intact, got {other:?}"),
        }
    }

    /// End of input and an empty line are different answers: one means "nothing this
    /// time", the other "nothing ever again".
    #[test]
    fn an_empty_line_is_not_end_of_input() {
        let mut d = Discipline::new();
        match d.feed(b'\n') {
            Step::Line { bytes, .. } => assert!(bytes.is_empty()),
            other => panic!("expected an empty line, got {other:?}"),
        }
    }

    /// Arrow keys are three bytes and must not appear in the line.
    #[test]
    fn cursor_keys_are_reported_not_typed() {
        let mut d = Discipline::new();
        assert_eq!(d.feed(0x1b), Step::None);
        assert_eq!(d.feed(b'['), Step::None);
        assert_eq!(d.feed(b'A'), Step::Key(Key::Up));
        assert_eq!(d.feed(0x1b), Step::None);
        assert_eq!(d.feed(b'['), Step::None);
        assert_eq!(d.feed(b'B'), Step::Key(Key::Down));
        match d.feed(b'\n') {
            Step::Line { bytes, .. } => assert!(bytes.is_empty(), "got {bytes:?}"),
            other => panic!("expected an empty line, got {other:?}"),
        }
    }

    /// An unrecognised sequence is consumed to its end rather than leaking its bytes into
    /// the line — the failure that would make a stray arrow key corrupt a command.
    #[test]
    fn an_unknown_escape_sequence_leaks_nothing() {
        let mut d = Discipline::new();
        for b in b"ab\x1b[Zcd\n" {
            if let Step::Line { bytes, .. } = d.feed(*b) {
                assert_eq!(bytes, b"abcd");
                return;
            }
        }
        panic!("no line produced");
    }

    /// A bare ESC is a key with no meaning here, and must not swallow what follows.
    #[test]
    fn a_bare_escape_consumes_exactly_two_bytes() {
        // Renamed 2026-08-12: it was `a_bare_escape_does_not_eat_the_next_character`, which
        // promises the opposite of what it asserts — the `x` *is* consumed. That is correct
        // (`ESC x` is how a terminal sends Alt-x, and this discipline does not act on it), but
        // a name that contradicts its assertion is worse than no name (PR #191 review).
        let mut d = Discipline::new();
        d.feed(0x1b);
        match d.feed(b'x') {
            // ESC-then-`x` is a two-byte sequence we do not recognise; `x` ends it.
            Step::None => {}
            other => panic!("expected the sequence to end, got {other:?}"),
        }
        assert!(d.line().is_empty());
        // And the byte after those two is ordinary text again.
        d.feed(b'y');
        assert_eq!(d.line(), b"y");
    }

    #[test]
    fn a_parameterised_sequence_leaves_nothing_behind() {
        // **The bug this state machine shipped with.** `ESC [ 3 ~` is Delete on every real
        // terminal; ending the sequence at the `3` left the `~` to be typed and echoed.
        for seq in [
            &b"\x1b[3~"[..], // Delete
            &b"\x1b[2~"[..], // Insert
            &b"\x1b[5~"[..], // PageUp
            &b"\x1b[6~"[..], // PageDown
            &b"\x1b[H"[..],  // Home
            &b"\x1b[F"[..],  // End
            &b"\x1b[1;5D"[..], // a modified cursor key: two parameters and a separator
        ] {
            let mut d = Discipline::new();
            for b in seq {
                d.feed(*b);
            }
            for b in b"hi" {
                d.feed(*b);
            }
            assert_eq!(d.line(), b"hi", "{seq:x?} leaked into the line");
        }
    }

    #[test]
    fn the_arrows_still_arrive_after_the_widening() {
        // The sequences this discipline *does* act on must survive a change made for the ones
        // it does not.
        for (seq, key) in [(&b"\x1b[A"[..], Key::Up), (&b"\x1b[B"[..], Key::Down)] {
            let mut d = Discipline::new();
            let mut got = None;
            for b in seq {
                if let Step::Key(k) = d.feed(*b) {
                    got = Some(k);
                }
            }
            assert_eq!(got, Some(key), "{seq:x?}");
        }
    }

    #[test]
    fn an_escape_inside_a_sequence_starts_a_new_one() {
        // A program interrupted mid-sequence emits a fresh one, and the new `ESC` must not be
        // read as the old sequence's payload. Added with the widening above — and it had no
        // test until a break-test said so, which is the same gap `libterm::parse` had at the
        // same place.
        let mut d = Discipline::new();
        let mut got = None;
        for b in b"\x1b[1\x1b[Ahi" {
            if let Step::Key(k) = d.feed(*b) {
                got = Some(k);
            }
        }
        assert_eq!(got, Some(Key::Up), "the second sequence was eaten by the first");
        assert_eq!(d.line(), b"hi");
    }

    #[test]
    fn a_malformed_csi_does_not_swallow_the_stream() {
        // Staying in the CSI state on a byte that can appear in no sequence would make the
        // terminal go silent from one corrupt byte onward.
        let mut d = Discipline::new();
        for b in b"\x1b[1\x7fZhi" {
            d.feed(*b);
        }
        // `0x7f` can appear in no CSI, so the sequence is abandoned there — and `Z` is then
        // ordinary text rather than the tail of something. That is the same answer
        // `libterm::parse` gives, and the alternative is a terminal that goes silent from one
        // corrupt byte onward.
        assert_eq!(d.line(), b"Zhi");
    }

    /// The redraw primitive: erase exactly what is displayed, then draw the new line.
    #[test]
    fn replacing_a_line_erases_what_was_shown() {
        let mut d = Discipline::new();
        drive(&mut d, "abc");
        let out = d.replace_line(b"xy");
        assert_eq!(out, b"\x08 \x08\x08 \x08\x08 \x08xy");
        assert_eq!(d.line(), b"xy");
    }

    /// With nothing echoed there is nothing to erase, and nothing to draw.
    #[test]
    fn replacing_a_hidden_line_writes_nothing() {
        let mut d = Discipline::new();
        d.set_echo(false);
        drive(&mut d, "secret");
        assert_eq!(d.replace_line(b"x"), b"");
        assert_eq!(d.line(), b"x", "the buffer still changed");
    }

    /// A recalled line then behaves as if typed.
    #[test]
    fn a_replaced_line_can_be_edited_and_submitted() {
        let mut d = Discipline::new();
        d.replace_line(b"list /");
        d.feed(0x08); // backspace
        match d.feed(b'\n') {
            Step::Line { bytes, .. } => assert_eq!(bytes, b"list "),
            other => panic!("expected a line, got {other:?}"),
        }
    }

    #[test]
    fn undefined_control_bytes_are_dropped() {
        let mut d = Discipline::new();
        assert_eq!(drive(&mut d, "a\x01\x02b\n").0, ["ab"]);
    }

    /// A tty handed to a new reader must not carry half of somebody else's input.
    #[test]
    fn reset_discards_a_partial_line() {
        let mut d = Discipline::new();
        drive(&mut d, "partial");
        d.reset();
        assert_eq!(drive(&mut d, "fresh\n").0, ["fresh"]);
    }
}
