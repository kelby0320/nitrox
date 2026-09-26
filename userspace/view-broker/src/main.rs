//! `view-broker` — run a program in a **view**: its caller's namespace plus a profile's grants,
//! when `/system/views.toml` says the caller may (`docs/planning/administration.md` § Part A).
//!
//! **What it holds, and so what a bug here reaches.** It is spawned by `init` with
//! `BIND_NAMESPACE`, which it needs to bind grants into the views it builds, and it inherits the
//! root namespace, which is where the grants come from: every block device, for `disks`. **And the
//! root filesystem**, through which it reads `/system/views.toml`, replaces it since Part D.2, and
//! makes and removes `/home/<name>` for the accounts it fronts since Part D.3. It never holds a
//! session's ingredients — the caller hands it a copy of its own namespace (`sys_ns_derive`), and
//! it copies that again before binding anything, so the caller cannot keep a handle to what it
//! builds. It never touches a terminal: `with` reads the password.
//!
//! **Two kinds of channel off one forwarding endpoint**, which `init` binds at `/svc/views`: a
//! login supervisor resolves `/svc/views/session` for a channel to open and close sessions on,
//! and binds the same endpoint into each session at `/dev/views` with the base `/s/<session>`, so a
//! process there resolves a channel the broker already knows the session of. See
//! `librsproto::views` for the ops.
//!
//! **And a channel per grant that needs one**: the `views` grant binds the same endpoint at
//! `/dev/policy` and `accounts` at `/dev/accounts`, each with a base naming the session, so a
//! resolve there is a channel for that grant's ops alone (administration Parts D.2 and D.3).
//!
//! **A resource server holding `BIND_NAMESPACE`** — the second after `desktop-shell`, and for the
//! same reason: it binds only into namespaces it creates, and never registers itself
//! (`graphical-session.md` §3, `userspace/CLAUDE.md`).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use libkern::debug::Line;
use libkern::*;
use librsproto::namespace::{OBJECT_KIND_CHANNEL, RESOLVE_REPLY_LEN, parse_resolve_request, resolve_reply};
use librsproto::auth::{
    OP_AUTH_ADD, OP_AUTH_LIST, OP_AUTH_REMOVE, OP_AUTH_SET_PASSWORD, build_account_request, parse_account_list,
    parse_account_request,
};
use librsproto::storage::{OP_STORAGE_IN_USE, parse_in_use};
use librsproto::views::*;
use librsproto::{OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};
use libstream::setup::{Streams, bootstrap_arg0, pipe, send_setup_full};
use libstream::wire::{ByteSource, Record, TypeTag, Value, read_value};
use view_broker::accounts;
use view_broker::exits::Exits;
use view_broker::pacing::{Held, MAX_FAILURES};
use view_broker::policy::{self, Auth, Decision, Grant};
use view_broker::sessions::Sessions;
use view_broker::slots::Load;
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
    /// A password waiting in [`Broker::held`] to be checked: the `Password` request to answer,
    /// and the password.
    queued: Option<(u64, Vec<u8>)>,
}

enum State {
    Idle,
    Password(Pending),
    /// A `ChangePassword` held until the session's delay from its last failure has passed: the
    /// request to answer, the current password and the new one (administration Part D.3).
    Changing { request_id: u64, current: Vec<u8>, new: Vec<u8> },
    Running { process: u64, life: u64, view_ns: u64, view: String, program: String },
    Done,
}

struct Client {
    ch: u64,
    session: u64,
    state: State,
}

/// Where the storage service mints an admin endpoint (administration Part C.6).
const STORAGE_ADMIN: &[u8] = b"/svc/storage/admin-endpoint";
/// How long the storage service may take to answer `InUse`: it answers from what it holds, so this
/// bounds a service that is wedged, not an ordinary wait.
const IN_USE_WAIT_NS: u64 = 5_000_000_000;

/// **The storage service, as this broker reaches it** (administration Part C.6): an admin endpoint,
/// which the `storage` grant binds into a view, and an admin session of the broker's own, on which
/// it asks `InUse` before the `disks` grant.
///
/// **Resolved when first needed, not at startup**: `init` spawns the broker before the device
/// manager and the storage service, so at startup there is nothing to resolve. A failure is not
/// remembered, so a service that came up later is found the next time.
#[derive(Default)]
struct Storage {
    endpoint: u64,
    session: u64,
}

/// Which grant's endpoint a channel was resolved through.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Endpoint {
    /// `/dev/policy`, the `views` grant's: `Show` and `Install` (administration Part D.2).
    Policy,
    /// `/dev/accounts`, the `accounts` grant's: `AddAccount`, `RemoveAccount` and `SetPassword`
    /// (administration Part D.3).
    Accounts,
}

