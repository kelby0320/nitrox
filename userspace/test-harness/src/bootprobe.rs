//! `boot-probe` — the in-guest substrate checks, and the boot verdict.
//!
//! **Why this is a program and not a phase of a supervisor.** The SMP and floating-point
//! gates below lived in `session-mgr`, and the filesystem tests still live in `init`. They
//! were there because *the verdict* was, not because they belong to a supervisor:
//! `sched_gate` called itself "the Phase 3 clause 3 verdict gate, checked synchronously at
//! the single PASS point", which is a statement about where the verdict is, and nothing
//! about sessions. Move the verdict to a program whose job is adjudication and the probes
//! follow it out. See [`docs/planning/test-path-retrofit.md`](../../../docs/planning/test-path-retrofit.md).
//!
//! **What it now owns**, as of Part B: the clause-3 scheduler gate, the hard-float gate,
//! and `SYS_TEST_EXIT`. `init` still writes a FAIL verdict for a critical-path boot failure
//! or a crashed demo chain — that is a different question ("did the boot get this far") and
//! it is answered before this program starts. Part C moves `init`'s filesystem tests here.
//!
//! **The ordering that makes the gates meaningful is `init`'s, and it is serial.**
//! `init::supervise` runs the demo chain synchronously and fails the run on a non-zero
//! exit, and only then hands off to the login chain that reaches `service-mgr` and this
//! program. So everything the run adjudicates has already happened when the gates run, and
//! they are the last thing before the only `SYS_TEST_EXIT(PASS)` call. That placement is
//! the whole reason `fp_gate` was moved out of the demo `parent` in the first place — see
//! its own doc comment.
//!
//! **Started by `service-mgr`** from `/initramfs/etc/services.toml`, which carries a
//! `[service.boot-probe]` table only in selftest / test-harness images. It is an ordinary
//! declared service: a control channel at `rdx`, a LOOKUP-only view of the root namespace
//! at `rsi`, no syscaps, and `policy = "never"` — start once, do not restart.

#![no_std]
#![no_main]

/// `Line` builds its text on the heap, so this bin needs an allocator.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

use libkern::debug::Line;
use libkern::{
    RIGHT_MAP_READ, RIGHT_MAP_WRITE, SYS_FILE_CREATE, SYS_FILE_GROW, SYS_FILE_SYNC,
    SYS_HANDLE_CLOSE, SYS_MEMORY_MAP, SYS_MEMORY_UNMAP, SYS_NS_LOOKUP, SYS_TEST_EXIT, SYS_WAIT,
    TEST_EXIT_FAILURE, TEST_EXIT_SUCCESS, exit, kprint, syscall1, syscall2, syscall4,
    syscall5,
};

/// Page size, for the mapped-file checks below.
const PAGE: u64 = 4096;

static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];

/// Block until `handle` signals. `false` if the wait returned anything but one ready.
fn wait_one(handle: u64) -> bool {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = handle;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    waited == 1
}

/// Resolve `path` in `ns` with `rights`, awaiting the `PendingOperation`. Returns
/// `(status, handle)`; a non-zero status means the lookup failed and `handle` is `0`.
fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> (i32, u64) {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po < 0 {
        return (po as i32, 0);
    }
    if !wait_one(po as u64) {
        // SAFETY: closing our own PO.
        unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
        return (-1, 0);
    }
    // SAFETY: the wait completed, so the kernel wrote a 24-byte `IoResult`.
    let (status, handle) = unsafe {
        (
            i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]),
            u64::from_le_bytes([
                WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
                WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
            ]),
        )
    };
    // SAFETY: closing our own PO handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
    (status, handle)
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

