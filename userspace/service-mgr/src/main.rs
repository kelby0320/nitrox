//! `service-mgr` — the userspace service manager.
//!
//! Starts, supervises and restarts the system's services; see
//! `docs/architecture/service-manager.md`.
//!
//! It parses `/system/services.toml`, starts **every** service in it, and on a child's exit
//! applies *that child's* restart policy + backoff. Each service gets a **control channel**:
//! service-mgr keeps one end and can send a graceful `CTRL_OP_SHUTDOWN`. A requested shutdown is
//! never restarted, even under `policy = always`.
//!
//! That channel is also how a child's exit is **attributed**: `KIND_CHILD_EXITED` names a pid
//! and nothing maps a process handle to one, so the discriminator is which endpoint closed. See
//! `TODO(child-exit-attribution)`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libkern::{CTRL_OP_SHUTDOWN, KError, Rights, SysCaps, TERMINAL_OP_EMERGENCY, TERMINAL_OP_FINISH};
use libos::boot::Start;
use libos::log::{self, Line};
use libos::{
    CanLookup, Carried, Channel, Deadline, File, Handle, MapRead, Msg, Namespace, Notify, NsMutable, NsReadOnly, Only,
    Process, Ref, Signalled, Spawn, WaitSet, block_on, time,
};
use librsproto::server;
use service_mgr::bringup::{self, Entry, Failed, Step};
use service_mgr::registry;
use service_mgr::service_toml::{self, Backoff, RestartConfig, RestartPolicy, ServiceDecl};
use service_mgr::{services, wake};

mod reaping;
use reaping::{Ready, Reaping, channel_peer_closed, drain_codes, take_ready};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// How many declared services this supervisor holds at once. Not bounded by the wait set: only a
/// control channel that can still bring a message is waited on, since a death is also a
/// `ChildExited` on the notification channel.
const MAX_SERVICES: usize = 24;
const WAIT_MAX: usize = libkern::abi::MAX_WAIT_HANDLES;

/// How deep each route is: a full ring answers the resolver `WouldBlock` at once.
const SERVE_DEPTH: u32 = 64;

const READY_TIMEOUT_NS: u64 = 30_000_000_000; // 30 s

/// How often a wake comes while a control channel that can bring a message has no place in the
/// wait set, and so is read on every wake instead: a tick.
const UNSLOTTED_POLL_NS: u64 = 10_000_000;

type Endpoint = Handle<Channel, Only>;
/// A namespace this manager binds in: the root with `init`'s rights, or its registry.
type BindNs = Handle<Namespace, NsMutable>;

/// What `session-mgr` and `desktop-session-mgr` are granted: their control endpoint, moved in as
/// `rdx`, and `BIND_NAMESPACE`, re-delegated to construct per-session namespaces. **The two
/// supervisors are unaware of each other**, so serial stays the recovery path; the same user may
/// be logged in twice.
const SUPERVISOR_CONTROL: Rights = Rights::from_bits(Rights::SEND.bits() | Rights::RECV.bits() | Rights::WAIT.bits());

fn restart_name(p: RestartPolicy) -> &'static [u8] {
    match p {
        RestartPolicy::Never => b"never",
        RestartPolicy::OnFailure => b"on-failure",
        RestartPolicy::Always => b"always",
    }
}

/// Resolve `path` in `ns`: the handle, or `None` on failure.
fn lookup<'a, T, M, N: CanLookup + 'a>(
    ns: impl Into<Ref<'a, Namespace, N>>,
    path: &str,
    rights: Rights,
) -> Option<Handle<T, M>> {
    block_on(ns.into().lookup(path, rights)).ok()
}

/// Resolve the service's log endpoint under the logging service's `system/` subtree (only a
/// supervisor's namespace permits it). The logging service stamps the trusted `principal` and
/// `tier` from *this* channel; the service never names itself.
fn resolve_log_endpoint(registry: &BindNs, name: &str) -> Option<Endpoint> {
    // `TRANSFER` to move it into the child at spawn, which receives it attenuated to `SEND`.
    lookup(registry, &format!("/logging-service/system/{name}"), Rights::SEND | Rights::TRANSFER)
}

fn compute_backoff(cfg: &RestartConfig, attempts: u32) -> u64 {
    match cfg.backoff {
        Backoff::None => 0,
        Backoff::Linear => cfg.initial_ns,
        Backoff::Exponential => cfg.initial_ns.checked_shl(attempts).unwrap_or(u64::MAX).min(cfg.max_ns),
    }
}

fn should_restart(policy: RestartPolicy, code: i32) -> bool {
    match policy {
        RestartPolicy::Never => false,
        RestartPolicy::OnFailure => code != 0,
        RestartPolicy::Always => true,
    }
}

fn now_ns() -> u64 {
    time::now().unwrap_or(0)
}

fn create_control_channel() -> Option<(Endpoint, Endpoint)> {
    // **Depth 10, and the number bounds the send count rather than being a round one.** The
    // handoffs below are `SENDMODE_NOBLOCK` against a child that has not run yet, so a ring shorter
    // than the number of them does not block — it **drops the last handle silently**, and the
    // send reports success. Nine handoffs go to `desktop-session-mgr`, so it is ten deep for one
    // spare. `libsession::run_leader` carries the same warning.
    Handle::pair(10).ok()
}

/// Send `payload` and `carried` on `ch` as an unframed message of its own; on a refusal nothing
/// moved, and `carried` holds it.
fn send_msg(ch: &Endpoint, payload: &[u8], carried: &mut Carried) -> bool {
    let mut msg = Msg::new();
    msg.set(payload).is_ok() && ch.send(&msg, carried).is_ok()
}

fn send_control(ctrl: Option<&Endpoint>, op: u8) {
    if let Some(ctrl) = ctrl {
        send_msg(ctrl, &[op], &mut Carried::new());
    }
}

fn send_finish(terminal: Option<&Endpoint>, reboot: bool) {
    let Some(terminal) = terminal else {
        log::write(b"service-mgr: no terminal channel, so init cannot be told to finish\n");
        return;
    };
    send_msg(terminal, &[TERMINAL_OP_FINISH, reboot as u8], &mut Carried::new());
}

/// Read + parse the declarations from `/system/services.toml` on the root filesystem, which
/// `init` has mounted before it spawns this process. Empty (with a logged reason) if the file is
/// absent or holds nothing well-formed. A **test image differs from a release image by data**.
/// Also returns whether a critical declaration was skipped, each skipped one being said here
/// since the parser cannot log (`bringup::unfit`).
fn load_declarations(root: &Handle<Namespace, NsReadOnly>) -> (Vec<ServiceDecl>, bool) {
    let read = libfs::read(root, "/system/services.toml").ok();
    let text = match read.and_then(|b| String::from_utf8(b).ok()) {
        Some(t) => t,
        None => {
            log::write(b"service-mgr: no service declarations found at /system/services.toml\n");
            return (Vec::new(), false);
        }
    };
    let (mut decls, skipped) = service_toml::parse_all_reporting(&text);
    for s in &skipped {
        let mut l = Line::new();
        l.s(b"service-mgr: declaration '").s(s.name.as_bytes()).s(b"' skipped: ");
        l.s(s.why.describe().as_bytes());
        if s.critical {
            l.s(b" -- and it is critical");
        }
        l.end();
    }
    let skipped_critical = skipped.iter().any(|s| s.critical);
    if decls.is_empty() {
        log::write(b"service-mgr: declaration parse error\n");
        return (decls, skipped_critical);
    }
    // Keep the first `MAX_SERVICES` and **say** which were dropped: a silent truncation would
    // read as "everything declared is running".
    while decls.len() > MAX_SERVICES {
        let dropped = decls.pop().expect("len > MAX_SERVICES");
        Line::new()
            .s(b"service-mgr: '")
            .s(dropped.name.as_bytes())
            .s(b"' NOT started -- more than MAX_SERVICES declared")
            .end();
    }
    for decl in &decls {
        Line::new()
            .s(b"service-mgr: parsed service '")
            .s(decl.name.as_bytes())
            .s(b"' (executable=")
            .s(decl.executable.as_bytes())
            .s(b", restart=")
            .s(restart_name(decl.restart.policy))
            .s(b", max_attempts=")
            .u(decl.restart.max_attempts as u64)
            .s(b")")
            .end();
    }
    (decls, skipped_critical)
}

