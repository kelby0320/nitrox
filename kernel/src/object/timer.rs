//! The [`Timer`] kernel object — a waitable monotonic-deadline source.
//!
//! Holds an armed `deadline_ns` (absolute monotonic ns, the clock of
//! [`arch::Timer::read_ns`](crate::arch::Timer)), an `interval_ns` (`0` = one-shot) and the
//! threads blocked in `sys_wait` on it. The scheduler tick fires it when the deadline elapses,
//! waking them (see [`crate::sched`]).
//!
//! ## Mutation discipline
//!
//! A `Timer` is shared through an [`ObjectRef`](crate::object::ObjectRef) yet mutated by the
//! scheduler: **all** its interior state lives in a [`SchedCell`], borrowed only with the token
//! the rank-1 `SCHED` lock keeps (`kernel/docs/lock-ordering.md`).

use crate::sched::{SchedCell, SchedToken, SlotHold, Slots};

use crate::libkern::{AllocError, KBox};
use crate::object::header::{Header, Ref, Shared, kobject};
use crate::object::{WaitQueue, Waiters};

/// A timer kernel object (`#[repr(C)]`, [`KObjectHeader`] first).
#[repr(C)]
pub struct Timer {
    header: Header<Self>,
    magic: u64,
    inner: SchedCell<TimerInner>,
    /// Its slot in the deadline heap, given back after its closing has taken its entry out.
    _slot: SlotHold,
}

kobject!(Timer, closing = crate::sched::timer_closing);
// SAFETY: a `Timer` changes once shared only in `inner`, a `SchedCell`.
unsafe impl Shared for Timer {}

struct TimerInner {
    /// Absolute monotonic-ns deadline; `0` = not armed.
    deadline_ns: u64,
    /// Re-arm period after each fire; `0` = one-shot.
    interval_ns: u64,
    /// `true` while this timer has a live entry in the scheduler deadline heap.
    in_heap: bool,
    /// Blocked threads, non-owning (each is kept alive by its own parked `ObjectRef` and removed
    /// from here before it unparks); at most [`Timer::MAX_WAITERS`].
    waiters: WaitQueue<{ Timer::MAX_WAITERS }>,
}

impl Timer {
    /// Sentinel written into [`Timer::magic`] at construction.
    pub const MAGIC: u64 = 0x54_69_6d_65_72_21_21_21; // "Timer!!!"

    /// Maximum simultaneous waiters.
    pub const MAX_WAITERS: usize = 4;