/// The Phase 4 **hardware floating point** verdict gate, checked synchronously at the
/// single PASS point — the same placement, and for the same reason, as [`sched_gate`].
///
/// Userspace now compiles for `x86_64-unknown-nitrox`, a hard-float target: `f64`
/// arithmetic lowers to `mulsd`/`addsd` instead of the `__muldf3` libcalls the old
/// soft-float target emitted, and the kernel swaps the FP register file on every context
/// switch. This gate proves that actually works, from ring 3:
///
/// - **Against integer math.** Σ v[k]² is computed in `f64` and again in `u64` and must
///   agree *exactly* — every value is a small exact integer, so the comparison is
///   bit-exact rather than epsilon-fuzzy. A self-consistent-but-wrong FPU (a bad
///   multiply, a stuck rounding mode, an `MXCSR` we failed to initialise) fails here
///   where a float-only check would not.
/// - **Round trip across a syscall.** `x → 2x+1 → (x-1)/2` is exactly invertible at
///   these magnitudes. The forward half runs, the process crosses into the kernel (and
///   may be preempted and migrated), and the inverse half must reproduce the original
///   bit patterns.
/// - **Scalar vs. AVX2, and `XCR0` from ring 3.** When the CPU has AVX2 *and* the OS
///   enabled the SSE+AVX state components — read back with `XGETBV`, which is userspace
///   independently confirming the `XCR0` write the kernel made in `fpu_init_cpu` — the
///   same sum computed through `#[target_feature(enable = "avx2")]` intrinsics must
///   match exactly. That is the per-function opt-in pattern the GUI toolkit's font and
///   image crates will use.
///
/// **Why beside the verdict and not in the demo `parent`.** It was in `parent` first, and a
/// KVM boot-loop showed it completing in only 2 of 15 runs: whoever owns the verdict races
/// the demo chain, so on a fast boot the run was adjudicated PASS while the FP workers were
/// still running — the check silently did not execute. Running it *immediately before* the
/// only `SYS_TEST_EXIT(PASS)` call is what makes it airtight, and that property moved here
/// intact when the verdict did: `boot-probe` is now the single PASS point, and `init` starts
/// it only after the demo chain has exited zero. `parent` keeps a *concurrent* multi-process
/// version as extra breadth; this one is the guarantee.
fn fp_gate() -> bool {
    const LANES: usize = 8;
    let mut v = [0f64; LANES];
    let mut expect_sq: u64 = 0;
    for k in 0..LANES {
        let n = 1024 + k as u64;
        v[k] = n as f64;
        expect_sq += n * n;
    }
    let original = v;

    let sum_scalar = |a: &[f64; LANES]| {
        let mut acc = 0.0f64;
        for x in a.iter() {
            acc += x * x;
        }
        acc
    };

    if sum_scalar(&v) != expect_sq as f64 {
        kprint(b"boot-probe: fp gate FAIL (f64 disagrees with integer math)\n");
        return false;
    }

    // Round trip across a syscall, with the transformed values live.
    for x in v.iter_mut() {
        *x = *x * 2.0 + 1.0;
    }
    kprint(b"");
    for x in v.iter_mut() {
        *x = (*x - 1.0) / 2.0;
    }
    if v != original || sum_scalar(&v) != expect_sq as f64 {
        kprint(b"boot-probe: fp gate FAIL (state lost across a syscall)\n");
        return false;
    }

    match fp_avx2_usable() {
        Err(()) => {
            kprint(b"boot-probe: fp gate FAIL (CPU has AVX2 but XCR0 lacks YMM state)\n");
            false
        }
        Ok(false) => {
            kprint(b"boot-probe: fp gate ok (f64 verified in ring 3; no AVX2)\n");
            true
        }
        Ok(true) => {
            // SAFETY: `fp_avx2_usable` confirmed the CPU feature and that the OS enabled
            // the SSE+AVX state components in `XCR0`.
            let simd = unsafe { fp_sum_squares_avx2(&v) };
            if simd != expect_sq as f64 {
                kprint(b"boot-probe: fp gate FAIL (avx2 disagrees with scalar)\n");
                return false;
            }
            kprint(b"boot-probe: fp gate ok (f64 + avx2 verified in ring 3)\n");
            true
        }
    }
}

