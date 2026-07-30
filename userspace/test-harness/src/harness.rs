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

extern crate alloc;

use core::arch::asm;
use libkern::*;
use libos::{Handle, Namespace, NsMutable, spawn, thread_create};
use librsproto::session::{Dir, DirError};

/// `alloc` backing for the `libstream` transport demo (`stream_transport_demo`).
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

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
    arg0: 0, // Tier 0: test-stage exits immediately (exit-storm)
    handles: [0; 4],
    rights: [0; 4],
    namespace: 0, // inherit the parent's namespace
    syscaps: 0,
};
/// A zeroed 4096-byte IPC message (empty payload, no transfers) for the
/// blocking-send demo, used for both send and recv.
static mut MSGBUF: [u8; 4096] = [0; 4096];
// (The directory demos' message buffers moved into `librsproto::session::Dir`, which owns
// the request/reply plumbing they used to hand-roll — each demo now passes it one stack
// buffer.)
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

// --- stdio/pipe transport demo (C3) -----------------------------------------
/// Thread args for the stream producer.
static mut STREAM_ARGS: ThreadArgs = ThreadArgs {
    entry: 0,
    user_sp: 0,
    arg0: 0,
    class: 0,
    rt_priority: 0,
    nice: 0,
    cpu_affinity: 0,
    _reserved: [0; 36],
};
/// The producer thread's send endpoint (an in-process hand-off — not the spawn ABI).
static mut STREAM_E0: u64 = 0;
/// Rows the producer writes; large enough that the stream far exceeds the pipe's
/// bounded ring, so the producer thread actually blocks on backpressure while the
/// consumer (the main thread) drains.
const STREAM_ROWS: i64 = 2000;
/// The producer thread's stack, in pages.
const STREAM_STACK_PAGES: u64 = 8;

/// Spawn args for the C3 **Tier-1 stage** (`child` conforming path): one moved handle
/// (the stage's bootstrap endpoint = the setup channel), `arg0` set at runtime to the
/// bootstrap descriptor. Inherits parent's LOOKUP-only namespace.
static mut SPAWN_STAGE: SpawnArgs = SpawnArgs {
    image: 0,     // resolved at spawn from /initramfs/sbin/test-stage
    handle_count: 1,
    move_mask: 1, // move handle 0 (the setup channel) to the stage
    arg0: 0,      // set to `bootstrap_arg0(true)` at runtime
    handles: [0; 4],
    rights: [ENDPOINT_RIGHTS, 0, 0, 0],
    namespace: 0, // inherit (LOOKUP-only)
    syscaps: 0,
};

/// The producer thread: write a `{ i: Int, name: String }` TSM1 stream of
/// [`STREAM_ROWS`] rows onto its endpoint via [`libstream::channel::ChannelSink`], then
/// exit cleanly. The process keeps the endpoint (handles are process-owned), so the
/// consumer sees no spurious `PeerClosed`.
extern "C" fn stream_producer() -> ! {
    use alloc::string::String;
    use libstream::channel::{ChannelSink, IpcPort};
    use libstream::table::TableWriter;
    use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

    // SAFETY: main initialised STREAM_E0 before creating this thread.
    let e0 = unsafe { (&raw const STREAM_E0).read() };
    let schema = Schema::new()
        .field("i", TypeTag::Int, TypeModifiers::NONE)
        .field("name", TypeTag::String, TypeModifiers::NONE);
    let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(e0), IPC_PAYLOAD_SIZE));
    let ok = tw.write_schema(StreamFlags::NONE, &schema).is_ok()
        && (0..STREAM_ROWS).all(|i| {
            tw.write_row(&[Value::Int(i), Value::Str(String::from("row"))])
                .is_ok()
        })
        && tw.finish_with_status(0).is_ok()
        && tw.into_sink().finish().is_ok();
    let _ = ok; // a mismatch surfaces as the consumer's fail-loud verdict
    // Clean thread exit (the process retains e0 for the consumer to finish draining).
    // SAFETY: `sys_thread_exit` ends this thread; the exit code is unused.
    unsafe { syscall1(SYS_THREAD_EXIT, 0) };
    // Unreachable once the thread is gone.
    loop {
        // SAFETY: `pause` is always valid in ring 3 with no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}

/// Move a TSM1 stream over a **real IPC channel between two threads** — the C3
/// transport proof. Main creates a small-ring pipe, spawns [`stream_producer`] on one
/// end, and consumes + verifies [`STREAM_ROWS`] rows on the other via
/// [`libstream::channel::ChannelReceiver`]. Because the stream far exceeds the ring, the
/// producer blocks on backpressure while main drains — exercising the real blocking-send
/// path. A mismatch is fail-loud (`exit(1)` → init fails the run).
fn stream_transport_demo() {
    use libstream::channel::{ChannelReceiver, IpcPort};
    use libstream::table::{Item, TableReader};
    kprint(b"test-harness: stream transport demo (2-thread pipe, backpressured)\n");

    // 1. A depth-4 pipe: small on purpose, so a 2000-row stream overflows the ring.
    // SAFETY: END0/END1 are valid writable out-params.
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut END0) as u64, (&raw mut END1) as u64, 4, 0)
    };
    if cr != 0 {
        kprint(b"test-harness: stream chan create FAIL\n");
        exit(1);
    }
    // SAFETY: the kernel wrote both endpoint handles.
    let (e0, e1) = unsafe { ((&raw const END0).read(), (&raw const END1).read()) };

    // 2. The producer thread's stack (mapped read/write; grows down from the top).
    let stack_bytes = STREAM_STACK_PAGES * PAGE;
    // SAFETY: register-only syscalls with valid arguments.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, stack_bytes, 0, 0, 0) };
    if mem < 0 {
        kprint(b"test-harness: stream stack create FAIL\n");
        exit(1);
    }
    // SAFETY: maps the stack object read/write at a kernel-chosen address.
    let base = unsafe {
        syscall4(SYS_MEMORY_MAP, mem as u64, 0, stack_bytes, RIGHT_MAP_READ | RIGHT_MAP_WRITE)
    };
    if base < 0 {
        kprint(b"test-harness: stream stack map FAIL\n");
        exit(1);
    }
    let stack_top = base as u64 + stack_bytes;

    // 3. Spawn the producer thread on `e0`.
    // SAFETY: STREAM_E0/STREAM_ARGS are our statics; we initialise them, then hand a
    // shared reference to the wrapper.
    let _producer = match unsafe {
        STREAM_E0 = e0;
        STREAM_ARGS.entry = stream_producer as *const () as u64;
        STREAM_ARGS.user_sp = stack_top;
        STREAM_ARGS.arg0 = 0;
        thread_create(&*(&raw const STREAM_ARGS))
    } {
        Ok(t) => t,
        Err(_) => {
            kprint(b"test-harness: stream producer thread FAIL\n");
            exit(1);
        }
    };

    // 4. Consume + verify on `e1`: STREAM_ROWS rows, row `i` carries `Int(i)`.
    let mut rx = ChannelReceiver::new(IpcPort::new(e1));
    let bytes = match rx.receive() {
        Ok(b) => b,
        Err(_) => {
            kprint(b"test-harness: stream receive FAIL\n");
            exit(1);
        }
    };
    let mut tr = match TableReader::new(&bytes) {
        Ok(t) => t,
        Err(_) => {
            kprint(b"test-harness: stream bad TSM1 header\n");
            exit(1);
        }
    };
    let mut n: i64 = 0;
    loop {
        match tr.next() {
            Some(Ok(Item::Row(vals))) => {
                if vals.first().and_then(|v| v.as_int()) != Some(n) {
                    kprint(b"test-harness: stream row MISMATCH\n");
                    exit(1);
                }
                n += 1;
            }
            Some(Ok(Item::End(_))) | None => break,
            _ => {
                kprint(b"test-harness: stream decode FAIL\n");
                exit(1);
            }
        }
    }
    if n != STREAM_ROWS {
        kprint(b"test-harness: stream wrong row count\n");
        exit(1);
    }
    // Release this demo's handles (both pipe ends + the producer's stack object) so a
    // long test run doesn't exhaust the harness's handle table. The producer thread has
    // self-exited; `_producer` (the thread handle) drops at the end of scope.
    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, e0);
        syscall1(SYS_HANDLE_CLOSE, e1);
        syscall1(SYS_HANDLE_CLOSE, mem as u64);
    }
    kprint(b"test-harness: stream transport ok (2000 rows over a real pipe, backpressured)\n");
}

