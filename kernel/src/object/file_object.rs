//! The [`FileObject`] kernel object — a mapped file's content, paged in on demand.
//!
//!
//! Where a [`MemoryObject`](crate::object::MemoryObject) is RAM committed **eagerly**, a
//! `FileObject` owns a sparse **page cache**: frames allocated and filled when a page is first
//! faulted, freed with the object. `sys_memory_map` builds a lazy
//! [`MappingKind::FileBacked`](crate::mm::vmm::MappingKind) VMA; the `#PF` handler faults pages
//! in through the cache. See `docs/architecture/memory-management.md`.
//!
//! [`fault_in_page`](FileObject::fault_in_page) reserves a frame on a miss, starts the
//! [`Producer`] fill and **parks the faulting thread** on the fill's `PendingOperation`,
//! *after* the `#PF` handler has released the address-space lock, so it holds no AS/cache lock.
//!
//! The cache carries its **own** rank-4 [`SpinLock`], acquired by the fault path *after* it
//! drops the address-space lock (never nested). It may allocate a frame under it (rank 4 →
//! rank-6 buddy) but never blocks under it.
//!
//! A Model A file whose server gave it an id is **cached in its registration** and every
//! resolve shares the object ([`resize`]d in place on a grow, create or truncate); the cache
//! indexes objects weakly, so a clean object goes with its last user. **A dirty object holds a
//! reference to itself** — mapped writable since the last write-back that began with no
//! writable mapping and saw none made — so a writer that exits without syncing loses nothing:
//! `sys_ns_sync` enumerates the cache and writes it back. Dirty is per object, not per page,
//! until `TODO(page-dirty-tracking)`.
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

/// How a [`FileObject`] **fills** a cache page on a fault.
pub enum Producer {
    /// Test producer: fills page `i` with the byte `base + i`, asynchronously via a DPC.
    Stub { base: u8 },
    /// Model B — a **non-block** filesystem: fill a page by sending a `File::ReadRange` to the
    /// resource server through the [`UserspaceServerReg`] `reg`'s forwarding endpoint. `reg` pins
    /// the registration; `suffix` names the file under the mount.
    ///
    /// [`UserspaceServerReg`]: crate::object::UserspaceServerReg
    FsServer { reg: ObjectRef, suffix: KString },
    /// Model A — a **block** filesystem: the kernel owns the file-data path. The run map (under
    /// the object's lock, since a size change replaces it) maps the file's blocks to device LBAs;
    /// a fault reads the page's block **zero-copy** from `device` into the cache frame via a block
    /// IRP. See `docs/architecture/filesystem-data-path.md`.
    ///
    /// `reg` and `file_id` are the file's identity: **a non-zero `file_id` means the object is in
    /// `reg`'s file cache** (`Drop` takes it out), and they name the file back to its server for
    /// the one thing it cannot learn otherwise — that it was **written**. An in-place overwrite
    /// never reaches the server, so the kernel sends `File::Touch` by id after a write-back
    /// ([`FileObject::writeback`]).
    FsServerBlocks { device: ObjectRef, block_size: u32, reg: ObjectRef, file_id: u64 },
}

/// One contiguous mapping from a file's blocks to the device (the kernel-side mirror of the
/// wire `BlockRun`, `docs/spec/rsproto-block-ops.md`); `device_lba` `0` is a hole (reads zero).
#[derive(Copy, Clone)]
pub struct BlockRun {
    pub file_block: u64,
    pub device_lba: u64,
    pub length: u32,
    pub flags: u32,
}

/// [`BlockRun::flags`] bit 0, **`UNWRITTEN`**: the run's blocks read as zero (a grow's reply
/// marks what it just allocated), so a fill of a page of only such blocks or holes reads
/// nothing. They are still the file's, so a write-back writes them: a fill and a write-back
/// look a block up differently ([`fill_block_in`], [`device_block_in`]).
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
    /// A frame is allocated (zeroed) and a fill is in flight: **not** yet safe to map.
    Loading,
    /// The frame holds the file's bytes (a last page's tail is zero-padding): safe to map.
    Ready,
}

struct CachePage {
    index: usize,
    frame: PhysAddr,
    state: PageState,
    /// While a fault's fill is in flight, **its `PendingOperation`**, which a second faulter
    /// blocks on; taken out when the page [settles](FileObject::settle). Never the last reference
    /// while here — the filler holds its own — so dropping it under the lock only decrements.
    fill: Option<ObjectRef>,
}

enum Fault {
    Hit(PhysAddr),
    Fill(PhysAddr, ObjectRef),
    Wait(ObjectRef),
    /// Loading with no fill to wait on (reserved outside the fault path, as host tests do).
    Busy,
    Oom,
}

/// Outcome of [`FileObject::reserve`] — what the fault path should do for a page.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Reserve {
    /// The page is cached and [`Ready`](PageState::Ready): map `frame`.
    Ready(PhysAddr),
    /// A fill is in flight (an earlier fault reserved it): wait for it.
    Loading(PhysAddr),
    /// A fresh, **zeroed** frame reserved (state `Loading`): the caller fills it, then calls
    /// [`mark_ready`](FileObject::mark_ready).
    New(PhysAddr),
    /// No frame (or cache slot) could be allocated.
    Oom,
}

