//! `parent` — the Phase-1 process-spawn demo supervisor.
//!
//! Booted by the kernel as PID 1 with one bootstrap argument: a handle to its
//! own notification channel (in `rdi`, the first `extern "C"` parameter). It:
//!
//! 0. runs the **exception demo**: maps a worker stack, creates a second thread
//!    in this process (`sys_thread_create`) whose entry deliberately faults,
//!    receives the `SegFault` on its notification channel, reads the faulting
//!    thread's registers (`sys_thread_get_registers`), and terminates it
//!    (`sys_exception_resume` with `Terminate`);
//! 1. creates an IPC channel (`sys_channel_create`) → two endpoints;
//! 2. spawns two `child` processes (`sys_process_spawn`), **moving** one
//!    endpoint into each — so the children share a channel they can talk over;
//! 3. blocks on its notification channel (`sys_wait`) and drains two
//!    `ChildExited` notifications (`sys_notif_recv`), reporting each;
//! 4. exits the whole process (`sys_process_exit`).
//!
//! This is the Phase-1 milestone proof: a multi-threaded supervisor that
//! suspends + inspects + terminates a faulting thread, plus two userspace
//! processes communicating over IPC, all spawned by a parent that learns of
//! their exits. (A real `init` with an initramfs and a service manager is
//! Phase 2.)

#![no_std]
#![no_main]

use core::arch::asm;

// --- Syscall numbers (must match `kernel/src/syscall/table.rs`) ----------
const SYS_HANDLE_CLOSE: u64 = 0;
const SYS_MEMORY_CREATE: u64 = 4;
const SYS_MEMORY_MAP: u64 = 5;
const SYS_WAIT: u64 = 10;
const SYS_NOTIF_RECV: u64 = 11;
const SYS_CHANNEL_CREATE: u64 = 12;
const SYS_CHANNEL_SEND: u64 = 13;
const SYS_CHANNEL_RECV: u64 = 14;
const SYS_PROCESS_SPAWN: u64 = 15;
const SYS_PROCESS_EXIT: u64 = 16;
const SYS_THREAD_CREATE: u64 = 19;
const SYS_THREAD_GET_REGISTERS: u64 = 20;
const SYS_EXCEPTION_RESUME: u64 = 21;
const SYS_NS_CREATE: u64 = 22;
const SYS_NS_LOOKUP: u64 = 23;
const SYS_NS_BIND: u64 = 24;
const SYS_NS_UNBIND: u64 = 25;
const SYS_ENTROPY_CREATE: u64 = 26;
const SYS_ENTROPY_READ: u64 = 27;
const SYS_DEBUG_KPRINT: u64 = 0xFFFF_0000;

/// `SendMode` values (`kernel/src/libkern/ipc.rs`).
const SENDMODE_BLOCK: u64 = 0;
const SENDMODE_NOBLOCK: u64 = 1;
const SENDMODE_BLOCKBOUNDED: u64 = 2;

/// `KError::TimedOut` (`kernel/src/syscall/error.rs`), as an `IoResult.status`.
const KERR_TIMED_OUT: i32 = -12;

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

/// `MAP_*` rights bits (`kernel/src/libkern/handle.rs`) for mapping the worker
/// stack read/write.
const RIGHT_MAP_READ: u64 = 1 << 15;
const RIGHT_MAP_WRITE: u64 = 1 << 16;

/// One page; the worker thread's stack size.
const PAGE: u64 = 4096;
/// `sys_exception_resume` disposition: terminate the thread with a code.
const DISPOSITION_TERMINATE: u64 = 2;

/// `ImageId::Child` (`kernel/src/libkern/spawn.rs`).
const IMAGE_CHILD: u32 = 0;
/// `Notification::ChildExited` discriminant (`kernel/src/libkern/notification.rs`).
const KIND_CHILD_EXITED: u32 = 0x0200;
/// `Notification::SegFault` discriminant (`kernel/src/libkern/notification.rs`).
const KIND_SEG_FAULT: u32 = 0x0100;

/// Userspace mirror of the kernel `ThreadArgs` (`kernel/src/libkern/thread.rs`):
/// a 64-byte block — `entry`, `user_sp`, `arg0`, then 40 reserved bytes.
#[repr(C)]
struct ThreadArgs {
    entry: u64,
    user_sp: u64,
    arg0: u64,
    reserved: [u8; 40],
}

