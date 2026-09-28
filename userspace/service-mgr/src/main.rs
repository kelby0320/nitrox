//! `service-mgr` — the userspace service manager (Phase 3).
//!
//! Spawned by init once critical-path boot is stable, it starts, supervises, and
//! restarts the system's services. See `docs/architecture/service-manager.md`.
//!
//! **The supervision spine:** parse the declarations from the initramfs
//! (`service_toml`), start **every** service in the file, and on a child's exit apply
//! *that child's* restart policy + backoff. Each service gets a **control channel**:
//! service-mgr keeps one end, moves the other to the service at spawn, and can send
//! lifecycle commands — here, a graceful `CTRL_OP_SHUTDOWN`. A supervisor-requested
//! shutdown is distinguished from an unexpected exit, so it is *not* restarted even
//! under `policy = always`.
//!
//! That control channel is also how a child's exit is **attributed**: `KIND_CHILD_EXITED`
//! names a child by pid and nothing maps a process handle to a pid, so the discriminator
//! is which endpoint closed rather than which pid died. See [`supervise`], and
//! `TODO(child-exit-attribution)` for what that still leaves open.
//!
//! `#![no_std]` + `#![no_main]`. Slice A uses `libkern` (raw syscalls) + `libheap`
//! (the `#[global_allocator]`); the design's `librsproto`/`libos` surface arrives with
//! the RS startup protocol in slice B.

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

/// The freeing userspace heap (slice 4), backing `alloc` for the declaration parser.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;


/// How many declared services this supervisor holds at once.
///
/// Bounded by the wait set: `sys_wait` takes at most `MAX_WAIT_HANDLES` handles, and this
/// supervisor spends one on its notification channel and one per running service's control
/// channel — so the ceiling is 31.
///
/// Twenty-four. Since administration Part E.1 a release image declares the **nine servers**
/// `init` used to start, and `heartbeat`; a test image adds seven more. It was twelve while
/// only the demo and the test clients were declared.
const MAX_SERVICES: usize = 24;
/// The wait set's fixed slots: the notification channel, this manager's serving endpoint, and
/// the terminal channel to `init`.
const FIXED_WAIT: usize = 3;

/// The fixed slots, then one control-channel handle per running service — the wait set
/// [`Mgr::run`] builds. Other callers use the first slot with a count of one.
static mut WAIT_HANDLES: [u64; FIXED_WAIT + MAX_SERVICES] = [0; FIXED_WAIT + MAX_SERVICES];
/// One 24-byte `IoResult` per waited handle.
static mut WAIT_RESULTS: [u8; 24 * (FIXED_WAIT + MAX_SERVICES)] = [0; 24 * (FIXED_WAIT + MAX_SERVICES)];
const _: () = assert!(FIXED_WAIT + MAX_SERVICES <= libkern::abi::MAX_WAIT_HANDLES);
/// The serving endpoint's receive buffers: resolves, forwarded from every server path.
static mut SRV_MSG: [u8; 4096] = [0; 4096];
static mut SRV_HANDLES: [u64; 8] = [0; 8];
static mut SRV_COUNT: usize = 0;
/// The login supervisors' process handles and control channels, **kept** since administration
/// Part E.1: shutdown (E.4) asks them to end their sessions. They used to be closed after the
/// handoff.
static mut SUPERVISORS: [u64; 4] = [0; 4];
/// How deep this manager's serving endpoint is. Every forwarded resolve on a server's path
/// queues here, and a full ring answers the resolver `WouldBlock` at once, so it is far deeper
/// than a server's usual four.
const SERVE_DEPTH: u64 = 64;
static mut NOTIF: Notification = Notification::zeroed();
static mut CLOCK_BUF: u64 = 0;
static mut CTRL_OUT0: u64 = 0;
static mut CTRL_OUT1: u64 = 0;
static mut SEND_MSG: IpcMsg = IpcMsg::ZEROED;
static mut SEND_HANDLES: [u64; 8] = [0; 8];
/// Recv buffers for a resource server's `Meta::Ready` (auth-service's client endpoint).
static mut RDY_MSG: [u8; 4096] = [0; 4096];
static mut RDY_HANDLES: [u64; 8] = [0; 8];
static mut RDY_COUNT: usize = 0;

/// Bounded wait for a spawned server's Ready.
const READY_TIMEOUT_NS: u64 = 30_000_000_000; // 30 s


/// Spawn args for `session-mgr`: its control endpoint is moved in at `rdx`
/// (`handles[0]`), over which service-mgr hands it the fs-server endpoint + the auth
/// channel. Re-delegated `BIND_NAMESPACE` (⊆ service-mgr's) so it can construct
/// per-session namespaces.
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

/// Spawn args for `desktop-session-mgr` — `session-mgr`'s graphical twin, same shape.
///
/// **Two supervisors, unaware of each other.** Neither arbitrates and there is no registry:
/// serial stays the recovery path by construction rather than by care, which is
/// `graphical-session.md` governing decision 3 holding trivially. It matches Linux, where
/// `getty` and `gdm` do not coordinate either. The accepted cost is on the record: the same
/// user may be logged in twice, with two namespaces.
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

/// Spawn args for the service being started/restarted. `image` and the control-channel
/// handle are filled per spawn; a leaf service inherits a LOOKUP-only handle to
/// service-mgr's namespace and holds no ambient capabilities.
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

