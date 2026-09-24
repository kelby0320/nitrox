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

extern crate alloc;

/// `Line` builds its text on the heap, so this bin needs an allocator.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

use libkern::debug::Line;
use libkern::abi::HandleInfo;
use libkern::{
    RIGHT_LOOKUP, RIGHT_TRANSFER, RIGHT_UNBIND, SYS_HANDLE_DUPLICATE, SYS_HANDLE_STAT,
    SYS_NS_DERIVE, SYS_NS_UNBIND,
};
use libkern::{
    IO_OPCODE_READ, IoOp, RIGHT_READ, SYS_FILE_TRUNCATE, SYS_IO_SUBMIT, SYS_MEMORY_CREATE,
    SYS_NS_SYNC,
};
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

// === what the device holds ==============================================================

/// What the **device** holds, as opposed to what the page cache does — the root partition
/// opened raw, read through the ext4 library `fs-server-ext4` is built on.
///
/// **Why the filesystem checks need it** (administration Part C.1). Every resolve of a file
/// now shares the one page-cache object any other resolve of it holds, so re-resolving a file
/// reads the cache, not the disk. The checks that used to prove a write "persisted" by
/// re-resolving would pass with no write-back at all.
struct RootDevice {
    device: u64,
    /// A page of scratch every read passes through, and its mapping.
    mem: u64,
    addr: u64,
}

impl RootDevice {
    /// The root partition, by the label this boot mounted it by — the disk image's or the live
    /// image's.
    fn open(ns: u64) -> Option<RootDevice> {
        let labels: [&[u8]; 2] =
            [b"/dev/disk/by-partlabel/nitrox-root", b"/dev/disk/by-partlabel/nitrox-live"];
        let device = labels.iter().find_map(|p| match ns_lookup(ns, p, RIGHT_READ) {
            (0, h) if h != 0 => Some(h),
            _ => None,
        })?;
        // SAFETY: register-only syscall.
        let mem = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
        if mem < 0 {
            close(device);
            return None;
        }
        // SAFETY: register-only syscall; `mem` is ours.
        let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem as u64, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
        if addr < 0 {
            close(mem as u64);
            close(device);
            return None;
        }
        Some(RootDevice { device, mem: mem as u64, addr: addr as u64 })
    }

    /// `len` bytes of the file at `path` from `offset`, as the device holds them. `None` if the
    /// file is shorter or does not read.
    fn read_file(&self, path: &[u8], offset: u64, len: usize) -> Option<alloc::vec::Vec<u8>> {
        let mut out = alloc::vec![0u8; len];
        match fs_server_ext4::ext4::read_file_range(self, path, offset, len, &mut out) {
            Ok(n) if n == len => Some(out),
            _ => None,
        }
    }
}

impl fs_server_ext4::BlockReader for RootDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), fs_server_ext4::FsError> {
        const SECTOR: u64 = 512;
        let mut done = 0usize;
        while done < buf.len() {
            let at = offset + done as u64;
            let start = at / SECTOR * SECTOR;
            let intra = (at - start) as usize;
            let take = (buf.len() - done).min(PAGE as usize - intra);
            let span = (intra + take).div_ceil(SECTOR as usize) as u64 * SECTOR;
            let op = IoOp { opcode: IO_OPCODE_READ, flags: 0, buffer: self.mem, buf_offset: 0, offset: start, length: span };
            // SAFETY: `device` is a block device handle this process holds; `&op` is a valid
            // `IoOp` naming a `MemoryObject` it owns.
            let po = unsafe { syscall2(SYS_IO_SUBMIT, self.device, (&op as *const IoOp) as u64) };
            if po < 0 || po_wait(po as u64) != (0, span) {
                return Err(fs_server_ext4::FsError::Io);
            }
            // SAFETY: `span <= PAGE` bytes are mapped at `addr`, which nothing else borrows.
            let src = unsafe { core::slice::from_raw_parts(self.addr as *const u8, span as usize) };
            buf[done..done + take].copy_from_slice(&src[intra..intra + take]);
            done += take;
        }
        Ok(())
    }
}

impl Drop for RootDevice {
    fn drop(&mut self) {
        // SAFETY: unmapping this value's own mapping.
        unsafe { syscall2(SYS_MEMORY_UNMAP, self.addr, 0) };
        close(self.mem);
        close(self.device);
    }
}

/// Wait for a `PendingOperation`, close it, and return its `(status, result)`; `(-1, 0)` if the
/// wait itself failed.
fn po_wait(po: u64) -> (i32, u64) {
    let ok = wait_one(po);
    // SAFETY: when the wait completed the kernel wrote a 24-byte `IoResult`.
    let r = unsafe {
        (
            i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]),
            u64::from_le_bytes([
                WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
                WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
            ]),
        )
    };
    close(po);
    if ok { r } else { (-1, 0) }
}

/// A size-changing resolve — `SYS_FILE_CREATE`, `_GROW` or `_TRUNCATE` of `path` to `size` —
/// returning the file's handle.
fn file_resize(nr: u64, ns: u64, path: &[u8], size: u64) -> Option<u64> {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe {
        syscall5(nr, ns, path.as_ptr() as u64, path.len() as u64, RIGHT_MAP_READ | RIGHT_MAP_WRITE, size)
    };
    if po < 0 {
        return None;
    }
    match po_wait(po as u64) {
        (0, h) if h != 0 => Some(h),
        _ => None,
    }
}

/// Map `len` bytes of file handle `h` with `rights`; the address, or `None`.
fn map_file(h: u64, len: u64, rights: u64) -> Option<u64> {
    // SAFETY: register-only syscall on a handle this process holds.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, h, 0, len, rights) };
    (addr >= 0).then_some(addr as u64)
}

/// The byte a pattern puts at `i` of page `p` — different per page, so a page served in the
/// other's place is caught.
fn c1_byte(p: u64, i: u64) -> u8 {
    (0xC1 + p as u8) ^ (i as u8)
}