/// Userspace mirror of the kernel `RegisterValues` (`kernel/src/libkern/thread.rs`):
/// 18 `u64`s — the 16 GPRs (incl. `rsp`), then `rip` (index 16) and `rflags`.
#[repr(C, align(8))]
struct RegisterValues {
    regs: [u64; 18],
}
/// Index of `rip` within [`RegisterValues::regs`].
const REG_RIP: usize = 16;

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
    /// Child's root namespace: 0 = inherit the parent's; non-null = a constructed
    /// (restricted) namespace the child gets a LOOKUP-only handle to.
    namespace: u64,
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
    namespace: 0, // set at runtime to the constructed child namespace
};
static mut SPAWN_B: SpawnArgs = SpawnArgs {
    image: IMAGE_CHILD,
    handle_count: 1,
    move_mask: 1,
    _pad: 0,
    arg0: 1, // role 1 = receiver
    handles: [0; 4],
    rights: [ENDPOINT_RIGHTS, 0, 0, 0],
    namespace: 0, // set at runtime to the constructed child namespace
};
static mut NOTIF: NotificationBuf = NotificationBuf { kind: 0, body: [0; 60] };
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
static mut WAIT_HANDLES: [u64; 1] = [0];
/// A zeroed 4096-byte IPC message (empty payload, no transfers) for the
/// blocking-send demo, used for both send and recv.
static mut MSGBUF: [u8; 4096] = [0; 4096];
/// Transferred-handle out-array for recv (always empty in the demo).
static mut HBUF: [u64; 8] = [0; 8];
/// Recv'd handle-count out-param.
static mut RECV_COUNT: usize = 0;
static mut WORKER_ARGS: ThreadArgs = ThreadArgs { entry: 0, user_sp: 0, arg0: 0, reserved: [0; 40] };
static mut WORKER_REGS: RegisterValues = RegisterValues { regs: [0; 18] };

/// The worker thread's entry point: write to a deliberately-unmapped address so
/// the very first access page-faults (`#PF`). The kernel suspends the thread,
/// delivers a `SegFault` to this process, and (after the supervisor's
/// `sys_exception_resume`) terminates it — so this never returns normally.
extern "C" fn worker_fault() -> ! {
    // SAFETY: this is the whole point — `0xdead_0000` is an unmapped user
    // address, so the store traps. The kernel never lets the store complete.
    unsafe { core::ptr::write_volatile(0xdead_0000usize as *mut u64, 0xc0ffee) };
    // Unreachable in practice (the kernel terminates us); spin defensively.
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}

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

