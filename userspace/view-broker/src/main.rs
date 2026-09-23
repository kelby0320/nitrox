//! `view-broker` — run a program in a **view**: its caller's namespace plus a profile's grants,
//! when `/system/views.toml` says the caller may (`docs/planning/administration.md` § Part A).
//!
//! **What it holds, and so what a bug here reaches.** It is spawned by `init` with
//! `BIND_NAMESPACE`, which it needs to bind grants into the views it builds, and it inherits the
//! root namespace, which is where the grants come from: every block device, for `disks`. It never
//! holds a session's ingredients — the caller hands it a copy of its own namespace
//! (`sys_ns_derive`), and it copies that again before binding anything, so the caller cannot keep a
//! handle to what it builds. It never touches a terminal: `with` reads the password.
//!
//! **Two kinds of channel off one forwarding endpoint**, which `init` binds at `/svc/views`: a
//! login supervisor resolves `/svc/views/session` for a channel to open and close sessions on,
//! and binds the same endpoint into each session at `/dev/views` with the base `/s/<session>`, so a
//! process there resolves a channel the broker already knows the session of. See
//! `librsproto::views` for the ops.
//!
//! **A resource server holding `BIND_NAMESPACE`** — the second after `desktop-shell`, and for the
//! same reason: it binds only into namespaces it creates, and never registers itself
//! (`graphical-session.md` §3, `userspace/CLAUDE.md`).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use libkern::debug::Line;
use libkern::*;
use librsproto::namespace::{OBJECT_KIND_CHANNEL, RESOLVE_REPLY_LEN, parse_resolve_request, resolve_reply};
use librsproto::views::*;
use librsproto::{OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};
use libstream::setup::{Streams, bootstrap_arg0, pipe, send_setup_full};
use libstream::wire::{ByteSource, Record, TypeTag, Value, read_value};
use view_broker::pacing::MAX_FAILURES;
use view_broker::policy::{self, Auth, Decision, Grant};
use view_broker::sessions::Sessions;
use view_broker::suffix::{self, Suffix};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;
/// Where the policy lives. Read for every request: it is small, and a stale copy is a bug.
const POLICY_PATH: &[u8] = b"/system/views.toml";

static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut RECV_HANDLES: [u64; 8] = [0; 8];
static mut RECV_COUNT: usize = 0;
static mut REPLY_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut REPLY_HANDLES: [u64; 8] = [0; 8];
static mut WAIT_HANDLES: [u64; MAX_WAIT_HANDLES] = [0; MAX_WAIT_HANDLES];
static mut WAIT_RESULTS: [u8; 24 * MAX_WAIT_HANDLES] = [0; 24 * MAX_WAIT_HANDLES];
static mut NOTIF: Notification = Notification::zeroed();
static mut CLOCK_BUF: u64 = 0;
static mut SPAWN: SpawnArgs = SpawnArgs {
    image: 0,
    handle_count: 2,
    move_mask: 0b11,
    arg0: 0,
    handles: [0; SPAWN_MAX_HANDLES],
    rights: [u64::MAX; SPAWN_MAX_HANDLES],
    namespace: 0,
    syscaps: 0,
};

fn now_ns() -> u64 {
    // SAFETY: CLOCK_BUF is a valid writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: on success the kernel wrote the ns count.
    unsafe { (&raw const CLOCK_BUF).read() }
}

fn close(h: u64) {
    if h != 0 {
        // SAFETY: closing a handle this process owns.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
    }
}

/// The handles a request brought: a copy of the caller's namespace, then whichever of its
/// streams and terminal it sent. Zero means absent.
#[derive(Default)]
struct Handles {
    ns: u64,
    stdin: u64,
    stdout: u64,
    stderr: u64,
    terminal: u64,
}

impl Handles {
    fn close(&mut self) {
        for h in [self.ns, self.stdin, self.stdout, self.stderr, self.terminal] {
            close(h);
        }
        *self = Handles::default();
    }
}

