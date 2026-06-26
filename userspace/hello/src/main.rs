//! `hello` — the first Nitrox userspace program (a throwaway Phase-1 proof).
//!
//! A freestanding ring-3 program that exercises the syscall surface available
//! so far and reports each step over the debug `sys_kprint` log:
//!
//! 1. prints a greeting;
//! 2. `sys_memory_create` → a one-page anonymous `MemoryObject`;
//! 3. `sys_memory_map` → maps it read/write and round-trips a byte through the
//!    mapped page (the proof that user PTEs point at the object's frames);
//! 4. `sys_handle_stat` / `duplicate` / `restrict` / `close` on the memory
//!    handle — the handle-operation syscalls' first end-to-end ring-3 run;
//! 4b. `sys_clock_read(Monotonic)` twice — the monotonic clock must advance;
//! 4c. `sys_timer_create` + `sys_timer_set` (~50 ms) + `sys_wait` — block on a
//!     timer and wake when it fires; plus a poll (`WouldBlock`) and a pure
//!     timeout (`TimedOut`) case;
//! 5. `sys_memory_unmap`, then `sys_process_exit`.
//!
//! It exists only to demonstrate the kernel can load an ELF, build a process +
//! address space, schedule a thread into ring 3, service syscalls, and tear
//! the process down. It is replaced by the real `init` (PID 1) once an
//! initramfs exists.
//!
//! Built as a **static, non-PIE `ET_EXEC`** at a low user virtual address —
//! see `user.ld` and `.cargo/config.toml`.

#![no_std]
#![no_main]

use core::arch::asm;
use libkern::{
    CLOCK_MONOTONIC, HandleInfo, IoResult, IpcMsg, KError, KOBJ_MEMORY_OBJECT, RIGHT_DUPLICATE,
    RIGHT_INSPECT, RIGHT_MAP_READ, RIGHT_MAP_WRITE, SENDMODE_NOBLOCK, SYS_CHANNEL_CREATE,
    SYS_CHANNEL_RECV, SYS_CHANNEL_SEND, SYS_CLOCK_READ, SYS_HANDLE_CLOSE, SYS_HANDLE_DUPLICATE,
    SYS_HANDLE_RESTRICT, SYS_HANDLE_STAT, SYS_MEMORY_CREATE, SYS_MEMORY_MAP, SYS_MEMORY_UNMAP,
    SYS_TIMER_CREATE, SYS_TIMER_SET, SYS_WAIT, exit, kprint, syscall1, syscall2, syscall4, syscall5,
};

const PAGE: u64 = 4096;

/// Errors (`KError`), as returned (negated) in rax — derived from `libkern`.
const E_WOULD_BLOCK: i64 = KError::WouldBlock.as_i32() as i64;
const E_TIMED_OUT: i64 = KError::TimedOut.as_i32() as i64;
const E_PEER_CLOSED: i64 = KError::PeerClosed.as_i32() as i64;

/// Out-buffer for one `sys_wait` result + a one-entry handles array, both in
/// writable `.bss`.
static mut WAIT_RESULTS: IoResult = IoResult { handle: 0, status: 0, reserved: 0, result: 0 };
static mut WAIT_HANDLES: [u64; 1] = [0];

/// The channel endpoint handles `sys_channel_create` writes back. Writable `.bss`.
static mut END0: u64 = 0;
static mut END1: u64 = 0;
/// Send/receive message buffers + the recv handle-count out-param.
static mut SEND_MSG: IpcMsg = IpcMsg::ZEROED;
static mut RECV_MSG: IpcMsg = IpcMsg::ZEROED;
static mut RECV_COUNT: usize = 0;

/// The payload `hello` sends to itself over IPC.
const IPC_PING: &[u8] = b"ipc: ping\n";

const MSG: &[u8] = b"hello from ring 3 (pid 1)\n";

/// Scratch buffer for `sys_handle_stat`'s `HandleInfo` out-parameter (writable `.bss`).
static mut STAT_BUF: HandleInfo = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };

