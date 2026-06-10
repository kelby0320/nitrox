//! `child` — the Phase-1 process-spawn demo worker.
//!
//! Spawned by `parent` with three bootstrap arguments (seeded by the kernel
//! into `rdi`/`rsi`/`rdx`, i.e. the three `extern "C"` parameters):
//!
//! - `notif`    — a handle to this process's own notification channel (unused
//!                here; present for symmetry with real processes);
//! - `endpoint` — one end of an IPC channel shared with the sibling child;
//! - `role`     — `0` = sender, `1` = receiver.
//!
//! Role 0 sends a message over `endpoint`; role 1 blocks on `endpoint` and
//! receives it. Each exits with its role as the status code, which the parent
//! observes as a `ChildExited`.

#![no_std]
#![no_main]

use core::arch::asm;

const SYS_WAIT: u64 = 10;
const SYS_CHANNEL_SEND: u64 = 13;
const SYS_CHANNEL_RECV: u64 = 14;
const SYS_PROCESS_EXIT: u64 = 16;
const SYS_DEBUG_KPRINT: u64 = 0xFFFF_0000;

/// `SendMode::NoBlock` (`kernel/src/libkern/ipc.rs`).
const SENDMODE_NOBLOCK: u64 = 1;

const MSG: &[u8] = b"child: ping from the sender\n";

/// Userspace mirror of the kernel `IpcMsg` (`kernel/src/libkern/ipc.rs`): one
/// page; header fields flat, then a 4008-byte payload and an 8-entry handle
/// array.
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
static mut WAIT_RESULTS: [u8; 16] = [0; 16];
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

/// `endpoint` (in `rsi`) is one end of the shared channel; `role` (in `rdx`)
/// selects sender (0) or receiver (1). `notif` (in `rdi`) is unused here.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, endpoint: u64, role: u64) -> ! {
    if role == 0 {
        // Sender: put a payload in the message and send it (NoBlock).
        // SAFETY: SEND_MSG is a valid writable buffer.
        unsafe {
            SEND_MSG.payload_len = MSG.len() as u32;
            let mut i = 0;
            while i < MSG.len() {
                SEND_MSG.payload[i] = MSG[i];
                i += 1;
            }
        }
        // SAFETY: valid endpoint handle + message pointer; NoBlock, no handles.
        let sr = unsafe {
            syscall5(SYS_CHANNEL_SEND, endpoint, (&raw const SEND_MSG) as u64, 0, 0, SENDMODE_NOBLOCK)
        };
        if sr == 0 {
            kprint(b"child[send]: sent a message to the sibling\n");
        } else {
            kprint(b"child[send]: send FAIL\n");
        }
        exit(0);
    } else {
        // Receiver: block on the endpoint, then receive + report the payload.
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
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
        // SAFETY: valid out-params; on success the kernel wrote a message.
        let rr = unsafe {
            syscall4(SYS_CHANNEL_RECV, endpoint, (&raw mut RECV_MSG) as u64, 0, (&raw mut RECV_COUNT) as u64)
        };
        if waited == 1 && rr == 0 {
            kprint(b"child[recv]: got a message: ");
            // SAFETY: RECV_MSG.payload[0..payload_len] is initialised; bound the
            // print to the payload length (and the buffer).
            let slice = unsafe {
                let len = (&raw const RECV_MSG.payload_len).read() as usize;
                let p = (&raw const RECV_MSG.payload) as *const u8;
                let n = if len > 4008 { 4008 } else { len };
                core::slice::from_raw_parts(p, n)
            };
            kprint(slice);
        } else {
            kprint(b"child[recv]: recv FAIL\n");
        }
        exit(1);
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}
