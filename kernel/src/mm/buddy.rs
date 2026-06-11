//! Physical-memory buddy allocator.
//!
//! A single `BuddyAllocator` covers every usable RAM region above 1 MiB.
//! Free blocks of order `o` (size `PAGE_SIZE << o`) live on an intrusive
//! singly-linked list whose next pointer is stored in the first 8 bytes of
//! each free frame, accessed through the higher-half direct map. A
//! coalesce bitmap carved out of physical memory at init time tracks
//! buddy-pair occupancy at each order: bit set means exactly one of the
//! pair is free.
//!
//! Locking is the caller's responsibility; the allocator is `!Sync`. Once
//! wrapped in a spinlock, `alloc` and `free` are non-blocking,
//! non-allocating, and safe to call from interrupt context (coalesce
//! recursion is bounded by `MAX_ORDER`).
//!
//! See `docs/architecture/overview.md` §"Memory management" for the
//! three-layer plan this fits into.

use crate::limine::{MEMMAP_USABLE, MemoryMapEntry, MemoryMapResponse};
use crate::mm::{PAGE_SHIFT, PAGE_SIZE, PhysAddr};

/// Largest allocatable block: `PAGE_SIZE << MAX_ORDER` = 4 MiB.
pub const MAX_ORDER: usize = 10;

/// Number of order levels (orders 0..=MAX_ORDER).
pub const NUM_ORDERS: usize = MAX_ORDER + 1;

/// Frames below 1 MiB are excluded from the allocator. Legacy DMA buffers,
/// the BIOS data area, and the AP bring-up trampoline (Phase 1.5+) need
/// to live there; reserving the range wholesale costs us 256 frames and
/// removes a class of subtle bugs.
const LOW_MEMORY_LIMIT: u64 = 0x10_0000;

/// Sentinel `next` value meaning "end of list". `PhysAddr(0)` is safe to
/// reserve because it sits below `LOW_MEMORY_LIMIT` and never enters the
/// allocator.
const NEXT_SENTINEL: u64 = 0;

/// Physical-memory buddy allocator.
///
/// **Locking.** This struct is not `Sync`. The kernel memory manager
/// wraps it in a spinlock; all `alloc` and `free` operations execute
/// under that lock.
///
/// **Allocation tracking.** The allocator only tracks *free* frames. The
/// caller must remember the order it requested and pass the same order
/// back to `free`; mismatched orders silently corrupt the coalesce bitmap.
pub struct BuddyAllocator {
    /// Head of each per-order free list. `None` if empty.
    free_lists: [Option<PhysAddr>; NUM_ORDERS],
    /// Pointer to the coalesce bitmap as accessed through the HHDM.
    coalesce_bits: *mut u64,
    /// Total `u64` words backing the bitmap.
    coalesce_words: usize,
    /// Word offset within `coalesce_bits` of each order's row.
    row_offsets: [usize; NUM_ORDERS],
    /// First frame number in the managed range.
    base_frame: usize,
    /// Total frames covered by `[base_frame, base_frame + num_frames)`.
    /// Holes within this range are simply never freed in.
    num_frames: usize,
    /// HHDM offset. `phys + hhdm_offset` is a virtual address usable to
    /// touch the corresponding physical byte.
    hhdm_offset: u64,
}

// SAFETY: the only non-`Send` field is the raw pointer to the coalesce
// bitmap. The caller serialises access through a spinlock, so the pointer
// is owned exclusively by whichever CPU currently holds it.
unsafe impl Send for BuddyAllocator {}