/// `CPUID`, unprivileged at CPL 3. Returns `(eax, ebx, ecx, edx)`.
fn fp_cpuid(leaf: u32, subleaf: u32) -> (u32, u32, u32, u32) {
    let (a, b, c, d);
    // SAFETY: `cpuid` has no memory effects and is valid in ring 3. `rbx` is reserved by
    // LLVM, so it is routed through `rsi` by hand.
    unsafe {
        core::arch::asm!(
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

/// `Ok(true)` if AVX2 is usable from this process, `Ok(false)` if the CPU or OS simply
/// does not offer it, `Err(())` if the CPU has AVX2 but the OS left the `YMM` state
/// component disabled — a kernel bug worth failing on rather than silently degrading.
fn fp_avx2_usable() -> Result<bool, ()> {
    let (_, _, ecx1, _) = fp_cpuid(1, 0);
    let osxsave = ecx1 & (1 << 27) != 0;
    let (_, ebx7, _, _) = fp_cpuid(7, 0);
    let cpu_has_avx2 = ebx7 & (1 << 5) != 0;
    if !osxsave {
        return Ok(false);
    }
    let (lo, hi): (u32, u32);
    // SAFETY: `CR4.OSXSAVE` confirmed above, so `XGETBV` is not `#UD`; ECX=0 selects
    // `XCR0`, the only extended control register that exists.
    unsafe {
        core::arch::asm!("xgetbv", in("ecx") 0u32, out("eax") lo, out("edx") hi,
                         options(nomem, nostack, preserves_flags));
    }
    let xcr0 = ((hi as u64) << 32) | (lo as u64);
    let ymm_enabled = xcr0 & 0b110 == 0b110; // SSE (bit 1) + AVX (bit 2)
    if cpu_has_avx2 && !ymm_enabled {
        return Err(());
    }
    Ok(cpu_has_avx2 && ymm_enabled)
}

/// Σ v[k]² through AVX2, four `f64` lanes at a time.
///
/// # Safety
/// The caller must have confirmed AVX2 is usable via [`fp_avx2_usable`].
#[target_feature(enable = "avx2")]
unsafe fn fp_sum_squares_avx2(v: &[f64; 8]) -> f64 {
    use core::arch::x86_64::*;
    // SAFETY: `v` is 8 contiguous `f64`, so both 4-lane loads stay in bounds; the caller
    // confirmed the AVX2 feature is present.
    unsafe {
        let a = _mm256_loadu_pd(v.as_ptr());
        let b = _mm256_loadu_pd(v.as_ptr().add(4));
        let acc = _mm256_add_pd(_mm256_mul_pd(a, a), _mm256_mul_pd(b, b));
        // The lane values are exact integers well under 2^53, so addition is exact and
        // this reassociation is bit-identical to the scalar left-to-right sum.
        let hi = _mm256_extractf128_pd(acc, 1);
        let lo = _mm256_castpd256_pd128(acc);
        let s = _mm_add_pd(lo, hi);
        let s = _mm_add_sd(s, _mm_unpackhi_pd(s, s));
        _mm_cvtsd_f64(s)
    }
}

/// The Phase 3 **clause 3** verdict gate, checked synchronously at the single
/// PASS point: resolve `/proc/sched/stats` through the inherited namespace, map
/// the snapshot, and require **≥ 2 CPUs with `switches` > 0** ("two CPUs
/// visibly active via `/proc`"). Login proving alone must not PASS a boot whose
/// SMP substrate has died — and because this runs *before* the only
/// `SYS_TEST_EXIT(PASS)` call, a failure cannot lose a race to the verdict (the
/// demo `parent`'s richer sched-stats check exits nonzero for init to fail the
/// run, but that path races the login chain; this placement is airtight).
fn sched_gate(root_ns: u64) -> bool {
    let (st, mem) = ns_lookup(root_ns, b"/proc/sched/stats", RIGHT_MAP_READ);
    if st != 0 || mem == 0 {
        kprint(b"boot-probe: sched gate: lookup FAIL\n");
        return false;
    }
    // SAFETY: register-only syscall; `mem` is a MemoryObject handle with MAP_READ.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, 4096, RIGHT_MAP_READ) };
    if addr < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
        kprint(b"boot-probe: sched gate: map FAIL\n");
        return false;
    }
    // SAFETY: `addr` is a page the kernel mapped MAP_READ holding the snapshot
    // text (zero-padded to the page).
    let text = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, 4096) };
    let active = cpus_with_switches(text);
    // SAFETY: unmapping the page mapped above (`text` is not used past here);
    // closing our own handle.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, addr as u64, 0);
        syscall1(SYS_HANDLE_CLOSE, mem);
    }
    if active >= 2 {
        kprint(b"boot-probe: sched gate ok (>=2 CPUs with switches>0)\n");
        true
    } else {
        kprint(b"boot-probe: sched gate FAIL (<2 CPUs with switches>0)\n");
        false
    }
}

