//! The [`InterruptObject`] kernel object — a hardware IRQ source exposed as a **waitable**.
//!
//! An ISR signals it via a DPC; a driver thread blocked in `sys_wait` on its handle services
//! the device in thread context. See `docs/architecture/drivers-and-irps.md`.
//!
//! A **latching edge counter**: each `signal` increments a pending count (an IRQ with no
//! waiter is not lost), and a `sys_wait` returning for the object **consumes** one. As in
//! [`PendingOperation`](super::pending_op), all state is reached only under `SCHED`.

use core::cell::UnsafeCell;

use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox, KVec};
use crate::object::header::KObjectHeader;

/// A hardware-interrupt waitable (`#[repr(C)]`, [`KObjectHeader`] first).
#[repr(C)]
pub struct InterruptObject {
    header: KObjectHeader,
    magic: u64,
    /// All mutable state, reached only under `SCHED`.
    inner: UnsafeCell<InterruptInner>,
}

struct InterruptInner {
    /// Pending unconsumed interrupts (saturating); `> 0` ⇒ the waitable is signaled.
    pending: u32,
    /// Blocked threads, non-owning (each is kept alive by its own parked `ObjectRef`).
    waiters: KVec<*mut ()>,
}

// SAFETY: identical reasoning to `PendingOperation` — the header refcount is
// atomic, and every access to `inner` is serialised under the single-CPU `SCHED`
// lock, so sharing/moving an `InterruptObject` across contexts cannot race.
unsafe impl Send for InterruptObject {}
// SAFETY: as `Send`.
unsafe impl Sync for InterruptObject {}

impl InterruptObject {
    /// Sentinel written into [`InterruptObject::magic`] at construction.
    pub const MAGIC: u64 = 0x49_6e_74_72_4f_62_6a_21; // "IntrObj!"

    /// Maximum simultaneous waiters; a device IRQ almost always has one. The reserve bounds
    /// `add_waiter` so it never allocates under `SCHED`.
    pub const MAX_WAITERS: usize = 4;