/// Out-parameter for `sys_clock_read`. Writable `.bss`, naturally 8-aligned.
static mut CLOCK_BUF: u64 = 0;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    kprint(MSG);

    // 1. Create a one-page memory object.
    // SAFETY: a valid syscall; returns a handle (>= 0) or a negative KError.
    let h = unsafe { syscall2(SYS_MEMORY_CREATE, PAGE, 0) };
    if h < 0 {
        kprint(b"memory: create FAIL\n");
        exit(1);
    }
    let h = h as u64;

    // 2. Map it read/write (anywhere).
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

    // stat → expect type MemoryObject and the MAP_READ right present.
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

    // duplicate → a second handle with only INSPECT|DUPLICATE.
    // SAFETY: valid syscall; returns a new handle or a negative KError.
    let dup = unsafe { syscall2(SYS_HANDLE_DUPLICATE, h, RIGHT_INSPECT | RIGHT_DUPLICATE) };
    if dup < 0 {
        handle_ops_ok = false;
    } else {
        let dup = dup as u64;
        // restrict the duplicate to INSPECT only, then close it.
        // SAFETY: valid syscalls operating on our own handle.
        let rr = unsafe { syscall2(SYS_HANDLE_RESTRICT, dup, RIGHT_INSPECT) };
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
    // A little observable work so the counter advances measurably between reads.
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

    // 4c. Timer + sys_wait: create a timer, arm it ~50 ms out, and block on it.
    // SAFETY: a valid syscall; returns a handle (>= 0) or a negative KError.
    let th = unsafe { syscall1(SYS_TIMER_CREATE, 0) };
    if th < 0 {
        kprint(b"timer: create FAIL\n");
        exit(1);
    }
    let th = th as u64;

    // Read t0, arm for t0 + 50 ms (absolute monotonic, one-shot), wait on it.
    // SAFETY: CLOCK_BUF is a writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    let t0 = unsafe { (&raw const CLOCK_BUF).read() };
    let fire_at = t0 + 50_000_000; // +50 ms
    // SAFETY: arming our own timer handle.
    let sr = unsafe { syscall4(SYS_TIMER_SET, th, fire_at, 0, 0) };
    // SAFETY: WAIT_HANDLES / WAIT_RESULTS are valid writable buffers.
    let waited = unsafe {
        WAIT_HANDLES[0] = th;
        // Generous 5 s overall deadline so the timer (not the deadline) wakes us.
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            t0 + 5_000_000_000,
        )
    };
    // SAFETY: on success (waited == 1) the kernel wrote one IoResult.
    let woke_handle = unsafe { (&raw const WAIT_RESULTS.handle).read() };
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    let t_after = unsafe { (&raw const CLOCK_BUF).read() };
    if sr == 0 && waited == 1 && woke_handle == th && t_after >= fire_at {
        kprint(b"timer: waited and woke ok\n");
    } else {
        kprint(b"timer: wait FAIL\n");
    }

    // 4d. Poll an unset timer (deadline 0) — nothing ready → WouldBlock.
    let th2 = unsafe { syscall1(SYS_TIMER_CREATE, 0) };
    if th2 >= 0 {
        let th2 = th2 as u64;
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
        unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
        let now = unsafe { (&raw const CLOCK_BUF).read() };
        unsafe { syscall4(SYS_TIMER_SET, th2, now + 10_000_000_000, 0, 0) }; // +10 s
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
        unsafe { syscall1(SYS_HANDLE_CLOSE, th2) };
    }
    unsafe { syscall1(SYS_HANDLE_CLOSE, th) };

    // 5. Unmap.
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
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut END0) as u64, (&raw mut END1) as u64, 4, 0)
    };
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
    let sr = unsafe {
        syscall5(SYS_CHANNEL_SEND, e0, (&raw const SEND_MSG) as u64, 0, 0, SENDMODE_NOBLOCK)
    };
    if sr != 0 {
        ipc_ok = false;
    }

    // Polling end0 finds nothing — the message went to end1's inbox, not end0's.
    // SAFETY: RECV_MSG / RECV_COUNT are valid writable out-params; handles unused.
    let pr = unsafe {
        syscall4(SYS_CHANNEL_RECV, e0, (&raw mut RECV_MSG) as u64, 0, (&raw mut RECV_COUNT) as u64)
    };
    if pr == E_WOULD_BLOCK {
        kprint(b"ipc: empty-endpoint would-block ok\n");
    } else {
        ipc_ok = false;
    }

    // Block on end1 (immediately signaled), then receive and verify the message.
    // SAFETY: WAIT_HANDLES / WAIT_RESULTS are valid writable buffers.
    let waited = unsafe {
        WAIT_HANDLES[0] = e1;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
    };
    // SAFETY: valid endpoint handle + writable out-params.
    let rr = unsafe {
        syscall4(SYS_CHANNEL_RECV, e1, (&raw mut RECV_MSG) as u64, 0, (&raw mut RECV_COUNT) as u64)
    };
    // SAFETY: on success the kernel wrote a full message into RECV_MSG.
    let (rsp, rlen) = unsafe {
        (
            (&raw const RECV_MSG.header.sender_pid).read(),
            (&raw const RECV_MSG.header.payload_len).read(),
        )
    };
    // SAFETY: RECV_MSG.payload is initialised; compare the echoed bytes.
    let payload_match = unsafe {
        let mut ok = rlen as usize == IPC_PING.len();
        let mut i = 0;
        while i < IPC_PING.len() {
            if RECV_MSG.payload[i] != IPC_PING[i] {
                ok = false;
            }
            i += 1;
        }
        ok
    };
    if rr == 0 && waited == 1 && rsp == 1 && payload_match {
        kprint(b"ipc: received message from pid 1 ok\n");
    } else {
        ipc_ok = false;
    }

    // A second recv on the now-drained end1 would block.
    // SAFETY: valid out-params.
    let dr = unsafe {
        syscall4(SYS_CHANNEL_RECV, e1, (&raw mut RECV_MSG) as u64, 0, (&raw mut RECV_COUNT) as u64)
    };
    if dr == E_WOULD_BLOCK {
        kprint(b"ipc: drained would-block ok\n");
    } else {
        ipc_ok = false;
    }

    // Close end0; receiving on end1 now reports the closed peer.
    // SAFETY: closing our own endpoint handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, e0) };
    // SAFETY: valid out-params.
    let pc = unsafe {
        syscall4(SYS_CHANNEL_RECV, e1, (&raw mut RECV_MSG) as u64, 0, (&raw mut RECV_COUNT) as u64)
    };
    if pc == E_PEER_CLOSED {
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
    // SAFETY-NOT: an intentional fault; control never returns past this store.
    unsafe { core::ptr::write_volatile(0x1000 as *mut u64, 0xDEAD) };
    // Unreachable: the store faults and the kernel terminates this thread.
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