/// Spawn the service `decl` names with a fresh control channel: its process, and this manager's
/// end of the channel, `None` if there is no channel.
fn spawn_service(
    root: &Handle<Namespace, NsReadOnly>,
    registry: Option<&BindNs>,
    decl: &ServiceDecl,
) -> (Option<Handle<Process, Only>>, Option<Endpoint>) {
    let Some(image) = lookup::<File, MapRead, _>(root, &decl.executable, Rights::MAP_READ) else {
        Line::new().s(b"service-mgr: image not found: ").s(decl.executable.as_bytes()).end();
        return (None, None);
    };
    let (smgr_end, svc_end) = match create_control_channel() {
        Some((ours, theirs)) => (Some(ours), Some(theirs)),
        None => {
            log::write(b"service-mgr: control channel create FAIL (spawning without control)\n");
            (None, None)
        }
    };
    // **Not for a server**: it resolves its own log, and its first control message is its
    // `Meta::Ready`. And it is resolved **in the registry**, not through `/log`: that path
    // forwards to this process, which would be waiting on itself.
    let log_ep =
        if decl.endpoint.is_some() { None } else { registry.and_then(|r| resolve_log_endpoint(r, &decl.name)) };
    if log_ep.is_none() && decl.endpoint.is_none() {
        log::write(b"service-mgr: log endpoint resolve FAIL (spawning without logging)\n");
    }
    Line::new().s(b"service-mgr: starting service '").s(decl.name.as_bytes()).s(b"'").end();
    // **The kernel *attenuates*, it does not refuse**: `child = parent & requested`, so a
    // declaration asking for more than service-mgr holds spawns with the extra bits gone.
    // Nothing reports a process its own syscaps, so it cannot check the subset itself
    // (`TODO(spawn-syscap-attenuation)`); it logs what was **requested** and says so.
    for u in &decl.unknown_syscaps {
        Line::new()
            .s(b"service-mgr: '")
            .s(decl.name.as_bytes())
            .s(b"' declares an unknown syscap '")
            .s(u.as_bytes())
            .s(b"' -- NOT granted")
            .end();
    }
    if decl.syscaps != 0 {
        Line::new()
            .s(b"service-mgr: '")
            .s(decl.name.as_bytes())
            .s(b"' requested syscaps 0x")
            .u(decl.syscaps)
            .s(b" (the kernel grants the subset service-mgr holds)")
            .end();
    }
    // A server sends `Meta::Ready`; any other service's end is one-way.
    let rights = if decl.endpoint.is_some() {
        Rights::SEND | Rights::RECV | Rights::TRANSFER | Rights::WAIT
    } else {
        Rights::RECV | Rights::WAIT
    };
    let mut spawn = Spawn::new(&image);
    if let Some(end) = svc_end {
        // Refused only past the kernel's handle count, which one grant is not.
        let _ = spawn.grant(end, rights);
    }
    spawn.syscaps(SysCaps::from_bits_truncate(decl.syscaps));
    // A refused spawn hands its grant back, which closes as `Refused` drops, with this end and the
    // log endpoint.
    let Ok(process) = spawn.spawn() else {
        log::write(b"service-mgr: spawn FAIL\n");
        return (None, None);
    };
    // The spawn ABI delivers one handle to a register, so the log endpoint follows on the
    // control channel, as a non-server's first message: empty without one, as heartbeat takes it.
    if decl.endpoint.is_none()
        && let Some(ours) = &smgr_end
    {
        send_handle(ours, log_ep);
    }
    (Some(process), smgr_end)
}

/// Spawn `path` with a fresh control channel's end moved in as its `rdx`, and `BIND_NAMESPACE`: its
/// process and this manager's end.
fn spawn_supervisor(root: &Handle<Namespace, NsReadOnly>, path: &str) -> Option<(Handle<Process, Only>, Endpoint)> {
    let Some(image) = lookup::<File, MapRead, _>(root, path, Rights::MAP_READ) else {
        log::write(b"service-mgr: image not found (login chain)\n");
        return None;
    };
    let (smgr_end, child_end) = create_control_channel()?;
    let mut spawn = Spawn::new(&image);
    // Refused only past the kernel's handle count, which one grant is not.
    let _ = spawn.grant(child_end, SUPERVISOR_CONTROL);
    spawn.syscaps(SysCaps::BIND_NAMESPACE);
    Some((spawn.spawn().ok()?, smgr_end))
}

/// Receive one handle from init's handoff channel; `None` if the message was empty or the receive
/// failed. **Bounded**: an unbounded wait would hang the supervisor on a message never coming.
fn recv_handoff<T, M>(ctrl: &Endpoint) -> Option<Handle<T, M>> {
    let mut set = WaitSet::<1>::new();
    let deadline = Deadline::at(now_ns().saturating_add(READY_TIMEOUT_NS));
    if !set.add(ctrl).and_then(|_| set.wait(deadline)).is_ok_and(|s| !s.is_empty()) {
        log::write(b"service-mgr: handoff timeout\n");
        return None;
    }
    // The one handle a handoff brings; any more close as `sent` drops.
    ctrl.recv(&mut Msg::new()).ok()?.take(0)
}

/// Move `handle` to a child over its control channel; one refused is closed, and as `None`
/// sends **an empty message**: the receiver reads the handoffs positionally.
fn send_handle<T, M>(ctrl: &Endpoint, handle: Option<Handle<T, M>>) {
    let mut carried = Carried::new();
    if let Some(handle) = handle
        && carried.push(handle).is_ok()
        && send_msg(ctrl, &[], &mut carried)
    {
        return;
    }
    drop(carried);
    send_msg(ctrl, &[], &mut Carried::new());
}

/// **What the login supervisors are handed**, in order. `fs` and `profile` are `init`'s servers'
/// own endpoints, couriered: this manager uses neither. Every other is a **route**, so a
/// restarted server is reached again from every session. `None` for one this boot lacks; a set
/// no supervisor took closes as it drops, as this manager never exits and would hold it for good.
#[derive(Default)]
struct ChainEndpoints {
    fs: Option<Endpoint>,
    profile: Option<Endpoint>,
    tty: Option<Endpoint>,
    /// The compositor's, which only the graphical column takes.
    draw: Option<Endpoint>,
    clip: Option<Endpoint>,
    views: Option<Endpoint>,
    devices: Option<Endpoint>,
    storage: Option<Endpoint>,
    /// The session endpoint for `/svc/services`: the table, and no admin.
    services: Option<Endpoint>,
}

/// A copy of `h` to hand on, or `None`.
fn dup_endpoint(h: Option<&Endpoint>) -> Option<Endpoint> {
    h?.duplicate(Rights::TRANSFER | Rights::DUPLICATE).ok()
}

impl ChainEndpoints {
    /// Every handle, in the order `desktop-session-mgr` receives them.
    fn all(self) -> [Option<Endpoint>; 9] {
        [self.fs, self.profile, self.tty, self.draw, self.clip, self.views, self.devices, self.storage, self.services]
    }

    fn duplicate(&self) -> ChainEndpoints {
        ChainEndpoints {
            fs: dup_endpoint(self.fs.as_ref()),
            profile: dup_endpoint(self.profile.as_ref()),
            tty: dup_endpoint(self.tty.as_ref()),
            draw: None,
            clip: dup_endpoint(self.clip.as_ref()),
            views: dup_endpoint(self.views.as_ref()),
            devices: dup_endpoint(self.devices.as_ref()),
            storage: dup_endpoint(self.storage.as_ref()),
            services: dup_endpoint(self.services.as_ref()),
        }
    }
}

/// Bring up the login chain: `session-mgr`, then `desktop-session-mgr`, each handed its
/// endpoints over its control channel. **The serial set is duplicated before it is sent**, since
/// `send_handle` moves: a failure here then costs the graphical login, not both.
fn bring_up_login_chain(root: &Handle<Namespace, NsReadOnly>, chain: ChainEndpoints) -> [Supervisor; 2] {
    let mut supervisors = [None, None];
    if chain.fs.is_none() {
        log::write(b"service-mgr: no fs endpoint; skipping login chain\n");
        return supervisors;
    }
    if chain.profile.is_none() {
        log::write(b"service-mgr: no profile endpoint; sessions will have no /bin\n");
    }
    let mut serial = chain;
    let draw = serial.draw.take();
    let mut desktop = serial.duplicate();
    desktop.draw = draw;
    let Some((sess_h, sess_ctrl)) = spawn_supervisor(root, "/bin/session-mgr") else {
        log::write(b"service-mgr: session-mgr spawn FAIL\n");
        return supervisors;
    };
    // **Positional**: a reorder silently makes a session bind its home to the profile server.
    let s = serial;
    for h in [s.fs, s.profile, s.tty, s.clip, s.views, s.devices, s.storage, s.services] {
        send_handle(&sess_ctrl, h);
    }
    // Kept, not closed: shutdown asks the supervisors to end their sessions.
    supervisors[0] = Some((sess_h, sess_ctrl));
    supervisors[1] = bring_up_desktop_session(root, desktop);
    if supervisors[1].is_none() {
        log::write(b"service-mgr: no graphical login (serial login is unaffected)\n");
    }
    log::write(b"service-mgr: login chain up (auth-service + session-mgr)\n");
    supervisors
}

/// A login supervisor's process and control channel, kept for a shutdown.
type Supervisor = Option<(Handle<Process, Only>, Endpoint)>;

fn bring_up_desktop_session(root: &Handle<Namespace, NsReadOnly>, set: ChainEndpoints) -> Supervisor {
    set.fs.as_ref()?;
    let Some((h, ctrl)) = spawn_supervisor(root, "/bin/desktop-session-mgr") else {
        log::write(b"service-mgr: desktop-session-mgr spawn FAIL\n");
        return None;
    };
    for handle in set.all() {
        send_handle(&ctrl, handle);
    }
    Some((h, ctrl))
}

