//! Per-segment storage and shuffled freelist initialisation.
//!
//! A segment is a fixed-size block of [`SEGMENT_LEN`] [`HandleEntry`]
//! slots — exactly 256 KiB, one buddy order-6 block — pointed at by
//! one entry of the handle table's directory. Per-segment metadata
//! (`free_head`, `free_count`) lives in [`HandleTable::Inner`] rather
//! than inline so the on-heap block is *exactly* the size the spec
//! requires; otherwise the buddy allocator would round 256 KiB + a
//! few u32s up to a 512 KiB block and waste half.
//!
//! ## Shuffled freelist
//!
//! Segment construction performs a Fisher-Yates shuffle of the slot
//! indices `0..SEGMENT_LEN` and threads them through the entries'
//! `free_next` fields, so successive allocations from a fresh segment
//! return slot indices in a pseudo-random order. Combined with the
//! 32-bit generation counter and the owner-PID check, this means the
//! low 20 bits of a freshly-issued handle reveal nothing about other
//! handles in the same segment.
//!
//! The Fisher-Yates pass uses a 16 KiB scratch buffer (one `u32` per
//! slot) which is itself allocated from the buddy via `kmalloc`/
//! `large_alloc`, then freed before this function returns. Sticking
//! the permutation in the entries' `free_next` fields and then
//! converting in-place would be marginally cheaper but fragile —
//! consecutive in-place rewrites would clobber the permutation
//! values they still need to read.

use core::ptr::{self, NonNull};
use core::sync::atomic::Ordering;

use crate::libkern::AllocError;
use crate::mm::slab::{kfree, kmalloc};

use super::SEGMENT_LEN;
use super::entry::{FREE_NEXT_TAIL, HandleEntry};
use super::prng::Xorshift64;

/// Storage for one segment's worth of [`HandleEntry`] slots.
///
/// The directory holds `AtomicPtr<SegmentEntries>` slots; a non-null
/// pointer is a fully-initialised, shuffled-freelist segment that
/// lives until [`HandleTable`](super::table::HandleTable) drops.
pub(crate) type SegmentEntries = [HandleEntry; SEGMENT_LEN];

/// Metadata for one allocated segment, indexed by segment id in
/// [`HandleTable::Inner`]. Updated only under the table's rank-3 lock,
/// so plain `u32`s are sound — readers never touch this struct, they
/// only walk `SegmentEntries`.
#[derive(Copy, Clone)]
pub(crate) struct SegmentMeta {
    /// Head of the segment's freelist (slot index), or [`FREE_NEXT_TAIL`]
    /// when no free slots remain.
    pub free_head: u32,
    /// Number of free slots in the segment.
    pub free_count: u32,
}

impl SegmentMeta {
    /// Initial state for an unallocated segment slot.
    pub(crate) const fn empty() -> Self {
        Self {
            free_head: FREE_NEXT_TAIL,
            free_count: 0,
        }
    }
}