/// The C3 **setup-message spawn** (Part C.2): spawn `child` as a Tier-1 stage — with a
/// bootstrap descriptor `arg0` and a setup channel — then send it a setup message that
/// transfers a `stdin` pipe end and carries `argv = ["stage-demo", "500"]`. Parent then
/// produces the `stdin` TSM1 stream; the stage reads `stdin` + `argv` from the setup
/// message and verifies the rows, exiting `0`/`1`. Parent reaps that exit and fails the
/// run on a non-zero code. Exercises the one spawn convention (Part B) end to end over a
/// real `sys_process_spawn` + setup message.
fn stage_spawn_demo(root_ns: u64, notif: u64) {
    use alloc::string::String;
    use libstream::channel::{ChannelSink, IpcPort};
    use libstream::setup::{Streams, bootstrap_arg0, pipe, send_setup};
    use libstream::table::TableWriter;
    use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

    kprint(b"test-harness: setup-message stage demo (spawn a Tier-1 stage)\n");
    const STAGE_ROWS: i64 = 500;

    // 1. Resolve the stage binary (the conforming `child` path).
    let (st, img) = ns_lookup_wait(root_ns, b"/initramfs/sbin/test-stage", RIGHT_MAP_READ);
    if st != 0 || img == 0 {
        kprint(b"test-harness: stage image FAIL\n");
        exit(1);
    }

    // 2. The stdin pipe (parent produces on `prod`; the stage reads the transferred end)
    //    and the setup channel (`setup_shell` = parent's end, `setup_stage` = the stage's
    //    bootstrap endpoint).
    let (prod, stage_stdin) = match pipe(4) {
        Ok(p) => p,
        Err(_) => {
            kprint(b"test-harness: stage stdin pipe FAIL\n");
            exit(1);
        }
    };
    let (setup_shell, setup_stage) = match pipe(4) {
        Ok(p) => p,
        Err(_) => {
            kprint(b"test-harness: stage setup chan FAIL\n");
            exit(1);
        }
    };

    // 3. Spawn `child` as a Tier-1 stage: endpoint = `setup_stage` (moved), `arg0` = the
    //    bootstrap descriptor. SAFETY: SPAWN_STAGE is our static, initialised here.
    let _stage = match unsafe {
        SPAWN_STAGE.image = img;
        SPAWN_STAGE.handles[0] = setup_stage;
        SPAWN_STAGE.arg0 = bootstrap_arg0(true);
        spawn(&*(&raw const SPAWN_STAGE))
    } {
        Ok(p) => p,
        Err(_) => {
            kprint(b"test-harness: stage spawn FAIL\n");
            exit(1);
        }
    };

    // 4. Send the setup message: transfer `stage_stdin` as stdin, carry `argv`.
    let streams = Streams {
        stdin: Some(stage_stdin),
        stdout: None,
        stderr: None,
    };
    if send_setup(setup_shell, &streams, &["stage", "500"]).is_err() {
        kprint(b"test-harness: stage send_setup FAIL\n");
        exit(1);
    }

    // 5. Produce the stdin stream on `prod` (the stage drains it concurrently). It waits
    //    for the terminator before verifying, so `finish` completes before it exits — no
    //    PeerClosed race.
    let schema = Schema::new()
        .field("i", TypeTag::Int, TypeModifiers::NONE)
        .field("name", TypeTag::String, TypeModifiers::NONE);
    let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(prod), IPC_PAYLOAD_SIZE));
    let ok = tw.write_schema(StreamFlags::NONE, &schema).is_ok()
        && (0..STAGE_ROWS).all(|i| {
            tw.write_row(&[Value::Int(i), Value::Str(String::from("row"))])
                .is_ok()
        })
        && tw.finish_with_status(0).is_ok()
        && tw.into_sink().finish().is_ok();
    if !ok {
        kprint(b"test-harness: stage produce FAIL\n");
        exit(1);
    }

    // 6. Reap the stage's exit — the first `ChildExited` (no other child process has been
    //    spawned yet) — and fail the run on a non-zero code.
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
        // SAFETY: NOTIF is a valid 64-byte writable out-param.
        let r = unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) };
        if r != 0 {
            continue; // WouldBlock: re-block
        }
        // SAFETY: the kernel wrote a 64-byte Notification into NOTIF.
        let kind = unsafe { (&raw const NOTIF.kind).read() };
        if kind == KIND_CHILD_EXITED {
            // SAFETY: body[8..12] is the exit code.
            let body = unsafe { (&raw const NOTIF.body).read() };
            let code = i32::from_le_bytes([body[8], body[9], body[10], body[11]]);
            if code != 0 {
                kprint(b"test-harness: stage exited non-zero\n");
                exit(1);
            }
            break;
        }
    }
    // Release our ends of the stdin pipe + setup channel (the stage's ends were moved to
    // it); don't leak handles across a long test run. `_stage` (the process handle) drops
    // at scope end. SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, prod);
        syscall1(SYS_HANDLE_CLOSE, setup_shell);
    }
    kprint(b"test-harness: setup-message stage ok (stdin stream + argv verified by the stage)\n");
}

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
    kprint(b"test-harness: mapping a worker stack\n");
    // 1. Allocate + map a one-page worker stack (read/write).
    // SAFETY: register-only syscalls with valid arguments.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if mem < 0 {
        kprint(b"test-harness: worker stack create FAIL\n");
        exit(1);
    }
    // SAFETY: maps the memory object read/write at a kernel-chosen address.
    let base = unsafe {
        syscall4(SYS_MEMORY_MAP, mem as u64, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE)
    };
    if base < 0 {
        kprint(b"test-harness: worker stack map FAIL\n");
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
            kprint(b"test-harness: thread_create FAIL\n");
            exit(1);
        }
    };
    kprint(b"test-harness: created worker thread; awaiting its fault\n");

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
            kprint(b"test-harness: wait FAIL\n");
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
        kprint(b"test-harness: get_registers FAIL\n");
        exit(1);
    }
    // SAFETY: the kernel wrote the 18-register snapshot into WORKER_REGS.
    let rip = unsafe { (&raw const WORKER_REGS.regs[REG_RIP]).read() };
    kprint(b"test-harness: worker faulted @ rip=");
    kprint_hex(rip);
    kprint(b" ; terminating\n");

    // 5. Terminate the worker (resume with the Terminate disposition, code 7).
    // SAFETY: register-only syscall.
    let er = unsafe {
        syscall4(SYS_EXCEPTION_RESUME, worker.raw().0, DISPOSITION_TERMINATE, 7, 0)
    };
    if er != 0 {
        kprint(b"test-harness: exception_resume FAIL\n");
        exit(1);
    }
    // The worker is not this process's last thread (we are still running), so
    // its termination produces no `ChildExited`. Drop our handle to it.
    // SAFETY: closing our own handle.
    // (worker Handle<Thread> closes on drop at function end — no explicit close)
    kprint(b"test-harness: worker terminated\n");
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
        kprint(b"test-harness: block-demo channel create FAIL\n");
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
        kprint(b"test-harness: block send FAIL\n");
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
        kprint(b"test-harness: block-demo recv FAIL\n");
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
        kprint(b"test-harness: blocking send completed via PendingOperation (");
        kprint_u64(filled);
        kprint(b" queued, 1 blocked-then-delivered)\n");
    } else {
        kprint(b"test-harness: block-demo wait unexpected\n");
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
        kprint(b"test-harness: block-bounded channel create FAIL\n");
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
        kprint(b"test-harness: block-bounded send FAIL\n");
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
        kprint(b"test-harness: blocking send timed out via PendingOperation (BlockBounded)\n");
    } else {
        kprint(b"test-harness: block-bounded demo unexpected\n");
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
    kprint(b"test-harness: ns-demo start (fresh namespace)\n");

    // (a) sys_ns_create: a fresh, full-rights namespace to bind into. A process
    //     binds into namespaces it owns; its inherited root namespace is
    //     LOOKUP-only (sandbox-by-construction), so the demo uses this one — which
    //     works whether parent is pid 1 or a child of init.
    // SAFETY: register-only syscall.
    let ns = unsafe { syscall1(SYS_NS_CREATE, 0) };
    if ns < 0 {
        kprint(b"test-harness: ns_create FAIL\n");
        return;
    }
    let ns = ns as u64;
    kprint(b"test-harness: ns_create ok\n");

    // (b) Create a MemoryObject to bind as a direct-handle resource.
    // SAFETY: register-only syscall.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if mem < 0 {
        kprint(b"test-harness: ns-demo mem create FAIL\n");
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
        kprint(b"test-harness: ns_bind FAIL\n");
        return;
    }
    kprint(b"test-harness: ns_bind /store ok\n");

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
        kprint(b"test-harness: ns_lookup FAIL\n");
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
        kprint(b"test-harness: ns_lookup wait unexpected\n");
        return;
    }
    kprint(b"test-harness: ns_lookup -> resolved handle=");
    kprint_u64(resolved);
    kprint(b"\n");

    // (f) Use the resolved handle: map it read/write — proves the binding handed
    //     back a usable, rights-attenuated MemoryObject handle.
    // SAFETY: `resolved` is a MemoryObject handle carrying MAP_READ|MAP_WRITE.
    let mapped = unsafe {
        syscall4(SYS_MEMORY_MAP, resolved, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE)
    };
    if mapped < 0 {
        kprint(b"test-harness: ns-demo map resolved FAIL\n");
        return;
    }
    kprint(b"test-harness: mapped resolved /store handle ok\n");

    // (g) sys_ns_unbind: remove "/store"; a follow-up lookup must complete the PO
    //     with a NotFound status (error delivered through the PO, not the syscall).
    // SAFETY: valid path pointer + handle.
    let ur = unsafe {
        syscall4(SYS_NS_UNBIND, ns, path.as_ptr() as u64, path.len() as u64, 0)
    };
    if ur != 0 {
        kprint(b"test-harness: ns_unbind FAIL\n");
        return;
    }
    // SAFETY: valid path pointer + handle.
    let po2 = unsafe {
        syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, RIGHT_MAP_READ)
    };
    if po2 < 0 {
        kprint(b"test-harness: ns_lookup-after-unbind FAIL\n");
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
        kprint(b"test-harness: ns_unbind ok (lookup-after-unbind -> NotFound)\n");
    } else {
        kprint(b"test-harness: ns-demo unbind unexpected\n");
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
    kprint(b"test-harness: ns-demo done\n");
}

/// Entropy demo: create an `EntropyObject` token and read CSPRNG bytes from it.
/// The pool seeds at boot (QEMU runs with `+rdrand,+rdseed`), so both reads return
/// `0` (synchronous fill) and the two 32-byte draws differ (the CSPRNG advances).
fn entropy_demo() {
    kprint(b"test-harness: entropy-demo start\n");
    // SAFETY: register-only syscall.
    let h = unsafe { syscall1(SYS_ENTROPY_CREATE, 0) };
    if h < 0 {
        kprint(b"test-harness: entropy_create FAIL\n");
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
        kprint(b"test-harness: entropy read not synchronous (unseeded?)\n");
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
    kprint(b"test-harness: entropy bytes[0..8]=");
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
    kprint(b"test-harness: /dev/entropy lookup-demo start\n");
    let path = b"/dev/entropy";
    // sys_ns_lookup → PendingOperation; the resolved handle arrives in IoResult.
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe {
        syscall4(SYS_NS_LOOKUP, root_ns, path.as_ptr() as u64, path.len() as u64, RIGHT_READ)
    };
    if po < 0 {
        kprint(b"test-harness: /dev/entropy lookup FAIL\n");
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
        kprint(b"test-harness: /dev/entropy lookup wait unexpected\n");
        return;
    }

    // Use the resolved EntropyObject handle: read 32 CSPRNG bytes (0 = synchronous
    // fill; the pool seeds at boot).
    let mut a = [0u8; 32];
    // SAFETY: valid writable 32-byte buffer; `len` ≤ ENTROPY_READ_MAX.
    let r = unsafe { syscall4(SYS_ENTROPY_READ, resolved, (&raw mut a) as u64, 32, 0) };
    if r != 0 {
        kprint(b"test-harness: /dev/entropy read not synchronous\n");
        unsafe { syscall1(SYS_HANDLE_CLOSE, resolved) };
        return;
    }
    let first = u64::from_le_bytes([a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7]]);
    kprint(b"test-harness: /dev/entropy resolved+read ok bytes[0..8]=");
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
/// `want`. The out-param must be a real [`HandleInfo`], never a hand-sized byte
/// array: the kernel writes the full struct (24 bytes since `size` was added),
/// and an undersized stack buffer here let the copy-out zero 8 bytes of the
/// caller's frame — which clobbered `_start`'s spilled `root_ns` and broke every
/// later root-namespace lookup (the 2026-07-24 "namespace premature free" hunt).
fn stat_is_type(h: u64, want: u32) -> bool {
    let mut info = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
    // SAFETY: valid writable `HandleInfo` out-param.
    let r = unsafe { syscall4(SYS_HANDLE_STAT, h, (&raw mut info) as u64, 0, 0) };
    if r != 0 {
        return false;
    }
    info.object_type == want
}

/// `/proc/self` demo: resolve the caller's own resources from the **root
/// namespace** (`rsi`) and prove each handle. No ambient authority — these resolve
/// only because the kernel bound `/proc/self/*` into pid 1's root namespace, and
/// each returns strictly *this* caller's own object (derived from syscall context).
fn proc_self_demo(root_ns: u64) {
    kprint(b"test-harness: /proc/self demo start\n");

    // /proc/self/process — request INSPECT; stat the handle, assert it's a Process.
    let (st, ph) = ns_lookup_wait(root_ns, b"/proc/self/process", RIGHT_INSPECT);
    if st != 0 || ph == 0 || !stat_is_type(ph, KOBJ_PROCESS) {
        kprint(b"test-harness: /proc/self/process FAIL\n");
        return;
    }
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, ph) };
    kprint(b"test-harness: /proc/self/process ok (own Process handle)\n");

    // /proc/self/thread — likewise, assert it's a Thread.
    let (st, th) = ns_lookup_wait(root_ns, b"/proc/self/thread", RIGHT_INSPECT);
    if st != 0 || th == 0 || !stat_is_type(th, KOBJ_THREAD) {
        kprint(b"test-harness: /proc/self/thread FAIL\n");
        return;
    }
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, th) };
    kprint(b"test-harness: /proc/self/thread ok (own Thread handle)\n");

    // /proc/self/namespace — request LOOKUP; assert it's a Namespace, then USE it:
    // resolve /dev/entropy *through* the returned handle (proves a live,
    // LOOKUP-capable namespace identical to our root).
    let (st, nh) = ns_lookup_wait(root_ns, b"/proc/self/namespace", RIGHT_LOOKUP | RIGHT_INSPECT);
    if st != 0 || nh == 0 || !stat_is_type(nh, KOBJ_NAMESPACE) {
        kprint(b"test-harness: /proc/self/namespace FAIL\n");
        return;
    }
    let (st2, eh) = ns_lookup_wait(nh, b"/dev/entropy", RIGHT_READ);
    if st2 != 0 || eh == 0 {
        kprint(b"test-harness: /proc/self/namespace lookup-through FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, nh) };
        return;
    }
    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, eh);
        syscall1(SYS_HANDLE_CLOSE, nh);
    }
    kprint(b"test-harness: /proc/self/namespace ok (resolved /dev/entropy through it)\n");
}

/// Initramfs demo: resolve `/initramfs/etc/init.toml` from the root namespace
/// (the kernel bound `/initramfs` at boot to the in-kernel CPIO server), map the
/// returned read-only `MemoryObject`, and print its first bytes — proving the
/// Limine module + CPIO parser + `/initramfs` server end-to-end, before Init
/// exists. (This is Init's real job in slice 5+; here it's just a substrate check.)
fn initramfs_demo(root_ns: u64) {
    kprint(b"test-harness: /initramfs demo start\n");
    let (st, mem) = ns_lookup_wait(root_ns, b"/initramfs/etc/init.toml", RIGHT_MAP_READ);
    if st != 0 || mem == 0 {
        kprint(b"test-harness: /initramfs/etc/init.toml lookup FAIL\n");
        return;
    }
    // Map the returned MemoryObject read-only and read its first bytes.
    // SAFETY: register-only syscall; `mem` is a MemoryObject handle with MAP_READ.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, PAGE, RIGHT_MAP_READ) };
    if addr < 0 {
        kprint(b"test-harness: /initramfs map FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
        return;
    }
    // SAFETY: `addr` is a page the kernel mapped MAP_READ holding the file's bytes;
    // read the first 16 in bounds (init.toml is far longer).
    let head = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, 16) };
    kprint(b"test-harness: /initramfs/etc/init.toml -> \"");
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
    kprint(b"test-harness: exit-storm start\n");
    let (st, img) = ns_lookup_wait(root_ns, b"/initramfs/sbin/test-stage", RIGHT_MAP_READ);
    if st != 0 || img == 0 {
        kprint(b"test-harness: exit-storm image lookup FAIL\n");
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
                    kprint(b"test-harness: exit-storm spawn FAIL\n");
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
                kprint(b"test-harness: exit-storm wait FAIL\n");
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
    kprint(b"test-harness: exit-storm ok (18 exits reaped)\n");
}

