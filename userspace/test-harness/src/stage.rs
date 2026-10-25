//! `test-stage` — a worker `parent` spawns. With a setup message pending (`arg0` marks it) it
//! dispatches on `argv[0]`: `"stage <rows>"` verifies a TSM1 stream on stdin, `"fp <seed>"` runs
//! the hard-float worker ([`run_fp_worker`]), `"test-stage grants"`, as a view broker starts it,
//! reports what a view's grants still reach ([`run_grants`]). With `arg0 == 0` it exits at once:
//! the exit-storm child, whose payload is the process teardown (kernel-stack free, TLB shootdown,
//! reap) racing across CPUs.

#![no_std]
#![no_main]

extern crate alloc;

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use libkern::{
    CLOCK_MONOTONIC, SYS_CLOCK_READ, SYS_HANDLE_CLOSE, SYS_TIMER_CREATE, SYS_TIMER_SET, SYS_WAIT, exit, kprint,
    syscall1, syscall2, syscall4,
};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

// The hard-float worker: real `mulsd`/`addsd`, not soft-float libcalls.

/// Exit codes, so `parent` can tell *which* invariant broke rather than just "nonzero".
const FP_EXIT_SCALAR_MISMATCH: i64 = 20;
const FP_EXIT_ROUNDTRIP: i64 = 21;
const FP_EXIT_SIMD_MISMATCH: i64 = 22;
/// The kernel enabled AVX in `XCR0` but the `YMM` state bit was not visible from ring 3.
const FP_EXIT_XCR0: i64 = 23;

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
fn avx2_usable() -> Result<bool, i64> {
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
    // SAFETY: a valid syscall; returns a handle (>= 0) or a negative KError.
    let th = unsafe { syscall1(SYS_TIMER_CREATE, 0) };
    if th < 0 {
        return; // no timer: fall through without napping rather than failing the check
    }
    let th = th as u64;
    // SAFETY: FP_CLOCK is a writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut FP_CLOCK) as u64) };
    // SAFETY: `FP_CLOCK` is a plain `u64` static only this thread touches; a failed clock read
    // leaves its last value.
    let now = unsafe { (&raw const FP_CLOCK).read() };
    let fire_at = now + ms * 1_000_000;
    // SAFETY: arming our own timer handle (absolute monotonic, one-shot).
    unsafe { syscall4(SYS_TIMER_SET, th, fire_at, 0, 0) };
    // SAFETY: FP_WAIT_* are valid writable buffers; the outer deadline is generous so
    // the timer, not the deadline, normally wakes us.
    unsafe {
        FP_WAIT_HANDLES[0] = th;
        syscall4(
            SYS_WAIT,
            (&raw const FP_WAIT_HANDLES) as u64,
            1,
            (&raw mut FP_WAIT_RESULTS) as u64,
            fire_at + 1_000_000_000,
        );
    }
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, th) };
}

