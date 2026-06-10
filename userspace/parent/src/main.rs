//! `parent` — the Phase-1 process-spawn demo supervisor.
//!
//! Booted by the kernel as PID 1 with one bootstrap argument: a handle to its
//! own notification channel (in `rdi`, the first `extern "C"` parameter). It:
//!
//! 1. creates an IPC channel (`sys_channel_create`) → two endpoints;
//! 2. spawns two `child` processes (`sys_process_spawn`), **moving** one
//!    endpoint into each — so the children share a channel they can talk over;
//! 3. blocks on its notification channel (`sys_wait`) and drains two
//!    `ChildExited` notifications (`sys_notif_recv`), reporting each;
//! 4. exits.
//!
//! This is the Phase-1 milestone proof: two userspace processes communicating
//! over IPC, both spawned by a parent that learns of their exits. (A real
//! `init` with an initramfs and a service manager is Phase 2.)

#![no_std]
#![no_main]

use core::arch::asm;

// --- Syscall numbers (must match `kernel/src/syscall/table.rs`) ----------
const SYS_HANDLE_CLOSE: u64 = 0;
const SYS_WAIT: u64 = 10;
const SYS_NOTIF_RECV: u64 = 11;
const SYS_CHANNEL_CREATE: u64 = 12;
const SYS_PROCESS_SPAWN: u64 = 15;
const SYS_PROCESS_EXIT: u64 = 16;
const SYS_DEBUG_KPRINT: u64 = 0xFFFF_0000;

/// Rights bits (`kernel/src/libkern/handle.rs`): the full set an endpoint
/// carries, handed to each child.
const RIGHT_DUPLICATE: u64 = 1 << 0;
const RIGHT_TRANSFER: u64 = 1 << 1;
const RIGHT_INSPECT: u64 = 1 << 2;
const RIGHT_WAIT: u64 = 1 << 3;
const RIGHT_SEND: u64 = 1 << 18;
const RIGHT_RECV: u64 = 1 << 19;
const ENDPOINT_RIGHTS: u64 =
    RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT | RIGHT_DUPLICATE | RIGHT_INSPECT | RIGHT_TRANSFER;

/// `ImageId::Child` (`kernel/src/libkern/spawn.rs`).
const IMAGE_CHILD: u32 = 0;
/// `Notification::ChildExited` discriminant (`kernel/src/libkern/notification.rs`).
const KIND_CHILD_EXITED: u32 = 0x0200;

/// Userspace mirror of the kernel `SpawnArgs` (`kernel/src/libkern/spawn.rs`).
#[repr(C)]
struct SpawnArgs {
    image: u32,
    handle_count: u32,
    move_mask: u32,
    _pad: u32,
    arg0: u64,
    handles: [u64; 4],
    rights: [u64; 4],
}

/// Mirror of the kernel `Notification` (`kernel/src/libkern/notification.rs`):
/// a 64-byte record, `u32` kind + 60-byte body.
#[repr(C, align(8))]
struct NotificationBuf {
    kind: u32,
    body: [u8; 60],
}

static mut END0: u64 = 0;
static mut END1: u64 = 0;
static mut SPAWN_A: SpawnArgs = SpawnArgs {
    image: IMAGE_CHILD,
    handle_count: 1,
    move_mask: 1, // move handle 0 to the child
    _pad: 0,
    arg0: 0, // role 0 = sender
    handles: [0; 4],
    rights: [ENDPOINT_RIGHTS, 0, 0, 0],
};
static mut SPAWN_B: SpawnArgs = SpawnArgs {
    image: IMAGE_CHILD,
    handle_count: 1,
    move_mask: 1,
    _pad: 0,
    arg0: 1, // role 1 = receiver
    handles: [0; 4],
    rights: [ENDPOINT_RIGHTS, 0, 0, 0],
};
static mut NOTIF: NotificationBuf = NotificationBuf { kind: 0, body: [0; 60] };
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
unsafe fn syscall1(nr: u64, a0: u64) -> i64 {
    // SAFETY: see `syscall4`.
    unsafe { syscall4(nr, a0, 0, 0, 0) }
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

/// Print a small unsigned decimal (for pids/codes), no allocation.
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

/// `notif` (in `rdi`) is this process's notification-channel handle, seeded by
/// the kernel at spawn. The other two bootstrap registers are unused here.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, _boot1: u64, _boot2: u64) -> ! {
    kprint(b"parent: up (pid 1), creating a channel\n");

    // 1. Create an IPC channel; depth 4.
    // SAFETY: END0/END1 are valid writable out-params.
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut END0) as u64, (&raw mut END1) as u64, 4, 0)
    };
    if cr != 0 {
        kprint(b"parent: channel create FAIL\n");
        exit(1);
    }
    // SAFETY: the kernel wrote both endpoint handles.
    let (e0, e1) = unsafe { ((&raw const END0).read(), (&raw const END1).read()) };

    // 2. Spawn two children, moving one endpoint into each.
    // SAFETY: SPAWN_A/SPAWN_B are valid writable arg blocks.
    unsafe {
        SPAWN_A.handles[0] = e0;
        SPAWN_B.handles[0] = e1;
    }
    // SAFETY: valid SpawnArgs pointer; returns a process handle or a neg error.
    let pa = unsafe { syscall1(SYS_PROCESS_SPAWN, (&raw const SPAWN_A) as u64) };
    let pb = unsafe { syscall1(SYS_PROCESS_SPAWN, (&raw const SPAWN_B) as u64) };
    if pa < 0 || pb < 0 {
        kprint(b"parent: spawn FAIL\n");
        exit(1);
    }
    kprint(b"parent: spawned two children sharing a channel\n");

    // 3. Drain two ChildExited notifications, blocking on our channel.
    let mut got = 0;
    while got < 2 {
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
            kprint(b"parent: wait FAIL\n");
            exit(1);
        }
        // Drain every queued notification this wake delivered.
        loop {
            // SAFETY: NOTIF is a valid 64-byte writable out-param.
            let r = unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) };
            if r != 0 {
                break; // WouldBlock: drained
            }
            // SAFETY: the kernel wrote a 64-byte Notification into NOTIF.
            let (kind, b) = unsafe { ((&raw const NOTIF.kind).read(), (&raw const NOTIF.body).read()) };
            if kind == KIND_CHILD_EXITED {
                let pid = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                let code = i32::from_le_bytes([b[8], b[9], b[10], b[11]]);
                kprint(b"parent: child exited pid=");
                kprint_u64(pid as u64);
                kprint(b" code=");
                kprint_u64(code as u64);
                kprint(b"\n");
                got += 1;
            }
        }
    }

    // 4. Tidy up the child process handles and exit.
    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, pa as u64);
        syscall1(SYS_HANDLE_CLOSE, pb as u64);
    }
    kprint(b"parent: both children reaped; exiting\n");
    exit(0);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}
