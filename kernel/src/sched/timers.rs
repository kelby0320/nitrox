//! Timers and deadlines, armed and fired.

use super::*;

/// Complete any `PendingOperation`s parked on the entropy pool becoming seeded (the unseeded
/// `sys_entropy_read` path), gated by a lock-free check so the common case costs one atomic
/// load. From the tick.
pub(super) fn wake_entropy_seed_waiters() {
    if crate::entropy::seed_wake_pending() {
        wake_seed_waiters(crate::entropy::drain_seed_waiters);
    }
}

/// Complete the seed waiters `drain` hands over. The entropy subsystem owns their refs, which
/// move into an interrupt's bag, so they reach `deferred_drops` rather than drop here. Drained
/// before `SCHED` is taken, so `ENTROPY` never nests in it. Bounded: waiters queue only pre-seed
/// and the drain fires post-seed, so the lifetime total is ≤ `SEED_WAITERS_MAX`.
pub(super) fn wake_seed_waiters(drain: impl FnOnce(&mut [Option<Ref<PendingOperation>>]) -> usize) {
    use crate::entropy::SEED_WAITERS_MAX;
    let mut buf: [Option<Ref<PendingOperation>>; SEED_WAITERS_MAX] = [const { None }; SEED_WAITERS_MAX];
    let n = drain(&mut buf);
    if n == 0 {
        return;
    }
    let mut bag = Deferred::<SEED_WAITERS_MAX>::in_interrupt();
    let mut g = bag.lock();
    for po in buf[..n].iter_mut().filter_map(Option::take) {
        signal_pending_op_with_result(&mut g, addr(&*po), 0, 0);
        g.defer(po.erase());
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
        match top.target {
            deadline::Target::Thread(th) => {
                // A `sys_wait` deadline: wake the waiting thread directly.
                let th = th as *mut ();
                // SAFETY: a heaped thread-deadline names a thread blocked in
                // `wait_on`, pinned in `blocked`; `SCHED` held.
                if unsafe { Thread::wait_try_wake(th) } {
                    make_runnable(g, th);
                }
            }
            deadline::Target::Timer(timer) => {
                let timer = timer as *mut ();
                // SAFETY: a heaped timer is whole: `timer_closing` removes its entry
                // under `SCHED` before the memory goes; `SCHED` held.
                unsafe { Timer::set_in_heap(timer, false) };
                fire_timer(g, timer, now);
            }
            deadline::Target::PendingSend { po, channel } => {
                // A `BlockBounded` send's delivery deadline elapsed: cancel the held send
                // and complete its `PendingOperation` with `TimedOut`. No `ObjectRef` drop
                // here (`SCHED` held — a transferred object's `Drop` could take the buddy
                // lock); the message + refs are reclaimed on the next recv / at close.
                let (po, channel) = (po as *mut (), channel as *mut ());
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
    // SAFETY: live Timer, `SCHED` held — drains its waiter list.
    let waiters = unsafe { Timer::take_waiters(timer) };
    wake_all(g, waiters, timer);
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
            deadline::Entry { deadline_ns: next, target: deadline::Target::Timer(timer as usize) },
        );
    }
}

/// Arm (or, with `deadline_ns == 0`, disarm) a timer: set its deadline/interval and
/// (re)insert its absolute-monotonic-ns heap entry. `Err(())` if the heap is at reserve.
#[allow(clippy::result_unit_err, reason = "one failure, the heap's reserve, which every caller reports one way")]
pub fn timer_arm(timer: &Timer, deadline_ns: u64, interval_ns: u64) -> Result<(), ()> {
    let mut g = SCHED.lock();
    let timer = addr(timer);
    // Drop any stale heap entry (re-arm).
    // SAFETY: pinned by the borrow, `SCHED` held.
    unsafe { unheap(&mut g, timer) };
    // SAFETY: live Timer, `SCHED` held.
    unsafe { Timer::set_armed(timer, deadline_ns, interval_ns) };
    if deadline_ns != 0 {
        if deadline::push_promised(
            &mut g,
            deadline::Entry { deadline_ns, target: deadline::Target::Timer(timer as usize) },
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
}

/// Remove `timer`'s heap entry, if it has one.
///
/// # Safety
///
/// `timer` is a `Timer` whose memory is whole; the caller holds `SCHED`.
unsafe fn unheap(g: &mut SchedState, timer: *mut ()) {
    // SAFETY: the caller's contract.
    if unsafe { Timer::in_heap(timer) } {
        deadline::remove(&mut g.deadlines, deadline::Target::Timer(timer as usize));
        // SAFETY: as above.
        unsafe { Timer::set_in_heap(timer, false) };
    }
}