impl BuddyAllocator {
    /// Build a buddy allocator from a Limine memory map.
    ///
    /// Two passes: first compute the managed frame range and reserve a
    /// coalesce bitmap inside a usable region; then walk every usable
    /// frame above 1 MiB (skipping the bitmap itself) and feed it to
    /// `free_frame(_, 0)`. The bitmap's XOR-on-free behaviour merges
    /// adjacent frees into higher-order blocks naturally.
    ///
    /// # Safety
    ///
    /// - `memory_map` must be a valid Limine response — its `entries`
    ///   pointer must reference a live array of `entry_count` valid
    ///   `MemoryMapEntry` pointers, none of which alias each other.
    /// - `hhdm_offset` must be the bootloader's HHDM base: for any
    ///   physical address `p` covered by a Usable entry, `(p +
    ///   hhdm_offset) as *mut _` must be a writable virtual address.
    /// - The allocator becomes the sole owner of every frame covered by
    ///   a Usable entry from the moment this function returns. Other
    ///   code must not read or write those frames until they are handed
    ///   out by `alloc`.
    pub unsafe fn new(memory_map: &MemoryMapResponse, hhdm_offset: u64) -> Self {
        // Pass 1: scan usable entries to find the bounding frame range.
        let mut min_base = u64::MAX;
        let mut max_end = 0u64;
        // SAFETY: caller asserts `memory_map` is valid.
        unsafe {
            for_each_usable(memory_map, |entry| {
                let (start, end) = clip_to_managed(entry);
                if end == 0 {
                    return;
                }
                if start < min_base {
                    min_base = start;
                }
                if end > max_end {
                    max_end = end;
                }
            });
        }
        assert!(
            min_base < max_end,
            "buddy: no usable RAM above 1 MiB in Limine memory map"
        );

        // The bit-index formula `(frame - base_frame) >> (order + 1)`
        // assumes `base_frame` aligns with the natural pair structure at
        // every order. To make it valid up to MAX_ORDER, round base_frame
        // down to a `2^(MAX_ORDER+1)`-frame boundary. The skipped frames
        // become permanent "phantoms": they have bitmap bits but the
        // second pass never feeds them in, so they stay marked as
        // allocated and out of reach.
        let raw_base = (min_base >> PAGE_SHIFT) as usize;
        let raw_end = (max_end >> PAGE_SHIFT) as usize;
        let pair_align = 1usize << (MAX_ORDER + 1);
        let base_frame = raw_base & !(pair_align - 1);
        let num_frames = raw_end - base_frame;

        // Compute per-order bitmap row sizes. Each order `o` has at most
        // `ceil(num_frames / 2^(o+1))` buddy pairs; round each row up to a
        // whole `u64` so toggle math stays word-aligned.
        let mut row_offsets = [0usize; NUM_ORDERS];
        let mut total_words = 0usize;
        let mut o = 0;
        while o < NUM_ORDERS {
            row_offsets[o] = total_words;
            let pairs = (num_frames + (1usize << (o + 1)) - 1) >> (o + 1);
            let words = (pairs + 63) / 64;
            total_words += words;
            o += 1;
        }
        let bitmap_bytes = total_words * 8;
        let bitmap_pages = (bitmap_bytes + PAGE_SIZE - 1) / PAGE_SIZE;
        // We reserve whole pages for the bitmap, so the region we need is
        // the page-rounded size, not `bitmap_bytes`. Searching for only
        // `bitmap_bytes` could pick a region with `bitmap_bytes <= span <
        // bitmap_pages * PAGE_SIZE`, leaving `bitmap_phys_end` past the
        // entry's end — the pass-2 skip check would then strip frames from
        // the *next* usable entry.
        let bitmap_reservation = (bitmap_pages * PAGE_SIZE) as u64;

        // Reserve a contiguous region for the bitmap. Must be page-aligned
        // and above 1 MiB.
        // SAFETY: caller asserts `memory_map` is valid.
        let bitmap_phys = unsafe { find_bitmap_region(memory_map, bitmap_reservation) }
            .expect("buddy: no usable region large enough for coalesce bitmap");
        let bitmap_phys_end = bitmap_phys + bitmap_reservation;

        // Zero the bitmap through the HHDM.
        // SAFETY: `bitmap_phys..+bitmap_bytes` was returned by
        // `find_bitmap_region` from a Usable entry; the HHDM maps that
        // physical range, and no other code has touched it yet.
        let bitmap_ptr = (bitmap_phys + hhdm_offset) as *mut u64;
        unsafe {
            core::ptr::write_bytes(bitmap_ptr as *mut u8, 0, bitmap_bytes);
        }

        let mut alloc = Self {
            free_lists: [None; NUM_ORDERS],
            coalesce_bits: bitmap_ptr,
            coalesce_words: total_words,
            row_offsets,
            base_frame,
            num_frames,
            hhdm_offset,
        };

        // TODO: zone split — DMA (<16 MiB) and Normal zones per
        // docs/architecture/memory-management.md. For now a single flat
        // allocator covers all usable memory; ISA-DMA-bound allocations
        // have no fast path.

        // Pass 2: feed every page in every Usable entry into the allocator
        // at order 0, skipping the bitmap's own pages. Coalescing builds
        // the higher-order free lists from the ground up.
        // SAFETY: caller asserts `memory_map` is valid.
        unsafe {
            for_each_usable(memory_map, |entry| {
                let (start, end) = clip_to_managed(entry);
                if end == 0 {
                    return;
                }
                let mut p = start;
                while p < end {
                    let inside_bitmap = p >= bitmap_phys && p < bitmap_phys_end;
                    if !inside_bitmap {
                        // SAFETY: `p` is a page inside a Usable entry,
                        // above 1 MiB, not occupied by the bitmap. The
                        // allocator-under-construction owns it.
                        alloc.free_frame(PhysAddr(p), 0);
                    }
                    p += PAGE_SIZE as u64;
                }
            });
        }

        alloc
    }

