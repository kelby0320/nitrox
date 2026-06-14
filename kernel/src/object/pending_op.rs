//! The [`PendingOperation`] kernel object — a one-shot completion waitable.
//!
//! A `PendingOperation` (PO) represents a single in-flight asynchronous
//! operation. Per the async-first rule (`docs/rationale/why-async-syscalls.md`),
//! a potentially-blocking syscall returns a PO handle immediately rather than
//! parking inside the syscall; the caller blocks on it via `sys_wait`. When the
//! operation completes — a DPC signalling an I/O completion, or (this slice) a
//! blocking IPC send whose message is finally delivered — the PO is **signalled**
//! with a completion `status`, waking every thread blocked on it. The status is
//! reported back through the `IoResult` that `sys_wait` writes.
//!
//! A PO is **one-shot**: it transitions `pending → signalled` exactly once and
//! stays signalled (a second [`signal`](PendingOperation::signal) is a no-op).
//! Once signalled, a `sys_wait` returns immediately via the fast-path check.
//!
//! ## Mutation discipline
//!
//! Identical to [`Timer`](crate::object::Timer): the PO is shared through an
//! [`ObjectRef`](crate::object::ObjectRef) yet its `signaled`/`status`/waiter set
//! are mutated by the scheduler. That is sound only because **all** interior
//! state lives in an [`UnsafeCell`] touched **exclusively while the rank-1
//! `SCHED` lock is held** (single-CPU serialisation; see
//! `kernel/docs/lock-ordering.md`). The `pub(crate) unsafe fn` accessors take a
//! type-erased `*mut ()` and reach the interior through that cell, forming no
//! aliasing `&mut PendingOperation`.

use core::cell::UnsafeCell;

use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox, KVec};
use crate::object::header::KObjectHeader;

/// A pending-operation kernel object.
///
/// `#[repr(C)]` with [`KObjectHeader`] first — see [`crate::object::header`].
#[repr(C)]
pub struct PendingOperation {
    header: KObjectHeader,
    /// Self-check sentinel; a live object always reads [`PendingOperation::MAGIC`].
    magic: u64,
    /// All mutable state, reached only under `SCHED` (see "Mutation discipline").
    inner: UnsafeCell<PendingOpInner>,
}

/// A `PendingOperation`'s scheduler-owned interior.
struct PendingOpInner {
    /// `true` once the operation has completed; one-shot (never cleared).
    signaled: bool,
    /// The completion status, valid once `signaled`. `0` = success; a negative
    /// value is a [`KError`](crate::libkern::KError) discriminant (e.g. the
    /// `TimedOut` / `PeerClosed` outcomes of a blocking IPC send). Surfaced to
    /// userspace as the `status` of the `IoResult` `sys_wait` writes.
    status: i32,
    /// The completion **result payload**, valid once `signaled` and meaningful
    /// when `status == 0` for operations that return a value rather than just a
    /// status — a namespace lookup delivers its **resolved handle** here.
    /// Surfaced as the `result` of the `IoResult` `sys_wait` writes; `0` for
    /// status-only completions (a blocking IPC send) and edge-style waitables.
    result: u64,
    /// Threads blocked on this PO, as type-erased `Thread` object pointers
    /// (non-owning — each waiter is kept alive by its own parked `ObjectRef` in
    /// the scheduler `blocked` list, and is removed from here before it unparks).
    /// Pre-reserved at [`PendingOperation::try_new`] to
    /// [`MAX_WAITERS`](PendingOperation::MAX_WAITERS); `add_waiter` never grows
    /// it under the lock.
    waiters: KVec<*mut ()>,
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

    /// Maximum simultaneous waiters on one PO. Bounds the pre-reserved waiter
    /// vector so `add_waiter` never allocates under `SCHED`; a PO almost always
    /// has exactly one waiter (the thread that submitted the operation).
    pub const MAX_WAITERS: usize = 4;