/// Fire the boot verdict — terminate QEMU via `SYS_TEST_EXIT` with pass or fail.
///
/// **The single PASS point.** `test-qemu` adjudicates the whole boot from this one call, so
/// every check that must gate the run has to happen before it and in this process. That is
/// why the SMP and floating-point gates above are here rather than wherever they are
/// conceptually at home: they were in `session-mgr` for the same reason, because the verdict
/// was, and they followed it here when it moved
/// (`docs/planning/test-path-retrofit.md` Part B).
///
/// Outside `test-qemu` the `isa-debug-exit` device is not attached, so the port write is
/// ignored and the syscall returns `Unsupported` — the caller carries on. It must not park
/// instead: doing so once stranded a thread and deadlocked every later TLB shootdown.
fn verdict(ok: bool) {
    let code = if ok { TEST_EXIT_SUCCESS } else { TEST_EXIT_FAILURE };
    kprint(if ok {
        b"boot-probe: test-harness verdict PASS\n"
    } else {
        b"boot-probe: test-harness verdict FAIL\n"
    });
    // SAFETY: SYS_TEST_EXIT takes the verdict in a0; under the kernel test-harness build
    // it writes isa-debug-exit and QEMU terminates (so this does not return in practice).
    unsafe { syscall1(SYS_TEST_EXIT, code as u64) };
}

// === the filesystem tests, moved out of PID 1 ==========================================
//
// Five of these lived in `init` — 32 % of the file — because `init` was where the boot
// self-test ran. They exercise `fs-server-ext4` through the namespace, which any program
// with the right bindings can do, and better: one that fails may do so without taking the
// boot with it. Retrofit Part C.
//
// **They now gate the verdict, which they never did before.** Every failure path in `init`
// was a bare `return` after a `FAIL` print — 19 of them — so a broken filesystem printed
// `init: create MISMATCH` and the run passed. That is decoration wearing the word "test",
// and exactly the class this plan is about, so each returns `bool` here and the verdict is
// their conjunction.
//
// **`subtree_bind_test` moved too, but its `/subtreetest` binding could not.** A test-only
// namespace bind is the one thing "data, not code" cannot express — `[handles].namespace` is
// unparsed and a declared service gets `namespace: 0`, an inherited root — so `init` still
// makes it under `selftest`, and that is the single cfg Part C could not remove. Removing it
// anyway broke the demo harness's case 8, which needs a binding that is *also* an openable
// directory to prove `move` refuses to recurse through a mount.
//
// What it smoke-tests — bind-mount sharing, one registration behind two names — is now
// *also* a deterministic kernel host test,
// `namespace::tests::one_registration_bound_twice_is_shared_not_duplicated`.

/// Size of the Part-5 large-file fixture (`/system/large.bin`). MUST match the
/// xtask generator (`tools/xtask/src/main.rs`). 32 KiB = 8 pages — past the old
/// 64 KiB eager read cap, so reading it proves the page cache lifts the cap.
/// (Was 64 pages; trimmed to 8 because each page demand-faults through the
/// stateless fs-server fill at ~325 ms/page under QEMU — read-ahead is a Phase-3
/// item, see docs/rationale/deferred-decisions.md.)
const LARGE_FILE_BYTES: usize = 32 * 1024;

/// The expected byte at file offset `i` of `/system/large.bin` — position-sensitive
/// (the page index `i >> 12` in the high part) so a mis-faulted page is detected.
/// MUST match the xtask generator.
fn fill_byte(i: usize) -> u8 {
    (((i >> 12) ^ i) & 0xFF) as u8
}