    /// Allocate an unarmed `Timer` (refcount one), reserving its slot in the deadline heap up
    /// front.
    pub fn try_new() -> Result<Ref<Self>, AllocError> {
        let slot = SlotHold::reserve(Slots::Timer)?;
        KBox::try_new(Self {
            header: Header::new(),
            magic: Self::MAGIC,
            inner: SchedCell::new(TimerInner {
                deadline_ns: 0,
                interval_ns: 0,
                in_heap: false,
                waiters: WaitQueue::new(),
            }),
            _slot: slot,
        })
        .map(Ref::new)
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    /// Arm (or disarm, with `deadline_ns == 0`) the timer.
    pub(crate) fn set_armed(&self, tok: &mut SchedToken, deadline_ns: u64, interval_ns: u64) {
        let inner = self.inner.get(tok);
        inner.deadline_ns = deadline_ns;
        inner.interval_ns = interval_ns;
    }

    /// The armed deadline (`0` = unarmed). Test-only; production reads it from the heap.
    #[cfg(test)]
    pub(crate) fn deadline(&self, tok: &SchedToken) -> u64 {
        self.inner.read(tok).deadline_ns
    }

    /// The re-arm interval (`0` = one-shot).
    pub(crate) fn interval(&self, tok: &SchedToken) -> u64 {
        self.inner.read(tok).interval_ns
    }

    /// Set the "present in the deadline heap" flag.
    pub(crate) fn set_in_heap(&self, tok: &mut SchedToken, v: bool) {
        self.inner.get(tok).in_heap = v;
    }

    /// `true` iff this timer has a live deadline-heap entry.
    pub(crate) fn in_heap(&self, tok: &SchedToken) -> bool {
        self.inner.read(tok).in_heap
    }

    /// Register `thread` as a waiter; `Err(())` at the cap, which callers map to `OutOfMemory`.
    pub(crate) fn add_waiter(&self, tok: &mut SchedToken, thread: *mut ()) -> Result<(), ()> {
        let inner = self.inner.get(tok);
        inner.waiters.add(thread)
    }

    /// Remove `thread` from the waiter set if present (idempotent).
    pub(crate) fn remove_waiter(&self, tok: &mut SchedToken, thread: *mut ()) {
        let inner = self.inner.get(tok);
        inner.waiters.remove(thread);
    }

    /// `true` iff the timer is armed and its deadline is at or before `now`, or it is a one-shot
    /// the tick has already fired: out of the heap with its deadline kept, the one state that
    /// means so. A waiter's `now` is read before it takes `SCHED`, and may predate that fire.
    pub(crate) fn already_signaled(&self, tok: &SchedToken, now_ns: u64) -> bool {
        let inner = self.inner.read(tok);
        let fired = inner.interval_ns == 0 && !inner.in_heap;
        inner.deadline_ns != 0 && (inner.deadline_ns <= now_ns || fired)
    }

    /// Take every waiter, leaving none.
    pub(crate) fn take_waiters(&self, tok: &mut SchedToken) -> Waiters<{ Timer::MAX_WAITERS }> {
        let inner = self.inner.get(tok);
        inner.waiters.take()
    }
}

impl Drop for Timer {
    /// A live waiter pins the timer (it holds an `ObjectRef` across `sys_wait`), so the last
    /// reference cannot drop while waiters remain; the assert documents that.
    fn drop(&mut self) {
        let inner = self.inner.get_mut();
        debug_assert!(inner.waiters.is_empty(), "Timer dropped with live waiters");
        debug_assert!(!inner.in_heap, "Timer dropped in the deadline heap");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;
    use crate::object::header::test_probe;

    #[test]
    fn try_new_is_unarmed_with_magic() {
        init_global_heap();
        let (t, tok) = (Timer::try_new().unwrap(), &mut SchedToken::for_test());
        assert!(t.magic_ok());
        assert_eq!(t.deadline(tok), 0);
        assert_eq!(t.interval(tok), 0);
        assert!(!t.in_heap(tok));
        assert!(!t.already_signaled(tok, u64::MAX));
    }

    #[test]
    fn arm_and_signal_predicate() {
        init_global_heap();
        let (t, tok) = (Timer::try_new().unwrap(), &mut SchedToken::for_test());
        t.set_armed(tok, 1000, 0); // armed as `timer_arm` arms: into the heap
        t.set_in_heap(tok, true);
        assert_eq!(t.deadline(tok), 1000);
        assert!(!t.already_signaled(tok, 999));
        assert!(t.already_signaled(tok, 1000));
        assert!(t.already_signaled(tok, 1001));
        t.set_armed(tok, 0, 0); // disarm
        t.set_in_heap(tok, false);
        assert!(!t.already_signaled(tok, u64::MAX));
    }

    #[test]
    fn waiters_add_remove_take_caps_at_max() {
        init_global_heap();
        let (t, tok) = (Timer::try_new().unwrap(), &mut SchedToken::for_test());
        let ths: [*mut (); Timer::MAX_WAITERS] =
            core::array::from_fn(|i| core::ptr::without_provenance_mut(0x1000 + i));
        for &th in &ths {
            assert!(t.add_waiter(tok, th).is_ok());
        }
        assert!(t.add_waiter(tok, core::ptr::without_provenance_mut(0xDEAD)).is_err());
        t.remove_waiter(tok, ths[1]);
        assert!(t.add_waiter(tok, core::ptr::without_provenance_mut(0xBEEF)).is_ok());
        assert_eq!(&t.take_waiters(tok)[..], &[ths[0], ths[2], ths[3], core::ptr::without_provenance_mut(0xBEEF)]);
        assert!(t.take_waiters(tok).is_empty());
    }

    #[test]
    fn dispatch_destroy_runs_timer_arm() {
        init_global_heap();
        test_probe::reset();
        let r = Timer::try_new().unwrap().erase();
        assert_eq!(test_probe::timer_destroys(), 0);
        drop(r);
        assert_eq!(test_probe::timer_destroys(), 1);
    }
}