/// **Administration Part C.1, end to end: one object per file, and a dirty one kept until a
/// sync.**
///
/// 1. **A write through one mapping is read through another without a sync** — two resolves of
///    the file share its object. The device does not have the write yet, which is what makes
///    the second read a reading of the cache.
/// 2. **Unmapped and closed without a sync, the write is kept, and `sys_ns_sync` writes it** —
///    a writer that forgot loses nothing. Checked on the device on both sides of the sync.
/// 3. **A truncate and a grow of a file held open read zero over the regrown range** — a whole
///    page and a partial tail — through a new mapping and on the device. The mapping reads the
///    kernel's half (pages retired, the tail zeroed); the device reads the server's (what the
///    grow allocated, and the tail it kept, zeroed).
fn file_cache_test(root_ns: u64) -> bool {
    let path = b"/system/c1-cache";
    let size = 2 * PAGE;
    let Some(dev) = RootDevice::open(root_ns) else {
        kprint(b"boot-probe: c1 root device FAIL\n");
        return false;
    };
    let on_device = |off: u64, n: usize| dev.read_file(path, off, n);
    let pattern = |p: u64, n: u64| (0..n).map(|i| c1_byte(p, i)).collect::<alloc::vec::Vec<u8>>();
    let zeroes = |n: usize| alloc::vec![0u8; n];

    // 1. Two resolves, two mappings, one object.
    let Some(w) = file_resize(SYS_FILE_CREATE, root_ns, path, size) else {
        kprint(b"boot-probe: c1 create FAIL\n");
        return false;
    };
    let (st, r) = ns_lookup(root_ns, path, RIGHT_MAP_READ);
    let (Some(wa), Some(ra)) = (
        map_file(w, size, RIGHT_MAP_READ | RIGHT_MAP_WRITE),
        if st == 0 { map_file(r, size, RIGHT_MAP_READ) } else { None },
    ) else {
        kprint(b"boot-probe: c1 map FAIL\n");
        return false;
    };
    for p in 0..2 {
        for i in 0..64 {
            // SAFETY: inside the writable mapping of `size` bytes.
            unsafe { ((wa + p * PAGE + i) as *mut u8).write_volatile(c1_byte(p, i)) };
        }
    }
    let mut shared = true;
    for p in 0..2 {
        for i in 0..64 {
            // SAFETY: inside the read-only mapping of `size` bytes.
            shared &= unsafe { ((ra + p * PAGE + i) as *const u8).read_volatile() } == c1_byte(p, i);
        }
    }
    let unwritten = on_device(0, 64) == Some(zeroes(64)) && on_device(PAGE, 64) == Some(zeroes(64));
    if shared && unwritten {
        kprint(b"boot-probe: c1 a write through one mapping reads through another, unsynced ok\n");
    } else {
        kprint(b"boot-probe: c1 shared-object MISMATCH\n");
    }

    // 2. Let go of everything without a sync; the write is kept, and `sys_ns_sync` writes it.
    // SAFETY: unmapping our own mappings.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, wa, 0);
        syscall2(SYS_MEMORY_UNMAP, ra, 0);
    }
    close(w);
    close(r);
    let kept = on_device(0, 64) == Some(zeroes(64));
    let sync_path = b"/system";
    // SAFETY: valid path pointer + namespace handle.
    let written = unsafe { syscall4(SYS_NS_SYNC, root_ns, sync_path.as_ptr() as u64, sync_path.len() as u64, 0) };
    let synced = written >= 1
        && on_device(0, 64) == Some(pattern(0, 64))
        && on_device(PAGE, 64) == Some(pattern(1, 64));
    if kept && synced {
        kprint(b"boot-probe: c1 an unsynced write closed reaches the device on sys_ns_sync ok\n");
    } else {
        kprint(b"boot-probe: c1 ns-sync MISMATCH\n");
    }

    // 3. Held open, with both pages resident: truncate into page 0, then grow back.
    let (st, h) = ns_lookup(root_ns, path, RIGHT_MAP_READ | RIGHT_MAP_WRITE);
    let Some(ha) = (if st == 0 { map_file(h, size, RIGHT_MAP_READ | RIGHT_MAP_WRITE) } else { None }) else {
        kprint(b"boot-probe: c1 reopen FAIL\n");
        return false;
    };
    // SAFETY: inside the mapping; faults both pages in.
    let resident = unsafe { ((ha + 1) as *const u8).read_volatile() == c1_byte(0, 1)
        && ((ha + PAGE + 1) as *const u8).read_volatile() == c1_byte(1, 1) };
    let truncated = file_resize(SYS_FILE_TRUNCATE, root_ns, path, 10).map(close).is_some();
    let g = file_resize(SYS_FILE_GROW, root_ns, path, size);
    let ga = g.and_then(|g| map_file(g, size, RIGHT_MAP_READ));
    let mut regrown = resident && truncated && ga.is_some();
    if let Some(ga) = ga {
        for i in 0..64 {
            // SAFETY: inside the read-only mapping of the regrown file.
            let (a, b) = unsafe {
                (((ga + i) as *const u8).read_volatile(), ((ga + PAGE + i) as *const u8).read_volatile())
            };
            regrown &= a == if i < 10 { c1_byte(0, i) } else { 0 } && b == 0;
        }
    }
    let mut kept_head = pattern(0, 10);
    kept_head.extend_from_slice(&zeroes(54));
    let regrown_on_device = on_device(0, 64) == Some(kept_head) && on_device(PAGE, 64) == Some(zeroes(64));
    // Clean up: unmapped first, so the sync leaves the file clean and it goes with its handles.
    // SAFETY: unmapping our own mappings; syncing our own writable handle.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, ha, 0);
        if let Some(ga) = ga {
            syscall2(SYS_MEMORY_UNMAP, ga, 0);
        }
        syscall1(SYS_FILE_SYNC, h);
    }
    close(h);
    if let Some(g) = g {
        close(g);
    }
    if regrown && regrown_on_device {
        kprint(b"boot-probe: c1 a truncate then a grow reads zero, a page and a tail ok\n");
    } else {
        kprint(b"boot-probe: c1 truncate-grow MISMATCH\n");
    }
    shared && unwritten && kept && synced && regrown && regrown_on_device
}

/// **Administration Part C.1b: an unlinked file's pages are not written back.** A file
/// written through a mapping and let go without a sync stays dirty in the kernel's cache.
/// When it is unlinked, the server's `File::Forget` takes it out of that cache before the
/// server frees its block. So a `sys_ns_sync` after the unlink finds nothing of it to write,
/// and the block the file had does not receive its bytes — which, after a free, could be
/// another file's.
///
/// The block is found before the unlink, through the ext4 library over the raw partition,
/// and read raw after the sync.
fn unlinked_file_test(root_ns: u64) -> bool {
    let path = b"/system/c1-unlinked";
    let Some(dev) = RootDevice::open(root_ns) else {
        kprint(b"boot-probe: c1b root device FAIL\n");
        return false;
    };
    let Some(w) = file_resize(SYS_FILE_CREATE, root_ns, path, PAGE) else {
        kprint(b"boot-probe: c1b create FAIL\n");
        return false;
    };
    let Some(wa) = map_file(w, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) else {
        close(w);
        kprint(b"boot-probe: c1b map FAIL\n");
        return false;
    };
    let pattern: alloc::vec::Vec<u8> = (0..64u64).map(|i| 0xF0 ^ i as u8).collect();
    for (i, b) in pattern.iter().enumerate() {
        // SAFETY: inside the writable mapping of one page.
        unsafe { ((wa + i as u64) as *mut u8).write_volatile(*b) };
    }
    // Let go without a sync: dirty, and kept by the kernel.
    // SAFETY: unmapping our own mapping.
    unsafe { syscall2(SYS_MEMORY_UNMAP, wa, 0) };
    close(w);

    // Where the file's one block is, and that it does not hold the bytes yet.
    let mut runs = [fs_server_ext4::BlockRun::default(); 4];
    let block_at = fs_server_ext4::ext4::map_file(&dev, path, &mut runs)
        .ok()
        .filter(|m| m.runs == 1 && runs[0].device_lba != 0)
        .map(|m| runs[0].device_lba * m.block_size as u64);
    let raw = |at: u64| {
        let mut b = [0u8; 64];
        fs_server_ext4::BlockReader::read_at(&dev, at, &mut b).ok().map(|()| b)
    };
    let Some(block_at) = block_at else {
        kprint(b"boot-probe: c1b block FAIL\n");
        return false;
    };
    let unwritten = raw(block_at).is_some_and(|b| b[..] != pattern[..]);

    // Unlink it through a session on its directory, then sync the mount.
    let mut buf = [0u8; 4096];
    let unlinked = match librsproto::session::Dir::open(root_ns, b"/system", &mut buf) {
        Ok(mut dir) => {
            let r = dir.unlink(b"c1-unlinked").is_ok();
            dir.close();
            r
        }
        Err(_) => false,
    };
    let sync_path = b"/system";
    // SAFETY: valid path pointer + namespace handle.
    let synced = unsafe { syscall4(SYS_NS_SYNC, root_ns, sync_path.as_ptr() as u64, sync_path.len() as u64, 0) } >= 0;
    let not_written = raw(block_at).is_some_and(|b| b[..] != pattern[..]);
    if unwritten && unlinked && synced && not_written {
        kprint(b"boot-probe: c1b an unlinked file's pages are not written back ok\n");
        true
    } else {
        kprint(b"boot-probe: c1b unlinked-file MISMATCH\n");
        false
    }
}

