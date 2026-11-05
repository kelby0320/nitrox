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
use libkern::{
    IPC_PAYLOAD_SIZE, IoResult, KError, KIND_CHILD_EXITED, KOBJ_NAMESPACE, KOBJ_PROCESS, KOBJ_THREAD, MAX_WAIT_HANDLES,
    Notification, RIGHT_DUPLICATE, RIGHT_INSPECT, RIGHT_RECV, RIGHT_SEND, RIGHT_TRANSFER, RIGHT_WAIT, RawHandle,
    Rights,
};
use libos::boot::Start;
use libos::log::{self, Line};
use libos::thread::{self, Stack};
use libos::{
    Carried, Channel, Deadline, File, Handle, MapRead, MapReadWrite, Memory, Msg, Namespace, Notify, NsMutable,
    NsReadOnly, Only, Op, Process, ReadOnly, Ref, Resource, Spawn, Thread, Timer, WaitSet, block_on, entropy, exit, fs,
    time,
};
use librsproto::session::{Dir, DirError};
use libstream::setup::bootstrap_arg0;

mod fault_demo;
#[path = "refused/harness.rs"]
mod refused;

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// The namespace this process resolves names in, and the queue it learns of its children on.
type Root = Handle<Namespace, NsReadOnly>;
type Notif = Handle<Notify, Only>;

/// The full rights set an IPC endpoint carries, handed to each child.
const ENDPOINT_RIGHTS: Rights =
    Rights::from_bits(RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT | RIGHT_DUPLICATE | RIGHT_INSPECT | RIGHT_TRANSFER);

/// One page.
const PAGE: usize = 4096;

/// Rows the producer writes: enough to overflow the pipe's ring, so it blocks on backpressure.
const STREAM_ROWS: i64 = 2000;
const STREAM_STACK_PAGES: usize = 8;

/// The producer thread: write a `{ i: Int, name: String }` TSM1 stream of [`STREAM_ROWS`] rows,
/// then exit; the process keeps the endpoint, so the consumer sees no spurious `PeerClosed`.
/// `end` is the raw value of the endpoint it writes to.
extern "C" fn stream_producer(_: u64, _: u64, _: u64, end: u64) -> ! {
    use alloc::string::String;
    use libstream::channel::{ChannelSink, IpcPortRef};
    use libstream::table::TableWriter;
    use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

    // SAFETY: `end` is an endpoint `stream_transport_demo` owns and closes only once it has read
    // the whole stream, so it is open for as long as this thread uses it; the borrow closes nothing.
    let e0 = unsafe { Ref::<Channel, Only>::from_raw(RawHandle(end), Rights::empty()) };
    let schema =
        Schema::new().field("i", TypeTag::Int, TypeModifiers::NONE).field("name", TypeTag::String, TypeModifiers::NONE);
    let mut tw = TableWriter::new(ChannelSink::new(IpcPortRef::new(e0), IPC_PAYLOAD_SIZE));
    let ok = tw.write_schema(StreamFlags::NONE, &schema).is_ok()
        && (0..STREAM_ROWS).all(|i| tw.write_row(&[Value::Int(i), Value::Str(String::from("row"))]).is_ok())
        && tw.finish_with_status(0).is_ok()
        && tw.into_sink().finish().is_ok();
    let _ = ok; // a mismatch surfaces as the consumer's fail-loud verdict
    thread::exit(0)
}

/// Move a TSM1 stream over a **real IPC channel between two threads**: the producer on one end of a
/// small-ring pipe, main verifying [`STREAM_ROWS`] rows on the other, so the producer blocks on
/// backpressure in the real blocking-send path; a mismatch is fail-loud.
fn stream_transport_demo() {
    use libstream::channel::{ChannelReceiver, IpcPortRef};
    use libstream::table::{Item, TableReader};
    log::write(b"test-harness: stream transport demo (2-thread pipe, backpressured)\n");

    // A depth-4 pipe, small on purpose, so a 2000-row stream overflows the ring.
    let Ok((e0, e1)) = Handle::<Channel, Only>::pair(4) else {
        log::write(b"test-harness: stream chan create FAIL\n");
        exit(1);
    };

    let Ok(stack) = Stack::new(STREAM_STACK_PAGES * PAGE) else {
        log::write(b"test-harness: stream stack create FAIL\n");
        exit(1);
    };
    let Ok(_producer) = thread::start(stream_producer, stack, e0.raw().0) else {
        log::write(b"test-harness: stream producer thread FAIL\n");
        exit(1);
    };

    let mut rx = ChannelReceiver::new(IpcPortRef::new(&e1));
    let bytes = match rx.receive(1 << 20) {
        Ok(b) => b,
        Err(_) => {
            log::write(b"test-harness: stream receive FAIL\n");
            exit(1);
        }
    };
    let mut tr = match TableReader::new(&bytes) {
        Ok(t) => t,
        Err(_) => {
            log::write(b"test-harness: stream bad TSM1 header\n");
            exit(1);
        }
    };
    let mut n: i64 = 0;
    loop {
        match tr.next() {
            Some(Ok(Item::Row(vals))) => {
                if vals.first().and_then(|v| v.as_int()) != Some(n) {
                    log::write(b"test-harness: stream row MISMATCH\n");
                    exit(1);
                }
                n += 1;
            }
            Some(Ok(Item::End(_))) | None => break,
            _ => {
                log::write(b"test-harness: stream decode FAIL\n");
                exit(1);
            }
        }
    }
    if n != STREAM_ROWS {
        log::write(b"test-harness: stream wrong row count\n");
        exit(1);
    }
    // Release this demo's handles so a long run does not exhaust the handle table.
    drop(e0);
    drop(e1);
    log::write(b"test-harness: stream transport ok (2000 rows over a real pipe, backpressured)\n");
}

/// The **setup-message spawn**: a Tier-1 `test-stage` receives a `stdin` pipe end and `argv =
/// ["stage", "500"]` in a setup message; we produce the stream and fail the run if the stage exits
/// non-zero.
fn stage_spawn_demo(root: &Root, notif: &Notif) {
    use alloc::string::String;
    use libstream::channel::{ChannelSink, IpcPortRef};
    use libstream::setup::{StageStreams, send_stage_setup};
    use libstream::table::TableWriter;
    use libstream::wire::Record;
    use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

    log::write(b"test-harness: setup-message stage demo (spawn a Tier-1 stage)\n");
    const STAGE_ROWS: i64 = 500;

    let Ok(img) = block_on(root.lookup::<File, MapRead>("/bin/test-stage", Rights::MAP_READ)) else {
        log::write(b"test-harness: stage image FAIL\n");
        exit(1);
    };

    let Ok((prod, stage_stdin)) = Handle::<Channel, Only>::pair(4) else {
        log::write(b"test-harness: stage stdin pipe FAIL\n");
        exit(1);
    };
    let Ok((setup_shell, setup_stage)) = Handle::<Channel, Only>::pair(4) else {
        log::write(b"test-harness: stage setup chan FAIL\n");
        exit(1);
    };

    let Some(_stage) = spawn_stage(&img, None, setup_stage) else {
        log::write(b"test-harness: stage spawn FAIL\n");
        exit(1);
    };
    drop(img);

    let streams = StageStreams { stdin: Some(stage_stdin), stdout: None, stderr: None };
    if send_stage_setup(&setup_shell, streams, None, &["stage", "500"], &Record::default(), Deadline::NEVER).is_err() {
        log::write(b"test-harness: stage send_setup FAIL\n");
        exit(1);
    }

    // Produce the stdin stream (the stage drains it concurrently). It waits for the terminator
    // before verifying, so `finish` completes before it exits: no `PeerClosed` race.
    let schema =
        Schema::new().field("i", TypeTag::Int, TypeModifiers::NONE).field("name", TypeTag::String, TypeModifiers::NONE);
    let mut tw = TableWriter::new(ChannelSink::new(IpcPortRef::new(&prod), IPC_PAYLOAD_SIZE));
    let ok = tw.write_schema(StreamFlags::NONE, &schema).is_ok()
        && (0..STAGE_ROWS).all(|i| tw.write_row(&[Value::Int(i), Value::Str(String::from("row"))]).is_ok())
        && tw.finish_with_status(0).is_ok()
        && tw.into_sink().finish().is_ok();
    if !ok {
        log::write(b"test-harness: stage produce FAIL\n");
        exit(1);
    }

    // Reap the stage's exit, the first `ChildExited` (no other child has been spawned yet).
    if reap_child_exit(notif) != 0 {
        log::write(b"test-harness: stage exited non-zero\n");
        exit(1);
    }
    drop(prod);
    drop(setup_shell);
    log::write(b"test-harness: setup-message stage ok (stdin stream + argv verified by the stage)\n");
}

/// The blocking-send / `PendingOperation` path: with a ring full, a `Block` send returns a PO (the
/// message held in-kernel); a recv frees a slot, completing it with status 0.
fn block_send_demo() {
    let Ok((a, b)) = Handle::<Channel, Only>::pair(4) else {
        log::write(b"test-harness: block-demo channel create FAIL\n");
        return;
    };

    let msg = Msg::new();
    let mut filled = 0u64;
    // Until `WouldBlock`: the ring is full.
    while a.send(&msg, &mut Carried::new()).is_ok() {
        filled += 1;
    }

    // One more, blocking: the ring is full, so this returns a PendingOperation, not a block.
    let Ok(mut op) = a.send_held(&msg, &mut Carried::new()) else {
        log::write(b"test-harness: block send FAIL\n");
        return;
    };

    let mut inbox = Msg::new();
    if b.recv(&mut inbox).is_err() {
        log::write(b"test-harness: block-demo recv FAIL\n");
        return;
    }

    let completed = completion(&mut op, Deadline::NEVER);
    if matches!(completed, Some(Ok(_))) {
        Line::new()
            .s(b"test-harness: blocking send completed via PendingOperation (")
            .u(filled)
            .s(b" queued, 1 blocked-then-delivered)")
            .end();
    } else {
        log::write(b"test-harness: block-demo wait unexpected\n");
    }

    while b.recv(&mut inbox).is_ok() {}
}

/// The `BlockBounded` send: with a full ring and a deadline already past, to an endpoint no one
/// receives, the PO completes `TimedOut` on the next tick.
fn block_bounded_demo() {
    let Ok((a, _b)) = Handle::<Channel, Only>::pair(4) else {
        log::write(b"test-harness: block-bounded channel create FAIL\n");
        return;
    };

    let msg = Msg::new();
    // Until `WouldBlock`: full.
    while a.send(&msg, &mut Carried::new()).is_ok() {}

    // A deadline of `1` is already past: held now, timed out on the next tick.
    let Ok(mut op) = a.send_by(&msg, &mut Carried::new(), Deadline::at(1)) else {
        log::write(b"test-harness: block-bounded send FAIL\n");
        return;
    };

    if matches!(completion(&mut op, Deadline::NEVER), Some(Err(e)) if e.code() == KError::TimedOut) {
        log::write(b"test-harness: blocking send timed out via PendingOperation (BlockBounded)\n");
    } else {
        log::write(b"test-harness: block-bounded demo unexpected\n");
    }
}

/// What a wait until `deadline` on `op` read for it: `None` if the wait failed or found it still in
/// flight.
fn completion(op: &mut Op, deadline: Deadline) -> Option<libos::Result<IoResult>> {
    let signalled = {
        let mut set = WaitSet::<1>::new();
        set.add_op(op).ok()?;
        set.wait(deadline).ok()?
    };
    op.completion_in(&signalled)
}

/// Namespace demo: `create → bind → lookup → wait → use` on a fresh namespace, proving the four
/// `sys_ns_*` syscalls and the async-lookup result word.
fn ns_demo() {
    log::write(b"test-harness: ns-demo start (fresh namespace)\n");

    // A fresh namespace to bind into: the inherited root is LOOKUP-only.
    let Ok(ns) = Handle::<Namespace, NsMutable>::create() else {
        log::write(b"test-harness: ns_create FAIL\n");
        return;
    };
    log::write(b"test-harness: ns_create ok\n");

    let Ok(mem) = Handle::<Memory, MapReadWrite>::create(PAGE) else {
        log::write(b"test-harness: ns-demo mem create FAIL\n");
        return;
    };

    // Bind via `Namespace::bind`, gated by the BIND right and the BIND_NAMESPACE syscap.
    if ns.bind("/store", &mem).is_err() {
        log::write(b"test-harness: ns_bind FAIL\n");
        return;
    }
    log::write(b"test-harness: ns_bind /store ok\n");

    let Ok(resolved) = block_on(ns.lookup::<Memory, MapReadWrite>("/store", Rights::MAP_READ | Rights::MAP_WRITE))
    else {
        log::write(b"test-harness: ns_lookup wait unexpected\n");
        return;
    };
    Line::new().s(b"test-harness: ns_lookup -> resolved handle=").u(resolved.raw().0).end();

    let Ok(_mapped) = resolved.map(PAGE) else {
        log::write(b"test-harness: ns-demo map resolved FAIL\n");
        return;
    };
    log::write(b"test-harness: mapped resolved /store handle ok\n");

    // Unbind; a later lookup must complete the PO with `NotFound` (errors arrive through the PO).
    if ns.unbind("/store").is_err() {
        log::write(b"test-harness: ns_unbind FAIL\n");
        return;
    }
    match block_on(ns.lookup::<Memory, MapRead>("/store", Rights::MAP_READ)) {
        Err(e) if e.code() == KError::NotFound => {
            log::write(b"test-harness: ns_unbind ok (lookup-after-unbind -> NotFound)\n");
        }
        _ => log::write(b"test-harness: ns-demo unbind unexpected\n"),
    }

    log::write(b"test-harness: ns-demo done\n");
}