#[inline]
unsafe fn syscall6(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let ret;
    // SAFETY: as `syscall5`, plus `r9` for the 6th argument.
    unsafe {
        asm!(
            "syscall",
            in("rax") nr, in("rdi") a0, in("rsi") a1, in("rdx") a2, in("r10") a3, in("r8") a4,
            in("r9") a5,
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

/// Print a 64-bit value as `0x`-prefixed, 16-digit lowercase hex (for the
/// faulting `rip`), no allocation.
fn kprint_hex(v: u64) {
    let mut buf = [0u8; 18];
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..16 {
        let nib = ((v >> ((15 - i) * 4)) & 0xf) as u8;
        buf[2 + i] = if nib < 10 { b'0' + nib } else { b'a' + (nib - 10) };
    }
    kprint(&buf);
}

/// The exception demo (step 0): create a second thread in this process that
/// immediately faults, observe the `SegFault`, inspect the faulting registers,
/// and terminate the thread. `notif` is this process's notification channel.
fn worker_exception_demo(notif: u64) {
    kprint(b"parent: mapping a worker stack\n");
    // 1. Allocate + map a one-page worker stack (read/write).
    // SAFETY: register-only syscalls with valid arguments.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if mem < 0 {
        kprint(b"parent: worker stack create FAIL\n");
        exit(1);
    }
    // SAFETY: maps the memory object read/write at a kernel-chosen address.
    let base = unsafe {
        syscall4(SYS_MEMORY_MAP, mem as u64, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE)
    };
    if base < 0 {
        kprint(b"parent: worker stack map FAIL\n");
        exit(1);
    }
    let stack_top = base as u64 + PAGE; // stacks grow down from the top

    // 2. Create the worker thread (entry = worker_fault, sp = stack top).
    // SAFETY: WORKER_ARGS is a valid writable arg block; the pointer is read by
    // the kernel.
    let worker = unsafe {
        WORKER_ARGS.entry = worker_fault as *const () as usize as u64;
        WORKER_ARGS.user_sp = stack_top;
        WORKER_ARGS.arg0 = 0;
        syscall1(SYS_THREAD_CREATE, (&raw const WORKER_ARGS) as u64)
    };
    if worker < 0 {
        kprint(b"parent: thread_create FAIL\n");
        exit(1);
    }
    kprint(b"parent: created worker thread; awaiting its fault\n");

    // 3. Block on our notification channel until the worker's SegFault arrives.
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
            kprint(b"parent: wait FAIL\n");
            exit(1);
        }
        // SAFETY: NOTIF is a valid 64-byte writable out-param.
        let r = unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) };
        if r != 0 {
            continue; // WouldBlock: re-block on the channel
        }
        // SAFETY: the kernel wrote a 64-byte Notification into NOTIF.
        let kind = unsafe { (&raw const NOTIF.kind).read() };
        if kind == KIND_SEG_FAULT {
            break;
        }
        // Ignore any other notification kind.
    }

    // 4. Read the faulting thread's registers and report the faulting rip.
    // SAFETY: WORKER_REGS is a valid writable RegisterValues out-param.
    let gr = unsafe {
        syscall4(SYS_THREAD_GET_REGISTERS, worker as u64, (&raw mut WORKER_REGS) as u64, 0, 0)
    };
    if gr != 0 {
        kprint(b"parent: get_registers FAIL\n");
        exit(1);
    }
    // SAFETY: the kernel wrote the 18-register snapshot into WORKER_REGS.
    let rip = unsafe { (&raw const WORKER_REGS.regs[REG_RIP]).read() };
    kprint(b"parent: worker faulted @ rip=");
    kprint_hex(rip);
    kprint(b" ; terminating\n");

    // 5. Terminate the worker (resume with the Terminate disposition, code 7).
    // SAFETY: register-only syscall.
    let er = unsafe {
        syscall4(SYS_EXCEPTION_RESUME, worker as u64, DISPOSITION_TERMINATE, 7, 0)
    };
    if er != 0 {
        kprint(b"parent: exception_resume FAIL\n");
        exit(1);
    }
    // The worker is not this process's last thread (we are still running), so
    // its termination produces no `ChildExited`. Drop our handle to it.
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, worker as u64) };
    kprint(b"parent: worker terminated\n");
}

/// Demonstrate the blocking-send / `PendingOperation` path end-to-end against the
/// live kernel: fill a channel's receive ring, then a `Block` send returns a
/// `PendingOperation` handle (the message is held in-kernel); a recv frees a slot,
/// promoting the held message and completing the PO; `sys_wait` on the PO then
/// reports the completion (status 0). Self-contained — the parent holds both ends.
fn block_send_demo() {
    // Fresh channel pair, depth 4, both ends held here.
    // SAFETY: END0/END1 are valid writable out-params.
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut END0) as u64, (&raw mut END1) as u64, 4, 0)
    };
    if cr != 0 {
        kprint(b"parent: block-demo channel create FAIL\n");
        return;
    }
    // SAFETY: the kernel wrote both endpoint handles.
    let (a, b) = unsafe { ((&raw const END0).read(), (&raw const END1).read()) };

    // Fill b's receive ring: NoBlock-send a->b until WouldBlock.
    let mut filled = 0u64;
    loop {
        // SAFETY: valid endpoint + zeroed message; count 0 (no transfers).
        let r = unsafe {
            syscall5(SYS_CHANNEL_SEND, a, (&raw const MSGBUF) as u64, 0, 0, SENDMODE_NOBLOCK)
        };
        if r == 0 {
            filled += 1;
        } else {
            break; // WouldBlock: the ring is full
        }
    }

    // One more, blocking: the ring is full, so this returns a PendingOperation
    // handle (>= 0) rather than blocking inside the syscall.
    // SAFETY: as above, with Block mode.
    let po = unsafe {
        syscall5(SYS_CHANNEL_SEND, a, (&raw const MSGBUF) as u64, 0, 0, SENDMODE_BLOCK)
    };
    if po < 0 {
        kprint(b"parent: block send FAIL\n");
        return;
    }
    let po = po as u64;

    // Receive one from b: frees a slot, so the held sender is promoted into the
    // ring and its PendingOperation completes.
    // SAFETY: valid out-params; the demo message carries no transferred handles.
    let rr = unsafe {
        syscall4(SYS_CHANNEL_RECV, b, (&raw mut MSGBUF) as u64, (&raw mut HBUF) as u64, (&raw mut RECV_COUNT) as u64)
    };
    if rr != 0 {
        kprint(b"parent: block-demo recv FAIL\n");
        return;
    }

    // Wait on the PendingOperation; it is now complete (status 0 = delivered).
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
    let waited = unsafe {
        WAIT_HANDLES[0] = po;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
    };
    // `IoResult.status` is the i32 at bytes 8..12 of the 16-byte record.
    let status = unsafe {
        i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]])
    };
    if waited == 1 && status == 0 {
        kprint(b"parent: blocking send completed via PendingOperation (");
        kprint_u64(filled);
        kprint(b" queued, 1 blocked-then-delivered)\n");
    } else {
        kprint(b"parent: block-demo wait unexpected\n");
    }

    // Drain the rest of b and close every handle.
    loop {
        // SAFETY: valid out-params.
        let r = unsafe {
            syscall4(SYS_CHANNEL_RECV, b, (&raw mut MSGBUF) as u64, (&raw mut HBUF) as u64, (&raw mut RECV_COUNT) as u64)
        };
        if r != 0 {
            break;
        }
    }
    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, po);
        syscall1(SYS_HANDLE_CLOSE, a);
        syscall1(SYS_HANDLE_CLOSE, b);
    }
}

