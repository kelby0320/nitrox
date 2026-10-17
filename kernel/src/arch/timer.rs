//! Architecture-neutral monotonic-time + per-CPU timer contract.
//!
//! [`ArchTimer`] is the kernel's source of monotonic time and the per-CPU
//! countdown timer that drives preemption (periodic) and deadline wakeups
//! (one-shot). On x86_64 this is the TSC (monotonic nanoseconds) plus the
//! local-APIC timer, both calibrated against the legacy PIT at boot; aarch64
//! would back it with the generic timer.
//!
//! [`read_ns`](ArchTimer::read_ns) is the monotonic clock;
//! [`start_periodic`](ArchTimer::start_periodic) drives the preemptive
//! scheduler tick. [`arm_oneshot_in`](ArchTimer::arm_oneshot_in) has no consumer yet.
//!
//! `crate::arch::Timer` — the *hardware* timer — is a distinct namespace from
//! `crate::object::Timer`, the waitable kernel object.

/// Monotonic time source + per-CPU countdown timer.
pub trait ArchTimer {
    /// Calibrate the monotonic-time source and the per-CPU timer against the
    /// platform reference clock, recording their frequencies and capturing a
    /// zero-point so [`read_ns`](ArchTimer::read_ns) starts near 0.
    ///
    /// Infallible: calibration cannot fail outright (a poor reference yields an
    /// imprecise, not absent, frequency); a CPU that does not advertise an
    /// invariant time source is warned about, not rejected.
    ///
    /// # Safety
    /// Ring-0 only. Call once per CPU during bring-up, **after** the local
    /// interrupt controller is up ([`crate::arch::Irq::init`]) — programming
    /// the per-CPU timer needs the controller's registers mapped.
    unsafe fn init();

    /// Monotonic nanoseconds since [`init`](ArchTimer::init). Never decreases.
    /// Safe and cheap (a counter read plus a multiply-shift); no side effects.
    fn read_ns() -> u64;

    /// Arm the per-CPU timer to fire repeatedly every `period_ns` at the
    /// kernel's timer vector.
    ///
    /// # Safety
    /// Ring-0 only; valid after [`init`](ArchTimer::init). Reconfigures
    /// interrupt-source delivery for the current CPU.
    unsafe fn start_periodic(period_ns: u64);

    /// Arm the per-CPU timer to fire **once** `delay_ns` from now at the timer
    /// vector. Relative by design — the timer is inherently a counting-down
    /// initial count.
    ///
    /// **Dormant.** Wait-queue deadlines are checked on the periodic preemption tick (the
    /// one LAPIC timer is already in periodic mode for preemption, so a concurrent one-shot
    /// isn't available), so this has no consumer. It awaits a tickless or second-timer design.
    ///
    /// # Safety
    /// As [`start_periodic`](ArchTimer::start_periodic).
    unsafe fn arm_oneshot_in(delay_ns: u64);

    /// Mask the per-CPU timer's local vector and halt its countdown so it
    /// raises nothing.
    ///
    /// # Safety
    /// Ring-0 only; valid after [`init`](ArchTimer::init).
    unsafe fn stop();

    /// Calibrated monotonic-source frequency in Hz (boot evidence / diagnostics).
    fn monotonic_hz() -> u64;

    /// Calibrated per-CPU-timer input frequency in Hz — the value the arming
    /// methods convert nanosecond intervals against (diagnostics).
    fn timer_hz() -> u64;
}
