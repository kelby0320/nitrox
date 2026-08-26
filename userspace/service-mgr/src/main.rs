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
use service_mgr::service_toml::{self, Backoff, RestartConfig, RestartPolicy, ServiceDecl};

/// The freeing userspace heap (slice 4), backing `alloc` for the declaration parser.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// One page; a service declaration is assumed to fit (true for the slice-A demo).
const PAGE: u64 = 4096;
/// Slice-A demo: how long to let the service run before requesting a graceful
/// shutdown over its control channel (exercises the control path end to end).
const DEMO_RUN_NS: u64 = 1_100_000_000; // ~1.1s (a few heartbeat beats)

/// How many declared services this supervisor holds at once.
///
/// Bounded by the wait set: `sys_wait` takes at most `MAX_WAIT_HANDLES` handles, and this
/// supervisor spends one on its notification channel and one per running service's control
/// channel — so the ceiling is 31.
///
/// Twelve. A test image declares **seven** since retrofit Part C2 moved `init`'s graphical
/// spawns and demo chain into declarations (`heartbeat`, `display-selftest`, `nxterm`,
/// `ui-testclient`, `input-testclient`, `test-harness`, `boot-probe`); a release image
/// declares one. This was four, which was "well past what the system declares" when the
/// system declared two.
const MAX_SERVICES: usize = 12;

/// `notif`, plus one control-channel handle per running service — the wait set
/// [`supervise`] builds. Other callers use the first slot with a count of one.
static mut WAIT_HANDLES: [u64; 1 + MAX_SERVICES] = [0; 1 + MAX_SERVICES];
/// One 24-byte `IoResult` per waited handle.
static mut WAIT_RESULTS: [u8; 24 * (1 + MAX_SERVICES)] = [0; 24 * (1 + MAX_SERVICES)];
const _: () = assert!(1 + MAX_SERVICES <= libkern::abi::MAX_WAIT_HANDLES);
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
fn resolve_log_endpoint(root_ns: u64, name: &str) -> u64 {
    let path = format!("/log/system/{name}");
    // `TRANSFER` so service-mgr can move the endpoint into the child at spawn; the child
    // itself receives it attenuated to `SEND` (the spawn grant mask, below).
    ns_lookup(root_ns, path.as_bytes(), RIGHT_SEND | RIGHT_TRANSFER)
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

/// Block for `duration_ns` on a one-shot monotonic timer (`timer_h`, reused across
/// backoffs). Best-effort; a `0` handle or duration returns promptly.
fn sleep_ns(timer_h: u64, duration_ns: u64) {
    if timer_h == 0 || duration_ns == 0 {
        return;
    }
    let fire_at = now_ns().saturating_add(duration_ns);
    // SAFETY: arming our own timer (absolute monotonic deadline, one-shot).
    unsafe { syscall4(SYS_TIMER_SET, timer_h, fire_at, 0, 0) };
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    unsafe {
        WAIT_HANDLES[0] = timer_h;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            fire_at.saturating_add(1_000_000_000),
        );
    }
}

/// Create a connected control-channel pair (depth 4). Returns `(smgr_end, svc_end)`:
/// service-mgr keeps `smgr_end`, the service receives `svc_end`. `None` on failure.
fn create_control_channel() -> Option<(u64, u64)> {
    // SAFETY: CTRL_OUT0/CTRL_OUT1 are valid writable out-params.
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut CTRL_OUT0) as u64, (&raw mut CTRL_OUT1) as u64, 4, 0)
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

