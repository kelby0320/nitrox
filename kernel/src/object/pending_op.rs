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
//! As for [`Timer`](crate::object::Timer): all interior state lives in an [`UnsafeCell`]
//! touched **exclusively under the rank-1 `SCHED` lock** (`kernel/docs/lock-ordering.md`),
//! reached through a type-erased `*mut ()` so no aliasing `&mut PendingOperation` is formed.

use core::cell::UnsafeCell;

use crate::libkern::handle::{KObjectType, Rights};
use crate::libkern::{AllocError, KBox, KVec};
use crate::object::ObjectRef;
use crate::object::header::KObjectHeader;

/// A pending-operation kernel object (`#[repr(C)]`, [`KObjectHeader`] first).
#[repr(C)]
pub struct PendingOperation {
    header: KObjectHeader,
    magic: u64,
    inner: UnsafeCell<PendingOpInner>,
}

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
    /// from here before it unparks); pre-reserved to [`PendingOperation::MAX_WAITERS`].
    waiters: KVec<*mut ()>,
    /// A lookup's resolved object, until a reader installs it: the handle is made when `sys_wait`
    /// reports the completion, so a PO freed unread releases the object and leaves no handle.
    parked: Option<(ObjectRef, Rights)>,
    /// A reader is installing `parked`; another waits for the handle it makes, the `result`.
    installing: bool,
}

// SAFETY: identical reasoning to `Timer`/`ObjectRef` — the header refcount is
// atomic, and every access to `inner` is serialised under the single-CPU `SCHED`
// lock, so sharing/moving a `PendingOperation` across contexts cannot race.
unsafe impl Send for PendingOperation {}
// SAFETY: as `Send`.
unsafe impl Sync for PendingOperation {}

impl PendingOperation {
    /// Sentinel written into [`PendingOperation::magic`] at construction.
    pub const MAGIC: u64 = 0x50_65_6e_64_4f_70_21_21; // "PendOp!!"

    /// Maximum simultaneous waiters; the reserve keeps `add_waiter` from allocating under
    /// `SCHED`. A PO almost always has one (the submitter).
    pub const MAX_WAITERS: usize = 4;