/// fs-server-rw Part C milestone (selftest): **overwrite** an existing file in place through
/// a `MAP_WRITE` mapping, `sys_file_sync`, then re-resolve (a fresh `FileObject` that reads
/// the block from disk) and verify the change persisted — proving the Model A write data path
/// (dirty pages → write IRPs → device) with no fs-server metadata write.
fn overwrite_test(root_ns: u64) -> bool {
    let path = b"/system/rwtest";
    let marker = [0xDEu8, 0xAD, 0xBE, 0xEF];

    // 1. Map MAP_READ | MAP_WRITE; note an untouched byte, then overwrite bytes 0..4.
    let (st, fh) = ns_lookup(root_ns, path, RIGHT_MAP_READ | RIGHT_MAP_WRITE);
    if st != 0 || fh == 0 {
        kprint(b"boot-probe: rwtest lookup FAIL\n");
        return false;
    }
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, fh, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        kprint(b"boot-probe: rwtest map FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
        return false;
    }
    let base = addr as u64;
    // SAFETY: byte 8 is within the mapped page; read the original (== 8) to compare later.
    let orig8 = unsafe { ((base + 8) as *const u8).read_volatile() };
    // SAFETY: bytes 0..4 are within the writable mapping — the write dirties the page.
    for (i, m) in marker.iter().enumerate() {
        unsafe { ((base + i as u64) as *mut u8).write_volatile(*m) };
    }
    // 2. Flush the mapping's pages to disk (Model A write IRPs to the existing LBAs).
    // SAFETY: `fh` is our writable FileObject handle.
    if unsafe { syscall1(SYS_FILE_SYNC, fh) } != 0 {
        kprint(b"boot-probe: rwtest sync FAIL\n");
    }

    // 3. Re-resolve (a fresh FileObject reads from disk) and verify the overwrite persisted
    //    and the untouched byte is unchanged.
    let (st2, fh2) = ns_lookup(root_ns, path, RIGHT_MAP_READ);
    if st2 != 0 || fh2 == 0 {
        kprint(b"boot-probe: rwtest re-read lookup FAIL\n");
        return false;
    }
    let addr2 = unsafe { syscall4(SYS_MEMORY_MAP, fh2, 0, PAGE, RIGHT_MAP_READ) };
    if addr2 < 0 {
        kprint(b"boot-probe: rwtest re-read map FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh2) };
        return false;
    }
    let base2 = addr2 as u64;
    let mut ok = true;
    for (i, m) in marker.iter().enumerate() {
        // SAFETY: within the mapped page.
        if unsafe { ((base2 + i as u64) as *const u8).read_volatile() } != *m {
            ok = false;
        }
    }
    // SAFETY: byte 8 within the page — must be unchanged.
    let reread8 = unsafe { ((base2 + 8) as *const u8).read_volatile() };
    if ok && reread8 == orig8 {
        kprint(b"boot-probe: rwtest overwrite persisted + verified ok\n");
        true
    } else {
        kprint(b"boot-probe: rwtest overwrite MISMATCH\n");
        false
    }
}

/// fs-server-rw Part D milestone (selftest): **grow** a file past EOF via `sys_file_grow`
/// (the fs-server allocates a block + extends its extent tree + updates the inode), write
/// into the newly-allocated region, `sys_file_sync`, then re-resolve and confirm the
/// appended data persisted — proving the write path's metadata mutation end to end.
fn grow_test(root_ns: u64) -> bool {
    let path = b"/system/rwtest";
    let marker = [0xC0u8, 0xFF, 0xEEu8, 0x11];
    let new_size: u64 = 8000; // 4096 (1 block) → 8000 (2 blocks)

    // 1. Grow-resolve: the fs-server grows the file, then replies its (2-block) map. The
    //    lookup returns a PO; wait for the handle.
    let po = unsafe {
        syscall5(
            SYS_FILE_GROW,
            root_ns,
            path.as_ptr() as u64,
            path.len() as u64,
            RIGHT_MAP_READ | RIGHT_MAP_WRITE,
            new_size,
        )
    };
    if po < 0 {
        kprint(b"boot-probe: grow submit FAIL\n");
        return false;
    }
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter.
    let (st, fh) = unsafe {
        WAIT_HANDLES[0] = po as u64;
        let w = syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        );
        let status =
            i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]);
        let handle = u64::from_le_bytes([
            WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
            WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
        ]);
        syscall1(SYS_HANDLE_CLOSE, po as u64);
        if w != 1 { (-1, 0) } else { (status, handle) }
    };
    if st != 0 || fh == 0 {
        kprint(b"boot-probe: grow FAIL\n");
        return false;
    }

    // 2. Map the grown file; write a marker in the **new** region (the appended 2nd block).
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, fh, 0, new_size, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        kprint(b"boot-probe: grow map FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
        return false;
    }
    let base = addr as u64;
    for (i, m) in marker.iter().enumerate() {
        // SAFETY: offset `PAGE + i` is in the 2nd mapped page (the appended block).
        unsafe { ((base + PAGE + i as u64) as *mut u8).write_volatile(*m) };
    }
    // SAFETY: `fh` is our writable handle.
    if unsafe { syscall1(SYS_FILE_SYNC, fh) } != 0 {
        kprint(b"boot-probe: grow sync FAIL\n");
    }

    // 3. Re-resolve (a fresh FileObject reads from disk) and verify the appended data.
    let (st2, fh2) = ns_lookup(root_ns, path, RIGHT_MAP_READ);
    if st2 != 0 || fh2 == 0 {
        kprint(b"boot-probe: grow re-read FAIL\n");
        return false;
    }
    let addr2 = unsafe { syscall4(SYS_MEMORY_MAP, fh2, 0, new_size, RIGHT_MAP_READ) };
    if addr2 < 0 {
        kprint(b"boot-probe: grow re-read map FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh2) };
        return false;
    }
    let base2 = addr2 as u64;
    let mut ok = true;
    for (i, m) in marker.iter().enumerate() {
        // SAFETY: within the 2nd mapped page.
        if unsafe { ((base2 + PAGE + i as u64) as *const u8).read_volatile() } != *m {
            ok = false;
        }
    }
    if ok {
        kprint(b"boot-probe: grow appended a block + persisted + verified ok\n");
        true
    } else {
        kprint(b"boot-probe: grow MISMATCH\n");
        false
    }
}

