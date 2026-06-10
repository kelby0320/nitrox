//! The [`NotificationChannel`] kernel object — a process's bounded, waitable
//! queue of [`Notification`]s.
//!
//! Each process owns one channel; the kernel enqueues structured events (this
//! slice: CPU-exception faults) and the process drains them with
//! `sys_notif_recv`. The channel is `sys_wait`-able — it signals when its queue
//! transitions empty→non-empty — so it composes with other waitables, exactly
//! like a [`Timer`](crate::object::Timer).
//!
//! ## Mutation discipline
//!
//! Like [`Timer`], all interior state lives in an [`UnsafeCell`] touched
//! **only while the rank-1 `SCHED` lock is held** (single-CPU serialisation;
//! see `kernel/docs/lock-ordering.md`). The `pub(crate) unsafe fn` accessors
//! take a type-erased `*mut ()` and reach the interior through that cell — no
//! aliasing `&mut NotificationChannel` is ever formed.

use core::cell::UnsafeCell;

use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox, KVec, Notification};
use crate::object::header::KObjectHeader;

/// A per-process notification queue.
///
/// `#[repr(C)]` with [`KObjectHeader`] first — see [`crate::object::header`].
#[repr(C)]
pub struct NotificationChannel {
    header: KObjectHeader,
    /// Self-check sentinel; a live object always reads [`NotificationChannel::MAGIC`].
    magic: u64,
    /// All mutable state, reached only under `SCHED`.
    inner: UnsafeCell<Inner>,
}

struct Inner {
    /// FIFO of pending notifications; pre-reserved to [`NotificationChannel::QUEUE_CAP`].
    queue: KVec<Notification>,
    /// Count of notifications dropped on overflow; surfaced as a synthetic
    /// `NotificationsDropped` on the next recv.
    dropped: u32,
    /// Threads blocked on this channel (type-erased `Thread` pointers; non-owning,
    /// removed before a waiter unparks). Pre-reserved to [`NotificationChannel::MAX_WAITERS`].
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

    /// Maximum simultaneous waiters; bounds the pre-reserved waiter vector so
    /// `add_waiter` never allocates under `SCHED`. Matches [`Timer::MAX_WAITERS`].
    ///
    /// [`Timer::MAX_WAITERS`]: crate::object::Timer::MAX_WAITERS
    pub const MAX_WAITERS: usize = 4;

    /// Allocate an empty channel with a refcount of one. The queue and waiter
    /// vectors are reserved up front (the only fallible growth), so later
    /// `enqueue`/`add_waiter` stay within capacity and never allocate under the
    /// scheduler lock.
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

    // --- Scheduler-only accessors --------------------------------------
    //
    // SAFETY (shared by all): `obj` addresses a live `NotificationChannel`
    // (pinned by an `ObjectRef` the caller holds), and the caller holds `SCHED`,
    // which — single-CPU — serialises all access to `inner`.

    /// Borrow the interior mutably (no aliasing; `SCHED` held).
    ///
    /// # Safety
    /// See the accessor contract above.
    #[allow(clippy::mut_from_ref)]
    unsafe fn inner<'a>(obj: *mut ()) -> &'a mut Inner {
        // SAFETY: forming a shared `&NotificationChannel` to reach the
        // `UnsafeCell`, then a `&mut Inner` through it, is the interior-
        // mutability contract — sound while `SCHED` serialises access.
        let c = unsafe { &*(obj as *const NotificationChannel) };
        unsafe { &mut *c.inner.get() }
    }

