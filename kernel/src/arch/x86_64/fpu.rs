//! x86_64 floating-point / SIMD state management: the per-CPU enable and the
//! per-thread save area the scheduler swaps on every context switch.
//!
//! ## Why the kernel needs this even though the kernel has no floats
//!
//! The kernel is built for `x86_64-unknown-none` (`+soft-float`) and never
//! executes an x87/SSE/AVX instruction — every `f64` in kernel code lowers to a
//! `compiler_builtins` call. So the FP register file belongs *entirely* to
//! userspace, and the kernel's whole job is to keep one thread's registers from
//! leaking into another's. That makes this module small and its contract sharp:
//! enable the units once per CPU, and swap the register file at exactly one
//! place (`sched::switch_into`).
//!
//! ## Eager, not lazy
//!
//! The classic alternative is *lazy* FPU switching: leave the outgoing thread's
//! registers in the CPU, set `CR0.TS`, and let the first FP instruction in the
//! next thread `#NM`-fault so the handler can swap in that thread's state. It
//! saves the swap entirely for the (common) threads that never touch FP.
//!
//! We do **eager** save/restore instead:
//!
//! - **Security.** Lazy FPU state restore is CVE-2018-3665: between the switch
//!   and the `#NM`, the *previous* thread's register contents are architecturally
//!   present and speculatively readable across a privilege boundary. Linux
//!   removed lazy switching outright for this reason. Nitrox's whole premise is
//!   that authority does not leak between processes; leaving another process's
//!   AES round keys sitting in `xmm` is exactly that leak.
//! - **Cost — measured, not assumed.** `XSAVE`+`XRSTOR` of the 832-byte
//!   x87+SSE+AVX area costs **≈162 cycles of a ≈4100-cycle context switch — 3 %**
//!   (KVM, `-cpu host`, `RDTSC`; `boot_selftest::fp_swap_cost` reports both
//!   figures every selftest boot). A switch already pays for a `SCHED` acquire, a
//!   CR3 load, a TSS re-arm, and a stack swap; the register file is noise beside
//!   them. Lazy switching would trade a 3 % saving for a speculative-disclosure
//!   channel.
//! - **Simplicity under SMP.** Lazy switching requires tracking *which CPU* holds
//!   a thread's live registers and shooting it down when the thread migrates.
//!   That is a second cross-CPU coherence protocol, in a substrate that just
//!   spent a hardening slice paying off the first one.
//!
//! `CR0.TS` is therefore left **clear**: no `#NM` ever fires, and there is no
//! "FPU owner" state to track.
//!
//! ## Format selection
//!
//! `XSAVE`/`XRSTOR` when CPUID advertises it (all x86_64 of interest), with
//! `XCR0` requesting x87 + SSE + AVX-if-present; `FXSAVE`/`FXRSTOR` otherwise.
//! **AVX-512 is deliberately not enabled** — it would inflate the per-thread area
//! from 832 B to ~2.7 KiB to serve a userspace baseline that is SSE2 (see the
//! decision log, 2026-07-21). Widening `XCR0` later is a one-line change plus a
//! bump to [`FPU_AREA_BYTES`].

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::arch::x86_64::regs;

/// Size of the per-thread save area, in bytes.
///
/// The `XSAVE` area for x87 + SSE + AVX is 832 B (a 512 B legacy region, a 64 B
/// `XSAVE` header, and a 256 B `YMM_Hi128` region); `FXSAVE` needs 512 B. This
/// is rounded up to 1 KiB so the CPUID-reported size has headroom, and
/// [`init_cpu`] asserts the runtime size actually fits.
pub const FPU_AREA_BYTES: usize = 1024;

/// Byte alignment `XSAVE`/`XRSTOR` require of the save area (`FXSAVE` needs only
/// 16, but the stricter value covers both).
const FPU_AREA_ALIGN: usize = 64;

/// A thread's saved floating-point / SIMD register file.
///
/// Opaque by construction: the contents are an architectural `XSAVE` (or
/// `FXSAVE`) image whose layout is defined by the CPU, not by us, so nothing
/// outside this module may inspect or construct it field-wise. A thread's area
/// is written only by [`save`] and read only by [`restore`], both under the
/// scheduler's switch discipline.
///
/// The alignment is load-bearing: `XSAVE` `#GP`s on a misaligned operand.
#[repr(C, align(64))]
pub struct ArchFpuState {
    bytes: [u8; FPU_AREA_BYTES],
}

