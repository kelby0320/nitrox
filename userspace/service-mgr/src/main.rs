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

use libkern::debug::Line;
use libkern::*;
use service_mgr::bringup::{self, Entry, Failed, Step};
use service_mgr::registry;
use service_mgr::service_toml::{self, Backoff, RestartConfig, RestartPolicy, ServiceDecl};
use service_mgr::services;

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// How many declared services this supervisor holds at once. Not bounded by the wait set: only a
/// server still starting has its control channel waited on, since a death is also a
/// `ChildExited` on the notification channel.
const MAX_SERVICES: usize = 24;
const WAIT_MAX: usize = libkern::abi::MAX_WAIT_HANDLES;

/// The wait set [`Mgr::run`] builds.
static mut WAIT_HANDLES: [u64; WAIT_MAX] = [0; WAIT_MAX];
static mut WAIT_RESULTS: [u8; 24 * WAIT_MAX] = [0; 24 * WAIT_MAX];
static mut SRV_MSG: [u8; 4096] = [0; 4096];
/// The login supervisors' process and control handles, kept for a shutdown.
static mut SUPERVISORS: [u64; 4] = [0; 4];
/// How deep each route is: a full ring answers the resolver `WouldBlock` at once.
const SERVE_DEPTH: u64 = 64;
static mut NOTIF: Notification = Notification::zeroed();
static mut CLOCK_BUF: u64 = 0;
static mut CTRL_OUT0: u64 = 0;
static mut CTRL_OUT1: u64 = 0;
static mut SEND_MSG: IpcMsg = IpcMsg::ZEROED;
static mut SEND_HANDLES: [u64; 8] = [0; 8];
static mut RDY_MSG: [u8; 4096] = [0; 4096];

const READY_TIMEOUT_NS: u64 = 30_000_000_000; // 30 s

/// Spawn args for `session-mgr`: its control endpoint is moved in at `handles[0]`, and it is
/// re-delegated `BIND_NAMESPACE` to construct per-session namespaces.
static mut SPAWN_SESSION: SpawnArgs = SpawnArgs {
    image: 0,
    handle_count: 1,
    move_mask: 1,
    arg0: 0,
    handles: [0; 4],
    rights: [RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT, 0, 0, 0],
    namespace: 0,
    syscaps: SYSCAP_BIND_NAMESPACE,
};

/// Spawn args for `desktop-session-mgr`. **The two supervisors are unaware of each other**, so
/// serial stays the recovery path; the same user may be logged in twice.
static mut SPAWN_DESKTOP_SESSION: SpawnArgs = SpawnArgs {
    image: 0,
    handle_count: 1,
    move_mask: 1,
    arg0: 0,
    handles: [0; 4],
    rights: [RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT, 0, 0, 0],
    namespace: 0,
    syscaps: SYSCAP_BIND_NAMESPACE,
};

static mut SPAWN_SERVICE: SpawnArgs = SpawnArgs {
    image: 0,
    handle_count: 0,
    move_mask: 0,
    arg0: 0,
    handles: [0; 4],
    rights: [0; 4],
    namespace: 0,
    syscaps: 0,
};

fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`; the slice is valid.
    unsafe {
        syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0);
    }
}

fn restart_name(p: RestartPolicy) -> &'static [u8] {
    match p {
        RestartPolicy::Never => b"never",
        RestartPolicy::OnFailure => b"on-failure",
        RestartPolicy::Always => b"always",
    }
}

/// Resolve `path` in `ns`: the handle, the caller's to close, or `0` on failure.
fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> u64 {
    libfs::lookup_wait(ns, path, rights).1
}

/// Resolve the service's log endpoint under the logging service's `system/` subtree (only a
/// supervisor's namespace permits it), or `0`. The logging service stamps the trusted
/// `principal` and `tier` from *this* channel; the service never names itself.
fn resolve_log_endpoint(registry: u64, name: &str) -> u64 {
    let path = format!("/logging-service/system/{name}");
    // `TRANSFER` to move it into the child at spawn, which receives it attenuated to `SEND`.
    ns_lookup(registry, path.as_bytes(), RIGHT_SEND | RIGHT_TRANSFER)
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
    // SAFETY: CLOCK_BUF is a valid writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: on success the kernel wrote the ns count into CLOCK_BUF.
    unsafe { (&raw const CLOCK_BUF).read() }
}

fn create_control_channel() -> Option<(u64, u64)> {
    // **Depth 10, and the number bounds the send count rather than being a round one.** The
    // handoffs below are `SENDMODE_NOBLOCK` against a child that has not run yet, so a ring shorter
    // than the number of them does not block — it **drops the last handle silently**, and the
    // send reports success. Nine handoffs go to `desktop-session-mgr`, so it is ten deep for one
    // spare. `libsession::spawn_leader` carries the same warning.
    // SAFETY: CTRL_OUT0/CTRL_OUT1 are valid writable out-params.
    let cr = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut CTRL_OUT0) as u64, (&raw mut CTRL_OUT1) as u64, 10, 0) };
    if cr != 0 {
        return None;
    }
    // SAFETY: on success the kernel wrote both endpoint handles.
    let (a, b) = unsafe { ((&raw const CTRL_OUT0).read(), (&raw const CTRL_OUT1).read()) };
    Some((a, b))
}

/// Move the log endpoint to a service as its first control message; closed if that failed.
fn send_log_handoff(ctrl: u64, log_ep: u64) {
    // SAFETY: SEND_MSG/SEND_HANDLES are valid buffers; transfer one handle, empty payload.
    let sr = unsafe {
        (&raw mut SEND_MSG.header.payload_len).write(0);
        SEND_HANDLES[0] = log_ep;
        syscall5(
            SYS_CHANNEL_SEND,
            ctrl,
            (&raw const SEND_MSG) as u64,
            (&raw const SEND_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        )
    };
    if sr != 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, log_ep) };
    }
}

fn send_control(ctrl: u64, op: u8) {
    if ctrl == 0 {
        return;
    }
    // SAFETY: SEND_MSG/SEND_HANDLES are valid buffers; write the 1-byte control payload.
    unsafe {
        (&raw mut SEND_MSG.header.payload_len).write(1);
        (&raw mut SEND_MSG.payload[0]).write(op);
        syscall5(
            SYS_CHANNEL_SEND,
            ctrl,
            (&raw const SEND_MSG) as u64,
            (&raw const SEND_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        );
    }
}

fn send_finish(terminal: u64, reboot: bool) {
    if terminal == 0 {
        kprint(b"service-mgr: no terminal channel, so init cannot be told to finish\n");
        return;
    }
    // SAFETY: SEND_MSG/SEND_HANDLES are valid buffers; a two-byte payload and no handles.
    unsafe {
        (&raw mut SEND_MSG.header.payload_len).write(2);
        (&raw mut SEND_MSG.payload[0]).write(TERMINAL_OP_FINISH);
        (&raw mut SEND_MSG.payload[1]).write(reboot as u8);
        syscall5(
            SYS_CHANNEL_SEND,
            terminal,
            (&raw const SEND_MSG) as u64,
            (&raw const SEND_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        );
    }
}

/// Read + parse the declarations from `/system/services.toml` on the root filesystem, which
/// `init` has mounted before it spawns this process. Empty (with a logged reason) if the file is
/// absent or holds nothing well-formed. A **test image differs from a release image by data**.
/// Also returns whether a critical declaration was skipped, each skipped one being said here
/// since the parser cannot log (`bringup::unfit`).
fn load_declarations(root_ns: u64) -> (alloc::vec::Vec<ServiceDecl>, bool) {
    let read = libfs::read_file(root_ns, b"/system/services.toml").ok();
    let text = match read.and_then(|b| String::from_utf8(b).ok()) {
        Some(t) => t,
        None => {
            kprint(b"service-mgr: no service declarations found at /system/services.toml\n");
            return (alloc::vec::Vec::new(), false);
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
        kprint(b"service-mgr: declaration parse error\n");
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

/// Spawn the service `decl` names with a fresh control channel; returns `(proc_handle,
/// control_end)`, the latter `0` if there is no channel.
fn spawn_service(root_ns: u64, registry: u64, decl: &ServiceDecl) -> (i64, u64) {
    let image = ns_lookup(root_ns, decl.executable.as_bytes(), RIGHT_MAP_READ);
    if image == 0 {
        Line::new().s(b"service-mgr: image not found: ").s(decl.executable.as_bytes()).end();
        return (-1, 0);
    }
    let (smgr_end, svc_end) = match create_control_channel() {
        Some(pair) => pair,
        None => {
            kprint(b"service-mgr: control channel create FAIL (spawning without control)\n");
            (0, 0)
        }
    };
    // **Not for a server**: it resolves its own log, and its first control message is its
    // `Meta::Ready`. And it is resolved **in the registry**, not through `/log`: that path
    // forwards to this process, which would be waiting on itself.
    let log_ep = if decl.endpoint.is_some() { 0 } else { resolve_log_endpoint(registry, &decl.name) };
    if log_ep == 0 && decl.endpoint.is_none() {
        kprint(b"service-mgr: log endpoint resolve FAIL (spawning without logging)\n");
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
    // SAFETY: SPAWN_SERVICE is this process's spawn block, owned by this single thread; `image`
    // and `svc_end` are handles we hold, and the kernel copies the block during the call
    let h = unsafe {
        SPAWN_SERVICE.image = image;
        SPAWN_SERVICE.syscaps = decl.syscaps;
        if svc_end != 0 {
            SPAWN_SERVICE.handles[0] = svc_end;
            SPAWN_SERVICE.handle_count = 1;
            SPAWN_SERVICE.move_mask = 1;
            // A server sends `Meta::Ready`; any other service's end is one-way.
            SPAWN_SERVICE.rights[0] = if decl.endpoint.is_some() {
                RIGHT_SEND | RIGHT_RECV | RIGHT_TRANSFER | RIGHT_WAIT
            } else {
                RIGHT_RECV | RIGHT_WAIT
            };
        } else {
            SPAWN_SERVICE.handle_count = 0;
            SPAWN_SERVICE.move_mask = 0;
        }
        syscall1(SYS_PROCESS_SPAWN, (&raw const SPAWN_SERVICE) as u64)
    };
    // SAFETY: the kernel copied the ELF during spawn; closing our own image handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, image) };
    if h < 0 {
        kprint(b"service-mgr: spawn FAIL\n");
        // SAFETY: closing our own handles (0 is ignored by the kernel).
        unsafe {
            if smgr_end != 0 {
                syscall1(SYS_HANDLE_CLOSE, smgr_end);
                syscall1(SYS_HANDLE_CLOSE, svc_end);
            }
            if log_ep != 0 {
                syscall1(SYS_HANDLE_CLOSE, log_ep);
            }
        }
        return (h, 0);
    }
    // The spawn ABI delivers one handle to a register, so the log endpoint follows on the
    // control channel.
    if log_ep != 0 {
        if smgr_end != 0 {
            send_log_handoff(smgr_end, log_ep);
        } else {
            // SAFETY: closing our own handle.
            unsafe { syscall1(SYS_HANDLE_CLOSE, log_ep) };
        }
    }
    (h, smgr_end)
}

fn spawn_with_control(root_ns: u64, path: &[u8], args: *mut SpawnArgs) -> (i64, u64) {
    let image = ns_lookup(root_ns, path, RIGHT_MAP_READ);
    if image == 0 {
        kprint(b"service-mgr: image not found (login chain)\n");
        return (-1, 0);
    }
    let (smgr_end, child_end) = match create_control_channel() {
        Some(pair) => pair,
        None => {
            // SAFETY: closing our own image handle.
            unsafe { syscall1(SYS_HANDLE_CLOSE, image) };
            return (-1, 0);
        }
    };
    // SAFETY: `args` is a valid writable arg block; move the control end into the child.
    let h = unsafe {
        (*args).image = image;
        (*args).handles[0] = child_end;
        (*args).handle_count = 1;
        (*args).move_mask = 1;
        syscall1(SYS_PROCESS_SPAWN, args as u64)
    };
    // SAFETY: the kernel copied the ELF during spawn; close our image handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, image) };
    if h < 0 {
        // SAFETY: closing our own handles.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, smgr_end);
            syscall1(SYS_HANDLE_CLOSE, child_end);
        }
        return (h, 0);
    }
    (h, smgr_end)
}

/// Receive one handle from init's handoff channel; `0` if the message was empty or the receive
/// failed. **Bounded**: an unbounded wait would hang the supervisor on a message never coming.
fn recv_handoff(ctrl: u64) -> u64 {
    let mut now: u64 = 0;
    // SAFETY: `&now` is a valid u64 out-param
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut now) as u64) };
    let deadline = now.saturating_add(READY_TIMEOUT_NS);
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS valid; one waiter, bounded deadline.
    let waited = unsafe {
        WAIT_HANDLES[0] = ctrl;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, deadline)
    };
    if waited < 1 {
        kprint(b"service-mgr: handoff timeout\n");
        return 0;
    }
    // The one handle a handoff brings; any more close as `sent` drops.
    // SAFETY: single-threaded; `RDY_MSG` is lent for the call.
    match unsafe { channel_recv_raw(ctrl, &raw mut RDY_MSG) } {
        Ok(mut sent) => sent.take(0).unwrap_or(0),
        Err(_) => 0,
    }
}

/// Move `handle` to a child over its control channel; closed if the send fails. A zero `handle`
/// sends **an empty message**: the receiver reads the handoffs positionally.
fn send_handle(ctrl: u64, handle: u64) {
    let count = if handle == 0 { 0 } else { 1 };
    // SAFETY: SEND_MSG/SEND_HANDLES valid; transfer `count` handles, empty payload.
    let sr = unsafe {
        (&raw mut SEND_MSG.header.payload_len).write(0);
        SEND_HANDLES[0] = handle;
        syscall5(
            SYS_CHANNEL_SEND,
            ctrl,
            (&raw const SEND_MSG) as u64,
            (&raw const SEND_HANDLES) as u64,
            count,
            SENDMODE_NOBLOCK,
        )
    };
    if sr != 0 && handle != 0 {
        // SAFETY: the transfer failed; reclaim the handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, handle) };
    }
}

/// **What the login supervisors are handed**, in order. `fs` and `profile` are `init`'s servers'
/// own endpoints, couriered: this manager uses neither. Every other is a **route**, so a
/// restarted server is reached again from every session. `0` for one this boot lacks.
#[derive(Default)]
struct ChainEndpoints {
    fs: u64,
    profile: u64,
    tty: u64,
    /// The compositor's, which only the graphical column takes.
    draw: u64,
    clip: u64,
    views: u64,
    devices: u64,
    storage: u64,
    /// The session endpoint for `/svc/services`: the table, and no admin.
    services: u64,
}

impl ChainEndpoints {
    /// Every handle, in the order `desktop-session-mgr` receives them.
    fn all(&self) -> [u64; 9] {
        [self.fs, self.profile, self.tty, self.draw, self.clip, self.views, self.devices, self.storage, self.services]
    }