/// Entropy demo: read CSPRNG bytes (the pool seeds at boot); the two 32-byte draws must differ.
fn entropy_demo() {
    log::write(b"test-harness: entropy-demo start\n");

    let Ok(source) = entropy::Source::new() else {
        log::write(b"test-harness: entropy_create FAIL\n");
        return;
    };
    let mut a = [0u8; 32];
    let mut b = [0u8; 32];
    if source.try_fill(&mut a).and_then(|()| source.try_fill(&mut b)).is_err() {
        // Not expected, the pool seeds at boot.
        log::write(b"test-harness: entropy read not synchronous (unseeded?)\n");
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
}

/// Kernel-server demo: resolve `/dev/entropy` from the root namespace and read from the handle the
/// in-kernel server hands back (boot-binding and `KernelServer` dispatch).
fn dev_entropy_lookup_demo(root: &Root) {
    log::write(b"test-harness: /dev/entropy lookup-demo start\n");
    let Ok(resolved) = block_on(root.lookup::<Resource, ReadOnly>("/dev/entropy", Rights::READ)) else {
        log::write(b"test-harness: /dev/entropy lookup wait unexpected\n");
        return;
    };

    let mut a = [0u8; 32];
    if entropy::Source::from_handle(resolved).try_fill(&mut a).is_err() {
        log::write(b"test-harness: /dev/entropy read not synchronous\n");
        return;
    }
    let first = u64::from_le_bytes([a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7]]);
    Line::new().s(b"test-harness: /dev/entropy resolved+read ok bytes[0..8]=").x(first).end();
}

/// Whether `h` is an object of type `want`.
fn stat_is_type<T, M>(h: &Handle<T, M>, want: u32) -> bool {
    h.stat().is_ok_and(|info| info.object_type == want)
}

/// Spawn `image` as a Tier-1 stage: `setup` is its one granted handle and its `arg0` says a setup
/// message is pending. It runs in `ns`, else shares this process's root.
fn spawn_stage(
    image: &Handle<File, MapRead>,
    ns: Option<&Handle<Namespace, NsMutable>>,
    setup: Handle<Channel, Only>,
) -> Option<Handle<Process, Only>> {
    let mut spawn = Spawn::new(image);
    spawn.arg0(bootstrap_arg0(true));
    if let Some(ns) = ns {
        spawn.namespace(ns);
    }
    spawn.grant(setup, ENDPOINT_RIGHTS).ok()?;
    spawn.spawn().ok()
}

/// `/proc/self` demo: the caller's own process, thread and namespace resolve from the root
/// namespace, each strictly *this* caller's object, and each handle is proved.
fn proc_self_demo(root: &Root) {
    log::write(b"test-harness: /proc/self demo start\n");

    let Some(ph) = block_on(root.lookup::<Process, Only>("/proc/self/process", Rights::INSPECT))
        .ok()
        .filter(|h| stat_is_type(h, KOBJ_PROCESS))
    else {
        log::write(b"test-harness: /proc/self/process FAIL\n");
        return;
    };
    drop(ph);
    log::write(b"test-harness: /proc/self/process ok (own Process handle)\n");

    let Some(th) = block_on(root.lookup::<Thread, Only>("/proc/self/thread", Rights::INSPECT))
        .ok()
        .filter(|h| stat_is_type(h, KOBJ_THREAD))
    else {
        log::write(b"test-harness: /proc/self/thread FAIL\n");
        return;
    };
    drop(th);
    log::write(b"test-harness: /proc/self/thread ok (own Thread handle)\n");

    // Use it: resolve /dev/entropy through the returned namespace handle.
    let Some(nh) =
        block_on(root.lookup::<Namespace, NsReadOnly>("/proc/self/namespace", Rights::LOOKUP | Rights::INSPECT))
            .ok()
            .filter(|h| stat_is_type(h, KOBJ_NAMESPACE))
    else {
        log::write(b"test-harness: /proc/self/namespace FAIL\n");
        return;
    };
    let Ok(eh) = block_on(nh.lookup::<Resource, ReadOnly>("/dev/entropy", Rights::READ)) else {
        log::write(b"test-harness: /proc/self/namespace lookup-through FAIL\n");
        return;
    };
    drop(eh);
    drop(nh);
    log::write(b"test-harness: /proc/self/namespace ok (resolved /dev/entropy through it)\n");
}

/// Initramfs demo: resolve `/initramfs/etc/init.toml` (the Limine module, served by the in-kernel
/// CPIO server), map it, and print its first bytes.
fn initramfs_demo(root: &Root) {
    log::write(b"test-harness: /initramfs demo start\n");
    let Ok(mem) = block_on(root.lookup::<Memory, MapRead>("/initramfs/etc/init.toml", Rights::MAP_READ)) else {
        log::write(b"test-harness: /initramfs/etc/init.toml lookup FAIL\n");
        return;
    };
    let Ok(mapping) = mem.map(PAGE) else {
        log::write(b"test-harness: /initramfs map FAIL\n");
        return;
    };
    // The first 16 bytes (init.toml is far longer).
    let mut head = [0u8; 16];
    let _ = mapping.read_into(0, &mut head);
    Line::new().s(b"test-harness: /initramfs/etc/init.toml -> \"").s(&head).s(b"...\"").end();
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
    let Ok(timer) = Handle::<Timer, Only>::create() else { return };
    let fire_at = time::now().unwrap_or(0) + ms * 1_000_000;
    let _ = timer.set(fire_at);
    // A generous outer deadline, so the timer (not the deadline) normally wakes us.
    let mut set = WaitSet::<1>::new();
    if set.add(&timer).is_ok() {
        let _ = set.wait(Deadline::at(fire_at + 1_000_000_000));
    }
}

/// Concurrent-exit stress: waves of immediately-exiting children are spawned and reaped, so
/// teardowns (kernel-stack frees, TLB shootdowns, reap sweeps) race each other and the login chain
/// across all CPUs.
///
/// It covers an exit's reap initiating a shootdown from an IF-masked syscall context (waves make
/// initiators collide), a reap racing a mid-switch-out sibling, and the reap lists' reserve
/// discipline across repeated drains. A lost exit hangs the wave (the wall-clock timeout fails the
/// run); a crash exits nonzero.
fn exit_storm_demo(root: &Root, notif: &Notif) {
    log::write(b"test-harness: exit-storm start\n");
    let Ok(img) = block_on(root.lookup::<File, MapRead>("/bin/test-stage", Rights::MAP_READ)) else {
        log::write(b"test-harness: exit-storm image lookup FAIL\n");
        exit(1);
    };
    const ROUNDS: usize = 6;
    const WAVE: usize = 3;
    for _ in 0..ROUNDS {
        let mut procs = [const { None }; WAVE];
        for slot in procs.iter_mut() {
            // Tier 0: `test-stage` with `arg0 == 0` and no handles exits at once.
            match Spawn::new(&img).spawn() {
                Ok(p) => *slot = Some(p),
                Err(_) => {
                    log::write(b"test-harness: exit-storm spawn FAIL\n");
                    exit(1);
                }
            }
        }
        let mut got = 0;
        while got < WAVE {
            if !wait_readable(notif) {
                log::write(b"test-harness: exit-storm wait FAIL\n");
                exit(1);
            }
            // Until `WouldBlock`: drained.
            while let Ok(Some(n)) = notif.recv() {
                if n.kind == KIND_CHILD_EXITED {
                    got += 1;
                }
            }
        }
        // `procs` drops here, reaping the wave while the next wave spawns: teardown and spawn race
        // by design.
    }
    drop(img);
    log::write(b"test-harness: exit-storm ok (18 exits reaped)\n");
}

