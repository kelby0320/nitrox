//! `parent` — the Phase-1 process-spawn demo supervisor.
//!
//! Spawned by **init** (PID 1) via `ImageId::Parent` as the slice-1/2/3 regression
//! chain (it was the boot pid-1 image through Phase 1 / early Phase 2). It receives
//! a handle to its own notification channel (`rdi`) and a LOOKUP-only handle to
//! init's root namespace (`rsi`). It:
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
//! 4. runs the **sched-stats demo** (`/proc/self/status` + `/proc/sched/stats`,
//!    the Phase 3 clause-3 gate — see [`sched_stats_demo`]);
//! 5. exits the whole process (`sys_process_exit`).
//!
//! This is the Phase-1 milestone proof: a multi-threaded supervisor that
//! suspends + inspects + terminates a faulting thread, plus two userspace
//! processes communicating over IPC, all spawned by a parent that learns of
//! their exits. (A real `init` with an initramfs and a service manager is
//! Phase 2.)

#![no_std]
#![no_main]

use core::arch::asm;
use libkern::*;
use libos::{Handle, Namespace, NsMutable, spawn, thread_create};

/// `KError::TimedOut` as an `IoResult.status` value (derived from `libkern`).
const KERR_TIMED_OUT: i32 = KError::TimedOut.as_i32();

/// The full rights set an IPC endpoint carries, handed to each child.
const ENDPOINT_RIGHTS: u64 =
    RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT | RIGHT_DUPLICATE | RIGHT_INSPECT | RIGHT_TRANSFER;

/// One page; the worker thread's stack size.
const PAGE: u64 = 4096;
/// `sys_exception_resume` disposition: terminate the thread with a code.
const DISPOSITION_TERMINATE: u64 = 2;

static mut END0: u64 = 0;
static mut END1: u64 = 0;
static mut SPAWN_A: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /initramfs/sbin/child
    handle_count: 1,
    move_mask: 1, // move handle 0 to the child
    arg0: 0, // role 0 = sender
    handles: [0; 4],
    rights: [ENDPOINT_RIGHTS, 0, 0, 0],
    namespace: 0, // set at runtime to the constructed child namespace
    syscaps: 0,   // children hold no ambient capabilities
};
static mut SPAWN_B: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /initramfs/sbin/child
    handle_count: 1,
    move_mask: 1,
    arg0: 1, // role 1 = receiver
    handles: [0; 4],
    rights: [ENDPOINT_RIGHTS, 0, 0, 0],
    namespace: 0, // set at runtime to the constructed child namespace
    syscaps: 0,   // children hold no ambient capabilities
};
static mut NOTIF: Notification = Notification::zeroed();
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
static mut WAIT_HANDLES: [u64; 1] = [0];
/// `sys_clock_read` out-param (the sched-stats demo's timer sleeps).
static mut CLOCK_BUF: u64 = 0;
/// Spawn args for the exit-storm stress children: `child` role 2 (exit
/// immediately), no handles, inherited namespace, empty syscaps. `image` is
/// filled per run.
static mut STORM_SPAWN: SpawnArgs = SpawnArgs {
    image: 0,
    handle_count: 0,
    move_mask: 0,
    arg0: 2, // role 2 = exit immediately
    handles: [0; 4],
    rights: [0; 4],
    namespace: 0, // inherit the parent's namespace
    syscaps: 0,
};
/// Spawn args for the hard-float workers: `child` role 3, no handles, inherited
/// namespace, empty syscaps. `image` and the per-worker seed in `arg0` are filled per
/// spawn (`arg0` = role in the low 8 bits, seed above — see `child`'s module docs).
static mut FP_SPAWN: SpawnArgs = SpawnArgs {
    image: 0,
    handle_count: 0,
    move_mask: 0,
    arg0: 3, // role 3 = hard-float worker; seed OR'd in per spawn
    handles: [0; 4],
    rights: [0; 4],
    namespace: 0, // inherit the parent's namespace
    syscaps: 0,
};
/// A zeroed 4096-byte IPC message (empty payload, no transfers) for the
/// blocking-send demo, used for both send and recv.
static mut MSGBUF: [u8; 4096] = [0; 4096];
/// Directory-listing demo buffers (dir-ops Part A): send a `File::ReadDir` and receive its
/// reply on an open directory-handle channel.
static mut DIR_SEND: [u8; 4096] = [0; 4096];
static mut DIR_RECV: [u8; 4096] = [0; 4096];
static mut DIR_XFER: [u64; 8] = [0; 8];
static mut DIR_XCOUNT: usize = 0;
/// Transferred-handle out-array for recv (always empty in the demo).
static mut HBUF: [u64; 8] = [0; 8];
/// Recv'd handle-count out-param.
static mut RECV_COUNT: usize = 0;
static mut WORKER_ARGS: ThreadArgs = ThreadArgs {
    entry: 0,
    user_sp: 0,
    arg0: 0,
    class: 0, // TimeShared
    rt_priority: 0,
    nice: 0,
    cpu_affinity: 0, // no restriction
    _reserved: [0; 36],
};
static mut WORKER_REGS: RegisterValues = RegisterValues { regs: [0; 18] };