    fn duplicate(&self) -> ChainEndpoints {
        // SAFETY: each is a handle this process holds, or `0`.
        unsafe {
            ChainEndpoints {
                fs: dup_endpoint(self.fs),
                profile: dup_endpoint(self.profile),
                tty: dup_endpoint(self.tty),
                draw: 0,
                clip: dup_endpoint(self.clip),
                views: dup_endpoint(self.views),
                devices: dup_endpoint(self.devices),
                storage: dup_endpoint(self.storage),
                services: dup_endpoint(self.services),
            }
        }
    }

    /// Close a set no supervisor took: this manager never exits, so it would be held for good.
    fn close(&self) {
        for h in self.all() {
            close(h);
        }
    }
}

/// Bring up the login chain: `session-mgr`, then `desktop-session-mgr`, each handed its
/// endpoints over its control channel. **The serial set is duplicated before it is sent**, since
/// `send_handle` moves: a failure here then costs the graphical login, not both.
fn bring_up_login_chain(root_ns: u64, chain: ChainEndpoints) {
    if chain.fs == 0 {
        kprint(b"service-mgr: no fs endpoint; skipping login chain\n");
        chain.close();
        return;
    }
    if chain.profile == 0 {
        kprint(b"service-mgr: no profile endpoint; sessions will have no /bin\n");
    }
    let mut serial = chain;
    let draw = core::mem::take(&mut serial.draw);
    let mut desktop = serial.duplicate();
    desktop.draw = draw;
    let (sess_h, sess_ctrl) = spawn_with_control(root_ns, b"/bin/session-mgr", &raw mut SPAWN_SESSION);
    if sess_h < 0 || sess_ctrl == 0 {
        kprint(b"service-mgr: session-mgr spawn FAIL\n");
        serial.close();
        desktop.close();
        return;
    }
    // **Positional**: a reorder silently makes a session bind its home to the profile server.
    let s = &serial;
    for h in [s.fs, s.profile, s.tty, s.clip, s.views, s.devices, s.storage, s.services] {
        send_handle(sess_ctrl, h);
    }
    // Kept, not closed: shutdown asks the supervisors to end their sessions.
    // SAFETY: single-threaded; the supervisors' slots.
    unsafe {
        SUPERVISORS[0] = sess_h as u64;
        SUPERVISORS[1] = sess_ctrl;
    }
    if !bring_up_desktop_session(root_ns, desktop) {
        kprint(b"service-mgr: no graphical login (serial login is unaffected)\n");
    }
    kprint(b"service-mgr: login chain up (auth-service + session-mgr)\n");
}

fn bring_up_desktop_session(root_ns: u64, set: ChainEndpoints) -> bool {
    if set.fs == 0 {
        set.close();
        return false;
    }
    let (h, ctrl) = spawn_with_control(root_ns, b"/bin/desktop-session-mgr", &raw mut SPAWN_DESKTOP_SESSION);
    if h < 0 || ctrl == 0 {
        kprint(b"service-mgr: desktop-session-mgr spawn FAIL\n");
        set.close();
        return false;
    }
    for h in set.all() {
        send_handle(ctrl, h);
    }
    // SAFETY: single-threaded; the supervisors' slots.
    unsafe {
        SUPERVISORS[2] = h as u64;
        SUPERVISORS[3] = ctrl;
    }
    true
}

/// # Safety
/// `h` must be a handle this process owns, or `0`.
unsafe fn dup_endpoint(h: u64) -> u64 {
    if h == 0 {
        return 0;
    }
    // SAFETY: the caller guarantees `h` is ours.
    let d = unsafe { syscall2(SYS_HANDLE_DUPLICATE, h, RIGHT_TRANSFER | RIGHT_DUPLICATE) };
    if d < 0 { 0 } else { d as u64 }
}

/// Bootstrap registers: `rdi` = notification channel, `rsi` = the lookup-only root namespace,
/// `rdx` = the **terminal channel** init moved in, `rcx` unused.
///
/// **What comes down the terminal channel first:**
/// 1. a root handle with `init`'s own rights, `BIND` and `UNBIND` among them, since binding the
///    servers is this manager's job and it is in `init`'s trust tier;
/// 2. the root filesystem's endpoint, and 3. the profile server's, both for the login chain.
///
/// The channel then stays open for asking `init` for the emergency shell. `0` means `init` made
/// no root handle, and then no server can be bound, which is said.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, terminal: u64, _arg0: u64) -> ! {
    kprint(b"service-mgr: up\n");
    let (root_bind, fs_endpoint, profile_endpoint) = if terminal == 0 {
        (0, 0, 0)
    } else {
        (recv_handoff(terminal), recv_handoff(terminal), recv_handoff(terminal))
    };
    if root_bind == 0 {
        kprint(b"service-mgr: no root handle from init -- no server can be bound\n");
    }
    // SAFETY: register-only syscall; returns a fresh namespace this process holds every right on.
    let registry = match unsafe { syscall0(SYS_NS_CREATE) } {
        n if n > 0 => n as u64,
        _ => {
            kprint(b"service-mgr: registry create FAIL -- no server can be reached\n");
            0
        }
    };
    let (decls, skipped_critical) = load_declarations(root_ns);
    let entries: alloc::vec::Vec<Entry> =
        decls.iter().map(|d| Entry { server: d.endpoint.is_some(), critical: d.critical }).collect();
    let svcs = decls
        .into_iter()
        .map(|decl| Supervised {
            decl,
            proc_h: 0,
            ctrl: 0,
            attempts: 0,
            running: false,
            requested_shutdown: false,
            phase: Phase::Down,
            endpoint: 0,
            root_bound: false,
            restart_at: None,
            exited: None,
            restarts: 0,
        })
        .collect();
    let mut m = Mgr {
        notif,
        root_ns,
        root_bind,
        registry,
        routes: alloc::vec::Vec::new(),
        services: Services::default(),
        held: alloc::vec::Vec::new(),
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

fn make_channel(depth: u64) -> Option<(u64, u64)> {
    let (mut a, mut b) = (0u64, 0u64);
    // SAFETY: valid writable out-params.
    let r = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut a) as u64, (&raw mut b) as u64, depth, 0) };
    (r == 0).then_some((a, b))
}

fn close(h: u64) {
    if h != 0 {
        // SAFETY: closing a handle this process owns.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
    }
}

/// Bind `endpoint` at `path` in `ns`, forwarding with `base` if any; `0` on success.
fn bind(ns: u64, path: &[u8], endpoint: u64, base: Option<&[u8]>) -> i64 {
    let (bp, bl) = base.map_or((0, 0), |b| (b.as_ptr() as u64, b.len() as u64));
    // SAFETY: a namespace handle this process holds with BIND, a valid path and base, and an
    // endpoint it holds.
    unsafe { syscall6(SYS_NS_BIND, ns, path.as_ptr() as u64, path.len() as u64, endpoint, bp, bl) }
}

fn unbind(ns: u64, path: &[u8]) {
    // SAFETY: a namespace handle this process holds with UNBIND, and a valid path.
    unsafe { syscall3(SYS_NS_UNBIND, ns, path.as_ptr() as u64, path.len() as u64) };
}

static mut REPLY_BUF: [u8; 4096] = [0; 4096];