/// A mapped file's content, backed by a demand-filled page cache.
#[repr(C)]
pub struct FileObject {
    header: KObjectHeader,
    magic: u64,
    /// Exact file size in bytes; the mapped range is bounded by it. **Changes in place** for a
    /// cached Model A file ([`resize`](Self::resize)), so it is atomic.
    size: AtomicUsize,
    /// Writable mappings that exist now: raised by [`writable_mapped`](Self::writable_mapped)
    /// before the mapping is made, lowered when such a VMA drops — atomic, since a VMA drops
    /// wherever its address space does and must take no lock here.
    writable_maps: AtomicU32,
    producer: Producer,
    inner: SpinLock<Inner>,
}

struct Inner {
    /// Cached pages. Sparse and unsorted; lookups are a linear scan. Each entry **owns** its
    /// frame (freed in [`FileObject::drop`]).
    pages: KVec<CachePage>,
    /// Frames a truncation took out of `pages`; a mapping that faulted one in may still point at
    /// it, so they are freed only with the object.
    retired: KVec<PhysAddr>,
    /// A Model A file's run map; empty for any other producer.
    runs: KVec<BlockRun>,
    /// **The object's own reference to itself, held while it is dirty** — from a writable mapping
    /// until a write-back that finds none left. It keeps a file whose writer exited without
    /// syncing alive in its registration's cache (which indexes objects only weakly) until a sync
    /// or unmount writes it. Taken only for a cached object, the kind a sync can find.
    self_pin: Option<ObjectRef>,
    /// Raised by every writable mapping; see [`clean_mark`](FileObject::clean_mark).
    map_gen: u64,
    /// **The file's server is freeing it** ([`forget`](FileObject::forget)). No device I/O of the
    /// object starts once this is set: a write-back stops and a fill reads as a hole, since the
    /// blocks are about to be someone else's.
    dead: bool,
    /// Device IRPs of this object issued and not yet ended (write-backs, Model A fills): raised
    /// under this lock with `dead` checked, lowered by the issuer ([`end_io`](FileObject::end_io)).
    io_in_flight: u32,
    /// The `File::Forget` answer waiting for `io_in_flight` to reach zero.
    forget_answer: Option<ObjectRef>,
}

#[derive(Debug)]
enum WriteStep {
    /// Write this many pages from this device block; the IRP is counted in flight.
    Go(u64, usize),
    Skip,
    Dead,
    NoMemory,
}

/// **The most pages one write-back IRP carries**: 1 MiB, about what Linux sends a SATA disk in
/// one command. A device's own limit (AHCI 248 fragments, USB storage 64) usually governs;
/// this bounds the RAM disk.
pub const RUN_MAX_PAGES: usize = 256;

fn run_pages(max_frags: Option<u32>) -> usize {
    max_frags.map_or(1, |m| (m as usize).clamp(1, RUN_MAX_PAGES))
}

/// **The most pages a Model A fill reads**, the faulted page and those after it: 128 KiB,
/// Linux's default read-ahead. A file read through reads in commands of this size, not a page
/// at a time; a reader that wanted one page pays for the rest in memory.
pub const READ_AHEAD_PAGES: usize = 32;
const _: () = assert!(READ_AHEAD_PAGES >= 1 && READ_AHEAD_PAGES <= 64, "the resident pages' mask is a u64");

#[derive(Debug, PartialEq, Eq)]
enum ReadStep {
    /// This many pages from this device block; their frames are in the caller's list.
    Go(u64, usize),
    Hole,
    NoMemory,
}

/// The outcome of [`FileObject::forget`].
pub enum Forgotten {
    /// Nothing of the file is in flight: the server may be answered now.
    Now,
    /// I/O of the file is in flight. The server is answered by completing this PO, which
    /// [`end_io`](FileObject::end_io) hands back when the last of it ends (the caller's own, or the
    /// one an earlier `Forget` of the file already waits on).
    Later(ObjectRef),
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum FillStart {
    Io,
    /// A Model A block IRP, **counted in the object's I/O in flight**: the faulter that issued it
    /// ends it ([`FileObject::finish_io`]). It reads this many pages, each `Loading` on its PO.
    DeviceIo(usize),
    Hole,
    Failed,
}

/// Page-cache fill and write-back counters, the measurement behind the read-ahead decision.
/// Always on: two atomic adds per fill are nothing beside the block read they bracket.
pub mod fill_stats {
    use core::sync::atomic::{AtomicU64, Ordering};

    /// Fills that issued a real producer request (a block IRP under Model A).
    pub static FILLS: AtomicU64 = AtomicU64::new(0);
    /// The pages those fills brought in: one a fill, and under Model A those read ahead.
    pub static FILL_PAGES: AtomicU64 = AtomicU64::new(0);
    /// Fills that resolved to a hole: counted apart, since averaging them in would flatter the
    /// real cost.
    pub static HOLE_FILLS: AtomicU64 = AtomicU64::new(0);
    /// Total nanoseconds spent inside fills that issued I/O (reserve → page Ready).
    pub static FILL_NS: AtomicU64 = AtomicU64::new(0);
    /// The slowest single such fill, in nanoseconds.
    pub static FILL_MAX_NS: AtomicU64 = AtomicU64::new(0);
    /// Faults that found the page already resident (the cache hit path).
    pub static HITS: AtomicU64 = AtomicU64::new(0);
    /// Faults that found the page mid-fill and `yield_now`-spun waiting for it.
    pub static SPINS: AtomicU64 = AtomicU64::new(0);
    /// Pages written back to their files' blocks.
    pub static WRITEBACK_PAGES: AtomicU64 = AtomicU64::new(0);
    /// The write IRPs that carried them: one per run of pages.
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