/// **Directory listing over the direct-RPC transport**: open `/system` as a session via
/// [`librsproto::session::Dir`] (the client's integration proof), enumerate it, and confirm
/// `current-generation` is listed *with plausible metadata*; a failure exits non-zero.
fn dir_list_demo(root: &Root) {
    log::write(b"test-harness: dir-list demo start\n");
    let mut buf = Msg::new();
    let mut dir = match Dir::resolve(root, "/system", &mut buf) {
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
    log::write(b"test-harness: dir-list ok (/system lists current-generation + metadata)\n");
}

fn return_fail(msg: &[u8]) -> ! {
    log::write(msg);
    exit(1)
}

/// Entries the dir-growth check creates: at 250-byte names that is ~4.1 KB, just past one 4 KiB
/// directory block, so the directory must grow.
const GROW_ENTRIES: usize = 16;

/// **Directory mutation over the direct-RPC transport**: mkdir a temp subdir, rename it, rmdir it,
/// each confirmed by a `ReadDir`, plus the error path: removing a missing name must be a *server*
/// error carrying a `KError`, not a transport failure or a false success.
fn dir_mutate_demo(root: &Root) {
    log::write(b"test-harness: dir-mutate demo start\n");
    let mut buf = Msg::new();
    let mut dir = match Dir::resolve(root, "/system", &mut buf) {
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
    log::write(b"test-harness: dir-mutate ok (mkdir + rename + rmdir, each ReadDir-verified)\n");
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
const LIST_FIXTURE_PATH: &str = "/system/nx-list";

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
fn list_pipeline_demo(root: &Root, notif: &Notif) {
    use libstream::table::{Item, TableReader};
    use libstream::{TypeTag, Value};

    log::write(b"test-harness: list-pipeline demo (the first coreutil over a real pipe)\n");

    let mut fbuf = Msg::new();
    let mut sys = match Dir::resolve(root, "/system", &mut fbuf) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: list fixture open FAIL\n"),
    };
    if sys.mkdir(LIST_FIXTURE_DIR).is_err() {
        return_fail(b"test-harness: list fixture mkdir FAIL\n");
    }
    sys.close();
    {
        let mut dbuf = Msg::new();
        let mut fixture = match Dir::resolve(root, LIST_FIXTURE_PATH, &mut dbuf) {
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

    let bytes = run_list(root, notif, &["list", LIST_FIXTURE_PATH], true);
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

    let _ = run_list(root, notif, &["list", LIST_FIXTURE_PATH], false);

    // 3b. `--recursive` must report children under a parent-relative path (`nx-list/e…`).
    let deep = run_list(root, notif, &["list", "--recursive", "/system"], true);
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
        let mut dbuf = Msg::new();
        let mut fixture = match Dir::resolve(root, LIST_FIXTURE_PATH, &mut dbuf) {
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
    let mut tbuf = Msg::new();
    let mut sys = match Dir::resolve(root, "/system", &mut tbuf) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: list fixture teardown FAIL\n"),
    };
    if sys.rmdir(LIST_FIXTURE_DIR).is_err() {
        return_fail(b"test-harness: list fixture rmdir FAIL\n");
    }
    sys.close();
    log::write(b"test-harness: list-pipeline ok (typed table over a real pipe, backpressure + early close)\n");
}

/// Spawn `list` with `argv` and either drain its stream or close the read end at once
/// (early-consumer close), requiring exit `0` either way; returns the bytes received.
fn run_list(root: &Root, notif: &Notif, argv: &[&str], consume: bool) -> alloc::vec::Vec<u8> {
    run_coreutil_capture(root, notif, "/bin/list", argv, consume)
}

/// Spawn a coreutil as a Tier-1 stage and **return its stdout stream** for the demo to assert on;
/// the pipe is depth 1, so a multi-message stream cannot complete without the consumer draining.
fn run_coreutil_capture(root: &Root, notif: &Notif, image: &str, argv: &[&str], consume: bool) -> alloc::vec::Vec<u8> {
    run_coreutil_capture_in(root, notif, image, argv, consume, None)
}

fn run_coreutil_capture_in(
    root: &Root,
    notif: &Notif,
    image: &str,
    argv: &[&str],
    consume: bool,
    child_ns: Option<&Handle<Namespace, NsMutable>>,
) -> alloc::vec::Vec<u8> {
    use alloc::vec::Vec;
    use libstream::channel::{ChannelReceiver, IpcPortRef};
    use libstream::setup::{StageStreams, send_stage_setup};
    use libstream::wire::Record;

    let Ok(img) = block_on(root.lookup::<File, MapRead>(image, Rights::MAP_READ)) else {
        return_fail(b"test-harness: coreutil image FAIL\n")
    };
    let Ok((rx, list_stdout)) = Handle::<Channel, Only>::pair(1) else {
        return_fail(b"test-harness: list stdout pipe FAIL\n")
    };
    let Ok((setup_shell, setup_stage)) = Handle::<Channel, Only>::pair(4) else {
        return_fail(b"test-harness: list setup chan FAIL\n")
    };
    let Some(_proc) = spawn_stage(&img, child_ns, setup_stage) else { return_fail(b"test-harness: list spawn FAIL\n") };
    drop(img);

    let streams = StageStreams {
        stdin: None, // a source stage: its input is the filesystem, not a pipe
        stdout: Some(list_stdout),
        stderr: None,
    };
    if send_stage_setup(&setup_shell, streams, None, argv, &Record::default(), Deadline::NEVER).is_err() {
        return_fail(b"test-harness: list send_setup FAIL\n");
    }

    let (out, rx) = if consume {
        match ChannelReceiver::new(IpcPortRef::new(&rx)).receive(1 << 20) {
            Ok(b) => (b, Some(rx)),
            Err(_) => return_fail(b"test-harness: list stdout receive FAIL\n"),
        }
    } else {
        // Close the read end unread: the stage's next send surfaces `PeerClosed`, a clean stop.
        drop(rx);
        (Vec::new(), None)
    };

    if reap_child_exit(notif) != 0 {
        return_fail(b"test-harness: list exited non-zero\n");
    }
    drop(rx);
    drop(setup_shell);
    out
}

/// Block until a `ChildExited` arrives and return its exit code; children are reaped one at a time,
/// so the first is this child's.
fn reap_child_exit(notif: &Notif) -> i32 {
    loop {
        if !wait_readable(notif) {
            continue;
        }
        // `WouldBlock`: re-block
        if let Ok(Some(n)) = notif.recv()
            && n.kind == KIND_CHILD_EXITED
        {
            return child_exit_code(&n);
        }
    }
}

/// The exit code a `ChildExited` carries.
fn child_exit_code(n: &Notification) -> i32 {
    i32::from_le_bytes([n.body[8], n.body[9], n.body[10], n.body[11]])
}

/// Whether `h` has something to receive, waiting for ever.
fn wait_readable<'a, T: 'a, M: 'a>(h: impl Into<Ref<'a, T, M>>) -> bool {
    let mut set = WaitSet::<1>::new();
    set.add(h).is_ok() && set.wait(Deadline::NEVER).is_ok_and(|s| !s.is_empty())
}

/// **The wall clock, from ring 3**: readable, a *plausible* date (not a fabricated one or 1970),
/// and advancing.
fn wall_clock_demo() {
    log::write(b"test-harness: wall-clock demo\n");
    let Ok(first) = time::wall() else {
        // Not a soft failure: without a wall clock the filesystem stamps 1970 on everything.
        return_fail(b"test-harness: CLOCK_REALTIME unreadable\n");
    };
    // 2024 .. 2100 in ns: catches a zero, a monotonic value returned as realtime, or a botched
    // epoch.
    const NS_2024: u64 = 1_704_067_200_000_000_000;
    const NS_2100: u64 = 4_102_444_800_000_000_000;
    if !(NS_2024..=NS_2100).contains(&first) {
        return_fail(b"test-harness: CLOCK_REALTIME is not a plausible date\n");
    }

    timer_sleep_ms(20);
    let second = time::wall().ok().filter(|&second| second > first);
    let Some(second) = second else {
        return_fail(b"test-harness: CLOCK_REALTIME did not advance\n");
    };
    // …at roughly wall-clock rate: a 20 ms nap must not show up as an hour (a broken multiplier).
    if second - first > 60_000_000_000 {
        return_fail(b"test-harness: CLOCK_REALTIME advanced implausibly fast\n");
    }
    log::write(b"test-harness: wall-clock ok (plausible date, advancing)\n");
}

/// **A dead stage's pipe closes**, the regression test for exit-time handle reclamation: `list` on
/// an unresolvable path exits non-zero without writing, and the consumer's read must end in
/// `PeerClosed`, not block for ever (its end of the pipe once stayed open).
fn dead_stage_closes_its_pipe_demo(root: &Root, notif: &Notif) {
    use libstream::channel::{ChannelReceiver, IpcPortRef};
    use libstream::setup::{StageStreams, send_stage_setup};
    use libstream::wire::{Record, WireError};

    log::write(b"test-harness: dead-stage pipe-close demo\n");
    let Ok(img) = block_on(root.lookup::<File, MapRead>("/bin/list", Rights::MAP_READ)) else {
        return_fail(b"test-harness: dead-stage image FAIL\n")
    };
    let Ok((rx, stdout)) = Handle::<Channel, Only>::pair(4) else {
        return_fail(b"test-harness: dead-stage pipe FAIL\n")
    };
    let Ok((setup_shell, setup_stage)) = Handle::<Channel, Only>::pair(4) else {
        return_fail(b"test-harness: dead-stage setup chan FAIL\n")
    };
    let Some(_proc) = spawn_stage(&img, None, setup_stage) else {
        return_fail(b"test-harness: dead-stage spawn FAIL\n")
    };
    drop(img);
    let streams = StageStreams { stdin: None, stdout: Some(stdout), stderr: None };
    let argv = ["list", "/nx-no-such-directory"];
    if send_stage_setup(&setup_shell, streams, None, &argv, &Record::default(), Deadline::NEVER).is_err() {
        return_fail(b"test-harness: dead-stage send_setup FAIL\n");
    }

    // The stage writes nothing and exits non-zero: `PeerClosed`, not a hang.
    match ChannelReceiver::new(IpcPortRef::new(&rx)).receive(1 << 20) {
        Err(WireError::PeerClosed) => {}
        Err(_) => return_fail(b"test-harness: dead-stage receive failed for the wrong reason\n"),
        Ok(_) => return_fail(b"test-harness: dead-stage produced a stream it should not have\n"),
    }
    if reap_child_exit(notif) == 0 {
        return_fail(b"test-harness: dead-stage exited 0 on an unresolvable path\n");
    }
    drop(rx);
    drop(setup_shell);
    log::write(b"test-harness: dead-stage ok (its pipe closed; peer saw PeerClosed)\n");
}

/// **`copy`, the mutation side of the filesystem.**
///
/// It must *change* the filesystem correctly and **refuse** the cases that would silently go wrong.
/// On a fixture under `/system`: a file copy matches in size and bytes through a fresh resolve; an
/// existing destination is refused without `--force` (exit `2`) and left **unchanged**; `--force`
/// overwrites; a longer destination shrinks first, leaving no stale tail; a directory copy is
/// recursive.
fn copy_demo(root: &Root, notif: &Notif) {
    log::write(b"test-harness: copy demo (the mutation side)\n");

    let mut buf = Msg::new();
    let mut sys = match Dir::resolve(root, "/system", &mut buf) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: copy fixture open FAIL\n"),
    };
    if sys.mkdir(b"nx-copy").is_err() {
        return_fail(b"test-harness: copy fixture mkdir FAIL\n");
    }
    sys.close();
    {
        let mut b2 = Msg::new();
        let mut d = match Dir::resolve(root, "/system/nx-copy", &mut b2) {
            Ok(d) => d,
            Err(_) => return_fail(b"test-harness: copy fixture reopen FAIL\n"),
        };
        if d.mkdir(b"sub").is_err() {
            return_fail(b"test-harness: copy fixture subdir FAIL\n");
        }
        d.close();
    }
    write_file(root, "/system/nx-copy/a.txt", COPY_CONTENT_A);
    write_file(root, "/system/nx-copy/sub/b.txt", COPY_CONTENT_B);

    if run_copy(root, notif, &["copy", "/system/nx-copy/a.txt", "/system/nx-copy/c.txt"]) != 0 {
        return_fail(b"test-harness: copy file exited non-zero\n");
    }
    if !file_matches(root, "/system/nx-copy/c.txt", COPY_CONTENT_A) {
        return_fail(b"test-harness: copied file content MISMATCH\n");
    }

    let code = run_copy(root, notif, &["copy", "/system/nx-copy/sub/b.txt", "/system/nx-copy/c.txt"]);
    if code == 0 {
        return_fail(b"test-harness: copy over an existing file wrongly succeeded\n");
    }
    if !file_matches(root, "/system/nx-copy/c.txt", COPY_CONTENT_A) {
        return_fail(b"test-harness: refused copy still modified the destination\n");
    }

    if run_copy(root, notif, &["copy", "--force", "/system/nx-copy/sub/b.txt", "/system/nx-copy/c.txt"]) != 0 {
        return_fail(b"test-harness: copy --force exited non-zero\n");
    }
    if !file_matches(root, "/system/nx-copy/c.txt", COPY_CONTENT_B) {
        return_fail(b"test-harness: --force did not overwrite\n");
    }

    // A LONGER destination must end up **exactly** the source: a stale tail fails the size check.
    write_file(root, "/system/nx-copy/long.txt", COPY_CONTENT_LONG);
    let code = run_copy(root, notif, &["copy", "--force", "/system/nx-copy/a.txt", "/system/nx-copy/long.txt"]);
    if code != 0 {
        return_fail(b"test-harness: copy over a longer file failed\n");
    }
    if !file_matches(root, "/system/nx-copy/long.txt", COPY_CONTENT_A) {
        return_fail(b"test-harness: overwrite left a stale tail (truncate did not take)\n");
    }

    if run_copy(root, notif, &["copy", "/system/nx-copy/sub", "/system/nx-copy/sub2"]) != 0 {
        return_fail(b"test-harness: copy directory exited non-zero\n");
    }
    if !file_matches(root, "/system/nx-copy/sub2/b.txt", COPY_CONTENT_B) {
        return_fail(b"test-harness: recursive copy did not reproduce the tree\n");
    }

    unlink_all(root, "/system/nx-copy/sub", &[b"b.txt"]);
    unlink_all(root, "/system/nx-copy/sub2", &[b"b.txt"]);
    unlink_all(root, "/system/nx-copy", &[b"a.txt", b"c.txt", b"long.txt"]);
    {
        let mut b3 = Msg::new();
        let mut d = match Dir::resolve(root, "/system/nx-copy", &mut b3) {
            Ok(d) => d,
            Err(_) => return_fail(b"test-harness: copy teardown open FAIL\n"),
        };
        if d.rmdir(b"sub").is_err() || d.rmdir(b"sub2").is_err() {
            return_fail(b"test-harness: copy teardown rmdir FAIL\n");
        }
        d.close();
    }
    let mut b4 = Msg::new();
    let mut sys = match Dir::resolve(root, "/system", &mut b4) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: copy teardown FAIL\n"),
    };
    if sys.rmdir(b"nx-copy").is_err() {
        return_fail(b"test-harness: copy teardown rmdir FAIL\n");
    }
    sys.close();
    log::write(b"test-harness: copy ok (file, recursive dir, overwrite-with-shrink)\n");
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
fn mkdir_remove_demo(root: &Root, notif: &Notif) {
    log::write(b"test-harness: mkdir/remove demo (Milestone 2 Part A)\n");
    const MK: &str = "/bin/mkdir";
    const RM: &str = "/bin/remove";

    if run_coreutil(root, notif, MK, &["mkdir", "/system/nx-a"]) != 0 {
        return_fail(b"test-harness: mkdir exited non-zero\n");
    }
    if !path_exists(root, "/system/nx-a") {
        return_fail(b"test-harness: mkdir did not create the directory\n");
    }
    if run_coreutil(root, notif, MK, &["mkdir", "/system/nx-a"]) == 0 {
        return_fail(b"test-harness: mkdir over an existing path wrongly succeeded\n");
    }

    if run_coreutil(root, notif, MK, &["mkdir", "--parents", "/system/nx-a/b/c"]) != 0 {
        return_fail(b"test-harness: mkdir --parents exited non-zero\n");
    }
    if !path_exists(root, "/system/nx-a/b/c") {
        return_fail(b"test-harness: mkdir --parents did not build the chain\n");
    }
    if run_coreutil(root, notif, MK, &["mkdir", "--parents", "/system/nx-a/b/c"]) != 0 {
        return_fail(b"test-harness: mkdir --parents was not idempotent\n");
    }

    if run_coreutil(root, notif, RM, &["remove", "/system/nx-a"]) == 0 {
        return_fail(b"test-harness: remove of a directory wrongly succeeded\n");
    }
    if !path_exists(root, "/system/nx-a") {
        return_fail(b"test-harness: refused remove still deleted the directory\n");
    }

    write_file(root, "/system/nx-a/f.txt", b"part a\n");
    if run_coreutil(root, notif, RM, &["remove", "/system/nx-a/f.txt"]) != 0 {
        return_fail(b"test-harness: remove file exited non-zero\n");
    }
    if path_exists(root, "/system/nx-a/f.txt") {
        return_fail(b"test-harness: remove did not delete the file\n");
    }

    if run_coreutil(root, notif, RM, &["remove", "/system/nx-a/gone.txt"]) == 0 {
        return_fail(b"test-harness: remove of a missing path wrongly succeeded\n");
    }
    if run_coreutil(root, notif, RM, &["remove", "--force", "/system/nx-a/gone.txt"]) != 0 {
        return_fail(b"test-harness: remove --force on a missing path failed\n");
    }

    // A namespace binding is refused. `/dev` isolates the check: `/dev/console` fails with or
    // without it, but `/dev` is a binding under a real directory, so without the refusal `--force`
    // turns it into a silent exit 0.
    if run_coreutil(root, notif, RM, &["remove", "--force", "/dev"]) == 0 {
        return_fail(b"test-harness: remove of a namespace binding wrongly succeeded\n");
    }
    if !path_exists(root, "/dev/console") {
        return_fail(b"test-harness: refused remove disturbed the console binding\n");
    }

    write_file(root, "/system/nx-a/b/c/deep.txt", b"deep\n");
    if run_coreutil(root, notif, RM, &["remove", "--recursive", "/system/nx-a"]) != 0 {
        return_fail(b"test-harness: remove --recursive exited non-zero\n");
    }
    if path_exists(root, "/system/nx-a") {
        return_fail(b"test-harness: remove --recursive left the tree behind\n");
    }

    // A relative path with no working directory must fail loud with the *specific* `EXIT_USAGE`
    // (2), not resolve against an arbitrary root; "non-zero" would pass on a segfault.
    if run_coreutil(root, notif, MK, &["mkdir", "relative-name"]) != 2 {
        return_fail(b"test-harness: a relative path without PWD did not fail as a usage error\n");
    }

    log::write(b"test-harness: mkdir/remove ok (created, refused, forced, recursed, binding safe, relative refused)\n");
}

/// **`rename` and `move`.**
///
/// `move`'s correctness lives on a *failure* path, so the cases are about **which** operation ran:
/// a same-mount `move` must report `method = "rename"` (the file arriving would pass for a copy),
/// and a cross-mount `move` must *detect* the case and not destroy the source when the fallback
/// cannot complete.
fn rename_move_demo(root: &Root, notif: &Notif) {
    use libstream::Value;
    use libstream::table::{Item, TableReader};

    log::write(b"test-harness: rename/move demo (Milestone 2 Part B)\n");
    const RN: &str = "/bin/rename";
    const MV: &str = "/bin/move";

    let mut buf = Msg::new();
    let mut sys = match Dir::resolve(root, "/system", &mut buf) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: rename fixture open FAIL\n"),
    };
    let _ = sys.mkdir(b"nx-b");
    let _ = sys.mkdir(b"nx-b2");
    sys.close();
    write_file(root, "/system/nx-b/one.txt", b"part b\n");

    if run_coreutil(root, notif, RN, &["rename", "/system/nx-b/one.txt", "/system/nx-b/two.txt"]) != 0 {
        return_fail(b"test-harness: rename exited non-zero\n");
    }
    if path_exists(root, "/system/nx-b/one.txt") || !path_exists(root, "/system/nx-b/two.txt") {
        return_fail(b"test-harness: rename did not re-point the name\n");
    }

    if run_coreutil(root, notif, RN, &["rename", "/system/nx-b/two.txt", "/system/nx-b2/three.txt"]) != 0 {
        return_fail(b"test-harness: cross-directory rename exited non-zero\n");
    }
    if !path_exists(root, "/system/nx-b2/three.txt") {
        return_fail(b"test-harness: cross-directory rename lost the file\n");
    }

    write_file(root, "/system/nx-b/keep.txt", b"keep\n");
    if run_coreutil(root, notif, RN, &["rename", "/system/nx-b2/three.txt", "/system/nx-b/keep.txt"]) == 0 {
        return_fail(b"test-harness: rename over an existing name wrongly succeeded\n");
    }
    if !path_exists(root, "/system/nx-b2/three.txt") {
        return_fail(b"test-harness: refused rename still moved the source\n");
    }
    if run_coreutil(root, notif, RN, &["rename", "--force", "/system/nx-b2/three.txt", "/system/nx-b/keep.txt"]) != 0 {
        return_fail(b"test-harness: rename --force exited non-zero\n");
    }

    let bytes =
        run_coreutil_capture(root, notif, MV, &["move", "/system/nx-b/keep.txt", "/system/nx-b2/moved.txt"], true);
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
    if path_exists(root, "/system/nx-b/keep.txt") || !path_exists(root, "/system/nx-b2/moved.txt") {
        return_fail(b"test-harness: move did not relocate the file\n");
    }

    // `/initramfs` is a read-only kernel server: the kernel reports a cross-device rename and
    // `move`'s copy fallback must fail, leaving the original.
    if run_coreutil(root, notif, MV, &["move", "/system/nx-b2/moved.txt", "/initramfs/moved.txt"]) == 0 {
        return_fail(b"test-harness: cross-mount move into a read-only server wrongly succeeded\n");
    }
    if !path_exists(root, "/system/nx-b2/moved.txt") {
        return_fail(b"test-harness: failed cross-mount move destroyed the source\n");
    }

    // A cross-mount move that SUCCEEDS, the half that never ran: `/scratch` is the fs-server bound
    // a second time.
    let bytes =
        run_coreutil_capture(root, notif, MV, &["move", "/system/nx-b2/moved.txt", "/scratch/landed.txt"], true);
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
    if path_exists(root, "/system/nx-b2/moved.txt") {
        return_fail(b"test-harness: cross-mount move left the original behind\n");
    }
    if !path_exists(root, "/scratch/landed.txt") {
        return_fail(b"test-harness: cross-mount move did not land the copy\n");
    }
    if !file_matches(root, "/scratch/landed.txt", b"part b\n") {
        return_fail(b"test-harness: cross-mount move corrupted the contents\n");
    }

    // A whole TREE across the mount, a file at each level: a copy that dropped the nested level
    // must fail.
    {
        let mut tb = Msg::new();
        if let Ok(mut d) = Dir::resolve(root, "/system", &mut tb) {
            let _ = d.mkdir(b"nx-tree");
            d.close();
        }
        let mut tb2 = Msg::new();
        if let Ok(mut d) = Dir::resolve(root, "/system/nx-tree", &mut tb2) {
            let _ = d.mkdir(b"inner");
            d.close();
        }
    }
    write_file(root, "/system/nx-tree/top.txt", b"top\n");
    write_file(root, "/system/nx-tree/inner/deep.txt", b"deep\n");

    if run_coreutil(root, notif, MV, &["move", "/system/nx-tree", "/scratch/tree"]) != 0 {
        return_fail(b"test-harness: cross-mount directory move exited non-zero\n");
    }
    if !file_matches(root, "/scratch/tree/top.txt", b"top\n")
        || !file_matches(root, "/scratch/tree/inner/deep.txt", b"deep\n")
    {
        return_fail(b"test-harness: cross-mount directory move did not carry the tree\n");
    }
    if path_exists(root, "/system/nx-tree/inner/deep.txt")
        || path_exists(root, "/system/nx-tree/top.txt")
        || is_dir_present(root, "/system/nx-tree")
    {
        return_fail(b"test-harness: cross-mount directory move left the source behind\n");
    }

    // A namespace binding is refused as a source, or `move` would copy *through* a mount and remove
    // another server's tree. The operand is `/subtreetest`, not `/dev`: `/dev` is not an openable
    // directory, so it fails with or without the refusal, whereas without it `move` of
    // `/subtreetest` (the fs-server based at `/system`) would delete `/system`.
    if run_coreutil(root, notif, MV, &["move", "/subtreetest", "/scratch/st"]) == 0 {
        return_fail(b"test-harness: move of a namespace binding wrongly succeeded\n");
    }
    // The destination was never even started; an exit code cannot tell "refused" from "failed
    // partway".
    if is_dir_present(root, "/scratch/st") {
        return_fail(b"test-harness: move of a binding copied through the mount\n");
    }
    if !path_exists(root, "/subtreetest/current-generation") {
        return_fail(b"test-harness: refused move of a binding damaged it\n");
    }

    unlink_all(root, "/scratch", &[b"landed.txt"]);
    remove_tree_best_effort(root, "/scratch/tree");
    let mut b3 = Msg::new();
    if let Ok(mut d) = Dir::resolve(root, "/system", &mut b3) {
        let _ = d.rmdir(b"nx-b");
        let _ = d.rmdir(b"nx-b2");
        d.close();
    }

    log::write(b"test-harness: rename/move ok (re-pointed, refused, forced, method=rename, cross-mount file + tree, binding safe)\n");
}

/// **`touch`.**
///
/// Case 2 matters: stamping an *existing* file must move its `mtime` forward, as asserting only
/// that `touch` creates files would pass an implementation with no stamping. The wait for the next
/// second is not padding: ext4 `mtime` is whole seconds.
fn touch_demo(root: &Root, notif: &Notif) {
    log::write(b"test-harness: touch demo (Milestone 2 Part C)\n");
    const TOUCH: &str = "/bin/touch";

    let mut buf = Msg::new();
    let mut sys = match Dir::resolve(root, "/system", &mut buf) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: touch fixture open FAIL\n"),
    };
    let _ = sys.mkdir(b"nx-c");
    sys.close();

    if run_coreutil(root, notif, TOUCH, &["touch", "/system/nx-c/new.txt"]) != 0 {
        return_fail(b"test-harness: touch exited non-zero\n");
    }
    if !path_exists(root, "/system/nx-c/new.txt") {
        return_fail(b"test-harness: touch did not create the file\n");
    }

    write_file(root, "/system/nx-c/old.txt", b"part c\n");
    let (before, _) = stat_entry(root, "/system/nx-c", b"old.txt");
    if before == 0 {
        return_fail(b"test-harness: touch fixture has no timestamp (wall clock unset?)\n");
    }
    wait_for_next_second();
    if run_coreutil(root, notif, TOUCH, &["touch", "/system/nx-c/old.txt"]) != 0 {
        return_fail(b"test-harness: touch on an existing file exited non-zero\n");
    }
    let (after, _) = stat_entry(root, "/system/nx-c", b"old.txt");
    if after <= before {
        return_fail(b"test-harness: touch did not advance mtime\n");
    }

    if run_coreutil(root, notif, TOUCH, &["touch", "--no-create", "/system/nx-c/absent.txt"]) != 0 {
        return_fail(b"test-harness: touch --no-create on a missing path failed\n");
    }
    if path_exists(root, "/system/nx-c/absent.txt") {
        return_fail(b"test-harness: touch --no-create created the file anyway\n");
    }

    if run_coreutil(root, notif, TOUCH, &["touch", "/system/nx-c"]) == 0 {
        return_fail(b"test-harness: touch on a directory wrongly succeeded\n");
    }

    unlink_all(root, "/system/nx-c", &[b"new.txt", b"old.txt"]);
    let mut b2 = Msg::new();
    if let Ok(mut d) = Dir::resolve(root, "/system", &mut b2) {
        let _ = d.rmdir(b"nx-c");
        d.close();
    }

    log::write(b"test-harness: touch ok (created, stamped, --no-create skipped, directory refused)\n");
}

