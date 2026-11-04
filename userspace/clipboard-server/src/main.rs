//! `clipboard-server` — the kill ring, served at `/dev/clipboard`.
//!
//! `service-mgr` spawns it with a control channel and binds the forwarding endpoint it answers
//! `Meta::Ready` with; it holds no `BIND_NAMESPACE`
//! (`docs/rationale/why-supervisor-registration.md`). A client that resolves `/dev/clipboard`
//! gets a session channel of its own and speaks the `Clipboard` category on it.
//!
//! A clipboard is shared mutable state between mutually untrusting programs, so "anything
//! running may read it" would be ambient authority. Here the binding is the authority: a
//! namespace without `/dev/clipboard` has no clipboard, and an endpoint attenuated to
//! `RIGHT_SEND` can copy and not read. It stores rather than brokers because the clipboard
//! would otherwise die with the application copied from.
//!
//! The ring itself is [`clipboard_server::Ring`], host-tested; this file is the plumbing.
//!
//! **It never logs an entry's bytes**: a clipboard eventually holds a password.

#![no_std]
#![no_main]

use clipboard_server::{Ring, RingError};
use libkern::{CTRL_OP_SHUTDOWN, KError, MAX_WAIT_HANDLES};
use libos::boot::Start;
use libos::control::{self, Control};
use libos::log::{self, Line};
use libos::{Carried, Channel, Deadline, Handle, Msg, Only, WaitSet};
use librsproto::clipboard::{
    CLIP_ENTRY_HEAD, CLIP_ERR_MALFORMED, CLIP_ERR_STALE, CLIP_INFO_LEN, CLIP_LIST_HEAD, CLIP_RING, ClipEntry, ClipInfo,
    ClipPaste, MAX_CLIP_BYTES, OP_CLIP_COPY, OP_CLIP_LIST, OP_CLIP_PASTE, write_list,
};
use librsproto::error::{ERROR_BODY_LEN, error_body};
use librsproto::namespace::parse_resolve_request;
use librsproto::server;
use librsproto::{OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode};

/// How many clients may hold a session: generous, as every application namespace binds this
/// endpoint. The wait set's other two are the endpoint and the control channel.
const MAX_SESSIONS: usize = MAX_WAIT_HANDLES - 2;
const _: () = assert!(2 + MAX_SESSIONS <= MAX_WAIT_HANDLES);

type Endpoint = Handle<Channel, Only>;

/// What a wait's index names.
#[derive(Copy, Clone)]
enum Waited {
    Resolve,
    Control,
    Session(usize),
}

struct Server {
    ring: Ring,
    serve_end: Endpoint,
    /// The control channel, for `service --stop`; `None` once its supervisor has gone, when it
    /// would stay signalled for good.
    control: Option<Endpoint>,
    /// Open sessions, one per client that resolved `/dev/clipboard`.
    sessions: [Option<Endpoint>; MAX_SESSIONS],
}

/// Add `h` to `set` as `what`.
fn watch<'a>(set: &mut WaitSet<'a, MAX_WAIT_HANDLES>, names: &mut [Waited], h: &'a Endpoint, what: Waited) {
    if let Ok(i) = set.add(h) {
        names[i] = what;
    }
}

fn send_reply(ch: &Endpoint, op: u16, request_id: u64, body: &[u8]) {
    let _ = server::send(ch, op, request_id, RS_FLAG_REPLY, body, &mut Carried::new());
}

/// Refuse a request with a `KError` **and** a `server_code`: a stale cycle and a malformed body
/// share `InvalidArgument` and mean opposite things (`docs/reference/error-codes.md`).
fn refuse(ch: &Endpoint, op: u16, request_id: u64, kerror: KError, server_code: u32) {
    let mut body = [0u8; ERROR_BODY_LEN];
    let Some(n) = error_body(&mut body, kerror.as_i32(), server_code, b"") else { return };
    let _ = server::send(ch, op, request_id, RS_FLAG_REPLY | RS_FLAG_ERROR, &body[..n], &mut Carried::new());
}

impl Server {
    fn serve_resolve(&mut self) {
        let mut msg = Msg::new();
        let Ok(_sent) = self.serve_end.recv(&mut msg) else {
            return;
        };
        let (op, request_id, bare) = match decode(msg.payload()) {
            // An empty suffix only: `/dev/clipboard` has no second level to resolve into.
            Ok(m) if m.op == OP_NS_RESOLVE => match parse_resolve_request(m.body) {
                Some(r) if r.suffix.is_empty() => (m.op, m.request_id, true),
                _ => (m.op, m.request_id, false),
            },
            Ok(m) => (m.op, m.request_id, false),
            Err(_) => return,
        };
        let serve_end = &self.serve_end;
        if !bare {
            refuse(serve_end, op, request_id, KError::NotFound, 0);
            return;
        }
        let Some(slot) = self.sessions.iter().position(Option::is_none) else {
            refuse(serve_end, op, request_id, KError::WouldBlock, 0);
            return;
        };
        let Ok((client_end, session_end)) = Handle::pair(4) else {
            refuse(serve_end, op, request_id, KError::KernelError, 0);
            return;
        };
        // Bound before replying, so a fast client's first request cannot arrive before the slot is
        // live; a reply that did not go is answered with an error, and the slot freed.
        self.sessions[slot] = Some(session_end);
        if !server::reply_channel(serve_end, request_id, client_end) {
            self.sessions[slot] = None;
        }
    }