/// Emit `msg` to the serial console via the debug kprint syscall.
fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`; the slice is valid.
    unsafe {
        syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0);
    }
}

/// The display name of a restart policy (for logging).
fn restart_name(p: RestartPolicy) -> &'static [u8] {
    match p {
        RestartPolicy::Never => b"never",
        RestartPolicy::OnFailure => b"on-failure",
        RestartPolicy::Always => b"always",
    }
}

/// Resolve `path` in namespace `ns` (MAP_READ) and return the resolved handle, or `0`
/// on failure. The `PendingOperation` is waited + closed; the resolved handle is the
/// caller's to close. Used both to resolve config files (mapped by `read_file`) and
/// program-image `MemoryObject`s (passed to spawn as `SpawnArgs.image`).
fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> u64 {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe {
        syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights)
    };
    if po < 0 {
        return 0;
    }
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = po as u64;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
    };
    // IoResult: status at bytes 8..12, resolved handle at 16..24.
    let (status, handle) = unsafe {
        (
            i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]),
            u64::from_le_bytes([
                WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
                WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
            ]),
        )
    };
    // SAFETY: closing our own PO handle (the resolved handle is separate).
    unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
    if waited != 1 || status != 0 {
        0
    } else {
        handle
    }
}

/// Resolve the service's System-tier log endpoint — `/log/<name>` under the logging
/// service, at the `system/` subtree (only a supervisor's namespace permits it). Returns
/// a `SEND`-righted channel handle (the service's `log`), or `0` if the logging service
/// is unavailable (spawn then proceeds without structured logging — non-fatal). The
/// logging service stamps the trusted `principal = <name>` / `tier = system` from *this*
/// channel; the service never names itself. See `docs/architecture/logging.md`.
fn resolve_log_endpoint(registry: u64, name: &str) -> u64 {
    let path = format!("/logging-service/system/{name}");
    // `TRANSFER` so service-mgr can move the endpoint into the child at spawn; the child
    // itself receives it attenuated to `SEND` (the spawn grant mask, below).
    ns_lookup(registry, path.as_bytes(), RIGHT_SEND | RIGHT_TRANSFER)
}

/// The backoff wait (ns) for the `attempts`-th restart (0-based) under `cfg`.
fn compute_backoff(cfg: &RestartConfig, attempts: u32) -> u64 {
    match cfg.backoff {
        Backoff::None => 0,
        Backoff::Linear => cfg.initial_ns,
        Backoff::Exponential => cfg
            .initial_ns
            .checked_shl(attempts)
            .unwrap_or(u64::MAX)
            .min(cfg.max_ns),
    }
}

/// Whether a service that exited with `code` should be restarted under `policy`.
fn should_restart(policy: RestartPolicy, code: i32) -> bool {
    match policy {
        RestartPolicy::Never => false,
        RestartPolicy::OnFailure => code != 0,
        RestartPolicy::Always => true,
    }
}

/// Read the monotonic clock (ns).
fn now_ns() -> u64 {
    // SAFETY: CLOCK_BUF is a valid writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: on success the kernel wrote the ns count into CLOCK_BUF.
    unsafe { (&raw const CLOCK_BUF).read() }
}

/// Create a connected control-channel pair (depth 4). Returns `(smgr_end, svc_end)`:
/// service-mgr keeps `smgr_end`, the service receives `svc_end`. `None` on failure.
fn create_control_channel() -> Option<(u64, u64)> {
    // **Depth 8, and the number bounds the send count rather than being a round one.** The
    // handoffs below are `SENDMODE_NOBLOCK` against a child that has not run yet, so a ring
    // shorter than the number of them does not block — it **drops the last handle silently**.
    // This was 4 while the graphical column sent four; M12 Part E's clipboard made it five, and
    // the symptom was a session whose namespace had no `/dev/clipboard` and a copy that failed
    // two processes away, with the send reporting success. **Seven since administration Part
    // B.4** go to `desktop-session-mgr`, so one slot is left. `libsession::spawn_leader` carries
    // the same warning from the same failure in M7 Part F — which is what named this one on
    // sight.
    // SAFETY: CTRL_OUT0/CTRL_OUT1 are valid writable out-params.
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut CTRL_OUT0) as u64, (&raw mut CTRL_OUT1) as u64, 8, 0)
    };
    if cr != 0 {
        return None;
    }
    // SAFETY: on success the kernel wrote both endpoint handles.
    let (a, b) = unsafe { ((&raw const CTRL_OUT0).read(), (&raw const CTRL_OUT1).read()) };
    Some((a, b))
}

/// Hand the resolved log endpoint to a service over its control endpoint, **transferring**
/// it (the service receives it as its first control message — a message with one moved
/// handle and no payload). After this, `log_ep` has moved to the service, or is closed if
/// the transfer failed.
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
        // The transfer failed; the handle did not move — reclaim it.
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, log_ep) };
    }
}

/// Send a control opcode to a service over its control endpoint (`ctrl`). No handles,
/// non-blocking (the control ring is otherwise idle).
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

/// Read + parse the service declarations. Empty (with a logged reason) if the file is
/// absent or holds nothing well-formed. Each `executable` is resolved to a `MemoryObject`
/// at spawn time.
///
/// **One file, every service in it.** The schema said each file declares one service and
/// the manager scans the directory; nothing can enumerate a directory of `.toml` files
/// (the initramfs is a CPIO archive the kernel looks up by name, `sys_ns_enumerate` lists
/// namespace bindings rather than directory entries, and `profile-server` projects only
/// packages' `bin/`), so the schema changed on 2026-08-21 instead. See the decision log.
///
/// This is what lets a **test image differ from a release image by data**: the same
/// `service-mgr` binary reads a file with one more table in it.
fn load_declarations(root_ns: u64) -> alloc::vec::Vec<ServiceDecl> {
    let read = libfs::read_file(root_ns, b"/initramfs/etc/services.toml").ok();
    let text = match read.and_then(|b| String::from_utf8(b).ok()) {
        Some(t) => t,
        None => {
            kprint(b"service-mgr: no service declarations found\n");
            return alloc::vec::Vec::new();
        }
    };
    let mut decls = service_toml::parse_all(&text);
    if decls.is_empty() {
        kprint(b"service-mgr: declaration parse error\n");
        return decls;
    }
    // More than the wait set can hold: keep the first `MAX_SERVICES` and **say** which
    // were dropped. A silent truncation would read as "everything declared is running".
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
    decls
}

/// Spawn the service `decl` names (image already resolved), with a fresh control
/// channel whose service end is moved to the child. Returns `(proc_handle,
/// control_end)`; `control_end` is `0` if the channel couldn't be created.
fn spawn_service(root_ns: u64, registry: u64, decl: &ServiceDecl) -> (i64, u64) {
    // Resolve the declared executable to its ELF `MemoryObject` (path-based spawn).
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
    // Resolve the service's System-tier log endpoint (the `log` handle + stdout/stderr
    // routing). Non-fatal: a service without it just has no structured logging.
    //
    // **Not for a server** (administration Part E.1): a server resolves its own log, as it did
    // under `init`, and its first control message is its `Meta::Ready` — a log handoff first is
    // a message none of them expects. And it is resolved **in the registry**, not through
    // `/log`: that path forwards to this process, which would be waiting on itself.
    let log_ep = if decl.endpoint.is_some() { 0 } else { resolve_log_endpoint(registry, &decl.name) };
    if log_ep == 0 && decl.endpoint.is_none() {
        kprint(b"service-mgr: log endpoint resolve FAIL (spawning without logging)\n");
    }
    Line::new().s(b"service-mgr: starting service '").s(decl.name.as_bytes()).s(b"'").end();
    // SAFETY: SPAWN_SERVICE is a valid writable arg block. Move the control endpoint into
    // the child (RECV + WAIT only) at `rdx`. The spawn ABI delivers only one handle to a
    // register, so the log endpoint is handed over the control channel after spawn (below),
    // mirroring init's device handoff to an fs-server.
    // **Declared authority.** Almost every service declares none; the demo chain declares
    // `BIND_NAMESPACE` because it constructs a namespace and binds `/session/user` into it.
    //
    // **The kernel *attenuates*, it does not refuse.** `sys_process_spawn` computes
    // `child = parent & requested` — a silent intersection — so a declaration asking for more
    // than service-mgr holds spawns successfully with the extra bits simply gone. An earlier
    // version of this comment said the spawn fails; it does not, and the log line below said
    // "granted" for bits the child never received (PR #229 review, finding 2).
    //
    // service-mgr cannot check the subset itself: nothing reports a process its own syscaps
    // (`/proc/self/status` carries pid and tid only), so it cannot know what it holds without
    // hardcoding a second copy of init's grant. Filed as `TODO(spawn-syscap-attenuation)`.
    // Until then the honest thing is to log what was **requested** and say so.
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
        // "requested", not "granted": the kernel intersects this with what service-mgr holds
        // and reports nothing about what it dropped.
        Line::new()
            .s(b"service-mgr: '")
            .s(decl.name.as_bytes())
            .s(b"' requested syscaps 0x")
            .u(decl.syscaps)
            .s(b" (the kernel grants the subset service-mgr holds)")
            .end();
    }
    let h = unsafe {
        SPAWN_SERVICE.image = image;
        SPAWN_SERVICE.syscaps = decl.syscaps;
        if svc_end != 0 {
            SPAWN_SERVICE.handles[0] = svc_end;
            SPAWN_SERVICE.handle_count = 1;
            SPAWN_SERVICE.move_mask = 1;
            // A server's end is what `init` gave one: it sends `Meta::Ready`, moving its
            // endpoint. Any other service's is one-way, so it cannot send at all.
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
    // The kernel copied the ELF during spawn; close service-mgr's image handle.
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, image) };
    if h < 0 {
        kprint(b"service-mgr: spawn FAIL\n");
        // Nothing was moved (spawn failed) — close the control ends + the log endpoint.
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
    // Hand the log endpoint to the service over its control channel (an IPC transfer —
    // the child receives it as its first control message). service-mgr thus vouches the
    // identity (it resolved `system/<name>`) without the child ever naming itself.
    if log_ep != 0 {
        if smgr_end != 0 {
            send_log_handoff(smgr_end, log_ep);
        } else {
            // No control channel to hand it over — drop it.
            // SAFETY: closing our own handle.
            unsafe { syscall1(SYS_HANDLE_CLOSE, log_ep) };
        }
    }
    // `svc_end` has moved to the child; retain `smgr_end` as the control endpoint.
    (h, smgr_end)
}

/// Spawn a child (resolved from `path`) with a fresh control channel whose child end
/// is moved in at `handles[0]`; returns `(proc_handle, smgr_control_end)`. The caller
/// fills the rest of `args` (rights/syscaps). `smgr_control_end` is `0` on failure.
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
        // Nothing moved (spawn failed) — close both control ends.
        // SAFETY: closing our own handles.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, smgr_end);
            syscall1(SYS_HANDLE_CLOSE, child_end);
        }
        return (h, 0);
    }
    (h, smgr_end)
}