/// **Directory listing over the direct-RPC transport** (dir-ops Part A). Opens `/system`
/// as a directory session (`sys_ns_lookup` resolves a directory path to a session channel —
/// `OBJECT_KIND_CHANNEL`), enumerates it, and confirms the known entry
/// `current-generation` is listed *with plausible metadata*. Proves the whole transport
/// end to end: endpoint acquisition, the session channel, name-addressed enumeration,
/// cursor-following, and reply correlation.
///
/// Drives [`librsproto::session::Dir`] — the shared client — rather than hand-rolled
/// syscall plumbing, so this demo is also the client's integration proof. A failure exits
/// non-zero (init's fail path).
fn dir_list_demo(root_ns: u64) {
    kprint(b"test-harness: dir-list demo start\n");
    let mut buf = [0u8; 4096];
    let mut dir = match Dir::open(root_ns, b"/system", &mut buf) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: dir-list open FAIL\n"),
    };

    let mut saw_currentgen = false;
    let mut saw_dot_dir = false;
    let listed = dir.read_dir(|e| {
        if e.name == b"current-generation" {
            saw_currentgen = true;
            // The entry carries its inode metadata (the fields `list` reports as
            // `Table<{name, size, kind, modified}>`). Check them against what this file
            // must be: a non-empty regular file, smaller than a block, stamped by the
            // image build — a zeroed or mis-decoded field fails every one of these, which
            // a name-only check would have missed.
            if e.kind != librsproto::file::DIRENT_KIND_FILE {
                return_fail(b"test-harness: dir-list FAIL (current-generation not a file)\n");
            }
            if e.size == 0 || e.size > 4096 {
                return_fail(b"test-harness: dir-list FAIL (implausible size)\n");
            }
            if e.mode & 0xF000 != 0x8000 {
                return_fail(b"test-harness: dir-list FAIL (mode not S_IFREG)\n");
            }
            // 1_600_000_000 = Sep 2020, comfortably before this image was built.
            if e.mtime < 1_600_000_000 {
                return_fail(b"test-harness: dir-list FAIL (implausible mtime)\n");
            }
        }
        if e.name == b"." {
            saw_dot_dir = e.kind == librsproto::file::DIRENT_KIND_DIR;
        }
        true
    });
    if listed.is_err() {
        return_fail(b"test-harness: dir-list enumerate FAIL\n");
    }
    dir.close();
    if !saw_currentgen {
        return_fail(b"test-harness: dir-list FAIL (current-generation not found)\n");
    }
    if !saw_dot_dir {
        return_fail(b"test-harness: dir-list FAIL (. missing or not a directory)\n");
    }
    kprint(b"test-harness: dir-list ok (/system lists current-generation + metadata)\n");
}

/// Print `msg` and exit non-zero — a `-> !` helper so the demo's `match`/closure arms can
/// bail without an awkward control-flow dance.
fn return_fail(msg: &[u8]) -> ! {
    kprint(msg);
    exit(1)
}

/// Entries the dir-growth check creates. At 250-byte names (~258 bytes per record) that is
/// ~4.1 KB — just past one 4 KiB directory block, so the directory must grow, without
/// paying for a sweep the host e2fsck tests already cover.
const GROW_ENTRIES: usize = 16;

/// **Directory mutation over the direct-RPC transport** (dir-ops Part B). On the same kind
/// of open directory session as `dir_list_demo`, exercises the name-addressed mutations end
/// to end: mkdir a temp subdir, confirm it appears, rename it, confirm the rename, then
/// rmdir it and confirm it is gone. Each op is a single request/reply on the session
/// channel; the session is bound to `/system`, so the names can only ever touch `/system`.
///
/// Also covers the client's error path: removing a name that does not exist must come back
/// as a *server* error carrying a `KError`, not as a transport failure or a false success.
fn dir_mutate_demo(root_ns: u64) {
    kprint(b"test-harness: dir-mutate demo start\n");
    let mut buf = [0u8; 4096];
    let mut dir = match Dir::open(root_ns, b"/system", &mut buf) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: dir-mutate open FAIL\n"),
    };

    // mkdir nx-tmp → confirm it appears (a ReadDir on the same session).
    if dir.mkdir(b"nx-tmp").is_err() {
        return_fail(b"test-harness: mkdir FAIL\n");
    }
    if !dir_contains(&mut dir, b"nx-tmp") {
        return_fail(b"test-harness: mkdir not visible\n");
    }
    // rename nx-tmp → nx-tmp2 → confirm the old name is gone and the new one present.
    if dir.rename(b"nx-tmp", b"nx-tmp2").is_err() {
        return_fail(b"test-harness: rename FAIL\n");
    }
    if dir_contains(&mut dir, b"nx-tmp") || !dir_contains(&mut dir, b"nx-tmp2") {
        return_fail(b"test-harness: rename not applied\n");
    }
    // A failing op must surface as a server error, distinguishable from a transport fault:
    // the client's whole point is that a caller can tell "no such entry" from "the pipe
    // broke" without decoding replies itself.
    match dir.rmdir(b"nx-does-not-exist") {
        Err(DirError::Server(_)) => {}
        Err(_) => return_fail(b"test-harness: rmdir(missing) reported a transport fault\n"),
        Ok(()) => return_fail(b"test-harness: rmdir(missing) wrongly succeeded\n"),
    }
    // Fill the directory past one block's worth of entries, on the **real** on-disk
    // filesystem rather than the host tests' synthetic fixture. `/system` has 4 KiB
    // blocks, and a 250-byte name costs ~258 bytes per record, so 16 entries is ~4.1 KB
    // — just over one block, which is the point. Before directory growth landed
    // (2026-07-29) this failed partway with `TooLarge`.
    //
    // Sized to *cross the boundary*, not to stress: each entry is several block
    // read-modify-writes through the fs-server, and a bigger sweep pushed the TCG run
    // past the harness timeout while proving nothing the host e2fsck tests do not.
    let mut name = [b'g'; 250];
    for i in 0..GROW_ENTRIES {
        name[0] = b'0' + (i / 10) as u8;
        name[1] = b'0' + (i % 10) as u8;
        if dir.mkdir(&name).is_err() {
            return_fail(b"test-harness: dir-grow mkdir FAIL (directory did not grow?)\n");
        }
    }
    // One enumeration, counting ours: the added blocks must be reachable through the
    // paginated ReadDir, not merely written. (Per-entry lookups would be 16 more full
    // walks for no extra coverage.)
    let mut seen = 0u32;
    let walked = dir.read_dir(|e| {
        if e.name.len() == 250 && e.name[2] == b'g' {
            seen += 1;
        }
        true
    });
    if walked.is_err() || seen != GROW_ENTRIES as u32 {
        return_fail(b"test-harness: dir-grow FAIL (entries missing after growth)\n");
    }
    for i in 0..GROW_ENTRIES {
        name[0] = b'0' + (i / 10) as u8;
        name[1] = b'0' + (i % 10) as u8;
        if dir.rmdir(&name).is_err() {
            return_fail(b"test-harness: dir-grow rmdir FAIL\n");
        }
    }

    // rmdir nx-tmp2 → confirm it is gone.
    if dir.rmdir(b"nx-tmp2").is_err() {
        return_fail(b"test-harness: rmdir FAIL\n");
    }
    if dir_contains(&mut dir, b"nx-tmp2") {
        return_fail(b"test-harness: rmdir not applied\n");
    }
    dir.close();
    kprint(b"test-harness: dir-mutate ok (mkdir + rename + rmdir, each ReadDir-verified)\n");
}

/// Whether the open directory currently lists an entry named `name`. Exits the demo on a
/// transport failure — a listing that cannot complete is not the same as a name that is
/// absent, and conflating them would let a broken transport read as a passing test.
fn dir_contains(dir: &mut Dir<'_>, name: &[u8]) -> bool {
    let mut found = false;
    // Returning `false` from the callback stops enumeration early — no need to drain the
    // rest of a directory once the name is seen.
    let r = dir.read_dir(|e| {
        if e.name == name {
            found = true;
        }
        !found
    });
    if r.is_err() {
        return_fail(b"test-harness: dir-contains enumerate FAIL\n");
    }
    found
}

/// Entries the `list` fixture directory gets, and how long each name is.
///
/// Chosen so the **TSM1 stream exceeds one IPC message** (`IPC_PAYLOAD_SIZE`) while the
/// ext4 directory data still fits **one 4 KiB block**: a row costs ~28 bytes plus the
/// name, an ext4 entry 8 plus the name. 30 × 110-byte names is ≈ 4.1 KB of stream (two
/// messages, so the depth-1 pipe really blocks the producer) against ≈ 3.5 KB of
/// directory data (under the block limit, so this never trips the deferred
/// grow-a-full-directory path). Both bounds are checked by the demo rather than assumed.
const LIST_FIXTURE_ENTRIES: usize = 30;
const LIST_FIXTURE_NAME_LEN: usize = 110;
/// The fixture directory, created under `/system` and removed at the end.
const LIST_FIXTURE_DIR: &[u8] = b"nx-list";
const LIST_FIXTURE_PATH: &[u8] = b"/system/nx-list";
const LIST_FIXTURE_PATH_STR: &str = "/system/nx-list";

/// Spawn args for a `list` stage: one moved handle (its bootstrap/setup endpoint).
static mut SPAWN_LIST: SpawnArgs = SpawnArgs {
    image: 0,     // resolved at spawn from /initramfs/sbin/list
    handle_count: 1,
    move_mask: 1, // move handle 0 (the setup channel) to the stage
    arg0: 0,      // set to `bootstrap_arg0(true)` at runtime
    handles: [0; 4],
    rights: [ENDPOINT_RIGHTS, 0, 0, 0],
    namespace: 0, // inherit (LOOKUP-only)
    syscaps: 0,
};

/// The `i`-th fixture entry name: a short distinctive prefix padded to
/// [`LIST_FIXTURE_NAME_LEN`], so names are long enough to size the stream predictably and
/// still individually identifiable.
fn fixture_name(i: usize) -> alloc::string::String {
    use alloc::string::String;
    let mut s = String::from("e");
    s.push((b'0' + (i / 10) as u8) as char);
    s.push((b'0' + (i % 10) as u8) as char);
    while s.len() < LIST_FIXTURE_NAME_LEN {
        s.push('x');
    }
    s
}

/// **The first coreutil, end to end through a real pipe** (coreutils Milestone 1).
///
/// Builds a fixture directory, spawns `list` as a Tier-1 stage with its `stdout` wired to
/// a **depth-1** pipe, and consumes the TSM1 table it produces. This is the first
/// integrated proof that the Milestone-1 substrate composes: dir-ops (C1) → the typed
/// value model (C2) → the stdio/setup convention (C3) → a real program.
///
/// Three things are checked that a smaller demo could not:
///
/// 1. **The typed contract** — the schema is exactly
///    `{name: String, size: Int, kind: String, modified: Int}`, and every fixture entry
///    arrives with the right kind and a plausible mtime.
/// 2. **Real backpressure** — the received stream is larger than one IPC payload, so it
///    provably spanned several messages on a pipe whose ring holds one: the producer
///    blocked and was woken as this side drained.
/// 3. **Early-consumer close** — a second `list` whose reader closes immediately must exit
///    **`0`**: `PeerClosed` is "stop producing, exit cleanly" (design §1, the `yes | head -1`
///    case), not a failure. A stage that treated it as an error would fail the run here.
fn list_pipeline_demo(root_ns: u64, notif: u64) {
    use libstream::table::{Item, TableReader};
    use libstream::{TypeTag, Value};

    kprint(b"test-harness: list-pipeline demo (the first coreutil over a real pipe)\n");

    // 1. Fixture: a directory of known entries, built through the same client `list` uses.
    let mut fbuf = [0u8; 4096];
    let mut sys = match Dir::open(root_ns, b"/system", &mut fbuf) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: list fixture open FAIL\n"),
    };
    if sys.mkdir(LIST_FIXTURE_DIR).is_err() {
        return_fail(b"test-harness: list fixture mkdir FAIL\n");
    }
    sys.close();
    {
        let mut dbuf = [0u8; 4096];
        let mut fixture = match Dir::open(root_ns, LIST_FIXTURE_PATH, &mut dbuf) {
            Ok(d) => d,
            Err(_) => return_fail(b"test-harness: list fixture reopen FAIL\n"),
        };
        for i in 0..LIST_FIXTURE_ENTRIES {
            if fixture.mkdir(fixture_name(i).as_bytes()).is_err() {
                // A failure here is most likely the deferred full-directory grow: the
                // fixture is sized to stay under one block, so this means the sizing
                // assumption broke, not that the test is flaky.
                return_fail(b"test-harness: list fixture entry mkdir FAIL (directory full?)\n");
            }
        }
        fixture.close();
    }

    // 2. Run `list <fixture>` with stdout on a depth-1 pipe and verify what arrives.
    let bytes = run_list(root_ns, notif, &["list", LIST_FIXTURE_PATH_STR], true);
    if bytes.len() <= IPC_PAYLOAD_SIZE {
        // Not a style check: if the stream fit one message, the pipe never filled and the
        // backpressure path below was not exercised at all.
        return_fail(b"test-harness: list stream fit one message (backpressure untested)\n");
    }
    let mut tr = match TableReader::new(&bytes) {
        Ok(t) => t,
        Err(_) => return_fail(b"test-harness: list stream bad TSM1 header\n"),
    };
    // The schema *is* the contract this coreutil publishes; check it field by field.
    let schema = tr.schema();
    let expect: [(&str, TypeTag); 4] = [
        ("name", TypeTag::String),
        ("size", TypeTag::Int),
        ("kind", TypeTag::String),
        ("modified", TypeTag::Int),
    ];
    if schema.fields.len() != expect.len() {
        return_fail(b"test-harness: list schema field count wrong\n");
    }
    for (field, (name, tag)) in schema.fields.iter().zip(expect.iter()) {
        if field.name != *name || field.ty != *tag {
            return_fail(b"test-harness: list schema mismatch\n");
        }
    }
    let mut rows = 0usize;
    let mut ended = false;
    loop {
        match tr.next() {
            Some(Ok(Item::Row(vals))) => {
                let name_ok = matches!(&vals[0], Value::Str(s) if s.len() == LIST_FIXTURE_NAME_LEN);
                let kind_ok = matches!(&vals[2], Value::Str(s) if s == "dir");
                // `size` is a directory's own data size — one block on this filesystem.
                let size_ok = matches!(vals[1], Value::Int(n) if n > 0);
                // `modified` must be a plausible date. These entries were created by
                // `mkdir` moments ago, so this asserts the whole chain end to end: the
                // kernel anchored a wall clock from the RTC, the fs-server read it, and
                // it reached the inode. (This check was shape-only while the system had
                // no wall clock; restoring it is what the clock slice is *for*.)
                let mtime_ok = matches!(vals[3], Value::Int(t) if t >= 1_600_000_000);
                if !name_ok || !kind_ok || !size_ok || !mtime_ok {
                    return_fail(b"test-harness: list row wrong (name/size/kind/modified)\n");
                }
                rows += 1;
            }
            Some(Ok(Item::End(status))) => {
                if status != 0 {
                    return_fail(b"test-harness: list stream terminator non-zero\n");
                }
                ended = true;
                break;
            }
            None => break,
            _ => return_fail(b"test-harness: list stream decode FAIL\n"),
        }
    }
    if !ended {
        return_fail(b"test-harness: list stream had no terminator\n");
    }
    // Exactly the fixture entries: `.`/`..` are filtered by `list`, so a count that
    // includes them (or drops a real entry) fails here.
    if rows != LIST_FIXTURE_ENTRIES {
        return_fail(b"test-harness: list row count wrong\n");
    }

    // 3. Early-consumer close: the reader goes away mid-stream; `list` must exit cleanly.
    let _ = run_list(root_ns, notif, &["list", LIST_FIXTURE_PATH_STR], false);

    // 3b. `--recursive`: listing `/system` must descend into the fixture and report its
    //     children under a parent-relative path (`nx-list/e…`, not a bare name) — otherwise a recursive listing
    //     could not tell two same-named files in different directories apart.
    let deep = run_list(root_ns, notif, &["list", "--recursive", "/system"], true);
    let mut tr = match TableReader::new(&deep) {
        Ok(t) => t,
        Err(_) => return_fail(b"test-harness: list --recursive bad TSM1 header\n"),
    };
    let mut nested = 0usize;
    while let Some(Ok(item)) = tr.next() {
        if let Item::Row(vals) = item {
            if let Value::Str(name) = &vals[0] {
                if name.starts_with("nx-list/e") {
                    nested += 1;
                }
            }
        }
    }
    if nested != LIST_FIXTURE_ENTRIES {
        return_fail(b"test-harness: list --recursive did not descend correctly\n");
    }

    // 4. Tear the fixture down, leaving the filesystem as we found it.
    {
        let mut dbuf = [0u8; 4096];
        let mut fixture = match Dir::open(root_ns, LIST_FIXTURE_PATH, &mut dbuf) {
            Ok(d) => d,
            Err(_) => return_fail(b"test-harness: list fixture teardown open FAIL\n"),
        };
        for i in 0..LIST_FIXTURE_ENTRIES {
            if fixture.rmdir(fixture_name(i).as_bytes()).is_err() {
                return_fail(b"test-harness: list fixture entry rmdir FAIL\n");
            }
        }
        fixture.close();
    }
    let mut tbuf = [0u8; 4096];
    let mut sys = match Dir::open(root_ns, b"/system", &mut tbuf) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: list fixture teardown FAIL\n"),
    };
    if sys.rmdir(LIST_FIXTURE_DIR).is_err() {
        return_fail(b"test-harness: list fixture rmdir FAIL\n");
    }
    sys.close();
    kprint(b"test-harness: list-pipeline ok (typed table over a real pipe, backpressure + early close)\n");
}