/// fs-server-rw Part C milestone (selftest): **overwrite** an existing file in place through
/// a `MAP_WRITE` mapping, `sys_file_sync`, then read the block **off the device** and verify
/// the change persisted — proving the Model A write data path (dirty pages → write IRPs →
/// device) with no fs-server metadata write. (A re-resolve read the disk until administration
/// Part C.1; it now shares this object, so the device is read directly — [`RootDevice`].)
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

    // 3. Read the device and verify the overwrite persisted and the untouched byte did not
    //    change.
    let on_device = RootDevice::open(root_ns).and_then(|d| d.read_file(path, 0, 9));
    let ok = on_device.as_deref().is_some_and(|b| b[..4] == marker);
    let reread8 = on_device.as_deref().map(|b| b[8]);
    if ok && reread8 == Some(orig8) {
        kprint(b"boot-probe: rwtest overwrite persisted + verified ok\n");
        true
    } else {
        kprint(b"boot-probe: rwtest overwrite MISMATCH\n");
        false
    }
}

/// fs-server-rw Part D milestone (selftest): **grow** a file past EOF via `sys_file_grow`
/// (the fs-server allocates a block + extends its extent tree + updates the inode), write
/// into the newly-allocated region, `sys_file_sync`, then read the device and confirm the
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

    // 3. Read the device — through the file's new extent, so the metadata is checked too —
    //    and verify the appended data.
    let ok = RootDevice::open(root_ns)
        .and_then(|d| d.read_file(path, PAGE, marker.len()))
        .is_some_and(|b| b == marker);
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
/// parent, then grows it to the target size), write into it, `sys_file_sync`, then read the
/// path off the device and confirm both that it resolves there and that its data persisted —
/// proving inode allocation + directory-entry insertion end to end.
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

    // 3. Read the device through the path: the directory entry is on disk (a path that did
    //    not exist before now resolves there) and so is the data.
    let ok = RootDevice::open(root_ns)
        .and_then(|d| d.read_file(path, 0, marker.len()))
        .is_some_and(|b| b == marker);
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
        & file_cache_test(root_ns)
        & unlinked_file_test(root_ns)
        & subtree_bind_test(root_ns)
        & auth_multi_client_test(root_ns)
        & ns_derive_test(root_ns)
        & view_broker_test(root_ns)
        & registry_test(root_ns)
        & devices_test(root_ns);
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

/// Prove `auth-service` serves **more than one client** — which is what M7 Part C claims and
/// what nothing else here would check.
///
/// `session-mgr` resolves `/svc/auth` at startup and holds its session for the machine's life,
/// so by the time this runs there is already one client. Resolving a second is the whole
/// assertion: before Part C `auth-service` minted one channel pair at startup and handed the
/// only client end to `session-mgr`, so this could not have succeeded however it was asked.
///
/// **Without this the claim rides on `desktop-session-mgr`, which does not exist until Part
/// D.** A capability specified, reasoned about, and reachable by nobody is exactly what the
/// title cap in PR #233 turned out to be, and the fix there was to test the path rather than
/// the component. This is that, one part earlier.
fn auth_multi_client_test(root_ns: u64) -> bool {
    let (st, ch) = ns_lookup(root_ns, b"/svc/auth", libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT);
    if st != 0 || ch == 0 {
        kprint(b"boot-probe: /svc/auth second session FAIL\n");
        return false;
    }
    // A second resolve, so the answer is not "one spare slot" but "it mints per caller".
    let (st2, ch2) = ns_lookup(root_ns, b"/svc/auth", libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT);
    // SAFETY: closing handles this process owns.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, ch);
        if ch2 != 0 {
            syscall1(SYS_HANDLE_CLOSE, ch2);
        }
    }
    // **The count is the assertion, not the handle values.** Two *successful* resolves while
    // `session-mgr` already holds one means three concurrent sessions, which one spare slot
    // cannot supply. Comparing `ch2 == ch` would be decoration: the probe holds `ch` open
    // across the second resolve and `HandleTable::allocate` never re-issues a live slot, so
    // the numbers differ whatever the server did (PR #235 review, finding 4).
    if st2 != 0 || ch2 == 0 {
        kprint(b"boot-probe: /svc/auth third session FAIL (not minted per caller)\n");
        return false;
    }
    kprint(b"boot-probe: /svc/auth mints a session per caller (session-mgr + 2) ok\n");
    true
}

/// Prove `sys_ns_derive` through the syscall, not only through `Namespace::try_derive`, whose host
/// tests cannot see the rights check, the handle the syscall allocates, or the rights it carries
/// (administration Part A.1).
///
/// Four claims, each one a view depends on:
/// 1. the copy resolves what its source resolves;
/// 2. it can be sent — `TRANSFER` — which the `LOOKUP`-only root this process was spawned with
///    cannot, and it can be pruned, `UNBIND`;
/// 3. pruning the copy leaves the source alone, the snapshot rule seen from the side that matters;
/// 4. a namespace handle without `LOOKUP` cannot be copied.
///
/// `/subtreetest` is the test image's own `[[bind]]`, which `subtree_bind_test` above already
/// resolves, so this borrows a binding that is known to be there rather than adding one.
fn ns_derive_test(root_ns: u64) -> bool {
    const PATH: &[u8] = b"/subtreetest/current-generation";
    // SAFETY: a namespace handle this process holds.
    let d = unsafe { syscall1(SYS_NS_DERIVE, root_ns) };
    if d <= 0 {
        Line::new().s(b"boot-probe: ns derive FAIL (").i(d as i64).s(b")").end();
        return false;
    }
    let d = d as u64;
    let close = |h: u64| {
        if h != 0 {
            // SAFETY: closing a handle this process owns.
            unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
        }
    };
    let mut ok = true;

    let (st, fh) = ns_lookup(d, PATH, RIGHT_MAP_READ);
    close(fh);
    if st != 0 || fh == 0 {
        kprint(b"boot-probe: ns derive: the copy does not resolve what the root does FAIL\n");
        ok = false;
    }

    let mut info = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
    // SAFETY: `info` is a writable 24-byte `HandleInfo`, the layout the kernel writes.
    let sr = unsafe { syscall2(SYS_HANDLE_STAT, d, (&raw mut info) as u64) };
    let want = RIGHT_TRANSFER | RIGHT_UNBIND;
    if sr != 0 || info.rights & want != want {
        kprint(b"boot-probe: ns derive: the copy cannot be sent or pruned FAIL\n");
        ok = false;
    }

    let bind = b"/subtreetest";
    // SAFETY: a namespace handle this process holds, and a valid path.
    let ur = unsafe { syscall4(SYS_NS_UNBIND, d, bind.as_ptr() as u64, bind.len() as u64, 0) };
    let (cst, cfh) = ns_lookup(d, PATH, RIGHT_MAP_READ);
    let (rst, rfh) = ns_lookup(root_ns, PATH, RIGHT_MAP_READ);
    close(cfh);
    close(rfh);
    if ur != 0 || cst == 0 {
        kprint(b"boot-probe: ns derive: unbinding in the copy did not take FAIL\n");
        ok = false;
    }
    if rst != 0 {
        kprint(b"boot-probe: ns derive: unbinding in the copy reached the root FAIL\n");
        ok = false;
    }

    // A handle to the same copy with everything but `LOOKUP`.
    // SAFETY: `d` carries `DUPLICATE`; the result is a handle this process owns.
    let blind = unsafe { syscall2(SYS_HANDLE_DUPLICATE, d, !RIGHT_LOOKUP) };
    // SAFETY: as above.
    let refused = blind > 0 && unsafe { syscall1(SYS_NS_DERIVE, blind as u64) } < 0;
    if blind > 0 {
        close(blind as u64);
    }
    if !refused {
        kprint(b"boot-probe: ns derive: copied a handle without LOOKUP FAIL\n");
        ok = false;
    }

    close(d);
    if ok {
        kprint(b"boot-probe: ns derive: a sendable snapshot, pruned without touching the root ok\n");
    }
    ok
}