/// **`date` and `sleep`.**
///
/// The calendar conversion and duration parser are host-tested; this covers the kernel half.
/// `sleep` genuinely **waits**, the assertion with teeth, as exiting zero proves nothing about a
/// `sleep` that returned at once.
fn date_sleep_demo(root: &Root, notif: &Notif) {
    use libstream::Value;
    use libstream::table::{Item, TableReader};

    log::write(b"test-harness: date/sleep demo (Milestone 2 Part D)\n");
    const DATE: &str = "/bin/date";
    const SLEEP: &str = "/bin/sleep";

    let ours = realtime_secs();
    if ours == 0 {
        return_fail(b"test-harness: wall clock unset, cannot check date\n");
    }
    let bytes = run_coreutil_capture(root, notif, DATE, &["date"], true);
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

    let bytes = run_coreutil_capture(root, notif, DATE, &["date", "--unix"], true);
    if let Ok(tr) = TableReader::new(&bytes) {
        if tr.schema().fields.len() != 1 {
            return_fail(b"test-harness: date --unix did not narrow the schema\n");
        }
    } else {
        return_fail(b"test-harness: date --unix produced no readable table\n");
    }

    // The lower bound is the whole assertion; no upper bound, as TCG scheduling delay is unbounded.
    let before = monotonic_ns();
    if run_coreutil(root, notif, SLEEP, &["sleep", "200ms"]) != 0 {
        return_fail(b"test-harness: sleep exited non-zero\n");
    }
    let elapsed = monotonic_ns().saturating_sub(before);
    if elapsed < 200_000_000 {
        return_fail(b"test-harness: sleep returned before its duration elapsed\n");
    }

    if run_coreutil(root, notif, SLEEP, &["sleep", "5x"]) == 0 {
        return_fail(b"test-harness: sleep accepted a malformed duration\n");
    }

    log::write(b"test-harness: date/sleep ok (fields agree, schema narrows, wait observed)\n");
}

fn monotonic_ns() -> u64 {
    time::now().unwrap_or(0)
}

