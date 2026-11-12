//! x86_64 [`ArchSmp`] impl: `current_cpu` is read from `RDTSCP` → `IA32_TSC_AUX`.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch::percpu::{CpuIndex, CpuPin};
use crate::arch::smp::{ArchSmp, MAX_CPUS};
use crate::arch::x86_64::regs;

/// `IA32_TSC_AUX`: each CPU's dense index; `RDTSCP`/`RDPID` read it back. Does not affect the TSC.
#[cfg(not(test))]
const MSR_TSC_AUX: u32 = 0xC000_0103;

const APIC_UNSET: u32 = u32::MAX;

/// Dense-index → hardware-APIC-id map. The BSP fills it before launching any AP
/// ([`bind_cpu_identity`]); each core then adopts **its own** index by matching its APIC id
/// ([`adopt_dense_index`]) rather than trusting a handed-in value, so no two cores share an
/// index (and thus per-CPU GDT/TSS/scheduler slots).
static DENSE_TO_APIC: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(APIC_UNSET) }; MAX_CPUS];

/// This core's hardware APIC id, from `CPUID.01H:EBX[31:24]`, available before x2APIC or
/// `IA32_TSC_AUX` is set. Sufficient while `MAX_CPUS <= 255`; beyond that `CPUID.0BH` is needed.
pub(super) fn hw_apic_id() -> u32 {
    let (_, ebx, _, _) = regs::cpuid(1, 0);
    ebx >> 24
}

/// Bind dense index `dense` to hardware APIC id `apic`, for every CPU **before** any AP is
/// launched. Refused if either is bound to another, as two cores could then adopt one index.
pub fn bind_cpu_identity(dense: u32, apic: u32) {
    let bound: [u32; MAX_CPUS] = core::array::from_fn(|i| DENSE_TO_APIC[i].load(Ordering::Acquire));
    assert!(binding_allowed(&bound, dense as usize, apic), "smp: index {dense} or APIC id {apic:#x} bound already");
    if (dense as usize) < MAX_CPUS {
        DENSE_TO_APIC[dense as usize].store(apic, Ordering::Release);
    }
}

/// Whether binding `dense` to `apic` leaves each index on one core.
fn binding_allowed(bound: &[u32], dense: usize, apic: u32) -> bool {
    bound.iter().enumerate().all(|(i, &a)| a == APIC_UNSET || (i == dense) == (a == apic))
}

/// Set the running core's dense index in `IA32_TSC_AUX` by looking up its APIC id. `None` if
/// never bound; the caller must **not** run with a guessed index, which would collide.
pub fn adopt_dense_index() -> Option<CpuIndex> {
    let apic = hw_apic_id();
    let i = DENSE_TO_APIC.iter().position(|slot| slot.load(Ordering::Acquire) == apic)?;
    let index = CpuIndex::new(i)?;
    set_index(index);
    Some(index)
}

fn set_index(index: CpuIndex) {
    #[cfg(not(test))]
    // SAFETY: IA32_TSC_AUX is architectural on every x86-64 CPU; writing the
    // logical index there only sets the value RDTSCP/RDPID return, leaving the
    // timestamp counter untouched. Ring-0 wrmsr.
    unsafe {
        regs::wrmsr(MSR_TSC_AUX, index.get() as u64)
    };
    #[cfg(test)]
    let _ = index;
}

/// The dense logical index (`0..MAX_CPUS`) of the CPU this runs on — the
/// unpinned primitive [`CpuIndex::current`] pins.
/// Read from the hardware, not hard-coded, so it is correct on every running CPU.
#[cfg(not(test))]
pub(in crate::arch) fn current_cpu() -> u32 {
    regs::rdtscp_aux()
}

#[cfg(test)]
pub(in crate::arch) fn current_cpu() -> u32 {
    // RDTSCP under `cargo test` returns the *host's* CPU id, which would overflow per-CPU
    // arrays sized to MAX_CPUS.
    0
}

