//! `test-stage` — a worker `parent` spawns. With a setup message pending (`arg0` marks it) it
//! dispatches on `argv[0]`: `"stage <rows>"` verifies a TSM1 stream on stdin, `"fp <seed>"` runs
//! the hard-float worker ([`run_fp_worker`]), `"test-stage grants"`, as a view broker starts it,
//! reports what a view's grants still reach ([`run_grants`]). With `arg0 == 0` it exits at once:
//! the exit-storm child, whose payload is the process teardown (kernel-stack free, TLB shootdown,
//! reap) racing across CPUs.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use libkern::{RawHandle, Rights};
use libos::boot::Start;
use libos::log;
use libos::thread::{self, Stack};
use libos::{
    Carried, Channel, Deadline, File, Handle, MapRead, MapReadWrite, Memory, Msg, Namespace, NsReadOnly, Only, Ref,
    Spawn, Timer, WaitSet, block_on, exit, time,
};
use libstream::setup::{StageSetup, receive_setup};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

// The hard-float worker: real `mulsd`/`addsd`, not soft-float libcalls.

/// Exit codes, so `parent` can tell *which* invariant broke rather than just "nonzero".
const FP_EXIT_SCALAR_MISMATCH: i32 = 20;
const FP_EXIT_ROUNDTRIP: i32 = 21;
const FP_EXIT_SIMD_MISMATCH: i32 = 22;
/// The kernel enabled AVX in `XCR0` but the `YMM` state bit was not visible from ring 3.
const FP_EXIT_XCR0: i32 = 23;

/// Values per worker: eight `f64`, two `ymm` registers' worth.
const FP_LANES: usize = 8;
const FP_ROUNDS: u32 = 12;

/// `CPUID` leaf/subleaf, returning `(eax, ebx, ecx, edx)`.
fn cpuid(leaf: u32, subleaf: u32) -> (u32, u32, u32, u32) {
    let (a, b, c, d);
    // SAFETY: `cpuid` has no memory effects and is valid in ring 3. `rbx` is
    // callee-saved and LLVM reserves it, so it is routed through `rsi` by hand.
    unsafe {
        asm!(
            "mov rsi, rbx",
            "cpuid",
            "xchg rsi, rbx",
            inlateout("eax") leaf => a,
            lateout("esi") b,
            inlateout("ecx") subleaf => c,
            lateout("edx") d,
            options(nostack, preserves_flags),
        );
    }
    (a, b, c, d)
}

