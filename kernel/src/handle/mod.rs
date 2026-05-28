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
//! ## ObjectRef seam (Phase 1, this slice)
//!
//! Step 7 of the spec's validation algorithm calls
//! `ObjectRef::try_acquire` to bump the target kernel object's
//! refcount. `KObjectHeader` lands in the next slice; until then,
//! [`try_acquire_refcount`] and [`release_refcount`] are stubs that
//! unconditionally succeed (a test-only override flag forces failure
//! so the error path is still exercised). When the next slice arrives
//! it rewrites these two free functions to dispatch on
//! [`KObjectType`](crate::libkern::handle::KObjectType) and bump
//! `KObjectHeader::refcount`; the handle table itself never changes.

use crate::libkern::handle::KObjectType;

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

/// In test builds, a one-shot **per-thread** flag the suite can set
/// to force the next [`try_acquire_refcount`] call on the same
/// thread to fail. Lets tests exercise the step-7 failure branch
/// without a real `KObjectHeader`.
///
/// Per-thread (rather than process-global) so that one test setting
/// the flag does not poison concurrent lookups on other threads —
/// cargo runs unit tests in parallel by default, and a global flag
/// would cause cross-test interference (a stress-test thread's
/// lookup would consume a flag the dedicated test set, and vice
/// versa).
#[cfg(test)]
std::thread_local! {
    pub(crate) static FAIL_NEXT_ACQUIRE: core::cell::Cell<bool> =
        const { core::cell::Cell::new(false) };
}

/// Step 7 of the spec's validation algorithm — try to bump the
/// referenced kernel object's refcount. Returns `false` if the
/// refcount was already zero (the object is being torn down) and
/// the lookup should fall through to `InvalidHandle`.
///
/// Phase 1 stub: unconditionally returns `true` (and consults
/// [`FAIL_NEXT_ACQUIRE`] under `cfg(test)`). The next slice rewrites
/// this to dispatch on `_ty` and bump
/// `KObjectHeader::refcount`. The handle table's lookup path is
/// shaped against this signature so the swap is mechanical.
pub(crate) fn try_acquire_refcount(_obj: *mut (), _ty: KObjectType) -> bool {
    #[cfg(test)]
    {
        if FAIL_NEXT_ACQUIRE.with(|f| f.replace(false)) {
            return false;
        }
    }
    true
}

/// Release a refcount previously acquired with [`try_acquire_refcount`].
/// Phase 1 stub: no-op. Rewritten alongside `try_acquire_refcount` in
/// the next slice.
pub(crate) fn release_refcount(_obj: *mut (), _ty: KObjectType) {
    // No-op until KObjectHeader exists.
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