/// Bootstrap registers: `rdi` = notification channel, `rsi` = the lookup-only root namespace,
/// `rdx` = the **terminal channel** init moved in, `rcx` unused.
///
/// **What comes down the terminal channel first:**
/// 1. a root handle with `init`'s own rights, `BIND` and `UNBIND` among them, since binding the
///    servers is this manager's job and it is in `init`'s trust tier;
/// 2. the root filesystem's endpoint, and 3. the profile server's, both for the login chain.
///
/// The channel then stays open for asking `init` for the emergency shell. An empty first handoff
/// means `init` made no root handle, and then no server can be bound, which is said.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, terminal: u64, arg0: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(notif, root_ns, terminal, arg0) };
    log::write(b"service-mgr: up\n");
    let terminal: Option<Endpoint> = start.take_handle0();
    let (root_bind, fs_endpoint, profile_endpoint) = match &terminal {
        None => (None, None, None),
        Some(t) => (recv_handoff(t), recv_handoff(t), recv_handoff(t)),
    };
    if root_bind.is_none() {
        log::write(b"service-mgr: no root handle from init -- no server can be bound\n");
    }
    let Some(root) = start.root.take() else {
        log::write(b"service-mgr: no root namespace -- nothing can start\n");
        send_control(terminal.as_ref(), TERMINAL_OP_EMERGENCY);
        loop {
            time::sleep_until(Deadline::NEVER);
        }
    };
    let registry = match BindNs::create() {
        Ok(r) => Some(r),
        Err(_) => {
            log::write(b"service-mgr: registry create FAIL -- no server can be reached\n");
            None
        }
    };
    let (decls, skipped_critical) = load_declarations(&root);
    let entries: Vec<Entry> =
        decls.iter().map(|d| Entry { server: d.endpoint.is_some(), critical: d.critical }).collect();
    let svcs = decls
        .into_iter()
        .map(|decl| Supervised {
            decl,
            proc_h: None,
            ctrl: None,
            attempts: 0,
            running: false,
            requested_shutdown: false,
            phase: Phase::Down,
            endpoint: None,
            root_bound: false,
            restart_at: None,
            exited: None,
            restarts: 0,
        })
        .collect();
    let mut m = Mgr {
        notif: start.notify.take(),
        root,
        root_bind,
        registry,
        routes: Vec::new(),
        services: Services::default(),
        held: Vec::new(),
        terminal,
        fs_endpoint,
        profile_endpoint,
        svcs,
        entries,
        started: 0,
        chain_started: false,
        awaiting: None,
        after_until: None,
        halted: false,
        reported: false,
        shutdown: None,
        supervisors: [None, None],
    };
    // **A file that has lost its critical servers starts nothing**: bring-up would reach a login
    // chain with no `auth-service` behind it, and no critical server would be there to fail.
    if let Some(why) = bringup::unfit(&m.entries, skipped_critical) {
        Line::new().s(b"service-mgr: ").s(why.as_bytes()).s(b" -- starting nothing").end();
        m.ask_for_the_emergency_shell();
    }
    m.open_services();
    m.run()
}

/// One message on `ch`, its handles closed: `Err(())` once **a peer that has gone** is all that is
/// left, as a closed channel stays signalled and a caller must drop it or the wait loop would
/// spin; `None` when none is queued. A route's cannot close.
fn recv_request(ch: &Endpoint) -> Result<Option<(u16, u64, Vec<u8>)>, ()> {
    let mut msg = Msg::new();
    loop {
        let _sent = match ch.recv(&mut msg) {
            Ok(sent) => sent,
            Err(e) if e.code() == KError::PeerClosed => return Err(()),
            Err(_) => return Ok(None),
        };
        if let Ok(m) = librsproto::decode(msg.payload()) {
            return Ok(Some((m.op, m.request_id, m.body.to_vec())));
        }
    }
}

/// Refuse request `op` on `ch`, saying `why`. **`None` answers no one**: a held request whose
/// session closed keeps going without one ([`Held::session`]).
fn refuse(ch: Option<&Endpoint>, op: u16, request_id: u64, err: KError, why: &[u8]) {
    if let Some(ch) = ch {
        server::reply_error(ch, op, request_id, err, why);
    }
}

fn reply_ok(ch: Option<&Endpoint>, op: u16, request_id: u64) {
    if let Some(ch) = ch {
        let _ = server::send(ch, op, request_id, librsproto::RS_FLAG_REPLY, &[], &mut Carried::new());
    }
}

/// An endpoint minted on a resolve, for the caller to keep: an admin one or a power one.
enum Minted {
    Admin(Endpoint),
    Power(Endpoint),
}

/// A resolve on an admin or power endpoint `end`: a session, **whatever its suffix**, unless
/// `held` already reach `max`. The session's end, for the caller to keep.
fn open_session(end: &Endpoint, op: u16, request_id: u64, held: usize, max: usize) -> Option<Endpoint> {
    let resolve = librsproto::OP_NS_RESOLVE;
    if op != resolve {
        server::reply_error(end, op, request_id, KError::Unsupported, b"");
        return None;
    }
    if held >= max {
        server::reply_error(end, resolve, request_id, KError::WouldBlock, b"");
        return None;
    }
    let Ok((client, ours)) = Handle::pair(4) else {
        server::reply_error(end, resolve, request_id, KError::KernelError, b"");
        return None;
    };
    server::reply_channel(end, request_id, client).then_some(ours)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Phase {
    /// Not running, or running and not usable — a server that never became ready.
    Down,
    /// A server whose `Meta::Ready` is due by `deadline`.
    Starting { deadline: u64 },
    /// Running.
    Up,
}

/// **A route**: one of this manager's own endpoints, reaching one place in the registry.
/// Every binding of a server's path — the root's, each session's, each application's — is a
/// route, never the server's own endpoint, so a restart is reached from all of them. And one
/// route reaches **one** server: one that reached any server by the suffix could not be handed
/// to a session, since `desktop-shell` could bind it at a base of its choosing.
struct Route {
    svc: usize,
    /// `None` for the server's own path; `Some(suffix)` for an endpoint it mints for sessions.
    through: Option<&'static str>,
    /// Where in the registry each resolve continues: `/<name>` or `/<name>.<suffix>`.
    base: String,
    client: Endpoint,
    serve: Endpoint,
    /// A derived route's endpoint as the server minted it, bound at `base`.
    target: Option<Endpoint>,
    /// Whether something is bound at `base`; else a resolve is `NotFound`.
    live: bool,
}

/// **An admin request waiting for what it asked**, answered then, never by a wait here.
struct Held {
    /// The session to answer on, by its handle's value, or `0` once it has closed: **what was
    /// asked goes on**, a restart's start included, and only the answer is lost. The handle is not
    /// kept here, since a closed one's value may be reused.
    session: u64,
    request_id: u64,
    op: u16,
    svc: usize,
    awaits: Awaits,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Awaits {
    /// The service's exit by `until`, then a start for a restart.
    Exit { then_start: bool, until: u64 },
    /// A server's `Meta::Ready`, bounded by its own deadline.
    Up,
}

/// How long a service has to exit before its stop is answered "still running", unless it
/// declares `stop_timeout`. There is no forcible kill.
const STOP_TIMEOUT_NS: u64 = 5_000_000_000;

/// How long the login supervisors have at shutdown; longer than each gives its session leader.
const SESSIONS_TIMEOUT_NS: u64 = 10_000_000_000;

/// How long each service has, at shutdown, to exit once asked, **unless its declaration says**
/// (`stop_timeout`). The storage service, writing back every filesystem it mounted, declares its
/// own: past this bound `init` would stop the machine mid-write.
const SHUTDOWN_STOP_NS: u64 = 3_000_000_000;

/// **A shutdown under way**, in [`service_mgr::shutdown`]'s order.
struct Shutdown {
    reboot: bool,
    stage: Stage,
}

#[derive(Copy, Clone)]
enum Stage {
    /// The login supervisors are ending their sessions, until `until`.
    Sessions { until: u64 },
    /// Service `i` was asked to stop, until `until`.
    Service { i: usize, until: u64 },
    /// `init` has been told to finish.
    Finished,
}

/// **What serves `/svc/services`**: its own endpoint, bound there in the root; the admin
/// endpoints minted from it, and the admin sessions opened on those.
#[derive(Default)]
struct Services {
    serve: Option<Endpoint>,
    /// **The session endpoint**, and the end it is answered on: the table and nothing else.
    /// Every login binds a duplicate at `/dev/services`.
    session_client: Option<Endpoint>,
    session_serve: Option<Endpoint>,
    admin_ends: Vec<Endpoint>,
    admin_sessions: Vec<Endpoint>,
    /// A session opened on a power endpoint takes `Shutdown` and nothing else.
    power_ends: Vec<Endpoint>,
    power_sessions: Vec<Endpoint>,
}

impl Services {
    /// Every handle that serves `/svc/services`.
    fn serving(&self) -> impl Iterator<Item = &Endpoint> {
        let ends = self.admin_ends.iter().chain(&self.admin_sessions).chain(&self.power_ends);
        self.serve.iter().chain(&self.session_serve).chain(ends).chain(&self.power_sessions)
    }
}

/// One supervised service: its declaration, its child, and the state its restart policy needs.
struct Supervised {
    decl: ServiceDecl,
    /// Its process while it runs; dropping it closes the handle.
    proc_h: Option<Handle<Process, Only>>,
    /// service-mgr's end of the child's control channel, while it runs and the channel could be
    /// created. **This is the exit discriminator** — see [`Mgr::poll`].
    ctrl: Option<Endpoint>,
    attempts: u32,
    running: bool,
    /// A requested stop is never restarted, whatever the policy says.
    requested_shutdown: bool,
    phase: Phase,
    /// This manager's copy of a server's endpoint while it is up.
    endpoint: Option<Endpoint>,
    /// Whether the server's root path is bound to its route; once is enough, a restart rebinds
    /// in the registry.
    root_bound: bool,
    /// When a restart is due. **A deadline, never a sleep**: every resolve on a server's path
    /// waits on this process.
    restart_at: Option<u64>,
    exited: Option<Option<i32>>,
    restarts: u32,
}

/// **It never blocks on anything that can wait on it.** Every resolve on a server's path — the
/// root's `/log`, `/svc/auth`, `/dev/tty` — comes to this process first, so a wait on a server
/// that is itself resolving one of those would be two processes waiting on each other. So a
/// server's `Meta::Ready`, a restart's backoff and an `after` are deadlines in the one wait, and
/// the resolves are answered on each pass their route signals. The lookups it does make wait on
/// the root filesystem, the profile server and servers already serving, none of which waits on
/// this.
///
/// **The registry.** Each server's endpoint is bound in a namespace of this manager's own, under
/// the server's name, and every other binding of the server's path is one of this manager's
/// [`Route`]s, answered `SUBNAMESPACE` into the registry, so a restart rebinds there alone.
struct Mgr {
    notif: Option<Handle<Notify, Only>>,
    /// The lookup-only root, for lookups.
    root: Handle<Namespace, NsReadOnly>,
    /// The root with `init`'s rights, for binding the servers' paths.
    root_bind: Option<BindNs>,
    registry: Option<BindNs>,
    /// One per server path and derived endpoint, kept for the boot.
    routes: Vec<Route>,
    services: Services,
    held: Vec<Held>,
    terminal: Option<Endpoint>,
    /// `init`'s endpoints for the login chain, until it takes them.
    fs_endpoint: Option<Endpoint>,
    profile_endpoint: Option<Endpoint>,
    svcs: Vec<Supervised>,
    entries: Vec<Entry>,
    started: usize,
    chain_started: bool,
    /// The server bring-up is waiting on: its `Ready` before the next declaration starts.
    awaiting: Option<usize>,
    after_until: Option<u64>,
    /// A critical server failed at boot, or a shutdown began: nothing more starts.
    halted: bool,
    reported: bool,
    shutdown: Option<Shutdown>,
    /// The login supervisors, `session-mgr`'s first, kept for a shutdown.
    supervisors: [Supervisor; 2],
}

/// What a wait's index names.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Waited {
    Notif,
    Route(usize),
    /// A handle serving `/svc/services`, by its value, as answering one may retire another.
    Services(u64),
    Ctrl(usize),
}

/// Add `h` to `set` as `what`.
fn watch<'a, T: 'a, M: 'a>(
    set: &mut WaitSet<'a, WAIT_MAX>,
    names: &mut [Waited],
    h: impl Into<Ref<'a, T, M>>,
    what: Waited,
) {
    if let Ok(i) = set.add(h) {
        names[i] = what;
    }
}

