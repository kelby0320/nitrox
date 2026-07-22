//! `child` — the Phase-1 IPC handle-transfer demo worker.
//!
//! Spawned by `parent` with three bootstrap arguments (seeded by the kernel into
//! `rdi`/`rsi`/`rdx`, i.e. the three `extern "C"` parameters):
//!
//! - `notif`    — a handle to this process's own notification channel (unused);
//! - `endpoint` — one end of an IPC channel shared with the sibling child;
//! - `arg0`     — low 8 bits select the **role**, the rest is a role-specific payload:
//!   `0` = sender, `1` = receiver, `2` = exit immediately (the exit-storm stress
//!   child; no endpoint needed), `3` = hard-float worker (payload = per-worker seed).
//!
//! Role 0 creates a `MemoryObject`, writes a marker into it, and **transfers the
//! handle** to the sibling over `endpoint` (capability propagation). Role 1
//! receives the handle, maps the same object, and reads the marker back —
//! proving the capability crossed the process boundary and aliases shared frames.
//!
//! Role 3 is the Phase-4 hardware-floating-point proof: ordinary Rust `f64` arithmetic
//! (plus an `#[target_feature(enable = "avx2")]` SIMD path) checked bit-exactly against
//! integer math, across syscalls and preemption, with a per-worker seed so concurrent
//! workers hold different live FP state. See [`run_fp_worker`].

#![no_std]
#![no_main]

use core::arch::asm;
use libkern::{
    IpcMsg, RIGHT_MAP_READ, RIGHT_MAP_WRITE, SENDMODE_NOBLOCK, SYS_CHANNEL_RECV, SYS_CHANNEL_SEND,
    CLOCK_MONOTONIC, SYS_CLOCK_READ, SYS_HANDLE_CLOSE, SYS_MEMORY_CREATE, SYS_MEMORY_MAP,
    SYS_NS_BIND, SYS_NS_LOOKUP, SYS_TIMER_CREATE, SYS_TIMER_SET, SYS_WAIT, exit, kprint, syscall1,
    syscall2, syscall4, syscall5,
};

const PAGE: u64 = 4096;
/// The marker the sender writes into the transferred object; the receiver
/// verifies it after mapping.
const MARKER: u64 = 0x00C0_FFEE;

static mut SEND_MSG: IpcMsg = IpcMsg::ZEROED;
static mut RECV_MSG: IpcMsg = IpcMsg::ZEROED;
static mut RECV_COUNT: usize = 0;
/// `sys_channel_send`/`recv` transferred-handle arrays.
static mut SEND_HANDLES: [u64; 1] = [0];
static mut RECV_HANDLES: [u64; 8] = [0; 8];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
static mut WAIT_HANDLES: [u64; 1] = [0];

/// Sender (role 0): create a MemoryObject, mark it, transfer the handle.
fn run_sender(endpoint: u64) -> ! {
    // SAFETY: valid syscalls; returns a handle or a negative error.
    let mem_h = unsafe { syscall2(SYS_MEMORY_CREATE, PAGE, 0) };
    if mem_h < 0 {
        kprint(b"child[send]: memory create FAIL\n");
        exit(1);
    }
    let mem_h = mem_h as u64;
    // Map it read/write and write the marker.
    // SAFETY: valid syscall; returns the mapped address or a negative error.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem_h, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        kprint(b"child[send]: memory map FAIL\n");
        exit(1);
    }
    // SAFETY: `addr` is a page the kernel mapped R/W into our address space.
    unsafe { (addr as u64 as *mut u64).write_volatile(MARKER) };

    // Build a one-handle message and transfer the memory handle to the sibling.
    // SAFETY: SEND_MSG / SEND_HANDLES are valid writable .bss buffers.
    unsafe {
        SEND_MSG.header.payload_len = 0;
        SEND_HANDLES[0] = mem_h;
    }
    // SAFETY: valid endpoint + message + handles pointer; count 1, NoBlock.
    let sr = unsafe {
        syscall5(
            SYS_CHANNEL_SEND,
            endpoint,
            (&raw const SEND_MSG) as u64,
            (&raw const SEND_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        )
    };
    if sr == 0 {
        kprint(b"child[send]: transferred a memory object to the sibling\n");
        exit(0);
    } else {
        kprint(b"child[send]: send FAIL\n");
        exit(1);
    }
}

