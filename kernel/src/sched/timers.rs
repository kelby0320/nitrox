//! Timers and deadlines, armed and fired.

use super::*;

/// Complete any `PendingOperation`s parked on the entropy pool becoming seeded (the unseeded
/// `sys_entropy_read` path), gated by a lock-free check so the common case costs one atomic
/// load. Caller holds `SCHED`.
///
/// The entropy subsystem owns the waiter refs; we move them out, signal each, and **park
/// them in [`SchedState::deferred_drops`]** for [`reap_pending`] to drop in thread context.
/// They must not drop here: an `ObjectRef` drop can reach the plain-spinlock allocator,
/// which may neither run under the IRQ-acquired `SCHED` lock (a cross-CPU deadlock against
/// an allocator holder whose own tick spins on `SCHED`) nor in this tick's IRQ context (a
/// same-CPU self-deadlock against an allocator holder it interrupted). See
/// `kernel/docs/lock-ordering.md`.
pub(super) fn wake_entropy_seed_waiters(g: &mut SchedState) {
    if !crate::entropy::seed_wake_pending() {
        return;
    }
    let mut buf: [Option<ObjectRef>; crate::entropy::SEED_WAITERS_MAX] =
        [const { None }; crate::entropy::SEED_WAITERS_MAX];
    let n = crate::entropy::drain_seed_waiters(&mut buf);
    for slot in buf[..n].iter_mut() {
        if let Some(po) = slot.take() {
            signal_pending_op_with_result(g, po.as_ptr(), 0, 0);
            // A move within the reserve — no drop, no allocation under the lock. Bounded:
            // waiters queue only pre-seed and the drain fires post-seed, so the lifetime
            // total is ≤ SEED_WAITERS_MAX (< the reserve).
            push_reserved(&mut g.deferred_drops, po, "deferred-drop list");
        }
    }
}

/// Drain every deadline at or before `now`, firing each: a timer signals and wakes its
/// waiters (and re-arms if periodic); a `sys_wait` thread-deadline wakes that thread
/// directly with its wait slots un-signaled, so it observes a timeout. Caller holds `SCHED`.
/// **No allocation, no blocking** — safe in the timer IRQ.
pub(super) fn fire_expired_deadlines(g: &mut SchedState, now: u64) {
    while let Some(top) = deadline::peek(&g.deadlines) {
        if top.deadline_ns > now {
            break;
        }
        deadline::pop_min(&mut g.deadlines);
        match top.kind {
            deadline::DeadlineKind::Thread => {
                // A `sys_wait` deadline: wake the waiting thread directly.
                let th = top.target as *mut ();
                // SAFETY: a heaped thread-deadline names a thread blocked in
                // `wait_on`, pinned in `blocked`; `SCHED` held.
                if unsafe { Thread::wait_try_wake(th) } {
                    make_runnable(g, th);
                }
            }
            deadline::DeadlineKind::Timer => {
                let timer = top.target as *mut ();
                // SAFETY: a heaped timer is whole: `timer_closing` removes its entry
                // under `SCHED` before the memory goes; `SCHED` held.
                unsafe { Timer::set_in_heap(timer, false) };
                fire_timer(g, timer, now);
            }
            deadline::DeadlineKind::PendingSend => {
                // A `BlockBounded` send's delivery deadline elapsed: cancel the held send
                // and complete its `PendingOperation` with `TimedOut`. No `ObjectRef` drop
                // here (`SCHED` held — a transferred object's `Drop` could take the buddy
                // lock); the message + refs are reclaimed on the next recv / at close.
                let channel = top.channel as *mut ();
                let po = top.target as *mut ();
                // SAFETY: `channel` is still open — `ipc_endpoint_closing`
                // removes its held sends' deadline entries, so a fired
                // pending-send deadline always names a live endpoint; `SCHED`
                // held. `cancel_pending_send` only sets a flag (no drop).
                unsafe { IpcChannel::cancel_pending_send(channel, po) };
                signal_pending_op(g, po, crate::syscall::error::KError::TimedOut as i32);
            }
        }
    }
}

