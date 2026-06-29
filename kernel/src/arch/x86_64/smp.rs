//! x86_64 [`ArchSmp`] impl. Slice 0 is single-CPU, but `current_cpu` is read
//! from the hardware (`RDTSCP` → `IA32_TSC_AUX`) rather than hard-coded, so the
//! per-CPU substrate is correct the moment APs run (slice 1). AP bring-up and the
//! real IPI (the local-APIC ICR) land in slice 1.

use crate::arch::smp::ArchSmp;
use crate::arch::x86_64::regs;

/// `IA32_TSC_AUX` — the CPU programs its dense logical index here at init;
/// `RDTSCP`/`RDPID` read it back. Writing it does not affect the TSC.
const MSR_TSC_AUX: u32 = 0xC000_0103;

/// The x86_64 [`ArchSmp`] implementation. Re-exported as `crate::arch::Smp`.
pub struct X86Smp;

impl ArchSmp for X86Smp {
    fn cpu_count() -> usize {
        // Slice 0 is single-CPU; the real count is learned from Limine's SMP
        // response at bring-up (slice 1).
        1
    }

    #[cfg(not(test))]
    fn current_cpu() -> u32 {
        // The dense logical index this CPU stored in IA32_TSC_AUX (see
        // `init_this_cpu`). Reset-default 0, so the BSP reads 0 even before init.
        regs::rdtscp_aux()
    }

    #[cfg(test)]
    fn current_cpu() -> u32 {
        // Host tests model a single CPU. RDTSCP under `cargo test` would return the
        // *host's* (unbounded) CPU id, overflowing per-CPU arrays sized to MAX_CPUS,
        // so report 0 — matching the pre-RDTSCP single-CPU behavior.
        0
    }

    fn init_this_cpu(index: u32) {
        // SAFETY: IA32_TSC_AUX is architectural on every x86-64 CPU; writing the
        // logical index there only sets the value RDTSCP/RDPID return, leaving the
        // timestamp counter untouched. Ring-0 wrmsr.
        unsafe { regs::wrmsr(MSR_TSC_AUX, index as u64) };
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
    fn single_cpu_reports_one_cpu() {
        assert_eq!(X86Smp::cpu_count(), 1);
        // `current_cpu` (RDTSCP) and `init_this_cpu` (wrmsr) are ring-0 hardware
        // ops that read/write the *host's* IA32_TSC_AUX under `cargo test`, so the
        // dense-index round-trip is verified under QEMU, not host-side.
    }
}