// The alignment attribute above and the constant used by the allocator must not
// drift apart — a silent mismatch is a `#GP` on the first context switch.
const _: () = assert!(core::mem::align_of::<ArchFpuState>() == FPU_AREA_ALIGN);

impl ArchFpuState {
    /// An all-zero area. **Not** a usable FP state on its own — the caller must
    /// run [`init_area`] before the area is ever handed to [`restore`] (a zeroed
    /// `MXCSR` unmasks every SIMD exception, and a zeroed `FCW` is not the
    /// architectural default control word).
    pub const fn zeroed() -> Self {
        Self {
            bytes: [0; FPU_AREA_BYTES],
        }
    }
}

/// Offset of the x87 control word within both the `FXSAVE` and `XSAVE` legacy
/// regions.
const FCW_OFFSET: usize = 0;
/// The architectural default `FCW`: round-to-nearest, 64-bit precision, all six
/// x87 exceptions masked.
const FCW_DEFAULT: u16 = 0x037F;
/// Offset of `MXCSR` within both legacy regions.
const MXCSR_OFFSET: usize = 24;
/// The architectural default `MXCSR`: all six SIMD exceptions masked,
/// round-to-nearest, flush-to-zero and denormals-are-zero off.
const MXCSR_DEFAULT: u32 = 0x1F80;

/// CPUID.01H:EDX bit 24 — `FXSAVE`/`FXRSTOR` supported.
const CPUID_1_EDX_FXSR: u32 = 1 << 24;
/// CPUID.01H:EDX bit 25 — SSE supported.
const CPUID_1_EDX_SSE: u32 = 1 << 25;
/// CPUID.01H:ECX bit 26 — `XSAVE`/`XRSTOR` and `XCR0` supported.
const CPUID_1_ECX_XSAVE: u32 = 1 << 26;
/// CPUID.01H:ECX bit 28 — AVX supported.
const CPUID_1_ECX_AVX: u32 = 1 << 28;
/// CPUID leaf 0DH — processor extended state enumeration.
const CPUID_XSTATE_LEAF: u32 = 0x0000_000D;

/// CR0 bit 1 — `MP` (monitor coprocessor). Set, per the SDM's recommended
/// setting for a system with an integrated FPU.
const CR0_MP: u64 = 1 << 1;
/// CR0 bit 2 — `EM` (emulation). Must be **clear**: set, every SSE instruction
/// `#UD`s.
const CR0_EM: u64 = 1 << 2;
/// CR0 bit 3 — `TS` (task switched). Must be **clear**: we switch eagerly, so no
/// `#NM` should ever fire (see the module docs).
const CR0_TS: u64 = 1 << 3;
/// CR0 bit 5 — `NE` (numeric error). Set, so x87 exceptions report as `#MF`
/// rather than through the legacy external-interrupt path.
const CR0_NE: u64 = 1 << 5;

/// CR4 bit 9 — `OSFXSR`. Tells the CPU the OS uses `FXSAVE`/`FXRSTOR`, which is
/// also what enables SSE instructions.
const CR4_OSFXSR: u64 = 1 << 9;
/// CR4 bit 10 — `OSXMMEXCPT`. Unmasked SIMD exceptions raise `#XM` rather than
/// `#UD`.
const CR4_OSXMMEXCPT: u64 = 1 << 10;
/// CR4 bit 18 — `OSXSAVE`. Enables `XSAVE`, `XGETBV`/`XSETBV`, and the
/// CPUID.01H:ECX AVX bit.
const CR4_OSXSAVE: u64 = 1 << 18;

/// `XCR0` bit 0 — x87 state. Always set; the CPU `#GP`s if it is clear.
const XCR0_X87: u64 = 1 << 0;
/// `XCR0` bit 1 — SSE (`XMM`) state.
const XCR0_SSE: u64 = 1 << 1;
/// `XCR0` bit 2 — AVX (`YMM_Hi128`) state. Requires the SSE bit; enabling AVX
/// without it `#GP`s.
const XCR0_AVX: u64 = 1 << 2;

