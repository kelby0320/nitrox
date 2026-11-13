//! The [`PendingOperation`] kernel object — a one-shot completion waitable.
//!
//! A PO is one in-flight asynchronous operation: per the async-first rule
//! (`docs/rationale/why-async-syscalls.md`), a potentially-blocking syscall returns a PO handle
//! and the caller blocks on it via `sys_wait`. Completion (a DPC for an I/O, or the delivery of
//! a blocking IPC send) **signals** it with a `status`, waking every thread blocked on it.
//!
//! **One-shot**: `pending → signalled` exactly once; a second signal is a no-op, so the first
//! completion wins.
//!
//! ## Mutation discipline
//!
//! As for [`Timer`](crate::object::Timer): all interior state lives in a [`SchedCell`], borrowed
//! only with the token the rank-1 `SCHED` lock keeps (`kernel/docs/lock-ordering.md`).

use crate::libkern::handle::Rights;
use crate::libkern::{AllocError, KBox};
use crate::object::ObjectRef;
use crate::object::header::{Header, Shared, kobject};
use crate::object::{WaitQueue, Waiters};
use crate::sched::{SchedCell, SchedToken};

/// A pending-operation kernel object (`#[repr(C)]`, [`KObjectHeader`] first).
#[repr(C)]
pub struct PendingOperation {
    header: Header<Self>,
    magic: u64,
    inner: SchedCell<PendingOpInner>,
}

kobject!(PendingOperation);
// SAFETY: a `PendingOperation` changes once shared only in `inner`, a `SchedCell`.
unsafe impl Shared for PendingOperation {}

struct PendingOpInner {
    /// `true` once the operation has completed; one-shot (never cleared).
    signaled: bool,
    /// The completion status, valid once `signaled`: `0` = success, else a negative
    /// [`KError`](crate::libkern::KError) discriminant (e.g. `TimedOut`, `PeerClosed`). Surfaced
    /// as the `status` of the `IoResult` `sys_wait` writes.
    status: i32,
    /// The completion **result payload**, meaningful when `status == 0` for operations that
    /// return a value (a namespace lookup's **resolved handle**); the `result` of the `IoResult`,
    /// `0` for status-only completions.
    result: u64,
    /// Blocked threads, non-owning (each is kept alive by its own parked `ObjectRef` and removed
    /// from here before it unparks); at most [`PendingOperation::MAX_WAITERS`].
    waiters: WaitQueue<{ PendingOperation::MAX_WAITERS }>,
    /// A lookup's resolved object, until a reader installs it: the handle is made when `sys_wait`
    /// reports the completion, so a PO freed unread releases the object and leaves no handle.
    parked: Option<(ObjectRef, Rights)>,
    /// A reader is installing `parked`; another waits for the handle it makes, the `result`.
    installing: bool,
}

impl PendingOperation {
    /// Sentinel written into [`PendingOperation::magic`] at construction.
    pub const MAGIC: u64 = 0x50_65_6e_64_4f_70_21_21; // "PendOp!!"

    /// Maximum simultaneous waiters. A PO almost always has one (the submitter).
    pub const MAX_WAITERS: usize = 4;