/// fs-server-rw Part E milestone (selftest): **create** a brand-new file via
/// `sys_file_create` (the fs-server allocates an inode + inserts a directory entry in the
/// parent, then grows it to the target size), write into it, `sys_file_sync`, then
/// re-resolve with a plain lookup and confirm both that the new path now resolves and that
/// its data persisted — proving inode allocation + directory-entry insertion end to end.
fn create_test(root_ns: u64) -> bool {
    let path = b"/system/created";
    let marker = [0xABu8, 0xCD, 0xEFu8, 0x42];
    let new_size: u64 = 4096; // fresh file → 1 block.

    // 1. Create-resolve: the fs-server creates the file, grows it, then replies its map.
    let po = unsafe {
        syscall5(
            SYS_FILE_CREATE,
            root_ns,
            path.as_ptr() as u64,
            path.len() as u64,
            RIGHT_MAP_READ | RIGHT_MAP_WRITE,
            new_size,
        )
    };
    if po < 0 {
        kprint(b"boot-probe: create submit FAIL\n");
        return false;
    }
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter.
    let (st, fh) = unsafe {
        WAIT_HANDLES[0] = po as u64;
        let w = syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        );
        let status =
            i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]);
        let handle = u64::from_le_bytes([
            WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
            WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
        ]);
        syscall1(SYS_HANDLE_CLOSE, po as u64);
        if w != 1 { (-1, 0) } else { (status, handle) }
    };
    if st != 0 || fh == 0 {
        kprint(b"boot-probe: create FAIL\n");
        return false;
    }

    // 2. Map the new file; write a marker at the start.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, fh, 0, new_size, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        kprint(b"boot-probe: create map FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
        return false;
    }
    let base = addr as u64;
    for (i, m) in marker.iter().enumerate() {
        // SAFETY: offset `i` is within the mapped first page.
        unsafe { ((base + i as u64) as *mut u8).write_volatile(*m) };
    }
    // SAFETY: `fh` is our writable handle.
    if unsafe { syscall1(SYS_FILE_SYNC, fh) } != 0 {
        kprint(b"boot-probe: create sync FAIL\n");
    }

    // 3. Re-resolve with a **plain** lookup (proves the directory entry is on disk: a path
    //    that did not exist before now resolves) and verify the data.
    let (st2, fh2) = ns_lookup(root_ns, path, RIGHT_MAP_READ);
    if st2 != 0 || fh2 == 0 {
        kprint(b"boot-probe: create re-read FAIL\n");
        return false;
    }
    let addr2 = unsafe { syscall4(SYS_MEMORY_MAP, fh2, 0, new_size, RIGHT_MAP_READ) };
    if addr2 < 0 {
        kprint(b"boot-probe: create re-read map FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh2) };
        return false;
    }
    let base2 = addr2 as u64;
    let mut ok = true;
    for (i, m) in marker.iter().enumerate() {
        // SAFETY: within the mapped first page.
        if unsafe { ((base2 + i as u64) as *const u8).read_volatile() } != *m {
            ok = false;
        }
    }
    if ok {
        kprint(b"boot-probe: create new file + persisted + verified ok\n");
        true
    } else {
        kprint(b"boot-probe: create MISMATCH\n");
        false
    }
}