/// Receiver (role 1): receive the transferred handle, map it, verify the marker.
fn run_receiver(endpoint: u64) -> ! {
    // Block until the message arrives.
    // SAFETY: WAIT_HANDLES / WAIT_RESULTS are valid writable buffers.
    let waited = unsafe {
        WAIT_HANDLES[0] = endpoint;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
    };
    // SAFETY: valid out-params; on success the kernel installed the handle(s).
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            endpoint,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    // SAFETY: on success the kernel wrote the count + handle values.
    let (count, mem_h) = unsafe { ((&raw const RECV_COUNT).read(), (&raw const RECV_HANDLES[0]).read()) };
    if waited != 1 || rr != 0 || count != 1 {
        kprint(b"child[recv]: recv FAIL\n");
        exit(1);
    }

    // Map the transferred object and read the marker back.
    // SAFETY: `mem_h` is a memory handle just installed in our table.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem_h, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        kprint(b"child[recv]: map transferred object FAIL\n");
        exit(1);
    }
    // SAFETY: `addr` is the mapped, transferred page.
    let got = unsafe { (addr as u64 as *const u64).read_volatile() };
    if got == MARKER {
        kprint(b"child[recv]: mapped transferred object, marker=0xc0ffee ok\n");
        exit(0);
    } else {
        kprint(b"child[recv]: marker mismatch\n");
        exit(1);
    }
}

/// Exercise the **inherited namespace** (sandbox-by-construction): resolve a path
/// the parent bound into the child's namespace, and confirm the inherited handle
/// is LOOKUP-only by attempting a bind and expecting `NoAccess`. `ns` is the
/// child's root-namespace handle (`rsi`); `resource` is any handle to try binding.
fn ns_inheritance_check(ns: u64, resource: u64) {
    if ns == 0 {
        kprint(b"child: no namespace inherited\n");
        return;
    }
    let path = b"/store";
    // Look up "/store" (requesting MAP_READ); wait for the pre-signalled PO.
    // SAFETY: valid path pointer + handle.
    let po = unsafe {
        syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, RIGHT_MAP_READ)
    };
    if po >= 0 {
        // SAFETY: WAIT_HANDLES / WAIT_RESULTS are valid writable buffers.
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
        if waited == 1 && status == 0 {
            kprint(b"child: /store resolved in inherited namespace\n");
        } else {
            kprint(b"child: /store lookup in inherited namespace MISS\n");
        }
    } else {
        kprint(b"child: ns_lookup FAIL\n");
    }
    // The inherited handle is LOOKUP-only: a bind must fail NoAccess (-2).
    let foo = b"/foo";
    // SAFETY: valid path pointer + handle.
    let br = unsafe {
        syscall4(SYS_NS_BIND, ns, foo.as_ptr() as u64, foo.len() as u64, resource)
    };
    if br == -2 {
        kprint(b"child: bind into inherited namespace denied (LOOKUP-only)\n");
    } else {
        kprint(b"child: bind unexpectedly allowed/other error\n");
    }
}

// === role 3: the hard-float worker =======================================================
//
// The first Nitrox userspace code that computes with **real hardware floating point** —
// ordinary Rust `f64` arithmetic compiled for `x86_64-unknown-nitrox`, lowered to `mulsd`
// / `addsd` rather than the `__muldf3` libcalls the old soft-float target emitted. See
// the decision log, 2026-07-21 (FP enablement Parts C and D).

/// Exit codes, so `parent` can tell *which* invariant broke rather than just "nonzero".
const FP_EXIT_SCALAR_MISMATCH: i64 = 20;
/// The round-trip transform did not return the values bit-exactly.
const FP_EXIT_ROUNDTRIP: i64 = 21;
/// The AVX2 path disagreed with the scalar path.
const FP_EXIT_SIMD_MISMATCH: i64 = 22;
/// The kernel enabled AVX in `XCR0` but the `YMM` state bit was not visible from ring 3.
const FP_EXIT_XCR0: i64 = 23;

/// Values per worker. Eight `f64` — two `ymm` registers' worth, so the AVX2 path below
/// does two vector iterations.
const FP_LANES: usize = 8;
/// Rounds of transform → syscall → inverse-transform → verify.
const FP_ROUNDS: u32 = 12;

/// `CPUID` leaf/subleaf, returning `(eax, ebx, ecx, edx)`.
///
/// Unprivileged: `CPUID` is legal at CPL 3.
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

/// Whether AVX2 is usable *from this process* — which is a question about the OS, not
/// just the CPU.
///
/// The full three-part check the ecosystem uses: the CPU implements AVX2
/// (`CPUID.07H:EBX[5]`), the OS turned on `XSAVE` (`CPUID.01H:ECX[27]`, mirroring
/// `CR4.OSXSAVE`), **and** the OS actually enabled the SSE + AVX state components in
/// `XCR0`. That last clause is the interesting one here: it is userspace independently
/// confirming, from ring 3, the `XCR0` write the kernel performed in `fpu_init_cpu`.
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
        // Horizontal sum of the four lanes. The lane values are exact integers well
        // under 2^53, so addition is exact and this reassociation is bit-identical to
        // the scalar left-to-right sum — which is precisely why the workload is built
        // from exact integers.
        let hi = _mm256_extractf128_pd(acc, 1);
        let lo = _mm256_castpd256_pd128(acc);
        let s = _mm_add_pd(lo, hi);
        let s = _mm_add_sd(s, _mm_unpackhi_pd(s, s));
        _mm_cvtsd_f64(s)
    }
}