/// Read `XCR0` from ring 3.
///
/// # Safety
/// `XGETBV` `#UD`s unless `CR4.OSXSAVE` is set, which the caller must have confirmed via
/// `CPUID.01H:ECX[27]`.
unsafe fn xgetbv0() -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: caller confirmed `OSXSAVE`; `XCR0` (ECX=0) is the only XCR that exists.
    unsafe {
        asm!("xgetbv", in("ecx") 0u32, out("eax") lo, out("edx") hi,
             options(nomem, nostack, preserves_flags));
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// Whether AVX2 is usable *from this process*.
///
/// Needs the CPU feature, `XSAVE` on, **and** the SSE + AVX state enabled in `XCR0` — userspace
/// confirming from ring 3 the write the kernel made in `fpu_init_cpu`.
fn avx2_usable() -> Result<bool, i32> {
    let (_, _, ecx1, _) = cpuid(1, 0);
    let osxsave = ecx1 & (1 << 27) != 0;
    let (_, ebx7, _, _) = cpuid(7, 0);
    let cpu_has_avx2 = ebx7 & (1 << 5) != 0;
    if !osxsave {
        // No `XSAVE` enabled ⇒ no AVX state ⇒ scalar only. Not an error.
        return Ok(false);
    }
    // SAFETY: `OSXSAVE` confirmed just above, so `XGETBV` is not `#UD`.
    let xcr0 = unsafe { xgetbv0() };
    let ymm_enabled = xcr0 & 0b110 == 0b110; // SSE (bit 1) + AVX (bit 2)
    if cpu_has_avx2 && !ymm_enabled {
        // The CPU has AVX2 and the OS enabled XSAVE, but not the YMM state — using AVX
        // would silently corrupt across a context switch. A kernel bug, so say so
        // loudly rather than quietly falling back.
        return Err(FP_EXIT_XCR0);
    }
    Ok(cpu_has_avx2 && ymm_enabled)
}

/// Σ v[k]² using AVX2 — four `f64` lanes at a time.
///
/// # Safety
/// The caller must have confirmed AVX2 is usable via [`avx2_usable`].
#[target_feature(enable = "avx2")]
unsafe fn sum_squares_avx2(v: &[f64; FP_LANES]) -> f64 {
    use core::arch::x86_64::*;
    // SAFETY: `v` is 8 contiguous `f64`; the two unaligned 4-lane loads stay in bounds,
    // and the caller has confirmed the AVX2 target feature is present.
    unsafe {
        let a = _mm256_loadu_pd(v.as_ptr());
        let b = _mm256_loadu_pd(v.as_ptr().add(4));
        let acc = _mm256_add_pd(_mm256_mul_pd(a, a), _mm256_mul_pd(b, b));
        // Exact integers under 2^53, so this reassociated sum equals the scalar one bit for bit.
        let hi = _mm256_extractf128_pd(acc, 1);
        let lo = _mm256_castpd256_pd128(acc);
        let s = _mm_add_pd(lo, hi);
        let s = _mm_add_sd(s, _mm_unpackhi_pd(s, s));
        _mm_cvtsd_f64(s)
    }
}

fn sum_squares_scalar(v: &[f64; FP_LANES]) -> f64 {
    let mut acc = 0.0f64;
    for x in v.iter() {
        acc += x * x;
    }
    acc
}

/// Sleep about `ms` milliseconds on a one-shot timer: a real deschedule and wake.
fn timer_nap_ms(ms: u64) {
    // No timer: fall through without napping rather than failing the check.
    let Ok(timer) = Handle::<Timer, Only>::create() else { return };
    let fire_at = time::now().unwrap_or(0) + ms * 1_000_000;
    let _ = timer.set(fire_at);
    // The deadline is generous so the timer, not the deadline, normally wakes us.
    let mut set = WaitSet::<1>::new();
    if set.add(&timer).is_ok() {
        let _ = set.wait(Deadline::at(fire_at + 1_000_000_000));
    }
}

/// Role 3 — prove hardware floating point works in ring 3 and that a process's FP state survives
/// syscalls, preemption and other processes doing the same.
///
/// Every value is a small exact integer in an `f64`, so each check is **bit-exact**: the sum of
/// squares must match `u64` math (catching a self-consistent-but-wrong FPU, a stuck rounding
/// mode, an uninitialised `MXCSR`); `x → 2x+1 → (x-1)/2` must round-trip across a timer nap; and
/// the AVX2 sum, when `YMM` state is enabled, must match the scalar one. `seed` differs per
/// worker, so a context switch that cross-wired two register files would show another worker's
/// values.
fn run_fp_worker(seed: u64) -> ! {
    // base ≤ 8·1024, so v[k]² and the sum stay far below 2^53 and every operation is exact.
    let base = (seed & 0x7) * 1024 + 1;
    let mut v = [0f64; FP_LANES];
    let mut expect_sq: u64 = 0;
    #[allow(clippy::needless_range_loop, reason = "the index also feeds arithmetic or a second array")]
    for k in 0..FP_LANES {
        let n = base + k as u64;
        v[k] = n as f64;
        expect_sq += n * n;
    }
    let original = v;

    if sum_squares_scalar(&v) != expect_sq as f64 {
        exit(FP_EXIT_SCALAR_MISMATCH);
    }

    let use_avx2 = match avx2_usable() {
        Ok(b) => b,
        Err(code) => exit(code),
    };
    if use_avx2 {
        // SAFETY: `avx2_usable` confirmed both the CPU feature and that the OS enabled
        // the SSE+AVX state components in `XCR0`.
        if unsafe { sum_squares_avx2(&v) } != expect_sq as f64 {
            exit(FP_EXIT_SIMD_MISMATCH);
        }
    }

    for _ in 0..FP_ROUNDS {
        for x in v.iter_mut() {
            *x = *x * 2.0 + 1.0;
        }
        // A real deschedule + wake guarantees a context switch (likely a CPU migration) under
        // TCG and KVM alike; a spin spanning a tick under TCG ends in microseconds under KVM.
        timer_nap_ms(2);
        for x in v.iter_mut() {
            *x = (*x - 1.0) / 2.0;
        }
        if v != original {
            exit(FP_EXIT_ROUNDTRIP);
        }
        if sum_squares_scalar(&v) != expect_sq as f64 {
            exit(FP_EXIT_SCALAR_MISMATCH);
        }
        if use_avx2 {
            // SAFETY: as above.
            if unsafe { sum_squares_avx2(&v) } != expect_sq as f64 {
                exit(FP_EXIT_SIMD_MISMATCH);
            }
        }
    }

    if use_avx2 {
        log::write(b"child: fp worker ok (f64 + avx2)\n");
    } else {
        log::write(b"child: fp worker ok (f64, no avx2)\n");
    }
    exit(0);
}

/// Parse a base-10 unsigned integer (no `std`); `None` on any non-digit.
fn parse_u64(s: &str) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for b in s.bytes() {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((b - b'0') as u64)?;
    }
    Some(v)
}

