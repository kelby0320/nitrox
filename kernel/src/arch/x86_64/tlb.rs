//! x86_64 TLB-shootdown transport: the IPI that invalidates stale translations on remote CPUs.
//!
//! The protocol is in [`crate::tlb`]. x86 keeps page tables cache-coherent across CPUs but
//! not the per-CPU TLB, so after a page-table edit other CPUs must be told to invalidate.

/// IPI vector for TLB shootdown, below the spurious vector (`0xFF`). A *returning* interrupt
/// (EOI + `iretq`).
pub const TLB_SHOOTDOWN_VECTOR: u8 = 0x40;

/// Send the TLB-shootdown IPI to the CPU with dense index `cpu`.
///
/// No-op if `cpu` is out of range or unbound: an unbound or parked core holds no live
/// translations.
pub fn send_shootdown_ipi(cpu: usize) {
    if let Some(apic) = super::smp::apic_of_dense(cpu) {
        // SAFETY: ring-0; x2APIC is enabled on this CPU (a shootdown is only
        // initiated well after bring-up); a fixed-delivery IPI of a valid vector.
        unsafe { super::apic::send_ipi(apic, TLB_SHOOTDOWN_VECTOR) };
    }
}