static mut FP_CLOCK: u64 = 0;
static mut FP_WAIT_HANDLES: [u64; 1] = [0];
static mut FP_WAIT_RESULTS: [u8; 24] = [0; 24];

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
        kprint(b"child: fp worker ok (f64 + avx2)\n");
    } else {
        kprint(b"child: fp worker ok (f64, no avx2)\n");
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
fn run_stage(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let boot = libstream::setup::bootstrap(notif, ns, endpoint, arg0);
    let setup = match boot.setup() {
        Some(Ok(s)) => s,
        _ => {
            kprint(b"test-stage: setup recv/parse FAIL\n");
            exit(1);
        }
    };
    match setup.argv.first().map(|s| s.as_str()) {
        Some("stage") => run_stage_consumer(&setup),
        Some("fp") => {
            let seed = setup.argv.get(1).and_then(|s| parse_u64(s)).unwrap_or(0);
            run_fp_worker(seed);
        }
        // A view broker names the program first, as a shell does.
        Some("test-stage") if setup.argv.get(1).is_some_and(|a| a == "grants") => run_grants(&setup, ns),
        _ => {
            kprint(b"test-stage: unknown argv[0]\n");
            exit(1);
        }
    }
}

/// The `"stage <rows>"` role: read + verify a TSM1 stream on `stdin` (`<rows>` rows, row
/// `i` carries `Int(i)`), the stream handle delivered via the setup message.
fn run_stage_consumer(setup: &libstream::setup::Setup) -> ! {
    use libstream::channel::{ChannelReceiver, IpcPort};
    use libstream::table::{Item, TableReader};

    let expect = match setup.argv.get(1).and_then(|s| parse_u64(s)) {
        Some(n) => n as i64,
        None => {
            kprint(b"test-stage: bad argv (rows)\n");
            exit(1);
        }
    };
    let stdin = match setup.streams.stdin {
        Some(h) => h,
        None => {
            kprint(b"test-stage: no stdin stream\n");
            exit(1);
        }
    };
    let bytes = match ChannelReceiver::new(IpcPort::new(stdin)).receive() {
        Ok(b) => b,
        Err(_) => {
            kprint(b"test-stage: stdin receive FAIL\n");
            exit(1);
        }
    };
    let mut tr = match TableReader::new(&bytes) {
        Ok(t) => t,
        Err(_) => {
            kprint(b"test-stage: bad TSM1 header\n");
            exit(1);
        }
    };
    let mut n: i64 = 0;
    loop {
        match tr.next() {
            Some(Ok(Item::Row(vals))) => {
                if vals.first().and_then(|v| v.as_int()) != Some(n) {
                    kprint(b"test-stage: row MISMATCH\n");
                    exit(1);
                }
                n += 1;
            }
            Some(Ok(Item::End(_))) | None => break,
            _ => {
                kprint(b"test-stage: decode FAIL\n");
                exit(1);
            }
        }
    }
    if n != expect {
        kprint(b"test-stage: wrong row count\n");
        exit(1);
    }
    kprint(b"test-stage: setup message + stdin stream verified ok\n");
    exit(0);
}

/// `arg0` of the child [`run_grants`] leaves behind: no setup message, its channel in `rdx`.
const GRANTS_CHILD: u64 = 0x4752_0000;

/// Where a view's grants bind, bit `i` each in what [`grants_resolved`] reports; bit 7 stands for
/// any `/dev/blk/<n>`, which the `disks` grant binds.
const GRANT_PATHS: [&[u8]; 7] = [
    b"/dev/storage/admin",
    b"/dev/services/admin",
    b"/dev/power",
    b"/dev/logs",
    b"/dev/policy",
    b"/dev/accounts",
    b"/dev/log",
];

/// Which of a view's grants resolve in `ns`, as bits of [`GRANT_PATHS`].
fn grants_resolved(ns: u64) -> u8 {
    use libkern::{RIGHT_INSPECT, RIGHT_MAP_READ, RIGHT_READ, RIGHT_RECV, RIGHT_SEND, RIGHT_WAIT};
    let rights = RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT | RIGHT_MAP_READ | RIGHT_READ | RIGHT_INSPECT;
    let found = |path: &[u8]| match libfs::lookup_wait(ns, path, rights) {
        (0, h) => {
            // SAFETY: closing a handle the lookup gave this process.
            unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
            true
        }
        _ => false,
    };
    let mut mask = 0;
    for (i, path) in GRANT_PATHS.iter().enumerate() {
        if found(path) {
            mask |= 1 << i;
        }
    }
    if (0..16).any(|n| found(alloc::format!("/dev/blk/{n}").as_bytes())) {
        mask |= 1 << 7;
    }
    mask
}

/// Send one rsproto message with `op` and `body` on `ch`, moving `handles`.
fn rs_send(ch: u64, op: u16, body: &[u8], handles: &[u64]) -> bool {
    let mut msg = [0u8; libkern::abi::IPC_MSG_SIZE]; // the kernel copies a whole message
    let Some(n) = librsproto::encode(&mut msg[24..], op, 1, 0, body, handles.len() as u16) else {
        return false;
    };
    msg[4..8].copy_from_slice(&(n as u32).to_le_bytes());
    msg[8] = handles.len() as u8;
    // SAFETY: a valid message buffer and handle array.
    let sr = unsafe {
        libkern::syscall5(
            libkern::SYS_CHANNEL_SEND,
            ch,
            msg.as_ptr() as u64,
            handles.as_ptr() as u64,
            handles.len() as u64,
            libkern::SENDMODE_NOBLOCK,
        )
    };
    sr == 0
}

/// The next rsproto message's op on `ch`, waiting for it; `None` once the peer has gone.
fn rs_recv(ch: u64) -> Option<u16> {
    let mut buf = [0u8; 4096];
    let mut hs = [0u64; 8];
    loop {
        let mut count = 0usize;
        // SAFETY: valid receive out-params.
        let rr = unsafe {
            syscall4(
                libkern::SYS_CHANNEL_RECV,
                ch,
                buf.as_mut_ptr() as u64,
                hs.as_mut_ptr() as u64,
                (&raw mut count) as u64,
            )
        };
        if rr == 0 {
            let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
            return librsproto::decode(&buf[24..24 + len.min(4096 - 24)]).ok().map(|m| m.op);
        }
        if rr == libkern::KError::PeerClosed.as_i32() as i64 {
            return None;
        }
        let waited = [ch];
        let mut results = [0u64; 8];
        // SAFETY: one handle to wait on and room for its result; no deadline.
        unsafe { syscall4(SYS_WAIT, waited.as_ptr() as u64, 1, results.as_mut_ptr() as u64, u64::MAX) };
    }
}

/// The `"grants"` role, run in a view by `boot-probe`, **never reading its notifications**, so a
/// request to stop goes unheeded. Each message on stdin is answered on stdout: op 1 with the
/// grants that resolve here ([`grants_resolved`]); op 2 by leaving a child on this namespace,
/// handing over a channel to it, and exiting.
fn run_grants(setup: &libstream::setup::Setup, ns: u64) -> ! {
    let (Some(stdin), Some(stdout)) = (setup.streams.stdin, setup.streams.stdout) else {
        exit(1);
    };
    while let Some(op) = rs_recv(stdin) {
        if op == 1 {
            rs_send(stdout, 1, &[grants_resolved(ns)], &[]);
            continue;
        }
        let (mut mine, mut its) = (0u64, 0u64);
        // SAFETY: valid writable out-params.
        let made =
            unsafe { syscall4(libkern::SYS_CHANNEL_CREATE, (&raw mut mine) as u64, (&raw mut its) as u64, 4, 0) } == 0;
        let (st, image) = libfs::lookup_wait(ns, b"/bin/test-stage", libkern::RIGHT_MAP_READ);
        if !made || st != 0 {
            exit(1);
        }
        let mut args = libkern::SpawnArgs {
            image,
            handle_count: 1,
            move_mask: 1,
            arg0: GRANTS_CHILD,
            handles: [0; libkern::SPAWN_MAX_HANDLES],
            rights: [0; libkern::SPAWN_MAX_HANDLES],
            namespace: 0,
            syscaps: 0,
        };
        args.handles[0] = its;
        args.rights[0] = u64::MAX;
        let spawned = libos::spawn(&args).is_ok();
        exit(if spawned && rs_send(stdout, 2, &[], &[mine]) { 0 } else { 1 });
    }
    exit(0);
}

/// The child [`run_grants`] leaves: one message on its channel is answered with the grants that
/// resolve in the namespace it shares with the program that started it.
fn run_grants_child(ns: u64, ch: u64) -> ! {
    if rs_recv(ch).is_some() {
        rs_send(ch, 1, &[grants_resolved(ns)], &[]);
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
fn start_thread(entry: extern "C" fn() -> !, affinity: u8) -> bool {
    const STACK: u64 = 4 * 4096;
    // SAFETY: register-only syscalls; the second maps the object the first made.
    let base = unsafe {
        let mem = syscall4(libkern::SYS_MEMORY_CREATE, STACK, 0, 0, 0);
        if mem < 0 {
            return false;
        }
        syscall4(libkern::SYS_MEMORY_MAP, mem as u64, 0, STACK, libkern::RIGHT_MAP_READ | libkern::RIGHT_MAP_WRITE)
    };
    let args = libkern::ThreadArgs {
        entry: entry as *const () as u64,
        user_sp: base as u64 + STACK,
        arg0: 0,
        class: 0,
        rt_priority: 0,
        nice: 0,
        cpu_affinity: affinity,
        _reserved: [0; 36],
    };
    base >= 0 && libos::thread_create(&args).is_ok()
}

fn thread_exit() -> ! {
    // SAFETY: `sys_thread_exit` ends this thread.
    unsafe { syscall1(libkern::SYS_THREAD_EXIT, 0) };
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}

/// `EXIT_WAITING`: a second thread blocks in `sys_wait` on `ch`, which nothing sends to; once it
/// has had time to block, a message says so on `ch` and the process exits.
fn exit_waiting(ch: u64) -> ! {
    EXIT_HANDLE.store(ch, Ordering::Release);
    if !start_thread(exit_waiter, 0) {
        exit(1);
    }
    while !EXIT_STARTED.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    timer_nap_ms(20);
    exit(if rs_send(ch, 1, &[], &[]) { 0 } else { 1 });
}

/// Waits on [`EXIT_HANDLE`] for good: a wait that returns waits again, so only an exit ends it.
extern "C" fn exit_waiter() -> ! {
    let ch = [EXIT_HANDLE.load(Ordering::Acquire)];
    let mut results = [0u8; 24];
    EXIT_STARTED.store(true, Ordering::Release);
    loop {
        // SAFETY: a one-handle list and a result buffer for it.
        unsafe { syscall4(SYS_WAIT, ch.as_ptr() as u64, 1, results.as_mut_ptr() as u64, u64::MAX) };
    }
}

/// `EXIT_RUNNING`: one thread counts in the page of `mem` on CPU 2, another exits the process
/// from CPU 3 once the count has begun, and the first thread leaves.
fn exit_running(mem: u64) -> ! {
    // SAFETY: register-only syscall mapping the page the parent handed over.
    let page =
        unsafe { syscall4(libkern::SYS_MEMORY_MAP, mem, 0, 4096, libkern::RIGHT_MAP_READ | libkern::RIGHT_MAP_WRITE) };
    if page < 0 {
        exit(1);
    }
    EXIT_HANDLE.store(page as u64, Ordering::Release);
    if !start_thread(exit_counter, 1 << 2) || !start_thread(exit_exiter, 1 << 3) {
        exit(1);
    }
    thread_exit();
}

extern "C" fn exit_counter() -> ! {
    // SAFETY: the page `exit_running` mapped, never unmapped; the parent only reads it.
    let count = unsafe { &*(EXIT_HANDLE.load(Ordering::Acquire) as *const AtomicU64) };
    loop {
        count.fetch_add(1, Ordering::Relaxed);
    }
}

extern "C" fn exit_exiter() -> ! {
    // SAFETY: as in `exit_counter`.
    let count = unsafe { &*(EXIT_HANDLE.load(Ordering::Acquire) as *const AtomicU64) };
    while count.load(Ordering::Relaxed) == 0 {
        core::hint::spin_loop();
    }
    exit(0);
}

/// `EXIT_ALONE`: the process's only thread leaves by `sys_thread_exit`, holding the channel.
fn exit_alone() -> ! {
    thread_exit();
}

const SPAWNS: u64 = 48;
static SPAWNED: AtomicU64 = AtomicU64::new(0);
/// The address of the page [`spawn_waiter`]s count in.
static SPAWN_COUNT: AtomicU64 = AtomicU64::new(0);
/// Every spawn's stack, mapped up front so the spawner spends its time in `sys_thread_create`.
static SPAWN_STACKS: AtomicU64 = AtomicU64::new(0);
const SPAWN_STACK: u64 = 4 * 4096;

/// `EXIT_SPAWNING`: a second thread starts [`spawn_waiter`]s as fast as it can, and the process
/// exits once a few have, so the exit meets one mid-creation.
fn exit_spawning(mem: u64) -> ! {
    let rw = libkern::RIGHT_MAP_READ | libkern::RIGHT_MAP_WRITE;
    let stacks = SPAWNS * SPAWN_STACK;
    let (mut ours, mut theirs) = (0u64, 0u64);
    // SAFETY: register-only syscalls and valid out-params; each map maps the object it names.
    let (page, base, made) = unsafe {
        let page = syscall4(libkern::SYS_MEMORY_MAP, mem, 0, 4096, rw);
        let obj = syscall4(libkern::SYS_MEMORY_CREATE, stacks, 0, 0, 0);
        let base = syscall4(libkern::SYS_MEMORY_MAP, obj.max(0) as u64, 0, stacks, rw);
        let made = syscall4(libkern::SYS_CHANNEL_CREATE, (&raw mut ours) as u64, (&raw mut theirs) as u64, 4, 0);
        (page, base, made == 0)
    };
    SPAWN_COUNT.store(page.max(0) as u64, Ordering::Release);
    SPAWN_STACKS.store(base.max(0) as u64, Ordering::Release);
    EXIT_HANDLE.store(ours, Ordering::Release);
    if page < 0 || base < 0 || !made || !start_thread(exit_spawner, 0) {
        exit(1);
    }
    while SPAWNED.load(Ordering::Acquire) < 4 {
        core::hint::spin_loop();
    }
    exit(0);
}

extern "C" fn exit_spawner() -> ! {
    let base = SPAWN_STACKS.load(Ordering::Acquire);
    for i in 0..SPAWNS {
        let args = libkern::ThreadArgs {
            entry: spawn_waiter as *const () as u64,
            user_sp: base + (i + 1) * SPAWN_STACK,
            arg0: 0,
            class: 0,
            rt_priority: 0,
            nice: 0,
            cpu_affinity: 0,
            _reserved: [0; 36],
        };
        if libos::thread_create(&args).is_err() {
            break;
        }
        SPAWNED.fetch_add(1, Ordering::Release);
    }
    thread_exit();
}

/// Waits on [`EXIT_HANDLE`] for good, counting each wait that fails, as all do once it is closed.
extern "C" fn spawn_waiter() -> ! {
    let ch = [EXIT_HANDLE.load(Ordering::Acquire)];
    // SAFETY: the page `exit_spawning` mapped, never unmapped; the parent only reads it.
    let count = unsafe { &*(SPAWN_COUNT.load(Ordering::Acquire) as *const AtomicU64) };
    let mut results = [0u8; 24];
    loop {
        // SAFETY: a one-handle list and a result buffer for it.
        if unsafe { syscall4(SYS_WAIT, ch.as_ptr() as u64, 1, results.as_mut_ptr() as u64, u64::MAX) } < 0 {
            count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Bootstrap registers: `rdi` notif, `rsi` namespace, `rdx` endpoint (the setup channel for a
/// Tier-1 stage), `rcx` `arg0`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    if libstream::setup::setup_is_pending(arg0) {
        run_stage(notif, ns, endpoint, arg0);
    }
    match arg0 {
        GRANTS_CHILD => run_grants_child(ns, endpoint),
        EXIT_WAITING => exit_waiting(endpoint),
        EXIT_RUNNING => exit_running(endpoint),
        EXIT_ALONE => exit_alone(),
        EXIT_SPAWNING => exit_spawning(endpoint),
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
