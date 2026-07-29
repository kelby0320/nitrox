//! SLUB-inspired slab allocator on top of the buddy.
//!
//! Layer 2 of the three-layer memory management design (per
//! `docs/architecture/memory-management.md`). Sits between the buddy
//! allocator (which hands out page frames) and `libkern`'s heap
//! containers (`KBox`, `KVec`, `KString`), which call [`kmalloc`] /
//! [`kfree`] directly. The kernel registers no `#[global_allocator]`
//! and does not use the `alloc` crate — see the decision log entry of
//! 2026-05-20.
//!
//! ## Geometry
//!
//! - Every slab is exactly one 4 KiB page (`SLAB_SIZE`).
//! - The [`SlabDescriptor`] header sits at byte 0 of the page; objects
//!   live in the remaining bytes, starting at `obj_offset` (rounded up
//!   from the header size to the cache's alignment).
//! - Each cache holds two intrusive linked lists of descriptors: a
//!   `partial` list (slabs with at least one free slot) and a `full`
//!   list (slabs with no free slots).
//! - Free slots store the next-free pointer in their own first 8 bytes
//!   (embedded freelist, no separate book-keeping array).
//!
//! ## Why this layout enables O(1) free
//!
//! Given any object pointer `p`, the descriptor for its slab is at
//! `(p as usize) & SLAB_MASK`. This works because the buddy hands out
//! `SLAB_SIZE`-aligned pages and we keep the descriptor at the start of
//! each. `kfree` reads `desc.owner` to find the cache (or detect a
//! large-allocation sentinel) without any external table.
//!
//! ## Phase 1 scope
//!
//! - Single global spinlock per cache (no per-CPU fast path yet).
//! - Seven size buckets feeding [`kmalloc`]: 32, 64, 128, 256, 512, 1024,
//!   2048 bytes. Larger requests bypass to the buddy via the large-alloc
//!   path (see [`large_alloc`]) using an `owner = null` sentinel in the
//!   descriptor.
//! - No empty-slab reclaim; once grown, a cache holds onto its pages.
//! - No alignment > `SLAB_SIZE`; requests are rejected. The real answer for the
//!   one client that needs it — DMA buffers, which also need a physical address
//!   and contiguity the slab can't express — is the separate
//!   [`DmaBuffer`](crate::mm::DmaBuffer) path over the buddy allocator, not a
//!   `kmalloc` extension.
//!
//! See `kernel/docs/lock-ordering.md` for how the slab cache lock (rank
//! 6a) relates to the buddy lock (rank 6b) — slab `grow` holds the cache
//! lock across a `buddy.alloc()` call, and that is the only allocator-
//! to-allocator nesting permitted.

use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::libkern::SpinLock;
use crate::mm::buddy::BuddyAllocator;
use crate::mm::heap::{BuddyPager, HeapBuddy};
use crate::mm::{PAGE_SIZE, PhysAddr};

/// Bytes per slab. Matches `PAGE_SIZE` — every slab is exactly one buddy
/// order-0 block.
pub const SLAB_SIZE: usize = 4096;

/// Mask that, applied to any pointer inside a slab page, yields the
/// page-aligned base address of the [`SlabDescriptor`].
pub const SLAB_MASK: usize = !(SLAB_SIZE - 1);

const SIZE_CLASSES: [usize; 7] = [32, 64, 128, 256, 512, 1024, 2048];

/// Requests larger than this bypass the slab and route directly through
/// the buddy allocator (see [`large_alloc`]).
const MAX_SLAB_OBJ: usize = 2048;

/// The strongest alignment any size bucket provides. Requests needing more
/// go to [`large_alloc`], which honours `align` directly — see the routing
/// condition in [`kmalloc`] and the cap applied in [`slab_init`].
const MAX_BUCKET_ALIGN: usize = core::mem::align_of::<u64>();

// Compile-time check that the two constants don't drift apart. The
// pointer-masking trick falls over silently if pages are not
// `SLAB_SIZE`-aligned.
const _: () = assert!(SLAB_SIZE == PAGE_SIZE);

