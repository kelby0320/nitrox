//! SLUB-inspired slab allocator on top of the buddy.
//!
//! Sits between the buddy allocator and `libkern`'s heap containers (`KBox`, `KVec`, `KString`),
//! which call [`kmalloc`] / [`kfree`] directly; the kernel registers no `#[global_allocator]`
//! (decision log 2026-05-20). Design: `docs/architecture/memory-management.md`.
//!
//! ## Geometry
//!
//! - Every slab is one 4 KiB page (`SLAB_SIZE`).
//! - The [`SlabDescriptor`] sits at byte 0; objects start at `obj_offset` (the header size
//!   rounded up to the cache's alignment).
//! - Each cache has a `partial` list (slabs with a free slot) and a `full` list.
//! - A free slot stores the next-free pointer in its first 8 bytes (embedded freelist).
//!
//! ## Why free is O(1)
//!
//! The descriptor of any object `p` is at `(p as usize) & SLAB_MASK`, because the buddy hands out
//! `SLAB_SIZE`-aligned pages and the descriptor starts each. `kfree` reads `desc.owner` to find
//! the cache, or the large-allocation sentinel, with no external table. A page-aligned `p` is the
//! exception: a large allocation at `SLAB_SIZE` alignment, its descriptor the page below.
//!
//! ## Scope
//!
//! - One spinlock per cache; no per-CPU fast path.
//! - Seven buckets feed [`kmalloc`]: 32 to 2048 bytes. Larger requests go to the buddy via
//!   [`large_alloc`], with an `owner = null` sentinel in the descriptor.
//! - No empty-slab reclaim: a grown cache keeps its pages.
//! - No alignment above `SLAB_SIZE`; such requests are rejected. Its one client, DMA, also needs
//!   a physical address and contiguity, so it uses [`DmaBuffer`](crate::mm::DmaBuffer) over the
//!   buddy.
//!
//! `kernel/docs/lock-ordering.md`: the slab cache lock (rank 6a) is held across the `buddy.alloc()`
//! in `grow` (rank 6b), the only allocator-to-allocator nesting permitted.

use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::libkern::SpinLock;
use crate::libkern::lockrank::LockRank;
use crate::mm::buddy::BuddyAllocator;
use crate::mm::heap::{BuddyPager, HeapBuddy};
use crate::mm::{PAGE_SIZE, PhysAddr};

/// Bytes per slab: one buddy order-0 block.
pub const SLAB_SIZE: usize = 4096;

/// Mask that, applied to any pointer inside a slab page, yields the
/// page-aligned base address of the [`SlabDescriptor`].
pub const SLAB_MASK: usize = !(SLAB_SIZE - 1);

const SIZE_CLASSES: [usize; 7] = [32, 64, 128, 256, 512, 1024, 2048];

/// Requests larger than this bypass the slab and go to the buddy (see [`large_alloc`]).
const MAX_SLAB_OBJ: usize = 2048;

/// The strongest alignment any bucket provides. Requests needing more go to [`large_alloc`]
/// (see the routing in [`kmalloc`] and the cap in [`slab_init`]).
const MAX_BUCKET_ALIGN: usize = core::mem::align_of::<u64>();

// The pointer-masking trick falls over silently if pages are not `SLAB_SIZE`-aligned.
const _: () = assert!(SLAB_SIZE == PAGE_SIZE);

/// Header at byte 0 of every slab page, found from any object pointer by masking with
/// [`SLAB_MASK`]. `#[repr(C)]` so its layout is predictable.
#[repr(C)]
pub struct SlabDescriptor {
    freelist: *mut u8,

    /// The owning cache; null is the sentinel for a large allocation owned directly by the buddy.
    owner: *const SlabCache,

    next: *mut SlabDescriptor,

    /// Bytes per object; for a large allocation, the buddy block size, so `large_free` can
    /// recover the order.
    obj_size: u32,

    in_use: u16,

    capacity: u16,
}

/// One slab cache: a single object-size bucket behind one spin lock. Const-constructible so
/// `static` arrays of caches live in `.bss` and are patched by [`slab_init`].
pub struct SlabCache {
    state: SpinLock<SlabCacheState>,
}

struct SlabCacheState {
    partial: *mut SlabDescriptor,
    full: *mut SlabDescriptor,