/// Receive one handle from init's handoff channel: an empty message carrying at most one
/// transferred handle. Returns `0` if the message was empty (init had that endpoint
/// missing) or the receive failed.
///
/// **Bounded, not indefinite.** init sends all three handoffs straight after the spawn,
/// so they are in the ring or about to be; a wait that could not end would mean a
/// supervisor hung on a message that is either there or never coming. The same
/// deadline the `Ready` handshake uses is more than enough.
fn recv_handoff(ctrl: u64) -> u64 {
    // SAFETY: `&now` is a valid u64 out-param.
    let mut now: u64 = 0;
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
    // SAFETY: valid recv out-params; the kernel installs any transferred handle at [0].
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ctrl,
            (&raw mut RDY_MSG) as u64,
            (&raw mut RDY_HANDLES) as u64,
            (&raw mut RDY_COUNT) as u64,
        )
    };
    let count = unsafe { (&raw const RDY_COUNT).read() };
    if rr != 0 || count < 1 {
        return 0;
    }
    // SAFETY: the kernel installed the transferred handle at handles[0].
    unsafe { (&raw const RDY_HANDLES[0]).read() }
}

/// Close whichever endpoints we still hold. Used on the login-chain abort paths, where
/// both are ours and neither has moved.
///
/// # Safety
/// Single-threaded service-mgr; both are its own handles, closed at most once.
unsafe fn close_endpoints(fs_endpoint: u64, profile_endpoint: u64) {
    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, fs_endpoint);
        if profile_endpoint != 0 {
            syscall1(SYS_HANDLE_CLOSE, profile_endpoint);
        }
    }
}

/// Transfer a single `handle` to a child over its control channel (`ctrl`) — an IPC
/// message with one moved handle and no payload (the child receives it as its next
/// control message). On failure the handle did not move; it is closed.
///
/// A zero `handle` sends **an empty message**, not nothing. The receiver reads the
/// handoffs positionally, so skipping the send would shift every later one up a slot and
/// hand session-mgr the auth channel where it expects the profile endpoint.
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