/// A request that has been allowed, waiting for its password or about to start.
struct Pending {
    view: String,
    program: String,
    args: Vec<String>,
    env: Vec<u8>,
    grants: Vec<Grant>,
    handles: Handles,
    failures: u8,
    /// A password that arrived while the session's delay was still running: when it may be
    /// checked, the `Password` request to answer, and the password.
    queued: Option<(u64, u64, Vec<u8>)>,
}

enum State {
    Idle,
    Password(Pending),
    Running { process: u64, life: u64, view_ns: u64, view: String, program: String },
    Done,
}

struct Client {
    ch: u64,
    session: u64,
    state: State,
}

struct Broker {
    root_ns: u64,
    notif: u64,
    serve_end: u64,
    auth_ch: u64,
    supervisors: Vec<u64>,
    clients: Vec<Client>,
    sessions: Sessions,
    /// Exit codes from `ChildExited`, in arrival order, and programs whose life channel closed
    /// before their code came. Paired first-to-first — see [`Broker::pair_exits`].
    codes: VecDeque<(i32, bool)>,
    exited: VecDeque<u64>,
    log: liblog::Logger,
}

/// Send `body` on `ch` as `op`, echoing `request_id`, with `flags`, moving `handles`.
fn send(ch: u64, op: u16, request_id: u64, flags: u32, body: &[u8], handles: &[u64]) -> bool {
    // SAFETY: REPLY_MSG/REPLY_HANDLES are valid buffers; single-threaded.
    unsafe {
        let Some(rs_len) =
            encode(&mut REPLY_MSG[PAYLOAD_OFF..], op, request_id, flags, body, handles.len() as u16)
        else {
            return false;
        };
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = handles.len() as u8;
        REPLY_HANDLES[..handles.len()].copy_from_slice(handles);
        syscall5(
            SYS_CHANNEL_SEND,
            ch,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            handles.len() as u64,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

fn reply_error(ch: u64, op: u16, request_id: u64, err: KError) {
    let _ = send(ch, op, request_id, RS_FLAG_REPLY | RS_FLAG_ERROR, &err.as_i32().to_le_bytes(), &[]);
}

fn reply_outcome(ch: u64, op: u16, request_id: u64, outcome: Outcome, reason: &str) {
    let mut body = [0u8; 512];
    let n = build_outcome(&mut body, outcome, reason.as_bytes())
        .or_else(|| build_outcome(&mut body, outcome, b"(reason too long)"))
        .unwrap_or(0);
    let _ = send(ch, op, request_id, RS_FLAG_REPLY, &body[..n], &[]);
}

/// Make a channel pair of `depth`. `(a, b)`.
fn make_channel(depth: u64) -> Option<(u64, u64)> {
    let (mut a, mut b) = (0u64, 0u64);
    // SAFETY: valid writable out-params.
    let r = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut a) as u64, (&raw mut b) as u64, depth, 0) };
    (r == 0).then_some((a, b))
}

/// Receive one message on `ch` into the static buffers. `Ok(None)` if nothing was queued,
/// `Err(())` if the peer has gone.
fn recv(ch: u64) -> Result<Option<(u16, u64, Vec<u8>)>, ()> {
    // SAFETY: valid recv out-params.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ch,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    if rr == KError::PeerClosed.as_i32() as i64 {
        return Err(());
    }
    if rr != 0 {
        return Ok(None);
    }
    // SAFETY: bounded read of the payload the kernel just wrote.
    let msg = unsafe {
        let len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        core::slice::from_raw_parts(
            ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
            len.min(MSG_LEN - PAYLOAD_OFF),
        )
    };
    match decode(msg) {
        Ok(m) => Ok(Some((m.op, m.request_id, m.body.to_vec()))),
        Err(_) => {
            // A message that does not decode may still have carried handles; they are ours now.
            for h in received() {
                close(h);
            }
            Ok(None)
        }
    }
}

/// The handles the last message moved to us.
fn received() -> Vec<u64> {
    // SAFETY: the kernel wrote RECV_COUNT handles into RECV_HANDLES.
    unsafe { RECV_HANDLES[..RECV_COUNT.min(8)].to_vec() }
}

fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> u64 {
    let (st, h) = libsession::ns_lookup(ns, path, rights);
    if st == 0 { h } else { 0 }
}

impl Broker {
    /// A record in the log — every request, failure and exit, and never a password.
    fn audit(&self, what: &str) {
        self.log.info(what);
    }