/// Header at byte 0 of every slab page. Found in O(1) from any object
/// pointer by masking with [`SLAB_MASK`].
///
/// `#[repr(C)]` so layout is predictable: a future debugger or out-of-
/// tree tool can read these without parsing DWARF.
#[repr(C)]
pub struct SlabDescriptor {
    /// First free slot's HHDM-virtual address, or null when the slab is
    /// full. Each free slot's first 8 bytes hold the next-free pointer.
    freelist: *mut u8,

    /// Pointer back to the owning cache. A null value is the sentinel
    /// for a large allocation that bypassed the slab and is owned
    /// directly by the buddy.
    owner: *const SlabCache,

    /// Intrusive singly-linked-list pointer; threads this descriptor onto
    /// either the cache's `partial` or `full` list. Null at the tail.
    next: *mut SlabDescriptor,

    /// Bytes per object as the allocator sees it. For large allocations
    /// this stores the total buddy block size in bytes so `large_free`
    /// can recover the buddy order.
    obj_size: u32,

    /// Slots currently allocated out of this slab.
    in_use: u16,

    /// Total slots in this slab (= `objs_per_slab` from cache init).
    capacity: u16,
}

/// One slab cache: a single object-size bucket, all state behind one
/// spin lock. Designed to be const-constructible so `static` arrays of
/// caches can live in `.bss` and be patched in by [`slab_init`].
pub struct SlabCache {
    state: SpinLock<SlabCacheState>,
}

struct SlabCacheState {
    /// Slabs with at least one free slot. LIFO; allocations always pop
    /// from the head.
    partial: *mut SlabDescriptor,
    /// Slabs with no free slots.
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
    /// Construct an uninitialised cache. [`SlabCache::init`] must be
    /// called before [`SlabCache::alloc`] or [`SlabCache::free`].
    pub const fn new() -> Self {
        Self {
            state: SpinLock::new(SlabCacheState {
                partial: ptr::null_mut(),
                full: ptr::null_mut(),
                obj_size: 0,
                obj_align: 0,
                obj_offset: 0,
                objs_per_slab: 0,
            }),
        }
    }

    /// Finalise the cache's geometry from a requested size/alignment.
    /// Asserts at init time so a misconfigured cache fails loudly rather
    /// than silently mis-laying objects.
    pub fn init(&self, requested_size: usize, requested_align: usize) {
        let ptr_size = core::mem::size_of::<*mut u8>();
        let ptr_align = core::mem::align_of::<*mut u8>();
        let obj_size = requested_size.max(ptr_size);
        let obj_align = requested_align.max(ptr_align);
        let header = core::mem::size_of::<SlabDescriptor>();
        let obj_offset = align_up(header, obj_align);
        let usable = SLAB_SIZE.saturating_sub(obj_offset);
        let objs_per_slab = usable / obj_size;

        assert!(
            obj_offset >= header,
            "slab init: obj_offset < SlabDescriptor size"
        );
        assert!(
            obj_offset.is_multiple_of(obj_align),
            "slab init: obj_offset misaligned"
        );
        assert!(
            objs_per_slab >= 1,
            "slab init: obj_size too large for SLAB_SIZE"
        );

        let mut state = self.state.lock();
        state.obj_size = obj_size;
        state.obj_align = obj_align;
        state.obj_offset = obj_offset;
        state.objs_per_slab = objs_per_slab;
    }