/// Bring up the login chain: spawn `auth-service` (await its `Meta::Ready` → the auth
/// client channel), then spawn `session-mgr` with re-delegated `BIND_NAMESPACE` and hand
/// it the fs-server endpoint, the profile-server endpoint, and the auth channel over its
/// control channel. Both endpoints come from init over the handoff channel at `rdx`.
///
/// service-mgr does not *use* either endpoint. It is a courier: neither the fs-server's
/// `/home` nor the profile's `/bin` is service-mgr's to bind, and holding them any longer
/// than the trip down would be authority it has no use for.
fn bring_up_login_chain(
    root_ns: u64,
    fs_endpoint: u64,
    profile_endpoint: u64,
    tty_endpoint: u64,
    draw_endpoint: u64,
    clip_endpoint: u64,
    views_endpoint: u64,
    devices_endpoint: u64,
) {
    if fs_endpoint == 0 {
        kprint(b"service-mgr: no fs endpoint; skipping login chain\n");
        // SAFETY: closing our own handles — the broker's and the device manager's endpoints are
        // no use without a session.
        unsafe {
            close_one(views_endpoint);
            close_one(devices_endpoint);
        }
        // A profile endpoint without an fs endpoint is no more usable — a session with
        // programs but no home is not a session. Don't retain it.
        if profile_endpoint != 0 {
            // SAFETY: closing our own handle.
            unsafe { syscall1(SYS_HANDLE_CLOSE, profile_endpoint) };
        }
        return;
    }
    // Not fatal: a session without `/bin` is the pre-Part-F shell — usable for the
    // in-process language, unable to spawn. Losing the login entirely over it would be a
    // worse trade, so this reports and continues.
    if profile_endpoint == 0 {
        kprint(b"service-mgr: no profile endpoint; sessions will have no /bin\n");
    }
    // auth-service is **init's** now (M7 Part C). It is a resource server bound at
    // `/svc/auth`, and only init can bind into the root namespace: a declared service is
    // spawned with `namespace: 0`, an inherited LOOKUP-only root. This was written here
    // first and the bind came back FAIL, which is how the constraint was found.

    // 2. session-mgr — spawn with BIND_NAMESPACE, then hand it the fs endpoint.
    // Duplicated **before** the serial column takes its set, since `send_handle` moves.
    // `TRANSFER | DUPLICATE` is what the hand-down needs and all it needs.
    // SAFETY: duplicating our own endpoint handles with attenuated rights.
    // **The clipboard is duplicated here too, and it is the first endpoint *both* columns
    // want for the same reason.** `/dev/draw` goes only to the graphical twin because a serial
    // session has no compositor; the clipboard is reachable as a path precisely so a pipeline
    // can use it (M12 decision 4), and pipelines run in both.
    // **And the view broker's** (administration Part A.4), for the clipboard's reason: both
    // columns open sessions with it, since `with` is typed at a shell in either. **And the device
    // manager's** (Part B.4): both columns bind `/dev/devices`.
    let (fs_dup, profile_dup, tty_dup, clip_dup, views_dup, devices_dup) = unsafe {
        (
            dup_endpoint(fs_endpoint),
            dup_endpoint(profile_endpoint),
            dup_endpoint(tty_endpoint),
            dup_endpoint(clip_endpoint),
            dup_endpoint(views_endpoint),
            dup_endpoint(devices_endpoint),
        )
    };
    let (sess_h, sess_ctrl) = spawn_with_control(root_ns, b"/bin/session-mgr", &raw mut SPAWN_SESSION);
    if sess_h < 0 || sess_ctrl == 0 {
        kprint(b"service-mgr: session-mgr spawn FAIL\n");
        // Both sets: the duplicates were minted before this spawn, so this path owns six
        // handles rather than three. service-mgr never exits, so a leak here is permanent.
        // SAFETY: closing our own handles (nothing handed off).
        unsafe {
            close_endpoints(fs_endpoint, profile_endpoint);
            close_endpoints(fs_dup, profile_dup);
            close_one(tty_endpoint);
            close_one(tty_dup);
            close_one(clip_endpoint);
            close_one(clip_dup);
            close_one(views_endpoint);
            close_one(views_dup);
            close_one(devices_endpoint);
            close_one(devices_dup);
        }
        return;
    }
    // Handoffs, in order: (1) the fs-server endpoint, (2) the profile-server endpoint,
    // (3) the tty server's forwarding endpoint, (4) the auth channel. session-mgr
    // receives them positionally, so the order is the
    // contract — a reorder here silently makes a session bind its home over IPC to the
    // profile server.
    send_handle(sess_ctrl, fs_endpoint);
    send_handle(sess_ctrl, profile_endpoint);
    send_handle(sess_ctrl, tty_endpoint);
    // (4) the clipboard server's forwarding endpoint (M12 Part E).
    send_handle(sess_ctrl, clip_endpoint);
    // (5) the view broker's forwarding endpoint (administration Part A.4).
    send_handle(sess_ctrl, views_endpoint);
    // (6) the device manager's forwarding endpoint (administration Part B.4).
    send_handle(sess_ctrl, devices_endpoint);
    // The auth channel is no longer couriered: session-mgr resolves `/svc/auth` for a
    // session of its own, and so will `desktop-session-mgr`.
    // **Kept, not closed** (administration Part E.1): shutdown asks the supervisors to end their
    // sessions, which takes their process handles.
    // SAFETY: single-threaded; the supervisors' slots.
    unsafe {
        SUPERVISORS[0] = sess_h as u64;
        SUPERVISORS[1] = sess_ctrl;
    }
    // **The graphical twin.** It needs the same three endpoints, and `send_handle` *moves*
    // them — so they are duplicated before the serial column is given its set. Duplicating
    // first rather than after means a failure here costs the graphical login, not both:
    // init makes the same argument where it retains the profile endpoint before binding it.
    if !bring_up_desktop_session(
        root_ns,
        fs_dup,
        profile_dup,
        tty_dup,
        draw_endpoint,
        clip_dup,
        views_dup,
        devices_dup,
    ) {
        // Non-fatal by design. A machine with a serial login and no graphical one is
        // degraded; a machine with neither is unreachable, and the serial column is already
        // up by this point.
        kprint(b"service-mgr: no graphical login (serial login is unaffected)\n");
    }
    kprint(b"service-mgr: login chain up (auth-service + session-mgr)\n");
}

