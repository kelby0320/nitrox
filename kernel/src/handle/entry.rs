//! [`HandleEntry`] — one slot in a segmented handle table.
//!
//! Layout is normative: `docs/spec/handle-encoding.md` § "Handle table
//! entry layout" requires exactly 64 bytes, 64-byte aligned, so each
//! entry occupies one x86_64 cache line and a [`Segment`](super::segment::Segment)
//! of [`SEGMENT_LEN`](super::SEGMENT_LEN) entries is exactly 256 KiB
//! (one buddy order-6 block).
//!
//! Every data field is an atomic of the matching primitive width so
//! lookup reads can proceed without `unsafe` pointer-through-reference
//! writes. The seqlock (`seq`) gives readers an *atomic snapshot of
//! multiple fields*; on its own each `Atomic*` only guarantees per-field
//! consistency. The single-writer precondition (only one writer in the
//! odd-`seq` window) is enforced by the handle table's rank-3 segment
//! lock — see `kernel/docs/lock-ordering.md`.
//!
//! ## Writer protocol
//!
//! A [`WriteGuard`] toggles `seq` even → odd on construction and
//! odd → even on drop. Between, the writer stores to the metadata
//! fields with `Relaxed` (the surrounding Release stores order them).
//! `debug_assert!` catches a torn writer that enters on an odd seq.
//!
//! ## Reader protocol
//!
//! [`read_snapshot`] loops until two `Acquire` loads of `seq` bracket
//! the metadata reads and observe matching even values. Inside the
//! bracket the field loads are `Relaxed` — `seq` does the ordering.
//! The returned snapshot carries the `seq` value observed so the
//! lookup path can perform the spec's step-8 re-check after acquiring
//! the object refcount.

use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering, fence};

use crate::libkern::handle::{KObjectType, RawHandle, Rights};

/// One slot in a [`Segment`](super::segment::Segment).
///
/// `#[repr(C, align(64))]` for the cache-line alignment the spec
/// requires. The two `_padN` fields and the implicit pad after
/// `owner_pid` together make the layout exactly 64 bytes — checked at
/// compile time below.
#[repr(C, align(64))]
pub(crate) struct HandleEntry {
    /// Seqlock counter. Even = stable; odd = a writer is mid-update.
    /// Readers loop until two even reads bracket their field loads.
    pub seq: AtomicU32,
    /// Bumped on every (re-)allocation that uses this slot. Distinct
    /// generations let an old `RawHandle` for a recycled slot fail the
    /// step-9 generation check rather than aliasing a different object.
    pub generation: AtomicU32,
    /// PID of the process that owns this handle. Spec § "Validation
    /// algorithm" step 10 — the security-critical check.
    pub owner_pid: AtomicU32,
    // Implicit 4-byte pad here so `rights` (a u64) is 8-byte aligned.
    /// Capability bits ([`Rights`]) carried by the handle. Stored as
    /// raw bits in an `AtomicU64` so loads/stores are lock-free; the
    /// snapshot path wraps in [`Rights::from_bits_truncate`].
    pub rights: AtomicU64,
    /// [`KObjectType`] discriminant. Stored as `u32` for atomic access;
    /// converted back via [`KObjectType::from_u32`] on the snapshot path.
    pub object_type: AtomicU32,
    /// Explicit pad (spec § "Handle table entry layout").
    pub _pad1: u32,
    /// Type-erased pointer to the kernel object. `null` while the slot
    /// is free or in the deferred-reclaim queue; non-null while live.
    /// Read in step 6 of the validation algorithm as a single
    /// `Acquire` load outside the seqlock loop.
    pub object: AtomicPtr<()>,
    /// Intrusive list pointer threading this entry onto its owning
    /// process's owned-handles list. `RawHandle::NULL` at the tail
    /// (and throughout this slice — the threading lands with the
    /// `Process` slice; see `docs/architecture/handle-system.md`).
    pub next_owned: AtomicU64,
    /// Index of the next free slot within the same segment when this
    /// slot is on the segment freelist. Meaningless when the slot is
    /// live. [`u32::MAX`] marks the freelist tail.
    pub free_next: AtomicU32,
    /// Explicit pad (spec § "Handle table entry layout").
    pub _pad2: u32,
}

// Spec § "Handle table entry layout" demands exactly 64 bytes,
// 64-byte aligned — required for the segment-as-one-cache-line-per-
// entry guarantee and for `Segment::entries` to be 256 KiB total.
const _: () = assert!(core::mem::size_of::<HandleEntry>() == 64);
const _: () = assert!(core::mem::align_of::<HandleEntry>() == 64);

/// Sentinel `free_next` value marking the tail of a segment freelist.
pub(crate) const FREE_NEXT_TAIL: u32 = u32::MAX;

impl HandleEntry {
    /// Construct a freshly-initialised entry: `seq = 0` (even, no writer),
    /// `generation = 0`, object null, `free_next` pointing nowhere yet.
    /// The owning segment overwrites `free_next` while threading the
    /// freelist.
    pub(crate) const fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
            generation: AtomicU32::new(0),
            owner_pid: AtomicU32::new(0),
            rights: AtomicU64::new(0),
            object_type: AtomicU32::new(KObjectType::Invalid as u32),
            _pad1: 0,
            object: AtomicPtr::new(core::ptr::null_mut()),
            next_owned: AtomicU64::new(RawHandle::NULL.bits()),
            free_next: AtomicU32::new(FREE_NEXT_TAIL),
            _pad2: 0,
        }
    }
}

/// Snapshot of the metadata fields guarded by the entry's seqlock,
/// taken as a single atomic tuple. The included `seq` is the value
/// observed at the bracketing reads; the lookup path re-checks it
/// after acquiring the object refcount (spec § "Validation algorithm"
/// step 8).
#[derive(Copy, Clone, Debug)]
pub(crate) struct EntrySnapshot {
    pub seq: u32,
    pub generation: u32,
    pub owner_pid: u32,
    pub rights: Rights,
    pub object_type: u32,
}

