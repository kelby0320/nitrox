//! Preemption control: the per-CPU disable count and the deferred reschedule.

use super::*;

/// Assert that a thread arriving from ring 3 has preemption enabled.
///
/// **The mirror of the underflow guard in [`preempt_enable`], for the imbalance that escapes
/// every other check.** A *leaked* `preempt_disable` wedges a CPU exactly as the wrap did:
/// `on_timer_tick` latches and returns forever, and a later enable goes 2 → 1 and never
/// reaches 0, so neither the underflow assert nor the replay ever fires. `switch_into`'s
/// assert catches it at the thread's next voluntary switch — but a thread that returns to
/// ring 3 and makes only non-blocking syscalls never reaches `switch_into`, and that CPU
/// simply stops preempting with nothing said.
///
/// This is the boundary where the truth is free, and it is the same argument
/// `assert_user_entry_safe` already makes for locks one line above the call site: a thread
/// that was executing in user mode cannot hold a kernel lock, and cannot have disabled
/// kernel preemption either. Debug builds only (PR #203 review, optional 1).
pub fn assert_user_entry_preempt_safe() {
    debug_assert_eq!(
        with_preempt_off(SchedState::this_cpu(), |c| c.load(Ordering::Relaxed)),
        0,
        "entered a syscall from ring 3 with preemption disabled — a kernel path leaked a \
         `preempt_disable`, and this CPU will stop preempting until something notices"
    );
}

/// Assert that a *voluntary* context switch is not happening inside a preempt-critical window.
///
/// Split out of [`switch_into`] rather than left inline, for the reason [`clear_online_bit`]
/// was: `switch_into` manipulates real registers and stacks and is reached by **no** host test
/// (kernel audit B.5(d)), so an assertion living inside it is pinned only by booting. As a free
/// function the rule is pinned by
/// `a_voluntary_switch_inside_a_preempt_critical_window_is_refused`.
///
/// A plain-lock holder trips `assert_switch_safe` because it holds a rank; a bare
/// [`preempt_disable`] — the `tlb`, `handle::global` and `reap_pending` windows — holds nothing,
/// so a yield or block inside one would switch away from a CPU that has asked not to be
/// descheduled, and strand the raised counter on a thread that is no longer running.
///
/// Debug builds only. No reachable violation exists today; this is what keeps that true.
pub(super) fn assert_switch_preempt_safe() {
    debug_assert_eq!(
        with_preempt_off(SchedState::this_cpu(), |c| c.load(Ordering::Relaxed)),
        0,
        "context switch with preemption disabled — a voluntary switch inside a \
         preempt-critical region deschedules a CPU that asked not to be, and strands the \
         raised counter on a thread that is no longer running"
    );
}

/// Per-CPU preemption-disable depth (see [`preempt_disable`]). Written only by
/// the thread running on that CPU (which, while nonzero, cannot migrate — that
/// is the point); read by the same CPU's tick / reschedule-IPI handlers.
#[cfg(not(test))]
static PREEMPT_OFF: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];

/// Set when a reschedule opportunity (tick expiry or reschedule IPI) was
/// skipped because [`PREEMPT_OFF`] was raised; consumed by [`preempt_enable`],
/// which replays the reschedule. Backstop: the CPU's next periodic tick.
#[cfg(not(test))]
static RESCHED_PENDING: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