    /// Read the counters. Relaxed: they are statistics.
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

    pub(crate) fn record_fill(ns: u64, pages: u64) {
        FILLS.fetch_add(1, Ordering::Relaxed);
        FILL_PAGES.fetch_add(pages, Ordering::Relaxed);
        FILL_NS.fetch_add(ns, Ordering::Relaxed);
        FILL_MAX_NS.fetch_max(ns, Ordering::Relaxed);
    }

    pub(crate) fn record_writeback(pages: u64, ns: u64) {
        WRITEBACK_PAGES.fetch_add(pages, Ordering::Relaxed);
        WRITEBACK_IRPS.fetch_add(1, Ordering::Relaxed);
        WRITEBACK_NS.fetch_add(ns, Ordering::Relaxed);
    }
}

impl FileObject {
    /// Sentinel written into [`FileObject::magic`] at construction.
    pub const MAGIC: u64 = 0x46_69_6c_65_4f_62_6a_21; // "FileObj!"

    /// Allocate an empty `FileObject` of `size` bytes whose pages `producer` fills on fault.
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

    /// Whether the object is dirty: holding its own reference is what dirty *is*.
    pub fn is_dirty(&self) -> bool {
        self.inner.lock().self_pin.is_some()
    }

    /// **A writable mapping of `file_obj` is about to be made.** Counted, and a cached object pins
    /// itself: it stays alive and findable in its registration's cache until a write-back finds no
    /// writable mapping left, however soon its users let go. Called **before** the address-space
    /// lock (this takes the object's own lock; they never nest); a mapping that then fails calls
    /// [`writable_unmapped`](Self::writable_unmapped). The pin is kept even then: a spurious dirty
    /// mark costs one write-back of clean pages, where a missing one costs data.
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

    /// A writable mapping of the object at `obj` has gone. Lock-free, since a VMA drops wherever
    /// its address space is torn down; saturating, so a never-counted VMA takes no one's count.
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
    /// frame, then go away before the write-back ends, and a check made only at the end would call
    /// that write written. The mark is the mapping generation, which any writable mapping made
    /// during the write-back moves; pass it to [`unpin_if_clean`](Self::unpin_if_clean). A writer
    /// that syncs while still mapped leaves its file dirty, so `libfs` and `nxsh` unmap first.
    pub fn clean_mark(&self) -> Option<u64> {
        let g = self.inner.lock();
        if self.writable_maps.load(Ordering::Acquire) == 0 { Some(g.map_gen) } else { None }
    }

    /// After a successful write-back that began at `mark`: **clean**, unless a writable mapping
    /// was made since. Clean drops the self-pin outside the lock, as it may be the last reference.
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

    /// **Change the file's size in place**, and its run map with it — the reply to a grow, create
    /// or truncate of a cached file. Everything past the smaller of the two sizes reads as zero,
    /// as the filesystem says: pages wholly past it leave the index for `retired` (a mapping that
    /// faulted one in keeps its frame, but no fault finds it and no write-back writes it), and the
    /// page holding it is zeroed from it to the page's end. On a grow that is the old end: a
    /// mapping made before an earlier shrink can write past the end and even fault pages in there
    /// (the fault path bounds an index by the mapping, not the size), and `reserve` hits by index,
    /// so those bytes would be served in the regrown range and written back into the blocks the
    /// grow allocated.
    ///
    /// A retired frame is freed only with the object: no reverse map says which page tables still
    /// point at it (`TODO(retired-frames)` in `deferred-decisions.md`). An IRP already in flight
    /// can land in a block the truncate freed (`TODO(truncate-inflight-writeback)`); every later
    /// one respects the new size. `Err` only if `retired` cannot grow (nothing changed).
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

    /// **The object a block-file reply installs.** `candidate` is built from the reply for the
    /// file its id names. If `reg` already caches a live object for that file, that object takes
    /// the candidate's size and run map ([`resize`](Self::resize)) and is returned and the
    /// candidate dropped; otherwise the candidate is entered in the cache and returned. Every
    /// resolve of one file gets one object.
    ///
    /// The candidate drops outside the cache's lock, since its `Drop` takes it. `Err` on
    /// allocation failure, with nothing entered or resized; and for a reply whose block size is
    /// not the cached object's (a producer's is fixed, and the reply's runs were checked only
    /// against its own), with nothing resized.
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
                // end of the call, and `resize` takes the other object's lock — nested page-cache
                // locks, which the rank checker refuses (it is inert under `cfg(test)`).
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
    /// **And it reads ahead**: up to `ahead` following pages, each not resident, inside the file,
    /// right after the one before on the device and not reading as zero — reserved `Loading` on
    /// the fault's `po`, so a fault on one waits on this fill. A resident page ends the run,
    /// whatever its state. Their frames follow `frame` in `frames`, unzeroed: the read fills each.
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
    /// lock for each run, so a `Forget` or resize that lands mid-write-back governs every run
    /// after it: page `start` and each following page that is resident, `Ready`, inside the file
    /// and right after the one before on the device — at most `max` — their frames left in
    /// `frames`. A run to write is one IRP, counted in flight.
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
        // Page `index`'s position in the cache, frame and device block; `None` if gone, still
        // filling, past the end, or over a hole. `from` is tried first: a file read or written
        // through faults in index order, so a run is found without a scan a page.
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

