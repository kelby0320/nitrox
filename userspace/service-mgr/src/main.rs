//! `service-mgr` — the userspace service manager (Phase 3).
//!
//! Spawned by init once critical-path boot is stable, it starts, supervises, and
//! restarts the system's services. See `docs/architecture/service-manager.md`.
//!
//! **Slice A (this file):** the supervision spine — parse a declaration from the
//! initramfs (`service_toml`, Part C), start the service, and on its exit apply the
//! restart policy + backoff (Part D). **Part E** adds a per-service **control
//! channel**: service-mgr keeps one end, moves the other to the service at spawn, and
//! can send lifecycle commands — here, a graceful `CTRL_OP_SHUTDOWN`. A supervisor-
//! requested shutdown is distinguished from an unexpected exit, so it is *not*
//! restarted even under `policy = always`.
//!
//! `#![no_std]` + `#![no_main]`. Slice A uses `libkern` (raw syscalls) + `libheap`
//! (the `#[global_allocator]`); the design's `librsproto`/`libos` surface arrives with
//! the RS startup protocol in slice B.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;

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

static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
static mut NOTIF: Notification = Notification::zeroed();
static mut CLOCK_BUF: u64 = 0;
static mut CTRL_OUT0: u64 = 0;
static mut CTRL_OUT1: u64 = 0;
static mut SEND_MSG: IpcMsg = IpcMsg::ZEROED;
static mut SEND_HANDLES: [u64; 8] = [0; 8];

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

