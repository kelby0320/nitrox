//! x86_64 local-APIC bring-up in **x2APIC** (MSR) mode ([`ArchIrq`] impl).
//!
//! The xAPIC MMIO offset `reg` maps to MSR `0x800 + (reg >> 4)`; no MMIO page is mapped and no
//! ACPI/MADT is needed (`docs/rationale/why-phased-acpi.md`). The kernel is x2APIC-only (the
//! x86-64-v2 baseline guarantees it; decision log 2026-06-26, `docs/architecture/scheduler.md`
//! §x2APIC). Firmware hands off in xAPIC mode, so [`init`] sets EXTD (bit 10) via the
//! SDM-mandated enabled→x2APIC two-step (disabled→x2APIC `#GP`s). QEMU needs ≥ 9.0 with
//! `+x2apic`. IPIs are one atomic write to the 64-bit ICR MSR (`0x830`), with no delivery-status
//! poll.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::cpu::ArchCpu;
use crate::arch::irq::{ArchIrq, SPURIOUS_VECTOR};
use crate::arch::x86_64::cpu::X86Cpu;
use crate::arch::x86_64::regs;
use crate::libkern::AllocError;

const MSR_IA32_APIC_BASE: u32 = 0x1B;
/// Bit 11 — APIC global enable (firmware usually sets it; preserved).
const APIC_BASE_GLOBAL_ENABLE: u64 = 1 << 11;
const APIC_BASE_X2APIC_ENABLE: u64 = 1 << 10;

const X2APIC_MSR_BASE: u32 = 0x800;
const MSR_X2APIC_ICR: u32 = 0x830;
/// The x2APIC **SELF IPI** register: writing a vector sends it, fixed and edge-triggered, to
/// this CPU alone.
const MSR_X2APIC_SELF_IPI: u32 = 0x83F;

// --- Local-APIC register offsets (Intel SDM Vol.3 Table 11-1; xAPIC MMIO offsets,
//     reused as `0x800 + offset>>4` MSR indices) ---
/// Local APIC ID register; in x2APIC the full 32-bit id (no `>> 24`).
const REG_APICID: u64 = 0x20;
const REG_TPR: u64 = 0x80;
const REG_EOI: u64 = 0xB0;
const REG_SVR: u64 = 0xF0;
const SVR_SOFTWARE_ENABLE: u32 = 1 << 8;

// --- ICR (IPI) low-dword fields. Fixed delivery (000) and physical dest (0) are zero. ---
/// ICR level bit (14) — assert; required for a fixed-delivery IPI.
const ICR_ASSERT: u64 = 1 << 14;

// --- LAPIC timer registers, programmed by `arch::x86_64::timer` through the shims below ---
/// LVT Timer entry: vector (bits 0–7), mask (bit 16), mode (bits 18:17: `00` one-shot, `01`
/// periodic, `10` TSC-deadline).
pub(crate) const REG_LVT_TIMER: u64 = 0x320;
/// Timer Initial Count — writing a non-zero value (re)starts the countdown.
pub(crate) const REG_TIMER_INIT_COUNT: u64 = 0x380;
/// Timer Current Count — the live, read-only countdown value.
pub(crate) const REG_TIMER_CUR_COUNT: u64 = 0x390;
/// Timer Divide Configuration — divides the input clock before the countdown.
pub(crate) const REG_TIMER_DIV_CONFIG: u64 = 0x3E0;

static ENABLED: AtomicBool = AtomicBool::new(false);

#[inline]
fn reg_to_msr(reg: u64) -> u32 {
    X2APIC_MSR_BASE + (reg >> 4) as u32
}

/// Read a local-APIC register by xAPIC offset via its x2APIC MSR.
///
/// # Safety
/// [`init`] must have run (x2APIC enabled); `reg` a valid register offset.
unsafe fn read_reg(reg: u64) -> u32 {
    debug_assert!(ENABLED.load(Ordering::Relaxed), "LAPIC accessed before init");
    // SAFETY: x2APIC register MSRs are readable once EXTD is set; `reg_to_msr`
    // yields a valid x2APIC register index. The low 32 bits are the value.
    unsafe { regs::rdmsr(reg_to_msr(reg)) as u32 }
}

/// Write a local-APIC register by xAPIC offset via its x2APIC MSR.
///
/// # Safety
/// As [`read_reg`].
unsafe fn write_reg(reg: u64, val: u32) {
    debug_assert!(ENABLED.load(Ordering::Relaxed), "LAPIC accessed before init");
    // SAFETY: as `read_reg`, for a register write.
    unsafe { regs::wrmsr(reg_to_msr(reg), val as u64) };
}

