//! x86_64 [`ArchSmp`] impl. Slice 0 is single-CPU, but `current_cpu` is read
//! from the hardware (`RDTSCP` → `IA32_TSC_AUX`) rather than hard-coded, so the
//! per-CPU substrate is correct the moment APs run (slice 1). AP bring-up and the
//! real IPI (the local-APIC ICR) land in slice 1.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch::smp::{ArchSmp, MAX_CPUS};
use crate::arch::x86_64::regs;

/// `IA32_TSC_AUX` — the CPU programs its dense logical index here at init;
/// `RDTSCP`/`RDPID` read it back. Writing it does not affect the TSC.
const MSR_TSC_AUX: u32 = 0xC000_0103;

/// Sentinel for an unbound dense slot in [`DENSE_TO_APIC`].
const APIC_UNSET: u32 = u32::MAX;

/// Dense-index → hardware-APIC-id map. The BSP fills this once, before launching
/// any AP, from Limine's CPU list ([`bind_cpu_identity`]); each core then adopts
/// **its own** dense index by matching its hardware APIC id ([`adopt_dense_index`])
/// rather than trusting a value handed to it. This makes dense indices unique and
/// stable by construction — a core can never end up sharing another core's index
/// (and thus its per-CPU GDT/TSS/scheduler slots), which is the failure mode a
/// racy `extra_argument` / reset-default-0 scheme allowed.
static DENSE_TO_APIC: [AtomicU32; MAX_CPUS] =
    [const { AtomicU32::new(APIC_UNSET) }; MAX_CPUS];

/// This core's hardware APIC id, read from `CPUID.01H:EBX[31:24]` (the initial
/// xAPIC id). Available on every core from its first instruction — before x2APIC
/// is enabled or `IA32_TSC_AUX` is set — and unique per core. Sufficient while
/// `MAX_CPUS <= 255` (APIC ids fit in 8 bits); a >255-CPU system would read the
/// 32-bit x2APIC id from `CPUID.0BH` instead.
fn hw_apic_id() -> u32 {
    let (_, ebx, _, _) = regs::cpuid(1, 0);
    ebx >> 24
}

/// Bind dense index `dense` to hardware APIC id `apic`. Called by the BSP for every
/// CPU (itself + each AP) **before** any AP is launched, so the map is fully
/// populated when APs adopt their indices.
pub fn bind_cpu_identity(dense: u32, apic: u32) {
    if (dense as usize) < MAX_CPUS {
        DENSE_TO_APIC[dense as usize].store(apic, Ordering::Release);
    }
}

/// Set the running core's dense index in `IA32_TSC_AUX` by looking up its hardware
/// APIC id in [`DENSE_TO_APIC`]. Returns the dense index, or `None` if this core's
/// APIC id was never bound (a bring-up bug) — the caller must **not** run with a
/// default/guessed index in that case, as it would collide with another core.
pub fn adopt_dense_index() -> Option<u32> {
    let apic = hw_apic_id();
    for i in 0..MAX_CPUS {
        if DENSE_TO_APIC[i].load(Ordering::Acquire) == apic {
            // SAFETY: `IA32_TSC_AUX` is architectural; writing this core's dense
            // index only sets what RDTSCP/RDPID return. Ring-0 wrmsr.
            unsafe { regs::wrmsr(MSR_TSC_AUX, i as u64) };
            return Some(i as u32);
        }
    }
    None
}

/// The x2APIC id bound to dense index `cpu`, or `None` if out of range / unbound.
/// Used by the TLB-shootdown transport ([`super::tlb::send_shootdown_ipi`]) to
/// target a CPU by its dense index — reusing the same hardware-identity map that
/// [`bind_cpu_identity`] populated at bring-up.
pub(crate) fn apic_of_dense(cpu: usize) -> Option<u32> {
    if cpu >= MAX_CPUS {
        return None;
    }
    match DENSE_TO_APIC[cpu].load(Ordering::Acquire) {
        APIC_UNSET => None,
        apic => Some(apic),
    }
}

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