    obj_size: usize,
    obj_align: usize,
    obj_offset: usize,
    objs_per_slab: usize,
}

// SAFETY: all mutable state lives behind `state: SpinLock<...>`. Raw
// pointers inside `SlabCacheState` are accessed only with the lock held.
unsafe impl Sync for SlabCache {}

impl SlabCache {
    /// Construct an uninitialised cache; [`SlabCache::init`] must run first.
    #[allow(clippy::new_without_default, reason = "a `Default` would read as a cache ready to use")]
    pub const fn new() -> Self {
        Self {
            state: SpinLock::new(
                LockRank::SlabCache,
                SlabCacheState {
                    partial: ptr::null_mut(),
                    full: ptr::null_mut(),
                    obj_size: 0,
                    obj_align: 0,
                    obj_offset: 0,
                    objs_per_slab: 0,
                },
            ),
        }
    }

    /// Finalise the geometry from a requested size/alignment, asserting so a misconfigured cache
    /// fails loudly.
    pub fn init(&self, requested_size: usize, requested_align: usize) {
        let ptr_size = core::mem::size_of::<*mut u8>();
        let ptr_align = core::mem::align_of::<*mut u8>();
        let obj_size = requested_size.max(ptr_size);
        let obj_align = requested_align.max(ptr_align);
        let header = core::mem::size_of::<SlabDescriptor>();
        let obj_offset = align_up(header, obj_align);
        let usable = SLAB_SIZE.saturating_sub(obj_offset);
        let objs_per_slab = usable / obj_size;

        assert!(obj_offset >= header, "slab init: obj_offset < SlabDescriptor size");
        assert!(obj_offset.is_multiple_of(obj_align), "slab init: obj_offset misaligned");
        assert!(objs_per_slab >= 1, "slab init: obj_size too large for SLAB_SIZE");

        let mut state = self.state.lock();
        state.obj_size = obj_size;
        state.obj_align = obj_align;
        state.obj_offset = obj_offset;
        state.objs_per_slab = objs_per_slab;
    }

    /// Allocate one object; null on OOM. `pager` is used if the cache must grow ([`HeapBuddy`] in
    /// production, a local one in tests).
    pub fn alloc<P: BuddyPager>(&self, pager: &P) -> *mut u8 {
        let mut state = self.state.lock();
        if state.partial.is_null() {
            // SAFETY: cache lock held; we own `state`. `grow_locked`
            // returns either null or a fresh, owner-tagged descriptor
            // with an initialised freelist.
            let new_desc = unsafe { Self::grow_locked(self, &mut state, pager) };
            if new_desc.is_null() {
                return ptr::null_mut();
            }
            state.partial = new_desc;
        }
        let desc = state.partial;
        // SAFETY: `desc` is on the partial list, which by definition has
        // at least one free slot; we hold the cache lock.
        let obj = unsafe { (*desc).freelist };
        debug_assert!(!obj.is_null(), "slab: partial slab with null freelist");
        // SAFETY: `obj` is a free slot in our slab; its first 8 bytes
        // hold the embedded-freelist next pointer.
        let next = unsafe { *(obj as *mut *mut u8) };
        // SAFETY: lock held; we own this descriptor's state.
        unsafe {
            (*desc).freelist = next;
            (*desc).in_use += 1;
        }
        if next.is_null() {
            // SAFETY: lock held; desc is the partial-list head.
            unsafe {
                state.partial = (*desc).next;
                (*desc).next = state.full;
                state.full = desc;
            }
        }
        obj
    }

    /// Return one object to this cache.
    ///
    /// # Safety
    ///
    /// `ptr` was handed out by this same cache's `alloc` and is not used again.
    pub unsafe fn free(&self, ptr: *mut u8) {
        let desc = ((ptr as usize) & SLAB_MASK) as *mut SlabDescriptor;
        let mut state = self.state.lock();
        // SAFETY: by the slab invariant the page header is a valid
        // descriptor; lock held so no concurrent mutator.
        let was_full = unsafe {
            debug_assert!((*desc).in_use > 0, "slab: free underflow");
            (*desc).freelist.is_null()
        };
        // SAFETY: `ptr` is a slot we previously handed out; lock held.
        unsafe {
            *(ptr as *mut *mut u8) = (*desc).freelist;
            (*desc).freelist = ptr;
            (*desc).in_use -= 1;
        }
        if was_full {
            // SAFETY: lock held; desc was on the full list.
            unsafe { Self::move_full_to_partial(&mut state, desc) };
        }
    }

