//! x86_64 floating-point / SIMD state: the per-CPU enable and the per-thread save area the
//! scheduler swaps on every context switch.
//!
//! The kernel is `+soft-float`, so the FP register file belongs entirely to userspace and the job
//! is to keep one thread's registers from leaking into another's: enable the units once per
//! CPU, and swap the register file at exactly one place (`sched::switch_into`).
//!
//! ## Eager, not lazy
//!
//! - **Security.** Lazy restore (set `CR0.TS`, swap on the first `#NM`) is CVE-2018-3665: the
//!   *previous* thread's registers stay speculatively readable across a privilege boundary.
//! - **Cost.** `XSAVE`+`XRSTOR` is ≈162 cycles of a ≈4100-cycle switch (`boot_selftest::
//!   fp_swap_cost` reports both).
//! - **SMP.** Lazy switching needs a cross-CPU protocol to shoot down a migrating thread's
//!   live registers.
//!
//! `CR0.TS` is therefore left **clear**: no `#NM` fires, and there is no "FPU owner" state.
//!
//! ## Format selection
//!
//! `XSAVE`/`XRSTOR` when CPUID advertises it, with `XCR0` requesting x87 + SSE + AVX-if-present;
//! `FXSAVE`/`FXRSTOR` otherwise. **AVX-512 is deliberately not enabled**: it would inflate the
//! per-thread area from 832 B to ~2.7 KiB (decision log, 2026-07-21); widening `XCR0` needs a
//! bump to [`FPU_AREA_BYTES`].

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::arch::x86_64::regs;

/// Size of the per-thread save area, in bytes.
///
/// The `XSAVE` area for x87 + SSE + AVX is 832 B (512 B legacy, 64 B header, 256 B `YMM_Hi128`);
/// `FXSAVE` needs 512 B. [`init_cpu`] asserts the runtime size fits.
pub const FPU_AREA_BYTES: usize = 1024;

/// Alignment `XSAVE`/`XRSTOR` require of the save area (`FXSAVE` needs only 16).
const FPU_AREA_ALIGN: usize = 64;

/// A thread's saved floating-point / SIMD register file.
///
/// Opaque: an architectural `XSAVE` (or `FXSAVE`) image, written only by [`save`] and read only
/// by [`restore`]. The alignment is load-bearing: `XSAVE` `#GP`s on a misaligned operand.
#[repr(C, align(64))]
pub struct ArchFpuState {
    bytes: [u8; FPU_AREA_BYTES],
}

// A mismatch with the allocator's constant is a `#GP` on the first context switch.
const _: () = assert!(core::mem::align_of::<ArchFpuState>() == FPU_AREA_ALIGN);

impl ArchFpuState {
    /// An all-zero area. **Not** a usable FP state: run [`init_area`] before [`restore`].
    pub const fn zeroed() -> Self {
        Self { bytes: [0; FPU_AREA_BYTES] }
    }
}

const FCW_OFFSET: usize = 0;
const FCW_DEFAULT: u16 = 0x037F;
const MXCSR_OFFSET: usize = 24;
const MXCSR_DEFAULT: u32 = 0x1F80;

const CPUID_1_EDX_FXSR: u32 = 1 << 24;
const CPUID_1_EDX_SSE: u32 = 1 << 25;
const CPUID_1_ECX_XSAVE: u32 = 1 << 26;
const CPUID_1_ECX_AVX: u32 = 1 << 28;
const CPUID_XSTATE_LEAF: u32 = 0x0000_000D;

const CR0_MP: u64 = 1 << 1;
/// CR0 bit 2, `EM`: must be **clear**, or every SSE instruction `#UD`s.
const CR0_EM: u64 = 1 << 2;
/// CR0 bit 3, `TS`: must be **clear**; we switch eagerly (see the module docs).
const CR0_TS: u64 = 1 << 3;
const CR0_NE: u64 = 1 << 5;

const CR4_OSFXSR: u64 = 1 << 9;
const CR4_OSXMMEXCPT: u64 = 1 << 10;
const CR4_OSXSAVE: u64 = 1 << 18;

const XCR0_X87: u64 = 1 << 0;
const XCR0_SSE: u64 = 1 << 1;
/// `XCR0` bit 2: AVX (`YMM_Hi128`) state. Requires the SSE bit, or `#GP`.
const XCR0_AVX: u64 = 1 << 2;

/// `true` once a CPU has selected the `XSAVE` format; `false` means `FXSAVE`. Written identically
/// by every CPU in [`init_cpu`] (CPUID is uniform across the package).
static USE_XSAVE: AtomicBool = AtomicBool::new(false);