    /// Allocate an unsignalled `PendingOperation` (refcount one), reserving waiters up front.
    pub fn try_new() -> Result<KBox<Self>, AllocError> {
        let mut waiters: KVec<*mut ()> = KVec::new();
        waiters.try_reserve(Self::MAX_WAITERS)?;
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::PendingOperation),
            magic: Self::MAGIC,
            inner: UnsafeCell::new(PendingOpInner {
                signaled: false,
                status: 0,
                result: 0,
                waiters,
                parked: None,
                installing: false,
            }),
        })
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    // SAFETY (shared by all): `obj` is a live `PendingOperation` pinned by the caller's
    // `ObjectRef`, and the caller holds `SCHED`, which serialises all access to `inner`.

    /// Borrow the interior mutably. Must not outlive the `SCHED` hold nor alias.
    ///
    /// # Safety
    /// See the accessor contract above.
    #[allow(clippy::mut_from_ref, reason = "this accessor's contract makes the `&mut` the only reference")]
    unsafe fn inner<'a>(obj: *mut ()) -> &'a mut PendingOpInner {
        // SAFETY: `obj` is a live PO; forming a shared `&PendingOperation` to
        // reach the `UnsafeCell`, then a `&mut PendingOpInner` through it, is the
        // interior-mutability contract — sound while `SCHED` serialises access.
        let p = unsafe { &*(obj as *const PendingOperation) };
        // SAFETY: as above.
        unsafe { &mut *p.inner.get() }
    }

    /// Mark the operation complete with `status` **and** a `result` payload. **One-shot**: a PO
    /// already signalled is untouched (the first completion wins). Returns `true` iff this call
    /// made the transition, so the caller knows to wake waiters.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn signal_with_result(obj: *mut (), status: i32, result: u64) -> bool {
        // SAFETY: the accessor contract: `obj` is a live `PendingOperation` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        if inner.signaled {
            return false;
        }
        inner.status = status;
        inner.result = result;
        inner.signaled = true;
        true
    }

    /// The completion status (`0` before signalled); stable after the one-shot transition.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn status(obj: *mut ()) -> i32 {
        // SAFETY: the accessor contract: `obj` is a live `PendingOperation` its caller pins, and `SCHED` is held.
        unsafe { Self::inner(obj) }.status
    }

    /// The completion result payload (`0` before signalled); stable after the transition.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn result(obj: *mut ()) -> u64 {
        // SAFETY: the accessor contract: `obj` is a live `PendingOperation` its caller pins, and `SCHED` is held.
        unsafe { Self::inner(obj) }.result
    }

    /// `true` iff the operation has completed — the `sys_wait` fast-path check.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn already_signaled(obj: *mut ()) -> bool {
        // SAFETY: the accessor contract: `obj` is a live `PendingOperation` its caller pins, and `SCHED` is held.
        unsafe { Self::inner(obj) }.signaled
    }

    /// Park a lookup's object for its reader (see `parked`).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn park(obj: *mut (), parked: Option<(ObjectRef, Rights)>) {
        // SAFETY: the accessor contract: `obj` is a live `PendingOperation` its caller pins, and `SCHED` is held.
        unsafe { Self::inner(obj) }.parked = parked;
    }

    /// The parked object, for a reader to install, then end with [`installed`](Self::installed);
    /// `None` with none parked, or with another reader installing it, which `Err` says.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn take_parked(obj: *mut ()) -> Result<Option<(ObjectRef, Rights)>, ()> {
        // SAFETY: the accessor contract: `obj` is a live `PendingOperation` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        if inner.installing {
            return Err(());
        }
        inner.installing = inner.parked.is_some();
        Ok(inner.parked.take())
    }

    /// End an install: its handle is the result now, or its error is the status, for good.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn installed(obj: *mut (), outcome: Result<u64, i32>) {
        // SAFETY: the accessor contract: `obj` is a live `PendingOperation` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        inner.installing = false;
        match outcome {
            Ok(handle) => inner.result = handle,
            Err(status) => inner.status = status,
        }
    }

    /// Register `thread` as a waiter; `Err(())` at the cap, which callers map to `OutOfMemory`.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn add_waiter(obj: *mut (), thread: *mut ()) -> Result<(), ()> {
        // SAFETY: the accessor contract: `obj` is a live `PendingOperation` its caller pins, and `SCHED` is held.
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
        // SAFETY: the accessor contract: `obj` is a live `PendingOperation` its caller pins, and `SCHED` is held.
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
        // SAFETY: the accessor contract: `obj` is a live `PendingOperation` its caller pins, and `SCHED` is held.
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

    fn fresh() -> *mut () {
        init_global_heap();
        KBox::into_raw(PendingOperation::try_new().unwrap()).as_ptr() as *mut ()
    }

    fn free(obj: *mut ()) {
        // SAFETY: `obj` carries the single creation reference and has no waiters.
        unsafe {
            drop(KBox::<PendingOperation>::from_raw(core::ptr::NonNull::new_unchecked(obj as *mut PendingOperation)));
        }
    }

    #[test]
    fn try_new_is_unsignalled_with_magic() {
        init_global_heap();
        let p = PendingOperation::try_new().unwrap();
        assert!(p.magic_ok());
        let obj = KBox::into_raw(p).as_ptr() as *mut ();
        // SAFETY: live PO, single-threaded test (stands in for SCHED).
        unsafe {
            assert!(!PendingOperation::already_signaled(obj));
            assert_eq!(PendingOperation::status(obj), 0);
        }
        free(obj);
    }

    #[test]
    fn signal_is_one_shot_and_records_status() {
        let obj = fresh();
        // SAFETY: live PO, single-threaded test.
        unsafe {
            assert!(PendingOperation::signal_with_result(obj, 0, 0));
            assert!(PendingOperation::already_signaled(obj));
            assert_eq!(PendingOperation::status(obj), 0);
            assert!(!PendingOperation::signal_with_result(obj, -12, 0));
            assert_eq!(PendingOperation::status(obj), 0, "status must not change");
        }
        free(obj);
    }

    #[test]
    fn signal_carries_a_negative_status() {
        let obj = fresh();
        // SAFETY: live PO, single-threaded test.
        unsafe {
            assert!(PendingOperation::signal_with_result(obj, -12, 0)); // e.g. TimedOut
            assert!(PendingOperation::already_signaled(obj));
            assert_eq!(PendingOperation::status(obj), -12);
            assert_eq!(PendingOperation::result(obj), 0);
        }
        free(obj);
    }

    #[test]
    fn signal_with_result_records_status_and_payload() {
        let obj = fresh();
        // SAFETY: live PO, single-threaded test.
        unsafe {
            assert_eq!(PendingOperation::result(obj), 0, "zero before signal");
            assert!(PendingOperation::signal_with_result(obj, 0, 0xCAFE));
            assert!(PendingOperation::already_signaled(obj));
            assert_eq!(PendingOperation::status(obj), 0);
            assert_eq!(PendingOperation::result(obj), 0xCAFE);
            assert!(!PendingOperation::signal_with_result(obj, -10, 0xDEAD));
            assert_eq!(PendingOperation::status(obj), 0, "status unchanged");
            assert_eq!(PendingOperation::result(obj), 0xCAFE, "result unchanged");
        }
        free(obj);
    }

    #[test]
    fn waiters_add_remove_take_caps_at_max() {
        let obj = fresh();
        // SAFETY: live PO, single-threaded test.
        unsafe {
            let ths: [*mut (); PendingOperation::MAX_WAITERS] = core::array::from_fn(|i| (0x1000 + i) as *mut ());
            for &t in &ths {
                assert!(PendingOperation::add_waiter(obj, t).is_ok());
            }
            assert!(PendingOperation::add_waiter(obj, 0xDEAD as *mut ()).is_err());
            PendingOperation::remove_waiter(obj, ths[1]);
            assert!(PendingOperation::add_waiter(obj, 0xBEEF as *mut ()).is_ok());
            let mut buf = [core::ptr::null_mut(); PendingOperation::MAX_WAITERS];
            assert_eq!(PendingOperation::take_waiters(obj, &mut buf), PendingOperation::MAX_WAITERS);
            let mut buf2 = [core::ptr::null_mut(); PendingOperation::MAX_WAITERS];
            assert_eq!(PendingOperation::take_waiters(obj, &mut buf2), 0);
        }
        free(obj);
    }

    #[test]
    fn dispatch_destroy_runs_pending_op_arm() {
        init_global_heap();
        test_probe::reset();
        let obj = fresh();
        // SAFETY: `obj` carries the single creation reference.
        let r = unsafe { ObjectRef::from_raw(obj, KObjectType::PendingOperation) };
        assert_eq!(test_probe::pending_op_destroys(), 0);
        drop(r);
        assert_eq!(test_probe::pending_op_destroys(), 1);
    }
}
