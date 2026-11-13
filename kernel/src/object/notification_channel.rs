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
//! As for [`Timer`], all interior state lives in a [`SchedCell`], borrowed only with the token the
//! rank-1 `SCHED` lock keeps (`kernel/docs/lock-ordering.md`).

use crate::libkern::{AllocError, KBox, Notification, Reserved};
use crate::object::header::{Header, Shared, kobject};
use crate::object::{WaitQueue, Waiters};
use crate::sched::{SchedCell, SchedToken};

/// A per-process notification queue (`#[repr(C)]`, [`KObjectHeader`] first).
#[repr(C)]
pub struct NotificationChannel {
    header: Header<Self>,
    magic: u64,
    /// All mutable state.
    inner: SchedCell<Inner>,
}

kobject!(NotificationChannel);
// SAFETY: a channel changes once shared only in `inner`, a `SchedCell`.
unsafe impl Shared for NotificationChannel {}

struct Inner {
    /// FIFO of pending notifications; pre-reserved to [`NotificationChannel::QUEUE_CAP`].
    queue: Reserved<Notification>,
    /// Notifications dropped on overflow; surfaced as a synthetic `NotificationsDropped` on the
    /// next recv.
    dropped: u32,
    /// Blocked threads (type-erased, non-owning, removed before a waiter unparks); at most
    /// [`NotificationChannel::MAX_WAITERS`].
    waiters: WaitQueue<{ NotificationChannel::MAX_WAITERS }>,
}

impl NotificationChannel {
    /// Sentinel written into [`NotificationChannel::magic`] at construction.
    pub const MAGIC: u64 = 0x4e_6f_74_69_43_68_21_21; // "NotiCh!!"

    /// Bounded queue depth (default per `docs/spec/notification-format.md`).
    pub const QUEUE_CAP: usize = 64;

    /// Maximum simultaneous waiters.
    pub const MAX_WAITERS: usize = 4;