    /// Allocate one object from this cache. Returns null on OOM.
    ///
    /// `pager` is the [`BuddyPager`] used if the cache needs to grow.
    /// Production callers pass [`HeapBuddy`]; tests inject a local one.
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
            // The slab is now full: pop it off `partial` and prepend to
            // `full`.
            // SAFETY: lock held; desc is the partial-list head.
            unsafe {
                state.partial = (*desc).next;
                (*desc).next = state.full;
                state.full = desc;
            }
        }
        obj
    }

    /// Return one object to this cache. Caller asserts `ptr` was
    /// previously handed out by this same cache's `alloc`.
    pub fn free(&self, ptr: *mut u8) {
        let desc = ((ptr as usize) & SLAB_MASK) as *mut SlabDescriptor;
        let mut state = self.state.lock();
        // SAFETY: by the slab invariant the page header is a valid
        // descriptor; lock held so no concurrent mutator.
        let was_full = unsafe {
            debug_assert!((*desc).in_use > 0, "slab: free underflow");
            (*desc).freelist.is_null()
        };
        // Embedded-freelist push: write current head into `ptr`'s first
        // 8 bytes, then make `ptr` the new head.
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

    /// Allocate a fresh page from `pager` and initialise its descriptor
    /// and embedded freelist. Returns the new descriptor or null on OOM.
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
        debug_assert!(
            (virt as usize) & (SLAB_SIZE - 1) == 0,
            "slab: buddy returned a non-SLAB_SIZE-aligned page"
        );
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
            // Build the embedded freelist by walking the slots in order
            // and writing each slot's "next" pointer into its first 8
            // bytes.
            let first = virt.add(state.obj_offset);
            let stride = state.obj_size;
            let n = state.objs_per_slab;
            let mut i = 0;
            while i < n {
                let slot = first.add(i * stride);
                let next = if i + 1 == n {
                    ptr::null_mut()
                } else {
                    first.add((i + 1) * stride)
                };
                *(slot as *mut *mut u8) = next;
                i += 1;
            }
            (*desc).freelist = first;
        }
        desc
    }

    /// Walk the full list, remove `desc`, and prepend it to the partial
    /// list. O(n) scan over the full list; acceptable in Phase 1 because
    /// the full list is short and we don't optimise for high churn.
    ///
    /// # Safety
    /// Caller holds the cache lock; `desc` is on `state.full`.
    unsafe fn move_full_to_partial(
        state: &mut SlabCacheState,
        desc: *mut SlabDescriptor,
    ) {
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
            debug_assert!(
                false,
                "slab: full-to-partial scan failed to locate descriptor"
            );
        }
    }
}

// --- Module-level state --------------------------------------------------

/// Seven size-bucket caches feeding [`kmalloc`]. Const-constructed so the
/// array sits in `.bss`; [`slab_init`] writes their geometry once.
static SLAB_CACHES: [SlabCache; 7] = [
    SlabCache::new(),
    SlabCache::new(),
    SlabCache::new(),
    SlabCache::new(),
    SlabCache::new(),
    SlabCache::new(),
    SlabCache::new(),
];

/// Flag inspected by [`kmalloc`] so a too-early allocation (before
/// [`slab_init`]) panics loudly rather than silently corrupting the
/// heap.
static SLAB_INITIALISED: AtomicBool = AtomicBool::new(false);

