//! x86_64 reschedule-IPI transport: reaches another CPU so it re-runs the scheduler *now*.
//!
//! Policy lives in [`crate::sched`]. Without the IPI a thread woken onto an idle CPU would wait
//! for that CPU's next tick, and a halted vCPU's self-timer is not a dependable wake source.

/// IPI vector for a remote reschedule, between the shootdown vector (`0x40`) and the spurious
/// vector (`0xFF`). A *returning* interrupt (EOI + `iretq`).
pub const RESCHEDULE_VECTOR: u8 = 0x41;

/// Send the reschedule IPI to the CPU with dense index `cpu`.
///
/// No-op if `cpu` is out of range or unbound: a parked core runs no threads.
pub fn send_reschedule_ipi(cpu: usize) {
    if let Some(apic) = super::smp::apic_of_dense(cpu) {
        // SAFETY: ring-0; x2APIC is enabled on this CPU (reschedules are only sent
        // well after bring-up); a fixed-delivery IPI of a valid vector.
        unsafe { super::apic::send_ipi(apic, RESCHEDULE_VECTOR) };
    }
}
