//! `test-harness` — the demo supervisor `init` spawns as the test image's regression chain. It is
//! given its notification channel (`rdi`) and a LOOKUP-only handle to init's root namespace
//! (`rsi`), and runs, in `_start`'s order, demos whose `test-harness:` lines `cargo xtask
//! test-qemu` matches: a faulting worker thread, blocking and bounded sends, namespace, entropy and
//! `/proc` demos, the coreutils, `nxsh`, filesystem and block demos, and last the sched-stats
//! check. A nonzero exit fails the run through init's fail path.

#![no_std]
#![no_main]

extern crate alloc;

use core::arch::asm;
use libkern::debug::Line;
use libkern::*;
use libos::{Handle, Namespace, NsMutable, spawn, thread_create};
use librsproto::session::{Dir, DirError};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const KERR_TIMED_OUT: i32 = KError::TimedOut.as_i32();

/// The full rights set an IPC endpoint carries, handed to each child.
const ENDPOINT_RIGHTS: u64 = RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT | RIGHT_DUPLICATE | RIGHT_INSPECT | RIGHT_TRANSFER;

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
/// Spawn args for the exit-storm children: `test-stage` with `arg0 == 0` and no handles; `image` is
/// set per run.
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
/// A zeroed 4096-byte IPC message for the blocking-send demos, used for both send and recv.
static mut MSGBUF: [u8; 4096] = [0; 4096];
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
static mut STREAM_E0: u64 = 0;
/// Rows the producer writes: enough to overflow the pipe's ring, so it blocks on backpressure.
const STREAM_ROWS: i64 = 2000;
const STREAM_STACK_PAGES: u64 = 8;

/// Spawn args for a **Tier-1 stage** of `test-stage`: its setup channel is the one moved handle.
static mut SPAWN_STAGE: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /bin/test-stage
    handle_count: 1,
    move_mask: 1, // move handle 0 (the setup channel) to the stage
    arg0: 0,      // set to `bootstrap_arg0(true)` at runtime
    handles: [0; 4],
    rights: [ENDPOINT_RIGHTS, 0, 0, 0],
    namespace: 0, // inherit (LOOKUP-only)
    syscaps: 0,
};

/// The producer thread: write a `{ i: Int, name: String }` TSM1 stream of [`STREAM_ROWS`] rows,
/// then exit; the process keeps the endpoint, so the consumer sees no spurious `PeerClosed`.
extern "C" fn stream_producer() -> ! {
    use alloc::string::String;
    use libstream::channel::{ChannelSink, IpcPort};
    use libstream::table::TableWriter;
    use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

    // SAFETY: main initialised STREAM_E0 before creating this thread.
    let e0 = unsafe { (&raw const STREAM_E0).read() };
    let schema =
        Schema::new().field("i", TypeTag::Int, TypeModifiers::NONE).field("name", TypeTag::String, TypeModifiers::NONE);
    let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(e0), IPC_PAYLOAD_SIZE));
    let ok = tw.write_schema(StreamFlags::NONE, &schema).is_ok()
        && (0..STREAM_ROWS).all(|i| tw.write_row(&[Value::Int(i), Value::Str(String::from("row"))]).is_ok())
        && tw.finish_with_status(0).is_ok()
        && tw.into_sink().finish().is_ok();
    let _ = ok; // a mismatch surfaces as the consumer's fail-loud verdict
    // SAFETY: `sys_thread_exit` ends this thread; the exit code is unused.
    unsafe { syscall1(SYS_THREAD_EXIT, 0) };
    loop {
        // SAFETY: `pause` is always valid in ring 3 with no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}

/// Move a TSM1 stream over a **real IPC channel between two threads**: the producer on one end of a
/// small-ring pipe, main verifying [`STREAM_ROWS`] rows on the other, so the producer blocks on
/// backpressure in the real blocking-send path; a mismatch is fail-loud.
fn stream_transport_demo() {
    use libstream::channel::{ChannelReceiver, IpcPort};
    use libstream::table::{Item, TableReader};
    kprint(b"test-harness: stream transport demo (2-thread pipe, backpressured)\n");

    // A depth-4 pipe, small on purpose, so a 2000-row stream overflows the ring.
    // SAFETY: END0/END1 are valid writable out-params.
    let cr = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut END0) as u64, (&raw mut END1) as u64, 4, 0) };
    if cr != 0 {
        kprint(b"test-harness: stream chan create FAIL\n");
        exit(1);
    }
    // SAFETY: the kernel wrote both endpoint handles.
    let (e0, e1) = unsafe { ((&raw const END0).read(), (&raw const END1).read()) };

    let stack_bytes = STREAM_STACK_PAGES * PAGE;
    // SAFETY: register-only syscalls with valid arguments.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, stack_bytes, 0, 0, 0) };
    if mem < 0 {
        kprint(b"test-harness: stream stack create FAIL\n");
        exit(1);
    }
    // SAFETY: maps the stack object read/write at a kernel-chosen address.
    let base = unsafe { syscall4(SYS_MEMORY_MAP, mem as u64, 0, stack_bytes, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if base < 0 {
        kprint(b"test-harness: stream stack map FAIL\n");
        exit(1);
    }
    let stack_top = base as u64 + stack_bytes;

    #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
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

    let mut rx = ChannelReceiver::new(IpcPort::new(e1));
    let bytes = match rx.receive(1 << 20) {
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
    // Release this demo's handles so a long run does not exhaust the handle table.
    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, e0);
        syscall1(SYS_HANDLE_CLOSE, e1);
        syscall1(SYS_HANDLE_CLOSE, mem as u64);
    }
    kprint(b"test-harness: stream transport ok (2000 rows over a real pipe, backpressured)\n");
}

/// The **setup-message spawn**: a Tier-1 `test-stage` receives a `stdin` pipe end and `argv =
/// ["stage", "500"]` in a setup message; we produce the stream and fail the run if the stage exits
/// non-zero.
fn stage_spawn_demo(root_ns: u64, notif: u64) {
    use alloc::string::String;
    use libstream::channel::{ChannelSink, IpcPort};
    use libstream::setup::{Streams, bootstrap_arg0, pipe, send_setup};
    use libstream::table::TableWriter;
    use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

    kprint(b"test-harness: setup-message stage demo (spawn a Tier-1 stage)\n");
    const STAGE_ROWS: i64 = 500;

    let (st, img) = ns_lookup_wait(root_ns, b"/bin/test-stage", RIGHT_MAP_READ);
    if st != 0 || img == 0 {
        kprint(b"test-harness: stage image FAIL\n");
        exit(1);
    }

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

    #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
    // SAFETY: SPAWN_STAGE is our static, initialised here; `setup_stage` is moved to the stage; `img`
    // is ours, and the spawn only read it.
    let _stage = match unsafe {
        SPAWN_STAGE.image = img;
        SPAWN_STAGE.handles[0] = setup_stage;
        SPAWN_STAGE.arg0 = bootstrap_arg0(true);
        let p = spawn(&*(&raw const SPAWN_STAGE));
        syscall1(SYS_HANDLE_CLOSE, img);
        p
    } {
        Ok(p) => p,
        Err(_) => {
            kprint(b"test-harness: stage spawn FAIL\n");
            exit(1);
        }
    };

    let streams = Streams { stdin: Some(stage_stdin), stdout: None, stderr: None };
    if send_setup(setup_shell, streams, &["stage", "500"]).is_err() {
        kprint(b"test-harness: stage send_setup FAIL\n");
        exit(1);
    }

    // Produce the stdin stream (the stage drains it concurrently). It waits for the terminator
    // before verifying, so `finish` completes before it exits: no `PeerClosed` race.
    let schema =
        Schema::new().field("i", TypeTag::Int, TypeModifiers::NONE).field("name", TypeTag::String, TypeModifiers::NONE);
    let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(prod), IPC_PAYLOAD_SIZE));
    let ok = tw.write_schema(StreamFlags::NONE, &schema).is_ok()
        && (0..STAGE_ROWS).all(|i| tw.write_row(&[Value::Int(i), Value::Str(String::from("row"))]).is_ok())
        && tw.finish_with_status(0).is_ok()
        && tw.into_sink().finish().is_ok();
    if !ok {
        kprint(b"test-harness: stage produce FAIL\n");
        exit(1);
    }

    // Reap the stage's exit, the first `ChildExited` (no other child has been spawned yet).
    loop {
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
        let waited = unsafe {
            WAIT_HANDLES[0] = notif;
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
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
    // at scope end. SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, prod);
        syscall1(SYS_HANDLE_CLOSE, setup_shell);
    }
    kprint(b"test-harness: setup-message stage ok (stdin stream + argv verified by the stage)\n");
}

/// The forwarding channel's kernel end (bound at `/fs`) and the server end this process serves.
static mut FWD_KEND: u64 = 0;
static mut FWD_SEND: u64 = 0;
static mut FWD_REQ: [u8; 4096] = [0; 4096];
static mut FWD_REPLY: [u8; 4096] = [0; 4096];
static mut FWD_REPLY_H: [u64; 8] = [0; 8];

/// The worker's entry: store to an unmapped address, so the kernel suspends the thread and delivers
/// a `SegFault`.
extern "C" fn worker_fault() -> ! {
    // SAFETY: this is the whole point — `0xdead_0000` is an unmapped user
    // address, so the store traps. The kernel never lets the store complete.
    unsafe { core::ptr::write_volatile(0xdead_0000usize as *mut u64, 0xc0ffee) };
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}

/// The exception demo: a second thread faults; we see the `SegFault` on `notif`, inspect its
/// registers, and terminate it.
fn worker_exception_demo(notif: u64) {
    kprint(b"test-harness: mapping a worker stack\n");
    // SAFETY: register-only syscalls with valid arguments.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if mem < 0 {
        kprint(b"test-harness: worker stack create FAIL\n");
        exit(1);
    }
    // SAFETY: maps the memory object read/write at a kernel-chosen address.
    let base = unsafe { syscall4(SYS_MEMORY_MAP, mem as u64, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if base < 0 {
        kprint(b"test-harness: worker stack map FAIL\n");
        exit(1);
    }
    let stack_top = base as u64 + PAGE; // stacks grow down from the top

    #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
    // SAFETY: WORKER_ARGS is our static, exclusively initialised here, then lent to the wrapper.
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

    loop {
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
        let waited = unsafe {
            WAIT_HANDLES[0] = notif;
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
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
    }

    // SAFETY: WORKER_REGS is a valid writable RegisterValues out-param.
    let gr = unsafe { syscall4(SYS_THREAD_GET_REGISTERS, worker.raw().0, (&raw mut WORKER_REGS) as u64, 0, 0) };
    if gr != 0 {
        kprint(b"test-harness: get_registers FAIL\n");
        exit(1);
    }
    // SAFETY: the kernel wrote the 18-register snapshot into WORKER_REGS.
    let rip = unsafe { (&raw const WORKER_REGS.regs[REG_RIP]).read() };
    Line::new().s(b"test-harness: worker faulted @ rip=").x(rip).s(b" ; terminating").end();

    // SAFETY: register-only syscall.
    let er = unsafe { syscall4(SYS_EXCEPTION_RESUME, worker.raw().0, DISPOSITION_TERMINATE, 7, 0) };
    if er != 0 {
        kprint(b"test-harness: exception_resume FAIL\n");
        exit(1);
    }
    // The worker is not this process's last thread, so its termination produces no `ChildExited`.
    kprint(b"test-harness: worker terminated\n");
}

/// The blocking-send / `PendingOperation` path: with a ring full, a `Block` send returns a PO (the
/// message held in-kernel); a recv frees a slot, completing it with status 0.
fn block_send_demo() {
    // SAFETY: END0/END1 are valid writable out-params.
    let cr = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut END0) as u64, (&raw mut END1) as u64, 4, 0) };
    if cr != 0 {
        kprint(b"test-harness: block-demo channel create FAIL\n");
        return;
    }
    // SAFETY: the kernel wrote both endpoint handles.
    let (a, b) = unsafe { ((&raw const END0).read(), (&raw const END1).read()) };

    let mut filled = 0u64;
    loop {
        // SAFETY: valid endpoint + zeroed message; count 0 (no transfers).
        let r = unsafe { syscall5(SYS_CHANNEL_SEND, a, (&raw const MSGBUF) as u64, 0, 0, SENDMODE_NOBLOCK) };
        if r == 0 {
            filled += 1;
        } else {
            break; // WouldBlock: the ring is full
        }
    }

    // One more, blocking: the ring is full, so this returns a PendingOperation, not a block.
    // SAFETY: as above, with Block mode.
    let po = unsafe { syscall5(SYS_CHANNEL_SEND, a, (&raw const MSGBUF) as u64, 0, 0, SENDMODE_BLOCK) };
    if po < 0 {
        kprint(b"test-harness: block send FAIL\n");
        return;
    }
    let po = po as u64;

    // SAFETY: single-threaded; `MSGBUF` is lent for the call.
    if unsafe { libkern::channel_recv_raw(b, &raw mut MSGBUF) }.is_err() {
        kprint(b"test-harness: block-demo recv FAIL\n");
        return;
    }

    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
    let waited = unsafe {
        WAIT_HANDLES[0] = po;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    // SAFETY: bytes 8..24 are inside `WAIT_RESULTS`, a static of plain bytes only this thread
    // touches; they hold the wait's result only when it returned 1.
    let status = unsafe { i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]) };
    if waited == 1 && status == 0 {
        Line::new()
            .s(b"test-harness: blocking send completed via PendingOperation (")
            .u(filled)
            .s(b" queued, 1 blocked-then-delivered)")
            .end();
    } else {
        kprint(b"test-harness: block-demo wait unexpected\n");
    }

    loop {
        // SAFETY: single-threaded; `MSGBUF` is lent for the call.
        if unsafe { libkern::channel_recv_raw(b, &raw mut MSGBUF) }.is_err() {
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

/// The `BlockBounded` send: with a full ring and a deadline already past, to an endpoint no one
/// receives, the PO completes `TimedOut` on the next tick.
fn block_bounded_demo() {
    // SAFETY: END0/END1 are valid writable out-params.
    let cr = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut END0) as u64, (&raw mut END1) as u64, 4, 0) };
    if cr != 0 {
        kprint(b"test-harness: block-bounded channel create FAIL\n");
        return;
    }
    // SAFETY: the kernel wrote both endpoint handles.
    let (a, b) = unsafe { ((&raw const END0).read(), (&raw const END1).read()) };

    loop {
        // SAFETY: valid endpoint + zeroed message; count 0 (no transfers).
        let r = unsafe { syscall5(SYS_CHANNEL_SEND, a, (&raw const MSGBUF) as u64, 0, 0, SENDMODE_NOBLOCK) };
        if r != 0 {
            break; // WouldBlock: full
        }
    }

    // A deadline of `1` is already past: held now, timed out on the next tick (the 6th arg).
    // SAFETY: as above, BlockBounded mode + a past deadline.
    let po = unsafe { syscall6(SYS_CHANNEL_SEND, a, (&raw const MSGBUF) as u64, 0, 0, SENDMODE_BLOCKBOUNDED, 1) };
    if po < 0 {
        kprint(b"test-harness: block-bounded send FAIL\n");
        return;
    }
    let po = po as u64;

    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
    let waited = unsafe {
        WAIT_HANDLES[0] = po;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    // SAFETY: bytes 8..24 are inside `WAIT_RESULTS`, a static of plain bytes only this thread
    // touches; they hold the wait's result only when it returned 1.
    let status = unsafe { i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]) };
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

