//! `service-mgr` — the userspace service manager (Phase 3).
//!
//! Spawned by init once critical-path boot is stable, it starts, supervises, and
//! restarts the system's services. See `docs/architecture/service-manager.md`.
//!
//! **Slice A, Part D (this file):** supervision with **restart policy + backoff**. On
//! a supervised service's exit, service-mgr consults the parsed policy
//! (`never`/`on-failure`/`always`), and — if a restart is due and `max_attempts` is
//! not exhausted — waits the backoff (a one-shot monotonic timer) and respawns.
//! Declaration parsing is Part C (`service_toml`); per-service control channels are
//! Part E. The parser lives in the crate library (`lib.rs`) so it is host-tested.
//!
//! `#![no_std]` + `#![no_main]`. Slice A uses `libkern` (raw syscalls) + `libheap`
//! (the `#[global_allocator]`); the design's `librsproto`/`libos` surface arrives with
//! the RS startup protocol in slice B.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;

use libkern::*;
use service_mgr::service_toml::{self, Backoff, RestartConfig, RestartPolicy, ServiceDecl};

/// The freeing userspace heap (slice 4), backing `alloc` for the declaration parser.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// One page; a service declaration is assumed to fit (true for the slice-A demo).
const PAGE: u64 = 4096;

static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
static mut NOTIF: Notification = Notification::zeroed();
static mut CLOCK_BUF: u64 = 0;

/// Spawn args for the service being started/restarted. The `image` field is filled
/// from the parsed declaration before each spawn; a leaf service inherits a
/// LOOKUP-only handle to service-mgr's namespace and holds no ambient capabilities.
static mut SPAWN_SERVICE: SpawnArgs = SpawnArgs {
    image: 0,
    handle_count: 0,
    move_mask: 0,
    _pad: 0,
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

/// Resolve a declared `executable` path to a kernel-embedded `ImageId`. The slice-A
/// stand-in for a path-based ELF loader: known executables map to their embedded
/// image. Goes away when spawning from a real filesystem path lands.
fn image_for_executable(exe: &str) -> Option<u32> {
    match exe {
        "/sbin/heartbeat" => Some(IMAGE_HEARTBEAT),
        _ => None,
    }
}

/// The backoff wait (ns) for the `attempts`-th restart (0-based) under `cfg`.
fn compute_backoff(cfg: &RestartConfig, attempts: u32) -> u64 {
    match cfg.backoff {
        Backoff::None => 0,
        Backoff::Linear => cfg.initial_ns,
        // initial << attempts, saturating, capped at max.
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

/// Block for `duration_ns` on a one-shot monotonic timer (`timer_h`, reused across
/// backoffs). Best-effort: a create/arm/wait failure returns promptly.
fn sleep_ns(timer_h: u64, duration_ns: u64) {
    if timer_h == 0 || duration_ns == 0 {
        return;
    }
    // SAFETY: CLOCK_BUF is a valid writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: on success the kernel wrote the ns count into CLOCK_BUF.
    let now = unsafe { (&raw const CLOCK_BUF).read() };
    let fire_at = now.saturating_add(duration_ns);
    // SAFETY: arming our own timer (absolute monotonic deadline, one-shot).
    unsafe { syscall4(SYS_TIMER_SET, timer_h, fire_at, 0, 0) };
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter. A
    // generous overall deadline past `fire_at` ensures the timer, not the wait
    // deadline, wakes us.
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

/// Resolve `path` in namespace `ns`, map the returned read-only `MemoryObject`, and
/// return its trimmed UTF-8 contents (the initramfs zero-fills the tail of the page).
/// Mirrors init's manifest read. `None` on any failure.
fn read_file(ns: u64, path: &[u8]) -> Option<String> {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe {
        syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, RIGHT_MAP_READ)
    };
    if po < 0 {
        return None;
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
    let (status, mem) = unsafe {
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
    if waited != 1 || status != 0 || mem == 0 {
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
    // Copied into an owned String; release service-mgr's handle to the object (the
    // page mapping persists via its own reference — slice A does not unmap).
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
    text
}

/// Read + parse the slice-A service declaration and resolve its executable to an
/// embedded image. `None` (with a logged reason) if it is absent/malformed/unknown.
fn load_declaration(root_ns: u64) -> Option<(ServiceDecl, u32)> {
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

    match image_for_executable(&decl.executable) {
        Some(image) => Some((decl, image)),
        None => {
            kprint(b"service-mgr: unknown executable '");
            kprint(decl.executable.as_bytes());
            kprint(b"'\n");
            None
        }
    }
}

/// Spawn the service `decl` names (image already resolved). Returns the process
/// handle, or a negative error.
fn spawn_service(decl: &ServiceDecl, image: u32) -> i64 {
    kprint(b"service-mgr: starting service '");
    kprint(decl.name.as_bytes());
    kprint(b"'\n");
    // SAFETY: SPAWN_SERVICE is a valid writable arg block; set the resolved image.
    let h = unsafe {
        SPAWN_SERVICE.image = image;
        syscall1(SYS_PROCESS_SPAWN, (&raw const SPAWN_SERVICE) as u64)
    };
    if h < 0 {
        kprint(b"service-mgr: spawn FAIL\n");
    }
    h
}

/// Bootstrap registers (see init's `_start`): `rdi` = notification channel, `rsi` =
/// namespace handle (delegated by init), `rdx`/`rcx` unused in slice A.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, _handle0: u64, _arg0: u64) -> ! {
    kprint(b"service-mgr: up\n");
    match load_declaration(root_ns) {
        Some((decl, image)) => {
            let h = spawn_service(&decl, image);
            supervise(notif, decl, image, h);
        }
        None => {
            kprint(b"service-mgr: no services to start; idling\n");
            idle(notif);
        }
    }
}

/// Supervise the single slice-A service: on its exit, apply the restart policy +
/// backoff, bounded by `max_attempts`. (Part E adds the control channel; a later
/// slice generalises this to a table of services.)
fn supervise(notif: u64, decl: ServiceDecl, image: u32, mut service_h: i64) -> ! {
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
    kprint(b"service-mgr: supervising '");
    kprint(decl.name.as_bytes());
    kprint(b"'\n");

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
            // SAFETY: closing our own process handle (reaping).
            unsafe { syscall1(SYS_HANDLE_CLOSE, service_h as u64) };
            service_h = 0;
            running = false;
            kprint(b"service-mgr: '");
            kprint(decl.name.as_bytes());
            kprint(b"' exited pid=");
            kprint_u64(cpid as u64);
            kprint(b" code=");
            kprint_u64(code as u64);
            kprint(b"\n");

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
            let h = spawn_service(&decl, image);
            if h > 0 {
                service_h = h;
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
