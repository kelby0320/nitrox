//! The [`InterruptObject`] kernel object — a hardware IRQ source exposed as a **waitable**.
//!
//! An ISR signals it via a DPC; a driver thread blocked in `sys_wait` on its handle services
//! the device in thread context. See `docs/architecture/drivers-and-irps.md`.
//!
//! A **latching edge counter**: each `signal` increments a pending count (an IRQ with no
//! waiter is not lost), and a `sys_wait` returning for the object **consumes** one. As in
//! [`PendingOperation`](super::pending_op), all state is in a [`SchedCell`].

use crate::libkern::{AllocError, KBox};
use crate::object::header::{Header, Shared, kobject};
use crate::object::{WaitQueue, Waiters};
use crate::sched::{SchedCell, SchedToken};

/// A hardware-interrupt waitable (`#[repr(C)]`, [`KObjectHeader`] first).
#[repr(C)]
pub struct InterruptObject {
    header: Header<Self>,
    magic: u64,
    /// All mutable state.
    inner: SchedCell<InterruptInner>,
}

kobject!(InterruptObject);
// SAFETY: an `InterruptObject` changes once shared only in `inner`, a `SchedCell`.
unsafe impl Shared for InterruptObject {}

struct InterruptInner {
    /// Pending unconsumed interrupts (saturating); `> 0` ⇒ the waitable is signaled.
    pending: u32,
    /// Blocked threads, non-owning (each is kept alive by its own parked `ObjectRef`).
    waiters: WaitQueue<{ InterruptObject::MAX_WAITERS }>,
}

impl InterruptObject {
    /// Sentinel written into [`InterruptObject::magic`] at construction.
    pub const MAGIC: u64 = 0x49_6e_74_72_4f_62_6a_21; // "IntrObj!"

    /// Maximum simultaneous waiters; a device IRQ almost always has one.
    pub const MAX_WAITERS: usize = 4;

    /// Allocate an unsignalled `InterruptObject` with a refcount of one.
    pub fn try_new() -> Result<KBox<Self>, AllocError> {
        KBox::try_new(Self {
            header: Header::new(),
            magic: Self::MAGIC,
            inner: SchedCell::new(InterruptInner { pending: 0, waiters: WaitQueue::new() }),
        })
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    /// Record an interrupt (saturating); returns `true`, as callers always wake waiters.
    pub(crate) fn signal(&self, tok: &mut SchedToken) -> bool {
        let inner = self.inner.get(tok);
        inner.pending = inner.pending.saturating_add(1);
        true
    }

    /// Consume one pending interrupt (a `sys_wait` returned for this object); idempotent at zero.
    pub(crate) fn consume(&self, tok: &mut SchedToken) {
        let inner = self.inner.get(tok);
        if inner.pending > 0 {
            inner.pending -= 1;
        }
    }

    /// `true` iff an interrupt is pending — the `sys_wait` fast-path predicate.
    pub(crate) fn already_signaled(&self, tok: &SchedToken) -> bool {
        self.inner.read(tok).pending > 0
    }

    /// Register `thread` as a waiter; `Err(())` at [`MAX_WAITERS`](Self::MAX_WAITERS).
    pub(crate) fn add_waiter(&self, tok: &mut SchedToken, thread: *mut ()) -> Result<(), ()> {
        let inner = self.inner.get(tok);
        inner.waiters.add(thread)
    }

    /// Remove `thread` from the waiter set if present (idempotent).
    pub(crate) fn remove_waiter(&self, tok: &mut SchedToken, thread: *mut ()) {
        let inner = self.inner.get(tok);
        inner.waiters.remove(thread);
    }

    /// Take every waiter, leaving none.
    pub(crate) fn take_waiters(&self, tok: &mut SchedToken) -> Waiters<{ InterruptObject::MAX_WAITERS }> {
        let inner = self.inner.get(tok);
        inner.waiters.take()
    }
}

impl Drop for InterruptObject {
    fn drop(&mut self) {
        // Each waiter is removed before it unparks, so none may remain.
        let inner = self.inner.get_mut();
        debug_assert!(inner.waiters.is_empty(), "InterruptObject dropped with waiters still parked");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;
    use crate::object::ObjectRef;
    use crate::object::header::test_probe;

    fn fresh() -> (KBox<InterruptObject>, SchedToken) {
        init_global_heap();
        (InterruptObject::try_new().unwrap(), SchedToken::for_test())
    }

    #[test]
    fn try_new_has_magic_and_is_unsignalled() {
        let (io, tok) = fresh();
        assert!(io.magic_ok());
        assert!(!io.already_signaled(&tok));
    }

    #[test]
    fn signal_latches_and_consume_clears() {
        let (io, mut tok) = fresh();
        let tok = &mut tok;
        assert!(io.signal(tok));
        assert!(io.already_signaled(tok));
        io.signal(tok);
        io.consume(tok);
        assert!(io.already_signaled(tok), "second IRQ still pending");
        io.consume(tok);
        assert!(!io.already_signaled(tok));
        io.consume(tok);
        assert!(!io.already_signaled(tok));
    }

    #[test]
    fn waiters_add_take_and_remove() {
        let (io, mut tok) = fresh();
        let tok = &mut tok;
        let t1 = 0x1000usize as *mut ();
        let t2 = 0x2000usize as *mut ();
        assert!(io.add_waiter(tok, t1).is_ok());
        assert!(io.add_waiter(tok, t2).is_ok());
        io.remove_waiter(tok, t1);
        assert_eq!(&io.take_waiters(tok)[..], &[t2]);
    }

    #[test]
    fn dropping_last_objectref_routes_through_dispatch_destroy() {
        init_global_heap();
        test_probe::reset();
        let r = ObjectRef::new(InterruptObject::try_new().unwrap());
        assert_eq!(test_probe::interrupt_object_destroys(), 0);
        drop(r);
        assert_eq!(test_probe::interrupt_object_destroys(), 1);
    }
}