fn send_rs(ch: u64, op: u16, request_id: u64, flags: u32, body: &[u8], handles: &[u64]) -> bool {
    // SAFETY: REPLY_BUF is this process's; single-threaded.
    unsafe {
        let count = handles.len() as u16;
        let Some(n) = librsproto::encode(&mut REPLY_BUF[24..], op, request_id, flags, body, count) else {
            return false;
        };
        REPLY_BUF[4..8].copy_from_slice(&(n as u32).to_le_bytes());
        REPLY_BUF[8] = handles.len() as u8;
        syscall5(
            SYS_CHANNEL_SEND,
            ch,
            (&raw const REPLY_BUF) as u64,
            handles.as_ptr() as u64,
            handles.len() as u64,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

/// An error reply: the whole twelve-byte `ErrorBody`, since a shorter one reaches a forwarded
/// resolve's resolver as `KernelError`.
fn reply_error(ch: u64, op: u16, request_id: u64, err: KError) {
    let mut body = [0u8; librsproto::error::ERROR_BODY_LEN];
    let n = librsproto::error::error_body(&mut body, err.as_i32(), 0, b"").unwrap_or(0);
    let flags = librsproto::RS_FLAG_REPLY | librsproto::RS_FLAG_ERROR;
    let _ = send_rs(ch, op, request_id, flags, &body[..n], &[]);
}

/// Answer a resolve with `SUBNAMESPACE`: `ns`, at `base`, standing for the first `consumed` bytes
/// of the suffix. **A failed send is answered with an error**: a forwarded resolve has no
/// deadline, and one left unanswered hangs its resolver.
fn reply_subnamespace(ch: u64, request_id: u64, ns: u64, consumed: usize, base: &[u8]) {
    use librsproto::namespace::{SUBNAMESPACE_PREFIX_LEN, subnamespace_reply};
    let mut body = [0u8; SUBNAMESPACE_PREFIX_LEN + 64];
    let reply = u16::try_from(consumed).ok().and_then(|c| subnamespace_reply(&mut body, c, base));
    let Some(n) = reply else {
        return reply_error(ch, librsproto::OP_NS_RESOLVE, request_id, KError::InvalidArgument);
    };
    // SAFETY: duplicating a namespace handle this process holds, narrowed to what a continued
    // resolve needs and what moving it needs.
    let dup = unsafe { syscall2(SYS_HANDLE_DUPLICATE, ns, RIGHT_LOOKUP | RIGHT_TRANSFER) };
    if dup <= 0 {
        return reply_error(ch, librsproto::OP_NS_RESOLVE, request_id, KError::KernelError);
    }
    let op = librsproto::OP_NS_RESOLVE;
    if !send_rs(ch, op, request_id, librsproto::RS_FLAG_REPLY, &body[..n], &[dup as u64]) {
        close(dup as u64);
        reply_error(ch, librsproto::OP_NS_RESOLVE, request_id, KError::KernelError);
    }
}

/// One message on this manager's own endpoint, its handles closed; `None` when none is queued.
fn recv_serve(ch: u64) -> Option<(u16, u64, alloc::vec::Vec<u8>)> {
    recv_request(ch).ok().flatten()
}

/// [`recv_serve`], telling **a peer that has gone** apart: `Err(())`. A closed channel stays
/// signalled, so a caller must drop it or the wait loop would spin. A route's cannot close.
fn recv_request(ch: u64) -> Result<Option<(u16, u64, alloc::vec::Vec<u8>)>, ()> {
    loop {
        // SAFETY: single-threaded; `SRV_MSG` is lent for the call.
        let _sent = match unsafe { channel_recv_raw(ch, &raw mut SRV_MSG) } {
            Ok(sent) => sent,
            Err(rr) if rr == KError::PeerClosed.as_i32() as i64 => return Err(()),
            Err(_) => return Ok(None),
        };
        // SAFETY: bounded read of the payload the kernel just wrote.
        let msg = unsafe {
            let len = u32::from_le_bytes([SRV_MSG[4], SRV_MSG[5], SRV_MSG[6], SRV_MSG[7]]) as usize;
            core::slice::from_raw_parts(((&raw const SRV_MSG) as *const u8).add(24), len.min(4096 - 24))
        };
        if let Ok(m) = librsproto::decode(msg) {
            return Ok(Some((m.op, m.request_id, m.body.to_vec())));
        }
    }
}

/// Refuse request `op` on `ch`, saying `why`. **`0` answers no one**: a held request whose session
/// closed keeps going without one ([`Held::session`]).
fn refuse(ch: u64, op: u16, request_id: u64, err: KError, why: &[u8]) {
    if ch == 0 {
        return;
    }
    let mut body = [0u8; librsproto::error::ERROR_BODY_LEN + 160];
    let n = librsproto::error::error_body(&mut body, err.as_i32(), 0, why).unwrap_or(0);
    let flags = librsproto::RS_FLAG_REPLY | librsproto::RS_FLAG_ERROR;
    let _ = send_rs(ch, op, request_id, flags, &body[..n], &[]);
}

fn reply_ok(ch: u64, op: u16, request_id: u64) {
    if ch == 0 {
        return;
    }
    let _ = send_rs(ch, op, request_id, librsproto::RS_FLAG_REPLY, &[], &[]);
}

fn reply_channel(ch: u64, request_id: u64, client_end: u64) -> bool {
    use librsproto::namespace::{OBJECT_KIND_CHANNEL, RESOLVE_REPLY_LEN, resolve_reply};
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    let (op, flags) = (librsproto::OP_NS_RESOLVE, librsproto::RS_FLAG_REPLY);
    let sent = send_rs(ch, op, request_id, flags, &body, &[client_end]);
    if !sent {
        close(client_end);
        reply_error(ch, librsproto::OP_NS_RESOLVE, request_id, KError::KernelError);
    }
    sent
}

fn reply_table(ch: u64, request_id: u64, bytes: &[u8]) {
    use librsproto::namespace::{OBJECT_KIND_MEMOBJ, RESOLVE_REPLY_LEN, resolve_reply};
    let resolve = librsproto::OP_NS_RESOLVE;
    // SAFETY: a plain anonymous object of `bytes.len()`.
    let obj = unsafe { syscall4(SYS_MEMORY_CREATE, bytes.len() as u64, 0, 0, 0) };
    if obj <= 0 {
        return reply_error(ch, resolve, request_id, KError::OutOfMemory);
    }
    let obj = obj as u64;
    let rw = RIGHT_MAP_READ | RIGHT_MAP_WRITE;
    // SAFETY: mapping an object this process just created, to fill it
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, obj, 0, bytes.len() as u64, rw) };
    if addr <= 0 {
        close(obj);
        return reply_error(ch, resolve, request_id, KError::OutOfMemory);
    }
    // SAFETY: `addr` maps at least `bytes.len()` writable bytes.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, bytes.len()) };
    // Unmapped before the reply: the mapping holds its own reference, so left behind it would
    // pin every table ever served.
    // SAFETY: unmapping a range this process mapped moments ago and never reads again.
    unsafe { syscall2(SYS_MEMORY_UNMAP, addr as u64, bytes.len() as u64) };
    let rights = RIGHT_MAP_READ | RIGHT_INSPECT | RIGHT_TRANSFER;
    // SAFETY: narrowing a handle this process holds to what a reader needs: mapping it, `INSPECT`
    // for its size, and moving it
    let ro = unsafe { syscall2(SYS_HANDLE_DUPLICATE, obj, rights) };
    close(obj);
    if ro <= 0 {
        return reply_error(ch, resolve, request_id, KError::KernelError);
    }
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_MEMOBJ, bytes.len() as u32);
    if !send_rs(ch, resolve, request_id, librsproto::RS_FLAG_REPLY, &body, &[ro as u64]) {
        close(ro as u64);
        reply_error(ch, resolve, request_id, KError::KernelError);
    }
}

enum Ready {
    /// A `Meta::Ready`, and the endpoint it moved.
    Endpoint(u64),
    /// A refusal, with its reason; or a message that was no `Ready`.
    Refused(alloc::string::String),
    Nothing,
    Closed,
}