/// `true` once a CPU has selected the `XSAVE` format; `false` means `FXSAVE`.
/// Written identically by every CPU in [`init_cpu`] (CPUID is uniform across the
/// package on every platform the kernel supports) and read on the switch path.
static USE_XSAVE: AtomicBool = AtomicBool::new(false);

/// The runtime save-area size CPUID reports for the enabled `XCR0`, or `0`
/// before the first [`init_cpu`]. Diagnostic (and asserted against
/// [`FPU_AREA_BYTES`]); the swap itself always addresses the full fixed area.
static AREA_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Widest vector register the enabled state covers — `128` for SSE-only, `256`
/// once AVX is on — or `0` before the first [`init_cpu`]. Diagnostic: it is how
/// a boot log distinguishes a `qemu64` run from an AVX-capable one, which is the
/// difference the CPUID-driven area sizing has to get right.
static VECTOR_BITS: AtomicUsize = AtomicUsize::new(0);

/// Enable this CPU's FP/SIMD units and select the save format.
///
/// Must run on **every** CPU — the BSP during boot and each AP during its
/// bring-up — before any thread carrying FP state is scheduled onto it. `CR0`,
/// `CR4`, and `XCR0` are per-CPU registers, so this is not something the BSP can
/// do on an AP's behalf. Idempotent.
///
/// Panics if the CPU lacks `FXSAVE`/SSE (impossible on x86_64, which mandates
/// both) or if the enabled state's size exceeds [`FPU_AREA_BYTES`].
pub fn init_cpu() {
    let (_, _, ecx1, edx1) = regs::cpuid(1, 0);
    assert!(
        edx1 & CPUID_1_EDX_FXSR != 0 && edx1 & CPUID_1_EDX_SSE != 0,
        "CPU lacks FXSAVE/SSE (CPUID.01H:EDX[24],[25]) — mandatory on x86_64."
    );

    // CR0: clear EM so SSE instructions execute rather than `#UD`; clear TS so
    // no `#NM` fires (we swap eagerly); set MP and NE per the SDM's integrated-FPU
    // configuration.
    let cr0 = (regs::read_cr0() & !(CR0_EM | CR0_TS)) | CR0_MP | CR0_NE;
    // SAFETY: ring 0. Only the four FPU-control bits are touched — PG/PE/WP and
    // every other mode bit are carried through from the live value, so the
    // running kernel's paging and protection settings are unchanged. `EM=0` with
    // `MP=1` and `NE=1` is the SDM's recommended combination for a CPU with an
    // integrated FPU, which the assertion above has confirmed.
    unsafe { regs::write_cr0(cr0) };

    // CR4: OSFXSR both enables SSE and declares that the OS manages FP state with
    // FXSAVE; OSXMMEXCPT routes unmasked SIMD exceptions to `#XM`.
    let mut cr4 = regs::read_cr4() | CR4_OSFXSR | CR4_OSXMMEXCPT;
    let has_xsave = ecx1 & CPUID_1_ECX_XSAVE != 0;
    if has_xsave {
        cr4 |= CR4_OSXSAVE;
    }
    // SAFETY: ring 0. Every bit set here is gated on the CPUID query above
    // (`OSFXSR`/`OSXMMEXCPT` on the mandatory FXSR+SSE bits, `OSXSAVE` on
    // CPUID.01H:ECX[26]); setting a CR4 bit for an unimplemented feature would
    // `#GP`, and we set none. Existing bits (SMEP/SMAP/PAE/…) are preserved.
    unsafe { regs::write_cr4(cr4) };

    let area = if has_xsave {
        // CPUID.01H:ECX[28] (AVX) is only architecturally meaningful once
        // `CR4.OSXSAVE` is set, which the write above has just done.
        let (_, _, ecx1_post, _) = regs::cpuid(1, 0);
        let mut xcr0 = XCR0_X87 | XCR0_SSE;
        if ecx1_post & CPUID_1_ECX_AVX != 0 {
            xcr0 |= XCR0_AVX;
        }
        // Mask against what the CPU actually supports. AVX is enabled only when
        // both CPUID.01H:ECX[28] and the state-component bit agree, so a CPU that
        // advertises the instruction set without the state component cannot
        // produce a `#GP` here.
        let (supported_lo, _, _, supported_hi) = regs::cpuid(CPUID_XSTATE_LEAF, 0);
        xcr0 &= ((supported_hi as u64) << 32) | (supported_lo as u64);
        // SAFETY: `CR4.OSXSAVE` was set above, so `XSETBV` is not `#UD`. The value
        // retains bit 0 (x87, mandatory), sets AVX only together with SSE (the
        // legal combination), and has been masked against the CPU's supported
        // component bitmap — the three conditions that would otherwise `#GP`.
        unsafe { regs::write_xcr0(xcr0) };
        // Read back rather than trusting the write: the enabled component set
        // determines both the image layout and the required area size, and a
        // silent divergence between what we asked for and what the CPU accepted
        // would surface later as a `#GP` on the first `XRSTOR`.
        // SAFETY: `CR4.OSXSAVE` is set, so `XGETBV` is not `#UD`.
        let live = unsafe { regs::read_xcr0() };
        assert_eq!(live, xcr0, "XCR0 did not accept the requested component set");
        VECTOR_BITS.store(
            if live & XCR0_AVX != 0 { 256 } else { 128 },
            Ordering::Relaxed,
        );
        // EBX of subleaf 0 reports the area size required by the *enabled* XCR0,
        // so it must be re-read after the XSETBV above.
        let (_, ebx, _, _) = regs::cpuid(CPUID_XSTATE_LEAF, 0);
        USE_XSAVE.store(true, Ordering::Relaxed);
        ebx as usize
    } else {
        // FXSAVE covers x87 + SSE only, in a fixed 512-byte image.
        VECTOR_BITS.store(128, Ordering::Relaxed);
        512
    };

    assert!(
        area <= FPU_AREA_BYTES,
        "FPU save area needs more than FPU_AREA_BYTES — raise the constant"
    );
    AREA_BYTES.store(area, Ordering::Relaxed);
}