struct Broker {
    storage: Storage,
    root_ns: u64,
    notif: u64,
    serve_end: u64,
    /// **A duplicate of this broker's own forwarding endpoint** — the one `init` binds at
    /// `/svc/views` — kept to bind into a view with a base of the broker's choosing: the `views`
    /// grant's `/dev/policy` (administration Part D.2). `0` if it would not duplicate, and then
    /// that grant is refused rather than given without its binding.
    forwarding: u64,
    auth_ch: u64,
    /// An admin session on `auth-service` (`/svc/auth/admin`), opened on first need: the account
    /// list the policy and a removal are judged against, and every account write the accounts
    /// endpoint asks for. `0` until then, or after it failed.
    auth_admin: u64,
    /// Channels resolved through a grant's endpoint in a view — `/dev/policy` or `/dev/accounts` —
    /// each with its session.
    endpoints: Vec<(u64, u64, Endpoint)>,
    supervisors: Vec<u64>,
    clients: Vec<Client>,
    sessions: Sessions,
    /// Passwords waiting to be checked, by client channel — see [`Held`] for why none of them
    /// carries a deadline of its own.
    held: Held,
    /// Programs whose life channel closed, and exit codes from `ChildExited`, each waiting for
    /// the other — see [`Exits`] for why either can come first.
    exits: Exits,
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

/// An error reply: an `ErrorBody`, as every server sends one. **Its whole twelve bytes** — the
/// kernel reads a shorter one on a forwarded resolve as malformed and hands the caller
/// `KernelError`, which is how a full broker's `WouldBlock` and a closed session's `NotFound`
/// once arrived (found by `boot-probe`'s full-broker step).
fn reply_error(ch: u64, op: u16, request_id: u64, err: KError) {
    let mut body = [0u8; librsproto::error::ERROR_BODY_LEN];
    let n = librsproto::error::error_body(&mut body, err.as_i32(), 0, b"").unwrap_or(0);
    let _ = send(ch, op, request_id, RS_FLAG_REPLY | RS_FLAG_ERROR, &body[..n], &[]);
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
    recv_full(ch).map(|m| m.map(|(op, request_id, _, body)| (op, request_id, body)))
}

/// [`recv`], with the message's flags: an answer from a server may be an error reply.
fn recv_full(ch: u64) -> Result<Option<(u16, u64, u32, Vec<u8>)>, ()> {
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
        Ok(m) => Ok(Some((m.op, m.request_id, m.flags, m.body.to_vec()))),
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

/// Where the `storage` grant is bound in a view.
const STORAGE_GRANT: &[u8] = b"/dev/storage/admin";
/// Where the `views` grant binds the broker's policy endpoint in a view (administration Part D.2).
const POLICY_GRANT: &[u8] = b"/dev/policy";
/// Where the `accounts` grant binds the broker's accounts endpoint in a view (administration Part
/// D.3).
const ACCOUNTS_GRANT: &[u8] = b"/dev/accounts";
/// Where a new policy is written before it is renamed over the old one.
const POLICY_NEW: &[u8] = b"/system/views.toml.new";
/// How long the broker waits for `auth-service`'s account list.
const ACCOUNTS_WAIT_NS: u64 = 5_000_000_000;

/// Wait on `h` until `deadline`. `true` if it became ready.
fn wait_until(h: u64, deadline: u64) -> bool {
    let handles = [h];
    let mut results = [0u8; 24];
    // SAFETY: valid one-entry wait arrays on this frame.
    unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, results.as_mut_ptr() as u64, deadline) == 1 }
}

/// Whether `op`'s body carries a password, and so must not outlive its use in any buffer.
fn carries_password(op: u16) -> bool {
    matches!(op, OP_VIEWS_PASSWORD | OP_VIEWS_CHANGE_PASSWORD | OP_VIEWS_ADD_ACCOUNT | OP_VIEWS_SET_PASSWORD)
}

/// The reason an error reply gives in its `ErrorBody`, or its `KError` when it gives none.
fn refusal(body: &[u8]) -> String {
    match librsproto::error::parse_error(body) {
        Some(e) if !e.msg.is_empty() => String::from_utf8_lossy(e.msg).into_owned(),
        Some(e) => alloc::format!("{:?}", KError::from_i32(e.kerror)),
        None => String::from("an answer that does not read"),
    }
}

/// What a refused name is told: the rule, never the name back — it has not been checked, so it
/// goes into no log line and no path.
const NAME_RULE: &str =
    "a name is 1 to 32 bytes: a lowercase letter or `_`, then lowercase letters, digits, `_` or `-`";

impl Broker {
    /// The storage service's admin endpoint, resolved on first need. `0` if the service is not
    /// there.
    fn storage_endpoint(&mut self) -> u64 {
        if self.storage.endpoint == 0 {
            self.storage.endpoint = ns_lookup(
                self.root_ns,
                STORAGE_ADMIN,
                RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT | RIGHT_DUPLICATE | RIGHT_TRANSFER,
            );
        }
        self.storage.endpoint
    }

    /// An admin session of the broker's own, opened on first need: the admin endpoint bound in a
    /// namespace made for the purpose, and resolved there. `0` if there is none to open.
    fn storage_session(&mut self) -> u64 {
        if self.storage.session != 0 {
            return self.storage.session;
        }
        let endpoint = self.storage_endpoint();
        if endpoint == 0 {
            return 0;
        }
        // SAFETY: register-only syscall; returns a fresh namespace handle.
        let ns = unsafe { syscall0(SYS_NS_CREATE) };
        if ns <= 0 {
            return 0;
        }
        let at = b"/admin";
        // SAFETY: a namespace this broker made, a valid path, and an endpoint it holds.
        let bound = unsafe { syscall4(SYS_NS_BIND, ns as u64, at.as_ptr() as u64, at.len() as u64, endpoint) } == 0;
        let session = if bound { ns_lookup(ns as u64, at, RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT) } else { 0 };
        close(ns as u64);
        self.storage.session = session;
        session
    }

