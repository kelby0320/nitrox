//! `child` — the Phase-1 IPC handle-transfer demo worker.
//!
//! Spawned by `parent` with three bootstrap arguments (seeded by the kernel into
//! `rdi`/`rsi`/`rdx`, i.e. the three `extern "C"` parameters):
//!
//! - `notif`    — a handle to this process's own notification channel (unused);
//! - `endpoint` — one end of an IPC channel shared with the sibling child;
//! - `role`     — `0` = sender, `1` = receiver.
//!
//! Role 0 creates a `MemoryObject`, writes a marker into it, and **transfers the
//! handle** to the sibling over `endpoint` (capability propagation). Role 1
//! receives the handle, maps the same object, and reads the marker back —
//! proving the capability crossed the process boundary and aliases shared frames.

#![no_std]
#![no_main]

use core::arch::asm;
use libkern::{
    IpcMsg, RIGHT_MAP_READ, RIGHT_MAP_WRITE, SENDMODE_NOBLOCK, SYS_CHANNEL_RECV, SYS_CHANNEL_SEND,
    SYS_MEMORY_CREATE, SYS_MEMORY_MAP, SYS_NS_BIND, SYS_NS_LOOKUP, SYS_WAIT, exit, kprint, syscall2,
    syscall4, syscall5,
};

const PAGE: u64 = 4096;
/// The marker the sender writes into the transferred object; the receiver
/// verifies it after mapping.
const MARKER: u64 = 0x00C0_FFEE;

static mut SEND_MSG: IpcMsg = IpcMsg::ZEROED;
static mut RECV_MSG: IpcMsg = IpcMsg::ZEROED;
static mut RECV_COUNT: usize = 0;
/// `sys_channel_send`/`recv` transferred-handle arrays.
static mut SEND_HANDLES: [u64; 1] = [0];
static mut RECV_HANDLES: [u64; 8] = [0; 8];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
static mut WAIT_HANDLES: [u64; 1] = [0];

/// Sender (role 0): create a MemoryObject, mark it, transfer the handle.
fn run_sender(endpoint: u64) -> ! {
    // SAFETY: valid syscalls; returns a handle or a negative error.
    let mem_h = unsafe { syscall2(SYS_MEMORY_CREATE, PAGE, 0) };
    if mem_h < 0 {
        kprint(b"child[send]: memory create FAIL\n");
        exit(1);
    }
    let mem_h = mem_h as u64;
    // Map it read/write and write the marker.
    // SAFETY: valid syscall; returns the mapped address or a negative error.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem_h, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        kprint(b"child[send]: memory map FAIL\n");
        exit(1);
    }
    // SAFETY: `addr` is a page the kernel mapped R/W into our address space.
    unsafe { (addr as u64 as *mut u64).write_volatile(MARKER) };

    // Build a one-handle message and transfer the memory handle to the sibling.
    // SAFETY: SEND_MSG / SEND_HANDLES are valid writable .bss buffers.
    unsafe {
        SEND_MSG.header.payload_len = 0;
        SEND_HANDLES[0] = mem_h;
    }
    // SAFETY: valid endpoint + message + handles pointer; count 1, NoBlock.
    let sr = unsafe {
        syscall5(
            SYS_CHANNEL_SEND,
            endpoint,
            (&raw const SEND_MSG) as u64,
            (&raw const SEND_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        )
    };
    if sr == 0 {
        kprint(b"child[send]: transferred a memory object to the sibling\n");
        exit(0);
    } else {
        kprint(b"child[send]: send FAIL\n");
        exit(1);
    }
}

/// Receiver (role 1): receive the transferred handle, map it, verify the marker.
fn run_receiver(endpoint: u64) -> ! {
    // Block until the message arrives.
    // SAFETY: WAIT_HANDLES / WAIT_RESULTS are valid writable buffers.
    let waited = unsafe {
        WAIT_HANDLES[0] = endpoint;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
    };
    // SAFETY: valid out-params; on success the kernel installed the handle(s).
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            endpoint,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    // SAFETY: on success the kernel wrote the count + handle values.
    let (count, mem_h) = unsafe { ((&raw const RECV_COUNT).read(), (&raw const RECV_HANDLES[0]).read()) };
    if waited != 1 || rr != 0 || count != 1 {
        kprint(b"child[recv]: recv FAIL\n");
        exit(1);
    }

    // Map the transferred object and read the marker back.
    // SAFETY: `mem_h` is a memory handle just installed in our table.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem_h, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        kprint(b"child[recv]: map transferred object FAIL\n");
        exit(1);
    }
    // SAFETY: `addr` is the mapped, transferred page.
    let got = unsafe { (addr as u64 as *const u64).read_volatile() };
    if got == MARKER {
        kprint(b"child[recv]: mapped transferred object, marker=0xc0ffee ok\n");
        exit(0);
    } else {
        kprint(b"child[recv]: marker mismatch\n");
        exit(1);
    }
}

/// Exercise the **inherited namespace** (sandbox-by-construction): resolve a path
/// the parent bound into the child's namespace, and confirm the inherited handle
/// is LOOKUP-only by attempting a bind and expecting `NoAccess`. `ns` is the
/// child's root-namespace handle (`rsi`); `resource` is any handle to try binding.
fn ns_inheritance_check(ns: u64, resource: u64) {
    if ns == 0 {
        kprint(b"child: no namespace inherited\n");
        return;
    }
    let path = b"/store";
    // Look up "/store" (requesting MAP_READ); wait for the pre-signalled PO.
    // SAFETY: valid path pointer + handle.
    let po = unsafe {
        syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, RIGHT_MAP_READ)
    };
    if po >= 0 {
        // SAFETY: WAIT_HANDLES / WAIT_RESULTS are valid writable buffers.
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
        if waited == 1 && status == 0 {
            kprint(b"child: /store resolved in inherited namespace\n");
        } else {
            kprint(b"child: /store lookup in inherited namespace MISS\n");
        }
    } else {
        kprint(b"child: ns_lookup FAIL\n");
    }
    // The inherited handle is LOOKUP-only: a bind must fail NoAccess (-2).
    let foo = b"/foo";
    // SAFETY: valid path pointer + handle.
    let br = unsafe {
        syscall4(SYS_NS_BIND, ns, foo.as_ptr() as u64, foo.len() as u64, resource)
    };
    if br == -2 {
        kprint(b"child: bind into inherited namespace denied (LOOKUP-only)\n");
    } else {
        kprint(b"child: bind unexpectedly allowed/other error\n");
    }
}

/// Bootstrap registers (`kernel/src/syscall/table.rs`): `rdi` = notification
/// channel (unused here), `rsi` = inherited root namespace, `rdx` = the shared
/// channel endpoint, `rcx` = `role` (0 = sender, 1 = receiver).
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, ns: u64, endpoint: u64, role: u64) -> ! {
    ns_inheritance_check(ns, endpoint);
    if role == 0 {
        run_sender(endpoint);
    } else {
        run_receiver(endpoint);
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}
