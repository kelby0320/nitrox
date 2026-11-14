//! `tty-server` — the userspace terminal server.
//!
//! Bound at `/dev/tty` by `service-mgr`. A client resolving it gets a **fresh per-caller
//! channel** (an `OBJECT_KIND_CHANNEL` resolve reply), and then speaks `Tty::ReadLine` /
//! `Write` / `SetMode` / `Close` on it.
//!
//! Console output used to go through `SYS_DEBUG_KPRINT`, an ambient syscall taking no handle, so
//! a process with an empty namespace could still write to the console and nothing could redirect
//! a shell's output. This server holds the raw device **exclusively** and a session gets a
//! channel; `SYS_DEBUG_KPRINT` is left as a kernel debug facility, used here as the write backend
//! and by `eshell`, which runs when this server does not exist.
//!
//! See `docs/architecture/console-and-tty.md`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec;

use libkern::{CTRL_OP_SHUTDOWN, IO_OPCODE_READ, IoOp, KError, MAX_WAIT_HANDLES, Rights};
use libos::boot::Start;
use libos::control::{self, Control};
use libos::log::{self, Line};
use libos::{
    Carried, Channel, Deadline, Handle, MapReadWrite, Mapping, Memory, Msg, Only, Op, ReadOnly, Resource, WaitSet,
    block_on, entropy, time,
};
use librsproto::namespace::parse_resolve_request;
use librsproto::server;
use librsproto::{
    OP_NS_RESOLVE, OP_TTY_ATTACH_BACKEND, OP_TTY_CLOSE, OP_TTY_INPUT, OP_TTY_OPEN_SIBLING, OP_TTY_OUTPUT, OP_TTY_READ,
    OP_TTY_READ_LINE, OP_TTY_REDEEM, OP_TTY_SET_MODE, OP_TTY_TOKEN, OP_TTY_WRITE, RS_FLAG_REPLY, TTY_MODE_ECHO, decode,
    encode,
};
use tty_server::routing::{Act, CONSOLE, ReadKind, Registry, Sink, backend_refusal};
use tty_server::tokens::Tokens;

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// One `sys_wait` slot goes to the serving endpoint, one to the outstanding console read and one
/// to the control channel. **The rest is halved**, because a terminal can also own a backend
/// channel that has to be waited on: worst case one per terminal, when every one is a window's.
const MAX_TTYS: usize = (MAX_WAIT_HANDLES - 3) / 2;
const _: () = assert!(3 + 2 * MAX_TTYS <= MAX_WAIT_HANDLES);
/// Bytes per console read submission.
const READ_CHUNK: u64 = 64;

type Endpoint = Handle<Channel, Only>;

/// The channels the server owns, keyed by the value the [`Registry`] names them with. A channel
/// leaves a map exactly when the registry drops it, and is closed there.
struct Links {
    /// Each terminal's server-side endpoint.
    ttys: BTreeMap<u64, Endpoint>,
    /// Each channel backend: the endpoint an emulator handed over.
    backends: BTreeMap<u64, Endpoint>,
}

/// What a wait's index names.
#[derive(Copy, Clone)]
enum Waited {
    Resolve,
    Control,
    Tty(u64),
    Backend(u64, u32),
    Read,
}

/// Add `h` to `set` as `what`.
fn watch<'a>(set: &mut WaitSet<'a, MAX_WAIT_HANDLES>, names: &mut [Waited], h: &'a Endpoint, what: Waited) {
    if let Ok(i) = set.add(h) {
        names[i] = what;
    }
}

/// The most body one message can carry ([`librsproto::RS_MAX_BODY`]), envelope included: a
/// larger chunk makes a message the kernel refuses, and `send_output` drops a failed send.
const MAX_BODY: usize = librsproto::RS_MAX_BODY;

/// Send a reply on `ch`: `op`/`request_id` echoed, `body` as the payload.
fn reply(ch: &Endpoint, op: u16, request_id: u64, body: &[u8]) {
    let _ = server::send(ch, op, request_id, RS_FLAG_REPLY, body, &mut Carried::new());
}

