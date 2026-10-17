//! The [`NotificationChannel`] kernel object — a process's bounded, waitable
//! queue of [`Notification`]s.
//!
//! Each process owns one channel; the kernel enqueues structured events (CPU-exception faults,
//! `ChildExited`) and the process drains them with `sys_notif_recv`. The channel is
//! `sys_wait`-able, signalling when its queue goes empty→non-empty, like a
//! [`Timer`](crate::object::Timer).
//!
//! ## Mutation discipline
//!
//! As for [`Timer`], all interior state lives in an [`UnsafeCell`] touched **only under the
//! rank-1 `SCHED` lock** (`kernel/docs/lock-ordering.md`), reached through a type-erased
//! `*mut ()` so no aliasing `&mut NotificationChannel` is formed.

use core::cell::UnsafeCell;

use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox, KVec, Notification};
use crate::object::header::KObjectHeader;

/// A per-process notification queue (`#[repr(C)]`, [`KObjectHeader`] first).
#[repr(C)]
pub struct NotificationChannel {
    header: KObjectHeader,
    magic: u64,
    /// All mutable state, reached only under `SCHED`.
    inner: UnsafeCell<Inner>,
}

struct Inner {
    /// FIFO of pending notifications; pre-reserved to [`NotificationChannel::QUEUE_CAP`].
    queue: KVec<Notification>,
    /// Notifications dropped on overflow; surfaced as a synthetic `NotificationsDropped` on the
    /// next recv.
    dropped: u32,
    /// Blocked threads (type-erased, non-owning, removed before a waiter unparks); reserved to
    /// [`NotificationChannel::MAX_WAITERS`].
    waiters: KVec<*mut ()>,
}

// SAFETY: identical reasoning to `Timer`/`ObjectRef` — the header refcount is
// atomic and every access to `inner` is serialised under the single-CPU `SCHED`
// lock, so sharing/moving a channel across contexts cannot race.
unsafe impl Send for NotificationChannel {}
// SAFETY: as `Send`.
unsafe impl Sync for NotificationChannel {}

impl NotificationChannel {
    /// Sentinel written into [`NotificationChannel::magic`] at construction.
    pub const MAGIC: u64 = 0x4e_6f_74_69_43_68_21_21; // "NotiCh!!"

    /// Bounded queue depth (default per `docs/spec/notification-format.md`).
    pub const QUEUE_CAP: usize = 64;

    /// Maximum simultaneous waiters; the reserve keeps `add_waiter` from allocating under
    /// `SCHED`.
    pub const MAX_WAITERS: usize = 4;

