//! The [`FileObject`] kernel object — a mapped file's content, paged in on demand.
//!
//! Where a [`MemoryObject`](crate::object::MemoryObject) is anonymous/shared RAM the
//! kernel commits **eagerly** (every frame at creation), a `FileObject` is a file's
//! content **paged in on demand** from a producer (slice 7's fs-server). It owns a
//! sparse **page cache**: a per-page-index table of physical frames, each allocated
//! and filled the first time that page is faulted, and freed when the object's last
//! reference goes away. `sys_memory_map` of a `FileObject` builds a lazy
//! [`MappingKind::FileBacked`](crate::mm::vmm::MappingKind) VMA (no eager PTEs); the
//! `#PF` handler faults pages in through this cache. See
//! `docs/architecture/memory-management.md` and the decision log (2026-06-25 — slice
//! 8 fill model + scope).
//!
//! ## Part 1 scope
//!
//! This file builds the object + its cache **data structure and lifecycle** only —
//! the producer reference (the fs-server endpoint + path suffix used to *fill* a
//! page) and the async fault path land in their own Parts (3 / 2), where they are
//! first consumed. So `FileObject::try_new` takes only the file `size` for now; the
//! cache is exercised through [`reserve`](FileObject::reserve) /
//! [`mark_ready`](FileObject::mark_ready) / [`lookup`](FileObject::lookup).
//!
//! ## Mutation discipline
//!
//! The cache is shared across every mapping of the object (potentially in several
//! address spaces), so — unlike a per-AS structure — it carries its **own**
//! rank-4 [`SpinLock`] (the `AddressSpace`/`Namespace` model), not the `SCHED` cell
//! pattern. The fault path acquires it *after* dropping the address-space lock (both
//! rank 4, never nested). It may allocate a frame under the lock (rank 4 → rank-6
//! buddy is a legal order) but never blocks under it (the fault parks on the fill
//! `PendingOperation`, outside the lock).

use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox, KVec, SpinLock};
use crate::mm::{PAGE_SIZE, PhysAddr, heap};
use crate::object::header::KObjectHeader;

/// Fill state of a cached page.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PageState {
    /// A frame is allocated (zeroed) and a fill is in flight — **not** yet safe to
    /// map (its bytes are still arriving from the producer).
    Loading,
    /// The frame holds the file's bytes (the last page's tail past the file size is
    /// zero-padding) and may be mapped into a client.
    Ready,
}

/// One cached page: its page **index** (byte offset ÷ page size), the owning frame,
/// and its fill state.
struct CachePage {
    index: usize,
    frame: PhysAddr,
    state: PageState,
}

/// Outcome of [`FileObject::reserve`] — what the fault path should do for a page.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Reserve {
    /// The page is cached and [`Ready`](PageState::Ready): map `frame` directly (a
    /// cache hit).
    Ready(PhysAddr),
    /// A fill is already in flight (an earlier fault reserved it): the caller waits
    /// for it rather than starting a second fill. `frame` is the loading frame.
    Loading(PhysAddr),
    /// A fresh, **zeroed** frame was reserved (state `Loading`): the caller must fill
    /// it (copy the file bytes in, leaving any tail zero) and then call
    /// [`mark_ready`](FileObject::mark_ready).
    New(PhysAddr),
    /// No frame (or cache slot) could be allocated.
    Oom,
}

/// A mapped file's content, backed by a demand-filled page cache.
///
/// `#[repr(C)]` with [`KObjectHeader`] first — see [`crate::object::header`].
#[repr(C)]
pub struct FileObject {
    header: KObjectHeader,
    /// Self-check sentinel; a live object always reads [`FileObject::MAGIC`].
    magic: u64,
    /// Exact file size in bytes. The last page's tail past this is zero-padding; the
    /// mapped range (hence the faultable pages) is bounded by it.
    size: usize,
    inner: SpinLock<Inner>,
}

struct Inner {
    /// Cached pages. Sparse (only faulted pages are present) and unsorted; lookups
    /// are a linear scan — O(n) in the number of *resident* pages, which is fine for
    /// slice-8 file sizes (a sorted index / tree is a later optimization if profiles
    /// demand it). Each entry **owns** its frame (freed in [`FileObject::drop`]).
    pages: KVec<CachePage>,
}

impl FileObject {
    /// Sentinel written into [`FileObject::magic`] at construction.
    pub const MAGIC: u64 = 0x46_69_6c_65_4f_62_6a_21; // "FileObj!"