/// Spawn `desktop-session-mgr` and hand it its own copies of the three endpoints.
///
/// `false` if it could not be started. Its greeter is a compositor client, so unlike
/// `session-mgr` it also needs `/dev/draw` — which it resolves itself from the inherited root
/// namespace, exactly as every other graphical client does.
fn bring_up_desktop_session(
    root_ns: u64,
    fs: u64,
    profile: u64,
    tty: u64,
    draw: u64,
    clip: u64,
    views: u64,
    devices: u64,
) -> bool {
    if fs == 0 {
        // The duplicates are this function's to release once it declines to use them.
        // SAFETY: closing our own handles.
        unsafe {
            close_one(profile);
            close_one(tty);
            close_one(draw);
            close_one(clip);
            close_one(views);
            close_one(devices);
        }
        return false;
    }
    let (h, ctrl) =
        spawn_with_control(root_ns, b"/bin/desktop-session-mgr", &raw mut SPAWN_DESKTOP_SESSION);
    if h < 0 || ctrl == 0 {
        kprint(b"service-mgr: desktop-session-mgr spawn FAIL\n");
        // SAFETY: closing our own handles (nothing handed off).
        unsafe {
            close_endpoints(fs, profile);
            close_one(tty);
            close_one(draw);
            close_one(clip);
            close_one(views);
            close_one(devices);
        }
        return false;
    }
    // The same positional order `session-mgr` receives in, for the same reason.
    send_handle(ctrl, fs);
    send_handle(ctrl, profile);
    send_handle(ctrl, tty);
    // The fourth, and the one the serial column does not get.
    send_handle(ctrl, draw);
    // The fifth: the clipboard, which both columns get (M12 Part E).
    send_handle(ctrl, clip);
    // The sixth: the view broker's, which both columns get too (administration Part A.4).
    send_handle(ctrl, views);
    // The seventh: the device manager's, which both columns bind at `/dev/devices` (Part B.4).
    // Seven of the control channel's eight — see `create_control_channel`.
    send_handle(ctrl, devices);
    // Kept, as `session-mgr`'s are: shutdown asks this one to end its sessions too.
    // SAFETY: single-threaded; the supervisors' slots.
    unsafe {
        SUPERVISORS[2] = h as u64;
        SUPERVISORS[3] = ctrl;
    }
    true
}

/// Close one handle if there is one.
///
/// # Safety
/// `h` must be a handle this process owns, or `0`.
unsafe fn close_one(h: u64) {
    if h != 0 {
        // SAFETY: the caller guarantees `h` is ours.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
    }
}

/// Duplicate an endpoint handle for a second supervisor, or `0` if there was none.
///
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

/// Bootstrap registers (see init's `_start`): `rdi` = notification channel, `rsi` = the root
/// namespace, lookup-only as every spawned process gets it, `rdx` = the **terminal channel** init
/// moved in, `rcx` unused.
///
/// **What comes down the terminal channel first** (administration Part E.1):
/// 1. a root handle with `init`'s own rights, `BIND` and `UNBIND` among them. `service-mgr` sits in
///    `init`'s trust tier, the maintainer's call, since binding the servers is its job now;
/// 2. the root filesystem's endpoint, and 3. the profile server's, both for the login chain.
///
/// The channel then stays open: it is how `service-mgr` asks `init` for the emergency shell.
/// `0` means `init` could not make it, and then no server can be bound, which is said.
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
    let (forwarding, serve_end) = make_channel(SERVE_DEPTH).unwrap_or_else(|| {
        kprint(b"service-mgr: endpoint create FAIL -- no server can be reached\n");
        (0, 0)
    });
    let decls = load_declarations(root_ns);
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
        })
        .collect();
    let mut m = Mgr {
        notif,
        root_ns,
        root_bind,
        registry,
        forwarding,
        serve_end,
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
    };
    m.run()
}

/// Make a channel pair of `depth`: `(a, b)`.
fn make_channel(depth: u64) -> Option<(u64, u64)> {
    let (mut a, mut b) = (0u64, 0u64);
    // SAFETY: valid writable out-params.
    let r = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut a) as u64, (&raw mut b) as u64, depth, 0) };
    (r == 0).then_some((a, b))
}

/// Close `h` if it is a handle.
fn close(h: u64) {
    if h != 0 {
        // SAFETY: closing a handle this process owns.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
    }
}

/// Bind `endpoint` at `path` in `ns`, forwarding with `base` if there is one. `0` on success.
fn bind(ns: u64, path: &[u8], endpoint: u64, base: Option<&[u8]>) -> i64 {
    let (bp, bl) = base.map_or((0, 0), |b| (b.as_ptr() as u64, b.len() as u64));
    // SAFETY: a namespace handle this process holds with BIND, a valid path and base, and an
    // endpoint it holds.
    unsafe { syscall6(SYS_NS_BIND, ns, path.as_ptr() as u64, path.len() as u64, endpoint, bp, bl) }
}

/// Unbind `path` in `ns`. `NotFound` for a path nothing is bound at, which callers expect.
fn unbind(ns: u64, path: &[u8]) {
    // SAFETY: a namespace handle this process holds with UNBIND, and a valid path.
    unsafe { syscall3(SYS_NS_UNBIND, ns, path.as_ptr() as u64, path.len() as u64) };
}

/// The reply buffer for this manager's own endpoint.
static mut REPLY_BUF: [u8; 4096] = [0; 4096];

/// Send an rsproto message on `ch`, moving `handles`.
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

/// An error reply: the whole twelve-byte `ErrorBody`, since a shorter one on a forwarded resolve
/// reaches the resolver as `KernelError`.
fn reply_error(ch: u64, op: u16, request_id: u64, err: KError) {
    let mut body = [0u8; librsproto::error::ERROR_BODY_LEN];
    let n = librsproto::error::error_body(&mut body, err.as_i32(), 0, b"").unwrap_or(0);
    let flags = librsproto::RS_FLAG_REPLY | librsproto::RS_FLAG_ERROR;
    let _ = send_rs(ch, op, request_id, flags, &body[..n], &[]);
}

/// Answer a resolve with `SUBNAMESPACE`: `ns`, at `base`, standing for the first `consumed` bytes
/// of the suffix. **A failed send is answered with an error**, since a forwarded resolve has no
/// deadline and one left unanswered hangs its resolver.
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

/// One message on this manager's own endpoint: `(op, request_id, body)`. Handles that came with it
/// are closed: a resolve carries none. `None` when nothing is queued.
fn recv_serve(ch: u64) -> Option<(u16, u64, alloc::vec::Vec<u8>)> {
    loop {
        // SAFETY: valid recv out-params.
        let rr = unsafe {
            syscall4(
                SYS_CHANNEL_RECV,
                ch,
                (&raw mut SRV_MSG) as u64,
                (&raw mut SRV_HANDLES) as u64,
                (&raw mut SRV_COUNT) as u64,
            )
        };
        if rr != 0 {
            return None;
        }
        // SAFETY: the kernel wrote the count and the handles it installed.
        let count = unsafe { (&raw const SRV_COUNT).read() }.min(8);
        for k in 0..count {
            // SAFETY: an installed handle, ours to close.
            close(unsafe { (&raw const SRV_HANDLES[k]).read() });
        }
        // SAFETY: bounded read of the payload the kernel just wrote.
        let msg = unsafe {
            let len = u32::from_le_bytes([SRV_MSG[4], SRV_MSG[5], SRV_MSG[6], SRV_MSG[7]]) as usize;
            core::slice::from_raw_parts(((&raw const SRV_MSG) as *const u8).add(24), len.min(4096 - 24))
        };
        if let Ok(m) = librsproto::decode(msg) {
            return Some((m.op, m.request_id, m.body.to_vec()));
        }
    }
}