    /// Allocate an empty channel (refcount one), reserving queue and waiters up front.
    pub fn try_new() -> Result<KBox<Self>, AllocError> {
        let mut queue: KVec<Notification> = KVec::new();
        queue.try_reserve(Self::QUEUE_CAP)?;
        let mut waiters: KVec<*mut ()> = KVec::new();
        waiters.try_reserve(Self::MAX_WAITERS)?;
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::NotificationChannel),
            magic: Self::MAGIC,
            inner: UnsafeCell::new(Inner { queue, dropped: 0, waiters }),
        })
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    // SAFETY (shared by all): `obj` is a live `NotificationChannel` pinned by the caller's
    // `ObjectRef`, and the caller holds `SCHED`, which serialises all access to `inner`.

    /// Borrow the interior mutably (no aliasing; `SCHED` held).
    ///
    /// # Safety
    /// See the accessor contract above.
    #[allow(clippy::mut_from_ref, reason = "this accessor's contract makes the `&mut` the only reference")]
    unsafe fn inner<'a>(obj: *mut ()) -> &'a mut Inner {
        // SAFETY: forming a shared `&NotificationChannel` to reach the
        // `UnsafeCell`, then a `&mut Inner` through it, is the interior-
        // mutability contract — sound while `SCHED` serialises access.
        let c = unsafe { &*(obj as *const NotificationChannel) };
        // SAFETY: as above.
        unsafe { &mut *c.inner.get() }
    }

    /// Enqueue `n`; `true` iff the channel went empty→signaled (the caller then wakes its
    /// waiters). Overflow (`docs/spec/notification-format.md`): on a full queue an exception
    /// notification evicts the oldest non-exception entry; otherwise (or if the queue is all
    /// exceptions) the drop counter is incremented.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn enqueue(obj: *mut (), n: Notification) -> bool {
        // SAFETY: the accessor contract: `obj` is a live `NotificationChannel` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        // "Signaled" is queue non-empty or a drop pending; report the edge to avoid a double wake.
        let was_signaled = !inner.queue.is_empty() || inner.dropped > 0;
        if inner.queue.len() < Self::QUEUE_CAP {
            // Non-growing: under `SCHED`, `try_push` would `kmalloc`. `QUEUE_CAP` is reserved at
            // construction, so this cannot refuse.
            let _ = inner.queue.push_within_capacity(n);
        } else if n.is_exception() {
            // Evict the oldest non-exception entry to keep fault info.
            if let Some(i) = inner.queue.iter().position(|e| !e.is_exception()) {
                inner.queue.remove(i);
                let _ = inner.queue.push_within_capacity(n);
            } else {
                // All exceptions and full: count the drop rather than grow or lose it silently.
                inner.dropped = inner.dropped.saturating_add(1);
            }
        } else {
            inner.dropped = inner.dropped.saturating_add(1);
        }
        !was_signaled
    }

    /// Pop the oldest notification, or first a synthetic `NotificationsDropped { count }` if drops
    /// are pending (resetting the counter).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn try_recv(obj: *mut ()) -> Option<Notification> {
        // SAFETY: the accessor contract: `obj` is a live `NotificationChannel` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        if inner.dropped > 0 {
            let count = inner.dropped;
            inner.dropped = 0;
            return Some(Notification::notifications_dropped(count));
        }
        if inner.queue.is_empty() { None } else { Some(inner.queue.remove(0)) }
    }

    /// `true` iff a recv would return something — the waitable "signaled" predicate.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn already_signaled(obj: *mut ()) -> bool {
        // SAFETY: the accessor contract: `obj` is a live `NotificationChannel` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        !inner.queue.is_empty() || inner.dropped > 0
    }

    /// Register `thread` as a waiter; `Err(())` at [`MAX_WAITERS`](Self::MAX_WAITERS).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn add_waiter(obj: *mut (), thread: *mut ()) -> Result<(), ()> {
        // SAFETY: the accessor contract: `obj` is a live `NotificationChannel` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        if inner.waiters.len() < Self::MAX_WAITERS {
            inner
                .waiters
                // `push_within_capacity`, not `try_push`: this runs under `SCHED`, and
                // `try_push` grows, which is `kmalloc` under the rank-1 lock. Construction
                // reserves `MAX_WAITERS`, so `len < MAX ⟹ len < cap`; the type should say so.
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
        // SAFETY: the accessor contract: `obj` is a live `NotificationChannel` its caller pins, and `SCHED` is held.
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
        // SAFETY: the accessor contract: `obj` is a live `NotificationChannel` its caller pins, and `SCHED` is held.
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