/// Spawn `list` with `argv` as a Tier-1 stage, `stdout` on a **depth-1** pipe (so the ring
/// fills and the producer must block), then either drain the stream (`consume`) or close
/// the read end immediately to exercise early-consumer close. Requires the stage to exit
/// `0` either way. Returns the received bytes (empty when not consuming).
fn run_list(root_ns: u64, notif: u64, argv: &[&str], consume: bool) -> alloc::vec::Vec<u8> {
    use alloc::vec::Vec;
    use libstream::channel::{ChannelReceiver, IpcPort};
    use libstream::setup::{Streams, bootstrap_arg0, pipe, send_setup};

    let (st, img) = ns_lookup_wait(root_ns, b"/initramfs/sbin/list", RIGHT_MAP_READ);
    if st != 0 || img == 0 {
        return_fail(b"test-harness: list image FAIL\n");
    }
    // Depth 1: the smallest ring there is, so a stream of more than one message cannot
    // complete without the consumer draining — real backpressure, not a big buffer.
    let (rx, list_stdout) = match pipe(1) {
        Ok(p) => p,
        Err(_) => return_fail(b"test-harness: list stdout pipe FAIL\n"),
    };
    let (setup_shell, setup_stage) = match pipe(4) {
        Ok(p) => p,
        Err(_) => return_fail(b"test-harness: list setup chan FAIL\n"),
    };
    // SAFETY: SPAWN_LIST is our static, initialised here; spawns are sequential.
    let _proc = match unsafe {
        SPAWN_LIST.image = img;
        SPAWN_LIST.handles[0] = setup_stage;
        SPAWN_LIST.arg0 = bootstrap_arg0(true);
        spawn(&*(&raw const SPAWN_LIST))
    } {
        Ok(p) => p,
        Err(_) => return_fail(b"test-harness: list spawn FAIL\n"),
    };

    let streams = Streams {
        stdin: None, // a source stage: its input is the filesystem, not a pipe
        stdout: Some(list_stdout),
        stderr: None,
    };
    if send_setup(setup_shell, &streams, argv).is_err() {
        return_fail(b"test-harness: list send_setup FAIL\n");
    }

    let out = if consume {
        match ChannelReceiver::new(IpcPort::new(rx)).receive() {
            Ok(b) => b,
            Err(_) => return_fail(b"test-harness: list stdout receive FAIL\n"),
        }
    } else {
        // Close the read end without reading a byte. The stage's next send surfaces
        // `PeerClosed`, which it must treat as a clean stop.
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, rx) };
        Vec::new()
    };

    if reap_child_exit(notif) != 0 {
        return_fail(b"test-harness: list exited non-zero\n");
    }
    // SAFETY: closing our own handles (the stage's ends were moved to it). When the read
    // end was already closed above, only the setup channel is left.
    unsafe {
        if consume {
            syscall1(SYS_HANDLE_CLOSE, rx);
        }
        syscall1(SYS_HANDLE_CLOSE, setup_shell);
    }
    out
}

/// Block until a `ChildExited` notification arrives, returning the child's exit code.
///
/// The harness spawns children one at a time and reaps each before the next, so the first
/// `ChildExited` is always this child's.
fn reap_child_exit(notif: u64) -> i32 {
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
        // SAFETY: NOTIF is a valid 64-byte writable out-param.
        let r = unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) };
        if r != 0 {
            continue; // WouldBlock: re-block
        }
        // SAFETY: the kernel wrote a 64-byte Notification into NOTIF.
        let kind = unsafe { (&raw const NOTIF.kind).read() };
        if kind == KIND_CHILD_EXITED {
            // SAFETY: body[8..12] is the exit code.
            let body = unsafe { (&raw const NOTIF.body).read() };
            return i32::from_le_bytes([body[8], body[9], body[10], body[11]]);
        }
    }
}

/// **The wall clock, from ring 3** (`CLOCK_REALTIME`).
///
/// Checks the three properties a filesystem timestamp depends on: the clock is
/// readable at all, it reports a *plausible* date rather than a fabricated one, and it
/// advances. A clock that returned a fixed value, or 1970, would pass a mere
/// "syscall succeeded" check and fail all three here.
fn wall_clock_demo() {
    kprint(b"test-harness: wall-clock demo\n");
    let mut first: u64 = 0;
    // SAFETY: valid u64 out-param.
    let r = unsafe { syscall4(SYS_CLOCK_READ, CLOCK_REALTIME, (&raw mut first) as u64, 0, 0) };
    if r != 0 {
        // Not a soft failure: without a wall clock the filesystem stamps 1970 on
        // everything, so a build that cannot read one must fail loudly.
        return_fail(b"test-harness: CLOCK_REALTIME unreadable\n");
    }
    // 2024-01-01 .. 2100-01-01 in nanoseconds. Wide enough never to need touching,
    // narrow enough to catch a zero, a monotonic value mistakenly returned as
    // realtime (nanoseconds since boot is a handful of seconds), or a botched
    // BCD/epoch conversion.
    const NS_2024: u64 = 1_704_067_200_000_000_000;
    const NS_2100: u64 = 4_102_444_800_000_000_000;
    if first < NS_2024 || first > NS_2100 {
        return_fail(b"test-harness: CLOCK_REALTIME is not a plausible date\n");
    }

    // It must advance. Sleeping on a timer rather than spinning keeps this honest
    // about wall-clock progress rather than measuring loop speed.
    timer_sleep_ms(20);
    let mut second: u64 = 0;
    // SAFETY: valid u64 out-param.
    let r2 = unsafe { syscall4(SYS_CLOCK_READ, CLOCK_REALTIME, (&raw mut second) as u64, 0, 0) };
    if r2 != 0 || second <= first {
        return_fail(b"test-harness: CLOCK_REALTIME did not advance\n");
    }
    // …and it must advance at roughly wall-clock rate: a 20 ms nap cannot show up as
    // an hour. (Generous upper bound — this is catching a broken multiplier, not
    // measuring scheduler latency.)
    if second - first > 60_000_000_000 {
        return_fail(b"test-harness: CLOCK_REALTIME advanced implausibly fast\n");
    }
    kprint(b"test-harness: wall-clock ok (plausible date, advancing)\n");
}

/// **A dead stage's pipe closes** — the regression test for exit-time handle reclamation
/// (decision log, 2026-07-24).
///
/// Spawns `list` on a path that does not resolve, so it exits non-zero **without writing
/// anything** to `stdout`, and requires the consumer's read to end in `PeerClosed` rather
/// than blocking forever. That only holds if the dead process's handles are actually
/// closed at exit: while they were not, its end of this pipe stayed open, the peer's
/// `sys_channel_recv` returned `WouldBlock` for ever, and this call never returned.
///
/// This is the case the pipeline model depends on for a stage that dies early (design §1).
/// The `yes | head -1` direction — the *consumer* closing — worked all along; this is the
/// other one.
fn dead_stage_closes_its_pipe_demo(root_ns: u64, notif: u64) {
    use libstream::channel::{ChannelReceiver, IpcPort};
    use libstream::setup::{Streams, bootstrap_arg0, pipe, send_setup};
    use libstream::wire::WireError;

    kprint(b"test-harness: dead-stage pipe-close demo\n");
    let (st, img) = ns_lookup_wait(root_ns, b"/initramfs/sbin/list", RIGHT_MAP_READ);
    if st != 0 || img == 0 {
        return_fail(b"test-harness: dead-stage image FAIL\n");
    }
    let (rx, stdout) = match pipe(4) {
        Ok(p) => p,
        Err(_) => return_fail(b"test-harness: dead-stage pipe FAIL\n"),
    };
    let (setup_shell, setup_stage) = match pipe(4) {
        Ok(p) => p,
        Err(_) => return_fail(b"test-harness: dead-stage setup chan FAIL\n"),
    };
    // SAFETY: SPAWN_LIST is our static; the coreutil spawns are sequential.
    let _proc = match unsafe {
        SPAWN_LIST.image = img;
        SPAWN_LIST.handles[0] = setup_stage;
        SPAWN_LIST.arg0 = bootstrap_arg0(true);
        spawn(&*(&raw const SPAWN_LIST))
    } {
        Ok(p) => p,
        Err(_) => return_fail(b"test-harness: dead-stage spawn FAIL\n"),
    };
    let streams = Streams { stdin: None, stdout: Some(stdout), stderr: None };
    if send_setup(setup_shell, &streams, &["list", "/nx-no-such-directory"]).is_err() {
        return_fail(b"test-harness: dead-stage send_setup FAIL\n");
    }

    // The stage writes nothing and exits non-zero. This must come back `PeerClosed` —
    // not a hang, and not a spurious success.
    match ChannelReceiver::new(IpcPort::new(rx)).receive() {
        Err(WireError::PeerClosed) => {}
        Err(_) => return_fail(b"test-harness: dead-stage receive failed for the wrong reason\n"),
        Ok(_) => return_fail(b"test-harness: dead-stage produced a stream it should not have\n"),
    }
    if reap_child_exit(notif) == 0 {
        return_fail(b"test-harness: dead-stage exited 0 on an unresolvable path\n");
    }
    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, rx);
        syscall1(SYS_HANDLE_CLOSE, setup_shell);
    }
    kprint(b"test-harness: dead-stage ok (its pipe closed; peer saw PeerClosed)\n");
}

