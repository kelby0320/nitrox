//! `view-broker` — run a program in a **view**: its caller's namespace plus a profile's grants,
//! when `/system/views.toml` says the caller may.
//!
//! **What it holds, and so what a bug here reaches.** It is spawned by `service-mgr` with
//! `BIND_NAMESPACE`, which it needs to bind grants into the views it builds, and `SYSTEM_CLOCK`,
//! which it never uses and passes to a program only for the `clock` grant — a capability can be
//! given at spawn only by a parent that holds it. It inherits the root namespace, which is where
//! the grants come from: every block device, for `disks`. **And the root filesystem**, through
//! which it reads and replaces `/system/views.toml` and makes and removes `/home/<name>` for the
//! accounts it fronts. It never holds a session's ingredients — the caller hands it a copy of its
//! own namespace (`sys_ns_create(from)`), and it copies that again before binding anything, so the
//! caller cannot keep a handle to what it builds. It never touches a terminal: `with` reads the
//! password.
//!
//! **One forwarding endpoint**, which `service-mgr` binds at `/svc/views`: a login supervisor
//! resolves `/svc/views/session` for a channel to open and close sessions on, and binds the same
//! endpoint into each session at `/dev/views` with the base `/s/<session>`, so a process there
//! resolves a channel the broker already knows the session of. The `views` and `accounts` grants
//! bind it at `/dev/policy` and `/dev/accounts` the same way, each channel answering that
//! grant's ops alone. See `librsproto::views` for the ops.
//!
//! It binds only into namespaces it creates, and never registers itself (`userspace/CLAUDE.md`).

#![no_std]
#![no_main]

extern crate alloc;

mod admin;
mod wire;

use alloc::string::String;
use alloc::vec::Vec;
use libkern::{CTRL_OP_SHUTDOWN, KError, KIND_CHILD_EXITED, MAX_WAIT_HANDLES, Rights, SysCaps, scrub};
use libos::boot::Start;
use libos::control::{self, Control};
use libos::log::{self, Line};
use libos::{
    Carried, Deadline, Handle, MapRead, Memory, Namespace, Notify, NsMutable, NsReadOnly, Only, Process, WaitSet,
};
use librsproto::auth::{OP_AUTH_SET_PASSWORD, build_account_request};
use librsproto::namespace::parse_resolve_request;
use librsproto::storage::{OP_STORAGE_IN_USE, parse_in_use};
use librsproto::views::*;
use librsproto::{OP_NS_RESOLVE, OP_TTY_REDEEM, RS_FLAG_ERROR, RS_FLAG_REPLY, TTY_TOKEN_LEN, server};
use libstream::setup::{StageStreams, bootstrap_arg0, send_stage_setup};
use libstream::wire::{ByteSource, Record, TypeTag, Value, read_value};
use view_broker::exits::Exits;
use view_broker::pacing::{Held, MAX_FAILURES};
use view_broker::policy::{self, Auth, Decision, Grant};
use view_broker::sessions::Sessions;
use view_broker::slots::Load;
use view_broker::spawning::{StartError, Started, spawn_program};
use view_broker::suffix::{self, Suffix};
use wire::{Ch, Root, lookup, now_ns, recv, recv_full, reply_error, reply_outcome, send, wait_until};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Where the policy lives. Read for every request: it is small, and a stale copy is a bug.
const POLICY_PATH: &str = "/system/views.toml";

/// The handles a request brought: a copy of the caller's namespace, then whichever of its
/// streams and terminal it sent. `None` is absent.
#[derive(Default)]
struct Handles {
    ns: Option<Handle<Namespace, NsReadOnly>>,
    stdin: Option<Ch>,
    stdout: Option<Ch>,
    stderr: Option<Ch>,
    terminal: Option<Ch>,
}

impl Handles {
    fn close(&mut self) {
        *self = Handles::default();
    }
}

/// A program started in its view.
struct Launched {
    process: Handle<Process, Only>,
    life: Ch,
    view_ns: Handle<Namespace, NsMutable>,
    granted: Granted,
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
    /// **The terminal the request came from**, as `tty-server` named it when the broker redeemed
    /// the request's token — never as the caller did. A password that succeeds is remembered for
    /// it; `None`, and nothing is remembered.
    backend: Option<u32>,
}

enum State {
    Idle,
    Password(Pending),
    /// A `ChangePassword` held until the session's delay from its last failure has passed: the
    /// request to answer, the current password and the new one.
    Changing {
        request_id: u64,
        current: Vec<u8>,
        new: Vec<u8>,
    },
    Running {
        process: Handle<Process, Only>,
        life: Ch,
        view_ns: Handle<Namespace, NsMutable>,
        view: String,
        program: String,
        granted: Granted,
    },
    Done,
}

/// **What a view's grants bound in it**, for [`unbind_grants`]: each path the broker bound — not a
/// `/dev/log` its caller already had — and whether it rebound the disks.
#[derive(Default)]
struct Granted {
    paths: Vec<&'static str>,
    disks: bool,
}

/// **Undo what a view's grants bound** for whatever shares the view, a program that ignored a
/// request to stop included; a copy made with `sys_ns_create(from)` keeps them (`TODO(forcible-kill)`).
fn unbind_grants(view_ns: &Handle<Namespace, NsMutable>, granted: &Granted) {
    if granted.disks {
        libsession::withdraw_block_devices(view_ns);
    }
    for at in &granted.paths {
        let _ = view_ns.unbind(at);
    }
}

struct Client {
    /// `None` once the client has gone but its program is still running.
    ch: Option<Ch>,
    session: u64,
    state: State,
}

impl Client {
    /// The channel's handle value, a name for the pacing queue and the wait; `0` once it has gone.
    fn key(&self) -> u64 {
        self.ch.as_ref().map_or(0, |c| c.raw().0)
    }
}

/// Where the storage service mints an admin endpoint.
const STORAGE_ADMIN: &str = "/svc/storage/admin-endpoint";
/// How long the storage service may take to answer `InUse`; it bounds a wedged service.
const IN_USE_WAIT_NS: u64 = 5_000_000_000;