#[cfg(test)]
std::thread_local! {
    /// Host-test backing for [`PREEMPT_OFF`] and [`RESCHED_PENDING`], **one pair per thread**.
    ///
    /// Both are per-CPU arrays indexed by `SchedState::this_cpu()`, and under `cfg(test)`
    /// `current_cpu()` reports `0` for every thread — so as process-globals every concurrent
    /// test taking any [`SpinLock`](crate::libkern::SpinLock) wrote the same two slots, since
    /// `lock()` calls [`preempt_disable`]. Nothing asserted on them from a host test, which is
    /// the only reason it never fired; the 2026-08-14 preempt-guard entry records this hazard
    /// as the reason those guards went untested. Per-thread, they can be.
    ///
    /// Scoped access rather than a leaked `'static` — `kernel/CLAUDE.md` forbids `Box::leak`
    /// patterns, and the closure form costs nothing on the production path, which indexes the
    /// static directly.
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

/// Disable preemption on the current CPU (nestable). While the depth is
/// nonzero, the tick and the reschedule IPI will not deschedule the running
/// thread (interrupts themselves stay enabled — handlers run; only the
/// *switch* is deferred, latched in [`RESCHED_PENDING`]).
///
/// This exists for **preemption-critical windows**: code holding a plain
/// spinlock other CPUs may spin on — the TLB-shootdown serialiser, the
/// allocator locks during reclamation. A preempted *normal* holder sits in a
/// ready queue and is rescheduled within a tick (slow, but live); the **idle
/// thread** is the fatal case — it parks in `idle_slot` and runs only when its
/// CPU has nothing else, so if it is descheduled while holding such a lock,
/// the spinners themselves keep every CPU busy and the holder is starved
/// forever (F12, decision log 2026-07-21 — an all-CPUs-spinning-on-the-
/// shootdown-lock deadlock found by the exit-storm stress).
///
/// Contract: pair with [`preempt_enable`] on the same thread; the window must
/// be bounded (no blocking, no `sys_wait`) and hold no `SCHED`-ordered lock.
///
/// [`SpinLock`](crate::libkern::SpinLock) calls this in `lock()` (and the
/// matching enable in its guard drop), making **every plain-spinlock critical
/// section a no-preemption region**: a holder is never descheduled, so a
/// spinner — even one with interrupts masked, which cannot tick — always waits
/// on a *running* holder and the wait is bounded by the critical section.
pub fn preempt_disable() {
    // Mask IRQs across read-index + increment: an interrupt between them could
    // deschedule/migrate this thread and the increment would land on the wrong
    // CPU's counter (sticking it nonzero — that CPU would never preempt again).
    // Once the count is raised, descheduling is impossible, which is what makes
    // the per-CPU counter track the thread for the window's duration.
    let prev = preempt_irqs_mask();
    with_preempt_off(SchedState::this_cpu(), |c| c.fetch_add(1, Ordering::Relaxed));
    preempt_irqs_restore(prev);
}

/// Re-enable preemption (see [`preempt_disable`]); at depth zero, replays a
/// reschedule that was skipped during the window. (If the thread is descheduled
/// or migrates between the depth reaching zero and the replay, the replay's
/// under-lock re-check makes it a no-op; the origin CPU's next tick covers any
/// consumed-but-unserviced wake.)
pub fn preempt_enable() {
    let prev = preempt_irqs_mask();
    let me = SchedState::this_cpu();
    // **Saturating, not wrapping.** An unbalanced enable at depth 0 would set the counter to
    // `u32::MAX`, and that CPU would never preempt again — the exact failure
    // `preempt_disable`'s own comment names, reached from the other direction. `[profile.dev]`
    // sets `overflow-checks = true` and it does not help here: an atomic RMW wraps by
    // definition, so the dev profile is as silent as release (kernel audit B.5(e)).
    //
    // The `debug_assert` is the loud half and the saturation is the safe half: a build that
    // checks catches the unbalanced enable at the moment it happens, and one that does not
    // still refuses to brick the CPU.
    //
    // **Do not "simplify" this to a plain load/store**, though the reason has changed and the
    // old one is no longer true. It used to be that `this_cpu()` collapses every host-test
    // thread onto index 0, so the RMW was load-bearing for the host configuration even though
    // interrupts being masked makes it redundant on target. The counter is now per-thread
    // under `cfg(test)` (see `PREEMPT_OFF_TLS`), so that collapse is gone and no *current*
    // configuration needs the atomicity.
    //
    // It stays because the target argument is a claim about a masking window, not about the
    // counter: `preempt_irqs_mask` is what makes the read-modify-write indivisible here, and a
    // plain load/store would make this line's correctness depend on that mask staying exactly
    // where it is. The RMW costs an uncontended `lock` prefix on a per-CPU line and removes
    // the coupling.
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
        // Mirror `on_reschedule_ipi`: resume an idle CPU into the scheduler; a
        // busy thread is left to its quantum (the next tick reschedules it).
        let g = SCHED.lock();
        let cpu = SchedState::this_cpu();
        if current_is_idle(&g) && (!g.ready[cpu].is_empty() || steal_available(&g, cpu)) {
            switch_to_next(g);
        }
    }
}

/// Mask interrupts for the preempt-counter update window. Host tests run ring-3
/// single-threaded (a real `cli` would `#GP`), and have no ticks to race — a
/// no-op there, mirroring the `IrqSpinLock` test backend.
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