/// Send an error reply, echoing `op`/`request_id` so the caller can match it.
fn reply_error(ch: &Endpoint, op: u16, request_id: u64, kerror: KError) {
    server::reply_error(ch, op, request_id, kerror, b"");
}

/// Send one `Tty::Output` message, **blocking if the emulator's ring is full**.
///
/// Every other send here is `NOBLOCK`: a reply goes to a client waiting for exactly it, so the
/// ring is empty. Output differs — a `NOBLOCK` send onto a full ring (the emulator is off
/// drawing) **silently discards a program's output**.
///
/// Blocking means a wedged emulator stalls **this whole server**, login terminal and serial
/// console shell included. It is a stall and not a deadlock: the compositor depends on nothing in
/// the tty path, and the emulator's send back is `NOBLOCK`. A *visible* stall beats an *invisible*
/// loss; a per-backend output queue the serve loop drains would cost neither, which is
/// `TODO(tty-output-queue)`.
fn send_output(ch: &Endpoint, body: &[u8]) {
    let mut msg = Msg::new();
    let Some(rs_len) = encode(msg.payload_mut(), OP_TTY_OUTPUT, 0, 0, body, 0) else {
        return;
    };
    if msg.set_len(rs_len).is_err() {
        return;
    }
    if let Ok(held) = ch.send_held(&msg, &mut Carried::new()) {
        // Waiting the delivery's `PendingOperation` is what blocks. Its status is ignored: a
        // failure means the emulator is gone, which the serve loop learns from `PeerClosed`.
        let _ = block_on(held);
    }
}

impl Links {
    /// Write to a sink: serial via the kernel's debug write, or an `OP_TTY_OUTPUT` message to
    /// whoever holds the terminal's backend channel.
    fn sink_write(&self, sink: Sink, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        match sink {
            Sink::Console => log::write(bytes),
            // Chunked: a write may exceed one message and the emulator cannot ask for the rest.
            Sink::Channel(h) => {
                if let Some(ch) = self.backends.get(&h) {
                    for part in bytes.chunks(MAX_BODY) {
                        send_output(ch, part);
                    }
                }
            }
        }
    }

    /// Perform what the registry decided.
    fn perform(&self, acts: &[Act]) {
        for a in acts {
            match a {
                Act::Write(sink, bytes) => self.sink_write(*sink, bytes),
                Act::Reply { ch, op, request_id, body } => {
                    if let Some(ch) = self.ttys.get(ch) {
                        reply(ch, *op, *request_id, body);
                    }
                }
                Act::Fail { ch, op, request_id, err } => {
                    if let Some(ch) = self.ttys.get(ch) {
                        reply_error(ch, *op, *request_id, KError::from_i32(*err));
                    }
                }
            }
        }
    }

    /// Answer terminal `ch` with an error, if it still exists.
    fn fail(&self, ch: u64, op: u16, request_id: u64, kerror: KError) {
        if let Some(ch) = self.ttys.get(&ch) {
            reply_error(ch, op, request_id, kerror);
        }
    }

    /// Answer terminal `ch`, if it still exists.
    fn answer(&self, ch: u64, op: u16, request_id: u64, body: &[u8]) {
        if let Some(ch) = self.ttys.get(&ch) {
            reply(ch, op, request_id, body);
        }
    }
}

/// The monotonic clock, in nanoseconds: what a token's thirty seconds are measured on.
fn now_ns() -> u64 {
    time::now().unwrap_or(0)
}

/// A token's 128 bits, from the kernel CSPRNG — **or none**: a token made up when the pool will
/// not answer is one someone else could make up too, and without one the caller is asked for its
/// password, which is safe. An unseeded pool is none too: the serve loop does not wait for it, and
/// a missing token only costs a password prompt.
fn random_token(source: Option<&entropy::Source>) -> Option<[u8; tty_server::tokens::LEN]> {
    let mut buf = [0u8; tty_server::tokens::LEN];
    source?.try_fill(&mut buf).ok()?;
    Some(buf)
}

