//! `hello` — a freestanding ring-3 program that exercises the syscall surface and reports each
//! step over `sys_kprint`; the boot gate counts on the lines it prints.
//!
//! Built as a static, non-PIE `ET_EXEC` at a low user address; see `user.ld` and
//! `.cargo/config.toml`.

#![no_std]
#![no_main]

use core::arch::asm;
use libkern::channel_recv_raw;
use libkern::{
    CLOCK_MONOTONIC, HandleInfo, IoResult, IpcMsg, KError, KOBJ_MEMORY_OBJECT, RIGHT_DUPLICATE, RIGHT_INSPECT,
    RIGHT_MAP_READ, RIGHT_MAP_WRITE, SENDMODE_NOBLOCK, SYS_CHANNEL_CREATE, SYS_CHANNEL_SEND, SYS_CLOCK_READ,
    SYS_HANDLE_CLOSE, SYS_HANDLE_DUPLICATE, SYS_HANDLE_RESTRICT, SYS_HANDLE_STAT, SYS_MEMORY_CREATE, SYS_MEMORY_MAP,
    SYS_MEMORY_UNMAP, SYS_TIMER_CREATE, SYS_TIMER_SET, SYS_WAIT, exit, kprint, syscall1, syscall2, syscall4, syscall5,
};

const PAGE: u64 = 4096;

const E_WOULD_BLOCK: i64 = KError::WouldBlock.as_i32() as i64;
const E_TIMED_OUT: i64 = KError::TimedOut.as_i32() as i64;
const E_PEER_CLOSED: i64 = KError::PeerClosed.as_i32() as i64;

static mut WAIT_RESULTS: IoResult = IoResult { handle: 0, status: 0, reserved: 0, result: 0 };
static mut WAIT_HANDLES: [u64; 1] = [0];

/// The channel endpoint handles `sys_channel_create` writes back. Writable `.bss`.
static mut END0: u64 = 0;
static mut END1: u64 = 0;
/// Send/receive message buffers + the recv handle-count out-param.
static mut SEND_MSG: IpcMsg = IpcMsg::ZEROED;
static mut RECV_MSG: [u8; libkern::abi::IPC_MSG_SIZE] = [0; libkern::abi::IPC_MSG_SIZE];

/// The payload `hello` sends to itself over IPC.
const IPC_PING: &[u8] = b"ipc: ping\n";

const MSG: &[u8] = b"hello from ring 3 (pid 1)\n";

/// Scratch buffer for `sys_handle_stat`'s `HandleInfo` out-parameter (writable `.bss`).
static mut STAT_BUF: HandleInfo = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };

/// Out-parameter for `sys_clock_read`. Writable `.bss`, naturally 8-aligned.
static mut CLOCK_BUF: u64 = 0;