    /// Allocate an empty `FileObject` for a file of `size` bytes, refcount one. No
    /// frames are allocated here — pages are reserved + filled lazily on fault.
    pub fn try_new(size: usize) -> Result<KBox<Self>, AllocError> {
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::FileObject),
            magic: Self::MAGIC,
            size,
            inner: SpinLock::new(Inner { pages: KVec::new() }),
        })
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    /// Exact file size in bytes.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Number of pages the file spans (`⌈size / PAGE⌉`; `0` for an empty file).
    pub fn npages(&self) -> usize {
        self.size.div_ceil(PAGE_SIZE)
    }

    /// The number of pages currently resident in the cache. Test/observability only.
    #[cfg(test)]
    pub(crate) fn resident_pages(&self) -> usize {
        self.inner.lock().pages.len()
    }

    /// Look up page `index`: its frame + [`PageState`] if cached, else `None`.
    pub fn lookup(&self, index: usize) -> Option<(PhysAddr, PageState)> {
        let g = self.inner.lock();
        g.pages
            .iter()
            .find(|p| p.index == index)
            .map(|p| (p.frame, p.state))
    }

    /// Reserve page `index` for the fault path. A cache hit returns
    /// [`Reserve::Ready`]; an in-flight fill returns [`Reserve::Loading`]; a miss
    /// allocates + **zeroes** a frame, inserts it `Loading`, and returns
    /// [`Reserve::New`] (the caller fills it then calls
    /// [`mark_ready`](Self::mark_ready)). [`Reserve::Oom`] if no frame/slot is free.
    /// Zeroing the fresh frame guarantees a partial (tail) page's padding is zero.
    pub fn reserve(&self, index: usize) -> Reserve {
        let mut g = self.inner.lock();
        if let Some(p) = g.pages.iter().find(|p| p.index == index) {
            return match p.state {
                PageState::Ready => Reserve::Ready(p.frame),
                PageState::Loading => Reserve::Loading(p.frame),
            };
        }
        // Reserve the slot before allocating the frame, so a `try_push` after the
        // alloc cannot fail (and so we never leak a frame on a vector-growth OOM).
        if g.pages.try_reserve(1).is_err() {
            return Reserve::Oom;
        }
        let Some(frame) = heap::buddy_alloc(0) else {
            return Reserve::Oom;
        };
        // SAFETY: `frame` was just returned by the buddy, is unaliased, and is
        // HHDM-reachable; zeroing prevents leaking stale memory (and zero-pads a
        // partial tail page).
        unsafe {
            core::ptr::write_bytes((frame.as_u64() + heap::hhdm_offset()) as *mut u8, 0, PAGE_SIZE);
        }
        g.pages
            .try_push(CachePage { index, frame, state: PageState::Loading })
            .expect("slot reserved above");
        Reserve::New(frame)
    }

    /// Transition page `index` from `Loading` to `Ready` (after its fill wrote the
    /// frame). A no-op if the page is absent or already `Ready`.
    pub fn mark_ready(&self, index: usize) {
        let mut g = self.inner.lock();
        if let Some(p) = g.pages.iter_mut().find(|p| p.index == index) {
            p.state = PageState::Ready;
        }
    }
}

