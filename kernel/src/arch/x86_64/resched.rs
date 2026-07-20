//! x86_64 reschedule-IPI transport: the inter-processor interrupt one CPU sends
//! to another to make it re-run the scheduler *now* — used when a thread is made
//! runnable on a **remote** CPU (a cross-CPU wake, or a spawn/placement onto an
//! otherwise-idle CPU). The coordination policy (who to poke, and what the target
//! does with it) is architecture-neutral and lives in [`crate::sched`]; this module
//! only knows how to *reach* another CPU, resolving its x2APIC id through the
//! bring-up identity map ([`super::smp::apic_of_dense`]).
//!
//! Without this, a woken thread parked on an idle CPU's run queue would not run
//! until that CPU's next periodic tick (a latency floor at best, and — since a
//! halted vCPU's self-timer is not a dependable wake source — a lost wakeup at
//! worst). The IPI makes cross-CPU wake delivery immediate and reliable: an
//! incoming IPI resumes a halted CPU and drives the reschedule directly.

/// IPI vector for a remote reschedule. In the free range above the device-IRQ
/// vectors (`0x30..=0x37`) and the TLB-shootdown vector (`0x40`), below the
/// spurious vector (`0xFF`). A *returning* interrupt (EOI + `iretq`), like the
/// timer and the shootdown IPI.
pub const RESCHEDULE_VECTOR: u8 = 0x41;

/// Send the reschedule IPI to the CPU with dense index `cpu`, resolving its
/// x2APIC id through the bring-up identity map. No-op if `cpu` is out of range or
/// unbound (a parked/unbound core runs no threads, so poking it is pointless).
pub fn send_reschedule_ipi(cpu: usize) {
    if let Some(apic) = super::smp::apic_of_dense(cpu) {
        // SAFETY: ring-0; x2APIC is enabled on this CPU (reschedules are only sent
        // well after bring-up); a fixed-delivery IPI of a valid vector.
        unsafe { super::apic::send_ipi(apic, RESCHEDULE_VECTOR) };
    }
}