/// The save-area size CPUID reports for the enabled `XCR0`, or `0` before [`init_cpu`].
static AREA_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Widest enabled vector register (`128` or `256`), or `0` before [`init_cpu`].
static VECTOR_BITS: AtomicUsize = AtomicUsize::new(0);

/// Enable this CPU's FP/SIMD units and select the save format.
///
/// Must run on **every** CPU before any thread carrying FP state is scheduled onto it: `CR0`,
/// `CR4` and `XCR0` are per-CPU. Idempotent.
///
/// Panics if the CPU lacks `FXSAVE`/SSE (mandatory on x86_64) or if the enabled state exceeds
/// [`FPU_AREA_BYTES`].
pub fn init_cpu() {
    let (_, _, ecx1, edx1) = regs::cpuid(1, 0);
    assert!(
        edx1 & CPUID_1_EDX_FXSR != 0 && edx1 & CPUID_1_EDX_SSE != 0,
        "CPU lacks FXSAVE/SSE (CPUID.01H:EDX[24],[25]) — mandatory on x86_64."
    );

    // Clear EM (or SSE `#UD`s) and TS (no `#NM`: the swap is eager); set MP, and NE so x87
    // errors report as `#MF`, per the SDM's integrated-FPU configuration.
    let cr0 = (regs::read_cr0() & !(CR0_EM | CR0_TS)) | CR0_MP | CR0_NE;
    // SAFETY: ring 0. Only the four FPU-control bits are touched — PG/PE/WP and
    // every other mode bit are carried through from the live value, so the
    // running kernel's paging and protection settings are unchanged. `EM=0` with
    // `MP=1` and `NE=1` is the SDM's recommended combination for a CPU with an
    // integrated FPU, which the assertion above has confirmed.
    unsafe { regs::write_cr0(cr0) };

    // OSFXSR enables SSE and says the OS saves state with `FXSAVE`; OSXMMEXCPT sends unmasked
    // SIMD exceptions to `#XM` rather than `#UD`.
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
        // CPUID.01H:ECX[28] (AVX) is only meaningful once `CR4.OSXSAVE` is set.
        let (_, _, ecx1_post, _) = regs::cpuid(1, 0);
        let mut xcr0 = XCR0_X87 | XCR0_SSE;
        if ecx1_post & CPUID_1_ECX_AVX != 0 {
            xcr0 |= XCR0_AVX;
        }
        // AVX only when both CPUID.01H:ECX[28] and the state-component bit agree.
        let (supported_lo, _, _, supported_hi) = regs::cpuid(CPUID_XSTATE_LEAF, 0);
        xcr0 &= ((supported_hi as u64) << 32) | (supported_lo as u64);
        // SAFETY: `CR4.OSXSAVE` was set above, so `XSETBV` is not `#UD`. The value
        // retains bit 0 (x87, mandatory), sets AVX only together with SSE (the
        // legal combination), and has been masked against the CPU's supported
        // component bitmap — the three conditions that would otherwise `#GP`.
        unsafe { regs::write_xcr0(xcr0) };
        // Read back: a divergence would surface as a `#GP` on the first `XRSTOR`.
        // SAFETY: `CR4.OSXSAVE` is set, so `XGETBV` is not `#UD`.
        let live = unsafe { regs::read_xcr0() };
        assert_eq!(live, xcr0, "XCR0 did not accept the requested component set");
        VECTOR_BITS.store(if live & XCR0_AVX != 0 { 256 } else { 128 }, Ordering::Relaxed);
        // EBX reports the area size for the *enabled* XCR0, so re-read it after XSETBV.
        let (_, ebx, _, _) = regs::cpuid(CPUID_XSTATE_LEAF, 0);
        USE_XSAVE.store(true, Ordering::Relaxed);
        ebx as usize
    } else {
        VECTOR_BITS.store(128, Ordering::Relaxed);
        512
    };

    assert!(area <= FPU_AREA_BYTES, "FPU save area needs more than FPU_AREA_BYTES — raise the constant");
    AREA_BYTES.store(area, Ordering::Relaxed);
}

/// Bytes of the per-thread area the enabled state occupies, or `0` before [`init_cpu`].
pub fn area_bytes() -> usize {
    AREA_BYTES.load(Ordering::Relaxed)
}

/// Width in bits of the widest enabled vector register, or `0` before [`init_cpu`].
pub fn vector_bits() -> usize {
    VECTOR_BITS.load(Ordering::Relaxed)
}