/// The slice-8 Part-5 milestone: map the **large** file `/system/large.bin`
/// (lazily, a `FileObject`) and read **every** byte — each first touch of a page is
/// a demand fault the kernel services by a `File::ReadRange` to the fs-server. Verify
/// the position-sensitive content (so a mis-filled / mis-ordered page is caught) and
/// log the result. Proves **multi-page demand faulting** past the old 64 KiB cap.
fn read_large_file(root_ns: u64) -> bool {
    let (st, fh) = ns_lookup(root_ns, b"/system/large.bin", RIGHT_MAP_READ);
    if st != 0 || fh == 0 {
        kprint(b"boot-probe: /system/large.bin lookup FAIL\n");
        return false;
    }
    // Map the whole file lazily (a FileBacked VMA — no frames until faulted).
    let addr =
        unsafe { syscall4(SYS_MEMORY_MAP, fh, 0, LARGE_FILE_BYTES as u64, RIGHT_MAP_READ) };
    if addr < 0 {
        kprint(b"boot-probe: large.bin map FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
        return false;
    }
    let base = addr as u64;
    let mut mismatches = 0u64;
    let mut i = 0usize;
    while i < LARGE_FILE_BYTES {
        // First touch of each page faults; the kernel demand-fills it from the
        // fs-server. Subsequent bytes in the page are plain (already-resident) reads.
        // SAFETY: `base + i` is within the mapped [0, LARGE_FILE_BYTES) file range.
        let got = unsafe { ((base + i as u64) as *const u8).read_volatile() };
        if got != fill_byte(i) {
            mismatches += 1;
        }
        i += 1;
    }
    let mut ok = false;
    if mismatches == 0 {
        Line::new()
            .s(b"boot-probe: large.bin verified ")
            .u(LARGE_FILE_BYTES as u64)
            .s(b" bytes across ")
            .u(LARGE_FILE_BYTES as u64 / PAGE)
            .s(b" demand-faulted pages ok")
            .end();
        ok = true;
    } else {
        Line::new().s(b"boot-probe: large.bin MISMATCH count=").u(mismatches).end();
    }
    // SAFETY: closing our own handle (the mapping keeps the object alive meanwhile).
    unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
    ok
}

/// auth+session Part B milestone (selftest): prove **subtree-scoped namespace
/// binding** end to end. `mount_one` bound the fs endpoint a second time at
/// `/subtreetest` scoped to base `/system` (sharing the server's registration), so a
/// lookup of `/subtreetest/current-generation` must forward `system/current-generation`
/// to the server and resolve to the *same* file as `/system/current-generation`. Read
/// the leading bytes of both and confirm they match — the kernel prepended the base to
/// the forwarded suffix, and the shared registration routed both replies correctly.
fn subtree_bind_test(root_ns: u64) -> bool {
    // Resolve + map the first page of `path` read-only; returns its address or 0.
    fn map_first_page(root_ns: u64, path: &[u8]) -> u64 {
        let (st, fh) = ns_lookup(root_ns, path, RIGHT_MAP_READ);
        if st != 0 || fh == 0 {
            return 0;
        }
        let addr = unsafe { syscall4(SYS_MEMORY_MAP, fh, 0, PAGE, RIGHT_MAP_READ) };
        // The mapping pins its own reference to the object; close the handle.
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
        if addr < 0 { 0 } else { addr as u64 }
    }

    let direct = map_first_page(root_ns, b"/system/current-generation");
    let via_sub = map_first_page(root_ns, b"/subtreetest/current-generation");
    if direct == 0 || via_sub == 0 {
        kprint(b"boot-probe: subtree resolve FAIL\n");
        return false;
    }
    // Compare the leading bytes (the file is a short text line; the page tail is
    // zero-padded, so the head suffices).
    let mut same = true;
    for i in 0..64u64 {
        // SAFETY: both addresses map a full page; `i < 64 < PAGE`.
        let a = unsafe { ((direct + i) as *const u8).read_volatile() };
        let b = unsafe { ((via_sub + i) as *const u8).read_volatile() };
        if a != b {
            same = false;
            break;
        }
    }
    // SAFETY: unmap our two mappings. `init` ran this and never exits, so this said
    // "init runs forever — don't leak"; `boot-probe` exits a few lines below and the address
    // space goes with it either way. Still right to tidy before the checks that follow — and
    // a file mapping released inside `AddressSpace::drop` is exactly what exposed the
    // lock-order violation this move found.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, direct, PAGE);
        syscall2(SYS_MEMORY_UNMAP, via_sub, PAGE);
    }
    if same {
        kprint(b"boot-probe: subtree bind (/subtreetest -> /system) resolves + matches ok\n");
        true
    } else {
        kprint(b"boot-probe: subtree bind MISMATCH\n");
        false
    }
}