/// Demonstrate the `BlockBounded` (deadline-bounded) send: fill a channel's ring
/// and issue a `BlockBounded` send with a deadline already in the past to an
/// endpoint no one receives. The held message can never be delivered, so on the
/// next timer tick its deadline elapses and the returned `PendingOperation`
/// completes `TimedOut` — `sys_wait` reports that status.
fn block_bounded_demo() {
    // SAFETY: END0/END1 are valid writable out-params.
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut END0) as u64, (&raw mut END1) as u64, 4, 0)
    };
    if cr != 0 {
        kprint(b"parent: block-bounded channel create FAIL\n");
        return;
    }
    // SAFETY: the kernel wrote both endpoint handles.
    let (a, b) = unsafe { ((&raw const END0).read(), (&raw const END1).read()) };

    // Fill b's receive ring so the next send must be held.
    loop {
        // SAFETY: valid endpoint + zeroed message; count 0 (no transfers).
        let r = unsafe {
            syscall5(SYS_CHANNEL_SEND, a, (&raw const MSGBUF) as u64, 0, 0, SENDMODE_NOBLOCK)
        };
        if r != 0 {
            break; // WouldBlock: full
        }
    }

    // BlockBounded send, deadline `1` (already in the past) → held now, timed out
    // on the next tick. The deadline is the 6th arg.
    // SAFETY: as above, BlockBounded mode + a past deadline.
    let po = unsafe {
        syscall6(SYS_CHANNEL_SEND, a, (&raw const MSGBUF) as u64, 0, 0, SENDMODE_BLOCKBOUNDED, 1)
    };
    if po < 0 {
        kprint(b"parent: block-bounded send FAIL\n");
        return;
    }
    let po = po as u64;

    // Wait on the PO; it completes `TimedOut` once the deadline fires.
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
    let waited = unsafe {
        WAIT_HANDLES[0] = po;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
    };
    // `IoResult.status` is the i32 at bytes 8..12 of the 16-byte record.
    let status = unsafe {
        i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]])
    };
    if waited == 1 && status == KERR_TIMED_OUT {
        kprint(b"parent: blocking send timed out via PendingOperation (BlockBounded)\n");
    } else {
        kprint(b"parent: block-bounded demo unexpected\n");
    }

    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, po);
        syscall1(SYS_HANDLE_CLOSE, a);
        syscall1(SYS_HANDLE_CLOSE, b);
    }
}