// --- Userspace-server forwarding demo (slice 7 Part 3) ----------------------
/// The kernel end of the forwarding channel (bound at `/fs` as a Userspace
/// Server) and the server end (this process recvs requests + replies on it).
static mut FWD_KEND: u64 = 0;
static mut FWD_SEND: u64 = 0;
/// Recv buffer for the kernel's forwarded `Namespace::Resolve` request.
static mut FWD_REQ: [u8; 4096] = [0; 4096];
static mut FWD_REQ_H: [u64; 8] = [0; 8];
static mut FWD_REQ_COUNT: usize = 0;
/// Reply message (rsproto reply in the IPC payload; the MemoryObject in handles).
static mut FWD_REPLY: [u8; 4096] = [0; 4096];
static mut FWD_REPLY_H: [u64; 8] = [0; 8];

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
    // libos::thread_create returns an owning Handle<Thread> (closed on drop at the end
    // of this function, replacing the explicit close below).
    // SAFETY: WORKER_ARGS is our static; we exclusively initialize it, then hand a
    // shared reference to the wrapper.
    let worker = match unsafe {
        WORKER_ARGS.entry = worker_fault as *const () as usize as u64;
        WORKER_ARGS.user_sp = stack_top;
        WORKER_ARGS.arg0 = 0;
        thread_create(&*(&raw const WORKER_ARGS))
    } {
        Ok(w) => w,
        Err(_) => {
            kprint(b"parent: thread_create FAIL\n");
            exit(1);
        }
    };
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
        syscall4(SYS_THREAD_GET_REGISTERS, worker.raw().0, (&raw mut WORKER_REGS) as u64, 0, 0)
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
        syscall4(SYS_EXCEPTION_RESUME, worker.raw().0, DISPOSITION_TERMINATE, 7, 0)
    };
    if er != 0 {
        kprint(b"parent: exception_resume FAIL\n");
        exit(1);
    }
    // The worker is not this process's last thread (we are still running), so
    // its termination produces no `ChildExited`. Drop our handle to it.
    // SAFETY: closing our own handle.
    // (worker Handle<Thread> closes on drop at function end — no explicit close)
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
fn ns_demo() {
    kprint(b"parent: ns-demo start (fresh namespace)\n");

    // (a) sys_ns_create: a fresh, full-rights namespace to bind into. A process
    //     binds into namespaces it owns; its inherited root namespace is
    //     LOOKUP-only (sandbox-by-construction), so the demo uses this one — which
    //     works whether parent is pid 1 or a child of init.
    // SAFETY: register-only syscall.
    let ns = unsafe { syscall1(SYS_NS_CREATE, 0) };
    if ns < 0 {
        kprint(b"parent: ns_create FAIL\n");
        return;
    }
    let ns = ns as u64;
    kprint(b"parent: ns_create ok\n");

    // (b) Create a MemoryObject to bind as a direct-handle resource.
    // SAFETY: register-only syscall.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if mem < 0 {
        kprint(b"parent: ns-demo mem create FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, ns) };
        return;
    }
    let mem = mem as u64;

    // (c) bind the MemoryObject at "/store" in the fresh namespace — via libos's typed
    // Namespace::bind. Gated by the BIND handle right (below) *and* the BIND_NAMESPACE
    // syscap (kernel-side); parent holds both (init grants it BIND_NAMESPACE). The raw
    // `path` byte slice is still used by the lookup/unbind steps below.
    let path = b"/store";
    // SAFETY: `ns` is parent's live namespace handle; borrow is non-owning (won't close it).
    let ns_h = unsafe { Handle::<Namespace, NsMutable>::borrow(RawHandle(ns), Rights::BIND) };
    if ns_h.bind("/store", RawHandle(mem)).is_err() {
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
            ns,
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
        syscall4(SYS_NS_UNBIND, ns, path.as_ptr() as u64, path.len() as u64, 0)
    };
    if ur != 0 {
        kprint(b"parent: ns_unbind FAIL\n");
        return;
    }
    // SAFETY: valid path pointer + handle.
    let po2 = unsafe {
        syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, RIGHT_MAP_READ)
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

    // Close the demo handles we still hold (resolved + the original mem + the POs
    // + the fresh namespace).
    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, po as u64);
        syscall1(SYS_HANDLE_CLOSE, po2 as u64);
        syscall1(SYS_HANDLE_CLOSE, resolved);
        syscall1(SYS_HANDLE_CLOSE, mem);
        syscall1(SYS_HANDLE_CLOSE, ns);
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
    // loop avoids inlined `[u8; N]` equality, which mis-compiles to an infinite loop
    // on this freestanding `-sse,+soft-float` target (see decision log 2026-06-22).
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

/// Kernel-server demo: resolve `/dev/entropy` from the **root namespace** (`rsi`)
/// the kernel bound at boot, and read from the handle it hands back. Unlike
/// `entropy_demo` (which mints a token with `sys_entropy_create`), this obtains
/// the `EntropyObject` purely through a namespace lookup that routes to an
/// in-kernel resource server — proving boot-binding + `KernelServer` dispatch
/// (`sys_ns_lookup` → server → installed handle → `IoResult.result`) end-to-end.
fn dev_entropy_lookup_demo(root_ns: u64) {
    kprint(b"parent: /dev/entropy lookup-demo start\n");
    let path = b"/dev/entropy";
    // sys_ns_lookup → PendingOperation; the resolved handle arrives in IoResult.
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe {
        syscall4(SYS_NS_LOOKUP, root_ns, path.as_ptr() as u64, path.len() as u64, RIGHT_READ)
    };
    if po < 0 {
        kprint(b"parent: /dev/entropy lookup FAIL\n");
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
        kprint(b"parent: /dev/entropy lookup wait unexpected\n");
        return;
    }

    // Use the resolved EntropyObject handle: read 32 CSPRNG bytes (0 = synchronous
    // fill; the pool seeds at boot).
    let mut a = [0u8; 32];
    // SAFETY: valid writable 32-byte buffer; `len` ≤ ENTROPY_READ_MAX.
    let r = unsafe { syscall4(SYS_ENTROPY_READ, resolved, (&raw mut a) as u64, 32, 0) };
    if r != 0 {
        kprint(b"parent: /dev/entropy read not synchronous\n");
        unsafe { syscall1(SYS_HANDLE_CLOSE, resolved) };
        return;
    }
    let first = u64::from_le_bytes([a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7]]);
    kprint(b"parent: /dev/entropy resolved+read ok bytes[0..8]=");
    kprint_hex(first);
    kprint(b"\n");

    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, resolved);
        syscall1(SYS_HANDLE_CLOSE, po as u64);
    }
}

/// Resolve `path` in namespace `ns` requesting `rights`, wait the PO, and return
/// `(status, resolved_handle)` (`IoResult`: status at bytes 8..12, handle at
/// 16..24). `status == 0` with a non-zero handle is success. Closes the PO; the
/// caller owns `resolved_handle`.
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

/// `sys_handle_stat` the handle and return whether its object-type field equals
/// `want` (`HandleInfo`: rights `u64` @0, object_type `u32` @8, generation @12).
fn stat_is_type(h: u64, want: u32) -> bool {
    let mut info = [0u8; 16];
    // SAFETY: valid 16-byte writable `HandleInfo` out-param.
    let r = unsafe { syscall4(SYS_HANDLE_STAT, h, (&raw mut info) as u64, 0, 0) };
    if r != 0 {
        return false;
    }
    u32::from_le_bytes([info[8], info[9], info[10], info[11]]) == want
}

/// `/proc/self` demo: resolve the caller's own resources from the **root
/// namespace** (`rsi`) and prove each handle. No ambient authority — these resolve
/// only because the kernel bound `/proc/self/*` into pid 1's root namespace, and
/// each returns strictly *this* caller's own object (derived from syscall context).
fn proc_self_demo(root_ns: u64) {
    kprint(b"parent: /proc/self demo start\n");

    // /proc/self/process — request INSPECT; stat the handle, assert it's a Process.
    let (st, ph) = ns_lookup_wait(root_ns, b"/proc/self/process", RIGHT_INSPECT);
    if st != 0 || ph == 0 || !stat_is_type(ph, KOBJ_PROCESS) {
        kprint(b"parent: /proc/self/process FAIL\n");
        return;
    }
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, ph) };
    kprint(b"parent: /proc/self/process ok (own Process handle)\n");

    // /proc/self/thread — likewise, assert it's a Thread.
    let (st, th) = ns_lookup_wait(root_ns, b"/proc/self/thread", RIGHT_INSPECT);
    if st != 0 || th == 0 || !stat_is_type(th, KOBJ_THREAD) {
        kprint(b"parent: /proc/self/thread FAIL\n");
        return;
    }
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, th) };
    kprint(b"parent: /proc/self/thread ok (own Thread handle)\n");

    // /proc/self/namespace — request LOOKUP; assert it's a Namespace, then USE it:
    // resolve /dev/entropy *through* the returned handle (proves a live,
    // LOOKUP-capable namespace identical to our root).
    let (st, nh) = ns_lookup_wait(root_ns, b"/proc/self/namespace", RIGHT_LOOKUP | RIGHT_INSPECT);
    if st != 0 || nh == 0 || !stat_is_type(nh, KOBJ_NAMESPACE) {
        kprint(b"parent: /proc/self/namespace FAIL\n");
        return;
    }
    let (st2, eh) = ns_lookup_wait(nh, b"/dev/entropy", RIGHT_READ);
    if st2 != 0 || eh == 0 {
        kprint(b"parent: /proc/self/namespace lookup-through FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, nh) };
        return;
    }
    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, eh);
        syscall1(SYS_HANDLE_CLOSE, nh);
    }
    kprint(b"parent: /proc/self/namespace ok (resolved /dev/entropy through it)\n");
}

