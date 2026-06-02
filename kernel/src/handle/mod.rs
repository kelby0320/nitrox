//! Segmented kernel handle table.
//!
//! Handles are 64-bit opaque capabilities — see
//! `docs/spec/handle-encoding.md` for the wire format and
//! `docs/architecture/handle-system.md` for the implementation
//! overview. This module owns the lookup-and-allocation machinery
//! (the directory of segments, the seqlock-protected entries, the
//! deferred reclamation queue, the RCU-style grace tracker). The
//! kernel-object substrate that the entries *point at* lives in a
//! separate module — kernel objects are dispatched on
//! [`KObjectType`](crate::libkern::handle::KObjectType) and arrive in
//! the slice that follows this one.
//!
//! ## Concurrency
//!
//! Two layers of synchronisation, by design:
//!
//! - The handle-table **segment lock** (`SpinLock<Inner>`, rank 3 in
//!   [`kernel/docs/lock-ordering.md`](../../docs/lock-ordering.md))
//!   serialises allocation, close, restrict, and segment-grow
//!   bookkeeping.
//! - Each entry's **seqlock** allows lookups to proceed without
//!   acquiring the rank-3 lock in the common case. Readers loop until
//!   two `Acquire` loads bracket a metadata snapshot with matching
//!   even seq values.
//!
//! The lookup hot path takes the segment lock only when entering a
//! read-side critical section to update the grace tracker — actually
//! the lock is *not* taken; the grace tracker uses its own atomics.
//! See [`grace`].
//!
//! ## ObjectRef seam
//!
//! Step 7 of the spec's validation algorithm bumps the target kernel
//! object's refcount; [`try_acquire_refcount`] reads the
//! [`KObjectHeader`](crate::object::KObjectHeader) at offset 0 of the
//! type-erased object pointer and calls
//! [`KObjectHeader::try_acquire`](crate::object::KObjectHeader::try_acquire)
//! (`Arc`-upgrade semantics — fails if the count was already zero). On
//! the lookup success path the bumped reference is adopted into an
//! [`ObjectRef`](crate::object::ObjectRef) at step 12; on the retry and
//! error paths [`release_refcount`] drops it (running the object's
//! destructor if it was the last). A test-only override flag
//! ([`FAIL_NEXT_ACQUIRE`]) forces the step-7 failure branch
//! deterministically, since racing a real count-to-zero is not
//! reproducible. The handle-table body is unchanged from the stub era:
//! the two free functions kept the same signatures.

use crate::libkern::handle::KObjectType;
use crate::object::ObjectRef;
use crate::object::header::KObjectHeader;

pub(crate) mod entry;
pub(crate) mod grace;
pub(crate) mod prng;
pub(crate) mod segment;
pub mod table;
pub(crate) mod type_rights;

pub use table::{
    ClosedObject, DEFER_RING_CAPACITY, HandleError, HandleStat, HandleTable, LookupOk,
};

/// Number of top-level directory slots.
///
/// Each slot points at a [`segment::SegmentEntries`] when allocated,
/// or null when not yet grown. Per `docs/spec/handle-encoding.md` §
/// "Default capacity".
pub const DIRECTORY_LEN: usize = 256;

/// Number of [`entry::HandleEntry`] slots per segment.
///
/// Combined with `DIRECTORY_LEN` this caps a table at ~1,048,576
/// handles. Per `docs/spec/handle-encoding.md` § "Default capacity".
pub const SEGMENT_LEN: usize = 4096;

const _: () = assert!(SEGMENT_LEN <= (1 << 20));
const _: () = assert!(DIRECTORY_LEN <= (1 << 12));

// --- ObjectRef seam (Phase 1 stub) ----------------------------------

#[cfg(test)]
std::thread_local! {
    /// One-shot **per-thread** flag the suite can set to force the next
    /// [`try_acquire_refcount`] call on the same thread to fail, so tests
    /// can exercise the step-7 failure branch deterministically (racing a
    /// real refcount-to-zero is not reproducible).
    ///
    /// Per-thread (rather than process-global) so that one test setting
    /// the flag does not poison concurrent lookups on other threads —
    /// cargo runs unit tests in parallel by default, and a global flag
    /// would cause cross-test interference (a stress-test thread's lookup
    /// would consume a flag the dedicated test set, and vice versa).
    pub(crate) static FAIL_NEXT_ACQUIRE: core::cell::Cell<bool> =
        const { core::cell::Cell::new(false) };
}

/// Step 7 of the spec's validation algorithm — try to bump the
/// referenced kernel object's refcount. Returns `false` if the
/// refcount was already zero (the object is being torn down) and
/// the lookup should fall through to `InvalidHandle`.
///
/// Reads the [`KObjectHeader`] at offset 0 of `obj` and calls
/// [`KObjectHeader::try_acquire`]. Under `cfg(test)` a one-shot
/// [`FAIL_NEXT_ACQUIRE`] flag forces the failure branch.
pub(crate) fn try_acquire_refcount(obj: *mut (), _ty: KObjectType) -> bool {
    #[cfg(test)]
    {
        if FAIL_NEXT_ACQUIRE.with(|f| f.replace(false)) {
            return false;
        }
    }
    // SAFETY: `obj` was observed non-null in a live handle entry under a
    // grace read-guard (lookup step 6 precedes step 7), so it addresses a
    // live kernel object whose first `#[repr(C)]` field is a
    // `KObjectHeader`. We only touch the atomic refcount.
    let header = unsafe { &*(obj as *const KObjectHeader) };
    header.try_acquire()
}

/// Release a refcount previously acquired with [`try_acquire_refcount`].
/// Runs the object's destructor if this was the last reference.
///
/// Implemented by adopting the reference into a transient [`ObjectRef`]
/// and dropping it, which performs the `Release` decrement, the
/// `Acquire` fence, and the type-dispatched destroy in one place.
pub(crate) fn release_refcount(obj: *mut (), ty: KObjectType) {
    // SAFETY: `obj` owns exactly the reference acquired by the matching
    // `try_acquire_refcount` on the lookup retry/error path; adopting it
    // into an `ObjectRef` and dropping it accounts for that one
    // reference exactly once.
    drop(unsafe { ObjectRef::from_raw(obj, ty) });
}

/// Return the calling context's id for the [`grace::GraceTracker`].
///
/// Phase 1 (single CPU, no preemption, no IRQs, no `Process` yet):
/// every operation runs in context 0. SMP will switch this to
/// `arch::cpu_id()`; the Process slice will switch it to
/// `Process::current().ctx_id()`. Tests override via a thread-local
/// counter so each `std::thread` gets a distinct id.
#[cfg(not(test))]
pub(crate) fn current_ctx_id() -> u32 {
    0
}

#[cfg(test)]
pub(crate) fn current_ctx_id() -> u32 {
    use core::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    std::thread_local! {
        static CTX_ID: u32 = NEXT.fetch_add(1, Ordering::Relaxed)
            % (grace::MAX_CTX as u32);
    }
    CTX_ID.with(|&id| id)
}
