//! x86_64 CPU feature-detection and control ([`ArchCpu`] impl).

use core::arch::asm;

use crate::arch::cpu::ArchCpu;
use crate::arch::x86_64::regs;

/// CPUID.01H:EDX bit 9 — on-chip local APIC present.
const CPUID_1_EDX_APIC: u32 = 1 << 9;

/// The x86_64 [`ArchCpu`] implementation. Zero-sized; re-exported as
/// `crate::arch::Cpu`.
pub struct X86Cpu;

impl ArchCpu for X86Cpu {
    fn has_apic() -> bool {
        let (_, _, _, edx) = regs::cpuid(1, 0);
        edx & CPUID_1_EDX_APIC != 0
    }

    unsafe fn halt() {
        // SAFETY: `hlt` is a ring-0 instruction with no memory side effects;
        // it parks the CPU until the next interrupt. The caller owns the
        // interrupt-flag state that governs wake-up (see the trait contract).
        unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}
