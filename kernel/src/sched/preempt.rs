//! Preemption control: the per-CPU disable count and the deferred reschedule.

use super::*;

/// Assert that a thread arriving from ring 3 has preemption enabled.
///
/// **The mirror of the underflow guard in [`preempt_enable`], for the imbalance that escapes
/// every other check.** A *leaked* `preempt_disable` wedges a CPU exactly as a wrap does:
/// `on_timer_tick` latches forever and a later enable goes 2 → 1, so neither the underflow
/// assert nor the replay fires. `switch_into`'s assert catches it only at the next voluntary
/// switch, and a thread making only non-blocking syscalls never reaches it.
///
/// This boundary is where the truth is free: a thread that was in user mode cannot hold a
/// kernel lock (the argument `assert_user_entry_safe` makes) nor have disabled preemption.
/// Debug builds only.
pub fn assert_user_entry_preempt_safe() {
    debug_assert_eq!(
        with_preempt_off(SchedState::this_cpu(), |c| c.load(Ordering::Relaxed)),
        0,
        "entered a syscall from ring 3 with preemption disabled — a kernel path leaked a \
         `preempt_disable`, and this CPU will stop preempting until something notices"
    );
}

/// Assert that a *voluntary* context switch is not inside a preempt-critical window.
///
/// Split out of [`switch_into`], like [`clear_online_bit`], so a host test can pin it
/// (`a_voluntary_switch_inside_a_preempt_critical_window_is_refused`); `switch_into` itself
/// is reached by no host test.
///
/// A plain-lock holder trips `assert_switch_safe` because it holds a rank; a bare
/// [`preempt_disable`] — the `tlb`, `handle::global` and `reap_pending` windows — holds
/// nothing, so a yield or block inside one would switch away from a CPU that asked not to be
/// descheduled and strand the raised counter on a thread no longer running. Debug builds only.
pub(super) fn assert_switch_preempt_safe() {
    debug_assert_eq!(
        with_preempt_off(SchedState::this_cpu(), |c| c.load(Ordering::Relaxed)),
        0,
        "context switch with preemption disabled — a voluntary switch inside a \
         preempt-critical region deschedules a CPU that asked not to be, and strands the \
         raised counter on a thread that is no longer running"
    );
}

/// Per-CPU preemption-disable depth (see [`preempt_disable`]). Written only by the thread
/// running on that CPU (which cannot migrate while it is nonzero); read by the same CPU's
/// tick and reschedule-IPI handlers.
#[cfg(not(test))]
static PREEMPT_OFF: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];

/// Set when a reschedule opportunity (tick expiry or reschedule IPI) was skipped because
/// [`PREEMPT_OFF`] was raised; consumed by [`preempt_enable`], which replays the
/// reschedule. Backstop: the CPU's next periodic tick.
#[cfg(not(test))]
static RESCHED_PENDING: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

#[cfg(test)]
std::thread_local! {
    /// Host-test backing for [`PREEMPT_OFF`] and [`RESCHED_PENDING`], **one pair per thread**.
    ///
    /// Under `cfg(test)` `current_cpu()` reports `0` for every thread, so as process-globals
    /// every concurrent test taking a [`SpinLock`](crate::libkern::SpinLock) (whose `lock()`
    /// calls [`preempt_disable`]) would write the same two slots. Scoped access rather than a
    /// leaked `'static` (`kernel/CLAUDE.md` forbids `Box::leak` patterns); the production
    /// path indexes the static directly.
    static PREEMPT_OFF_TLS: [AtomicU32; MAX_CPUS] =
        const { [const { AtomicU32::new(0) }; MAX_CPUS] };
    static RESCHED_PENDING_TLS: [AtomicBool; MAX_CPUS] =
        const { [const { AtomicBool::new(false) }; MAX_CPUS] };
}

/// This CPU's preempt-disable counter. See [`PREEMPT_OFF`].
#[cfg(not(test))]
#[inline(always)]
pub(super) fn with_preempt_off<R>(cpu: usize, f: impl FnOnce(&AtomicU32) -> R) -> R {
    f(&PREEMPT_OFF[cpu])
}

#[cfg(test)]
pub(super) fn with_preempt_off<R>(cpu: usize, f: impl FnOnce(&AtomicU32) -> R) -> R {
    PREEMPT_OFF_TLS.with(|a| f(&a[cpu]))
}

/// This CPU's deferred-reschedule latch. See [`RESCHED_PENDING`].
#[cfg(not(test))]
#[inline(always)]
pub(super) fn with_resched_pending<R>(cpu: usize, f: impl FnOnce(&AtomicBool) -> R) -> R {
    f(&RESCHED_PENDING[cpu])
}