/// Initramfs demo: resolve `/initramfs/etc/init.toml` from the root namespace
/// (the kernel bound `/initramfs` at boot to the in-kernel CPIO server), map the
/// returned read-only `MemoryObject`, and print its first bytes — proving the
/// Limine module + CPIO parser + `/initramfs` server end-to-end, before Init
/// exists. (This is Init's real job in slice 5+; here it's just a substrate check.)
fn initramfs_demo(root_ns: u64) {
    kprint(b"parent: /initramfs demo start\n");
    let (st, mem) = ns_lookup_wait(root_ns, b"/initramfs/etc/init.toml", RIGHT_MAP_READ);
    if st != 0 || mem == 0 {
        kprint(b"parent: /initramfs/etc/init.toml lookup FAIL\n");
        return;
    }
    // Map the returned MemoryObject read-only and read its first bytes.
    // SAFETY: register-only syscall; `mem` is a MemoryObject handle with MAP_READ.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, PAGE, RIGHT_MAP_READ) };
    if addr < 0 {
        kprint(b"parent: /initramfs map FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
        return;
    }
    // SAFETY: `addr` is a page the kernel mapped MAP_READ holding the file's bytes;
    // read the first 16 in bounds (init.toml is far longer).
    let head = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, 16) };
    kprint(b"parent: /initramfs/etc/init.toml -> \"");
    kprint(head);
    kprint(b"...\"\n");
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
}

/// Find the first occurrence of `key` in `text` and parse the ASCII decimal
/// run that follows it. `None` if the key is absent or not followed by a digit.
fn parse_field(text: &[u8], key: &[u8]) -> Option<u64> {
    let start = text.windows(key.len()).position(|w| w == key)? + key.len();
    let mut n: u64 = 0;
    let mut any = false;
    for &b in &text[start..] {
        if !b.is_ascii_digit() {
            break;
        }
        any = true;
        n = n.wrapping_mul(10).wrapping_add((b - b'0') as u64);
    }
    if any { Some(n) } else { None }
}

/// Count the `cpu=` rows in a `/proc/sched/stats` snapshot whose `switches`
/// counter is nonzero — the clause-3 "CPUs visibly active" measure.
fn cpus_with_switches(text: &[u8]) -> u64 {
    let mut n = 0;
    for line in text.split(|&b| b == b'\n') {
        if line.starts_with(b"cpu=") && parse_field(line, b"switches=").is_some_and(|v| v > 0) {
            n += 1;
        }
    }
    n
}

/// Block this thread for `ms` milliseconds on a one-shot timer (create → arm →
/// `sys_wait`). Best-effort: on any failure it just returns (the caller's retry
/// loop is attempt-bounded either way).
fn timer_sleep_ms(ms: u64) {
    // SAFETY: a valid syscall; returns a handle (>= 0) or a negative KError.
    let th = unsafe { syscall1(SYS_TIMER_CREATE, 0) };
    if th < 0 {
        return;
    }
    let th = th as u64;
    // SAFETY: CLOCK_BUF is a writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    let now = unsafe { (&raw const CLOCK_BUF).read() };
    let fire_at = now + ms * 1_000_000;
    // SAFETY: arming our own timer handle (absolute monotonic, one-shot).
    unsafe { syscall4(SYS_TIMER_SET, th, fire_at, 0, 0) };
    // SAFETY: WAIT_HANDLES / WAIT_RESULTS are valid writable buffers; generous
    // outer deadline so the timer (not the deadline) normally wakes us.
    unsafe {
        WAIT_HANDLES[0] = th;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            fire_at + 1_000_000_000,
        );
    }
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, th) };
}

/// Concurrent-exit stress (substrate-hardening Part F, decision log 2026-07-21):
/// spawn waves of immediately-exiting children (`child` role 2, no handles) and
/// reap each wave, so process teardowns — kernel-stack frees, their TLB
/// shootdowns, reap sweeps — race each other and the concurrently-running login
/// chain across all 4 CPUs. Regression cover for the review's F1 (each exit's
/// reap can initiate a shootdown from an IF-masked syscall context; waves make
/// initiators collide), F5 (reap vs a mid-switch-out sibling), and F11 (the
/// reap lists' reserve discipline across repeated drains). A lost exit hangs
/// the wave (→ the selftest wall-clock timeout fails the run); a crash exits
/// nonzero (→ init's fail path).
fn exit_storm_demo(root_ns: u64, notif: u64) {
    kprint(b"parent: exit-storm start\n");
    let (st, img) = ns_lookup_wait(root_ns, b"/initramfs/sbin/child", RIGHT_MAP_READ);
    if st != 0 || img == 0 {
        kprint(b"parent: exit-storm image lookup FAIL\n");
        exit(1);
    }
    const ROUNDS: usize = 6;
    const WAVE: usize = 3;
    for _ in 0..ROUNDS {
        let mut procs = [const { None }; WAVE];
        for slot in procs.iter_mut() {
            // SAFETY: STORM_SPAWN is a valid writable arg block, exclusively
            // read here (image set just above; no handle grants).
            let spawned = unsafe {
                STORM_SPAWN.image = img;
                spawn(&*(&raw const STORM_SPAWN))
            };
            match spawned {
                Ok(p) => *slot = Some(p),
                Err(_) => {
                    kprint(b"parent: exit-storm spawn FAIL\n");
                    exit(1);
                }
            }
        }
        // Drain this wave's ChildExited notifications (other kinds ignored).
        let mut got = 0;
        while got < WAVE {
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
                kprint(b"parent: exit-storm wait FAIL\n");
                exit(1);
            }
            loop {
                // SAFETY: NOTIF is a valid 64-byte writable out-param.
                let r = unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) };
                if r != 0 {
                    break; // WouldBlock: drained
                }
                // SAFETY: the kernel wrote a 64-byte Notification into NOTIF.
                if unsafe { (&raw const NOTIF.kind).read() } == KIND_CHILD_EXITED {
                    got += 1;
                }
            }
        }
        // `procs` drops here: closing each process handle reaps the wave while
        // the next wave's spawns run — teardown and spawn race by design.
    }
    // SAFETY: closing our own image handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, img) };
    kprint(b"parent: exit-storm ok (18 exits reaped)\n");
}

