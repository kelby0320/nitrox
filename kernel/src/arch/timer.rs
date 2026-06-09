//! Architecture-neutral monotonic-time + per-CPU timer contract.
//!
//! [`ArchTimer`] is the kernel's source of monotonic time and the per-CPU
//! countdown timer that drives preemption (periodic) and deadline wakeups
//! (one-shot). On x86_64 this is the TSC (monotonic nanoseconds) plus the
//! local-APIC timer, both calibrated against the legacy PIT at boot (see
//! `arch/x86_64/timer.rs`); a future aarch64 port would back it with the
//! generic timer.
//!
//! This slice ships **timekeeping only**: [`read_ns`](ArchTimer::read_ns) is
//! live, but the arming methods program the hardware while staying *dormant* —
//! interrupts are masked (IF=0) for the whole slice, so the countdown is
//! observable (the current-count register decrements) but never fires. The
//! periodic-tick consumer (plus the IRQ stub and `IF=1`) lands with preemptive
//! scheduling; the one-shot deadline consumer with wait queues.
//!
//! NOTE: `crate::arch::Timer` — the *hardware* timer re-exported from this
//! trait — is a distinct namespace from the future `crate::object::Timer`, the
//! waitable `Timer` kernel object (deferred to the wait-queues slice). They
//! never collide in the same scope.
//!
//! The active architecture's implementation is re-exported from `crate::arch`
//! as `Timer` (see `kernel/src/arch/mod.rs`).

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
    /// kernel's timer vector. Consumer: the preemptive-scheduling tick. Dormant
    /// this slice (IF=0), but the countdown runs.
    ///
    /// # Safety
    /// Ring-0 only; valid after [`init`](ArchTimer::init). Reconfigures
    /// interrupt-source delivery for the current CPU.
    unsafe fn start_periodic(period_ns: u64);

    /// Arm the per-CPU timer to fire **once** `delay_ns` from now at the timer
    /// vector. Relative by design — the timer is inherently a counting-down
    /// initial count, so the deadline→delay subtraction belongs in the
    /// wait-queue consumer. Consumer: wait-queue deadlines. Dormant this slice.
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