/// What a starting server's control channel held.
enum Ready {
    /// A `Meta::Ready`, and the endpoint it moved.
    Endpoint(u64),
    /// A refusal, with its reason; or a message that was no `Ready`.
    Refused(alloc::string::String),
    /// Nothing yet.
    Nothing,
    /// The server has gone.
    Closed,
}

/// Look for a starting server's `Meta::Ready` on its control channel, without waiting.
fn take_ready(ctrl: u64) -> Ready {
    // SAFETY: RDY_MSG/RDY_HANDLES/RDY_COUNT are valid writable out-params; non-blocking receive.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ctrl,
            (&raw mut RDY_MSG) as u64,
            (&raw mut RDY_HANDLES) as u64,
            (&raw mut RDY_COUNT) as u64,
        )
    };
    if rr == KError::PeerClosed as i64 {
        return Ready::Closed;
    }
    if rr != 0 {
        return Ready::Nothing;
    }
    // SAFETY: the kernel wrote the count and the handles it installed.
    let count = unsafe { (&raw const RDY_COUNT).read() }.min(8);
    // SAFETY: as above.
    let handles: alloc::vec::Vec<u64> =
        (0..count).map(|k| unsafe { (&raw const RDY_HANDLES[k]).read() }).collect();
    // SAFETY: bounded read of the payload the kernel just wrote.
    let msg = unsafe {
        let len = u32::from_le_bytes([RDY_MSG[4], RDY_MSG[5], RDY_MSG[6], RDY_MSG[7]]) as usize;
        core::slice::from_raw_parts(((&raw const RDY_MSG) as *const u8).add(24), len.min(4096 - 24))
    };
    let decoded = librsproto::decode(msg).ok().map(|m| (m.op, m.flags, m.body.to_vec()));
    let refused = |why: &[u8], handles: &[u64]| {
        handles.iter().for_each(|&h| close(h));
        Ready::Refused(alloc::string::String::from_utf8_lossy(why).into_owned())
    };
    match decoded {
        Some((op, flags, body))
            if op == librsproto::OP_READY && flags & librsproto::RS_FLAG_ERROR != 0 =>
        {
            let why = librsproto::error::parse_error(&body).map(|e| e.msg).unwrap_or(b"");
            let why: &[u8] = if why.is_empty() { b"it refused, and gave no reason" } else { why };
            refused(why, &handles)
        }
        Some((op, _, _)) if op == librsproto::OP_READY && !handles.is_empty() => {
            handles[1..].iter().for_each(|&h| close(h));
            Ready::Endpoint(handles[0])
        }
        Some((op, _, _)) if op == librsproto::OP_READY => {
            refused(b"a Ready with no endpoint in it", &handles)
        }
        _ => refused(b"its first message was not a Ready", &handles),
    }
}

/// Where a declared service is in its life.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Phase {
    /// Not running, or running and not usable — a server that never became ready.
    Down,
    /// A server spawned and not yet ready: its `Meta::Ready` is due by `deadline`.
    Starting { deadline: u64 },
    /// Running: a server bound in the registry, or any other service.
    Up,
}

/// One supervised service: its declaration, its child, and the state the restart
/// policy needs across exits.
struct Supervised {
    decl: ServiceDecl,
    /// The child's process handle, or `0` when it is not running.
    proc_h: i64,
    /// service-mgr's end of the child's control channel, or `0` when it is not running
    /// (or the channel could not be created). **This is the exit discriminator** — see
    /// [`Mgr::poll`].
    ctrl: u64,
    /// Restarts applied so far, against `decl.restart.max_attempts`.
    attempts: u32,
    running: bool,
    /// A supervisor-requested shutdown is intentional and is never restarted, whatever
    /// the policy says.
    requested_shutdown: bool,
    phase: Phase,
    /// This manager's copy of a server's endpoint while it is up — what the login supervisors are
    /// given copies of. `0` otherwise.
    endpoint: u64,
    /// Whether the server's root path is bound to this manager's endpoint. Once is enough: a
    /// restart rebinds in the registry, and every binding reaches the new server through it.
    root_bound: bool,
    /// When a restart is due, its backoff over. **A deadline, never a sleep**: every resolve on a
    /// server's path waits on this process.
    restart_at: Option<u64>,
}

/// **The service manager's state, and its one loop** (administration Part E.1).
///
/// **It never blocks on anything that can wait on it.** Every resolve on a server's path — the
/// root's `/log`, `/svc/auth`, `/dev/tty` — comes to this process first, so a wait on a server
/// that is itself resolving one of those would be two processes waiting on each other. So a
/// server's `Meta::Ready`, a restart's backoff and an `after` are deadlines in the one wait, and
/// the resolves are answered on every pass. The lookups it does make wait on the root filesystem,
/// the profile server and servers already serving, none of which waits on this.
///
/// **The registry.** Each server's endpoint is bound in a namespace of this manager's own, under
/// the server's name, and every other binding of the server's path is this manager's endpoint
/// with `/<name>` as its base. A resolve there is answered `SUBNAMESPACE` into the registry, so it
/// continues into whichever server is bound there now, and a restart rebinds there alone.
struct Mgr {
    notif: u64,
    /// The root, lookup-only, as this process was spawned with: for lookups.
    root_ns: u64,
    /// The root with `init`'s rights, for binding the servers' paths. `0` without one.
    root_bind: u64,
    registry: u64,
    /// This manager's endpoint: what every server path in the root is bound to.
    forwarding: u64,
    serve_end: u64,
    terminal: u64,
    /// The login chain's endpoints from `init`, until the chain takes them.
    fs_endpoint: u64,
    profile_endpoint: u64,
    svcs: alloc::vec::Vec<Supervised>,
    entries: alloc::vec::Vec<Entry>,
    /// Declarations started so far, in file order.
    started: usize,
    chain_started: bool,
    /// The server bring-up is waiting on: its `Ready` before the next declaration starts.
    awaiting: Option<usize>,
    /// When waiting for the next declaration's `after` gives up.
    after_until: Option<u64>,
    /// A critical server failed at boot: nothing more starts.
    halted: bool,
    /// Whether the supervised list has been reported.
    reported: bool,
}