/// **`copy`, the mutation side of the filesystem** (coreutils Milestone 1 Part D).
///
/// Where the `list` demo proves the read path and the pipe, this proves that a coreutil
/// can *change* the filesystem correctly: create a file, write its contents, build a
/// directory tree, and — importantly — **refuse** the cases that would silently produce a
/// wrong result.
///
/// The fixture is built under `/system` and removed afterwards. Checks, in order:
///
/// 1. **File copy** — the destination exists with the source's exact size, and its bytes
///    match, read back through a fresh resolve (so this is what actually reached the
///    filesystem, not what is sitting in a mapping we still hold).
/// 2. **An existing destination is refused** without `--force` — exit `2` (usage), and the
///    destination is left **unchanged**, which is the part that matters: a refusal that
///    had already clobbered the file would be worse than no refusal at all.
/// 3. **`--force` overwrites** a same-size destination.
/// 4. **Overwriting a longer file shrinks it first** — the destination ends up exactly the
///    source, with no byte of the longer original surviving past the new end.
/// 5. **Directory copy is recursive** — a nested tree arrives with its files' contents.
fn copy_demo(root_ns: u64, notif: u64) {
    kprint(b"test-harness: copy demo (the mutation side)\n");

    // --- fixture: /system/nx-copy/{a.txt, sub/b.txt} -------------------------
    let mut buf = [0u8; 4096];
    let mut sys = match Dir::open(root_ns, b"/system", &mut buf) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: copy fixture open FAIL\n"),
    };
    if sys.mkdir(b"nx-copy").is_err() {
        return_fail(b"test-harness: copy fixture mkdir FAIL\n");
    }
    sys.close();
    {
        let mut b2 = [0u8; 4096];
        let mut d = match Dir::open(root_ns, b"/system/nx-copy", &mut b2) {
            Ok(d) => d,
            Err(_) => return_fail(b"test-harness: copy fixture reopen FAIL\n"),
        };
        if d.mkdir(b"sub").is_err() {
            return_fail(b"test-harness: copy fixture subdir FAIL\n");
        }
        d.close();
    }
    write_file(root_ns, b"/system/nx-copy/a.txt", COPY_CONTENT_A);
    write_file(root_ns, b"/system/nx-copy/sub/b.txt", COPY_CONTENT_B);

    // --- 1. file → file ------------------------------------------------------
    if run_copy(root_ns, notif, &["copy", "/system/nx-copy/a.txt", "/system/nx-copy/c.txt"]) != 0 {
        return_fail(b"test-harness: copy file exited non-zero\n");
    }
    if !file_matches(root_ns, b"/system/nx-copy/c.txt", COPY_CONTENT_A) {
        return_fail(b"test-harness: copied file content MISMATCH\n");
    }

    // --- 2. an existing destination is refused, and left alone ---------------
    let code = run_copy(root_ns, notif, &["copy", "/system/nx-copy/sub/b.txt", "/system/nx-copy/c.txt"]);
    if code == 0 {
        return_fail(b"test-harness: copy over an existing file wrongly succeeded\n");
    }
    if !file_matches(root_ns, b"/system/nx-copy/c.txt", COPY_CONTENT_A) {
        return_fail(b"test-harness: refused copy still modified the destination\n");
    }

    // --- 3. --force overwrites (same size) -----------------------------------
    if run_copy(root_ns, notif, &["copy", "--force", "/system/nx-copy/sub/b.txt", "/system/nx-copy/c.txt"]) != 0 {
        return_fail(b"test-harness: copy --force exited non-zero\n");
    }
    if !file_matches(root_ns, b"/system/nx-copy/c.txt", COPY_CONTENT_B) {
        return_fail(b"test-harness: --force did not overwrite\n");
    }

    // --- 4. --force onto a LONGER destination shrinks it first ---------------
    // The case that was refused outright until the filesystem gained truncate. The
    // check that matters is not merely that it succeeds: the destination must end up
    // **exactly** the source, with no byte of the longer original surviving past the
    // new end. `file_matches` compares the size first, so a stale tail fails here.
    write_file(root_ns, b"/system/nx-copy/long.txt", COPY_CONTENT_LONG);
    let code = run_copy(root_ns, notif, &["copy", "--force", "/system/nx-copy/a.txt", "/system/nx-copy/long.txt"]);
    if code != 0 {
        return_fail(b"test-harness: copy over a longer file failed\n");
    }
    if !file_matches(root_ns, b"/system/nx-copy/long.txt", COPY_CONTENT_A) {
        return_fail(b"test-harness: overwrite left a stale tail (truncate did not take)\n");
    }

    // --- 5. directory copy is recursive --------------------------------------
    if run_copy(root_ns, notif, &["copy", "/system/nx-copy/sub", "/system/nx-copy/sub2"]) != 0 {
        return_fail(b"test-harness: copy directory exited non-zero\n");
    }
    if !file_matches(root_ns, b"/system/nx-copy/sub2/b.txt", COPY_CONTENT_B) {
        return_fail(b"test-harness: recursive copy did not reproduce the tree\n");
    }

    // --- teardown ------------------------------------------------------------
    unlink_all(root_ns, b"/system/nx-copy/sub", &[b"b.txt"]);
    unlink_all(root_ns, b"/system/nx-copy/sub2", &[b"b.txt"]);
    unlink_all(
        root_ns,
        b"/system/nx-copy",
        &[b"a.txt", b"c.txt", b"long.txt"],
    );
    {
        let mut b3 = [0u8; 4096];
        let mut d = match Dir::open(root_ns, b"/system/nx-copy", &mut b3) {
            Ok(d) => d,
            Err(_) => return_fail(b"test-harness: copy teardown open FAIL\n"),
        };
        if d.rmdir(b"sub").is_err() || d.rmdir(b"sub2").is_err() {
            return_fail(b"test-harness: copy teardown rmdir FAIL\n");
        }
        d.close();
    }
    let mut b4 = [0u8; 4096];
    let mut sys = match Dir::open(root_ns, b"/system", &mut b4) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: copy teardown FAIL\n"),
    };
    if sys.rmdir(b"nx-copy").is_err() {
        return_fail(b"test-harness: copy teardown rmdir FAIL\n");
    }
    sys.close();
    kprint(b"test-harness: copy ok (file, recursive dir, overwrite-with-shrink)\n");
}

/// Fixture contents. `A` and `B` are the same length so the `--force` overwrite in step 3
/// is a same-size write (the case that *is* supported), while `LONG` is longer than `A` so
/// step 4 hits the no-truncate refusal.
const COPY_CONTENT_A: &[u8] = b"alpha content, fixed length.\n";
const COPY_CONTENT_B: &[u8] = b"bravo content, fixed length.\n";
const COPY_CONTENT_LONG: &[u8] =
    b"a much longer destination whose tail would survive a short overwrite\n";

/// Spawn `copy` with `argv` as a Tier-1 stage (stdout wired, so its report table is
/// produced and drained) and return its exit code.
/// **Milestone 2 Part A — `mkdir` and `remove`.**
///
/// The directory verbs, driven as real Tier-1 stages. Both need operands, so neither can
/// be exercised by a bare Tier-0 spawn — `argv` only arrives in a setup message.
///
/// The cases here are the ones where an implementation can look right and be wrong:
/// `--parents` idempotence (which cannot be decided from the error code, since the
/// filesystem reports "already exists" and "not empty" identically), the `--recursive`
/// safety rail, `--force` suppressing *only* absence, and — the one with teeth —
/// `remove` refusing a **namespace binding**, so that a recursive delete can never
/// unbind a mount point.
fn mkdir_remove_demo(root_ns: u64, notif: u64) {
    kprint(b"test-harness: mkdir/remove demo (Milestone 2 Part A)\n");
    const MK: &[u8] = b"/initramfs/sbin/mkdir";
    const RM: &[u8] = b"/initramfs/sbin/remove";

    // --- 1. mkdir creates, and refuses an existing path ----------------------
    if run_coreutil(root_ns, notif, MK, &["mkdir", "/system/nx-a"]) != 0 {
        return_fail(b"test-harness: mkdir exited non-zero\n");
    }
    if !path_exists(root_ns, b"/system/nx-a") {
        return_fail(b"test-harness: mkdir did not create the directory\n");
    }
    if run_coreutil(root_ns, notif, MK, &["mkdir", "/system/nx-a"]) == 0 {
        return_fail(b"test-harness: mkdir over an existing path wrongly succeeded\n");
    }

    // --- 2. --parents builds a chain, and is idempotent ----------------------
    // Two assertions in one: intermediates get created, and re-running is a success
    // rather than an error. The second is the reason the flag relaxes the exists check
    // and not only the parent check.
    if run_coreutil(root_ns, notif, MK, &["mkdir", "--parents", "/system/nx-a/b/c"]) != 0 {
        return_fail(b"test-harness: mkdir --parents exited non-zero\n");
    }
    if !path_exists(root_ns, b"/system/nx-a/b/c") {
        return_fail(b"test-harness: mkdir --parents did not build the chain\n");
    }
    if run_coreutil(root_ns, notif, MK, &["mkdir", "--parents", "/system/nx-a/b/c"]) != 0 {
        return_fail(b"test-harness: mkdir --parents was not idempotent\n");
    }

    // --- 3. remove refuses a directory without --recursive, and leaves it ----
    if run_coreutil(root_ns, notif, RM, &["remove", "/system/nx-a"]) == 0 {
        return_fail(b"test-harness: remove of a directory wrongly succeeded\n");
    }
    if !path_exists(root_ns, b"/system/nx-a") {
        return_fail(b"test-harness: refused remove still deleted the directory\n");
    }

    // --- 4. remove takes a file ----------------------------------------------
    write_file(root_ns, b"/system/nx-a/f.txt", b"part a\n");
    if run_coreutil(root_ns, notif, RM, &["remove", "/system/nx-a/f.txt"]) != 0 {
        return_fail(b"test-harness: remove file exited non-zero\n");
    }
    if path_exists(root_ns, b"/system/nx-a/f.txt") {
        return_fail(b"test-harness: remove did not delete the file\n");
    }

    // --- 5. a missing path: an error, unless --force -------------------------
    if run_coreutil(root_ns, notif, RM, &["remove", "/system/nx-a/gone.txt"]) == 0 {
        return_fail(b"test-harness: remove of a missing path wrongly succeeded\n");
    }
    if run_coreutil(root_ns, notif, RM, &["remove", "--force", "/system/nx-a/gone.txt"]) != 0 {
        return_fail(b"test-harness: remove --force on a missing path failed\n");
    }

    // --- 6. a namespace binding is refused ----------------------------------
    // The property that keeps a recursive delete from unbinding a mount point.
    //
    // `/dev` is the case that **isolates** the check, and picking it took a second
    // attempt: `/dev/console` fails with or without the check, because `/dev` is not a
    // filesystem directory to open — so asserting on it proves nothing about the check
    // itself. `/dev` *is* a binding directly beneath a real filesystem directory (`/`),
    // so without the refusal it classifies as "missing", and `--force` turns that into a
    // silent exit 0: a no-op reported as success. With the refusal it is a named error.
    if run_coreutil(root_ns, notif, RM, &["remove", "--force", "/dev"]) == 0 {
        return_fail(b"test-harness: remove of a namespace binding wrongly succeeded\n");
    }
    if !path_exists(root_ns, b"/dev/console") {
        return_fail(b"test-harness: refused remove disturbed the console binding\n");
    }

    // --- 7. --recursive takes the whole tree ---------------------------------
    write_file(root_ns, b"/system/nx-a/b/c/deep.txt", b"deep\n");
    if run_coreutil(root_ns, notif, RM, &["remove", "--recursive", "/system/nx-a"]) != 0 {
        return_fail(b"test-harness: remove --recursive exited non-zero\n");
    }
    if path_exists(root_ns, b"/system/nx-a") {
        return_fail(b"test-harness: remove --recursive left the tree behind\n");
    }

    kprint(b"test-harness: mkdir/remove ok (created, refused, forced, recursed, binding safe)\n");
}

fn run_copy(root_ns: u64, notif: u64, argv: &[&str]) -> i32 {
    run_coreutil(root_ns, notif, b"/initramfs/sbin/copy", argv)
}

/// Spawn a coreutil as a **Tier-1 stage** — setup message, `argv`, a `stdout` pipe — and
/// return its exit status.
///
/// Tier 1 rather than a bare spawn because most coreutils are meaningless without
/// operands, and `argv` only arrives in the setup message. Generic over the image path
/// so `copy`, `mkdir` and `remove` share one spawn rather than three copies of it.
fn run_coreutil(root_ns: u64, notif: u64, image: &[u8], argv: &[&str]) -> i32 {
    use libstream::channel::{ChannelReceiver, IpcPort};
    use libstream::setup::{Streams, bootstrap_arg0, pipe, send_setup};

    let (st, img) = ns_lookup_wait(root_ns, image, RIGHT_MAP_READ);
    if st != 0 || img == 0 {
        return_fail(b"test-harness: coreutil image FAIL\n");
    }
    let (rx, stdout) = match pipe(4) {
        Ok(p) => p,
        Err(_) => return_fail(b"test-harness: copy stdout pipe FAIL\n"),
    };
    let (setup_shell, setup_stage) = match pipe(4) {
        Ok(p) => p,
        Err(_) => return_fail(b"test-harness: copy setup chan FAIL\n"),
    };
    // SAFETY: SPAWN_LIST is our static (shared by the coreutil spawns, which are
    // sequential); initialised here for this spawn.
    let _proc = match unsafe {
        SPAWN_LIST.image = img;
        SPAWN_LIST.handles[0] = setup_stage;
        SPAWN_LIST.arg0 = bootstrap_arg0(true);
        spawn(&*(&raw const SPAWN_LIST))
    } {
        Ok(p) => p,
        Err(_) => return_fail(b"test-harness: copy spawn FAIL\n"),
    };
    let streams = Streams {
        stdin: None,
        stdout: Some(stdout),
        stderr: None,
    };
    if send_setup(setup_shell, &streams, argv).is_err() {
        return_fail(b"test-harness: copy send_setup FAIL\n");
    }
    // Drain first, then reap — and note that a **failing** `copy` exits without writing
    // its report at all. That case is the point: the receive must end in `PeerClosed`,
    // which requires the dead stage's end of this pipe to actually close. It only does
    // because a process's handles are now swept at exit (decision log, 2026-07-24); with
    // that sweep missing this call hangs forever, so this demo is the regression test for
    // it.
    let _ = ChannelReceiver::new(IpcPort::new(rx)).receive();
    let code = reap_child_exit(notif);
    drop(_proc);
    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, rx);
        syscall1(SYS_HANDLE_CLOSE, setup_shell);
    }
    code
}