impl Drop for NotificationChannel {
    /// A live waiter pins this channel (it holds an `ObjectRef` across `sys_wait`), so the last
    /// reference cannot drop while waiters remain.
    fn drop(&mut self) {
        debug_assert!(self.inner.get_mut().waiters.is_empty(), "NotificationChannel dropped with live waiters");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libkern::notification::{FaultKind, KIND_NOTIFICATIONS_DROPPED};
    use crate::mm::test_support::init_global_heap;
    use crate::object::ObjectRef;
    use crate::object::header::test_probe;

    fn new_chan() -> *mut () {
        KBox::into_raw(NotificationChannel::try_new().unwrap()).as_ptr() as *mut ()
    }

    fn drop_chan(obj: *mut ()) {
        // SAFETY: single creation reference; reclaim it.
        drop(unsafe {
            KBox::<NotificationChannel>::from_raw(core::ptr::NonNull::new_unchecked(obj as *mut NotificationChannel))
        });
    }

    #[test]
    fn enqueue_recv_fifo_and_empty_edge() {
        init_global_heap();
        let obj = new_chan();
        // SAFETY: live channel, single-threaded test (stands in for SCHED).
        unsafe {
            assert!(!NotificationChannel::already_signaled(obj));
            assert!(NotificationChannel::try_recv(obj).is_none());
            assert!(NotificationChannel::enqueue(obj, Notification::divide_by_zero(1, 0x10)));
            assert!(!NotificationChannel::enqueue(obj, Notification::illegal_insn(2, 0x20)));
            assert!(NotificationChannel::already_signaled(obj));
            assert_eq!(
                NotificationChannel::try_recv(obj).unwrap().kind(),
                crate::libkern::notification::KIND_DIVIDE_BY_ZERO
            );
            assert_eq!(
                NotificationChannel::try_recv(obj).unwrap().kind(),
                crate::libkern::notification::KIND_ILLEGAL_INSN
            );
            assert!(NotificationChannel::try_recv(obj).is_none());
        }
        drop_chan(obj);
    }

    #[test]
    fn overflow_non_exception_drops_and_synthesizes() {
        init_global_heap();
        let obj = new_chan();
        // SAFETY: live channel, single-threaded test.
        unsafe {
            for _ in 0..NotificationChannel::QUEUE_CAP {
                NotificationChannel::enqueue(obj, Notification::notifications_dropped(0));
            }
            for _ in 0..5 {
                NotificationChannel::enqueue(obj, Notification::notifications_dropped(0));
            }
            let first = NotificationChannel::try_recv(obj).unwrap();
            assert_eq!(first.kind(), KIND_NOTIFICATIONS_DROPPED);
            assert_eq!(u32::from_le_bytes(first.as_bytes()[4..8].try_into().unwrap()), 5);
            let mut n = 0;
            while NotificationChannel::try_recv(obj).is_some() {
                n += 1;
            }
            assert_eq!(n, NotificationChannel::QUEUE_CAP);
        }
        drop_chan(obj);
    }

    #[test]
    fn overflow_exception_evicts_oldest_non_exception() {
        init_global_heap();
        let obj = new_chan();
        // SAFETY: live channel, single-threaded test.
        unsafe {
            for _ in 0..NotificationChannel::QUEUE_CAP {
                NotificationChannel::enqueue(obj, Notification::notifications_dropped(0));
            }
            NotificationChannel::enqueue(obj, Notification::seg_fault(7, 0x1000, FaultKind::NotMapped));
            let mut saw_seg = false;
            let mut count = 0;
            while let Some(n) = NotificationChannel::try_recv(obj) {
                count += 1;
                if n.is_exception() {
                    saw_seg = true;
                }
            }
            assert_eq!(count, NotificationChannel::QUEUE_CAP, "stayed at capacity, no drop");
            assert!(saw_seg, "the seg_fault survived");
        }
        drop_chan(obj);
    }

    #[test]
    fn overflow_all_exceptions_drop_counts() {
        init_global_heap();
        let obj = new_chan();
        // SAFETY: live channel, single-threaded test.
        unsafe {
            for _ in 0..NotificationChannel::QUEUE_CAP {
                NotificationChannel::enqueue(obj, Notification::seg_fault(1, 0, FaultKind::NotMapped));
            }
            NotificationChannel::enqueue(obj, Notification::seg_fault(2, 0, FaultKind::NotMapped));
            let first = NotificationChannel::try_recv(obj).unwrap();
            assert_eq!(first.kind(), KIND_NOTIFICATIONS_DROPPED);
            assert_eq!(u32::from_le_bytes(first.as_bytes()[4..8].try_into().unwrap()), 1);
        }
        drop_chan(obj);
    }

    #[test]
    fn waiters_add_remove_take_caps_at_max() {
        init_global_heap();
        let obj = new_chan();
        // SAFETY: live channel, single-threaded test.
        unsafe {
            let ths: [*mut (); NotificationChannel::MAX_WAITERS] = core::array::from_fn(|i| (0x1000 + i) as *mut ());
            for &t in &ths {
                assert!(NotificationChannel::add_waiter(obj, t).is_ok());
            }
            assert!(NotificationChannel::add_waiter(obj, 0xDEAD as *mut ()).is_err());
            NotificationChannel::remove_waiter(obj, ths[0]);
            assert!(NotificationChannel::add_waiter(obj, 0xBEEF as *mut ()).is_ok());
            let mut buf = [core::ptr::null_mut(); NotificationChannel::MAX_WAITERS];
            assert_eq!(NotificationChannel::take_waiters(obj, &mut buf), NotificationChannel::MAX_WAITERS);
            let mut buf2 = [core::ptr::null_mut(); NotificationChannel::MAX_WAITERS];
            assert_eq!(NotificationChannel::take_waiters(obj, &mut buf2), 0);
        }
        drop_chan(obj);
    }

    #[test]
    fn dispatch_destroy_runs_channel_arm() {
        init_global_heap();
        test_probe::reset();
        let obj = new_chan();
        // SAFETY: `obj` carries the single creation reference.
        let r = unsafe { ObjectRef::from_raw(obj, KObjectType::NotificationChannel) };
        assert_eq!(test_probe::notification_channel_destroys(), 0);
        drop(r);
        assert_eq!(test_probe::notification_channel_destroys(), 1);
    }
}