/// Namespace demo: `create → bind → lookup → wait → use` on a fresh namespace, proving the four
/// `sys_ns_*` syscalls and the async-lookup result word.
fn ns_demo() {
    kprint(b"test-harness: ns-demo start (fresh namespace)\n");

    // A fresh namespace to bind into: the inherited root is LOOKUP-only.
    // SAFETY: register-only syscall.
    let ns = unsafe { syscall1(SYS_NS_CREATE, 0) };
    if ns < 0 {
        kprint(b"test-harness: ns_create FAIL\n");
        return;
    }
    let ns = ns as u64;
    kprint(b"test-harness: ns_create ok\n");

    // SAFETY: register-only syscall.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if mem < 0 {
        kprint(b"test-harness: ns-demo mem create FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, ns) };
        return;
    }
    let mem = mem as u64;

    // Bind via `Namespace::bind`, gated by the BIND right and the BIND_NAMESPACE syscap.
    let path = b"/store";
    // SAFETY: `ns` is parent's live namespace handle; borrow is non-owning (won't close it).
    let ns_h = unsafe { Handle::<Namespace, NsMutable>::borrow(RawHandle(ns), Rights::BIND) };
    if ns_h.bind("/store", RawHandle(mem)).is_err() {
        kprint(b"test-harness: ns_bind FAIL\n");
        return;
    }
    kprint(b"test-harness: ns_bind /store ok\n");

    // SAFETY: valid path pointer + handle.
    let po = unsafe {
        syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, RIGHT_MAP_READ | RIGHT_MAP_WRITE)
    };
    if po < 0 {
        kprint(b"test-harness: ns_lookup FAIL\n");
        return;
    }
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
    let waited = unsafe {
        WAIT_HANDLES[0] = po as u64;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    // SAFETY: bytes 8..24 are inside `WAIT_RESULTS`, a static of plain bytes only this thread
    // touches; they hold the wait's result only when it returned 1.
    let status = unsafe { i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]) };
    // SAFETY: bytes 8..24 are inside `WAIT_RESULTS`, a static of plain bytes only this thread
    // touches; they hold the wait's result only when it returned 1.
    let resolved = unsafe {
        u64::from_le_bytes([
            WAIT_RESULTS[16],
            WAIT_RESULTS[17],
            WAIT_RESULTS[18],
            WAIT_RESULTS[19],
            WAIT_RESULTS[20],
            WAIT_RESULTS[21],
            WAIT_RESULTS[22],
            WAIT_RESULTS[23],
        ])
    };
    if waited != 1 || status != 0 || resolved == 0 {
        kprint(b"test-harness: ns_lookup wait unexpected\n");
        return;
    }
    Line::new().s(b"test-harness: ns_lookup -> resolved handle=").u(resolved).end();

    // SAFETY: `resolved` is a MemoryObject handle carrying MAP_READ|MAP_WRITE.
    let mapped = unsafe { syscall4(SYS_MEMORY_MAP, resolved, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if mapped < 0 {
        kprint(b"test-harness: ns-demo map resolved FAIL\n");
        return;
    }
    kprint(b"test-harness: mapped resolved /store handle ok\n");

    // Unbind; a later lookup must complete the PO with `NotFound` (errors arrive through the PO).
    // SAFETY: valid path pointer + handle.
    let ur = unsafe { syscall4(SYS_NS_UNBIND, ns, path.as_ptr() as u64, path.len() as u64, 0) };
    if ur != 0 {
        kprint(b"test-harness: ns_unbind FAIL\n");
        return;
    }
    // SAFETY: valid path pointer + handle.
    let po2 = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, RIGHT_MAP_READ) };
    if po2 < 0 {
        kprint(b"test-harness: ns_lookup-after-unbind FAIL\n");
        return;
    }
    // SAFETY: valid wait buffers.
    let waited2 = unsafe {
        WAIT_HANDLES[0] = po2 as u64;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    // SAFETY: bytes 8..24 are inside `WAIT_RESULTS`, a static of plain bytes only this thread
    // touches; they hold the wait's result only when it returned 1.
    let status2 = unsafe { i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]) };
    // KError::NotFound = -10.
    if waited2 == 1 && status2 == -10 {
        kprint(b"test-harness: ns_unbind ok (lookup-after-unbind -> NotFound)\n");
    } else {
        kprint(b"test-harness: ns-demo unbind unexpected\n");
    }

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

/// Entropy demo: create an `EntropyObject` and read CSPRNG bytes (the pool seeds at boot); the two
/// 32-byte draws must differ.
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
    // SAFETY: `b` is a valid writable 32-byte buffer; `len` <= ENTROPY_READ_MAX
    let r2 = unsafe { syscall4(SYS_ENTROPY_READ, h, (&raw mut b) as u64, 32, 0) };
    if r1 != 0 || r2 != 0 {
        // A positive return would mean unseeded; not expected, the pool seeds at boot.
        kprint(b"test-harness: entropy read not synchronous (unseeded?)\n");
        // SAFETY: closing our own handle
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
        return;
    }
    // A manual loop: inlined `[u8; N]` equality once mis-compiled to an infinite loop on the old
    // soft-float target.
    let mut differ = false;
    for i in 0..32 {
        if a[i] != b[i] {
            differ = true;
            break;
        }
    }
    let first = u64::from_le_bytes([a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7]]);
    // Assembled across the `if`, so the verdict stays on the same line as the bytes it judges.
    let mut l = Line::new();
    l.s(b"test-harness: entropy bytes[0..8]=").x(first);
    if differ {
        l.s(b" (two reads differ) entropy ok");
    } else {
        l.s(b" entropy-demo UNEXPECTED (reads identical)");
    }
    l.end();
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
}

/// Kernel-server demo: resolve `/dev/entropy` from the root namespace and read from the handle the
/// in-kernel server hands back (boot-binding and `KernelServer` dispatch).
fn dev_entropy_lookup_demo(root_ns: u64) {
    kprint(b"test-harness: /dev/entropy lookup-demo start\n");
    let path = b"/dev/entropy";
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, root_ns, path.as_ptr() as u64, path.len() as u64, RIGHT_READ) };
    if po < 0 {
        kprint(b"test-harness: /dev/entropy lookup FAIL\n");
        return;
    }
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
    let waited = unsafe {
        WAIT_HANDLES[0] = po as u64;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    // SAFETY: bytes 8..24 are inside `WAIT_RESULTS`, a static of plain bytes only this thread
    // touches; they hold the wait's result only when it returned 1.
    let status = unsafe { i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]) };
    // SAFETY: bytes 8..24 are inside `WAIT_RESULTS`, a static of plain bytes only this thread
    // touches; they hold the wait's result only when it returned 1.
    let resolved = unsafe {
        u64::from_le_bytes([
            WAIT_RESULTS[16],
            WAIT_RESULTS[17],
            WAIT_RESULTS[18],
            WAIT_RESULTS[19],
            WAIT_RESULTS[20],
            WAIT_RESULTS[21],
            WAIT_RESULTS[22],
            WAIT_RESULTS[23],
        ])
    };
    if waited != 1 || status != 0 || resolved == 0 {
        kprint(b"test-harness: /dev/entropy lookup wait unexpected\n");
        return;
    }

    let mut a = [0u8; 32];
    // SAFETY: valid writable 32-byte buffer; `len` ≤ ENTROPY_READ_MAX.
    let r = unsafe { syscall4(SYS_ENTROPY_READ, resolved, (&raw mut a) as u64, 32, 0) };
    if r != 0 {
        kprint(b"test-harness: /dev/entropy read not synchronous\n");
        // SAFETY: closing our own handle
        unsafe { syscall1(SYS_HANDLE_CLOSE, resolved) };
        return;
    }
    let first = u64::from_le_bytes([a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7]]);
    Line::new().s(b"test-harness: /dev/entropy resolved+read ok bytes[0..8]=").x(first).end();

    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, resolved);
        syscall1(SYS_HANDLE_CLOSE, po as u64);
    }
}

/// Resolve `path` in `ns` with `rights`, wait the PO, and return `(status, resolved_handle)`;
/// closes the PO, the caller owns the handle.
fn ns_lookup_wait(ns: u64, path: &[u8], rights: u64) -> (i32, u64) {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po < 0 {
        return (po as i32, 0);
    }
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
    let waited = unsafe {
        WAIT_HANDLES[0] = po as u64;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    // SAFETY: bytes 8..24 are inside `WAIT_RESULTS`, a static of plain bytes only this thread
    // touches; they hold the wait's result only when it returned 1.
    let status = unsafe { i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]) };
    // SAFETY: bytes 8..24 are inside `WAIT_RESULTS`, a static of plain bytes only this thread
    // touches; they hold the wait's result only when it returned 1.
    let resolved = unsafe {
        u64::from_le_bytes([
            WAIT_RESULTS[16],
            WAIT_RESULTS[17],
            WAIT_RESULTS[18],
            WAIT_RESULTS[19],
            WAIT_RESULTS[20],
            WAIT_RESULTS[21],
            WAIT_RESULTS[22],
            WAIT_RESULTS[23],
        ])
    };
    // SAFETY: closing our own PO handle (the resolved handle is separate).
    unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
    if waited != 1 {
        return (-1, 0);
    }
    (status, resolved)
}

/// Whether `sys_handle_stat` reports object type `want`.
///
/// The out-param must be a real [`HandleInfo`]: an undersized buffer once clobbered `_start`'s
/// spilled `root_ns` and broke every later root-namespace lookup.
fn stat_is_type(h: u64, want: u32) -> bool {
    let mut info = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
    // SAFETY: valid writable `HandleInfo` out-param.
    let r = unsafe { syscall4(SYS_HANDLE_STAT, h, (&raw mut info) as u64, 0, 0) };
    if r != 0 {
        return false;
    }
    info.object_type == want
}

/// `/proc/self` demo: the caller's own process, thread and namespace resolve from the root
/// namespace, each strictly *this* caller's object, and each handle is proved.
fn proc_self_demo(root_ns: u64) {
    kprint(b"test-harness: /proc/self demo start\n");

    let (st, ph) = ns_lookup_wait(root_ns, b"/proc/self/process", RIGHT_INSPECT);
    if st != 0 || ph == 0 || !stat_is_type(ph, KOBJ_PROCESS) {
        kprint(b"test-harness: /proc/self/process FAIL\n");
        return;
    }
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, ph) };
    kprint(b"test-harness: /proc/self/process ok (own Process handle)\n");

    let (st, th) = ns_lookup_wait(root_ns, b"/proc/self/thread", RIGHT_INSPECT);
    if st != 0 || th == 0 || !stat_is_type(th, KOBJ_THREAD) {
        kprint(b"test-harness: /proc/self/thread FAIL\n");
        return;
    }
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, th) };
    kprint(b"test-harness: /proc/self/thread ok (own Thread handle)\n");

    // Use it: resolve /dev/entropy through the returned namespace handle.
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

/// Initramfs demo: resolve `/initramfs/etc/init.toml` (the Limine module, served by the in-kernel
/// CPIO server), map it, and print its first bytes.
fn initramfs_demo(root_ns: u64) {
    kprint(b"test-harness: /initramfs demo start\n");
    let (st, mem) = ns_lookup_wait(root_ns, b"/initramfs/etc/init.toml", RIGHT_MAP_READ);
    if st != 0 || mem == 0 {
        kprint(b"test-harness: /initramfs/etc/init.toml lookup FAIL\n");
        return;
    }
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
    Line::new().s(b"test-harness: /initramfs/etc/init.toml -> \"").s(head).s(b"...\"").end();
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
}

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

fn cpus_with_switches(text: &[u8]) -> u64 {
    let mut n = 0;
    for line in text.split(|&b| b == b'\n') {
        if line.starts_with(b"cpu=") && parse_field(line, b"switches=").is_some_and(|v| v > 0) {
            n += 1;
        }
    }
    n
}

