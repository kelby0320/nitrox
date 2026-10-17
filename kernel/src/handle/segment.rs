//! Per-segment storage and shuffled freelist initialisation.
//!
//! A segment is [`SEGMENT_LEN`] [`HandleEntry`] slots, exactly 256 KiB (one
//! buddy order-6 block), pointed at by one directory entry; its metadata lives
//! in [`HandleTable::Inner`] since 256 KiB plus a few `u32`s would round up to a
//! 512 KiB block.
//!
//! Construction Fisher-Yates shuffles the slot indices through the entries'
//! `free_next`, so a fresh segment hands out slots in pseudo-random order: with
//! the generation counter and owner-PID check, the low 20 bits of a fresh handle
//! reveal nothing about its neighbours. The shuffle uses a 16 KiB scratch buffer
//! from `kmalloc`, freed before returning; permuting in place would clobber
//! values still to be read.

use core::ptr::{self, NonNull};
use core::sync::atomic::Ordering;

use crate::libkern::AllocError;
use crate::mm::slab::{kfree, kmalloc};

use super::SEGMENT_LEN;
use super::entry::{FREE_NEXT_TAIL, HandleEntry};
use super::prng::Xorshift64;

/// Storage for one segment's [`HandleEntry`] slots. A non-null directory pointer
/// is a fully-initialised segment that lives until the table drops.
pub(crate) type SegmentEntries = [HandleEntry; SEGMENT_LEN];

/// Metadata for one allocated segment, indexed by segment id in
/// [`HandleTable::Inner`]. Updated only under the rank-3 lock; readers walk
/// `SegmentEntries` and never touch it.
#[derive(Copy, Clone)]
pub(crate) struct SegmentMeta {
    /// Head of the freelist (slot index), or [`FREE_NEXT_TAIL`] if none are free.
    pub free_head: u32,
    /// Number of free slots in the segment.
    pub free_count: u32,
}

impl SegmentMeta {
    /// Initial state for an unallocated segment slot.
    pub(crate) const fn empty() -> Self {
        Self { free_head: FREE_NEXT_TAIL, free_count: 0 }
    }
}

/// Allocate a segment, initialise every entry, Fisher-Yates shuffle the slot
/// indices and thread them through the freelist; returns the entries block and
/// the metadata to install in [`HandleTable::Inner::segment_meta`]. The block is
/// a 256 KiB buddy order-6 allocation, the scratch buffer order 2.
///
/// Does **not** take the rank-3 lock: callers must have dropped it, since
/// nothing may allocate under a spinlock (`kernel/CLAUDE.md` § "Forbidden
/// patterns").
pub(crate) fn try_alloc_initialised(seed: u64) -> Result<(NonNull<SegmentEntries>, SegmentMeta), AllocError> {
    let entries_bytes = core::mem::size_of::<SegmentEntries>();
    let entries_align = core::mem::align_of::<SegmentEntries>();
    let entries_raw = kmalloc(entries_bytes, entries_align) as *mut SegmentEntries;
    let entries_ptr = NonNull::new(entries_raw).ok_or(AllocError)?;

    // SAFETY: `entries_raw` is a fresh 256 KiB region aligned for
    // `SegmentEntries`, holding undefined bytes; writing a `HandleEntry` into
    // each slot initialises it fully. No other thread holds a reference: the
    // directory pointer is published only after we return.
    unsafe {
        let entries_first: *mut HandleEntry = entries_raw as *mut HandleEntry;
        for i in 0..SEGMENT_LEN {
            ptr::write(entries_first.add(i), HandleEntry::new());
        }
    }

    let perm_bytes = SEGMENT_LEN * core::mem::size_of::<u32>();
    let perm_align = core::mem::align_of::<u32>();
    let perm_raw = kmalloc(perm_bytes, perm_align) as *mut u32;
    if perm_raw.is_null() {
        // SAFETY: `entries_raw` came from the matching `kmalloc` above and
        // has no live references — the writes initialising the
        // entries are not borrowed.
        unsafe { kfree(entries_raw as *mut u8) };
        return Err(AllocError);
    }

    // SAFETY: `perm_raw` is freshly allocated, aligned, holds undefined
    // bytes; the writes below initialise it to the identity permutation.
    unsafe {
        for i in 0..SEGMENT_LEN {
            ptr::write(perm_raw.add(i), i as u32);
        }
    }

    let mut prng = Xorshift64::new(seed);
    for i in (1..SEGMENT_LEN).rev() {
        let j = prng.gen_below((i as u32) + 1) as usize;
        // SAFETY: `i` and `j` are both `< SEGMENT_LEN`, so the
        // pointer adds are within the allocation and the reads/writes
        // touch initialised storage.
        unsafe {
            let tmp = ptr::read(perm_raw.add(i));
            ptr::write(perm_raw.add(i), ptr::read(perm_raw.add(j)));
            ptr::write(perm_raw.add(j), tmp);
        }
    }

    // SAFETY: each `perm_raw.add(k)` is in-bounds and initialised; the
    // entries pointer was just fully populated above.
    unsafe {
        let perm = core::slice::from_raw_parts(perm_raw, SEGMENT_LEN);
        let entries = entries_ptr.as_ref();
        for k in 0..SEGMENT_LEN - 1 {
            let cur = perm[k] as usize;
            let next = perm[k + 1];
            entries[cur].free_next.store(next, Ordering::Relaxed);
        }
        let tail = perm[SEGMENT_LEN - 1] as usize;
        entries[tail].free_next.store(FREE_NEXT_TAIL, Ordering::Relaxed);
    }

    let free_head = {
        // SAFETY: `perm_raw.add(0)` is in-bounds and initialised.
        unsafe { ptr::read(perm_raw) }
    };

    // SAFETY: `perm_raw` came from the matching `kmalloc` above; we are done
    // with it and hold no live references.
    unsafe { kfree(perm_raw as *mut u8) };

    Ok((entries_ptr, SegmentMeta { free_head, free_count: SEGMENT_LEN as u32 }))
}