/// The demo account the build seeds, and the policy it seeds for it (`xtask`'s `DEMO_USER` and
/// `DEMO_PASSWORD`, and `seeded_views_toml`). A build input, not a secret — `init`'s login
/// selftest used the same literals.
const DEMO_USER: &[u8] = b"alice";
const DEMO_PASSWORD: &[u8] = b"correct horse battery staple";

fn clock_ns() -> u64 {
    let mut t = 0u64;
    // SAFETY: `t` is a valid writable u64 out-param.
    unsafe { syscall2(libkern::SYS_CLOCK_READ, libkern::abi::CLOCK_MONOTONIC, (&raw mut t) as u64) };
    t
}

/// One exchange with the view broker: send `op` on `ch` carrying `body` and moving `handles`, and
/// wait — at most ten seconds — for the reply to it. An `Exited` that arrives first is set aside in
/// `exited` rather than mistaken for the reply. `None` if nothing came.
fn views_call(
    ch: u64,
    op: u16,
    request_id: u64,
    body: &[u8],
    handles: &[u64],
    exited: &mut alloc::vec::Vec<alloc::vec::Vec<u8>>,
) -> Option<(bool, alloc::vec::Vec<u8>)> {
    if !rs_send(ch, op, request_id, body, handles) {
        return None;
    }
    views_receive(ch, request_id, exited)
}

/// Send one rsproto message on `ch`, moving `handles`, without waiting for an answer — to the view
/// broker, or down an endpoint as the kernel would forward a resolve.
fn rs_send(ch: u64, op: u16, request_id: u64, body: &[u8], handles: &[u64]) -> bool {
    let mut msg = [0u8; 4096];
    let Some(n) = librsproto::encode(&mut msg[24..], op, request_id, 0, body, handles.len() as u16) else {
        return false;
    };
    msg[4..8].copy_from_slice(&(n as u32).to_le_bytes());
    msg[8] = handles.len() as u8;
    // SAFETY: valid message buffer and handle array.
    let sr = unsafe {
        syscall5(
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

/// Wait for the message with `request_id` on `ch` (`0` for the next `Exited`), setting any other
/// `Exited` aside in `exited`. Ten seconds at most.
fn views_receive(
    ch: u64,
    request_id: u64,
    exited: &mut alloc::vec::Vec<alloc::vec::Vec<u8>>,
) -> Option<(bool, alloc::vec::Vec<u8>)> {
    let deadline = clock_ns() + 10_000_000_000;
    loop {
        let m = receive(ch, deadline)?;
        if m.op == librsproto::views::OP_VIEWS_EXITED && m.request_id == 0 {
            if request_id == 0 {
                return Some((false, m.body));
            }
            exited.push(m.body);
            continue;
        }
        if m.request_id == request_id {
            return Some((m.error, m.body));
        }
    }
}

/// One rsproto message, received and copied out, with the handles that came with it.
struct Received {
    op: u16,
    request_id: u64,
    error: bool,
    body: alloc::vec::Vec<u8>,
    handles: alloc::vec::Vec<u64>,
}

/// The next message on `ch`, waiting until `deadline` on the monotonic clock — `0` only looks.
/// `None` if nothing came by then, or if what came is not rsproto.
fn receive(ch: u64, deadline: u64) -> Option<Received> {
    let mut buf = [0u8; 4096];
    let mut hs = [0u64; 8];
    loop {
        let mut count = 0usize;
        // SAFETY: valid recv out-params.
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
            let m = librsproto::decode(&buf[24..24 + len.min(4096 - 24)]).ok()?;
            return Some(Received {
                op: m.op,
                request_id: m.request_id,
                error: m.is_error(),
                body: m.body.to_vec(),
                handles: hs[..count.min(hs.len())].to_vec(),
            });
        }
        if clock_ns() >= deadline {
            return None;
        }
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter, with a deadline.
        unsafe {
            WAIT_HANDLES[0] = ch;
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, deadline);
        }
    }
}

/// What the kernel says of handle `h`. `None` if it is not one this process holds.
fn stat(h: u64) -> Option<HandleInfo> {
    let mut info = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
    // SAFETY: `info` is a writable 24-byte `HandleInfo`, the layout the kernel writes.
    let sr = unsafe { syscall2(SYS_HANDLE_STAT, h, (&raw mut info) as u64) };
    (sr == 0).then_some(info)
}

/// Map a read-only object and copy it out, so nothing borrows the mapping past this call.
fn read_all(h: u64) -> Option<alloc::vec::Vec<u8>> {
    let size = stat(h)?.size;
    // SAFETY: register-only syscall; `h` is a MemoryObject handle with MAP_READ.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, h, 0, size, RIGHT_MAP_READ) };
    if addr < 0 {
        return None;
    }
    // SAFETY: `size` bytes are mapped read-only at `addr` until the unmap below.
    let bytes = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, size as usize) }.to_vec();
    // SAFETY: unmapping what was mapped above; `bytes` is a copy.
    unsafe { syscall2(SYS_MEMORY_UNMAP, addr as u64, 0) };
    Some(bytes)
}

/// Sleep `ms` milliseconds on a one-shot timer — `sys_wait` refuses an empty handle list, so a
/// deadline alone is not a sleep. Returns at once if no timer can be made.
fn sleep_ms(ms: u64) {
    // SAFETY: register-only syscall; returns a handle or a negative KError.
    let th = unsafe { syscall1(libkern::SYS_TIMER_CREATE, 0) };
    if th < 0 {
        return;
    }
    let fire_at = clock_ns() + ms * 1_000_000;
    // SAFETY: arming this process's own timer, one-shot at an absolute monotonic time.
    unsafe { syscall4(libkern::SYS_TIMER_SET, th as u64, fire_at, 0, 0) };
    wait_one(th as u64);
    close(th as u64);
}

