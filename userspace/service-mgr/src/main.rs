//! `service-mgr` — the userspace service manager (Phase 3).
//!
//! Spawned by init once critical-path boot is stable, it starts, supervises, and
//! (as later parts land) restarts the system's services. See
//! `docs/architecture/service-manager.md`.
//!
//! **Slice A, Part C (this file):** service-mgr reads a service **declaration** from
//! the initramfs (`/initramfs/etc/services/heartbeat.toml`), parses it (see
//! `service_toml`), resolves its executable to a kernel-embedded image, and drives the
//! spawn from the declaration rather than a hard-coded `SpawnArgs`. Restart policy +
//! backoff (Part D) and per-service control channels (Part E) come next. The parser
//! lives in the crate library (`lib.rs`) so it is host-tested.
//!
//! `#![no_std]` + `#![no_main]`. Slice A uses `libkern` (raw syscalls) + `libheap`
//! (the `#[global_allocator]`, for the parser's owned strings); the design's
//! `librsproto`/`libos` surface arrives with the RS startup protocol in slice B.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;

use libkern::*;
use service_mgr::service_toml::{self, RestartPolicy};

/// The freeing userspace heap (slice 4), backing `alloc` for the declaration parser.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// One page; a service declaration is assumed to fit (true for the slice-A demo).
const PAGE: u64 = 4096;

static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
static mut NOTIF: Notification = Notification::zeroed();

/// Spawn args for the service being started. The `image` field is filled from the
/// parsed declaration before each spawn; a leaf service inherits a LOOKUP-only handle
/// to service-mgr's namespace and holds no ambient capabilities. Part D grows this to
/// carry the declaration's handle grants.
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

/// Resolve a declared `executable` path to a kernel-embedded `ImageId`. This is the
/// slice-A stand-in for a path-based ELF loader: known executables map to their
/// embedded image; anything else is unknown. Goes away when spawning from a real
/// filesystem path lands.
fn image_for_executable(exe: &str) -> Option<u32> {
    match exe {
        "/sbin/heartbeat" => Some(IMAGE_HEARTBEAT),
        _ => None,
    }
}

/// Resolve `path` in namespace `ns`, map the returned read-only `MemoryObject`, and
/// return its trimmed UTF-8 contents (the initramfs zero-fills the tail of the page).
/// Mirrors init's manifest read. `None` on any failure.
fn read_file(ns: u64, path: &[u8]) -> Option<String> {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, RIGHT_MAP_READ) };
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

/// Read + parse the slice-A service declaration and start the service it names.
/// Returns the started service's process handle, or a negative error.
fn start_services(root_ns: u64) -> i64 {
    let text = match read_file(root_ns, b"/initramfs/etc/services/heartbeat.toml") {
        Some(t) => t,
        None => {
            kprint(b"service-mgr: no service declarations found\n");
            return -1;
        }
    };
    let decl = match service_toml::parse(&text) {
        Some(d) => d,
        None => {
            kprint(b"service-mgr: declaration parse error\n");
            return -1;
        }
    };
    kprint(b"service-mgr: parsed service '");
    kprint(decl.name.as_bytes());
    kprint(b"' (executable=");
    kprint(decl.executable.as_bytes());
    kprint(b", restart=");
    kprint(restart_name(decl.restart));
    kprint(b")\n");

    let image = match image_for_executable(&decl.executable) {
        Some(i) => i,
        None => {
            kprint(b"service-mgr: unknown executable '");
            kprint(decl.executable.as_bytes());
            kprint(b"'\n");
            return -1;
        }
    };
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
    } else {
        kprint(b"service-mgr: started\n");
    }
    h
}

/// Bootstrap registers (see init's `_start`): `rdi` = notification channel, `rsi` =
/// namespace handle (delegated by init), `rdx`/`rcx` unused in slice A.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, _handle0: u64, _arg0: u64) -> ! {
    kprint(b"service-mgr: up\n");
    let service_h = start_services(root_ns);
    supervise(notif, service_h);
}

/// The supervision loop. Part C: wait for child exits and log each reap; then idle.
/// Part D grows this into restart-policy + backoff handling (the parsed policy is
/// threaded through here).
fn supervise(notif: u64, mut service_h: i64) -> ! {
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
                if service_h > 0 {
                    // SAFETY: closing our own process handle (reaping). Part D consults
                    // the restart policy here instead of just dropping it.
                    unsafe { syscall1(SYS_HANDLE_CLOSE, service_h as u64) };
                    service_h = 0;
                    kprint(b"service-mgr: service reaped (no restart policy yet)\n");
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