/// **`list /dev` — a kernel-served directory is listable** (Slice D3).
///
/// Nothing is *mounted* at `/dev`: its contents are namespace bindings the kernel serves
/// (`entropy`, `blk`, `console`, `log`). `list` used to fail there, because it only knew how
/// to open an fs-server directory session. It now takes the union of the filesystem under a
/// path and the bindings directly beneath it — which is how mount points have always shown
/// up in a parent listing — so `/dev` needs no special case.
///
/// Checks:
///
/// 1. **`list /dev` succeeds and names a known binding.** The interesting half: there is no
///    filesystem here at all, so every row comes from namespace enumeration.
/// 2. **`list /` still shows filesystem entries**, so adding the binding source did not
///    replace the fs one. Without this the demo would pass against a `list` that had
///    stopped reading filesystems entirely.
fn dev_listing_demo(root_ns: u64, notif: u64) {
    kprint(b"test-harness: dev-listing demo (a kernel-served directory is listable)\n");

    let dev = run_list(root_ns, notif, &["list", "/dev"], true);
    if !contains(&dev, b"entropy") {
        return_fail(b"test-harness: list /dev did not name the entropy binding\n");
    }

    // `/` is the case that genuinely needs both sources: the root filesystem's own entries
    // plus the bindings alongside them.
    let root = run_list(root_ns, notif, &["list", "/"], true);
    if !contains(&root, b"system") {
        return_fail(b"test-harness: list / lost its filesystem entries\n");
    }
    if !contains(&root, b"dev") {
        return_fail(b"test-harness: list / did not show the /dev binding\n");
    }

    kprint(b"test-harness: dev-listing ok (/dev from bindings, / from both sources)\n");
}

/// Is `needle` a subsequence of contiguous bytes in `hay`? The listing arrives as a TSM1
/// stream; a name appears in it verbatim, which is enough to assert on without decoding.
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// **`mtime` survives an in-place overwrite** (Slice C4).
///
/// The timestamp gap users actually notice. Under Model A the kernel owns the file-data
/// path, so a **same-length** rewrite — map the file, change the bytes, `sys_file_sync` —
/// reaches the device with no resolve and no IPC. The fs-server never hears about it, so
/// before C4 nothing moved the inode's `mtime`: a file edited ten times in place still
/// reported the moment it was *created*.
///
/// The check has to cross a wall-clock second, because ext4 second-granularity `mtime`
/// cannot show a change finer than that. So: create, note `mtime`, wait for the clock's
/// second to tick over, overwrite in place, and re-read.
///
/// Checks, in order:
///
/// 1. **The content really changed** — otherwise a stale-`mtime` bug and a
///    silently-failed-write bug look identical, and the interesting one is hidden.
/// 2. **`mtime` advanced** — the actual fix.
/// 3. **The size did not change** — proving this went through the in-place path and not
///    an accidental grow/truncate, which would have stamped `mtime` the old way and made
///    the check pass for the wrong reason.
fn mtime_overwrite_demo(root_ns: u64) {
    kprint(b"test-harness: mtime demo (an in-place overwrite is not invisible)\n");

    let mut buf = [0u8; 4096];
    let mut sys = match Dir::open(root_ns, b"/system", &mut buf) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: mtime fixture open FAIL\n"),
    };
    if sys.mkdir(b"nx-mtime").is_err() {
        return_fail(b"test-harness: mtime fixture mkdir FAIL\n");
    }
    sys.close();
    write_file(root_ns, b"/system/nx-mtime/f.txt", MTIME_BEFORE);

    let (before_mtime, before_size) = stat_entry(root_ns, b"/system/nx-mtime", b"f.txt");
    if before_mtime == 0 {
        return_fail(b"test-harness: fixture has no mtime (wall clock unset?)\n");
    }

    // Cross a wall-clock second: ext4 stores whole seconds, so an overwrite inside the
    // same second is genuinely indistinguishable from no overwrite at all.
    wait_for_next_second();

    // The case under test: same length, written straight into the mapping, flushed.
    overwrite_in_place(root_ns, b"/system/nx-mtime/f.txt", MTIME_AFTER);

    // --- 1. the content really changed ---------------------------------------
    if !file_matches(root_ns, b"/system/nx-mtime/f.txt", MTIME_AFTER) {
        return_fail(b"test-harness: in-place overwrite did not take\n");
    }

    // --- 2 + 3. mtime moved, size did not ------------------------------------
    let (after_mtime, after_size) = stat_entry(root_ns, b"/system/nx-mtime", b"f.txt");
    if after_mtime <= before_mtime {
        return_fail(b"test-harness: in-place overwrite left mtime stale\n");
    }
    if after_size != before_size {
        return_fail(b"test-harness: overwrite changed the size, not the in-place path\n");
    }

    // --- teardown -------------------------------------------------------------
    unlink_all(root_ns, b"/system/nx-mtime", &[b"f.txt"]);
    let mut b2 = [0u8; 4096];
    let mut sys = match Dir::open(root_ns, b"/system", &mut b2) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: mtime teardown FAIL\n"),
    };
    if sys.rmdir(b"nx-mtime").is_err() {
        return_fail(b"test-harness: mtime teardown rmdir FAIL\n");
    }
    sys.close();
    kprint(b"test-harness: mtime ok (in-place overwrite moved mtime, size unchanged)\n");
}

/// Fixture contents for [`mtime_overwrite_demo`] — **the same length on purpose**. A
/// different length would resize the file, which reaches the server through the ordinary
/// resolve path and would stamp `mtime` even without C4.
const MTIME_BEFORE: &[u8] = b"the original contents, fixed.\n";
const MTIME_AFTER: &[u8] = b"the rewritten contents, same.\n";

/// `(mtime, size)` of `name` inside directory `dir`, via a `ReadDir` listing.
fn stat_entry(ns: u64, dir: &[u8], name: &[u8]) -> (i64, u64) {
    let mut buf = [0u8; 4096];
    let mut d = match Dir::open(ns, dir, &mut buf) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: stat open FAIL\n"),
    };
    let mut found = (0i64, 0u64);
    let r = d.read_dir(|e| {
        if e.name == name {
            found = (e.mtime, e.size);
            return false; // stop early
        }
        true
    });
    d.close();
    if r.is_err() {
        return_fail(b"test-harness: stat ReadDir FAIL\n");
    }
    found
}

/// Block until the realtime clock's *second* changes, so a subsequent write lands in a
/// second distinguishable from the previous one. Bounded — if the clock is not running,
/// the caller's check fails on its own rather than hanging here.
fn wait_for_next_second() {
    let start = realtime_secs();
    for _ in 0..40 {
        if realtime_secs() != start {
            return;
        }
        timer_sleep_ms(100);
    }
}

/// The realtime clock, in whole seconds (`0` if unset).
fn realtime_secs() -> i64 {
    // SAFETY: CLOCK_BUF is a valid writable u64 out-param.
    let r = unsafe { syscall2(SYS_CLOCK_READ, CLOCK_REALTIME, (&raw mut CLOCK_BUF) as u64) };
    if r != 0 {
        return 0;
    }
    // SAFETY: the syscall wrote 8 bytes.
    (unsafe { (&raw const CLOCK_BUF).read() } / 1_000_000_000) as i64
}

/// Overwrite `path` with `content` **in place**: resolve it (no size change at all), map it
/// writable, write the bytes, and `sys_file_sync`. `content` must be the file's current
/// length — the point is to exercise the path that never reaches the server.
fn overwrite_in_place(ns: u64, path: &[u8], content: &[u8]) {
    let size = content.len() as u64;
    let (st, fh) = ns_lookup_wait(ns, path, RIGHT_MAP_READ | RIGHT_MAP_WRITE);
    if st != 0 || fh == 0 {
        return_fail(b"test-harness: in-place resolve FAIL\n");
    }
    // SAFETY: mapping our own writable file handle.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, fh, 0, size, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        return_fail(b"test-harness: in-place map FAIL\n");
    }
    for (i, b) in content.iter().enumerate() {
        // SAFETY: `i` is within the `size`-byte mapping.
        unsafe { ((addr as u64 + i as u64) as *mut u8).write_volatile(*b) };
    }
    // SAFETY: flushing, unmapping, and closing our own handle.
    let synced = unsafe { syscall1(SYS_FILE_SYNC, fh) };
    // SAFETY: as above.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, addr as u64, size);
        syscall1(SYS_HANDLE_CLOSE, fh);
    }
    if synced != 0 {
        return_fail(b"test-harness: in-place sync FAIL\n");
    }
}

/// **Server fan-out: more concurrent directory sessions than the old cap** (Slice C3).
///
/// `fs-server-ext4` waits on one `sys_wait` set holding its serving endpoint plus every
/// live directory session, so the kernel's `MAX_WAIT_HANDLES` *is* its client ceiling. It
/// was 8, giving 7 sessions — which a shell pipeline reaches in normal use, not under
/// stress. Raising it is only meaningful if the server genuinely serves the extra ones, so
/// this opens the full set at once and then *uses* one from well past the old limit.
///
/// Everything here is derived from [`MAX_WAIT_HANDLES`] rather than written out, so the
/// check tracks the constant instead of pinning yesterday's number.
///
/// Checks, in order:
///
/// 1. **All `MAX_SESSIONS` open concurrently** — every one of them, held simultaneously.
/// 2. **One past the old cap of 7 actually works** — a real `ReadDir` on a late session.
///    Opening a handle proves nothing on its own; the wait set has to cover it too.
/// 3. **The cap is still enforced** — the next open past the table is refused, and refused
///    *cleanly* (`WouldBlock`), not by wedging the server.
/// 4. **Slots come back on close** — after closing them all, a fresh open succeeds. Without
///    this, the test would pass just as well against a server that leaked every slot.
fn session_fanout_demo(root_ns: u64) {
    use librsproto::session::DIR_SESSION_RIGHTS;
    kprint(b"test-harness: session fan-out demo (past the old 7-session cap)\n");

    /// The server's ceiling: its wait set is `serve_end` + one slot per session.
    const MAX_SESSIONS: usize = MAX_WAIT_HANDLES - 1;
    /// The limit this replaced — the point of the check is to work well beyond it.
    const OLD_CAP: usize = 7;

    let mut held = [0u64; MAX_SESSIONS];

    // --- 1. open the whole table at once -------------------------------------
    for (i, slot) in held.iter_mut().enumerate() {
        let (st, h) = ns_lookup_wait(root_ns, b"/system", DIR_SESSION_RIGHTS);
        if st != 0 || h == 0 {
            // Report how far we got: "stopped at 7" and "stopped at 20" are very
            // different failures, and the count is the whole diagnosis.
            kprint(b"test-harness: session fan-out stopped early at session ");
            kprint_hex(i as u64);
            kprint(b"\n");
            return_fail(b"test-harness: could not open the full session table\n");
        }
        *slot = h;
    }

    // --- 2. a session past the old cap is really served ----------------------
    // The one that matters. A handle can exist without the server ever waiting on it, in
    // which case this round trip is what hangs (and the run's wall-clock timeout catches
    // it) rather than returning wrong data.
    {
        let mut buf = [0u8; 4096];
        let late = held[MAX_SESSIONS - 1];
        let mut d = match Dir::from_endpoint(late, &mut buf) {
            Ok(d) => d,
            Err(_) => return_fail(b"test-harness: wrapping a late session FAILED\n"),
        };
        let mut seen = 0usize;
        if d.read_dir(|_| {
            seen += 1;
            true
        })
        .is_err()
        {
            return_fail(b"test-harness: ReadDir on a session past the old cap FAILED\n");
        }
        if seen == 0 {
            return_fail(b"test-harness: late session listed nothing (/system is not empty)\n");
        }
        // `from_endpoint` took ownership; hand it back so the teardown below closes once.
        core::mem::forget(d);
    }
    if MAX_SESSIONS <= OLD_CAP {
        return_fail(b"test-harness: session cap did not actually move past 7\n");
    }

    // --- 3. one past the table is refused cleanly ----------------------------
    let (st, h) = ns_lookup_wait(root_ns, b"/system", DIR_SESSION_RIGHTS);
    if st == 0 && h != 0 {
        return_fail(b"test-harness: opening past the session table wrongly succeeded\n");
    }
    if st != KERR_WOULD_BLOCK {
        return_fail(b"test-harness: a full session table did not report WouldBlock\n");
    }

    // --- 4. closing frees the slots ------------------------------------------
    for h in held.iter() {
        // SAFETY: closing session handles this process owns.
        unsafe { syscall1(SYS_HANDLE_CLOSE, *h) };
    }
    // The server reclaims a slot when it observes `PeerClosed` on that endpoint, which it
    // does on its **next wait** — so reclamation is eventual, not synchronous, and closing
    // then immediately re-opening races the server. This check used to assert the eventual
    // property on the first attempt and lost that race roughly one boot in twenty under
    // host load, reporting a reclamation failure that had not happened. (The two-pass serve
    // loop orders reclaims ahead of new opens *within one wait batch*; it cannot help when
    // the close notifications and the open land in different batches.)
    //
    // Retry on `WouldBlock`, sleeping between attempts so the server is actually scheduled
    // rather than spun against. This costs nothing on the happy path — the first attempt
    // normally succeeds and never sleeps — and a genuinely broken reclamation still fails,
    // just after the full budget instead of immediately.
    let (mut st, mut h) = (KERR_WOULD_BLOCK, 0u64);
    for attempt in 0..RECLAIM_ATTEMPTS {
        let r = ns_lookup_wait(root_ns, b"/system", DIR_SESSION_RIGHTS);
        st = r.0;
        h = r.1;
        // Any other status is a real answer (success or a genuine error) — stop and judge
        // it below rather than burning the budget.
        if st != KERR_WOULD_BLOCK {
            break;
        }
        if attempt + 1 < RECLAIM_ATTEMPTS {
            timer_sleep_ms(RECLAIM_POLL_MS);
        }
    }
    if st != 0 || h == 0 {
        return_fail(b"test-harness: session slots were not reclaimed on close\n");
    }
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, h) };

    kprint(b"test-harness: session fan-out ok (full table open, served, refused, reclaimed)\n");
}

