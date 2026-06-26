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
//! ## The fault fill (slice 8 Part 2b)
//!
//! [`fault_in_page`](FileObject::fault_in_page) is the demand-fault entry: on a miss
//! it [`reserve`](FileObject::reserve)s a frame, starts the object's [`Producer`]
//! fill (asynchronous), and **parks the faulting thread** on the fill's
//! `PendingOperation` until it completes — called from the `#PF` handler *after* the
//! address-space lock is released, so it blocks without holding any AS/cache lock.
//! Slice 8 ships only the self-test [`Producer::Stub`]; the real `FsServer` producer
//! (an IPC range-read) is Part 3.
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

use core::ptr::NonNull;

use crate::dpc::Dpc;
use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox, KVec, SpinLock};
use crate::mm::{PAGE_SIZE, PhysAddr, heap};
use crate::object::header::KObjectHeader;
use crate::object::{ObjectRef, PendingOperation};

/// How a [`FileObject`] **fills** a cache page on a fault — the producer behind the
/// page cache's fill seam. Slice 8 ships only the self-test [`Stub`](Producer::Stub);
/// the real fs-server producer (`FsServer { endpoint, suffix }`, an IPC range-read)
/// lands in Part 3.
pub enum Producer {
    /// Self-test producer: fills page `i` with the byte `base + i`, **asynchronously**
    /// — it enqueues a DPC (drained at the next interrupt-dispatch tail) so the
    /// faulting thread genuinely parks and resumes. Backs the page-cache fault
    /// self-test fixture; no fs-server / IPC.
    Stub { base: u8 },
}

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
    /// How a missing page is filled on fault (the producer behind the fill seam).
    producer: Producer,
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

    /// Allocate an empty `FileObject` for a file of `size` bytes whose pages are
    /// filled on fault by `producer`. Refcount one; no frames are allocated here —
    /// pages are reserved + filled lazily on fault.
    pub fn try_new(size: usize, producer: Producer) -> Result<KBox<Self>, AllocError> {
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::FileObject),
            magic: Self::MAGIC,
            size,
            producer,
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

    /// Drop a still-`Loading` page (freeing its frame) — the rollback for a fill that
    /// could not be *started* (e.g. the producer's request allocation failed), so a
    /// later fault re-reserves it cleanly. A no-op if the page is absent or already
    /// `Ready`.
    pub fn cancel_reserve(&self, index: usize) {
        let mut g = self.inner.lock();
        if let Some(pos) = g
            .pages
            .iter()
            .position(|p| p.index == index && p.state == PageState::Loading)
        {
            let p = g.pages.remove(pos);
            heap::buddy_free(p.frame, 0);
        }
    }

    /// **Fault page `index` in**, blocking until it is resident: a cache hit returns
    /// at once; a miss reserves a frame, starts the producer fill (asynchronous), and
    /// **parks the calling thread** on the fill's `PendingOperation` until it
    /// completes, then returns the frame. `None` on a frame/PO allocation failure or
    /// a failed fill. Called from the page-fault handler **after** the address-space
    /// lock is released (`AddressSpace::file_backing` → here → `map_file_page`), so
    /// blocking here parks the faulting thread without holding any AS/cache lock.
    /// `file_obj` is the caller's reference to *this* object (so the deferred fill can
    /// keep it alive); `debug_assert`ed to be a `FileObject`.
    ///
    /// Single-CPU note: a concurrent fault on the *same* page (`Loading`) cannot
    /// arise in the slice-8 milestone (one faulter per object); it is handled
    /// conservatively by yielding until the in-flight fill completes. A proper
    /// "wait on the in-flight fill's PO" is deferred (see `deferred-decisions.md`).
    pub fn fault_in_page(file_obj: &ObjectRef, index: usize) -> Option<PhysAddr> {
        debug_assert_eq!(file_obj.object_type(), KObjectType::FileObject);
        // SAFETY: `file_obj` pins a live `FileObject` (header at offset 0).
        let fo: &FileObject = unsafe { &*(file_obj.as_ptr() as *const FileObject) };
        loop {
            match fo.reserve(index) {
                Reserve::Ready(frame) => return Some(frame),
                Reserve::New(frame) => {
                    // The fill's PendingOperation: this thread blocks on it; the
                    // producer completes it when the page's bytes have arrived.
                    let po = match PendingOperation::try_new() {
                        // SAFETY: adopt the single creation reference.
                        Ok(p) => unsafe {
                            ObjectRef::from_raw(
                                KBox::into_raw(p).as_ptr() as *mut (),
                                KObjectType::PendingOperation,
                            )
                        },
                        Err(_) => {
                            fo.cancel_reserve(index);
                            return None;
                        }
                    };
                    if !fo.start_fill(file_obj, index, frame, &po) {
                        // Could not start the fill (allocation failure); roll the
                        // reserved page back so a retry is clean.
                        fo.cancel_reserve(index);
                        return None;
                    }
                    if !block_on_po(&po) {
                        return None; // fill reported failure / could not register
                    }
                    // Loop: the page is now `Ready` → return its frame.
                }
                Reserve::Loading(_) => {
                    // Another fault is filling this page; let it (and its DPC) run.
                    crate::sched::yield_now();
                }
                Reserve::Oom => return None,
            }
        }
    }

    /// Start an asynchronous fill of `frame` for page `index`, completing `po` when
    /// done (the producer also marks the page `Ready`). Dispatches on the object's
    /// [`Producer`]. `file_obj` is this object's reference (the deferred fill clones
    /// it to stay alive). Returns `false` if the fill could not be started. The stub
    /// producer enqueues a DPC; the fs-server producer (Part 3) sends a range-read.
    fn start_fill(
        &self,
        file_obj: &ObjectRef,
        index: usize,
        frame: PhysAddr,
        po: &ObjectRef,
    ) -> bool {
        match self.producer {
            Producer::Stub { base } => stub_start_fill(file_obj, index, frame, po, base),
        }
    }
}