    /// Allocate an empty channel (refcount one), reserving its queue up front.
    pub fn try_new() -> Result<KBox<Self>, AllocError> {
        let queue = Reserved::with_capacity(Self::QUEUE_CAP)?;
        KBox::try_new(Self {
            header: Header::new(),
            magic: Self::MAGIC,
            inner: SchedCell::new(Inner { queue, dropped: 0, waiters: WaitQueue::new() }),
        })
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    /// Enqueue `n`; `true` iff the channel went empty→signaled (the caller then wakes its
    /// waiters). Overflow (`docs/spec/notification-format.md`): on a full queue an exception
    /// notification evicts the oldest non-exception entry; otherwise (or if the queue is all
    /// exceptions) the drop counter is incremented.
    pub(crate) fn enqueue(&self, tok: &mut SchedToken, n: Notification) -> bool {
        let inner = self.inner.get(tok);
        // "Signaled" is queue non-empty or a drop pending; report the edge to avoid a double wake.
        let was_signaled = !inner.queue.is_empty() || inner.dropped > 0;
        if inner.queue.len() < Self::QUEUE_CAP {
            // `QUEUE_CAP` is reserved at construction, so this cannot refuse.
            let _ = inner.queue.push(n);
        } else if n.is_exception() {
            // Evict the oldest non-exception entry to keep fault info.
            if let Some(i) = inner.queue.iter().position(|e| !e.is_exception()) {
                inner.queue.remove(i);
                let _ = inner.queue.push(n);
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
    pub(crate) fn try_recv(&self, tok: &mut SchedToken) -> Option<Notification> {
        let inner = self.inner.get(tok);
        if inner.dropped > 0 {
            let count = inner.dropped;
            inner.dropped = 0;
            return Some(Notification::notifications_dropped(count));
        }
        if inner.queue.is_empty() { None } else { Some(inner.queue.remove(0)) }
    }

    /// `true` iff a recv would return something — the waitable "signaled" predicate.
    pub(crate) fn already_signaled(&self, tok: &SchedToken) -> bool {
        let inner = self.inner.read(tok);
        !inner.queue.is_empty() || inner.dropped > 0
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
    pub(crate) fn take_waiters(&self, tok: &mut SchedToken) -> Waiters<{ NotificationChannel::MAX_WAITERS }> {
        let inner = self.inner.get(tok);
        inner.waiters.take()
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

    fn new_chan() -> (KBox<NotificationChannel>, SchedToken) {
        (NotificationChannel::try_new().unwrap(), SchedToken::for_test())
    }

    #[test]
    fn enqueue_recv_fifo_and_empty_edge() {
        init_global_heap();
        let (obj, mut tok) = new_chan();
        let tok = &mut tok;
        assert!(!obj.already_signaled(tok));
        assert!(obj.try_recv(tok).is_none());
        assert!(obj.enqueue(tok, Notification::divide_by_zero(1, 0x10)));
        assert!(!obj.enqueue(tok, Notification::illegal_insn(2, 0x20)));
        assert!(obj.already_signaled(tok));
        assert_eq!(obj.try_recv(tok).unwrap().kind(), crate::libkern::notification::KIND_DIVIDE_BY_ZERO);
        assert_eq!(obj.try_recv(tok).unwrap().kind(), crate::libkern::notification::KIND_ILLEGAL_INSN);
        assert!(obj.try_recv(tok).is_none());
    }

    #[test]
    fn overflow_non_exception_drops_and_synthesizes() {
        init_global_heap();
        let (obj, mut tok) = new_chan();
        let tok = &mut tok;
        for _ in 0..NotificationChannel::QUEUE_CAP {
            obj.enqueue(tok, Notification::notifications_dropped(0));
        }
        for _ in 0..5 {
            obj.enqueue(tok, Notification::notifications_dropped(0));
        }
        let first = obj.try_recv(tok).unwrap();
        assert_eq!(first.kind(), KIND_NOTIFICATIONS_DROPPED);
        assert_eq!(u32::from_le_bytes(first.as_bytes()[4..8].try_into().unwrap()), 5);
        let mut n = 0;
        while obj.try_recv(tok).is_some() {
            n += 1;
        }
        assert_eq!(n, NotificationChannel::QUEUE_CAP);
    }

    #[test]
    fn overflow_exception_evicts_oldest_non_exception() {
        init_global_heap();
        let (obj, mut tok) = new_chan();
        let tok = &mut tok;
        for _ in 0..NotificationChannel::QUEUE_CAP {
            obj.enqueue(tok, Notification::notifications_dropped(0));
        }
        obj.enqueue(tok, Notification::seg_fault(7, 0x1000, FaultKind::NotMapped));
        let mut saw_seg = false;
        let mut count = 0;
        while let Some(n) = obj.try_recv(tok) {
            count += 1;
            if n.is_exception() {
                saw_seg = true;
            }
        }
        assert_eq!(count, NotificationChannel::QUEUE_CAP, "stayed at capacity, no drop");
        assert!(saw_seg, "the seg_fault survived");
    }

    #[test]
    fn overflow_all_exceptions_drop_counts() {
        init_global_heap();
        let (obj, mut tok) = new_chan();
        let tok = &mut tok;
        for _ in 0..NotificationChannel::QUEUE_CAP {
            obj.enqueue(tok, Notification::seg_fault(1, 0, FaultKind::NotMapped));
        }
        obj.enqueue(tok, Notification::seg_fault(2, 0, FaultKind::NotMapped));
        let first = obj.try_recv(tok).unwrap();
        assert_eq!(first.kind(), KIND_NOTIFICATIONS_DROPPED);
        assert_eq!(u32::from_le_bytes(first.as_bytes()[4..8].try_into().unwrap()), 1);
    }

    #[test]
    fn waiters_add_remove_take_caps_at_max() {
        init_global_heap();
        let (obj, mut tok) = new_chan();
        let tok = &mut tok;
        let ths: [*mut (); NotificationChannel::MAX_WAITERS] =
            core::array::from_fn(|i| core::ptr::without_provenance_mut(0x1000 + i));
        for &t in &ths {
            assert!(obj.add_waiter(tok, t).is_ok());
        }
        assert!(obj.add_waiter(tok, core::ptr::without_provenance_mut(0xDEAD)).is_err());
        obj.remove_waiter(tok, ths[0]);
        assert!(obj.add_waiter(tok, core::ptr::without_provenance_mut(0xBEEF)).is_ok());
        assert_eq!(&obj.take_waiters(tok)[..], &[ths[1], ths[2], ths[3], core::ptr::without_provenance_mut(0xBEEF)]);
        assert!(obj.take_waiters(tok).is_empty());
    }

    #[test]
    fn dispatch_destroy_runs_channel_arm() {
        init_global_heap();
        test_probe::reset();
        let r = ObjectRef::new(NotificationChannel::try_new().unwrap());
        assert_eq!(test_probe::notification_channel_destroys(), 0);
        drop(r);
        assert_eq!(test_probe::notification_channel_destroys(), 1);
    }
}