    fn principal(&self, session: u64) -> Option<String> {
        self.sessions.get(session).map(|s| s.principal.clone())
    }

    /// The forwarding endpoint: a supervisor wanting a channel, or a client in a session.
    fn serve_resolve(&mut self) {
        let Ok(Some((op, request_id, body))) = recv(self.serve_end) else {
            return;
        };
        for h in received() {
            close(h);
        }
        let asked = match parse_resolve_request(&body) {
            Some(r) if op == OP_NS_RESOLVE => suffix::parse(r.suffix),
            _ => Suffix::Unknown,
        };
        let session = match asked {
            Suffix::Supervisor => None,
            Suffix::Client(id) if self.sessions.get(id).is_some() => Some(id),
            _ => {
                reply_error(self.serve_end, OP_NS_RESOLVE, request_id, KError::NotFound);
                return;
            }
        };
        // Every channel is a slot in the one wait set this process has.
        if self.wait_count() >= MAX_WAIT_HANDLES {
            reply_error(self.serve_end, OP_NS_RESOLVE, request_id, KError::WouldBlock);
            return;
        }
        let Some((client_end, ours)) = make_channel(4) else {
            reply_error(self.serve_end, OP_NS_RESOLVE, request_id, KError::KernelError);
            return;
        };
        let mut body = [0u8; RESOLVE_REPLY_LEN];
        let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
        if !send(self.serve_end, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, &[client_end]) {
            close(client_end);
            close(ours);
            return;
        }
        match session {
            None => self.supervisors.push(ours),
            Some(id) => self.clients.push(Client { ch: ours, session: id, state: State::Idle }),
        }
    }

