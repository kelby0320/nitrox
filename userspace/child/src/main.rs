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

const SYS_MEMORY_CREATE: u64 = 4;
const SYS_MEMORY_MAP: u64 = 5;
const SYS_WAIT: u64 = 10;
const SYS_CHANNEL_SEND: u64 = 13;
const SYS_CHANNEL_RECV: u64 = 14;
const SYS_PROCESS_EXIT: u64 = 16;
const SYS_DEBUG_KPRINT: u64 = 0xFFFF_0000;

/// `SendMode::NoBlock` (`kernel/src/libkern/ipc.rs`).
const SENDMODE_NOBLOCK: u64 = 1;
/// Rights bits (`kernel/src/libkern/handle.rs`).
const RIGHT_MAP_READ: u64 = 1 << 15;
const RIGHT_MAP_WRITE: u64 = 1 << 16;

const PAGE: u64 = 4096;
/// The marker the sender writes into the transferred object; the receiver
/// verifies it after mapping.
const MARKER: u64 = 0x00C0_FFEE;

/// Userspace mirror of the kernel `IpcMsg` (`kernel/src/libkern/ipc.rs`): one
/// page; header fields flat, then a 4008-byte payload and an 8-entry handle array.
#[repr(C, align(4096))]
struct IpcMsgBuf {
    sender_pid: u32,
    payload_len: u32,
    handle_count: u8,
    _pad1: u8,
    flags: u16,
    _pad2: [u8; 4],
    timestamp: u64,
    payload: [u8; 4008],
    handles: [u64; 8],
}

impl IpcMsgBuf {
    const ZEROED: IpcMsgBuf = IpcMsgBuf {
        sender_pid: 0,
        payload_len: 0,
        handle_count: 0,
        _pad1: 0,
        flags: 0,
        _pad2: [0; 4],
        timestamp: 0,
        payload: [0; 4008],
        handles: [0; 8],
    };
}

static mut SEND_MSG: IpcMsgBuf = IpcMsgBuf::ZEROED;
static mut RECV_MSG: IpcMsgBuf = IpcMsgBuf::ZEROED;
static mut RECV_COUNT: usize = 0;
/// `sys_channel_send`/`recv` transferred-handle arrays.
static mut SEND_HANDLES: [u64; 1] = [0];
static mut RECV_HANDLES: [u64; 8] = [0; 8];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
static mut WAIT_HANDLES: [u64; 1] = [0];

#[inline]
unsafe fn syscall4(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret;
    // SAFETY: register-only syscall; clobbers only the documented scratch regs.
    unsafe {
        asm!(
            "syscall",
            in("rax") nr, in("rdi") a0, in("rsi") a1, in("rdx") a2, in("r10") a3,
            out("rcx") _, out("r11") _, lateout("rax") ret,
        );
    }
    ret
}

#[inline]
unsafe fn syscall2(nr: u64, a0: u64, a1: u64) -> i64 {
    // SAFETY: see `syscall4`.
    unsafe { syscall4(nr, a0, a1, 0, 0) }
}

#[inline]
unsafe fn syscall5(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> i64 {
    let ret;
    // SAFETY: as `syscall4`, plus `r8` for the 5th argument.
    unsafe {
        asm!(
            "syscall",
            in("rax") nr, in("rdi") a0, in("rsi") a1, in("rdx") a2, in("r10") a3, in("r8") a4,
            out("rcx") _, out("r11") _, lateout("rax") ret,
        );
    }
    ret
}

fn kprint(msg: &[u8]) {
    // SAFETY: passes a valid (ptr, len) the kernel copies from.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

fn exit(status: i64) -> ! {
    // SAFETY: process exit diverges in the kernel; control never returns.
    unsafe {
        asm!("syscall", in("rax") SYS_PROCESS_EXIT, in("rdi") status, options(noreturn, nostack));
    }
}

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
        SEND_MSG.payload_len = 0;
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

/// `endpoint` (in `rsi`) is one end of the shared channel; `role` (in `rdx`)
/// selects sender (0) or receiver (1). `notif` (in `rdi`) is unused here.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, endpoint: u64, role: u64) -> ! {
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
