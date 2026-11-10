//! [`Outgoing`]: the thread a switch leaves, only ever the one this CPU is running.

use core::ops::Deref;

use super::{Ref, SchedState, Thread};

/// The thread taken out of this CPU's `current` slot, so the one this CPU is running. Its field is
/// this module's, so only [`take_current`] makes one, and a switch saves this CPU's context into
/// its own thread.
pub(super) struct Outgoing(Ref<Thread>);

impl Outgoing {
    pub(super) fn into_ref(self) -> Ref<Thread> {
        self.0
    }
}

impl Deref for Outgoing {
    type Target = Ref<Thread>;
    fn deref(&self) -> &Ref<Thread> {
        &self.0
    }
}

/// Take the running thread out of this CPU's `current` slot.
pub(super) fn take_current(g: &mut SchedState) -> Outgoing {
    Outgoing(g.cur_slot().take().expect("current set after init"))
}