/// Block this thread for `ms` on a one-shot timer; best-effort, returning on any failure.
fn timer_sleep_ms(ms: u64) {
    // SAFETY: a valid syscall; returns a handle (>= 0) or a negative KError.
    let th = unsafe { syscall1(SYS_TIMER_CREATE, 0) };
    if th < 0 {
        return;
    }
    let th = th as u64;
    // SAFETY: CLOCK_BUF is a writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: `CLOCK_BUF` is a plain `u64` static only this thread touches; a failed clock read
    // leaves its last value.
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

/// Concurrent-exit stress: waves of immediately-exiting children are spawned and reaped, so
/// teardowns (kernel-stack frees, TLB shootdowns, reap sweeps) race each other and the login chain
/// across all CPUs.
///
/// It covers an exit's reap initiating a shootdown from an IF-masked syscall context (waves make
/// initiators collide), a reap racing a mid-switch-out sibling, and the reap lists' reserve
/// discipline across repeated drains. A lost exit hangs the wave (the wall-clock timeout fails the
/// run); a crash exits nonzero.
fn exit_storm_demo(root_ns: u64, notif: u64) {
    kprint(b"test-harness: exit-storm start\n");
    let (st, img) = ns_lookup_wait(root_ns, b"/bin/test-stage", RIGHT_MAP_READ);
    if st != 0 || img == 0 {
        kprint(b"test-harness: exit-storm image lookup FAIL\n");
        exit(1);
    }
    const ROUNDS: usize = 6;
    const WAVE: usize = 3;
    for _ in 0..ROUNDS {
        let mut procs = [const { None }; WAVE];
        for slot in procs.iter_mut() {
            #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
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
        let mut got = 0;
        while got < WAVE {
            // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
            let waited = unsafe {
                WAIT_HANDLES[0] = notif;
                syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
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
        // `procs` drops here, reaping the wave while the next wave spawns: teardown and spawn race
        // by design.
    }
    // SAFETY: closing our own image handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, img) };
    kprint(b"test-harness: exit-storm ok (18 exits reaped)\n");
}

/// **Directory listing over the direct-RPC transport**: open `/system` as a session via
/// [`librsproto::session::Dir`] (the client's integration proof), enumerate it, and confirm
/// `current-generation` is listed *with plausible metadata*; a failure exits non-zero.
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
            // Check the inode metadata against what this file must be: a zeroed or mis-decoded
            // field fails, which a name-only check would miss.
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

fn return_fail(msg: &[u8]) -> ! {
    kprint(msg);
    exit(1)
}

/// Entries the dir-growth check creates: at 250-byte names that is ~4.1 KB, just past one 4 KiB
/// directory block, so the directory must grow.
const GROW_ENTRIES: usize = 16;

/// **Directory mutation over the direct-RPC transport**: mkdir a temp subdir, rename it, rmdir it,
/// each confirmed by a `ReadDir`, plus the error path: removing a missing name must be a *server*
/// error carrying a `KError`, not a transport failure or a false success.
fn dir_mutate_demo(root_ns: u64) {
    kprint(b"test-harness: dir-mutate demo start\n");
    let mut buf = [0u8; 4096];
    let mut dir = match Dir::open(root_ns, b"/system", &mut buf) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: dir-mutate open FAIL\n"),
    };

    if dir.mkdir(b"nx-tmp").is_err() {
        return_fail(b"test-harness: mkdir FAIL\n");
    }
    if !dir_contains(&mut dir, b"nx-tmp") {
        return_fail(b"test-harness: mkdir not visible\n");
    }
    if dir.rename(b"nx-tmp", b"nx-tmp2").is_err() {
        return_fail(b"test-harness: rename FAIL\n");
    }
    if dir_contains(&mut dir, b"nx-tmp") || !dir_contains(&mut dir, b"nx-tmp2") {
        return_fail(b"test-harness: rename not applied\n");
    }
    match dir.rmdir(b"nx-does-not-exist") {
        Err(DirError::Server(_)) => {}
        Err(_) => return_fail(b"test-harness: rmdir(missing) reported a transport fault\n"),
        Ok(()) => return_fail(b"test-harness: rmdir(missing) wrongly succeeded\n"),
    }
    // Fill the directory past one block on the **real** filesystem: 16 × 250-byte names is just
    // over one 4 KiB block, which failed with `TooLarge` before directory growth. Sized to *cross
    // the boundary*, not stress: a bigger sweep pushed the TCG run past the timeout.
    let mut name = [b'g'; 250];
    for i in 0..GROW_ENTRIES {
        name[0] = b'0' + (i / 10) as u8;
        name[1] = b'0' + (i % 10) as u8;
        if dir.mkdir(&name).is_err() {
            return_fail(b"test-harness: dir-grow mkdir FAIL (directory did not grow?)\n");
        }
    }
    // One enumeration, counting ours: the added blocks must be reachable through paginated
    // `ReadDir`.
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

    if dir.rmdir(b"nx-tmp2").is_err() {
        return_fail(b"test-harness: rmdir FAIL\n");
    }
    if dir_contains(&mut dir, b"nx-tmp2") {
        return_fail(b"test-harness: rmdir not applied\n");
    }
    dir.close();
    kprint(b"test-harness: dir-mutate ok (mkdir + rename + rmdir, each ReadDir-verified)\n");
}

/// Whether the open directory lists `name`; exits on a transport failure, which is not an absent
/// name.
fn dir_contains(dir: &mut Dir<'_>, name: &[u8]) -> bool {
    let mut found = false;
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

/// Entries the `list` fixture directory gets, and their name length: the **TSM1 stream exceeds one
/// IPC message** (the depth-1 pipe really blocks the producer) while the directory data still fits
/// **one 4 KiB block**, avoiding the deferred directory grow: 30 × 110-byte names are ≈ 4.1 KB of
/// stream (two messages) against ≈ 3.5 KB of directory data, both bounds checked by the demo.
const LIST_FIXTURE_ENTRIES: usize = 30;
const LIST_FIXTURE_NAME_LEN: usize = 110;
const LIST_FIXTURE_DIR: &[u8] = b"nx-list";
const LIST_FIXTURE_PATH: &[u8] = b"/system/nx-list";
const LIST_FIXTURE_PATH_STR: &str = "/system/nx-list";

static mut SPAWN_LIST: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /bin/list
    handle_count: 1,
    move_mask: 1, // move handle 0 (the setup channel) to the stage
    arg0: 0,      // set to `bootstrap_arg0(true)` at runtime
    handles: [0; 4],
    rights: [ENDPOINT_RIGHTS, 0, 0, 0],
    namespace: 0, // inherit (LOOKUP-only)
    syscaps: 0,
};

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

/// **The first coreutil, end to end through a real pipe.**
///
/// Spawns `list` on a fixture with `stdout` on a **depth-1** pipe and checks the **typed contract**
/// (schema exactly `{name: String, size: Int, kind: String, modified: Int}`, each entry the right
/// kind with a plausible mtime), **real backpressure** (the stream exceeds one IPC payload on a
/// one-slot ring) and **early-consumer close**: a second `list` whose reader closes at once must
/// exit **`0`**, as `PeerClosed` means stop cleanly (`yes | head -1`).
fn list_pipeline_demo(root_ns: u64, notif: u64) {
    use libstream::table::{Item, TableReader};
    use libstream::{TypeTag, Value};

    kprint(b"test-harness: list-pipeline demo (the first coreutil over a real pipe)\n");

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
                // Most likely the deferred full-directory grow: the sizing assumption broke.
                return_fail(b"test-harness: list fixture entry mkdir FAIL (directory full?)\n");
            }
        }
        fixture.close();
    }

    let bytes = run_list(root_ns, notif, &["list", LIST_FIXTURE_PATH_STR], true);
    if bytes.len() <= IPC_PAYLOAD_SIZE {
        // If the stream fit one message the pipe never filled and backpressure went unexercised.
        return_fail(b"test-harness: list stream fit one message (backpressure untested)\n");
    }
    let mut tr = match TableReader::new(&bytes) {
        Ok(t) => t,
        Err(_) => return_fail(b"test-harness: list stream bad TSM1 header\n"),
    };
    let schema = tr.schema();
    let expect: [(&str, TypeTag); 4] =
        [("name", TypeTag::String), ("size", TypeTag::Int), ("kind", TypeTag::String), ("modified", TypeTag::Int)];
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
                // `modified` must be a plausible date: these entries were just created, so it
                // asserts the RTC-anchored wall clock reached the inode.
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
    // Exactly the fixture entries: `list` filters `.`/`..`.
    if rows != LIST_FIXTURE_ENTRIES {
        return_fail(b"test-harness: list row count wrong\n");
    }

    let _ = run_list(root_ns, notif, &["list", LIST_FIXTURE_PATH_STR], false);

    // 3b. `--recursive` must report children under a parent-relative path (`nx-list/e…`).
    let deep = run_list(root_ns, notif, &["list", "--recursive", "/system"], true);
    let mut tr = match TableReader::new(&deep) {
        Ok(t) => t,
        Err(_) => return_fail(b"test-harness: list --recursive bad TSM1 header\n"),
    };
    let mut nested = 0usize;
    while let Some(Ok(item)) = tr.next() {
        if let Item::Row(vals) = item
            && let Value::Str(name) = &vals[0]
            && name.starts_with("nx-list/e")
        {
            nested += 1;
        }
    }
    if nested != LIST_FIXTURE_ENTRIES {
        return_fail(b"test-harness: list --recursive did not descend correctly\n");
    }

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

/// Spawn `list` with `argv` and either drain its stream or close the read end at once
/// (early-consumer close), requiring exit `0` either way; returns the bytes received.
fn run_list(root_ns: u64, notif: u64, argv: &[&str], consume: bool) -> alloc::vec::Vec<u8> {
    run_coreutil_capture(root_ns, notif, b"/bin/list", argv, consume)
}

/// Spawn a coreutil as a Tier-1 stage and **return its stdout stream** for the demo to assert on;
/// the pipe is depth 1, so a multi-message stream cannot complete without the consumer draining.
fn run_coreutil_capture(root_ns: u64, notif: u64, image: &[u8], argv: &[&str], consume: bool) -> alloc::vec::Vec<u8> {
    run_coreutil_capture_in(root_ns, notif, image, argv, consume, 0)
}

fn run_coreutil_capture_in(
    root_ns: u64,
    notif: u64,
    image: &[u8],
    argv: &[&str],
    consume: bool,
    child_ns: u64,
) -> alloc::vec::Vec<u8> {
    use alloc::vec::Vec;
    use libstream::channel::{ChannelReceiver, IpcPort};
    use libstream::setup::{Streams, bootstrap_arg0, pipe, send_setup};

    let (st, img) = ns_lookup_wait(root_ns, image, RIGHT_MAP_READ);
    if st != 0 || img == 0 {
        return_fail(b"test-harness: coreutil image FAIL\n");
    }
    let (rx, list_stdout) = match pipe(1) {
        Ok(p) => p,
        Err(_) => return_fail(b"test-harness: list stdout pipe FAIL\n"),
    };
    let (setup_shell, setup_stage) = match pipe(4) {
        Ok(p) => p,
        Err(_) => return_fail(b"test-harness: list setup chan FAIL\n"),
    };
    #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
    // SAFETY: SPAWN_LIST is our static, initialised here; spawns are sequential; `img` is ours.
    let _proc = match unsafe {
        SPAWN_LIST.image = img;
        SPAWN_LIST.handles[0] = setup_stage;
        SPAWN_LIST.arg0 = bootstrap_arg0(true);
        SPAWN_LIST.namespace = child_ns;
        let p = spawn(&*(&raw const SPAWN_LIST));
        syscall1(SYS_HANDLE_CLOSE, img);
        SPAWN_LIST.namespace = 0;
        p
    } {
        Ok(p) => p,
        Err(_) => return_fail(b"test-harness: list spawn FAIL\n"),
    };

    let streams = Streams {
        stdin: None, // a source stage: its input is the filesystem, not a pipe
        stdout: Some(list_stdout),
        stderr: None,
    };
    if send_setup(setup_shell, streams, argv).is_err() {
        return_fail(b"test-harness: list send_setup FAIL\n");
    }

    let out = if consume {
        match ChannelReceiver::new(IpcPort::new(rx)).receive(1 << 20) {
            Ok(b) => b,
            Err(_) => return_fail(b"test-harness: list stdout receive FAIL\n"),
        }
    } else {
        // Close the read end unread: the stage's next send surfaces `PeerClosed`, a clean stop.
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

/// Block until a `ChildExited` arrives and return its exit code; children are reaped one at a time,
/// so the first is this child's.
fn reap_child_exit(notif: u64) -> i32 {
    loop {
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
        let waited = unsafe {
            WAIT_HANDLES[0] = notif;
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
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

/// **The wall clock, from ring 3**: readable, a *plausible* date (not a fabricated one or 1970),
/// and advancing.
fn wall_clock_demo() {
    kprint(b"test-harness: wall-clock demo\n");
    let mut first: u64 = 0;
    // SAFETY: valid u64 out-param.
    let r = unsafe { syscall4(SYS_CLOCK_READ, CLOCK_REALTIME, (&raw mut first) as u64, 0, 0) };
    if r != 0 {
        // Not a soft failure: without a wall clock the filesystem stamps 1970 on everything.
        return_fail(b"test-harness: CLOCK_REALTIME unreadable\n");
    }
    // 2024 .. 2100 in ns: catches a zero, a monotonic value returned as realtime, or a botched
    // epoch.
    const NS_2024: u64 = 1_704_067_200_000_000_000;
    const NS_2100: u64 = 4_102_444_800_000_000_000;
    if !(NS_2024..=NS_2100).contains(&first) {
        return_fail(b"test-harness: CLOCK_REALTIME is not a plausible date\n");
    }

    timer_sleep_ms(20);
    let mut second: u64 = 0;
    // SAFETY: valid u64 out-param.
    let r2 = unsafe { syscall4(SYS_CLOCK_READ, CLOCK_REALTIME, (&raw mut second) as u64, 0, 0) };
    if r2 != 0 || second <= first {
        return_fail(b"test-harness: CLOCK_REALTIME did not advance\n");
    }
    // …at roughly wall-clock rate: a 20 ms nap must not show up as an hour (a broken multiplier).
    if second - first > 60_000_000_000 {
        return_fail(b"test-harness: CLOCK_REALTIME advanced implausibly fast\n");
    }
    kprint(b"test-harness: wall-clock ok (plausible date, advancing)\n");
}

/// **A dead stage's pipe closes**, the regression test for exit-time handle reclamation: `list` on
/// an unresolvable path exits non-zero without writing, and the consumer's read must end in
/// `PeerClosed`, not block for ever (its end of the pipe once stayed open).
fn dead_stage_closes_its_pipe_demo(root_ns: u64, notif: u64) {
    use libstream::channel::{ChannelReceiver, IpcPort};
    use libstream::setup::{Streams, bootstrap_arg0, pipe, send_setup};
    use libstream::wire::WireError;

    kprint(b"test-harness: dead-stage pipe-close demo\n");
    let (st, img) = ns_lookup_wait(root_ns, b"/bin/list", RIGHT_MAP_READ);
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
    #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
    // SAFETY: SPAWN_LIST is our static; the coreutil spawns are sequential; `img` is ours.
    let _proc = match unsafe {
        SPAWN_LIST.image = img;
        SPAWN_LIST.handles[0] = setup_stage;
        SPAWN_LIST.arg0 = bootstrap_arg0(true);
        let p = spawn(&*(&raw const SPAWN_LIST));
        syscall1(SYS_HANDLE_CLOSE, img);
        p
    } {
        Ok(p) => p,
        Err(_) => return_fail(b"test-harness: dead-stage spawn FAIL\n"),
    };
    let streams = Streams { stdin: None, stdout: Some(stdout), stderr: None };
    if send_setup(setup_shell, streams, &["list", "/nx-no-such-directory"]).is_err() {
        return_fail(b"test-harness: dead-stage send_setup FAIL\n");
    }

    // The stage writes nothing and exits non-zero: `PeerClosed`, not a hang.
    match ChannelReceiver::new(IpcPort::new(rx)).receive(1 << 20) {
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

/// **`copy`, the mutation side of the filesystem.**
///
/// It must *change* the filesystem correctly and **refuse** the cases that would silently go wrong.
/// On a fixture under `/system`: a file copy matches in size and bytes through a fresh resolve; an
/// existing destination is refused without `--force` (exit `2`) and left **unchanged**; `--force`
/// overwrites; a longer destination shrinks first, leaving no stale tail; a directory copy is
/// recursive.
fn copy_demo(root_ns: u64, notif: u64) {
    kprint(b"test-harness: copy demo (the mutation side)\n");

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

    if run_copy(root_ns, notif, &["copy", "/system/nx-copy/a.txt", "/system/nx-copy/c.txt"]) != 0 {
        return_fail(b"test-harness: copy file exited non-zero\n");
    }
    if !file_matches(root_ns, b"/system/nx-copy/c.txt", COPY_CONTENT_A) {
        return_fail(b"test-harness: copied file content MISMATCH\n");
    }

    let code = run_copy(root_ns, notif, &["copy", "/system/nx-copy/sub/b.txt", "/system/nx-copy/c.txt"]);
    if code == 0 {
        return_fail(b"test-harness: copy over an existing file wrongly succeeded\n");
    }
    if !file_matches(root_ns, b"/system/nx-copy/c.txt", COPY_CONTENT_A) {
        return_fail(b"test-harness: refused copy still modified the destination\n");
    }

    if run_copy(root_ns, notif, &["copy", "--force", "/system/nx-copy/sub/b.txt", "/system/nx-copy/c.txt"]) != 0 {
        return_fail(b"test-harness: copy --force exited non-zero\n");
    }
    if !file_matches(root_ns, b"/system/nx-copy/c.txt", COPY_CONTENT_B) {
        return_fail(b"test-harness: --force did not overwrite\n");
    }

    // A LONGER destination must end up **exactly** the source: a stale tail fails the size check.
    write_file(root_ns, b"/system/nx-copy/long.txt", COPY_CONTENT_LONG);
    let code = run_copy(root_ns, notif, &["copy", "--force", "/system/nx-copy/a.txt", "/system/nx-copy/long.txt"]);
    if code != 0 {
        return_fail(b"test-harness: copy over a longer file failed\n");
    }
    if !file_matches(root_ns, b"/system/nx-copy/long.txt", COPY_CONTENT_A) {
        return_fail(b"test-harness: overwrite left a stale tail (truncate did not take)\n");
    }

    if run_copy(root_ns, notif, &["copy", "/system/nx-copy/sub", "/system/nx-copy/sub2"]) != 0 {
        return_fail(b"test-harness: copy directory exited non-zero\n");
    }
    if !file_matches(root_ns, b"/system/nx-copy/sub2/b.txt", COPY_CONTENT_B) {
        return_fail(b"test-harness: recursive copy did not reproduce the tree\n");
    }

    unlink_all(root_ns, b"/system/nx-copy/sub", &[b"b.txt"]);
    unlink_all(root_ns, b"/system/nx-copy/sub2", &[b"b.txt"]);
    unlink_all(root_ns, b"/system/nx-copy", &[b"a.txt", b"c.txt", b"long.txt"]);
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

/// Fixture contents: `A` and `B` are the same length (a same-size `--force`); `LONG` exercises the
/// shrink.
const COPY_CONTENT_A: &[u8] = b"alpha content, fixed length.\n";
const COPY_CONTENT_B: &[u8] = b"bravo content, fixed length.\n";
const COPY_CONTENT_LONG: &[u8] = b"a much longer destination whose tail would survive a short overwrite\n";

/// **`mkdir` and `remove`.**
///
/// Real Tier-1 stages, on the cases where an implementation can look right and be wrong:
/// `--parents` idempotence (not decidable from the error code), the `--recursive` rail, `--force`
/// suppressing *only* absence, and `remove` refusing a **namespace binding** so a recursive delete
/// can never unbind a mount point.
fn mkdir_remove_demo(root_ns: u64, notif: u64) {
    kprint(b"test-harness: mkdir/remove demo (Milestone 2 Part A)\n");
    const MK: &[u8] = b"/bin/mkdir";
    const RM: &[u8] = b"/bin/remove";

    if run_coreutil(root_ns, notif, MK, &["mkdir", "/system/nx-a"]) != 0 {
        return_fail(b"test-harness: mkdir exited non-zero\n");
    }
    if !path_exists(root_ns, b"/system/nx-a") {
        return_fail(b"test-harness: mkdir did not create the directory\n");
    }
    if run_coreutil(root_ns, notif, MK, &["mkdir", "/system/nx-a"]) == 0 {
        return_fail(b"test-harness: mkdir over an existing path wrongly succeeded\n");
    }

    if run_coreutil(root_ns, notif, MK, &["mkdir", "--parents", "/system/nx-a/b/c"]) != 0 {
        return_fail(b"test-harness: mkdir --parents exited non-zero\n");
    }
    if !path_exists(root_ns, b"/system/nx-a/b/c") {
        return_fail(b"test-harness: mkdir --parents did not build the chain\n");
    }
    if run_coreutil(root_ns, notif, MK, &["mkdir", "--parents", "/system/nx-a/b/c"]) != 0 {
        return_fail(b"test-harness: mkdir --parents was not idempotent\n");
    }

    if run_coreutil(root_ns, notif, RM, &["remove", "/system/nx-a"]) == 0 {
        return_fail(b"test-harness: remove of a directory wrongly succeeded\n");
    }
    if !path_exists(root_ns, b"/system/nx-a") {
        return_fail(b"test-harness: refused remove still deleted the directory\n");
    }

    write_file(root_ns, b"/system/nx-a/f.txt", b"part a\n");
    if run_coreutil(root_ns, notif, RM, &["remove", "/system/nx-a/f.txt"]) != 0 {
        return_fail(b"test-harness: remove file exited non-zero\n");
    }
    if path_exists(root_ns, b"/system/nx-a/f.txt") {
        return_fail(b"test-harness: remove did not delete the file\n");
    }

    if run_coreutil(root_ns, notif, RM, &["remove", "/system/nx-a/gone.txt"]) == 0 {
        return_fail(b"test-harness: remove of a missing path wrongly succeeded\n");
    }
    if run_coreutil(root_ns, notif, RM, &["remove", "--force", "/system/nx-a/gone.txt"]) != 0 {
        return_fail(b"test-harness: remove --force on a missing path failed\n");
    }

    // A namespace binding is refused. `/dev` isolates the check: `/dev/console` fails with or
    // without it, but `/dev` is a binding under a real directory, so without the refusal `--force`
    // turns it into a silent exit 0.
    if run_coreutil(root_ns, notif, RM, &["remove", "--force", "/dev"]) == 0 {
        return_fail(b"test-harness: remove of a namespace binding wrongly succeeded\n");
    }
    if !path_exists(root_ns, b"/dev/console") {
        return_fail(b"test-harness: refused remove disturbed the console binding\n");
    }

    write_file(root_ns, b"/system/nx-a/b/c/deep.txt", b"deep\n");
    if run_coreutil(root_ns, notif, RM, &["remove", "--recursive", "/system/nx-a"]) != 0 {
        return_fail(b"test-harness: remove --recursive exited non-zero\n");
    }
    if path_exists(root_ns, b"/system/nx-a") {
        return_fail(b"test-harness: remove --recursive left the tree behind\n");
    }

    // A relative path with no working directory must fail loud with the *specific* `EXIT_USAGE`
    // (2), not resolve against an arbitrary root; "non-zero" would pass on a segfault.
    if run_coreutil(root_ns, notif, MK, &["mkdir", "relative-name"]) != 2 {
        return_fail(b"test-harness: a relative path without PWD did not fail as a usage error\n");
    }

    kprint(b"test-harness: mkdir/remove ok (created, refused, forced, recursed, binding safe, relative refused)\n");
}

/// **`rename` and `move`.**
///
/// `move`'s correctness lives on a *failure* path, so the cases are about **which** operation ran:
/// a same-mount `move` must report `method = "rename"` (the file arriving would pass for a copy),
/// and a cross-mount `move` must *detect* the case and not destroy the source when the fallback
/// cannot complete.
fn rename_move_demo(root_ns: u64, notif: u64) {
    use libstream::Value;
    use libstream::table::{Item, TableReader};

    kprint(b"test-harness: rename/move demo (Milestone 2 Part B)\n");
    const RN: &[u8] = b"/bin/rename";
    const MV: &[u8] = b"/bin/move";

    let mut buf = [0u8; 4096];
    let mut sys = match Dir::open(root_ns, b"/system", &mut buf) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: rename fixture open FAIL\n"),
    };
    let _ = sys.mkdir(b"nx-b");
    let _ = sys.mkdir(b"nx-b2");
    sys.close();
    write_file(root_ns, b"/system/nx-b/one.txt", b"part b\n");

    if run_coreutil(root_ns, notif, RN, &["rename", "/system/nx-b/one.txt", "/system/nx-b/two.txt"]) != 0 {
        return_fail(b"test-harness: rename exited non-zero\n");
    }
    if path_exists(root_ns, b"/system/nx-b/one.txt") || !path_exists(root_ns, b"/system/nx-b/two.txt") {
        return_fail(b"test-harness: rename did not re-point the name\n");
    }

    if run_coreutil(root_ns, notif, RN, &["rename", "/system/nx-b/two.txt", "/system/nx-b2/three.txt"]) != 0 {
        return_fail(b"test-harness: cross-directory rename exited non-zero\n");
    }
    if !path_exists(root_ns, b"/system/nx-b2/three.txt") {
        return_fail(b"test-harness: cross-directory rename lost the file\n");
    }

    write_file(root_ns, b"/system/nx-b/keep.txt", b"keep\n");
    if run_coreutil(root_ns, notif, RN, &["rename", "/system/nx-b2/three.txt", "/system/nx-b/keep.txt"]) == 0 {
        return_fail(b"test-harness: rename over an existing name wrongly succeeded\n");
    }
    if !path_exists(root_ns, b"/system/nx-b2/three.txt") {
        return_fail(b"test-harness: refused rename still moved the source\n");
    }
    if run_coreutil(root_ns, notif, RN, &["rename", "--force", "/system/nx-b2/three.txt", "/system/nx-b/keep.txt"]) != 0
    {
        return_fail(b"test-harness: rename --force exited non-zero\n");
    }

    let bytes =
        run_coreutil_capture(root_ns, notif, MV, &["move", "/system/nx-b/keep.txt", "/system/nx-b2/moved.txt"], true);
    let mut method_seen = false;
    if let Ok(mut tr) = TableReader::new(&bytes) {
        while let Some(Ok(item)) = tr.next() {
            if let Item::Row(vals) = item
                && matches!(&vals[2], Value::Str(s) if s == "rename")
            {
                method_seen = true;
            }
        }
    }
    if !method_seen {
        return_fail(b"test-harness: same-mount move did not report method=rename\n");
    }
    if path_exists(root_ns, b"/system/nx-b/keep.txt") || !path_exists(root_ns, b"/system/nx-b2/moved.txt") {
        return_fail(b"test-harness: move did not relocate the file\n");
    }

    // `/initramfs` is a read-only kernel server: the kernel reports a cross-device rename and
    // `move`'s copy fallback must fail, leaving the original.
    if run_coreutil(root_ns, notif, MV, &["move", "/system/nx-b2/moved.txt", "/initramfs/moved.txt"]) == 0 {
        return_fail(b"test-harness: cross-mount move into a read-only server wrongly succeeded\n");
    }
    if !path_exists(root_ns, b"/system/nx-b2/moved.txt") {
        return_fail(b"test-harness: failed cross-mount move destroyed the source\n");
    }

    // A cross-mount move that SUCCEEDS, the half that never ran: `/scratch` is the fs-server bound
    // a second time.
    let bytes =
        run_coreutil_capture(root_ns, notif, MV, &["move", "/system/nx-b2/moved.txt", "/scratch/landed.txt"], true);
    // `method = "copy"`, specifically *not* "rename": a second binding that was not a distinct
    // mount would rename.
    let mut copied = false;
    if let Ok(mut tr) = TableReader::new(&bytes) {
        while let Some(Ok(item)) = tr.next() {
            if let Item::Row(vals) = item
                && matches!(&vals[2], Value::Str(s) if s == "copy")
            {
                copied = true;
            }
        }
    }
    if !copied {
        return_fail(b"test-harness: cross-mount move did not report method=copy\n");
    }
    if path_exists(root_ns, b"/system/nx-b2/moved.txt") {
        return_fail(b"test-harness: cross-mount move left the original behind\n");
    }
    if !path_exists(root_ns, b"/scratch/landed.txt") {
        return_fail(b"test-harness: cross-mount move did not land the copy\n");
    }
    if !file_matches(root_ns, b"/scratch/landed.txt", b"part b\n") {
        return_fail(b"test-harness: cross-mount move corrupted the contents\n");
    }

    // A whole TREE across the mount, a file at each level: a copy that dropped the nested level
    // must fail.
    {
        let mut tb = [0u8; 4096];
        if let Ok(mut d) = Dir::open(root_ns, b"/system", &mut tb) {
            let _ = d.mkdir(b"nx-tree");
            d.close();
        }
        let mut tb2 = [0u8; 4096];
        if let Ok(mut d) = Dir::open(root_ns, b"/system/nx-tree", &mut tb2) {
            let _ = d.mkdir(b"inner");
            d.close();
        }
    }
    write_file(root_ns, b"/system/nx-tree/top.txt", b"top\n");
    write_file(root_ns, b"/system/nx-tree/inner/deep.txt", b"deep\n");

    if run_coreutil(root_ns, notif, MV, &["move", "/system/nx-tree", "/scratch/tree"]) != 0 {
        return_fail(b"test-harness: cross-mount directory move exited non-zero\n");
    }
    if !file_matches(root_ns, b"/scratch/tree/top.txt", b"top\n")
        || !file_matches(root_ns, b"/scratch/tree/inner/deep.txt", b"deep\n")
    {
        return_fail(b"test-harness: cross-mount directory move did not carry the tree\n");
    }
    if path_exists(root_ns, b"/system/nx-tree/inner/deep.txt")
        || path_exists(root_ns, b"/system/nx-tree/top.txt")
        || is_dir_present(root_ns, b"/system/nx-tree")
    {
        return_fail(b"test-harness: cross-mount directory move left the source behind\n");
    }

    // A namespace binding is refused as a source, or `move` would copy *through* a mount and remove
    // another server's tree. The operand is `/subtreetest`, not `/dev`: `/dev` is not an openable
    // directory, so it fails with or without the refusal, whereas without it `move` of
    // `/subtreetest` (the fs-server based at `/system`) would delete `/system`.
    if run_coreutil(root_ns, notif, MV, &["move", "/subtreetest", "/scratch/st"]) == 0 {
        return_fail(b"test-harness: move of a namespace binding wrongly succeeded\n");
    }
    // The destination was never even started; an exit code cannot tell "refused" from "failed
    // partway".
    if is_dir_present(root_ns, b"/scratch/st") {
        return_fail(b"test-harness: move of a binding copied through the mount\n");
    }
    if !path_exists(root_ns, b"/subtreetest/current-generation") {
        return_fail(b"test-harness: refused move of a binding damaged it\n");
    }

    unlink_all(root_ns, b"/scratch", &[b"landed.txt"]);
    remove_tree_best_effort(root_ns, b"/scratch/tree");
    let mut b3 = [0u8; 4096];
    if let Ok(mut d) = Dir::open(root_ns, b"/system", &mut b3) {
        let _ = d.rmdir(b"nx-b");
        let _ = d.rmdir(b"nx-b2");
        d.close();
    }

    kprint(b"test-harness: rename/move ok (re-pointed, refused, forced, method=rename, cross-mount file + tree, binding safe)\n");
}

/// **`touch`.**
///
/// Case 2 matters: stamping an *existing* file must move its `mtime` forward, as asserting only
/// that `touch` creates files would pass an implementation with no stamping. The wait for the next
/// second is not padding: ext4 `mtime` is whole seconds.
fn touch_demo(root_ns: u64, notif: u64) {
    kprint(b"test-harness: touch demo (Milestone 2 Part C)\n");
    const TOUCH: &[u8] = b"/bin/touch";

    let mut buf = [0u8; 4096];
    let mut sys = match Dir::open(root_ns, b"/system", &mut buf) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: touch fixture open FAIL\n"),
    };
    let _ = sys.mkdir(b"nx-c");
    sys.close();

    if run_coreutil(root_ns, notif, TOUCH, &["touch", "/system/nx-c/new.txt"]) != 0 {
        return_fail(b"test-harness: touch exited non-zero\n");
    }
    if !path_exists(root_ns, b"/system/nx-c/new.txt") {
        return_fail(b"test-harness: touch did not create the file\n");
    }

    write_file(root_ns, b"/system/nx-c/old.txt", b"part c\n");
    let (before, _) = stat_entry(root_ns, b"/system/nx-c", b"old.txt");
    if before == 0 {
        return_fail(b"test-harness: touch fixture has no timestamp (wall clock unset?)\n");
    }
    wait_for_next_second();
    if run_coreutil(root_ns, notif, TOUCH, &["touch", "/system/nx-c/old.txt"]) != 0 {
        return_fail(b"test-harness: touch on an existing file exited non-zero\n");
    }
    let (after, _) = stat_entry(root_ns, b"/system/nx-c", b"old.txt");
    if after <= before {
        return_fail(b"test-harness: touch did not advance mtime\n");
    }

    if run_coreutil(root_ns, notif, TOUCH, &["touch", "--no-create", "/system/nx-c/absent.txt"]) != 0 {
        return_fail(b"test-harness: touch --no-create on a missing path failed\n");
    }
    if path_exists(root_ns, b"/system/nx-c/absent.txt") {
        return_fail(b"test-harness: touch --no-create created the file anyway\n");
    }

    if run_coreutil(root_ns, notif, TOUCH, &["touch", "/system/nx-c"]) == 0 {
        return_fail(b"test-harness: touch on a directory wrongly succeeded\n");
    }

    unlink_all(root_ns, b"/system/nx-c", &[b"new.txt", b"old.txt"]);
    let mut b2 = [0u8; 4096];
    if let Ok(mut d) = Dir::open(root_ns, b"/system", &mut b2) {
        let _ = d.rmdir(b"nx-c");
        d.close();
    }

    kprint(b"test-harness: touch ok (created, stamped, --no-create skipped, directory refused)\n");
}

/// **`date` and `sleep`.**
///
/// The calendar conversion and duration parser are host-tested; this covers the kernel half.
/// `sleep` genuinely **waits**, the assertion with teeth, as exiting zero proves nothing about a
/// `sleep` that returned at once.
fn date_sleep_demo(root_ns: u64, notif: u64) {
    use libstream::Value;
    use libstream::table::{Item, TableReader};

    kprint(b"test-harness: date/sleep demo (Milestone 2 Part D)\n");
    const DATE: &[u8] = b"/bin/date";
    const SLEEP: &[u8] = b"/bin/sleep";

    let ours = realtime_secs();
    if ours == 0 {
        return_fail(b"test-harness: wall clock unset, cannot check date\n");
    }
    let bytes = run_coreutil_capture(root_ns, notif, DATE, &["date"], true);
    let mut unix_field = 0i64;
    let mut year_field = 0i64;
    let mut fields = 0usize;
    if let Ok(mut tr) = TableReader::new(&bytes) {
        fields = tr.schema().fields.len();
        while let Some(Ok(item)) = tr.next() {
            if let Item::Row(vals) = item {
                if let Value::Int(u) = vals[0] {
                    unix_field = u;
                }
                if let Value::Int(y) = vals[1] {
                    year_field = y;
                }
            }
        }
    }
    if fields != 7 {
        return_fail(b"test-harness: date schema is not the seven expected fields\n");
    }
    let skew = if unix_field > ours { unix_field - ours } else { ours - unix_field };
    if skew > 60 {
        return_fail(b"test-harness: date disagrees with our own clock reading\n");
    }
    // The calendar arithmetic is host-tested; a year of 1970 would mean the epoch fell through
    // unconverted.
    if year_field < 2020 {
        return_fail(b"test-harness: date reported an implausible year\n");
    }

    let bytes = run_coreutil_capture(root_ns, notif, DATE, &["date", "--unix"], true);
    if let Ok(tr) = TableReader::new(&bytes) {
        if tr.schema().fields.len() != 1 {
            return_fail(b"test-harness: date --unix did not narrow the schema\n");
        }
    } else {
        return_fail(b"test-harness: date --unix produced no readable table\n");
    }

    // The lower bound is the whole assertion; no upper bound, as TCG scheduling delay is unbounded.
    let before = monotonic_ns();
    if run_coreutil(root_ns, notif, SLEEP, &["sleep", "200ms"]) != 0 {
        return_fail(b"test-harness: sleep exited non-zero\n");
    }
    let elapsed = monotonic_ns().saturating_sub(before);
    if elapsed < 200_000_000 {
        return_fail(b"test-harness: sleep returned before its duration elapsed\n");
    }

    if run_coreutil(root_ns, notif, SLEEP, &["sleep", "5x"]) == 0 {
        return_fail(b"test-harness: sleep accepted a malformed duration\n");
    }

    kprint(b"test-harness: date/sleep ok (fields agree, schema narrows, wait observed)\n");
}

fn monotonic_ns() -> u64 {
    // SAFETY: CLOCK_BUF is a valid writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: `CLOCK_BUF` is a plain `u64` static only this thread touches; a failed clock read
    // leaves its last value.
    unsafe { (&raw const CLOCK_BUF).read() }
}

/// **`whoami`.**
///
/// Identity is a *session* concept published by `session-mgr` at `/session/user`, not a kernel
/// fact. Given a namespace with the name bound `whoami` reports it; given one **without**, it fails
/// rather than inventing a default. Both shapes are built here, the only way to test the absent
/// case.
fn whoami_demo(root_ns: u64, notif: u64) {
    use libstream::Value;
    use libstream::table::{Item, TableReader};

    kprint(b"test-harness: whoami demo (Milestone 2 Part E)\n");
    const WHOAMI: &[u8] = b"/bin/whoami";
    const NAME: &[u8] = b"harness-user";

    // SAFETY: register-only syscall creating a namespace we own
    let ns = unsafe { syscall0(SYS_NS_CREATE) };
    if ns < 0 {
        return_fail(b"test-harness: whoami ns_create FAIL\n");
    }
    let ns = ns as u64;
    publish_session_user(ns, NAME);
    // Nothing else is bound: the spawn hands the child an image *handle*, so `/session/user` alone
    // is a complete environment.

    let bytes = run_coreutil_capture_in(root_ns, notif, WHOAMI, &["whoami"], true, ns);
    let mut got_name = false;
    if let Ok(mut tr) = TableReader::new(&bytes) {
        if tr.schema().fields.len() != 1 || tr.schema().fields[0].name != "user" {
            return_fail(b"test-harness: whoami schema is not a single `user` field\n");
        }
        while let Some(Ok(item)) = tr.next() {
            if let Item::Row(vals) = item
                && matches!(&vals[0], Value::Str(s) if s.as_bytes() == NAME)
            {
                got_name = true;
            }
        }
    }
    if !got_name {
        return_fail(b"test-harness: whoami did not report the bound session user\n");
    }

    // SAFETY: register-only syscall creating a namespace we own
    let bare = unsafe { syscall0(SYS_NS_CREATE) };
    if bare < 0 {
        return_fail(b"test-harness: whoami bare ns_create FAIL\n");
    }
    let bare = bare as u64;
    if run_coreutil_in(root_ns, notif, WHOAMI, &["whoami"], bare) == 0 {
        return_fail(b"test-harness: whoami without a session wrongly succeeded\n");
    }

    // SAFETY: closing namespaces this demo created.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, ns);
        syscall1(SYS_HANDLE_CLOSE, bare);
    }
    kprint(b"test-harness: whoami ok (reported the session user; no session is an error)\n");
}

/// The ABI pass: `AlreadyExists` and `NotEmpty` must arrive **as themselves** across both
/// boundaries that carry a `KError`.
///
/// Host tests pin the discriminants but run no syscall or wire round trip: the kernel returns its
/// error in a syscall's `isize`, the fs-server in an `ErrorBody` the client parses back.
fn error_granularity_demo(root_ns: u64) {
    kprint(b"test-harness: error-granularity demo (ABI pass)\n");

    // `NsError::AlreadyBound` once shared `InvalidArgument` with a malformed path.
    // SAFETY: a page-sized anonymous object, and a fresh namespace to bind it into.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, 4096, 0, 0, 0) };
    // SAFETY: register-only syscall creating a namespace we own
    let ns = unsafe { syscall4(SYS_NS_CREATE, 0, 0, 0, 0) };
    if mem < 0 || ns < 0 {
        return_fail(b"test-harness: error-granularity setup FAIL\n");
    }
    let (mem, ns) = (mem as u64, ns as u64);
    let path = b"/thing";
    let bind = |p: &[u8]| {
        // SAFETY: valid namespace handle, path, and object handle (direct-handle bind).
        unsafe { syscall6(SYS_NS_BIND, ns, p.as_ptr() as u64, p.len() as u64, mem, 0, 0) }
    };
    if bind(path) != 0 {
        return_fail(b"test-harness: error-granularity first bind FAIL\n");
    }
    let again = bind(path);
    if again != KError::AlreadyExists.as_i32() as i64 {
        return_fail(b"test-harness: duplicate ns_bind did not report AlreadyExists\n");
    }
    // The contrast: a *malformed* path must still be `InvalidArgument`.
    if bind(b"no-leading-slash") != KError::InvalidArgument.as_i32() as i64 {
        return_fail(b"test-harness: a malformed bind path did not report InvalidArgument\n");
    }
    // SAFETY: closing handles this demo created.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, mem);
        syscall1(SYS_HANDLE_CLOSE, ns);
    }

    {
        let mut buf = [0u8; 4096];
        let mut dir = match Dir::open(root_ns, b"/system", &mut buf) {
            Ok(d) => d,
            Err(_) => return_fail(b"test-harness: error-granularity open FAIL\n"),
        };
        if dir.mkdir(b"nx-eg").is_err() {
            return_fail(b"test-harness: error-granularity mkdir FAIL\n");
        }
        match dir.mkdir(b"nx-eg") {
            Err(DirError::Server(k)) if k == KError::AlreadyExists.as_i32() => {}
            Err(DirError::Server(k)) if k == KError::InvalidArgument.as_i32() => {
                return_fail(b"test-harness: mkdir collision still collapses to InvalidArgument\n")
            }
            _ => return_fail(b"test-harness: mkdir over an existing name did not report AlreadyExists\n"),
        }
        dir.close();
    }
    {
        let mut buf = [0u8; 4096];
        let mut kid = match Dir::open(root_ns, b"/system/nx-eg", &mut buf) {
            Ok(d) => d,
            Err(_) => return_fail(b"test-harness: error-granularity child open FAIL\n"),
        };
        let made = kid.mkdir(b"kid");
        kid.close();
        if made.is_err() {
            return_fail(b"test-harness: error-granularity child mkdir FAIL\n");
        }
    }
    {
        let mut buf = [0u8; 4096];
        let mut dir = match Dir::open(root_ns, b"/system", &mut buf) {
            Ok(d) => d,
            Err(_) => return_fail(b"test-harness: error-granularity reopen FAIL\n"),
        };
        match dir.rmdir(b"nx-eg") {
            Err(DirError::Server(k)) if k == KError::NotEmpty.as_i32() => {}
            Err(DirError::Server(k)) if k == KError::InvalidArgument.as_i32() => {
                return_fail(b"test-harness: rmdir of a populated directory still collapses\n")
            }
            _ => return_fail(b"test-harness: rmdir of a populated directory did not report NotEmpty\n"),
        }
        dir.close();
    }
    // Tidy up, confirming the same rmdir succeeds once empty.
    {
        let mut buf = [0u8; 4096];
        let mut kid = match Dir::open(root_ns, b"/system/nx-eg", &mut buf) {
            Ok(d) => d,
            Err(_) => return_fail(b"test-harness: error-granularity cleanup open FAIL\n"),
        };
        let gone = kid.rmdir(b"kid");
        kid.close();
        if gone.is_err() {
            return_fail(b"test-harness: error-granularity cleanup FAIL\n");
        }
    }
    {
        let mut buf = [0u8; 4096];
        let mut dir = match Dir::open(root_ns, b"/system", &mut buf) {
            Ok(d) => d,
            Err(_) => return_fail(b"test-harness: error-granularity final open FAIL\n"),
        };
        let gone = dir.rmdir(b"nx-eg");
        dir.close();
        if gone.is_err() {
            return_fail(b"test-harness: emptied directory still would not rmdir\n");
        }
    }
    kprint(b"test-harness: error granularity ok (AlreadyExists and NotEmpty cross both boundaries)\n");
}

/// **`nxsh` runs a real script against real processes**: what only this shows is that it resolves a
/// program image, creates pipes, spawns a process and turns a TSM1 stream back into a value on the
/// real kernel.
fn nxsh_demo(root_ns: u64, notif: u64) {
    kprint(b"test-harness: nxsh demo (Milestone 3 Parts C+D)\n");
    const NXSH: &[u8] = b"/bin/nxsh";

    // 1. The language in ring 3: the script compares its own answer, so a wrong result exits
    // non-zero.
    let ok = run_coreutil(root_ns, notif, NXSH, &["nxsh", "-c", "let x = 2 + 3\nlet y = x * 1.5\nif y != 7.5 { bad }"]);
    if ok != 0 {
        return_fail(b"test-harness: nxsh -c did not evaluate correctly\n");
    }

    // 2. A real external stage: spawn `list`, read the stream back, index the table.
    let ran =
        run_coreutil(root_ns, notif, NXSH, &["nxsh", "-c", "let t = list /system\nif (t[0]).name == \"\" { bad }"]);
    if ran != 0 {
        return_fail(b"test-harness: nxsh could not run a real pipeline stage\n");
    }

    // 2b. **`list /` — the root is a path, not a division sign.**
    let root = run_coreutil(root_ns, notif, NXSH, &["nxsh", "-c", "if (list / | count) < 1 { bad }"]);
    if root != 0 {
        return_fail(b"test-harness: nxsh could not list the root directory\n");
    }

    // 2c. The control: `/` must still divide; the two checks mean something only together.
    if run_coreutil(root_ns, notif, NXSH, &["nxsh", "-c", "if 6 / 2 != 3 { bad }"]) != 0 {
        return_fail(b"test-harness: nxsh stopped treating `/` as division\n");
    }

    // 3. A failing stage fails the script: `remove` of a missing path must reach the exit code.
    let failed = run_coreutil(root_ns, notif, NXSH, &["nxsh", "-c", "remove /system/nx-does-not-exist"]);
    if failed == 0 {
        return_fail(b"test-harness: a failing stage did not fail the script\n");
    }

    // 4. A name that is neither a binding nor a program is an error, not a silent skip.
    if run_coreutil(root_ns, notif, NXSH, &["nxsh", "-c", "definitely-not-a-program"]) == 0 {
        return_fail(b"test-harness: an unknown command wrongly succeeded\n");
    }

    // 5. One external stage at the head, then filter, sort, take, count in-process: one process
    // boundary.
    let piped = run_coreutil(
        root_ns,
        notif,
        NXSH,
        &["nxsh", "-c", "let n = list /system | filter size > 0 | sort size | take 2 | count\nif n != 2 { bad }"],
    );
    if piped != 0 {
        return_fail(b"test-harness: nxsh generic-operator pipeline failed\n");
    }

    // 6. `save` then `open` round-trips through the real filesystem.
    let saved = run_coreutil(
        root_ns,
        notif,
        NXSH,
        &[
            "nxsh",
            "-c",
            "list /system | take 1 | save /system/nx-d.tsm\nlet back = open /system/nx-d.tsm\nif (back | count) != 1 { bad }",
        ],
    );
    if saved != 0 {
        return_fail(b"test-harness: nxsh save/open round trip failed\n");
    }
    unlink_all(root_ns, b"/system", &[b"nx-d.tsm"]);

    // 7. A `def` with a default and the `_` fill, `match`, and `try`/`catch` over a real pipeline.
    let lang = run_coreutil(
        root_ns,
        notif,
        NXSH,
        &[
            "nxsh",
            "-c",
            // `try` is a *statement*, so the caught message lands in a `mut`.
            "def rows(t: Table, least: Int = 1) -> Int { t | filter size > 0 | count }\nlet n = list /system | rows(_, least: 1)\nlet kind = match n { 0 => \"empty\"\n _ => \"some\" }\nmut msg = \"none\"\ntry { let z = 1 / 0 } catch (e) { msg = e.message }\nif kind != \"some\" { bad }\nif msg != \"division by zero\" { bad }",
        ],
    );
    if lang != 0 {
        return_fail(b"test-harness: nxsh def/match/try did not run\n");
    }

    // 8. The REPL's language side; the console loop itself needs a device and a person.
    let repl = run_coreutil(root_ns, notif, NXSH, &["nxsh", "-c", "let n = list /system | count\nif n < 1 { bad }"]);
    if repl != 0 {
        return_fail(b"test-harness: nxsh repl-line evaluation failed\n");
    }

    // 9. `filter … ~= /…/` over a real listing: `grep`'s job was a missing *operator*.
    let re = run_coreutil(
        root_ns,
        notif,
        NXSH,
        &[
            "nxsh",
            "-c",
            "let all = list /system | count\nlet some = list /system | filter name ~= /^nx-/ | count\nif some >= all { bad }",
        ],
    );
    if re != 0 {
        return_fail(b"test-harness: nxsh regex filter failed\n");
    }

    // 10. **`cd /system`, then a *relative* path used two ways**: by the shell (`open ./nx-c.tsm`)
    // and by a spawned program (`list .`), which must agree. The shell passes `.` through rather
    // than pre-resolving, or the program's own resolution would go untested.
    let agree = run_coreutil(
        root_ns,
        notif,
        NXSH,
        &[
            "nxsh",
            "-c",
            "cd /system\nlet here = list . | count\nlet root = list /system | count\nif here != root { bad }\nlist /system | take 1 | save ./nx-c.tsm\nif (open ./nx-c.tsm | count) != 1 { bad }",
        ],
    );
    if agree != 0 {
        return_fail(b"test-harness: nxsh cd/relative-path agreement failed\n");
    }
    unlink_all(root_ns, b"/system", &[b"nx-c.tsm"]);

    kprint(b"test-harness: nxsh ok (evaluated, spawned, operators, save/open, def+match+try, repl, regex, cd)\n");
}

/// Publish `name` at `/session/user` in `ns`, the way `session-mgr` does for a login.
fn publish_session_user(ns: u64, name: &[u8]) {
    // SAFETY: a page-sized anonymous object.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, 4096, 0, 0, 0) };
    if mem < 0 {
        return_fail(b"test-harness: session user create FAIL\n");
    }
    let mem = mem as u64;
    // SAFETY: maps the object just created, read/write.
    let base = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, 4096, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if base < 0 {
        return_fail(b"test-harness: session user map FAIL\n");
    }
    // SAFETY: `base` is a live one-page writable mapping; `name` is far shorter.
    unsafe {
        let dst = core::slice::from_raw_parts_mut(base as *mut u8, 4096);
        dst[..name.len()].copy_from_slice(name);
        dst[name.len()] = 0;
        syscall4(SYS_MEMORY_UNMAP, base as u64, 4096, 0, 0);
    }
    let path = b"/session/user";
    // SAFETY: valid namespace handle, path, and object handle (direct-handle bind).
    let br = unsafe { syscall6(SYS_NS_BIND, ns, path.as_ptr() as u64, path.len() as u64, mem, 0, 0) };
    // SAFETY: closing our own handle; the bind cloned its own reference.
    unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
    if br != 0 {
        return_fail(b"test-harness: session user bind FAIL\n");
    }
}

fn run_copy(root_ns: u64, notif: u64, argv: &[&str]) -> i32 {
    run_coreutil(root_ns, notif, b"/bin/copy", argv)
}

/// Spawn a coreutil as a **Tier-1 stage** (setup message, `argv`, a `stdout` pipe) and return its
/// exit status; the image path is a parameter so `copy`, `mkdir` and `remove` share one spawn.
fn run_coreutil(root_ns: u64, notif: u64, image: &[u8], argv: &[&str]) -> i32 {
    run_coreutil_in(root_ns, notif, image, argv, 0)
}

/// As [`run_coreutil`], but the child runs in `child_ns` (`0` = inherit): `whoami` reads
/// `/session/user`, which exists only in a session namespace.
fn run_coreutil_in(root_ns: u64, notif: u64, image: &[u8], argv: &[&str], child_ns: u64) -> i32 {
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
    #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
    // SAFETY: SPAWN_LIST is our static (shared by the coreutil spawns, which are
    // sequential); initialised here for this spawn; `img` is ours.
    let _proc = match unsafe {
        SPAWN_LIST.image = img;
        SPAWN_LIST.handles[0] = setup_stage;
        SPAWN_LIST.arg0 = bootstrap_arg0(true);
        SPAWN_LIST.namespace = child_ns;
        let p = spawn(&*(&raw const SPAWN_LIST));
        syscall1(SYS_HANDLE_CLOSE, img);
        SPAWN_LIST.namespace = 0; // restore the shared default for later spawns
        p
    } {
        Ok(p) => p,
        Err(_) => return_fail(b"test-harness: coreutil spawn FAIL\n"),
    };
    let streams = Streams { stdin: None, stdout: Some(stdout), stderr: None };
    if send_setup(setup_shell, streams, argv).is_err() {
        return_fail(b"test-harness: copy send_setup FAIL\n");
    }
    // Drain first, then reap. A **failing** `copy` exits without writing, so the receive must end
    // in `PeerClosed`; that needs the exit-time handle sweep, without which this call hangs.
    let _ = ChannelReceiver::new(IpcPort::new(rx)).receive(1 << 20);
    let code = reap_child_exit(notif);
    drop(_proc);
    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, rx);
        syscall1(SYS_HANDLE_CLOSE, setup_shell);
    }
    code
}

/// **`list /dev` — a kernel-served directory is listable.**
///
/// Nothing is *mounted* at `/dev`: its rows come from kernel namespace bindings. `list /` must
/// still show filesystem entries, or the demo would pass against a `list` that stopped reading
/// filesystems.
fn dev_listing_demo(root_ns: u64, notif: u64) {
    kprint(b"test-harness: dev-listing demo (a kernel-served directory is listable)\n");

    let dev = run_list(root_ns, notif, &["list", "/dev"], true);
    if !contains(&dev, b"entropy") {
        return_fail(b"test-harness: list /dev did not name the entropy binding\n");
    }

    // `/` needs both sources: the root filesystem's entries plus the bindings alongside.
    let root = run_list(root_ns, notif, &["list", "/"], true);
    if !contains(&root, b"system") {
        return_fail(b"test-harness: list / lost its filesystem entries\n");
    }
    if !contains(&root, b"dev") {
        return_fail(b"test-harness: list / did not show the /dev binding\n");
    }

    kprint(b"test-harness: dev-listing ok (/dev from bindings, / from both sources)\n");
}

/// **The profile projects the coreutils into `/bin`.**
///
/// It resolves through the real chain (kernel forwarding to the profile server, which probes each
/// manifest package and re-exports the store `FileObject`), so a stale manifest hash, an unstaged
/// binary or an unbuilt package fails it. The **negative control matters more**: a resolve says
/// nothing about projection unless some `/bin/<name>` does not.
fn bin_projection_demo(root_ns: u64, notif: u64) {
    kprint(b"test-harness: bin-projection demo (the profile puts the coreutils on /bin)\n");

    // The negative control first: if this resolves, every later check is worthless.
    let (st, h) = ns_lookup_wait(root_ns, b"/bin/definitely-not-a-program", RIGHT_MAP_READ);
    if st == 0 && h != 0 {
        // SAFETY: closing the handle we were wrongly given.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
        return_fail(b"test-harness: /bin resolved a program that does not exist\n");
    }

    // Every profiled program, not one: a manifest can be wrong about a member.
    for prog in [
        b"/bin/list".as_slice(),
        b"/bin/copy",
        b"/bin/mkdir",
        b"/bin/remove",
        b"/bin/rename",
        b"/bin/move",
        b"/bin/touch",
        b"/bin/date",
        b"/bin/sleep",
        b"/bin/whoami",
        b"/bin/nxsh",
    ] {
        let (st, h) = ns_lookup_wait(root_ns, prog, RIGHT_MAP_READ);
        if st != 0 || h == 0 {
            return_fail(b"test-harness: a profiled program did not resolve on /bin\n");
        }
        // SAFETY: closing our own resolved handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
    }

    // The handle is the program, not merely a handle: spawn it and read its table.
    let out = run_coreutil_capture(root_ns, notif, b"/bin/list", &["list", "/"], true);
    if !contains(&out, b"system") {
        return_fail(b"test-harness: /bin/list did not produce a listing\n");
    }

    kprint(b"test-harness: bin-projection ok (11 programs via the store, unknown names still miss)\n");
}

/// **A log source whose writer is gone must be retired, not waited on forever.**
///
/// A channel whose peer closed is *permanently* signaled, so `logging-service` spun a CPU in
/// `sys_wait`. Worse, deferred handle reclamation ran only on the **idle thread**, so a CPU that
/// never idles reclaimed nothing and a dead stage's pipe stayed open (`list /bin` hung). **The
/// assertion is "a CPU goes idle", not "switches stay low"**: a lone spinner is preempted back to
/// itself and barely moves the switch counter.
fn dead_log_source_demo(root_ns: u64) {
    kprint(b"test-harness: dead-log-source demo (a closed source must be retired)\n");

    // Register a source and close the write end, as a principal that logs and exits does; the sleep
    // lets the service register it first.
    let (st, h) = ns_lookup_wait(root_ns, b"/log/system/nx-probe", RIGHT_SEND | RIGHT_TRANSFER);
    if st != 0 || h == 0 {
        return_fail(b"test-harness: could not open a log source\n");
    }
    timer_sleep_ms(200);
    // SAFETY: closing our own write end; the service holds the read end.
    unsafe { syscall1(SYS_HANDLE_CLOSE, h) };

    // This thread's own CPU never reads idle, so `online - 1`; take the best sample.
    let online = cpus_online(root_ns);
    if online < 2 {
        kprint(b"test-harness: dead-log-source skipped (needs >= 2 CPUs)\n");
        return;
    }
    // **Sample until the system is quiet, not for a fixed second.** A spin is *permanent*, so
    // exiting early once enough CPUs are idle is safe; a fixed 1 s window was a race against
    // `ui-testclient`'s concurrent window churn.
    const MAX_SAMPLES: u32 = 50; // 5 s at 100 ms, versus 1 s before
    let mut best = 0u64;
    for _ in 0..MAX_SAMPLES {
        timer_sleep_ms(100);
        let idle = idle_cpus(root_ns);
        if idle > best {
            best = idle;
        }
        if best + 1 >= online {
            break;
        }
    }
    if best + 1 < online {
        Line::new().s(b"test-harness: only ").u(best).s(b" of ").u(online).s(b" CPUs ever went idle").end();
        return_fail(b"test-harness: a closed log source left a CPU spinning\n");
    }
    Line::new()
        .s(b"test-harness: dead-log-source ok (")
        .u(best)
        .s(b" of ")
        .u(online)
        .s(b" CPUs idle -- the service let go)")
        .end();
}

/// Read `/proc/sched/stats` into `f` as text. `false` if it could not be read.
fn with_sched_stats(root_ns: u64, f: &mut dyn FnMut(&[u8])) -> bool {
    let (st, mem) = ns_lookup_wait(root_ns, b"/proc/sched/stats", RIGHT_MAP_READ);
    if st != 0 || mem == 0 {
        return false;
    }
    // SAFETY: `mem` is a MemoryObject handle carrying MAP_READ.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, PAGE, RIGHT_MAP_READ) };
    if addr < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
        return false;
    }
    // SAFETY: `addr` is a MAP_READ page holding the stats text, zero-padded.
    let text = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, PAGE as usize) };
    f(text);
    // SAFETY: unmapping the page we mapped (`text` is dead here) + closing our handle.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, addr as u64, PAGE);
        syscall1(SYS_HANDLE_CLOSE, mem);
    }
    true
}

