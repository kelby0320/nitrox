//! x86_64 TLB-shootdown transport: the inter-processor interrupt used to
//! invalidate stale translations on remote CPUs. The coordination protocol (who
//! to notify, waiting for acknowledgement) is architecture-neutral and lives in
//! [`crate::tlb`]; this module only knows how to *reach* another CPU (send the
//! IPI). Destinations are resolved through the hardware-identity map built at
//! bring-up ([`super::smp::apic_of_dense`]), so no separate table is kept here.
//!
//! x86 keeps memory (including page tables) cache-coherent across CPUs, but does
//! **not** keep the per-CPU TLB / paging-structure caches coherent: after one CPU
//! edits a page table, other CPUs may still hold stale translations until told to
//! invalidate. This module carries that "tell them" signal.

/// IPI vector for TLB shootdown. In the free range above the device-IRQ vectors
/// (`0x30..=0x37`) and below the spurious vector (`0xFF`). A *returning*
/// interrupt (EOI + `iretq`), like the timer.
pub const TLB_SHOOTDOWN_VECTOR: u8 = 0x40;

/// Send the TLB-shootdown IPI to the CPU with dense index `cpu`, resolving its
/// x2APIC id through the bring-up identity map. No-op if `cpu` is out of range or
/// unbound (an unbound/parked core holds no live translations, so skipping it is
/// safe).
pub fn send_shootdown_ipi(cpu: usize) {
    if let Some(apic) = super::smp::apic_of_dense(cpu) {
        // SAFETY: ring-0; x2APIC is enabled on this CPU (a shootdown is only
        // initiated well after bring-up); a fixed-delivery IPI of a valid vector.
        unsafe { super::apic::send_ipi(apic, TLB_SHOOTDOWN_VECTOR) };
    }
}