    /// Enqueue `n`. Returns `true` iff the channel went empty→signaled (the
    /// caller must then wake its waiters). Overflow policy
    /// (`docs/spec/notification-format.md`): if the queue is full, an exception
    /// notification evicts the oldest non-exception entry; otherwise (and if the
    /// queue is all-exceptions) the drop counter is incremented.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn enqueue(obj: *mut (), n: Notification) -> bool {
        let inner = unsafe { Self::inner(obj) };
        // "Signaled" == queue non-empty OR a drop is pending. Report the edge so
        // waiters aren't double-woken when already wakeable.
        let was_signaled = !inner.queue.is_empty() || inner.dropped > 0;
        if inner.queue.len() < Self::QUEUE_CAP {
            inner.queue.try_push(n).expect("within reserved queue capacity");
        } else if n.is_exception() {
            // Evict the oldest non-exception entry to preserve fault info.
            if let Some(i) = inner.queue.iter().position(|e| !e.is_exception()) {
                inner.queue.remove(i);
                inner.queue.try_push(n).expect("freed a slot");
            } else {
                // All-exceptions and full: drop-count rather than grow/lose silently.
                inner.dropped = inner.dropped.saturating_add(1);
            }
        } else {
            inner.dropped = inner.dropped.saturating_add(1);
        }
        !was_signaled
    }

    /// Pop the oldest notification, or — if drops are pending — a synthetic
    /// `NotificationsDropped { count }` first (resetting the counter). `None`
    /// when truly empty.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn try_recv(obj: *mut ()) -> Option<Notification> {
        let inner = unsafe { Self::inner(obj) };
        if inner.dropped > 0 {
            let count = inner.dropped;
            inner.dropped = 0;
            return Some(Notification::notifications_dropped(count));
        }
        if inner.queue.is_empty() {
            None
        } else {
            Some(inner.queue.remove(0))
        }
    }

    /// `true` iff a recv would return something (queue non-empty or a drop is
    /// pending) — the waitable "signaled" predicate.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn already_signaled(obj: *mut ()) -> bool {
        let inner = unsafe { Self::inner(obj) };
        !inner.queue.is_empty() || inner.dropped > 0
    }

    /// Register `thread` as a waiter. `Err(())` if already at
    /// [`MAX_WAITERS`](Self::MAX_WAITERS); never grows under the lock.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn add_waiter(obj: *mut (), thread: *mut ()) -> Result<(), ()> {
        let inner = unsafe { Self::inner(obj) };
        if inner.waiters.len() < Self::MAX_WAITERS {
            inner
                .waiters
                .try_push(thread)
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
        let inner = unsafe { Self::inner(obj) };
        if let Some(i) = inner.waiters.iter().position(|&w| w == thread) {
            inner.waiters.remove(i);
        }
    }

    /// Drain every waiter into `out` (clearing the set), returning the count.
    /// `out` must be at least [`MAX_WAITERS`](Self::MAX_WAITERS) long.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn take_waiters(obj: *mut (), out: &mut [*mut ()]) -> usize {
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
    /// A live waiter pins this channel (each holds an `ObjectRef` on it across
    /// `sys_wait`), so the last reference cannot drop while waiters remain.
    fn drop(&mut self) {
        debug_assert!(
            self.inner.get_mut().waiters.is_empty(),
            "NotificationChannel dropped with live waiters"
        );
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
            KBox::<NotificationChannel>::from_raw(core::ptr::NonNull::new_unchecked(
                obj as *mut NotificationChannel,
            ))
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
            // First enqueue reports the empty→signaled edge; second does not.
            assert!(NotificationChannel::enqueue(obj, Notification::divide_by_zero(1, 0x10)));
            assert!(!NotificationChannel::enqueue(obj, Notification::illegal_insn(2, 0x20)));
            assert!(NotificationChannel::already_signaled(obj));
            // FIFO order.
            assert_eq!(NotificationChannel::try_recv(obj).unwrap().kind(),
                crate::libkern::notification::KIND_DIVIDE_BY_ZERO);
            assert_eq!(NotificationChannel::try_recv(obj).unwrap().kind(),
                crate::libkern::notification::KIND_ILLEGAL_INSN);
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
            // Fill with non-exception notifications (use NotificationsDropped kind,
            // which is not in the 0x0100 range).
            for _ in 0..NotificationChannel::QUEUE_CAP {
                NotificationChannel::enqueue(obj, Notification::notifications_dropped(0));
            }
            // Five more overflow → dropped += 5.
            for _ in 0..5 {
                NotificationChannel::enqueue(obj, Notification::notifications_dropped(0));
            }
            // Drain the 64 queued first... actually dropped is surfaced FIRST.
            let first = NotificationChannel::try_recv(obj).unwrap();
            assert_eq!(first.kind(), KIND_NOTIFICATIONS_DROPPED);
            assert_eq!(u32::from_le_bytes(first.as_bytes()[4..8].try_into().unwrap()), 5);
            // Then the 64 queued entries remain.
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
            // A full queue + an exception → evicts oldest non-exception, no drop.
            NotificationChannel::enqueue(obj, Notification::seg_fault(7, 0x1000, FaultKind::NotMapped));
            // No drop recorded.
            // Drain: the seg_fault is now in the queue (last), 63 non-exceptions before it.
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
            // Full of exceptions → the next exception drop-counts (no eviction).
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
            let ths: [*mut (); NotificationChannel::MAX_WAITERS] =
                core::array::from_fn(|i| (0x1000 + i) as *mut ());
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