fn idle_cpus(root_ns: u64) -> u64 {
    let mut n = 0;
    with_sched_stats(root_ns, &mut |text| {
        for line in text.split(|&b| b == b'\n') {
            if line.starts_with(b"cpu=") && parse_field(line, b"idle=") == Some(1) {
                n += 1;
            }
        }
    });
    n
}

fn cpus_online(root_ns: u64) -> u64 {
    let mut n = 0;
    with_sched_stats(root_ns, &mut |text| {
        for line in text.split(|&b| b == b'\n') {
            if let Some(v) = parse_field(line, b"cpus_online=") {
                n = v;
                return;
            }
        }
    });
    n
}

/// **`/bin` is listable, and the listing agrees with what a spawn would find.**
///
/// `/bin` is a *view*, the union of every profile package's `bin/`, so the profile server answers
/// `READ_DIR` itself. Listing and resolve share one index so they cannot drift; a program that
/// lists but will not spawn, or the reverse, is what this guards.
fn bin_listing_demo(root_ns: u64, notif: u64) {
    kprint(b"test-harness: bin-listing demo (the projected /bin is a real directory)\n");

    let out = run_coreutil_capture(root_ns, notif, b"/bin/list", &["list", "/bin"], true);

    // Every program the profile provides is listed, not one name.
    for prog in [
        b"list".as_slice(),
        b"copy",
        b"mkdir",
        b"remove",
        b"rename",
        b"move",
        b"touch",
        b"date",
        b"sleep",
        b"whoami",
        b"nxsh",
    ] {
        if !contains(&out, prog) {
            return_fail(b"test-harness: a profiled program is missing from `list /bin`\n");
        }
    }

    // **The union spans packages**: `heartbeat` and the coreutils are separate store packages.
    if !contains(&out, b"heartbeat") {
        return_fail(b"test-harness: `list /bin` did not merge across packages\n");
    }

    // The negative control: a listing that named everything would pass the rest.
    if contains(&out, b"definitely-not-a-program") {
        return_fail(b"test-harness: `list /bin` invented an entry\n");
    }

    kprint(b"test-harness: bin-listing ok (both packages merged, nothing invented)\n");
}