    /// Allocate a block of `2^order` contiguous pages.
    ///
    /// Returns `None` if no block at the requested order can be served,
    /// even after splitting higher orders.
    pub fn alloc(&mut self, order: usize) -> Option<PhysAddr> {
        debug_assert!(order <= MAX_ORDER, "buddy: alloc order {order} > MAX_ORDER");
        if order > MAX_ORDER {
            return None;
        }
        if let Some(addr) = self.pop_free(order) {
            // The pair's bit was 1 (we were the lone free buddy); toggle
            // to 0 to reflect that no buddy is free at this order anymore.
            self.toggle_coalesce_bit(addr.frame(), order);
            return Some(addr);
        }
        if order == MAX_ORDER {
            return None;
        }
        // Split a higher-order block. The recursive `alloc` consumes its
        // pair's bit; we then push the unused upper half onto our order's
        // free list (which toggles our pair's bit from 0 to 1).
        let block = self.alloc(order + 1)?;
        let upper = PhysAddr(block.as_u64() + ((PAGE_SIZE as u64) << order));
        self.push_free(upper, order);
        self.toggle_coalesce_bit(upper.frame(), order);
        Some(block)
    }

    /// Return a block to the allocator. `order` must match the order
    /// originally passed to `alloc`.
    pub fn free(&mut self, addr: PhysAddr, order: usize) {
        debug_assert!(order <= MAX_ORDER, "buddy: free order {order} > MAX_ORDER");
        debug_assert!(
            addr.is_page_aligned(),
            "buddy: free of unaligned address {:#x}",
            addr.as_u64()
        );
        let frame = addr.frame();
        debug_assert!(
            frame >= self.base_frame && frame < self.base_frame + self.num_frames,
            "buddy: free of out-of-range frame {frame}"
        );
        self.free_frame(addr, order);
    }

    /// Smallest order whose block size is `>= size` bytes. `size == 0`
    /// returns 0 (single-page allocation).
    pub const fn order_for_size(size: usize) -> usize {
        let pages = if size == 0 {
            1
        } else {
            (size + PAGE_SIZE - 1) / PAGE_SIZE
        };
        let mut o = 0usize;
        let mut covered = 1usize;
        while covered < pages {
            covered <<= 1;
            o += 1;
        }
        o
    }

    // --- Internals ------------------------------------------------------

    fn free_frame(&mut self, addr: PhysAddr, order: usize) {
        let frame = addr.frame();
        let bit_after = self.toggle_coalesce_bit(frame, order);
        if order == MAX_ORDER || bit_after == 1 {
            // Buddy is not free at this order (bit_after == 1) or there
            // is no higher order to coalesce into.
            self.push_free(addr, order);
            return;
        }
        // bit_after == 0: buddy was the lone free block at this order;
        // both halves are now free, merge upward.
        let buddy_frame = frame ^ (1 << order);
        let buddy_addr = PhysAddr((buddy_frame as u64) << PAGE_SHIFT);
        let removed = self.remove_from_free_list(buddy_addr, order);
        debug_assert!(
            removed,
            "buddy: coalesce expected buddy {:#x} on free_lists[{order}]",
            buddy_addr.as_u64()
        );
        let parent_frame = frame & !(1usize << order);
        let parent_addr = PhysAddr((parent_frame as u64) << PAGE_SHIFT);
        self.free_frame(parent_addr, order + 1);
    }