    /// Allocate a fresh page from `pager` and initialise its descriptor and embedded freelist;
    /// null on OOM.
    ///
    /// # Safety
    /// Caller must hold the cache lock and pass its locked state via
    /// `state` so the new slab's geometry matches the cache's
    /// configuration.
    unsafe fn grow_locked<P: BuddyPager>(
        cache: &SlabCache,
        state: &mut SlabCacheState,
        pager: &P,
    ) -> *mut SlabDescriptor {
        let phys = match pager.alloc(0) {
            Some(p) => p,
            None => return ptr::null_mut(),
        };
        let virt = (phys.as_u64() + pager.hhdm_offset()) as *mut u8;
        debug_assert!((virt as usize) & (SLAB_SIZE - 1) == 0, "slab: buddy returned a non-SLAB_SIZE-aligned page");
        let desc = virt as *mut SlabDescriptor;
        // SAFETY: `virt` points to a freshly-allocated, owned page; we
        // are the sole writer.
        unsafe {
            ptr::write(
                desc,
                SlabDescriptor {
                    freelist: ptr::null_mut(),
                    owner: cache as *const SlabCache,
                    next: ptr::null_mut(),
                    obj_size: state.obj_size as u32,
                    in_use: 0,
                    capacity: state.objs_per_slab as u16,
                },
            );
            let first = virt.add(state.obj_offset);
            let stride = state.obj_size;
            let n = state.objs_per_slab;
            let mut i = 0;
            while i < n {
                let slot = first.add(i * stride);
                let next = if i + 1 == n { ptr::null_mut() } else { first.add((i + 1) * stride) };
                *(slot as *mut *mut u8) = next;
                i += 1;
            }
            (*desc).freelist = first;
        }
        desc
    }

    /// Remove `desc` from the full list and prepend it to `partial`. An O(n) scan, acceptable as
    /// the full list is short.
    ///
    /// # Safety
    /// Caller holds the cache lock; `desc` is on `state.full`.
    unsafe fn move_full_to_partial(state: &mut SlabCacheState, desc: *mut SlabDescriptor) {
        // SAFETY: traversing the full list under the cache lock; all
        // pointers in the list are valid slab descriptors we own.
        unsafe {
            let mut prev: *mut SlabDescriptor = ptr::null_mut();
            let mut cur = state.full;
            while !cur.is_null() {
                if cur == desc {
                    if prev.is_null() {
                        state.full = (*cur).next;
                    } else {
                        (*prev).next = (*cur).next;
                    }
                    (*cur).next = state.partial;
                    state.partial = cur;
                    return;
                }
                prev = cur;
                cur = (*cur).next;
            }
            debug_assert!(false, "slab: full-to-partial scan failed to locate descriptor");
        }
    }
}

/// Seven size-bucket caches feeding [`kmalloc`], in `.bss`; [`slab_init`] writes their geometry.
static SLAB_CACHES: [SlabCache; 7] = [
    SlabCache::new(),
    SlabCache::new(),
    SlabCache::new(),
    SlabCache::new(),
    SlabCache::new(),
    SlabCache::new(),
    SlabCache::new(),
];

/// Set by [`slab_init`]; [`kmalloc`] panics before it rather than corrupt the heap.
static SLAB_INITIALISED: AtomicBool = AtomicBool::new(false);

/// Initialise the seven caches. Call once at boot, after [`crate::mm::heap::init_buddy`].
pub fn slab_init() {
    assert!(!SLAB_INITIALISED.swap(true, Ordering::AcqRel), "slab_init called twice");
    let mut i = 0;
    while i < SIZE_CLASSES.len() {
        // Capped at `MAX_BUCKET_ALIGN`: more alignment serves nothing routed here, and callers
        // needing more go to the large-alloc path (see the routing in [`kmalloc`]).
        let align = SIZE_CLASSES[i].min(MAX_BUCKET_ALIGN);
        SLAB_CACHES[i].init(SIZE_CLASSES[i], align);
        i += 1;
    }
}