/// Σ v[k]², in plain scalar Rust.
fn sum_squares_scalar(v: &[f64; FP_LANES]) -> f64 {
    let mut acc = 0.0f64;
    for x in v.iter() {
        acc += x * x;
    }
    acc
}

/// Sleep roughly `ms` milliseconds by arming a one-shot timer and blocking on it —
/// a genuine deschedule, so the scheduler switches this thread out and back in.
fn timer_nap_ms(ms: u64) {
    // SAFETY: a valid syscall; returns a handle (>= 0) or a negative KError.
    let th = unsafe { syscall1(SYS_TIMER_CREATE, 0) };
    if th < 0 {
        return; // no timer: fall through without napping rather than failing the check
    }
    let th = th as u64;
    // SAFETY: FP_CLOCK is a writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut FP_CLOCK) as u64) };
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

/// Out-params for [`timer_nap_ms`]. Single-threaded process; touched only there.
static mut FP_CLOCK: u64 = 0;
static mut FP_WAIT_HANDLES: [u64; 1] = [0];
static mut FP_WAIT_RESULTS: [u8; 24] = [0; 24];

/// Role 3 — prove hardware floating point works in ring 3, and that a process's FP state
/// survives syscalls, preemption, and other processes doing the same thing.
///
/// Every value is a small exact integer held in an `f64`, which makes all of the checks
/// **bit-exact** rather than epsilon-fuzzy:
///
/// - **Cross-check against integer math.** Σ v[k]² is computed in `f64` and again in
///   `u64`, and the two must agree exactly. A self-consistent-but-wrong FPU (a bad
///   multiply, a stuck rounding mode, an `MXCSR` we failed to initialise) fails here,
///   where a float-only check would not.
/// - **Round-trip invariant across a syscall.** `x → 2x+1 → (x-1)/2` is exactly
///   invertible in binary floating point for these magnitudes. The forward half runs,
///   then the worker crosses into the kernel and burns enough cycles to be preempted
///   several times, then the inverse half must reproduce the original bit patterns.
/// - **Scalar vs. AVX2.** When the OS has enabled `YMM` state, the same sum computed
///   through `#[target_feature(enable = "avx2")]` intrinsics must match the scalar
///   result exactly — the per-function opt-in pattern the GUI toolkit's font and image
///   crates will use.
///
/// `seed` differs per worker, so concurrent workers hold *different* live FP state; if
/// the context switch cross-wired two processes' register files, the round-trip check
/// would see another worker's values.
fn run_fp_worker(seed: u64) -> ! {
    // Small exact integers: base ≤ 8·1024, so v[k]² and their sum stay far below 2^53
    // and every operation below is exact in `f64`.
    let base = (seed & 0x7) * 1024 + 1;
    let mut v = [0f64; FP_LANES];
    let mut expect_sq: u64 = 0;
    for k in 0..FP_LANES {
        let n = base + k as u64;
        v[k] = n as f64;
        expect_sq += n * n;
    }
    let original = v;

    // The FPU must agree with integer arithmetic.
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
        // Block on a short timer while the transformed values are live. A real
        // deschedule + wake beats burning cycles: it *guarantees* a context switch (and
        // very likely a CPU migration) at the same cost under TCG and KVM, whereas a
        // spin long enough to span a tick under emulation finishes in microseconds under
        // KVM and might never be preempted at all.
        timer_nap_ms(2);
        for x in v.iter_mut() {
            *x = (*x - 1.0) / 2.0;
        }
        // Bit-exact: these are integers, so the round trip is lossless.
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

/// Bootstrap registers (`kernel/src/syscall/table.rs`): `rdi` = notification
/// channel (unused here), `rsi` = inherited root namespace, `rdx` = the shared
/// channel endpoint, `rcx` = `arg0`.
///
/// `arg0` is split: the low 8 bits select the **role**, the rest is a role-specific
/// payload. Roles 0–2 carry no payload, so their `arg0` values are unchanged; role 3
/// takes its per-worker seed from the high bits.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let role = arg0 & 0xFF;
    // Role 2 — the exit-storm stress child (spawned with no handles): exit
    // immediately. The payload IS the process teardown — kernel-stack free,
    // TLB shootdown, reap — racing across CPUs (substrate-hardening Part F;
    // decision log 2026-07-21).
    if role == 2 {
        exit(0);
    }
    // Role 3 — the hard-float worker (spawned with no handles).
    if role == 3 {
        run_fp_worker(arg0 >> 8);
    }
    ns_inheritance_check(ns, endpoint);
    if role == 0 {
        run_sender(endpoint);
    } else {
        run_receiver(endpoint);
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}
