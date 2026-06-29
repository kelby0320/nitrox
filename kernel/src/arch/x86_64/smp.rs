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

    unsafe fn send_ipi(target: u32, vector: u8) {
        // Fixed-delivery IPI via the local APIC's ICR (a single 64-bit x2APIC MSR
        // write). `target` is the destination CPU's x2APIC id.
        // SAFETY: ring-0; x2APIC is enabled on the calling CPU by the boot/AP
        // bring-up path before any IPI is sent.
        unsafe { crate::arch::x86_64::apic::send_ipi(target, vector) };
    }
}

/// Per-CPU architecture bring-up for an application processor, run on the AP
/// itself at the start of its entry. [`X86Smp::init_this_cpu`] must already have
/// established this CPU's logical index (so `current_cpu()` is correct for the
/// per-CPU GDT/TSS, syscall block, etc.). Loads this CPU's GDT/TSS, the shared
/// IDT, the memory protections (NX/SMEP/SMAP), enters x2APIC, and arms the syscall
/// MSRs (`KERNEL_GS_BASE` → this CPU's block). Leaves interrupts masked.
pub fn ap_cpu_init() {
    use crate::arch::cpu::ArchCpu;
    super::gdt::init();
    super::idt::load();
    super::cpu::X86Cpu::init_protections();
    // SAFETY: ring-0 AP bring-up path; the CPU advertises x2APIC (asserted inside
    // `enable_this_cpu`), and this runs before the AP touches the local APIC.
    unsafe { super::apic::enable_this_cpu() };
    super::syscall::init_syscall_entry();
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