/// Read a consistent snapshot of the entry's seqlock-guarded fields.
///
/// Spins on `seq` until two `Acquire` loads bracket the metadata reads
/// and observe matching even values. Returns the snapshot together
/// with the observed `seq` so the caller can re-check it after object
/// refcount acquisition.
pub(crate) fn read_snapshot(entry: &HandleEntry) -> EntrySnapshot {
    loop {
        let s1 = entry.seq.load(Ordering::Acquire);
        if s1 & 1 != 0 {
            core::hint::spin_loop();
            continue;
        }
        let generation = entry.generation.load(Ordering::Relaxed);
        let owner_pid = entry.owner_pid.load(Ordering::Relaxed);
        let rights = Rights::from_bits_truncate(entry.rights.load(Ordering::Relaxed));
        let object_type = entry.object_type.load(Ordering::Relaxed);
        fence(Ordering::Acquire);
        let s2 = entry.seq.load(Ordering::Acquire);
        if s1 == s2 {
            return EntrySnapshot {
                seq: s1,
                generation,
                owner_pid,
                rights,
                object_type,
            };
        }
        // A writer landed during the read; retry.
    }
}

/// RAII handle proving the holder is inside the entry's seqlock writer
/// window. Construction flips `seq` even → odd; `Drop` flips odd →
/// even, publishing the writes.
///
/// The single-writer precondition that lets us use a `Relaxed` initial
/// load is enforced externally: every caller must hold the handle
/// table's rank-3 segment lock for the duration of the guard. In
/// debug builds, entering on an odd `seq` is a panic — a tripwire for
/// a concurrent-writer bug.
pub(crate) struct WriteGuard<'a> {
    entry: &'a HandleEntry,
    seq_at_enter: u32,
}

impl<'a> WriteGuard<'a> {
    pub(crate) fn new(entry: &'a HandleEntry) -> Self {
        let s = entry.seq.load(Ordering::Relaxed);
        debug_assert_eq!(
            s & 1,
            0,
            "WriteGuard entered on odd seq — concurrent writers",
        );
        entry.seq.store(s.wrapping_add(1), Ordering::Release);
        Self {
            entry,
            seq_at_enter: s,
        }
    }
}

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        self.entry
            .seq
            .store(self.seq_at_enter.wrapping_add(2), Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_exactly_64_bytes_aligned_64() {
        assert_eq!(core::mem::size_of::<HandleEntry>(), 64);
        assert_eq!(core::mem::align_of::<HandleEntry>(), 64);
    }

    #[test]
    fn new_entry_starts_clean() {
        let e = HandleEntry::new();
        assert_eq!(e.seq.load(Ordering::Relaxed), 0);
        assert_eq!(e.generation.load(Ordering::Relaxed), 0);
        assert_eq!(e.owner_pid.load(Ordering::Relaxed), 0);
        assert_eq!(e.rights.load(Ordering::Relaxed), 0);
        assert_eq!(e.object_type.load(Ordering::Relaxed), KObjectType::Invalid as u32);
        assert!(e.object.load(Ordering::Relaxed).is_null());
        assert_eq!(e.next_owned.load(Ordering::Relaxed), RawHandle::NULL.bits());
        assert_eq!(e.free_next.load(Ordering::Relaxed), FREE_NEXT_TAIL);
    }

    #[test]
    fn write_guard_brackets_seq_with_odd_then_even() {
        let e = HandleEntry::new();
        {
            let _g = WriteGuard::new(&e);
            // Inside the guard, seq is odd.
            assert_eq!(e.seq.load(Ordering::Relaxed) & 1, 1);
            e.generation.store(7, Ordering::Relaxed);
        }
        // After drop, seq is even and exactly 2 greater than the start.
        assert_eq!(e.seq.load(Ordering::Relaxed), 2);
        assert_eq!(e.generation.load(Ordering::Relaxed), 7);
    }

    #[test]
    fn read_snapshot_observes_writes_after_guard_drops() {
        let e = HandleEntry::new();
        {
            let _g = WriteGuard::new(&e);
            e.generation.store(42, Ordering::Relaxed);
            e.owner_pid.store(99, Ordering::Relaxed);
            e.rights.store((Rights::READ | Rights::WRITE).bits(), Ordering::Relaxed);
            e.object_type
                .store(KObjectType::IoRing as u32, Ordering::Relaxed);
        }
        let snap = read_snapshot(&e);
        assert_eq!(snap.generation, 42);
        assert_eq!(snap.owner_pid, 99);
        assert_eq!(snap.rights.bits(), (Rights::READ | Rights::WRITE).bits());
        assert_eq!(snap.object_type, KObjectType::IoRing as u32);
        // seq returned matches the field
        assert_eq!(snap.seq, e.seq.load(Ordering::Relaxed));
        assert_eq!(snap.seq & 1, 0);
    }

    #[test]
    fn back_to_back_writes_increment_seq_by_two_each() {
        let e = HandleEntry::new();
        {
            let _g = WriteGuard::new(&e);
            e.generation.store(1, Ordering::Relaxed);
        }
        assert_eq!(e.seq.load(Ordering::Relaxed), 2);
        {
            let _g = WriteGuard::new(&e);
            e.generation.store(2, Ordering::Relaxed);
        }
        assert_eq!(e.seq.load(Ordering::Relaxed), 4);
        {
            let _g = WriteGuard::new(&e);
            e.generation.store(3, Ordering::Relaxed);
        }
        assert_eq!(e.seq.load(Ordering::Relaxed), 6);
    }
}