/// Bootstrap registers, as `service-mgr`'s `SPAWN_SERVICE` fills them: `rdi` = this
/// process's notification channel, `rsi` = the inherited LOOKUP-only root namespace,
/// `rdx` = the control-channel endpoint (`RECV | WAIT`), `rcx` = `arg0` (unused).
///
/// **The control endpoint is held until exit, and closing it early is a bug.** An earlier
/// version of this file closed it as its second instruction, reasoning that a probe with no
/// lifecycle protocol to serve has no use for it. It does have one use, and it is not the
/// probe's: `service-mgr` reads *this handle's* closure as "the child is gone"
/// (`supervise`), because a pid on `KIND_CHILD_EXITED` cannot be matched to a process
/// handle. Closing it early therefore reports a death that has not happened — observed as
/// `'boot-probe' exited code=unknown` printed before this function's own next line, and
/// under `policy = "always"` as a *second copy of a live service*, which is the exact
/// failure this program was added to prove is gone (PR #226 review, finding 1).
///
/// So: hold it, and let process teardown close it. That is what makes "peer closed" mean
/// "child exited", and it is a contract on every declared service rather than a quirk of
/// this one — see `docs/spec/service-toml-schema.md`.
///
/// **The verdict is fired, then the process exits.** `verdict` does not return under
/// `test-qemu` (QEMU terminates), but it does everywhere else — every other gate boots this
/// image without the `isa-debug-exit` device — so there is a real path past it.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"boot-probe: up\n");
    let _ = control;
    // `&` and not `&&`: every check runs and reports, so one failure does not hide the
    // rest — a boot that fails three of these should say three, not one.
    let ok = sched_gate(root_ns)
        & fp_gate()
        & read_large_file(root_ns)
        & overwrite_test(root_ns)
        & grow_test(root_ns)
        & create_test(root_ns)
        & subtree_bind_test(root_ns);
    verdict(ok);
    // **Reached only where the verdict device is absent** — every gate except `test-qemu`
    // boots this image without `isa-debug-exit`, so `SYS_TEST_EXIT` returns `Unsupported` and
    // execution continues here. Naming `test-qemu` on this line would name the one gate that
    // is not running when it is reached.
    //
    // The exit code tracks the gates, and `service-mgr` reports it. `check-terminal`'s
    // `check_service_attribution` requires `code=0`, so a `sched_gate` or `fp_gate` regression
    // fails **that** gate — with a message about attribution, since that is what it asserts.
    // The `boot-probe: … FAIL` line naming the real cause is directly above it in the same
    // transcript.
    exit(if ok { 0 } else { 1 });
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"boot-probe: PANIC\n");
    // A panicking probe must not let the run pass by defaulting to silence.
    verdict(false);
    exit(1);
}