/// **The reaper thread is alive and being scheduled.**
///
/// Deferred reclamation once ran only at `exit_*`/`yield_now`/`suspend_with_fault` and in the idle
/// loop, so one busy-looping service starved it and hung peers waiting on a `PeerClosed`. **Its
/// failure mode is silence**: if its wake broke, the per-CPU sweep would cover for it, so
/// `reaper_closed` in `/proc/sched/stats` makes "it is running" an assertion; by now the harness
/// has reaped many children, so it cannot still be zero.
fn reaper_demo(root_ns: u64) {
    kprint(b"test-harness: reaper demo (deferred reclamation has a thread of its own)\n");
    let closed = stats_field(root_ns, b"reaper_closed=");
    if closed == 0 {
        return_fail(b"test-harness: the reaper has closed nothing -- parked, or never woken\n");
    }

    // **Exit-context teardown is on the path**: a process closes its own handle table at exit, so
    // `PeerClosed` arrives then; if that stopped, the reaper would cover for it.
    let early = stats_field(root_ns, b"exit_closed=");
    if early == 0 {
        return_fail(b"test-harness: no handles closed in exit context -- teardown is not running\n");
    }

    Line::new()
        .s(b"test-harness: reaper ok (reaper pids: ")
        .u(closed)
        .s(b", handles closed at exit: ")
        .u(early)
        .s(b")")
        .end();
}