    fn push_free(&mut self, addr: PhysAddr, order: usize) {
        let head = self.free_lists[order];
        // SAFETY: `addr` is a free frame currently owned by the
        // allocator; the allocator reserves the first 8 bytes of every
        // free frame for the intrusive next pointer.
        unsafe {
            self.write_next(addr, head);
        }
        self.free_lists[order] = Some(addr);
    }

    fn pop_free(&mut self, order: usize) -> Option<PhysAddr> {
        let head = self.free_lists[order]?;
        // SAFETY: `head` came off our own free list, so its next pointer
        // slot was written by a previous `push_free`/`write_next`.
        let next = unsafe { self.read_next(head) };
        self.free_lists[order] = next;
        Some(head)
    }

    fn remove_from_free_list(&mut self, addr: PhysAddr, order: usize) -> bool {
        let mut prev: Option<PhysAddr> = None;
        let mut cur = self.free_lists[order];
        while let Some(node) = cur {
            // SAFETY: `node` is on our free list; its next pointer slot
            // was written by `push_free`.
            let next = unsafe { self.read_next(node) };
            if node == addr {
                match prev {
                    Some(prev_addr) => {
                        // SAFETY: `prev_addr` is on our free list.
                        unsafe { self.write_next(prev_addr, next) };
                    }
                    None => {
                        self.free_lists[order] = next;
                    }
                }
                return true;
            }
            prev = Some(node);
            cur = next;
        }
        false
    }

    /// XOR the coalesce bit for `(frame, order)`'s pair. Returns the bit
    /// value after the toggle (0 or 1).
    fn toggle_coalesce_bit(&mut self, frame: usize, order: usize) -> u64 {
        let bit = (frame - self.base_frame) >> (order + 1);
        let word_idx = self.row_offsets[order] + (bit / 64);
        let bit_in_word = bit % 64;
        debug_assert!(word_idx < self.coalesce_words);
        // SAFETY: `word_idx < coalesce_words`; `coalesce_bits` points to
        // `coalesce_words` u64s allocated and zeroed during `new`.
        unsafe {
            let word_ptr = self.coalesce_bits.add(word_idx);
            let mask = 1u64 << bit_in_word;
            let new = (*word_ptr) ^ mask;
            *word_ptr = new;
            (new >> bit_in_word) & 1
        }
    }

    /// Write a free frame's intrusive next pointer.
    ///
    /// # Safety
    /// `addr` must point to a free, allocator-owned frame; the HHDM
    /// mapping for `addr` must be live.
    unsafe fn write_next(&self, addr: PhysAddr, next: Option<PhysAddr>) {
        let ptr = (addr.as_u64() + self.hhdm_offset) as *mut u64;
        let value = match next {
            Some(p) => p.as_u64(),
            None => NEXT_SENTINEL,
        };
        // SAFETY: per the function-level contract; `*mut u64` writes a
        // single naturally-aligned word.
        unsafe {
            core::ptr::write_volatile(ptr, value);
        }
    }

    /// Read a free frame's intrusive next pointer.
    ///
    /// # Safety
    /// `addr` must point to a free, allocator-owned frame; the HHDM
    /// mapping for `addr` must be live.
    unsafe fn read_next(&self, addr: PhysAddr) -> Option<PhysAddr> {
        let ptr = (addr.as_u64() + self.hhdm_offset) as *const u64;
        // SAFETY: per the function-level contract.
        let v = unsafe { core::ptr::read_volatile(ptr) };
        if v == NEXT_SENTINEL {
            None
        } else {
            Some(PhysAddr(v))
        }
    }
}

// --- Memory-map helpers --------------------------------------------------

/// Run `f` on every `Usable` entry of `memory_map`.
///
/// # Safety
/// `memory_map` must be a valid Limine response (see
/// `BuddyAllocator::new`).
unsafe fn for_each_usable<F: FnMut(&MemoryMapEntry)>(
    memory_map: &MemoryMapResponse,
    mut f: F,
) {
    let count = memory_map.entry_count as usize;
    let mut i = 0;
    while i < count {
        // SAFETY: caller asserts `entries` references an array of `count`
        // valid pointers.
        let entry_ptr = unsafe { *memory_map.entries.add(i) };
        if !entry_ptr.is_null() {
            // SAFETY: caller asserts each pointer references a valid
            // entry that outlives this call.
            let entry = unsafe { &*entry_ptr };
            if entry.kind == MEMMAP_USABLE {
                f(entry);
            }
        }
        i += 1;
    }
}