    /// **An IRP counted by [`begin_fill`](Self::begin_fill) or [`begin_write`](Self::begin_write)
    /// has completed.** If it was the last in flight of a forgotten file, the `Forget` answer comes
    /// back for the caller to complete — outside this lock, since completing takes the scheduler's.
    pub fn end_io(&self) -> Option<ObjectRef> {
        let mut g = self.inner.lock();
        g.io_in_flight = g.io_in_flight.saturating_sub(1);
        if g.dead && g.io_in_flight == 0 { g.forget_answer.take() } else { None }
    }

    fn finish_io(&self) {
        if let Some(answer) = self.end_io() {
            // SAFETY: `answer` is the `Forget` answer's `PendingOperation` reference.
            unsafe { crate::sched::complete_pending_op(answer.as_ptr(), 0, 0) };
        }
    }

    /// **The file's server is about to free it** (`File::Forget`). From now on no device I/O of
    /// the object starts: a write-back stops before its next run, and a fill reads as a hole. The
    /// dirty pin is released, so the object goes when its users do, its pages unwritten. Its
    /// registration has already marked its cache entry forgotten
    /// ([`UserspaceServerReg::forget_file`]), so a later resolve of the id gets a new object. The
    /// caller's reference keeps it alive across this call.
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

    #[cfg(test)]
    pub(crate) fn is_dead(&self) -> bool {
        self.inner.lock().dead
    }

    #[cfg(test)]
    pub(crate) fn resident_pages(&self) -> usize {
        self.inner.lock().pages.len()
    }

    #[cfg(test)]
    pub(crate) fn retired_frames(&self) -> usize {
        self.inner.lock().retired.len()
    }

    #[cfg(test)]
    pub(crate) fn writable_maps(&self) -> u32 {
        self.writable_maps.load(Ordering::Acquire)
    }

    /// Look up page `index`: its frame + [`PageState`] if cached, else `None`.
    pub fn lookup(&self, index: usize) -> Option<(PhysAddr, PageState)> {
        let g = self.inner.lock();
        g.pages.iter().find(|p| p.index == index).map(|p| (p.frame, p.state))
    }