/// **Directory listing over the direct-RPC transport** (dir-ops Part A). Opens `/system`
/// as a directory handle (`sys_ns_lookup` resolves a directory path to a session channel —
/// `OBJECT_KIND_CHANNEL`), then issues `File::ReadDir` on that channel, following the
/// cursor across replies, and confirms the known entry `current-generation` is listed.
/// Proves the whole transport end to end: endpoint acquisition, the session channel,
/// name-addressed enumeration, and reply correlation. A failure exits non-zero (init's
/// fail path); like the other early demos it runs before the login chain adjudicates.
fn dir_list_demo(root_ns: u64) {
    kprint(b"parent: dir-list demo start\n");
    // Open the directory: resolving a directory path yields a session channel (SEND|RECV).
    let (st, dir_ch) = ns_lookup_wait(root_ns, b"/system", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
    if st != 0 || dir_ch == 0 {
        kprint(b"parent: dir-list open FAIL\n");
        exit(1);
    }

    let mut cursor = 0u64;
    let mut saw_currentgen = false;
    let mut rounds = 0u32;
    loop {
        rounds += 1;
        if rounds > 64 {
            kprint(b"parent: dir-list runaway (cursor did not terminate)\n");
            exit(1);
        }
        // Build File::ReadDir{cursor} into the send buffer's payload region (offset 24).
        // SAFETY: DIR_SEND is a valid writable buffer; the rsproto body is bounded.
        let ok = unsafe {
            let mut body = [0u8; 8];
            let bn = match librsproto::file::read_dir_request(&mut body, cursor) {
                Some(n) => n,
                None => return_fail(b"parent: dir-list request build FAIL\n"),
            };
            let reply = core::slice::from_raw_parts_mut(((&raw mut DIR_SEND) as *mut u8).add(24), 4096 - 24);
            match librsproto::encode(reply, librsproto::OP_FILE_READ_DIR, 0, 0, &body[..bn], 0) {
                Some(rn) => {
                    DIR_SEND[4..8].copy_from_slice(&(rn as u32).to_le_bytes());
                    DIR_SEND[8] = 0;
                    true
                }
                None => false,
            }
        };
        if !ok {
            kprint(b"parent: dir-list encode FAIL\n");
            exit(1);
        }
        // Send on the directory channel.
        // SAFETY: valid endpoint + message; no transferred handles.
        let sr = unsafe {
            syscall5(SYS_CHANNEL_SEND, dir_ch, (&raw const DIR_SEND) as u64, 0, 0, SENDMODE_NOBLOCK)
        };
        if sr != 0 {
            kprint(b"parent: dir-list send FAIL\n");
            exit(1);
        }
        // Wait for + receive the reply on the same channel.
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid single-waiter buffers.
        let waited = unsafe {
            WAIT_HANDLES[0] = dir_ch;
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
        };
        if waited != 1 {
            kprint(b"parent: dir-list wait FAIL\n");
            exit(1);
        }
        // SAFETY: valid recv out-params.
        let rr = unsafe {
            syscall4(SYS_CHANNEL_RECV, dir_ch, (&raw mut DIR_RECV) as u64, (&raw mut DIR_XFER) as u64, (&raw mut DIR_XCOUNT) as u64)
        };
        if rr != 0 {
            kprint(b"parent: dir-list recv FAIL\n");
            exit(1);
        }
        // Decode the reply and scan its entries.
        // SAFETY: DIR_RECV holds the reply; the payload slice is bounded.
        let next_cursor = unsafe {
            let payload_len = u32::from_le_bytes([DIR_RECV[4], DIR_RECV[5], DIR_RECV[6], DIR_RECV[7]]) as usize;
            let payload = core::slice::from_raw_parts(((&raw const DIR_RECV) as *const u8).add(24), payload_len.min(4096 - 24));
            let m = match librsproto::decode(payload) {
                Ok(m) => m,
                Err(_) => return_fail(b"parent: dir-list decode FAIL\n"),
            };
            if m.is_error() {
                return_fail(b"parent: dir-list error reply\n");
            }
            let (hdr, iter) = match librsproto::file::parse_read_dir_reply(m.body) {
                Some(x) => x,
                None => return_fail(b"parent: dir-list parse FAIL\n"),
            };
            for e in iter {
                if e.name == b"current-generation" {
                    saw_currentgen = true;
                }
            }
            hdr.next_cursor
        };
        if next_cursor == 0 {
            break;
        }
        cursor = next_cursor;
    }

    // SAFETY: closing our own channel handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, dir_ch) };
    if saw_currentgen {
        kprint(b"parent: dir-list ok (/system lists current-generation)\n");
    } else {
        kprint(b"parent: dir-list FAIL (current-generation not found)\n");
        exit(1);
    }
}

/// Print `msg` and exit non-zero — a `-> !` helper so the demo's `match`/closure arms can
/// bail without an awkward control-flow dance.
fn return_fail(msg: &[u8]) -> ! {
    kprint(msg);
    exit(1)
}

/// **Directory mutation over the direct-RPC transport** (dir-ops Part B). On the same kind
/// of open directory handle as `dir_list_demo`, exercises the name-addressed mutations end
/// to end: mkdir a temp subdir, confirm it appears, rename it, confirm the rename, then
/// rmdir it and confirm it is gone. Each op is a single request/reply on the session
/// channel; the handle is bound to `/system`, so the names can only ever touch `/system`.
fn dir_mutate_demo(root_ns: u64) {
    kprint(b"parent: dir-mutate demo start\n");
    let (st, dir_ch) = ns_lookup_wait(root_ns, b"/system", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
    if st != 0 || dir_ch == 0 {
        kprint(b"parent: dir-mutate open FAIL\n");
        exit(1);
    }
    // mkdir nx-tmp → confirm it appears (a ReadDir on the same session).
    let mut body = [0u8; 32];
    let n = librsproto::file::name_request(&mut body, b"nx-tmp").unwrap();
    if !session_mutate(dir_ch, librsproto::OP_FILE_MKDIR, &body[..n]) {
        kprint(b"parent: mkdir FAIL\n");
        exit(1);
    }
    if !session_dir_contains(dir_ch, b"nx-tmp") {
        kprint(b"parent: mkdir not visible\n");
        exit(1);
    }
    // rename nx-tmp → nx-tmp2 → confirm the old name is gone and the new one present.
    let mut rbody = [0u8; 48];
    let rn = librsproto::file::rename_request(&mut rbody, b"nx-tmp", b"nx-tmp2").unwrap();
    if !session_mutate(dir_ch, librsproto::OP_FILE_RENAME, &rbody[..rn]) {
        kprint(b"parent: rename FAIL\n");
        exit(1);
    }
    if session_dir_contains(dir_ch, b"nx-tmp") || !session_dir_contains(dir_ch, b"nx-tmp2") {
        kprint(b"parent: rename not applied\n");
        exit(1);
    }
    // rmdir nx-tmp2 → confirm it is gone.
    let mut dbody = [0u8; 32];
    let dn = librsproto::file::name_request(&mut dbody, b"nx-tmp2").unwrap();
    if !session_mutate(dir_ch, librsproto::OP_FILE_RMDIR, &dbody[..dn]) {
        kprint(b"parent: rmdir FAIL\n");
        exit(1);
    }
    if session_dir_contains(dir_ch, b"nx-tmp2") {
        kprint(b"parent: rmdir not applied\n");
        exit(1);
    }
    // SAFETY: closing our own channel handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, dir_ch) };
    kprint(b"parent: dir-mutate ok (mkdir + rename + rmdir, each ReadDir-verified)\n");
}

/// Send one mutation op (`body` already built) on the directory channel and await its
/// reply; returns `true` on a non-error reply. Exits the demo on a transport failure.
fn session_mutate(dir_ch: u64, op: u16, body: &[u8]) -> bool {
    // SAFETY: DIR_SEND is a valid writable buffer; the rsproto message is bounded.
    let sent = unsafe {
        let region = core::slice::from_raw_parts_mut(((&raw mut DIR_SEND) as *mut u8).add(24), 4096 - 24);
        match librsproto::encode(region, op, 0, 0, body, 0) {
            Some(rn) => {
                DIR_SEND[4..8].copy_from_slice(&(rn as u32).to_le_bytes());
                DIR_SEND[8] = 0;
                syscall5(SYS_CHANNEL_SEND, dir_ch, (&raw const DIR_SEND) as u64, 0, 0, SENDMODE_NOBLOCK) == 0
            }
            None => false,
        }
    };
    if !sent {
        kprint(b"parent: dir-mutate send FAIL\n");
        exit(1);
    }
    // SAFETY: single-waiter buffers.
    let waited = unsafe {
        WAIT_HANDLES[0] = dir_ch;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    if waited != 1 {
        kprint(b"parent: dir-mutate wait FAIL\n");
        exit(1);
    }
    // SAFETY: valid recv out-params.
    let rr = unsafe {
        syscall4(SYS_CHANNEL_RECV, dir_ch, (&raw mut DIR_RECV) as u64, (&raw mut DIR_XFER) as u64, (&raw mut DIR_XCOUNT) as u64)
    };
    if rr != 0 {
        kprint(b"parent: dir-mutate recv FAIL\n");
        exit(1);
    }
    // SAFETY: DIR_RECV holds the reply; the payload slice is bounded.
    unsafe {
        let payload_len = u32::from_le_bytes([DIR_RECV[4], DIR_RECV[5], DIR_RECV[6], DIR_RECV[7]]) as usize;
        let payload = core::slice::from_raw_parts(((&raw const DIR_RECV) as *const u8).add(24), payload_len.min(4096 - 24));
        match librsproto::decode(payload) {
            Ok(m) => !m.is_error(),
            Err(_) => false,
        }
    }
}


/// Whether the bound directory currently lists an entry named `name` (drains the ReadDir
/// cursor across replies). Exits the demo on a transport failure.
fn session_dir_contains(dir_ch: u64, name: &[u8]) -> bool {
    let mut cursor = 0u64;
    let mut rounds = 0u32;
    loop {
        rounds += 1;
        if rounds > 64 {
            kprint(b"parent: dir-contains runaway\n");
            exit(1);
        }
        // Build + send File::ReadDir{cursor}.
        // SAFETY: DIR_SEND is a valid writable buffer; the rsproto body is bounded.
        let sent = unsafe {
            let mut b = [0u8; 8];
            let bn = librsproto::file::read_dir_request(&mut b, cursor).unwrap();
            let region = core::slice::from_raw_parts_mut(((&raw mut DIR_SEND) as *mut u8).add(24), 4096 - 24);
            match librsproto::encode(region, librsproto::OP_FILE_READ_DIR, 0, 0, &b[..bn], 0) {
                Some(rn) => {
                    DIR_SEND[4..8].copy_from_slice(&(rn as u32).to_le_bytes());
                    DIR_SEND[8] = 0;
                    syscall5(SYS_CHANNEL_SEND, dir_ch, (&raw const DIR_SEND) as u64, 0, 0, SENDMODE_NOBLOCK) == 0
                }
                None => false,
            }
        };
        if !sent {
            kprint(b"parent: dir-contains send FAIL\n");
            exit(1);
        }
        // SAFETY: single-waiter buffers.
        let waited = unsafe {
            WAIT_HANDLES[0] = dir_ch;
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
        };
        if waited != 1 {
            kprint(b"parent: dir-contains wait FAIL\n");
            exit(1);
        }
        // SAFETY: valid recv out-params.
        let rr = unsafe {
            syscall4(SYS_CHANNEL_RECV, dir_ch, (&raw mut DIR_RECV) as u64, (&raw mut DIR_XFER) as u64, (&raw mut DIR_XCOUNT) as u64)
        };
        if rr != 0 {
            kprint(b"parent: dir-contains recv FAIL\n");
            exit(1);
        }
        // SAFETY: DIR_RECV holds the reply; the payload slice is bounded.
        let (found, next) = unsafe {
            let payload_len = u32::from_le_bytes([DIR_RECV[4], DIR_RECV[5], DIR_RECV[6], DIR_RECV[7]]) as usize;
            let payload = core::slice::from_raw_parts(((&raw const DIR_RECV) as *const u8).add(24), payload_len.min(4096 - 24));
            let m = match librsproto::decode(payload) { Ok(m) => m, Err(_) => return false };
            if m.is_error() { return false; }
            let (hdr, iter) = match librsproto::file::parse_read_dir_reply(m.body) { Some(x) => x, None => return false };
            let mut f = false;
            for e in iter {
                if e.name == name { f = true; }
            }
            (f, hdr.next_cursor)
        };
        if found {
            return true;
        }
        if next == 0 {
            return false;
        }
        cursor = next;
    }
}

/// **Hardware floating point, end to end in ring 3** (Phase 4 FP enablement Part D;
/// decision log 2026-07-21).
///
/// Spawns `FP_WORKERS` copies of `child` role 3, each with a different seed, and requires
/// every one to exit `0`. A worker checks its own `f64` arithmetic bit-exactly against
/// integer math, round-trips values across syscalls and preemption, and — when the OS has
/// enabled `YMM` state — cross-checks an `#[target_feature(enable = "avx2")]` SIMD path
/// against the scalar one. See `child::run_fp_worker` for what each exit code means.
///
/// Running several concurrently is the point: the workers hold *different* live FP state
/// on different CPUs, so a context switch that cross-wired two processes' register files
/// would show up as one worker seeing another's values. That is the ring-3 counterpart to
/// the kernel-thread isolation the boot selftest proves — this one goes through real
/// compiler-generated float, real syscalls, and a real address-space switch.
///
/// A nonzero exit code fails the run (`exit(1)` → init's fail path → FAIL verdict).
fn fp_hardfloat_demo(root_ns: u64, notif: u64) {
    const FP_WORKERS: usize = 3;
    // No "start" banner: session-mgr owns the PASS verdict and races this process, so on
    // a fast (KVM) boot the run can be adjudicated while these workers are still going.
    // Announcing a start we might not finish reads like a hang; staying silent until
    // there is a result is honest — the *guarantee* lives in session-mgr's `fp_gate`,
    // checked synchronously at the verdict. This demo is breadth on top of that.
    let (st, img) = ns_lookup_wait(root_ns, b"/initramfs/sbin/child", RIGHT_MAP_READ);
    if st != 0 || img == 0 {
        kprint(b"parent: hard-float image lookup FAIL\n");
        exit(1);
    }
    let mut procs = [const { None }; FP_WORKERS];
    for (i, slot) in procs.iter_mut().enumerate() {
        // SAFETY: FP_SPAWN is a valid writable arg block, exclusively written and read
        // here (image + seeded arg0; no handle grants).
        let spawned = unsafe {
            FP_SPAWN.image = img;
            // Role 3 in the low byte, this worker's seed above it.
            FP_SPAWN.arg0 = 3 | ((i as u64 + 1) << 8);
            spawn(&*(&raw const FP_SPAWN))
        };
        match spawned {
            Ok(p) => *slot = Some(p),
            Err(_) => {
                kprint(b"parent: hard-float spawn FAIL\n");
                exit(1);
            }
        }
    }
    // Collect one ChildExited per worker; any nonzero code is a real FP failure.
    let mut got = 0;
    while got < FP_WORKERS {
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
            kprint(b"parent: hard-float wait FAIL\n");
            exit(1);
        }
        loop {
            // SAFETY: NOTIF is a valid 64-byte writable out-param.
            let r = unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) };
            if r != 0 {
                break; // WouldBlock: drained
            }
            // SAFETY: the kernel wrote a 64-byte Notification into NOTIF.
            let (kind, b) =
                unsafe { ((&raw const NOTIF.kind).read(), (&raw const NOTIF.body).read()) };
            if kind == KIND_CHILD_EXITED {
                let code = i32::from_le_bytes([b[8], b[9], b[10], b[11]]);
                if code != 0 {
                    kprint(b"parent: hard-float worker FAILED code=");
                    kprint_u64(code as u64);
                    kprint(b"\n");
                    exit(1);
                }
                got += 1;
            }
        }
    }
    // SAFETY: closing our own image handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, img) };
    kprint(b"parent: hard-float ok (3 workers, f64 + simd verified in ring 3)\n");
}