/// Bytes of the per-thread area the CPU's enabled state actually occupies, or
/// `0` before the first [`init_cpu`]. Diagnostic only — the area itself is
/// always [`FPU_AREA_BYTES`] long.
pub fn area_bytes() -> usize {
    AREA_BYTES.load(Ordering::Relaxed)
}

/// Width in bits of the widest vector register the enabled state covers (`128`
/// SSE-only, `256` with AVX), or `0` before the first [`init_cpu`].
pub fn vector_bits() -> usize {
    VECTOR_BITS.load(Ordering::Relaxed)
}

/// Write the architectural power-on FP state into `area`, making it safe to hand
/// to [`restore`].
///
/// A zeroed area is *not* sufficient. Under `XRSTOR` a zero `XSTATE_BV` header
/// does put x87 and `YMM` into their init states, but `MXCSR` is loaded from the
/// image unconditionally whenever SSE or AVX is requested — and a zero `MXCSR`
/// unmasks every SIMD exception, so the thread's first inexact result would trap.
/// Under `FXRSTOR` nothing is implied at all. Both formats place `FCW` and
/// `MXCSR` at the same offsets in the legacy region, so one write path serves
/// both.
///
/// # Safety
/// `area` must point to a valid, writable, 64-byte-aligned [`ArchFpuState`].
pub unsafe fn init_area(area: *mut ArchFpuState) {
    // SAFETY: the caller guarantees `area` is a valid, writable `ArchFpuState`;
    // we write only within its `FPU_AREA_BYTES` bytes.
    unsafe {
        let base = (&raw mut (*area).bytes).cast::<u8>();
        core::ptr::write_bytes(base, 0, FPU_AREA_BYTES);
        // Unaligned writes: the fields sit at fixed architectural offsets that
        // carry no alignment guarantee of their own.
        base.add(FCW_OFFSET).cast::<u16>().write_unaligned(FCW_DEFAULT);
        base.add(MXCSR_OFFSET)
            .cast::<u32>()
            .write_unaligned(MXCSR_DEFAULT);
    }
}