/// What the serve loop works on: the registry and the channels it names, the console and the
/// outstanding read.
struct Server {
    /// The outstanding console read, if one is submitted; ahead of `map` and `buf` so it drops
    /// first, as an `Op` dropped before it completes waits for its device.
    read: Option<Op>,
    reg: Registry,
    tokens: Tokens,
    links: Links,
    serve_end: Endpoint,
    /// The control channel (`service --stop`), until its supervisor has gone and it would stay
    /// signalled for good.
    control: Option<Endpoint>,
    console: Handle<Resource, ReadOnly>,
    /// The page every console read lands in, and its mapping.
    map: Mapping<MapReadWrite>,
    buf: Handle<Memory, MapReadWrite>,
    /// Where tokens' bits come from: `/dev/entropy`, if the root binds it.
    entropy: Option<entropy::Source>,
}

impl Server {
    /// `Tty::OpenSibling` on terminal `of`: mint another terminal on its backend and hand the
    /// caller the new channel, moved in the reply.
    ///
    /// A shell's stages get their terminals here: a stage's prompt must appear where the shell's does,
    /// and resolving `/dev/tty` would land it on the console. The cap is a resolve's, and a refusal
    /// is `WouldBlock`, which the shell answers by running the stage with no terminal.
    fn open_sibling(&mut self, of: u64, request_id: u64) {
        if self.reg.len() >= MAX_TTYS {
            self.links.fail(of, OP_TTY_OPEN_SIBLING, request_id, KError::WouldBlock);
            return;
        }
        let Ok((client_end, server_end)) = Handle::pair(4) else {
            self.links.fail(of, OP_TTY_OPEN_SIBLING, request_id, KError::KernelError);
            return;
        };
        let key = server_end.raw().0;
        // Registered before the reply so the terminal exists when first used; undone if it cannot go.
        if !self.reg.open_sibling(key, of) {
            self.links.fail(of, OP_TTY_OPEN_SIBLING, request_id, KError::NotFound);
            return;
        }
        let mut carried = Carried::new();
        let sent = match (carried.push(client_end), self.links.ttys.get(&of)) {
            (Ok(()), Some(of_end)) => {
                server::send(of_end, OP_TTY_OPEN_SIBLING, request_id, RS_FLAG_REPLY, &[], &mut carried)
            }
            _ => Err(KError::KernelError.into()),
        };
        match sent {
            Ok(()) => {
                self.links.ttys.insert(key, server_end);
                // With the count, so a leak is visible: a stage's terminal not freed would hold one of
                // `MAX_TTYS` slots for good. `test-interactive` compares it across identical commands.
                Line::new()
                    .s(b"tty-server: terminal opened beside another, ")
                    .u(self.reg.len() as u64)
                    .s(b" open")
                    .end();
            }
            Err(e) => {
                // The client end never moved, so nobody can use the terminal: take it back out, and
                // say so. A ring too full for the reply drops this too, and the shell keeps waiting.
                self.reg.close(key);
                drop(carried);
                self.links.fail(of, OP_TTY_OPEN_SIBLING, request_id, e.code());
            }
        }
    }

    /// Reply to a forwarded `/dev/tty` resolve with a fresh terminal channel.
    fn open_tty(&mut self, request_id: u64) {
        if self.reg.len() >= MAX_TTYS {
            reply_error(&self.serve_end, OP_NS_RESOLVE, request_id, KError::WouldBlock);
            return;
        }
        let Ok((client_end, server_end)) = Handle::pair(4) else {
            reply_error(&self.serve_end, OP_NS_RESOLVE, request_id, KError::KernelError);
            return;
        };
        let key = server_end.raw().0;
        // A reply that did not go is answered with an error, a forwarded resolve having no
        // deadline, and the client end is closed with it.
        if server::reply_channel(&self.serve_end, request_id, client_end) {
            self.reg.open(key);
            self.links.ttys.insert(key, server_end);
            log::write(b"tty-server: terminal opened\n");
        }
    }

