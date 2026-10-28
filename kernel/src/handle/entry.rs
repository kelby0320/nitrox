//! [`HandleEntry`] — one slot in a segmented handle table.
//!
//! Layout is normative (`docs/spec/handle-encoding.md` § "Handle table entry
//! layout"): 64 bytes, 64-byte aligned, so a
//! [`Segment`](super::segment::Segment) of [`SEGMENT_LEN`](super::SEGMENT_LEN)
//! entries is exactly 256 KiB (one buddy order-6 block).
//!
//! Every field is an atomic, so lookups need no `unsafe`; the seqlock `seq`
//! gives readers an atomic snapshot of several fields. The single-writer
//! precondition is the rank-3 segment lock (`kernel/docs/lock-ordering.md`).
//!
//! A [`WriteGuard`] toggles `seq` even → odd on construction and back on drop;
//! between, metadata stores are `Relaxed`. [`read_snapshot`] loops until two
//! `Acquire` loads of `seq` bracket the (`Relaxed`) field loads and match on an
//! even value, and carries `seq` so lookup can re-check it (spec step 8) after
//! taking the object refcount.

use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering, fence};

use crate::libkern::handle::{KObjectType, RawHandle, Rights};

/// One slot in a [`Segment`](super::segment::Segment): exactly 64 bytes
/// (checked below).
#[repr(C, align(64))]
pub(crate) struct HandleEntry {
    /// Seqlock counter. Even = stable; odd = a writer is mid-update.
    pub seq: AtomicU32,
    /// Bumped on every (re-)allocation, so a stale `RawHandle` for a recycled slot
    /// fails the generation check instead of aliasing a new object.
    pub generation: AtomicU32,
    /// PID of the owning process — the security-critical check (spec step 10).
    pub owner_pid: AtomicU32,
    /// Capability bits ([`Rights`]) as raw `u64`.
    pub rights: AtomicU64,
    /// [`KObjectType`] discriminant.
    pub object_type: AtomicU32,
    /// Nonzero while a [`Claim`](super::table::Claim) moves the handle; read and written only
    /// under the rank-3 lock, and cleared by the next allocation of the slot.
    pub in_transit: AtomicU32,
    /// Type-erased object pointer: null while the slot is free or in the
    /// deferred-reclaim queue. Read in step 6 as one `Acquire` load.
    pub object: AtomicPtr<()>,
    /// Reserved for an owner's handle list; unused. Release-at-exit is a segment
    /// scan keyed on `owner_pid`, keeping other paths free of list upkeep under the
    /// rank-3 lock (`docs/architecture/handle-system.md` § Releasing a process's
    /// handles at exit).
    pub next_owned: AtomicU64,
    /// Next free slot in the segment freelist ([`u32::MAX`] = tail).
    pub free_next: AtomicU32,
    /// Explicit pad.
    pub _pad2: u32,
}

const _: () = assert!(core::mem::size_of::<HandleEntry>() == 64);
const _: () = assert!(core::mem::align_of::<HandleEntry>() == 64);

/// Sentinel `free_next` value marking the tail of a segment freelist.
pub(crate) const FREE_NEXT_TAIL: u32 = u32::MAX;

impl HandleEntry {
    /// A fresh entry; the owning segment overwrites `free_next` when threading the
    /// freelist.
    pub(crate) const fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
            generation: AtomicU32::new(0),
            owner_pid: AtomicU32::new(0),
            rights: AtomicU64::new(0),
            object_type: AtomicU32::new(KObjectType::Invalid as u32),
            in_transit: AtomicU32::new(0),
            object: AtomicPtr::new(core::ptr::null_mut()),
            next_owned: AtomicU64::new(RawHandle::NULL.bits()),
            free_next: AtomicU32::new(FREE_NEXT_TAIL),
            _pad2: 0,
        }
    }
}

/// The seqlock-guarded fields, read as one atomic tuple; `seq` is re-checked
/// after the refcount is taken (spec step 8).
#[derive(Copy, Clone, Debug)]
pub(crate) struct EntrySnapshot {
    pub seq: u32,
    pub generation: u32,
    pub owner_pid: u32,
    pub rights: Rights,
    pub object_type: u32,
}

/// Read a consistent snapshot of the seqlock-guarded fields.
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
            return EntrySnapshot { seq: s1, generation, owner_pid, rights, object_type };
        }
    }
}

/// RAII proof the holder is in the entry's seqlock writer window: `seq` flips
/// even → odd on construction and back on drop, publishing the writes. Callers
/// must hold the rank-3 segment lock throughout (the single-writer precondition
/// that lets the first load be `Relaxed`); debug builds panic on entering an odd
/// `seq`.
pub(crate) struct WriteGuard<'a> {
    entry: &'a HandleEntry,
    seq_at_enter: u32,
}

impl<'a> WriteGuard<'a> {
    pub(crate) fn new(entry: &'a HandleEntry) -> Self {
        let s = entry.seq.load(Ordering::Relaxed);
        debug_assert_eq!(s & 1, 0, "WriteGuard entered on odd seq — concurrent writers",);
        entry.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        // Orders the odd `seq` before the field stores that follow, which a Release store cannot.
        fence(Ordering::Release);
        Self { entry, seq_at_enter: s }
    }
}

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        self.entry.seq.store(self.seq_at_enter.wrapping_add(2), Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A snapshot is never torn**: the writer's odd `seq` is ordered before its field stores, or a
    /// weakly ordered reader sees new fields between two reads of the old even one. x86 keeps
    /// stores in order, so only Miri's weak-memory emulation can see it.
    #[test]
    fn a_snapshot_is_never_torn() {
        const WRITES: u32 = if cfg!(miri) { 300 } else { 100_000 };
        let e = HandleEntry::new();
        std::thread::scope(|s| {
            s.spawn(|| {
                for i in 1..=WRITES {
                    let _wg = WriteGuard::new(&e);
                    e.generation.store(i, Ordering::Relaxed);
                    e.owner_pid.store(i, Ordering::Relaxed);
                }
            });
            for _ in 0..WRITES {
                let snap = read_snapshot(&e);
                assert_eq!(snap.generation, snap.owner_pid, "a torn snapshot accepted");
            }
        });
    }

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
            assert_eq!(e.seq.load(Ordering::Relaxed) & 1, 1);
            e.generation.store(7, Ordering::Relaxed);
        }
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
            e.object_type.store(KObjectType::IoRing as u32, Ordering::Relaxed);
        }
        let snap = read_snapshot(&e);
        assert_eq!(snap.generation, 42);
        assert_eq!(snap.owner_pid, 99);
        assert_eq!(snap.rights.bits(), (Rights::READ | Rights::WRITE).bits());
        assert_eq!(snap.object_type, KObjectType::IoRing as u32);
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