/// LAPIC register read for sibling arch modules (the timer impl). Forwards to [`read_reg`].
///
/// # Safety
/// As [`read_reg`]: [`init`] must have run; `reg` a valid register offset.
pub(crate) unsafe fn read_reg_shared(reg: u64) -> u32 {
    // SAFETY: forwarded under the caller's contract.
    unsafe { read_reg(reg) }
}

/// LAPIC register write for sibling arch modules. Forwards to [`write_reg`].
///
/// # Safety
/// As [`write_reg`].
pub(crate) unsafe fn write_reg_shared(reg: u64, val: u32) {
    // SAFETY: forwarded under the caller's contract.
    unsafe { write_reg(reg, val) };
}

/// `true` once [`init`] has enabled x2APIC on this CPU.
pub(crate) fn is_initialised() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Enable x2APIC on the running CPU and software-enable the local APIC. Shared by the BSP
/// ([`XApic::init`]) and each AP's bring-up. Idempotent per CPU.
///
/// # Safety
/// Ring-0. The CPU must advertise x2APIC (asserted here); `IA32_APIC_BASE` is
/// architectural. Runs once per CPU before that CPU programs its timer / EOIs.
pub(crate) unsafe fn enable_this_cpu() {
    assert!(X86Cpu::has_apic(), "no on-chip local APIC — the kernel requires one (CPUID.01H:EDX.9).");
    assert!(has_x2apic(), "CPU lacks x2APIC (CPUID.01H:ECX.21) — required.");

    // SAFETY: IA32_APIC_BASE is architectural. If not already in x2APIC, enter it
    // via the SDM-mandated two-step: first the *xAPIC*-enabled state (global-enable
    // set, EXTD clear), then set EXTD. The direct disabled→x2APIC transition and
    // the reverse x2APIC→xAPIC transition both `#GP`, so we must not clear EXTD
    // when it is already set (e.g. firmware/Limine left the CPU in x2APIC).
    unsafe {
        let base = regs::rdmsr(MSR_IA32_APIC_BASE);
        if base & APIC_BASE_X2APIC_ENABLE == 0 {
            regs::wrmsr(MSR_IA32_APIC_BASE, (base | APIC_BASE_GLOBAL_ENABLE) & !APIC_BASE_X2APIC_ENABLE);
            regs::wrmsr(MSR_IA32_APIC_BASE, base | APIC_BASE_GLOBAL_ENABLE | APIC_BASE_X2APIC_ENABLE);
        }
    }
    ENABLED.store(true, Ordering::Relaxed);

    // Software-enable, program the spurious vector, accept all priorities.
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

/// Send an **NMI** to `target`, whatever that CPU is doing: the only delivery mode that reaches
/// a CPU spinning on a lock with **interrupts masked**, which a Fixed IPI would not, so the
/// only one usable for stopping the machine. The vector field is ignored (the target takes
/// vector 2), so it is left zero.
///
/// # Safety
/// Ring-0; x2APIC must be enabled on the calling CPU ([`x2apic_enabled_here`]).
pub(crate) unsafe fn send_nmi(target: u32) {
    // Delivery mode is ICR[10:8]; NMI is 0b100.
    const ICR_DELIVERY_NMI: u64 = 0b100 << 8;
    let icr = ((target as u64) << 32) | ICR_ASSERT | ICR_DELIVERY_NMI;
    // SAFETY: writing the x2APIC ICR MSR is the architected way to send an IPI;
    // valid in ring 0 once x2APIC is enabled.
    unsafe { regs::wrmsr(MSR_X2APIC_ICR, icr) };
}

/// `true` if **this** CPU has entered x2APIC mode, so `wrmsr` to the ICR is legal.
///
/// Unlike [`has_x2apic`] (what the part supports): an AP that has not yet run `ap_cpu_init`
/// supports x2APIC without having enabled it, and the ICR write `#GP`s; early boot is where
/// a panicking core is in that state.
pub(crate) fn x2apic_enabled_here() -> bool {
    const WANT: u64 = APIC_BASE_GLOBAL_ENABLE | APIC_BASE_X2APIC_ENABLE;
    // SAFETY: reading `IA32_APIC_BASE` is valid in ring 0 on any CPU with an APIC.
    let base = unsafe { regs::rdmsr(MSR_IA32_APIC_BASE) };
    base & WANT == WANT
}

fn has_x2apic() -> bool {
    let (_, _, ecx, _) = regs::cpuid(1, 0);
    ecx & (1 << 21) != 0
}

/// The x86_64 [`ArchIrq`] implementation (x2APIC); re-exported as `crate::arch::Irq`.
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

    unsafe fn raise_on_self(vector: u8) {
        // SAFETY: writing the SELF IPI MSR is the architected way to interrupt this CPU; valid
        // in ring 0 once x2APIC is enabled here (the caller's contract), and the caller
        // guarantees `vector` has a handler.
        unsafe { regs::wrmsr(MSR_X2APIC_SELF_IPI, vector as u64) };
    }
}