/// Close `h` if it is a handle at all.
fn close(h: u64) {
    if h != 0 {
        // SAFETY: closing a handle this process holds.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
    }
}

/// `/dev/registry`'s records, read through `ns`. `None` if it is not bound there or does not read.
fn registry_records(ns: u64) -> Option<alloc::vec::Vec<libkern::device::DeviceRecord>> {
    let (st, snap) = ns_lookup(ns, b"/dev/registry", RIGHT_MAP_READ | libkern::RIGHT_INSPECT);
    if st != 0 || snap == 0 {
        return None;
    }
    let bytes = read_all(snap);
    close(snap);
    Some(libkern::device::records(&bytes?).ok()?.collect())
}

/// **The view broker, through its own protocol** (administration Part A.3), before any shell or
/// `with` exists to drive it. `boot-probe` holds the unscoped root namespace, so it can be both a
/// supervisor — opening a session for the demo account — and a client in that session, at
/// `/svc/views/s/<id>`, which is exactly what a session's `/dev/views` reaches.
///
/// What it proves, in the order it runs, each a thing a later piece depends on:
/// 1. a request the policy allows asks for a password;
/// 2. a wrong password is refused with a retry, and the right one — offered straight after — is
///    **held for the session's delay** before it is answered;
/// 3. **the grant arrived**: `nxinstall` is sent a copy of this namespace with `/dev/blk`
///    removed, and still exits 0, which is "listed the devices it can see" — its 1 would be
///    "none";
/// 4. a client the broker lets in can start its program with its exit heard, even when the
///    broker has let in all it can;
/// 5. **one guess per delay, however many requests**: passwords queued on two requests during
///    one delay are answered a delay apart, not together when it ends;
/// 6. a program outside a rule's `run` is refused by the policy;
/// 7. `with --check`'s op refuses a policy nobody could administer, and a handle sent with an op
///    that takes none is closed rather than kept;
/// 8. once the session is closed, its base names nothing.
fn view_broker_test(root_ns: u64) -> bool {
    use librsproto::views::*;
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: view broker: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT;
    let mut exited = alloc::vec::Vec::new();

    let (st, sup) = ns_lookup(root_ns, b"/svc/views/session", chan);
    if st != 0 || sup == 0 {
        return fail(b"no supervisor channel at /svc/views/session");
    }
    let session = match views_call(sup, OP_VIEWS_OPEN_SESSION, 1, DEMO_USER, &[], &mut exited) {
        Some((false, body)) => match parse_session_id(&body) {
            Some(id) => id,
            None => return fail(b"OpenSession's reply"),
        },
        _ => return fail(b"OpenSession"),
    };
    let client_path = alloc::format!("/svc/views/s/{session}");
    let (st, cli) = ns_lookup(root_ns, client_path.as_bytes(), chan);
    if st != 0 || cli == 0 {
        return fail(b"no client channel at the session's base");
    }

    // A copy of this namespace with every disk removed, so a 0 from `nxinstall` can only be the
    // grant's doing.
    // SAFETY: a namespace handle this process holds.
    let copy = unsafe { syscall1(SYS_NS_DERIVE, root_ns) };
    if copy <= 0 {
        return fail(b"derive");
    }
    let copy = copy as u64;
    let blk = b"/dev/blk";
    // SAFETY: a namespace handle this process holds, and a valid path.
    unsafe { syscall4(SYS_NS_UNBIND, copy, blk.as_ptr() as u64, blk.len() as u64, 0) };
    let (bst, bh) = ns_lookup(copy, b"/dev/blk/0", libkern::RIGHT_READ);
    if bst == 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, bh) };
        return fail(b"precondition: the copy still reaches /dev/blk/0");
    }

    // **Keep a duplicate of what is sent**, the way a caller hoping to reach the view would: the
    // broker must build the view in a namespace of its own, so nothing it grants reaches this.
    // SAFETY: `copy` carries `DUPLICATE`; the result is a handle this process owns.
    let kept = unsafe { syscall2(SYS_HANDLE_DUPLICATE, copy, u64::MAX) };
    if kept <= 0 {
        return fail(b"duplicate the copy");
    }
    let kept = kept as u64;

    let mut req = [0u8; 256];
    let Some(n) = build_request(&mut req, 0, b"admin", b"nxinstall", &[], b"") else {
        return fail(b"build a request");
    };
    match views_call(cli, OP_VIEWS_REQUEST, 2, &req[..n], &[copy], &mut exited) {
        Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::NeedPassword, _))) => {}
        _ => return fail(b"an allowed request did not ask for a password"),
    }
    match views_call(cli, OP_VIEWS_PASSWORD, 3, b"not the password", &[], &mut exited) {
        Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::Denied { retry: true }, _))) => {}
        _ => return fail(b"a wrong password was not refused with a retry"),
    }
    let refused_at = clock_ns();
    match views_call(cli, OP_VIEWS_PASSWORD, 4, DEMO_PASSWORD, &[], &mut exited) {
        Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::Started, _))) => {}
        _ => return fail(b"the right password did not start the program"),
    }
    // **Held, not merely slow.** The delay is two seconds; the scheduler's tick is ten
    // milliseconds, so anything past 1.9 s is the broker holding the check, and an answer much
    // sooner is a broker that did not.
    let held = clock_ns().saturating_sub(refused_at);
    if held < 1_900_000_000 {
        return fail(b"the password after a wrong one was answered without the session's delay");
    }
    // The program is running with its grant; the namespace this process sent must not have it.
    let (kst, kh) = ns_lookup(kept, b"/dev/blk/0", libkern::RIGHT_READ);
    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, kept);
        if kh != 0 {
            syscall1(SYS_HANDLE_CLOSE, kh);
        }
    }
    if kst == 0 {
        return fail(b"the grant was bound into the namespace the caller sent, which it still holds");
    }
    let code = match exited.pop().or_else(|| views_receive(cli, 0, &mut exited).map(|r| r.1)) {
        Some(body) => parse_exited(&body),
        None => None,
    };
    if code != Some((0, false)) {
        Line::new().s(b"boot-probe: view broker: nxinstall exited ").i(code.map_or(-99, |c| c.0 as i64)).end();
        return fail(b"the program did not see the granted disks");
    }
    // SAFETY: closing our own handle; the request is done.
    unsafe { syscall1(SYS_HANDLE_CLOSE, cli) };

    // 4. **A client let in can always start its program.** Open clients until the broker refuses
    // one, then start a program on the last it let in: that program's exit must still be heard.
    // Admitting by what was open let the last channel into the last slot, and its program's life
    // channel then had none — an exit never seen (PR #329 review, finding 5).
    let mut filled = alloc::vec::Vec::new();
    let refused = loop {
        let (st, ch) = ns_lookup(root_ns, client_path.as_bytes(), chan);
        if st != 0 || ch == 0 {
            break st;
        }
        filled.push(ch);
        if filled.len() > libkern::MAX_WAIT_HANDLES {
            return fail(b"the broker never refused a client");
        }
    };
    if refused != libkern::KError::WouldBlock.as_i32() {
        Line::new().s(b"boot-probe: view broker: client ").u(filled.len() as u64 + 1).s(b" refused with ").i(refused as i64).end();
        return fail(b"a client was refused for something other than room");
    }
    let Some(&last) = filled.last() else {
        return fail(b"no client was let in");
    };
    // SAFETY: a namespace handle this process holds.
    let copy = unsafe { syscall1(SYS_NS_DERIVE, root_ns) };
    let Some(n) = build_request(&mut req, 0, b"admin", b"nxinstall", &[], b"") else {
        return fail(b"build a request");
    };
    if copy <= 0 || !matches!(views_call(last, OP_VIEWS_REQUEST, 8, &req[..n], &[copy as u64], &mut exited), Some((false, _))) {
        return fail(b"the last client's request");
    }
    match views_call(last, OP_VIEWS_PASSWORD, 9, DEMO_PASSWORD, &[], &mut exited) {
        Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::Started, _))) => {}
        _ => return fail(b"the last client's program did not start"),
    }
    let heard = exited.pop().or_else(|| views_receive(last, 0, &mut exited).map(|r| r.1));
    if heard.as_deref().and_then(parse_exited).is_none() {
        Line::new().s(b"boot-probe: view broker: ").u(filled.len() as u64).s(b" clients let in").end();
        return fail(b"the exit of the last client's program was never heard");
    }
    for ch in filled {
        // SAFETY: closing our own handles.
        unsafe { syscall1(SYS_HANDLE_CLOSE, ch) };
    }

    // 5. Two more requests, `b` and `c`. `b` fails; then a password goes on each before the
    // delay ends. Both are held — and when the first of them fails too, it holds the second
    // (PR #329 review, blocking finding 1: they were once checked back to back).
    let mut pair = [0u64; 2];
    for (k, slot) in pair.iter_mut().enumerate() {
        let (st, ch) = ns_lookup(root_ns, client_path.as_bytes(), chan);
        // SAFETY: a namespace handle this process holds.
        let copy = unsafe { syscall1(SYS_NS_DERIVE, root_ns) };
        if st != 0 || ch == 0 || copy <= 0 {
            return fail(b"a client channel and a copy for the paced pair");
        }
        let Some(n) = build_request(&mut req, 0, b"admin", b"nxinstall", &[], b"") else {
            return fail(b"build a request");
        };
        match views_call(ch, OP_VIEWS_REQUEST, 10 + k as u64, &req[..n], &[copy as u64], &mut exited) {
            Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::NeedPassword, _))) => {}
            _ => return fail(b"the paced pair's request did not ask for a password"),
        }
        *slot = ch;
    }
    let [b, c] = pair;
    let retry = |r: Option<(bool, alloc::vec::Vec<u8>)>| {
        matches!(r, Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::Denied { retry: true }, _))))
    };
    if !retry(views_call(b, OP_VIEWS_PASSWORD, 20, b"wrong", &[], &mut exited)) {
        return fail(b"the paced pair's first wrong password");
    }
    let t0 = clock_ns();
    if !rs_send(b, OP_VIEWS_PASSWORD, 21, b"wrong again", &[]) || !rs_send(c, OP_VIEWS_PASSWORD, 22, b"wrong", &[]) {
        return fail(b"send the paced pair's passwords");
    }
    if !retry(views_receive(b, 21, &mut exited)) {
        return fail(b"the first held password's answer");
    }
    let t1 = clock_ns();
    if !retry(views_receive(c, 22, &mut exited)) {
        return fail(b"the second held password's answer");
    }
    let t2 = clock_ns();
    if t1.saturating_sub(t0) < 1_900_000_000 || t2.saturating_sub(t1) < 1_900_000_000 {
        Line::new().s(b"boot-probe: view broker: held answers after ").u((t1 - t0) / 1_000_000).s(b" ms and ").u(t2.saturating_sub(t1) / 1_000_000).s(b" ms").end();
        return fail(b"two passwords held in one delay were checked together");
    }
    // SAFETY: closing our own handles; the broker drops what they asked for.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, b);
        syscall1(SYS_HANDLE_CLOSE, c);
    }

    // A second client in the same session: a program the `install` view does not include.
    let (st, cli2) = ns_lookup(root_ns, client_path.as_bytes(), chan);
    if st != 0 || cli2 == 0 {
        return fail(b"a second client channel");
    }
    // SAFETY: a namespace handle this process holds.
    let copy2 = unsafe { syscall1(SYS_NS_DERIVE, root_ns) };
    let Some(n) = build_request(&mut req, 0, b"install", b"nxsh", &[], b"") else {
        return fail(b"build a request");
    };
    match views_call(cli2, OP_VIEWS_REQUEST, 5, &req[..n], &[copy2 as u64], &mut exited) {
        Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::Denied { retry: false }, _))) => {}
        _ => return fail(b"a program outside the rule's `run` was not refused"),
    }
    let orphaned = b"[profile.admin]\ngrants = [\"disks\"]\n";
    match views_call(cli2, OP_VIEWS_CHECK, 6, orphaned, &[], &mut exited) {
        Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::Denied { .. }, _))) => {}
        _ => return fail(b"a policy nobody could administer passed the check"),
    }
    // **A handle sent with anything but a request is closed.** Each op carries one end of a new
    // channel; the broker closing it is what the other end sees as `PeerClosed` — a leak would
    // leave it open, and any program in any session can send handles (PR #329 review, finding 4).
    for (k, op) in [OP_VIEWS_STOP, OP_VIEWS_LIST, OP_VIEWS_CHECK].into_iter().enumerate() {
        let (mut ours, mut theirs) = (0u64, 0u64);
        // SAFETY: valid writable out-params.
        let r = unsafe { syscall4(libkern::SYS_CHANNEL_CREATE, (&raw mut ours) as u64, (&raw mut theirs) as u64, 1, 0) };
        if r != 0 {
            return fail(b"a channel to send the broker");
        }
        if views_call(cli2, op, 30 + k as u64, b"", &[theirs], &mut exited).is_none() {
            return fail(b"no answer to an op carrying a handle");
        }
        let mut buf = [0u8; 64];
        let mut hs = [0u64; 1];
        let mut count = 0usize;
        // SAFETY: valid recv out-params; `ours` is this process's.
        let rr = unsafe {
            let rr = syscall4(libkern::SYS_CHANNEL_RECV, ours, buf.as_mut_ptr() as u64, hs.as_mut_ptr() as u64, (&raw mut count) as u64);
            syscall1(SYS_HANDLE_CLOSE, ours);
            rr
        };
        if rr != libkern::KError::PeerClosed.as_i32() as i64 {
            Line::new().s(b"boot-probe: view broker: op ").u(op as u64).s(b" kept the handle sent with it").end();
            return fail(b"a handle sent with an op that takes none was kept");
        }
    }
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, cli2) };

    let mut id = [0u8; 8];
    let n = build_session_id(&mut id, session).unwrap_or(0);
    if !matches!(views_call(sup, OP_VIEWS_CLOSE_SESSION, 7, &id[..n], &[], &mut exited), Some((false, _))) {
        return fail(b"CloseSession");
    }
    let (st, gone) = ns_lookup(root_ns, client_path.as_bytes(), chan);
    if st == 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, gone) };
        return fail(b"a closed session's base still resolves");
    }
    // `NotFound`, not merely a failure: an error body the kernel cannot read is `KernelError`.
    if st != libkern::KError::NotFound.as_i32() {
        Line::new().s(b"boot-probe: view broker: a closed session's base answered ").i(st as i64).end();
        return fail(b"a closed session's base was refused for something other than being gone");
    }
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, sup) };
    kprint(b"boot-probe: view broker: password held, one per delay, grant arrived, exit heard when full, policy refused, stray handles closed, session closed ok\n");
    true
}