fn take_ready(ctrl: u64) -> Ready {
    // SAFETY: single-threaded; `RDY_MSG` is lent for the call.
    let mut sent = match unsafe { channel_recv_raw(ctrl, &raw mut RDY_MSG) } {
        Ok(sent) => sent,
        Err(rr) if rr == KError::PeerClosed as i64 => return Ready::Closed,
        Err(_) => return Ready::Nothing,
    };
    // SAFETY: bounded read of the payload the kernel just wrote.
    let msg = unsafe {
        let len = u32::from_le_bytes([RDY_MSG[4], RDY_MSG[5], RDY_MSG[6], RDY_MSG[7]]) as usize;
        core::slice::from_raw_parts(((&raw const RDY_MSG) as *const u8).add(24), len.min(4096 - 24))
    };
    let decoded = librsproto::decode(msg).ok().map(|m| (m.op, m.flags, m.body.to_vec()));
    let endpoint = decoded.as_ref().and_then(|d| service_mgr::ready::kept(d.0, d.1)).and_then(|i| sent.take(i));
    let refused = |why: &[u8]| Ready::Refused(alloc::string::String::from_utf8_lossy(why).into_owned());
    match decoded {
        Some((op, flags, body)) if op == librsproto::OP_READY && flags & librsproto::RS_FLAG_ERROR != 0 => {
            let why = librsproto::error::parse_error(&body).map(|e| e.msg).unwrap_or(b"");
            let why: &[u8] = if why.is_empty() { b"it refused, and gave no reason" } else { why };
            refused(why)
        }
        Some((op, _, _)) if op == librsproto::OP_READY => match endpoint {
            Some(endpoint) => Ready::Endpoint(endpoint),
            None => refused(b"a Ready with no endpoint in it"),
        },
        _ => refused(b"its first message was not a Ready"),
    }
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
    client: u64,
    serve: u64,
    /// A derived route's endpoint as the server minted it, bound at `base`; else `0`.
    target: u64,
    /// Whether something is bound at `base`; else a resolve is `NotFound`.
    live: bool,
}

/// **An admin request waiting for what it asked**, answered then, never by a wait here.
struct Held {
    /// The session to answer on, or `0` once it has closed: **what was asked goes on**, a
    /// restart's start included, and only the answer is lost. The handle is not kept, since it
    /// may be reused.
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
    serve: u64,
    /// **The session endpoint**, and the end it is answered on: the table and nothing else.
    /// Every login binds a duplicate at `/dev/services`.
    session_client: u64,
    session_serve: u64,
    admin_ends: alloc::vec::Vec<u64>,
    admin_sessions: alloc::vec::Vec<u64>,
    /// A session opened on a power endpoint takes `Shutdown` and nothing else.
    power_ends: alloc::vec::Vec<u64>,
    power_sessions: alloc::vec::Vec<u64>,
}