    /// Free terminal `ch`: close its endpoint and drop it.
    ///
    /// **This is the revocation point.** This kernel has no revocation, so a process that outlived
    /// its session while holding a `/dev/tty` handle cannot have it taken away; the server ceasing to
    /// serve the channel makes teardown a guarantee — see `docs/architecture/console-and-tty.md`.
    fn free_tty(&mut self, ch: u64) {
        // The backend goes with the last terminal on it; closing a channel backend tells the
        // emulator the terminal is gone.
        for h in self.reg.close_and_retire(ch) {
            self.links.backends.remove(&h);
        }
        self.links.ttys.remove(&ch);
        log::write(b"tty-server: terminal closed\n");
    }

    /// Handle one request on terminal `ch`'s channel. Returns `false` if the terminal is gone.
    fn serve_tty(&mut self, ch: u64) -> bool {
        let Some(end) = self.links.ttys.get(&ch) else { return true };
        // Only `AttachBackend` takes a handle; whatever came with anything else closes as `sent` drops.
        let mut msg = Msg::new();
        let mut sent = match end.recv(&mut msg) {
            Ok(sent) => sent,
            // `PeerClosed` means the terminal is finished; anything else is an empty ring.
            Err(e) => return e.code() != KError::PeerClosed,
        };
        let Ok(m) = decode(msg.payload()) else { return true };
        let (op, request_id, body) = (m.op, m.request_id, m.body);
        match op {
            OP_TTY_WRITE => {
                let acts = self.reg.write(ch, request_id, body);
                self.links.perform(&acts);
            }
            OP_TTY_SET_MODE => {
                let flags = body.first().copied().unwrap_or(TTY_MODE_ECHO);
                let acts = self.reg.set_mode(ch, request_id, flags & TTY_MODE_ECHO != 0);
                self.links.perform(&acts);
            }
            OP_TTY_READ_LINE | OP_TTY_READ => {
                let kind = if op == OP_TTY_READ { ReadKind::Raw } else { ReadKind::Line };
                let acts = self.reg.read(ch, request_id, kind);
                self.links.perform(&acts);
            }
            OP_TTY_ATTACH_BACKEND => {
                let handle: Option<Endpoint> = sent.take(0);
                // Succeeding silently with no handle would leave the emulator waiting for output that
                // goes to the console.
                let refused = match &handle {
                    None => Some(KError::InvalidArgument),
                    Some(h) => match h.stat() {
                        Ok(info) => backend_refusal(info.object_type, info.rights),
                        // Without `INSPECT` the server cannot see what it was given, so cannot take it.
                        Err(e) if e.code() == KError::NoAccess => Some(KError::NoAccess),
                        Err(_) => Some(KError::InvalidArgument),
                    },
                };
                match (refused, handle) {
                    (None, Some(h)) => {
                        let key = h.raw().0;
                        if let Some((_, left)) = self.reg.attach_backend(ch, key) {
                            self.links.backends.insert(key, h);
                            // Closed, so the emulator of the backend the terminal left sees it end.
                            if let Some(l) = left {
                                self.links.backends.remove(&l);
                            }
                            self.links.answer(ch, OP_TTY_ATTACH_BACKEND, request_id, &[]);
                            log::write(b"tty-server: terminal attached to a backend channel\n");
                        } else {
                            drop(h);
                            self.links.fail(ch, op, request_id, KError::NotFound);
                        }
                    }
                    (refused, handle) => {
                        drop(handle);
                        self.links.fail(ch, op, request_id, refused.unwrap_or(KError::InvalidArgument));
                    }
                }
            }
            OP_TTY_OPEN_SIBLING => self.open_sibling(ch, request_id),
            // A token for this terminal's backend: how the view broker learns which terminal a
            // request came from without asking the requester.
            OP_TTY_TOKEN => match (self.reg.backend_of_terminal(ch), random_token(self.entropy.as_ref())) {
                (Some(backend), Some(token)) => {
                    self.tokens.mint(token, backend, now_ns());
                    self.links.answer(ch, op, request_id, &token);
                }
                (None, _) => self.links.fail(ch, op, request_id, KError::NotFound),
                (_, None) => self.links.fail(ch, op, request_id, KError::KernelError),
            },
            // Which backend a token names, once. On any terminal: knowing the token is the proof.
            OP_TTY_REDEEM => match self.tokens.redeem(body, now_ns()) {
                Some(backend) => self.links.answer(ch, op, request_id, &backend.to_le_bytes()),
                None => self.links.fail(ch, op, request_id, KError::NotFound),
            },
            OP_TTY_CLOSE => {
                self.links.answer(ch, OP_TTY_CLOSE, request_id, &[]);
                return false;
            }
            _ => self.links.fail(ch, op, request_id, KError::Unsupported),
        }
        true
    }