/// One `name=value` field from `/proc/sched/stats`; `0` if unreadable, which callers treat as
/// failure.
fn stats_field(root_ns: u64, key: &[u8]) -> u64 {
    let (st, mem) = ns_lookup_wait(root_ns, b"/proc/sched/stats", RIGHT_MAP_READ);
    if st != 0 || mem == 0 {
        return 0;
    }
    // SAFETY: `mem` is a MemoryObject handle carrying MAP_READ.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, PAGE, RIGHT_MAP_READ) };
    if addr < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
        return 0;
    }
    // SAFETY: `addr` is a MAP_READ page holding the stats text, zero-padded.
    let text = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, PAGE as usize) };
    let mut out = 0u64;
    for line in text.split(|&b| b == b'\n') {
        if let Some(v) = parse_field(line, key) {
            out = v;
            break;
        }
    }
    // SAFETY: unmapping the page we mapped (`text` is dead here) + closing our handle.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, addr as u64, PAGE);
        syscall1(SYS_HANDLE_CLOSE, mem);
    }
    out
}

/// **`/dev/tty` is a capability, and writing to it is a request.**
///
/// Output once went through the ambient `SYS_DEBUG_KPRINT`, so a process with an empty namespace
/// could still write to the console; the tty server owns the device and hands out channels. This
/// exercises resolve, write, set mode and close, with the **negative control that
/// `/dev/tty/anything` must not resolve**.
fn tty_demo(root_ns: u64, notif: u64) {
    let _ = notif;
    kprint(b"test-harness: tty demo (terminal output is a capability)\n");

    let (st, tty) = ns_lookup_wait(root_ns, b"/dev/tty", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
    if st != 0 || tty == 0 {
        return_fail(b"test-harness: /dev/tty did not yield a channel\n");
    }

    // A write is a request that is answered, via the server's backend: the *capability* path.
    if !tty_request(tty, librsproto::OP_TTY_WRITE, 1, b"[tty demo wrote this]\r\n") {
        return_fail(b"test-harness: tty write was not acknowledged\n");
    }

    // Echo is the server's state, not a parameter each caller must remember.
    if !tty_request(tty, librsproto::OP_TTY_SET_MODE, 2, &[0]) {
        return_fail(b"test-harness: tty set-mode was not acknowledged\n");
    }
    if !tty_request(tty, librsproto::OP_TTY_SET_MODE, 3, &[librsproto::TTY_MODE_ECHO]) {
        return_fail(b"test-harness: tty set-mode (echo on) was not acknowledged\n");
    }

    // Close is the revocation point: handles cannot be taken back, so the server declining to serve
    // is the guarantee.
    if !tty_request(tty, librsproto::OP_TTY_CLOSE, 4, &[]) {
        return_fail(b"test-harness: tty close was not acknowledged\n");
    }
    // SAFETY: closing our own channel handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, tty) };

    let (st2, h2) = ns_lookup_wait(root_ns, b"/dev/tty/nope", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
    if st2 == 0 && h2 != 0 {
        // SAFETY: closing the handle we were wrongly given.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h2) };
        return_fail(b"test-harness: /dev/tty resolved a name it does not own\n");
    }

    kprint(b"test-harness: tty ok (write + mode + close acknowledged, unknown name refused)\n");
}

/// Send one tty request and wait for its reply; `true` if acknowledged without an error flag.
fn tty_request(ch: u64, op: u16, request_id: u64, body: &[u8]) -> bool {
    // SAFETY: TTY_MSG is a valid buffer; the rsproto message goes at offset 24.
    let sent = unsafe {
        let Some(rs_len) = librsproto::encode(&mut TTY_MSG[24..], op, request_id, 0, body, 0) else {
            return false;
        };
        TTY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        TTY_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            ch,
            (&raw const TTY_MSG) as u64,
            (&raw const TTY_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        ) == 0
    };
    if !sent {
        return false;
    }
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = ch;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    if waited < 1 {
        return false;
    }
    // SAFETY: single-threaded; `TTY_MSG` is lent for the call.
    let Ok(_sent) = (unsafe { libkern::channel_recv_raw(ch, &raw mut TTY_MSG) }) else {
        return false;
    };
    // SAFETY: bounded read-only slice over the reply.
    unsafe {
        let payload_len = u32::from_le_bytes([TTY_MSG[4], TTY_MSG[5], TTY_MSG[6], TTY_MSG[7]]) as usize;
        let rep = core::slice::from_raw_parts(((&raw const TTY_MSG) as *const u8).add(24), payload_len.min(4096 - 24));
        match librsproto::decode(rep) {
            Ok(m) => m.op == op && !m.is_error(),
            Err(_) => false,
        }
    }
}

static mut TTY_MSG: [u8; 4096] = [0; 4096];
static mut TTY_HANDLES: [u64; 8] = [0; 8];

/// Whether `needle` occurs in `hay`; names appear verbatim in a TSM1 listing.
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// **`mtime` survives an in-place overwrite.**
///
/// A **same-length** rewrite (map, change bytes, `sys_file_sync`) reaches the device with no
/// resolve and no IPC, so the fs-server never hears of it and `mtime` once kept reporting creation.
/// The check crosses a wall-clock second and asserts: the content changed (else a stale `mtime` and
/// a failed write look alike), `mtime` advanced, and the size did not (so it was the in-place
/// path).
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

    // Cross a wall-clock second: ext4 stores whole seconds.
    wait_for_next_second();

    overwrite_in_place(root_ns, b"/system/nx-mtime/f.txt", MTIME_AFTER);

    if !file_matches(root_ns, b"/system/nx-mtime/f.txt", MTIME_AFTER) {
        return_fail(b"test-harness: in-place overwrite did not take\n");
    }

    let (after_mtime, after_size) = stat_entry(root_ns, b"/system/nx-mtime", b"f.txt");
    if after_mtime <= before_mtime {
        return_fail(b"test-harness: in-place overwrite left mtime stale\n");
    }
    if after_size != before_size {
        return_fail(b"test-harness: overwrite changed the size, not the in-place path\n");
    }

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

/// Fixture contents, **the same length on purpose**: a different length would resize the file and
/// stamp `mtime` without the in-place path.
const MTIME_BEFORE: &[u8] = b"the original contents, fixed.\n";
const MTIME_AFTER: &[u8] = b"the rewritten contents, same.\n";

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

/// Block until the realtime clock's *second* changes; bounded, so a stopped clock fails the
/// caller's check.
fn wait_for_next_second() {
    let start = realtime_secs();
    for _ in 0..40 {
        if realtime_secs() != start {
            return;
        }
        timer_sleep_ms(100);
    }
}

fn realtime_secs() -> i64 {
    // SAFETY: CLOCK_BUF is a valid writable u64 out-param.
    let r = unsafe { syscall2(SYS_CLOCK_READ, CLOCK_REALTIME, (&raw mut CLOCK_BUF) as u64) };
    if r != 0 {
        return 0;
    }
    // SAFETY: the syscall wrote 8 bytes.
    (unsafe { (&raw const CLOCK_BUF).read() } / 1_000_000_000) as i64
}

/// Overwrite `path` with `content` **in place**: map, write, `sys_file_sync`; `content` must be the
/// file's current length.
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

/// **Server fan-out: more concurrent directory sessions than the old cap.**
///
/// `fs-server-ext4`'s client ceiling is the kernel's `MAX_WAIT_HANDLES` (once 8, giving 7 sessions,
/// which a shell pipeline reaches), and everything here derives from that constant rather than
/// pinning a number. Checks: all `MAX_SESSIONS` open at once; a `ReadDir` on a
/// session past the old 7 is really served; the next open is refused cleanly with `WouldBlock`; and
/// closing them all frees the slots, which a leaking server would fail.
fn session_fanout_demo(root_ns: u64) {
    use librsproto::session::DIR_SESSION_RIGHTS;
    kprint(b"test-harness: session fan-out demo (past the old 7-session cap)\n");

    /// The server's ceiling: its wait set is its endpoint, its control channel (which `init` keeps
    /// open for `Meta::Unmount`), and one slot per session.
    const MAX_SESSIONS: usize = MAX_WAIT_HANDLES - 2;
    const OLD_CAP: usize = 7;

    let mut held = [0u64; MAX_SESSIONS];

    for (i, slot) in held.iter_mut().enumerate() {
        let (st, h) = ns_lookup_wait(root_ns, b"/system", DIR_SESSION_RIGHTS);
        if st != 0 || h == 0 {
            // Report how far we got: the count is the diagnosis.
            Line::new().s(b"test-harness: session fan-out stopped early at session ").x(i as u64).end();
            return_fail(b"test-harness: could not open the full session table\n");
        }
        *slot = h;
    }

    // The one that matters: a handle can exist without the server waiting on it, and then this
    // round trip hangs.
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
    }
    if MAX_SESSIONS <= OLD_CAP {
        return_fail(b"test-harness: session cap did not actually move past 7\n");
    }

    let (st, h) = ns_lookup_wait(root_ns, b"/system", DIR_SESSION_RIGHTS);
    if st == 0 && h != 0 {
        return_fail(b"test-harness: opening past the session table wrongly succeeded\n");
    }
    if st != KERR_WOULD_BLOCK {
        return_fail(b"test-harness: a full session table did not report WouldBlock\n");
    }

    for h in held.iter() {
        // SAFETY: closing session handles this process owns.
        unsafe { syscall1(SYS_HANDLE_CLOSE, *h) };
    }
    // Reclamation is eventual, not synchronous: the server frees a slot on its **next wait**, and
    // asserting it on the first attempt lost that race about one boot in twenty. The serve loop's
    // two passes order reclaims first only within one wait batch, not across batches. Retry on
    // `WouldBlock`, sleeping between attempts.
    let (mut st, mut h) = (KERR_WOULD_BLOCK, 0u64);
    for attempt in 0..RECLAIM_ATTEMPTS {
        let r = ns_lookup_wait(root_ns, b"/system", DIR_SESSION_RIGHTS);
        st = r.0;
        h = r.1;
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

const KERR_WOULD_BLOCK: i32 = -11;

/// Budget for the server to reclaim closed slots: attempts and the pause between them (~2 s,
/// bounded if reclamation is broken).
const RECLAIM_ATTEMPTS: u32 = 40;
const RECLAIM_POLL_MS: u64 = 50;

/// **`rename` — re-pointing a name instead of copying it.**
///
/// No file data moves, so a reader never sees a half-written duplicate. Checks: within one
/// directory (the new name has the content *and* the old is gone, or it would pass for a copy);
/// across directories (`dir_repoint`); an occupied destination refused without `RENAME_REPLACE`
/// with *both* paths untouched; `RENAME_REPLACE` overwrites; a missing source fails; and a
/// cross-filesystem destination is refused with `Unsupported` by the *kernel*, which tells `move`
/// to fall back to copy + unlink.
fn rename_demo(root_ns: u64) {
    use librsproto::namespace::RENAME_REPLACE;
    kprint(b"test-harness: rename demo (re-point a name, don't copy it)\n");

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

    if rename_wait(root_ns, b"/system/nx-ren/a.txt", b"/system/nx-ren/a2.txt", 0) != 0 {
        return_fail(b"test-harness: rename within a directory FAILED\n");
    }
    if !file_matches(root_ns, b"/system/nx-ren/a2.txt", RENAME_CONTENT_A) {
        return_fail(b"test-harness: renamed file has the wrong content\n");
    }
    if path_exists(root_ns, b"/system/nx-ren/a.txt") {
        return_fail(b"test-harness: rename left the old name behind (it copied)\n");
    }

    if rename_wait(root_ns, b"/system/nx-ren/a2.txt", b"/system/nx-ren/sub/a3.txt", 0) != 0 {
        return_fail(b"test-harness: cross-directory rename FAILED\n");
    }
    if !file_matches(root_ns, b"/system/nx-ren/sub/a3.txt", RENAME_CONTENT_A) {
        return_fail(b"test-harness: cross-directory rename lost the content\n");
    }
    if path_exists(root_ns, b"/system/nx-ren/a2.txt") {
        return_fail(b"test-harness: cross-directory rename left the source behind\n");
    }

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

    let st = rename_wait(root_ns, b"/system/nx-ren/sub/a3.txt", b"/system/nx-ren/b.txt", RENAME_REPLACE as u64);
    if st != 0 {
        return_fail(b"test-harness: RENAME_REPLACE FAILED\n");
    }
    if !file_matches(root_ns, b"/system/nx-ren/b.txt", RENAME_CONTENT_A) {
        return_fail(b"test-harness: RENAME_REPLACE did not replace the destination\n");
    }
    if path_exists(root_ns, b"/system/nx-ren/sub/a3.txt") {
        return_fail(b"test-harness: RENAME_REPLACE left the source behind\n");
    }

    if rename_wait(root_ns, b"/system/nx-ren/nope.txt", b"/system/nx-ren/c.txt", 0) == 0 {
        return_fail(b"test-harness: rename of a missing source wrongly succeeded\n");
    }
    if path_exists(root_ns, b"/system/nx-ren/c.txt") {
        return_fail(b"test-harness: failed rename created the destination anyway\n");
    }

    let st = rename_wait(root_ns, b"/system/nx-ren/b.txt", b"/initramfs/b.txt", 0);
    if st != KERR_UNSUPPORTED {
        return_fail(b"test-harness: cross-filesystem rename was not refused with Unsupported\n");
    }
    if !file_matches(root_ns, b"/system/nx-ren/b.txt", RENAME_CONTENT_A) {
        return_fail(b"test-harness: refused cross-filesystem rename disturbed the source\n");
    }
    // …and with the *source* off the filesystem, a different kernel path with no forwarding arm.
    let st = rename_wait(root_ns, b"/bin/copy", b"/system/nx-ren/copy", 0);
    if st != KERR_UNSUPPORTED {
        return_fail(b"test-harness: rename off a non-filesystem source was not refused\n");
    }
    if path_exists(root_ns, b"/system/nx-ren/copy") {
        return_fail(b"test-harness: refused rename created a destination\n");
    }

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

/// Fixture contents, of different lengths so content cannot follow the name by accident.
const RENAME_CONTENT_A: &[u8] = b"alpha, the file that moves.\n";
const RENAME_CONTENT_B: &[u8] = b"bravo, the destination that is already occupied.\n";

const KERR_UNSUPPORTED: i32 = -52;

/// Issue a `sys_file_rename`, wait, and return the status (a rename resolves to no handle).
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
    po_wait(po as u64).0
}

/// Whether `path` resolves; [`file_matches`] cannot tell *absent* from *present with other
/// content*.
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
        syscall5(SYS_FILE_CREATE, ns, path.as_ptr() as u64, path.len() as u64, RIGHT_MAP_READ | RIGHT_MAP_WRITE, size)
    };
    if po < 0 {
        return_fail(b"test-harness: fixture create FAIL\n");
    }
    let (st, fh) = po_wait_pair(po as u64);
    if st != 0 || fh == 0 {
        return_fail(b"test-harness: fixture create FAIL\n");
    }
    // SAFETY: mapping our own writable file handle.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, fh, 0, size, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
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

/// Whether `path` resolves, freshly, to a file of exactly `expect`'s bytes (what reached the
/// filesystem).
fn file_matches(ns: u64, path: &[u8], expect: &[u8]) -> bool {
    let (st, fh) = ns_lookup_wait(ns, path, RIGHT_MAP_READ | RIGHT_INSPECT);
    if st != 0 || fh == 0 {
        return false;
    }
    let mut info = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
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

/// Whether `path` names a live directory; one does not resolve to a mappable object, so this opens
/// a session.
fn is_dir_present(ns: u64, path: &[u8]) -> bool {
    let mut buf = [0u8; 4096];
    match Dir::open(ns, path, &mut buf) {
        Ok(d) => {
            d.close();
            true
        }
        Err(_) => false,
    }
}

/// Tear down a tree without asserting, so a directory a failed case never created cannot fail the
/// run.
fn remove_tree_best_effort(ns: u64, path: &[u8]) {
    let mut names: alloc::vec::Vec<alloc::vec::Vec<u8>> = alloc::vec::Vec::new();
    let mut kinds: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    {
        let mut buf = [0u8; 4096];
        let Ok(mut d) = Dir::open(ns, path, &mut buf) else { return };
        let _ = d.read_dir(|e| {
            if e.name != b"." && e.name != b".." {
                names.push(e.name.to_vec());
                kinds.push(e.kind);
            }
            true
        });
        d.close();
    }
    for (i, n) in names.iter().enumerate() {
        let mut child = alloc::vec::Vec::from(path);
        child.push(b'/');
        child.extend_from_slice(n);
        if kinds[i] == librsproto::file::DIRENT_KIND_DIR {
            remove_tree_best_effort(ns, &child);
        } else {
            let mut buf = [0u8; 4096];
            if let Ok(mut d) = Dir::open(ns, path, &mut buf) {
                let _ = d.unlink(n);
                d.close();
            }
        }
    }
    let mut buf = [0u8; 4096];
    if let Ok(mut d) = Dir::open(ns, parent_of(path), &mut buf) {
        let _ = d.rmdir(basename_of(path));
        d.close();
    }
}

fn parent_of(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&c| c == b'/') {
        Some(0) => b"/",
        Some(i) => &path[..i],
        None => b".",
    }
}

fn basename_of(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&c| c == b'/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

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

fn po_wait_pair(po: u64) -> (i32, u64) {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid single-waiter buffers.
    let waited = unsafe {
        WAIT_HANDLES[0] = po;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    // SAFETY: bytes 8..24 are inside `WAIT_RESULTS`, a static of plain bytes only this thread
    // touches; they hold the wait's result only when it returned 1.
    let (status, result) = unsafe {
        (
            i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]),
            u64::from_le_bytes([
                WAIT_RESULTS[16],
                WAIT_RESULTS[17],
                WAIT_RESULTS[18],
                WAIT_RESULTS[19],
                WAIT_RESULTS[20],
                WAIT_RESULTS[21],
                WAIT_RESULTS[22],
                WAIT_RESULTS[23],
            ]),
        )
    };
    // SAFETY: closing the PO we own.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
    if waited != 1 { (-1, 0) } else { (status, result) }
}

/// **Hardware floating point, end to end in ring 3.**
///
/// Spawns `FP_WORKERS` copies of `test-stage`'s `"fp"` role with different seeds and requires every
/// one to exit `0` (`run_fp_worker` documents the codes). They run concurrently on purpose:
/// different live FP state on different CPUs means a context switch that cross-wired two register
/// files shows as one worker seeing another's values.
fn fp_hardfloat_demo(root_ns: u64, notif: u64) {
    const FP_WORKERS: usize = 3;
    // No "start" banner: session-mgr owns the PASS verdict and races this process, so under KVM the
    // run can be adjudicated mid-demo; the guarantee is session-mgr's `fp_gate`.
    use libstream::setup::{Streams, bootstrap_arg0, pipe, send_setup};
    // Each worker's seed travels in `argv` of the setup message, never in `arg0`.
    const SEEDS: [&str; FP_WORKERS] = ["1", "2", "3"];
    let (st, img) = ns_lookup_wait(root_ns, b"/bin/test-stage", RIGHT_MAP_READ);
    if st != 0 || img == 0 {
        kprint(b"test-harness: hard-float image lookup FAIL\n");
        exit(1);
    }
    let mut procs = [const { None }; FP_WORKERS];
    for (i, slot) in procs.iter_mut().enumerate() {
        let (setup_shell, setup_stage) = match pipe(4) {
            Ok(p) => p,
            Err(_) => {
                kprint(b"test-harness: hard-float setup chan FAIL\n");
                exit(1);
            }
        };
        #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
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
        let streams = Streams { stdin: None, stdout: None, stderr: None };
        if send_setup(setup_shell, streams, &["fp", SEEDS[i]]).is_err() {
            kprint(b"test-harness: hard-float send_setup FAIL\n");
            exit(1);
        }
        // SAFETY: closing our end of the setup channel; the message is delivered.
        unsafe { syscall1(SYS_HANDLE_CLOSE, setup_shell) };
    }
    let mut got = 0;
    while got < FP_WORKERS {
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
        let waited = unsafe {
            WAIT_HANDLES[0] = notif;
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
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
            let (kind, b) = unsafe { ((&raw const NOTIF.kind).read(), (&raw const NOTIF.body).read()) };
            if kind == KIND_CHILD_EXITED {
                let code = i32::from_le_bytes([b[8], b[9], b[10], b[11]]);
                if code != 0 {
                    Line::new().s(b"test-harness: hard-float worker FAILED code=").i(code as i64).end();
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

/// `/proc/self/status` + `/proc/sched/stats` demo: "two CPUs visibly active via `/proc`".
/// **Verdict-gated**: a failure exits nonzero, which init turns into a FAIL, so an SMP-liveness
/// regression fails `xtask test-qemu`.
///
/// `status` must show a spawned identity (pid ≥ 2, tid ≥ 1); `stats` must show **≥ 2 CPUs with
/// `switches` > 0**. It runs last, once the other demos have put work on several CPUs; counters
/// only grow, so it retries (100 ms, ~5 s) before declaring the run dead.
fn sched_stats_demo(root_ns: u64) {
    kprint(b"test-harness: sched-stats demo start\n");

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
    Line::new().s(b"test-harness: /proc/self/status ok pid=").u(pid).s(b" tid=").u(tid).end();
    // SAFETY: unmapping the page we mapped above (`text` is not used past here); closing our own
    // handle.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, addr as u64, PAGE);
        syscall1(SYS_HANDLE_CLOSE, mem);
    }

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
            // Echo the winning snapshot into the boot log.
            let len = text.iter().position(|&b| b == 0).unwrap_or(text.len());
            Line::new().s(b"test-harness: /proc/sched/stats ok (").u(active).s(b" CPUs with switches>0):").end();
            kprint(&text[..len]);
        }
        // SAFETY: unmapping the page mapped above (`text` is not used past
        // here); closing our own handle (each lookup minted a fresh snapshot).
        unsafe {
            syscall2(SYS_MEMORY_UNMAP, addr as u64, PAGE);
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

fn po_wait(po: u64) -> (i32, u64) {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
    let waited = unsafe {
        WAIT_HANDLES[0] = po;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    // SAFETY: bytes 8..24 are inside `WAIT_RESULTS`, a static of plain bytes only this thread
    // touches; they hold the wait's result only when it returned 1.
    let status = unsafe { i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]) };
    // SAFETY: bytes 8..24 are inside `WAIT_RESULTS`, a static of plain bytes only this thread
    // touches; they hold the wait's result only when it returned 1.
    let result = unsafe {
        u64::from_le_bytes([
            WAIT_RESULTS[16],
            WAIT_RESULTS[17],
            WAIT_RESULTS[18],
            WAIT_RESULTS[19],
            WAIT_RESULTS[20],
            WAIT_RESULTS[21],
            WAIT_RESULTS[22],
            WAIT_RESULTS[23],
        ])
    };
    // SAFETY: closing our own PO handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
    if waited != 1 { (-1, 0) } else { (status, result) }
}

/// Read sector 0 of block device `path` via `sys_io_submit`; returns the 16-bit value at offset 510
/// (`0xAA55`), or `-1`.
fn read_block_sector0(root_ns: u64, path: &[u8]) -> i32 {
    let (st, dev) = ns_lookup_wait(root_ns, path, RIGHT_READ);
    if st != 0 || dev == 0 {
        return -1;
    }
    // SAFETY: register-only syscall.
    let buf = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if buf < 0 {
        // SAFETY: closing our own handle
        unsafe { syscall1(SYS_HANDLE_CLOSE, dev) };
        return -1;
    }
    // SAFETY: `buf` is a fresh MemoryObject handle with full MAP rights.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, buf as u64, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    let mut sig: i32 = -1;
    if addr >= 0 {
        let op = IoOp { opcode: IO_OPCODE_READ, flags: 0, buffer: buf as u64, buf_offset: 0, offset: 0, length: 512 };
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

/// Block-storage demo: sector 0 of the first whole disk, of the first GPT partition (proving the
/// partition layer rebases the offset), and of the ESP under its stable `by-partlabel` name, each
/// verifying the boot signature.
///
/// **Which `/dev/blk/<n>` each is comes from the registry**, not boot order: a scratch RAM disk,
/// published before any partition, once made the line call the RAM disk a partition.
fn block_demo(root_ns: u64) {
    use libkern::device::DeviceKind;
    kprint(b"test-harness: /dev/blk demo start\n");
    let (st, snap) = ns_lookup_wait(root_ns, b"/dev/registry", RIGHT_MAP_READ | RIGHT_INSPECT);
    let blocks: alloc::vec::Vec<(u32, DeviceKind)> = if st == 0 && snap != 0 {
        let mut info = abi::HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
        // SAFETY: `info` is a writable 24-byte `HandleInfo`, the layout the kernel writes.
        let sr = unsafe { syscall2(SYS_HANDLE_STAT, snap, (&raw mut info) as u64) };
        let addr = if sr == 0 {
            // SAFETY: register-only syscall; `snap` is a MemoryObject handle with MAP_READ.
            unsafe { syscall4(SYS_MEMORY_MAP, snap, 0, info.size, RIGHT_MAP_READ) }
        } else {
            -1
        };
        let found = if addr > 0 {
            // SAFETY: `info.size` bytes are mapped read-only at `addr` until the unmap below.
            let bytes = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, info.size as usize) };
            let out = libkern::device::records(bytes)
                .map(|rs| rs.filter_map(|r| r.block_index().map(|n| (n, r.kind()))).collect())
                .unwrap_or_default();
            // SAFETY: unmapping what was mapped above; `out` holds copies.
            unsafe { syscall2(SYS_MEMORY_UNMAP, addr as u64, info.size) };
            out
        } else {
            alloc::vec::Vec::new()
        };
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, snap) };
        found
    } else {
        alloc::vec::Vec::new()
    };
    if blocks.is_empty() {
        kprint(b"test-harness: /dev/blk demo FAIL (the registry lists no block device)\n");
        return;
    }
    // **What each device is**, before reading any: the installer refuses anything but a `disk`.
    let path = |n: u32, leaf: &str| alloc::format!("/dev/blk/{n}{leaf}");
    for &(n, _) in &blocks {
        report_block_info(
            root_ns,
            path(n, "/info").as_bytes(),
            path(n, "").replacen("/dev", "test-harness: /dev", 1).as_bytes(),
        );
    }
    for (kind, word) in [(DeviceKind::Disk, "disk"), (DeviceKind::Partition, "partition")] {
        match blocks.iter().find(|&&(_, k)| k == kind) {
            Some(&(n, _)) => report_block_read(
                root_ns,
                path(n, "").as_bytes(),
                alloc::format!("test-harness: /dev/blk/{n} ({word}) read").as_bytes(),
            ),
            None => Line::new().s(b"test-harness: /dev/blk demo: no ").s(word.as_bytes()).s(b" to read FAIL").end(),
        }
    }
    report_block_read(
        root_ns,
        b"/dev/disk/by-partlabel/NITROX_ESP",
        b"test-harness: /dev/disk/by-partlabel/NITROX_ESP read",
    );
}

fn report_block_info(root_ns: u64, path: &[u8], label: &[u8]) {
    let (st, mem) = ns_lookup_wait(root_ns, path, RIGHT_MAP_READ);
    if st != 0 || mem == 0 {
        kprint(label);
        kprint(b" info FAIL (no leaf)\n");
        return;
    }
    let len = core::mem::size_of::<libkern::abi::BlockDeviceInfo>();
    // SAFETY: the leaf is a read-only object holding one record, and `len` is its size.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, len as u64, RIGHT_MAP_READ) };
    if addr < 0 {
        kprint(label);
        kprint(b" info FAIL (unmappable)\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
        return;
    }
    // SAFETY: `addr` maps `len` readable bytes the kernel filled with one record.
    let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, len) };
    match libkern::abi::BlockDeviceInfo::read(bytes) {
        Some(info) => {
            let mut line = Line::new();
            line.s(label)
                .s(b" is a ")
                .s(info.kind().name().as_bytes())
                .s(b", ")
                .u(info.byte_capacity() / (1024 * 1024))
                .s(b" MiB, named ")
                .untrusted(info.name())
                .end();
        }
        None => {
            kprint(label);
            kprint(b" info FAIL (short record)\n");
        }
    }
    // SAFETY: our own mapping and handle; nothing else refers to either.
    unsafe {
        syscall3(SYS_MEMORY_UNMAP, addr as u64, len as u64, 0);
        syscall1(SYS_HANDLE_CLOSE, mem);
    }
}

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

/// Userspace-server forwarding demo: the kernel's **transparent namespace forwarding** end to end
/// in one process, which plays both lookup client and resource server.
///
/// It binds a channel end at `/fs`, looks up `/fs/hello` (the kernel forwards a
/// `Namespace::Resolve` into the other end), parses that request, and replies transferring a
/// read-only `MemoryObject`; the kernel completes the lookup inline in our send.
fn forward_demo() {
    kprint(b"test-harness: userspace-server forwarding demo start\n");
    const CONTENT: &[u8] = b"STUB\n";

    // SAFETY: FWD_KEND/FWD_SEND are valid writable out-params.
    let cr = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut FWD_KEND) as u64, (&raw mut FWD_SEND) as u64, 4, 0) };
    if cr != 0 {
        kprint(b"test-harness: fwd channel create FAIL\n");
        return;
    }
    // SAFETY: the kernel wrote both endpoint handles.
    let (kend, send_end) = unsafe { ((&raw const FWD_KEND).read(), (&raw const FWD_SEND).read()) };

    // SAFETY: register-only syscall creating a namespace we own
    let ns = unsafe { syscall1(SYS_NS_CREATE, 0) };
    if ns < 0 {
        kprint(b"test-harness: fwd ns create FAIL\n");
        return;
    }
    let ns = ns as u64;
    let mount = b"/fs";
    // SAFETY: valid path pointer + namespace/endpoint handles. Binding an
    // `IpcChannel` makes the kernel adopt it as a Userspace Server.
    let br = unsafe { syscall4(SYS_NS_BIND, ns, mount.as_ptr() as u64, mount.len() as u64, kend) };
    if br != 0 {
        kprint(b"test-harness: fwd bind FAIL\n");
        return;
    }

    let path = b"/fs/hello";
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, RIGHT_MAP_READ) };
    if po < 0 {
        kprint(b"test-harness: fwd lookup submit FAIL\n");
        return;
    }
    let po = po as u64;

    // SAFETY: single-threaded; `FWD_REQ` is lent for the call.
    let Ok(_sent) = (unsafe { libkern::channel_recv_raw(send_end, &raw mut FWD_REQ) }) else {
        kprint(b"test-harness: fwd recv request FAIL\n");
        return;
    };

    // SAFETY: FWD_REQ was filled by the recv above; bytes 4..8 are inside it; this thread owns it.
    let payload_len = unsafe { u32::from_le_bytes([FWD_REQ[4], FWD_REQ[5], FWD_REQ[6], FWD_REQ[7]]) as usize };
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

    // SAFETY: register-only syscall creating a memory object we own
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if mem < 0 {
        kprint(b"test-harness: fwd memobj create FAIL\n");
        return;
    }
    let mem = mem as u64;
    // SAFETY: mapping the memory object we just created, to fill it
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        kprint(b"test-harness: fwd memobj map FAIL\n");
        return;
    }
    // SAFETY: `addr` is a page the kernel mapped R/W into our address space.
    unsafe {
        core::slice::from_raw_parts_mut(addr as u64 as *mut u8, CONTENT.len()).copy_from_slice(CONTENT);
    }

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
    // SAFETY: set the IpcMsg header's payload_len (@4) + handle_count (@8) and the transferred-
    // handle slot.
    unsafe {
        FWD_REPLY[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        FWD_REPLY[8] = 1;
        FWD_REPLY_H[0] = mem;
    }

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

    let (st, resolved) = po_wait(po);
    if st != 0 || resolved == 0 {
        kprint(b"test-harness: fwd lookup result FAIL\n");
        return;
    }

    // SAFETY: mapping the resolved memory object, read-only; the result is checked below
    let raddr = unsafe { syscall4(SYS_MEMORY_MAP, resolved, 0, PAGE, RIGHT_MAP_READ) };
    if raddr < 0 {
        kprint(b"test-harness: fwd map resolved FAIL\n");
        return;
    }
    // SAFETY: `raddr` is the mapped, kernel-installed MemoryObject.
    let matches = unsafe { core::slice::from_raw_parts(raddr as u64 as *const u8, CONTENT.len()) == CONTENT };
    if matches {
        kprint(b"test-harness: forwarded lookup returned 'STUB' via fs-server ok\n");
    } else {
        kprint(b"test-harness: fwd content mismatch\n");
    }
}

