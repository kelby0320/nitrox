//! x86_64 local-APIC bring-up in **x2APIC** (MSR) mode ([`ArchIrq`] impl).
//!
//! x2APIC reaches the local-APIC registers through MSRs: the xAPIC MMIO offset
//! `reg` maps to MSR `0x800 + (reg >> 4)`. No MMIO page is mapped, and MSR access
//! (`rdmsr`/`wrmsr`) is serialising, so register ordering is simpler than the
//! volatile-MMIO xAPIC model. The controller is found and enabled straight from
//! `IA32_APIC_BASE` — no ACPI/MADT needed (see `docs/rationale/why-phased-acpi.md`).
//!
//! **x2APIC-only (committed).** The ≈2014 / x86-64-v2 + SMEP/SMAP baseline
//! guarantees x2APIC, so the kernel assumes it rather than carrying an xAPIC
//! fallback (decision log 2026-06-26; `docs/architecture/scheduler.md` §x2APIC).
//! Firmware hands off in xAPIC mode (`IA32_APIC_BASE` = enabled, EXTD clear);
//! [`init`] sets EXTD (bit 10) to enter x2APIC, via the SDM-mandated
//! enabled→x2APIC two-step (the direct disabled→x2APIC transition `#GP`s). The dev
//! loop runs QEMU ≥ 9.0 with `+x2apic` (TCG only emulates x2APIC from 9.0). IPIs
//! use the single 64-bit ICR MSR (`0x830`) — one atomic write, no ICR-high/low
//! two-step and no delivery-status poll.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::cpu::ArchCpu;
use crate::arch::irq::{ArchIrq, SPURIOUS_VECTOR};
use crate::arch::x86_64::cpu::X86Cpu;
use crate::arch::x86_64::regs;
use crate::libkern::AllocError;

/// `IA32_APIC_BASE` — APIC global-enable + x2APIC-enable bits live here.
const MSR_IA32_APIC_BASE: u32 = 0x1B;
/// Bit 11 — APIC global enable (firmware usually sets it; preserve it).
const APIC_BASE_GLOBAL_ENABLE: u64 = 1 << 11;
/// Bit 10 — x2APIC enable (`EXTD`). Set on top of global-enable to enter x2APIC.
const APIC_BASE_X2APIC_ENABLE: u64 = 1 << 10;

/// x2APIC MSR window base: xAPIC offset `reg` → MSR `X2APIC_MSR_BASE + (reg >> 4)`.
const X2APIC_MSR_BASE: u32 = 0x800;
/// The Interrupt Command Register as a single 64-bit MSR (= `0x800 + 0x300>>4`).
const MSR_X2APIC_ICR: u32 = 0x830;

// --- Local-APIC register offsets (Intel SDM Vol.3 Table 11-1; xAPIC MMIO offsets,
//     reused here as `0x800 + offset>>4` MSR indices) ------------------------
/// Local APIC ID register. In x2APIC the full 32-bit id is the register value
/// (no `>> 24` as in xAPIC's 8-bit id).
const REG_APICID: u64 = 0x20;
/// Task-Priority Register.
const REG_TPR: u64 = 0x80;
/// End-Of-Interrupt register (write 0).
const REG_EOI: u64 = 0xB0;
/// Spurious-Interrupt-Vector register.
const REG_SVR: u64 = 0xF0;
/// SVR bit 8 — APIC software enable.
const SVR_SOFTWARE_ENABLE: u32 = 1 << 8;

// --- ICR (IPI) low-dword fields. Delivery mode Fixed (000) + physical dest (0)
//     are all-zero; `level = assert` is set explicitly. ----------------------
/// ICR level bit (14) — assert. Required form for a fixed-delivery IPI.
const ICR_ASSERT: u64 = 1 << 14;

// --- LAPIC timer registers -------------------------------------------------
//
// Programmed by the timekeeping impl (`arch::x86_64::timer`); exposed here as
// `pub(crate)` (with the accessor shims below) so `apic.rs` stays the single
// owner of the register-access logic.
/// LVT Timer entry: vector (bits 0–7), delivery mask (bit 16), and timer mode
/// (bits 18:17 — `00` one-shot, `01` periodic, `10` TSC-deadline).
pub(crate) const REG_LVT_TIMER: u64 = 0x320;
/// Timer Initial Count — writing a non-zero value (re)starts the countdown.
pub(crate) const REG_TIMER_INIT_COUNT: u64 = 0x380;
/// Timer Current Count — the live, read-only countdown value.
pub(crate) const REG_TIMER_CUR_COUNT: u64 = 0x390;
/// Timer Divide Configuration — divides the input clock before the countdown.
pub(crate) const REG_TIMER_DIV_CONFIG: u64 = 0x3E0;

/// `true` once [`init`] has put this CPU's local APIC into x2APIC mode. The
/// precondition the timekeeping impl asserts before programming the timer.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Map a xAPIC register offset to its x2APIC MSR index.
#[inline]
fn reg_to_msr(reg: u64) -> u32 {
    X2APIC_MSR_BASE + (reg >> 4) as u32
}