/// Fire one timer: signal + wake all its waiters, then re-arm if periodic. Caller holds
/// `SCHED`.
fn fire_timer(g: &mut SchedState, timer: *mut (), now: u64) {
    let mut buf = [core::ptr::null_mut(); Timer::MAX_WAITERS];
    // SAFETY: live Timer, `SCHED` held — drains its waiter list.
    let n = unsafe { Timer::take_waiters(timer, &mut buf) };
    for &th in &buf[..n] {
        // SAFETY: each waiter is a thread blocked in `wait_on`, pinned in
        // `blocked`; `SCHED` held. Mark this timer's slot signaled, then claim
        // the thread for wakeup (CAS); the first claimer makes it runnable.
        unsafe {
            Thread::wait_mark_signaled(th, timer as usize);
            if Thread::wait_try_wake(th) {
                make_runnable(g, th);
            }
        }
    }
    // SAFETY: live Timer, `SCHED` held.
    let interval = unsafe { Timer::interval(timer) };
    // A one-shot keeps its passed deadline, out of the heap, so a wait that arrives after this
    // tick still finds it signaled.
    if interval > 0 {
        let next = now.saturating_add(interval);
        // SAFETY: live Timer, `SCHED` held — re-arm the periodic timer.
        unsafe {
            Timer::set_armed(timer, next, interval);
            Timer::set_in_heap(timer, true);
        }
        // Re-push into the slot the timer holds (its entry was just popped).
        let _ = deadline::push_promised(
            g,
            deadline::Entry {
                deadline_ns: next,
                target: timer as usize,
                kind: deadline::DeadlineKind::Timer,
                channel: 0,
            },
        );
    }
}

/// Arm (or, with `deadline_ns == 0`, disarm) a timer: set its deadline/interval and
/// (re)insert its absolute-monotonic-ns heap entry. `Err(())` if the heap is at reserve.
///
/// # Safety
///
/// `timer` points at a live `Timer`, pinned for the call by a reference the caller holds.
#[allow(clippy::result_unit_err, reason = "one failure, the heap's reserve, which every caller reports one way")]
pub unsafe fn timer_arm(timer: *mut (), deadline_ns: u64, interval_ns: u64) -> Result<(), ()> {
    let mut g = SCHED.lock();
    // Drop any stale heap entry (re-arm).
    // SAFETY: live Timer (caller holds its `ObjectRef`), `SCHED` held.
    unsafe { unheap(&mut g, timer) };
    // SAFETY: live Timer, `SCHED` held.
    unsafe { Timer::set_armed(timer, deadline_ns, interval_ns) };
    if deadline_ns != 0 {
        if deadline::push_promised(
            &mut g,
            deadline::Entry { deadline_ns, target: timer as usize, kind: deadline::DeadlineKind::Timer, channel: 0 },
        )
        .is_err()
        {
            // SAFETY: live Timer, `SCHED` held — undo the arm on heap overflow.
            unsafe { Timer::set_armed(timer, 0, 0) };
            return Err(());
        }
        // SAFETY: live Timer, `SCHED` held.
        unsafe { Timer::set_in_heap(timer, true) };
    }
    Ok(())
}

/// A timer is being destroyed: drop its heap entry, which names it by address without a
/// reference. Called only from [`Timer::drop`](crate::object::Timer), outside `SCHED`.
///
/// # Safety
///
/// `timer` is the `Timer` being dropped: its memory whole, its last reference gone.
pub unsafe fn timer_closing(timer: *mut ()) {
    let mut g = SCHED.lock();
    // SAFETY: the memory is whole until the drop returns; `SCHED` held.
    unsafe { unheap(&mut g, timer) };
    deadline::release_slots(&mut g, Slots::Timer);
}

/// Remove `timer`'s heap entry, if it has one.
///
/// # Safety
///
/// `timer` is a `Timer` whose memory is whole; the caller holds `SCHED`.
unsafe fn unheap(g: &mut SchedState, timer: *mut ()) {
    // SAFETY: the caller's contract.
    if unsafe { Timer::in_heap(timer) } {
        deadline::remove(&mut g.deadlines, timer as usize, deadline::DeadlineKind::Timer);
        // SAFETY: as above.
        unsafe { Timer::set_in_heap(timer, false) };
    }
}