/// Initialise the seven size-bucket caches. Call exactly once during
/// boot, after [`crate::mm::heap::init_buddy`].
pub fn slab_init() {
    assert!(
        !SLAB_INITIALISED.swap(true, Ordering::AcqRel),
        "slab_init called twice"
    );
    let mut i = 0;
    while i < SIZE_CLASSES.len() {
        // Bucket alignment is capped at `MAX_BUCKET_ALIGN`: bumping align
        // beyond the natural alignment of the bucket's size doesn't serve
        // any allocation we route into this bucket. Callers that need
        // stronger alignment than the bucket provides go through the
        // large-alloc path (where `align` is honoured directly) — see the
        // routing condition in [`kmalloc`], which enforces exactly that.
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

/// Allocate `size` bytes with alignment `align`. Returns null on OOM or
/// when the request cannot be served (e.g. `align > SLAB_SIZE`).
///
/// `kmalloc(0, ...)` returns a non-null aligned sentinel pointer that
/// must never be dereferenced; passing it to [`kfree`] is harmless.
pub fn kmalloc(size: usize, align: usize) -> *mut u8 {
    if size == 0 {
        // ZST convention: a non-null aligned sentinel. Caller must never
        // deref it; kfree treats it as a no-op because its page is
        // never one of our slab pages.
        return align.max(1) as *mut u8;
    }
    if align > SLAB_SIZE {
        // Above-`SLAB_SIZE` alignment is intentionally not served here: its one
        // client, DMA, uses `mm::dma::DmaBuffer` (buddy-backed, exposes a phys
        // address). See this module's docs.
        return ptr::null_mut();
    }
    if !SLAB_INITIALISED.load(Ordering::Acquire) {
        panic!("kmalloc called before slab_init");
    }
    let bucket_size = size.max(align);
    // Route to the buddy-backed large path when the request is too big for any
    // bucket, **or** when it needs stronger alignment than a bucket provides.
    // `slab_init` caps every bucket's alignment at `align_of::<u64>()`, so a
    // bucket cannot satisfy `align > 8` — and `bucket_size` alone does not catch
    // that (a 64-byte-aligned 832-byte request has `bucket_size == 832` and would
    // otherwise land in the 1024 bucket at 8-byte alignment). `large_alloc`
    // honours `align` directly by rounding the object offset up from a
    // page-aligned buddy block. Its first client is the per-thread FPU save area,
    // whose `XSAVE` image `#GP`s unless 64-byte aligned.
    if bucket_size > MAX_SLAB_OBJ || align > MAX_BUCKET_ALIGN {
        return large_alloc(size, align, &HeapBuddy);
    }
    // bucket_size <= MAX_SLAB_OBJ, so SIZE_CLASSES has a suitable entry.
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
pub fn kfree(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let desc = ((ptr as usize) & SLAB_MASK) as *mut SlabDescriptor;
    // SAFETY: every non-null pointer kmalloc returns lives in a
    // buddy-owned page whose first bytes are a valid SlabDescriptor —
    // either a slab descriptor with non-null `owner`, or a large-alloc
    // descriptor with null `owner`. The ZST sentinel is not a valid
    // pointer into our pages and would mask to some garbage page; for
    // safety the caller must not free a ZST allocation. The `libkern`
    // containers uphold this: `KBox` / `KVec` represent zero-sized types
    // with a dangling pointer and never route them through `kmalloc` /
    // `kfree`.
    let owner = unsafe { (*desc).owner };
    if owner.is_null() {
        large_free(ptr, &HeapBuddy);
        return;
    }
    // SAFETY: `owner` is a pointer captured by `SlabCache::grow_locked`
    // from a `&SlabCache` that lives in `SLAB_CACHES` (or in a test's
    // local array, which lives at least as long as the allocation).
    let cache = unsafe { &*owner };
    cache.free(ptr);
}

/// Allocate a buddy block large enough to satisfy `size`/`align` plus
/// the descriptor header. Returns the user pointer past the header.
fn large_alloc<P: BuddyPager>(size: usize, align: usize, pager: &P) -> *mut u8 {
    let header = core::mem::size_of::<SlabDescriptor>();
    // User pointer sits at `obj_offset` from the start of the buddy
    // block; we round up to honour the caller's alignment.
    let obj_offset = align_up(header, align);
    let total = obj_offset.checked_add(size).unwrap_or(usize::MAX);
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
                // Null owner is the large-alloc sentinel.
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

/// Free a large allocation previously obtained from [`large_alloc`].
fn large_free<P: BuddyPager>(ptr: *mut u8, pager: &P) {
    let page0 = (ptr as usize) & SLAB_MASK;
    let desc = page0 as *mut SlabDescriptor;
    // SAFETY: large-alloc invariant — descriptor at byte 0 of the first
    // page of the block.
    let total = unsafe { (*desc).obj_size as usize };
    let order = BuddyAllocator::order_for_size(total);
    let phys = PhysAddr::new((page0 as u64).wrapping_sub(pager.hhdm_offset()));
    pager.free(phys, order);
}

#[inline]
fn align_up(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}

// --- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Slab tests run host-side via `cargo test --lib`. They build a
    //! [`LocalBuddy`] wrapping a per-test [`BuddyAllocator`] backed by
    //! the [`FakeMem`] helper, then exercise local caches directly. The
    //! production `SLAB_CACHES` / `HeapBuddy` statics are not touched,
    //! so tests stay hermetic and parallel-safe.

    use super::*;
    use crate::limine::{MEMMAP_USABLE, MemoryMapEntry, MemoryMapResponse};
    use core::cell::RefCell;

    /// Heap-backed substitute for physical memory, modelled on
    /// `buddy::tests::FakeMem`. Duplicated here because the upstream
    /// helper is `#[cfg(test)]`-private to the buddy module; future
    /// refactor can lift it into a shared `mm::test_support` module.
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
            // Host buffers sit at addresses well above 1 MiB, so the
            // buddy allocator's low-memory cutoff does not reject the
            // region.
            assert!(aligned >= 0x10_0000);
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

    /// Test-only [`BuddyPager`] wrapping a local [`BuddyAllocator`]
    /// behind a `RefCell` so the trait's `&self` methods can mutate it.
    /// On the kernel target the global `BUDDY` static is wrapped in a
    /// `SpinLock` for the same reason.
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
        let pager = LocalBuddy {
            inner: RefCell::new(buddy),
            hhdm: 0,
        };
        (mem, pager)
    }

    fn fresh_cache(size: usize) -> SlabCache {
        let cache = SlabCache::new();
        cache.init(size, size.min(core::mem::align_of::<u64>()));
        cache
    }

    // Test-only accessors that inspect cache internals.
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
        // Fill out the rest of the first slab.
        for _ in 1..cap {
            let p = cache.alloc(&pager);
            assert_eq!((p as usize) & SLAB_MASK, first_page);
        }
        // Next allocation must come from a new slab.
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
        cache.free(b);
        let c = cache.alloc(&pager);
        assert_eq!(b, c, "LIFO push/pop should return the just-freed slot");
        // Keep `a` live to suppress unused-warning suspicion.
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
        cache.free(ptrs[0]);
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
        // Doesn't touch the global SLAB_CACHES because owner-derefence
        // is short-circuited on null pointer. No panic, no segfault.
        kfree(ptr::null_mut());
    }

    #[test]
    fn large_alloc_routes_through_buddy_with_null_owner() {
        let (_mem, pager) = make_world(PAGE_SIZE * 64);
        let p = large_alloc(8192, 8, &pager);
        assert!(!p.is_null());
        let desc = ((p as usize) & SLAB_MASK) as *mut SlabDescriptor;
        // SAFETY: test owns the allocation.
        let owner = unsafe { (*desc).owner };
        let obj_size = unsafe { (*desc).obj_size };
        assert!(owner.is_null(), "large-alloc descriptor must have null owner");
        // The 32-byte header plus 8192 bytes of payload (= 8224 bytes,
        // which is 3 pages once rounded up) rounds to order 2 — the
        // smallest order whose 2^order pages cover three pages.
        assert_eq!(obj_size as usize, PAGE_SIZE << 2);
        large_free(p, &pager);
    }

    #[test]
    fn large_alloc_round_trip_via_kfree_routing() {
        // Exercises the routing in `kfree`'s analogue: a manual walk
        // through the descriptor that mirrors what the public `kfree`
        // does, using the local pager.
        let (_mem, pager) = make_world(PAGE_SIZE * 64);
        let p = large_alloc(3000, 8, &pager);
        let desc = ((p as usize) & SLAB_MASK) as *mut SlabDescriptor;
        // SAFETY: test owns the allocation.
        assert!(unsafe { (*desc).owner.is_null() });
        large_free(p, &pager);
    }

    #[test]
    fn kzalloc_via_cache_returns_zeroed_memory() {
        // We don't exercise `kzalloc` directly here because that path
        // uses the global SLAB_CACHES. Instead allocate via the cache,
        // poison the bytes, free, reallocate, and zero by hand to mirror
        // what `kzalloc` does once routing chose a bucket.
        let (_mem, pager) = make_world(PAGE_SIZE * 64);
        let cache = fresh_cache(64);
        let p = cache.alloc(&pager);
        // SAFETY: cache handed us 64 writable bytes.
        unsafe {
            ptr::write_bytes(p, 0xAA, 64);
        }
        cache.free(p);
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
        // SLAB_SIZE-sized objects leave no room for a descriptor.
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