/// How long a token's redeem may take before the request is asked for a password instead.
const REDEEM_WAIT_NS: u64 = 2_000_000_000;

/// The storage service as this broker reaches it: an admin endpoint, which the `storage` grant
/// binds into a view, and an admin session of its own, on which it asks `InUse` before `disks`.
///
/// **Resolved when first needed**: `service-mgr` spawns the broker before the storage service. A
/// failure is not remembered, so a service that came up later is found the next time.
#[derive(Default)]
struct Storage {
    endpoint: Option<Ch>,
    session: Option<Ch>,
}

/// Which grant's endpoint a channel was resolved through.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Endpoint {
    Policy,
    Accounts,
}

struct Broker {
    storage: Storage,
    /// Endpoints for the `services`, `power` and `logs` grants, each held for the boot and
    /// **resolved when first needed**, as the storage service's is; `None` until then.
    services_admin: Option<Ch>,
    power: Option<Ch>,
    logs: Option<Ch>,
    root_ns: Root,
    notif: Handle<Notify, Only>,
    /// The control channel, for a shutdown's `CTRL_OP_SHUTDOWN`. `service --stop` never sends it:
    /// the broker is `essential`. `None` once `service-mgr` is gone.
    control: Option<Ch>,
    serve_end: Ch,
    /// **A duplicate of this broker's own forwarding endpoint** — the one `service-mgr` binds at
    /// `/svc/views` — kept to bind into a view with a base of the broker's choosing: the `views`
    /// grant's `/dev/policy`. `None` if it would not duplicate, and then that grant is refused
    /// rather than given without its binding.
    forwarding: Option<Ch>,
    auth_ch: Option<Ch>,
    /// An admin session on `auth-service` (`/svc/auth/admin`), opened on first need: the account
    /// list the policy and a removal are judged against, and every account write the accounts
    /// endpoint asks for. `None` until then, or after it failed.
    auth_admin: Option<Ch>,
    /// Channels resolved through `/dev/policy` or `/dev/accounts`, each with its session.
    endpoints: Vec<(Ch, u64, Endpoint)>,
    supervisors: Vec<Ch>,
    clients: Vec<Client>,
    sessions: Sessions,
    /// Passwords waiting to be checked, by client channel.
    held: Held,
    /// Closed life channels and `ChildExited` codes, each waiting for the other.
    exits: Exits,
    log: liblog::Logger,
}

// Each `*_GRANT` is where its grant is bound in a view; `SERVICES_ADMIN`, `POWER_ENDPOINT` and
// `LOG_READ_ENDPOINT` are where the service behind one mints its endpoint.
const STORAGE_GRANT: &str = "/dev/storage/admin";
const SERVICES_ADMIN: &str = "/svc/services/admin-endpoint";
const SERVICES_GRANT: &str = "/dev/services/admin";
const POWER_ENDPOINT: &str = "/svc/services/power-endpoint";
const POWER_GRANT: &str = "/dev/power";
const LOG_READ_ENDPOINT: &str = "/log/read-endpoint";
const LOGS_GRANT: &str = "/dev/logs";
/// **The kernel's log**, where the root namespace binds it and where the `kernel-log` grant binds a
/// snapshot of it in a view: one path, so `log --kernel` reads where a program in the root
/// namespace would.
const KERNEL_LOG: &str = "/dev/log";
const POLICY_GRANT: &str = "/dev/policy";
const ACCOUNTS_GRANT: &str = "/dev/accounts";

/// What an admin endpoint is looked up with.
fn endpoint_rights() -> Rights {
    Rights::SEND | Rights::RECV | Rights::WAIT | Rights::DUPLICATE | Rights::TRANSFER
}

/// The endpoint `slot` holds, looked up at `path` in `root` the first time it is asked for.
fn endpoint<'a>(root: &Root, slot: &'a mut Option<Ch>, path: &str) -> Option<&'a Ch> {
    if slot.is_none() {
        *slot = lookup(root, path, endpoint_rights());
    }
    slot.as_ref()
}

/// Bind the endpoint `slot` holds, looked up at `from` in `root`, at `at` in `view_ns`; whether it
/// bound.
fn bind_endpoint_at(
    root: &Root,
    slot: &mut Option<Ch>,
    from: &str,
    view_ns: &Handle<Namespace, NsMutable>,
    at: &str,
) -> bool {
    endpoint(root, slot, from).is_some_and(|e| view_ns.bind(at, e).is_ok())
}

impl Broker {
    /// **What is in use**: the registry ids the storage service's `InUse` names. `None` if it did
    /// not answer — and then the session is dropped, so the next ask opens a new one.
    fn in_use(&mut self) -> Option<Vec<u32>> {
        if self.storage.session.is_none() {
            self.storage.session = self.open_storage_session();
        }
        let session = self.storage.session.take()?;
        let asked = send(&session, OP_STORAGE_IN_USE, 1, 0, &[]);
        let deadline = now_ns().saturating_add(IN_USE_WAIT_NS);
        let mut ids = Vec::new();
        let answered = asked
            && loop {
                match recv(&session) {
                    Ok(Some((op, 1, body, _))) if op == OP_STORAGE_IN_USE => {
                        break parse_in_use(&body, |id| ids.push(id));
                    }
                    // A stray is dropped; nothing else is asked on this session.
                    Ok(Some(_)) => {}
                    Ok(None) if wait_until(&session, deadline) => {}
                    _ => break false,
                }
            };
        if !answered {
            return None;
        }
        self.storage.session = Some(session);
        Some(ids)
    }

    /// An admin session of the broker's own: the admin endpoint bound in a namespace made for the
    /// purpose, and resolved there. `None` if there is none to open.
    fn open_storage_session(&mut self) -> Option<Ch> {
        let endpoint = endpoint(&self.root_ns, &mut self.storage.endpoint, STORAGE_ADMIN)?;
        let ns = Handle::<Namespace, NsMutable>::create().ok()?;
        ns.bind("/admin", endpoint).ok()?;
        lookup(&ns, "/admin", Rights::SEND | Rights::RECV | Rights::WAIT)
    }