/// `KError::WouldBlock`, the status a full session table reports.
const KERR_WOULD_BLOCK: i32 = -11;

/// Budget for the server to reclaim closed session slots: attempts, and the pause
/// between them. ~2 s in total — orders of magnitude more than the server needs (it
/// reclaims on its very next wait), while still failing in bounded time if reclamation
/// is genuinely broken rather than merely not-yet-happened.
const RECLAIM_ATTEMPTS: u32 = 40;
/// Pause between reclamation retries. Long enough that the fs-server is scheduled and
/// drains its close notifications, rather than the harness spinning against it.
const RECLAIM_POLL_MS: u64 = 50;

/// **`rename` — re-pointing a name instead of copying it** (Slice C2).
///
/// `copy_demo` proved the filesystem can be *written*; this proves a name can be
/// **re-pointed**, which is what `move` needs and what copy-then-unlink only approximates.
/// No file data moves at all: a directory entry changes which inode it names, so a reader
/// sees either the old name or the new one and never a half-written duplicate.
///
/// Checks, in order:
///
/// 1. **Within one directory** — the plain case. The new name has the content *and* the old
///    name is gone; checking only the first would pass for a copy.
/// 2. **Across directories** — the case `move` actually needs, and the one the ext4 layer
///    grew `dir_repoint` for: the entry leaves one directory's block and enters another's.
/// 3. **An occupied destination is refused** without `RENAME_REPLACE` — and, the part that
///    matters, *both* paths survive untouched. A refusal that had already unlinked the
///    source would be worse than no refusal.
/// 4. **`RENAME_REPLACE` overwrites**, and the replaced file's content is really gone.
/// 5. **A missing source fails** rather than conjuring a destination.
/// 6. **A cross-filesystem destination is refused** with `Unsupported` rather than attempted.
///    This one is the *kernel's* check, not the server's: `/system` is the ext4 mount and
///    `/initramfs` is a different binding, so the request must never reach a server at all.
///    It is also what tells `move` to fall back to copy + unlink (POSIX spells it `EXDEV`).
fn rename_demo(root_ns: u64) {
    use librsproto::namespace::RENAME_REPLACE;
    kprint(b"test-harness: rename demo (re-point a name, don't copy it)\n");

    // --- fixture: /system/nx-ren/{a.txt, b.txt, sub/} ------------------------
    let mut buf = [0u8; 4096];
    let mut sys = match Dir::open(root_ns, b"/system", &mut buf) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: rename fixture open FAIL\n"),
    };
    if sys.mkdir(b"nx-ren").is_err() {
        return_fail(b"test-harness: rename fixture mkdir FAIL\n");
    }
    sys.close();
    {
        let mut b2 = [0u8; 4096];
        let mut d = match Dir::open(root_ns, b"/system/nx-ren", &mut b2) {
            Ok(d) => d,
            Err(_) => return_fail(b"test-harness: rename fixture reopen FAIL\n"),
        };
        if d.mkdir(b"sub").is_err() {
            return_fail(b"test-harness: rename fixture subdir FAIL\n");
        }
        d.close();
    }
    write_file(root_ns, b"/system/nx-ren/a.txt", RENAME_CONTENT_A);
    write_file(root_ns, b"/system/nx-ren/b.txt", RENAME_CONTENT_B);

    // --- 1. within one directory ---------------------------------------------
    if rename_wait(root_ns, b"/system/nx-ren/a.txt", b"/system/nx-ren/a2.txt", 0) != 0 {
        return_fail(b"test-harness: rename within a directory FAILED\n");
    }
    if !file_matches(root_ns, b"/system/nx-ren/a2.txt", RENAME_CONTENT_A) {
        return_fail(b"test-harness: renamed file has the wrong content\n");
    }
    if path_exists(root_ns, b"/system/nx-ren/a.txt") {
        return_fail(b"test-harness: rename left the old name behind (it copied)\n");
    }

    // --- 2. across directories ------------------------------------------------
    if rename_wait(root_ns, b"/system/nx-ren/a2.txt", b"/system/nx-ren/sub/a3.txt", 0) != 0 {
        return_fail(b"test-harness: cross-directory rename FAILED\n");
    }
    if !file_matches(root_ns, b"/system/nx-ren/sub/a3.txt", RENAME_CONTENT_A) {
        return_fail(b"test-harness: cross-directory rename lost the content\n");
    }
    if path_exists(root_ns, b"/system/nx-ren/a2.txt") {
        return_fail(b"test-harness: cross-directory rename left the source behind\n");
    }

    // --- 3. an occupied destination is refused, and both paths survive --------
    let st = rename_wait(root_ns, b"/system/nx-ren/sub/a3.txt", b"/system/nx-ren/b.txt", 0);
    if st == 0 {
        return_fail(b"test-harness: rename over an existing file wrongly succeeded\n");
    }
    if !file_matches(root_ns, b"/system/nx-ren/b.txt", RENAME_CONTENT_B) {
        return_fail(b"test-harness: refused rename still clobbered the destination\n");
    }
    if !file_matches(root_ns, b"/system/nx-ren/sub/a3.txt", RENAME_CONTENT_A) {
        return_fail(b"test-harness: refused rename still removed the source\n");
    }

    // --- 4. RENAME_REPLACE overwrites ----------------------------------------
    let st = rename_wait(
        root_ns,
        b"/system/nx-ren/sub/a3.txt",
        b"/system/nx-ren/b.txt",
        RENAME_REPLACE as u64,
    );
    if st != 0 {
        return_fail(b"test-harness: RENAME_REPLACE FAILED\n");
    }
    if !file_matches(root_ns, b"/system/nx-ren/b.txt", RENAME_CONTENT_A) {
        return_fail(b"test-harness: RENAME_REPLACE did not replace the destination\n");
    }
    if path_exists(root_ns, b"/system/nx-ren/sub/a3.txt") {
        return_fail(b"test-harness: RENAME_REPLACE left the source behind\n");
    }

    // --- 5. a missing source fails -------------------------------------------
    if rename_wait(root_ns, b"/system/nx-ren/nope.txt", b"/system/nx-ren/c.txt", 0) == 0 {
        return_fail(b"test-harness: rename of a missing source wrongly succeeded\n");
    }
    if path_exists(root_ns, b"/system/nx-ren/c.txt") {
        return_fail(b"test-harness: failed rename created the destination anyway\n");
    }

    // --- 6. a cross-filesystem destination is refused, not attempted ----------
    let st = rename_wait(root_ns, b"/system/nx-ren/b.txt", b"/initramfs/b.txt", 0);
    if st != KERR_UNSUPPORTED {
        return_fail(b"test-harness: cross-filesystem rename was not refused with Unsupported\n");
    }
    if !file_matches(root_ns, b"/system/nx-ren/b.txt", RENAME_CONTENT_A) {
        return_fail(b"test-harness: refused cross-filesystem rename disturbed the source\n");
    }
    // …and with the *source* off the filesystem, which takes a different path through the
    // kernel: the source resolves to a kernel server, so there is no forwarding arm to
    // catch the verdict.
    let st = rename_wait(root_ns, b"/initramfs/sbin/copy", b"/system/nx-ren/copy", 0);
    if st != KERR_UNSUPPORTED {
        return_fail(b"test-harness: rename off a non-filesystem source was not refused\n");
    }
    if path_exists(root_ns, b"/system/nx-ren/copy") {
        return_fail(b"test-harness: refused rename created a destination\n");
    }

    // --- teardown -------------------------------------------------------------
    unlink_all(root_ns, b"/system/nx-ren", &[b"b.txt"]);
    {
        let mut b3 = [0u8; 4096];
        let mut d = match Dir::open(root_ns, b"/system/nx-ren", &mut b3) {
            Ok(d) => d,
            Err(_) => return_fail(b"test-harness: rename teardown open FAIL\n"),
        };
        if d.rmdir(b"sub").is_err() {
            return_fail(b"test-harness: rename teardown rmdir FAIL\n");
        }
        d.close();
    }
    let mut b4 = [0u8; 4096];
    let mut sys = match Dir::open(root_ns, b"/system", &mut b4) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: rename teardown FAIL\n"),
    };
    if sys.rmdir(b"nx-ren").is_err() {
        return_fail(b"test-harness: rename teardown rmdir FAIL\n");
    }
    sys.close();
    kprint(b"test-harness: rename ok (same dir, cross dir, replace, refusals)\n");
}

/// Fixture contents for [`rename_demo`]. Different lengths, so a check that the content
/// followed the name cannot pass by accident on a same-size file.
const RENAME_CONTENT_A: &[u8] = b"alpha, the file that moves.\n";
const RENAME_CONTENT_B: &[u8] = b"bravo, the destination that is already occupied.\n";

/// `KError::Unsupported`, the status a cross-filesystem rename completes with.
const KERR_UNSUPPORTED: i32 = -52;

/// Issue a `sys_file_rename` and wait for its completion, returning the status. A rename
/// resolves to no object, so there is no handle to install or close.
fn rename_wait(ns: u64, src: &[u8], dst: &[u8], flags: u64) -> i32 {
    // SAFETY: two valid path slices + a namespace handle.
    let po = unsafe {
        syscall6(
            SYS_FILE_RENAME,
            ns,
            src.as_ptr() as u64,
            src.len() as u64,
            dst.as_ptr() as u64,
            dst.len() as u64,
            flags,
        )
    };
    if po < 0 {
        return po as i32;
    }
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    unsafe {
        WAIT_HANDLES[0] = po as u64;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        );
    }
    // SAFETY: the wait wrote one 24-byte `IoResult`; `status` is at offset 8.
    let status = unsafe {
        i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]])
    };
    // SAFETY: closing our own PO handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
    status
}

/// Does `path` resolve? The "old name is gone" half of a rename check — [`file_matches`]
/// cannot tell *absent* from *present with different content*, and both would be bugs.
fn path_exists(ns: u64, path: &[u8]) -> bool {
    let (st, h) = ns_lookup_wait(ns, path, RIGHT_MAP_READ);
    if h != 0 {
        // SAFETY: closing a handle just installed into our table.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
    }
    st == 0 && h != 0
}

/// Create `path` with exactly `content` (the fixture writer).
fn write_file(ns: u64, path: &[u8], content: &[u8]) {
    let size = content.len() as u64;
    // SAFETY: valid path slice + namespace handle.
    let po = unsafe {
        syscall5(
            SYS_FILE_CREATE,
            ns,
            path.as_ptr() as u64,
            path.len() as u64,
            RIGHT_MAP_READ | RIGHT_MAP_WRITE,
            size,
        )
    };
    if po < 0 {
        return_fail(b"test-harness: fixture create FAIL\n");
    }
    let (st, fh) = po_wait_pair(po as u64);
    if st != 0 || fh == 0 {
        return_fail(b"test-harness: fixture create FAIL\n");
    }
    // SAFETY: mapping our own writable file handle.
    let addr = unsafe {
        syscall4(SYS_MEMORY_MAP, fh, 0, size, RIGHT_MAP_READ | RIGHT_MAP_WRITE)
    };
    if addr < 0 {
        return_fail(b"test-harness: fixture map FAIL\n");
    }
    for (i, b) in content.iter().enumerate() {
        // SAFETY: `i` is within the `size`-byte mapping.
        unsafe { ((addr as u64 + i as u64) as *mut u8).write_volatile(*b) };
    }
    // SAFETY: flushing, unmapping, and closing our own handle.
    unsafe {
        syscall1(SYS_FILE_SYNC, fh);
        syscall2(SYS_MEMORY_UNMAP, addr as u64, size);
        syscall1(SYS_HANDLE_CLOSE, fh);
    }
}