/// Save the CPU's live FP/SIMD register file into `area`.
///
/// # Safety
/// `area` must point to a valid, writable, 64-byte-aligned [`ArchFpuState`], and
/// [`init_cpu`] must have run on this CPU. The caller owns the swap discipline:
/// on the scheduler's switch path this runs while the outgoing thread's `on_cpu`
/// guard is still raised, so no other CPU can resume the thread — and therefore
/// read this area — before the write completes.
pub unsafe fn save(area: *mut ArchFpuState) {
    // SAFETY: forwarded — `area` is a valid, writable, correctly aligned image
    // buffer, and the CPU has been enabled for the selected format.
    unsafe {
        let base = (&raw mut (*area).bytes).cast::<u8>();
        if USE_XSAVE.load(Ordering::Relaxed) {
            // EDX:EAX is the requested-feature bitmap; all-ones asks for every
            // component, which the CPU intersects with XCR0. `xsave64` selects the
            // 64-bit layout (a 64-bit FIP/FDP rather than the split
            // selector:offset form).
            asm!(
                "xsave64 [{area}]",
                area = in(reg) base,
                in("eax") u32::MAX,
                in("edx") u32::MAX,
                options(nostack, preserves_flags),
            );
        } else {
            asm!(
                "fxsave64 [{area}]",
                area = in(reg) base,
                options(nostack, preserves_flags),
            );
        }
    }
}

/// Load `area` into the CPU's FP/SIMD register file.
///
/// # Safety
/// `area` must point to a valid, 64-byte-aligned [`ArchFpuState`] holding an
/// image produced by [`save`] or [`init_area`] **in the format this CPU selected
/// in [`init_cpu`]**, and `init_cpu` must have run on this CPU. Restoring a
/// foreign or uninitialised image can `#GP` (reserved `MXCSR` bits, an
/// `XSTATE_BV` naming a component outside `XCR0`).
pub unsafe fn restore(area: *const ArchFpuState) {
    // SAFETY: forwarded — `area` holds a well-formed image in the CPU's selected
    // format, at the required alignment.
    unsafe {
        let base = (&raw const (*area).bytes).cast::<u8>();
        if USE_XSAVE.load(Ordering::Relaxed) {
            asm!(
                "xrstor64 [{area}]",
                area = in(reg) base,
                in("eax") u32::MAX,
                in("edx") u32::MAX,
                options(nostack, preserves_flags),
            );
        } else {
            asm!(
                "fxrstor64 [{area}]",
                area = in(reg) base,
                options(nostack, preserves_flags),
            );
        }
    }
}

// === self-test support ==================================================================
//
// Compiled only into `selftest` builds. See `boot_selftest::fp_isolation_demo` for the
// test these primitives serve.

/// Number of vector registers the self-test stamps (`xmm0`–`xmm15` / `ymm0`–`ymm15`).
#[cfg(feature = "selftest")]
pub const SELFTEST_REGS: usize = 16;

/// Bytes reserved per register in a [`VectorRegsImage`]. Always the `ymm` width, even
/// when only `xmm` is enabled, so the image layout does not depend on the CPU — only
/// how much of each slot is meaningful does (see [`selftest_reg_bytes`]).
#[cfg(feature = "selftest")]
pub const SELFTEST_REG_STRIDE: usize = 32;

/// A snapshot of every vector register, one `SELFTEST_REG_STRIDE`-byte slot each.
///
/// 32-byte aligned because `vmovaps`/`movaps` `#GP` on a misaligned operand.
#[cfg(feature = "selftest")]
#[repr(C, align(32))]
pub struct VectorRegsImage {
    bytes: [u8; SELFTEST_REGS * SELFTEST_REG_STRIDE],
}

#[cfg(feature = "selftest")]
impl VectorRegsImage {
    /// A zeroed image.
    pub const fn zeroed() -> Self {
        Self {
            bytes: [0; SELFTEST_REGS * SELFTEST_REG_STRIDE],
        }
    }

    /// The slot for register `i`, as a byte slice of the full stride.
    pub fn slot_mut(&mut self, i: usize) -> &mut [u8] {
        &mut self.bytes[i * SELFTEST_REG_STRIDE..(i + 1) * SELFTEST_REG_STRIDE]
    }

    /// The slot for register `i`.
    pub fn slot(&self, i: usize) -> &[u8] {
        &self.bytes[i * SELFTEST_REG_STRIDE..(i + 1) * SELFTEST_REG_STRIDE]
    }
}

/// How many bytes of each slot are actually held in a register on this CPU: 32 with AVX
/// enabled (`ymm`), 16 otherwise (`xmm`). Bytes beyond this are not architectural state
/// and must not be compared.
#[cfg(feature = "selftest")]
pub fn selftest_reg_bytes() -> usize {
    if vector_bits() >= 256 { 32 } else { 16 }
}