/// `/proc/self/status` + `/proc/sched/stats` demo — the Phase 3 **clause 3**
/// milestone check ("two CPUs visibly active via `/proc`"). **Verdict-gated**:
/// any failure here exits nonzero, which init (under `test-harness`) turns into
/// a FAIL verdict — an SMP-liveness regression fails `xtask test-qemu` outright.
///
/// Both surfaces are synthesized read-only `MemoryObject` text snapshots (the
/// capture → format → synthesize discipline; see
/// `docs/architecture/scheduler.md` § "The stats surface"):
///
/// 1. `/proc/self/status` — map it and parse the `pid=`/`tid=` rows; ours must
///    be a real spawned identity (pid ≥ 2 — init is 1 — and tid ≥ 1).
/// 2. `/proc/sched/stats` — each lookup returns a *fresh* snapshot; require
///    **≥ 2 CPUs with `switches` > 0**. Runs last in the demo chain (the
///    spawn/IPC demos and the concurrent login chain have exercised multiple
///    CPUs by now); counters only grow, so retry with a 100 ms timer sleep
///    (up to ~5 s) before declaring the run dead.
fn sched_stats_demo(root_ns: u64) {
    kprint(b"parent: sched-stats demo start\n");

    // --- /proc/self/status: the caller's own numeric identity.
    let (st, mem) = ns_lookup_wait(root_ns, b"/proc/self/status", RIGHT_MAP_READ);
    if st != 0 || mem == 0 {
        kprint(b"parent: /proc/self/status lookup FAIL\n");
        exit(1);
    }
    // SAFETY: register-only syscall; `mem` is a MemoryObject handle with MAP_READ.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, PAGE, RIGHT_MAP_READ) };
    if addr < 0 {
        kprint(b"parent: /proc/self/status map FAIL\n");
        exit(1);
    }
    // SAFETY: `addr` is a page the kernel mapped MAP_READ holding the status
    // text (zero-padded to the page).
    let text = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, PAGE as usize) };
    let pid = parse_field(text, b"pid=").unwrap_or(0);
    let tid = parse_field(text, b"tid=").unwrap_or(0);
    if pid < 2 || tid < 1 {
        kprint(b"parent: /proc/self/status content FAIL\n");
        exit(1);
    }
    kprint(b"parent: /proc/self/status ok pid=");
    kprint_u64(pid);
    kprint(b" tid=");
    kprint_u64(tid);
    kprint(b"\n");
    // SAFETY: unmapping the page we mapped above (`text` is not used past here);
    // closing our own handle.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, addr as u64, 0);
        syscall1(SYS_HANDLE_CLOSE, mem);
    }

    // --- /proc/sched/stats: >= 2 CPUs with switches > 0.
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let (st, mem) = ns_lookup_wait(root_ns, b"/proc/sched/stats", RIGHT_MAP_READ);
        if st != 0 || mem == 0 {
            kprint(b"parent: /proc/sched/stats lookup FAIL\n");
            exit(1);
        }
        // SAFETY: register-only syscall; `mem` is a MemoryObject handle with MAP_READ.
        let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, PAGE, RIGHT_MAP_READ) };
        if addr < 0 {
            kprint(b"parent: /proc/sched/stats map FAIL\n");
            exit(1);
        }
        // SAFETY: `addr` is a page the kernel mapped MAP_READ holding the
        // snapshot text (zero-padded to the page).
        let text = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, PAGE as usize) };
        let active = cpus_with_switches(text);
        let done = active >= 2;
        if done {
            // Echo the winning snapshot into the boot log (grep-visible
            // evidence of the milestone, alongside the machine-checked gate).
            let len = text.iter().position(|&b| b == 0).unwrap_or(text.len());
            kprint(b"parent: /proc/sched/stats ok (");
            kprint_u64(active);
            kprint(b" CPUs with switches>0):\n");
            kprint(&text[..len]);
        }
        // SAFETY: unmapping the page mapped above (`text` is not used past
        // here); closing our own handle (each lookup minted a fresh snapshot).
        unsafe {
            syscall2(SYS_MEMORY_UNMAP, addr as u64, 0);
            syscall1(SYS_HANDLE_CLOSE, mem);
        }
        if done {
            return;
        }
        if attempt >= 50 {
            kprint(b"parent: /proc/sched/stats FAIL (<2 CPUs with switches>0)\n");
            exit(1);
        }
        timer_sleep_ms(100);
    }
}

