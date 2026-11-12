//! What `SCHED` guards, as a type: the state of an object other CPUs share, in a [`SchedCell`]
//! borrowed only with the [`SchedToken`] the held lock keeps, and the uncounted pointers between
//! such objects, followed with it.

use core::cell::UnsafeCell;
use core::marker::PhantomData;

use crate::arch::percpu::CpuPin;
use crate::object::header::{Shared, Weak};

/// The right to borrow what `SCHED` guards. The scheduler's state keeps the one there is, so a
/// `&mut` to it is had only while the lock is held, and lends one [`SchedCell`]'s interior at a
/// time.
pub struct SchedToken(PhantomData<core::cell::Cell<()>>);

impl SchedToken {
    /// The token the scheduler's state keeps.
    ///
    /// # Safety
    /// It is kept in a `SchedState`, and no other token reaches the cells that state's objects
    /// hold.
    pub(super) const unsafe fn new() -> Self {
        Self(PhantomData)
    }

    /// The pin the held lock lends.
    pub(super) fn pin(&self) -> CpuPin<'_> {
        // SAFETY: the one token is reached only through `SCHED`'s guard, which masks this CPU's
        // interrupts while held; a test's own is on the host, where every thread reads index 0.
        unsafe { CpuPin::assume() }
    }

    /// A token for a test's own objects, which no `SchedState` reaches.
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self(PhantomData)
    }
}

/// State that `SCHED` guards, in an object other CPUs share: borrowed only with the
/// [`SchedToken`].
#[repr(transparent)]
pub struct SchedCell<T>(UnsafeCell<T>);

// SAFETY: the interior is reached only with the token, which the holder of `SCHED` alone has and
// which lends one borrow at a time, so the cell may be shared wherever a `T` may move.
unsafe impl<T: Send> Sync for SchedCell<T> {}

impl<T> SchedCell<T> {
    /// A cell holding `value`.
    pub const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }

    /// The interior, for as long as the token is lent.
    #[allow(clippy::mut_from_ref, reason = "the token lent for the same lifetime makes the borrow exclusive")]
    pub fn get<'t>(&'t self, _: &'t mut SchedToken) -> &'t mut T {
        // SAFETY: the token is exclusive, and its holder holds `SCHED`, so no other borrow of any
        // cell's interior is live while this one is.
        unsafe { &mut *self.0.get() }
    }

    /// The interior, read for as long as the token is lent shared, which excludes every
    /// [`get`](Self::get).
    pub fn read<'t>(&'t self, _: &'t SchedToken) -> &'t T {
        // SAFETY: as `get`: no `&mut` to any interior is live while the token is shared.
        unsafe { &*self.0.get() }
    }

    /// The interior, its owner's alone.
    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut()
    }

    /// The interior's address, with no reference made, for an owner that holds only a pointer.
    pub const fn raw_get(this: *const Self) -> *mut T {
        UnsafeCell::raw_get(this.cast())
    }
}

/// The target of the [`Weak`] that `weak` picks from `cell`'s interior, with the token lent back
/// for as long. The target is whole while `SCHED` is held, which the token, lent for `'t`, keeps:
/// its closing clears the pointer under the lock before it is freed.
pub fn follow<'t, S, T: Shared>(
    cell: &SchedCell<S>,
    tok: &'t mut SchedToken,
    weak: impl FnOnce(&S) -> Option<&Weak<T>>,
) -> Option<(&'t T, &'t mut SchedToken)> {
    let target = weak(cell.read(tok))?.as_ptr();
    // SAFETY: a `Weak` kept in a `SchedCell` is behind `SCHED`, the lock its target's closing takes
    // to clear it before the target is freed (`Weak::new`'s contract), and `SCHED` is held for
    // `'t`, the token's loan; `T: Shared`, so a `&T` may stand beside writes made through its cells.
    Some((unsafe { &*target }, tok))
}
