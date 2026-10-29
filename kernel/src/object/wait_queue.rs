//! [`WaitQueue`]: the threads blocked on one waitable object.

use crate::libkern::ArrayVec;

/// At most `N` threads waiting on one object, inline, so adding one under `SCHED` never allocates.
pub(crate) struct WaitQueue<const N: usize>(ArrayVec<*mut (), N>);

impl<const N: usize> WaitQueue<N> {
    /// No waiters.
    pub(crate) const fn new() -> Self {
        Self(ArrayVec::new())
    }

    /// Add `thread`; `Err(())` when `N` already wait.
    pub(crate) fn add(&mut self, thread: *mut ()) -> Result<(), ()> {
        self.0.try_push(thread).map_err(|_| ())
    }

    /// Remove `thread` if it waits (idempotent).
    pub(crate) fn remove(&mut self, thread: *mut ()) {
        if let Some(i) = self.0.iter().position(|&w| w == thread) {
            self.0.remove(i);
        }
    }

    /// Every waiter, in the order they were added, leaving none.
    pub(crate) fn take(&mut self) -> ArrayVec<*mut (), N> {
        core::mem::take(&mut self.0)
    }

    /// Whether no thread waits.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<const N: usize> Default for WaitQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}