    /// **Which terminal a token was minted for**, asked of `tty-server` over a terminal this
    /// broker resolves itself — never over one a caller sent, which could be a channel the caller
    /// serves. Resolved for the one question and closed after, since every
    /// terminal is a slot in the server's wait set. `None` if the token names nothing or the server
    /// does not answer in time: then the request is asked for a password, which is safe.
    fn redeem(&self, token: &[u8; TTY_TOKEN_LEN]) -> Option<u32> {
        let tty: Ch = lookup(&self.root_ns, "/dev/tty", Rights::SEND | Rights::RECV | Rights::WAIT)?;
        let deadline = now_ns().saturating_add(REDEEM_WAIT_NS);
        send(&tty, OP_TTY_REDEEM, 1, 0, token)
            .then(|| {
                loop {
                    match recv_full(&tty) {
                        // An error reply is not an answer, by its flag: a refusal read as an id
                        // would make every refused token name one terminal, and a window opened
                        // under it would cover them all.
                        Ok(Some((op, 1, flags, body, _))) if op == OP_TTY_REDEEM => {
                            break (flags & RS_FLAG_ERROR == 0)
                                .then(|| <[u8; 4]>::try_from(body.as_slice()).ok().map(u32::from_le_bytes))
                                .flatten();
                        }
                        // A stray, or the error reply: nothing else is asked on this terminal.
                        Ok(Some(_)) => {}
                        Ok(None) if wait_until(&tty, deadline) => {}
                        _ => break None,
                    }
                }
            })
            .flatten()
    }

    /// Log a record: every request, failure and exit, and never a password.
    fn audit(&self, what: &str) {
        self.log.info(what);
    }

    fn principal(&self, session: u64) -> Option<String> {
        self.sessions.get(session).map(|s| s.principal.clone())
    }

    fn serve_resolve(&mut self) {
        let Ok(Some((op, request_id, body, _sent))) = recv(&self.serve_end) else {
            return;
        };
        let asked = match parse_resolve_request(&body) {
            Some(r) if op == OP_NS_RESOLVE => suffix::parse(r.suffix),
            _ => Suffix::Unknown,
        };
        let (session, endpoint) = match asked {
            Suffix::Supervisor => (None, None),
            Suffix::Client(id) if self.sessions.get(id).is_some() => (Some(id), None),
            // The policy endpoint, as the `views` grant binds it: a channel of its own, on which
            // only `Show` and `Install` are answered.
            Suffix::Policy(id) if self.sessions.get(id).is_some() => (Some(id), Some(Endpoint::Policy)),
            // The accounts endpoint, as the `accounts` grant binds it.
            Suffix::Accounts(id) if self.sessions.get(id).is_some() => (Some(id), Some(Endpoint::Accounts)),
            _ => {
                reply_error(&self.serve_end, OP_NS_RESOLVE, request_id, KError::NotFound);
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
            reply_error(&self.serve_end, OP_NS_RESOLVE, request_id, KError::WouldBlock);
            return;
        }
        let Ok((client_end, ours)) = Handle::pair(4) else {
            reply_error(&self.serve_end, OP_NS_RESOLVE, request_id, KError::KernelError);
            return;
        };
        if !server::reply_channel(&self.serve_end, request_id, client_end) {
            return;
        }
        match (session, endpoint) {
            (None, _) => self.supervisors.push(ours),
            (Some(id), Some(e)) => self.endpoints.push((ours, id, e)),
            (Some(id), None) => self.clients.push(Client { ch: Some(ours), session: id, state: State::Idle }),
        }
    }

    fn serve_supervisor(&mut self, i: usize) {
        let (op, request_id, body, _sent) = match recv(&self.supervisors[i]) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(()) => {
                self.supervisors.remove(i);
                return;
            }
        };
        match op {
            OP_VIEWS_OPEN_SESSION => {
                let principal = match core::str::from_utf8(&body) {
                    Ok(p) if !p.is_empty() && p.len() <= 64 => p,
                    _ => return reply_error(&self.supervisors[i], op, request_id, KError::InvalidArgument),
                };
                let id = self.sessions.open(principal);
                let mut out = [0u8; 8];
                let n = build_session_id(&mut out, id).unwrap_or(0);
                // Logged before the reply: the supervisor builds the session's namespace on the
                // reply and says so, and a gate waiting for this line must not see it after that.
                Line::new().s(b"view-broker: session ").u(id).s(b" opened").end();
                let _ = send(&self.supervisors[i], op, request_id, RS_FLAG_REPLY, &out[..n]);
            }
            OP_VIEWS_CLOSE_SESSION => {
                let Some(id) = parse_session_id(&body) else {
                    return reply_error(&self.supervisors[i], op, request_id, KError::InvalidArgument);
                };
                self.end_session(id);
                let _ = send(&self.supervisors[i], op, request_id, RS_FLAG_REPLY, &[]);
            }
            _ => reply_error(&self.supervisors[i], op, request_id, KError::Unsupported),
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
            let key = c.key();
            match &mut c.state {
                State::Running { process, view_ns, granted, .. } => {
                    let _ = process.terminate();
                    unbind_grants(view_ns, granted);
                }
                State::Password(p) => {
                    p.handles.close();
                    self.held.forget(key);
                    if let Some((request_id, mut pw)) = p.queued.take() {
                        scrub(&mut pw);
                        let why = "this session has ended";
                        if let Some(ch) = &c.ch {
                            reply_outcome(ch, OP_VIEWS_PASSWORD, request_id, Outcome::Denied { retry: false }, why);
                        }
                    }
                    c.state = State::Done;
                }
                State::Changing { request_id, current, new } => {
                    scrub(current);
                    scrub(new);
                    self.held.forget(key);
                    let why = "this session has ended";
                    if let Some(ch) = &c.ch {
                        let denied = Outcome::Denied { retry: false };
                        reply_outcome(ch, OP_VIEWS_CHANGE_PASSWORD, *request_id, denied, why);
                    }
                    c.state = State::Done;
                }
                _ => {}
            }
        }
        // Its grants' channels close with it: nothing more is heard under its bases.
        let (gone, kept): (Vec<_>, Vec<_>) = self.endpoints.drain(..).partition(|e| e.1 == id);
        self.endpoints = kept;
        drop(gone);
        Line::new().s(b"view-broker: session ").u(id).s(b" ended").end();
    }