/// Wait on a single `PendingOperation` handle and return its completion
/// `(status, result)` from the `IoResult` (status at bytes 8..12, result at
/// 16..24). Closes `po`.
fn po_wait(po: u64) -> (i32, u64) {
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
    let status = unsafe {
        i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]])
    };
    let result = unsafe {
        u64::from_le_bytes([
            WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
            WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
        ])
    };
    // SAFETY: closing our own PO handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
    if waited != 1 { (-1, 0) } else { (status, result) }
}

/// Resolve `path` to a block device, `sys_io_submit` a 512-byte read of its
/// sector 0 into a mapped buffer, wait, and return the 16-bit value at offset 510
/// (the boot signature `0xAA55`), or `-1` on any failure. The full userspace
/// block-I/O path: lookup → `sys_io_submit` → `sys_wait` → data.
fn read_block_sector0(root_ns: u64, path: &[u8]) -> i32 {
    let (st, dev) = ns_lookup_wait(root_ns, path, RIGHT_READ);
    if st != 0 || dev == 0 {
        return -1;
    }
    // SAFETY: register-only syscall.
    let buf = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if buf < 0 {
        unsafe { syscall1(SYS_HANDLE_CLOSE, dev) };
        return -1;
    }
    // The controller DMAs the sector into the buffer (MAP_WRITE); MAP_READ to verify.
    // SAFETY: `buf` is a fresh MemoryObject handle with full MAP rights.
    let addr = unsafe {
        syscall4(SYS_MEMORY_MAP, buf as u64, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE)
    };
    let mut sig: i32 = -1;
    if addr >= 0 {
        let op = IoOp {
            opcode: IO_OPCODE_READ,
            flags: 0,
            buffer: buf as u64,
            buf_offset: 0,
            offset: 0,
            length: 512,
        };
        // SAFETY: `dev` is a block DeviceNode with READ; `&op` is a valid IoOp.
        let po = unsafe { syscall2(SYS_IO_SUBMIT, dev, (&op as *const IoOp) as u64) };
        if po >= 0 {
            let (status, result) = po_wait(po as u64);
            if status == 0 && result == 512 {
                // SAFETY: `addr` maps the 512 DMAed bytes; 510..512 in bounds.
                sig = unsafe { ((addr as u64 + 510) as *const u16).read_unaligned() } as i32;
            }
        }
    }
    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, buf as u64);
        syscall1(SYS_HANDLE_CLOSE, dev);
    }
    sig
}