    /// A supervisor's channel: sessions opening and closing.
    fn serve_supervisor(&mut self, i: usize) {
        let ch = self.supervisors[i];
        let (op, request_id, body) = match recv(ch) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(()) => {
                close(ch);
                self.supervisors.remove(i);
                return;
            }
        };
        for h in received() {
            close(h);
        }
        match op {
            OP_VIEWS_OPEN_SESSION => {
                let principal = match core::str::from_utf8(&body) {
                    Ok(p) if !p.is_empty() && p.len() <= 64 => p,
                    _ => return reply_error(ch, op, request_id, KError::InvalidArgument),
                };
                let id = self.sessions.open(principal);
                let mut out = [0u8; 8];
                let n = build_session_id(&mut out, id).unwrap_or(0);
                let _ = send(ch, op, request_id, RS_FLAG_REPLY, &out[..n], &[]);
                Line::new().s(b"view-broker: session ").u(id).s(b" opened").end();
            }
            OP_VIEWS_CLOSE_SESSION => {
                let Some(id) = parse_session_id(&body) else {
                    return reply_error(ch, op, request_id, KError::InvalidArgument);
                };
                self.end_session(id);
                let _ = send(ch, op, request_id, RS_FLAG_REPLY, &[], &[]);
            }
            _ => reply_error(ch, op, request_id, KError::Unsupported),
        }
    }

    /// A session ended: ask what it started to stop, and take back the grants. Its clients stay
    /// until they go, but nothing more is heard under its base.
    ///
    /// **Asked, not forced** — `TODO(forcible-kill)`. `sys_process_terminate` is a request, so a
    /// program that ignores it keeps running, and keeps whatever it had already resolved: unbinding
    /// the grants stops only *new* lookups, since the kernel has no revocation.
    fn end_session(&mut self, id: u64) {
        if self.sessions.close(id).is_none() {
            return;
        }
        for c in self.clients.iter_mut().filter(|c| c.session == id) {
            match &mut c.state {
                State::Running { process, view_ns, .. } => {
                    // SAFETY: a Process handle this broker owns, with SIGNAL from spawn.
                    unsafe { syscall1(SYS_PROCESS_TERMINATE, *process) };
                    libsession::unbind_block_devices(*view_ns);
                }
                State::Password(p) => {
                    p.handles.close();
                    c.state = State::Done;
                }
                _ => {}
            }
        }
        Line::new().s(b"view-broker: session ").u(id).s(b" ended").end();
    }

    /// A client's channel: requests, passwords, stops, listings and checks.
    fn serve_client(&mut self, i: usize, now: u64) {
        let ch = self.clients[i].ch;
        let (op, request_id, body) = match recv(ch) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(()) => return self.client_gone(i),
        };
        let handles = received();
        let session = self.clients[i].session;
        let Some(principal) = self.principal(session) else {
            for h in handles {
                close(h);
            }
            return reply_outcome(ch, op, request_id, Outcome::Denied { retry: false }, "this session has ended");
        };
        match op {
            OP_VIEWS_REQUEST => self.request(i, request_id, &body, handles, &principal, now),
            OP_VIEWS_PASSWORD => {
                for h in handles {
                    close(h);
                }
                self.password(i, request_id, body, now);
            }
            OP_VIEWS_STOP => {
                if let State::Running { process, .. } = &self.clients[i].state {
                    // SAFETY: a Process handle this broker owns, with SIGNAL from spawn.
                    unsafe { syscall1(SYS_PROCESS_TERMINATE, *process) };
                }
                let _ = send(ch, op, request_id, RS_FLAG_REPLY, &[], &[]);
            }
            OP_VIEWS_LIST => self.list(ch, request_id, &principal),
            OP_VIEWS_CHECK => {
                let verdict = match core::str::from_utf8(&body) {
                    Ok(text) => policy::check(text).map(|_| ()).map_err(|e| alloc::format!("{e}")),
                    Err(_) => Err(String::from("the file is not text")),
                };
                match verdict {
                    Ok(()) => reply_outcome(ch, op, request_id, Outcome::Started, "the policy is valid"),
                    Err(e) => reply_outcome(ch, op, request_id, Outcome::Denied { retry: false }, &e),
                }
            }
            _ => {
                for h in handles {
                    close(h);
                }
                reply_error(ch, op, request_id, KError::Unsupported);
            }
        }
    }

    fn read_policy(&self) -> Result<policy::Policy, String> {
        let bytes = libfs::read_file(self.root_ns, POLICY_PATH)
            .map_err(|_| String::from("there is no policy at /system/views.toml"))?;
        let text = core::str::from_utf8(&bytes).map_err(|_| String::from("the policy is not text"))?;
        policy::parse(text).map_err(|e| alloc::format!("the policy does not read — {e}"))
    }

    fn request(&mut self, i: usize, request_id: u64, body: &[u8], handles: Vec<u64>, principal: &str, now: u64) {
        let ch = self.clients[i].ch;
        let deny = |handles: Vec<u64>, reason: &str| {
            for h in handles {
                close(h);
            }
            reply_outcome(ch, OP_VIEWS_REQUEST, request_id, Outcome::Denied { retry: false }, reason);
        };
        if !matches!(self.clients[i].state, State::Idle) {
            return deny(handles, "this channel already made a request");
        }
        let Some(r) = parse_request(body) else {
            return deny(handles, "the request does not read");
        };
        if handles.len() != r.handle_count() {
            return deny(handles, "the request's handles do not match what it says it carries");
        }
        let (Ok(view), Ok(program)) = (core::str::from_utf8(r.view), core::str::from_utf8(r.program)) else {
            return deny(handles, "a view and a program are names");
        };
        if !policy::is_bare_name(program) {
            return deny(handles, "a program is a bare name, resolved under /bin");
        }
        let mut args = Vec::new();
        for a in r.args() {
            match core::str::from_utf8(a) {
                Ok(s) => args.push(String::from(s)),
                Err(_) => return deny(handles, "an argument is not text"),
            }
        }
        let mut hs = Handles { ns: handles[0], ..Handles::default() };
        let mut k = 1;
        for (bit, slot) in [
            (REQ_STDIN, &mut hs.stdin),
            (REQ_STDOUT, &mut hs.stdout),
            (REQ_STDERR, &mut hs.stderr),
            (REQ_TERMINAL, &mut hs.terminal),
        ] {
            if r.handles & bit != 0 {
                *slot = handles[k];
                k += 1;
            }
        }
        let policy = match self.read_policy() {
            Ok(p) => p,
            Err(e) => {
                Line::new().s(b"view-broker: ").s(e.as_bytes()).end();
                hs.close();
                return reply_outcome(ch, OP_VIEWS_REQUEST, request_id, Outcome::Denied { retry: false }, &e);
            }
        };
        let what = alloc::format!("{principal} {view} {program}");
        let (grants, auth) = match policy.decide(principal, view, program) {
            Decision::Deny(reason) => {
                self.audit(&alloc::format!("view: {what} — denied: {reason}"));
                hs.close();
                return reply_outcome(ch, OP_VIEWS_REQUEST, request_id, Outcome::Denied { retry: false }, &reason);
            }
            Decision::Allow { profile, auth, .. } => (profile.grants.clone(), auth),
        };
        let pending = Pending {
            view: String::from(view),
            program: String::from(program),
            args,
            env: r.env.to_vec(),
            grants,
            handles: hs,
            failures: 0,
            queued: None,
        };
        match auth {
            Auth::Password => {
                self.audit(&alloc::format!("view: {what} — allowed, asking for a password"));
                self.clients[i].state = State::Password(pending);
                reply_outcome(ch, OP_VIEWS_REQUEST, request_id, Outcome::NeedPassword, "");
            }
            Auth::None => {
                let _ = now;
                self.start(i, OP_VIEWS_REQUEST, request_id, pending, principal);
            }
        }
    }

    /// A password for a request waiting on one. Checked now, or when the session's delay from
    /// its last failure has passed — on whichever request that failure was.
    fn password(&mut self, i: usize, request_id: u64, pw: Vec<u8>, now: u64) {
        let ch = self.clients[i].ch;
        let session = self.clients[i].session;
        let State::Password(p) = &mut self.clients[i].state else {
            return reply_outcome(ch, OP_VIEWS_PASSWORD, request_id, Outcome::Denied { retry: false }, "no request is waiting for a password");
        };
        if p.queued.is_some() {
            return reply_outcome(ch, OP_VIEWS_PASSWORD, request_id, Outcome::Denied { retry: false }, "a password is already waiting to be checked");
        }
        let at = self.sessions.get(session).map_or(now, |s| s.pacing.check_at(now));
        p.queued = Some((at, request_id, pw));
        if at <= now {
            self.check_due(i, now);
        }
    }

    /// Check a queued password whose time has come.
    fn check_due(&mut self, i: usize, now: u64) {
        let ch = self.clients[i].ch;
        let session = self.clients[i].session;
        let Some(principal) = self.principal(session) else {
            return;
        };
        let State::Password(p) = &mut self.clients[i].state else {
            return;
        };
        let Some((at, request_id, pw)) = p.queued.take() else {
            return;
        };
        if at > now {
            p.queued = Some((at, request_id, pw));
            return;
        }
        if self.auth_ch == 0 {
            self.auth_ch = ns_lookup(self.root_ns, b"/svc/auth", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
        }
        let mut home = [0u8; 256];
        let ok = self.auth_ch != 0 && libsession::authenticate(self.auth_ch, principal.as_bytes(), &pw, &mut home).is_some();
        let State::Password(p) = &mut self.clients[i].state else {
            return;
        };
        let what = alloc::format!("{principal} {} {}", p.view, p.program);
        if ok {
            let State::Password(p) = core::mem::replace(&mut self.clients[i].state, State::Idle) else {
                return;
            };
            self.start(i, OP_VIEWS_PASSWORD, request_id, p, &principal);
            return;
        }
        p.failures += 1;
        let failures = p.failures;
        if let Some(s) = self.sessions.get_mut(session) {
            s.pacing.failed(now);
        }
        self.audit(&alloc::format!("view: {what} — wrong password ({failures} of {MAX_FAILURES})"));
        if failures >= MAX_FAILURES {
            if let State::Password(p) = &mut self.clients[i].state {
                p.handles.close();
            }
            self.clients[i].state = State::Done;
            reply_outcome(ch, OP_VIEWS_PASSWORD, request_id, Outcome::Denied { retry: false }, "three wrong passwords");
        } else {
            reply_outcome(ch, OP_VIEWS_PASSWORD, request_id, Outcome::Denied { retry: true }, "wrong password");
        }
    }

    /// Build the view and spawn the program in it.
    fn start(&mut self, i: usize, op: u16, request_id: u64, mut p: Pending, principal: &str) {
        let ch = self.clients[i].ch;
        let what = alloc::format!("{principal} {} {}", p.view, p.program);
        let fail = |this: &mut Broker, p: &mut Pending, why: &str| {
            p.handles.close();
            this.clients[i].state = State::Done;
            this.audit(&alloc::format!("view: {what} — could not start: {why}"));
            reply_outcome(ch, op, request_id, Outcome::Denied { retry: false }, why);
        };
        // **Copied again, never used as sent** — whoever sent a namespace may still hold it.
        // SAFETY: a namespace handle the client moved to us.
        let view_ns = unsafe { syscall1(SYS_NS_DERIVE, p.handles.ns) };
        close(p.handles.ns);
        p.handles.ns = 0;
        if view_ns <= 0 {
            return fail(self, &mut p, "could not copy the caller's namespace");
        }
        let view_ns = view_ns as u64;
        for g in &p.grants {
            match g {
                Grant::Disks => {
                    libsession::rebind_block_devices(self.root_ns, view_ns);
                }
            }
        }
        // **Resolved in the broker's own namespace, not the view** — a caller can prune its copy,
        // and a name must not fall through to some shorter binding there.
        let mut path = String::from("/bin/");
        path.push_str(&p.program);
        let image = ns_lookup(self.root_ns, path.as_bytes(), RIGHT_MAP_READ);
        if image == 0 {
            close(view_ns);
            return fail(self, &mut p, "there is no such program");
        }
        let (Ok((setup_ours, setup_child)), Some((life_ours, life_child))) = (pipe(1), make_channel(1)) else {
            close(image);
            close(view_ns);
            return fail(self, &mut p, "out of channels");
        };
        // SAFETY: SPAWN is our static, filled immediately before use; single-threaded.
        let process = unsafe {
            SPAWN.image = image;
            SPAWN.handles[0] = setup_child;
            // **The life channel**: a handle the program never learns about, so it stays open
            // until the program exits — which is how the broker learns *which* program it was
            // (`TODO(child-exit-attribution)`, as `service-mgr` does it).
            SPAWN.handles[1] = life_child;
            SPAWN.namespace = view_ns;
            SPAWN.syscaps = 0;
            SPAWN.arg0 = bootstrap_arg0(true);
            syscall1(SYS_PROCESS_SPAWN, (&raw const SPAWN) as u64)
        };
        close(image);
        if process < 0 {
            for h in [setup_ours, setup_child, life_ours, life_child, view_ns] {
                close(h);
            }
            return fail(self, &mut p, "the program would not spawn");
        }
        let env = view_env(&p.env, &p.view);
        let mut argv: Vec<&str> = Vec::with_capacity(1 + p.args.len());
        argv.push(&p.program);
        argv.extend(p.args.iter().map(|s| s.as_str()));
        let streams = Streams {
            stdin: (p.handles.stdin != 0).then_some(p.handles.stdin),
            stdout: (p.handles.stdout != 0).then_some(p.handles.stdout),
            stderr: (p.handles.stderr != 0).then_some(p.handles.stderr),
        };
        let terminal = (p.handles.terminal != 0).then_some(p.handles.terminal);
        let sent = send_setup_full(setup_ours, &streams, terminal, &argv, &env).is_ok();
        close(setup_ours);
        if sent {
            // The setup message moved them.
            p.handles = Handles::default();
        } else {
            p.handles.close();
            // The program is running without its streams; ask it to stop, and report it.
            // SAFETY: a Process handle this broker owns.
            unsafe { syscall1(SYS_PROCESS_TERMINATE, process as u64) };
        }
        self.audit(&alloc::format!("view: {what} — started"));
        self.clients[i].state = State::Running {
            process: process as u64,
            life: life_ours,
            view_ns,
            view: p.view,
            program: p.program,
        };
        reply_outcome(ch, op, request_id, Outcome::Started, "");
    }

    fn list(&self, ch: u64, request_id: u64, principal: &str) {
        let policy = match self.read_policy() {
            Ok(p) => p,
            Err(_) => return reply_error(ch, OP_VIEWS_LIST, request_id, KError::InvalidArgument),
        };
        let mut out = [0u8; 2048];
        let mut at = 2;
        for (view, run, password) in policy.rows_for(principal) {
            let row = Row { view: view.as_bytes(), run: run.as_bytes(), password };
            if push_row(&mut out, &mut at, &row).is_none() {
                break;
            }
        }
        let _ = send(ch, OP_VIEWS_LIST, request_id, RS_FLAG_REPLY, &out[..at], &[]);
    }

    /// A client's channel closed: `with` is gone. If it left a program running, ask it to stop —
    /// nobody is left to see its output or stop it.
    fn client_gone(&mut self, i: usize) {
        let c = self.clients.remove(i);
        match c.state {
            State::Running { process, life, view_ns, .. } => {
                // SAFETY: a Process handle this broker owns.
                unsafe { syscall1(SYS_PROCESS_TERMINATE, process) };
                // Still reaped when it exits: its life channel stays in the wait set.
                self.clients.push(Client {
                    ch: 0,
                    session: c.session,
                    state: State::Running { process, life, view_ns, view: String::new(), program: String::new() },
                });
            }
            State::Password(mut p) => p.handles.close(),
            _ => {}
        }
        close(c.ch);
    }

    /// Take every `ChildExited` queued on the notification channel.
    fn drain_notifications(&mut self) {
        loop {
            // SAFETY: NOTIF is a valid 64-byte out-param.
            let r = unsafe { syscall4(SYS_NOTIF_RECV, self.notif, (&raw mut NOTIF) as u64, 0, 0) };
            if r != 0 {
                break;
            }
            // SAFETY: the kernel wrote a notification into NOTIF.
            let (kind, body) = unsafe { ((&raw const NOTIF.kind).read(), (&raw const NOTIF.body).read()) };
            if kind == KIND_CHILD_EXITED {
                let exit_kind = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                let code = i32::from_le_bytes([body[8], body[9], body[10], body[11]]);
                self.codes.push_back((code, exit_kind != 0));
            }
        }
        self.pair_exits();
    }

    /// A program's life channel closed: it exited.
    fn life_closed(&mut self, life: u64) {
        self.exited.push_back(life);
        self.drain_notifications();
    }

    /// Pair exited programs with exit codes, first with first. **One exit per wake pairs
    /// correctly; two in the same wake can swap codes** — the residual of
    /// `TODO(child-exit-attribution)`, which `service-mgr` lives with too.
    fn pair_exits(&mut self) {
        while !self.exited.is_empty() && !self.codes.is_empty() {
            let life = self.exited.pop_front().unwrap_or(0);
            let (code, crashed) = self.codes.pop_front().unwrap_or((0, true));
            let Some(i) = self.clients.iter().position(|c| matches!(c.state, State::Running { life: l, .. } if l == life)) else {
                continue;
            };
            let session = self.clients[i].session;
            let ch = self.clients[i].ch;
            let State::Running { process, life, view_ns, view, program } =
                core::mem::replace(&mut self.clients[i].state, State::Done)
            else {
                continue;
            };
            for h in [process, life, view_ns] {
                close(h);
            }
            let principal = self.principal(session).unwrap_or_default();
            self.audit(&alloc::format!("view: {principal} {view} {program} — exited, code {code}"));
            if ch != 0 {
                let mut body = [0u8; EXITED_LEN];
                let n = build_exited(&mut body, code, crashed).unwrap_or(0);
                let _ = send(ch, OP_VIEWS_EXITED, 0, 0, &body[..n], &[]);
            } else {
                self.clients.remove(i);
            }
        }
    }

    /// Every handle the wait set holds.
    fn wait_count(&self) -> usize {
        2 + self.supervisors.len()
            + self.clients.iter().filter(|c| c.ch != 0).count()
            + self.clients.iter().filter(|c| matches!(c.state, State::Running { .. })).count()
    }

    /// The soonest a queued password may be checked.
    fn next_deadline(&self) -> u64 {
        self.clients
            .iter()
            .filter_map(|c| match &c.state {
                State::Password(p) => p.queued.as_ref().map(|q| q.0),
                _ => None,
            })
            .min()
            .unwrap_or(u64::MAX)
    }

    fn run_due(&mut self, now: u64) {
        for i in 0..self.clients.len() {
            if matches!(&self.clients[i].state, State::Password(p) if p.queued.as_ref().is_some_and(|q| q.0 <= now)) {
                self.check_due(i, now);
            }
        }
    }
}