    /// Reserve page `index` for the fault path: [`Reserve::Ready`] on a hit, [`Reserve::Loading`]
    /// for a fill in flight; on a miss a **zeroed** frame (so a tail page's padding is zero)
    /// inserted `Loading`, as [`Reserve::New`] for the caller to fill and
    /// [`mark_ready`](Self::mark_ready). [`Reserve::Oom`] if no frame or slot is free.
    pub fn reserve(&self, index: usize) -> Reserve {
        let mut g = self.inner.lock();
        if let Some(p) = g.pages.iter().find(|p| p.index == index) {
            return match p.state {
                PageState::Ready => Reserve::Ready(p.frame),
                PageState::Loading => Reserve::Loading(p.frame),
            };
        }
        // Reserve the slot before allocating the frame, so a `try_push` cannot fail and leak it.
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

    /// The fault path's [`reserve`](Self::reserve): a miss also makes the fill's `PendingOperation`
    /// and records it on the page for a second faulter to wait on. Both allocations are under the
    /// lock, which ranks above the allocators.
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
    /// otherwise out of the cache with its frame freed. Idempotent: the filler or a second faulter
    /// that woke first may call it. Only the page whose fill *is* `po` is touched: a
    /// [`resize`](Self::resize) can retire a page mid-fill and a later fault reserve the index
    /// afresh, and settling by index alone would call that new page ready while its own fill still
    /// ran. Matched by the PO, not the frame: every caller holds a reference to the PO, so its
    /// address cannot be reused meanwhile, where a freed frame's can.
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

    /// Transition page `index` from `Loading` to `Ready`; a no-op if absent or already `Ready`.
    pub fn mark_ready(&self, index: usize) {
        let mut g = self.inner.lock();
        if let Some(p) = g.pages.iter_mut().find(|p| p.index == index) {
            p.state = PageState::Ready;
        }
    }

    /// **Fault page `index` in**, blocking until it is resident: a hit returns at once; a miss
    /// reserves a frame, starts the producer fill, and **parks the calling thread** on the fill's
    /// `PendingOperation`. `None` on a frame/PO allocation failure or a failed fill. Called from
    /// the page-fault handler **after** the address-space lock is released, so it holds no
    /// AS/cache lock. `file_obj` is the caller's reference to *this* object, which the deferred
    /// fill keeps alive.
    ///
    /// **A second faulter of a page being filled waits on that fill's PO**, not a `yield_now`
    /// spin: the handler runs with interrupts off and `yield_now` returns at once when nothing
    /// else is ready, so the CPU never acknowledged a TLB shootdown and the machine stopped.
    /// Whoever wakes first settles the page. A failed fill leaves its pages out of the cache for
    /// the next fault to retry; **one that read ahead, or that a faulter waited on, is first
    /// retried for the faulted page alone**, as the error may have been on a page it never asked
    /// for. A faulter that cannot register on a PO (`MAX_WAITERS`) parks a millisecond and looks
    /// again.
    pub fn fault_in_page(file_obj: &ObjectRef, index: usize) -> Option<PhysAddr> {
        Self::fault_in_page_with(file_obj, index, &mut wait_for_fill, &mut park_briefly)
    }

    /// [`fault_in_page`](Self::fault_in_page) with its waits given, so host tests play the device.
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
                    let started = crate::arch::Timer::read_ns();
                    let started_kind = fo.start_fill(file_obj, index, frame, &po, read_ahead);
                    if started_kind == FillStart::Failed {
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
                    // A hole does no device I/O; counting it as a fill would understate their cost.
                    if started_kind == FillStart::Hole {
                        fill_stats::HOLE_FILLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    } else {
                        fill_stats::record_fill(crate::arch::Timer::read_ns().saturating_sub(started), pages as u64);
                    }
                }
                Fault::Wait(po) => {
                    fill_stats::SPINS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    let ok = wait(&po);
                    fo.settle(index, &po, ok);
                    if !ok && read_ahead {
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
    /// writeback), a run of index-adjacent, device-adjacent pages per **block write IRP**
    /// ([`begin_write`](Self::begin_write)), blocking on each; pages over a hole are skipped.
    /// `file_obj` pins the frames across the IRPs. [`WriteBack::Written`] when every write
    /// succeeded, [`WriteBack::Gone`] when the device refused one `PeerClosed` (it has left, and no
    /// later write-back can do better), else [`WriteBack::Failed`].
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

    /// **[`writeback`](Self::writeback)'s loop**, with the IRP left to `issue` (handed each run's
    /// frames, byte offset and length; answers the status), so a host test can record each IRP.
    fn write_runs(
        &self,
        block_size: u32,
        max: usize,
        mut issue: impl FnMut(&[PhysAddr], u64, u64) -> i32,
    ) -> WriteBack {
        // The resident pages, in order; each run is decided afresh as its IRP is issued, so a
        // `Forget` stops the write-back before its next run and a resize redirects it.
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

    /// **Let go of a dirty file whose device has gone** (a write-back refused `PeerClosed`). The
    /// self-pin is dropped, outside the lock, so the file goes with its last user rather than
    /// staying for the boot with its pages, device and registration. Returns how many pages were
    /// resident: what was not written.
    ///
    /// **Not marked dead**, as a `Forget` marks it: a dead file's fill reads a hole, but a fill of
    /// a departed device's file must fail (refused `PeerClosed`), not read as zeros.
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
    /// Model A file; `None` for any other. For the post-writeback `File::Touch`, which must reach
    /// a server *without* a resolve — by id, since a cached object outlives the resolve that named
    /// it. The clone pins the registration across the send.
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

    /// Materialize the whole file into a fresh heap buffer of [`size`](Self::size) bytes via
    /// [`fault_in_page`](Self::fault_in_page), **blocking on each fill**, so only where blocking
    /// is allowed (a syscall thread, not a DPC/IRQ). `sys_process_spawn` loads an ELF with it.
    /// `AllocError` on a buffer-allocation or fill failure.
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

    /// Start an asynchronous fill of `frame` for page `index`, completing `po`, by dispatching on
    /// the [`Producer`]. `file_obj` is this object's reference (the deferred fill clones it).
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

    /// Model A fill: read page `index`'s device block **zero-copy** into `frame` via a block IRP
    /// (completing `po`), with the pages read ahead ([`begin_fill`](Self::begin_fill)); a hole or a
    /// block past the map completes `po` at once. `Failed` only on an allocation failure, the
    /// pages read ahead let go. The fault path marks the pages `Ready` after the wait.
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
        // Its device block (none for a hole, a page of blocks that read as zero, or a forgotten
        // file) and the pages read ahead of it are one IRP, in flight until the faulter ends it.
        let ahead = match read_ahead {
            true => READ_AHEAD_PAGES.min(run_pages(crate::io::block::max_frags(device))) - 1,
            false => 0,
        };
        let mut frames = KVec::new();
        match self.begin_fill(index, frame, block_size, ahead, po, &mut frames) {
            ReadStep::Hole => {
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

    /// Start a real fs-server fill: send a `File::ReadRange` for page `index`'s byte range over
    /// the server's forwarding endpoint (`reg`) and leave the fill **pending** — the reply copies
    /// the bytes into `frame`, marks the page ready, and completes `po`. `false` if the request
    /// could not be built or sent (the caller rolls the page back).
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
        // A heap-bounced message, allocated zeroed **in place**: building it on the stack takes 4
        // KiB of kernel stack.
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

/// Park the current thread on the fill `po` until it completes, and answer its completion
/// status (`0` = success). The `wait_on` fast path returns at once if `po` already completed.
fn wait_status(po: &ObjectRef) -> i32 {
    // SAFETY: `po` pins the `PendingOperation` waited on.
    match unsafe { crate::sched::wait_on(&[po.as_ptr() as usize], u64::MAX, 0) } {
        // SAFETY: `po` is a live `PendingOperation` reference held across the call.
        crate::sched::WaitResult::Signaled(_) => unsafe { crate::sched::pending_op_completion(po.as_ptr()) }.0,
        // OutOfMemory (waiter registration failed); TimedOut cannot occur (no deadline).
        _ => crate::syscall::error::KError::OutOfMemory as i32,
    }
}

/// **What a write-back came to.**
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
/// register (`MAX_WAITERS`) parks and retries: the fill completes without it, and never spins.
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

/// Park for a millisecond — a sleep with a deadline on no object, so the CPU runs something
/// else or idles with interrupts on; a `yield_now` from the fault handler returns at once.
fn park_briefly() {
    let now = crate::arch::Timer::read_ns();
    // SAFETY: no objects: a sleep until the deadline.
    let _ = unsafe { crate::sched::wait_on(&[], now + 1_000_000, now) };
}

/// A self-test fill in flight: the DPC and what it needs, heap-boxed so its `Dpc` has a stable
/// address. The DPC fills `frame`, marks the page `Ready`, completes `po`, and frees the box.
struct StubFillBox {
    dpc: Dpc,
    file_obj: ObjectRef,
    po: ObjectRef,
    frame: PhysAddr,
    index: usize,
    fill_byte: u8,
}

/// Start a stub fill (page `index` ← the byte `base + index`) by enqueuing a DPC. `false` on
/// box-allocation failure. The box clones `file_obj` and `po` so they outlive the deferred fill.
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

/// DPC: write the stub byte into the cache frame, mark the page `Ready`, complete the PO.
fn stub_fill_dpc(ctx: *mut ()) {
    // Dropping `bx` here releases two `ObjectRef`s in DPC context, which the block path stopped
    // doing (decision log, 2026-08-06): a last-reference drop reaches `SlabCache::free` and can
    // deadlock against an allocator holder on the same CPU. Safe *here* only because
    // `Producer::Stub` is built solely from `#[cfg(test)]` code, so this never runs on a boot.
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
}

impl Drop for FileObject {
    /// Free every cached frame (raw `PhysAddr`s). Runs when the last reference releases, with no
    /// lock held on entry, so taking the cache lock is one ordered acquisition.
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
        // **Out of the registration's cache** after the page lock is released (the cache lock ranks
        // above it), by address: a new object for the file may have replaced this one.
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

/// The device block holding file block `file_block` in `runs`; `0` for a hole or past the map.
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

/// **The device block a fill of `blocks` file blocks from `file_block` reads** — the first
/// one's — or `0` when it reads nothing: the first is a hole or past the map, or **every one
/// is a hole or `UNWRITTEN`** ([`RUN_UNWRITTEN`]); one block not promised zero reads the page.
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

/// Zero the resident page holding byte `end` from `end` to its end — bytes past a file's end
/// that a size change must not let resurface. Nothing to do on a boundary or non-resident page.
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

    fn fobj(size: usize) -> KBox<FileObject> {
        FileObject::try_new(size, Producer::Stub { base: 0 }).unwrap()
    }

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

        let frame = match f.reserve(2) {
            Reserve::New(frame) => frame,
            other => panic!("expected New, got {other:?}"),
        };
        assert_eq!(frame_byte(frame, 0), 0, "fresh frame must be zeroed");
        assert_eq!(frame_byte(frame, PAGE_SIZE - 1), 0);
        assert_eq!(f.resident_pages(), 1);
        assert_eq!(f.lookup(2), Some((frame, PageState::Loading)));

        assert_eq!(f.reserve(2), Reserve::Loading(frame));
        assert_eq!(f.resident_pages(), 1);

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

    use crate::object::device_node::{BlockGeometry, DeviceClass, ResourceDescriptor};
    use crate::object::{DeviceNode, IpcChannel, UserspaceServerReg};

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

    fn candidate(reg: &ObjectRef, id: u64, size: usize) -> ObjectRef {
        candidate_in(reg, id, size, 4096)
    }

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

    fn resolve(reg: &ObjectRef, id: u64, size: usize) -> ObjectRef {
        FileObject::cache_in(reg, candidate(reg, id, size)).unwrap()
    }

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

    /// **A reply that changes a cached file's block size is refused**: the kernel's map check
    /// reads each reply's runs against that reply's block size, and the object keeps its own.
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
        let frame = fill(file_of(&a), 0, 0x5A);
        assert_eq!(file_of(&b).lookup(0), Some((frame, PageState::Ready)));
    }

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
        let again = resolve(&reg, 7, PAGE_SIZE);
        assert_eq!(reg_of(&reg).cached_files(), 1);
        drop((first, again));
    }

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
        let mark = file_of(&found).clean_mark().unwrap();
        FileObject::unpin_if_clean(&found, mark);
        drop(found);
        assert_eq!(test_probe::file_object_destroys(), 1);
        assert_eq!(reg_of(&reg).cached_files(), 0);
    }

    #[test]
    fn a_write_back_cleans_only_if_no_writable_mapping_existed_throughout() {
        init_global_heap();
        let reg = registration();
        let a = resolve(&reg, 7, PAGE_SIZE);
        let fo = file_of(&a);

        FileObject::writable_mapped(&a);
        assert_eq!(fo.clean_mark(), None);
        // SAFETY: `a` pins the `FileObject`.
        unsafe { FileObject::writable_unmapped(a.as_ptr()) };
        assert!(fo.is_dirty(), "the sync that began mapped does not clean");

        let mark = fo.clean_mark().unwrap();
        FileObject::writable_mapped(&a);
        // SAFETY: `a` pins the `FileObject`.
        unsafe { FileObject::writable_unmapped(a.as_ptr()) };
        FileObject::unpin_if_clean(&a, mark);
        assert!(fo.is_dirty(), "a mapping during the write-back keeps it dirty");

        let mark = fo.clean_mark().unwrap();
        FileObject::unpin_if_clean(&a, mark);
        assert!(!fo.is_dirty());
        assert_eq!(fo.writable_maps(), 0);
    }

    /// An object with no id is in no cache a sync could search, so a pin on it would be a leak.
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
        // SAFETY: `a` pins the `FileObject`.
        unsafe { FileObject::writable_unmapped(a.as_ptr()) };
        assert_eq!(file_of(&a).writable_maps(), 0);
    }

    /// **A truncate and then a grow read zero over the regrown range.** The test holds the retired
    /// frames to their old bytes: "a mapping's pages stay valid" alone passes for a design that
    /// kept the pages.
    #[test]
    fn a_truncate_then_a_grow_reads_zero_over_the_regrown_range() {
        init_global_heap();
        let f = fobj(3 * PAGE_SIZE);
        let p0 = fill(&f, 0, 0xAB);
        let p1 = fill(&f, 1, 0xAB);
        let p2 = fill(&f, 2, 0xAB);

        f.resize(10, KVec::new()).unwrap();
        assert_eq!(f.lookup(1), None, "a whole page past the end leaves the index");
        assert_eq!(f.lookup(2), None);
        assert_eq!(f.retired_frames(), 2);
        assert_eq!((frame_byte(p1, 0), frame_byte(p2, 0)), (0xAB, 0xAB), "a mapping of one stays valid");
        assert_eq!(frame_byte(p0, 9), 0xAB, "the kept bytes");
        assert_eq!(frame_byte(p0, 10), 0, "the partial tail is zeroed");

        write_frame_byte(p0, 100, 0xCD);
        let phantom = fill(&f, 1, 0xCD);

        f.resize(3 * PAGE_SIZE, KVec::new()).unwrap();
        assert_eq!(f.size(), 3 * PAGE_SIZE);
        assert_eq!(frame_byte(p0, 9), 0xAB);
        assert_eq!(frame_byte(p0, 100), 0, "the partial tail reads zero after the grow");
        assert_eq!(f.lookup(1), None, "the page faulted in past the end is not served");
        assert_eq!(frame_byte(phantom, 0), 0xCD, "its frame is retired, not reused");
        // A page not resident is filled afresh: zeroed, then from the map.
        match f.reserve(1) {
            Reserve::New(fr) => assert_eq!(frame_byte(fr, 0), 0),
            o => panic!("{o:?}"),
        }
    }

    fn po() -> ObjectRef {
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        unsafe {
            ObjectRef::from_raw(
                KBox::into_raw(PendingOperation::try_new().unwrap()).as_ptr() as *mut (),
                KObjectType::PendingOperation,
            )
        }
    }

    fn read_one(f: &FileObject, index: usize, block_size: u32) -> u64 {
        match f.begin_fill(index, PhysAddr(0), block_size, 0, &po(), &mut KVec::new()) {
            ReadStep::Go(block, 1) => block,
            ReadStep::Hole => 0,
            other => panic!("{other:?}"),
        }
    }

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
    /// to them**: one lookup for both would read the flag as a hole and skip the write. A page
    /// holding one block the server did not promise is read whole.
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

    type Irp = (std::vec::Vec<PhysAddr>, u64, u64);

    fn write_runs(f: &FileObject, block_size: u32, max: usize) -> (WriteBack, std::vec::Vec<Irp>) {
        let mut irps = std::vec::Vec::new();
        let w = f.write_runs(block_size, max, |frames, offset, length| {
            irps.push((frames.to_vec(), offset, length));
            0
        });
        (w, irps)
    }

    fn irp(
        frames: &std::collections::BTreeMap<usize, PhysAddr>,
        pages: core::ops::Range<usize>,
        block: u64,
        block_size: u64,
    ) -> Irp {
        let length = (pages.len() * PAGE_SIZE) as u64;
        (pages.map(|i| frames[&i]).collect(), block * block_size, length)
    }

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

    #[test]
    fn a_run_of_pages_in_one_kib_blocks_is_one_irp_while_the_device_is_contiguous() {
        init_global_heap();
        let reg = registration();
        let (f, frames) = mapped_file(&reg, 7, 8, 1024, &[(0, 400, 32)], 0..8);
        assert_eq!(write_runs(file_of(&f), 1024, 64).1, [irp(&frames, 0..8, 400, 1024)]);
        let (g, frames) = mapped_file(&reg, 8, 8, 1024, &[(0, 400, 16), (16, 900, 16)], 0..8);
        assert_eq!(write_runs(file_of(&g), 1024, 64).1, [irp(&frames, 0..4, 400, 1024), irp(&frames, 4..8, 900, 1024)]);
    }

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

    fn miss(f: &FileObject, index: usize) -> (PhysAddr, ObjectRef) {
        match f.reserve_fault(index) {
            Fault::Fill(frame, po) => (frame, po),
            _ => panic!("page {index} was not a miss"),
        }
    }

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

    struct Held(core::cell::RefCell<std::vec::Vec<*mut crate::io::irp::Irp>>);

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

    #[test]
    fn a_fault_fails_at_its_second_failed_fill() {
        use core::cell::RefCell;
        init_global_heap();
        let held = Held(RefCell::new(std::vec::Vec::new()));
        let f = held_disk_file(&held);
        let reads = RefCell::new(std::vec::Vec::new());
        let fo = file_of(&f);

        let (frame, read_0) = miss(fo, 0);
        assert_eq!(fo.start_fill(&f, 0, frame, &read_0, true), FillStart::DeviceIo(8));
        let mut read_5 = None;
        let mut waits = 0;
        let mut wait = |po: &ObjectRef| -> bool {
            waits += 1;
            let ok = complete_held(&held, &reads, 5, po);
            if waits == 1 {
                assert!(!ok);
                fo.finish_io();
                for k in 0..8 {
                    fo.settle(k, &read_0, false);
                }
                let (frame, po) = miss(fo, 5);
                assert_eq!(fo.start_fill(&f, 5, frame, &po, true), FillStart::DeviceIo(8));
                read_5 = Some(po);
            }
            ok
        };
        assert_eq!(FileObject::fault_in_page_with(&f, 5, &mut wait, &mut || panic!()), None);
        assert_eq!(*reads.borrow(), [(0, 8), (5, 8)], "two reads, both failed, and no third");
        fo.finish_io();
        let read_5 = read_5.unwrap();
        for k in 5..13 {
            fo.settle(k, &read_5, false);
        }
        crate::io::block::reclaim_completed();
    }

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
        let Forgotten::Later(second) = fo.forget(&po()) else { panic!() };
        assert_eq!(second.as_ptr(), answer.as_ptr());
        let done = fo.end_io().expect("the last IRP ending hands the answer back");
        assert_eq!(done.as_ptr(), answer.as_ptr());
        assert!(fo.end_io().is_none(), "and only once");
    }

    /// **A second `Forget` through the registration waits on the first one's answer**: the first
    /// must not take the entry out, or a second is answered at once with the IRP still in flight.
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
        let live = reg_of(&reg).cache_objects().unwrap();
        let pinned = live.iter().find(|f| f.as_ptr() == dirty_at).unwrap().clone();
        drop(live);
        FileObject::unpin_if_clean(&pinned, mark);
        drop(pinned);
        assert_eq!(count(), 1, "cleaned, it goes");

        assert_eq!(read_one(file_of(&held), 1, 4096), 101);
        let answer = po();
        assert!(matches!(reg_of(&reg).forget_file(7, &answer), Forgotten::Later(_)));
        assert_eq!(reg_of(&reg).cached_files(), 1, "the entry stays while the read is in flight");
        assert_eq!(count(), 0, "a forgotten file is not counted, though something holds it");
        assert!(file_of(&held).end_io().is_some());
        drop(held);
    }

    /// **A write-back its device refused `PeerClosed` lets the file go**, and no other failure
    /// does: [`FileObject::let_go`] leaves the file alive, so its next fill still asks its device
    /// and fails, rather than reading a hole.
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

    fn fault(f: &FileObject, index: usize) -> (&'static str, Option<PhysAddr>, Option<ObjectRef>) {
        match f.reserve_fault(index) {
            Fault::Hit(fr) => ("hit", Some(fr), None),
            Fault::Fill(fr, po) => ("fill", Some(fr), Some(po)),
            Fault::Wait(po) => ("wait", None, Some(po)),
            Fault::Busy => ("busy", None, None),
            Fault::Oom => ("oom", None, None),
        }
    }

    #[test]
    fn a_second_faulter_waits_on_the_fill_and_either_can_settle_it() {
        init_global_heap();
        let f = fobj(2 * PAGE_SIZE);
        let ("fill", Some(frame), Some(po)) = fault(&f, 1) else { panic!("a miss fills") };
        let ("wait", None, Some(waits_on)) = fault(&f, 1) else { panic!("a second faulter waits") };
        assert_eq!(waits_on.as_ptr(), po.as_ptr(), "on the fill's own PO");
        f.settle(1, &waits_on, true);
        assert_eq!(f.lookup(1), Some((frame, PageState::Ready)));
        f.settle(1, &po, true);
        assert!(matches!(fault(&f, 1), ("hit", Some(fr), None) if fr == frame));
        let _ = f.reserve(0);
        assert!(matches!(fault(&f, 0), ("busy", None, None)));
    }

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
        }
    }

    #[test]
    fn dispatch_destroy_runs_file_object_arm() {
        init_global_heap();
        test_probe::reset();
        let f = fobj(PAGE_SIZE);
        let _ = f.reserve(0);
        let ptr = KBox::into_raw(f).as_ptr() as *mut ();
        // SAFETY: `ptr` carries the single creation reference.
        let r = unsafe { ObjectRef::from_raw(ptr, KObjectType::FileObject) };
        assert_eq!(test_probe::file_object_destroys(), 0);
        drop(r);
        assert_eq!(test_probe::file_object_destroys(), 1);
    }
}