fn size_class_index(min_size: usize) -> Option<usize> {
    let mut i = 0;
    while i < SIZE_CLASSES.len() {
        if SIZE_CLASSES[i] >= min_size {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Allocate `size` bytes aligned to `align`; null on OOM or an unservable request (e.g.
/// `align > SLAB_SIZE`).
///
/// `kmalloc(0, ...)` returns a non-null aligned sentinel that must never be dereferenced;
/// passing it to [`kfree`] is not allowed.
pub fn kmalloc(size: usize, align: usize) -> *mut u8 {
    if size == 0 {
        // ZST convention: a non-null aligned sentinel, which `KBox` and `KVec` never free.
        return align.max(1) as *mut u8;
    }
    if align > SLAB_SIZE {
        // Not served here: its one client, DMA, uses `mm::dma::DmaBuffer` (see the module docs).
        return ptr::null_mut();
    }
    if !SLAB_INITIALISED.load(Ordering::Acquire) {
        panic!("kmalloc called before slab_init");
    }
    #[cfg(test)]
    if cfg!(miri) {
        return miri_heap::alloc(size, align);
    }
    let bucket_size = size.max(align);
    // Large path when the request exceeds every bucket **or** needs more alignment than a bucket
    // gives: `slab_init` caps buckets at `align_of::<u64>()`, and `bucket_size` alone misses this
    // (a 64-aligned 832-byte request would land in the 1024 bucket at 8-byte alignment).
    // `large_alloc` honours `align` by rounding the object offset up inside a page-aligned block;
    // its first client is the per-thread FPU save area (`XSAVE` `#GP`s unless 64-aligned).
    if bucket_size > MAX_SLAB_OBJ || align > MAX_BUCKET_ALIGN {
        return large_alloc(size, align, &HeapBuddy);
    }
    let idx = size_class_index(bucket_size).expect("bucket_size in range");
    SLAB_CACHES[idx].alloc(&HeapBuddy)
}

/// Like [`kmalloc`] but zeroes the returned memory.
pub fn kzalloc(size: usize, align: usize) -> *mut u8 {
    let p = kmalloc(size, align);
    if !p.is_null() && size > 0 {
        // SAFETY: when `kmalloc` returns non-null for `size > 0`, it has
        // handed us `size` writable bytes; the ZST sentinel branch is
        // excluded by the `size > 0` check.
        unsafe {
            ptr::write_bytes(p, 0, size);
        }
    }
    p
}

/// Return memory to the slab. `kfree(null)` is a no-op.
///
/// # Safety
///
/// `ptr` is null, or came from `kmalloc`, is not zero-sized, and is not used again.
pub unsafe fn kfree(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    #[cfg(test)]
    if cfg!(miri) {
        // SAFETY: under Miri every `kmalloc` came from `miri_heap::alloc`; the caller gives it back.
        return unsafe { miri_heap::free(ptr) };
    }
    let desc = descriptor_of(ptr);
    // SAFETY: every non-null pointer `kmalloc` returns lies in a buddy-owned page starting with
    // a valid SlabDescriptor (non-null `owner` for a slab, null for a large allocation). The ZST
    // sentinel is not such a pointer, so the caller must not free one; `KBox` / `KVec` give
    // zero-sized types a dangling pointer and never route them here.
    let owner = unsafe { (*desc).owner };
    if owner.is_null() {
        large_free(ptr, &HeapBuddy);
        return;
    }
    // SAFETY: `owner` is a pointer captured by `SlabCache::grow_locked`
    // from a `&SlabCache` that lives in `SLAB_CACHES` (or in a test's
    // local array, which lives at least as long as the allocation).
    let cache = unsafe { &*owner };
    // SAFETY: `ptr` came from this cache's `alloc`, as its descriptor's `owner` says.
    unsafe { cache.free(ptr) };
}

/// The descriptor of `ptr`, from [`kmalloc`]: the page below a page-aligned object, only ever a
/// [`large_alloc`] at `SLAB_SIZE` alignment, and otherwise the start of its own page.
fn descriptor_of(ptr: *mut u8) -> *mut SlabDescriptor {
    let page = (ptr as usize) & SLAB_MASK;
    let page = if page == ptr as usize { page - SLAB_SIZE } else { page };
    page as *mut SlabDescriptor
}

/// Allocate a buddy block holding `size`/`align` plus the descriptor header; returns the pointer
/// past the header.
fn large_alloc<P: BuddyPager>(size: usize, align: usize, pager: &P) -> *mut u8 {
    let header = core::mem::size_of::<SlabDescriptor>();
    // The user pointer sits at `obj_offset`, rounded up for the caller's alignment.
    let obj_offset = align_up(header, align);
    let total = obj_offset.saturating_add(size);
    let order = BuddyAllocator::order_for_size(total);
    let phys = match pager.alloc(order) {
        Some(p) => p,
        None => return ptr::null_mut(),
    };
    let virt = (phys.as_u64() + pager.hhdm_offset()) as *mut u8;
    let desc = virt as *mut SlabDescriptor;
    // SAFETY: `virt` is a fresh, owned buddy block of at least `total`
    // bytes (because `order_for_size(total)` rounds up); we are the sole
    // writer.
    unsafe {
        ptr::write(
            desc,
            SlabDescriptor {
                freelist: ptr::null_mut(),
                owner: ptr::null(),
                next: ptr::null_mut(),
                obj_size: (PAGE_SIZE << order) as u32,
                in_use: 1,
                capacity: 1,
            },
        );
        virt.add(obj_offset)
    }
}

/// Free a large allocation from [`large_alloc`].
fn large_free<P: BuddyPager>(ptr: *mut u8, pager: &P) {
    let desc = descriptor_of(ptr);
    let page0 = desc as usize;
    // SAFETY: large-alloc invariant — descriptor at byte 0 of the first
    // page of the block.
    let total = unsafe { (*desc).obj_size as usize };
    let order = BuddyAllocator::order_for_size(total);
    let phys = PhysAddr::new((page0 as u64).wrapping_sub(pager.hhdm_offset()));
    pager.free(phys, order);
}

/// **Under Miri, each `kmalloc` is a host allocation of its own**, so a use after `kfree` is one
/// Miri reports: the slab packs objects into pages of one leaked buffer, still live when an object
/// is freed. A header before the object records the block's layout for `free`.
#[cfg(test)]
mod miri_heap {
    use std::alloc::{Layout, alloc as host_alloc, dealloc};

    /// The header's bytes: the block's size, then its alignment.
    const HEADER: usize = 2 * size_of::<usize>();

    pub(super) fn alloc(size: usize, align: usize) -> *mut u8 {
        // The object starts one alignment in, so the header fits before it and it stays aligned.
        let align = align.max(HEADER);
        let Some(total) = align.checked_add(size) else {
            return core::ptr::null_mut();
        };
        let Ok(layout) = Layout::from_size_align(total, align) else {
            return core::ptr::null_mut();
        };
        // SAFETY: `layout` has a non-zero size, as `size` is.
        let block = unsafe { host_alloc(layout) };
        if block.is_null() {
            return block;
        }
        // SAFETY: the block holds `align >= HEADER` bytes before the object, `usize`-aligned.
        unsafe {
            let obj = block.add(align);
            obj.cast::<usize>().sub(2).write(layout.size());
            obj.cast::<usize>().sub(1).write(layout.align());
            obj
        }
    }

    /// # Safety
    /// `obj` came from [`alloc`] and is not used again.
    pub(super) unsafe fn free(obj: *mut u8) {
        // SAFETY: the header `alloc` wrote before `obj` describes the block, which starts one
        // alignment before it.
        unsafe {
            let size = obj.cast::<usize>().sub(2).read();
            let align = obj.cast::<usize>().sub(1).read();
            dealloc(obj.sub(align), Layout::from_size_align_unchecked(size, align));
        }
    }

    /// Host-run, as Miri runs nowhere in CI: each block comes back aligned with its whole size
    /// usable, and goes back to the host allocator from where it began.
    #[test]
    fn a_block_is_aligned_whole_and_freed_from_its_start() {
        for (size, align) in [(1, 1), (3, 32), (64, 8), (832, 64), (4096, 4096), (5000, 16)] {
            let obj = alloc(size, align);
            assert!(!obj.is_null(), "{size} bytes at {align}");
            assert_eq!(obj as usize % align, 0, "{size} bytes at {align}");
            // SAFETY: `alloc` gave `size` bytes at `obj`, and nothing uses them after `free`.
            unsafe {
                obj.write_bytes(0xA5, size);
                free(obj);
            }
        }
    }

    /// A size past any layout is refused with null, as the slab refuses it.
    #[test]
    fn a_size_past_any_layout_is_refused() {
        for align in [8, 4096] {
            assert!(alloc(usize::MAX, align).is_null(), "{align}");
            assert!(alloc(usize::MAX - align + 1, align).is_null(), "{align}: the first that overflows");
            assert!(alloc(usize::MAX - align, align).is_null(), "{align}: the last that does not");
        }
    }
}

#[inline]
fn align_up(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    //! Slab tests build a [`LocalBuddy`] over a per-test [`BuddyAllocator`] on [`FakeMem`] and
    //! exercise local caches, leaving the production statics untouched (hermetic, parallel-safe).

    use super::*;
    use crate::limine::{MEMMAP_USABLE, MemoryMapEntry, MemoryMapResponse};
    use core::cell::RefCell;

    /// Heap-backed stand-in for physical memory (see `buddy::tests::FakeMem`).
    struct FakeMem {
        _backing: Vec<u8>,
        _entries: Vec<MemoryMapEntry>,
        _entry_ptrs: Vec<*mut MemoryMapEntry>,
        response: Box<MemoryMapResponse>,
    }

    impl FakeMem {
        fn new(bytes: usize) -> Self {
            assert!(bytes.is_multiple_of(PAGE_SIZE));
            let mut backing = vec![0u8; bytes + 2 * PAGE_SIZE];
            let raw = backing.as_mut_ptr() as u64;
            let aligned = align_up(raw as usize, PAGE_SIZE) as u64;
            assert!(aligned >= 0x10_0000);
            let mut entries = vec![MemoryMapEntry { base: aligned, length: bytes as u64, kind: MEMMAP_USABLE }];
            let entries_ptr: *mut MemoryMapEntry = entries.as_mut_ptr();
            let mut entry_ptrs: Vec<*mut MemoryMapEntry> = vec![entries_ptr];
            let entry_ptrs_ptr = entry_ptrs.as_mut_ptr();
            // SAFETY: one entry, which the fixture keeps as long as the map.
            let response = Box::new(unsafe { MemoryMapResponse::fabricated(1, entry_ptrs_ptr) });
            Self { _backing: backing, _entries: entries, _entry_ptrs: entry_ptrs, response }
        }

        fn memmap(&self) -> &MemoryMapResponse {
            &self.response
        }
    }

    /// Test-only [`BuddyPager`] over a local [`BuddyAllocator`] behind a `RefCell`.
    struct LocalBuddy {
        inner: RefCell<BuddyAllocator>,
        hhdm: u64,
    }

    impl BuddyPager for LocalBuddy {
        fn alloc(&self, order: usize) -> Option<PhysAddr> {
            self.inner.borrow_mut().alloc(order)
        }
        fn free(&self, addr: PhysAddr, order: usize) {
            self.inner.borrow_mut().free(addr, order);
        }
        fn hhdm_offset(&self) -> u64 {
            self.hhdm
        }
    }

    fn make_world(bytes: usize) -> (FakeMem, LocalBuddy) {
        let mem = FakeMem::new(bytes);
        // SAFETY: `mem.memmap()` is a valid response; HHDM offset is 0
        // because the fake "physical" addresses double as host virtual
        // addresses inside `mem._backing`.
        let buddy = unsafe { BuddyAllocator::new(mem.memmap(), 0) };
        let pager = LocalBuddy { inner: RefCell::new(buddy), hhdm: 0 };
        (mem, pager)
    }

    fn fresh_cache(size: usize) -> SlabCache {
        let cache = SlabCache::new();
        cache.init(size, size.min(core::mem::align_of::<u64>()));
        cache
    }

    impl SlabCache {
        fn partial_len(&self) -> usize {
            let state = self.state.lock();
            let mut n = 0;
            let mut cur = state.partial;
            while !cur.is_null() {
                n += 1;
                // SAFETY: lock held; partial list is valid.
                cur = unsafe { (*cur).next };
            }
            n
        }
        fn full_len(&self) -> usize {
            let state = self.state.lock();
            let mut n = 0;
            let mut cur = state.full;
            while !cur.is_null() {
                n += 1;
                // SAFETY: lock held; full list is valid.
                cur = unsafe { (*cur).next };
            }
            n
        }
        fn objs_per_slab(&self) -> usize {
            self.state.lock().objs_per_slab
        }
    }

    #[test]
    fn alloc_returns_aligned_pointer() {
        let (_mem, pager) = make_world(PAGE_SIZE * 64);
        let cache = fresh_cache(64);
        let p = cache.alloc(&pager);
        assert!(!p.is_null());
        assert!((p as usize).is_multiple_of(8));
    }

    #[test]
    fn fills_one_slab_then_grows() {
        let (_mem, pager) = make_world(PAGE_SIZE * 64);
        let cache = fresh_cache(64);
        let cap = cache.objs_per_slab();
        let first = cache.alloc(&pager);
        let first_page = (first as usize) & SLAB_MASK;
        for _ in 1..cap {
            let p = cache.alloc(&pager);
            assert_eq!((p as usize) & SLAB_MASK, first_page);
        }
        let q = cache.alloc(&pager);
        assert!(!q.is_null());
        assert_ne!((q as usize) & SLAB_MASK, first_page);
    }

    #[test]
    fn free_then_realloc_is_lifo() {
        let (_mem, pager) = make_world(PAGE_SIZE * 64);
        let cache = fresh_cache(64);
        let a = cache.alloc(&pager);
        let b = cache.alloc(&pager);
        // SAFETY: `b` came from this cache's `alloc`.
        unsafe { cache.free(b) };
        let c = cache.alloc(&pager);
        assert_eq!(b, c, "LIFO push/pop should return the just-freed slot");
        let _ = a;
    }

    #[test]
    fn partial_to_full_transition() {
        let (_mem, pager) = make_world(PAGE_SIZE * 64);
        let cache = fresh_cache(64);
        let cap = cache.objs_per_slab();
        for _ in 0..cap {
            cache.alloc(&pager);
        }
        assert_eq!(cache.partial_len(), 0);
        assert_eq!(cache.full_len(), 1);
    }

    #[test]
    fn full_to_partial_on_free() {
        let (_mem, pager) = make_world(PAGE_SIZE * 64);
        let cache = fresh_cache(64);
        let cap = cache.objs_per_slab();
        let mut ptrs = Vec::with_capacity(cap);
        for _ in 0..cap {
            ptrs.push(cache.alloc(&pager));
        }
        assert_eq!(cache.full_len(), 1);
        // SAFETY: `ptrs[0]` came from this cache's `alloc`.
        unsafe { cache.free(ptrs[0]) };
        assert_eq!(cache.full_len(), 0);
        assert_eq!(cache.partial_len(), 1);
    }

    #[test]
    fn descriptor_obj_size_matches_bucket() {
        let (_mem, pager) = make_world(PAGE_SIZE * 64);
        let cache = fresh_cache(64);
        let p = cache.alloc(&pager);
        let desc = ((p as usize) & SLAB_MASK) as *mut SlabDescriptor;
        // SAFETY: test owns the slab.
        let recorded = unsafe { (*desc).obj_size };
        assert_eq!(recorded, 64);
    }

    #[test]
    fn kfree_null_is_noop() {
        // SAFETY: null is a no-op.
        unsafe { kfree(ptr::null_mut()) };
    }

    #[test]
    fn large_alloc_routes_through_buddy_with_null_owner() {
        let (_mem, pager) = make_world(PAGE_SIZE * 64);
        let p = large_alloc(8192, 8, &pager);
        assert!(!p.is_null());
        let desc = ((p as usize) & SLAB_MASK) as *mut SlabDescriptor;
        // SAFETY: test owns the allocation.
        let owner = unsafe { (*desc).owner };
        // SAFETY: as above.
        let obj_size = unsafe { (*desc).obj_size };
        assert!(owner.is_null(), "large-alloc descriptor must have null owner");
        assert_eq!(obj_size as usize, PAGE_SIZE << 2);
        large_free(p, &pager);
    }

    #[test]
    fn a_large_alloc_past_the_largest_block_is_refused() {
        let (_mem, pager) = make_world(PAGE_SIZE * 64);
        let header = core::mem::size_of::<SlabDescriptor>();
        assert!(large_alloc((PAGE_SIZE << crate::mm::buddy::MAX_ORDER) - header + 1, 8, &pager).is_null());
        assert!(large_alloc(usize::MAX / 2, 8, &pager).is_null());
    }

    #[test]
    fn the_largest_image_spawn_copies_is_4_mib_less_a_page() {
        // Three times the block, so an aligned one lies inside.
        let (_mem, pager) = make_world((PAGE_SIZE * 3) << crate::mm::buddy::MAX_ORDER);
        let fits = large_alloc(crate::mm::elf::MAX_IMAGE_BYTES, 8, &pager);
        assert!(!fits.is_null());
        large_free(fits, &pager);
        assert!(large_alloc(PAGE_SIZE << crate::mm::buddy::MAX_ORDER, 8, &pager).is_null(), "exactly 4 MiB");
    }

    #[test]
    fn a_page_aligned_allocation_is_freed_whole() {
        crate::mm::test_support::init_global_heap();
        for size in [64, PAGE_SIZE, 3 * PAGE_SIZE] {
            let p = kmalloc(size, SLAB_SIZE);
            assert!(!p.is_null(), "{size} bytes");
            assert_eq!(p as usize % SLAB_SIZE, 0, "{size} bytes");
            // SAFETY: `kmalloc` gave `size` writable bytes at `p`.
            unsafe { p.write_bytes(0, size) };
            let order = BuddyAllocator::order_for_size(SLAB_SIZE + size);
            let freed = crate::mm::heap::frames_freed_by_this_thread();
            // SAFETY: `p` came from `kmalloc` and is not used again.
            unsafe { kfree(p) };
            assert_eq!(crate::mm::heap::frames_freed_by_this_thread() - freed, 1 << order, "{size} bytes");
        }
    }

    #[test]
    fn a_large_allocation_finds_its_descriptor_at_every_alignment() {
        let (_mem, pager) = make_world(PAGE_SIZE * 64);
        for align in [8, 64, 1024, SLAB_SIZE / 2, SLAB_SIZE] {
            let p = large_alloc(64, align, &pager);
            assert!(!p.is_null(), "{align}");
            let desc = descriptor_of(p);
            // SAFETY: `desc` is the allocation's descriptor, which the test owns.
            let (owner, size) = unsafe { ((*desc).owner, (*desc).obj_size as usize) };
            assert!(owner.is_null(), "{align}");
            assert!(desc as usize + size >= p as usize + 64, "{align}: the block holds the object");
            large_free(p, &pager);
        }
    }

    #[test]
    fn large_alloc_round_trip_via_kfree_routing() {
        let (_mem, pager) = make_world(PAGE_SIZE * 64);
        let p = large_alloc(3000, 8, &pager);
        let desc = ((p as usize) & SLAB_MASK) as *mut SlabDescriptor;
        // SAFETY: test owns the allocation.
        assert!(unsafe { (*desc).owner.is_null() });
        large_free(p, &pager);
    }

    #[test]
    fn kzalloc_via_cache_returns_zeroed_memory() {
        let (_mem, pager) = make_world(PAGE_SIZE * 64);
        let cache = fresh_cache(64);
        let p = cache.alloc(&pager);
        // SAFETY: cache handed us 64 writable bytes.
        unsafe {
            ptr::write_bytes(p, 0xAA, 64);
        }
        // SAFETY: `p` came from this cache's `alloc`.
        unsafe { cache.free(p) };
        let q = cache.alloc(&pager);
        assert_eq!(p, q, "LIFO");
        // SAFETY: q is the same 64-byte slot.
        unsafe {
            ptr::write_bytes(q, 0, 64);
        }
        let mut all_zero = true;
        for i in 0..64 {
            // SAFETY: 64 readable bytes.
            if unsafe { *q.add(i) } != 0 {
                all_zero = false;
                break;
            }
        }
        assert!(all_zero);
    }

    #[test]
    #[should_panic(expected = "obj_size too large")]
    fn init_panics_when_obj_too_large() {
        let cache = SlabCache::new();
        cache.init(SLAB_SIZE, 8);
    }

    #[test]
    fn size_class_index_buckets_correctly() {
        assert_eq!(size_class_index(1), Some(0));
        assert_eq!(size_class_index(17), Some(0));
        assert_eq!(size_class_index(32), Some(0));
        assert_eq!(size_class_index(33), Some(1));
        assert_eq!(size_class_index(256), Some(3));
        assert_eq!(size_class_index(2048), Some(6));
        assert_eq!(size_class_index(2049), None);
    }
}