impl Drop for FileObject {
    /// Free every cached frame. Runs when the last reference releases (via
    /// `dispatch_destroy` dropping the owning `KBox`). Like
    /// [`MemoryObject`](crate::object::MemoryObject), the object holds raw
    /// `PhysAddr`s with no owning wrapper, so it frees them itself. No contention
    /// here (the last reference is dropping), and no lock is held on entry, so taking
    /// the cache lock is a single, ordered acquisition.
    fn drop(&mut self) {
        for p in self.inner.lock().pages.iter() {
            heap::buddy_free(p.frame, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;
    use crate::object::ObjectRef;
    use crate::object::header::test_probe;

    /// Read a byte from a cache frame through the HHDM (the test stands in for the
    /// fault path / producer that would write/read it).
    fn frame_byte(frame: PhysAddr, off: usize) -> u8 {
        // SAFETY: `frame` is a live, HHDM-reachable cache frame; read-only.
        unsafe { *((frame.as_u64() + heap::hhdm_offset()) as *const u8).add(off) }
    }
    fn write_frame_byte(frame: PhysAddr, off: usize, v: u8) {
        // SAFETY: `frame` is a live, HHDM-reachable, unaliased cache frame.
        unsafe { *((frame.as_u64() + heap::hhdm_offset()) as *mut u8).add(off) = v };
    }

    #[test]
    fn size_and_npages_round_up() {
        init_global_heap();
        assert_eq!(FileObject::try_new(0).unwrap().npages(), 0);
        assert_eq!(FileObject::try_new(1).unwrap().npages(), 1);
        let f = FileObject::try_new(2 * PAGE_SIZE + 1).unwrap();
        assert_eq!(f.size(), 2 * PAGE_SIZE + 1);
        assert_eq!(f.npages(), 3);
        assert!(f.magic_ok());
    }

    #[test]
    fn reserve_then_mark_ready_lifecycle() {
        init_global_heap();
        let f = FileObject::try_new(4 * PAGE_SIZE).unwrap();

        // A miss reserves a fresh, zeroed Loading frame.
        let frame = match f.reserve(2) {
            Reserve::New(frame) => frame,
            other => panic!("expected New, got {other:?}"),
        };
        assert_eq!(frame_byte(frame, 0), 0, "fresh frame must be zeroed");
        assert_eq!(frame_byte(frame, PAGE_SIZE - 1), 0);
        assert_eq!(f.resident_pages(), 1);
        assert_eq!(f.lookup(2), Some((frame, PageState::Loading)));

        // Reserving the same page again while loading returns the same frame, no
        // second allocation.
        assert_eq!(f.reserve(2), Reserve::Loading(frame));
        assert_eq!(f.resident_pages(), 1);

        // The producer fills the frame; mark it ready.
        write_frame_byte(frame, 0, 0xAB);
        f.mark_ready(2);
        assert_eq!(f.lookup(2), Some((frame, PageState::Ready)));
        assert_eq!(f.reserve(2), Reserve::Ready(frame));
        assert_eq!(frame_byte(frame, 0), 0xAB, "ready frame keeps its bytes");
    }

    #[test]
    fn distinct_pages_get_distinct_frames() {
        init_global_heap();
        let f = FileObject::try_new(8 * PAGE_SIZE).unwrap();
        let a = match f.reserve(0) { Reserve::New(fr) => fr, o => panic!("{o:?}") };
        let b = match f.reserve(5) { Reserve::New(fr) => fr, o => panic!("{o:?}") };
        let c = match f.reserve(3) { Reserve::New(fr) => fr, o => panic!("{o:?}") };
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
        assert_eq!(f.resident_pages(), 3);
        // Each looks up independently with its own frame.
        assert_eq!(f.lookup(0), Some((a, PageState::Loading)));
        assert_eq!(f.lookup(5), Some((b, PageState::Loading)));
        assert_eq!(f.lookup(3), Some((c, PageState::Loading)));
        assert_eq!(f.lookup(7), None);
    }

    #[test]
    fn mark_ready_absent_is_noop() {
        init_global_heap();
        let f = FileObject::try_new(PAGE_SIZE).unwrap();
        f.mark_ready(0); // not present — no panic
        assert_eq!(f.lookup(0), None);
    }

    #[test]
    fn drop_frees_cached_frames_no_leak() {
        // Build a FileObject, resident-fault several pages, drop — repeated enough
        // that a frame leak would exhaust the 16 MiB test heap.
        init_global_heap();
        for _ in 0..64 {
            let f = FileObject::try_new(16 * PAGE_SIZE).unwrap();
            for i in 0..8 {
                assert!(matches!(f.reserve(i), Reserve::New(_)));
            }
            assert_eq!(f.resident_pages(), 8);
            // Dropped here — its 8 cached frames must be freed.
        }
    }

    #[test]
    fn dispatch_destroy_runs_file_object_arm() {
        init_global_heap();
        test_probe::reset();
        let f = FileObject::try_new(PAGE_SIZE).unwrap();
        // Reserve a page so the destructor has a frame to free.
        let _ = f.reserve(0);
        let ptr = KBox::into_raw(f).as_ptr() as *mut ();
        // SAFETY: `ptr` carries the single creation reference.
        let r = unsafe { ObjectRef::from_raw(ptr, KObjectType::FileObject) };
        assert_eq!(test_probe::file_object_destroys(), 0);
        drop(r);
        assert_eq!(test_probe::file_object_destroys(), 1);
    }
}