/// What signalled in `waited`, by name.
fn fired(waited: &Signalled<WAIT_MAX>, names: &[Waited]) -> Vec<Waited> {
    waited.iter().map(|(i, _)| names[i]).collect()
}

impl Mgr {
    fn run(&mut self) -> ! {
        loop {
            self.advance();
            // **The notification channel, every route, what serves `/svc/services`, and each
            // control channel that can still bring a message**, a starting server's `Ready` above
            // all. **Only what signalled is read**, bar the looks the wait cannot make
            // ([`wake`]): a death is a `ChildExited`, and its pass looks at every channel.
            let ctrls: Vec<wake::Ctrl> = self
                .svcs
                .iter()
                .map(|s| wake::Ctrl { open: s.running && s.ctrl.is_some(), speaks: s.phase != Phase::Up })
                .collect();
            let slotted = wake::slotted(&ctrls, registry::STARTING_ROOM);
            let mut deadline = self.next_deadline();
            if wake::unslotted(&ctrls, &slotted) {
                deadline = deadline.min(now_ns().saturating_add(UNSLOTTED_POLL_NS));
            }
            let deadline = Deadline::at(deadline);
            let mut names = [Waited::Notif; WAIT_MAX];
            let waited = {
                let mut set = WaitSet::new();
                if let Some(q) = &self.notif {
                    watch(&mut set, &mut names, q, Waited::Notif);
                }
                self.watch_routes(&mut set, &mut names);
                for h in self.services.serving() {
                    watch(&mut set, &mut names, h, Waited::Services(h.raw().0));
                }
                for &i in &slotted {
                    if let Some(c) = &self.svcs[i].ctrl {
                        watch(&mut set, &mut names, c, Waited::Ctrl(i));
                    }
                }
                set.wait(deadline)
            };
            // A wait that failed says nothing of what signalled, so everything is looked at.
            let (fired, all) = match &waited {
                Ok(s) => (fired(s, &names), false),
                Err(_) => (Vec::new(), true),
            };
            let notified = all || fired.contains(&Waited::Notif);
            self.serve_resolves(|r| all || fired.contains(&Waited::Route(r)));
            self.serve_services(|h| all || fired.contains(&Waited::Services(h)));
            let mut codes = Vec::new();
            if notified {
                drain_codes(self.notif.as_ref(), &mut codes);
            }
            let signalled: Vec<usize> =
                fired.iter().filter_map(|w| if let Waited::Ctrl(i) = w { Some(*i) } else { None }).collect();
            self.poll(&ctrls, &wake::read(&ctrls, &slotted, &signalled, notified), &mut codes);
            self.due();
            self.shutdown_step();
            for code in codes {
                Line::new().s(b"service-mgr: an unsupervised child exited code=").i(code as i64).end();
            }
        }
    }