/// Return the page-aligned `[start, end)` of a usable entry, clipped
/// against `LOW_MEMORY_LIMIT`. Returns `(0, 0)` if the entry is wholly
/// below 1 MiB or otherwise empty after alignment.
fn clip_to_managed(entry: &MemoryMapEntry) -> (u64, u64) {
    let end = entry.base.saturating_add(entry.length);
    if end <= LOW_MEMORY_LIMIT {
        return (0, 0);
    }
    let start_raw = entry.base.max(LOW_MEMORY_LIMIT);
    let start = (start_raw + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1);
    let end = end & !(PAGE_SIZE as u64 - 1);
    if end <= start {
        (0, 0)
    } else {
        (start, end)
    }
}

/// Find a page-aligned, above-1-MiB physical address inside some Usable
/// entry where `reservation_bytes` consecutive bytes will fit. Callers
/// pass the page-rounded reservation size (not the bare bitmap length)
/// so the chosen region fully contains every page the bitmap occupies.
///
/// # Safety
/// `memory_map` must be a valid Limine response.
unsafe fn find_bitmap_region(
    memory_map: &MemoryMapResponse,
    reservation_bytes: u64,
) -> Option<u64> {
    let mut chosen: Option<u64> = None;
    // SAFETY: caller asserts `memory_map` is valid.
    unsafe {
        for_each_usable(memory_map, |entry| {
            if chosen.is_some() {
                return;
            }
            let (start, end) = clip_to_managed(entry);
            if end == 0 {
                return;
            }
            if end - start >= reservation_bytes {
                chosen = Some(start);
            }
        });
    }
    chosen
}