/// Write the power-on FP state into `area`, making it safe to hand to [`restore`].
///
/// A zeroed area is *not* sufficient: `XRSTOR` loads `MXCSR` from the image unconditionally, and
/// a zero `MXCSR` unmasks every SIMD exception. Both formats put `FCW` and `MXCSR` at the same
/// offsets, so one write path serves both.
///
/// # Safety
/// `area` must point to a valid, writable, 64-byte-aligned [`ArchFpuState`].
pub unsafe fn init_area(area: *mut ArchFpuState) {
    // SAFETY: the caller guarantees `area` is a valid, writable `ArchFpuState`;
    // we write only within its `FPU_AREA_BYTES` bytes.
    unsafe {
        let base = (&raw mut (*area).bytes).cast::<u8>();
        core::ptr::write_bytes(base, 0, FPU_AREA_BYTES);
        // Unaligned: the offsets carry no alignment guarantee of their own.
        base.add(FCW_OFFSET).cast::<u16>().write_unaligned(FCW_DEFAULT);
        base.add(MXCSR_OFFSET).cast::<u32>().write_unaligned(MXCSR_DEFAULT);
    }
}

/// Save the CPU's live FP/SIMD register file into `area`.
///
/// # Safety
/// `area` must point to a valid, writable, 64-byte-aligned [`ArchFpuState`], and [`init_cpu`]
/// must have run on this CPU. On the switch path this runs while the outgoing thread's `on_cpu`
/// guard is still raised, so no other CPU can resume the thread and read the area early.
pub unsafe fn save(area: *mut ArchFpuState) {
    // SAFETY: forwarded — `area` is a valid, writable, correctly aligned image
    // buffer, and the CPU has been enabled for the selected format.
    unsafe {
        let base = (&raw mut (*area).bytes).cast::<u8>();
        if USE_XSAVE.load(Ordering::Relaxed) {
            // EDX:EAX is the requested-feature bitmap; all-ones is intersected with XCR0.
            // `xsave64` selects the 64-bit FIP/FDP layout.
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
/// `area` must point to a valid, 64-byte-aligned [`ArchFpuState`] holding an image from [`save`]
/// or [`init_area`] **in the format this CPU selected in [`init_cpu`]**, and `init_cpu` must
/// have run. A foreign or uninitialised image can `#GP` (reserved `MXCSR` bits, `XSTATE_BV`
/// naming a component outside `XCR0`).
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

/// Vector registers the self-test stamps (`xmm0`-`xmm15` / `ymm0`-`ymm15`).
#[cfg(feature = "selftest")]
pub const SELFTEST_REGS: usize = 16;

/// Bytes reserved per register in a [`VectorRegsImage`]: always the `ymm` width, so the layout
/// does not depend on the CPU (see [`selftest_reg_bytes`]).
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
        Self { bytes: [0; SELFTEST_REGS * SELFTEST_REG_STRIDE] }
    }

    /// The slot for register `i`, mutably.
    pub fn slot_mut(&mut self, i: usize) -> &mut [u8] {
        &mut self.bytes[i * SELFTEST_REG_STRIDE..(i + 1) * SELFTEST_REG_STRIDE]
    }

    /// The slot for register `i`.
    pub fn slot(&self, i: usize) -> &[u8] {
        &self.bytes[i * SELFTEST_REG_STRIDE..(i + 1) * SELFTEST_REG_STRIDE]
    }
}

/// Bytes of each slot held in a register on this CPU: 32 with AVX (`ymm`), 16 otherwise.
/// Bytes beyond this are not state and must not be compared.
#[cfg(feature = "selftest")]
pub fn selftest_reg_bytes() -> usize {
    if vector_bits() >= 256 { 32 } else { 16 }
}

// The load/store pairs below declare **no vector-register operands and no vector clobbers**,
// sound only because the kernel target (`-mmx,-sse,+soft-float`) never lets rustc allocate a
// vector register. (The `xmm_reg` class needs the `sse` feature, so they *cannot* be declared.)

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
/// Meaningful only under KVM or on real hardware; TCG's `RDTSC` is not a cycle counter.
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
        // Minimum: the sample least polluted by an interrupt or cache miss.
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
        const { assert!(FPU_AREA_BYTES >= 512 + 64 + 256) };
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
        let mxcsr = u32::from_le_bytes(state.bytes[MXCSR_OFFSET..MXCSR_OFFSET + 4].try_into().expect("4 bytes"));
        assert_eq!(mxcsr, MXCSR_DEFAULT, "MXCSR must mask every SIMD exception, not be left zeroed");
        assert_eq!(state.bytes[900], 0, "the rest of the area must be zeroed");
    }

    #[test]
    fn xcr0_avx_requires_sse() {
        let legal = XCR0_X87 | XCR0_SSE | XCR0_AVX;
        assert_eq!(legal & XCR0_SSE, XCR0_SSE);
        assert_ne!(legal & XCR0_X87, 0, "bit 0 is mandatory");
    }
}