/// **`whoami`.**
///
/// Identity is a *session* concept published by `session-mgr` at `/session/user`, not a kernel
/// fact. Given a namespace with the name bound `whoami` reports it; given one **without**, it fails
/// rather than inventing a default. Both shapes are built here, the only way to test the absent
/// case.
fn whoami_demo(root: &Root, notif: &Notif) {
    use libstream::Value;
    use libstream::table::{Item, TableReader};

    log::write(b"test-harness: whoami demo (Milestone 2 Part E)\n");
    const WHOAMI: &str = "/bin/whoami";
    const NAME: &[u8] = b"harness-user";

    let Ok(ns) = Handle::<Namespace, NsMutable>::create() else {
        return_fail(b"test-harness: whoami ns_create FAIL\n")
    };
    publish_session_user(&ns, NAME);
    // Nothing else is bound: the spawn hands the child an image *handle*, so `/session/user` alone
    // is a complete environment.

    let bytes = run_coreutil_capture_in(root, notif, WHOAMI, &["whoami"], true, Some(&ns));
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

    let Ok(bare) = Handle::<Namespace, NsMutable>::create() else {
        return_fail(b"test-harness: whoami bare ns_create FAIL\n")
    };
    if run_coreutil_in(root, notif, WHOAMI, &["whoami"], Some(&bare)) == 0 {
        return_fail(b"test-harness: whoami without a session wrongly succeeded\n");
    }

    drop(ns);
    drop(bare);
    log::write(b"test-harness: whoami ok (reported the session user; no session is an error)\n");
}