    /// Allocate an unsignalled `PendingOperation` (refcount one).
    pub fn try_new() -> Result<KBox<Self>, AllocError> {
        KBox::try_new(Self {
            header: Header::new(),
            magic: Self::MAGIC,
            inner: SchedCell::new(PendingOpInner {
                signaled: false,
                status: 0,
                result: 0,
                waiters: WaitQueue::new(),
                parked: None,
                installing: false,
            }),
        })
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    /// Mark the operation complete with `status` **and** a `result` payload. **One-shot**: a PO
    /// already signalled is untouched (the first completion wins). Returns `true` iff this call
    /// made the transition, so the caller knows to wake waiters.
    pub(crate) fn signal_with_result(&self, tok: &mut SchedToken, status: i32, result: u64) -> bool {
        let inner = self.inner.get(tok);
        if inner.signaled {
            return false;
        }
        inner.status = status;
        inner.result = result;
        inner.signaled = true;
        true
    }

    /// The completion status (`0` before signalled); stable after the one-shot transition.
    pub(crate) fn status(&self, tok: &SchedToken) -> i32 {
        self.inner.read(tok).status
    }

    /// The completion result payload (`0` before signalled); stable after the transition.
    pub(crate) fn result(&self, tok: &SchedToken) -> u64 {
        self.inner.read(tok).result
    }

    /// `true` iff the operation has completed — the `sys_wait` fast-path check.
    pub(crate) fn already_signaled(&self, tok: &SchedToken) -> bool {
        self.inner.read(tok).signaled
    }

    /// Park a lookup's object for its reader (see `parked`).
    pub(crate) fn park(&self, tok: &mut SchedToken, parked: Option<(ObjectRef, Rights)>) {
        self.inner.get(tok).parked = parked;
    }

    /// The parked object, for a reader to install, then end with [`installed`](Self::installed);
    /// `None` with none parked, or with another reader installing it, which `Err` says.
    pub(crate) fn take_parked(&self, tok: &mut SchedToken) -> Result<Option<(ObjectRef, Rights)>, ()> {
        let inner = self.inner.get(tok);
        if inner.installing {
            return Err(());
        }
        inner.installing = inner.parked.is_some();
        Ok(inner.parked.take())
    }

    /// End an install: its handle is the result now, or its error is the status, for good.
    pub(crate) fn installed(&self, tok: &mut SchedToken, outcome: Result<u64, i32>) {
        let inner = self.inner.get(tok);
        inner.installing = false;
        match outcome {
            Ok(handle) => inner.result = handle,
            Err(status) => inner.status = status,
        }
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

    /// Take every waiter, leaving none.
    pub(crate) fn take_waiters(&self, tok: &mut SchedToken) -> Waiters<{ PendingOperation::MAX_WAITERS }> {
        let inner = self.inner.get(tok);
        inner.waiters.take()
    }
}

impl Drop for PendingOperation {
    /// A live waiter pins this PO (it holds an `ObjectRef` across `sys_wait`), so the last
    /// reference cannot drop while waiters remain; the assert documents that.
    fn drop(&mut self) {
        debug_assert!(self.inner.get_mut().waiters.is_empty(), "PendingOperation dropped with live waiters");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;
    use crate::object::ObjectRef;
    use crate::object::header::test_probe;

    fn fresh() -> (KBox<PendingOperation>, SchedToken) {
        init_global_heap();
        (PendingOperation::try_new().unwrap(), SchedToken::for_test())
    }

    #[test]
    fn try_new_is_unsignalled_with_magic() {
        let (p, tok) = fresh();
        assert!(p.magic_ok());
        assert!(!p.already_signaled(&tok));
        assert_eq!(p.status(&tok), 0);
    }

    #[test]
    fn signal_is_one_shot_and_records_status() {
        let (p, mut tok) = fresh();
        assert!(p.signal_with_result(&mut tok, 0, 0));
        assert!(p.already_signaled(&tok));
        assert_eq!(p.status(&tok), 0);
        assert!(!p.signal_with_result(&mut tok, -12, 0));
        assert_eq!(p.status(&tok), 0, "status must not change");
    }

    #[test]
    fn signal_carries_a_negative_status() {
        let (p, mut tok) = fresh();
        assert!(p.signal_with_result(&mut tok, -12, 0)); // e.g. TimedOut
        assert!(p.already_signaled(&tok));
        assert_eq!(p.status(&tok), -12);
        assert_eq!(p.result(&tok), 0);
    }

    #[test]
    fn signal_with_result_records_status_and_payload() {
        let (p, mut tok) = fresh();
        assert_eq!(p.result(&tok), 0, "zero before signal");
        assert!(p.signal_with_result(&mut tok, 0, 0xCAFE));
        assert!(p.already_signaled(&tok));
        assert_eq!(p.status(&tok), 0);
        assert_eq!(p.result(&tok), 0xCAFE);
        assert!(!p.signal_with_result(&mut tok, -10, 0xDEAD));
        assert_eq!(p.status(&tok), 0, "status unchanged");
        assert_eq!(p.result(&tok), 0xCAFE, "result unchanged");
    }

    #[test]
    fn waiters_add_remove_take_caps_at_max() {
        let (p, mut tok) = fresh();
        let tok = &mut tok;
        let ths: [*mut (); PendingOperation::MAX_WAITERS] = core::array::from_fn(|i| (0x1000 + i) as *mut ());
        for &t in &ths {
            assert!(p.add_waiter(tok, t).is_ok());
        }
        assert!(p.add_waiter(tok, core::ptr::without_provenance_mut(0xDEAD)).is_err());
        p.remove_waiter(tok, ths[1]);
        assert!(p.add_waiter(tok, core::ptr::without_provenance_mut(0xBEEF)).is_ok());
        assert_eq!(&p.take_waiters(tok)[..], &[ths[0], ths[2], ths[3], core::ptr::without_provenance_mut(0xBEEF)]);
        assert!(p.take_waiters(tok).is_empty());
    }

    #[test]
    fn dispatch_destroy_runs_pending_op_arm() {
        init_global_heap();
        test_probe::reset();
        let r = ObjectRef::new(PendingOperation::try_new().unwrap());
        assert_eq!(test_probe::pending_op_destroys(), 0);
        drop(r);
        assert_eq!(test_probe::pending_op_destroys(), 1);
    }
}