/// A **Tier-1 stage**: receive the setup message, then dispatch on `argv[0]`.
///
/// `"stage <rows>"` verifies a TSM1 stream on stdin, `"fp <seed>"` runs a hard-float worker. Role
/// and params come from `argv`, streams from the setup message, never from `arg0`. Exits `0`/`1`.
fn run_stage(endpoint: Option<Handle<Channel, Only>>, root: Option<&Handle<Namespace, NsReadOnly>>) -> ! {
    let setup = endpoint.as_ref().and_then(|e| receive_setup(e, Deadline::NEVER).ok().flatten());
    let Some(setup) = setup else {
        log::write(b"test-stage: setup recv/parse FAIL\n");
        exit(1);
    };
    match setup.argv.first().map(|s| s.as_str()) {
        Some("stage") => run_stage_consumer(&setup),
        Some("fp") => {
            let seed = setup.argv.get(1).and_then(|s| parse_u64(s)).unwrap_or(0);
            run_fp_worker(seed);
        }
        Some("hold") => run_hold(&setup),
        // A view broker names the program first, as a shell does.
        Some("test-stage") if setup.argv.get(1).is_some_and(|a| a == "grants") => run_grants(&setup, root),
        _ => {
            log::write(b"test-stage: unknown argv[0]\n");
            exit(1);
        }
    }
}

/// The `"hold"` role: its setup taken, wait until `stdin`'s other end closes, then exit, so a test
/// can look at what taking the setup left while the stage still runs.
fn run_hold(setup: &StageSetup) -> ! {
    if let Some(stdin) = &setup.streams.stdin {
        let mut set = WaitSet::<1>::new();
        let _ = set.add(stdin);
        let _ = set.wait(Deadline::NEVER);
    }
    exit(0);
}

/// The `"stage <rows>"` role: read + verify a TSM1 stream on `stdin` (`<rows>` rows, row
/// `i` carries `Int(i)`), the stream handle delivered via the setup message.
fn run_stage_consumer(setup: &StageSetup) -> ! {
    use libstream::channel::{ChannelReceiver, IpcPortRef};
    use libstream::table::{Item, TableReader};

    let expect = match setup.argv.get(1).and_then(|s| parse_u64(s)) {
        Some(n) => n as i64,
        None => {
            log::write(b"test-stage: bad argv (rows)\n");
            exit(1);
        }
    };
    let stdin = match &setup.streams.stdin {
        Some(h) => h,
        None => {
            log::write(b"test-stage: no stdin stream\n");
            exit(1);
        }
    };
    let bytes = match ChannelReceiver::new(IpcPortRef::new(stdin)).receive(1 << 20) {
        Ok(b) => b,
        Err(_) => {
            log::write(b"test-stage: stdin receive FAIL\n");
            exit(1);
        }
    };
    let mut tr = match TableReader::new(&bytes) {
        Ok(t) => t,
        Err(_) => {
            log::write(b"test-stage: bad TSM1 header\n");
            exit(1);
        }
    };
    let mut n: i64 = 0;
    loop {
        match tr.next() {
            Some(Ok(Item::Row(vals))) => {
                if vals.first().and_then(|v| v.as_int()) != Some(n) {
                    log::write(b"test-stage: row MISMATCH\n");
                    exit(1);
                }
                n += 1;
            }
            Some(Ok(Item::End(_))) | None => break,
            _ => {
                log::write(b"test-stage: decode FAIL\n");
                exit(1);
            }
        }
    }
    if n != expect {
        log::write(b"test-stage: wrong row count\n");
        exit(1);
    }
    log::write(b"test-stage: setup message + stdin stream verified ok\n");
    exit(0);
}

