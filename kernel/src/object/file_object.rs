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
//! The real producer is [`Producer::FsServer`]: it sends a `File::ReadRange` over the
//! resource server's forwarding endpoint, and the reply (landed by the kernel's
//! reply-completion path) copies the bytes into the frame, marks it ready, and
//! completes the fill PO. [`Producer::Stub`] is a self-test producer for host tests.
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
//!
//! ## One object per file, and dirty ones kept (administration Part C.1)
//!
//! A Model A file whose server gave it an id is **cached in its registration**, and every
//! resolve of that file shares the object; a grow, create or truncate [`resize`]s it in place.
//! The cache indexes objects weakly: a clean object goes when its last user does.
//!
//! **A dirty object holds a reference to itself.** Dirty means mapped writable since the last
//! write-back that began with no writable mapping and saw none made. Until such a write-back,
//! the object stays alive and findable, so a writer that exits without syncing loses nothing:
//! `sys_ns_sync` enumerates the registration's cache and writes it back. Dirty is per object,
//! not per page, until `TODO(page-dirty-tracking)`: a write-back writes every resident page.
//!
//! [`resize`]: FileObject::resize

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::dpc::Dpc;
use crate::arch::timer::ArchTimer;
use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox, KString, KVec, SpinLock};
use crate::mm::{PAGE_SIZE, PhysAddr, heap};
use crate::object::header::KObjectHeader;
use crate::object::{ObjectRef, PendingOperation, StoredMsg};
use crate::libkern::lockrank::LockRank;

/// How a [`FileObject`] **fills** a cache page on a fault — the producer behind the
/// page cache's fill seam. The real producer is [`FsServer`](Producer::FsServer)
/// (an IPC `File::ReadRange` to the resource server); [`Stub`](Producer::Stub) is a
/// self-test producer retained for host tests.
pub enum Producer {
    /// Self-test producer: fills page `i` with the byte `base + i`, **asynchronously**
    /// — it enqueues a DPC (drained at the next interrupt-dispatch tail) so the
    /// faulting thread genuinely parks and resumes. No fs-server / IPC.
    Stub { base: u8 },
    /// Model B — a **non-block** filesystem: fill a page by sending a `File::ReadRange`
    /// over the resource server's forwarding endpoint (the same [`UserspaceServerReg`]
    /// the namespace binding uses) and copying the replied bytes into the cache frame.
    /// `reg` pins the registration (so it outlives the file); `suffix` names the
    /// file under the mount (the fill is stateless — re-sent on every range).
    ///
    /// [`UserspaceServerReg`]: crate::object::UserspaceServerReg
    FsServer { reg: ObjectRef, suffix: KString },
    /// Model A — a **block** filesystem: the kernel owns the file-data path. The file's run
    /// map (kept under the object's lock, since a size change replaces it) maps its blocks to
    /// device LBAs; a fault reads the page's block **zero-copy** straight from `device` into
    /// the cache frame via a block IRP. `device` pins the block `DeviceNode`. `block_size` is
    /// the filesystem block size. See `docs/architecture/filesystem-data-path.md`.
    ///
    /// `reg` and `file_id` are the file's identity: the registration it was resolved through
    /// and the id its server gave it. **A non-zero `file_id` means the object is in `reg`'s
    /// file cache** — one object per file, shared by every resolve of it (administration Part
    /// C.1) — and `Drop` takes it out again. They also name the file back to its server for
    /// the one thing the server cannot otherwise learn: that the file was **written**. An
    /// in-place, same-length overwrite never reaches the server (no resolve, no IPC), so the
    /// kernel sends `File::Touch` by id after a write-back. See [`FileObject::writeback`].
    FsServerBlocks { device: ObjectRef, block_size: u32, reg: ObjectRef, file_id: u64 },
}

/// One contiguous mapping from a file's blocks to the device (the kernel-side mirror of
/// the wire `BlockRun`, `docs/spec/rsproto-block-ops.md`). `device_lba` is a filesystem
/// block number (`0` = a hole → reads as zero).
#[derive(Copy, Clone)]
pub struct BlockRun {
    pub file_block: u64,
    pub device_lba: u64,
    pub length: u32,
    pub flags: u32,
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
    /// While a fault's fill is in flight, **its `PendingOperation`**, so a second faulter of
    /// the page blocks on it rather than spinning (administration Part C.1 made that
    /// ordinary: every process running one binary shares its image's object). Taken out when
    /// the page [settles](FileObject::settle). Never the last reference while here — the
    /// filler holds its own until it settles — so dropping it under the lock only decrements.
    fill: Option<ObjectRef>,
}

/// What the fault path does for a page ([`FileObject::reserve_fault`]).
enum Fault {
    /// Cached and ready: map this frame.
    Hit(PhysAddr),
    /// A fresh, zeroed frame reserved for this faulter to fill, completing this PO.
    Fill(PhysAddr, ObjectRef),
    /// Another faulter is filling the page: wait on its PO.
    Wait(ObjectRef),
    /// Loading with no fill to wait on — reserved outside the fault path, which only a host
    /// test does. Try again shortly.
    Busy,
    /// No frame, slot or PO could be allocated.
    Oom,
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
    /// mapped range (hence the faultable pages) is bounded by it. **Changes in place** for a
    /// cached Model A file — a grow, create or truncate of it updates the one object every
    /// resolve shares ([`resize`](Self::resize)) — so it is atomic.
    size: AtomicUsize,
    /// Writable mappings of this object that exist now. Raised by
    /// [`writable_mapped`](Self::writable_mapped) before the mapping is made, lowered when
    /// such a VMA drops — atomic, because a VMA drops wherever its address space does and
    /// must take no lock here.
    writable_maps: AtomicU32,
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
    /// Frames a truncation took out of `pages`: past the new end, so no fault finds them and
    /// no write-back writes them, but a mapping that faulted one in before may still point at
    /// it — so they are freed only with the object.
    retired: KVec<PhysAddr>,
    /// A Model A file's run map; empty for any other producer.
    runs: KVec<BlockRun>,
    /// **The object's own reference to itself, held while it is dirty** — from a writable
    /// mapping until a write-back that finds no writable mapping left. It is what keeps a
    /// file whose writer exited without syncing alive, in its registration's cache, until a
    /// sync or an unmount writes it; its file cache only indexes objects, weakly. Taken only
    /// for a cached object, the kind a sync can find.
    self_pin: Option<ObjectRef>,
    /// Raised by every writable mapping. A write-back that began with no writable mapping
    /// cleans the object only if this has not moved by its end ([`clean_mark`](FileObject::clean_mark)).
    map_gen: u64,
    /// **The file's server is freeing it** ([`forget`](FileObject::forget), administration Part
    /// C.1b). No device I/O of the object starts once this is set: a write-back stops, and a
    /// fill reads as a hole, since the blocks are about to be someone else's.
    dead: bool,
    /// Device IRPs of this object issued and not yet ended — write-backs and Model A fills.
    /// Raised under this lock, with `dead` checked, as each is issued; lowered by the thread
    /// that issued it once it completes ([`end_io`](FileObject::end_io)).
    io_in_flight: u32,
    /// The `File::Forget` answer waiting for `io_in_flight` to reach zero.
    forget_answer: Option<ObjectRef>,
}

/// What [`FileObject::begin_write`] says to do with one page of a write-back.
enum WriteStep {
    /// Write `frame` to this device block; the IRP is counted in flight.
    Go(PhysAddr, u64),
    /// Nothing to write for this page — gone from the cache, past the end, or over a hole.
    Skip,
    /// The file is being freed: write nothing more of it.
    Dead,
}

/// The outcome of [`FileObject::forget`].
pub enum Forgotten {
    /// Nothing of the file is in flight: the server may be answered now.
    Now,
    /// I/O of the file is in flight. The server is answered by completing this PO, which
    /// [`end_io`](FileObject::end_io) hands back when the last of it ends — the caller's own
    /// answer, or the one an earlier `Forget` of the same file already waits on.
    Later(ObjectRef),
}