    /// Feed in what a terminal emulator sent on backend `id`'s channel `key`. Returns `false` if
    /// the emulator is gone, which ends every terminal on that backend.
    fn serve_backend(&mut self, id: u32, key: u64) -> bool {
        let Some(end) = self.links.backends.get(&key) else { return true };
        let mut msg = Msg::new();
        let _sent = match end.recv(&mut msg) {
            Ok(sent) => sent,
            Err(e) => return e.code() != KError::PeerClosed,
        };
        let Ok(m) = decode(msg.payload()) else { return true };
        // Anything but input is ignored: this channel carries unsolicited messages, so there is no
        // request to fail.
        if m.op == OP_TTY_INPUT {
            let acts = self.reg.feed(id, m.body);
            self.links.perform(&acts);
        }
        true
    }

    /// Drain every queued forwarded resolve on the serving endpoint.
    fn drain_resolves(&mut self) {
        loop {
            let mut msg = Msg::new();
            let _sent = match self.serve_end.recv(&mut msg) {
                Ok(sent) => sent,
                // A forwarding endpoint with no peer is the end of this server, not a message to skip: the
                // kernel keeps a peer-closed channel signalled, so going round again would spin a CPU.
                Err(e) if e.code() == KError::PeerClosed => {
                    log::write(b"tty-server: forwarding endpoint closed\n");
                    libos::exit(1);
                }
                Err(_) => return,
            };
            let (op, request_id, ok) = match decode(msg.payload()) {
                // Only the terminal itself is addressable, not `/dev/tty/anything`.
                Ok(m) if m.op == OP_NS_RESOLVE => match parse_resolve_request(m.body) {
                    Some(r) => (m.op, m.request_id, r.suffix.is_empty()),
                    None => (m.op, m.request_id, false),
                },
                Ok(m) => (m.op, m.request_id, false),
                Err(_) => (0, 0, false),
            };
            if ok {
                self.open_tty(request_id);
            } else if op == OP_NS_RESOLVE {
                reply_error(&self.serve_end, op, request_id, KError::NotFound);
            } else {
                reply_error(&self.serve_end, op, request_id, KError::Unsupported);
            }
        }
    }

    /// Submit the console read if none is outstanding.
    ///
    /// A console read is always outstanding: `Ctrl-C` has to be seen **while the shell is
    /// busy**, when nobody is waiting for input. This server is the only reader of
    /// `/dev/console` in a live session, so the single-reader driver is not contended.
    /// Bytes that arrive with no reader still queue.
    fn submit_read(&mut self) {
        if self.read.is_none() {
            let op = IoOp {
                opcode: IO_OPCODE_READ,
                flags: 0,
                buffer: self.buf.raw().0,
                buf_offset: 0,
                offset: 0,
                length: READ_CHUNK,
            };
            self.read = Op::submit(self.console.raw(), &op).ok();
        }
    }