/// `arg0` of the child [`run_grants`] leaves behind: no setup message, its channel in `rdx`.
const GRANTS_CHILD: u64 = 0x4752_0000;

/// Where a view's grants bind, bit `i` each in what [`grants_resolved`] reports; bit 7 stands for
/// any `/dev/blk/<n>`, which the `disks` grant binds.
const GRANT_PATHS: [&str; 7] = [
    "/dev/storage/admin",
    "/dev/services/admin",
    "/dev/power",
    "/dev/logs",
    "/dev/policy",
    "/dev/accounts",
    "/dev/log",
];

/// Which of a view's grants resolve in `ns`, as bits of [`GRANT_PATHS`].
fn grants_resolved(ns: Option<&Handle<Namespace, NsReadOnly>>) -> u8 {
    let rights = Rights::SEND | Rights::RECV | Rights::WAIT | Rights::MAP_READ | Rights::READ | Rights::INSPECT;
    let found = |path: &str| ns.is_some_and(|ns| block_on(ns.lookup::<File, MapRead>(path, rights)).is_ok());
    let mut mask = 0;
    for (i, path) in GRANT_PATHS.iter().enumerate() {
        if found(path) {
            mask |= 1 << i;
        }
    }
    if (0..16).any(|n| found(&alloc::format!("/dev/blk/{n}"))) {
        mask |= 1 << 7;
    }
    mask
}

/// Send one rsproto message with `op` and `body` on `ch`, moving `handles`.
fn rs_send(ch: Ref<'_, Channel, Only>, op: u16, body: &[u8], handles: &mut Carried) -> bool {
    let mut msg = Msg::new();
    let Some(n) = librsproto::encode(msg.payload_mut(), op, 1, 0, body, handles.len() as u16) else {
        return false;
    };
    msg.set_len(n).is_ok() && ch.send(&msg, handles).is_ok()
}

/// The next rsproto message's op on `ch`, waiting for it; `None` once the peer has gone.
fn rs_recv(ch: Ref<'_, Channel, Only>) -> Option<u16> {
    let mut msg = Msg::new();
    loop {
        match ch.recv(&mut msg) {
            Ok(_) => return librsproto::decode(msg.payload()).ok().map(|m| m.op),
            Err(e) if e.code() == libkern::KError::PeerClosed => return None,
            Err(_) => {}
        }
        let mut set = WaitSet::<1>::new();
        let _ = set.add(ch);
        let _ = set.wait(Deadline::NEVER);
    }
}

/// The `"grants"` role, run in a view by `boot-probe`, **never reading its notifications**, so a
/// request to stop goes unheeded. Each message on stdin is answered on stdout: op 1 with the
/// grants that resolve here ([`grants_resolved`]); op 2 by leaving a child on this namespace,
/// handing over a channel to it, and exiting.
fn run_grants(setup: &StageSetup, ns: Option<&Handle<Namespace, NsReadOnly>>) -> ! {
    let (Some(stdin), Some(stdout)) = (&setup.streams.stdin, &setup.streams.stdout) else {
        exit(1);
    };
    while let Some(op) = rs_recv(stdin.as_ref()) {
        if op == 1 {
            rs_send(stdout.as_ref(), 1, &[grants_resolved(ns)], &mut Carried::new());
            continue;
        }
        let made = Handle::<Channel, Only>::pair(4);
        let image = ns.map(|ns| block_on(ns.lookup::<File, MapRead>("/bin/test-stage", Rights::MAP_READ)));
        let (Ok((mine, its)), Some(Ok(image))) = (made, image) else {
            exit(1);
        };
        let spawned = {
            let mut spawn = Spawn::new(&image);
            spawn.arg0(GRANTS_CHILD);
            spawn.grant(its, Rights::from_bits(u64::MAX)).is_ok() && spawn.spawn().is_ok()
        };
        drop(image);
        let mut handed = Carried::new();
        let _ = handed.push(mine);
        exit(if spawned && rs_send(stdout.as_ref(), 2, &[], &mut handed) { 0 } else { 1 });
    }
    exit(0);
}