    fn serve_client(&mut self, i: usize) {
        let Some(ch) = &self.clients[i].ch else { return };
        let (op, request_id, mut body, sent) = match recv(ch) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(()) => return self.client_gone(i),
        };
        // Only a request carries handles: it takes them, and whatever came with anything else
        // closes as `sent` drops, here, not in each arm, where one that forgot would keep them.
        let carried = match op {
            OP_VIEWS_REQUEST => sent,
            _ => {
                drop(sent);
                Carried::new()
            }
        };
        let session = self.clients[i].session;
        let Some(principal) = self.principal(session) else {
            drop(carried);
            scrub(&mut body);
            return self.tell(i, op, request_id, Outcome::Denied { retry: false }, "this session has ended");
        };
        match op {
            OP_VIEWS_REQUEST => self.request(i, request_id, &body, carried, &principal),
            OP_VIEWS_PASSWORD => self.password(i, request_id, core::mem::take(&mut body)),
            OP_VIEWS_CHANGE_PASSWORD => self.change_password(i, request_id, &body),
            OP_VIEWS_ACCOUNTS => self.list_accounts(i, request_id),
            OP_VIEWS_STOP => {
                if let State::Running { process, .. } = &self.clients[i].state {
                    let _ = process.terminate();
                }
                self.answer(i, op, request_id, &[]);
            }
            OP_VIEWS_LIST => self.list(i, request_id, &principal),
            // `with --forget`: every remembered password of the session, every terminal's.
            // Forgetting more than asked is the safe direction.
            OP_VIEWS_FORGET => {
                if let Some(s) = self.sessions.get_mut(session) {
                    s.grace.forget();
                }
                self.audit(&alloc::format!("view: {principal} — remembered passwords forgotten"));
                self.answer(i, op, request_id, &[]);
            }
            // What a `Request` would be answered, running nothing: the desktop asks before it
            // closes a window for a Restart or a Shut down. Not audited, as `List` is not: it
            // tells the session only what `List` does.
            OP_VIEWS_DECIDE => {
                let question = parse_decide(&body)
                    .and_then(|(v, p)| Some((core::str::from_utf8(v).ok()?, core::str::from_utf8(p).ok()?)));
                let (o, why) = match (question, self.read_policy()) {
                    (None, _) => (Outcome::Denied { retry: false }, String::from("the question does not read")),
                    (Some(_), Err(e)) => (Outcome::Denied { retry: false }, e),
                    (Some((view, program)), Ok(policy)) => policy.would(&principal, view, program),
                };
                self.tell(i, op, request_id, o, &why);
            }
            OP_VIEWS_CHECK => {
                let verdict = self.judge(&body).map(|_| ());
                match verdict {
                    Ok(()) => self.tell(i, op, request_id, Outcome::Started, "the policy is valid"),
                    Err(e) => self.tell(i, op, request_id, Outcome::Denied { retry: false }, &e),
                }
            }
            _ => self.refuse(i, op, request_id, KError::Unsupported),
        }
        scrub(&mut body);
    }

    /// Answer client `i` with `body`, if its channel is still there.
    fn answer(&self, i: usize, op: u16, request_id: u64, body: &[u8]) {
        if let Some(ch) = &self.clients[i].ch {
            let _ = send(ch, op, request_id, RS_FLAG_REPLY, body);
        }
    }

    /// Answer client `i` with an [`Outcome`], if its channel is still there.
    fn tell(&self, i: usize, op: u16, request_id: u64, outcome: Outcome, reason: &str) {
        if let Some(ch) = &self.clients[i].ch {
            reply_outcome(ch, op, request_id, outcome, reason);
        }
    }

    /// Answer client `i` with an error, if its channel is still there.
    fn refuse(&self, i: usize, op: u16, request_id: u64, err: KError) {
        if let Some(ch) = &self.clients[i].ch {
            reply_error(ch, op, request_id, err);
        }
    }

    fn read_policy(&self) -> Result<policy::Policy, String> {
        let bytes = libfs::read(&self.root_ns, POLICY_PATH)
            .map_err(|_| String::from("there is no policy at /system/views.toml"))?;
        let text = core::str::from_utf8(&bytes).map_err(|_| String::from("the policy is not text"))?;
        policy::parse(text).map_err(|e| alloc::format!("the policy does not read — {e}"))
    }

    fn request(&mut self, i: usize, request_id: u64, body: &[u8], mut sent: Carried, principal: &str) {
        let deny = |this: &Broker, sent: Carried, reason: &str| {
            drop(sent);
            this.tell(i, OP_VIEWS_REQUEST, request_id, Outcome::Denied { retry: false }, reason);
        };
        if !matches!(self.clients[i].state, State::Idle) {
            return deny(self, sent, "this channel already made a request");
        }
        let Some(r) = parse_request(body) else {
            return deny(self, sent, "the request does not read");
        };
        if sent.len() != r.handle_count() {
            return deny(self, sent, "the request's handles do not match what it says it carries");
        }
        let (Ok(view), Ok(program)) = (core::str::from_utf8(r.view), core::str::from_utf8(r.program)) else {
            return deny(self, sent, "a view and a program are names");
        };
        if !policy::is_bare_name(program) {
            return deny(self, sent, "a program is a bare name, resolved under /bin");
        }
        let mut args = Vec::new();
        for a in r.args() {
            match core::str::from_utf8(a) {
                Ok(s) => args.push(String::from(s)),
                Err(_) => return deny(self, sent, "an argument is not text"),
            }
        }
        let mut hs = Handles { ns: sent.take(0), ..Handles::default() };
        let mut k = 1;
        for (bit, slot) in [
            (REQ_STDIN, &mut hs.stdin),
            (REQ_STDOUT, &mut hs.stdout),
            (REQ_STDERR, &mut hs.stderr),
            (REQ_TERMINAL, &mut hs.terminal),
        ] {
            if r.handles & bit != 0 {
                *slot = sent.take(k);
                k += 1;
            }
        }
        let policy = match self.read_policy() {
            Ok(p) => p,
            Err(e) => {
                Line::new().s(b"view-broker: ").s(e.as_bytes()).end();
                hs.close();
                return self.tell(i, OP_VIEWS_REQUEST, request_id, Outcome::Denied { retry: false }, &e);
            }
        };
        let what = alloc::format!("{principal} {view} {program}");
        let token = r.token;
        let (grants, auth) = match policy.decide(principal, view, program) {
            Decision::Deny(reason) => {
                self.audit(&alloc::format!("view: {what} — denied: {reason}"));
                hs.close();
                return self.tell(i, OP_VIEWS_REQUEST, request_id, Outcome::Denied { retry: false }, &reason);
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
            backend: None,
        };
        let mut pending = pending;
        match auth {
            Auth::Password => {
                // **Which terminal, from `tty-server` — not from the caller.** Redeemed only when a
                // password would be asked: a request that needs none says nothing about a window.
                pending.backend = token.and_then(|t| self.redeem(&t));
                let session = self.clients[i].session;
                let remembered = pending
                    .backend
                    .is_some_and(|b| self.sessions.get(session).is_some_and(|s| s.grace.covers(b, view, now_ns())));
                if remembered {
                    self.audit(&alloc::format!("view: {what} — allowed, within the grace period"));
                    return self.start(i, OP_VIEWS_REQUEST, request_id, pending, principal);
                }
                self.audit(&alloc::format!("view: {what} — allowed, asking for a password"));
                self.clients[i].state = State::Password(pending);
                self.tell(i, OP_VIEWS_REQUEST, request_id, Outcome::NeedPassword, "");
            }
            Auth::None => self.start(i, OP_VIEWS_REQUEST, request_id, pending, principal),
        }
    }

    /// A password for a request waiting on one: held, and checked by [`Broker::run_due`] once
    /// the session's delay from its last failure has passed.
    fn password(&mut self, i: usize, request_id: u64, mut pw: Vec<u8>) {
        let (key, session) = (self.clients[i].key(), self.clients[i].session);
        let refuse = |this: &Broker, pw: &mut Vec<u8>, why: &str| {
            scrub(pw);
            this.tell(i, OP_VIEWS_PASSWORD, request_id, Outcome::Denied { retry: false }, why);
        };
        let State::Password(p) = &mut self.clients[i].state else {
            return refuse(self, &mut pw, "no request is waiting for a password");
        };
        if p.queued.is_some() {
            return refuse(self, &mut pw, "a password is already waiting to be checked");
        }
        p.queued = Some((request_id, pw));
        self.held.hold(key, session);
    }

    /// Whether `password` is `principal`'s, asked of `auth-service`'s oracle, opened on first need.
    fn authenticate(&mut self, principal: &str, password: &[u8]) -> bool {
        if self.auth_ch.is_none() {
            self.auth_ch = lookup(&self.root_ns, "/svc/auth", Rights::SEND | Rights::RECV | Rights::WAIT);
        }
        let mut home = [0u8; 256];
        self.auth_ch
            .as_ref()
            .is_some_and(|ch| libsession::authenticate_on(ch, principal.as_bytes(), password, &mut home).is_some())
    }

    /// Check client `i`'s held password: its session's delay has passed.
    fn check(&mut self, i: usize) {
        if matches!(self.clients[i].state, State::Changing { .. }) {
            return self.check_change(i);
        }
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
            let State::Password(mut p) = core::mem::replace(&mut self.clients[i].state, State::Idle) else {
                return;
            };
            // **Decided again**, since the password can come long after the request: a policy
            // installed meanwhile must not be outrun. Allowed with other grants is asked again.
            let decided = self.read_policy().and_then(|policy| match policy.decide(&principal, &p.view, &p.program) {
                Decision::Allow { profile, .. } if profile.grants == p.grants => Ok(()),
                Decision::Allow { .. } => Err(String::from("the view's grants changed while its password was asked")),
                Decision::Deny(reason) => Err(reason),
            });
            if let Err(reason) = decided {
                p.handles.close();
                self.clients[i].state = State::Done;
                self.audit(&alloc::format!("view: {what} — denied when its password came: {reason}"));
                return self.tell(i, OP_VIEWS_PASSWORD, request_id, Outcome::Denied { retry: false }, &reason);
            }
            // Remembered for the terminal it was typed at, and for this view.
            if let (Some(b), Some(s)) = (p.backend, self.sessions.get_mut(session)) {
                s.grace.remember(b, &p.view, now_ns());
            }
            self.start(i, OP_VIEWS_PASSWORD, request_id, p, &principal);
            return;
        }
        p.failures += 1;
        let failures = p.failures;
        // From when the check ended, not when the wake that ran it began: the delay is the gap
        // between one answer and the next check. **A refused password forgets every window of the
        // session**: someone at it does not know the password, so nothing typed earlier is assumed.
        if let Some(s) = self.sessions.get_mut(session) {
            s.pacing.failed(now_ns());
            s.grace.forget();
        }
        self.audit(&alloc::format!("view: {what} — wrong password ({failures} of {MAX_FAILURES})"));
        if failures >= MAX_FAILURES {
            if let State::Password(p) = &mut self.clients[i].state {
                p.handles.close();
            }
            self.clients[i].state = State::Done;
            self.tell(i, OP_VIEWS_PASSWORD, request_id, Outcome::Denied { retry: false }, "three wrong passwords");
        } else {
            self.tell(i, OP_VIEWS_PASSWORD, request_id, Outcome::Denied { retry: true }, "wrong password");
        }
    }

    /// **`ChangePassword`**: the new password checked against the rules here, and the request
    /// **held**, as a `Password` is, until the session's delay from its last failure has passed —
    /// so a wrong current password here and a wrong one for `with` share one pace.
    /// [`Broker::check_change`] answers it.
    fn change_password(&mut self, i: usize, request_id: u64, body: &[u8]) {
        let (key, session) = (self.clients[i].key(), self.clients[i].session);
        let refuse = |this: &Broker, why: &str| {
            this.tell(i, OP_VIEWS_CHANGE_PASSWORD, request_id, Outcome::Denied { retry: false }, why);
        };
        if !matches!(self.clients[i].state, State::Idle) {
            return refuse(self, "this channel is already busy with a request");
        }
        let Some((current, new)) = parse_password_change(body) else {
            return refuse(self, "the request does not read");
        };
        if !libusers::valid_password(new) {
            return refuse(self, "a password is 1 to 128 bytes");
        }
        self.clients[i].state = State::Changing { request_id, current: current.to_vec(), new: new.to_vec() };
        self.held.hold(key, session);
    }

    /// A held `ChangePassword`, its session's delay passed: the current password checked as the
    /// session's principal's, and on success the new one set. **A wrong current password is a
    /// failure like any other**: it holds the session's next check, on whichever request.
    fn check_change(&mut self, i: usize) {
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
                self.tell(i, OP_VIEWS_CHANGE_PASSWORD, request_id, Outcome::Started, "your password is changed");
            }
            Err(why) => {
                self.audit(&alloc::format!("accounts: {principal}'s password change was refused: {why}"));
                self.tell(i, OP_VIEWS_CHANGE_PASSWORD, request_id, Outcome::Denied { retry: false }, &why);
            }
        }
    }

    /// Bind this broker's forwarding endpoint at `at` in `view_ns`, with `base` naming the session
    /// it serves. `false` if it would not bind, or there is none.
    fn bind_forwarding(&self, view_ns: &Handle<Namespace, NsMutable>, at: &str, base: &str) -> bool {
        self.forwarding.as_ref().is_some_and(|f| view_ns.bind_server(at, f.as_ref(), base).is_ok())
    }

    fn start(&mut self, i: usize, op: u16, request_id: u64, mut p: Pending, principal: &str) {
        let what = alloc::format!("{principal} {} {}", p.view, p.program);
        let started = self.launch(self.clients[i].session, &mut p);
        let launched = match started {
            Ok(launched) => launched,
            Err(why) => {
                p.handles.close();
                self.clients[i].state = State::Done;
                self.audit(&alloc::format!("view: {what} — could not start: {why}"));
                return self.tell(i, op, request_id, Outcome::Denied { retry: false }, why);
            }
        };
        self.audit(&alloc::format!("view: {what} — started"));
        let Launched { process, life, view_ns, granted } = launched;
        self.clients[i].state = State::Running { process, life, view_ns, view: p.view, program: p.program, granted };
        self.tell(i, op, request_id, Outcome::Started, "");
    }

    /// Build `p`'s view and start its program in it. The view and everything made on the way are
    /// closed on a refusal.
    fn launch(&mut self, session: u64, p: &mut Pending) -> Result<Launched, &'static str> {
        // **Copied again, never used as sent** — whoever sent a namespace may still hold it.
        let copied = p.handles.ns.take().map(|ns| ns.copy());
        let Some(Ok(view_ns)) = copied else {
            return Err("could not copy the caller's namespace");
        };
        let mut syscaps = SysCaps::empty();
        let mut kernel_log_ours = false;
        for g in &p.grants {
            match g {
                // Every disk not in use: a mounted filesystem's device, `init`'s root included,
                // and the disk under it are left out, since a raw write there lands underneath a
                // live server. Refused, not granted blind, when the storage service cannot say
                // what is in use.
                Grant::Disks => {
                    let Some(in_use) = self.in_use() else {
                        return Err("the storage service could not say which disks are in use");
                    };
                    libsession::share_block_devices(&self.root_ns, &view_ns, &in_use);
                }
                // The service answers every request on a session opened at the binding, so
                // holding it is the authority.
                Grant::Storage => {
                    let slot = &mut self.storage.endpoint;
                    if !bind_endpoint_at(&self.root_ns, slot, STORAGE_ADMIN, &view_ns, STORAGE_GRANT) {
                        return Err("the storage service is not there to grant");
                    }
                }
                // As for storage, bar an essential service's stop, which `service-mgr` refuses.
                Grant::Services => {
                    let slot = &mut self.services_admin;
                    if !bind_endpoint_at(&self.root_ns, slot, SERVICES_ADMIN, &view_ns, SERVICES_GRANT) {
                        return Err("service-mgr's admin endpoint is not there to grant");
                    }
                }
                // A session opened here takes `Shutdown` and nothing else.
                Grant::Power => {
                    if !bind_endpoint_at(&self.root_ns, &mut self.power, POWER_ENDPOINT, &view_ns, POWER_GRANT) {
                        return Err("service-mgr's power endpoint is not there to grant");
                    }
                }
                // Any resolve here opens a read session.
                Grant::Logs => {
                    if !bind_endpoint_at(&self.root_ns, &mut self.logs, LOG_READ_ENDPOINT, &view_ns, LOGS_GRANT) {
                        return Err("the logging service's read endpoint is not there to grant");
                    }
                }
                // The kernel answers a resolve of `/dev/log` with a fresh snapshot, so the one
                // taken here is the log as this view is made, bound at the same path as a direct
                // handle — readable and sizeable, and nothing more.
                Grant::KernelLog => {
                    let direct = Rights::TRANSFER | Rights::DUPLICATE;
                    let snapshot: Option<Handle<Memory, MapRead>> =
                        lookup(&self.root_ns, KERNEL_LOG, Rights::MAP_READ | Rights::INSPECT | direct);
                    let bind = snapshot.as_ref().map(|s| view_ns.bind(KERNEL_LOG, s));
                    // A caller that already binds `/dev/log` keeps its own.
                    kernel_log_ours = matches!(bind, Some(Ok(())));
                    let kept = matches!(&bind, Some(Err(e)) if e.code() == KError::AlreadyExists);
                    drop(snapshot); // The bind took its own reference.
                    if !kernel_log_ours && !kept {
                        return Err("the kernel's log is not there to grant");
                    }
                }
                // `SYSTEM_CLOCK` in the spawn below, which the kernel intersects with the broker's
                // own. Binds nothing.
                Grant::Clock => syscaps |= SysCaps::SYSTEM_CLOCK,
                // The broker's own endpoint with the base `/policy/<session>`, so a resolve there
                // reaches the broker as this session's policy channel.
                Grant::Views => {
                    let base = alloc::format!("/policy/{session}");
                    if !self.bind_forwarding(&view_ns, POLICY_GRANT, &base) {
                        return Err("the policy endpoint could not be granted");
                    }
                }
                Grant::Accounts => {
                    let base = alloc::format!("/accounts/{session}");
                    if !self.bind_forwarding(&view_ns, ACCOUNTS_GRANT, &base) {
                        return Err("the accounts endpoint could not be granted");
                    }
                }
            }
        }
        let paths = p.grants.iter().filter_map(|g| match g {
            Grant::Storage => Some(STORAGE_GRANT),
            Grant::Services => Some(SERVICES_GRANT),
            Grant::Power => Some(POWER_GRANT),
            Grant::Logs => Some(LOGS_GRANT),
            Grant::Views => Some(POLICY_GRANT),
            Grant::Accounts => Some(ACCOUNTS_GRANT),
            Grant::KernelLog => kernel_log_ours.then_some(KERNEL_LOG),
            Grant::Disks | Grant::Clock => None,
        });
        let granted = Granted { paths: paths.collect(), disks: p.grants.contains(&Grant::Disks) };
        let started = spawn_program(&self.root_ns, &p.program, &view_ns, bootstrap_arg0(true), syscaps)
            .map_err(StartError::reason)?;
        let env = view_env(&p.env, &p.view);
        let mut argv: Vec<&str> = Vec::with_capacity(1 + p.args.len());
        argv.push(&p.program);
        argv.extend(p.args.iter().map(|s| s.as_str()));
        let streams = StageStreams {
            stdin: p.handles.stdin.take(),
            stdout: p.handles.stdout.take(),
            stderr: p.handles.stderr.take(),
        };
        // The setup message takes them, sent or closed.
        let sent = send_stage_setup(&started.setup, streams, p.handles.terminal.take(), &argv, &env, Deadline::NEVER);
        let Started { process, setup, life } = started;
        drop(setup);
        if sent.is_err() {
            // The program is running without its streams; ask it to stop, and report it.
            let _ = process.terminate();
        }
        Ok(Launched { process, life, view_ns, granted })
    }

    fn list(&self, i: usize, request_id: u64, principal: &str) {
        let policy = match self.read_policy() {
            Ok(p) => p,
            Err(_) => return self.refuse(i, OP_VIEWS_LIST, request_id, KError::InvalidArgument),
        };
        let mut out = [0u8; 2048];
        let mut at = 2;
        let rows = policy.rows_for(principal);
        for (i, (view, run, password)) in rows.iter().enumerate() {
            let row = Row { view: view.as_bytes(), run: run.as_bytes(), password: *password };
            if push_row(&mut out, &mut at, &row).is_none() {
                // Said, since the reply cannot: a cut listing reads as whole.
                Line::new()
                    .s(b"view-broker: a listing cut at ")
                    .u(i as u64)
                    .s(b" of ")
                    .u(rows.len() as u64)
                    .s(b" rows, the rest past 2 KiB")
                    .end();
                break;
            }
        }
        self.answer(i, OP_VIEWS_LIST, request_id, &out[..at]);
    }

    /// A client's channel closed. A program it left running is asked to stop.
    fn client_gone(&mut self, i: usize) {
        let c = self.clients.remove(i);
        let key = c.key();
        match c.state {
            State::Running { process, life, view_ns, granted, .. } => {
                let _ = process.terminate();
                let state =
                    State::Running { process, life, view_ns, view: String::new(), program: String::new(), granted };
                self.clients.push(Client { ch: None, session: c.session, state });
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
        self.held.forget(key);
    }

    fn drain_notifications(&mut self) {
        while let Ok(Some(n)) = self.notif.recv() {
            let libkern::Notification { kind, body } = n;
            if kind == KIND_CHILD_EXITED {
                let exit_kind = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                let code = i32::from_le_bytes([body[8], body[9], body[10], body[11]]);
                self.exits.code(code, exit_kind != 0);
            }
        }
        self.pair_exits();
    }

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
            let running =
                |l: u64| clients.iter().any(|c| matches!(&c.state, State::Running { life, .. } if life.raw().0 == l));
            let Some((life, code, crashed)) = self.exits.next(running) else {
                break;
            };
            let Some(i) = self
                .clients
                .iter()
                .position(|c| matches!(&c.state, State::Running { life: l, .. } if l.raw().0 == life))
            else {
                continue;
            };
            let session = self.clients[i].session;
            let State::Running { process, life, view_ns, view, program, granted } =
                core::mem::replace(&mut self.clients[i].state, State::Done)
            else {
                continue;
            };
            // **Unbound before the namespace is let go**: a process the program started shares it.
            unbind_grants(&view_ns, &granted);
            drop(process);
            drop(life);
            drop(view_ns);
            let principal = self.principal(session).unwrap_or_default();
            self.audit(&alloc::format!("view: {principal} {view} {program} — exited, code {code}"));
            if let Some(ch) = &self.clients[i].ch {
                let mut body = [0u8; EXITED_LEN];
                let n = build_exited(&mut body, code, crashed).unwrap_or(0);
                let _ = send(ch, OP_VIEWS_EXITED, 0, 0, &body[..n]);
            } else {
                self.clients.remove(i);
            }
        }
    }

    fn load(&self) -> Load {
        let (mut clients, mut singles) = (0, 0);
        for c in &self.clients {
            // A client whose request is over keeps only its channel; a program whose client has
            // gone, only its life channel. Anything else may yet hold both.
            if c.ch.is_none() || matches!(c.state, State::Done) {
                singles += 1;
            } else {
                clients += 1;
            }
        }
        Load { supervisors: self.supervisors.len(), clients, singles: singles + self.endpoints.len() }
    }

    fn next_deadline(&self) -> u64 {
        self.held.next_due(&self.sessions, now_ns()).unwrap_or(u64::MAX)
    }

    /// Check every held password its session now allows, **one at a time and asking again after
    /// each** — a failure holds the rest of its session's.
    fn run_due(&mut self) {
        while let Some(ch) = self.held.take_ready(&self.sessions, now_ns()) {
            if let Some(i) = self.clients.iter().position(|c| c.key() == ch && ch != 0) {
                self.check(i);
            }
        }
    }
}

