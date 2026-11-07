//! [`Deferred`]: the references a `SCHED` hold lets go of, dropped once it is released.

use core::ops::{Deref, DerefMut};

use super::{DEFERRED_DROP_RESERVE, SCHED, SchedState, push_reserved, reserve_exhausted};
use crate::libkern::IrqSpinLockGuard;
use crate::object::ObjectRef;

/// At most `N` references a site lets go of while it holds `SCHED`, dropped when this bag is: after
/// the guard [`lock`](Self::lock) returns, which borrows it, so the bag is declared first and
/// drops last. An `ObjectRef` drop can reach the allocator, never legal under the rank-1 lock
/// (`kernel/docs/lock-ordering.md`). Sized to the site's burst; a full one is a sizing bug.
pub(super) struct Deferred<const N: usize> {
    refs: [Option<ObjectRef>; N],
    len: usize,
    /// Whether the site runs in interrupt context, where nothing may be freed at all: the guard
    /// then moves what it holds to `deferred_drops`, for `reap_pending` to drop in a thread.
    interrupt: bool,
}

impl<const N: usize> Deferred<N> {
    /// A bag for a site in thread context.
    pub(super) fn new() -> Self {
        Self { refs: [const { None }; N], len: 0, interrupt: false }
    }

    /// A bag for a site in interrupt context, no larger than `deferred_drops`' reserve. That list is
    /// shared and drained only by `reap_pending`, so its room at the drop rests on each such site's
    /// own lifetime bound ([`DEFERRED_DROP_RESERVE`]).
    pub(super) fn in_interrupt() -> Self {
        const { assert!(N <= DEFERRED_DROP_RESERVE, "an interrupt's bag past deferred_drops' reserve") };
        Self { interrupt: true, ..Self::new() }
    }

    /// Lock `SCHED`. What is deferred through the guard drops with this bag, after the guard.
    pub(super) fn lock(&mut self) -> Held<'_, N> {
        Held { g: SCHED.lock(), bag: self }
    }
}

/// `SCHED` held, with a bag for the references the hold lets go of. It derefs to the state, so
/// it is passed wherever a `&mut SchedState` is; it never becomes the plain guard a switch takes,
/// so no bag crosses a switch.
pub(super) struct Held<'b, const N: usize> {
    g: IrqSpinLockGuard<'static, SchedState>,
    bag: &'b mut Deferred<N>,
}

impl<const N: usize> Held<'_, N> {
    /// Keep `r` for the bag to drop once `SCHED` is released.
    #[track_caller]
    pub(super) fn defer(&mut self, r: ObjectRef) {
        let Some(slot) = self.bag.refs.get_mut(self.bag.len) else { reserve_exhausted(r, "deferred drops") };
        *slot = Some(r);
        self.bag.len += 1;
    }
}

impl<const N: usize> Drop for Held<'_, N> {
    fn drop(&mut self) {
        if self.bag.interrupt {
            for r in self.bag.refs[..self.bag.len].iter_mut().filter_map(Option::take) {
                push_reserved(&mut self.g.deferred_drops, r, "deferred-drop list");
            }
            self.bag.len = 0;
        }
        // `g` drops after this body, releasing `SCHED` before the bag's references can drop.
    }
}

impl<const N: usize> Deref for Held<'_, N> {
    type Target = SchedState;
    fn deref(&self) -> &SchedState {
        &self.g
    }
}

impl<const N: usize> DerefMut for Held<'_, N> {
    fn deref_mut(&mut self) -> &mut SchedState {
        &mut self.g
    }
}
