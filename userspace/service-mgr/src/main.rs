//! `service-mgr` — the userspace service manager (Phase 3).
//!
//! Spawned by init once critical-path boot is stable, it starts, supervises, and
//! (as later parts land) restarts the system's services. See
//! `docs/architecture/service-manager.md`.
//!
//! **Slice A, Part B (this file):** the supervision spine. service-mgr spawns the
//! demo `heartbeat` service, waits for it to exit via the notification channel, and
//! logs the reap — proving the init → service-mgr → service spawn/reap chain end to
//! end. Declaration parsing (Part C), restart policy + backoff (Part D), and
//! per-service control channels come next.
//!
//! `#![no_std]` + `#![no_main]`. Slice A is `libkern`-only (raw syscalls, like init's
//! bootstrap); the design's `librsproto`/`libos` surface arrives with the Resource
//! Server Startup Protocol in slice B.

#![no_std]
#![no_main]

use libkern::*;

static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
static mut NOTIF: Notification = Notification::zeroed();

/// Spawn args for the demo `heartbeat` service: no handles, no ambient capabilities
/// (a leaf service), inheriting a LOOKUP-only handle to service-mgr's namespace. In
/// Part C these come from the parsed service declaration rather than a static.
static mut SPAWN_HEARTBEAT: SpawnArgs = SpawnArgs {
    image: IMAGE_HEARTBEAT,
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

/// Bootstrap registers (see init's `_start`): `rdi` = notification channel, `rsi` =
/// namespace handle (delegated by init), `rdx`/`rcx` unused in slice A.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, _root_ns: u64, _handle0: u64, _arg0: u64) -> ! {
    kprint(b"service-mgr: up\n");

    // Part B: a single hard-coded service. Spawn `heartbeat` and supervise it.
    kprint(b"service-mgr: starting service 'heartbeat'\n");
    // SAFETY: SPAWN_HEARTBEAT is a valid writable arg block.
    let hb = unsafe { syscall1(SYS_PROCESS_SPAWN, (&raw const SPAWN_HEARTBEAT) as u64) };
    if hb < 0 {
        kprint(b"service-mgr: heartbeat spawn FAIL\n");
    } else {
        kprint(b"service-mgr: heartbeat started\n");
    }

    supervise(notif, hb);
}

/// The supervision loop. Part B: wait for child exits and log each reap; then idle.
/// Part D grows this into restart-policy + backoff handling.
fn supervise(notif: u64, mut heartbeat_h: i64) -> ! {
    kprint(b"service-mgr: supervising\n");
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
            if kind == KIND_CHILD_EXITED {
                let cpid = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                let code = i32::from_le_bytes([body[8], body[9], body[10], body[11]]);
                kprint(b"service-mgr: service exited pid=");
                kprint_u64(cpid as u64);
                kprint(b" code=");
                kprint_u64(code as u64);
                kprint(b"\n");
                // Release service-mgr's reference to the exited child (reaping). Part D
                // consults the restart policy here instead of just dropping it.
                if heartbeat_h > 0 {
                    // SAFETY: closing our own process handle.
                    unsafe { syscall1(SYS_HANDLE_CLOSE, heartbeat_h as u64) };
                    heartbeat_h = 0;
                    kprint(b"service-mgr: 'heartbeat' reaped (no restart policy yet)\n");
                }
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