    /// The serve loop: the forwarding endpoint, every open terminal, and one outstanding
    /// console read, all in one `sys_wait`.
    fn serve_loop(&mut self) -> ! {
        log::write(b"tty-server: serving /dev/tty over the console\n");
        loop {
            self.submit_read();

            // Rebuilt each turn beside the wait set, so a handle's slot and its meaning cannot drift.
            let mut names = [Waited::Resolve; MAX_WAIT_HANDLES];
            let signalled = {
                let mut set = WaitSet::new();
                watch(&mut set, &mut names, &self.serve_end, Waited::Resolve);
                if let Some(c) = &self.control {
                    watch(&mut set, &mut names, c, Waited::Control);
                }
                for k in self.reg.channels() {
                    if let Some(h) = self.links.ttys.get(&k) {
                        watch(&mut set, &mut names, h, Waited::Tty(k));
                    }
                }
                for k in self.reg.backend_channels() {
                    if let (Some(h), Some(id)) = (self.links.backends.get(&k), self.reg.backend_of_channel(k)) {
                        watch(&mut set, &mut names, h, Waited::Backend(k, id));
                    }
                }
                if let Some(read) = &self.read
                    && let Ok(i) = set.add_op(read)
                {
                    names[i] = Waited::Read;
                }
                set.wait(Deadline::NEVER)
            };
            let Ok(signalled) = signalled else { continue };

            for (i, _) in signalled.iter() {
                match names[i] {
                    Waited::Resolve => self.drain_resolves(),
                    Waited::Control => {
                        // Every terminal ends with this process; its programs' next read or write
                        // finds the channel closed.
                        if control::recv(&mut self.control) == Control::Op(CTRL_OP_SHUTDOWN) {
                            log::write(b"tty-server: asked to stop, exiting\n");
                            libos::exit(0);
                        }
                    }
                    Waited::Read => {
                        let Some(done) = self.read.as_mut().and_then(|op| op.completion_in(&signalled)) else {
                            continue;
                        };
                        // Consumed; the next iteration submits another.
                        self.read = None;
                        let n = done.map_or(0, |r| r.result.min(READ_CHUNK)) as usize;
                        let mut bytes = vec![0u8; n];
                        let _ = self.map.read_into(0, &mut bytes);
                        let acts = self.reg.feed(CONSOLE, &bytes);
                        self.links.perform(&acts);
                    }
                    Waited::Tty(k) => {
                        if self.links.ttys.contains_key(&k) && !self.serve_tty(k) {
                            self.free_tty(k);
                        }
                    }
                    Waited::Backend(k, id) => {
                        if self.links.backends.contains_key(&k) && !self.serve_backend(id, k) {
                            // The emulator is gone, so every terminal on its backend ends: leaving one alive
                            // would give its programs a `/dev/tty` that silently discards everything
                            // written to it, which looks like a hang.
                            log::write(b"tty-server: a backend went away\n");
                            for ch in self.reg.ttys_on(id) {
                                self.free_tty(ch);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Bootstrap registers: `rdi` = notification channel (unused), `rsi` = the inherited root
/// namespace (resolves `/dev/console`), `rdx` = its supervisor's control endpoint, `rcx` unused.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, control: u64, arg0: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(notif, root_ns, control, arg0) };
    log::write(b"tty-server: up\n");
    let control: Option<Endpoint> = start.take_handle0();

    // The raw device is held **exclusively** from here on: a session gets `/dev/tty` and not
    // `/dev/console`, so it cannot reach the device at all.
    let console = start
        .root
        .as_ref()
        .and_then(|root| block_on(root.lookup::<Resource, ReadOnly>("/dev/console", Rights::READ)).ok());
    let Some(console) = console else {
        log::write(b"tty-server: /dev/console not found\n");
        libos::exit(1);
    };
    let entropy = start.root.as_ref().and_then(|root| entropy::Source::open(root).ok());

    // One page, mapped once, for every console read.
    let Ok(buf) = Handle::<Memory, MapReadWrite>::create(4096) else {
        log::write(b"tty-server: read buffer FAIL\n");
        libos::exit(1);
    };
    let Ok(map) = buf.map(4096) else {
        log::write(b"tty-server: read buffer map FAIL\n");
        libos::exit(1);
    };

    let Ok((kernel_end, serve_end)) = Handle::pair(4) else {
        log::write(b"tty-server: channel create FAIL\n");
        libos::exit(1);
    };
    if !control.as_ref().is_some_and(|c| server::ready(c, b"tty-server", kernel_end)) {
        log::write(b"tty-server: Ready send FAIL\n");
        libos::exit(1);
    }
    let mut srv = Server {
        read: None,
        reg: Registry::new(),
        tokens: Tokens::new(),
        links: Links { ttys: BTreeMap::new(), backends: BTreeMap::new() },
        serve_end,
        control,
        console,
        map,
        buf,
        entropy,
    };
    srv.serve_loop();
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    log::write(b"tty-server: PANIC\n");
    libos::exit(1);
}