/// The caller's environment with `view` set to the view the program runs in.
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

/// Add `h` to `set`, noting its handle value at the index it took.
fn watch<'a, T: 'a, M: 'a>(
    set: &mut WaitSet<'a, MAX_WAIT_HANDLES>,
    raws: &mut [u64; MAX_WAIT_HANDLES],
    h: &'a Handle<T, M>,
) {
    if let Ok(i) = set.add(h) {
        raws[i] = h.raw().0;
    }
}

impl Broker {
    /// The life channel of the running program whose handle value is `raw`.
    fn life(&self, raw: u64) -> Option<&Ch> {
        self.clients.iter().find_map(|c| match &c.state {
            State::Running { life, .. } if life.raw().0 == raw => Some(life),
            _ => None,
        })
    }

    fn serve_loop(&mut self) -> ! {
        loop {
            // Every channel is in the wait set, and `serve_resolve` admits one only when the
            // load's worst case — every client running a program — still fits
            // (`view_broker::slots`). `watch` checks the bound all the same.
            let mut raws = [0; MAX_WAIT_HANDLES];
            let deadline = Deadline::at(self.next_deadline());
            let signalled = {
                let mut set = WaitSet::new();
                watch(&mut set, &mut raws, &self.serve_end);
                watch(&mut set, &mut raws, &self.notif);
                if let Some(c) = &self.control {
                    watch(&mut set, &mut raws, c);
                }
                for s in &self.supervisors {
                    watch(&mut set, &mut raws, s);
                }
                for (ch, _, _) in &self.endpoints {
                    watch(&mut set, &mut raws, ch);
                }
                for c in &self.clients {
                    if let Some(ch) = &c.ch {
                        watch(&mut set, &mut raws, ch);
                    }
                    // A closed life channel stays ready: waiting on it again until its code arrived
                    // would spin, reporting the same exit on every wake.
                    if let State::Running { life, .. } = &c.state
                        && !self.exits.is_closed(life.raw().0)
                    {
                        watch(&mut set, &mut raws, life);
                    }
                }
                set.wait(deadline)
            };
            if let Ok(signalled) = signalled {
                self.dispatch(signalled.iter().map(|(j, _)| raws[j]));
            }
            self.run_due();
        }
    }

