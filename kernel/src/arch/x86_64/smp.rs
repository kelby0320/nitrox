//! x86_64 SMP stub ([`ArchSmp`] impl). Phase 1 is single-CPU.

use crate::arch::smp::ArchSmp;

/// The x86_64 [`ArchSmp`] stub. Re-exported as `crate::arch::Smp`.
pub struct X86Smp;

impl ArchSmp for X86Smp {
    fn cpu_count() -> usize {
        1
    }

    fn current_cpu() -> u32 {
        0
    }

    unsafe fn send_ipi(_target: u32, _vector: u8) {
        // Phase 1 is single-CPU: no second CPU exists to target, so an IPI
        // is unreachable by construction — a logic error, not a recoverable
        // condition. The real implementation (the local-APIC Interrupt
        // Command Register) lands with Phase-3 SMP bring-up.
        // TODO(phase-3-smp): implement via the local-APIC ICR.
        unimplemented!("send_ipi: SMP is Phase 3 (see docs/planning/implementation-plan.md)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_cpu_stub_reports_one_cpu() {
        assert_eq!(X86Smp::cpu_count(), 1);
        assert_eq!(X86Smp::current_cpu(), 0);
    }
}