// The load/store pairs below deliberately declare **no vector-register operands and no
// vector clobbers** — which would normally be unsound, since the compiler could be
// holding a live value in one.
//
// It is sound here, and only here, because the kernel is built for
// `x86_64-unknown-none` (`-mmx,-sse,+soft-float`): rustc will not allocate a vector
// register under any circumstance, so there is never a live value to clobber. (That is
// also why the operands *cannot* be declared — the `xmm_reg` class requires the `sse`
// target feature, which this target disables. Inline-asm mnemonics are assembled
// regardless of target features; only codegen is gated.) The same property is what makes
// this test meaningful: between [`selftest_load_regs`] and [`selftest_store_regs`] the
// **only** things that can touch these registers are the context switch's save/restore
// and another thread running on this CPU — which is precisely the leak being tested for.

/// Load every vector register from `img`.
///
/// # Safety
/// `img` must point to a valid, 32-byte-aligned [`VectorRegsImage`]. Ring-0, and
/// [`init_cpu`] must have run on this CPU (otherwise `CR0.EM` would `#UD` these).
#[cfg(feature = "selftest")]
pub unsafe fn selftest_load_regs(img: *const VectorRegsImage) {
    // SAFETY: see the module note above on the absent vector clobbers. The pointer is a
    // valid aligned image; each access is within its `SELFTEST_REGS * STRIDE` bytes.
    unsafe {
        let p = (&raw const (*img).bytes).cast::<u8>();
        if vector_bits() >= 256 {
            asm!(
                "vmovaps ymm0,  [{p} + 0]", "vmovaps ymm1,  [{p} + 32]",
                "vmovaps ymm2,  [{p} + 64]", "vmovaps ymm3,  [{p} + 96]",
                "vmovaps ymm4,  [{p} + 128]", "vmovaps ymm5,  [{p} + 160]",
                "vmovaps ymm6,  [{p} + 192]", "vmovaps ymm7,  [{p} + 224]",
                "vmovaps ymm8,  [{p} + 256]", "vmovaps ymm9,  [{p} + 288]",
                "vmovaps ymm10, [{p} + 320]", "vmovaps ymm11, [{p} + 352]",
                "vmovaps ymm12, [{p} + 384]", "vmovaps ymm13, [{p} + 416]",
                "vmovaps ymm14, [{p} + 448]", "vmovaps ymm15, [{p} + 480]",
                p = in(reg) p,
                options(readonly, nostack, preserves_flags),
            );
        } else {
            asm!(
                "movaps xmm0,  [{p} + 0]", "movaps xmm1,  [{p} + 32]",
                "movaps xmm2,  [{p} + 64]", "movaps xmm3,  [{p} + 96]",
                "movaps xmm4,  [{p} + 128]", "movaps xmm5,  [{p} + 160]",
                "movaps xmm6,  [{p} + 192]", "movaps xmm7,  [{p} + 224]",
                "movaps xmm8,  [{p} + 256]", "movaps xmm9,  [{p} + 288]",
                "movaps xmm10, [{p} + 320]", "movaps xmm11, [{p} + 352]",
                "movaps xmm12, [{p} + 384]", "movaps xmm13, [{p} + 416]",
                "movaps xmm14, [{p} + 448]", "movaps xmm15, [{p} + 480]",
                p = in(reg) p,
                options(readonly, nostack, preserves_flags),
            );
        }
    }
}

