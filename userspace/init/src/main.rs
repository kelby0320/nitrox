//! `init` — PID 1 (bootstrapping form, Phase 2 slice 4 Part 5).
//!
//! The kernel loads init as the first userspace process (`run_first_userspace`),
//! handing it a notification channel (`rdi`) and a full-rights root namespace
//! (`rsi`) carrying the boot kernel-server bindings (`/initramfs`, `/dev/entropy`,
//! `/proc/self/*`). init:
//!
//! 1. reports the handle set it received;
//! 2. reads + parses `/initramfs/etc/init.toml` and logs the topo-sorted mount
//!    plan (the actual fs-server spawn → Ready handshake → `sys_ns_bind` is
//!    deferred to slice 7 — there are no fs-servers or block devices yet);
//! 3. spawns `parent` (the slice-1/2/3 demo chain: `parent` → `child`);
//! 4. enters the reaping loop, closing the process handle of each exited child.
//!
//! Per `userspace/init/CLAUDE.md`, init uses `libkern` + `alloc` only and never
//! `panic!`s in normal operation.

#![no_std]
#![no_main]

extern crate alloc;

use core::arch::asm;
use init::heap::BumpAlloc;
use init::manifest::{self, Mode};
use libkern::*;

#[global_allocator]
static ALLOC: BumpAlloc = BumpAlloc;

/// One page; init.toml is assumed to fit (true for the bootstrapping manifest).
const PAGE: u64 = 4096;

static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
static mut NOTIF: Notification = Notification::zeroed();
/// Spawn args for the demo `parent`: no handles, inherit a LOOKUP-only handle to
/// init's root namespace (so parent can resolve the kernel servers but not bind
/// into init's root — it constructs its own namespaces for its children).
static mut SPAWN_PARENT: SpawnArgs = SpawnArgs {
    image: IMAGE_PARENT,
    handle_count: 0,
    move_mask: 0,
    _pad: 0,
    arg0: 0,
    handles: [0; 4],
    rights: [0; 4],
    namespace: 0,
};

/// Resolve `path` in namespace `ns` requesting `rights`, wait the PO, and return
/// `(status, resolved_handle)` (`IoResult`: status at bytes 8..12, handle 16..24).
fn ns_lookup_wait(ns: u64, path: &[u8], rights: u64) -> (i32, u64) {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe {
        syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights)
    };
    if po < 0 {
        return (po as i32, 0);
    }
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
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
    let status = unsafe {
        i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]])
    };
    let resolved = unsafe {
        u64::from_le_bytes([
            WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
            WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
        ])
    };
    // SAFETY: closing our own PO handle (the resolved handle is separate).
    unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
    if waited != 1 {
        return (-1, 0);
    }
    (status, resolved)
}

/// Read + parse `/initramfs/etc/init.toml` and log the topo-sorted mount plan.
/// Actual mounting (spawn fs-server → Ready → bind) is deferred to slice 7.
fn read_manifest(root_ns: u64) {
    let (st, mem) = ns_lookup_wait(root_ns, b"/initramfs/etc/init.toml", RIGHT_MAP_READ);
    if st != 0 || mem == 0 {
        kprint(b"init: /initramfs/etc/init.toml not found (would drop to eshell)\n");
        return;
    }
    // Map the read-only MemoryObject the initramfs server handed back. init.toml
    // is text and fits in one page; the server zero-fills the tail, so we trim
    // trailing NULs to recover the exact file content.
    // SAFETY: `mem` is a MemoryObject handle with MAP_READ.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, PAGE, RIGHT_MAP_READ) };
    if addr < 0 {
        kprint(b"init: init.toml map FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
        return;
    }
    // SAFETY: `addr` is a MAP_READ page holding the file bytes + zero padding.
    let bytes = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, PAGE as usize) };
    let len = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    match core::str::from_utf8(&bytes[..len]) {
        Ok(text) => match manifest::parse(text) {
            Ok(mounts) => {
                kprint(b"init: init.toml OK, ");
                kprint_u64(mounts.len() as u64);
                kprint(b" mount(s) (shallowest first):\n");
                for m in &mounts {
                    kprint(b"init:   ");
                    kprint(m.mount_point.as_bytes());
                    kprint(b": ");
                    kprint(m.fs_server.as_bytes());
                    kprint(b" on ");
                    kprint(m.device.as_bytes());
                    kprint(b" (");
                    kprint(match m.mode {
                        Mode::Ro => b"ro" as &[u8],
                        Mode::Rw => b"rw",
                    });
                    kprint(b") -- spawn/Ready/bind deferred to slice 7\n");
                }
            }
            Err(_) => kprint(b"init: init.toml parse error (would drop to eshell)\n"),
        },
        Err(_) => kprint(b"init: init.toml not UTF-8 (would drop to eshell)\n"),
    }
    // SAFETY: closing our own handle; the mapping kept the object alive, and the
    // parsed mounts own their strings, so the mapped bytes are no longer needed.
    unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
}

/// Spawn the demo `parent`, then reap exited children forever. As PID 1, init is
/// the eventual parent of every orphan; here its only child is `parent`.
fn supervise(notif: u64) -> ! {
    kprint(b"init: spawning parent (slice-1/2/3 demo chain)\n");
    // SAFETY: SPAWN_PARENT is a valid writable arg block.
    let mut parent_h = unsafe { syscall1(SYS_PROCESS_SPAWN, (&raw const SPAWN_PARENT) as u64) };
    if parent_h < 0 {
        kprint(b"init: parent spawn FAIL\n");
        parent_h = 0;
    }

    kprint(b"init: entering reaping loop\n");
    loop {
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
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
                kprint(b"init: reaped pid=");
                kprint_u64(cpid as u64);
                kprint(b" code=");
                kprint_u64(code as u64);
                kprint(b"\n");
                // Release init's reference to the exited child (its only child is
                // `parent`). Reparented orphans have no handle here — the kernel
                // tears them down; init just observes their exit.
                if parent_h != 0 {
                    // SAFETY: closing our own process handle.
                    unsafe { syscall1(SYS_HANDLE_CLOSE, parent_h as u64) };
                    parent_h = 0;
                }
            }
        }
    }
}

/// Bootstrap registers: `rdi` = notification channel, `rsi` = root namespace
/// (full-rights, kernel-bound servers), `rdx`/`rcx` unused (init takes no
/// installed handles or arg0 from the kernel).
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, _handle0: u64, _arg0: u64) -> ! {
    kprint(b"init: up (pid 1)\n");
    let count = (notif != 0) as u64 + (root_ns != 0) as u64;
    kprint(b"init: received ");
    kprint_u64(count);
    kprint(b" handles (notif=");
    kprint_u64(notif);
    kprint(b", ns=");
    kprint_u64(root_ns);
    kprint(b")\n");

    read_manifest(root_ns);
    supervise(notif);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // init must not panic in normal operation (`userspace/init/CLAUDE.md`); this
    // is the last-ditch handler. Report and spin (no eshell handoff yet — slice 9+).
    kprint(b"init: PANIC\n");
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}