/// Park the current thread on the fill `po` until it completes; `true` iff the fill
/// reported success (`status == 0`). Uses the scheduler's `wait_on` primitive — the
/// fast path returns at once if `po` already completed (no lost wakeup). `now = 0`
/// is fine: a no-deadline (`u64::MAX`) PO wait uses it only for the already-signalled
/// check, which a `PendingOperation` answers from its flag.
fn block_on_po(po: &ObjectRef) -> bool {
    match crate::sched::wait_on(&[po.as_ptr() as usize], u64::MAX, 0) {
        crate::sched::WaitResult::Signaled(_) => crate::sched::pending_op_completion(po.as_ptr()).0 == 0,
        // OutOfMemory (waiter registration failed); TimedOut cannot occur (no deadline).
        _ => false,
    }
}

/// A self-test fill in flight: the DPC + everything it needs, heap-boxed so its
/// `Dpc` has a stable address. The DPC writes `fill_byte` into `frame`, marks page
/// `index` of `file_obj` `Ready`, completes `po`, and frees this box (releasing its
/// `file_obj` / `po` references).
struct StubFillBox {
    dpc: Dpc,
    file_obj: ObjectRef,
    po: ObjectRef,
    frame: PhysAddr,
    index: usize,
    fill_byte: u8,
}

/// Start a stub fill (page `index` ← the byte `base + index`) by enqueuing a DPC,
/// drained at the next interrupt-dispatch tail — so the faulting thread genuinely
/// parks and resumes. `false` on box-allocation failure. `file_obj` is the object's
/// reference; the box clones it (and `po`) so they outlive the deferred fill.
fn stub_start_fill(
    file_obj: &ObjectRef,
    index: usize,
    frame: PhysAddr,
    po: &ObjectRef,
    base: u8,
) -> bool {
    let bx = match KBox::try_new(StubFillBox {
        dpc: Dpc::new(stub_fill_dpc, core::ptr::null_mut()),
        file_obj: file_obj.clone(),
        po: po.clone(),
        frame,
        index,
        fill_byte: base.wrapping_add(index as u8),
    }) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let bx_ptr = KBox::into_raw(bx).as_ptr();
    // SAFETY: `bx_ptr` is a freshly placed, uniquely-owned box; point its DPC at
    // itself (now that it has a stable address) and queue it.
    unsafe {
        (*bx_ptr).dpc = Dpc::new(stub_fill_dpc, bx_ptr as *mut ());
        crate::dpc::enqueue(&(*bx_ptr).dpc);
    }
    true
}

/// DPC: write the stub byte into the cache frame, mark the page `Ready`, complete
/// the fill PO (waking the parked faulter), and free the box.
fn stub_fill_dpc(ctx: *mut ()) {
    let bx_ptr = ctx as *mut StubFillBox;
    // SAFETY: `ctx` is the `StubFillBox` we placed in `stub_start_fill`; reclaim it.
    let bx = unsafe { KBox::from_raw(NonNull::new_unchecked(bx_ptr)) };
    // SAFETY: `frame` is a live, HHDM-reachable cache frame owned by `file_obj`
    // (which `bx` keeps alive); fill the whole page with the stub byte.
    unsafe {
        core::ptr::write_bytes(
            (bx.frame.as_u64() + heap::hhdm_offset()) as *mut u8,
            bx.fill_byte,
            PAGE_SIZE,
        );
    }
    // SAFETY: `bx.file_obj` pins a live `FileObject`.
    let fo: &FileObject = unsafe { &*(bx.file_obj.as_ptr() as *const FileObject) };
    fo.mark_ready(bx.index);
    crate::sched::complete_pending_op(bx.po.as_ptr(), 0, 0);
    // `bx` drops here: frees the box, releasing the `file_obj` + `po` references.
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
    use crate::object::header::test_probe;

    /// A `FileObject` of `size` bytes with a (here-unused) stub producer — the cache
    /// tests drive `reserve`/`mark_ready` directly, never the fault path.
    fn fobj(size: usize) -> KBox<FileObject> {
        FileObject::try_new(size, Producer::Stub { base: 0 }).unwrap()
    }

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
        assert_eq!(fobj(0).npages(), 0);
        assert_eq!(fobj(1).npages(), 1);
        let f = fobj(2 * PAGE_SIZE + 1);
        assert_eq!(f.size(), 2 * PAGE_SIZE + 1);
        assert_eq!(f.npages(), 3);
        assert!(f.magic_ok());
    }

    #[test]
    fn reserve_then_mark_ready_lifecycle() {
        init_global_heap();
        let f = fobj(4 * PAGE_SIZE);

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
        let f = fobj(8 * PAGE_SIZE);
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
        let f = fobj(PAGE_SIZE);
        f.mark_ready(0); // not present — no panic
        assert_eq!(f.lookup(0), None);
    }

    #[test]
    fn drop_frees_cached_frames_no_leak() {
        // Build a FileObject, resident-fault several pages, drop — repeated enough
        // that a frame leak would exhaust the 16 MiB test heap.
        init_global_heap();
        for _ in 0..64 {
            let f = fobj(16 * PAGE_SIZE);
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
        let f = fobj(PAGE_SIZE);
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