/// Whether `path` resolves to a file of exactly `expect`'s length and bytes.
///
/// Resolves fresh, so this reads what reached the filesystem rather than a mapping the
/// writer still holds.
fn file_matches(ns: u64, path: &[u8], expect: &[u8]) -> bool {
    let (st, fh) = ns_lookup_wait(ns, path, RIGHT_MAP_READ | RIGHT_INSPECT);
    if st != 0 || fh == 0 {
        return false;
    }
    let mut info = HandleInfo {
        rights: 0,
        object_type: 0,
        generation: 0,
        size: 0,
    };
    // SAFETY: a real, correctly sized `HandleInfo` out-param.
    let r = unsafe { syscall2(SYS_HANDLE_STAT, fh, (&raw mut info) as u64) };
    if r != 0 || info.size != expect.len() as u64 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
        return false;
    }
    // SAFETY: mapping a readable file handle.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, fh, 0, info.size, RIGHT_MAP_READ) };
    if addr < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
        return false;
    }
    let mut ok = true;
    for (i, b) in expect.iter().enumerate() {
        // SAFETY: `i` is within the mapped `info.size` bytes.
        let got = unsafe { ((addr as u64 + i as u64) as *const u8).read_volatile() };
        if got != *b {
            ok = false;
            break;
        }
    }
    // SAFETY: unmapping + closing our own resources.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, addr as u64, info.size);
        syscall1(SYS_HANDLE_CLOSE, fh);
    }
    ok
}

/// Remove the named entries from `dir` (fixture teardown).
fn unlink_all(ns: u64, dir: &[u8], names: &[&[u8]]) {
    let mut buf = [0u8; 4096];
    let mut d = match Dir::open(ns, dir, &mut buf) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: teardown open FAIL\n"),
    };
    for name in names {
        if d.unlink(name).is_err() {
            return_fail(b"test-harness: teardown unlink FAIL\n");
        }
    }
    d.close();
}

/// `sys_wait` on a `PendingOperation`, returning `(status, result)` and closing it.
fn po_wait_pair(po: u64) -> (i32, u64) {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid single-waiter buffers.
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
    let (status, result) = unsafe {
        (
            i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]),
            u64::from_le_bytes([
                WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
                WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
            ]),
        )
    };
    // SAFETY: closing the PO we own.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
    if waited != 1 { (-1, 0) } else { (status, result) }
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
    use libstream::setup::{Streams, bootstrap_arg0, pipe, send_setup};
    // Each worker's seed is passed conforming — via `argv` in the setup message
    // (`["fp", "<seed>"]`), never a role field in `arg0`.
    const SEEDS: [&str; FP_WORKERS] = ["1", "2", "3"];
    let (st, img) = ns_lookup_wait(root_ns, b"/initramfs/sbin/test-stage", RIGHT_MAP_READ);
    if st != 0 || img == 0 {
        kprint(b"test-harness: hard-float image lookup FAIL\n");
        exit(1);
    }
    let mut procs = [const { None }; FP_WORKERS];
    for (i, slot) in procs.iter_mut().enumerate() {
        // A per-worker setup channel; the stage's bootstrap endpoint = `setup_stage`.
        let (setup_shell, setup_stage) = match pipe(4) {
            Ok(p) => p,
            Err(_) => {
                kprint(b"test-harness: hard-float setup chan FAIL\n");
                exit(1);
            }
        };
        // SAFETY: SPAWN_STAGE is our static, exclusively written + read here.
        let spawned = unsafe {
            SPAWN_STAGE.image = img;
            SPAWN_STAGE.handles[0] = setup_stage;
            SPAWN_STAGE.arg0 = bootstrap_arg0(true);
            spawn(&*(&raw const SPAWN_STAGE))
        };
        match spawned {
            Ok(p) => *slot = Some(p),
            Err(_) => {
                kprint(b"test-harness: hard-float spawn FAIL\n");
                exit(1);
            }
        }
        // Wire the worker: run the "fp" role with this seed (no streams).
        let streams = Streams { stdin: None, stdout: None, stderr: None };
        if send_setup(setup_shell, &streams, &["fp", SEEDS[i]]).is_err() {
            kprint(b"test-harness: hard-float send_setup FAIL\n");
            exit(1);
        }
        // The setup message is delivered; drop our end of the setup channel (don't leak
        // a handle per worker). SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, setup_shell) };
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
            kprint(b"test-harness: hard-float wait FAIL\n");
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
                    kprint(b"test-harness: hard-float worker FAILED code=");
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
    kprint(b"test-harness: hard-float ok (3 workers, f64 + simd verified in ring 3)\n");
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
    kprint(b"test-harness: sched-stats demo start\n");

    // --- /proc/self/status: the caller's own numeric identity.
    let (st, mem) = ns_lookup_wait(root_ns, b"/proc/self/status", RIGHT_MAP_READ);
    if st != 0 || mem == 0 {
        kprint(b"test-harness: /proc/self/status lookup FAIL\n");
        exit(1);
    }
    // SAFETY: register-only syscall; `mem` is a MemoryObject handle with MAP_READ.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, PAGE, RIGHT_MAP_READ) };
    if addr < 0 {
        kprint(b"test-harness: /proc/self/status map FAIL\n");
        exit(1);
    }
    // SAFETY: `addr` is a page the kernel mapped MAP_READ holding the status
    // text (zero-padded to the page).
    let text = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, PAGE as usize) };
    let pid = parse_field(text, b"pid=").unwrap_or(0);
    let tid = parse_field(text, b"tid=").unwrap_or(0);
    if pid < 2 || tid < 1 {
        kprint(b"test-harness: /proc/self/status content FAIL\n");
        exit(1);
    }
    kprint(b"test-harness: /proc/self/status ok pid=");
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
            kprint(b"test-harness: /proc/sched/stats lookup FAIL\n");
            exit(1);
        }
        // SAFETY: register-only syscall; `mem` is a MemoryObject handle with MAP_READ.
        let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, PAGE, RIGHT_MAP_READ) };
        if addr < 0 {
            kprint(b"test-harness: /proc/sched/stats map FAIL\n");
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
            kprint(b"test-harness: /proc/sched/stats ok (");
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
            kprint(b"test-harness: /proc/sched/stats FAIL (<2 CPUs with switches>0)\n");
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
    kprint(b"test-harness: /dev/blk demo start\n");
    report_block_read(root_ns, b"/dev/blk/0", b"test-harness: /dev/blk/0 (disk) read");
    report_block_read(root_ns, b"/dev/blk/1", b"test-harness: /dev/blk/1 (partition) read");
    report_block_read(
        root_ns,
        b"/dev/disk/by-partlabel/NITROX_ESP",
        b"test-harness: /dev/disk/by-partlabel/NITROX_ESP read",
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
    kprint(b"test-harness: userspace-server forwarding demo start\n");
    const CONTENT: &[u8] = b"STUB\n";

    // 1. Channel pair: one end becomes the kernel forwarding endpoint, the other
    //    is the end this process serves requests on.
    // SAFETY: FWD_KEND/FWD_SEND are valid writable out-params.
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut FWD_KEND) as u64, (&raw mut FWD_SEND) as u64, 4, 0)
    };
    if cr != 0 {
        kprint(b"test-harness: fwd channel create FAIL\n");
        return;
    }
    // SAFETY: the kernel wrote both endpoint handles.
    let (kend, send_end) = unsafe { ((&raw const FWD_KEND).read(), (&raw const FWD_SEND).read()) };

    // 2. Fresh namespace; bind the kernel end at /fs as a Userspace Server.
    let ns = unsafe { syscall1(SYS_NS_CREATE, 0) };
    if ns < 0 {
        kprint(b"test-harness: fwd ns create FAIL\n");
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
        kprint(b"test-harness: fwd bind FAIL\n");
        return;
    }

    // 3. Async lookup of /fs/hello — the kernel forwards a Resolve to us.
    let path = b"/fs/hello";
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe {
        syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, RIGHT_MAP_READ)
    };
    if po < 0 {
        kprint(b"test-harness: fwd lookup submit FAIL\n");
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
        kprint(b"test-harness: fwd recv request FAIL\n");
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
            kprint(b"test-harness: fwd request decode FAIL\n");
            return;
        }
    };
    if request.op != librsproto::OP_NS_RESOLVE {
        kprint(b"test-harness: fwd request op mismatch\n");
        return;
    }
    let request_id = request.request_id;

    // 6. Build a read-only MemoryObject holding the stub content.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if mem < 0 {
        kprint(b"test-harness: fwd memobj create FAIL\n");
        return;
    }
    let mem = mem as u64;
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        kprint(b"test-harness: fwd memobj map FAIL\n");
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
            kprint(b"test-harness: fwd reply body FAIL\n");
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
                kprint(b"test-harness: fwd reply encode FAIL\n");
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
        kprint(b"test-harness: fwd reply send FAIL\n");
        return;
    }

    // 9. Wait the lookup PO (already completed by the inline reply) and read the
    //    resolved handle.
    let (st, resolved) = po_wait(po);
    if st != 0 || resolved == 0 {
        kprint(b"test-harness: fwd lookup result FAIL\n");
        return;
    }

    // 10. Map the resolved MemoryObject and verify the content round-tripped.
    let raddr = unsafe { syscall4(SYS_MEMORY_MAP, resolved, 0, PAGE, RIGHT_MAP_READ) };
    if raddr < 0 {
        kprint(b"test-harness: fwd map resolved FAIL\n");
        return;
    }
    // SAFETY: `raddr` is the mapped, kernel-installed MemoryObject.
    let matches = unsafe {
        core::slice::from_raw_parts(raddr as u64 as *const u8, CONTENT.len()) == CONTENT
    };
    if matches {
        kprint(b"test-harness: forwarded lookup returned 'STUB' via fs-server ok\n");
    } else {
        kprint(b"test-harness: fwd content mismatch\n");
    }
}

/// `notif` (in `rdi`) is this process's notification-channel handle and
/// `root_ns` (in `rsi`) its root-namespace handle, both seeded by the kernel at
/// spawn. The third bootstrap register is unused here.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, _boot2: u64) -> ! {
    kprint(b"test-harness: up (demo supervisor, spawned by init)\n");

    // 0. stdio/pipe transport (C3): a TSM1 stream over a real IPC channel between two
    //    threads, with real backpressure. **First**, so it completes (and a failure
    //    reaches init's `code != 0` fail path) before the concurrent login chain fires
    //    the verdict — same race the FP demo dodges by running early.
    stream_transport_demo();

    // 0b. stdio/pipe setup-message spawn (C3 Part C.2): spawn a Tier-1 stage and hand it
    //     stdin + argv via a setup message. Also early (before the login-chain verdict).
    stage_spawn_demo(root_ns, notif);

    // 0a. Exception demo: a worker thread faults; we suspend, inspect, terminate.
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

    // 0a4. The first coreutil end to end (coreutils Milestone 1): `list` as a spawned
    //      Tier-1 stage, its typed table consumed over a real depth-1 pipe. Early, like
    //      its neighbours, so it completes before the login chain adjudicates.
    list_pipeline_demo(root_ns, notif);

    // 0a5. `copy` — the mutation side of the filesystem, including the two cases it must
    //      refuse rather than get wrong (existing destination; no-truncate overwrite).
    copy_demo(root_ns, notif);
    mkdir_remove_demo(root_ns, notif);

    // 0a5b. `rename` — the move that moves no data, and the four cases it must refuse
    //       (occupied destination, missing source, and either end off the filesystem).
    rename_demo(root_ns);

    // 0a5c. Server fan-out: the concurrent-session ceiling is the kernel's wait width, so
    //       prove the server serves a session well past the old 7 — not just opens one.
    session_fanout_demo(root_ns);

    // 0a5d. An in-place, same-length overwrite is invisible to the fs-server under Model A
    //       — prove the kernel now tells it, so mtime stops reporting the file's creation.
    mtime_overwrite_demo(root_ns);

    // 0a5e. `/dev` is kernel-served, not mounted — prove `list` can list it.
    dev_listing_demo(root_ns, notif);

    // 0a6. A stage that dies without writing must close its pipe, so the peer sees
    //      `PeerClosed` instead of hanging (exit-time handle reclamation).
    dead_stage_closes_its_pipe_demo(root_ns, notif);

    // 0a7. The wall clock, from ring 3 — the source of every filesystem timestamp.
    wall_clock_demo();

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


    // 4. The concurrent-exit stress: waves of exiting children race teardown
    // against spawn, the login chain, and each other (substrate-hardening
    // regression cover — see `exit_storm_demo`).
    exit_storm_demo(root_ns, notif);

    // 5. The sched-stats milestone check runs LAST, after the spawn/IPC demos
    // above have put real work on multiple CPUs (and the login chain has been
    // running concurrently throughout) — see `sched_stats_demo`.
    sched_stats_demo(root_ns);

    kprint(b"test-harness: all smoke tests passed; exiting\n");
    exit(0);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}