#[cfg(test)]
pub(super) fn with_resched_pending<R>(cpu: usize, f: impl FnOnce(&AtomicBool) -> R) -> R {
    RESCHED_PENDING_TLS.with(|a| f(&a[cpu]))
}

/// Disable preemption on the current CPU (nestable). While the depth is nonzero, the tick
/// and the reschedule IPI will not deschedule the running thread (interrupts stay enabled;
/// only the *switch* is deferred, latched in [`RESCHED_PENDING`]).
///
/// This exists for **preemption-critical windows**: code holding a plain spinlock other CPUs
/// may spin on — the TLB-shootdown serialiser, the allocator locks during reclamation. A
/// preempted *normal* holder is rescheduled within a tick (slow, but live); the **idle
/// thread** is the fatal case — it runs only when its CPU has nothing else, so if it is
/// descheduled holding such a lock, the spinners keep every CPU busy and the holder is
/// starved forever.
///
/// Contract: pair with [`preempt_enable`] on the same thread; the window must be bounded (no
/// blocking, no `sys_wait`) and hold no `SCHED`-ordered lock.
///
/// [`SpinLock`](crate::libkern::SpinLock) calls this in `lock()` (and the enable in its
/// guard drop), making **every plain-spinlock critical section a no-preemption region**: a
/// spinner, even with interrupts masked, always waits on a *running* holder, so the wait is
/// bounded by the critical section.
pub fn preempt_disable() {
    // Mask IRQs across read-index + increment: an interrupt between them could migrate this
    // thread and the increment would land on the wrong CPU's counter, sticking it nonzero.
    // Once the count is raised, descheduling is impossible.
    let prev = preempt_irqs_mask();
    with_preempt_off(SchedState::this_cpu(), |c| c.fetch_add(1, Ordering::Relaxed));
    preempt_irqs_restore(prev);
}

/// Re-enable preemption (see [`preempt_disable`]); at depth zero, replays a reschedule that
/// was skipped during the window. (If the thread is descheduled or migrates between the
/// depth reaching zero and the replay, the replay's under-lock re-check makes it a no-op;
/// the origin CPU's next tick covers any consumed-but-unserviced wake.)
pub fn preempt_enable() {
    let prev = preempt_irqs_mask();
    let me = SchedState::this_cpu();
    // **Saturating, not wrapping.** An unbalanced enable at depth 0 would set the counter to
    // `u32::MAX` and that CPU would never preempt again. Overflow checks do not help: an
    // atomic RMW wraps by definition. The `debug_assert` is the loud half and the saturation
    // the safe half.
    //
    // **Do not "simplify" this to a plain load/store.** Masked interrupts make the RMW
    // redundant on target, but `preempt_irqs_mask` is what makes it indivisible, and a plain
    // load/store would make this line's correctness depend on that mask staying exactly
    // where it is. The RMW costs an uncontended `lock` prefix and removes the coupling.
    let depth_before = with_preempt_off(me, |c| {
        c.try_update(Ordering::Relaxed, Ordering::Relaxed, |d| Some(d.saturating_sub(1)))
            // unwrap_or: the closure is infallible, so `try_update` cannot return `Err`.
            .unwrap_or(0)
    });
    debug_assert_ne!(
        depth_before, 0,
        "preempt_enable with no matching preempt_disable — the counter would have wrapped \
         to u32::MAX and this CPU would never preempt again"
    );
    let replay = depth_before == 1 && with_resched_pending(me, |c| c.swap(false, Ordering::AcqRel));
    preempt_irqs_restore(prev);
    if replay {
        // Mirror `on_reschedule_ipi`: resume an idle CPU into the scheduler; a busy thread
        // is left to its quantum (the next tick reschedules it).
        let g = SCHED.lock();
        let cpu = SchedState::this_cpu();
        if current_is_idle(&g) && (!g.ready[cpu].is_empty() || steal_available(&g, cpu)) {
            switch_to_next(g);
        }
    }
}

/// Mask interrupts for the preempt-counter update window. A no-op on the host, where tests
/// run ring-3 with no ticks to race (a real `cli` would `#GP`).
#[inline]
fn preempt_irqs_mask() -> bool {
    #[cfg(not(test))]
    {
        // SAFETY: ring-0; a microseconds-bounded masked window, restored below.
        unsafe { Cpu::interrupts_disable() }
    }
    #[cfg(test)]
    {
        false
    }
}

/// Restore the interrupt state captured by [`preempt_irqs_mask`].
#[inline]
fn preempt_irqs_restore(prev: bool) {
    #[cfg(not(test))]
    // SAFETY: ring-0; restoring the state captured by the paired mask.
    unsafe {
        Cpu::interrupts_restore(prev)
    };
    #[cfg(test)]
    let _ = prev;
}