/// Page-cache fill counters — the measurement behind the read-ahead decision (Slice B2).
///
/// Two atomic adds per fill, which is nothing beside the block read they bracket, so these
/// are always on rather than feature-gated: the same numbers are what a `/proc` page-cache
/// surface would report, and a decision to cluster fills should rest on measurement rather
/// than on the stale ~325 ms/page figure from the Model-B era (`deferred-decisions.md`).
/// What starting a fill actually did — the caller needs to tell a real device round trip
/// from a hole that completed synchronously, both to account for it and (for a hole) to
/// know no I/O latency was involved.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum FillStart {
    /// A producer request was issued; the faulter will park until it completes.
    Io,
    /// A Model A block IRP was issued and **counted in the object's I/O in flight**, so the
    /// faulter that issued it ends it ([`FileObject::finish_io`]) once it completes.
    DeviceIo,
    /// A hole / unmapped range: the zeroed frame is already correct and the PO was
    /// completed synchronously.
    Hole,
    /// The fill could not be started (allocation failure); the caller rolls back.
    Failed,
}

pub mod fill_stats {
    use core::sync::atomic::{AtomicU64, Ordering};

    /// Fills that issued a real producer request (a block IRP under Model A).
    pub static FILLS: AtomicU64 = AtomicU64::new(0);
    /// Fills that resolved to a hole / unmapped range — the zeroed frame was already
    /// correct, so the PO completed synchronously with no device I/O. Counted apart
    /// because averaging them in would flatter the real cost.
    pub static HOLE_FILLS: AtomicU64 = AtomicU64::new(0);
    /// Total nanoseconds spent inside fills that issued I/O (reserve → page Ready).
    pub static FILL_NS: AtomicU64 = AtomicU64::new(0);
    /// The slowest single such fill, in nanoseconds.
    pub static FILL_MAX_NS: AtomicU64 = AtomicU64::new(0);
    /// Faults that found the page already resident (the cache hit path).
    pub static HITS: AtomicU64 = AtomicU64::new(0);
    /// Faults that found the page mid-fill and `yield_now`-spun waiting for it — the
    /// number that says whether B3 (block the second faulter) is theoretical or real.
    pub static SPINS: AtomicU64 = AtomicU64::new(0);

    /// A snapshot of the counters.
    #[derive(Copy, Clone, Debug, Default)]
    pub struct Snapshot {
        pub fills: u64,
        pub hole_fills: u64,
        pub fill_ns: u64,
        pub fill_max_ns: u64,
        pub hits: u64,
        pub spins: u64,
    }

    /// Read the counters. Relaxed throughout: these are statistics, and a torn read
    /// across counters would at worst misreport by one fill.
    pub fn snapshot() -> Snapshot {
        Snapshot {
            fills: FILLS.load(Ordering::Relaxed),
            hole_fills: HOLE_FILLS.load(Ordering::Relaxed),
            fill_ns: FILL_NS.load(Ordering::Relaxed),
            fill_max_ns: FILL_MAX_NS.load(Ordering::Relaxed),
            hits: HITS.load(Ordering::Relaxed),
            spins: SPINS.load(Ordering::Relaxed),
        }
    }

    /// Record one completed fill that issued I/O.
    pub(crate) fn record_fill(ns: u64) {
        FILLS.fetch_add(1, Ordering::Relaxed);
        FILL_NS.fetch_add(ns, Ordering::Relaxed);
        FILL_MAX_NS.fetch_max(ns, Ordering::Relaxed);
    }
}

impl FileObject {
    /// Sentinel written into [`FileObject::magic`] at construction.
    pub const MAGIC: u64 = 0x46_69_6c_65_4f_62_6a_21; // "FileObj!"

    /// Allocate an empty `FileObject` for a file of `size` bytes whose pages are
    /// filled on fault by `producer`. Refcount one; no frames are allocated here —
    /// pages are reserved + filled lazily on fault.
    pub fn try_new(size: usize, producer: Producer) -> Result<KBox<Self>, AllocError> {
        Self::try_new_with_runs(size, producer, KVec::new())
    }