/// **The device registry, through its binding** (administration Part B.1). The kernel's host tests
/// see its table as a value; this sees `/dev/registry` as a process does, and holds it to the
/// paths it has to agree with:
/// - the snapshot decodes, through the reader everything else will use;
/// - **its block records are exactly what probing `/dev/blk` finds** — the same number, and each
///   record's served index resolving to a device of the record's size, whose `info` gives the
///   record's name;
/// - **the keyboard is served at 0 and the mouse at 1**, each resolving under `/dev/input/raw`;
/// - every record's `/dev/registry/<id>` is a device node, **and its id is its place** — which a
///   phantom record read past the count, all zeros, cannot be.
///
/// The paths and the records read one field in the kernel, so a disagreement here would be a new
/// path that stopped reading it.
fn registry_test(root_ns: u64) -> bool {
    use libkern::device::DeviceKind;
    use libkern::{RIGHT_INSPECT, RIGHT_READ};
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: registry: ").s(what).s(b" FAIL").end();
        false
    };

    let Some(all) = registry_records(root_ns) else {
        return fail(b"no /dev/registry in the root namespace that reads");
    };
    let total = all.len();
    let mut blocks = 0u32;
    let (mut keyboard, mut mouse) = (false, false);
    for (place, r) in all.iter().enumerate() {
        if r.id as usize != place {
            Line::new().s(b"boot-probe: registry: record ").u(place as u64).s(b" says it is ").u(r.id as u64).end();
            return fail(b"the records are not the table in order");
        }
        let path = alloc::format!("/dev/registry/{}", r.id);
        let (st, node) = ns_lookup(root_ns, path.as_bytes(), RIGHT_READ | RIGHT_INSPECT);
        let kind_ok = st == 0 && stat(node).is_some_and(|i| i.object_type == libkern::KObjectType::DeviceNode as u32);
        close(node);
        if !kind_ok {
            Line::new().s(b"boot-probe: registry: ").s(path.as_bytes()).s(b" is not a device node").end();
            return fail(b"a record's id does not resolve to its node");
        }
        match r.kind() {
            DeviceKind::Disk | DeviceKind::Partition | DeviceKind::RamDisk => {
                blocks += 1;
                let dev = alloc::format!("/dev/blk/{}", r.served);
                let (st, h) = ns_lookup(root_ns, dev.as_bytes(), RIGHT_READ | RIGHT_INSPECT);
                let size = if st == 0 { stat(h).map(|i| i.size) } else { None };
                close(h);
                let want = r.logical_block_size as u64 * r.block_count;
                if size != Some(want) {
                    Line::new().s(b"boot-probe: registry: ").s(dev.as_bytes()).s(b" is not the record's size").end();
                    return fail(b"a block record's served index names another device");
                }
                let info_path = alloc::format!("/dev/blk/{}/info", r.served);
                let (st, ih) = ns_lookup(root_ns, info_path.as_bytes(), RIGHT_MAP_READ | RIGHT_INSPECT);
                let info = if st == 0 { read_all(ih) } else { None };
                close(ih);
                // `BlockDeviceInfo`: `name_len` at 16, `name` from 24.
                let named = info.is_some_and(|b| {
                    let n = u32::from_le_bytes([b[16], b[17], b[18], b[19]]) as usize;
                    b.get(24..24 + n) == Some(r.name())
                });
                if !named {
                    return fail(b"a block record's name is not its device's");
                }
            }
            DeviceKind::Keyboard | DeviceKind::Mouse => {
                let want = if r.kind() == DeviceKind::Keyboard { 0 } else { 1 };
                let raw = alloc::format!("/dev/input/raw/{}", r.served);
                let (st, h) = ns_lookup(root_ns, raw.as_bytes(), RIGHT_READ | RIGHT_INSPECT);
                close(h);
                if r.served != want || st != 0 {
                    return fail(b"an input record is not at its raw index");
                }
                if r.kind() == DeviceKind::Keyboard {
                    keyboard = true;
                } else {
                    mouse = true;
                }
            }
            _ => {}
        }
    }
    // What the probes find, the old way.
    let mut probed = 0u32;
    loop {
        let dev = alloc::format!("/dev/blk/{probed}");
        let (st, h) = ns_lookup(root_ns, dev.as_bytes(), RIGHT_READ | RIGHT_INSPECT);
        close(h);
        if st != 0 {
            break;
        }
        probed += 1;
    }
    if probed != blocks {
        Line::new().s(b"boot-probe: registry: ").u(blocks as u64).s(b" block records, ").u(probed as u64).s(b" probed").end();
        return fail(b"the block records are not what /dev/blk serves");
    }
    if !keyboard || !mouse {
        return fail(b"no keyboard and mouse records");
    }
    Line::new()
        .s(b"boot-probe: registry: ")
        .u(total as u64)
        .s(b" nodes, ")
        .u(blocks as u64)
        .s(b" block devices as /dev/blk serves them, keyboard and mouse at their raw indices ok")
        .end();
    true
}