impl Mgr {
    fn run(&mut self) -> ! {
        loop {
            self.advance();
            let mut count = 0usize;
            // SAFETY: WAIT_HANDLES holds FIXED_WAIT + MAX_SERVICES slots, and at most that many
            // are pushed: MAX_SERVICES is the declarations' cap.
            unsafe {
                let mut push = |h: u64| {
                    if h != 0 && count < FIXED_WAIT + MAX_SERVICES {
                        WAIT_HANDLES[count] = h;
                        count += 1;
                    }
                };
                push(self.notif);
                push(self.serve_end);
                for s in &self.svcs {
                    if s.running {
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
            // **Level-triggered, so everything is looked at**, whichever handle woke the wait.
            self.serve_resolves();
            let mut codes = alloc::vec::Vec::new();
            drain_codes(self.notif, &mut codes);
            self.poll(&mut codes);
            self.due();
            // Anything left belongs to a child this manager does not supervise — the login
            // supervisors, whose exits reach this same channel. Reported rather than dropped.
            for code in codes {
                Line::new().s(b"service-mgr: an unsupervised child exited code=").i(code as i64).end();
            }
        }
    }

    /// The soonest thing that is due: a server's `Ready` deadline, a restart, an `after`.
    fn next_deadline(&self) -> u64 {
        let mut d = self.after_until.unwrap_or(u64::MAX);
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

    /// Answer every resolve queued on this manager's endpoint: `SUBNAMESPACE` into the registry
    /// for a server that is up, `NotFound` for anything else.
    fn serve_resolves(&mut self) {
        if self.serve_end == 0 {
            return;
        }
        while let Some((op, request_id, body)) = recv_serve(self.serve_end) {
            if op != librsproto::OP_NS_RESOLVE {
                reply_error(self.serve_end, op, request_id, KError::Unsupported);
                continue;
            }
            let routed = librsproto::namespace::parse_resolve_request(&body)
                .and_then(|r| registry::route(r.suffix));
            let up = routed.filter(|(name, _)| {
                self.svcs.iter().any(|s| s.decl.name == *name && s.phase == Phase::Up && s.endpoint != 0)
            });
            match up {
                Some((name, consumed)) => {
                    let at = registry::base(name);
                    let (ch, ns) = (self.serve_end, self.registry);
                    reply_subnamespace(ch, request_id, ns, consumed, at.as_bytes());
                }
                None => {
                    reply_error(self.serve_end, librsproto::OP_NS_RESOLVE, request_id, KError::NotFound)
                }
            }
        }
    }

    /// Start what bring-up allows: declarations in file order, each server's `Ready` awaited
    /// before the next, and the login chain after the last server.
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
                    self.start(i);
                }
            }
        }
    }

    /// Whether declaration `i`'s `after` dependencies have finished — or the wait for them has
    /// run out, which is reported and then taken as yes. **`after` means "has exited"**, and it
    /// orders backwards only: a name declared later has not started, and cannot be waited for.
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
                    Line::new()
                        .s(b"service-mgr: what '")
                        .s(name.as_bytes())
                        .s(b"' waited for has finished")
                        .end();
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