    /// Serve each channel in `ready`, by its handle value. Resolves last: a new client is admitted
    /// by what the wait set holds, so every closed channel this wake reports is let go first. In
    /// the wait set's order the forwarding endpoint, its first slot, would come before them, and a
    /// client arriving in the same wake as others' closes would be refused `WouldBlock` with its
    /// slots about to free.
    fn dispatch(&mut self, ready: impl Iterator<Item = u64>) {
        let mut resolves = false;
        for h in ready {
            if h == self.serve_end.raw().0 {
                resolves = true;
            } else if h == self.notif.raw().0 {
                self.drain_notifications();
            } else if self.control.as_ref().is_some_and(|c| c.raw().0 == h) {
                // Nothing to save on the way out: every write to the policy file is made whole
                // before it is answered, as `auth-service`'s to the users file are.
                if control::recv(&mut self.control) == Control::Op(CTRL_OP_SHUTDOWN) {
                    log::write(b"view-broker: asked to stop, exiting\n");
                    libos::exit(0);
                }
            } else if let Some(i) = self.supervisors.iter().position(|s| s.raw().0 == h) {
                self.serve_supervisor(i);
            } else if let Some(i) = self.endpoints.iter().position(|(ch, _, _)| ch.raw().0 == h) {
                self.serve_endpoint(i);
            } else if let Some(i) = self.clients.iter().position(|c| c.key() == h && h != 0) {
                self.serve_client(i);
            } else if let Some(life) = self.life(h) {
                // A life channel only ever signals its peer closing. The program holds the other
                // end, so anything it sends on it is closed unread.
                if recv(life).is_err() {
                    self.life_closed(h);
                }
            }
        }
        if resolves {
            self.serve_resolve();
        }
    }
}