    /// As [`try_new`](Self::try_new), for a Model A file whose blocks `runs` maps.
    pub fn try_new_with_runs(
        size: usize,
        producer: Producer,
        runs: KVec<BlockRun>,
    ) -> Result<KBox<Self>, AllocError> {
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::FileObject),
            magic: Self::MAGIC,
            size: AtomicUsize::new(size),
            writable_maps: AtomicU32::new(0),
            producer,
            inner: SpinLock::new(
                LockRank::KernelObject,
                Inner {
                    pages: KVec::new(),
                    retired: KVec::new(),
                    runs,
                    self_pin: None,
                    map_gen: 0,
                    dead: false,
                    io_in_flight: 0,
                    forget_answer: None,
                },
            ),
        })
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    /// Exact file size in bytes.
    pub fn size(&self) -> usize {
        self.size.load(Ordering::Acquire)
    }

    /// Number of pages the file spans (`⌈size / PAGE⌉`; `0` for an empty file).
    pub fn npages(&self) -> usize {
        self.size().div_ceil(PAGE_SIZE)
    }

    /// The id this object is cached under in its registration, if it is cached.
    pub fn file_id(&self) -> Option<u64> {
        match &self.producer {
            Producer::FsServerBlocks { file_id, .. } if *file_id != 0 => Some(*file_id),
            _ => None,
        }
    }

    /// Whether the object is dirty — mapped writable since its last write-back found no
    /// writable mapping. Holding its own reference is what dirty *is*.
    pub fn is_dirty(&self) -> bool {
        self.inner.lock().self_pin.is_some()
    }

    /// **A writable mapping of `file_obj` is about to be made.** Counted, and a cached object
    /// pins itself: it stays alive — and findable in its registration's cache — until a
    /// write-back that finds no writable mapping left, however soon its users let go.
    ///
    /// Called **before** the address-space lock is taken, since this takes the object's own
    /// lock and the two are never nested; a mapping that then fails calls
    /// [`writable_unmapped`](Self::writable_unmapped). The pin is kept even then — a spurious
    /// dirty mark costs one write-back of clean pages, where a missing one costs data.
    pub fn writable_mapped(file_obj: &ObjectRef) {
        debug_assert_eq!(file_obj.object_type(), KObjectType::FileObject);
        // SAFETY: `file_obj` pins a live `FileObject` (header at offset 0).
        let fo: &FileObject = unsafe { &*(file_obj.as_ptr() as *const FileObject) };
        let mut g = fo.inner.lock();
        fo.writable_maps.fetch_add(1, Ordering::AcqRel);
        g.map_gen = g.map_gen.wrapping_add(1);
        // A forgotten object is out of the cache, so a pin on it is one nothing could find.
        if g.self_pin.is_none() && fo.file_id().is_some() && !g.dead {
            g.self_pin = Some(file_obj.clone());
        }
    }

    /// A writable mapping of the object at `obj` has gone. Atomic and lock-free, because a
    /// VMA drops wherever its address space is torn down. Saturating, since a VMA that was
    /// never counted must not take another's count with it.
    pub fn writable_unmapped(obj: *mut ()) {
        // SAFETY: the caller's `ObjectRef` (the VMA's) pins a live `FileObject` at `obj`.
        let fo: &FileObject = unsafe { &*(obj as *const FileObject) };
        let _ = fo.writable_maps.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
    }

    /// **Taken before a write-back: whether it can leave the object clean.** `Some` only if no
    /// writable mapping exists now — one that does could write after its page's IRP read the
    /// frame, then go away before the write-back ends, and a check made only at the end would
    /// call that write written. The mark is the mapping generation, which any writable mapping
    /// made during the write-back moves. Pass it to [`unpin_if_clean`](Self::unpin_if_clean).
    ///
    /// So a writer that syncs while still mapped leaves its file dirty, and the next write-back
    /// after it unmaps cleans it. That is why `libfs` and `nxsh` unmap before they sync.
    pub fn clean_mark(&self) -> Option<u64> {
        let g = self.inner.lock();
        if self.writable_maps.load(Ordering::Acquire) == 0 { Some(g.map_gen) } else { None }
    }

    /// After a successful write-back that began at `mark`: **clean**, unless a writable
    /// mapping was made since. Clean drops the self-pin, outside the lock, since it may be the
    /// last reference.
    pub fn unpin_if_clean(file_obj: &ObjectRef, mark: u64) {
        debug_assert_eq!(file_obj.object_type(), KObjectType::FileObject);
        // SAFETY: `file_obj` pins a live `FileObject` (header at offset 0).
        let fo: &FileObject = unsafe { &*(file_obj.as_ptr() as *const FileObject) };
        let pin = {
            let mut g = fo.inner.lock();
            if g.map_gen == mark { g.self_pin.take() } else { None }
        };
        drop(pin);
    }

    /// **Change the file's size in place**, and its run map with it — the reply to a grow,
    /// create or truncate of a cached file (administration Part C.1).
    ///
    /// **What a page says stays honest across it.** Everything past the smaller of the two
    /// sizes is made to read as zero, which is what the filesystem says is there:
    /// - pages wholly past it leave the index for `retired`. A mapping that faulted one in
    ///   keeps a valid frame, but no fault finds it again and no write-back writes it;
    /// - the page holding it is zeroed from it to the page's end.
    ///
    /// On a shrink that is the new end. On a grow it is the old one, since a mapping made
    /// before an earlier shrink can write past the end, and can even fault pages in there —
    /// the fault path bounds an index by the mapping, not the size. Without this, `reserve`,
    /// which hits by index whatever the size, would serve those bytes in a regrown range and
    /// write them back into the blocks the grow allocated.
    ///
    /// A retired frame is freed only with the object, since no reverse map says which page
    /// tables still point at it (`TODO(retired-frames)` in `deferred-decisions.md`).
    ///
    /// A write-back decides each page as it issues the page's IRP, so every IRP after this
    /// respects the new size. An IRP already in flight is not stopped, and can land in a block
    /// the truncate freed (`TODO(truncate-inflight-writeback)`).
    ///
    /// `Err` only if `retired` cannot grow, in which case nothing changed.
    pub fn resize(&self, new_size: usize, runs: KVec<BlockRun>) -> Result<(), AllocError> {
        let mut g = self.inner.lock();
        let edge = new_size.min(self.size());
        let keep = edge.div_ceil(PAGE_SIZE);
        let gone = g.pages.iter().filter(|p| p.index >= keep).count();
        g.retired.try_reserve(gone)?;
        let mut i = 0;
        while i < g.pages.len() {
            if g.pages[i].index >= keep {
                let p = g.pages.remove(i);
                // `try_reserve` above guarantees this push does not allocate.
                let _ = g.retired.try_push(p.frame);
            } else {
                i += 1;
            }
        }
        zero_tail(&g, edge);
        g.runs = runs;
        self.size.store(new_size, Ordering::Release);
        Ok(())
    }

    /// **The object a block-file reply installs** (administration Part C.1). `candidate` is
    /// new, built from the reply for the file its id names. If `reg` already caches a live
    /// object for that file, that object takes the candidate's size and run map
    /// ([`resize`](Self::resize)) and is returned, and the candidate is dropped. Otherwise the
    /// candidate is entered in the cache and returned. Either way, every resolve of one file
    /// gets one object.
    ///
    /// The candidate drops outside the cache's lock, since its `Drop` takes that lock. `Err`
    /// only on allocation failure, with nothing entered and nothing resized.
    pub fn cache_in(reg: &ObjectRef, candidate: ObjectRef) -> Result<ObjectRef, AllocError> {
        debug_assert_eq!(reg.object_type(), KObjectType::UserspaceServerReg);
        debug_assert_eq!(candidate.object_type(), KObjectType::FileObject);
        // SAFETY: both references pin live objects of the asserted types (header at offset 0).
        let (r, cand) = unsafe {
            (
                &*(reg.as_ptr() as *const crate::object::UserspaceServerReg),
                &*(candidate.as_ptr() as *const FileObject),
            )
        };
        let id = cand.file_id().expect("a cached object has an id");
        match r.cache_get_or_insert(id, &candidate)? {
            None => Ok(candidate),
            Some(existing) => {
                // SAFETY: `existing` pins a live `FileObject`.
                let ex = unsafe { &*(existing.as_ptr() as *const FileObject) };
                // **Its own statement**: a guard made inside `resize`'s argument list lives to the
                // end of the call, and `resize` takes the other object's lock — two page-cache
                // locks nested, which the rank checker refuses (a boot caught it; a host test
                // cannot, since the checker is inert under `cfg(test)`).
                let runs = core::mem::replace(&mut cand.inner.lock().runs, KVec::new());
                ex.resize(cand.size(), runs)?;
                drop(candidate);
                Ok(existing)
            }
        }
    }

    /// **A fill is about to read file block `file_block`**: its device block, counted in flight
    /// — or `0`, counting nothing, for a hole, a block past the map, or a forgotten file,
    /// whose blocks may already be another file's.
    fn begin_read(&self, file_block: u64) -> u64 {
        let mut g = self.inner.lock();
        if g.dead {
            return 0;
        }
        let block = device_block_in(&g.runs, file_block);
        if block != 0 {
            g.io_in_flight += 1;
        }
        block
    }

    /// **A write-back is about to write page `index`**, decided afresh under the lock for each
    /// page, so a `Forget` or a resize that lands mid-write-back governs every page after it.
    /// A page to write is counted in flight.
    fn begin_write(&self, index: usize, block_size: u32) -> WriteStep {
        let mut g = self.inner.lock();
        if g.dead {
            return WriteStep::Dead;
        }
        let Some(frame) = g
            .pages
            .iter()
            .find(|p| p.index == index && p.state == PageState::Ready)
            .map(|p| p.frame)
        else {
            return WriteStep::Skip;
        };
        if index >= self.npages() {
            return WriteStep::Skip;
        }
        let block = device_block_in(&g.runs, (index * PAGE_SIZE) as u64 / block_size as u64);
        if block == 0 {
            return WriteStep::Skip; // a hole: growth goes through a resolve, not write-back
        }
        g.io_in_flight += 1;
        WriteStep::Go(frame, block)
    }

    /// **An IRP counted by [`begin_read`](Self::begin_read) or
    /// [`begin_write`](Self::begin_write) has completed.** If it was the last in flight of a
    /// forgotten file, the `Forget` answer comes back for the caller to complete — outside
    /// this lock, since completing takes the scheduler's.
    pub fn end_io(&self) -> Option<ObjectRef> {
        let mut g = self.inner.lock();
        g.io_in_flight = g.io_in_flight.saturating_sub(1);
        if g.dead && g.io_in_flight == 0 { g.forget_answer.take() } else { None }
    }

    /// [`end_io`](Self::end_io), completing a `Forget` answer it hands back.
    fn finish_io(&self) {
        if let Some(answer) = self.end_io() {
            crate::sched::complete_pending_op(answer.as_ptr(), 0, 0);
        }
    }

    /// **The file's server is about to free it** (`File::Forget`, administration Part C.1b).
    /// From now on no device I/O of the object starts: a write-back stops before its next
    /// page, and a fill reads as a hole — the `dead` mark alone decides both, so the run map is
    /// left as it was. The dirty pin is released, so the object goes when its users do, its
    /// pages unwritten. The caller has already taken it
    /// out of its registration's cache, so a later resolve of the id gets a new object, and
    /// the caller's reference keeps it alive across this call, whatever the pin was.
    ///
    /// `answer` is the server's: [`Forgotten::Now`] if nothing is in flight, else the PO to
    /// hand the server, completed when the last I/O ends ([`end_io`](Self::end_io)).
    pub fn forget(&self, answer: &ObjectRef) -> Forgotten {
        let (outcome, pin) = {
            let mut g = self.inner.lock();
            g.dead = true;
            let pin = g.self_pin.take();
            let outcome = if g.io_in_flight == 0 {
                Forgotten::Now
            } else if let Some(waiting) = &g.forget_answer {
                Forgotten::Later(waiting.clone())
            } else {
                g.forget_answer = Some(answer.clone());
                Forgotten::Later(answer.clone())
            };
            (outcome, pin)
        };
        drop(pin);
        outcome
    }

    /// Whether the file's server has forgotten it. Test/observability only.
    #[cfg(test)]
    pub(crate) fn is_dead(&self) -> bool {
        self.inner.lock().dead
    }

    /// The number of pages currently resident in the cache. Test/observability only.
    #[cfg(test)]
    pub(crate) fn resident_pages(&self) -> usize {
        self.inner.lock().pages.len()
    }

    /// How many frames a size change has retired. Test only.
    #[cfg(test)]
    pub(crate) fn retired_frames(&self) -> usize {
        self.inner.lock().retired.len()
    }

    /// How many writable mappings of the object exist. Test only.
    #[cfg(test)]
    pub(crate) fn writable_maps(&self) -> u32 {
        self.writable_maps.load(Ordering::Acquire)
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
            .try_push(CachePage { index, frame, state: PageState::Loading, fill: None })
            .expect("slot reserved above");
        Reserve::New(frame)
    }

    /// The fault path's [`reserve`](Self::reserve): a miss also makes the fill's
    /// `PendingOperation` and records it on the page, so a second faulter finds something
    /// to wait on. Both allocations happen under the lock, which ranks above the allocators.
    fn reserve_fault(&self, index: usize) -> Fault {
        let mut g = self.inner.lock();
        if let Some(p) = g.pages.iter().find(|p| p.index == index) {
            return match (p.state, &p.fill) {
                (PageState::Ready, _) => Fault::Hit(p.frame),
                (PageState::Loading, Some(po)) => Fault::Wait(po.clone()),
                (PageState::Loading, None) => Fault::Busy,
            };
        }
        if g.pages.try_reserve(1).is_err() {
            return Fault::Oom;
        }
        let po = match PendingOperation::try_new() {
            // SAFETY: adopt the single creation reference.
            Ok(p) => unsafe {
                ObjectRef::from_raw(KBox::into_raw(p).as_ptr() as *mut (), KObjectType::PendingOperation)
            },
            Err(_) => return Fault::Oom,
        };
        let Some(frame) = heap::buddy_alloc(0) else {
            drop(g); // the PO's only reference drops outside the lock
            drop(po);
            return Fault::Oom;
        };
        // SAFETY: `frame` was just returned by the buddy, is unaliased, and is HHDM-reachable;
        // zeroing prevents leaking stale memory (and zero-pads a partial tail page).
        unsafe {
            core::ptr::write_bytes((frame.as_u64() + heap::hhdm_offset()) as *mut u8, 0, PAGE_SIZE);
        }
        g.pages
            .try_push(CachePage { index, frame, state: PageState::Loading, fill: Some(po.clone()) })
            .expect("slot reserved above");
        Fault::Fill(frame, po)
    }

    /// **Settle page `index`'s fill `po`** once it has completed: `Ready` if it succeeded,
    /// otherwise out of the cache with its frame freed, so a later fault starts afresh.
    ///
    /// Whoever waited on it may call this — the filler, or a second faulter that woke first —
    /// and it is idempotent. Only the page whose fill *is* `po` is touched: a
    /// [`resize`](Self::resize) can retire a page mid-fill and a later fault reserve the
    /// index afresh, and settling by index alone would call that new page ready while its own
    /// fill still ran. Matched by the PO rather than the frame, since every caller holds a
    /// reference to the PO, so its address cannot be reused meanwhile, where a freed frame's
    /// can.
    fn settle(&self, index: usize, po: &ObjectRef, ok: bool) {
        let taken = {
            let mut g = self.inner.lock();
            let at = g
                .pages
                .iter()
                .position(|p| p.index == index && p.fill.as_ref().is_some_and(|f| f.as_ptr() == po.as_ptr()));
            match at {
                None => None,
                Some(i) if ok => {
                    g.pages[i].state = PageState::Ready;
                    g.pages[i].fill.take()
                }
                Some(i) => {
                    let p = g.pages.remove(i);
                    heap::buddy_free(p.frame, 0);
                    p.fill
                }
            }
        };
        drop(taken);
    }

    /// Transition page `index` from `Loading` to `Ready` (after its fill wrote the
    /// frame). A no-op if the page is absent or already `Ready`.
    pub fn mark_ready(&self, index: usize) {
        let mut g = self.inner.lock();
        if let Some(p) = g.pages.iter_mut().find(|p| p.index == index) {
            p.state = PageState::Ready;
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
    /// **A second faulter of a page being filled waits on that fill's PO** (administration
    /// Part C.1, closing the "concurrent same-page faults" deferral). It used to `yield_now`
    /// until the page was ready, which was unreachable while every resolve had an object of
    /// its own. With one object per file it is ordinary — every process running a binary
    /// shares its image — and the yield was a spin: the fault handler runs with interrupts
    /// off, `yield_now` returns at once when nothing else is ready, so the CPU never
    /// acknowledged a TLB shootdown and the machine stopped. Whoever wakes first settles the
    /// page, so no waiter spins on a completed fill whose filler has not run yet.
    ///
    /// A failed fill fails every faulter that waited on it, and leaves the page out of the
    /// cache for the next fault to try again. A faulter that cannot register on a PO — at
    /// `PendingOperation::MAX_WAITERS` — parks a millisecond and looks again, rather than
    /// spinning.
    pub fn fault_in_page(file_obj: &ObjectRef, index: usize) -> Option<PhysAddr> {
        debug_assert_eq!(file_obj.object_type(), KObjectType::FileObject);
        // SAFETY: `file_obj` pins a live `FileObject` (header at offset 0).
        let fo: &FileObject = unsafe { &*(file_obj.as_ptr() as *const FileObject) };
        loop {
            match fo.reserve_fault(index) {
                Fault::Hit(frame) => {
                    fill_stats::HITS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    return Some(frame);
                }
                Fault::Fill(frame, po) => {
                    // Timed from just before the producer starts to the page being
                    // `Ready`: the whole park → I/O → wake → resume round trip a faulting
                    // thread actually waits through.
                    let started = crate::arch::Timer::read_ns();
                    let started_kind = fo.start_fill(file_obj, index, frame, &po);
                    if started_kind == FillStart::Failed {
                        // Could not start the fill (allocation failure); roll the reserved
                        // page back so a retry is clean. Nothing will complete `po`, so
                        // complete it here for anyone already waiting on it.
                        fo.settle(index, &po, false);
                        crate::sched::complete_pending_op(
                            po.as_ptr(),
                            crate::syscall::error::KError::OutOfMemory as i32,
                            0,
                        );
                        return None;
                    }
                    let ok = wait_for_fill(&po);
                    if started_kind == FillStart::DeviceIo {
                        fo.finish_io();
                    }
                    fo.settle(index, &po, ok);
                    if !ok {
                        return None;
                    }
                    // A hole completes its PO synchronously with no device I/O; counting
                    // it as a fill would understate what a real fill costs.
                    if started_kind == FillStart::Hole {
                        fill_stats::HOLE_FILLS
                            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    } else {
                        fill_stats::record_fill(
                            crate::arch::Timer::read_ns().saturating_sub(started),
                        );
                    }
                    // Loop: the page is now `Ready` → return its frame.
                }
                Fault::Wait(po) => {
                    fill_stats::SPINS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    let ok = wait_for_fill(&po);
                    fo.settle(index, &po, ok);
                    if !ok {
                        return None;
                    }
                }
                Fault::Busy => park_briefly(),
                Fault::Oom => return None,
            }
        }
    }

    /// Flush every resident, block-backed page to the device (the **Model A** overwrite
    /// writeback). For each `Ready` cache page, translate its block via the producer's run
    /// map and issue a block **write** IRP from the frame to that LBA, blocking on each.
    /// Pages over a hole (`device_lba == 0`, unallocated) are skipped — growing a file is
    /// Part D. `file_obj` is this object's reference (pins the frames across the IRPs). Runs
    /// in a syscall thread (it blocks). Returns `true` iff every write succeeded; `false`
    /// for a non-block producer or an I/O/allocation failure.
    pub fn writeback(file_obj: &ObjectRef) -> bool {
        debug_assert_eq!(file_obj.object_type(), KObjectType::FileObject);
        // SAFETY: `file_obj` pins a live `FileObject` (header at offset 0).
        let fo: &FileObject = unsafe { &*(file_obj.as_ptr() as *const FileObject) };
        // The producer is immutable; the run map is read under the lock with the pages.
        let (device, block_size) = match &fo.producer {
            Producer::FsServerBlocks { device, block_size, .. } => (device.clone(), *block_size),
            _ => return false,
        };
        // Which pages are resident, under the lock; each is then looked at again as its IRP is
        // issued ([`begin_write`](Self::begin_write)), so a `Forget` stops the write-back
        // before its next page and a resize redirects it.
        let mut indices: KVec<usize> = KVec::new();
        {
            let inner = fo.inner.lock();
            if indices.try_reserve(inner.pages.len()).is_err() {
                return false;
            }
            for p in inner.pages.iter().filter(|p| p.state == PageState::Ready) {
                let _ = indices.try_push(p.index);
            }
        }
        for index in indices.iter().copied() {
            let po = match PendingOperation::try_new() {
                // SAFETY: adopt the single creation reference.
                Ok(p) => unsafe {
                    ObjectRef::from_raw(KBox::into_raw(p).as_ptr() as *mut (), KObjectType::PendingOperation)
                },
                Err(_) => return false,
            };
            let (frame, dev_block) = match fo.begin_write(index, block_size) {
                WriteStep::Go(frame, dev_block) => (frame, dev_block),
                WriteStep::Skip => continue,
                // Forgotten: what is unwritten stays so — its blocks are about to be freed.
                WriteStep::Dead => return true,
            };
            let dev_offset = dev_block * block_size as u64;
            if crate::io::block::dispatch_block_irp_into_frame(
                &device,
                frame,
                file_obj.clone(),
                &po,
                crate::libkern::io_op::IoOpcode::Write,
                dev_offset,
                PAGE_SIZE as u64,
            )
            .is_err()
            {
                fo.finish_io();
                return false;
            }
            let ok = block_on_po(&po);
            fo.finish_io();
            if !ok {
                return false;
            }
        }
        true
    }

    /// The `(registration, file id)` naming this file to its filesystem server, for a cached
    /// Model A (block) file; `None` for any other.
    ///
    /// Exists for the post-writeback `File::Touch` — the only thing in the system that has
    /// to tell a Model A server something about a file *without* a resolve. By id, since a
    /// cached object outlives the resolve that named it. The clone pins the registration
    /// across the send.
    pub fn touch_target(file_obj: &ObjectRef) -> Option<(ObjectRef, u64)> {
        debug_assert_eq!(file_obj.object_type(), KObjectType::FileObject);
        // SAFETY: `file_obj` pins a live `FileObject` (header at offset 0).
        let fo: &FileObject = unsafe { &*(file_obj.as_ptr() as *const FileObject) };
        // A forgotten file's id may already name another file.
        if fo.inner.lock().dead {
            return None;
        }
        match &fo.producer {
            Producer::FsServerBlocks { reg, file_id, .. } if *file_id != 0 => Some((reg.clone(), *file_id)),
            _ => None,
        }
    }

    /// Materialize the whole file into a fresh contiguous heap buffer (page-rounded
    /// [`size`](Self::size) bytes; the tail past the real data stays zero). Drives the
    /// producer via [`fault_in_page`](Self::fault_in_page) page by page — **blocking on
    /// each fill** — so it must run where blocking is allowed (a syscall thread, not a
    /// DPC/IRQ). `sys_process_spawn` uses it to load an ELF whose image is a store
    /// `FileObject` (a demand-paged file on the fs-server), mirroring
    /// [`MemoryObject::copy_to_kvec`](crate::object::MemoryObject::copy_to_kvec).
    /// `file_obj` is the caller's reference to *this* object. `AllocError` on a
    /// buffer-allocation or fill failure.
    pub fn read_to_kvec(file_obj: &ObjectRef) -> Result<KVec<u8>, AllocError> {
        debug_assert_eq!(file_obj.object_type(), KObjectType::FileObject);
        // SAFETY: `file_obj` pins a live `FileObject` (header at offset 0).
        let fo: &FileObject = unsafe { &*(file_obj.as_ptr() as *const FileObject) };
        let size = fo.size();
        let mut buf = KVec::new();
        buf.try_reserve(size)?;
        let mut remaining = size;
        for i in 0..fo.npages() {
            if remaining == 0 {
                break;
            }
            // Blocks on the producer until page `i` is resident.
            let frame = FileObject::fault_in_page(file_obj, i).ok_or(AllocError)?;
            let n = core::cmp::min(PAGE_SIZE, remaining);
            // SAFETY: `fault_in_page` returned a resident, HHDM-reachable frame for page
            // `i`; reading `n <= PAGE_SIZE` bytes from its HHDM mapping is sound.
            let page = unsafe {
                core::slice::from_raw_parts((frame.as_u64() + heap::hhdm_offset()) as *const u8, n)
            };
            buf.try_extend_from_slice(page)?;
            remaining -= n;
        }
        Ok(buf)
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
    ) -> FillStart {
        match &self.producer {
            Producer::Stub { base } => {
                if stub_start_fill(file_obj, index, frame, po, *base) {
                    FillStart::Io
                } else {
                    FillStart::Failed
                }
            }
            Producer::FsServer { reg, suffix } => {
                if self.fs_server_start_fill(file_obj, index, frame, po, reg, suffix) {
                    FillStart::Io
                } else {
                    FillStart::Failed
                }
            }
            Producer::FsServerBlocks { device, block_size, .. } => {
                self.model_a_start_fill(file_obj, index, frame, po, device, *block_size)
            }
        }
    }

    /// Model A fill: read page `index`'s device block **zero-copy** into `frame` via a block
    /// IRP (completing `po`), or — for a hole / block past the map — complete `po` at once
    /// (the reserved frame is already zeroed). `false` only on an allocation failure. The
    /// page is marked `Ready` by the fault path after the wait.
    fn model_a_start_fill(
        &self,
        file_obj: &ObjectRef,
        index: usize,
        frame: PhysAddr,
        po: &ObjectRef,
        device: &ObjectRef,
        block_size: u32,
    ) -> FillStart {
        // The page's first filesystem block (block_size == PAGE_SIZE for slice-1 fixtures,
        // so this is `index`; the general form handles bs | PAGE where a page's blocks are
        // contiguous within one run).
        let file_block = (index * PAGE_SIZE) as u64 / block_size as u64;
        // Locate the run covering `file_block` → its device block (0 = hole, or a forgotten
        // file). A block to read is counted in flight until the faulter ends it.
        match self.begin_read(file_block) {
            0 => {
                // Hole or unmapped: the zeroed frame is already correct. Complete the PO
                // synchronously so the parked faulter wakes at once (no IRP).
                crate::sched::complete_pending_op(po.as_ptr(), 0, 0);
                FillStart::Hole
            }
            dev_block => {
                let dev_offset = dev_block * block_size as u64;
                // One page of data (one block when block_size == PAGE). `file_obj` pins the
                // FileObject (hence the frame) for the IRP's lifetime.
                match crate::io::block::dispatch_block_irp_into_frame(
                    device,
                    frame,
                    file_obj.clone(),
                    po,
                    crate::libkern::io_op::IoOpcode::Read,
                    dev_offset,
                    PAGE_SIZE as u64,
                ) {
                    Ok(()) => FillStart::DeviceIo,
                    Err(_) => {
                        self.finish_io(); // never issued, so never in flight
                        FillStart::Failed
                    }
                }
            }
        }
    }

    /// Start a real fs-server fill: send a `File::ReadRange` for page `index`'s byte
    /// range over the server's forwarding endpoint (`reg`) and leave the fill
    /// **pending** — the `ReadRange` reply (in the server's send) copies the bytes
    /// into `frame`, marks the page ready, and completes `po` (waking the parked
    /// faulter). `false` if the request could not be built/sent (the caller rolls
    /// the page back). The range is `[index·PAGE, min(PAGE, size − offset))`; a
    /// short tail at end-of-file leaves the rest of the (zeroed) frame as padding.
    fn fs_server_start_fill(
        &self,
        file_obj: &ObjectRef,
        index: usize,
        frame: PhysAddr,
        po: &ObjectRef,
        reg: &ObjectRef,
        suffix: &KString,
    ) -> bool {
        let offset = (index * PAGE_SIZE) as u64;
        let remaining = self.size().saturating_sub(index * PAGE_SIZE);
        let len = remaining.min(PAGE_SIZE) as u32;
        // Build the ReadRange request in a heap-bounced message (4 KiB, allocated
        // zeroed **in place** — `try_new(StoredMsg::zeroed())` would build it in this
        // frame first, which is 4 KiB of kernel stack), as the forwarded lookup does.
        // SAFETY: `StoredMsg` is plain `#[repr(C)]` data; all-zero is a valid value.
        let mut msg = match unsafe { KBox::<StoredMsg>::try_new_zeroed() } {
            Ok(m) => m,
            Err(_) => return false,
        };
        let body_len =
            match crate::rsproto::build_read_range_request(&mut msg.payload, offset, len, suffix.as_bytes()) {
                Some(n) => n,
                None => return false,
            };
        msg.header.payload_len = body_len as u32;
        msg.header.handle_count = 0;
        // Originate: records the pending fill on `reg` and pushes the request. The
        // reply completes `po`; `Busy`/`Full`/`PeerClosed` fail this fault.
        matches!(
            crate::sched::us_forward_originate_fill(reg.as_ptr(), &mut msg, po, file_obj, frame, index),
            crate::sched::ForwardOutcome::Pending
        )
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

/// Wait for a page fill's `po` to complete; `true` iff it succeeded. A waiter that cannot
/// register — the PO already has `MAX_WAITERS` — parks and tries again, since the fill
/// completes without it: a fill is never abandoned for want of a waiter slot, and the retry
/// is a park, never a spin.
fn wait_for_fill(po: &ObjectRef) -> bool {
    loop {
        match crate::sched::wait_on(&[po.as_ptr() as usize], u64::MAX, 0) {
            crate::sched::WaitResult::Signaled(_) => {
                return crate::sched::pending_op_completion(po.as_ptr()).0 == 0;
            }
            _ => park_briefly(),
        }
    }
}

/// Park the current thread for a millisecond — a sleep on no object with a deadline, so the
/// CPU runs something else or idles with interrupts on. What the fault path does where it
/// used to `yield_now`: from the fault handler, with interrupts off and nothing else ready, a
/// yield returns at once and the CPU takes no interrupt at all.
fn park_briefly() {
    let now = crate::arch::Timer::read_ns();
    let _ = crate::sched::wait_on(&[], now + 1_000_000, now);
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
    // Dropping `bx` here releases two `ObjectRef`s in DPC context, which the block path
    // stopped doing (decision log, 2026-08-06) because a last-reference drop reaches
    // `SlabCache::free` and can deadlock against an allocator holder on the same CPU. It is
    // safe *here* only because `Producer::Stub` is constructed solely from `#[cfg(test)]`
    // code, so this never runs on a real boot. Noted so it does not read as a live
    // counter-example to the rule (PR #177 review, finding 6).
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
        {
            let g = self.inner.lock();
            for p in g.pages.iter() {
                heap::buddy_free(p.frame, 0);
            }
            for f in g.retired.iter() {
                heap::buddy_free(*f, 0);
            }
            // `self_pin` is `None`: a pinned object has a reference, so is not dropping.
        }
        // **Out of the registration's cache**, by address, after the page lock is released:
        // the cache lock ranks above it and is never taken inside it. By address rather than
        // id, since a new object for the same file may already have replaced this one there.
        if let Producer::FsServerBlocks { reg, file_id, .. } = &self.producer {
            if *file_id != 0 {
                // SAFETY: `reg` pins a live `UserspaceServerReg`.
                let r: &crate::object::UserspaceServerReg =
                    unsafe { &*(reg.as_ptr() as *const crate::object::UserspaceServerReg) };
                r.cache_forget_object(self as *mut Self as *mut ());
            }
        }
    }
}

/// The device block holding file block `file_block` in `runs`; `0` for a hole or a block
/// past the map.
fn device_block_in(runs: &KVec<BlockRun>, file_block: u64) -> u64 {
    runs.iter()
        .find_map(|r| {
            if file_block >= r.file_block && file_block < r.file_block + r.length as u64 {
                Some(if r.device_lba == 0 { 0 } else { r.device_lba + (file_block - r.file_block) })
            } else {
                None
            }
        })
        .unwrap_or(0)
}

/// Zero the resident page holding byte `end` from `end` to the page's end — the bytes past a
/// file's end that a size change must not let resurface. Nothing to do on a page boundary, or
/// if that page is not resident (it will fill as zero past the end, or from a hole).
fn zero_tail(g: &Inner, end: usize) {
    let within = end % PAGE_SIZE;
    if within == 0 {
        return;
    }
    let index = end / PAGE_SIZE;
    if let Some(p) = g.pages.iter().find(|p| p.index == index && p.state == PageState::Ready) {
        // SAFETY: `p.frame` is a live, HHDM-reachable cache frame this object owns; the
        // range `[within, PAGE_SIZE)` lies inside it.
        unsafe {
            core::ptr::write_bytes(
                (p.frame.as_u64() + heap::hhdm_offset() + within as u64) as *mut u8,
                0,
                PAGE_SIZE - within,
            );
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

    // --- One object per file (administration Part C.1) ---------------------

    use crate::object::device_node::{BlockGeometry, DeviceClass, ResourceDescriptor};
    use crate::object::{DeviceNode, IpcChannel, UserspaceServerReg};

    /// A registration to cache files in, over an endpoint whose peer is gone.
    fn registration() -> ObjectRef {
        let (a, b) = IpcChannel::try_new_pair(4).unwrap();
        // SAFETY: `into_raw` yields each single creation reference; adopt them.
        let ep = unsafe { ObjectRef::from_raw(KBox::into_raw(a).as_ptr() as *mut (), KObjectType::IpcChannel) };
        drop(unsafe { ObjectRef::from_raw(KBox::into_raw(b).as_ptr() as *mut (), KObjectType::IpcChannel) });
        let r = UserspaceServerReg::try_new(ep).unwrap();
        // SAFETY: as above.
        unsafe { ObjectRef::from_raw(KBox::into_raw(r).as_ptr() as *mut (), KObjectType::UserspaceServerReg) }
    }

    fn reg_of(reg: &ObjectRef) -> &UserspaceServerReg {
        // SAFETY: `reg` pins a live registration.
        unsafe { &*(reg.as_ptr() as *const UserspaceServerReg) }
    }

    fn file_of(f: &ObjectRef) -> &FileObject {
        // SAFETY: `f` pins a live `FileObject`.
        unsafe { &*(f.as_ptr() as *const FileObject) }
    }

    /// What a block-file reply builds for file `id` of `reg`, `size` bytes over a device the
    /// test never reads (these tests drive the cache, not the fill). `id` `0` is uncached.
    fn candidate(reg: &ObjectRef, id: u64, size: usize) -> ObjectRef {
        let dev = DeviceNode::try_new(DeviceClass::Other, ResourceDescriptor::ZERO, BlockGeometry::ZERO).unwrap();
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        let device = unsafe { ObjectRef::from_raw(KBox::into_raw(dev).as_ptr() as *mut (), KObjectType::DeviceNode) };
        let f = FileObject::try_new(size, Producer::FsServerBlocks { device, block_size: 4096, reg: reg.clone(), file_id: id })
            .unwrap();
        // SAFETY: as above.
        unsafe { ObjectRef::from_raw(KBox::into_raw(f).as_ptr() as *mut (), KObjectType::FileObject) }
    }

    /// The reply path's step for a file with an id: [`FileObject::cache_in`].
    fn resolve(reg: &ObjectRef, id: u64, size: usize) -> ObjectRef {
        FileObject::cache_in(reg, candidate(reg, id, size)).unwrap()
    }

    /// Fill page `index` with `byte` and mark it ready, as a fault and its fill would.
    fn fill(f: &FileObject, index: usize, byte: u8) -> PhysAddr {
        let frame = match f.reserve(index) { Reserve::New(fr) => fr, o => panic!("{o:?}") };
        for off in 0..PAGE_SIZE {
            write_frame_byte(frame, off, byte);
        }
        f.mark_ready(index);
        frame
    }

    #[test]
    fn two_resolves_of_one_file_share_one_object() {
        init_global_heap();
        let reg = registration();
        let a = resolve(&reg, 7, PAGE_SIZE);
        let b = resolve(&reg, 7, PAGE_SIZE);
        assert_eq!(a.as_ptr(), b.as_ptr(), "one file, one object");
        let other = resolve(&reg, 8, PAGE_SIZE);
        assert_ne!(other.as_ptr(), a.as_ptr(), "another file, another object");
        assert_eq!(reg_of(&reg).cached_files(), 2);
        // A page written through one is the page the other reads — no sync between them.
        let frame = fill(file_of(&a), 0, 0x5A);
        assert_eq!(file_of(&b).lookup(0), Some((frame, PageState::Ready)));
    }

    /// **The second resolve carries the file as the server now sees it** — a grow or a
    /// truncate is a resolve — and the one object takes its size.
    #[test]
    fn a_later_resolve_resizes_the_shared_object() {
        init_global_heap();
        let reg = registration();
        let a = resolve(&reg, 7, PAGE_SIZE);
        let b = resolve(&reg, 7, 3 * PAGE_SIZE);
        assert_eq!(a.as_ptr(), b.as_ptr());
        assert_eq!(file_of(&a).size(), 3 * PAGE_SIZE, "the grow reached the first holder too");
    }

    #[test]
    fn a_clean_object_leaves_the_cache_with_its_last_user() {
        init_global_heap();
        let reg = registration();
        let a = resolve(&reg, 7, PAGE_SIZE);
        let first = a.as_ptr();
        FileObject::writable_mapped(&a);
        FileObject::writable_unmapped(a.as_ptr());
        let mark = file_of(&a).clean_mark().expect("no writable mapping left");
        FileObject::unpin_if_clean(&a, mark);
        assert!(!file_of(&a).is_dirty());
        test_probe::reset();
        drop(a);
        assert_eq!(test_probe::file_object_destroys(), 1, "clean, and nothing holds it");
        assert_eq!(reg_of(&reg).cached_files(), 0, "and its entry went with it");
        // A new resolve is a new object — the id no longer names a live one.
        let again = resolve(&reg, 7, PAGE_SIZE);
        assert_eq!(reg_of(&reg).cached_files(), 1);
        drop((first, again));
    }

    /// **A writer that exits without syncing loses nothing**: its object pins itself, stays
    /// in the cache, and the next resolve of the file finds it — pages and all.
    #[test]
    fn a_dirty_object_outlives_its_last_user_and_is_found_again() {
        init_global_heap();
        let reg = registration();
        let a = resolve(&reg, 7, PAGE_SIZE);
        let first = a.as_ptr();
        let frame = fill(file_of(&a), 0, 0xC3);
        FileObject::writable_mapped(&a);
        FileObject::writable_unmapped(a.as_ptr());
        assert!(file_of(&a).is_dirty());
        test_probe::reset();
        drop(a);
        assert_eq!(test_probe::file_object_destroys(), 0, "dirty: it holds itself");
        assert_eq!(reg_of(&reg).cached_files(), 1);
        let found = resolve(&reg, 7, PAGE_SIZE);
        assert_eq!(found.as_ptr(), first, "the next resolve finds it");
        test_probe::reset(); // the resolve's own candidate was dropped for it
        assert_eq!(file_of(&found).lookup(0), Some((frame, PageState::Ready)), "with its written page");
        assert_eq!(reg_of(&reg).cache_objects().unwrap().len(), 1, "and a sync can enumerate it");
        // Written back and clean, it goes with its last user.
        let mark = file_of(&found).clean_mark().unwrap();
        FileObject::unpin_if_clean(&found, mark);
        drop(found);
        assert_eq!(test_probe::file_object_destroys(), 1);
        assert_eq!(reg_of(&reg).cached_files(), 0);
    }

    /// **A write-back cleans only if no writable mapping existed at any point in it.** One
    /// that exists at the start could write after its page's IRP and go before the end; one
    /// made during it could do the same. A check made only at the end would call either
    /// write written.
    #[test]
    fn a_write_back_cleans_only_if_no_writable_mapping_existed_throughout() {
        init_global_heap();
        let reg = registration();
        let a = resolve(&reg, 7, PAGE_SIZE);
        let fo = file_of(&a);

        // A mapping present at the start: no mark, so nothing cleans — even once it is gone.
        FileObject::writable_mapped(&a);
        assert_eq!(fo.clean_mark(), None);
        FileObject::writable_unmapped(a.as_ptr());
        assert!(fo.is_dirty(), "the sync that began mapped does not clean");

        // A mapping made and gone during the write-back moves the mark.
        let mark = fo.clean_mark().unwrap();
        FileObject::writable_mapped(&a);
        FileObject::writable_unmapped(a.as_ptr());
        FileObject::unpin_if_clean(&a, mark);
        assert!(fo.is_dirty(), "a mapping during the write-back keeps it dirty");

        // None at the start and none since: clean.
        let mark = fo.clean_mark().unwrap();
        FileObject::unpin_if_clean(&a, mark);
        assert!(!fo.is_dirty());
        assert_eq!(fo.writable_maps(), 0);
    }

    /// An object with no id is not in a cache a sync could search, so it never pins itself —
    /// a pin nothing can find is a leak.
    #[test]
    fn an_uncached_object_never_pins_itself() {
        init_global_heap();
        let reg = registration();
        let a = candidate(&reg, 0, PAGE_SIZE);
        FileObject::writable_mapped(&a);
        assert_eq!(file_of(&a).writable_maps(), 1);
        assert!(!file_of(&a).is_dirty());
        FileObject::writable_unmapped(a.as_ptr());
        assert_eq!(reg_of(&reg).cached_files(), 0);
        // And the count saturates: a stray decrement does not wrap it.
        FileObject::writable_unmapped(a.as_ptr());
        assert_eq!(file_of(&a).writable_maps(), 0);
    }

    /// **A truncate and then a grow read zero over the regrown range** — a whole page and a
    /// partial tail. A design that kept the pages would serve the old bytes over both, where
    /// "a mapping's pages stay valid" alone passes for either design: so the test holds the
    /// retired frames to their old bytes as well as the index to zero.
    #[test]
    fn a_truncate_then_a_grow_reads_zero_over_the_regrown_range() {
        init_global_heap();
        let f = fobj(3 * PAGE_SIZE);
        let p0 = fill(&f, 0, 0xAB);
        let p1 = fill(&f, 1, 0xAB);
        let p2 = fill(&f, 2, 0xAB);

        // Truncate to ten bytes into page 0.
        f.resize(10, KVec::new()).unwrap();
        assert_eq!(f.lookup(1), None, "a whole page past the end leaves the index");
        assert_eq!(f.lookup(2), None);
        assert_eq!(f.retired_frames(), 2);
        assert_eq!((frame_byte(p1, 0), frame_byte(p2, 0)), (0xAB, 0xAB), "a mapping of one stays valid");
        assert_eq!(frame_byte(p0, 9), 0xAB, "the kept bytes");
        assert_eq!(frame_byte(p0, 10), 0, "the partial tail is zeroed");

        // A mapping made before the truncate writes past the end, and faults a page back in
        // there — the fault path bounds an index by the mapping, not the size.
        write_frame_byte(p0, 100, 0xCD);
        let phantom = fill(&f, 1, 0xCD);

        // Grow back.
        f.resize(3 * PAGE_SIZE, KVec::new()).unwrap();
        assert_eq!(f.size(), 3 * PAGE_SIZE);
        assert_eq!(frame_byte(p0, 9), 0xAB);
        assert_eq!(frame_byte(p0, 100), 0, "the partial tail reads zero after the grow");
        assert_eq!(f.lookup(1), None, "the page faulted in past the end is not served");
        assert_eq!(frame_byte(phantom, 0), 0xCD, "its frame is retired, not reused");
        // A page not resident is filled afresh — zeroed, then from the map, which a grow's
        // server zeroes on the device.
        match f.reserve(1) {
            Reserve::New(fr) => assert_eq!(frame_byte(fr, 0), 0),
            o => panic!("{o:?}"),
        }
    }

    // --- `File::Forget` (administration Part C.1b) --------------------------

    fn po() -> ObjectRef {
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        unsafe {
            ObjectRef::from_raw(
                KBox::into_raw(PendingOperation::try_new().unwrap()).as_ptr() as *mut (),
                KObjectType::PendingOperation,
            )
        }
    }

    /// File `id` of `reg`, two pages over device blocks 100 and 101, both resident and ready.
    fn two_block_file(reg: &ObjectRef, id: u64) -> ObjectRef {
        let f = resolve(reg, id, 2 * PAGE_SIZE);
        let mut runs = KVec::new();
        runs.try_push(BlockRun { file_block: 0, device_lba: 100, length: 2, flags: 0 }).unwrap();
        file_of(&f).resize(2 * PAGE_SIZE, runs).unwrap();
        fill(file_of(&f), 0, 0xA0);
        fill(file_of(&f), 1, 0xA1);
        f
    }

    /// **A forgotten file is never written back**, is out of the cache — so the next resolve
    /// of its id, which after an unlink may be another file, gets a new object — and lets go
    /// of its dirty pin, so it goes with its users.
    #[test]
    fn a_forgotten_file_is_never_written_back_and_leaves_the_cache() {
        init_global_heap();
        let reg = registration();
        let a = two_block_file(&reg, 7);
        let first = a.as_ptr();
        FileObject::writable_mapped(&a);
        FileObject::writable_unmapped(a.as_ptr());
        assert!(file_of(&a).is_dirty());
        assert!(matches!(file_of(&a).begin_write(0, 4096), WriteStep::Go(_, 100)));
        assert!(file_of(&a).end_io().is_none(), "nothing forgotten yet");

        let taken = reg_of(&reg).cache_take(7).expect("cached");
        assert!(matches!(file_of(&taken).forget(&po()), Forgotten::Now), "nothing in flight");
        drop(taken);
        assert!(file_of(&a).is_dead());
        assert!(!file_of(&a).is_dirty(), "the pin is gone");
        assert!(matches!(file_of(&a).begin_write(0, 4096), WriteStep::Dead));
        assert_eq!(file_of(&a).begin_read(0), 0, "a fill reads a hole, not a freed block");
        assert_eq!(FileObject::touch_target(&a).map(|(_, id)| id), None, "and no touch names it");
        let again = resolve(&reg, 7, PAGE_SIZE);
        assert_ne!(again.as_ptr(), first, "a new object for the id");
        test_probe::reset();
        drop(a);
        assert_eq!(test_probe::file_object_destroys(), 1, "unpinned: it went with its user");
    }

    /// **A `Forget` during a write-back is answered only after the IRP in flight, and nothing
    /// is written after it.** The answer comes back from the `end_io` of that IRP.
    #[test]
    fn a_forget_mid_write_back_is_answered_after_the_irp_in_flight() {
        init_global_heap();
        let reg = registration();
        let a = two_block_file(&reg, 7);
        let fo = file_of(&a);
        assert!(matches!(fo.begin_write(0, 4096), WriteStep::Go(_, 100)), "page 0's IRP is issued");
        let answer = po();
        let Forgotten::Later(wait_on) = fo.forget(&answer) else { panic!("an IRP is in flight") };
        assert_eq!(wait_on.as_ptr(), answer.as_ptr());
        assert!(matches!(fo.begin_write(1, 4096), WriteStep::Dead), "page 1 is not written");
        // A second `Forget` of the file waits on the same answer.
        let Forgotten::Later(second) = fo.forget(&po()) else { panic!() };
        assert_eq!(second.as_ptr(), answer.as_ptr());
        let done = fo.end_io().expect("the last IRP ending hands the answer back");
        assert_eq!(done.as_ptr(), answer.as_ptr());
        assert!(fo.end_io().is_none(), "and only once");
    }

    /// **A fill in flight holds the answer too**: its read was issued against a block the
    /// server has not freed yet, and must land before it does.
    #[test]
    fn a_fill_in_flight_holds_the_forget_answer() {
        init_global_heap();
        let reg = registration();
        let a = two_block_file(&reg, 7);
        let fo = file_of(&a);
        assert_eq!(fo.begin_read(1), 101);
        let answer = po();
        assert!(matches!(fo.forget(&answer), Forgotten::Later(_)));
        assert_eq!(fo.begin_read(0), 0, "no read starts after it");
        assert_eq!(fo.end_io().map(|p| p.as_ptr()), Some(answer.as_ptr()));
    }

    /// A forgotten object is out of the cache, so a writable mapping must not pin it: nothing
    /// could ever find it to clean.
    #[test]
    fn a_forgotten_file_is_never_pinned() {
        init_global_heap();
        let reg = registration();
        let a = two_block_file(&reg, 7);
        drop(reg_of(&reg).cache_take(7));
        let _ = file_of(&a).forget(&po());
        FileObject::writable_mapped(&a);
        assert!(!file_of(&a).is_dirty());
        FileObject::writable_unmapped(a.as_ptr());
    }

    /// The fault path's reservation, as a test can read it: the frame, and the PO to wait on
    /// or complete.
    fn fault(f: &FileObject, index: usize) -> (&'static str, Option<PhysAddr>, Option<ObjectRef>) {
        match f.reserve_fault(index) {
            Fault::Hit(fr) => ("hit", Some(fr), None),
            Fault::Fill(fr, po) => ("fill", Some(fr), Some(po)),
            Fault::Wait(po) => ("wait", None, Some(po)),
            Fault::Busy => ("busy", None, None),
            Fault::Oom => ("oom", None, None),
        }
    }

    /// **A second faulter of a page being filled is given the fill's PO to wait on** — the
    /// fix for a spin that stopped a boot once one object per file made two processes faulting
    /// one page ordinary — and **whoever wakes first settles it**, so no waiter spins on a
    /// completed fill whose filler has not run yet.
    #[test]
    fn a_second_faulter_waits_on_the_fill_and_either_can_settle_it() {
        init_global_heap();
        let f = fobj(2 * PAGE_SIZE);
        let ("fill", Some(frame), Some(po)) = fault(&f, 1) else { panic!("a miss fills") };
        let ("wait", None, Some(waits_on)) = fault(&f, 1) else { panic!("a second faulter waits") };
        assert_eq!(waits_on.as_ptr(), po.as_ptr(), "on the fill's own PO");
        // The waiter woke first and settles it; the filler's settle after is a no-op.
        f.settle(1, &waits_on, true);
        assert_eq!(f.lookup(1), Some((frame, PageState::Ready)));
        f.settle(1, &po, true);
        assert!(matches!(fault(&f, 1), ("hit", Some(fr), None) if fr == frame));
        // A page reserved outside the fault path has no PO: busy, not a wait on nothing.
        let _ = f.reserve(0);
        assert!(matches!(fault(&f, 0), ("busy", None, None)));
    }

    /// **A failed fill leaves the page out of the cache**, so the next fault fills it afresh
    /// rather than waiting on a page nothing will ever finish.
    #[test]
    fn a_failed_fill_leaves_the_page_for_the_next_fault() {
        init_global_heap();
        let f = fobj(PAGE_SIZE);
        let ("fill", Some(_), Some(po)) = fault(&f, 0) else { panic!() };
        f.settle(0, &po, false);
        assert_eq!(f.lookup(0), None);
        let ("fill", Some(_), Some(again)) = fault(&f, 0) else { panic!("filled afresh") };
        assert_ne!(again.as_ptr(), po.as_ptr());
    }

    /// **A fill whose page was retired under it settles nothing** — in particular not the page
    /// a later fault reserved at the same index, whose own fill is still running.
    #[test]
    fn a_fill_retired_mid_flight_does_not_settle_the_next_page() {
        init_global_heap();
        let f = fobj(2 * PAGE_SIZE);
        let ("fill", Some(first), Some(po1)) = fault(&f, 1) else { panic!() };
        f.resize(PAGE_SIZE, KVec::new()).unwrap(); // retires page 1, still loading
        f.resize(2 * PAGE_SIZE, KVec::new()).unwrap();
        let ("fill", Some(second), Some(po2)) = fault(&f, 1) else { panic!() };
        assert_ne!(first, second);
        f.settle(1, &po1, true); // the first fill completes
        assert_eq!(f.lookup(1), Some((second, PageState::Loading)), "the second is still filling");
        f.settle(1, &po1, false); // nor does its failure take the second page away
        assert_eq!(f.lookup(1), Some((second, PageState::Loading)));
        f.settle(1, &po2, true);
        assert_eq!(f.lookup(1), Some((second, PageState::Ready)));
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