    fn serve_session(&mut self, slot: usize) {
        let Some(ch) = &self.sessions[slot] else { return };
        let mut msg = Msg::new();
        let Ok(_sent) = ch.recv(&mut msg) else {
            // A closed peer frees the slot; otherwise every exited application holds one forever.
            self.sessions[slot] = None;
            return;
        };
        let Ok(m) = decode(msg.payload()) else { return };
        match m.op {
            OP_CLIP_COPY => serve_copy(&mut self.ring, ch, m.request_id, m.body),
            OP_CLIP_PASTE => serve_paste(&self.ring, ch, m.request_id, m.body),
            OP_CLIP_LIST => serve_list(&self.ring, ch, m.request_id),
            // `Unsupported`, not `InvalidArgument`: well-formed, just not for this resource.
            op => refuse(ch, op, m.request_id, KError::Unsupported, 0),
        }
    }

    fn serve_loop(&mut self) -> ! {
        log::write(b"clipboard-server: serving Clipboard ops over /dev/clipboard\n");
        loop {
            let mut names = [Waited::Resolve; MAX_WAIT_HANDLES];
            let signalled = {
                let mut set = WaitSet::new();
                watch(&mut set, &mut names, &self.serve_end, Waited::Resolve);
                if let Some(c) = &self.control {
                    watch(&mut set, &mut names, c, Waited::Control);
                }
                for (slot, s) in self.sessions.iter().enumerate() {
                    if let Some(s) = s {
                        watch(&mut set, &mut names, s, Waited::Session(slot));
                    }
                }
                set.wait(Deadline::NEVER)
            };
            let Ok(signalled) = signalled else { continue };
            for (i, _) in signalled.iter() {
                match names[i] {
                    Waited::Resolve => self.serve_resolve(),
                    Waited::Control => {
                        if control::recv(&mut self.control) == Control::Op(CTRL_OP_SHUTDOWN) {
                            log::write(b"clipboard-server: asked to stop, exiting\n");
                            libos::exit(0);
                        }
                    }
                    Waited::Session(slot) => self.serve_session(slot),
                }
            }
        }
    }
}

fn serve_copy(ring: &mut Ring, ch: &Endpoint, request_id: u64, body: &[u8]) {
    let Some(entry) = ClipEntry::read(body) else {
        refuse(ch, OP_CLIP_COPY, request_id, KError::InvalidArgument, CLIP_ERR_MALFORMED);
        return;
    };
    match ring.push(entry.kind, entry.bytes) {
        Ok(serial) => {
            send_reply(ch, OP_CLIP_COPY, request_id, &serial.to_le_bytes());
            Line::new()
                .s(b"clipboard-server: copied ")
                .i(entry.bytes.len() as i64)
                .s(b" bytes, serial ")
                .i(serial as i64)
                .end();
        }
        Err(RingError::TooLarge) => refuse(ch, OP_CLIP_COPY, request_id, KError::TooLarge, 0),
        // An arm rather than `unreachable!()`: a panic takes the clipboard down for every client.
        Err(_) => refuse(ch, OP_CLIP_COPY, request_id, KError::KernelError, 0),
    }
}

fn serve_paste(ring: &Ring, ch: &Endpoint, request_id: u64, body: &[u8]) {
    let Some(req) = ClipPaste::read(body) else {
        refuse(ch, OP_CLIP_PASTE, request_id, KError::InvalidArgument, CLIP_ERR_MALFORMED);
        return;
    };
    match ring.get(req.index as usize, req.expect) {
        Ok((kind, bytes)) => {
            let entry = ClipEntry { serial: ring.serial(), kind, bytes };
            // Sized for an entry at the cap, which is the largest this can be.
            let mut out = [0u8; CLIP_ENTRY_HEAD + MAX_CLIP_BYTES];
            let Some(n) = entry.write(&mut out) else {
                refuse(ch, OP_CLIP_PASTE, request_id, KError::KernelError, 0);
                return;
            };
            send_reply(ch, OP_CLIP_PASTE, request_id, &out[..n]);
        }
        Err(RingError::Stale) => refuse(ch, OP_CLIP_PASTE, request_id, KError::InvalidArgument, CLIP_ERR_STALE),
        Err(_) => refuse(ch, OP_CLIP_PASTE, request_id, KError::NotFound, 0),
    }
}

fn serve_list(ring: &Ring, ch: &Endpoint, request_id: u64) {
    let mut rows = [ClipInfo::default(); CLIP_RING];
    let n = ring.list(&mut rows);
    let mut body = [0u8; CLIP_LIST_HEAD + CLIP_RING * CLIP_INFO_LEN];
    let Some(len) = write_list(&mut body, ring.serial(), &rows[..n]) else {
        refuse(ch, OP_CLIP_LIST, request_id, KError::KernelError, 0);
        return;
    };
    send_reply(ch, OP_CLIP_LIST, request_id, &body[..len]);
}

/// Bootstrap registers: `rdx` is the control channel its supervisor installed; the rest are unused.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, control: u64, arg0: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(notif, root_ns, control, arg0) };
    log::write(b"clipboard-server: up\n");
    let control: Option<Endpoint> = start.take_handle0();
    let Ok((client_end, serve_end)) = Handle::pair(4) else {
        log::write(b"clipboard-server: channel create FAIL\n");
        libos::exit(1);
    };
    if !control.as_ref().is_some_and(|c| server::ready(c, b"clipboard-server", client_end)) {
        log::write(b"clipboard-server: Ready send FAIL\n");
        libos::exit(1);
    }
    let mut srv = Server { ring: Ring::new(), serve_end, control, sessions: [const { None }; MAX_SESSIONS] };
    srv.serve_loop();
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    log::write(b"clipboard-server: PANIC\n");
    libos::exit(1);
}