/// The child [`run_grants`] leaves: one message on its channel is answered with the grants that
/// resolve in the namespace it shares with the program that started it.
fn run_grants_child(ns: Option<&Handle<Namespace, NsReadOnly>>, ch: &Handle<Channel, Only>) -> ! {
    if rs_recv(ch.as_ref()).is_some() {
        rs_send(ch.as_ref(), 1, &[grants_resolved(ns)], &mut Carried::new());
    }
    exit(0);
}

/// `arg0` of `boot-probe`'s exit children, each with its first handle in `rdx`.
const EXIT_WAITING: u64 = 0x4558_0001;
const EXIT_RUNNING: u64 = 0x4558_0002;
const EXIT_ALONE: u64 = 0x4558_0003;
const EXIT_SPAWNING: u64 = 0x4558_0004;

/// The handle an exit child's second thread uses: a channel to wait on, or a counter's address.
static EXIT_HANDLE: AtomicU64 = AtomicU64::new(0);
/// Set by an exit child's second thread once it runs.
static EXIT_STARTED: AtomicBool = AtomicBool::new(false);

/// Start a thread at `entry` on the CPUs in `affinity` (`0` for any), on a stack never unmapped.
fn start_thread(entry: thread::Entry, affinity: u8) -> bool {
    Stack::new(4 * 4096).and_then(|stack| thread::start_on(entry, stack, 0, affinity)).is_ok()
}

/// The channel [`EXIT_HANDLE`] holds, as the thread that waits on it borrows it.
fn exit_channel() -> Ref<'static, Channel, Only> {
    // SAFETY: the main thread stored a channel it owns before starting any thread, and its frame,
    // which never returns, holds it open until the process exits.
    unsafe { Ref::from_raw(RawHandle(EXIT_HANDLE.load(Ordering::Acquire)), Rights::empty()) }
}

/// The counter at `addr`, in a page a parent shares and only reads.
fn shared_count(addr: &AtomicU64) -> &'static AtomicU64 {
    // SAFETY: `addr` holds the address of a page the main thread mapped and keeps for the life of
    // the process, and an atomic is what a page written by one side and read by another holds.
    unsafe { &*(addr.load(Ordering::Acquire) as *const AtomicU64) }
}

/// `EXIT_WAITING`: a second thread blocks in `sys_wait` on `ch`, which nothing sends to; once it
/// has had time to block, a message says so on `ch` and the process exits.
fn exit_waiting(ch: Option<Handle<Channel, Only>>) -> ! {
    let Some(ch) = ch else { exit(1) };
    EXIT_HANDLE.store(ch.raw().0, Ordering::Release);
    if !start_thread(exit_waiter, 0) {
        exit(1);
    }
    while !EXIT_STARTED.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    timer_nap_ms(20);
    exit(if rs_send(ch.as_ref(), 1, &[], &mut Carried::new()) { 0 } else { 1 });
}

/// Waits on [`EXIT_HANDLE`] for good: a wait that returns waits again, so only an exit ends it.
extern "C" fn exit_waiter(_: u64, _: u64, _: u64, _: u64) -> ! {
    let mut set = WaitSet::<1>::new();
    let _ = set.add(exit_channel());
    EXIT_STARTED.store(true, Ordering::Release);
    loop {
        let _ = set.wait(Deadline::NEVER);
    }
}

/// `EXIT_RUNNING`: one thread counts in the page of `mem` on CPU 2, another exits the process
/// from CPU 3 once the count has begun, and the first thread leaves.
fn exit_running(mem: Option<Handle<Memory, MapReadWrite>>) -> ! {
    let Some(Ok(page)) = mem.as_ref().map(|mem| mem.map(4096)) else { exit(1) };
    EXIT_HANDLE.store(page.into_raw().0 as u64, Ordering::Release);
    if !start_thread(exit_counter, 1 << 2) || !start_thread(exit_exiter, 1 << 3) {
        exit(1);
    }
    thread::exit(0);
}