/// **The device manager, through its own paths** (administration Part B.2), before any owner but
/// `input-server` exists. `block` has no owner until Part C's storage service, so the probe takes
/// it:
/// - **the subscription replays every block device as `Arrived`, each with its node, then
///   `Settled`** with the count — the same devices `/dev/registry` lists as block;
/// - **a second subscription is refused while the first is held**, and taken once it is closed —
///   one owner per class, the kernel's one reader per device kept at the manager;
/// - **`info` lists `all.tsm` and a file per device, and `all.tsm` is a table** with a row per
///   device the registry has;
/// - **`input` is refused, because `input-server` holds it** (Part B.3) — which is how a probe
///   sees that the input server took its devices from the manager rather than from the raw paths.
///   Were it not held, this resolve would take the class for a moment and give it back;
/// - **the info-only endpoint answers the tables and nothing else** (Part B.4): `block` and
///   another `info-endpoint` are `NotFound` on it, where the root endpoint subscribes and mints,
///   and `info` opens the directory.
fn devices_test(root_ns: u64) -> bool {
    use libkern::device::DeviceKind;
    use librsproto::devices::{OP_DEVICES_ARRIVED, OP_DEVICES_SETTLED, parse_arrived, parse_settled};
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: devices: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT;

    // What the registry says, to hold the manager to.
    let Some(registry) = registry_records(root_ns) else {
        return fail(b"the registry does not read");
    };
    let block_ids: alloc::vec::Vec<u32> = registry
        .iter()
        .filter(|r| matches!(r.kind(), DeviceKind::Disk | DeviceKind::Partition | DeviceKind::RamDisk))
        .map(|r| r.id)
        .collect();

    // A subscription's replay: the ids that arrived, each with a device node, and what `Settled`
    // counted. **Read without waiting**, because the manager queues all of it before the resolve
    // completes. This states the property rather than guarding it: a manager that replied first
    // and sent after passes whenever its sends beat this process's wake, as they did in the boot
    // that tried it. `subscribe`'s order is what holds it.
    let read_replay = |owner: u64| -> Result<(alloc::vec::Vec<u32>, Option<u32>), &'static [u8]> {
        let mut arrived = alloc::vec::Vec::new();
        loop {
            let Some(m) = receive(owner, 0) else {
                return Err(b"the replay was not queued when the subscription completed");
            };
            let node_ok = m.handles.len() == 1
                && stat(m.handles[0]).is_some_and(|i| i.object_type == libkern::KObjectType::DeviceNode as u32);
            m.handles.iter().for_each(|&h| close(h));
            if m.op == OP_DEVICES_SETTLED {
                return Ok((arrived, parse_settled(&m.body)));
            }
            if m.op != OP_DEVICES_ARRIVED || !node_ok {
                return Err(b"a replay message was not an arrival carrying a device node");
            }
            let Some(rec) = parse_arrived(&m.body) else {
                return Err(b"an arrival's body is not a record");
            };
            arrived.push(u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]));
        }
    };
    let replayed_the_registry = |owner: u64| match read_replay(owner) {
        Ok((arrived, settled)) if arrived == block_ids && settled == Some(block_ids.len() as u32) => Ok(()),
        Ok((arrived, _)) => {
            Line::new().s(b"boot-probe: devices: ").u(arrived.len() as u64).s(b" arrived, ").u(block_ids.len() as u64).s(b" block records").end();
            Err(&b"the replay is not the registry's block devices"[..])
        }
        Err(what) => Err(what),
    };

    // Subscribe, and read the replay.
    let (st, owner) = ns_lookup(root_ns, b"/svc/devices/block", chan);
    if st != 0 || owner == 0 {
        return fail(b"/svc/devices/block would not subscribe");
    }
    if let Err(what) = replayed_the_registry(owner) {
        close(owner);
        return fail(what);
    }

    // One owner at a time.
    let (st, second) = ns_lookup(root_ns, b"/svc/devices/block", chan);
    close(second);
    if st != libkern::KError::AlreadyExists.as_i32() {
        close(owner);
        Line::new().s(b"boot-probe: devices: a second subscription answered ").i(st as i64).end();
        return fail(b"a second owner was not refused");
    }
    close(owner);
    // The manager notices the close in its own time; a subscription sent before it has is refused
    // like any other, so ask a few times.
    let mut retaken = 0;
    for _ in 0..50 {
        let (st, again) = ns_lookup(root_ns, b"/svc/devices/block", chan);
        if st == 0 {
            retaken = again;
            break;
        }
        sleep_ms(20);
    }
    if retaken == 0 {
        return fail(b"the class was not taken again once its owner closed");
    }
    // A new owner is sent the whole class again, not what the last one left.
    let again = replayed_the_registry(retaken);
    close(retaken);
    if let Err(what) = again {
        return fail(what);
    }

    // `input` has its owner from boot on: `init` waits for `input-server`'s `Ready`, which comes
    // only after it has subscribed and settled.
    let (st, input) = ns_lookup(root_ns, b"/svc/devices/input", chan);
    close(input);
    if st != libkern::KError::AlreadyExists.as_i32() {
        Line::new().s(b"boot-probe: devices: a subscription to input answered ").i(st as i64).end();
        return fail(b"input is not held, so input-server did not take its devices from the manager");
    }

    // **The endpoint a session is given answers the tables and nothing else** (Part B.4). `init`
    // couriers one of these for every session's `/dev/devices`, and `desktop-shell` holds it with
    // `BIND_NAMESPACE`, so it could bind it with no base — where the root endpoint would take
    // `block` as a subscription to every disk. The probe cannot bind (it holds no syscaps), but a
    // forwarding endpoint is a channel and the manager answers whatever resolve arrives on it, so
    // the probe sends the resolves a namespace would forward, as the kernel would.
    let (st, endpoint) = ns_lookup(root_ns, b"/svc/devices/info-endpoint", chan);
    if st != 0 || endpoint == 0 {
        return fail(b"no info-only endpoint at /svc/devices/info-endpoint");
    }
    let forward = |request_id: u64, suffix: &[u8]| -> Option<Received> {
        let mut body = [0u8; 64];
        let n = librsproto::namespace::resolve_request(&mut body, chan, 0, suffix)?;
        if !rs_send(endpoint, librsproto::OP_NS_RESOLVE, request_id, &body[..n], &[]) {
            return None;
        }
        let deadline = clock_ns() + 5_000_000_000;
        loop {
            let m = receive(endpoint, deadline)?;
            if m.request_id == request_id {
                return Some(m);
            }
            m.handles.iter().for_each(|&h| close(h));
        }
    };
    let refused = |m: &Option<Received>| {
        m.as_ref().is_some_and(|m| {
            m.error
                && m.handles.is_empty()
                && librsproto::error::parse_error(&m.body)
                    .is_some_and(|e| e.kerror == libkern::KError::NotFound.as_i32())
        })
    };
    let block = forward(1, b"block");
    let minted = forward(2, b"info-endpoint");
    let listing = forward(3, b"info");
    close(endpoint);
    for m in [&block, &minted, &listing] {
        if let Some(m) = m {
            m.handles.iter().for_each(|&h| close(h));
        }
    }
    if !refused(&block) {
        return fail(b"the info-only endpoint answered `block` as something other than NotFound");
    }
    if !refused(&minted) {
        return fail(b"the info-only endpoint minted another");
    }
    if !listing.as_ref().is_some_and(|m| !m.error && m.handles.len() == 1) {
        return fail(b"the info-only endpoint would not open its directory");
    }

    // The information side.
    let mut dirbuf = alloc::vec![0u8; libkern::abi::IPC_MSG_SIZE];
    let Ok(mut dir) = librsproto::session::Dir::open(root_ns, b"/svc/devices/info", &mut dirbuf) else {
        return fail(b"/svc/devices/info is not a directory");
    };
    let mut names = 0usize;
    let mut has_all = false;
    let listed = dir.read_dir(|e| {
        if e.name != b"." && e.name != b".." {
            names += 1;
            has_all |= e.name == b"all.tsm";
        }
        true
    });
    dir.close();
    if listed.is_err() {
        return fail(b"the directory would not list");
    }
    if !has_all || names != registry.len() + 1 {
        Line::new().s(b"boot-probe: devices: ").u(names as u64).s(b" entries for ").u(registry.len() as u64).s(b" devices").end();
        return fail(b"the directory is not all.tsm and a file per device");
    }
    let (st, table) = ns_lookup(root_ns, b"/svc/devices/info/all.tsm", RIGHT_MAP_READ | libkern::RIGHT_INSPECT);
    let tbytes = if st == 0 { read_all(table) } else { None };
    close(table);
    let Some(tbytes) = tbytes else {
        return fail(b"all.tsm would not map");
    };
    // The object is page-sized; the decoder stops at the table's terminator.
    let rows = match libstream::wire::Table::decode(&tbytes) {
        Ok(t) => t.rows.len(),
        Err(_) => return fail(b"all.tsm is not a TSM1 table"),
    };
    if rows != registry.len() {
        return fail(b"all.tsm has not a row per device");
    }
    Line::new()
        .s(b"boot-probe: devices: block replayed ")
        .u(block_ids.len() as u64)
        .s(b" and settled before the resolve completed, a second owner refused, taken and replayed again once closed, input held by input-server, the info-only endpoint refusing block, all.tsm has ")
        .u(rows as u64)
        .s(b" rows ok")
        .end();
    true
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"boot-probe: PANIC\n");
    // A panicking probe must not let the run pass by defaulting to silence.
    verdict(false);
    exit(1);
}