/// Print a small unsigned decimal.
fn kprint_u64(mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if v == 0 {
        kprint(b"0");
        return;
    }
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    kprint(&buf[i..]);
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

/// Read + parse the slice-A service declaration. `None` (with a logged reason) if
/// absent or malformed. The executable is resolved to a `MemoryObject` at spawn time.
fn load_declaration(root_ns: u64) -> Option<ServiceDecl> {
    let text = match read_file(root_ns, b"/initramfs/etc/services/heartbeat.toml") {
        Some(t) => t,
        None => {
            kprint(b"service-mgr: no service declarations found\n");
            return None;
        }
    };
    let decl = match service_toml::parse(&text) {
        Some(d) => d,
        None => {
            kprint(b"service-mgr: declaration parse error\n");
            return None;
        }
    };
    kprint(b"service-mgr: parsed service '");
    kprint(decl.name.as_bytes());
    kprint(b"' (executable=");
    kprint(decl.executable.as_bytes());
    kprint(b", restart=");
    kprint(restart_name(decl.restart.policy));
    kprint(b", max_attempts=");
    kprint_u64(decl.restart.max_attempts as u64);
    kprint(b")\n");
    Some(decl)
}

/// Spawn the service `decl` names (image already resolved), with a fresh control
/// channel whose service end is moved to the child. Returns `(proc_handle,
/// control_end)`; `control_end` is `0` if the channel couldn't be created.
fn spawn_service(root_ns: u64, decl: &ServiceDecl) -> (i64, u64) {
    // Resolve the declared executable to its ELF `MemoryObject` (path-based spawn).
    let image = ns_lookup(root_ns, decl.executable.as_bytes(), RIGHT_MAP_READ);
    if image == 0 {
        kprint(b"service-mgr: image not found: ");
        kprint(decl.executable.as_bytes());
        kprint(b"\n");
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
    kprint(b"service-mgr: starting service '");
    kprint(decl.name.as_bytes());
    kprint(b"'\n");
    // SAFETY: SPAWN_SERVICE is a valid writable arg block. Moved handles, in child
    // register order: `handles[0]` = control endpoint (RECV + WAIT — it receives
    // commands) at `rdx`; `handles[1]` = log endpoint (SEND) at `rcx`. The log slot is
    // only used when the control slot is present, so positions stay fixed.
    let h = unsafe {
        SPAWN_SERVICE.image = image;
        if svc_end != 0 {
            SPAWN_SERVICE.handles[0] = svc_end;
            SPAWN_SERVICE.rights[0] = RIGHT_RECV | RIGHT_WAIT;
            if log_ep != 0 {
                SPAWN_SERVICE.handles[1] = log_ep;
                SPAWN_SERVICE.rights[1] = RIGHT_SEND;
                SPAWN_SERVICE.handle_count = 2;
                SPAWN_SERVICE.move_mask = 0b11;
            } else {
                SPAWN_SERVICE.handle_count = 1;
                SPAWN_SERVICE.move_mask = 0b1;
            }
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
        // The service + log endpoints were not moved (spawn failed) — close them.
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
    // `svc_end` has moved to the child; retain `smgr_end` as the control endpoint.
    (h, smgr_end)
}

/// Bootstrap registers (see init's `_start`): `rdi` = notification channel, `rsi` =
/// namespace handle (delegated by init), `rdx`/`rcx` unused in slice A.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, _handle0: u64, _arg0: u64) -> ! {
    kprint(b"service-mgr: up\n");
    match load_declaration(root_ns) {
        Some(decl) => {
            let (h, ctrl) = spawn_service(root_ns, &decl);
            supervise(notif, root_ns, decl, h, ctrl);
        }
        None => {
            kprint(b"service-mgr: no services to start; idling\n");
            idle(notif);
        }
    }
}

/// Supervise the single slice-A service: on its exit, apply the restart policy +
/// backoff (Part D), bounded by `max_attempts`. **Part E:** after `DEMO_RUN_NS`,
/// request a graceful shutdown over the control channel; a requested shutdown is not
/// restarted, even under `policy = always`. (A later slice generalises this to a table
/// of services and a real shutdown trigger.)
fn supervise(notif: u64, root_ns: u64, decl: ServiceDecl, mut service_h: i64, mut ctrl: u64) -> ! {
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
    let mut attempts: u32 = 0;
    let mut running = service_h > 0;
    let mut requested_shutdown = false;
    // Demo: schedule the graceful-shutdown request.
    let shutdown_at = now_ns().saturating_add(DEMO_RUN_NS);
    kprint(b"service-mgr: supervising '");
    kprint(decl.name.as_bytes());
    kprint(b"'\n");

    loop {
        // Wait on the notification channel; while the service runs and no shutdown has
        // been requested, wake at `shutdown_at` to send it.
        let deadline = if running && !requested_shutdown {
            shutdown_at
        } else {
            u64::MAX
        };
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
        let waited = unsafe {
            WAIT_HANDLES[0] = notif;
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                1,
                (&raw mut WAIT_RESULTS) as u64,
                deadline,
            )
        };
        if waited < 1 {
            // Deadline reached with the service still running: request shutdown once.
            if running && !requested_shutdown {
                kprint(b"service-mgr: requesting graceful shutdown of '");
                kprint(decl.name.as_bytes());
                kprint(b"'\n");
                send_control(ctrl, CTRL_OP_SHUTDOWN);
                requested_shutdown = true;
            }
            continue;
        }
        // Drain every queued notification this wake delivered.
        loop {
            // SAFETY: NOTIF is a valid 64-byte writable out-param.
            let r = unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) };
            if r != 0 {
                break; // WouldBlock: drained
            }
            // SAFETY: the kernel wrote a 64-byte Notification into NOTIF.
            let (kind, body) =
                unsafe { ((&raw const NOTIF.kind).read(), (&raw const NOTIF.body).read()) };
            if kind != KIND_CHILD_EXITED || !running {
                continue;
            }
            let cpid = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            let code = i32::from_le_bytes([body[8], body[9], body[10], body[11]]);
            // SAFETY: closing our own process + control handles (reaping).
            unsafe {
                syscall1(SYS_HANDLE_CLOSE, service_h as u64);
                if ctrl != 0 {
                    syscall1(SYS_HANDLE_CLOSE, ctrl);
                }
            }
            service_h = 0;
            ctrl = 0;
            running = false;
            kprint(b"service-mgr: '");
            kprint(decl.name.as_bytes());
            kprint(b"' exited pid=");
            kprint_u64(cpid as u64);
            kprint(b" code=");
            kprint_u64(code as u64);
            kprint(b"\n");

            // A supervisor-requested shutdown is intentional — never restart it, even
            // under `policy = always`.
            if requested_shutdown {
                kprint(b"service-mgr: '");
                kprint(decl.name.as_bytes());
                kprint(b"' stopped as requested (policy=");
                kprint(restart_name(decl.restart.policy));
                kprint(b" overridden -- not restarting)\n");
                continue;
            }

            // Unexpected exit — apply the restart policy + backoff (Part D).
            if !should_restart(decl.restart.policy, code) {
                kprint(b"service-mgr: '");
                kprint(decl.name.as_bytes());
                kprint(b"' stopped (policy=");
                kprint(restart_name(decl.restart.policy));
                kprint(b", not restarting)\n");
                continue;
            }
            if decl.restart.max_attempts != 0 && attempts >= decl.restart.max_attempts {
                kprint(b"service-mgr: '");
                kprint(decl.name.as_bytes());
                kprint(b"' gave up after ");
                kprint_u64(attempts as u64);
                kprint(b" restart(s)\n");
                continue;
            }
            let backoff = compute_backoff(&decl.restart, attempts);
            kprint(b"service-mgr: restarting '");
            kprint(decl.name.as_bytes());
            kprint(b"' (attempt ");
            kprint_u64((attempts + 1) as u64);
            if decl.restart.max_attempts != 0 {
                kprint(b" of ");
                kprint_u64(decl.restart.max_attempts as u64);
            }
            kprint(b") after ");
            kprint_u64(backoff / 1_000_000);
            kprint(b"ms backoff\n");
            sleep_ns(timer_h, backoff);
            let (h, new_ctrl) = spawn_service(root_ns, &decl);
            if h > 0 {
                service_h = h;
                ctrl = new_ctrl;
                running = true;
                attempts += 1;
            }
        }
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