// --- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A heap-backed substitute for physical memory. The buffer's host
    /// virtual address doubles as the "physical" address the buddy
    /// allocator sees; the HHDM offset is therefore 0 and any
    /// `(phys + hhdm_offset) as *mut _` resolves to a valid host pointer
    /// inside `_backing`.
    ///
    /// Heap-stable layers (`Vec`'s allocation pointer and `Box`'s contents
    /// don't move when their owning struct moves) keep the pointer chain
    /// `response -> entries -> entry` valid for the lifetime of `FakeMem`.
    struct FakeMem {
        _backing: Vec<u8>,
        _entries: Vec<MemoryMapEntry>,
        _entry_ptrs: Vec<*mut MemoryMapEntry>,
        response: Box<MemoryMapResponse>,
    }

    impl FakeMem {
        /// `bytes` of usable RAM, page-aligned. Real host virtual
        /// addresses sit well above 1 MiB, so the buddy allocator's
        /// low-memory cutoff doesn't reject the region.
        fn new(bytes: usize) -> Self {
            assert!(bytes % PAGE_SIZE == 0);
            // Over-allocate so the page-aligned start lies inside the
            // buffer with room for whatever bitmap pages the allocator
            // carves off the front.
            let mut backing = vec![0u8; bytes + 2 * PAGE_SIZE];
            let raw = backing.as_mut_ptr() as u64;
            let aligned = (raw + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1);
            assert!(
                aligned >= LOW_MEMORY_LIMIT,
                "host buffer at {aligned:#x} should sit above 1 MiB"
            );

            let mut entries = vec![MemoryMapEntry {
                base: aligned,
                length: bytes as u64,
                kind: MEMMAP_USABLE,
            }];
            let entries_ptr: *mut MemoryMapEntry = entries.as_mut_ptr();
            let mut entry_ptrs: Vec<*mut MemoryMapEntry> = vec![entries_ptr];
            let entry_ptrs_ptr = entry_ptrs.as_mut_ptr();

            let response = Box::new(MemoryMapResponse {
                revision: 0,
                entry_count: 1,
                entries: entry_ptrs_ptr,
            });

            Self {
                _backing: backing,
                _entries: entries,
                _entry_ptrs: entry_ptrs,
                response,
            }
        }

        fn memmap(&self) -> &MemoryMapResponse {
            &self.response
        }
    }

    fn make_alloc(bytes: usize) -> (FakeMem, BuddyAllocator) {
        let mem = FakeMem::new(bytes);
        // SAFETY: `mem.memmap()` is a valid response built above; the
        // HHDM offset is zero because the fake "physical" addresses are
        // also host virtual addresses inside `_backing`; the test holds
        // the only reference to `mem`.
        let alloc = unsafe { BuddyAllocator::new(mem.memmap(), 0) };
        (mem, alloc)
    }

    #[test]
    fn order_for_size_round_up() {
        assert_eq!(BuddyAllocator::order_for_size(0), 0);
        assert_eq!(BuddyAllocator::order_for_size(1), 0);
        assert_eq!(BuddyAllocator::order_for_size(PAGE_SIZE), 0);
        assert_eq!(BuddyAllocator::order_for_size(PAGE_SIZE + 1), 1);
        assert_eq!(BuddyAllocator::order_for_size(PAGE_SIZE * 2), 1);
        assert_eq!(BuddyAllocator::order_for_size(PAGE_SIZE * 3), 2);
        assert_eq!(BuddyAllocator::order_for_size(PAGE_SIZE * 4), 2);
        assert_eq!(
            BuddyAllocator::order_for_size(PAGE_SIZE << MAX_ORDER),
            MAX_ORDER
        );
    }

    #[test]
    fn alloc_order_zero_returns_some() {
        let (_mem, mut a) = make_alloc(PAGE_SIZE * 64);
        let p = a.alloc(0).expect("first order-0 alloc should succeed");
        assert!(p.is_page_aligned());
    }

    #[test]
    fn exhausts_then_fails() {
        let (_mem, mut a) = make_alloc(PAGE_SIZE * 64);
        let mut handed_out = 0usize;
        while a.alloc(0).is_some() {
            handed_out += 1;
            assert!(handed_out < 1024, "runaway allocation loop");
        }
        assert!(handed_out > 0, "should hand out at least one page");
        assert!(
            a.alloc(0).is_none(),
            "alloc after exhaustion must be None"
        );
    }

    #[test]
    fn free_then_realloc_returns_same_block() {
        let (_mem, mut a) = make_alloc(PAGE_SIZE * 64);
        let p = a.alloc(0).expect("alloc 0");
        a.free(p, 0);
        let q = a.alloc(0).expect("alloc 0 after free");
        assert_eq!(
            p, q,
            "LIFO free list should return the just-freed block"
        );
    }

    #[test]
    fn freeing_two_buddies_coalesces() {
        let (_mem, mut a) = make_alloc(PAGE_SIZE * 64);
        // Pull a known order-1 block so the two halves are guaranteed
        // buddies at order 0. Then free both at order 0 — the bitmap's
        // XOR semantics are symmetric, so two order-0 frees of buddies
        // must coalesce back into a single order-1 block at the lower
        // half's address.
        let block = a.alloc(1).expect("alloc order 1");
        assert_eq!(
            block.frame() & 1,
            0,
            "order-1 block must be order-1-aligned"
        );
        let hi = PhysAddr(block.as_u64() + PAGE_SIZE as u64);
        a.free(block, 0);
        a.free(hi, 0);
        let merged = a.alloc(1).expect("coalesced order-1 alloc");
        assert_eq!(
            merged, block,
            "coalesced block should start at the lower buddy"
        );
    }

    #[test]
    fn split_higher_order_to_satisfy_lower() {
        let (_mem, mut a) = make_alloc(PAGE_SIZE * 64);
        // The split path puts the unused upper half onto `free_lists[0]`
        // so the very next alloc returns its buddy. Detect that pattern
        // by checking each consecutive pair of successful allocs in a
        // sliding window — at least one pair must be a true order-0
        // buddy pair (lower half on an even frame, halves one page
        // apart) before the allocator runs out.
        let mut last: Option<PhysAddr> = None;
        let mut observed_buddy_pair = false;
        while let Some(cur) = a.alloc(0) {
            if let Some(prev) = last {
                let (lo, hi) = if prev.as_u64() < cur.as_u64() {
                    (prev, cur)
                } else {
                    (cur, prev)
                };
                if hi.as_u64() - lo.as_u64() == PAGE_SIZE as u64
                    && (lo.frame() & 1) == 0
                {
                    observed_buddy_pair = true;
                    break;
                }
            }
            last = Some(cur);
        }
        assert!(
            observed_buddy_pair,
            "split path should produce at least one consecutive buddy pair"
        );
    }
}
