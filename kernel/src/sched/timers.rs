//! Timers and deadlines, armed and fired.

use super::*;

/// Complete any `PendingOperation`s parked on the entropy pool becoming seeded (an unseeded read
/// of `/dev/entropy`), gated by a lock-free check so the common case costs one atomic load. From
/// the tick.
pub(super) fn wake_entropy_seed_waiters() {
    if crate::entropy::seed_wake_pending() {
        wake_seed_waiters(crate::entropy::drain_seed_waiters);
    }
}

/// Complete the seed waiters `drain` hands over. The entropy subsystem owns their refs, which
/// move into an interrupt's bag, so they reach `deferred_drops` rather than drop here. Drained
/// before `SCHED` is taken, so `ENTROPY` never nests in it. Bounded: waiters queue only pre-seed
/// and the drain fires post-seed, so the lifetime total is ≤ `SEED_WAITERS_MAX`.
pub(crate) fn wake_seed_waiters(drain: impl FnOnce(&mut [Option<Ref<PendingOperation>>]) -> usize) {
    use crate::entropy::SEED_WAITERS_MAX;
    let mut buf: [Option<Ref<PendingOperation>>; SEED_WAITERS_MAX] = [const { None }; SEED_WAITERS_MAX];
    let n = drain(&mut buf);
    if n == 0 {
        return;
    }
    let mut bag = Deferred::<SEED_WAITERS_MAX>::in_interrupt();
    let mut g = bag.lock();
    for po in buf[..n].iter_mut().filter_map(Option::take) {
        signal_pending_op_with_result(&mut g, &po, 0, 0);
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
                // SAFETY: a heaped thread-deadline names a thread in `wait_on`, which a scheduler
                // list pins until it takes the entry out under `SCHED`, which is held.
                let th = unsafe { &*(th as *const Thread) };
                if th.wait_try_wake() {
                    make_runnable(g, th);
                }
            }
            deadline::Target::Timer(timer) => {
                // SAFETY: a heaped timer is whole: its closing removes the entry under `SCHED`
                // before the memory goes, and `SCHED` is held.
                let timer = unsafe { &*(timer as *const Timer) };
                timer.set_in_heap(&mut g.tok, false);
                fire_timer(g, timer, now);
            }
            deadline::Target::PendingSend { po, channel } => {
                // A `BlockBounded` send's delivery deadline elapsed: cancel the held send
                // and complete its `PendingOperation` with `TimedOut`. No `ObjectRef` drop
                // here (`SCHED` held — a transferred object's `Drop` could take the buddy
                // lock); the message + refs are reclaimed on the next recv / at close.
                // SAFETY: `channel` is still open: its closing removes its held sends' deadline
                // entries under `SCHED`, which is held, before the memory goes.
                let channel = unsafe { &*(channel as *const IpcChannel) };
                // SAFETY: the PO is pinned by the held send in `channel`'s queue, which only a
                // receive or the closing takes out, and neither runs while `SCHED` is held.
                let po = unsafe { &*(po as *const PendingOperation) };
                // Only a flag is set: nothing is dropped here.
                channel.cancel_pending_send(&mut g.tok, po);
                signal_pending_op(g, po, crate::syscall::error::KError::TimedOut as i32);
            }
        }
    }
}

/// Fire one timer: signal + wake all its waiters, then re-arm if periodic. Caller holds
/// `SCHED`.
fn fire_timer(g: &mut SchedState, timer: &Timer, now: u64) {
    let waiters = timer.take_waiters(&mut g.tok);
    wake_all(g, waiters, addr(timer));
    let interval = timer.interval(&g.tok);
    // A one-shot keeps its passed deadline, out of the heap, so a wait that arrives after this
    // tick still finds it signaled.
    if interval > 0 {
        let next = now.saturating_add(interval);
        timer.set_armed(&mut g.tok, next, interval);
        timer.set_in_heap(&mut g.tok, true);
        // Re-push into the slot the timer holds (its entry was just popped).
        let _ = deadline::push_promised(
            g,
            deadline::Entry { deadline_ns: next, target: deadline::Target::Timer(addr(timer) as usize) },
        );
    }
}

/// Arm (or, with `deadline_ns == 0`, disarm) a timer: set its deadline/interval and
/// (re)insert its absolute-monotonic-ns heap entry. `Err(())` if the heap is at reserve.
#[allow(clippy::result_unit_err, reason = "one failure, the heap's reserve, which every caller reports one way")]
pub fn timer_arm(timer: &Timer, deadline_ns: u64, interval_ns: u64) -> Result<(), ()> {
    arm(&mut SCHED.lock(), timer, deadline_ns, interval_ns)
}

/// [`timer_arm`], under a held `SCHED`.
pub(super) fn arm(g: &mut SchedState, timer: &Timer, deadline_ns: u64, interval_ns: u64) -> Result<(), ()> {
    // Drop any stale heap entry (re-arm).
    unheap(g, timer);
    timer.set_armed(&mut g.tok, deadline_ns, interval_ns);
    if deadline_ns != 0 {
        let target = deadline::Target::Timer(addr(timer) as usize);
        if deadline::push_promised(g, deadline::Entry { deadline_ns, target }).is_err() {
            // Undo the arm on heap overflow.
            timer.set_armed(&mut g.tok, 0, 0);
            return Err(());
        }
        timer.set_in_heap(&mut g.tok, true);
    }
    Ok(())
}

/// A timer's last reference has gone: drop its heap entry, which names it by address without a
/// reference. Its closing, outside `SCHED`, before the memory goes.
pub(crate) fn timer_closing(timer: &Timer, _: Closing) {
    unheap(&mut SCHED.lock(), timer);
}

/// Remove `timer`'s heap entry, if it has one.
fn unheap(g: &mut SchedState, timer: &Timer) {
    if timer.in_heap(&g.tok) {
        deadline::remove(&mut g.deadlines, deadline::Target::Timer(addr(timer) as usize));
        timer.set_in_heap(&mut g.tok, false);
    }
}
