//! Segmented kernel handle table.
//!
//! Handles are 64-bit opaque capabilities (`docs/spec/handle-encoding.md`;
//! overview in `docs/architecture/handle-system.md`). This module owns the
//! lookup-and-allocation machinery: segment directory, seqlock-protected
//! entries, deferred reclamation queue and grace tracker.
//!
//! ## Concurrency
//!
//! The **segment lock** (`SpinLock<Inner>`, rank 3 in
//! [`kernel/docs/lock-ordering.md`](../../docs/lock-ordering.md)) serialises
//! allocation, close, restrict and segment growth. Each entry's **seqlock** lets
//! lookups proceed without it. The lookup hot path takes no lock; the grace
//! tracker uses its own atomics (see [`grace`]).
//!
//! ## ObjectRef seam
//!
//! Step 7 of the validation algorithm bumps the object's refcount:
//! [`try_acquire_refcount`] calls
//! [`KObjectHeader::try_acquire`](crate::object::KObjectHeader::try_acquire)
//! (`Arc`-upgrade semantics: fails if the count was already zero) on the header
//! at offset 0 of the type-erased pointer. On success the reference is adopted
//! into an [`ObjectRef`] at step 12; on the retry and
//! error paths [`release_refcount`] drops it. The test-only
//! [`FAIL_NEXT_ACQUIRE`] forces the step-7 failure branch.

use crate::libkern::handle::KObjectType;
use crate::object::ObjectRef;
use crate::object::header::KObjectHeader;

pub(crate) mod entry;
pub mod global;
pub(crate) mod grace;
pub(crate) mod prng;
pub(crate) mod segment;
pub mod table;
pub(crate) mod type_rights;

pub use table::{
    Claim, ClosedObject, DEFER_RING_CAPACITY, HandleError, HandleStat, HandleTable, LookupOk, SweepCursor,
};

/// Number of top-level directory slots, each a
/// [`segment::SegmentEntries`] or null until grown (spec § "Default capacity").
pub const DIRECTORY_LEN: usize = 256;

/// Number of [`entry::HandleEntry`] slots per segment; with `DIRECTORY_LEN`
/// this caps a table at ~1,048,576 handles (spec § "Default capacity").
pub const SEGMENT_LEN: usize = 4096;

const _: () = assert!(SEGMENT_LEN <= (1 << 20));
const _: () = assert!(DIRECTORY_LEN <= (1 << 12));

#[cfg(test)]
std::thread_local! {
    /// One-shot **per-thread** flag forcing the next [`try_acquire_refcount`] on
    /// this thread to fail; per-thread so another thread's lookup cannot consume it.
    pub(crate) static FAIL_NEXT_ACQUIRE: core::cell::Cell<bool> =
        const { core::cell::Cell::new(false) };
}

/// Step 7 of the validation algorithm: try to bump the object's refcount.
/// Returns `false` if it was already zero (the object is being torn down), and
/// lookup then returns `InvalidHandle`. Under `cfg(test)` [`FAIL_NEXT_ACQUIRE`]
/// forces the failure.
///
/// # Safety
///
/// `obj` addresses a kernel object whose memory is whole for the call: read from a live entry
/// under the grace guard.
pub(crate) unsafe fn try_acquire_refcount(obj: *mut (), _ty: KObjectType) -> bool {
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

/// Release a refcount taken with [`try_acquire_refcount`] by adopting it into a
/// transient [`ObjectRef`] and dropping it, which runs the destructor if it was
/// the last.
///
/// # Safety
///
/// `obj` is a kernel object of type `ty`, and the caller gives up a reference
/// [`try_acquire_refcount`] took on it.
pub(crate) unsafe fn release_refcount(obj: *mut (), ty: KObjectType) {
    // SAFETY: `obj` owns exactly the reference acquired by the matching
    // `try_acquire_refcount` on the lookup retry/error path; adopting it
    // into an `ObjectRef` and dropping it accounts for that one
    // reference exactly once.
    drop(unsafe { ObjectRef::from_raw(obj, ty) });
}

/// The calling context's id for the [`grace::GraceTracker`]: the running CPU's
/// dense id, valid since `MAX_CPUS <= grace::MAX_CTX`. Tests override it with a
/// thread-local counter so each `std::thread` gets a distinct id.
#[cfg(not(test))]
pub(crate) fn current_ctx_id() -> u32 {
    use crate::arch::smp::ArchSmp;
    crate::arch::Smp::current_cpu()
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