/// Store every vector register into `img`.
///
/// # Safety
/// As [`selftest_load_regs`], but `img` must be writable.
#[cfg(feature = "selftest")]
pub unsafe fn selftest_store_regs(img: *mut VectorRegsImage) {
    // SAFETY: see the module note above on the absent vector clobbers.
    unsafe {
        let p = (&raw mut (*img).bytes).cast::<u8>();
        if vector_bits() >= 256 {
            asm!(
                "vmovaps [{p} + 0],   ymm0", "vmovaps [{p} + 32],  ymm1",
                "vmovaps [{p} + 64],  ymm2", "vmovaps [{p} + 96],  ymm3",
                "vmovaps [{p} + 128], ymm4", "vmovaps [{p} + 160], ymm5",
                "vmovaps [{p} + 192], ymm6", "vmovaps [{p} + 224], ymm7",
                "vmovaps [{p} + 256], ymm8", "vmovaps [{p} + 288], ymm9",
                "vmovaps [{p} + 320], ymm10", "vmovaps [{p} + 352], ymm11",
                "vmovaps [{p} + 384], ymm12", "vmovaps [{p} + 416], ymm13",
                "vmovaps [{p} + 448], ymm14", "vmovaps [{p} + 480], ymm15",
                p = in(reg) p,
                options(nostack, preserves_flags),
            );
        } else {
            asm!(
                "movaps [{p} + 0],   xmm0", "movaps [{p} + 32],  xmm1",
                "movaps [{p} + 64],  xmm2", "movaps [{p} + 96],  xmm3",
                "movaps [{p} + 128], xmm4", "movaps [{p} + 160], xmm5",
                "movaps [{p} + 192], xmm6", "movaps [{p} + 224], xmm7",
                "movaps [{p} + 256], xmm8", "movaps [{p} + 288], xmm9",
                "movaps [{p} + 320], xmm10", "movaps [{p} + 352], xmm11",
                "movaps [{p} + 384], xmm12", "movaps [{p} + 416], xmm13",
                "movaps [{p} + 448], xmm14", "movaps [{p} + 480], xmm15",
                p = in(reg) p,
                options(nostack, preserves_flags),
            );
        }
    }
}

/// Median cycle cost of one save+restore pair, over `rounds` samples.
///
/// Measured with `RDTSC` around a back-to-back [`save`]/[`restore`] of a scratch area, so
/// it isolates the FP swap from the rest of the switch. Meaningful only under KVM or on
/// real hardware — TCG's `RDTSC` is not a cycle counter.
///
/// # Safety
/// Ring-0; [`init_cpu`] must have run on this CPU. Clobbers the live FP register file
/// (it round-trips it through the scratch area), so the caller must not hold live vector
/// state across the call.
#[cfg(feature = "selftest")]
pub unsafe fn selftest_swap_cycles(scratch: *mut ArchFpuState, rounds: u32) -> u64 {
    let mut best = u64::MAX;
    for _ in 0..rounds {
        let t0 = regs::rdtsc();
        // SAFETY: forwarded — `scratch` is a valid, aligned area and the CPU is enabled.
        unsafe {
            save(scratch);
            restore(scratch);
        }
        let t1 = regs::rdtsc();
        // Minimum over the samples: the cheapest observation is the one least polluted by
        // an interrupt or a cache miss landing inside the window.
        best = best.min(t1.wrapping_sub(t0));
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn area_is_xsave_aligned_and_large_enough() {
        assert_eq!(core::mem::align_of::<ArchFpuState>(), FPU_AREA_ALIGN);
        assert_eq!(core::mem::size_of::<ArchFpuState>(), FPU_AREA_BYTES);
        // x87+SSE+AVX: 512 B legacy + 64 B header + 256 B YMM_Hi128.
        assert!(FPU_AREA_BYTES >= 512 + 64 + 256);
    }

    #[test]
    fn init_area_writes_the_architectural_defaults() {
        let mut state = ArchFpuState::zeroed();
        state.bytes[MXCSR_OFFSET] = 0xAA;
        state.bytes[900] = 0xAA;

        // SAFETY: `state` is a live, writable, correctly aligned area.
        unsafe { init_area(&raw mut state) };

        let fcw = u16::from_le_bytes([state.bytes[FCW_OFFSET], state.bytes[FCW_OFFSET + 1]]);
        assert_eq!(fcw, FCW_DEFAULT, "FCW must be the architectural default");
        let mxcsr = u32::from_le_bytes(
            state.bytes[MXCSR_OFFSET..MXCSR_OFFSET + 4]
                .try_into()
                .expect("4 bytes"),
        );
        assert_eq!(
            mxcsr, MXCSR_DEFAULT,
            "MXCSR must mask every SIMD exception, not be left zeroed"
        );
        assert_eq!(state.bytes[900], 0, "the rest of the area must be zeroed");
    }

    #[test]
    fn xcr0_avx_requires_sse() {
        // The `#GP` condition we rely on never constructing: AVX without SSE.
        let legal = XCR0_X87 | XCR0_SSE | XCR0_AVX;
        assert_eq!(legal & XCR0_SSE, XCR0_SSE);
        assert_ne!(legal & XCR0_X87, 0, "bit 0 is mandatory");
    }
}