extern "C" fn exit_counter(_: u64, _: u64, _: u64, _: u64) -> ! {
    let count = shared_count(&EXIT_HANDLE);
    loop {
        count.fetch_add(1, Ordering::Relaxed);
    }
}

extern "C" fn exit_exiter(_: u64, _: u64, _: u64, _: u64) -> ! {
    let count = shared_count(&EXIT_HANDLE);
    while count.load(Ordering::Relaxed) == 0 {
        core::hint::spin_loop();
    }
    exit(0);
}

/// `EXIT_ALONE`: the process's only thread leaves by `sys_thread_exit`, holding the channel.
fn exit_alone() -> ! {
    thread::exit(0);
}

const SPAWNS: u64 = 48;
static SPAWNED: AtomicU64 = AtomicU64::new(0);
/// The address of the page [`spawn_waiter`]s count in.
static SPAWN_COUNT: AtomicU64 = AtomicU64::new(0);
/// Every spawn's stack, made up front so the spawner spends its time in `sys_thread_create`: a
/// `Box<Vec<Stack>>` leaked by `exit_spawning` for the spawner to take.
static SPAWN_STACKS: AtomicPtr<Vec<Stack>> = AtomicPtr::new(core::ptr::null_mut());
const SPAWN_STACK: usize = 4 * 4096;

/// `EXIT_SPAWNING`: a second thread starts [`spawn_waiter`]s as fast as it can, and the process
/// exits once a few have, so the exit meets one mid-creation.
fn exit_spawning(mem: Option<Handle<Memory, MapReadWrite>>) -> ! {
    let page = mem.as_ref().map(|mem| mem.map(4096));
    let stacks = (0..SPAWNS).map(|_| Stack::new(SPAWN_STACK)).collect::<Result<Vec<_>, _>>();
    let channel = Handle::<Channel, Only>::pair(4);
    let (Some(Ok(page)), Ok(stacks), Ok((ours, _theirs))) = (page, stacks, channel) else { exit(1) };
    SPAWN_COUNT.store(page.into_raw().0 as u64, Ordering::Release);
    SPAWN_STACKS.store(Box::into_raw(Box::new(stacks)), Ordering::Release);
    EXIT_HANDLE.store(ours.raw().0, Ordering::Release);
    if !start_thread(exit_spawner, 0) {
        exit(1);
    }
    while SPAWNED.load(Ordering::Acquire) < 4 {
        core::hint::spin_loop();
    }
    exit(0);
}

extern "C" fn exit_spawner(_: u64, _: u64, _: u64, _: u64) -> ! {
    let stacks = SPAWN_STACKS.swap(core::ptr::null_mut(), Ordering::Acquire);
    // SAFETY: `exit_spawning` leaked this box for the spawner alone, and the swap hands it out once.
    let stacks = unsafe { Box::from_raw(stacks) };
    for stack in *stacks {
        if thread::start(spawn_waiter, stack, 0).is_err() {
            break;
        }
        SPAWNED.fetch_add(1, Ordering::Release);
    }
    thread::exit(0);
}

/// Waits on [`EXIT_HANDLE`] for good, counting each wait that fails, as all do once it is closed.
extern "C" fn spawn_waiter(_: u64, _: u64, _: u64, _: u64) -> ! {
    let count = shared_count(&SPAWN_COUNT);
    let mut set = WaitSet::<1>::new();
    let _ = set.add(exit_channel());
    loop {
        if set.wait(Deadline::NEVER).is_err() {
            count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Bootstrap registers: `rdi` notif, `rsi` namespace, `rdx` endpoint (the setup channel for a
/// Tier-1 stage), `rcx` `arg0`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(rdi: u64, rsi: u64, rdx: u64, rcx: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(rdi, rsi, rdx, rcx) };
    if libstream::setup::setup_is_pending(start.arg0) {
        run_stage(start.take_handle0(), start.root.as_ref());
    }
    match start.arg0 {
        GRANTS_CHILD => match start.take_handle0() {
            Some(ch) => run_grants_child(start.root.as_ref(), &ch),
            None => exit(1),
        },
        EXIT_WAITING => exit_waiting(start.take_handle0()),
        EXIT_RUNNING => exit_running(start.take_handle0()),
        EXIT_ALONE => exit_alone(),
        EXIT_SPAWNING => exit_spawning(start.take_handle0()),
        _ => exit(0),
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}