/// Read a local-APIC register by (xAPIC) offset via its x2APIC MSR.
///
/// # Safety
/// [`init`] must have run (x2APIC enabled); `reg` a valid register offset.
unsafe fn read_reg(reg: u64) -> u32 {
    debug_assert!(ENABLED.load(Ordering::Relaxed), "LAPIC accessed before init");
    // SAFETY: x2APIC register MSRs are readable once EXTD is set; `reg_to_msr`
    // yields a valid x2APIC register index. The low 32 bits are the value.
    unsafe { regs::rdmsr(reg_to_msr(reg)) as u32 }
}

/// Write a local-APIC register by (xAPIC) offset via its x2APIC MSR.
///
/// # Safety
/// As [`read_reg`].
unsafe fn write_reg(reg: u64, val: u32) {
    debug_assert!(ENABLED.load(Ordering::Relaxed), "LAPIC accessed before init");
    // SAFETY: as `read_reg`, for a register write.
    unsafe { regs::wrmsr(reg_to_msr(reg), val as u64) };
}

/// LAPIC register read for sibling arch modules (the timekeeping impl programs
/// the timer LVT through this). Forwards to the private [`read_reg`].
///
/// # Safety
/// As [`read_reg`]: [`init`] must have run; `reg` a valid register offset.
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

/// `true` once [`init`] has enabled x2APIC on this CPU — the precondition the
/// timekeeping impl asserts before programming the timer registers.
pub(crate) fn is_initialised() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Enable x2APIC on the running CPU and software-enable the local APIC. Shared by
/// the BSP ([`XApic::init`]) and each AP's bring-up. Idempotent per CPU.
///
/// # Safety
/// Ring-0. The CPU must advertise x2APIC (asserted here); `IA32_APIC_BASE` is
/// architectural. Runs once per CPU before that CPU programs its timer / EOIs.
pub(crate) unsafe fn enable_this_cpu() {
    assert!(
        X86Cpu::has_apic(),
        "no on-chip local APIC — the kernel requires one (CPUID.01H:EDX.9)."
    );
    assert!(has_x2apic(), "CPU lacks x2APIC (CPUID.01H:ECX.21) — required.");

    // SAFETY: IA32_APIC_BASE is architectural. If not already in x2APIC, enter it
    // via the SDM-mandated two-step: first the *xAPIC*-enabled state (global-enable
    // set, EXTD clear), then set EXTD. The direct disabled→x2APIC transition and
    // the reverse x2APIC→xAPIC transition both `#GP`, so we must not clear EXTD
    // when it is already set (e.g. firmware/Limine left the CPU in x2APIC).
    unsafe {
        let base = regs::rdmsr(MSR_IA32_APIC_BASE);
        if base & APIC_BASE_X2APIC_ENABLE == 0 {
            regs::wrmsr(
                MSR_IA32_APIC_BASE,
                (base | APIC_BASE_GLOBAL_ENABLE) & !APIC_BASE_X2APIC_ENABLE,
            );
            regs::wrmsr(
                MSR_IA32_APIC_BASE,
                base | APIC_BASE_GLOBAL_ENABLE | APIC_BASE_X2APIC_ENABLE,
            );
        }
    }
    ENABLED.store(true, Ordering::Relaxed);

    // Software-enable + program the spurious vector; accept all priorities.
    // SAFETY: x2APIC is now enabled on this CPU.
    unsafe {
        write_reg(REG_SVR, SVR_SOFTWARE_ENABLE | SPURIOUS_VECTOR as u32);
        write_reg(REG_TPR, 0);
    }
}

/// Send a fixed-delivery IPI of `vector` to the CPU whose x2APIC id is `target`.
///
/// # Safety
/// Ring-0; x2APIC must be enabled on the calling CPU. The single 64-bit ICR MSR
/// write issues the IPI atomically — no ICR-high/low split, no delivery-status
/// poll (x2APIC removes it).
pub(crate) unsafe fn send_ipi(target: u32, vector: u8) {
    // ICR = dest[63:32] | assert | (delivery Fixed=000, physical dest=0) | vector.
    let icr = ((target as u64) << 32) | ICR_ASSERT | (vector as u64);
    // SAFETY: writing the x2APIC ICR MSR is the architected way to send an IPI;
    // valid in ring 0 once x2APIC is enabled.
    unsafe { regs::wrmsr(MSR_X2APIC_ICR, icr) };
}

/// `true` if the CPU advertises x2APIC (`CPUID.01H:ECX[21]`).
fn has_x2apic() -> bool {
    let (_, _, ecx, _) = regs::cpuid(1, 0);
    ecx & (1 << 21) != 0
}

/// The x86_64 [`ArchIrq`] implementation (x2APIC). Zero-sized; re-exported as
/// `crate::arch::Irq`.
pub struct XApic;

impl ArchIrq for XApic {
    unsafe fn init() -> Result<(), AllocError> {
        // SAFETY: ring-0 boot path on the BSP; enables x2APIC + software-enables
        // the local APIC. No allocation occurs (x2APIC needs no MMIO mapping), so
        // this no longer fails — the `Result` is kept for the trait signature.
        unsafe { enable_this_cpu() };
        Ok(())
    }

    unsafe fn eoi() {
        // SAFETY: ring-0 write of 0 to the EOI register MSR; valid after `init()`
        // (the caller's contract).
        unsafe { write_reg(REG_EOI, 0) };
    }

    fn id() -> u32 {
        // SAFETY: the ID register MSR is readable once x2APIC is enabled; the read
        // has no side effects. In x2APIC the value is the full 32-bit id.
        unsafe { read_reg(REG_APICID) }
    }
}