/// Bootstrap registers: `rdi` is `notif`, `rsi` is `root_ns`; the third is unused.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, _boot2: u64) -> ! {
    kprint(b"test-harness: up (demo supervisor, spawned by init)\n");

    // Order matters. The transport, stage, FP, directory and `list` demos run **early**: the login
    // chain owns the PASS verdict and races this process, so a demo placed last can be adjudicated
    // out from under it, as the FP check was under KVM, silently never running. Sched-stats runs
    // LAST, once the others have put work on several CPUs.
    stream_transport_demo();

    stage_spawn_demo(root_ns, notif);

    worker_exception_demo(notif);

    fp_hardfloat_demo(root_ns, notif);

    dir_list_demo(root_ns);

    dir_mutate_demo(root_ns);

    list_pipeline_demo(root_ns, notif);

    // The coreutil demos: each spawns real Tier-1 stages and cleans up its fixture under `/system`.
    copy_demo(root_ns, notif);
    mkdir_remove_demo(root_ns, notif);
    rename_move_demo(root_ns, notif);
    touch_demo(root_ns, notif);
    date_sleep_demo(root_ns, notif);
    whoami_demo(root_ns, notif);
    error_granularity_demo(root_ns);
    nxsh_demo(root_ns, notif);

    rename_demo(root_ns);

    session_fanout_demo(root_ns);

    mtime_overwrite_demo(root_ns);

    dev_listing_demo(root_ns, notif);

    bin_projection_demo(root_ns, notif);

    dead_log_source_demo(root_ns);

    bin_listing_demo(root_ns, notif);

    tty_demo(root_ns, notif);

    dead_stage_closes_its_pipe_demo(root_ns, notif);

    wall_clock_demo();

    // The kernel substrate: blocking sends, namespaces, entropy, `/proc`, the initramfs, block
    // devices and userspace-server forwarding.
    block_send_demo();
    block_bounded_demo();

    ns_demo();

    entropy_demo();

    dev_entropy_lookup_demo(root_ns);

    proc_self_demo(root_ns);

    initramfs_demo(root_ns);

    block_demo(root_ns);

    forward_demo();

    // Teardown racing spawn, then the check that reclamation does not depend on an idle CPU.
    exit_storm_demo(root_ns, notif);

    reaper_demo(root_ns);

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