/// Resolve `path` in namespace `ns`, map the returned read-only `MemoryObject`, and
/// return its trimmed UTF-8 contents. Mirrors init's manifest read. `None` on failure.
fn read_file(ns: u64, path: &[u8]) -> Option<String> {
    let mem = ns_lookup(ns, path, RIGHT_MAP_READ);
    if mem == 0 {
        return None;
    }
    // SAFETY: `mem` is a MemoryObject handle with MAP_READ.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, PAGE, RIGHT_MAP_READ) };
    if addr < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
        return None;
    }
    // SAFETY: `addr` is a MAP_READ page holding the file bytes + zero padding.
    let bytes = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, PAGE as usize) };
    let len = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let text = core::str::from_utf8(&bytes[..len]).ok().map(String::from);
    // SAFETY: closing our own handle (the page mapping persists via its own reference).
    unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
    text
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
    let text = match read_file(root_ns, b"/initramfs/etc/services.toml") {
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
fn spawn_service(root_ns: u64, decl: &ServiceDecl) -> (i64, u64) {
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
    let log_ep = resolve_log_endpoint(root_ns, &decl.name);
    if log_ep == 0 {
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
            SPAWN_SERVICE.rights[0] = RIGHT_RECV | RIGHT_WAIT;
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
/// **Bounded, not indefinite.** init sends both handoffs before service-mgr's first
/// instruction runs, so they are already in the ring; a wait that could not end would
/// mean a supervisor hung on a message that is either there or never coming. The same
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
) {
    if fs_endpoint == 0 {
        kprint(b"service-mgr: no fs endpoint; skipping login chain\n");
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
    let (fs_dup, profile_dup, tty_dup) = unsafe {
        (
            dup_endpoint(fs_endpoint),
            dup_endpoint(profile_endpoint),
            dup_endpoint(tty_endpoint),
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
    // The auth channel is no longer couriered: session-mgr resolves `/svc/auth` for a
    // session of its own, and so will `desktop-session-mgr`.
    // The handoffs are queued in session-mgr's inbox; the control channel + our process
    // handle are no longer needed for Part D (session-mgr runs independently).
    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, sess_ctrl);
        syscall1(SYS_HANDLE_CLOSE, sess_h as u64);
    }
    // **The graphical twin.** It needs the same three endpoints, and `send_handle` *moves*
    // them — so they are duplicated before the serial column is given its set. Duplicating
    // first rather than after means a failure here costs the graphical login, not both:
    // init makes the same argument where it retains the profile endpoint before binding it.
    if !bring_up_desktop_session(root_ns, fs_dup, profile_dup, tty_dup, draw_endpoint) {
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
fn bring_up_desktop_session(root_ns: u64, fs: u64, profile: u64, tty: u64, draw: u64) -> bool {
    if fs == 0 {
        // The duplicates are this function's to release once it declines to use them.
        // SAFETY: closing our own handles.
        unsafe {
            close_one(profile);
            close_one(tty);
            close_one(draw);
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
        }
        return false;
    }
    // The same positional order `session-mgr` receives in, for the same reason.
    send_handle(ctrl, fs);
    send_handle(ctrl, profile);
    send_handle(ctrl, tty);
    // The fourth, and the one the serial column does not get.
    send_handle(ctrl, draw);
    // SAFETY: closing our own handles; the twin runs independently from here.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, ctrl);
        syscall1(SYS_HANDLE_CLOSE, h as u64);
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

/// Bootstrap registers (see init's `_start`): `rdi` = notification channel, `rsi` =
/// namespace handle (delegated by init), `rdx` = the **handoff channel** init moved in,
/// `rcx` unused.
///
/// `rdx` carried the fs-server endpoint directly until the profile server's endpoint
/// needed the same trip: only `handles[0]` reaches a child, so a second endpoint needs a
/// channel rather than a second register. `0` means init had nothing to hand over — a
/// service-mgr **restart**, since the endpoints moved to the first one and cannot move
/// twice. That is a degraded but running system (services supervised, no new logins), not
/// a reason to refuse to start.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, handoff: u64, _arg0: u64) -> ! {
    kprint(b"service-mgr: up\n");
    // The handoffs, in init's send order: the fs-server endpoint, then the profile
    // server's. Positional — see `bring_up_login_chain`.
    let (fs_endpoint, profile_endpoint, tty_endpoint, draw_endpoint) = if handoff == 0 {
        (0, 0, 0, 0)
    } else {
        let fs = recv_handoff(handoff);
        let profile = recv_handoff(handoff);
        let tty = recv_handoff(handoff);
        // The compositor's forwarding endpoint. Only the graphical column takes it: a serial
        // session has no use for `/dev/draw`, and handing it one would be authority for
        // nothing.
        let draw = recv_handoff(handoff);
        // SAFETY: closing our own handoff-channel end; every handoff is in hand.
        unsafe { syscall1(SYS_HANDLE_CLOSE, handoff) };
        (fs, profile, tty, draw)
    };
    // Bring up the login chain (auth-service + session-mgr) before the service demo.
    bring_up_login_chain(root_ns, fs_endpoint, profile_endpoint, tty_endpoint, draw_endpoint);
    supervise(notif, root_ns, load_declarations(root_ns));
}

/// One supervised service: its declaration, its child, and the state the restart
/// policy needs across exits.
struct Supervised {
    decl: ServiceDecl,
    /// The child's process handle, or `0` when it is not running.
    proc_h: i64,
    /// service-mgr's end of the child's control channel, or `0` when it is not running
    /// (or the channel could not be created). **This is the exit discriminator** — see
    /// [`supervise`].
    ctrl: u64,
    /// Restarts applied so far, against `decl.restart.max_attempts`.
    attempts: u32,
    running: bool,
    /// A supervisor-requested shutdown is intentional and is never restarted, whatever
    /// the policy says.
    requested_shutdown: bool,
}

/// Supervise every declared service: on a child's exit, apply *that child's* restart
/// policy + backoff, bounded by its `max_attempts`.
///
/// **How this knows which child exited, which is the whole design.** `KIND_CHILD_EXITED`
/// names the child by **pid**, and nothing in this system maps a process handle to a pid —
/// `sys_process_spawn` returns a handle, `HandleInfo` carries no pid, there is no pid
/// syscall, and `/proc` has no per-pid tree (`TODO(child-exit-attribution)`). A supervisor
/// with two children would learn *that* one exited and never *which*, which is why this
/// function held exactly one service until 2026-08-21.
///
/// The discriminator is the **control channel**, not the notification. Each service has
/// its own, service-mgr holds the other end, and when the child dies its end is destroyed:
/// the kernel nulls the survivor's peer pointer and *signals* it
/// (`sched::ipc_endpoint_closing` → `signal_ipc_endpoint`), which is the same wake path
/// `sys_wait` uses. So the control handles go in the wait set, `sys_wait` returns the
/// handle that signalled, and a `sys_channel_recv` on it answers `PeerClosed` (`-13`)
/// rather than `WouldBlock` (`-11`). A handle cannot be recycled under its holder the way
/// a pid can, so *which* endpoint closed is never ambiguous.
///
/// **Exact given one thing, which is a contract on the service rather than a property of
/// this code:** a declared service must hold its control endpoint until it exits. What
/// this observes is the endpoint closing; that is the child exiting only because nothing
/// else closes it. A service that closes it early is reported dead while it runs, and
/// under `policy = "always"` gets a *second live copy* — found exactly that way, in
/// `boot-probe` itself (PR #226 review, finding 1). The contract is written down in
/// `docs/spec/service-toml-schema.md`; there is no way to verify it from here.
///
/// **The exit *code* is still unattributed**, and that is the deliberate residual. It
/// arrives on `KIND_CHILD_EXITED` beside a pid this process cannot match, so codes are
/// collected **per wake** and paired with whichever service that wake found dead. A wake
/// is the right scope because a child's exit enqueues its notification and destroys its
/// endpoint under the same `SCHED` hold, so both reach one `sys_wait`. Codes left over at
/// the end of a wake are **discarded, and counted**: they belong to a child that is not
/// supervised here — `bring_up_login_chain` spawns `auth-service` and `session-mgr`, and
/// every child's exit reaches its parent's notification channel whether the parent
/// supervises it or not. Carrying them forward would mispair them with the next supervised
/// death (PR #226 review, finding 4).
///
/// Within one wake, two deaths can still swap their codes, which matters only to
/// `on-failure`; `never` and `always` do not read the code. A service found dead with no
/// code is treated as a **failure**, because a crash that outruns its notification is the
/// case worth restarting.
///
/// **The demo shutdown applies to the first declared service only.** It exercises the
/// control path end to end after `DEMO_RUN_NS`; a real shutdown trigger is still deferred.
fn supervise(notif: u64, root_ns: u64, decls: alloc::vec::Vec<ServiceDecl>) -> ! {
    // A reusable one-shot timer for backoff sleeps.
    let timer_h = {
        // SAFETY: a valid syscall; returns a handle (>= 0) or a negative KError.
        let t = unsafe { syscall1(SYS_TIMER_CREATE, 0) };
        if t < 0 {
            kprint(b"service-mgr: timer create FAIL (backoff disabled)\n");
            0
        } else {
            t as u64
        }
    };

    // Start every declaration, in file order — which *is* the start order, so "start B after
    // A" is written by putting A first. `after` is the stronger claim: A has already exited.
    let mut svcs: alloc::vec::Vec<Supervised> = alloc::vec::Vec::new();
    for decl in decls {
        await_dependencies(&decl, &svcs);
        let (proc_h, ctrl) = spawn_service(root_ns, &decl);
        // A service that could not be spawned is recorded as not running rather than
        // dropped: its declaration still describes what should exist, and the log line
        // `spawn_service` emitted is the record of why it does not.
        svcs.push(Supervised {
            decl,
            proc_h,
            ctrl,
            attempts: 0,
            running: proc_h > 0,
            requested_shutdown: false,
        });
    }
    if svcs.is_empty() {
        kprint(b"service-mgr: no services to start; idling\n");
        idle(notif);
    }
    // A service with no control channel cannot be attributed on exit — say so once, here,
    // rather than letting it look supervised. `spawn_service` already logged the failure.
    for s in &svcs {
        if s.running && s.ctrl == 0 {
            Line::new()
                .s(b"service-mgr: '")
                .s(s.decl.name.as_bytes())
                .s(b"' has no control channel -- its exit cannot be attributed")
                .end();
        }
    }
    {
        let mut l = Line::new();
        l.s(b"service-mgr: supervising ").u(svcs.len() as u64).s(b" service(s):");
        for s in &svcs {
            l.s(b" '").s(s.decl.name.as_bytes()).s(b"'");
        }
        l.end();
    }

    // Demo: schedule the graceful-shutdown request for the first declared service.
    let shutdown_at = now_ns().saturating_add(DEMO_RUN_NS);

    loop {
        // Build the wait set: the notification channel, then each running service's
        // control channel.
        //
        // **Which slot signalled is not read**, and does not need to be: step 2 below polls
        // every running service's channel, and `IpcChannel::already_signaled` is level- not
        // edge-triggered, so a close that happened between the poll and the next `sys_wait`
        // is still there to find. An earlier version kept a `slot_of` map from wait slot to
        // service index and never indexed it (PR #226 review, finding 7).
        let mut count = 1usize;
        // SAFETY: WAIT_HANDLES is a valid writable array of `1 + MAX_SERVICES`.
        unsafe { WAIT_HANDLES[0] = notif };
        for s in svcs.iter() {
            if s.running && s.ctrl != 0 && count < 1 + MAX_SERVICES {
                // SAFETY: `count` is bounded by the array length by the condition above.
                unsafe { WAIT_HANDLES[count] = s.ctrl };
                count += 1;
            }
        }
        // Wake at the demo shutdown while the first service is still running and has not
        // been asked to stop; otherwise sleep until something happens.
        let deadline = match svcs.first() {
            Some(s) if s.running && !s.requested_shutdown => shutdown_at,
            _ => u64::MAX,
        };
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers sized for
        // `1 + MAX_SERVICES`; `count` is within that.
        let waited = unsafe {
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                count as u64,
                (&raw mut WAIT_RESULTS) as u64,
                deadline,
            )
        };
        if waited < 1 {
            // Deadline reached: request the demo shutdown once.
            if let Some(s) = svcs.first_mut()
                && s.running
                && !s.requested_shutdown
            {
                Line::new()
                    .s(b"service-mgr: requesting graceful shutdown of '")
                    .s(s.decl.name.as_bytes())
                    .s(b"'")
                    .end();
                send_control(s.ctrl, CTRL_OP_SHUTDOWN);
                s.requested_shutdown = true;
            }
            continue;
        }

        // 1. Drain every queued notification, collecting exit codes. Which child each
        //    belongs to is unknowable here — see this function's doc comment. The vector is
        //    **per wake**: a code with no death to pair with by the end of this iteration
        //    belongs to a child service-mgr does not supervise.
        let mut codes: alloc::vec::Vec<i32> = alloc::vec::Vec::new();
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

        // 2. Ask each running service's control channel whether its peer is gone. This
        //    is the attribution: the handle that answers `PeerClosed` names the service.
        let mut dead: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
        for (i, s) in svcs.iter().enumerate() {
            if !s.running || s.ctrl == 0 {
                continue;
            }
            if channel_peer_closed(s.ctrl) {
                dead.push(i);
            }
        }

        for i in dead {
            let code = if codes.is_empty() { None } else { Some(codes.remove(0)) };
            // SAFETY: closing our own process + control handles (reaping).
            unsafe {
                if svcs[i].proc_h > 0 {
                    syscall1(SYS_HANDLE_CLOSE, svcs[i].proc_h as u64);
                }
                if svcs[i].ctrl != 0 {
                    syscall1(SYS_HANDLE_CLOSE, svcs[i].ctrl);
                }
            }
            svcs[i].proc_h = 0;
            svcs[i].ctrl = 0;
            svcs[i].running = false;

            let mut l = Line::new();
            l.s(b"service-mgr: '").s(svcs[i].decl.name.as_bytes()).s(b"' exited");
            match code {
                Some(c) => l.s(b" code=").i(c as i64),
                // Its notification has not arrived (or was consumed by a sibling that
                // exited in the same wake). Named rather than printed as a fake `0`.
                None => l.s(b" code=unknown"),
            };
            l.end();

            // A supervisor-requested shutdown is intentional — never restart it, even
            // under `policy = always`.
            if svcs[i].requested_shutdown {
                Line::new()
                    .s(b"service-mgr: '")
                    .s(svcs[i].decl.name.as_bytes())
                    .s(b"' stopped as requested (policy=")
                    .s(restart_name(svcs[i].decl.restart.policy))
                    .s(b" overridden -- not restarting)")
                    .end();
                continue;
            }

            // An unknown code is treated as a failure: a crash that outran its
            // notification is the case `on-failure` exists to restart.
            if !should_restart(svcs[i].decl.restart.policy, code.unwrap_or(-1)) {
                Line::new()
                    .s(b"service-mgr: '")
                    .s(svcs[i].decl.name.as_bytes())
                    .s(b"' stopped (policy=")
                    .s(restart_name(svcs[i].decl.restart.policy))
                    .s(b", not restarting)")
                    .end();
                continue;
            }
            if svcs[i].decl.restart.max_attempts != 0
                && svcs[i].attempts >= svcs[i].decl.restart.max_attempts
            {
                Line::new()
                    .s(b"service-mgr: '")
                    .s(svcs[i].decl.name.as_bytes())
                    .s(b"' gave up after ")
                    .u(svcs[i].attempts as u64)
                    .s(b" restart(s)")
                    .end();
                continue;
            }
            let backoff = compute_backoff(&svcs[i].decl.restart, svcs[i].attempts);
            // Assembled across the `if` rather than emitted in pieces — the whole point of
            // the helper is that a conditional fragment does not become its own line.
            let mut l = Line::new();
            l.s(b"service-mgr: restarting '")
                .s(svcs[i].decl.name.as_bytes())
                .s(b"' (attempt ")
                .u((svcs[i].attempts + 1) as u64);
            if svcs[i].decl.restart.max_attempts != 0 {
                l.s(b" of ").u(svcs[i].decl.restart.max_attempts as u64);
            }
            l.s(b") after ").u(backoff / 1_000_000).s(b"ms backoff").end();
            sleep_ns(timer_h, backoff);
            let (h, new_ctrl) = spawn_service(root_ns, &svcs[i].decl);
            if h > 0 {
                svcs[i].proc_h = h;
                svcs[i].ctrl = new_ctrl;
                svcs[i].running = true;
                svcs[i].attempts += 1;
            }
        }

        // Anything left belongs to a child this supervisor does not hold — `auth-service`
        // and `session-mgr` are spawned by `bring_up_login_chain` and reach this same
        // notification channel. Reported rather than dropped silently: on a release boot
        // either of those exiting is a system fault, and this is the only place that sees it.
        for code in codes {
            Line::new()
                .s(b"service-mgr: an unsupervised child exited code=")
                .i(code as i64)
                .end();
        }
    }
}

/// How long to wait for a service named in another's `after` to finish.
///
/// Bounded because the wait is for something that may never happen: `after` means "has
/// exited", and nothing stops a declaration from naming a service that keeps running. A hang
/// there would present as a boot that stops with no message, which is the worst failure this
/// supervisor can produce; timing out and saying so is strictly better.
const AFTER_TIMEOUT_NS: u64 = 20_000_000_000; // 20 s

/// Block until every service `decl` lists in `after` has exited, or the wait times out.
///
/// **What `after` means here.** The schema says a dependency must "reach ready state", and
/// for a service that exits — a one-shot — finishing *is* readiness. There is no readiness
/// protocol for a service that keeps running, so this waits for the control channel to close,
/// which is the same signal [`supervise`] attributes exits with.
///
/// **It orders backwards only.** A dependency is matched against the services already
/// started, so naming one declared *later* in the file cannot wait for it — the file's order
/// is the start order, and `after` strengthens it rather than reordering it.
///
/// Four things are **not** errors, and each is reported rather than fatal: a name that has not
/// started (not in this image, or declared later), a service running without a control channel
/// and so unwaitable, a dependency that already finished, and the timeout. The dependent still
/// starts — refusing to start it would turn a mis-typed name into a boot that silently lacks a
/// service.
fn await_dependencies(decl: &ServiceDecl, svcs: &[Supervised]) {
    for dep in &decl.after {
        // **Only services already started are candidates**, so `after` orders *backwards* in
        // file order. A name declared later in the file lands here, and the message must not
        // say "not declared" — it is, four lines down, and `parsed service '<name>'` for it is
        // earlier in the same transcript (PR #229 review, finding 5).
        let Some(i) = svcs.iter().position(|s| &s.decl.name == dep) else {
            Line::new()
                .s(b"service-mgr: '")
                .s(decl.name.as_bytes())
                .s(b"' waits on '")
                .s(dep.as_bytes())
                .s(b"', which has not started -- declared later in the file, or not at all")
                .end();
            continue;
        };
        if !svcs[i].running {
            continue; // already finished — nothing to wait for, and nothing to say
        }
        if svcs[i].ctrl == 0 {
            // **Running, but unwaitable.** The exit signal is the control channel closing, so
            // a service whose channel could not be created cannot be waited on — `boot-probe`
            // would start alongside a live `test-harness`, which is the race `after` exists to
            // prevent. Silent until 2026-08-24 (PR #229 review, finding 4); the doc above
            // promised it was reported, and it was not.
            Line::new()
                .s(b"service-mgr: '")
                .s(dep.as_bytes())
                .s(b"' has no control channel -- cannot wait for it; starting '")
                .s(decl.name.as_bytes())
                .s(b"' UNORDERED")
                .end();
            continue;
        }
        Line::new()
            .s(b"service-mgr: '")
            .s(decl.name.as_bytes())
            .s(b"' waits for '")
            .s(dep.as_bytes())
            .s(b"' to finish")
            .end();
        let deadline = now_ns().saturating_add(AFTER_TIMEOUT_NS);
        let mut timed_out = false;
        while !channel_peer_closed(svcs[i].ctrl) {
            if now_ns() >= deadline {
                timed_out = true;
                break;
            }
            // Wait on the dependency's control channel itself: it signals when the peer
            // closes, which is the exit. The deadline keeps a never-exiting dependency from
            // parking this supervisor forever.
            // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
            unsafe {
                WAIT_HANDLES[0] = svcs[i].ctrl;
                syscall4(
                    SYS_WAIT,
                    (&raw const WAIT_HANDLES) as u64,
                    1,
                    (&raw mut WAIT_RESULTS) as u64,
                    deadline,
                )
            };
        }
        if timed_out {
            Line::new()
                .s(b"service-mgr: '")
                .s(dep.as_bytes())
                .s(b"' did not finish within the wait -- starting '")
                .s(decl.name.as_bytes())
                .s(b"' anyway (does it ever exit?)")
                .end();
            continue;
        }
        // **Observed, not reaped.** Handling the exit here would strand its code — the
        // `KIND_CHILD_EXITED` notification is still queued, and `supervise` is what pairs
        // codes with deaths — and would skip the restart policy entirely. `PeerClosed` is
        // level-triggered, so `supervise`'s first pass sees the same closed channel, drains
        // the matching notification, and handles it exactly like any other exit.
        Line::new().s(b"service-mgr: '").s(dep.as_bytes()).s(b"' finished").end();
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

/// No supervised services: drain notifications forever (nothing to restart). The
/// slice-A fallback when the declaration is absent or unresolvable.
fn idle(notif: u64) -> ! {
    kprint(b"service-mgr: idle\n");
    loop {
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
        let waited = unsafe {
            WAIT_HANDLES[0] = notif;
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                1,
                (&raw mut WAIT_RESULTS) as u64,
                u64::MAX,
            )
        };
        if waited < 1 {
            continue;
        }
        // Drain (and discard) whatever woke us.
        loop {
            // SAFETY: NOTIF is a valid 64-byte writable out-param.
            let r = unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) };
            if r != 0 {
                break;
            }
        }
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