/// Bootstrap registers: `rdi` = notification channel (where `ChildExited` arrives), `rsi` = the
/// inherited root namespace, `rdx` = the control channel `service-mgr` installed, `rcx` = `arg0`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, control: u64, arg0: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(notif, root_ns, control, arg0) };
    log::write(b"view-broker: up\n");
    let (Some(notif), Some(root_ns)) = (start.notify.take(), start.root.take()) else {
        log::write(b"view-broker: no notification queue or root namespace\n");
        libos::exit(1);
    };
    let control: Option<Ch> = start.take_handle0();
    let Ok((client_end, serve_end)) = Handle::pair(4) else {
        log::write(b"view-broker: channel create FAIL\n");
        libos::exit(1);
    };
    // Kept before `Ready` moves the endpoint to `service-mgr`: the `views` grant binds it into a
    // view.
    let forwarding = client_end.duplicate(Rights::from_bits(u64::MAX)).ok();
    if !control.as_ref().is_some_and(|c| server::ready(c, b"view-broker", client_end)) {
        log::write(b"view-broker: Ready send FAIL\n");
        libos::exit(1);
    }
    let mut b = Broker {
        storage: Storage::default(),
        services_admin: None,
        power: None,
        logs: None,
        log: liblog::Logger::open(&root_ns, "/log/system/view-broker"),
        root_ns,
        notif,
        control,
        serve_end,
        forwarding,
        auth_ch: None,
        auth_admin: None,
        endpoints: Vec::new(),
        supervisors: Vec::new(),
        clients: Vec::new(),
        sessions: Sessions::new(),
        held: Held::default(),
        exits: Exits::default(),
    };
    b.serve_loop();
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    log::write(b"view-broker: PANIC\n");
    libos::exit(1);
}