    /// Allocate an unsignalled `InterruptObject` with a refcount of one.
    pub fn try_new() -> Result<KBox<Self>, AllocError> {
        let mut waiters: KVec<*mut ()> = KVec::new();
        waiters.try_reserve(Self::MAX_WAITERS)?;
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::InterruptObject),
            magic: Self::MAGIC,
            inner: UnsafeCell::new(InterruptInner { pending: 0, waiters }),
        })
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    // SAFETY (shared by all): `obj` is a live `InterruptObject` pinned by the caller's
    // `ObjectRef`, and the caller holds `SCHED`, which serialises all access to `inner`.

    /// Borrow the interior mutably. Must not outlive the `SCHED` hold nor alias.
    ///
    /// # Safety
    /// See the accessor contract above.
    #[allow(clippy::mut_from_ref, reason = "this accessor's contract makes the `&mut` the only reference")]
    unsafe fn inner<'a>(obj: *mut ()) -> &'a mut InterruptInner {
        // SAFETY: `obj` is a live `InterruptObject`; forming a shared reference to
        // reach the `UnsafeCell`, then a `&mut InterruptInner` through it, is the
        // interior-mutability contract — sound while `SCHED` serialises access.
        let p = unsafe { &*(obj as *const InterruptObject) };
        // SAFETY: as above.
        unsafe { &mut *p.inner.get() }
    }

    /// Record an interrupt (saturating); returns `true`, as callers always wake waiters.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn signal(obj: *mut ()) -> bool {
        // SAFETY: the accessor contract: `obj` is a live `InterruptObject` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        inner.pending = inner.pending.saturating_add(1);
        true
    }

    /// Consume one pending interrupt (a `sys_wait` returned for this object); idempotent at zero.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn consume(obj: *mut ()) {
        // SAFETY: the accessor contract: `obj` is a live `InterruptObject` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        if inner.pending > 0 {
            inner.pending -= 1;
        }
    }

    /// `true` iff an interrupt is pending — the `sys_wait` fast-path predicate.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn already_signaled(obj: *mut ()) -> bool {
        // SAFETY: the accessor contract: `obj` is a live `InterruptObject` its caller pins, and `SCHED` is held.
        unsafe { Self::inner(obj) }.pending > 0
    }

    /// Register `thread` as a waiter; `Err(())` at [`MAX_WAITERS`](Self::MAX_WAITERS).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn add_waiter(obj: *mut (), thread: *mut ()) -> Result<(), ()> {
        // SAFETY: the accessor contract: `obj` is a live `InterruptObject` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        if inner.waiters.len() < Self::MAX_WAITERS {
            inner
                .waiters
                // `push_within_capacity`, not `try_push`: this runs under `SCHED`, and `try_push`
                // grows, which is `kmalloc` under the rank-1 lock. Construction reserves
                // `MAX_WAITERS`, so `len < MAX ⟹ len < cap`; the type should say so.
                .push_within_capacity(thread)
                .expect("within reserved waiter capacity");
            Ok(())
        } else {
            Err(())
        }
    }

    /// Remove `thread` from the waiter set if present (idempotent).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn remove_waiter(obj: *mut (), thread: *mut ()) {
        // SAFETY: the accessor contract: `obj` is a live `InterruptObject` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        if let Some(i) = inner.waiters.iter().position(|&w| w == thread) {
            inner.waiters.remove(i);
        }
    }

    /// Drain every waiter into `out` (at least [`MAX_WAITERS`](Self::MAX_WAITERS) long).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn take_waiters(obj: *mut (), out: &mut [*mut ()]) -> usize {
        // SAFETY: the accessor contract: `obj` is a live `InterruptObject` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        let n = inner.waiters.len();
        debug_assert!(out.len() >= n);
        for (i, &w) in inner.waiters.iter().enumerate() {
            out[i] = w;
        }
        inner.waiters.clear();
        n
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

    fn fresh() -> *mut () {
        init_global_heap();
        KBox::into_raw(InterruptObject::try_new().unwrap()).as_ptr() as *mut ()
    }

    #[test]
    fn try_new_has_magic_and_is_unsignalled() {
        init_global_heap();
        let io = InterruptObject::try_new().unwrap();
        assert!(io.magic_ok());
        let p = KBox::into_raw(io).as_ptr() as *mut ();
        // SAFETY: live object, single-threaded test stands in for SCHED.
        assert!(!unsafe { InterruptObject::already_signaled(p) });
        // SAFETY: adopts the creation reference `into_raw` gave up, to drop it.
        drop(unsafe { ObjectRef::from_raw(p, KObjectType::InterruptObject) });
    }

    #[test]
    fn signal_latches_and_consume_clears() {
        let p = fresh();
        // SAFETY: live object; single-threaded test serialises like SCHED.
        unsafe {
            assert!(InterruptObject::signal(p));
            assert!(InterruptObject::already_signaled(p));
            InterruptObject::signal(p);
            InterruptObject::consume(p);
            assert!(InterruptObject::already_signaled(p), "second IRQ still pending");
            InterruptObject::consume(p);
            assert!(!InterruptObject::already_signaled(p));
            InterruptObject::consume(p);
            assert!(!InterruptObject::already_signaled(p));
        }
        // SAFETY: adopts the creation reference `into_raw` gave up, to drop it.
        drop(unsafe { ObjectRef::from_raw(p, KObjectType::InterruptObject) });
    }

    #[test]
    fn waiters_add_take_and_remove() {
        let p = fresh();
        let t1 = 0x1000usize as *mut ();
        let t2 = 0x2000usize as *mut ();
        // SAFETY: live object; single-threaded test.
        unsafe {
            assert!(InterruptObject::add_waiter(p, t1).is_ok());
            assert!(InterruptObject::add_waiter(p, t2).is_ok());
            InterruptObject::remove_waiter(p, t1);
            let mut buf = [core::ptr::null_mut(); InterruptObject::MAX_WAITERS];
            let n = InterruptObject::take_waiters(p, &mut buf);
            assert_eq!(n, 1);
            assert_eq!(buf[0], t2);
        }
        // SAFETY: adopts the creation reference `into_raw` gave up, to drop it.
        drop(unsafe { ObjectRef::from_raw(p, KObjectType::InterruptObject) });
    }

    #[test]
    fn dropping_last_objectref_routes_through_dispatch_destroy() {
        init_global_heap();
        test_probe::reset();
        // SAFETY: adopt the creation reference and drop it.
        let r = unsafe {
            ObjectRef::from_raw(
                KBox::into_raw(InterruptObject::try_new().unwrap()).as_ptr() as *mut (),
                KObjectType::InterruptObject,
            )
        };
        assert_eq!(test_probe::interrupt_object_destroys(), 0);
        drop(r);
        assert_eq!(test_probe::interrupt_object_destroys(), 1);
    }
}