/// The x2APIC id bound to dense index `cpu`, or `None` if out of range or unbound.
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
        // Not learned from Limine's SMP response: TODO(smp-cpu-count).
        1
    }

    #[cfg(not(test))]
    fn bound_index() -> Option<CpuIndex> {
        // An unbound core reads the reset default 0 and would otherwise pass for the BSP, so
        // the index must map back to *this* core's APIC id.
        let cpu = current_cpu() as usize;
        CpuIndex::new(cpu).filter(|_| apic_of_dense(cpu) == Some(hw_apic_id()))
    }

    #[cfg(test)]
    fn bound_index() -> Option<CpuIndex> {
        // Host tests model one CPU whose identity is by definition its own.
        Some(CpuIndex::BSP)
    }

    fn adopt_boot_index() -> CpuIndex {
        bind_cpu_identity(0, hw_apic_id());
        set_index(CpuIndex::BSP);
        CpuIndex::BSP
    }

    unsafe fn send_ipi(target: u32, vector: u8) {
        // `target` is the destination CPU's x2APIC id.
        // SAFETY: ring-0; x2APIC is enabled on the calling CPU by the boot/AP
        // bring-up path before any IPI is sent.
        unsafe { crate::arch::x86_64::apic::send_ipi(target, vector) };
    }
}

/// Per-CPU bring-up for an application processor, run on the AP itself.
///
/// [`adopt_dense_index`] must already have set this CPU's index, so `CpuIndex::current` is
/// correct for the per-CPU GDT/TSS and syscall block. Loads the GDT/TSS and shared IDT, enables
/// NX/WP/SMEP/SMAP, enters x2APIC, and arms the syscall MSRs. Leaves interrupts masked.
pub fn ap_cpu_init(pin: &CpuPin<'_>) {
    use crate::arch::cpu::ArchCpu;
    super::gdt::init(pin);
    super::idt::load();
    super::cpu::X86Cpu::init_protections();
    // CR0/CR4/XCR0 are per-CPU. Must precede the first `context_switch` on this CPU, which
    // unconditionally restores the incoming thread's save area.
    super::fpu::init_cpu();
    // The cache-policy table is per-CPU; silent because the BSP's line already said whether the
    // platform's table matched.
    //
    // SAFETY: ring-0 AP bring-up, before this CPU makes or follows any mapping of its own.
    let _ = unsafe {
        <super::memory_types::X86MemoryTypes as crate::arch::memory_types::ArchMemoryTypes>::install_policy()
    };
    // SAFETY: ring-0 AP bring-up path; the CPU advertises x2APIC (asserted inside
    // `enable_this_cpu`), and this runs before the AP touches the local APIC.
    unsafe { super::apic::enable_this_cpu() };
    super::syscall::init_syscall_entry(pin);
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_last_index_is_the_one_below_max_cpus() {
        use crate::arch::percpu::CpuIndex;
        assert_eq!(CpuIndex::new(MAX_CPUS - 1).map(CpuIndex::get), Some(MAX_CPUS - 1));
        assert_eq!(CpuIndex::new(MAX_CPUS), None);
    }

    use super::*;

    /// A binding that would put two cores on one index is refused; the same pair again is not.
    #[test]
    fn no_index_is_bound_to_two_cores() {
        let mut bound = [APIC_UNSET; MAX_CPUS];
        (bound[0], bound[1]) = (7, 9);
        assert!(binding_allowed(&bound, 2, 11));
        assert!(binding_allowed(&bound, 0, 7));
        assert!(!binding_allowed(&bound, 0, 11), "the boot index, taken by a second core");
        assert!(!binding_allowed(&bound, 2, 9), "one core, two indices");
    }

    #[test]
    fn single_cpu_reports_one_cpu() {
        assert_eq!(X86Smp::cpu_count(), 1);
        // The dense-index round-trip touches the *host's* IA32_TSC_AUX, so QEMU verifies it.
    }
}