/// The caller's environment with `view` set to the view the program runs in, so a program — an
/// elevated shell's prompt, say — can know. A caller that sent none gets one with just that.
fn view_env(bytes: &[u8], view: &str) -> Record {
    let record = if bytes.is_empty() {
        Record::default()
    } else {
        match read_value(&mut ByteSource::new(bytes), TypeTag::Record) {
            Ok(Value::Record(r)) => (*r).clone(),
            _ => Record::default(),
        }
    };
    record.with_str_field("view", view)
}

/// Send `init` `Meta::Ready`, naming this server and carrying the forwarding endpoint's client
/// end — the handshake every server here speaks to the supervisor that binds it.
fn send_ready(control: u64, client_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 16];
    let Some(n) = librsproto::meta::ready(&mut body, b"view-broker") else {
        return false;
    };
    send(control, librsproto::OP_READY, 0, 0, &body[..n], &[client_end])
}

/// Bootstrap registers: `rdi` = notification channel (where `ChildExited` arrives), `rsi` = the
/// inherited root namespace, `rdx` = the control channel `init` installed, `rcx` = `arg0`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"view-broker: up\n");
    let Some((client_end, serve_end)) = make_channel(4) else {
        kprint(b"view-broker: channel create FAIL\n");
        exit(1);
    };
    if !send_ready(control, client_end) {
        kprint(b"view-broker: Ready send FAIL\n");
        exit(1);
    }
    let mut b = Broker {
        root_ns,
        notif,
        serve_end,
        auth_ch: 0,
        supervisors: Vec::new(),
        clients: Vec::new(),
        sessions: Sessions::new(),
        codes: VecDeque::new(),
        exited: VecDeque::new(),
        log: liblog::open_source(root_ns, b"/log/system/view-broker"),
    };
    loop {
        // SAFETY: WAIT_HANDLES holds MAX_WAIT_HANDLES slots and `wait_count` is kept within it
        // by `serve_resolve`, which refuses a channel that would not fit.
        let (n, waited) = unsafe {
            let mut n = 0usize;
            let mut push = |h: u64| {
                if n < MAX_WAIT_HANDLES {
                    WAIT_HANDLES[n] = h;
                    n += 1;
                }
            };
            push(b.serve_end);
            push(b.notif);
            for &s in &b.supervisors {
                push(s);
            }
            for c in &b.clients {
                if c.ch != 0 {
                    push(c.ch);
                }
                if let State::Running { life, .. } = c.state {
                    push(life);
                }
            }
            let deadline = b.next_deadline();
            let w = syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                n as u64,
                (&raw mut WAIT_RESULTS) as u64,
                deadline,
            );
            (n, w)
        };
        let _ = n;
        let now = now_ns();
        if waited > 0 {
            for j in 0..waited as usize {
                // SAFETY: `waited` records were written; the handle is the first word of each.
                let h = unsafe {
                    let off = j * 24;
                    u64::from_le_bytes(WAIT_RESULTS[off..off + 8].try_into().unwrap_or([0; 8]))
                };
                if h == b.serve_end {
                    b.serve_resolve();
                } else if h == b.notif {
                    b.drain_notifications();
                } else if let Some(i) = b.supervisors.iter().position(|&s| s == h) {
                    b.serve_supervisor(i);
                } else if let Some(i) = b.clients.iter().position(|c| c.ch == h && h != 0) {
                    b.serve_client(i, now);
                } else if b.clients.iter().any(|c| matches!(c.state, State::Running { life, .. } if life == h)) {
                    // A life channel only ever signals its peer closing.
                    if let Err(()) = recv(h) {
                        b.life_closed(h);
                    }
                }
            }
        }
        b.run_due(now);
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"view-broker: PANIC\n");
    exit(1);
}