    /// Allocate an unsignalled `PendingOperation` with a refcount of one. The
    /// waiter vector is reserved up front (the only fallible growth) so later
    /// `add_waiter`s stay within capacity and never allocate under `SCHED`.
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
            }),
        })
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    // --- Scheduler-only accessors --------------------------------------
    //
    // SAFETY (shared by all): `obj` addresses a live `PendingOperation` (pinned
    // by an `ObjectRef` the caller holds), and the caller holds the `SCHED`
    // lock, which — single-CPU — serialises all access to `inner`.

    /// Borrow the interior mutably. Must not outlive the `SCHED` hold nor alias.
    ///
    /// # Safety
    /// See the accessor contract above.
    #[allow(clippy::mut_from_ref)]
    unsafe fn inner<'a>(obj: *mut ()) -> &'a mut PendingOpInner {
        // SAFETY: `obj` is a live PO; forming a shared `&PendingOperation` to
        // reach the `UnsafeCell`, then a `&mut PendingOpInner` through it, is the
        // interior-mutability contract — sound while `SCHED` serialises access.
        let p = unsafe { &*(obj as *const PendingOperation) };
        unsafe { &mut *p.inner.get() }
    }

    /// Mark the operation complete with `status` **and** a `result` payload,
    /// **one-shot**: a PO already signalled is left untouched (the first
    /// completion wins). Returns `true` iff this call performed the transition
    /// (so the caller knows to wake waiters). Pairs with the scheduler's
    /// `signal_pending_op_with_result`. A namespace lookup uses this to deliver
    /// its resolved handle in `result`.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn signal_with_result(obj: *mut (), status: i32, result: u64) -> bool {
        let inner = unsafe { Self::inner(obj) };
        if inner.signaled {
            return false;
        }
        inner.status = status;
        inner.result = result;
        inner.signaled = true;
        true
    }

    /// The completion status (meaningful once signalled; `0` before). Stable
    /// after the one-shot transition, so it may be read at `sys_wait`
    /// result-build time.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn status(obj: *mut ()) -> i32 {
        unsafe { Self::inner(obj) }.status
    }

    /// The completion result payload (meaningful once signalled with `status ==
    /// 0`; `0` before). Stable after the one-shot transition, so it may be read
    /// at `sys_wait` result-build time alongside [`status`](Self::status).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn result(obj: *mut ()) -> u64 {
        unsafe { Self::inner(obj) }.result
    }

    /// `true` iff the operation has completed — the waitable "signaled"
    /// predicate (the `sys_wait` fast-path check).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn already_signaled(obj: *mut ()) -> bool {
        unsafe { Self::inner(obj) }.signaled
    }

    /// Register `thread` as a waiter. `Err(())` if already at
    /// [`MAX_WAITERS`](Self::MAX_WAITERS) (caller maps to `OutOfMemory`); never
    /// grows the vector under the lock.
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

impl Drop for PendingOperation {
    /// A live waiter pins this PO (each holds an `ObjectRef` on it across
    /// `sys_wait`), so the last reference cannot drop while waiters remain — the
    /// assert documents that invariant. The `KVec` storage frees with the `KBox`.
    fn drop(&mut self) {
        debug_assert!(
            self.inner.get_mut().waiters.is_empty(),
            "PendingOperation dropped with live waiters"
        );
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
            drop(KBox::<PendingOperation>::from_raw(
                core::ptr::NonNull::new_unchecked(obj as *mut PendingOperation),
            ));
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
            // A second signal is a no-op: the first completion wins.
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
            // A status-only completion leaves the payload zero.
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
            // A successful lookup-style completion: status 0, resolved handle.
            assert!(PendingOperation::signal_with_result(obj, 0, 0xCAFE));
            assert!(PendingOperation::already_signaled(obj));
            assert_eq!(PendingOperation::status(obj), 0);
            assert_eq!(PendingOperation::result(obj), 0xCAFE);
            // One-shot: a second completion (even with a payload) is a no-op.
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
            let ths: [*mut (); PendingOperation::MAX_WAITERS] =
                core::array::from_fn(|i| (0x1000 + i) as *mut ());
            for &t in &ths {
                assert!(PendingOperation::add_waiter(obj, t).is_ok());
            }
            assert!(PendingOperation::add_waiter(obj, 0xDEAD as *mut ()).is_err());
            PendingOperation::remove_waiter(obj, ths[1]);
            assert!(PendingOperation::add_waiter(obj, 0xBEEF as *mut ()).is_ok());
            let mut buf = [core::ptr::null_mut(); PendingOperation::MAX_WAITERS];
            assert_eq!(
                PendingOperation::take_waiters(obj, &mut buf),
                PendingOperation::MAX_WAITERS
            );
            // Empty after drain (so Drop's assert holds).
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