/// The ABI pass: `AlreadyExists` and `NotEmpty` must arrive **as themselves** across both
/// boundaries that carry a `KError`.
///
/// Host tests pin the discriminants but run no syscall or wire round trip: the kernel returns its
/// error in a syscall's `isize`, the fs-server in an `ErrorBody` the client parses back.
fn error_granularity_demo(root: &Root) {
    log::write(b"test-harness: error-granularity demo (ABI pass)\n");

    // `NsError::AlreadyBound` once shared `InvalidArgument` with a malformed path.
    let (Ok(mem), Ok(ns)) = (Handle::<Memory, MapReadWrite>::create(4096), Handle::<Namespace, NsMutable>::create())
    else {
        return_fail(b"test-harness: error-granularity setup FAIL\n")
    };
    let bind = |p: &str| ns.bind(p, &mem).map_err(|e| e.code());
    if bind("/thing").is_err() {
        return_fail(b"test-harness: error-granularity first bind FAIL\n");
    }
    if bind("/thing") != Err(KError::AlreadyExists) {
        return_fail(b"test-harness: duplicate ns_bind did not report AlreadyExists\n");
    }
    // The contrast: a *malformed* path must still be `InvalidArgument`.
    if bind("no-leading-slash") != Err(KError::InvalidArgument) {
        return_fail(b"test-harness: a malformed bind path did not report InvalidArgument\n");
    }
    drop(mem);
    drop(ns);

    {
        let mut buf = Msg::new();
        let mut dir = match Dir::resolve(root, "/system", &mut buf) {
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
        let mut buf = Msg::new();
        let mut kid = match Dir::resolve(root, "/system/nx-eg", &mut buf) {
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
        let mut buf = Msg::new();
        let mut dir = match Dir::resolve(root, "/system", &mut buf) {
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
        let mut buf = Msg::new();
        let mut kid = match Dir::resolve(root, "/system/nx-eg", &mut buf) {
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
        let mut buf = Msg::new();
        let mut dir = match Dir::resolve(root, "/system", &mut buf) {
            Ok(d) => d,
            Err(_) => return_fail(b"test-harness: error-granularity final open FAIL\n"),
        };
        let gone = dir.rmdir(b"nx-eg");
        dir.close();
        if gone.is_err() {
            return_fail(b"test-harness: emptied directory still would not rmdir\n");
        }
    }
    log::write(b"test-harness: error granularity ok (AlreadyExists and NotEmpty cross both boundaries)\n");
}

/// **`nxsh` runs a real script against real processes**: what only this shows is that it resolves a
/// program image, creates pipes, spawns a process and turns a TSM1 stream back into a value on the
/// real kernel.
fn nxsh_demo(root: &Root, notif: &Notif) {
    log::write(b"test-harness: nxsh demo (Milestone 3 Parts C+D)\n");
    const NXSH: &str = "/bin/nxsh";

    // 1. The language in ring 3: the script compares its own answer, so a wrong result exits
    // non-zero.
    let ok = run_coreutil(root, notif, NXSH, &["nxsh", "-c", "let x = 2 + 3\nlet y = x * 1.5\nif y != 7.5 { bad }"]);
    if ok != 0 {
        return_fail(b"test-harness: nxsh -c did not evaluate correctly\n");
    }

    // 2. A real external stage: spawn `list`, read the stream back, index the table.
    let ran = run_coreutil(root, notif, NXSH, &["nxsh", "-c", "let t = list /system\nif (t[0]).name == \"\" { bad }"]);
    if ran != 0 {
        return_fail(b"test-harness: nxsh could not run a real pipeline stage\n");
    }

    // 2b. **`list /` — the root is a path, not a division sign.**
    let top = run_coreutil(root, notif, NXSH, &["nxsh", "-c", "if (list / | count) < 1 { bad }"]);
    if top != 0 {
        return_fail(b"test-harness: nxsh could not list the root directory\n");
    }

    // 2c. The control: `/` must still divide; the two checks mean something only together.
    if run_coreutil(root, notif, NXSH, &["nxsh", "-c", "if 6 / 2 != 3 { bad }"]) != 0 {
        return_fail(b"test-harness: nxsh stopped treating `/` as division\n");
    }

    // 3. A failing stage fails the script: `remove` of a missing path must reach the exit code.
    let failed = run_coreutil(root, notif, NXSH, &["nxsh", "-c", "remove /system/nx-does-not-exist"]);
    if failed == 0 {
        return_fail(b"test-harness: a failing stage did not fail the script\n");
    }

    // 4. A name that is neither a binding nor a program is an error, not a silent skip.
    if run_coreutil(root, notif, NXSH, &["nxsh", "-c", "definitely-not-a-program"]) == 0 {
        return_fail(b"test-harness: an unknown command wrongly succeeded\n");
    }

    // 5. One external stage at the head, then filter, sort, take, count in-process: one process
    // boundary.
    let piped = run_coreutil(
        root,
        notif,
        NXSH,
        &["nxsh", "-c", "let n = list /system | filter size > 0 | sort size | take 2 | count\nif n != 2 { bad }"],
    );
    if piped != 0 {
        return_fail(b"test-harness: nxsh generic-operator pipeline failed\n");
    }

    // 6. `save` then `open` round-trips through the real filesystem.
    let saved = run_coreutil(
        root,
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
    unlink_all(root, "/system", &[b"nx-d.tsm"]);

    // 7. A `def` with a default and the `_` fill, `match`, and `try`/`catch` over a real pipeline.
    let lang = run_coreutil(
        root,
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
    let repl = run_coreutil(root, notif, NXSH, &["nxsh", "-c", "let n = list /system | count\nif n < 1 { bad }"]);
    if repl != 0 {
        return_fail(b"test-harness: nxsh repl-line evaluation failed\n");
    }

    // 9. `filter … ~= /…/` over a real listing: `grep`'s job was a missing *operator*.
    let re = run_coreutil(
        root,
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
        root,
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
    unlink_all(root, "/system", &[b"nx-c.tsm"]);

    log::write(b"test-harness: nxsh ok (evaluated, spawned, operators, save/open, def+match+try, repl, regex, cd)\n");
}

/// Publish `name` at `/session/user` in `ns`, the way `session-mgr` does for a login.
fn publish_session_user(ns: &Handle<Namespace, NsMutable>, name: &[u8]) {
    let Ok(mem) = Handle::<Memory, MapReadWrite>::create(4096) else {
        return_fail(b"test-harness: session user create FAIL\n")
    };
    let Ok(mut mapping) = mem.map(4096) else { return_fail(b"test-harness: session user map FAIL\n") };
    // The terminator follows the name; the page is zeroed, so it is there already, and written
    // all the same.
    if mapping.write_from(0, name).and_then(|()| mapping.write_from(name.len(), &[0])).is_err() {
        return_fail(b"test-harness: session user map FAIL\n");
    }
    drop(mapping);
    // The bind clones its own reference, so closing ours leaves it bound.
    let bound = ns.bind("/session/user", &mem);
    drop(mem);
    if bound.is_err() {
        return_fail(b"test-harness: session user bind FAIL\n");
    }
}

fn run_copy(root: &Root, notif: &Notif, argv: &[&str]) -> i32 {
    run_coreutil(root, notif, "/bin/copy", argv)
}

/// Spawn a coreutil as a **Tier-1 stage** (setup message, `argv`, a `stdout` pipe) and return its
/// exit status; the image path is a parameter so `copy`, `mkdir` and `remove` share one spawn.
fn run_coreutil(root: &Root, notif: &Notif, image: &str, argv: &[&str]) -> i32 {
    run_coreutil_in(root, notif, image, argv, None)
}

/// As [`run_coreutil`], but the child runs in `child_ns` (`None` = share this process's root):
/// `whoami` reads `/session/user`, which exists only in a session namespace.
fn run_coreutil_in(
    root: &Root,
    notif: &Notif,
    image: &str,
    argv: &[&str],
    child_ns: Option<&Handle<Namespace, NsMutable>>,
) -> i32 {
    use libstream::channel::{ChannelReceiver, IpcPortRef};
    use libstream::setup::{StageStreams, send_stage_setup};
    use libstream::wire::Record;

    let Ok(img) = block_on(root.lookup::<File, MapRead>(image, Rights::MAP_READ)) else {
        return_fail(b"test-harness: coreutil image FAIL\n")
    };
    let Ok((rx, stdout)) = Handle::<Channel, Only>::pair(4) else {
        return_fail(b"test-harness: copy stdout pipe FAIL\n")
    };
    let Ok((setup_shell, setup_stage)) = Handle::<Channel, Only>::pair(4) else {
        return_fail(b"test-harness: copy setup chan FAIL\n")
    };
    let Some(proc) = spawn_stage(&img, child_ns, setup_stage) else {
        return_fail(b"test-harness: coreutil spawn FAIL\n")
    };
    drop(img);
    let streams = StageStreams { stdin: None, stdout: Some(stdout), stderr: None };
    if send_stage_setup(&setup_shell, streams, None, argv, &Record::default(), Deadline::NEVER).is_err() {
        return_fail(b"test-harness: copy send_setup FAIL\n");
    }
    // Drain first, then reap. A **failing** `copy` exits without writing, so the receive must end
    // in `PeerClosed`; that needs the exit-time handle sweep, without which this call hangs.
    let _ = ChannelReceiver::new(IpcPortRef::new(&rx)).receive(1 << 20);
    let code = reap_child_exit(notif);
    drop(proc);
    drop(rx);
    drop(setup_shell);
    code
}

/// **`list /dev` — a kernel-served directory is listable.**
///
/// Nothing is *mounted* at `/dev`: its rows come from kernel namespace bindings. `list /` must
/// still show filesystem entries, or the demo would pass against a `list` that stopped reading
/// filesystems.
fn dev_listing_demo(root: &Root, notif: &Notif) {
    log::write(b"test-harness: dev-listing demo (a kernel-served directory is listable)\n");

    let dev = run_list(root, notif, &["list", "/dev"], true);
    if !contains(&dev, b"entropy") {
        return_fail(b"test-harness: list /dev did not name the entropy binding\n");
    }

    // `/` needs both sources: the root filesystem's entries plus the bindings alongside.
    let top = run_list(root, notif, &["list", "/"], true);
    if !contains(&top, b"system") {
        return_fail(b"test-harness: list / lost its filesystem entries\n");
    }
    if !contains(&top, b"dev") {
        return_fail(b"test-harness: list / did not show the /dev binding\n");
    }

    log::write(b"test-harness: dev-listing ok (/dev from bindings, / from both sources)\n");
}

/// **The profile projects the coreutils into `/bin`.**
///
/// It resolves through the real chain (kernel forwarding to the profile server, which probes each
/// manifest package and re-exports the store `FileObject`), so a stale manifest hash, an unstaged
/// binary or an unbuilt package fails it. The **negative control matters more**: a resolve says
/// nothing about projection unless some `/bin/<name>` does not.
fn bin_projection_demo(root: &Root, notif: &Notif) {
    log::write(b"test-harness: bin-projection demo (the profile puts the coreutils on /bin)\n");

    // The negative control first: if this resolves, every later check is worthless.
    if block_on(root.lookup::<File, MapRead>("/bin/definitely-not-a-program", Rights::MAP_READ)).is_ok() {
        return_fail(b"test-harness: /bin resolved a program that does not exist\n");
    }

    // Every profiled program, not one: a manifest can be wrong about a member.
    for prog in [
        "/bin/list",
        "/bin/copy",
        "/bin/mkdir",
        "/bin/remove",
        "/bin/rename",
        "/bin/move",
        "/bin/touch",
        "/bin/date",
        "/bin/sleep",
        "/bin/whoami",
        "/bin/nxsh",
    ] {
        if !path_exists(root, prog) {
            return_fail(b"test-harness: a profiled program did not resolve on /bin\n");
        }
    }

    // The handle is the program, not merely a handle: spawn it and read its table.
    let out = run_coreutil_capture(root, notif, "/bin/list", &["list", "/"], true);
    if !contains(&out, b"system") {
        return_fail(b"test-harness: /bin/list did not produce a listing\n");
    }

    log::write(b"test-harness: bin-projection ok (11 programs via the store, unknown names still miss)\n");
}

/// **A log source whose writer is gone must be retired, not waited on forever.**
///
/// A channel whose peer closed is *permanently* signaled, so `logging-service` spun a CPU in
/// `sys_wait`. Worse, deferred handle reclamation ran only on the **idle thread**, so a CPU that
/// never idles reclaimed nothing and a dead stage's pipe stayed open (`list /bin` hung). **The
/// assertion is "a CPU goes idle", not "switches stay low"**: a lone spinner is preempted back to
/// itself and barely moves the switch counter.
fn dead_log_source_demo(root: &Root) {
    log::write(b"test-harness: dead-log-source demo (a closed source must be retired)\n");

    // Register a source and close the write end, as a principal that logs and exits does; the sleep
    // lets the service register it first.
    let Ok(h) = block_on(root.lookup::<Channel, Only>("/log/system/nx-probe", Rights::SEND | Rights::TRANSFER)) else {
        return_fail(b"test-harness: could not open a log source\n")
    };
    timer_sleep_ms(200);
    // Our own write end; the service holds the read end.
    drop(h);

    // This thread's own CPU never reads idle, so `online - 1`; take the best sample.
    let online = cpus_online(root);
    if online < 2 {
        log::write(b"test-harness: dead-log-source skipped (needs >= 2 CPUs)\n");
        return;
    }
    // **Sample until the system is quiet, not for a fixed second.** A spin is *permanent*, so
    // exiting early once enough CPUs are idle is safe; a fixed 1 s window was a race against
    // `ui-testclient`'s concurrent window churn.
    const MAX_SAMPLES: u32 = 50; // 5 s at 100 ms, versus 1 s before
    let mut best = 0u64;
    for _ in 0..MAX_SAMPLES {
        timer_sleep_ms(100);
        let idle = idle_cpus(root);
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
fn with_sched_stats(root: &Root, f: &mut dyn FnMut(&[u8])) -> bool {
    let Ok(mem) = block_on(root.lookup::<Memory, MapRead>("/proc/sched/stats", Rights::MAP_READ)) else {
        return false;
    };
    let Some(text) = page_of(&mem) else { return false };
    f(&text);
    true
}

/// The first page of `mem`, copied out.
fn page_of(mem: &Handle<Memory, MapRead>) -> Option<[u8; PAGE]> {
    let mapping = mem.map(PAGE).ok()?;
    let mut page = [0u8; PAGE];
    mapping.read_into(0, &mut page).ok()?;
    Some(page)
}

fn idle_cpus(root: &Root) -> u64 {
    let mut n = 0;
    with_sched_stats(root, &mut |text| {
        for line in text.split(|&b| b == b'\n') {
            if line.starts_with(b"cpu=") && parse_field(line, b"idle=") == Some(1) {
                n += 1;
            }
        }
    });
    n
}

fn cpus_online(root: &Root) -> u64 {
    let mut n = 0;
    with_sched_stats(root, &mut |text| {
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
fn bin_listing_demo(root: &Root, notif: &Notif) {
    log::write(b"test-harness: bin-listing demo (the projected /bin is a real directory)\n");

    let out = run_coreutil_capture(root, notif, "/bin/list", &["list", "/bin"], true);

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

    log::write(b"test-harness: bin-listing ok (both packages merged, nothing invented)\n");
}

/// **The reaper thread is alive and being scheduled.**
///
/// Deferred reclamation once ran only at `exit_*`/`yield_now`/`suspend_with_fault` and in the idle
/// loop, so one busy-looping service starved it and hung peers waiting on a `PeerClosed`. **Its
/// failure mode is silence**: if its wake broke, the per-CPU sweep would cover for it, so
/// `reaper_closed` in `/proc/sched/stats` makes "it is running" an assertion; by now the harness
/// has reaped many children, so it cannot still be zero.
fn reaper_demo(root: &Root) {
    log::write(b"test-harness: reaper demo (deferred reclamation has a thread of its own)\n");
    let closed = stats_field(root, b"reaper_closed=");
    if closed == 0 {
        return_fail(b"test-harness: the reaper has closed nothing -- parked, or never woken\n");
    }

    // **Exit-context teardown is on the path**: a process closes its own handle table at exit, so
    // `PeerClosed` arrives then; if that stopped, the reaper would cover for it.
    let early = stats_field(root, b"exit_closed=");
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
fn stats_field(root: &Root, key: &[u8]) -> u64 {
    let mut out = 0u64;
    with_sched_stats(root, &mut |text| {
        for line in text.split(|&b| b == b'\n') {
            if let Some(v) = parse_field(line, key) {
                out = v;
                break;
            }
        }
    });
    out
}

/// **`/dev/tty` is a capability, and writing to it is a request.**
///
/// Output once went through the ambient `SYS_DEBUG_KPRINT`, so a process with an empty namespace
/// could still write to the console; the tty server owns the device and hands out channels. This
/// exercises resolve, write, set mode and close, with the **negative control that
/// `/dev/tty/anything` must not resolve**.
fn tty_demo(root: &Root, notif: &Notif) {
    let _ = notif;
    log::write(b"test-harness: tty demo (terminal output is a capability)\n");

    let rights = Rights::SEND | Rights::RECV | Rights::WAIT;
    let Ok(tty) = block_on(root.lookup::<Channel, Only>("/dev/tty", rights)) else {
        return_fail(b"test-harness: /dev/tty did not yield a channel\n")
    };

    // A write is a request that is answered, via the server's backend: the *capability* path.
    if !tty_request(&tty, librsproto::OP_TTY_WRITE, 1, b"[tty demo wrote this]\r\n") {
        return_fail(b"test-harness: tty write was not acknowledged\n");
    }

    // Echo is the server's state, not a parameter each caller must remember.
    if !tty_request(&tty, librsproto::OP_TTY_SET_MODE, 2, &[0]) {
        return_fail(b"test-harness: tty set-mode was not acknowledged\n");
    }
    if !tty_request(&tty, librsproto::OP_TTY_SET_MODE, 3, &[librsproto::TTY_MODE_ECHO]) {
        return_fail(b"test-harness: tty set-mode (echo on) was not acknowledged\n");
    }

    // Close is the revocation point: handles cannot be taken back, so the server declining to serve
    // is the guarantee.
    if !tty_request(&tty, librsproto::OP_TTY_CLOSE, 4, &[]) {
        return_fail(b"test-harness: tty close was not acknowledged\n");
    }
    drop(tty);

    if block_on(root.lookup::<Channel, Only>("/dev/tty/nope", rights)).is_ok() {
        return_fail(b"test-harness: /dev/tty resolved a name it does not own\n");
    }

    log::write(b"test-harness: tty ok (write + mode + close acknowledged, unknown name refused)\n");
}

/// Send one tty request and wait for its reply; `true` if acknowledged without an error flag.
fn tty_request(ch: &Handle<Channel, Only>, op: u16, request_id: u64, body: &[u8]) -> bool {
    let mut msg = Msg::new();
    let Some(rs_len) = librsproto::encode(msg.payload_mut(), op, request_id, 0, body, 0) else {
        return false;
    };
    if msg.set_len(rs_len).is_err() || ch.send(&msg, &mut Carried::new()).is_err() {
        return false;
    }
    if !wait_readable(ch) || ch.recv(&mut msg).is_err() {
        return false;
    }
    match librsproto::decode(msg.payload()) {
        Ok(m) => m.op == op && !m.is_error(),
        Err(_) => false,
    }
}

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
fn mtime_overwrite_demo(root: &Root) {
    log::write(b"test-harness: mtime demo (an in-place overwrite is not invisible)\n");

    let mut buf = Msg::new();
    let mut sys = match Dir::resolve(root, "/system", &mut buf) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: mtime fixture open FAIL\n"),
    };
    if sys.mkdir(b"nx-mtime").is_err() {
        return_fail(b"test-harness: mtime fixture mkdir FAIL\n");
    }
    sys.close();
    write_file(root, "/system/nx-mtime/f.txt", MTIME_BEFORE);

    let (before_mtime, before_size) = stat_entry(root, "/system/nx-mtime", b"f.txt");
    if before_mtime == 0 {
        return_fail(b"test-harness: fixture has no mtime (wall clock unset?)\n");
    }

    // Cross a wall-clock second: ext4 stores whole seconds.
    wait_for_next_second();

    overwrite_in_place(root, "/system/nx-mtime/f.txt", MTIME_AFTER);

    if !file_matches(root, "/system/nx-mtime/f.txt", MTIME_AFTER) {
        return_fail(b"test-harness: in-place overwrite did not take\n");
    }

    let (after_mtime, after_size) = stat_entry(root, "/system/nx-mtime", b"f.txt");
    if after_mtime <= before_mtime {
        return_fail(b"test-harness: in-place overwrite left mtime stale\n");
    }
    if after_size != before_size {
        return_fail(b"test-harness: overwrite changed the size, not the in-place path\n");
    }

    unlink_all(root, "/system/nx-mtime", &[b"f.txt"]);
    let mut b2 = Msg::new();
    let mut sys = match Dir::resolve(root, "/system", &mut b2) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: mtime teardown FAIL\n"),
    };
    if sys.rmdir(b"nx-mtime").is_err() {
        return_fail(b"test-harness: mtime teardown rmdir FAIL\n");
    }
    sys.close();
    log::write(b"test-harness: mtime ok (in-place overwrite moved mtime, size unchanged)\n");
}

/// Fixture contents, **the same length on purpose**: a different length would resize the file and
/// stamp `mtime` without the in-place path.
const MTIME_BEFORE: &[u8] = b"the original contents, fixed.\n";
const MTIME_AFTER: &[u8] = b"the rewritten contents, same.\n";

fn stat_entry(ns: &Root, dir: &str, name: &[u8]) -> (i64, u64) {
    let mut buf = Msg::new();
    let mut d = match Dir::resolve(ns, dir, &mut buf) {
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
    time::wall().map_or(0, |ns| (ns / 1_000_000_000) as i64)
}

/// Overwrite `path` with `content` **in place**: map, write, flush; `content` must be the
/// file's current length.
fn overwrite_in_place(ns: &Root, path: &str, content: &[u8]) {
    let size = content.len();
    let Ok(fh) = block_on(ns.lookup::<File, MapReadWrite>(path, Rights::MAP_READ | Rights::MAP_WRITE)) else {
        return_fail(b"test-harness: in-place resolve FAIL\n")
    };
    let Ok(mut mapping) = fh.map(size) else { return_fail(b"test-harness: in-place map FAIL\n") };
    if mapping.write_from(0, content).is_err() {
        return_fail(b"test-harness: in-place map FAIL\n");
    }
    let synced = block_on(fs::flush(&fh));
    drop(mapping);
    drop(fh);
    if synced.is_err() {
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
fn session_fanout_demo(root: &Root) {
    use librsproto::session::DIR_SESSION_RIGHTS;
    log::write(b"test-harness: session fan-out demo (past the old 7-session cap)\n");

    /// The server's ceiling: its wait set is its endpoint, its control channel (which `init` keeps
    /// open for `Meta::Unmount`), and one slot per session.
    const MAX_SESSIONS: usize = MAX_WAIT_HANDLES - 2;
    const OLD_CAP: usize = 7;

    let rights = Rights::from_bits(DIR_SESSION_RIGHTS);
    let mut held = alloc::vec::Vec::with_capacity(MAX_SESSIONS);

    for i in 0..MAX_SESSIONS {
        let Ok(h) = block_on(root.lookup::<Channel, Only>("/system", rights)) else {
            // Report how far we got: the count is the diagnosis.
            Line::new().s(b"test-harness: session fan-out stopped early at session ").x(i as u64).end();
            return_fail(b"test-harness: could not open the full session table\n");
        };
        held.push(h);
    }

    // The one that matters: a handle can exist without the server waiting on it, and then this
    // round trip hangs.
    let mut buf = Msg::new();
    let Some(late) = held.pop() else { return_fail(b"test-harness: could not open the full session table\n") };
    let mut d = Dir::new(late, &mut buf);
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
    if MAX_SESSIONS <= OLD_CAP {
        return_fail(b"test-harness: session cap did not actually move past 7\n");
    }

    match block_on(root.lookup::<Channel, Only>("/system", rights)) {
        Ok(_) => return_fail(b"test-harness: opening past the session table wrongly succeeded\n"),
        Err(e) if e.code() != KError::WouldBlock => {
            return_fail(b"test-harness: a full session table did not report WouldBlock\n")
        }
        Err(_) => {}
    }

    drop(held);
    d.close();
    // Reclamation is eventual, not synchronous: the server frees a slot on its **next wait**, and
    // asserting it on the first attempt lost that race about one boot in twenty. The serve loop's
    // two passes order reclaims first only within one wait batch, not across batches. Retry on
    // `WouldBlock`, sleeping between attempts.
    let mut reopened = None;
    for attempt in 0..RECLAIM_ATTEMPTS {
        let r = block_on(root.lookup::<Channel, Only>("/system", rights));
        let full = matches!(&r, Err(e) if e.code() == KError::WouldBlock);
        reopened = Some(r);
        if !full {
            break;
        }
        if attempt + 1 < RECLAIM_ATTEMPTS {
            timer_sleep_ms(RECLAIM_POLL_MS);
        }
    }
    if !matches!(reopened, Some(Ok(_))) {
        return_fail(b"test-harness: session slots were not reclaimed on close\n");
    }
    drop(reopened);

    log::write(b"test-harness: session fan-out ok (full table open, served, refused, reclaimed)\n");
}

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
fn rename_demo(root: &Root) {
    use librsproto::namespace::RENAME_REPLACE;
    log::write(b"test-harness: rename demo (re-point a name, don't copy it)\n");

    let mut buf = Msg::new();
    let mut sys = match Dir::resolve(root, "/system", &mut buf) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: rename fixture open FAIL\n"),
    };
    if sys.mkdir(b"nx-ren").is_err() {
        return_fail(b"test-harness: rename fixture mkdir FAIL\n");
    }
    sys.close();
    {
        let mut b2 = Msg::new();
        let mut d = match Dir::resolve(root, "/system/nx-ren", &mut b2) {
            Ok(d) => d,
            Err(_) => return_fail(b"test-harness: rename fixture reopen FAIL\n"),
        };
        if d.mkdir(b"sub").is_err() {
            return_fail(b"test-harness: rename fixture subdir FAIL\n");
        }
        d.close();
    }
    write_file(root, "/system/nx-ren/a.txt", RENAME_CONTENT_A);
    write_file(root, "/system/nx-ren/b.txt", RENAME_CONTENT_B);

    if rename_wait(root, "/system/nx-ren/a.txt", "/system/nx-ren/a2.txt", 0).is_err() {
        return_fail(b"test-harness: rename within a directory FAILED\n");
    }
    if !file_matches(root, "/system/nx-ren/a2.txt", RENAME_CONTENT_A) {
        return_fail(b"test-harness: renamed file has the wrong content\n");
    }
    if path_exists(root, "/system/nx-ren/a.txt") {
        return_fail(b"test-harness: rename left the old name behind (it copied)\n");
    }

    if rename_wait(root, "/system/nx-ren/a2.txt", "/system/nx-ren/sub/a3.txt", 0).is_err() {
        return_fail(b"test-harness: cross-directory rename FAILED\n");
    }
    if !file_matches(root, "/system/nx-ren/sub/a3.txt", RENAME_CONTENT_A) {
        return_fail(b"test-harness: cross-directory rename lost the content\n");
    }
    if path_exists(root, "/system/nx-ren/a2.txt") {
        return_fail(b"test-harness: cross-directory rename left the source behind\n");
    }

    let st = rename_wait(root, "/system/nx-ren/sub/a3.txt", "/system/nx-ren/b.txt", 0);
    if st.is_ok() {
        return_fail(b"test-harness: rename over an existing file wrongly succeeded\n");
    }
    if !file_matches(root, "/system/nx-ren/b.txt", RENAME_CONTENT_B) {
        return_fail(b"test-harness: refused rename still clobbered the destination\n");
    }
    if !file_matches(root, "/system/nx-ren/sub/a3.txt", RENAME_CONTENT_A) {
        return_fail(b"test-harness: refused rename still removed the source\n");
    }

    let st = rename_wait(root, "/system/nx-ren/sub/a3.txt", "/system/nx-ren/b.txt", RENAME_REPLACE);
    if st.is_err() {
        return_fail(b"test-harness: RENAME_REPLACE FAILED\n");
    }
    if !file_matches(root, "/system/nx-ren/b.txt", RENAME_CONTENT_A) {
        return_fail(b"test-harness: RENAME_REPLACE did not replace the destination\n");
    }
    if path_exists(root, "/system/nx-ren/sub/a3.txt") {
        return_fail(b"test-harness: RENAME_REPLACE left the source behind\n");
    }

    if rename_wait(root, "/system/nx-ren/nope.txt", "/system/nx-ren/c.txt", 0).is_ok() {
        return_fail(b"test-harness: rename of a missing source wrongly succeeded\n");
    }
    if path_exists(root, "/system/nx-ren/c.txt") {
        return_fail(b"test-harness: failed rename created the destination anyway\n");
    }

    let st = rename_wait(root, "/system/nx-ren/b.txt", "/initramfs/b.txt", 0);
    if st != Err(KError::Unsupported) {
        return_fail(b"test-harness: cross-filesystem rename was not refused with Unsupported\n");
    }
    if !file_matches(root, "/system/nx-ren/b.txt", RENAME_CONTENT_A) {
        return_fail(b"test-harness: refused cross-filesystem rename disturbed the source\n");
    }
    // …and with the *source* off the filesystem, a different kernel path with no forwarding arm.
    let st = rename_wait(root, "/bin/copy", "/system/nx-ren/copy", 0);
    if st != Err(KError::Unsupported) {
        return_fail(b"test-harness: rename off a non-filesystem source was not refused\n");
    }
    if path_exists(root, "/system/nx-ren/copy") {
        return_fail(b"test-harness: refused rename created a destination\n");
    }

    unlink_all(root, "/system/nx-ren", &[b"b.txt"]);
    {
        let mut b3 = Msg::new();
        let mut d = match Dir::resolve(root, "/system/nx-ren", &mut b3) {
            Ok(d) => d,
            Err(_) => return_fail(b"test-harness: rename teardown open FAIL\n"),
        };
        if d.rmdir(b"sub").is_err() {
            return_fail(b"test-harness: rename teardown rmdir FAIL\n");
        }
        d.close();
    }
    let mut b4 = Msg::new();
    let mut sys = match Dir::resolve(root, "/system", &mut b4) {
        Ok(d) => d,
        Err(_) => return_fail(b"test-harness: rename teardown FAIL\n"),
    };
    if sys.rmdir(b"nx-ren").is_err() {
        return_fail(b"test-harness: rename teardown rmdir FAIL\n");
    }
    sys.close();
    log::write(b"test-harness: rename ok (same dir, cross dir, replace, refusals)\n");
}

/// Fixture contents, of different lengths so content cannot follow the name by accident.
const RENAME_CONTENT_A: &[u8] = b"alpha, the file that moves.\n";
const RENAME_CONTENT_B: &[u8] = b"bravo, the destination that is already occupied.\n";

/// Rename `src` to `dst` with `flags`, waiting for the answer.
fn rename_wait(ns: &Root, src: &str, dst: &str, flags: u16) -> Result<(), KError> {
    block_on(fs::rename(ns, src, dst, flags)).map_err(|e| e.code())
}

/// Whether `path` resolves; [`file_matches`] cannot tell *absent* from *present with other
/// content*.
fn path_exists(ns: &Root, path: &str) -> bool {
    block_on(ns.lookup::<File, MapRead>(path, Rights::MAP_READ)).is_ok()
}

/// Create `path` with exactly `content` (the fixture writer).
fn write_file(ns: &Root, path: &str, content: &[u8]) {
    let size = content.len();
    let Ok(fh) = block_on(fs::create::<_, MapReadWrite>(ns, path, size as u64, Rights::MAP_READ | Rights::MAP_WRITE))
    else {
        return_fail(b"test-harness: fixture create FAIL\n")
    };
    let Ok(mut mapping) = fh.map(size) else { return_fail(b"test-harness: fixture map FAIL\n") };
    if mapping.write_from(0, content).is_err() {
        return_fail(b"test-harness: fixture map FAIL\n");
    }
    let _ = block_on(fs::flush(&fh));
}

/// Whether `path` resolves, freshly, to a file of exactly `expect`'s bytes (what reached the
/// filesystem).
fn file_matches(ns: &Root, path: &str, expect: &[u8]) -> bool {
    let Ok(fh) = block_on(ns.lookup::<File, MapRead>(path, Rights::MAP_READ | Rights::INSPECT)) else {
        return false;
    };
    let Ok(info) = fh.stat() else { return false };
    if info.size != expect.len() as u64 {
        return false;
    }
    let Ok(mapping) = fh.map(expect.len()) else { return false };
    let mut got = alloc::vec![0u8; expect.len()];
    mapping.read_into(0, &mut got).is_ok() && got == expect
}

/// Whether `path` names a live directory; one does not resolve to a mappable object, so this opens
/// a session.
fn is_dir_present(ns: &Root, path: &str) -> bool {
    let mut buf = Msg::new();
    Dir::resolve(ns, path, &mut buf).is_ok()
}

/// Tear down a tree without asserting, so a directory a failed case never created cannot fail the
/// run.
fn remove_tree_best_effort(ns: &Root, path: &str) {
    let mut names: alloc::vec::Vec<alloc::vec::Vec<u8>> = alloc::vec::Vec::new();
    let mut kinds: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    {
        let mut buf = Msg::new();
        let Ok(mut d) = Dir::resolve(ns, path, &mut buf) else { return };
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
        let Ok(name) = core::str::from_utf8(n) else { continue };
        if kinds[i] == librsproto::file::DIRENT_KIND_DIR {
            remove_tree_best_effort(ns, &alloc::format!("{path}/{name}"));
        } else {
            let mut buf = Msg::new();
            if let Ok(mut d) = Dir::resolve(ns, path, &mut buf) {
                let _ = d.unlink(n);
                d.close();
            }
        }
    }
    let mut buf = Msg::new();
    if let Ok(mut d) = Dir::resolve(ns, parent_of(path), &mut buf) {
        let _ = d.rmdir(basename_of(path).as_bytes());
        d.close();
    }
}

fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => ".",
    }
}

fn basename_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

fn unlink_all(ns: &Root, dir: &str, names: &[&[u8]]) {
    let mut buf = Msg::new();
    let mut d = match Dir::resolve(ns, dir, &mut buf) {
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

/// **Hardware floating point, end to end in ring 3.**
///
/// Spawns `FP_WORKERS` copies of `test-stage`'s `"fp"` role with different seeds and requires every
/// one to exit `0` (`run_fp_worker` documents the codes). They run concurrently on purpose:
/// different live FP state on different CPUs means a context switch that cross-wired two register
/// files shows as one worker seeing another's values.
fn fp_hardfloat_demo(root: &Root, notif: &Notif) {
    const FP_WORKERS: usize = 3;
    // No "start" banner: session-mgr owns the PASS verdict and races this process, so under KVM the
    // run can be adjudicated mid-demo; the guarantee is session-mgr's `fp_gate`.
    use libstream::setup::{StageStreams, send_stage_setup};
    use libstream::wire::Record;
    // Each worker's seed travels in `argv` of the setup message, never in `arg0`.
    const SEEDS: [&str; FP_WORKERS] = ["1", "2", "3"];
    let Ok(img) = block_on(root.lookup::<File, MapRead>("/bin/test-stage", Rights::MAP_READ)) else {
        log::write(b"test-harness: hard-float image lookup FAIL\n");
        exit(1);
    };
    let mut procs = [const { None }; FP_WORKERS];
    for (i, slot) in procs.iter_mut().enumerate() {
        let Ok((setup_shell, setup_stage)) = Handle::<Channel, Only>::pair(4) else {
            log::write(b"test-harness: hard-float setup chan FAIL\n");
            exit(1);
        };
        match spawn_stage(&img, None, setup_stage) {
            Some(p) => *slot = Some(p),
            None => {
                log::write(b"test-harness: hard-float spawn FAIL\n");
                exit(1);
            }
        }
        let streams = StageStreams::default();
        if send_stage_setup(&setup_shell, streams, None, &["fp", SEEDS[i]], &Record::default(), Deadline::NEVER)
            .is_err()
        {
            log::write(b"test-harness: hard-float send_setup FAIL\n");
            exit(1);
        }
        // Our end of the setup channel; the message is delivered.
        drop(setup_shell);
    }
    let mut got = 0;
    while got < FP_WORKERS {
        if !wait_readable(notif) {
            log::write(b"test-harness: hard-float wait FAIL\n");
            exit(1);
        }
        // Until `WouldBlock`: drained.
        while let Ok(Some(n)) = notif.recv() {
            if n.kind == KIND_CHILD_EXITED {
                let code = child_exit_code(&n);
                if code != 0 {
                    Line::new().s(b"test-harness: hard-float worker FAILED code=").i(code as i64).end();
                    exit(1);
                }
                got += 1;
            }
        }
    }
    drop(img);
    log::write(b"test-harness: hard-float ok (3 workers, f64 + simd verified in ring 3)\n");
}

/// `/proc/self/status` + `/proc/sched/stats` demo: "two CPUs visibly active via `/proc`".
/// **Verdict-gated**: a failure exits nonzero, which init turns into a FAIL, so an SMP-liveness
/// regression fails `xtask test-qemu`.
///
/// `status` must show a spawned identity (pid ≥ 2, tid ≥ 1); `stats` must show **≥ 2 CPUs with
/// `switches` > 0**. It runs last, once the other demos have put work on several CPUs; counters
/// only grow, so it retries (100 ms, ~5 s) before declaring the run dead.
fn sched_stats_demo(root: &Root) {
    log::write(b"test-harness: sched-stats demo start\n");

    let Ok(mem) = block_on(root.lookup::<Memory, MapRead>("/proc/self/status", Rights::MAP_READ)) else {
        log::write(b"test-harness: /proc/self/status lookup FAIL\n");
        exit(1);
    };
    let Some(text) = page_of(&mem) else {
        log::write(b"test-harness: /proc/self/status map FAIL\n");
        exit(1);
    };
    let pid = parse_field(&text, b"pid=").unwrap_or(0);
    let tid = parse_field(&text, b"tid=").unwrap_or(0);
    if pid < 2 || tid < 1 {
        log::write(b"test-harness: /proc/self/status content FAIL\n");
        exit(1);
    }
    Line::new().s(b"test-harness: /proc/self/status ok pid=").u(pid).s(b" tid=").u(tid).end();
    drop(mem);

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        // Each lookup mints a fresh snapshot.
        let Ok(mem) = block_on(root.lookup::<Memory, MapRead>("/proc/sched/stats", Rights::MAP_READ)) else {
            log::write(b"test-harness: /proc/sched/stats lookup FAIL\n");
            exit(1);
        };
        let Some(text) = page_of(&mem) else {
            log::write(b"test-harness: /proc/sched/stats map FAIL\n");
            exit(1);
        };
        let active = cpus_with_switches(&text);
        let done = active >= 2;
        if done {
            // Echo the winning snapshot into the boot log.
            let len = text.iter().position(|&b| b == 0).unwrap_or(text.len());
            Line::new().s(b"test-harness: /proc/sched/stats ok (").u(active).s(b" CPUs with switches>0):").end();
            log::write(&text[..len]);
        }
        drop(mem);
        if done {
            return;
        }
        if attempt >= 50 {
            log::write(b"test-harness: /proc/sched/stats FAIL (<2 CPUs with switches>0)\n");
            exit(1);
        }
        timer_sleep_ms(100);
    }
}

/// Read sector 0 of block device `path`; returns the 16-bit value at offset 510 (`0xAA55`), or `-1`.
fn read_block_sector0(root: &Root, path: &str) -> i32 {
    let Ok(dev) = block_on(root.lookup::<Resource, ReadOnly>(path, Rights::READ)) else { return -1 };
    let Ok(buf) = Handle::<Memory, MapReadWrite>::create(PAGE) else { return -1 };
    let Ok(mapping) = buf.map(PAGE) else { return -1 };
    if block_on(dev.read(&buf, 0, 0, 512)) != Ok(512) {
        return -1;
    }
    let mut sig = [0u8; 2];
    if mapping.read_into(510, &mut sig).is_err() {
        return -1;
    }
    u16::from_le_bytes(sig) as i32
}

/// Block-storage demo: sector 0 of the first whole disk, of the first GPT partition (proving the
/// partition layer rebases the offset), and of the ESP under its stable `by-partlabel` name, each
/// verifying the boot signature.
///
/// **Which `/dev/blk/<n>` each is comes from the registry**, not boot order: a scratch RAM disk,
/// published before any partition, once made the line call the RAM disk a partition.
fn block_demo(root: &Root) {
    use libkern::device::DeviceKind;
    log::write(b"test-harness: /dev/blk demo start\n");
    let blocks: alloc::vec::Vec<(u32, DeviceKind)> =
        block_on(root.lookup::<Memory, MapRead>("/dev/registry", Rights::MAP_READ | Rights::INSPECT))
            .ok()
            .and_then(|snap| {
                let size = snap.stat().ok()?.size as usize;
                let mapping = snap.map(size).ok()?;
                let mut bytes = alloc::vec![0u8; size];
                mapping.read_into(0, &mut bytes).ok()?;
                libkern::device::records(&bytes)
                    .map(|rs| rs.filter_map(|r| r.block_index().map(|n| (n, r.kind()))).collect())
                    .ok()
            })
            .unwrap_or_default();
    if blocks.is_empty() {
        log::write(b"test-harness: /dev/blk demo FAIL (the registry lists no block device)\n");
        return;
    }
    // **What each device is**, before reading any: the installer refuses anything but a `disk`.
    let path = |n: u32, leaf: &str| alloc::format!("/dev/blk/{n}{leaf}");
    for &(n, _) in &blocks {
        report_block_info(root, &path(n, "/info"), path(n, "").replacen("/dev", "test-harness: /dev", 1).as_bytes());
    }
    for (kind, word) in [(DeviceKind::Disk, "disk"), (DeviceKind::Partition, "partition")] {
        match blocks.iter().find(|&&(_, k)| k == kind) {
            Some(&(n, _)) => report_block_read(
                root,
                &path(n, ""),
                alloc::format!("test-harness: /dev/blk/{n} ({word}) read").as_bytes(),
            ),
            None => Line::new().s(b"test-harness: /dev/blk demo: no ").s(word.as_bytes()).s(b" to read FAIL").end(),
        }
    }
    report_block_read(
        root,
        "/dev/disk/by-partlabel/NITROX_ESP",
        b"test-harness: /dev/disk/by-partlabel/NITROX_ESP read",
    );
}

fn report_block_info(root: &Root, path: &str, label: &[u8]) {
    let Ok(mem) = block_on(root.lookup::<Memory, MapRead>(path, Rights::MAP_READ)) else {
        log::write(label);
        log::write(b" info FAIL (no leaf)\n");
        return;
    };
    let len = core::mem::size_of::<libkern::abi::BlockDeviceInfo>();
    let Ok(mapping) = mem.map(len) else {
        log::write(label);
        log::write(b" info FAIL (unmappable)\n");
        return;
    };
    // The leaf is a read-only object holding one record, and `len` is its size.
    let mut bytes = alloc::vec![0u8; len];
    let _ = mapping.read_into(0, &mut bytes);
    match libkern::abi::BlockDeviceInfo::read(&bytes) {
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
            log::write(label);
            log::write(b" info FAIL (short record)\n");
        }
    }
}

fn report_block_read(root: &Root, path: &str, label: &[u8]) {
    let sig = read_block_sector0(root, path);
    log::write(label);
    if sig == 0xAA55 {
        log::write(b" OK (sector 0 boot sig 0x55AA)\n");
    } else if sig < 0 {
        log::write(b" FAIL\n");
    } else {
        log::write(b" OK (no 0x55AA sig)\n");
    }
}

/// Userspace-server forwarding demo: the kernel's **transparent namespace forwarding** end to end
/// in one process, which plays both lookup client and resource server.
///
/// It binds a channel end at `/fs`, looks up `/fs/hello` (the kernel forwards a
/// `Namespace::Resolve` into the other end), parses that request, and replies transferring a
/// read-only `MemoryObject`; the kernel completes the lookup inline in our send.
fn forward_demo() {
    log::write(b"test-harness: userspace-server forwarding demo start\n");
    const CONTENT: &[u8] = b"STUB\n";

    let Ok((kend, send_end)) = Handle::<Channel, Only>::pair(4) else {
        log::write(b"test-harness: fwd channel create FAIL\n");
        return;
    };

    let Ok(ns) = Handle::<Namespace, NsMutable>::create() else {
        log::write(b"test-harness: fwd ns create FAIL\n");
        return;
    };
    // Binding an `IpcChannel` makes the kernel adopt it as a Userspace Server.
    if ns.bind_server("/fs", kend.as_ref(), "").is_err() {
        log::write(b"test-harness: fwd bind FAIL\n");
        return;
    }

    let Ok(mut lookup) = ns.start_lookup::<Memory, MapRead>("/fs/hello", Rights::MAP_READ) else {
        log::write(b"test-harness: fwd lookup submit FAIL\n");
        return;
    };

    let mut req = Msg::new();
    if send_end.recv(&mut req).is_err() {
        log::write(b"test-harness: fwd recv request FAIL\n");
        return;
    }

    let request = match librsproto::decode(req.payload()) {
        Ok(m) => m,
        Err(_) => {
            log::write(b"test-harness: fwd request decode FAIL\n");
            return;
        }
    };
    if request.op != librsproto::OP_NS_RESOLVE {
        log::write(b"test-harness: fwd request op mismatch\n");
        return;
    }
    let request_id = request.request_id;

    let Ok(mem) = Handle::<Memory, MapReadWrite>::create(PAGE) else {
        log::write(b"test-harness: fwd memobj create FAIL\n");
        return;
    };
    let Ok(mut mapping) = mem.map(PAGE) else {
        log::write(b"test-harness: fwd memobj map FAIL\n");
        return;
    };
    let _ = mapping.write_from(0, CONTENT);

    let mut body = [0u8; 16];
    let body_len = match librsproto::namespace::resolve_reply(
        &mut body,
        librsproto::namespace::OBJECT_KIND_MEMOBJ,
        CONTENT.len() as u32,
    ) {
        Some(n) => n,
        None => {
            log::write(b"test-harness: fwd reply body FAIL\n");
            return;
        }
    };
    // The rsproto reply goes in the IPC payload, with the one transferred handle.
    let mut reply = Msg::new();
    let Some(rs_len) = librsproto::encode(
        reply.payload_mut(),
        librsproto::OP_NS_RESOLVE,
        request_id,
        librsproto::RS_FLAG_REPLY,
        &body[..body_len],
        1,
    ) else {
        log::write(b"test-harness: fwd reply encode FAIL\n");
        return;
    };
    let _ = reply.set_len(rs_len);
    let mut carried = Carried::new();
    let _ = carried.push(mem);

    if send_end.send(&reply, &mut carried).is_err() {
        log::write(b"test-harness: fwd reply send FAIL\n");
        return;
    }

    let resolved = {
        let signalled = {
            let mut set = WaitSet::<1>::new();
            set.add_op(lookup.op()).ok().and_then(|_| set.wait(Deadline::NEVER).ok())
        };
        signalled.and_then(|s| lookup.completion_in(&s)).and_then(|r| r.ok())
    };
    let Some(resolved) = resolved else {
        log::write(b"test-harness: fwd lookup result FAIL\n");
        return;
    };

    // The mapped, kernel-installed MemoryObject, read-only.
    let Ok(seen) = resolved.map(PAGE) else {
        log::write(b"test-harness: fwd map resolved FAIL\n");
        return;
    };
    let mut got = [0u8; CONTENT.len()];
    let _ = seen.read_into(0, &mut got);
    if got == *CONTENT {
        log::write(b"test-harness: forwarded lookup returned 'STUB' via fs-server ok\n");
    } else {
        log::write(b"test-harness: fwd content mismatch\n");
    }
}

/// Bootstrap registers: `rdi` is `notif`, `rsi` is `root_ns`; the third is unused.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, boot2: u64, arg0: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(notif, root_ns, boot2, arg0) };
    let (Some(notif), Some(root)) = (start.notify.take(), start.root.take()) else {
        return_fail(b"test-harness: bootstrap handles missing\n")
    };
    let (root, notif) = (&root, &notif);
    log::write(b"test-harness: up (demo supervisor, spawned by init)\n");

    // Order matters. The transport, stage, FP, directory and `list` demos run **early**: the login
    // chain owns the PASS verdict and races this process, so a demo placed last can be adjudicated
    // out from under it, as the FP check was under KVM, silently never running. Sched-stats runs
    // LAST, once the others have put work on several CPUs.
    stream_transport_demo();

    stage_spawn_demo(root, notif);

    fault_demo::run(notif);

    fp_hardfloat_demo(root, notif);

    dir_list_demo(root);

    dir_mutate_demo(root);

    list_pipeline_demo(root, notif);

    // The coreutil demos: each spawns real Tier-1 stages and cleans up its fixture under `/system`.
    copy_demo(root, notif);
    mkdir_remove_demo(root, notif);
    rename_move_demo(root, notif);
    touch_demo(root, notif);
    date_sleep_demo(root, notif);
    whoami_demo(root, notif);
    error_granularity_demo(root);
    nxsh_demo(root, notif);

    rename_demo(root);

    session_fanout_demo(root);

    mtime_overwrite_demo(root);

    dev_listing_demo(root, notif);

    bin_projection_demo(root, notif);

    dead_log_source_demo(root);

    bin_listing_demo(root, notif);

    tty_demo(root, notif);

    dead_stage_closes_its_pipe_demo(root, notif);

    wall_clock_demo();

    // The kernel substrate: blocking sends, namespaces, entropy, `/proc`, the initramfs, block
    // devices and userspace-server forwarding.
    block_send_demo();
    block_bounded_demo();

    ns_demo();

    entropy_demo();

    dev_entropy_lookup_demo(root);

    proc_self_demo(root);

    initramfs_demo(root);

    block_demo(root);

    forward_demo();

    // Teardown racing spawn, then the check that reclamation does not depend on an idle CPU.
    exit_storm_demo(root, notif);

    reaper_demo(root);

    sched_stats_demo(root);

    log::write(b"test-harness: all smoke tests passed; exiting\n");
    exit(0);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}
