//! x86_64 local-APIC bring-up in **xAPIC** (MMIO) mode ([`ArchIrq`] impl).
//!
//! xAPIC addresses the local-APIC registers through a 4 KiB memory-mapped page
//! (physical base from `IA32_APIC_BASE`, normally `0xFEE0_0000`). We map that
//! page once into the shared kernel vmap as uncached (`PageFlags::NO_CACHE`)
//! and access the 32-bit registers by volatile MMIO. The controller is found
//! straight from the MSR — no ACPI/MADT needed (see
//! `docs/rationale/why-phased-acpi.md`).
//!
//! **xAPIC, not x2APIC, for now.** x2APIC accesses the same registers through
//! MSRs (`0x800 + reg>>4`) instead of MMIO and uses 32-bit APIC IDs; it is the
//! right mode on real hardware (and mandatory once SMP exceeds 255 logical
//! CPUs). It is deferred: TCG only began emulating x2APIC in **QEMU 9.0** (the
//! dev loop runs older QEMU under TCG), and it is not needed until SMP /
//! real-hardware bring-up. The xAPIC↔x2APIC difference is localised to the
//! register accessors here, so dual-mode (auto-detect, prefer x2APIC) is a small
//! change when the time comes. See `docs/rationale/deferred-decisions.md`
//! ("x2APIC mode") and the decision log (2026-06-11).

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::cpu::ArchCpu;
use crate::arch::irq::{ArchIrq, SPURIOUS_VECTOR};
use crate::arch::paging::{ArchPaging, PageFlags};
use crate::arch::x86_64::cpu::X86Cpu;
use crate::arch::x86_64::regs;
use crate::arch::Paging;
use crate::libkern::AllocError;
use crate::mm::{PAGE_SIZE, PhysAddr, kvmap};

/// `IA32_APIC_BASE` — APIC global-enable bit + the controller's MMIO base.
const MSR_IA32_APIC_BASE: u32 = 0x1B;
/// Bit 11 — APIC global enable (firmware usually sets it; preserve it).
const APIC_BASE_GLOBAL_ENABLE: u64 = 1 << 11;
/// The MMIO base address occupies bits 12.. of `IA32_APIC_BASE`.
const APIC_BASE_ADDR_MASK: u64 = 0xFFFF_F000;

// --- xAPIC MMIO register offsets (Intel SDM Vol.3 Table 10-1) --------------
/// Local APIC ID register; the ID is in bits 24–31.
const REG_APICID: u64 = 0x20;
/// Task-Priority Register.
const REG_TPR: u64 = 0x80;
/// End-Of-Interrupt register (write 0).
const REG_EOI: u64 = 0xB0;
/// Spurious-Interrupt-Vector register.
const REG_SVR: u64 = 0xF0;
/// SVR bit 8 — APIC software enable.
const SVR_SOFTWARE_ENABLE: u32 = 1 << 8;

// --- xAPIC timer registers -------------------------------------------------
//
// Programmed by the timekeeping impl (`arch::x86_64::timer`); exposed here as
// `pub(crate)` (with the MMIO shims below) so `apic.rs` stays the single owner
// of `LAPIC_BASE` and the volatile-access logic.
/// LVT Timer entry: vector (bits 0–7), delivery mask (bit 16), and timer mode
/// (bits 18:17 — `00` one-shot, `01` periodic, `10` TSC-deadline).
pub(crate) const REG_LVT_TIMER: u64 = 0x320;
/// Timer Initial Count — writing a non-zero value (re)starts the countdown.
pub(crate) const REG_TIMER_INIT_COUNT: u64 = 0x380;
/// Timer Current Count — the live, read-only countdown value.
pub(crate) const REG_TIMER_CUR_COUNT: u64 = 0x390;
/// Timer Divide Configuration — divides the input clock before the countdown.
pub(crate) const REG_TIMER_DIV_CONFIG: u64 = 0x3E0;

/// Kernel-virtual base of the mapped LAPIC MMIO page. Set once by [`init`];
/// `0` means not-yet-initialised. Read by `eoi`/`id`.
static LAPIC_BASE: AtomicU64 = AtomicU64::new(0);

/// Read a LAPIC register by offset.
///
/// # Safety
/// [`init`] must have run (so `LAPIC_BASE` points at the mapped, uncached
/// MMIO page); `reg` must be a valid 4-byte-aligned register offset.
unsafe fn read_reg(reg: u64) -> u32 {
    let base = LAPIC_BASE.load(Ordering::Relaxed);
    debug_assert!(base != 0, "LAPIC accessed before init");
    // SAFETY: `base + reg` is inside the uncached LAPIC MMIO page mapped by
    // `init`; a 32-bit volatile read of a register is the defined access.
    unsafe { core::ptr::read_volatile((base + reg) as *const u32) }
}