/// Block-storage demo: read sector 0 of the whole disk (`/dev/blk/0`), of its
/// first GPT partition (`/dev/blk/1`, proving the partition layer rebases the
/// offset), and of the same partition under its stable `/dev/disk/by-partlabel`
/// name. Each verifies the `0x55AA` boot signature.
fn block_demo(root_ns: u64) {
    kprint(b"parent: /dev/blk demo start\n");
    report_block_read(root_ns, b"/dev/blk/0", b"parent: /dev/blk/0 (disk) read");
    report_block_read(root_ns, b"/dev/blk/1", b"parent: /dev/blk/1 (partition) read");
    report_block_read(
        root_ns,
        b"/dev/disk/by-partlabel/NITROX_ESP",
        b"parent: /dev/disk/by-partlabel/NITROX_ESP read",
    );
}

/// Read+verify one block path and log the outcome under `label`.
fn report_block_read(root_ns: u64, path: &[u8], label: &[u8]) {
    let sig = read_block_sector0(root_ns, path);
    kprint(label);
    if sig == 0xAA55 {
        kprint(b" OK (sector 0 boot sig 0x55AA)\n");
    } else if sig < 0 {
        kprint(b" FAIL\n");
    } else {
        kprint(b" OK (no 0x55AA sig)\n");
    }
}

/// Userspace-server forwarding demo (slice 7 Part 3): prove the kernel's
/// **transparent namespace forwarding** end to end, single-process. This process
/// plays both roles — the lookup *client* and the resource *server* — so the whole
/// loop is exercised without a second binary or a disk:
///
/// 1. create a channel pair; **bind one end at `/fs` as a Userspace Server** (the
///    kernel adopts it as the kernel forwarding endpoint);
/// 2. issue an async `sys_ns_lookup` of `/fs/hello` — the kernel forwards a
///    `Namespace::Resolve` (suffix `hello`) into our *other* endpoint and leaves
///    the lookup `PendingOperation` pending;
/// 3. recv that request, parse it with `librsproto` (proving the kernel's
///    hand-coded request matches the library codec), build a read-only
///    `MemoryObject` of `b"STUB\n"`, and **reply transferring it** — the kernel
///    completes the waiting lookup PO inline in our send;
/// 4. `sys_wait` the PO, map the resolved `MemoryObject`, and verify the content.
///
/// This isolates the highest-risk Part-3 mechanism (the kernel as an async IPC
/// client + cross-context handle install) behind a stub, before the real
/// `fs-server-ext4` process / ext4 disk exist (Parts 4–6).
fn forward_demo() {
    kprint(b"parent: userspace-server forwarding demo start\n");
    const CONTENT: &[u8] = b"STUB\n";

    // 1. Channel pair: one end becomes the kernel forwarding endpoint, the other
    //    is the end this process serves requests on.
    // SAFETY: FWD_KEND/FWD_SEND are valid writable out-params.
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut FWD_KEND) as u64, (&raw mut FWD_SEND) as u64, 4, 0)
    };
    if cr != 0 {
        kprint(b"parent: fwd channel create FAIL\n");
        return;
    }
    // SAFETY: the kernel wrote both endpoint handles.
    let (kend, send_end) = unsafe { ((&raw const FWD_KEND).read(), (&raw const FWD_SEND).read()) };

    // 2. Fresh namespace; bind the kernel end at /fs as a Userspace Server.
    let ns = unsafe { syscall1(SYS_NS_CREATE, 0) };
    if ns < 0 {
        kprint(b"parent: fwd ns create FAIL\n");
        return;
    }
    let ns = ns as u64;
    let mount = b"/fs";
    // SAFETY: valid path pointer + namespace/endpoint handles. Binding an
    // `IpcChannel` makes the kernel adopt it as a Userspace Server.
    let br = unsafe {
        syscall4(SYS_NS_BIND, ns, mount.as_ptr() as u64, mount.len() as u64, kend)
    };
    if br != 0 {
        kprint(b"parent: fwd bind FAIL\n");
        return;
    }

    // 3. Async lookup of /fs/hello — the kernel forwards a Resolve to us.
    let path = b"/fs/hello";
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe {
        syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, RIGHT_MAP_READ)
    };
    if po < 0 {
        kprint(b"parent: fwd lookup submit FAIL\n");
        return;
    }
    let po = po as u64;

    // 4. Receive the forwarded Resolve request on the server end.
    // SAFETY: valid endpoint + writable out-params.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            send_end,
            (&raw mut FWD_REQ) as u64,
            (&raw mut FWD_REQ_H) as u64,
            (&raw mut FWD_REQ_COUNT) as u64,
        )
    };
    if rr != 0 {
        kprint(b"parent: fwd recv request FAIL\n");
        return;
    }

    // 5. Parse the request via librsproto (IpcMsg: payload_len @4, payload @24).
    let payload_len = unsafe {
        u32::from_le_bytes([FWD_REQ[4], FWD_REQ[5], FWD_REQ[6], FWD_REQ[7]]) as usize
    };
    // SAFETY: payload_len ≤ 4072; the slice stays within FWD_REQ.
    let req_payload = unsafe { &FWD_REQ[24..24 + payload_len] };
    let request = match librsproto::decode(req_payload) {
        Ok(m) => m,
        Err(_) => {
            kprint(b"parent: fwd request decode FAIL\n");
            return;
        }
    };
    if request.op != librsproto::OP_NS_RESOLVE {
        kprint(b"parent: fwd request op mismatch\n");
        return;
    }
    let request_id = request.request_id;

    // 6. Build a read-only MemoryObject holding the stub content.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if mem < 0 {
        kprint(b"parent: fwd memobj create FAIL\n");
        return;
    }
    let mem = mem as u64;
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        kprint(b"parent: fwd memobj map FAIL\n");
        return;
    }
    // SAFETY: `addr` is a page the kernel mapped R/W into our address space.
    unsafe {
        core::slice::from_raw_parts_mut(addr as u64 as *mut u8, CONTENT.len())
            .copy_from_slice(CONTENT);
    }

    // 7. Build the rsproto reply (echo request_id; REPLY flag; ResolveReply body)
    //    into the reply IpcMsg's payload, and stage the MemoryObject for transfer.
    let mut body = [0u8; 16];
    let body_len = match librsproto::namespace::resolve_reply(
        &mut body,
        librsproto::namespace::OBJECT_KIND_MEMOBJ,
        CONTENT.len() as u32,
    ) {
        Some(n) => n,
        None => {
            kprint(b"parent: fwd reply body FAIL\n");
            return;
        }
    };
    // SAFETY: FWD_REPLY is a valid 4096-byte buffer; the rsproto reply goes in the
    // IPC payload region (offset 24).
    let rs_len = unsafe {
        match librsproto::encode(
            &mut FWD_REPLY[24..],
            librsproto::OP_NS_RESOLVE,
            request_id,
            librsproto::RS_FLAG_REPLY,
            &body[..body_len],
            1,
        ) {
            Some(n) => n,
            None => {
                kprint(b"parent: fwd reply encode FAIL\n");
                return;
            }
        }
    };
    // SAFETY: set the IpcMsg header's payload_len (@4) + handle_count (@8) and the
    // transferred-handle slot.
    unsafe {
        FWD_REPLY[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        FWD_REPLY[8] = 1;
        FWD_REPLY_H[0] = mem;
    }

    // 8. Send the reply, transferring the MemoryObject. The kernel detects that the
    //    peer is its forwarding endpoint and completes the lookup PO inline.
    // SAFETY: valid endpoint + message + 1-handle transfer array.
    let sr = unsafe {
        syscall5(
            SYS_CHANNEL_SEND,
            send_end,
            (&raw const FWD_REPLY) as u64,
            (&raw const FWD_REPLY_H) as u64,
            1,
            SENDMODE_NOBLOCK,
        )
    };
    if sr != 0 {
        kprint(b"parent: fwd reply send FAIL\n");
        return;
    }

    // 9. Wait the lookup PO (already completed by the inline reply) and read the
    //    resolved handle.
    let (st, resolved) = po_wait(po);
    if st != 0 || resolved == 0 {
        kprint(b"parent: fwd lookup result FAIL\n");
        return;
    }

    // 10. Map the resolved MemoryObject and verify the content round-tripped.
    let raddr = unsafe { syscall4(SYS_MEMORY_MAP, resolved, 0, PAGE, RIGHT_MAP_READ) };
    if raddr < 0 {
        kprint(b"parent: fwd map resolved FAIL\n");
        return;
    }
    // SAFETY: `raddr` is the mapped, kernel-installed MemoryObject.
    let matches = unsafe {
        core::slice::from_raw_parts(raddr as u64 as *const u8, CONTENT.len()) == CONTENT
    };
    if matches {
        kprint(b"parent: forwarded lookup returned 'STUB' via fs-server ok\n");
    } else {
        kprint(b"parent: fwd content mismatch\n");
    }
}