/// Namespace demo: exercise the full `create → bind → lookup → wait → use`
/// path against this process's **root namespace** (`root_ns`, seeded in `rsi` by
/// the kernel — `Process::namespace`). Proves all four `sys_ns_*` syscalls plus
/// the async-lookup result word (`IoResult.result` carries the resolved handle).
fn ns_demo(root_ns: u64) {
    kprint(b"parent: ns-demo start (root ns in rsi)\n");

    // (a) sys_ns_create: a fresh, independent namespace; close it after — proves
    //     create returns a usable, full-rights handle distinct from the root.
    // SAFETY: register-only syscall.
    let fresh = unsafe { syscall1(SYS_NS_CREATE, 0) };
    if fresh < 0 {
        kprint(b"parent: ns_create FAIL\n");
        return;
    }
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, fresh as u64) };
    kprint(b"parent: ns_create ok\n");

    // (b) Create a MemoryObject to bind as a direct-handle resource.
    // SAFETY: register-only syscall.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if mem < 0 {
        kprint(b"parent: ns-demo mem create FAIL\n");
        return;
    }
    let mem = mem as u64;

    // (c) sys_ns_bind: bind the MemoryObject at "/store" in the root namespace.
    // SAFETY: PATH is a valid readable byte slice; valid handles.
    let path = b"/store";
    let br = unsafe {
        syscall4(SYS_NS_BIND, root_ns, path.as_ptr() as u64, path.len() as u64, mem)
    };
    if br != 0 {
        kprint(b"parent: ns_bind FAIL\n");
        return;
    }
    kprint(b"parent: ns_bind /store ok\n");

    // (d) sys_ns_lookup: resolve "/store" requesting MAP_READ|MAP_WRITE. Returns
    //     a PendingOperation; (e) sys_wait yields the resolved handle in
    //     IoResult.result.
    // SAFETY: valid path pointer + handle.
    let po = unsafe {
        syscall4(
            SYS_NS_LOOKUP,
            root_ns,
            path.as_ptr() as u64,
            path.len() as u64,
            RIGHT_MAP_READ | RIGHT_MAP_WRITE,
        )
    };
    if po < 0 {
        kprint(b"parent: ns_lookup FAIL\n");
        return;
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
    // `IoResult`: status at bytes 8..12, result (resolved handle) at 16..24.
    let status = unsafe {
        i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]])
    };
    let resolved = unsafe {
        u64::from_le_bytes([
            WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
            WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
        ])
    };
    if waited != 1 || status != 0 || resolved == 0 {
        kprint(b"parent: ns_lookup wait unexpected\n");
        return;
    }
    kprint(b"parent: ns_lookup -> resolved handle=");
    kprint_u64(resolved);
    kprint(b"\n");

    // (f) Use the resolved handle: map it read/write — proves the binding handed
    //     back a usable, rights-attenuated MemoryObject handle.
    // SAFETY: `resolved` is a MemoryObject handle carrying MAP_READ|MAP_WRITE.
    let mapped = unsafe {
        syscall4(SYS_MEMORY_MAP, resolved, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE)
    };
    if mapped < 0 {
        kprint(b"parent: ns-demo map resolved FAIL\n");
        return;
    }
    kprint(b"parent: mapped resolved /store handle ok\n");

    // (g) sys_ns_unbind: remove "/store"; a follow-up lookup must complete the PO
    //     with a NotFound status (error delivered through the PO, not the syscall).
    // SAFETY: valid path pointer + handle.
    let ur = unsafe {
        syscall4(SYS_NS_UNBIND, root_ns, path.as_ptr() as u64, path.len() as u64, 0)
    };
    if ur != 0 {
        kprint(b"parent: ns_unbind FAIL\n");
        return;
    }
    // SAFETY: valid path pointer + handle.
    let po2 = unsafe {
        syscall4(SYS_NS_LOOKUP, root_ns, path.as_ptr() as u64, path.len() as u64, RIGHT_MAP_READ)
    };
    if po2 < 0 {
        kprint(b"parent: ns_lookup-after-unbind FAIL\n");
        return;
    }
    // SAFETY: valid wait buffers.
    let waited2 = unsafe {
        WAIT_HANDLES[0] = po2 as u64;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
    };
    let status2 = unsafe {
        i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]])
    };
    // KError::NotFound = -10.
    if waited2 == 1 && status2 == -10 {
        kprint(b"parent: ns_unbind ok (lookup-after-unbind -> NotFound)\n");
    } else {
        kprint(b"parent: ns-demo unbind unexpected\n");
    }

    // Close the demo handles we still hold (resolved + the original mem + the POs).
    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, po as u64);
        syscall1(SYS_HANDLE_CLOSE, po2 as u64);
        syscall1(SYS_HANDLE_CLOSE, resolved);
        syscall1(SYS_HANDLE_CLOSE, mem);
    }
    kprint(b"parent: ns-demo done\n");
}