/// Entry point: runs each demo step, then faults on purpose.
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    kprint(MSG);

    // SAFETY: a valid syscall; returns a handle (>= 0) or a negative KError.
    let h = unsafe { syscall2(SYS_MEMORY_CREATE, PAGE, 0) };
    if h < 0 {
        kprint(b"memory: create FAIL\n");
        exit(1);
    }
    let h = h as u64;

    // SAFETY: valid syscall; returns the mapped address or a negative KError.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, h, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        kprint(b"memory: map FAIL\n");
        exit(1);
    }
    let addr = addr as u64;

    // 3. Round-trip a byte through the mapped page. Volatile so the write and
    //    read are not elided — the point is to touch the object's frame.
    let p = addr as *mut u8;
    // SAFETY: `addr` is a page the kernel just mapped R/W into our address
    // space; a single in-bounds byte access is sound.
    let got = unsafe {
        p.write_volatile(0xA5);
        p.read_volatile()
    };
    if got == 0xA5 {
        kprint(b"memory: roundtrip ok\n");
    } else {
        kprint(b"memory: roundtrip FAIL\n");
    }

    // 4. Exercise the handle-operation syscalls on the memory handle. Each
    //    step prints its own failure so a regression is easy to localise.
    let mut handle_ops_ok = true;

    // SAFETY: STAT_BUF is a valid writable out-param of the right layout.
    let stat_ret = unsafe { syscall2(SYS_HANDLE_STAT, h, (&raw mut STAT_BUF) as u64) };
    // SAFETY: on success the kernel wrote a HandleInfo into STAT_BUF.
    let (ot, rights) = unsafe {
        let sb = &raw const STAT_BUF;
        ((*sb).object_type, (*sb).rights)
    };
    if stat_ret != 0 || ot != KOBJ_MEMORY_OBJECT || (rights & RIGHT_MAP_READ) == 0 {
        handle_ops_ok = false;
    }

    // SAFETY: valid syscall; returns a new handle or a negative KError.
    let dup = unsafe { syscall2(SYS_HANDLE_DUPLICATE, h, RIGHT_INSPECT | RIGHT_DUPLICATE) };
    if dup < 0 {
        handle_ops_ok = false;
    } else {
        let dup = dup as u64;
        // SAFETY: valid syscalls operating on our own handle.
        let rr = unsafe { syscall2(SYS_HANDLE_RESTRICT, dup, RIGHT_INSPECT) };
        // SAFETY: closing the duplicate handle we own
        let cr = unsafe { syscall1(SYS_HANDLE_CLOSE, dup) };
        if rr != 0 || cr != 0 {
            handle_ops_ok = false;
        }
    }

    if handle_ops_ok {
        kprint(b"handle-ops ok\n");
    } else {
        kprint(b"handle-ops FAIL\n");
    }

    // 4b. Read the monotonic clock twice with work in between; it must advance.
    // SAFETY: CLOCK_BUF is a valid writable u64 out-parameter.
    let r1 = unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: on success the kernel wrote the nanosecond count into CLOCK_BUF.
    let t1 = unsafe { (&raw const CLOCK_BUF).read() };
    let mut spin = 0u64;
    for _ in 0..200_000 {
        // SAFETY-free: a volatile-style accumulate the optimiser can't drop.
        spin = spin.wrapping_add(core::hint::black_box(1));
    }
    let _ = spin;
    // SAFETY: as the first read.
    let r2 = unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: as the first read.
    let t2 = unsafe { (&raw const CLOCK_BUF).read() };
    if r1 == 0 && r2 == 0 && t2 > t1 {
        kprint(b"clock: monotonic advancing\n");
    } else {
        kprint(b"clock: monotonic FAIL\n");
    }

    // SAFETY: a valid syscall; returns a handle (>= 0) or a negative KError.
    let th = unsafe { syscall1(SYS_TIMER_CREATE, 0) };
    if th < 0 {
        kprint(b"timer: create FAIL\n");
        exit(1);
    }
    let th = th as u64;

    // SAFETY: CLOCK_BUF is a writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: `CLOCK_BUF` is a plain `u64` static only this thread touches; a failed clock read
    // leaves its last value.
    let t0 = unsafe { (&raw const CLOCK_BUF).read() };
    let fire_at = t0 + 50_000_000; // +50 ms
    // SAFETY: arming our own timer handle.
    let sr = unsafe { syscall4(SYS_TIMER_SET, th, fire_at, 0, 0) };
    // SAFETY: WAIT_HANDLES / WAIT_RESULTS are valid writable buffers.
    let waited = unsafe {
        WAIT_HANDLES[0] = th;
        // Generous 5 s overall deadline so the timer (not the deadline) wakes us.
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, t0 + 5_000_000_000)
    };
    // SAFETY: on success (waited == 1) the kernel wrote one IoResult.
    let woke_handle = unsafe { (&raw const WAIT_RESULTS.handle).read() };
    // SAFETY: CLOCK_BUF is a writable u64 out-param
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: `CLOCK_BUF` is a plain `u64` static only this thread touches; a failed clock read
    // leaves its last value.
    let t_after = unsafe { (&raw const CLOCK_BUF).read() };
    if sr == 0 && waited == 1 && woke_handle == th && t_after >= fire_at {
        kprint(b"timer: waited and woke ok\n");
    } else {
        kprint(b"timer: wait FAIL\n");
    }

    // 4d. Poll an unset timer (deadline 0) — nothing ready → WouldBlock.
    // SAFETY: register-only syscall creating a timer we own
    let th2 = unsafe { syscall1(SYS_TIMER_CREATE, 0) };
    if th2 >= 0 {
        let th2 = th2 as u64;
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid statics owned by this thread; the zero
        // deadline polls rather than blocks
        let pr = unsafe {
            WAIT_HANDLES[0] = th2;
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, 0)
        };
        if pr == E_WOULD_BLOCK {
            kprint(b"wait: poll empty as expected\n");
        } else {
            kprint(b"wait: poll FAIL\n");
        }

        // 4e. Pure timeout: arm far in the future, wait with a near deadline.
        // SAFETY: CLOCK_BUF is a writable u64 out-param
        unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
        // SAFETY: `CLOCK_BUF` is a plain `u64` static only this thread touches; a failed clock read
        // leaves its last value.
        let now = unsafe { (&raw const CLOCK_BUF).read() };
        // SAFETY: arming our own timer handle
        unsafe { syscall4(SYS_TIMER_SET, th2, now + 10_000_000_000, 0, 0) }; // +10 s
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid statics owned by this thread; the deadline bounds the wait
        let tr = unsafe {
            WAIT_HANDLES[0] = th2;
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                1,
                (&raw mut WAIT_RESULTS) as u64,
                now + 30_000_000, // +30 ms
            )
        };
        if tr == E_TIMED_OUT {
            kprint(b"wait: timed out as expected\n");
        } else {
            kprint(b"wait: timeout FAIL\n");
        }
        // SAFETY: closing our own timer handle
        unsafe { syscall1(SYS_HANDLE_CLOSE, th2) };
    }
    // SAFETY: closing our own timer handle
    unsafe { syscall1(SYS_HANDLE_CLOSE, th) };

    // SAFETY: valid syscall on a region we mapped above.
    let ur = unsafe { syscall2(SYS_MEMORY_UNMAP, addr, PAGE) };
    if ur == 0 {
        kprint(b"memory: unmap ok\n");
    } else {
        kprint(b"memory: unmap FAIL\n");
    }

    // 5b. IPC round-trip. Create a channel, hold both endpoints, and send a
    //     message end0→end1, then receive it on end1 — a full ring-3
    //     create/send/wait/recv/peer-closed exercise of `sys_channel_*`.
    //     (Cross-process IPC + handle transfer arrive with process spawn.)
    let mut ipc_ok = true;
    // SAFETY: END0/END1 are valid writable u64 out-params; depth 4.
    let cr = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut END0) as u64, (&raw mut END1) as u64, 4, 0) };
    // SAFETY: on success the kernel wrote both endpoint handles.
    let (e0, e1) = unsafe { ((&raw const END0).read(), (&raw const END1).read()) };
    if cr != 0 {
        ipc_ok = false;
    }

    // Fill the send buffer's payload and send on end0 (NoBlock). The message
    // lands in end1's inbox.
    // SAFETY: SEND_MSG is a valid writable buffer in .bss.
    unsafe {
        SEND_MSG.header.payload_len = IPC_PING.len() as u32;
        let mut i = 0;
        while i < IPC_PING.len() {
            SEND_MSG.payload[i] = IPC_PING[i];
            i += 1;
        }
    }
    // SAFETY: valid endpoint handle + message pointer; NoBlock, no handles.
    let sr = unsafe { syscall5(SYS_CHANNEL_SEND, e0, (&raw const SEND_MSG) as u64, 0, 0, SENDMODE_NOBLOCK) };
    if sr != 0 {
        ipc_ok = false;
    }

    // Polling end0 finds nothing — the message went to end1's inbox, not end0's.
    // SAFETY: single-threaded; `RECV_MSG` is lent for each receive below.
    let pr = unsafe { channel_recv_raw(e0, &raw mut RECV_MSG) }.err();
    if pr == Some(E_WOULD_BLOCK) {
        kprint(b"ipc: empty-endpoint would-block ok\n");
    } else {
        ipc_ok = false;
    }

    // SAFETY: WAIT_HANDLES / WAIT_RESULTS are valid writable buffers.
    let waited = unsafe {
        WAIT_HANDLES[0] = e1;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    // SAFETY: valid endpoint handle + writable out-params.
    // SAFETY: as above.
    let rr = unsafe { channel_recv_raw(e1, &raw mut RECV_MSG) }.is_ok();
    // SAFETY: on success the kernel wrote a full message into RECV_MSG; nothing writes it now.
    let (rsp, rlen) = unsafe {
        (
            u32::from_le_bytes([RECV_MSG[0], RECV_MSG[1], RECV_MSG[2], RECV_MSG[3]]),
            u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]),
        )
    };
    // SAFETY: RECV_MSG is initialised; compare the echoed bytes.
    let payload_match = unsafe {
        let mut ok = rlen as usize == IPC_PING.len();
        let mut i = 0;
        while i < IPC_PING.len() {
            if RECV_MSG[libkern::abi::IPC_HEADER_SIZE + i] != IPC_PING[i] {
                ok = false;
            }
            i += 1;
        }
        ok
    };
    if rr && waited == 1 && rsp == 1 && payload_match {
        kprint(b"ipc: received message from pid 1 ok\n");
    } else {
        ipc_ok = false;
    }

    // SAFETY: as above.
    let dr = unsafe { channel_recv_raw(e1, &raw mut RECV_MSG) }.err();
    if dr == Some(E_WOULD_BLOCK) {
        kprint(b"ipc: drained would-block ok\n");
    } else {
        ipc_ok = false;
    }

    // SAFETY: closing our own endpoint handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, e0) };
    // SAFETY: as above.
    let pc = unsafe { channel_recv_raw(e1, &raw mut RECV_MSG) }.err();
    if pc == Some(E_PEER_CLOSED) {
        kprint(b"ipc: peer-closed detected ok\n");
    } else {
        ipc_ok = false;
    }
    // SAFETY: closing the surviving endpoint handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, e1) };

    if ipc_ok {
        kprint(b"ipc: ok\n");
    } else {
        kprint(b"ipc: FAIL\n");
    }

    // 6. Deliberately fault. With notifications, a userspace fault is delivered
    //    to the process's supervisor as a SegFault notification and this thread
    //    is terminated — the kernel survives (it used to halt). The supervisor
    //    (the kernel boot thread, this slice) reports the catch. We write to a
    //    never-mapped low address (page 1) → #PF (not-present, write).
    kprint(b"hello: triggering a deliberate fault\n");
    // SAFETY: a deliberate fault: page 1 is never mapped, so the store faults and the kernel terminates this thread
    unsafe { core::ptr::write_volatile(0x1000 as *mut u64, 0xDEAD) };
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}