/// Allocate a fresh segment, initialise every entry to its
/// `HandleEntry::new()` defaults, then Fisher-Yates shuffle slot
/// indices `0..SEGMENT_LEN` and thread them through the freelist.
///
/// Returns the pointer to the entries block together with the
/// metadata to install in [`HandleTable::Inner::segment_meta`].
///
/// `kmalloc(256 KiB)` routes to `large_alloc` and through the buddy
/// at order 6 (4 × 64 = 256 KiB). The scratch permutation buffer is
/// 16 KiB and also routes through the buddy at order 2.
///
/// This function does **not** take the rank-3 lock; in fact callers
/// must drop it before allocating. See `kernel/CLAUDE.md` §
/// "Forbidden patterns": no allocations while holding a spinlock.
pub(crate) fn try_alloc_initialised(
    seed: u64,
) -> Result<(NonNull<SegmentEntries>, SegmentMeta), AllocError> {
    // --- Allocate the 256 KiB entries block. ---
    let entries_bytes = core::mem::size_of::<SegmentEntries>();
    let entries_align = core::mem::align_of::<SegmentEntries>();
    let entries_raw = kmalloc(entries_bytes, entries_align) as *mut SegmentEntries;
    let entries_ptr = NonNull::new(entries_raw).ok_or(AllocError)?;

    // SAFETY: `entries_raw` is a freshly-allocated 256 KiB region with
    // alignment matching `SegmentEntries`; it currently holds undefined
    // bytes. Writing a fresh `HandleEntry` into each slot leaves the
    // region fully initialised. No other thread holds a reference to
    // this region — the directory pointer is published only after we
    // return.
    unsafe {
        let entries_first: *mut HandleEntry = entries_raw as *mut HandleEntry;
        for i in 0..SEGMENT_LEN {
            ptr::write(entries_first.add(i), HandleEntry::new());
        }
    }

    // --- Allocate the scratch permutation buffer. ---
    let perm_bytes = SEGMENT_LEN * core::mem::size_of::<u32>();
    let perm_align = core::mem::align_of::<u32>();
    let perm_raw = kmalloc(perm_bytes, perm_align) as *mut u32;
    if perm_raw.is_null() {
        // `entries_raw` came from the matching `kmalloc` above and
        // has no live references — the writes initialising the
        // entries are not borrowed.
        kfree(entries_raw as *mut u8);
        return Err(AllocError);
    }

    // SAFETY: `perm_raw` is freshly allocated, aligned, holds undefined
    // bytes; the writes below initialise it to the identity permutation.
    unsafe {
        for i in 0..SEGMENT_LEN {
            ptr::write(perm_raw.add(i), i as u32);
        }
    }

    // --- Fisher-Yates shuffle in place. ---
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

    // --- Thread the freelist through the entries. ---
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
        entries[tail]
            .free_next
            .store(FREE_NEXT_TAIL, Ordering::Relaxed);
    }

    let free_head = {
        // SAFETY: `perm_raw.add(0)` is in-bounds and initialised.
        unsafe { ptr::read(perm_raw) }
    };

    // `perm_raw` came from the matching `kmalloc` above; we are done
    // with it and hold no live references.
    kfree(perm_raw as *mut u8);

    Ok((
        entries_ptr,
        SegmentMeta {
            free_head,
            free_count: SEGMENT_LEN as u32,
        },
    ))
}

/// Free a segment previously returned by [`try_alloc_initialised`].
///
/// # Safety
///
/// `entries` must be a pointer returned by [`try_alloc_initialised`],
/// not yet freed, with no outstanding references into the entries
/// block. The handle table guarantees this by only freeing segments
/// during its own `Drop` after every directory entry has been cleared.
pub(crate) unsafe fn free_entries(entries: NonNull<SegmentEntries>) {
    // `kfree` is the matching deallocator for `kmalloc`; safety of
    // this call is forwarded from the function contract on
    // `free_entries` itself.
    kfree(entries.as_ptr() as *mut u8);
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
        // Walking the freelist must visit exactly SEGMENT_LEN distinct
        // slot indices.
        let entries_ref = unsafe { entries.as_ref() };
        let mut seen = [false; SEGMENT_LEN];
        let mut idx = meta.free_head;
        let mut count = 0usize;
        while idx != FREE_NEXT_TAIL {
            assert!(
                !seen[idx as usize],
                "freelist revisited slot {idx} — chain has a cycle"
            );
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
        let entries_ref = unsafe { entries.as_ref() };
        // Collect the slot order from the freelist.
        let mut order = [0u32; 16];
        let mut idx = meta.free_head;
        for slot in &mut order {
            assert_ne!(idx, FREE_NEXT_TAIL);
            *slot = idx;
            idx = entries_ref[idx as usize].free_next.load(Ordering::Relaxed);
        }
        // The first 16 slots in a shuffled freelist must not be the
        // sequential 0..16 — that would mean we returned the identity
        // permutation. The xorshift PRNG is deterministic, so this is a
        // stable test, not a flaky probabilistic one.
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
        let r1 = unsafe { e1.as_ref() };
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