    /// **What is in use**: the registry ids the storage service's `InUse` names. `None` if it did
    /// not answer — and then the session is dropped, so the next ask opens a new one.
    fn in_use(&mut self) -> Option<Vec<u32>> {
        let session = self.storage_session();
        if session == 0 {
            return None;
        }
        let asked = send(session, OP_STORAGE_IN_USE, 1, 0, &[], &[]);
        let deadline = now_ns().saturating_add(IN_USE_WAIT_NS);
        let mut ids = Vec::new();
        let answered = asked
            && loop {
                match recv(session) {
                    Ok(Some((op, 1, body))) if op == OP_STORAGE_IN_USE => break parse_in_use(&body, |id| ids.push(id)),
                    // A stray is dropped; nothing else is asked on this session.
                    Ok(Some(_)) => received().into_iter().for_each(close),
                    Ok(None) if wait_until(session, deadline) => {}
                    _ => break false,
                }
            };
        if !answered {
            close(session);
            self.storage.session = 0;
            return None;
        }
        Some(ids)
    }

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
        let (session, endpoint) = match asked {
            Suffix::Supervisor => (None, None),
            Suffix::Client(id) if self.sessions.get(id).is_some() => (Some(id), None),
            // **The policy endpoint, as the `views` grant binds it** (administration Part D.2): a
            // channel of its own, on which only `Show` and `Install` are answered.
            Suffix::Policy(id) if self.sessions.get(id).is_some() => (Some(id), Some(Endpoint::Policy)),
            // **The accounts endpoint, as the `accounts` grant binds it** (Part D.3).
            Suffix::Accounts(id) if self.sessions.get(id).is_some() => (Some(id), Some(Endpoint::Accounts)),
            _ => {
                reply_error(self.serve_end, OP_NS_RESOLVE, request_id, KError::NotFound);
                return;
            }
        };
        // Every channel is a slot in the one wait set this process has, and a client's program
        // will need a second. A grant's endpoint starts no program, so it is one slot, as a
        // supervisor's is.
        let load = self.load();
        let fits = if session.is_none() || endpoint.is_some() {
            load.admits_supervisor(MAX_WAIT_HANDLES)
        } else {
            load.admits_client(MAX_WAIT_HANDLES)
        };
        if !fits {
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
        match (session, endpoint) {
            (None, _) => self.supervisors.push(ours),
            (Some(id), Some(e)) => self.endpoints.push((ours, id, e)),
            (Some(id), None) => self.clients.push(Client { ch: ours, session: id, state: State::Idle }),
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
                    // The `storage`, `views` and `accounts` grants too; `NotFound` for a view not
                    // given one.
                    for at in [STORAGE_GRANT, POLICY_GRANT, ACCOUNTS_GRANT] {
                        // SAFETY: valid namespace handle and path.
                        unsafe { syscall3(SYS_NS_UNBIND, *view_ns, at.as_ptr() as u64, at.len() as u64) };
                    }
                }
                State::Password(p) => {
                    p.handles.close();
                    self.held.forget(c.ch);
                    if let Some((request_id, mut pw)) = p.queued.take() {
                        scrub(&mut pw);
                        let why = "this session has ended";
                        reply_outcome(c.ch, OP_VIEWS_PASSWORD, request_id, Outcome::Denied { retry: false }, why);
                    }
                    c.state = State::Done;
                }
                State::Changing { request_id, current, new } => {
                    scrub(current);
                    scrub(new);
                    self.held.forget(c.ch);
                    let why = "this session has ended";
                    reply_outcome(c.ch, OP_VIEWS_CHANGE_PASSWORD, *request_id, Outcome::Denied { retry: false }, why);
                    c.state = State::Done;
                }
                _ => {}
            }
        }
        // Its grants' channels close with it: nothing more is heard under its bases.
        let (gone, kept): (Vec<_>, Vec<_>) = self.endpoints.drain(..).partition(|e| e.1 == id);
        self.endpoints = kept;
        gone.iter().for_each(|&(ch, _, _)| close(ch));
        Line::new().s(b"view-broker: session ").u(id).s(b" ended").end();
    }

    /// **The accounts that exist**, from `auth-service`'s `List` on an admin session of the
    /// broker's own, opened on first need. `None` if it cannot be asked or does not answer — and
    /// then the session is dropped, so the next ask opens a new one. Judging a policy fails closed
    /// without it: a policy cannot be said to leave an administrator when nobody knows who exists.
    fn accounts(&mut self) -> Option<Vec<(String, String)>> {
        let body = self.ask_auth(OP_AUTH_LIST, &[]).ok()?;
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        Some(parse_account_list(&body)?.iter().map(|(n, h)| (text(n), text(h))).collect())
    }

    /// **Ask `auth-service`'s admin session**, opened on first need: send `op` with `body`, and
    /// wait at most `ACCOUNTS_WAIT_NS` for the answer — its body, or the reason it refused. A
    /// session that does not answer is dropped, so the next ask opens a new one.
    ///
    /// **The body may carry a password**, so the send buffer is zeroed as soon as the kernel has
    /// the message; the caller zeroes its own copy.
    fn ask_auth(&mut self, op: u16, body: &[u8]) -> Result<Vec<u8>, String> {
        if self.auth_admin == 0 {
            self.auth_admin = ns_lookup(self.root_ns, b"/svc/auth/admin", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
        }
        let admin = self.auth_admin;
        if admin == 0 {
            return Err(String::from("auth-service could not be reached"));
        }
        let asked = send(admin, op, 1, 0, body, &[]);
        // SAFETY: our send buffer; single-threaded, and the kernel copied the message during the
        // send.
        unsafe { scrub(&mut *(&raw mut REPLY_MSG)) };
        let deadline = now_ns().saturating_add(ACCOUNTS_WAIT_NS);
        let answer = asked
            .then(|| loop {
                match recv_full(admin) {
                    Ok(Some((o, 1, flags, body))) if o == op => break Some((flags & RS_FLAG_ERROR != 0, body)),
                    Ok(Some(_)) => received().into_iter().for_each(close),
                    Ok(None) if wait_until(admin, deadline) => {}
                    _ => break None,
                }
            })
            .flatten();
        match answer {
            Some((false, body)) => Ok(body),
            Some((true, body)) => Err(refusal(&body)),
            None => {
                close(admin);
                self.auth_admin = 0;
                Err(String::from("auth-service did not answer"))
            }
        }
    }

    /// **Judge a policy's text** as `Check` and `Install` do: it must be text, it must read, and an
    /// account that exists must be able to administer under it (`policy::check`). The reason, if
    /// not.
    fn judge(&mut self, text: &[u8]) -> Result<policy::Policy, String> {
        let text = core::str::from_utf8(text).map_err(|_| String::from("the file is not text"))?;
        let accounts = self
            .accounts()
            .ok_or_else(|| String::from("the accounts could not be listed, so the policy cannot be judged"))?;
        let names: Vec<&str> = accounts.iter().map(|(n, _)| n.as_str()).collect();
        policy::check(text, &names).map_err(|e| alloc::format!("{e}"))
    }

    /// A channel resolved through a grant's endpoint in a view: the policy's or the accounts'.
    /// From a session each is reached only through its grant's binding, so holding it is the
    /// authority.
    fn serve_endpoint(&mut self, i: usize) {
        let (ch, session, endpoint) = self.endpoints[i];
        let (op, request_id, mut body) = match recv(ch) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(()) => {
                close(ch);
                self.endpoints.remove(i);
                return;
            }
        };
        if carries_password(op) {
            // The password now lives only in `body`, until it has been used.
            // SAFETY: our receive buffer; single-threaded, and the message was copied out.
            unsafe { scrub(&mut *(&raw mut RECV_MSG)) };
        }
        received().into_iter().for_each(close);
        let who = self.principal(session).unwrap_or_default();
        match endpoint {
            Endpoint::Policy => self.serve_policy(ch, op, request_id, &body, &who),
            Endpoint::Accounts => self.serve_accounts(ch, op, request_id, &body, &who),
        }
        scrub(&mut body);
    }

    /// **A policy channel**: `Show` and `Install`, and nothing else (administration Part D.2).
    fn serve_policy(&mut self, ch: u64, op: u16, request_id: u64, body: &[u8], who: &str) {
        match op {
            OP_VIEWS_SHOW => match libfs::read_file(self.root_ns, POLICY_PATH) {
                Ok(text) if text.len() <= POLICY_MAX => {
                    let _ = send(ch, op, request_id, RS_FLAG_REPLY, &text, &[]);
                }
                Ok(_) => reply_error(ch, op, request_id, KError::TooLarge),
                Err(_) => reply_error(ch, op, request_id, KError::NotFound),
            },
            OP_VIEWS_INSTALL => {
                let installed = if body.len() > POLICY_MAX {
                    Err(alloc::format!("a policy is at most {POLICY_MAX} bytes"))
                } else {
                    self.judge(body).and_then(|_| {
                        libfs::write_file(self.root_ns, POLICY_NEW, body)
                            .and_then(|()| libfs::rename(self.root_ns, POLICY_NEW, POLICY_PATH, true))
                            .map_err(|_| String::from("the policy could not be written"))
                    })
                };
                match installed {
                    Ok(()) => {
                        self.audit(&alloc::format!("policy: {who} installed a new policy"));
                        reply_outcome(ch, op, request_id, Outcome::Started, "the policy is installed");
                    }
                    Err(why) => {
                        self.audit(&alloc::format!("policy: {who}'s policy was refused: {why}"));
                        reply_outcome(ch, op, request_id, Outcome::Denied { retry: false }, &why);
                    }
                }
            }
            _ => reply_error(ch, op, request_id, KError::Unsupported),
        }
    }

    /// A client's channel: requests, passwords, stops, listings and checks.
    fn serve_client(&mut self, i: usize) {
        let ch = self.clients[i].ch;
        let (op, request_id, mut body) = match recv(ch) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(()) => return self.client_gone(i),
        };
        if carries_password(op) {
            // The password now lives only in `body`, until it has been checked.
            // SAFETY: our receive buffer; single-threaded, and the message was copied out.
            unsafe { scrub(&mut *(&raw mut RECV_MSG)) };
        }
        // **Only a request carries handles.** Whatever came with anything else is closed here,
        // once, rather than by each arm — an arm that forgot would keep them for good, and any
        // program in any session can send them.
        let mut handles = received();
        if op != OP_VIEWS_REQUEST {
            for h in handles.drain(..) {
                close(h);
            }
        }
        let session = self.clients[i].session;
        let Some(principal) = self.principal(session) else {
            for h in handles {
                close(h);
            }
            scrub(&mut body);
            return reply_outcome(ch, op, request_id, Outcome::Denied { retry: false }, "this session has ended");
        };
        match op {
            OP_VIEWS_REQUEST => self.request(i, request_id, &body, handles, &principal),
            OP_VIEWS_PASSWORD => self.password(i, request_id, core::mem::take(&mut body)),
            OP_VIEWS_CHANGE_PASSWORD => self.change_password(i, request_id, &body),
            OP_VIEWS_ACCOUNTS => self.list_accounts(ch, request_id),
            OP_VIEWS_STOP => {
                if let State::Running { process, .. } = &self.clients[i].state {
                    // SAFETY: a Process handle this broker owns, with SIGNAL from spawn.
                    unsafe { syscall1(SYS_PROCESS_TERMINATE, *process) };
                }
                let _ = send(ch, op, request_id, RS_FLAG_REPLY, &[], &[]);
            }
            OP_VIEWS_LIST => self.list(ch, request_id, &principal),
            OP_VIEWS_CHECK => {
                let verdict = self.judge(&body).map(|_| ());
                match verdict {
                    Ok(()) => reply_outcome(ch, op, request_id, Outcome::Started, "the policy is valid"),
                    Err(e) => reply_outcome(ch, op, request_id, Outcome::Denied { retry: false }, &e),
                }
            }
            _ => reply_error(ch, op, request_id, KError::Unsupported),
        }
        scrub(&mut body);
    }

    fn read_policy(&self) -> Result<policy::Policy, String> {
        let bytes = libfs::read_file(self.root_ns, POLICY_PATH)
            .map_err(|_| String::from("there is no policy at /system/views.toml"))?;
        let text = core::str::from_utf8(&bytes).map_err(|_| String::from("the policy is not text"))?;
        policy::parse(text).map_err(|e| alloc::format!("the policy does not read — {e}"))
    }

    fn request(&mut self, i: usize, request_id: u64, body: &[u8], handles: Vec<u64>, principal: &str) {
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
            Auth::None => self.start(i, OP_VIEWS_REQUEST, request_id, pending, principal),
        }
    }

    /// A password for a request waiting on one: held, and checked by [`Broker::run_due`] once
    /// the session's delay from its last failure has passed — on whichever request that failure
    /// was, and whenever it came.
    fn password(&mut self, i: usize, request_id: u64, mut pw: Vec<u8>) {
        let ch = self.clients[i].ch;
        let session = self.clients[i].session;
        let refuse = |pw: &mut Vec<u8>, why: &str| {
            scrub(pw);
            reply_outcome(ch, OP_VIEWS_PASSWORD, request_id, Outcome::Denied { retry: false }, why);
        };
        let State::Password(p) = &mut self.clients[i].state else {
            return refuse(&mut pw, "no request is waiting for a password");
        };
        if p.queued.is_some() {
            return refuse(&mut pw, "a password is already waiting to be checked");
        }
        p.queued = Some((request_id, pw));
        self.held.hold(ch, session);
    }

    /// Whether `password` is `principal`'s, asked of `auth-service`'s oracle, opened on first need.
    fn authenticate(&mut self, principal: &str, password: &[u8]) -> bool {
        if self.auth_ch == 0 {
            self.auth_ch = ns_lookup(self.root_ns, b"/svc/auth", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
        }
        let mut home = [0u8; 256];
        self.auth_ch != 0 && libsession::authenticate(self.auth_ch, principal.as_bytes(), password, &mut home).is_some()
    }

    /// Check client `i`'s held password: its session's delay has passed.
    fn check(&mut self, i: usize) {
        if matches!(self.clients[i].state, State::Changing { .. }) {
            return self.check_change(i);
        }
        let ch = self.clients[i].ch;
        let session = self.clients[i].session;
        let Some(principal) = self.principal(session) else {
            return;
        };
        let State::Password(p) = &mut self.clients[i].state else {
            return;
        };
        let Some((request_id, mut pw)) = p.queued.take() else {
            return;
        };
        let ok = self.authenticate(&principal, &pw);
        scrub(&mut pw);
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
        // From when the check ended, not when the wake that ran it began: the delay is the gap
        // between one answer and the next check.
        if let Some(s) = self.sessions.get_mut(session) {
            s.pacing.failed(now_ns());
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

    /// **`ChangePassword`** (administration Part D.3): the new password checked against the rules
    /// here, and the request **held**, as a `Password` is, until the session's delay from its last
    /// failure has passed — so a wrong current password here and a wrong one for `with` share one
    /// pace. [`Broker::check_change`] answers it.
    fn change_password(&mut self, i: usize, request_id: u64, body: &[u8]) {
        let ch = self.clients[i].ch;
        let session = self.clients[i].session;
        let refuse = |why: &str| {
            reply_outcome(ch, OP_VIEWS_CHANGE_PASSWORD, request_id, Outcome::Denied { retry: false }, why);
        };
        if !matches!(self.clients[i].state, State::Idle) {
            return refuse("this channel is already busy with a request");
        }
        let Some((current, new)) = parse_password_change(body) else {
            return refuse("the request does not read");
        };
        if !libusers::valid_password(new) {
            return refuse("a password is 1 to 128 bytes");
        }
        self.clients[i].state = State::Changing { request_id, current: current.to_vec(), new: new.to_vec() };
        self.held.hold(ch, session);
    }

    /// A held `ChangePassword`, its session's delay passed: the current password checked as the
    /// session's principal's, and on success the new one set. **A wrong current password is a
    /// failure like any other**: it holds the session's next check, on whichever request.
    fn check_change(&mut self, i: usize) {
        let ch = self.clients[i].ch;
        let session = self.clients[i].session;
        let taken = core::mem::replace(&mut self.clients[i].state, State::Idle);
        let State::Changing { request_id, mut current, mut new } = taken else {
            return;
        };
        let Some(principal) = self.principal(session) else {
            scrub(&mut current);
            scrub(&mut new);
            return;
        };
        let right = self.authenticate(&principal, &current);
        scrub(&mut current);
        let answer = if right {
            let mut req = [0u8; 256];
            let asked = match build_account_request(&mut req, principal.as_bytes(), &new) {
                Some(n) => self.ask_auth(OP_AUTH_SET_PASSWORD, &req[..n]).map(|_| ()),
                None => Err(String::from("the new password does not fit a request")),
            };
            scrub(&mut req);
            asked
        } else {
            // From when the check ended, as for `with`'s.
            if let Some(s) = self.sessions.get_mut(session) {
                s.pacing.failed(now_ns());
            }
            Err(String::from("wrong password"))
        };
        scrub(&mut new);
        match answer {
            Ok(()) => {
                self.audit(&alloc::format!("accounts: {principal} changed their own password"));
                reply_outcome(ch, OP_VIEWS_CHANGE_PASSWORD, request_id, Outcome::Started, "your password is changed");
            }
            Err(why) => {
                self.audit(&alloc::format!("accounts: {principal}'s password change was refused: {why}"));
                reply_outcome(ch, OP_VIEWS_CHANGE_PASSWORD, request_id, Outcome::Denied { retry: false }, &why);
            }
        }
    }

    /// **`Accounts`** (administration Part D.3): every account, its home, its open sessions and
    /// whether it could administer — anyone's to read, as a Unix `passwd` file is.
    fn list_accounts(&mut self, ch: u64, request_id: u64) {
        let Some(listed) = self.accounts() else {
            return reply_error(ch, OP_VIEWS_ACCOUNTS, request_id, KError::IoError);
        };
        let policy = self.read_policy().ok();
        let mut out = [0u8; POLICY_MAX];
        let mut at = 2;
        for a in accounts::shown(&listed, &self.sessions, policy.as_ref()) {
            let row = AccountRow {
                name: a.name.as_bytes(),
                home: a.home.as_bytes(),
                sessions: a.sessions.min(u16::MAX as usize) as u16,
                administers: a.administers,
            };
            if push_account(&mut out, &mut at, &row).is_none() {
                return reply_error(ch, OP_VIEWS_ACCOUNTS, request_id, KError::TooLarge);
            }
        }
        let _ = send(ch, OP_VIEWS_ACCOUNTS, request_id, RS_FLAG_REPLY, &out[..at], &[]);
    }

    /// **An accounts channel** (administration Part D.3): `AddAccount`, `RemoveAccount` and
    /// `SetPassword`, and nothing else. Every answer is audited as the session's principal, and a
    /// refusal with the guard that refused it.
    fn serve_accounts(&mut self, ch: u64, op: u16, request_id: u64, body: &[u8], who: &str) {
        let (asked, answer) = match op {
            OP_VIEWS_ADD_ACCOUNT => ("add", self.add_account(body)),
            OP_VIEWS_REMOVE_ACCOUNT => ("removal", self.remove_account(body)),
            OP_VIEWS_SET_PASSWORD => ("password setting", self.set_password(body)),
            _ => return reply_error(ch, op, request_id, KError::Unsupported),
        };
        match answer {
            Ok(done) => {
                self.audit(&alloc::format!("accounts: {who} {done}"));
                reply_outcome(ch, op, request_id, Outcome::Started, &done);
            }
            Err(why) => {
                self.audit(&alloc::format!("accounts: {who}'s {asked} was refused: {why}"));
                reply_outcome(ch, op, request_id, Outcome::Denied { retry: false }, &why);
            }
        }
    }

    /// **`AddAccount`**: the name and password checked, the home and its folders made — or an
    /// existing home adopted — and then the record, by `auth-service`. A home this made is taken
    /// away again if the record is refused, so a failure leaves nothing behind.
    fn add_account(&mut self, body: &[u8]) -> Result<String, String> {
        let r = parse_account_request(body).ok_or_else(|| String::from("the request does not read"))?;
        if !libusers::valid_name(r.username) {
            return Err(String::from(NAME_RULE));
        }
        if !libusers::valid_password(r.password) {
            return Err(String::from("a password is 1 to 128 bytes"));
        }
        // Checked, so ASCII: fit for a path and a log line.
        let name = String::from_utf8_lossy(r.username).into_owned();
        let listed = self.accounts().ok_or_else(|| String::from("the accounts could not be listed"))?;
        if listed.iter().any(|(n, _)| *n == name) {
            return Err(alloc::format!("an account named {name} already exists"));
        }
        let home = alloc::format!("/home/{name}");
        let made = match libfs::mkdir(self.root_ns, home.as_bytes()) {
            Ok(()) => true,
            Err(_) if libfs::is_dir(self.root_ns, home.as_bytes()) => false,
            Err(_) => return Err(alloc::format!("{home} could not be made")),
        };
        let root_ns = self.root_ns;
        let undo = || {
            if made {
                let _ = libfs::remove_tree(root_ns, home.as_bytes(), &mut |_, _| {});
            }
        };
        // **The folders a person's files go in** (`TODO(home-folders)`, resolved here for every
        // home an administrator adds): the same three `nxfiles`' sidebar offers.
        for folder in libfs::HOME_FOLDERS {
            let dir = libfs::join(home.as_bytes(), folder.as_bytes());
            if libfs::mkdir(root_ns, dir.as_bytes()).is_err() && !libfs::is_dir(root_ns, dir.as_bytes()) {
                undo();
                return Err(alloc::format!("{dir} could not be made"));
            }
        }
        let mut req = [0u8; 256];
        let asked = match build_account_request(&mut req, r.username, r.password) {
            Some(n) => self.ask_auth(OP_AUTH_ADD, &req[..n]).map(|_| ()),
            None => Err(String::from("the request does not fit")),
        };
        scrub(&mut req);
        match asked {
            Ok(()) if made => Ok(alloc::format!("added {name}, with a new home at {home}")),
            Ok(()) => Ok(alloc::format!("added {name}, adopting {home}, which was already there")),
            Err(why) => {
                undo();
                Err(why)
            }
        }
    }

    /// **`RemoveAccount`**: refused while the account is logged in, and when no account left
    /// could administer (`view_broker::accounts::refuse_removal`); then the record, by
    /// `auth-service`, and the home only if asked.
    fn remove_account(&mut self, body: &[u8]) -> Result<String, String> {
        let (name, home_too) = parse_remove_account(body).ok_or_else(|| String::from("the request does not read"))?;
        if !libusers::valid_name(name) {
            return Err(String::from(NAME_RULE));
        }
        let name = String::from_utf8_lossy(name).into_owned();
        let listed = self.accounts().ok_or_else(|| String::from("the accounts could not be listed"))?;
        let names: Vec<&str> = listed.iter().map(|(n, _)| n.as_str()).collect();
        let policy = self.read_policy();
        let policy = policy.as_ref().map_err(String::as_str);
        if let Some(why) = accounts::refuse_removal(&name, &names, &self.sessions, policy) {
            return Err(why);
        }
        self.ask_auth(OP_AUTH_REMOVE, name.as_bytes())?;
        let home = alloc::format!("/home/{name}");
        if !home_too {
            return Ok(alloc::format!("removed {name}, keeping {home}"));
        }
        Ok(match libfs::remove_tree(self.root_ns, home.as_bytes(), &mut |_, _| {}) {
            Ok(()) => alloc::format!("removed {name}, and {home} with it"),
            Err(_) if !libfs::is_dir(self.root_ns, home.as_bytes()) => {
                alloc::format!("removed {name}; there was no {home}")
            }
            Err(_) => alloc::format!("removed {name}, but {home} could not be removed"),
        })
    }

    /// **`SetPassword`**: an administrator setting someone's password, with no current one to
    /// prove — the grant is the proof.
    fn set_password(&mut self, body: &[u8]) -> Result<String, String> {
        let r = parse_account_request(body).ok_or_else(|| String::from("the request does not read"))?;
        if !libusers::valid_name(r.username) {
            return Err(String::from(NAME_RULE));
        }
        if !libusers::valid_password(r.password) {
            return Err(String::from("a password is 1 to 128 bytes"));
        }
        let name = String::from_utf8_lossy(r.username).into_owned();
        let mut req = [0u8; 256];
        let asked = match build_account_request(&mut req, r.username, r.password) {
            Some(n) => self.ask_auth(OP_AUTH_SET_PASSWORD, &req[..n]).map(|_| ()),
            None => Err(String::from("the request does not fit")),
        };
        scrub(&mut req);
        asked.map(|()| alloc::format!("set a new password for {name}"))
    }

    /// Bind this broker's own forwarding endpoint at `at` in `view_ns`, with `base` — how a grant
    /// gives a view an endpoint that knows which session it serves. `false` if it would not bind,
    /// or there is no endpoint to bind.
    fn bind_endpoint(&self, view_ns: u64, at: &[u8], base: &str) -> bool {
        // SAFETY: a namespace this broker made, a valid path and base, and an endpoint it holds.
        self.forwarding != 0
            && unsafe {
                syscall6(
                    SYS_NS_BIND,
                    view_ns,
                    at.as_ptr() as u64,
                    at.len() as u64,
                    self.forwarding,
                    base.as_ptr() as u64,
                    base.len() as u64,
                )
            } == 0
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
                // **Every disk not in use** (administration Part C.6): a mounted filesystem's
                // device, `init`'s root included, and the disk under it are left out, since a raw
                // write there lands underneath a live server. **Refused, not granted blind**, when
                // the storage service cannot say what is in use.
                Grant::Disks => {
                    let Some(in_use) = self.in_use() else {
                        close(view_ns);
                        return fail(self, &mut p, "the storage service could not say which disks are in use");
                    };
                    libsession::rebind_block_devices_except(self.root_ns, view_ns, &in_use);
                }
                // **Mounting and unmounting**: the storage service's admin endpoint, at
                // `/dev/storage/admin`. The service answers every request on a session opened
                // there, so holding this binding is the authority.
                Grant::Storage => {
                    let endpoint = self.storage_endpoint();
                    // SAFETY: a namespace this broker made, a valid path, and an endpoint it holds.
                    let bound = endpoint != 0
                        && unsafe {
                            syscall4(SYS_NS_BIND, view_ns, STORAGE_GRANT.as_ptr() as u64, STORAGE_GRANT.len() as u64, endpoint)
                        } == 0;
                    if !bound {
                        close(view_ns);
                        return fail(self, &mut p, "the storage service is not there to grant");
                    }
                }
                // **Changing the policy** (administration Part D.2): the broker's own forwarding
                // endpoint at `/dev/policy`, with the base `/policy/<session>`, so a resolve there
                // reaches the broker as this session's policy channel.
                Grant::Views => {
                    let base = alloc::format!("/policy/{}", self.clients[i].session);
                    if !self.bind_endpoint(view_ns, POLICY_GRANT, &base) {
                        close(view_ns);
                        return fail(self, &mut p, "the policy endpoint could not be granted");
                    }
                }
                // **Administering accounts** (administration Part D.3): the same endpoint at
                // `/dev/accounts`, with the base `/accounts/<session>`.
                Grant::Accounts => {
                    let base = alloc::format!("/accounts/{}", self.clients[i].session);
                    if !self.bind_endpoint(view_ns, ACCOUNTS_GRANT, &base) {
                        close(view_ns);
                        return fail(self, &mut p, "the accounts endpoint could not be granted");
                    }
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
            State::Password(mut p) => {
                p.handles.close();
                if let Some((_, mut pw)) = p.queued.take() {
                    scrub(&mut pw);
                }
            }
            State::Changing { mut current, mut new, .. } => {
                scrub(&mut current);
                scrub(&mut new);
            }
            _ => {}
        }
        self.held.forget(c.ch);
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
                self.exits.code(code, exit_kind != 0);
            }
        }
        self.pair_exits();
    }

    /// A program's life channel closed: it exited. Its code may not be queued yet.
    fn life_closed(&mut self, life: u64) {
        self.exits.closed(life);
        self.drain_notifications();
    }

    /// Pair exited programs with exit codes, first with first. **One exit per wake pairs
    /// correctly; two in the same wake can swap codes** — the residual of
    /// `TODO(child-exit-attribution)`, which `service-mgr` lives with too.
    fn pair_exits(&mut self) {
        loop {
            let clients = &self.clients;
            let running = |l: u64| clients.iter().any(|c| matches!(c.state, State::Running { life, .. } if life == l));
            let Some((life, code, crashed)) = self.exits.next(running) else {
                break;
            };
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

    /// What the wait set holds, by the most each may come to need.
    fn load(&self) -> Load {
        let (mut clients, mut singles) = (0, 0);
        for c in &self.clients {
            // A client whose request is over keeps only its channel; a program whose client has
            // gone, only its life channel. Anything else may yet hold both.
            if c.ch == 0 || matches!(c.state, State::Done) {
                singles += 1;
            } else {
                clients += 1;
            }
        }
        Load { supervisors: self.supervisors.len(), clients, singles: singles + self.endpoints.len() }
    }

    /// The soonest a held password may be checked.
    fn next_deadline(&self) -> u64 {
        self.held.next_due(&self.sessions, now_ns()).unwrap_or(u64::MAX)
    }

    /// Check every held password its session now allows, **one at a time and asking again after
    /// each** — a failure holds the rest of its session's.
    fn run_due(&mut self) {
        while let Some(ch) = self.held.take_ready(&self.sessions, now_ns()) {
            if let Some(i) = self.clients.iter().position(|c| c.ch == ch && ch != 0) {
                self.check(i);
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
    // Kept before `Ready` moves the endpoint to `init`: the `views` grant binds it into a view.
    // SAFETY: duplicating an endpoint this process holds, with every right it has.
    let forwarding = unsafe { syscall2(SYS_HANDLE_DUPLICATE, client_end, u64::MAX) };
    let forwarding = if forwarding > 0 { forwarding as u64 } else { 0 };
    if !send_ready(control, client_end) {
        kprint(b"view-broker: Ready send FAIL\n");
        exit(1);
    }
    let mut b = Broker {
        storage: Storage::default(),
        root_ns,
        notif,
        serve_end,
        forwarding,
        auth_ch: 0,
        auth_admin: 0,
        endpoints: Vec::new(),
        supervisors: Vec::new(),
        clients: Vec::new(),
        sessions: Sessions::new(),
        held: Held::default(),
        exits: Exits::default(),
        log: liblog::open_source(root_ns, b"/log/system/view-broker"),
    };
    loop {
        // SAFETY: WAIT_HANDLES holds MAX_WAIT_HANDLES slots, and `serve_resolve` admits a channel
        // only when the load's worst case — every client running a program — still fits
        // (`view_broker::slots`). `push` checks the bound all the same.
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
            for &(ch, _, _) in &b.endpoints {
                push(ch);
            }
            for c in &b.clients {
                if c.ch != 0 {
                    push(c.ch);
                }
                // A closed life channel stays ready: waiting on it again until its code arrived
                // would spin, reporting the same exit on every wake.
                if let State::Running { life, .. } = c.state
                    && !b.exits.is_closed(life)
                {
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
                } else if let Some(i) = b.endpoints.iter().position(|&(ch, _, _)| ch == h) {
                    b.serve_endpoint(i);
                } else if let Some(i) = b.clients.iter().position(|c| c.ch == h && h != 0) {
                    b.serve_client(i);
                } else if b.clients.iter().any(|c| matches!(c.state, State::Running { life, .. } if life == h)) {
                    // A life channel only ever signals its peer closing. The program holds the other
                    // end, so anything it sends on it is closed unread.
                    match recv(h) {
                        Err(()) => b.life_closed(h),
                        Ok(Some(_)) => {
                            for stray in received() {
                                close(stray);
                            }
                        }
                        Ok(None) => {}
                    }
                }
            }
        }
        b.run_due();
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"view-broker: PANIC\n");
    exit(1);
}
