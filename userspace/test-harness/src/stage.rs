//! `test-stage` — a worker `parent` spawns. With a setup message pending (`arg0` marks it) it
//! dispatches on `argv[0]`: `"stage <rows>"` verifies a TSM1 stream on stdin, `"fp <seed>"` runs
//! the hard-float worker ([`run_fp_worker`]). With `arg0 == 0` it exits at once: the exit-storm
//! child, whose payload is the process teardown (kernel-stack free, TLB shootdown, reap) racing
//! across CPUs.

#![no_std]
#![no_main]

extern crate alloc;

use core::arch::asm;
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

/// Bootstrap registers: `rdi` notif, `rsi` namespace, `rdx` endpoint (the setup channel for a
/// Tier-1 stage), `rcx` `arg0`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    if libstream::setup::setup_is_pending(arg0) {
        run_stage(notif, ns, endpoint, arg0);
    }
    exit(0);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}