/// `notif` (in `rdi`) is this process's notification-channel handle and
/// `root_ns` (in `rsi`) its root-namespace handle, both seeded by the kernel at
/// spawn. The third bootstrap register is unused here.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, _boot2: u64) -> ! {
    kprint(b"parent: up (demo supervisor, spawned by init)\n");

    // 0. Exception demo: a worker thread faults; we suspend, inspect, terminate.
    worker_exception_demo(notif);

    // 0a. Hardware floating point in ring 3 (Phase-4 FP enablement Part D). Runs
    //     **early**, not at the end of the chain, and deliberately so: the login chain
    //     owns the PASS verdict and races this process, so a demo placed last can have
    //     the run adjudicated out from under it — which is exactly what happened under
    //     KVM, where the boot is fast enough that the verdict fired mid-demo and the
    //     check silently never ran. Up front it always completes, and a failure always
    //     reaches init's `code != 0` fail path in time to fail the run.
    fp_hardfloat_demo(root_ns, notif);

    // 0a2. Directory listing over the direct-RPC transport (dir-ops Part A). Early, before
    //      the login chain adjudicates, for the same reason as the FP demo above.
    dir_list_demo(root_ns);

    // 0a2b. REPRO INSTRUMENTATION (uncommitted): pure ReadDir loop on one session — the
    //       intermittent fs-server I/O hang (2026-07-23 decision log).

    // 0a3. Directory mutation over the same transport (dir-ops Part B): mkdir + rmdir.
    dir_mutate_demo(root_ns);

    // 0b. Blocking-send / PendingOperation demos (async-I/O primitive).
    block_send_demo();
    block_bounded_demo();

    // 0c. Namespace demo: create / bind / lookup / wait / use / unbind on a fresh
    //     namespace (parent's inherited root is LOOKUP-only under init).
    ns_demo();

    // 0d. Entropy demo: create an EntropyObject and read CSPRNG bytes.
    entropy_demo();

    // 0e. Kernel-server demo: resolve /dev/entropy (boot-bound by the kernel) and
    //     read from the handle the in-kernel server hands back.
    dev_entropy_lookup_demo(root_ns);

    // 0f. /proc/self self-reference servers: resolve our own process/thread/namespace
    //     from the root namespace and prove each handle.
    proc_self_demo(root_ns);

    // 0g. Initramfs substrate: resolve + map /initramfs/etc/init.toml (the Limine
    //     module, served by the in-kernel CPIO server bound at boot).
    initramfs_demo(root_ns);

    // 0h. Block storage: resolve /dev/blk/0 (the AHCI disk), submit an async read
    //     of sector 0, and verify the boot signature — the full userspace
    //     sys_io_submit path against real hardware.
    block_demo(root_ns);

    // 0i. Userspace-server forwarding: bind an IPC endpoint as a Userspace Server,
    //     look a path up through it, serve the kernel-forwarded Resolve, and map
    //     the returned MemoryObject — the slice-7 transparent-forwarding proof.
    forward_demo();

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
    // Resolve the child program image from the initramfs (path-based spawn).
    let (cst, child_img) = ns_lookup_wait(root_ns, b"/initramfs/sbin/child", RIGHT_MAP_READ);
    if cst != 0 || child_img == 0 {
        kprint(b"parent: child image not found\n");
        exit(1);
    }
    // SAFETY: SPAWN_A/SPAWN_B are valid writable arg blocks.
    unsafe {
        SPAWN_A.image = child_img;
        SPAWN_B.image = child_img;
        SPAWN_A.handles[0] = e0;
        SPAWN_B.handles[0] = e1;
        SPAWN_A.namespace = child_ns as u64;
        SPAWN_B.namespace = child_ns as u64;
    }
    // SAFETY: valid SpawnArgs pointer; returns a process handle or a neg error.
    // libos::spawn returns owning Handle<Process> handles — held until the end of this
    // function (past the reap-count below), then dropped to reap the children (their
    // handles were previously leaked until process exit).
    // SAFETY: SPAWN_A/SPAWN_B are our statics, exclusively read here.
    let (_pa, _pb) = match unsafe { (spawn(&*(&raw const SPAWN_A)), spawn(&*(&raw const SPAWN_B))) } {
        (Ok(a), Ok(b)) => (a, b),
        _ => {
            kprint(b"parent: spawn FAIL\n");
            exit(1);
        }
    };
    // The kernel copied the child ELF at each spawn; close parent's image handle.
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, child_img) };
    // `_pa`/`_pb` are owning Handle<Process> — they reap the children by closing on drop
    // at the end of this function (see below).
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

    // 4. The concurrent-exit stress: waves of exiting children race teardown
    // against spawn, the login chain, and each other (substrate-hardening
    // regression cover — see `exit_storm_demo`).
    exit_storm_demo(root_ns, notif);

    // 5. The sched-stats milestone check runs LAST, after the spawn/IPC demos
    // above have put real work on multiple CPUs (and the login chain has been
    // running concurrently throughout) — see `sched_stats_demo`.
    sched_stats_demo(root_ns);

    // 5. Exit. The child process handles (`_pa`/`_pb`, owning libos Handles) reap on
    // drop as this function returns into the exit below.
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