    /// Spawn declaration `i`. A server becomes `Starting`, and bring-up waits for its `Ready`.
    fn start(&mut self, i: usize) {
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
            if server {
                self.awaiting = Some(i);
            }
            if s.ctrl == 0 {
                Line::new()
                    .s(b"service-mgr: '")
                    .s(s.decl.name.as_bytes())
                    .s(b"' has no control channel -- its exit cannot be attributed")
                    .end();
            }
        } else if server {
            // The spawn already said why. A server that is not running did not come up.
            self.awaiting = Some(i);
            self.failed_to_start(i);
        }
    }

    /// Look at every running service's control channel: a starting server's `Ready`, and
    /// deaths — then pair the deaths with the codes this wake collected.
    ///
    /// **Which child exited is decided by its control channel closing**, not by the
    /// notification: `KIND_CHILD_EXITED` names a pid, and nothing maps a handle to one
    /// (`TODO(child-exit-attribution)`). The code is taken from the notification queue in arrival
    /// order, and **the close can come first**: `sys_process_exit` closes a child's handles
    /// before it queues the notification, so a wake with more deaths than codes waits for the
    /// rest, up to [`CODE_GRACE_NS`] — answering resolves meanwhile. A death still without a code
    /// is treated as a failure.
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

    /// Wait on the notification channel until `deadline` — answering resolves meanwhile, since
    /// they wait on this process. `false` once the deadline has passed.
    fn wait_codes(&mut self, deadline: u64) -> bool {
        let handles = [self.notif, self.serve_end];
        let n = if self.serve_end != 0 { 2 } else { 1 };
        let mut results = [0u8; 48];
        // SAFETY: valid wait arrays on this frame.
        let w = unsafe {
            syscall4(SYS_WAIT, handles.as_ptr() as u64, n, results.as_mut_ptr() as u64, deadline)
        };
        self.serve_resolves();
        w >= 1
    }

    /// A server's `Meta::Ready` arrived: bind its endpoint in the registry — replacing a
    /// predecessor's — and, the first time, its root path to this manager's endpoint.
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
        if !self.svcs[i].root_bound {
            let bound = self.root_bind != 0
                && self.forwarding != 0
                && bind(self.root_bind, path.as_bytes(), self.forwarding, Some(at.as_bytes())) == 0;
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
        Line::new().s(b"service-mgr: ").s(name.as_bytes()).s(b" bound at ").s(path.as_bytes()).end();
        if self.awaiting == Some(i) {
            self.awaiting = None;
        }
    }

    /// Server `i` did not come up. At bring-up, a critical one stops the boot and asks `init` for
    /// the emergency shell; any other is reported and passed.
    fn failed_to_start(&mut self, i: usize) {
        self.svcs[i].phase = Phase::Down;
        if self.awaiting != Some(i) {
            return;
        }
        self.awaiting = None;
        let name = self.svcs[i].decl.name.clone();
        match bringup::failed_at_boot(self.entries[i]) {
            Failed::Emergency => {
                self.halted = true;
                Line::new()
                    .s(b"service-mgr: '")
                    .s(name.as_bytes())
                    .s(b"' is critical and did not come up -- starting nothing more; ")
                    .s(b"asking init for the emergency shell")
                    .end();
                send_control(self.terminal, TERMINAL_OP_EMERGENCY);
            }
            Failed::Continue => {
                Line::new()
                    .s(b"service-mgr: '")
                    .s(name.as_bytes())
                    .s(b"' did not come up; going on without it")
                    .end();
            }
        }
    }

    /// Service `i` has exited with `code`: take its registry binding away, and apply its
    /// restart policy — a restart **scheduled**, after its backoff.
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
            let mut l = Line::new();
            l.s(b"service-mgr: '").s(s.decl.name.as_bytes()).s(b"' exited");
            match code {
                Some(c) => l.s(b" code=").i(c as i64),
                None => l.s(b" code=unknown"),
            };
            l.end();
            // **Its path answers `NotFound` until it is back**, rather than reaching a server that
            // has gone.
            if s.endpoint != 0 {
                unbind(self.registry, registry::base(&s.decl.name).as_bytes());
                close(s.endpoint);
                s.endpoint = 0;
            }
        }
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
        l.s(b"service-mgr: restarting '")
            .s(s.decl.name.as_bytes())
            .s(b"' (attempt ")
            .u((s.attempts + 1) as u64);
        if s.decl.restart.max_attempts != 0 {
            l.s(b" of ").u(s.decl.restart.max_attempts as u64);
        }
        l.s(b") after ").u(backoff / 1_000_000).s(b"ms backoff").end();
        s.restart_at = Some(now_ns().saturating_add(backoff));
    }

    /// Whatever is due: a restart whose backoff is over, and a server whose `Ready` is late.
    fn due(&mut self) {
        let now = now_ns();
        for i in 0..self.svcs.len() {
            if self.svcs[i].restart_at.is_some_and(|at| at <= now) {
                self.svcs[i].restart_at = None;
                self.svcs[i].attempts += 1;
                let awaiting = self.awaiting;
                self.start(i);
                // A restart after boot does not hold bring-up up; one during it replaces the
                // attempt bring-up was already waiting on.
                if awaiting != Some(i) && self.awaiting == Some(i) {
                    self.awaiting = awaiting;
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

    /// A server's endpoint, duplicated for a login supervisor, or `0` if it is not up.
    fn endpoint_copy(&self, name: &str) -> u64 {
        match self.svcs.iter().find(|s| s.decl.name == name && s.phase == Phase::Up) {
            // SAFETY: an endpoint handle this process holds.
            Some(s) => unsafe { dup_endpoint(s.endpoint) },
            None => 0,
        }
    }

    /// Start the login chain with the servers' endpoints, as `init` used to hand them over. The
    /// device manager's info endpoint is minted by resolving it **in the registry**: through the
    /// root's `/svc/devices` it would come to this process, which would be waiting on itself.
    fn start_login_chain(&mut self) {
        let devices = if self.svcs.iter().any(|s| s.decl.name == "device-mgr" && s.phase == Phase::Up) {
            ns_lookup(self.registry, b"/device-mgr/info-endpoint", RIGHT_TRANSFER | RIGHT_DUPLICATE)
        } else {
            0
        };
        let (fs, profile) = (self.fs_endpoint, self.profile_endpoint);
        self.fs_endpoint = 0;
        self.profile_endpoint = 0;
        bring_up_login_chain(
            self.root_ns,
            fs,
            profile,
            self.endpoint_copy("tty-server"),
            self.endpoint_copy("compositor"),
            self.endpoint_copy("clipboard-server"),
            self.endpoint_copy("view-broker"),
            devices,
        );
    }
}

/// How long to wait for a service named in another's `after` to finish.
///
/// Bounded because the wait is for something that may never happen: `after` means "has
/// exited", and nothing stops a declaration from naming a service that keeps running. A hang
/// there would present as a boot that stops with no message, which is the worst failure this
/// supervisor can produce; timing out and saying so is strictly better.
const AFTER_TIMEOUT_NS: u64 = 20_000_000_000; // 20 s

/// How long a service found dead waits for its exit code. **The close can come first.**
/// `sys_process_exit` closes the child's handles, its control endpoint among them, before it
/// queues `ChildExited`. So on another CPU, a wake can find a service dead with its code still to
/// come. `check-terminal` saw `'boot-probe' exited code=unknown` that way (administration C.7).
/// The gap is the rest of one exit syscall; this bound only matters for a code that never comes.
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
        let (kind, body) =
            unsafe { ((&raw const NOTIF.kind).read(), (&raw const NOTIF.body).read()) };
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
/// `sys_channel_recv` distinguishes the two empty cases — `WouldBlock` (`-11`) when the
/// ring is merely empty and the peer is alive, `PeerClosed` (`-13`) when it is empty and
/// the peer is gone. That difference is what makes a control channel an exit
/// discriminator; see [`supervise`].
///
/// **A drain, not a single receive**, because a receive that returns `0` has *consumed* a
/// message: a queued message would otherwise mask the close behind it and be silently
/// eaten on the way. Today nothing can be queued here — a service's control end is granted
/// `RECV | WAIT` and no `SEND` (`spawn_service`), so the channel is one-way by capability —
/// but "the answer is right because the peer holds no send right" is a fact about a
/// neighbouring function, and this one should not depend on it silently.
///
/// A message that *is* found is reported rather than dropped: there is no service→manager
/// control protocol, so its arrival would mean the grant changed.
fn channel_peer_closed(ch: u64) -> bool {
    loop {
        // SAFETY: RDY_MSG/RDY_HANDLES/RDY_COUNT are valid writable out-params; this is a
        // non-blocking receive on a channel handle service-mgr owns.
        let r = unsafe {
            syscall4(
                SYS_CHANNEL_RECV,
                ch,
                (&raw mut RDY_MSG) as u64,
                (&raw mut RDY_HANDLES) as u64,
                (&raw mut RDY_COUNT) as u64,
            )
        };
        if r == KError::PeerClosed as i64 {
            return true;
        }
        if r != 0 {
            return false; // WouldBlock (alive and quiet), or an error we cannot act on
        }
        kprint(b"service-mgr: unexpected message on a control channel (dropped)\n");
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