    /// Add every route to `set`.
    fn watch_routes<'a>(&'a self, set: &mut WaitSet<'a, WAIT_MAX>, names: &mut [Waited]) {
        for (r, route) in self.routes.iter().enumerate() {
            watch(set, names, &route.serve, Waited::Route(r));
        }
    }

    fn next_deadline(&self) -> u64 {
        // **Not an `after` once halted**: nothing more starts, so nothing clears it, and a deadline
        // left in the past would spin this loop.
        let mut d = if self.halted { u64::MAX } else { self.after_until.unwrap_or(u64::MAX) };
        for h in &self.held {
            if let Awaits::Exit { until, .. } = h.awaits {
                d = d.min(until);
            }
        }
        if let Some(Stage::Sessions { until } | Stage::Service { until, .. }) = self.shutdown.as_ref().map(|s| s.stage)
        {
            d = d.min(until)
        }
        for s in &self.svcs {
            if let Phase::Starting { deadline } = s.phase {
                d = d.min(deadline);
            }
            if let Some(at) = s.restart_at {
                d = d.min(at);
            }
        }
        d
    }

    /// Answer every resolve queued on each route `pick` names: `SUBNAMESPACE` into the registry at
    /// the route's base while something is bound there, `NotFound` while not.
    fn serve_resolves(&self, pick: impl Fn(usize) -> bool) {
        for (_, r) in self.routes.iter().enumerate().filter(|&(k, _)| pick(k)) {
            while let Ok(Some((op, request_id, body))) = recv_request(&r.serve) {
                let resolve = librsproto::OP_NS_RESOLVE;
                if op != resolve {
                    server::reply_error(&r.serve, op, request_id, KError::Unsupported, b"");
                } else if librsproto::namespace::parse_resolve_request(&body).is_none() {
                    server::reply_error(&r.serve, resolve, request_id, KError::InvalidArgument, b"");
                } else if let (true, Some(registry)) = (r.live, &self.registry) {
                    server::reply_subnamespace(&r.serve, request_id, registry.as_ref(), 0, r.base.as_bytes());
                } else {
                    server::reply_error(&r.serve, resolve, request_id, KError::NotFound, b"");
                }
            }
        }
    }

    fn advance(&mut self) {
        while !self.halted && self.awaiting.is_none() {
            match bringup::next(&self.entries, self.started, self.chain_started) {
                Step::LoginChain => {
                    self.chain_started = true;
                    self.start_login_chain();
                }
                Step::Done => {
                    if !self.reported {
                        self.reported = true;
                        let mut l = Line::new();
                        l.s(b"service-mgr: supervising ").u(self.svcs.len() as u64).s(b" service(s):");
                        for s in &self.svcs {
                            l.s(b" '").s(s.decl.name.as_bytes()).s(b"'");
                        }
                        l.end();
                    }
                    return;
                }
                Step::Start(i) => {
                    if !self.after_finished(i) {
                        return;
                    }
                    self.started += 1;
                    self.start(i, true);
                }
            }
        }
    }

    /// Whether declaration `i`'s `after` dependencies have finished, or the wait has run out
    /// (reported, then taken as yes). **`after` means "has exited"** and orders backwards only: a
    /// name declared later has not started, and cannot be waited for.
    fn after_finished(&mut self, i: usize) -> bool {
        let name = self.svcs[i].decl.name.clone();
        let mut waiting = Vec::new();
        for dep in &self.svcs[i].decl.after {
            match self.svcs[..i].iter().position(|s| &s.decl.name == dep) {
                None if self.after_until.is_none() => {
                    Line::new()
                        .s(b"service-mgr: '")
                        .s(name.as_bytes())
                        .s(b"' waits on '")
                        .s(dep.as_bytes())
                        .s(b"', which has not started -- declared later in the file, or not at all")
                        .end();
                }
                Some(j) if self.svcs[j].running && self.svcs[j].ctrl.is_some() => waiting.push(dep.clone()),
                _ => {}
            }
        }
        let now = now_ns();
        match self.after_until {
            _ if waiting.is_empty() => {
                if self.after_until.take().is_some() {
                    Line::new().s(b"service-mgr: what '").s(name.as_bytes()).s(b"' waited for has finished").end();
                }
                true
            }
            None => {
                for dep in &waiting {
                    Line::new()
                        .s(b"service-mgr: '")
                        .s(name.as_bytes())
                        .s(b"' waits for '")
                        .s(dep.as_bytes())
                        .s(b"' to finish")
                        .end();
                }
                self.after_until = Some(now.saturating_add(AFTER_TIMEOUT_NS));
                false
            }
            Some(until) if now >= until => {
                self.after_until = None;
                Line::new()
                    .s(b"service-mgr: what '")
                    .s(name.as_bytes())
                    .s(b"' waits for did not finish within the wait -- ")
                    .s(b"starting it anyway (does it ever exit?)")
                    .end();
                true
            }
            Some(_) => false,
        }
    }

    fn start(&mut self, i: usize, bringup: bool) -> bool {
        let server = self.svcs[i].decl.endpoint.is_some();
        let (h, ctrl) = spawn_service(&self.root, self.registry.as_ref(), &self.svcs[i].decl);
        let s = &mut self.svcs[i];
        if h.is_some() {
            s.proc_h = h;
            s.ctrl = ctrl;
            s.running = true;
            s.phase = if server {
                Phase::Starting { deadline: now_ns().saturating_add(READY_TIMEOUT_NS) }
            } else {
                Phase::Up
            };
            // **Only bring-up waits**: a restart after boot holds nothing up, and one during
            // bring-up must not displace the server it is waiting on.
            if server && bringup {
                self.awaiting = Some(i);
            }
            if s.ctrl.is_none() {
                Line::new()
                    .s(b"service-mgr: '")
                    .s(s.decl.name.as_bytes())
                    .s(b"' has no control channel -- its exit cannot be attributed")
                    .end();
            }
            true
        } else {
            if bringup && server {
                self.awaiting = Some(i);
                self.failed_to_start(i);
            }
            false
        }
    }

    /// **Which child exited is decided by its control channel closing**, not by the
    /// notification: `KIND_CHILD_EXITED` names a pid, and nothing maps a handle to one
    /// (`TODO(child-exit-attribution)`). The code is taken from the notification queue in arrival
    /// order, and **the close can come first**: `sys_process_exit` closes a child's handles
    /// before it queues the notification, so a wake with more deaths than codes waits for the
    /// rest, up to [`reaping::CODE_GRACE_NS`], answering resolves meanwhile. A death still without
    /// a code is a failure. The channels in `read` are looked at, and every open one not found dead
    /// again whenever codes come that no death found explains ([`wake::deaths`]).
    fn poll(&mut self, ctrls: &[wake::Ctrl], read: &[usize], codes: &mut Vec<i32>) {
        let dead = wake::deaths(&mut Reaping::new(self), ctrls, read, codes);
        for i in dead {
            let code = if codes.is_empty() { None } else { Some(codes.remove(0)) };
            self.reap(i, code);
        }
    }

    /// Look at service `i`'s control channel: a starting server's `Ready`, registered or refused,
    /// or any other's close. Whether it has closed, its service dead.
    fn look(&mut self, i: usize) -> bool {
        let s = &self.svcs[i];
        let Some(ctrl) = s.ctrl.as_ref().filter(|_| s.running) else {
            return false;
        };
        if !matches!(s.phase, Phase::Starting { .. }) {
            return channel_peer_closed(ctrl);
        }
        let ready = take_ready(ctrl);
        match ready {
            Ready::Endpoint(ep) => self.register(i, ep),
            Ready::Refused(why) => {
                Line::new()
                    .s(b"service-mgr: '")
                    .s(self.svcs[i].decl.name.as_bytes())
                    .s(b"' did not come up: ")
                    .untrusted(why.as_bytes())
                    .end();
                self.failed_to_start(i);
            }
            Ready::Nothing => {}
            Ready::Closed => return true,
        }
        false
    }

    /// Wait for the notification queue or any route until `deadline`, answering the routes that
    /// signalled: whether the queue did.
    fn wait_codes(&self, deadline: u64) -> bool {
        let Some(notif) = &self.notif else {
            return false;
        };
        let mut names = [Waited::Notif; WAIT_MAX];
        let waited = {
            let mut set = WaitSet::new();
            watch(&mut set, &mut names, notif, Waited::Notif);
            self.watch_routes(&mut set, &mut names);
            set.wait(Deadline::at(deadline))
        };
        let Ok(waited) = waited else {
            return false;
        };
        let fired = fired(&waited, &names);
        self.serve_resolves(|r| fired.contains(&Waited::Route(r)));
        fired.contains(&Waited::Notif)
    }

    /// Resolve `path` in `ns`, **answering resolves until it completes** or `deadline` passes:
    /// the server asked may be resolving through a route itself.
    fn lookup_serving(&self, ns: &BindNs, path: &str, rights: Rights, deadline: u64) -> Option<Endpoint> {
        let mut lookup = ns.start_lookup::<Channel, Only>(path, rights).ok()?;
        loop {
            let mut names = [Waited::Notif; WAIT_MAX];
            let waited = {
                let mut set = WaitSet::new();
                let _ = set.add_op(lookup.op());
                self.watch_routes(&mut set, &mut names);
                set.wait(Deadline::at(deadline))
            };
            if let Ok(waited) = &waited {
                let fired = fired(waited, &names);
                self.serve_resolves(|r| fired.contains(&Waited::Route(r)));
                if let Some(done) = lookup.completion_in(waited) {
                    return done.ok();
                }
            }
            if now_ns() >= deadline {
                return None;
            }
        }
    }

    /// A server's `Meta::Ready` arrived: bind its endpoint in the registry, replacing a
    /// predecessor's, and the first time its root path to its route; then derive its endpoints.
    fn register(&mut self, i: usize, endpoint: Endpoint) {
        let name = self.svcs[i].decl.name.clone();
        let path = self.svcs[i].decl.endpoint.clone().unwrap_or_default();
        let at = registry::base(&name);
        let Some(registry) = self.registry.as_ref().filter(|_| registry::valid_name(&name)) else {
            Line::new()
                .s(b"service-mgr: '")
                .s(name.as_bytes())
                .s(b"' cannot be registered: its name is not one the registry binds")
                .end();
            drop(endpoint);
            self.failed_to_start(i);
            return;
        };
        let _ = registry.unbind(&at);
        if registry.bind_server(&at, endpoint.as_ref(), "").is_err() {
            Line::new().s(b"service-mgr: '").s(name.as_bytes()).s(b"' registry bind FAIL").end();
            drop(endpoint);
            self.failed_to_start(i);
            return;
        }
        let Some(r) = self.route(i, None) else {
            if let Some(registry) = &self.registry {
                let _ = registry.unbind(&at);
            }
            drop(endpoint);
            self.failed_to_start(i);
            return;
        };
        self.routes[r].live = true;
        if !self.svcs[i].root_bound {
            let client = self.routes[r].client.as_ref();
            let bound = self.root_bind.as_ref().is_some_and(|root| root.bind_server(&path, client, "").is_ok());
            if bound {
                self.svcs[i].root_bound = true;
            } else {
                Line::new()
                    .s(b"service-mgr: bind FAIL at ")
                    .s(path.as_bytes())
                    .s(b" for '")
                    .s(name.as_bytes())
                    .s(b"'")
                    .end();
            }
        }
        let s = &mut self.svcs[i];
        s.endpoint = Some(endpoint);
        s.phase = Phase::Up;
        // **Said only when it is so**: a gate reads this line as the server reachable at its
        // path, and without the root binding it is reachable only through the session routes.
        if s.root_bound {
            Line::new().s(b"service-mgr: ").s(name.as_bytes()).s(b" bound at ").s(path.as_bytes()).end();
        } else {
            let mut l = Line::new();
            l.s(b"service-mgr: ").s(name.as_bytes()).s(b" is up, unbound at ").s(path.as_bytes()).end();
        }
        for through in registry::derives(&name) {
            self.derive(i, through);
        }
        if self.awaiting == Some(i) {
            self.awaiting = None;
        }
        self.settle_up(i, true);
    }

    /// Ask server `i` for the endpoint it mints by a resolve of `through`, and bind it in the
    /// registry, each time it comes up. One that does not answer is said, and gets no route.
    fn derive(&mut self, i: usize, through: &'static str) {
        let name = self.svcs[i].decl.name.clone();
        let path = format!("{}/{}", registry::base(&name), through);
        let deadline = now_ns().saturating_add(DERIVE_TIMEOUT_NS);
        let rights = Rights::TRANSFER | Rights::DUPLICATE;
        let h = self.registry.as_ref().and_then(|registry| self.lookup_serving(registry, &path, rights, deadline));
        let at = registry::derived(&name, through);
        if let Some(registry) = &self.registry {
            let _ = registry.unbind(&at);
        }
        let route = if h.is_none() { None } else { self.route(i, Some(through)) };
        let bound = route.is_some()
            && self
                .registry
                .as_ref()
                .zip(h.as_ref())
                .is_some_and(|(reg, h)| reg.bind_server(&at, h.as_ref(), "").is_ok());
        let Some(r) = route.filter(|_| bound) else {
            drop(h);
            Line::new()
                .s(b"service-mgr: '")
                .s(name.as_bytes())
                .s(b"' gave no ")
                .s(through.as_bytes())
                .s(b" -- sessions will not reach it")
                .end();
            return;
        };
        let route = &mut self.routes[r];
        route.target = h;
        route.live = true;
        Line::new()
            .s(b"service-mgr: sessions reach ")
            .s(name.as_bytes())
            .s(b" through its ")
            .s(through.as_bytes())
            .end();
    }

    /// The route for server `i`'s own path or a derived endpoint, made when first needed. `None`
    /// when there is no room, which is said.
    fn route(&mut self, i: usize, through: Option<&'static str>) -> Option<usize> {
        if let Some(r) = self.routes.iter().position(|r| r.svc == i && r.through == through) {
            return Some(r);
        }
        let name = &self.svcs[i].decl.name;
        let room = self.routes.len() < registry::MAX_ROUTES;
        let made = if room { Handle::pair(SERVE_DEPTH).ok() } else { None };
        let Some((client, serve)) = made else {
            let why: &[u8] = if room { b"channel create FAIL" } else { b"all in use" };
            Line::new().s(b"service-mgr: no route for '").s(name.as_bytes()).s(b"' -- ").s(why).end();
            return None;
        };
        let base = match through {
            None => registry::base(name),
            Some(t) => registry::derived(name, t),
        };
        self.routes.push(Route { svc: i, through, base, client, serve, target: None, live: false });
        Some(self.routes.len() - 1)
    }

    /// A duplicate of the route to server `name` for a login supervisor while something is bound
    /// there.
    fn route_copy(&self, name: &str, through: Option<&str>) -> Option<Endpoint> {
        let found = self.routes.iter().find(|r| r.live && r.through == through && self.svcs[r.svc].decl.name == name);
        dup_endpoint(found.map(|r| &r.client))
    }

    /// An admin or power session by its handle's value; `None` for `0`, one that has closed.
    fn session_of(&self, id: u64) -> Option<&Endpoint> {
        let s = &self.services;
        s.admin_sessions.iter().chain(&s.power_sessions).find(|h| id != 0 && h.raw().0 == id)
    }

    /// Start nothing more, and ask `init` for the emergency shell. Only before the terminal
    /// server is up, which is what lets the shell take the console.
    fn ask_for_the_emergency_shell(&mut self) {
        self.halted = true;
        log::write(b"service-mgr: asking init for the emergency shell\n");
        send_control(self.terminal.as_ref(), TERMINAL_OP_EMERGENCY);
    }

    /// Server `i` did not come up. At bring-up, a critical one stops the boot; any other is
    /// reported and passed.
    fn failed_to_start(&mut self, i: usize) {
        self.svcs[i].phase = Phase::Down;
        self.settle_up(i, false);
        if self.awaiting != Some(i) {
            return;
        }
        self.awaiting = None;
        let name = self.svcs[i].decl.name.clone();
        match bringup::failed_at_boot(self.entries[i]) {
            Failed::Emergency => {
                Line::new()
                    .s(b"service-mgr: '")
                    .s(name.as_bytes())
                    .s(b"' is critical and did not come up -- starting nothing more")
                    .end();
                self.ask_for_the_emergency_shell();
            }
            Failed::Continue => {
                Line::new().s(b"service-mgr: '").s(name.as_bytes()).s(b"' did not come up; going on without it").end();
            }
        }
    }

    /// Service `i` has exited with `code`: unbind it, and apply its restart policy.
    fn reap(&mut self, i: usize, code: Option<i32>) {
        let was = self.svcs[i].phase;
        {
            let s = &mut self.svcs[i];
            s.proc_h = None;
            s.ctrl = None;
            s.running = false;
            s.phase = Phase::Down;
            s.exited = Some(code);
            let mut l = Line::new();
            l.s(b"service-mgr: '").s(s.decl.name.as_bytes()).s(b"' exited");
            match code {
                Some(c) => l.s(b" code=").i(c as i64),
                None => l.s(b" code=unknown"),
            };
            l.end();
            // **Its path answers `NotFound` until it is back**, rather than reaching a server that
            // has gone — and so does every endpoint it minted for sessions.
            if let Some(endpoint) = s.endpoint.take() {
                if let Some(registry) = &self.registry {
                    let _ = registry.unbind(&registry::base(&s.decl.name));
                }
                drop(endpoint);
            }
        }
        for r in self.routes.iter_mut().filter(|r| r.svc == i) {
            r.live = false;
            if let Some(target) = r.target.take() {
                if let Some(registry) = &self.registry {
                    let _ = registry.unbind(&r.base);
                }
                drop(target);
            }
        }
        self.apply_policy(i, code, was);
        self.settle_exit(i);
    }

    /// Apply service `i`'s restart policy to an exit with `code`, or to a restart that could not
    /// spawn. `was` is its phase before: one that died starting and will not be restarted did
    /// not come up.
    fn apply_policy(&mut self, i: usize, code: Option<i32>, was: Phase) {
        let s = &self.svcs[i];
        let restarting = !s.requested_shutdown
            && should_restart(s.decl.restart.policy, code.unwrap_or(-1))
            && (s.decl.restart.max_attempts == 0 || s.attempts < s.decl.restart.max_attempts);
        if matches!(was, Phase::Starting { .. }) && !restarting {
            self.failed_to_start(i);
        }
        let s = &mut self.svcs[i];
        if s.requested_shutdown {
            Line::new()
                .s(b"service-mgr: '")
                .s(s.decl.name.as_bytes())
                .s(b"' stopped as requested (policy=")
                .s(restart_name(s.decl.restart.policy))
                .s(b" overridden -- not restarting)")
                .end();
            return;
        }
        if !should_restart(s.decl.restart.policy, code.unwrap_or(-1)) {
            Line::new()
                .s(b"service-mgr: '")
                .s(s.decl.name.as_bytes())
                .s(b"' stopped (policy=")
                .s(restart_name(s.decl.restart.policy))
                .s(b", not restarting)")
                .end();
            return;
        }
        if !restarting {
            Line::new()
                .s(b"service-mgr: '")
                .s(s.decl.name.as_bytes())
                .s(b"' gave up after ")
                .u(s.attempts as u64)
                .s(b" restart(s)")
                .end();
            return;
        }
        let backoff = compute_backoff(&s.decl.restart, s.attempts);
        let mut l = Line::new();
        l.s(b"service-mgr: restarting '").s(s.decl.name.as_bytes()).s(b"' (attempt ").u((s.attempts + 1) as u64);
        if s.decl.restart.max_attempts != 0 {
            l.s(b" of ").u(s.decl.restart.max_attempts as u64);
        }
        l.s(b") after ").u(backoff / 1_000_000).s(b"ms backoff").end();
        s.restart_at = Some(now_ns().saturating_add(backoff));
    }

    /// Whatever is due: a restart, a late `Ready`, a stop past its bound.
    fn due(&mut self) {
        let now = now_ns();
        // **A stop not honoured within its bound** is answered so, and stays asked: an exit after
        // it is still a stop that was requested, and is not restarted.
        let mut k = 0;
        while k < self.held.len() {
            match self.held[k].awaits {
                Awaits::Exit { until, .. } if until <= now => {
                    let h = self.held.remove(k);
                    Line::new()
                        .s(b"service-mgr: '")
                        .s(self.svcs[h.svc].decl.name.as_bytes())
                        .s(b"' was asked to stop, and is still running")
                        .end();
                    let why = b"it was asked to stop, and is still running";
                    refuse(self.session_of(h.session), h.op, h.request_id, KError::TimedOut, why);
                }
                _ => k += 1,
            }
        }
        for i in 0..self.svcs.len() {
            if self.svcs[i].restart_at.is_some_and(|at| at <= now) {
                self.svcs[i].restart_at = None;
                self.svcs[i].attempts += 1;
                // A restart is bring-up's only when it replaces the attempt of the server
                // bring-up is waiting on, which died starting.
                let bringup = self.awaiting == Some(i);
                self.svcs[i].restarts += 1;
                if !self.start(i, bringup) && !bringup {
                    // **A restart that could not spawn is a restart that failed**, and its policy
                    // decides what next — never the bring-up rule, which would ask for the
                    // emergency shell with the terminal server holding the console.
                    Line::new()
                        .s(b"service-mgr: '")
                        .s(self.svcs[i].decl.name.as_bytes())
                        .s(b"' could not be restarted")
                        .end();
                    self.apply_policy(i, Some(-1), Phase::Down);
                    // **A start that waited for this one is answered** once its policy has
                    // nothing more to try; otherwise it waits for the next attempt.
                    if self.svcs[i].restart_at.is_none() {
                        self.settle_up(i, false);
                    }
                }
            }
            if let Phase::Starting { deadline } = self.svcs[i].phase
                && deadline <= now
            {
                Line::new()
                    .s(b"service-mgr: '")
                    .s(self.svcs[i].decl.name.as_bytes())
                    .s(b"' sent no Ready within ")
                    .u(READY_TIMEOUT_NS / 1_000_000_000)
                    .s(b" s")
                    .end();
                self.failed_to_start(i);
            }
        }
    }

    /// **Serve `/svc/services`** from an endpoint bound there in the root.
    fn open_services(&mut self) {
        let Ok((client, serve)) = Handle::pair(SERVE_DEPTH) else {
            log::write(b"service-mgr: /svc/services channel create FAIL\n");
            return;
        };
        if !self.root_bind.as_ref().is_some_and(|root| root.bind_server("/svc/services", client.as_ref(), "").is_ok()) {
            log::write(b"service-mgr: bind FAIL at /svc/services\n");
            return;
        }
        // The binding holds its own reference; this end is not needed again.
        drop(client);
        self.services.serve = Some(serve);
        log::write(b"service-mgr: serving /svc/services\n");
        // Answered on its own end, so a session reaches the table and never an admin endpoint.
        match Handle::pair(SERVE_DEPTH) {
            Ok((client, serve)) => {
                self.services.session_client = Some(client);
                self.services.session_serve = Some(serve);
            }
            Err(_) => log::write(b"service-mgr: services session endpoint create FAIL -- no /dev/services\n"),
        }
    }

    fn table(&self) -> Vec<u8> {
        let rows: Vec<(&str, services::State, u32)> = self
            .svcs
            .iter()
            .map(|s| {
                let starting = matches!(s.phase, Phase::Starting { .. });
                let usable = s.phase == Phase::Up;
                let state = services::state(s.running, starting, usable, s.requested_shutdown, s.exited);
                (s.decl.name.as_str(), state, s.restarts)
            })
            .collect();
        services::table(&rows)
    }

    /// **Sessions, then endpoints, then resolves**, so that a slot a closed channel frees is free
    /// before a new one is asked for in the same wake: a session before an endpoint opens another,
    /// and an endpoint before `/svc/services` mints another. Answered the other way round, a
    /// `power-endpoint` resolve arriving with its last holder's close was refused `WouldBlock` with
    /// the one slot about to be freed. Only the handles `pick` names, by value, are read.
    fn serve_services(&mut self, pick: impl Fn(u64) -> bool) {
        let mut k = 0;
        while k < self.services.admin_sessions.len() {
            let session = self.services.admin_sessions[k].raw().0;
            if !pick(session) {
                k += 1;
                continue;
            }
            match recv_request(&self.services.admin_sessions[k]) {
                Ok(Some((op, request_id, body))) => self.admin_request(session, op, request_id, &body),
                Ok(None) => k += 1,
                Err(()) => {
                    self.services.admin_sessions.remove(k);
                    // **What it asked goes on, unanswered.** Dropping the request instead would
                    // drop a restart's start with its answer, and leave the service stopped.
                    for h in self.held.iter_mut().filter(|h| h.session == session) {
                        h.session = 0;
                    }
                }
            }
        }
        let mut k = 0;
        while k < self.services.power_sessions.len() {
            let session = self.services.power_sessions[k].raw().0;
            if !pick(session) {
                k += 1;
                continue;
            }
            match recv_request(&self.services.power_sessions[k]) {
                Ok(Some((op, request_id, body))) => self.power_request(session, op, request_id, &body),
                Ok(None) => k += 1,
                // A shutdown already asked goes on: it was answered when it began.
                Err(()) => {
                    self.services.power_sessions.remove(k);
                }
            }
        }
        let mut k = 0;
        while k < self.services.admin_ends.len() {
            if !pick(self.services.admin_ends[k].raw().0) {
                k += 1;
                continue;
            }
            match recv_request(&self.services.admin_ends[k]) {
                Ok(Some((op, request_id, _))) => {
                    let (held, max) = (self.services.admin_sessions.len(), services::MAX_ADMIN_SESSIONS);
                    if let Some(s) = open_session(&self.services.admin_ends[k], op, request_id, held, max) {
                        self.services.admin_sessions.push(s);
                    }
                }
                Ok(None) => k += 1,
                Err(()) => {
                    self.services.admin_ends.remove(k);
                }
            }
        }
        let mut k = 0;
        while k < self.services.power_ends.len() {
            if !pick(self.services.power_ends[k].raw().0) {
                k += 1;
                continue;
            }
            match recv_request(&self.services.power_ends[k]) {
                Ok(Some((op, request_id, _))) => {
                    let (held, max) = (self.services.power_sessions.len(), services::MAX_POWER_SESSIONS);
                    if let Some(s) = open_session(&self.services.power_ends[k], op, request_id, held, max) {
                        self.services.power_sessions.push(s);
                    }
                }
                Ok(None) => k += 1,
                Err(()) => {
                    self.services.power_ends.remove(k);
                }
            }
        }
        if self.services.serve.as_ref().is_some_and(|root| pick(root.raw().0)) {
            while let Some(root) = &self.services.serve {
                match recv_request(root) {
                    Ok(Some((op, request_id, body))) => {
                        let minted = self.answer_resolve(root, op, request_id, &body, false);
                        self.keep(minted);
                    }
                    Ok(None) => break,
                    Err(()) => self.services.serve = None,
                }
            }
        }
        // This manager holds its other end, so it cannot close.
        if self.services.session_serve.as_ref().is_some_and(|s| pick(s.raw().0)) {
            while let Some(session) = &self.services.session_serve
                && let Ok(Some((op, request_id, body))) = recv_request(session)
            {
                let minted = self.answer_resolve(session, op, request_id, &body, true);
                self.keep(minted);
            }
        }
    }

    /// A resolve on `/svc/services` or a session endpoint: the table, or on the first only, an
    /// admin or power endpoint, which the caller keeps.
    fn answer_resolve(&self, from: &Endpoint, op: u16, request_id: u64, body: &[u8], session: bool) -> Option<Minted> {
        let resolve = librsproto::OP_NS_RESOLVE;
        if op != resolve {
            server::reply_error(from, op, request_id, KError::Unsupported, b"");
            return None;
        }
        let Some(r) = librsproto::namespace::parse_resolve_request(body) else {
            server::reply_error(from, resolve, request_id, KError::InvalidArgument, b"");
            return None;
        };
        type Kind = fn(Endpoint) -> Minted;
        let (held, max, what, kind): (usize, usize, &[u8], Kind) = match services::asked(r.suffix, session) {
            services::Asked::Table => {
                server::reply_object(from, request_id, &self.table());
                return None;
            }
            services::Asked::AdminEndpoint => {
                let what = b"service-mgr: an admin endpoint minted\n";
                (self.services.admin_ends.len(), services::MAX_ADMIN_ENDPOINTS, what, Minted::Admin)
            }
            services::Asked::PowerEndpoint => {
                let what = b"service-mgr: a power endpoint minted\n";
                (self.services.power_ends.len(), services::MAX_POWER_ENDPOINTS, what, Minted::Power)
            }
            services::Asked::Unknown => {
                server::reply_error(from, resolve, request_id, KError::NotFound, b"");
                return None;
            }
        };
        if held >= max {
            server::reply_error(from, resolve, request_id, KError::WouldBlock, b"");
            return None;
        }
        let Ok((client, ours)) = Handle::pair(4) else {
            server::reply_error(from, resolve, request_id, KError::KernelError, b"");
            return None;
        };
        if !server::reply_channel(from, request_id, client) {
            return None;
        }
        log::write(what);
        Some(kind(ours))
    }

    /// Keep an endpoint a resolve minted.
    fn keep(&mut self, minted: Option<Minted>) {
        match minted {
            Some(Minted::Admin(end)) => self.services.admin_ends.push(end),
            Some(Minted::Power(end)) => self.services.power_ends.push(end),
            None => {}
        }
    }

    /// `Shutdown`, on a power session: answered as soon as it has begun, since nothing comes back
    /// after it, and refused while one is under way.
    fn power_request(&mut self, session: u64, op: u16, request_id: u64, body: &[u8]) {
        use librsproto::services::{OP_SERVICES_SHUTDOWN, parse_shutdown};
        if op != OP_SERVICES_SHUTDOWN {
            let why = b"a power session asks for a shutdown and nothing else";
            return refuse(self.session_of(session), op, request_id, KError::NoAccess, why);
        }
        let Some(reboot) = parse_shutdown(body) else {
            let why = b"a shutdown's body is one byte, to halt or to reboot";
            return refuse(self.session_of(session), op, request_id, KError::InvalidArgument, why);
        };
        if self.shutdown.is_some() {
            return refuse(self.session_of(session), op, request_id, KError::WouldBlock, b"a shutdown is under way");
        }
        reply_ok(self.session_of(session), op, request_id);
        self.begin_shutdown(reboot);
    }

    /// **Begin a shutdown**: nothing more starts, every waiting request is refused, and the login
    /// supervisors are asked to end their sessions; [`Self::shutdown_step`] takes the rest.
    fn begin_shutdown(&mut self, reboot: bool) {
        log::write(if reboot {
            b"service-mgr: shutting down, to reboot, as asked\n"
        } else {
            b"service-mgr: shutting down, as asked\n"
        });
        // Nothing more starts, bring-up or restart, and whatever exits from here was asked to.
        self.halted = true;
        for s in &mut self.svcs {
            s.restart_at = None;
            s.requested_shutdown = true;
        }
        // **Every request waiting is refused**: a restart's start in particular must not run.
        for h in core::mem::take(&mut self.held) {
            refuse(self.session_of(h.session), h.op, h.request_id, KError::WouldBlock, b"the machine is shutting down");
        }
        // The sessions first: each supervisor ends its session and exits, and its control channel
        // closing is how that is seen.
        log::write(b"service-mgr: asking the login supervisors to end their sessions\n");
        for (process, _) in self.supervisors.iter().flatten() {
            let _ = process.terminate();
        }
        let until = now_ns().saturating_add(SESSIONS_TIMEOUT_NS);
        self.shutdown = Some(Shutdown { reboot, stage: Stage::Sessions { until } });
    }

    /// **Take a shutdown as far as it can go now**: past each stage once it is done or its bound
    /// has run out, since a stop not honoured holds up only the machine's stop. Then `init` is
    /// told to finish.
    fn shutdown_step(&mut self) {
        let Some((reboot, stage)) = self.shutdown.as_ref().map(|s| (s.reboot, s.stage)) else {
            return;
        };
        let now = now_ns();
        let below = match stage {
            Stage::Sessions { until } => {
                let gone = |k: usize| self.supervisors[k].as_ref().is_none_or(|(_, c)| channel_peer_closed(c));
                let (text, desktop) = (gone(0), gone(1));
                if !(text && desktop) && now < until {
                    return;
                }
                if text && desktop {
                    log::write(b"service-mgr: the sessions have ended\n");
                } else {
                    let mut l = Line::new();
                    l.s(b"service-mgr: still running after the sessions' bound:");
                    if !text {
                        l.s(b" session-mgr");
                    }
                    if !desktop {
                        l.s(b" desktop-session-mgr");
                    }
                    l.s(b" -- the shutdown goes on").end();
                }
                self.svcs.len()
            }
            Stage::Service { i, until } => {
                if self.svcs[i].running && now < until {
                    return;
                }
                if self.svcs[i].running {
                    Line::new()
                        .s(b"service-mgr: '")
                        .s(self.svcs[i].decl.name.as_bytes())
                        .s(b"' is still running after it was asked to stop -- the shutdown goes on")
                        .end();
                }
                i
            }
            Stage::Finished => return,
        };
        let running: Vec<bool> = self.svcs.iter().map(|s| s.running).collect();
        let mut below = below;
        let next = loop {
            match service_mgr::shutdown::next_to_stop(&running, below) {
                Some(i) if self.svcs[i].ctrl.is_none() => {
                    Line::new()
                        .s(b"service-mgr: '")
                        .s(self.svcs[i].decl.name.as_bytes())
                        .s(b"' has no control channel to ask on -- passed over")
                        .end();
                    below = i;
                }
                other => break other,
            }
        };
        let stage = match next {
            Some(i) => {
                Line::new()
                    .s(b"service-mgr: asking '")
                    .s(self.svcs[i].decl.name.as_bytes())
                    .s(b"' to stop, for the shutdown")
                    .end();
                send_control(self.svcs[i].ctrl.as_ref(), CTRL_OP_SHUTDOWN);
                let bound = self.svcs[i].decl.stop_timeout.unwrap_or(SHUTDOWN_STOP_NS);
                Stage::Service { i, until: now.saturating_add(bound) }
            }
            None => {
                log::write(b"service-mgr: every service has been asked to stop; telling init to finish\n");
                send_finish(self.terminal.as_ref(), reboot);
                Stage::Finished
            }
        };
        if let Some(s) = self.shutdown.as_mut() {
            s.stage = stage;
        }
    }

    /// `Start`, `Stop` or `Restart`, on an admin session: refused now, or answered once done.
    fn admin_request(&mut self, session: u64, op: u16, request_id: u64, body: &[u8]) {
        if op == librsproto::services::OP_SERVICES_SHUTDOWN {
            let why = b"a shutdown is asked on a power session, which the `power` grant opens";
            return refuse(self.session_of(session), op, request_id, KError::NoAccess, why);
        }
        if self.shutdown.is_some() {
            return refuse(
                self.session_of(session),
                op,
                request_id,
                KError::WouldBlock,
                b"the machine is shutting down",
            );
        }
        let named = |n: &str| self.svcs.iter().position(|s| s.decl.name == n);
        let found = core::str::from_utf8(body).ok().and_then(named);
        let facts = found.map(|i| {
            let s = &self.svcs[i];
            let (restart_due, reached) = (s.restart_at.is_some(), bringup::reached(i, self.started));
            services::Found { essential: s.decl.essential, running: s.running, restart_due, reached }
        });
        let action = match services::decide(op, facts) {
            Ok(a) => a,
            Err((err, why)) => return refuse(self.session_of(session), op, request_id, err, why.as_bytes()),
        };
        // `decide` has refused a name no service has, so this only returns if that changes.
        let Some(i) = found else { return };
        if self.held.iter().any(|h| h.svc == i) {
            let why = b"it is being started or stopped already";
            return refuse(self.session_of(session), op, request_id, KError::WouldBlock, why);
        }
        let name = self.svcs[i].decl.name.clone();
        match action {
            services::Action::CancelRestart => {
                Line::new().s(b"service-mgr: '").s(name.as_bytes()).s(b"' will not be restarted, as asked").end();
                let s = &mut self.svcs[i];
                s.restart_at = None;
                s.requested_shutdown = true;
                reply_ok(self.session_of(session), op, request_id);
            }
            services::Action::Start => {
                Line::new().s(b"service-mgr: starting '").s(name.as_bytes()).s(b"', as asked").end();
                self.start_asked(i, Held { session, request_id, op, svc: i, awaits: Awaits::Up });
            }
            services::Action::Stop | services::Action::StopThenStart => {
                let then_start = action == services::Action::StopThenStart;
                if self.svcs[i].ctrl.is_none() {
                    let why = b"it has no control channel to ask on";
                    return refuse(self.session_of(session), op, request_id, KError::Unsupported, why);
                }
                Line::new()
                    .s(b"service-mgr: asking '")
                    .s(name.as_bytes())
                    .s(if then_start { b"' to stop, to start it again" } else { b"' to stop" })
                    .end();
                send_control(self.svcs[i].ctrl.as_ref(), CTRL_OP_SHUTDOWN);
                let s = &mut self.svcs[i];
                s.requested_shutdown = true;
                s.restart_at = None;
                let bound = self.svcs[i].decl.stop_timeout.unwrap_or(STOP_TIMEOUT_NS);
                let until = now_ns().saturating_add(bound);
                let awaits = Awaits::Exit { then_start, until };
                self.held.push(Held { session, request_id, op, svc: i, awaits });
            }
        }
    }

    /// Start service `i` because it was asked, answering `held` when it is up. A start asked for
    /// starts afresh: no stop stands, and its restart attempts begin again.
    fn start_asked(&mut self, i: usize, held: Held) {
        let s = &mut self.svcs[i];
        s.requested_shutdown = false;
        s.attempts = 0;
        s.restart_at = None;
        if !self.start(i, false) {
            let why = b"it could not be spawned";
            return refuse(self.session_of(held.session), held.op, held.request_id, KError::KernelError, why);
        }
        if matches!(self.svcs[i].phase, Phase::Starting { .. }) {
            self.held.push(held);
        } else {
            reply_ok(self.session_of(held.session), held.op, held.request_id);
        }
    }

    /// Service `i` has exited: answer a stop that waited for it, or restart it.
    fn settle_exit(&mut self, i: usize) {
        let waits_exit = |h: &Held| h.svc == i && matches!(h.awaits, Awaits::Exit { .. });
        let Some(k) = self.held.iter().position(waits_exit) else {
            return;
        };
        let h = self.held.remove(k);
        match h.awaits {
            Awaits::Exit { then_start: true, .. } => {
                self.svcs[i].restarts += 1;
                Line::new()
                    .s(b"service-mgr: starting '")
                    .s(self.svcs[i].decl.name.as_bytes())
                    .s(b"' again, as asked")
                    .end();
                self.start_asked(i, Held { awaits: Awaits::Up, ..h });
            }
            _ => reply_ok(self.session_of(h.session), h.op, h.request_id),
        }
    }

    fn settle_up(&mut self, i: usize, up: bool) {
        let Some(k) = self.held.iter().position(|h| h.svc == i && h.awaits == Awaits::Up) else {
            return;
        };
        let h = self.held.remove(k);
        if up {
            reply_ok(self.session_of(h.session), h.op, h.request_id);
        } else {
            refuse(self.session_of(h.session), h.op, h.request_id, KError::TimedOut, b"it did not come up");
        }
    }

    /// Start the login chain with `init`'s two endpoints and a route to each server a session
    /// binds, never the server's own endpoint.
    fn start_login_chain(&mut self) {
        let chain = ChainEndpoints {
            fs: self.fs_endpoint.take(),
            profile: self.profile_endpoint.take(),
            tty: self.route_copy("tty-server", None),
            draw: self.route_copy("compositor", None),
            clip: self.route_copy("clipboard-server", None),
            views: self.route_copy("view-broker", None),
            devices: self.route_copy("device-mgr", Some("info-endpoint")),
            storage: self.route_copy("storage-service", Some("session-endpoint")),
            services: dup_endpoint(self.services.session_client.as_ref()),
        };
        self.supervisors = bring_up_login_chain(&self.root, chain);
    }
}

/// How long a server has to answer for an endpoint it mints for sessions.
const DERIVE_TIMEOUT_NS: u64 = 5_000_000_000;

/// How long to wait for a service named in another's `after` to finish.
///
/// Bounded because the wait is for something that may never happen: `after` means "has
/// exited", and nothing stops a declaration from naming a service that keeps running. A hang
/// would be a boot that stops with no message.
const AFTER_TIMEOUT_NS: u64 = 20_000_000_000; // 20 s

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    log::write(b"service-mgr: PANIC\n");
    libos::exit(1);
}