/// Entropy demo: create an `EntropyObject` token and read CSPRNG bytes from it.
/// The pool seeds at boot (QEMU runs with `+rdrand,+rdseed`), so both reads return
/// `0` (synchronous fill) and the two 32-byte draws differ (the CSPRNG advances).
fn entropy_demo() {
    kprint(b"parent: entropy-demo start\n");
    // SAFETY: register-only syscall.
    let h = unsafe { syscall1(SYS_ENTROPY_CREATE, 0) };
    if h < 0 {
        kprint(b"parent: entropy_create FAIL\n");
        return;
    }
    let h = h as u64;

    let mut a = [0u8; 32];
    let mut b = [0u8; 32];
    // SAFETY: valid writable 32-byte buffers; `len` ≤ ENTROPY_READ_MAX.
    let r1 = unsafe { syscall4(SYS_ENTROPY_READ, h, (&raw mut a) as u64, 32, 0) };
    let r2 = unsafe { syscall4(SYS_ENTROPY_READ, h, (&raw mut b) as u64, 32, 0) };
    if r1 != 0 || r2 != 0 {
        // A positive return would mean "unseeded, wait on the PO" — not expected
        // here (the pool seeds at boot). Report and bail.
        kprint(b"parent: entropy read not synchronous (unseeded?)\n");
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
        return;
    }
    // The two 32-byte draws must differ (the CSPRNG advances each read). A manual
    // loop avoids emitting a `memcmp` intrinsic this freestanding binary lacks.
    let mut differ = false;
    for i in 0..32 {
        if a[i] != b[i] {
            differ = true;
            break;
        }
    }
    let first = u64::from_le_bytes([a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7]]);
    kprint(b"parent: entropy bytes[0..8]=");
    kprint_hex(first);
    if differ {
        kprint(b" (two reads differ) entropy ok\n");
    } else {
        kprint(b" entropy-demo UNEXPECTED (reads identical)\n");
    }
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
}

/// `notif` (in `rdi`) is this process's notification-channel handle and
/// `root_ns` (in `rsi`) its root-namespace handle, both seeded by the kernel at
/// spawn. The third bootstrap register is unused here.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, _boot2: u64) -> ! {
    kprint(b"parent: up (pid 1)\n");

    // 0. Exception demo: a worker thread faults; we suspend, inspect, terminate.
    worker_exception_demo(notif);

    // 0b. Blocking-send / PendingOperation demos (async-I/O primitive).
    block_send_demo();
    block_bounded_demo();

    // 0c. Namespace demo: create / bind / lookup / wait / use / unbind against
    //     this process's root namespace (Process::namespace, in rsi).
    ns_demo(root_ns);

    // 0d. Entropy demo: create an EntropyObject and read CSPRNG bytes.
    entropy_demo();

    kprint(b"parent: creating a channel\n");

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

    // 1b. Construct a *restricted* namespace for the children (sandbox-by-
    //     construction): a fresh namespace with just `/store` bound to a
    //     MemoryObject. The children inherit a LOOKUP-only handle to it.
    let child_ns = unsafe { syscall1(SYS_NS_CREATE, 0) };
    let store_mem = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if child_ns >= 0 && store_mem >= 0 {
        let sp = b"/store";
        // SAFETY: valid path pointer + handles.
        let br = unsafe {
            syscall4(SYS_NS_BIND, child_ns as u64, sp.as_ptr() as u64, sp.len() as u64, store_mem as u64)
        };
        if br == 0 {
            kprint(b"parent: built child namespace (/store)\n");
        } else {
            kprint(b"parent: child-namespace bind FAIL\n");
        }
    } else {
        kprint(b"parent: child-namespace create FAIL\n");
    }

    // 2. Spawn two children, moving one endpoint into each, and handing each the
    //    constructed namespace (a LOOKUP-only handle to it lands in the child).
    // SAFETY: SPAWN_A/SPAWN_B are valid writable arg blocks.
    unsafe {
        SPAWN_A.handles[0] = e0;
        SPAWN_B.handles[0] = e1;
        SPAWN_A.namespace = child_ns as u64;
        SPAWN_B.namespace = child_ns as u64;
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