/// Write a LAPIC register by offset.
///
/// # Safety
/// As [`read_reg`].
unsafe fn write_reg(reg: u64, val: u32) {
    let base = LAPIC_BASE.load(Ordering::Relaxed);
    debug_assert!(base != 0, "LAPIC accessed before init");
    // SAFETY: as `read_reg`, for a 32-bit volatile write.
    unsafe { core::ptr::write_volatile((base + reg) as *mut u32, val) };
}

/// LAPIC register read for sibling arch modules (the timekeeping impl programs
/// the timer LVT through this). Forwards to the private [`read_reg`].
///
/// # Safety
/// As [`read_reg`]: [`init`] must have run; `reg` a valid 4-byte-aligned offset.
pub(crate) unsafe fn read_reg_shared(reg: u64) -> u32 {
    // SAFETY: forwarded under the caller's contract.
    unsafe { read_reg(reg) }
}

/// LAPIC register write for sibling arch modules. Forwards to the private
/// [`write_reg`].
///
/// # Safety
/// As [`write_reg`].
pub(crate) unsafe fn write_reg_shared(reg: u64, val: u32) {
    // SAFETY: forwarded under the caller's contract.
    unsafe { write_reg(reg, val) };
}

/// `true` once [`init`] has mapped the LAPIC MMIO page — the precondition the
/// timekeeping impl asserts before programming the timer registers.
pub(crate) fn is_initialised() -> bool {
    LAPIC_BASE.load(Ordering::Relaxed) != 0
}

/// The x86_64 [`ArchIrq`] implementation (xAPIC). Zero-sized; re-exported as
/// `crate::arch::Irq`.
pub struct XApic;

impl ArchIrq for XApic {
    unsafe fn init() -> Result<(), AllocError> {
        assert!(
            X86Cpu::has_apic(),
            "no on-chip local APIC — the kernel requires one (CPUID.01H:EDX.9)."
        );

        // Discover the MMIO base from the MSR and ensure the controller is
        // globally enabled (read-modify-write so the firmware-set base and
        // other bits are preserved; xAPIC mode = x2APIC bit left clear).
        // SAFETY: IA32_APIC_BASE is architectural on every long-mode CPU.
        let apic_base_msr = unsafe { regs::rdmsr(MSR_IA32_APIC_BASE) };
        // SAFETY: as above; we only OR in the global-enable bit.
        unsafe { regs::wrmsr(MSR_IA32_APIC_BASE, apic_base_msr | APIC_BASE_GLOBAL_ENABLE) };
        let phys = PhysAddr(apic_base_msr & APIC_BASE_ADDR_MASK);

        // Map the register page into the shared kernel vmap as uncached,
        // kernel-only, writable, non-executable.
        let va = kvmap::vmap_alloc_pages(1)?;
        let flags = PageFlags::WRITABLE | PageFlags::NO_CACHE;
        // SAFETY: `va` is a fresh kernel-vmap page (never mapped); `phys` is
        // the LAPIC's MMIO frame from the MSR; mapping into the boot root is
        // visible from every address space (shared kernel-half PDPTs). Any
        // failure is an out-of-memory condition for the intermediate tables.
        unsafe { Paging::map_page(Paging::active_root(), va, phys, flags) }.map_err(|_| AllocError)?;
        debug_assert!(PAGE_SIZE >= 0x400, "LAPIC register file fits in one page");
        LAPIC_BASE.store(va.as_u64(), Ordering::Relaxed);

        // Software-enable + program the spurious vector; accept all priorities.
        // SAFETY: LAPIC_BASE is now set to the mapped uncached page.
        unsafe {
            write_reg(REG_SVR, SVR_SOFTWARE_ENABLE | SPURIOUS_VECTOR as u32);
            write_reg(REG_TPR, 0);
        }
        Ok(())
    }

    unsafe fn eoi() {
        // SAFETY: ring-0 MMIO write of 0 to the EOI register; valid after
        // `init()` has run (the caller's contract).
        unsafe { write_reg(REG_EOI, 0) };
    }

    fn id() -> u32 {
        // SAFETY: the ID register is readable once `init()` has mapped the
        // page; the read has no side effects. The ID is in bits 24–31.
        (unsafe { read_reg(REG_APICID) }) >> 24
    }
}