/// One supervised service: its declaration, its child, and the state its restart policy needs.
struct Supervised {
    decl: ServiceDecl,
    proc_h: i64,
    /// service-mgr's end of the child's control channel, or `0` when it is not running or the
    /// channel could not be created. **This is the exit discriminator** — see [`Mgr::poll`].
    ctrl: u64,
    attempts: u32,
    running: bool,
    /// A requested stop is never restarted, whatever the policy says.
    requested_shutdown: bool,
    phase: Phase,
    /// This manager's copy of a server's endpoint while it is up, else `0`.
    endpoint: u64,
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
/// the resolves are answered on every pass. The lookups it does make wait on the root filesystem,
/// the profile server and servers already serving, none of which waits on this.
///
/// **The registry.** Each server's endpoint is bound in a namespace of this manager's own, under
/// the server's name, and every other binding of the server's path is one of this manager's
/// [`Route`]s, answered `SUBNAMESPACE` into the registry, so a restart rebinds there alone.
struct Mgr {
    notif: u64,
    /// The lookup-only root, for lookups.
    root_ns: u64,
    /// The root with `init`'s rights, for binding the servers' paths; `0` without one.
    root_bind: u64,
    registry: u64,
    /// One per server path and derived endpoint, kept for the boot.
    routes: alloc::vec::Vec<Route>,
    services: Services,
    held: alloc::vec::Vec<Held>,
    terminal: u64,
    /// `init`'s endpoints for the login chain, until it takes them.
    fs_endpoint: u64,
    profile_endpoint: u64,
    svcs: alloc::vec::Vec<Supervised>,
    entries: alloc::vec::Vec<Entry>,
    started: usize,
    chain_started: bool,
    /// The server bring-up is waiting on: its `Ready` before the next declaration starts.
    awaiting: Option<usize>,
    after_until: Option<u64>,
    /// A critical server failed at boot, or a shutdown began: nothing more starts.
    halted: bool,
    reported: bool,
    shutdown: Option<Shutdown>,
}

impl Mgr {
    fn run(&mut self) -> ! {
        loop {
            self.advance();
            // **The notification channel, every route, and the servers still starting** — whose
            // `Ready` is the one thing a control channel brings. A death needs no slot: it queues
            // `ChildExited`, and each pass looks at every channel ([`Mgr::poll`]).
            let mut count = 0usize;
            // SAFETY: WAIT_HANDLES holds WAIT_MAX slots, and `push` stops there.
            unsafe {
                let mut push = |h: u64| {
                    if h != 0 && count < WAIT_MAX {
                        WAIT_HANDLES[count] = h;
                        count += 1;
                    }
                };
                push(self.notif);
                for r in &self.routes {
                    push(r.serve);
                }
                push(self.services.serve);
                push(self.services.session_serve);
                for &h in self.services.admin_ends.iter().chain(&self.services.admin_sessions) {
                    push(h);
                }
                for &h in self.services.power_ends.iter().chain(&self.services.power_sessions) {
                    push(h);
                }
                for s in &self.svcs {
                    if s.running && matches!(s.phase, Phase::Starting { .. }) {
                        push(s.ctrl);
                    }
                }
            }
            let deadline = self.next_deadline();
            // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid for `count` entries.
            unsafe {
                syscall4(
                    SYS_WAIT,
                    (&raw const WAIT_HANDLES) as u64,
                    count as u64,
                    (&raw mut WAIT_RESULTS) as u64,
                    deadline,
                )
            };
            self.serve_resolves();
            self.serve_services();
            let mut codes = alloc::vec::Vec::new();
            drain_codes(self.notif, &mut codes);
            self.poll(&mut codes);
            self.due();
            self.shutdown_step();
            for code in codes {
                Line::new().s(b"service-mgr: an unsupervised child exited code=").i(code as i64).end();
            }
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

    /// Answer every resolve queued on every route: `SUBNAMESPACE` into the registry at the
    /// route's base while something is bound there, `NotFound` while not.
    fn serve_resolves(&mut self) {
        for r in &self.routes {
            while let Some((op, request_id, body)) = recv_serve(r.serve) {
                let resolve = librsproto::OP_NS_RESOLVE;
                if op != resolve {
                    reply_error(r.serve, op, request_id, KError::Unsupported);
                } else if librsproto::namespace::parse_resolve_request(&body).is_none() {
                    reply_error(r.serve, resolve, request_id, KError::InvalidArgument);
                } else if r.live {
                    reply_subnamespace(r.serve, request_id, self.registry, 0, r.base.as_bytes());
                } else {
                    reply_error(r.serve, resolve, request_id, KError::NotFound);
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
        let mut waiting = alloc::vec::Vec::new();
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
                Some(j) if self.svcs[j].running && self.svcs[j].ctrl != 0 => waiting.push(dep.clone()),
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
        let (h, ctrl) = spawn_service(self.root_ns, self.registry, &self.svcs[i].decl);
        let s = &mut self.svcs[i];
        if h > 0 {
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
            if s.ctrl == 0 {
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
    /// rest, up to [`CODE_GRACE_NS`], answering resolves meanwhile. A death still without a code
    /// is a failure.
    fn poll(&mut self, codes: &mut alloc::vec::Vec<i32>) {
        let mut dead = alloc::vec::Vec::new();
        for i in 0..self.svcs.len() {
            let (running, ctrl, phase) = (self.svcs[i].running, self.svcs[i].ctrl, self.svcs[i].phase);
            if !running || ctrl == 0 {
                continue;
            }
            if let Phase::Starting { .. } = phase {
                match take_ready(ctrl) {
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
                    Ready::Closed => dead.push(i),
                }
            } else if channel_peer_closed(ctrl) {
                dead.push(i);
            }
        }
        if dead.len() > codes.len() {
            let until = now_ns().saturating_add(CODE_GRACE_NS);
            while dead.len() > codes.len() && self.wait_codes(until) {
                drain_codes(self.notif, codes);
            }
        }
        for i in dead {
            let code = if codes.is_empty() { None } else { Some(codes.remove(0)) };
            self.reap(i, code);
        }
    }

    fn wait_codes(&mut self, deadline: u64) -> bool {
        let w = self.wait_serving(self.notif, deadline).is_some();
        self.serve_resolves();
        w
    }

    /// Wait for `h` or any route until `deadline`: `h`'s result if ready; the caller answers
    /// the routes.
    fn wait_serving(&self, h: u64, deadline: u64) -> Option<[u8; 24]> {
        let mut handles = [0u64; WAIT_MAX];
        let mut n = 0;
        for x in core::iter::once(h).chain(self.routes.iter().map(|r| r.serve)) {
            if x != 0 && n < WAIT_MAX {
                handles[n] = x;
                n += 1;
            }
        }
        let mut results = [0u8; 24 * WAIT_MAX];
        // SAFETY: valid wait arrays on this frame, `n` entries.
        let w = unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, n as u64, results.as_mut_ptr() as u64, deadline) };
        (0..w.max(0) as usize)
            .map(|k| &results[24 * k..24 * (k + 1)])
            .find_map(|e| (u64::from_le_bytes(e[..8].try_into().ok()?) == h).then(|| e.try_into().ok()).flatten())
    }

    /// Resolve `path` in `ns`, **answering resolves until it completes** or `deadline` passes:
    /// the server asked may be resolving through a route itself.
    fn lookup_serving(&mut self, ns: u64, path: &[u8], rights: u64, deadline: u64) -> u64 {
        // SAFETY: a namespace handle this process holds, and a valid path.
        let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
        if po <= 0 {
            return 0;
        }
        let po = po as u64;
        let got = loop {
            let done = self.wait_serving(po, deadline);
            self.serve_resolves();
            if let Some(e) = done {
                let status = i32::from_le_bytes([e[8], e[9], e[10], e[11]]);
                let handle = u64::from_le_bytes(e[16..24].try_into().unwrap_or([0; 8]));
                break if status == 0 { handle } else { 0 };
            }
            if now_ns() >= deadline {
                break 0;
            }
        };
        close(po);
        got
    }

    /// A server's `Meta::Ready` arrived: bind its endpoint in the registry, replacing a
    /// predecessor's, and the first time its root path to its route; then derive its endpoints.
    fn register(&mut self, i: usize, endpoint: u64) {
        let name = self.svcs[i].decl.name.clone();
        let path = self.svcs[i].decl.endpoint.clone().unwrap_or_default();
        if !registry::valid_name(&name) || self.registry == 0 {
            Line::new()
                .s(b"service-mgr: '")
                .s(name.as_bytes())
                .s(b"' cannot be registered: its name is not one the registry binds")
                .end();
            close(endpoint);
            self.failed_to_start(i);
            return;
        }
        let at = registry::base(&name);
        unbind(self.registry, at.as_bytes());
        if bind(self.registry, at.as_bytes(), endpoint, None) != 0 {
            Line::new().s(b"service-mgr: '").s(name.as_bytes()).s(b"' registry bind FAIL").end();
            close(endpoint);
            self.failed_to_start(i);
            return;
        }
        let Some(r) = self.route(i, None) else {
            unbind(self.registry, at.as_bytes());
            close(endpoint);
            self.failed_to_start(i);
            return;
        };
        self.routes[r].live = true;
        if !self.svcs[i].root_bound {
            let client = self.routes[r].client;
            let bound = self.root_bind != 0 && bind(self.root_bind, path.as_bytes(), client, None) == 0;
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
        close(s.endpoint);
        s.endpoint = endpoint;
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
        let rights = RIGHT_TRANSFER | RIGHT_DUPLICATE;
        let h = self.lookup_serving(self.registry, path.as_bytes(), rights, deadline);
        let at = registry::derived(&name, through);
        unbind(self.registry, at.as_bytes());
        let route = if h == 0 { None } else { self.route(i, Some(through)) };
        let bound = route.is_some() && bind(self.registry, at.as_bytes(), h, None) == 0;
        let Some(r) = route.filter(|_| bound) else {
            close(h);
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
        close(route.target);
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
        let made = if room { make_channel(SERVE_DEPTH) } else { None };
        let Some((client, serve)) = made else {
            let why: &[u8] = if room { b"channel create FAIL" } else { b"all in use" };
            Line::new().s(b"service-mgr: no route for '").s(name.as_bytes()).s(b"' -- ").s(why).end();
            return None;
        };
        let base = match through {
            None => registry::base(name),
            Some(t) => registry::derived(name, t),
        };
        self.routes.push(Route { svc: i, through, base, client, serve, target: 0, live: false });
        Some(self.routes.len() - 1)
    }

    /// A duplicate of the route to server `name` for a login supervisor while something is bound
    /// there; `0` otherwise.
    fn route_copy(&self, name: &str, through: Option<&str>) -> u64 {
        let found = self.routes.iter().find(|r| r.live && r.through == through && self.svcs[r.svc].decl.name == name);
        match found {
            // SAFETY: a channel end this process holds.
            Some(r) => unsafe { dup_endpoint(r.client) },
            None => 0,
        }
    }

    /// Start nothing more, and ask `init` for the emergency shell. Only before the terminal
    /// server is up, which is what lets the shell take the console.
    fn ask_for_the_emergency_shell(&mut self) {
        self.halted = true;
        kprint(b"service-mgr: asking init for the emergency shell\n");
        send_control(self.terminal, TERMINAL_OP_EMERGENCY);
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
            if s.proc_h > 0 {
                close(s.proc_h as u64);
            }
            close(s.ctrl);
            s.proc_h = 0;
            s.ctrl = 0;
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
            if s.endpoint != 0 {
                unbind(self.registry, registry::base(&s.decl.name).as_bytes());
                close(s.endpoint);
                s.endpoint = 0;
            }
        }
        for r in self.routes.iter_mut().filter(|r| r.svc == i) {
            r.live = false;
            if r.target != 0 {
                unbind(self.registry, r.base.as_bytes());
                close(r.target);
                r.target = 0;
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
                    refuse(h.session, h.op, h.request_id, KError::TimedOut, why);
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
        let Some((client, serve)) = make_channel(SERVE_DEPTH) else {
            kprint(b"service-mgr: /svc/services channel create FAIL\n");
            return;
        };
        if self.root_bind == 0 || bind(self.root_bind, b"/svc/services", client, None) != 0 {
            kprint(b"service-mgr: bind FAIL at /svc/services\n");
            close(client);
            close(serve);
            return;
        }
        // The binding holds its own reference; this end is not needed again.
        close(client);
        self.services.serve = serve;
        kprint(b"service-mgr: serving /svc/services\n");
        // Answered on its own end, so a session reaches the table and never an admin endpoint.
        match make_channel(SERVE_DEPTH) {
            Some((client, serve)) => {
                self.services.session_client = client;
                self.services.session_serve = serve;
            }
            None => kprint(b"service-mgr: services session endpoint create FAIL -- no /dev/services\n"),
        }
    }

    fn table(&self) -> alloc::vec::Vec<u8> {
        let rows: alloc::vec::Vec<(&str, services::State, u32)> = self
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
    /// the one slot about to be freed.
    fn serve_services(&mut self) {
        let mut k = 0;
        while k < self.services.admin_sessions.len() {
            let session = self.services.admin_sessions[k];
            match recv_request(session) {
                Ok(Some((op, request_id, body))) => self.admin_request(session, op, request_id, &body),
                Ok(None) => k += 1,
                Err(()) => {
                    close(session);
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
            let session = self.services.power_sessions[k];
            match recv_request(session) {
                Ok(Some((op, request_id, body))) => self.power_request(session, op, request_id, &body),
                Ok(None) => k += 1,
                // A shutdown already asked goes on: it was answered when it began.
                Err(()) => {
                    close(session);
                    self.services.power_sessions.remove(k);
                }
            }
        }
        let mut k = 0;
        while k < self.services.admin_ends.len() {
            let end = self.services.admin_ends[k];
            match recv_request(end) {
                Ok(Some((op, request_id, _))) => self.open_admin_session(end, op, request_id),
                Ok(None) => k += 1,
                Err(()) => {
                    close(end);
                    self.services.admin_ends.remove(k);
                }
            }
        }
        let mut k = 0;
        while k < self.services.power_ends.len() {
            let end = self.services.power_ends[k];
            match recv_request(end) {
                Ok(Some((op, request_id, _))) => self.open_power_session(end, op, request_id),
                Ok(None) => k += 1,
                Err(()) => {
                    close(end);
                    self.services.power_ends.remove(k);
                }
            }
        }
        let root = self.services.serve;
        if root != 0 {
            loop {
                match recv_request(root) {
                    Ok(Some((op, request_id, body))) => self.answer_resolve(root, op, request_id, &body, false),
                    Ok(None) => break,
                    Err(()) => {
                        close(root);
                        self.services.serve = 0;
                        break;
                    }
                }
            }
        }
        // This manager holds its other end, so it cannot close.
        let session = self.services.session_serve;
        if session != 0 {
            while let Ok(Some((op, request_id, body))) = recv_request(session) {
                self.answer_resolve(session, op, request_id, &body, true);
            }
        }
    }

    /// A resolve on `/svc/services` or a session endpoint: the table, or on the first only, an
    /// admin or power endpoint.
    fn answer_resolve(&mut self, from: u64, op: u16, request_id: u64, body: &[u8], session: bool) {
        let resolve = librsproto::OP_NS_RESOLVE;
        if op != resolve {
            return reply_error(from, op, request_id, KError::Unsupported);
        }
        let Some(r) = librsproto::namespace::parse_resolve_request(body) else {
            return reply_error(from, resolve, request_id, KError::InvalidArgument);
        };
        match services::asked(r.suffix, session) {
            services::Asked::Table => reply_table(from, request_id, &self.table()),
            services::Asked::AdminEndpoint => {
                if self.services.admin_ends.len() >= services::MAX_ADMIN_ENDPOINTS {
                    return reply_error(from, resolve, request_id, KError::WouldBlock);
                }
                let Some((client, ours)) = make_channel(4) else {
                    return reply_error(from, resolve, request_id, KError::KernelError);
                };
                if reply_channel(from, request_id, client) {
                    self.services.admin_ends.push(ours);
                    kprint(b"service-mgr: an admin endpoint minted\n");
                } else {
                    close(ours);
                }
            }
            services::Asked::PowerEndpoint => {
                if self.services.power_ends.len() >= services::MAX_POWER_ENDPOINTS {
                    return reply_error(from, resolve, request_id, KError::WouldBlock);
                }
                let Some((client, ours)) = make_channel(4) else {
                    return reply_error(from, resolve, request_id, KError::KernelError);
                };
                if reply_channel(from, request_id, client) {
                    self.services.power_ends.push(ours);
                    kprint(b"service-mgr: a power endpoint minted\n");
                } else {
                    close(ours);
                }
            }
            services::Asked::Unknown => reply_error(from, resolve, request_id, KError::NotFound),
        }
    }

    fn open_admin_session(&mut self, end: u64, op: u16, request_id: u64) {
        let resolve = librsproto::OP_NS_RESOLVE;
        if op != resolve {
            return reply_error(end, op, request_id, KError::Unsupported);
        }
        if self.services.admin_sessions.len() >= services::MAX_ADMIN_SESSIONS {
            return reply_error(end, resolve, request_id, KError::WouldBlock);
        }
        let Some((client, ours)) = make_channel(4) else {
            return reply_error(end, resolve, request_id, KError::KernelError);
        };
        if reply_channel(end, request_id, client) {
            self.services.admin_sessions.push(ours);
        } else {
            close(ours);
        }
    }

    /// A resolve on an admin or power endpoint opens a session, whatever its suffix.
    fn open_power_session(&mut self, end: u64, op: u16, request_id: u64) {
        let resolve = librsproto::OP_NS_RESOLVE;
        if op != resolve {
            return reply_error(end, op, request_id, KError::Unsupported);
        }
        if self.services.power_sessions.len() >= services::MAX_POWER_SESSIONS {
            return reply_error(end, resolve, request_id, KError::WouldBlock);
        }
        let Some((client, ours)) = make_channel(4) else {
            return reply_error(end, resolve, request_id, KError::KernelError);
        };
        if reply_channel(end, request_id, client) {
            self.services.power_sessions.push(ours);
        } else {
            close(ours);
        }
    }

    /// `Shutdown`, on a power session: answered as soon as it has begun, since nothing comes back
    /// after it, and refused while one is under way.
    fn power_request(&mut self, session: u64, op: u16, request_id: u64, body: &[u8]) {
        use librsproto::services::{OP_SERVICES_SHUTDOWN, parse_shutdown};
        if op != OP_SERVICES_SHUTDOWN {
            let why = b"a power session asks for a shutdown and nothing else";
            return refuse(session, op, request_id, KError::NoAccess, why);
        }
        let Some(reboot) = parse_shutdown(body) else {
            let why = b"a shutdown's body is one byte, to halt or to reboot";
            return refuse(session, op, request_id, KError::InvalidArgument, why);
        };
        if self.shutdown.is_some() {
            return refuse(session, op, request_id, KError::WouldBlock, b"a shutdown is under way");
        }
        reply_ok(session, op, request_id);
        self.begin_shutdown(reboot);
    }

    /// **Begin a shutdown**: nothing more starts, every waiting request is refused, and the login
    /// supervisors are asked to end their sessions; [`Self::shutdown_step`] takes the rest.
    fn begin_shutdown(&mut self, reboot: bool) {
        kprint(if reboot {
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
            refuse(h.session, h.op, h.request_id, KError::WouldBlock, b"the machine is shutting down");
        }
        // The sessions first: each supervisor ends its session and exits, and its control channel
        // closing is how that is seen.
        kprint(b"service-mgr: asking the login supervisors to end their sessions\n");
        for k in [0, 2] {
            // SAFETY: single-threaded; the supervisors' slots.
            let h = unsafe { SUPERVISORS[k] };
            if h != 0 {
                // SAFETY: a register-only syscall on a process handle this manager holds.
                unsafe { syscall1(SYS_PROCESS_TERMINATE, h) };
            }
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
                let gone = |k: usize| {
                    // SAFETY: single-threaded; SUPERVISORS has four slots and `k` is 1 or 3
                    let c = unsafe { SUPERVISORS[k] };
                    c == 0 || channel_peer_closed(c)
                };
                let (text, desktop) = (gone(1), gone(3));
                if !(text && desktop) && now < until {
                    return;
                }
                if text && desktop {
                    kprint(b"service-mgr: the sessions have ended\n");
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
        let running: alloc::vec::Vec<bool> = self.svcs.iter().map(|s| s.running).collect();
        let mut below = below;
        let next = loop {
            match service_mgr::shutdown::next_to_stop(&running, below) {
                Some(i) if self.svcs[i].ctrl == 0 => {
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
                send_control(self.svcs[i].ctrl, CTRL_OP_SHUTDOWN);
                let bound = self.svcs[i].decl.stop_timeout.unwrap_or(SHUTDOWN_STOP_NS);
                Stage::Service { i, until: now.saturating_add(bound) }
            }
            None => {
                kprint(b"service-mgr: every service has been asked to stop; telling init to finish\n");
                send_finish(self.terminal, reboot);
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
            return refuse(session, op, request_id, KError::NoAccess, why);
        }
        if self.shutdown.is_some() {
            return refuse(session, op, request_id, KError::WouldBlock, b"the machine is shutting down");
        }
        let named = |n: &str| self.svcs.iter().position(|s| s.decl.name == n);
        let found = core::str::from_utf8(body).ok().and_then(named);
        let facts = found.map(|i| {
            let s = &self.svcs[i];
            let restart_due = s.restart_at.is_some();
            services::Found { essential: s.decl.essential, running: s.running, restart_due }
        });
        let action = match services::decide(op, facts) {
            Ok(a) => a,
            Err((err, why)) => return refuse(session, op, request_id, err, why.as_bytes()),
        };
        // `decide` has refused a name no service has, so this only returns if that changes.
        let Some(i) = found else { return };
        if self.held.iter().any(|h| h.svc == i) {
            let why = b"it is being started or stopped already";
            return refuse(session, op, request_id, KError::WouldBlock, why);
        }
        let name = self.svcs[i].decl.name.clone();
        match action {
            services::Action::CancelRestart => {
                Line::new().s(b"service-mgr: '").s(name.as_bytes()).s(b"' will not be restarted, as asked").end();
                let s = &mut self.svcs[i];
                s.restart_at = None;
                s.requested_shutdown = true;
                reply_ok(session, op, request_id);
            }
            services::Action::Start => {
                Line::new().s(b"service-mgr: starting '").s(name.as_bytes()).s(b"', as asked").end();
                self.start_asked(i, Held { session, request_id, op, svc: i, awaits: Awaits::Up });
            }
            services::Action::Stop | services::Action::StopThenStart => {
                let then_start = action == services::Action::StopThenStart;
                let ctrl = self.svcs[i].ctrl;
                if ctrl == 0 {
                    let why = b"it has no control channel to ask on";
                    return refuse(session, op, request_id, KError::Unsupported, why);
                }
                Line::new()
                    .s(b"service-mgr: asking '")
                    .s(name.as_bytes())
                    .s(if then_start { b"' to stop, to start it again" } else { b"' to stop" })
                    .end();
                send_control(ctrl, CTRL_OP_SHUTDOWN);
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
            return refuse(held.session, held.op, held.request_id, KError::KernelError, why);
        }
        if matches!(self.svcs[i].phase, Phase::Starting { .. }) {
            self.held.push(held);
        } else {
            reply_ok(held.session, held.op, held.request_id);
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
            _ => reply_ok(h.session, h.op, h.request_id),
        }
    }

    fn settle_up(&mut self, i: usize, up: bool) {
        let Some(k) = self.held.iter().position(|h| h.svc == i && h.awaits == Awaits::Up) else {
            return;
        };
        let h = self.held.remove(k);
        if up {
            reply_ok(h.session, h.op, h.request_id);
        } else {
            refuse(h.session, h.op, h.request_id, KError::TimedOut, b"it did not come up");
        }
    }

    /// Start the login chain with `init`'s two endpoints and a route to each server a session
    /// binds, never the server's own endpoint.
    fn start_login_chain(&mut self) {
        let chain = ChainEndpoints {
            fs: core::mem::take(&mut self.fs_endpoint),
            profile: core::mem::take(&mut self.profile_endpoint),
            tty: self.route_copy("tty-server", None),
            draw: self.route_copy("compositor", None),
            clip: self.route_copy("clipboard-server", None),
            views: self.route_copy("view-broker", None),
            devices: self.route_copy("device-mgr", Some("info-endpoint")),
            storage: self.route_copy("storage-service", Some("session-endpoint")),
            // SAFETY: a channel end this process holds, or `0`.
            services: unsafe { dup_endpoint(self.services.session_client) },
        };
        bring_up_login_chain(self.root_ns, chain);
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

/// How long a service found dead waits for its exit code. **The close can come first**:
/// `sys_process_exit` closes the child's handles before it queues `ChildExited`, so a wake on
/// another CPU can find it dead with the code still to come, and log `code=unknown`.
const CODE_GRACE_NS: u64 = 1_000_000_000;

/// Take every `ChildExited` queued on `notif`, logging each, and append its code to `codes`.
fn drain_codes(notif: u64, codes: &mut alloc::vec::Vec<i32>) {
    loop {
        // SAFETY: NOTIF is a valid 64-byte writable out-param.
        let r = unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) };
        if r != 0 {
            break; // WouldBlock: drained
        }
        // SAFETY: the kernel wrote a 64-byte Notification into NOTIF.
        let (kind, body) = unsafe { ((&raw const NOTIF.kind).read(), (&raw const NOTIF.body).read()) };
        if kind != KIND_CHILD_EXITED {
            continue;
        }
        let cpid = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        let code = i32::from_le_bytes([body[8], body[9], body[10], body[11]]);
        Line::new()
            .s(b"service-mgr: reaped pid=")
            .u(cpid as u64)
            // `.i`, not `.u`: an exit code is signed.
            .s(b" code=")
            .i(code as i64)
            .end();
        codes.push(code);
    }
}

/// Whether `ch`'s peer has gone: drain the endpoint until it answers.
///
/// `sys_channel_recv` answers `WouldBlock` when the ring is merely empty and the peer is alive,
/// `PeerClosed` when it is empty and the peer is gone; that is what makes a control channel an exit
/// discriminator (see [`Mgr::poll`]). **A drain, not a single receive**, because a receive that
/// returns `0` has *consumed* a message: a queued message would otherwise mask the close behind it.
/// A service that is not a server cannot send here — its control end is granted `RECV | WAIT` and
/// no `SEND` — but **a server can**, and this is where a `Ready` lands that came after its
/// deadline.
///
/// A message found here is reported, and **every handle it carried is closed**: a late `Ready`
/// carries the server's endpoint, which nothing binds now, and a server whose endpoint has
/// closed ends itself — its exit is then attributed here, as any other's.
fn channel_peer_closed(ch: u64) -> bool {
    loop {
        // SAFETY: single-threaded; `RDY_MSG` is lent for the call. What came closes as `sent`
        // drops, at the end of this pass.
        let sent = match unsafe { channel_recv_raw(ch, &raw mut RDY_MSG) } {
            Ok(sent) => sent,
            Err(r) if r == KError::PeerClosed as i64 => return true,
            Err(_) => return false, // WouldBlock (alive and quiet), or an error we cannot act on
        };
        let count = sent.count();
        Line::new()
            .s(b"service-mgr: unexpected message on a control channel (dropped, and ")
            .u(count as u64)
            .s(b" handle(s) it carried closed)")
            .end();
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"service-mgr: PANIC\n");
    // SAFETY: terminate with a non-zero code; does not return.
    unsafe { syscall1(SYS_PROCESS_EXIT, 1) };
    loop {
        core::hint::spin_loop();
    }
}
