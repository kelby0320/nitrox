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

use crate::arch::timer::ArchTimer;
use crate::dpc::Dpc;
use crate::libkern::handle::KObjectType;
use crate::libkern::lockrank::LockRank;
use crate::libkern::{AllocError, KBox, KString, KVec, SpinLock};
use crate::mm::{PAGE_SIZE, PhysAddr, heap};
use crate::object::header::KObjectHeader;
use crate::object::{ObjectRef, PendingOperation, StoredMsg};

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

/// [`BlockRun::flags`] bit 0, **`UNWRITTEN`**: the run's blocks read as zero — a grow's reply marks
/// what it just allocated, which no byte of the file has reached — so a fill of a page that is all
/// such blocks, or holes, reads nothing (Phase 6 Part H.4). The blocks are still the file's: a
/// write-back writes them as any other's, which is why a fill and a write-back look a block up
/// differently ([`fill_block_in`], [`device_block_in`]).
pub const RUN_UNWRITTEN: u32 = 1 << 0;

/// **Why [`FileObject::cache_in`] refused a block-file reply.**
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CacheInError {
    /// Nothing could be allocated.
    OutOfMemory,
    /// The reply's block size is not the one the cached object for its file was made with.
    BlockSizeChanged,
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

/// What [`FileObject::begin_write`] says to do with a run of a write-back.
#[derive(Debug)]
enum WriteStep {
    /// Write this many pages, whose frames it left in the caller's list, from this device block;
    /// the IRP is counted in flight.
    Go(u64, usize),
    /// Nothing to write for the run's first page — gone from the cache, past the end, or over a
    /// hole.
    Skip,
    /// The file is being freed: write nothing more of it.
    Dead,
    /// The list of frames could not be made.
    NoMemory,
}

/// **The most pages one write-back IRP carries**, whatever the device takes (Phase 6 Part H.4):
/// 1 MiB, about what Linux sends a SATA disk in one command (its `max_sectors_kb`, 1,280). A
/// device's own limit is usually lower — AHCI's 248 fragments, USB storage's 64 — and governs;
/// this bounds the one with none, the RAM disk.
pub const RUN_MAX_PAGES: usize = 256;

/// **How many pages a run may hold on a device that describes `max_frags` fragments a command**
/// — a page each — at most [`RUN_MAX_PAGES`], and at least one.
fn run_pages(max_frags: Option<u32>) -> usize {
    max_frags.map_or(1, |m| (m as usize).clamp(1, RUN_MAX_PAGES))
}

/// **The most pages a Model A fill reads**, the faulted page and those after it (Phase 6 Part
/// H.4): 128 KiB, Linux's default read-ahead. A file read through — a copy's source, an image
/// being loaded — reads in commands of this size rather than a page; a reader that wanted one page
/// pays for the rest in memory and one longer command. Within every device's limit, as
/// [`run_pages`] bounds it anyway.
pub const READ_AHEAD_PAGES: usize = 32;
const _: () = assert!(READ_AHEAD_PAGES >= 1 && READ_AHEAD_PAGES <= 64, "the resident pages' mask is a u64");

/// What [`FileObject::begin_fill`] says a fill reads.
#[derive(Debug, PartialEq, Eq)]
enum ReadStep {
    /// This many pages from this device block, their frames in the caller's list, the IRP counted
    /// in flight.
    Go(u64, usize),
    /// Nothing: the page reads as zero, or the file is forgotten.
    Hole,
    /// The list of frames could not be made.
    NoMemory,
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
/// are always on rather than feature-gated: a decision to cluster fills should rest on
/// measurement rather than on the stale ~325 ms/page figure from the Model-B era
/// (`deferred-decisions.md`). **Served since Phase 6 Part H.2**, with the write-backs', at
/// `/dev/registry/io` and as `io.tsm`'s `page-cache` row.
/// What starting a fill actually did — the caller needs to tell a real device round trip
/// from a hole that completed synchronously, both to account for it and (for a hole) to
/// know no I/O latency was involved.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum FillStart {
    /// A producer request was issued; the faulter will park until it completes.
    Io,
    /// A Model A block IRP was issued and **counted in the object's I/O in flight**, so the
    /// faulter that issued it ends it ([`FileObject::finish_io`]) once it completes. It reads this
    /// many pages, the faulted one and those read ahead of it, each `Loading` on the fill's PO.
    DeviceIo(usize),
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
    /// The pages those fills brought in: one a fill, and under Model A the pages it read ahead
    /// besides (Phase 6 Part H.4).
    pub static FILL_PAGES: AtomicU64 = AtomicU64::new(0);
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
    /// Pages written back to their files' blocks (Phase 6 Part H.2).
    pub static WRITEBACK_PAGES: AtomicU64 = AtomicU64::new(0);
    /// The write IRPs that carried them: one for each run of pages on the device, up to the
    /// device's limit (Phase 6 Part H.4).
    pub static WRITEBACK_IRPS: AtomicU64 = AtomicU64::new(0);
    /// Nanoseconds those IRPs took, from submission to completion.
    pub static WRITEBACK_NS: AtomicU64 = AtomicU64::new(0);

    /// A snapshot of the counters.
    #[derive(Copy, Clone, Debug, Default)]
    pub struct Snapshot {
        pub fills: u64,
        pub fill_pages: u64,
        pub hole_fills: u64,
        pub fill_ns: u64,
        pub fill_max_ns: u64,
        pub hits: u64,
        pub spins: u64,
        pub writeback_pages: u64,
        pub writeback_irps: u64,
        pub writeback_ns: u64,
    }

    /// Read the counters. Relaxed throughout: these are statistics, and a torn read
    /// across counters would at worst misreport by one fill.
    pub fn snapshot() -> Snapshot {
        Snapshot {
            fills: FILLS.load(Ordering::Relaxed),
            fill_pages: FILL_PAGES.load(Ordering::Relaxed),
            hole_fills: HOLE_FILLS.load(Ordering::Relaxed),
            fill_ns: FILL_NS.load(Ordering::Relaxed),
            fill_max_ns: FILL_MAX_NS.load(Ordering::Relaxed),
            hits: HITS.load(Ordering::Relaxed),
            spins: SPINS.load(Ordering::Relaxed),
            writeback_pages: WRITEBACK_PAGES.load(Ordering::Relaxed),
            writeback_irps: WRITEBACK_IRPS.load(Ordering::Relaxed),
            writeback_ns: WRITEBACK_NS.load(Ordering::Relaxed),
        }
    }

    /// Record one completed fill that issued I/O, bringing in `pages` pages.
    pub(crate) fn record_fill(ns: u64, pages: u64) {
        FILLS.fetch_add(1, Ordering::Relaxed);
        FILL_PAGES.fetch_add(pages, Ordering::Relaxed);
        FILL_NS.fetch_add(ns, Ordering::Relaxed);
        FILL_MAX_NS.fetch_max(ns, Ordering::Relaxed);
    }

    /// Record one write-back IRP, carrying `pages` pages, that took `ns` from submission to
    /// completion.
    pub(crate) fn record_writeback(pages: u64, ns: u64) {
        WRITEBACK_PAGES.fetch_add(pages, Ordering::Relaxed);
        WRITEBACK_IRPS.fetch_add(1, Ordering::Relaxed);
        WRITEBACK_NS.fetch_add(ns, Ordering::Relaxed);
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
    pub fn try_new_with_runs(size: usize, producer: Producer, runs: KVec<BlockRun>) -> Result<KBox<Self>, AllocError> {
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

    /// The unit of its runs, for a Model A file: the block size its server replied.
    fn block_size(&self) -> Option<u32> {
        match &self.producer {
            Producer::FsServerBlocks { block_size, .. } => Some(*block_size),
            _ => None,
        }
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
        // No sync writes a forgotten object, so a pin on it is one nothing would ever clean.
        if g.self_pin.is_none() && fo.file_id().is_some() && !g.dead {
            g.self_pin = Some(file_obj.clone());
        }
    }

    /// A writable mapping of the object at `obj` has gone. Atomic and lock-free, because a
    /// VMA drops wherever its address space is torn down. Saturating, since a VMA that was
    /// never counted must not take another's count with it.
    ///
    /// # Safety
    ///
    /// `obj` points at a live `FileObject`, pinned for the call by a reference the caller holds.
    pub unsafe fn writable_unmapped(obj: *mut ()) {
        // SAFETY: the caller's `ObjectRef` (the VMA's) pins a live `FileObject` at `obj`.
        let fo: &FileObject = unsafe { &*(obj as *const FileObject) };
        let _ = fo.writable_maps.try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
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
    /// A write-back decides each run of pages as it issues the run's IRP, so every IRP after this
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
    /// The candidate drops outside the cache's lock, since its `Drop` takes that lock. `Err` on
    /// allocation failure, with nothing entered and nothing resized; and **for a reply whose block
    /// size is not the cached object's** (PR #365 review, finding 3), with nothing resized. Its runs
    /// were checked against its own block size, and the object keeps the one it was made with — a
    /// producer's is fixed — so they would be used against a size nothing checked them for. A file
    /// changes block size only if its server does something no filesystem does.
    pub fn cache_in(reg: &ObjectRef, candidate: ObjectRef) -> Result<ObjectRef, CacheInError> {
        debug_assert_eq!(reg.object_type(), KObjectType::UserspaceServerReg);
        debug_assert_eq!(candidate.object_type(), KObjectType::FileObject);
        // SAFETY: both references pin live objects of the asserted types (header at offset 0).
        let (r, cand) = unsafe {
            (&*(reg.as_ptr() as *const crate::object::UserspaceServerReg), &*(candidate.as_ptr() as *const FileObject))
        };
        let id = cand.file_id().expect("a cached object has an id");
        match r.cache_get_or_insert(id, &candidate).map_err(|_| CacheInError::OutOfMemory)? {
            None => Ok(candidate),
            Some(existing) => {
                // SAFETY: `existing` pins a live `FileObject`.
                let ex = unsafe { &*(existing.as_ptr() as *const FileObject) };
                if ex.block_size() != cand.block_size() {
                    drop(candidate);
                    return Err(CacheInError::BlockSizeChanged);
                }
                // **Its own statement**: a guard made inside `resize`'s argument list lives to the
                // end of the call, and `resize` takes the other object's lock — two page-cache
                // locks nested, which the rank checker refuses (a boot caught it; a host test
                // cannot, since the checker is inert under `cfg(test)`).
                let runs = core::mem::take(&mut cand.inner.lock().runs);
                ex.resize(cand.size(), runs).map_err(|_| CacheInError::OutOfMemory)?;
                drop(candidate);
                Ok(existing)
            }
        }
    }

    /// **A fill is about to read page `index`, into the `frame` its fault reserved**: the pages it
    /// reads from the page's first device block, counted as one IRP in flight — or
    /// [`Hole`](ReadStep::Hole), counting nothing, for a hole, a block past the map, a page of
    /// blocks that read as zero ([`fill_block_in`]), or a forgotten file, whose blocks may already
    /// be another file's.
    ///
    /// **And it reads ahead** (Phase 6 Part H.4): up to `ahead` pages after the faulted one, each
    /// not resident, inside the file, and on the device right after the one before, none of them
    /// reading as zero — reserved here, `Loading` on the fault's `po`, so a fault on one of them
    /// waits on this fill as a second faulter of the page does, and settles it. A resident page
    /// ends the run, whatever its state. Their frames follow `frame` in `frames`. They are not
    /// zeroed: the read fills each whole, and a page whose read fails leaves the cache unmapped.
    fn begin_fill(
        &self,
        index: usize,
        frame: PhysAddr,
        block_size: u32,
        ahead: usize,
        po: &ObjectRef,
        frames: &mut KVec<PhysAddr>,
    ) -> ReadStep {
        let ahead = ahead.min(READ_AHEAD_PAGES - 1);
        frames.clear();
        if frames.try_reserve(1 + ahead).is_err() {
            return ReadStep::NoMemory;
        }
        let bs = block_size as u64;
        let per_page = (PAGE_SIZE as u64 / bs).max(1);
        let first_block = |i: usize| (i * PAGE_SIZE) as u64 / bs;
        let mut g = self.inner.lock();
        if g.dead {
            return ReadStep::Hole;
        }
        let first = fill_block_in(&g.runs, first_block(index), per_page);
        if first == 0 {
            return ReadStep::Hole;
        }
        // `try_reserve` above guarantees these pushes do not allocate.
        let _ = frames.try_push(frame);
        // The pages after it that are resident, found in one pass over the cache.
        let ahead = ahead.min(self.npages().saturating_sub(index + 1));
        let mut resident = 0u64;
        for p in g.pages.iter().filter(|p| p.index > index && p.index <= index + ahead) {
            resident |= 1 << (p.index - index - 1);
        }
        // A page reading as zero has no block (`0`), so it ends the run as a page elsewhere does.
        let mut n = 1;
        while n <= ahead && resident & (1 << (n - 1)) == 0 {
            let block = fill_block_in(&g.runs, first_block(index + n), per_page);
            if block * bs != first * bs + (n * PAGE_SIZE) as u64 || g.pages.try_reserve(1).is_err() {
                break;
            }
            let Some(f) = heap::buddy_alloc(0) else { break };
            g.pages
                .try_push(CachePage { index: index + n, frame: f, state: PageState::Loading, fill: Some(po.clone()) })
                .expect("slot reserved above");
            let _ = frames.try_push(f);
            n += 1;
        }
        g.io_in_flight += 1;
        ReadStep::Go(first, n)
    }

    /// **A write-back is about to write the run of pages from `start`**, decided afresh under the
    /// lock for each run, so a `Forget` or a resize that lands mid-write-back governs every run
    /// after it. The run is page `start` and each page after it that is resident and `Ready`,
    /// inside the file, and on the device right after the one before — at most `max`, the most the
    /// device takes in one command (Phase 6 Part H.4) — their frames left in `frames`, in order.
    /// A run to write is one IRP, counted in flight.
    fn begin_write(&self, start: usize, block_size: u32, max: usize, frames: &mut KVec<PhysAddr>) -> WriteStep {
        frames.clear();
        if frames.try_reserve(max).is_err() {
            return WriteStep::NoMemory;
        }
        let mut g = self.inner.lock();
        if g.dead {
            return WriteStep::Dead;
        }
        let npages = self.npages();
        let bs = block_size as u64;
        // Page `index`'s position in the cache, frame and device block, or `None` if it has nothing
        // to write: gone from the cache, still filling, past the end, or over a hole — growth goes
        // through a resolve, not write-back. Position `from` is looked at first: pages enter the
        // cache in the order they were faulted, which for a file read or written through is the
        // order of their indices, so a run is found without a scan a page.
        let target = |g: &Inner, index: usize, from: usize| -> Option<(usize, PhysAddr, u64)> {
            if index >= npages {
                return None;
            }
            let at = match g.pages.get(from) {
                Some(p) if p.index == index => from,
                _ => g.pages.iter().position(|p| p.index == index)?,
            };
            let p = &g.pages[at];
            let block = device_block_in(&g.runs, (index * PAGE_SIZE) as u64 / bs);
            (p.state == PageState::Ready && block != 0).then_some((at, p.frame, block))
        };
        let Some((mut at, frame, first)) = target(&g, start, 0) else {
            return WriteStep::Skip;
        };
        // `try_reserve` above guarantees these pushes do not allocate.
        let _ = frames.try_push(frame);
        while frames.len() < max {
            let k = frames.len();
            match target(&g, start + k, at + 1) {
                Some((next, frame, block)) if block * bs == first * bs + (k * PAGE_SIZE) as u64 => {
                    let _ = frames.try_push(frame);
                    at = next;
                }
                _ => break,
            }
        }
        g.io_in_flight += 1;
        WriteStep::Go(first, frames.len())
    }

    /// **An IRP counted by [`begin_fill`](Self::begin_fill) or
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
            // SAFETY: `answer` is the `Forget` answer's `PendingOperation` reference.
            unsafe { crate::sched::complete_pending_op(answer.as_ptr(), 0, 0) };
        }
    }

    /// **The file's server is about to free it** (`File::Forget`, administration Part C.1b).
    /// From now on no device I/O of the object starts: a write-back stops before its next
    /// run, and a fill reads as a hole — the `dead` mark alone decides both, so the run map is
    /// left as it was. The dirty pin is released, so the object goes when its users do, its
    /// pages unwritten. Its registration has already marked its cache entry forgotten
    /// ([`UserspaceServerReg::forget_file`]), so a later resolve of the id gets a new object.
    /// The caller's reference keeps it alive across this call, whatever the pin was.
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
        g.pages.iter().find(|p| p.index == index).map(|p| (p.frame, p.state))
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
    /// A failed fill leaves its pages out of the cache for the next fault to try again. **One
    /// that read ahead is first tried again for the faulted page alone** (Phase 6 Part H.4), as
    /// Linux does: the error may have been on a page after it, which this fault never asked for.
    /// **So is one a faulter waited on** (PR #371 review, finding 3): it may have been another
    /// fault's read-ahead, failed by a page this one never asked for either, and a faulter fails
    /// only when a fill of its page alone has. A faulter that cannot register on a PO — at
    /// `PendingOperation::MAX_WAITERS` — parks a millisecond and looks again, rather than
    /// spinning.
    pub fn fault_in_page(file_obj: &ObjectRef, index: usize) -> Option<PhysAddr> {
        Self::fault_in_page_with(file_obj, index, &mut wait_for_fill, &mut park_briefly)
    }

    /// [`fault_in_page`](Self::fault_in_page) with its waits given: `wait` blocks on a fill's PO
    /// and says whether the fill succeeded, and `park` passes a moment. The kernel's are the
    /// scheduler's; a host test's play the device and a second faulter.
    fn fault_in_page_with(
        file_obj: &ObjectRef,
        index: usize,
        wait: &mut dyn FnMut(&ObjectRef) -> bool,
        park: &mut dyn FnMut(),
    ) -> Option<PhysAddr> {
        debug_assert_eq!(file_obj.object_type(), KObjectType::FileObject);
        // SAFETY: `file_obj` pins a live `FileObject` (header at offset 0).
        let fo: &FileObject = unsafe { &*(file_obj.as_ptr() as *const FileObject) };
        // Whether this fault's fill may read ahead, and whether a failed fill may be tried again:
        // neither after a failure, so a fault fails at the second.
        let mut read_ahead = true;
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
                    let started_kind = fo.start_fill(file_obj, index, frame, &po, read_ahead);
                    if started_kind == FillStart::Failed {
                        // Could not start the fill (allocation failure); roll the reserved
                        // page back so a retry is clean. Nothing will complete `po`, so
                        // complete it here for anyone already waiting on it.
                        fo.settle(index, &po, false);
                        // SAFETY: `po` pins the fill's `PendingOperation`.
                        unsafe {
                            crate::sched::complete_pending_op(
                                po.as_ptr(),
                                crate::syscall::error::KError::OutOfMemory as i32,
                                0,
                            )
                        };
                        return None;
                    }
                    let ok = wait(&po);
                    // The faulted page, and any read ahead of it on the same PO.
                    let pages = match started_kind {
                        FillStart::DeviceIo(pages) => {
                            fo.finish_io();
                            pages
                        }
                        _ => 1,
                    };
                    for k in 0..pages {
                        fo.settle(index + k, &po, ok);
                    }
                    if !ok && pages > 1 {
                        read_ahead = false;
                        continue;
                    }
                    if !ok {
                        return None;
                    }
                    // A hole completes its PO synchronously with no device I/O; counting
                    // it as a fill would understate what a real fill costs.
                    if started_kind == FillStart::Hole {
                        fill_stats::HOLE_FILLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    } else {
                        fill_stats::record_fill(crate::arch::Timer::read_ns().saturating_sub(started), pages as u64);
                    }
                    // Loop: the page is now `Ready` → return its frame.
                }
                Fault::Wait(po) => {
                    fill_stats::SPINS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    let ok = wait(&po);
                    fo.settle(index, &po, ok);
                    if !ok && read_ahead {
                        // Perhaps another fault's read-ahead, failed past this page.
                        read_ahead = false;
                        continue;
                    }
                    if !ok {
                        return None;
                    }
                }
                Fault::Busy => park(),
                Fault::Oom => return None,
            }
        }
    }

    /// Flush every resident, block-backed page to the device (the **Model A** overwrite
    /// writeback). The `Ready` cache pages, in order of index, are translated through the
    /// producer's run map and written **a run of them a block write IRP** — pages next to each
    /// other in the file and on the device, up to the most the device takes in one command
    /// ([`begin_write`](Self::begin_write), Phase 6 Part H.4) — blocking on each. Pages over a
    /// hole (`device_lba == 0`, unallocated) are skipped — growing a file is Part D. `file_obj`
    /// is this object's reference (pins the frames across the IRPs). Runs in a syscall thread
    /// (it blocks). [`WriteBack::Written`] when every write succeeded,
    /// [`WriteBack::Gone`] when the device refused one `PeerClosed` — it has left, and no later
    /// write-back can do better — and [`WriteBack::Failed`] for a non-block producer or any
    /// other I/O or allocation failure.
    pub fn writeback(file_obj: &ObjectRef) -> WriteBack {
        debug_assert_eq!(file_obj.object_type(), KObjectType::FileObject);
        // SAFETY: `file_obj` pins a live `FileObject` (header at offset 0).
        let fo: &FileObject = unsafe { &*(file_obj.as_ptr() as *const FileObject) };
        // The producer is immutable; the run map is read under the lock with the pages.
        let (device, block_size) = match &fo.producer {
            Producer::FsServerBlocks { device, block_size, .. } => (device.clone(), *block_size),
            _ => return WriteBack::Failed,
        };
        let max = run_pages(crate::io::block::max_frags(&device));
        fo.write_runs(block_size, max, |frames, dev_offset, length| {
            let po = match PendingOperation::try_new() {
                // SAFETY: adopt the single creation reference.
                Ok(p) => unsafe {
                    ObjectRef::from_raw(KBox::into_raw(p).as_ptr() as *mut (), KObjectType::PendingOperation)
                },
                Err(_) => return crate::syscall::error::KError::OutOfMemory as i32,
            };
            let started = crate::arch::Timer::read_ns();
            if let Err(e) = crate::io::block::dispatch_block_irp_into_frames(
                &device,
                frames,
                file_obj.clone(),
                &po,
                crate::libkern::io_op::IoOpcode::Write,
                dev_offset,
                length,
            ) {
                return e as i32;
            }
            let status = wait_status(&po);
            fill_stats::record_writeback(frames.len() as u64, crate::arch::Timer::read_ns().saturating_sub(started));
            status
        })
    }

    /// **[`writeback`](Self::writeback)'s loop**, with the IRP left to `issue`: each run of pages
    /// [`begin_write`](Self::begin_write) decides, at most `max`, handed over as its frames, the
    /// device byte offset and the length, `issue` answering the IRP's completion status. Apart so a
    /// host test can drive it with a device that records what reached it.
    fn write_runs(
        &self,
        block_size: u32,
        max: usize,
        mut issue: impl FnMut(&[PhysAddr], u64, u64) -> i32,
    ) -> WriteBack {
        // Which pages are resident, under the lock, in order; each run is then decided afresh as
        // its IRP is issued, so a `Forget` stops the write-back before its next run and a resize
        // redirects it.
        let mut indices: KVec<usize> = KVec::new();
        {
            let inner = self.inner.lock();
            if indices.try_reserve(inner.pages.len()).is_err() {
                return WriteBack::Failed;
            }
            for p in inner.pages.iter().filter(|p| p.state == PageState::Ready) {
                let _ = indices.try_push(p.index);
            }
        }
        indices.sort_unstable();
        let mut frames: KVec<PhysAddr> = KVec::new();
        let mut at = 0;
        while at < indices.len() {
            let start = indices[at];
            let (dev_block, pages) = match self.begin_write(start, block_size, max, &mut frames) {
                WriteStep::Go(dev_block, pages) => (dev_block, pages),
                WriteStep::Skip => {
                    at += 1;
                    continue;
                }
                // Forgotten: what is unwritten stays so — its blocks are about to be freed.
                WriteStep::Dead => return WriteBack::Written,
                WriteStep::NoMemory => return WriteBack::Failed,
            };
            let status = issue(&frames, dev_block * block_size as u64, (pages * PAGE_SIZE) as u64);
            self.finish_io();
            match WriteBack::of(status) {
                WriteBack::Written => {}
                w => return w,
            }
            // Past the pages the run wrote. It may have taken pages faulted in since the list was
            // made, which is only an earlier write of them.
            while at < indices.len() && indices[at] < start + pages {
                at += 1;
            }
        }
        WriteBack::Written
    }

    /// **Let go of a dirty file whose device has gone** (Phase 6 Part D.3): a write-back its device
    /// refused `PeerClosed`, which no later one can do better. Its self-pin is dropped, outside the
    /// lock, so the file goes with its last user rather than staying for the boot with its pages,
    /// its device and its registration. How many of its pages were resident: what was not written.
    ///
    /// **Not marked dead**, as a `Forget` marks it: a dead file's fill reads a hole, and a fill of a
    /// departed device's file must fail — as it does, refused `PeerClosed` — not read as zeros.
    pub fn let_go(file_obj: &ObjectRef) -> usize {
        debug_assert_eq!(file_obj.object_type(), KObjectType::FileObject);
        // SAFETY: `file_obj` pins a live `FileObject` (header at offset 0).
        let fo: &FileObject = unsafe { &*(file_obj.as_ptr() as *const FileObject) };
        let (pin, pages) = {
            let mut g = fo.inner.lock();
            let pages = g.pages.iter().filter(|p| p.state == PageState::Ready).count();
            (g.self_pin.take(), pages)
        };
        drop(pin);
        pages
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
            let page = unsafe { core::slice::from_raw_parts((frame.as_u64() + heap::hhdm_offset()) as *const u8, n) };
            buf.try_extend_from_slice(page)?;
            remaining -= n;
        }
        Ok(buf)
    }

    /// Start an asynchronous fill of `frame` for page `index`, completing `po` when
    /// done (the producer also marks the page `Ready`). Dispatches on the object's
    /// [`Producer`]; a Model A fill reads pages ahead of it if `read_ahead`. `file_obj` is this object's reference (the deferred fill clones
    /// it to stay alive). Returns `false` if the fill could not be started. The stub
    /// producer enqueues a DPC; the fs-server producer (Part 3) sends a range-read.
    fn start_fill(
        &self,
        file_obj: &ObjectRef,
        index: usize,
        frame: PhysAddr,
        po: &ObjectRef,
        read_ahead: bool,
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
                self.model_a_start_fill(file_obj, index, frame, po, device, *block_size, read_ahead)
            }
        }
    }

    /// Model A fill: read page `index`'s device block **zero-copy** into `frame` via a block
    /// IRP (completing `po`), with the pages read ahead of it ([`begin_fill`](Self::begin_fill)),
    /// or — for a hole / block past the map — complete `po` at once (the reserved frame is
    /// already zeroed). `Failed` only on an allocation failure, the pages read ahead let go. The
    /// pages are marked `Ready` by the fault path after the wait.
    #[allow(clippy::too_many_arguments, reason = "the fill's object, page, frame, operation, device and run")]
    fn model_a_start_fill(
        &self,
        file_obj: &ObjectRef,
        index: usize,
        frame: PhysAddr,
        po: &ObjectRef,
        device: &ObjectRef,
        block_size: u32,
        read_ahead: bool,
    ) -> FillStart {
        // The page's first filesystem block is `index * PAGE / block_size`; the general form
        // handles bs | PAGE where a page's blocks are contiguous within one run. Its device block
        // (none for a hole, a page of blocks that read as zero, or a forgotten file) and the pages
        // read ahead of it are one IRP, counted in flight until the faulter ends it.
        let ahead = match read_ahead {
            true => READ_AHEAD_PAGES.min(run_pages(crate::io::block::max_frags(device))) - 1,
            false => 0,
        };
        let mut frames = KVec::new();
        match self.begin_fill(index, frame, block_size, ahead, po, &mut frames) {
            ReadStep::Hole => {
                // Hole or unmapped: the zeroed frame is already correct. Complete the PO
                // synchronously so the parked faulter wakes at once (no IRP).
                // SAFETY: `po` pins the fill's `PendingOperation`.
                unsafe { crate::sched::complete_pending_op(po.as_ptr(), 0, 0) };
                FillStart::Hole
            }
            ReadStep::NoMemory => FillStart::Failed,
            ReadStep::Go(dev_block, pages) => {
                // `file_obj` pins the FileObject (hence the frames) for the IRP's lifetime.
                match crate::io::block::dispatch_block_irp_into_frames(
                    device,
                    &frames,
                    file_obj.clone(),
                    po,
                    crate::libkern::io_op::IoOpcode::Read,
                    dev_block * block_size as u64,
                    (pages * PAGE_SIZE) as u64,
                ) {
                    Ok(()) => FillStart::DeviceIo(pages),
                    Err(_) => {
                        self.finish_io(); // never issued, so never in flight
                        // The pages read ahead go now; the faulted one, the caller settles.
                        for k in 1..pages {
                            self.settle(index + k, po, false);
                        }
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
        let body_len = match crate::rsproto::build_read_range_request(&mut msg.payload, offset, len, suffix.as_bytes())
        {
            Some(n) => n,
            None => return false,
        };
        msg.header.payload_len = body_len as u32;
        msg.header.handle_count = 0;
        // Originate: records the pending fill on `reg` and pushes the request. The
        // reply completes `po`; `Busy`/`Full`/`PeerClosed` fail this fault.
        matches!(
            // SAFETY: `reg` is a live `UserspaceServerReg` reference held across the call.
            unsafe { crate::sched::us_forward_originate_fill(reg.as_ptr(), &mut msg, po, file_obj, frame, index) },
            crate::sched::ForwardOutcome::Pending
        )
    }
}

/// Park the current thread on the fill `po` until it completes; `true` iff the fill
/// reported success (`status == 0`). Uses the scheduler's `wait_on` primitive — the
/// fast path returns at once if `po` already completed (no lost wakeup). `now = 0`
/// is fine: a no-deadline (`u64::MAX`) PO wait uses it only for the already-signalled
/// check, which a `PendingOperation` answers from its flag.
fn wait_status(po: &ObjectRef) -> i32 {
    // SAFETY: `po` pins the `PendingOperation` waited on.
    match unsafe { crate::sched::wait_on(&[po.as_ptr() as usize], u64::MAX, 0) } {
        // SAFETY: `po` is a live `PendingOperation` reference held across the call.
        crate::sched::WaitResult::Signaled(_) => unsafe { crate::sched::pending_op_completion(po.as_ptr()) }.0,
        // OutOfMemory (waiter registration failed); TimedOut cannot occur (no deadline).
        _ => crate::syscall::error::KError::OutOfMemory as i32,
    }
}

/// **What a write-back came to** (Phase 6 Part D.3).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WriteBack {
    /// Every resident page is on the device.
    Written,
    /// A page did not reach it; the file stays dirty, for a later write-back to try again.
    Failed,
    /// **Its device refused a page `PeerClosed`: it has gone**, and no later write-back can do
    /// better. The caller lets the file go ([`FileObject::let_go`]).
    Gone,
}

impl WriteBack {
    /// What a page's write, completed with `status`, makes of the write-back.
    pub fn of(status: i32) -> WriteBack {
        match status {
            0 => WriteBack::Written,
            s if s == crate::syscall::error::KError::PeerClosed as i32 => WriteBack::Gone,
            _ => WriteBack::Failed,
        }
    }
}

/// Wait for a page fill's `po` to complete; `true` iff it succeeded. A waiter that cannot
/// register — the PO already has `MAX_WAITERS` — parks and tries again, since the fill
/// completes without it: a fill is never abandoned for want of a waiter slot, and the retry
/// is a park, never a spin.
fn wait_for_fill(po: &ObjectRef) -> bool {
    loop {
        // SAFETY: `po` pins the `PendingOperation` waited on.
        match unsafe { crate::sched::wait_on(&[po.as_ptr() as usize], u64::MAX, 0) } {
            crate::sched::WaitResult::Signaled(_) => {
                // SAFETY: `po` is a live `PendingOperation` reference held across the call.
                return unsafe { crate::sched::pending_op_completion(po.as_ptr()) }.0 == 0;
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
    // SAFETY: no objects: a sleep until the deadline.
    let _ = unsafe { crate::sched::wait_on(&[], now + 1_000_000, now) };
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
fn stub_start_fill(file_obj: &ObjectRef, index: usize, frame: PhysAddr, po: &ObjectRef, base: u8) -> bool {
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
        core::ptr::write_bytes((bx.frame.as_u64() + heap::hhdm_offset()) as *mut u8, bx.fill_byte, PAGE_SIZE);
    }
    // SAFETY: `bx.file_obj` pins a live `FileObject`.
    let fo: &FileObject = unsafe { &*(bx.file_obj.as_ptr() as *const FileObject) };
    fo.mark_ready(bx.index);
    // SAFETY: `bx` owns the reference pinning its `PendingOperation`.
    unsafe { crate::sched::complete_pending_op(bx.po.as_ptr(), 0, 0) };
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
        if let Producer::FsServerBlocks { reg, file_id, .. } = &self.producer
            && *file_id != 0
        {
            // SAFETY: `reg` pins a live `UserspaceServerReg`.
            let r: &crate::object::UserspaceServerReg =
                unsafe { &*(reg.as_ptr() as *const crate::object::UserspaceServerReg) };
            r.cache_forget_object(self as *mut Self as *mut ());
        }
    }
}

/// The device block holding file block `file_block` in `runs`; `0` for a hole or a block
/// past the map.
pub(crate) fn device_block_in(runs: &KVec<BlockRun>, file_block: u64) -> u64 {
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

/// **The device block a fill of `blocks` file blocks from `file_block` reads** — the first one's
/// — or `0` when it reads nothing: the first is a hole or past the map, or **every one is a hole
/// or `UNWRITTEN`** ([`RUN_UNWRITTEN`]), which reads as zero. A page with one block a server has
/// not promised is zero is read whole, as before.
pub(crate) fn fill_block_in(runs: &KVec<BlockRun>, file_block: u64, blocks: u64) -> u64 {
    let first = device_block_in(runs, file_block);
    if first == 0 {
        return 0;
    }
    let zero = |b: u64| {
        runs.iter()
            .find(|r| b >= r.file_block && b < r.file_block + r.length as u64)
            .is_none_or(|r| r.device_lba == 0 || r.flags & RUN_UNWRITTEN != 0)
    };
    if (file_block..file_block + blocks.max(1)).all(zero) { 0 } else { first }
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
        let a = match f.reserve(0) {
            Reserve::New(fr) => fr,
            o => panic!("{o:?}"),
        };
        let b = match f.reserve(5) {
            Reserve::New(fr) => fr,
            o => panic!("{o:?}"),
        };
        let c = match f.reserve(3) {
            Reserve::New(fr) => fr,
            o => panic!("{o:?}"),
        };
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
        // SAFETY: adopts the peer's creation reference, to drop it.
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
        candidate_in(reg, id, size, 4096)
    }

    /// [`candidate`] in blocks of `block_size`.
    fn candidate_in(reg: &ObjectRef, id: u64, size: usize, block_size: u32) -> ObjectRef {
        let dev = DeviceNode::try_new(DeviceClass::Other, ResourceDescriptor::ZERO, BlockGeometry::ZERO).unwrap();
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        let device = unsafe { ObjectRef::from_raw(KBox::into_raw(dev).as_ptr() as *mut (), KObjectType::DeviceNode) };
        let f =
            FileObject::try_new(size, Producer::FsServerBlocks { device, block_size, reg: reg.clone(), file_id: id })
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
        let frame = match f.reserve(index) {
            Reserve::New(fr) => fr,
            o => panic!("{o:?}"),
        };
        for off in 0..PAGE_SIZE {
            write_frame_byte(frame, off, byte);
        }
        f.mark_ready(index);
        frame
    }

    /// **A reply that changes a cached file's block size is refused**, the cached object as it was
    /// (PR #365 review, finding 3). The kernel's map check reads each reply's runs against that
    /// reply's block size; the object keeps the one it was made with, so taking the second reply's
    /// runs would use them against a size nothing checked them for. The same size is taken as ever.
    #[test]
    fn a_reply_changing_a_cached_files_block_size_is_refused() {
        init_global_heap();
        let reg = registration();
        let first = FileObject::cache_in(&reg, candidate_in(&reg, 7, 2 * PAGE_SIZE, 4096)).unwrap();
        let changed = FileObject::cache_in(&reg, candidate_in(&reg, 7, PAGE_SIZE, 512));
        assert_eq!(changed.err(), Some(CacheInError::BlockSizeChanged));
        assert_eq!(file_of(&first).size(), 2 * PAGE_SIZE, "nothing resized");
        let same = FileObject::cache_in(&reg, candidate_in(&reg, 7, PAGE_SIZE, 4096)).unwrap();
        assert_eq!(same.as_ptr(), first.as_ptr());
        assert_eq!(file_of(&first).size(), PAGE_SIZE, "a reply in its own block size resizes it");
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
        // SAFETY: `a` pins the `FileObject`.
        unsafe { FileObject::writable_unmapped(a.as_ptr()) };
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
        // SAFETY: `a` pins the `FileObject`.
        unsafe { FileObject::writable_unmapped(a.as_ptr()) };
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
        // SAFETY: `a` pins the `FileObject`.
        unsafe { FileObject::writable_unmapped(a.as_ptr()) };
        assert!(fo.is_dirty(), "the sync that began mapped does not clean");

        // A mapping made and gone during the write-back moves the mark.
        let mark = fo.clean_mark().unwrap();
        FileObject::writable_mapped(&a);
        // SAFETY: `a` pins the `FileObject`.
        unsafe { FileObject::writable_unmapped(a.as_ptr()) };
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
        // SAFETY: `a` pins the `FileObject`.
        unsafe { FileObject::writable_unmapped(a.as_ptr()) };
        assert_eq!(reg_of(&reg).cached_files(), 0);
        // And the count saturates: a stray decrement does not wrap it.
        // SAFETY: `a` pins the `FileObject`.
        unsafe { FileObject::writable_unmapped(a.as_ptr()) };
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

    /// [`FileObject::begin_fill`] of page `index` in blocks of `block_size`, reading nothing ahead:
    /// the device block it reads, or `0` for none.
    fn read_one(f: &FileObject, index: usize, block_size: u32) -> u64 {
        match f.begin_fill(index, PhysAddr(0), block_size, 0, &po(), &mut KVec::new()) {
            ReadStep::Go(block, 1) => block,
            ReadStep::Hole => 0,
            other => panic!("{other:?}"),
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

    /// **A page of blocks a grow just allocated fills as zero without a read, and is written back
    /// to them** (Phase 6 Part H.4). `UNWRITTEN` is a server's promise that the blocks read as zero,
    /// so a fill of a page that is all such blocks issues no IRP. They are still the file's, so a
    /// write-back of that page goes to them: one lookup for both would read the flag as a hole and
    /// skip the write. A page holding one block the server did not promise is read whole, as before;
    /// a page of grown blocks and holes reads nothing.
    #[test]
    fn a_page_a_grow_just_allocated_fills_as_zero_without_a_read_and_is_written_back() {
        init_global_heap();
        let reg = registration();
        let run = |file_block, device_lba, length, flags| BlockRun { file_block, device_lba, length, flags };
        // Four-KiB blocks: page 0 the file's old block, pages 1 and 2 a grow's.
        let f = resolve(&reg, 7, 3 * PAGE_SIZE);
        let mut runs = KVec::new();
        runs.try_push(run(0, 100, 1, 0)).unwrap();
        runs.try_push(run(1, 101, 2, RUN_UNWRITTEN)).unwrap();
        file_of(&f).resize(3 * PAGE_SIZE, runs).unwrap();
        assert_eq!(read_one(file_of(&f), 0, 4096), 100, "the old block is read");
        file_of(&f).end_io();
        assert_eq!(read_one(file_of(&f), 1, 4096), 0, "a grown block is not");
        assert_eq!(read_one(file_of(&f), 2, 4096), 0);
        fill(file_of(&f), 1, 0xB1);
        assert!(
            matches!(file_of(&f).begin_write(1, 4096, 1, &mut KVec::new()), WriteStep::Go(101, 1)),
            "but is written back"
        );
        file_of(&f).end_io();

        // One-KiB blocks, four to a page: page 0 holds the old block and three of the grow's, page 1
        // four of the grow's.
        let g = FileObject::cache_in(&reg, candidate_in(&reg, 8, 2 * PAGE_SIZE, 1024)).unwrap();
        let mut runs = KVec::new();
        runs.try_push(run(0, 200, 1, 0)).unwrap();
        runs.try_push(run(1, 201, 7, RUN_UNWRITTEN)).unwrap();
        file_of(&g).resize(2 * PAGE_SIZE, runs).unwrap();
        assert_eq!(read_one(file_of(&g), 0, 1024), 200, "a page with one old block is read whole");
        file_of(&g).end_io();
        assert_eq!(read_one(file_of(&g), 1, 1024), 0, "a page of grown blocks is not");

        // Grown blocks beside a hole in one page read nothing; the flag alone does not make a hole.
        let mut runs = KVec::new();
        runs.try_push(run(0, 300, 2, RUN_UNWRITTEN)).unwrap();
        runs.try_push(run(2, 0, 2, 0)).unwrap();
        assert_eq!(fill_block_in(&runs, 0, 4), 0);
        assert_eq!(device_block_in(&runs, 1), 301, "a write-back's lookup ignores the flag");
    }

    /// File `id` of `reg`, `pages` pages in blocks of `block_size`, mapped by `map` — `(file block,
    /// device block, length)` — with each page of `resident` filled and ready, in that order. The
    /// frames by page index.
    fn mapped_file(
        reg: &ObjectRef,
        id: u64,
        pages: usize,
        block_size: u32,
        map: &[(u64, u64, u32)],
        resident: impl Iterator<Item = usize>,
    ) -> (ObjectRef, std::collections::BTreeMap<usize, PhysAddr>) {
        let f = FileObject::cache_in(reg, candidate_in(reg, id, pages * PAGE_SIZE, block_size)).unwrap();
        let mut runs = KVec::new();
        for &(file_block, device_lba, length) in map {
            runs.try_push(BlockRun { file_block, device_lba, length, flags: 0 }).unwrap();
        }
        file_of(&f).resize(pages * PAGE_SIZE, runs).unwrap();
        let frames = resident.map(|i| (i, fill(file_of(&f), i, i as u8))).collect();
        (f, frames)
    }

    /// One IRP a write-back issued: its frames, device byte offset and length.
    type Irp = (std::vec::Vec<PhysAddr>, u64, u64);

    /// [`FileObject::write_runs`] over a device that takes each IRP, and what reached it.
    fn write_runs(f: &FileObject, block_size: u32, max: usize) -> (WriteBack, std::vec::Vec<Irp>) {
        let mut irps = std::vec::Vec::new();
        let w = f.write_runs(block_size, max, |frames, offset, length| {
            irps.push((frames.to_vec(), offset, length));
            0
        });
        (w, irps)
    }

    /// The IRP that writes pages `pages` from device block `block` of `block_size` bytes.
    fn irp(
        frames: &std::collections::BTreeMap<usize, PhysAddr>,
        pages: core::ops::Range<usize>,
        block: u64,
        block_size: u64,
    ) -> Irp {
        let length = (pages.len() * PAGE_SIZE) as u64;
        (pages.map(|i| frames[&i]).collect(), block * block_size, length)
    }

    /// **A write-back sends a run of pages an IRP, up to the device's limit** (Phase 6 Part H.4): a
    /// hundred pages next to each other in the file and on the device go as an IRP of 64 and one
    /// of 36 to a device taking 64, and as one of 100 to a device taking more — the frames in the
    /// order of the file whatever order the pages were faulted in, here the reverse. And nothing is
    /// left in flight, so a `Forget` after it is answered at once.
    #[test]
    fn a_write_back_sends_a_run_of_pages_an_irp_up_to_the_devices_limit() {
        init_global_heap();
        let reg = registration();
        let (f, frames) = mapped_file(&reg, 7, 100, 4096, &[(0, 1000, 100)], (0..100).rev());
        let (w, irps) = write_runs(file_of(&f), 4096, 64);
        assert_eq!(w, WriteBack::Written);
        assert_eq!(irps, [irp(&frames, 0..64, 1000, 4096), irp(&frames, 64..100, 1064, 4096)]);
        let (_, irps) = write_runs(file_of(&f), 4096, RUN_MAX_PAGES);
        assert_eq!(irps, [irp(&frames, 0..100, 1000, 4096)]);
        assert!(matches!(file_of(&f).forget(&po()), Forgotten::Now), "nothing in flight");
    }

    /// **A run ends where the next page has nothing to write beside it**: a gap on the device, a
    /// hole, a page not in the cache, a page still filling, and the file's end — pages a mapping
    /// faulted in past it are never written. The pages after each are written as runs of their
    /// own.
    #[test]
    fn a_run_ends_at_a_gap_on_the_device_a_hole_a_missing_or_filling_page_and_the_end() {
        init_global_heap();
        let reg = registration();
        // Pages 0-9 at device block 1000, 10-19 at 2000, 20-23 a hole, 24-39 at 3000.
        let map = [(0, 1000, 10), (10, 2000, 10), (20, 0, 4), (24, 3000, 16)];
        let (f, mut frames) = mapped_file(&reg, 7, 40, 4096, &map, (0..40).filter(|&i| i != 30 && i != 33));
        let fo = file_of(&f);
        assert!(matches!(fo.reserve(33), Reserve::New(_)), "page 33 filling");
        // The file cut to 38 pages, and pages 38 and 39 faulted in again past its end.
        let mut runs = KVec::new();
        for &(file_block, device_lba, length) in &map {
            runs.try_push(BlockRun { file_block, device_lba, length, flags: 0 }).unwrap();
        }
        fo.resize(38 * PAGE_SIZE, runs).unwrap();
        frames.insert(38, fill(fo, 38, 38));
        frames.insert(39, fill(fo, 39, 39));
        let (w, irps) = write_runs(fo, 4096, 64);
        assert_eq!(w, WriteBack::Written);
        assert_eq!(
            irps,
            [
                irp(&frames, 0..10, 1000, 4096),
                irp(&frames, 10..20, 2000, 4096),
                irp(&frames, 24..30, 3000, 4096),
                irp(&frames, 31..33, 3007, 4096),
                irp(&frames, 34..38, 3010, 4096),
            ]
        );
    }

    /// **A `Forget` mid-write-back stops it before its next run, and a refusal ends it**: the run in
    /// flight is the last written, and the `Forget` is answered when it ends; a device gone
    /// (`PeerClosed`) or failing ends the write-back with that run.
    #[test]
    fn a_forget_or_a_refusal_mid_write_back_stops_it_at_the_next_run() {
        init_global_heap();
        let reg = registration();
        let (f, _) = mapped_file(&reg, 7, 100, 4096, &[(0, 1000, 100)], 0..100);
        let fo = file_of(&f);
        let (mut issued, mut forgotten) = (0, None);
        let w = fo.write_runs(4096, 10, |_, _, _| {
            issued += 1;
            if issued == 2 {
                forgotten = Some(fo.forget(&po()));
            }
            0
        });
        assert_eq!((w, issued), (WriteBack::Written, 2));
        assert!(matches!(forgotten, Some(Forgotten::Later(_))), "the second run was in flight");
        assert!(matches!(fo.forget(&po()), Forgotten::Now), "and its end answered it");

        for (id, status, outcome) in [
            (8, crate::syscall::error::KError::PeerClosed as i32, WriteBack::Gone),
            (9, crate::syscall::error::KError::InvalidArgument as i32, WriteBack::Failed),
        ] {
            let (g, _) = mapped_file(&reg, id, 100, 4096, &[(0, 2000, 100)], 0..100);
            let mut issued = 0;
            let w = file_of(&g).write_runs(4096, 10, |_, _, _| {
                issued += 1;
                if issued == 2 { status } else { 0 }
            });
            assert_eq!((w, issued), (outcome, 2));
            assert!(matches!(file_of(&g).forget(&po()), Forgotten::Now), "the refused run ended");
        }
    }

    /// **In blocks smaller than a page a run is still pages next to each other on the device**:
    /// eight pages of four 1 KiB blocks each, mapped by one run, go as one IRP; mapped by two runs
    /// apart, as one each.
    #[test]
    fn a_run_of_pages_in_one_kib_blocks_is_one_irp_while_the_device_is_contiguous() {
        init_global_heap();
        let reg = registration();
        let (f, frames) = mapped_file(&reg, 7, 8, 1024, &[(0, 400, 32)], 0..8);
        assert_eq!(write_runs(file_of(&f), 1024, 64).1, [irp(&frames, 0..8, 400, 1024)]);
        let (g, frames) = mapped_file(&reg, 8, 8, 1024, &[(0, 400, 16), (16, 900, 16)], 0..8);
        assert_eq!(write_runs(file_of(&g), 1024, 64).1, [irp(&frames, 0..4, 400, 1024), irp(&frames, 4..8, 900, 1024)]);
    }

    /// **A run holds as many pages as the device takes fragments**, a page a fragment, never more
    /// than [`RUN_MAX_PAGES`] and never none: USB storage's 64, AHCI's 248, the RAM disk's no
    /// limit.
    #[test]
    fn a_run_holds_the_devices_fragments_at_most_run_max_pages_and_at_least_one() {
        assert_eq!(run_pages(Some(64)), 64);
        assert_eq!(run_pages(Some(248)), 248);
        assert_eq!(run_pages(Some(RUN_MAX_PAGES as u32)), RUN_MAX_PAGES);
        assert_eq!(run_pages(Some(RUN_MAX_PAGES as u32 + 1)), RUN_MAX_PAGES);
        assert_eq!(run_pages(Some(u32::MAX)), RUN_MAX_PAGES);
        assert_eq!(run_pages(Some(0)), 1);
        assert_eq!(run_pages(None), 1);
    }

    /// The fault's half of a fill of page `index`: its reservation, as [`FileObject::reserve_fault`]
    /// makes it — the frame and the fill's PO.
    fn miss(f: &FileObject, index: usize) -> (PhysAddr, ObjectRef) {
        match f.reserve_fault(index) {
            Fault::Fill(frame, po) => (frame, po),
            _ => panic!("page {index} was not a miss"),
        }
    }

    /// A fill of page `index` reading up to `ahead` pages after it: what [`FileObject::begin_fill`]
    /// said, and the frames it read into. Every page of it is then settled ready and the IRP ended,
    /// as the fault path does once the read completes.
    fn fill_ahead(f: &FileObject, index: usize, block_size: u32, ahead: usize) -> (ReadStep, std::vec::Vec<PhysAddr>) {
        let (frame, fill_po) = miss(f, index);
        let mut frames = KVec::new();
        let step = f.begin_fill(index, frame, block_size, ahead, &fill_po, &mut frames);
        let pages = match step {
            ReadStep::Go(_, pages) => {
                f.end_io();
                pages
            }
            _ => 1,
        };
        for k in 0..pages {
            f.settle(index + k, &fill_po, true);
        }
        (step, frames.to_vec())
    }

    /// **A fill reads ahead the pages after it, in one IRP, until the device or the file stops it**
    /// (Phase 6 Part H.4): from page 0 the 31 after it, each reserved `Loading` on the fault's PO —
    /// so a fault on one waits on that fill — and its frame after the faulted page's; from page 32
    /// eight, to where the file's blocks jump on the device; from page 96 the four left in the file,
    /// though its blocks run on past its end.
    #[test]
    fn a_fill_reads_ahead_in_one_irp_until_the_device_or_the_file_stops_it() {
        init_global_heap();
        let reg = registration();
        let (f, _) = mapped_file(&reg, 7, 100, 4096, &[(0, 1000, 40), (40, 5000, 60)], 0..0);
        let fo = file_of(&f);
        let (frame, fill_po) = miss(fo, 0);
        let mut frames = KVec::new();
        assert_eq!(fo.begin_fill(0, frame, 4096, 31, &fill_po, &mut frames), ReadStep::Go(1000, 32));
        assert_eq!(frames[0], frame, "the faulted page's frame first");
        for (k, &fr) in frames.iter().enumerate().skip(1) {
            assert_eq!(fo.lookup(k), Some((fr, PageState::Loading)), "page {k} reserved into its frame");
        }
        assert!(
            matches!(fo.reserve_fault(5), Fault::Wait(w) if w.as_ptr() == fill_po.as_ptr()),
            "a fault on one waits on the fill"
        );
        assert!(matches!(fo.forget(&po()), Forgotten::Later(_)), "one IRP in flight");
        let (f, _) = mapped_file(&reg, 8, 100, 4096, &[(0, 1000, 40), (40, 5000, 64)], 0..0);
        let fo = file_of(&f);
        assert_eq!(fill_ahead(fo, 0, 4096, 31).0, ReadStep::Go(1000, 32));
        assert_eq!(fo.lookup(31).map(|(_, s)| s), Some(PageState::Ready), "settled with the faulted page");
        assert_eq!(fill_ahead(fo, 32, 4096, 31).0, ReadStep::Go(1032, 8), "to the jump on the device");
        assert_eq!(fill_ahead(fo, 40, 4096, 31).0, ReadStep::Go(5000, 32));
        assert_eq!(fill_ahead(fo, 96, 4096, 31).0, ReadStep::Go(5056, 4), "to the file's end");
        assert!(matches!(fo.forget(&po()), Forgotten::Now), "each fill one IRP, each ended");
    }

    /// **Read-ahead stops at a page already in the cache, at blocks that read as zero, and at the
    /// map's end**, and a fill with nothing to read ahead reads its page alone: a resident page,
    /// whatever its state, is never read over; a run a grow just allocated (`UNWRITTEN`) or a hole
    /// fills without a read when its own page faults; a device taking one fragment gets one page.
    #[test]
    fn read_ahead_stops_at_a_resident_page_blocks_that_read_as_zero_and_the_maps_end() {
        init_global_heap();
        let reg = registration();
        let f = resolve(&reg, 7, 40 * PAGE_SIZE);
        let run = |file_block, device_lba, length, flags| BlockRun { file_block, device_lba, length, flags };
        let mut runs = KVec::new();
        for r in [run(0, 1000, 20, 0), run(20, 1020, 4, RUN_UNWRITTEN), run(24, 0, 4, 0), run(28, 1028, 10, 0)] {
            runs.try_push(r).unwrap();
        }
        let fo = file_of(&f);
        fo.resize(40 * PAGE_SIZE, runs).unwrap();
        fill(fo, 5, 5);
        assert!(matches!(fo.reserve(9), Reserve::New(_)), "page 9 filling, by another path");
        assert_eq!(fill_ahead(fo, 0, 4096, 31).0, ReadStep::Go(1000, 5), "to the ready page 5");
        assert_eq!(fill_ahead(fo, 6, 4096, 31).0, ReadStep::Go(1006, 3), "to the loading page 9");
        assert_eq!(fill_ahead(fo, 10, 4096, 31).0, ReadStep::Go(1010, 10), "to the grown blocks");
        assert_eq!(fill_ahead(fo, 20, 4096, 31).0, ReadStep::Hole, "which fill without a read");
        assert_eq!(fill_ahead(fo, 24, 4096, 31).0, ReadStep::Hole, "as a hole does");
        assert_eq!(fill_ahead(fo, 28, 4096, 31).0, ReadStep::Go(1028, 10), "to the map's end");
        assert_eq!(fill_ahead(fo, 38, 4096, 31).0, ReadStep::Hole, "past it");
        let (g, _) = mapped_file(&reg, 8, 10, 4096, &[(0, 2000, 10)], 0..0);
        assert_eq!(fill_ahead(file_of(&g), 0, 4096, 0).0, ReadStep::Go(2000, 1), "nothing ahead");
    }

    /// **In blocks smaller than a page, read-ahead is pages next to each other on the device**:
    /// sixteen pages of four 1 KiB blocks, mapped by one run, read in one IRP; a forgotten file
    /// reads nothing, ahead or not.
    #[test]
    fn read_ahead_in_one_kib_blocks_and_never_for_a_forgotten_file() {
        init_global_heap();
        let reg = registration();
        let (f, _) = mapped_file(&reg, 7, 16, 1024, &[(0, 400, 64)], 0..0);
        assert_eq!(fill_ahead(file_of(&f), 0, 1024, 31).0, ReadStep::Go(400, 16));
        let (g, _) = mapped_file(&reg, 8, 16, 1024, &[(0, 400, 64)], 0..0);
        let _ = file_of(&g).forget(&po());
        let (step, frames) = fill_ahead(file_of(&g), 0, 1024, 31);
        assert_eq!((step, frames.len()), (ReadStep::Hole, 0));
        assert_eq!(file_of(&g).resident_pages(), 1, "nothing reserved ahead");
    }

    /// **A Model A fill goes to the device as one IRP of its page and those read ahead, as many as
    /// the device takes**: through `start_fill`, on a device describing eight fragments a command,
    /// a fault on page 0 of a 20-page file reads eight pages from its first block — offset, length
    /// and a fragment a frame, in order — and its completion completes the fault's PO. A fill told
    /// not to read ahead, as a fault retrying after a failed one is, reads its page alone.
    #[test]
    fn a_fill_reads_its_page_and_those_ahead_in_one_irp_within_the_devices_limit() {
        use crate::io::block::BlockBackend;
        use crate::io::irp::{Irp, PhysFrag};
        use crate::libkern::block::BlockKind;
        use core::cell::RefCell;
        init_global_heap();
        /// Each IRP that reached the device, with its fragments.
        struct Seen(RefCell<std::vec::Vec<(*mut Irp, std::vec::Vec<PhysFrag>)>>);
        fn disk_submit(irp: *mut Irp, ctx: *mut ()) {
            // SAFETY: `ctx` is the test's `Seen`; `irp` is a live IRP whose buffer describes `count`
            // fragments.
            unsafe {
                let b = (*irp).buffer;
                let frags = core::slice::from_raw_parts(b.frags as *const PhysFrag, b.count as usize).to_vec();
                (*(ctx as *const Seen)).0.borrow_mut().push((irp, frags));
            }
        }
        fn disk_poll(_: *mut ()) {}
        let seen = Seen(RefCell::new(std::vec::Vec::new()));
        let backend =
            BlockBackend { submit: disk_submit, poll: disk_poll, ctx: &seen as *const Seen as *mut (), max_frags: 8 };
        let geometry = BlockGeometry { logical_block_size: 512, block_count: 1 << 20 };
        let node =
            DeviceNode::try_new_block(ResourceDescriptor::ZERO, geometry, BlockKind::Disk, b"disk", backend).unwrap();
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        let device = unsafe { ObjectRef::from_raw(KBox::into_raw(node).as_ptr() as *mut (), KObjectType::DeviceNode) };
        let reg = registration();
        let producer = Producer::FsServerBlocks { device, block_size: 4096, reg: reg.clone(), file_id: 7 };
        let cand = FileObject::try_new(20 * PAGE_SIZE, producer).unwrap();
        // SAFETY: as above.
        let cand = unsafe { ObjectRef::from_raw(KBox::into_raw(cand).as_ptr() as *mut (), KObjectType::FileObject) };
        let f = FileObject::cache_in(&reg, cand).unwrap();
        let mut runs = KVec::new();
        runs.try_push(BlockRun { file_block: 0, device_lba: 1000, length: 20, flags: 0 }).unwrap();
        file_of(&f).resize(20 * PAGE_SIZE, runs).unwrap();

        let (frame, fill_po) = miss(file_of(&f), 0);
        assert_eq!(file_of(&f).start_fill(&f, 0, frame, &fill_po, true), FillStart::DeviceIo(8));
        let (irp, frags) = seen.0.borrow_mut().pop().expect("one IRP");
        assert!(seen.0.borrow().is_empty());
        // SAFETY: the IRP is live until it is completed below.
        let (op, offset, length) = unsafe { ((*irp).op, (*irp).offset, (*irp).length) };
        assert_eq!((op, offset, length), (crate::io::irp::IrpOp::Read as u32, 1000 * 4096, 8 * PAGE_SIZE as u64));
        let expected: std::vec::Vec<u64> = (0..8).map(|i| file_of(&f).lookup(i).unwrap().0.as_u64()).collect();
        assert_eq!(
            frags.iter().map(|fr| fr.base).collect::<std::vec::Vec<_>>(),
            expected,
            "a fragment a page, in order"
        );
        assert_eq!(expected[0], frame.as_u64());
        // SAFETY: as above; the device's side of a completion, a success of the whole length.
        unsafe { ((*irp).status, (*irp).transferred) = (0, length) };
        // SAFETY: `irp` is the recording device's in-flight IRP.
        unsafe { crate::io::block::complete_for_test(irp) };
        // SAFETY: `fill_po` is a live `PendingOperation` reference held across the call.
        let completed = unsafe { crate::sched::pending_op_completion(fill_po.as_ptr()) };
        assert_eq!(completed, (0, length), "the fault's PO completed");

        let (frame, fill_po) = miss(file_of(&f), 8);
        assert_eq!(file_of(&f).start_fill(&f, 8, frame, &fill_po, false), FillStart::DeviceIo(1));
        let (irp, frags) = seen.0.borrow_mut().pop().expect("one IRP");
        // SAFETY: the IRP is live until it is completed below.
        let (offset, length) = unsafe { ((*irp).offset, (*irp).length) };
        assert_eq!((offset, length, frags.len()), (1008 * 4096, PAGE_SIZE as u64, 1), "page 8 alone");
        assert_eq!(file_of(&f).lookup(9), None, "nothing reserved after it");
        // SAFETY: as above.
        unsafe { ((*irp).status, (*irp).transferred) = (0, length) };
        // SAFETY: `irp` is the recording device's in-flight IRP.
        unsafe { crate::io::block::complete_for_test(irp) };
        crate::io::block::reclaim_completed();
    }

    /// The IRPs a [`held_disk_file`]'s disk took and has not completed.
    struct Held(core::cell::RefCell<std::vec::Vec<*mut crate::io::irp::Irp>>);

    /// **A 20-page file on a disk that holds each IRP for the test to complete**: one run at
    /// device block 1000 in 4 KiB blocks, and eight fragments an IRP.
    fn held_disk_file(held: &Held) -> ObjectRef {
        use crate::io::block::BlockBackend;
        use crate::io::irp::Irp;
        use crate::libkern::block::BlockKind;
        fn disk_submit(irp: *mut Irp, ctx: *mut ()) {
            // SAFETY: `ctx` is the test's `Held`.
            unsafe { (*(ctx as *const Held)).0.borrow_mut().push(irp) };
        }
        fn disk_poll(_: *mut ()) {}
        let backend =
            BlockBackend { submit: disk_submit, poll: disk_poll, ctx: held as *const Held as *mut (), max_frags: 8 };
        let geometry = BlockGeometry { logical_block_size: 512, block_count: 1 << 20 };
        let node =
            DeviceNode::try_new_block(ResourceDescriptor::ZERO, geometry, BlockKind::Disk, b"disk", backend).unwrap();
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        let device = unsafe { ObjectRef::from_raw(KBox::into_raw(node).as_ptr() as *mut (), KObjectType::DeviceNode) };
        let reg = registration();
        let producer = Producer::FsServerBlocks { device, block_size: 4096, reg: reg.clone(), file_id: 7 };
        let cand = FileObject::try_new(20 * PAGE_SIZE, producer).unwrap();
        // SAFETY: as above.
        let cand = unsafe { ObjectRef::from_raw(KBox::into_raw(cand).as_ptr() as *mut (), KObjectType::FileObject) };
        let f = FileObject::cache_in(&reg, cand).unwrap();
        let mut runs = KVec::new();
        runs.try_push(BlockRun { file_block: 0, device_lba: 1000, length: 20, flags: 0 }).unwrap();
        file_of(&f).resize(20 * PAGE_SIZE, runs).unwrap();
        f
    }

    /// **The disk's side, then the scheduler's**: every IRP `held` completes, failed if it reads
    /// page `bad`, each recorded in `reads` as (first page, pages); then whether `po`'s fill
    /// succeeded, as a fault woken from it learns.
    fn complete_held(
        held: &Held,
        reads: &core::cell::RefCell<std::vec::Vec<(u64, u64)>>,
        bad: u64,
        po: &ObjectRef,
    ) -> bool {
        let irps: std::vec::Vec<_> = held.0.borrow_mut().drain(..).collect();
        for irp in irps {
            // SAFETY: the IRP is live until it is completed here; this is the device's side.
            unsafe {
                let (first, pages) = ((*irp).offset / 4096 - 1000, (*irp).length / PAGE_SIZE as u64);
                reads.borrow_mut().push((first, pages));
                ((*irp).status, (*irp).transferred) = if (first..first + pages).contains(&bad) {
                    (crate::syscall::error::KError::IoError as i32, 0)
                } else {
                    (0, (*irp).length)
                };
            }
            // SAFETY: `irp` is the recording device's in-flight IRP.
            unsafe { crate::io::block::complete_for_test(irp) };
        }
        assert!(reads.borrow().len() < 16, "a fault that never gives up");
        // SAFETY: `po` is a live `PendingOperation` reference held across the call.
        unsafe { crate::sched::pending_op_completion(po.as_ptr()) }.0 == 0
    }

    /// **A fault waiting on another's read-ahead is not failed by a page it never asked for** (PR
    /// #371 review, finding 3). Three faults of one file, as processes running one binary make:
    /// page 0's reads ahead to page 7, and before it wakes, faults of pages 3 and 5 wait on that
    /// read. The disk fails any read of page 5. Each fault tries its page again alone: page 3's
    /// and page 0's have it, and page 5's fails, after one read alone and no more.
    #[test]
    fn a_failed_read_ahead_fails_only_the_faults_of_the_page_that_failed() {
        use core::cell::RefCell;
        init_global_heap();
        let held = Held(RefCell::new(std::vec::Vec::new()));
        let f = held_disk_file(&held);
        let reads = RefCell::new(std::vec::Vec::new());
        let disk = |po: &ObjectRef| complete_held(&held, &reads, 5, po);

        let (mut page_3, mut page_5) = (None, None);
        let mut first = true;
        let mut wait_0 = |po: &ObjectRef| -> bool {
            let ok = disk(po);
            if core::mem::take(&mut first) {
                // Before page 0's fault wakes, the others fault: each page is still its read's.
                for k in [3, 5] {
                    match file_of(&f).reserve_fault(k) {
                        Fault::Wait(on) => assert_eq!(on.as_ptr(), po.as_ptr(), "page {k} waits on page 0's read"),
                        _ => panic!("page {k} is not being read"),
                    }
                }
                page_3 = FileObject::fault_in_page_with(&f, 3, &mut |po| disk(po), &mut || panic!());
                page_5 = FileObject::fault_in_page_with(&f, 5, &mut |po| disk(po), &mut || panic!());
            }
            ok
        };
        let page_0 = FileObject::fault_in_page_with(&f, 0, &mut wait_0, &mut || panic!());

        assert_eq!(*reads.borrow(), [(0, 8), (3, 1), (5, 1), (0, 1)], "the read ahead, then each page alone");
        for (k, got) in [(0, page_0), (3, page_3)] {
            assert!(got.is_some(), "page {k}'s fault failed for page 5's error");
            assert_eq!(file_of(&f).lookup(k), got.map(|frame| (frame, PageState::Ready)));
        }
        assert_eq!(page_5, None, "page 5 does not read");
        assert_eq!(file_of(&f).lookup(5), None, "and is left for the next fault to try");
        crate::io::block::reclaim_completed();
    }

    /// **A fault fails at its second failed fill, waited on or its own.** Page 5's fault waits on
    /// page 0's read-ahead, which fails; before it wakes, page 0's fault has let its pages go and
    /// another fault of page 5 has started a read of its own, so the first finds page 5 being
    /// read again and waits on that. It fails too, and the fault fails, rather than reading page 5
    /// a third time.
    #[test]
    fn a_fault_fails_at_its_second_failed_fill() {
        use core::cell::RefCell;
        init_global_heap();
        let held = Held(RefCell::new(std::vec::Vec::new()));
        let f = held_disk_file(&held);
        let reads = RefCell::new(std::vec::Vec::new());
        let fo = file_of(&f);

        // Page 0's fault, by hand: its read ahead to page 7, page 5 among them.
        let (frame, read_0) = miss(fo, 0);
        assert_eq!(fo.start_fill(&f, 0, frame, &read_0, true), FillStart::DeviceIo(8));
        let mut read_5 = None;
        let mut waits = 0;
        let mut wait = |po: &ObjectRef| -> bool {
            waits += 1;
            let ok = complete_held(&held, &reads, 5, po);
            if waits == 1 {
                // Page 0's fault wakes first and lets its pages go, as `fault_in_page` does.
                assert!(!ok);
                fo.finish_io();
                for k in 0..8 {
                    fo.settle(k, &read_0, false);
                }
                // Another fault of page 5 starts its read.
                let (frame, po) = miss(fo, 5);
                assert_eq!(fo.start_fill(&f, 5, frame, &po, true), FillStart::DeviceIo(8));
                read_5 = Some(po);
            }
            ok
        };
        assert_eq!(FileObject::fault_in_page_with(&f, 5, &mut wait, &mut || panic!()), None);
        assert_eq!(*reads.borrow(), [(0, 8), (5, 8)], "two reads, both failed, and no third");
        // The other fault of page 5 lets its read go.
        fo.finish_io();
        let read_5 = read_5.unwrap();
        for k in 5..13 {
            fo.settle(k, &read_5, false);
        }
        crate::io::block::reclaim_completed();
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
        // SAFETY: `a` pins the `FileObject`.
        unsafe { FileObject::writable_unmapped(a.as_ptr()) };
        assert!(file_of(&a).is_dirty());
        assert!(matches!(file_of(&a).begin_write(0, 4096, 1, &mut KVec::new()), WriteStep::Go(100, 1)));
        assert!(file_of(&a).end_io().is_none(), "nothing forgotten yet");

        assert!(matches!(reg_of(&reg).forget_file(7, &po()), Forgotten::Now), "nothing in flight");
        assert_eq!(reg_of(&reg).cached_files(), 0, "so its entry went at once");
        assert!(file_of(&a).is_dead());
        assert!(!file_of(&a).is_dirty(), "the pin is gone");
        assert!(matches!(file_of(&a).begin_write(0, 4096, 1, &mut KVec::new()), WriteStep::Dead));
        assert_eq!(read_one(file_of(&a), 0, 4096), 0, "a fill reads a hole, not a freed block");
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
        assert!(
            matches!(fo.begin_write(0, 4096, 1, &mut KVec::new()), WriteStep::Go(100, 1)),
            "page 0's IRP is issued"
        );
        let answer = po();
        let Forgotten::Later(wait_on) = fo.forget(&answer) else { panic!("an IRP is in flight") };
        assert_eq!(wait_on.as_ptr(), answer.as_ptr());
        assert!(matches!(fo.begin_write(1, 4096, 1, &mut KVec::new()), WriteStep::Dead), "page 1 is not written");
        // A second `Forget` of the file waits on the same answer.
        let Forgotten::Later(second) = fo.forget(&po()) else { panic!() };
        assert_eq!(second.as_ptr(), answer.as_ptr());
        let done = fo.end_io().expect("the last IRP ending hands the answer back");
        assert_eq!(done.as_ptr(), answer.as_ptr());
        assert!(fo.end_io().is_none(), "and only once");
    }

    /// **A second `Forget` through the registration waits on the first one's answer** — the
    /// path a server's sends take, which the test above does not. The first `Forget` used to
    /// take the entry out, so a second found nothing and was answered at once with the IRP
    /// still in flight (PR #335 review, finding 1).
    #[test]
    fn a_second_forget_waits_on_the_first_ones_answer() {
        init_global_heap();
        let reg = registration();
        let a = two_block_file(&reg, 7);
        let fo = file_of(&a);
        assert!(
            matches!(fo.begin_write(0, 4096, 1, &mut KVec::new()), WriteStep::Go(100, 1)),
            "page 0's IRP is issued"
        );
        let answer = po();
        let Forgotten::Later(first) = reg_of(&reg).forget_file(7, &answer) else { panic!("an IRP is in flight") };
        assert_eq!(first.as_ptr(), answer.as_ptr());
        let Forgotten::Later(second) = reg_of(&reg).forget_file(7, &po()) else {
            panic!("a second Forget found nothing to wait on")
        };
        assert_eq!(second.as_ptr(), answer.as_ptr(), "the first one's answer");
        assert_eq!(fo.end_io().map(|p| p.as_ptr()), Some(answer.as_ptr()));
        assert!(matches!(reg_of(&reg).forget_file(7, &po()), Forgotten::Now), "nothing in flight now");
        assert_eq!(reg_of(&reg).cached_files(), 0, "and the entry has gone");
    }

    /// **A file forgotten mid-I/O is found by `Forget` alone.** A resolve of its id gets a new
    /// object and a sync passes it by. A `Forget` after that means the new file, while the old
    /// one's answer still waits for its IRP.
    #[test]
    fn a_file_forgotten_mid_io_is_found_by_no_resolve_or_sync() {
        init_global_heap();
        let reg = registration();
        let a = two_block_file(&reg, 7);
        assert_eq!(read_one(file_of(&a), 1, 4096), 101);
        let answer = po();
        assert!(matches!(reg_of(&reg).forget_file(7, &answer), Forgotten::Later(_)));
        assert_eq!(reg_of(&reg).cache_objects().unwrap().len(), 0, "no sync sees it");
        let again = resolve(&reg, 7, PAGE_SIZE);
        assert_ne!(again.as_ptr(), a.as_ptr(), "a resolve gets a new object");
        assert!(matches!(reg_of(&reg).forget_file(7, &po()), Forgotten::Now), "the new file has nothing in flight");
        assert!(file_of(&again).is_dead(), "and it is the one forgotten");
        assert_eq!(
            file_of(&a).end_io().map(|p| p.as_ptr()),
            Some(answer.as_ptr()),
            "the old answer still waited for its IRP"
        );
    }

    /// **A fill in flight holds the answer too**: its read was issued against a block the
    /// server has not freed yet, and must land before it does.
    #[test]
    fn a_fill_in_flight_holds_the_forget_answer() {
        init_global_heap();
        let reg = registration();
        let a = two_block_file(&reg, 7);
        let fo = file_of(&a);
        assert_eq!(read_one(fo, 1, 4096), 101);
        let answer = po();
        assert!(matches!(fo.forget(&answer), Forgotten::Later(_)));
        assert_eq!(read_one(fo, 0, 4096), 0, "no read starts after it");
        assert_eq!(fo.end_io().map(|p| p.as_ptr()), Some(answer.as_ptr()));
    }

    /// **What `sys_ns_held` counts: the cache's live files, a forgotten one aside** (administration
    /// Part C.5c). A file nothing holds leaves the cache at once. A dirty one holds itself, and goes
    /// once a write-back cleans it, which is why the storage service asks after a sync. A file
    /// something holds stays, and that is the answer an unmount is refused on.
    #[test]
    fn the_held_count_is_what_a_sync_leaves_alive() {
        init_global_heap();
        let reg = registration();
        let count = || reg_of(&reg).cache_objects().unwrap().len();
        let held = two_block_file(&reg, 7);
        let dirty = two_block_file(&reg, 8);
        let gone = two_block_file(&reg, 9);
        assert_eq!(count(), 3);
        drop(gone);
        assert_eq!(count(), 2, "a file nothing holds leaves the cache");

        FileObject::writable_mapped(&dirty);
        // SAFETY: `dirty` pins the `FileObject`.
        unsafe { FileObject::writable_unmapped(dirty.as_ptr()) };
        let mark = file_of(&dirty).clean_mark().expect("no writable mapping remains");
        let dirty_at = dirty.as_ptr();
        drop(dirty);
        assert_eq!(count(), 2, "a dirty file holds itself");
        // What a write-back that cleans ends with, without its IRPs.
        let live = reg_of(&reg).cache_objects().unwrap();
        let pinned = live.iter().find(|f| f.as_ptr() == dirty_at).unwrap().clone();
        drop(live);
        FileObject::unpin_if_clean(&pinned, mark);
        drop(pinned);
        assert_eq!(count(), 1, "cleaned, it goes");

        // Forgotten with a read in flight, so the entry stays — marked, and not counted.
        assert_eq!(read_one(file_of(&held), 1, 4096), 101);
        let answer = po();
        assert!(matches!(reg_of(&reg).forget_file(7, &answer), Forgotten::Later(_)));
        assert_eq!(reg_of(&reg).cached_files(), 1, "the entry stays while the read is in flight");
        assert_eq!(count(), 0, "a forgotten file is not counted, though something holds it");
        assert!(file_of(&held).end_io().is_some());
        drop(held);
    }

    /// **A write-back its device refused `PeerClosed` lets the file go** (Phase 6 Part D.3): only that
    /// refusal is for good, so only it leads to [`FileObject::let_go`] — which drops the pin and
    /// says what was resident, and **leaves the file alive**: unlike a forgotten one, its next fill
    /// still asks its device, and fails, rather than reading a hole.
    #[test]
    fn a_write_back_refused_for_good_lets_the_file_go_and_no_other() {
        init_global_heap();
        assert_eq!(WriteBack::of(0), WriteBack::Written);
        assert_eq!(WriteBack::of(crate::syscall::error::KError::PeerClosed as i32), WriteBack::Gone);
        assert_eq!(
            WriteBack::of(crate::syscall::error::KError::IoError as i32),
            WriteBack::Failed,
            "a disk that may answer again"
        );
        let reg = registration();
        let a = two_block_file(&reg, 7);
        FileObject::writable_mapped(&a);
        // SAFETY: `a` pins the `FileObject`.
        unsafe { FileObject::writable_unmapped(a.as_ptr()) };
        assert!(file_of(&a).is_dirty());
        assert_eq!(FileObject::let_go(&a), 2, "both pages resident, unwritten");
        assert!(!file_of(&a).is_dirty(), "the pin is gone");
        assert!(!file_of(&a).is_dead(), "but not forgotten");
        assert_ne!(read_one(file_of(&a), 1, 4096), 0, "a fill still reads its block, not a hole");
    }

    /// No sync writes a forgotten object, so a writable mapping must not pin it: nothing would
    /// ever clean it.
    #[test]
    fn a_forgotten_file_is_never_pinned() {
        init_global_heap();
        let reg = registration();
        let a = two_block_file(&reg, 7);
        let _ = reg_of(&reg).forget_file(7, &po());
        FileObject::writable_mapped(&a);
        assert!(!file_of(&a).is_dirty());
        // SAFETY: `a` pins the `FileObject`.
        unsafe { FileObject::writable_unmapped(a.as_ptr()) };
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