/// Free a segment previously returned by [`try_alloc_initialised`].
///
/// # Safety
///
/// `entries` is a pointer from [`try_alloc_initialised`], not yet freed, with
/// no outstanding references into the block. The table frees segments only in
/// its own `Drop`, after every directory entry is cleared.
pub(crate) unsafe fn free_entries(entries: NonNull<SegmentEntries>) {
    // SAFETY: `kfree` is the matching deallocator for `kmalloc`; safety of
    // this call is forwarded from the function contract on
    // `free_entries` itself.
    unsafe { kfree(entries.as_ptr() as *mut u8) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;

    #[test]
    fn segment_entries_block_is_256_kib() {
        assert_eq!(core::mem::size_of::<SegmentEntries>(), 256 * 1024);
        assert_eq!(core::mem::align_of::<SegmentEntries>(), 64);
    }

    #[test]
    fn try_alloc_initialised_yields_full_freelist() {
        init_global_heap();
        let (entries, meta) = try_alloc_initialised(0xCAFE_BABE).unwrap();
        assert_eq!(meta.free_count, SEGMENT_LEN as u32);
        // SAFETY: `try_alloc_initialised` returned an initialised segment this test owns.
        let entries_ref = unsafe { entries.as_ref() };
        let mut seen = [false; SEGMENT_LEN];
        let mut idx = meta.free_head;
        let mut count = 0usize;
        while idx != FREE_NEXT_TAIL {
            assert!(!seen[idx as usize], "freelist revisited slot {idx} — chain has a cycle");
            seen[idx as usize] = true;
            count += 1;
            assert!(count <= SEGMENT_LEN, "freelist longer than the segment");
            idx = entries_ref[idx as usize].free_next.load(Ordering::Relaxed);
        }
        assert_eq!(count, SEGMENT_LEN);
        assert!(seen.iter().all(|&v| v), "freelist missed at least one slot");

        // SAFETY: we just allocated this segment and have not exposed
        // the pointer anywhere else.
        unsafe { free_entries(entries) };
    }

    #[test]
    fn freelist_is_actually_shuffled() {
        init_global_heap();
        let (entries, meta) = try_alloc_initialised(0x9E37_79B9_7F4A_7C15).unwrap();
        // SAFETY: `try_alloc_initialised` returned an initialised segment this test owns.
        let entries_ref = unsafe { entries.as_ref() };
        let mut order = [0u32; 16];
        let mut idx = meta.free_head;
        for slot in &mut order {
            assert_ne!(idx, FREE_NEXT_TAIL);
            *slot = idx;
            idx = entries_ref[idx as usize].free_next.load(Ordering::Relaxed);
        }
        // The PRNG is deterministic: an identity prefix means the shuffle did
        // nothing.
        let identity: [u32; 16] = core::array::from_fn(|i| i as u32);
        assert_ne!(order, identity, "freelist matches identity — Fisher-Yates failed");

        // SAFETY: as previous test.
        unsafe { free_entries(entries) };
    }

    #[test]
    fn same_seed_yields_same_freelist_order() {
        init_global_heap();
        let (e1, m1) = try_alloc_initialised(0x1234_5678).unwrap();
        let (e2, m2) = try_alloc_initialised(0x1234_5678).unwrap();
        // SAFETY: `try_alloc_initialised` returned an initialised segment this test owns.
        let r1 = unsafe { e1.as_ref() };
        // SAFETY: as above.
        let r2 = unsafe { e2.as_ref() };
        assert_eq!(m1.free_head, m2.free_head);
        let mut idx1 = m1.free_head;
        let mut idx2 = m2.free_head;
        for _ in 0..64 {
            assert_eq!(idx1, idx2);
            if idx1 == FREE_NEXT_TAIL {
                break;
            }
            idx1 = r1[idx1 as usize].free_next.load(Ordering::Relaxed);
            idx2 = r2[idx2 as usize].free_next.load(Ordering::Relaxed);
        }
        // SAFETY: as previous tests.
        unsafe {
            free_entries(e1);
            free_entries(e2);
        }
    }
}
