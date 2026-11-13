//! The [`FileObject`] kernel object — a mapped file's content, paged in on demand.
//!
//!
//! Where a [`MemoryObject`](crate::object::MemoryObject) is RAM committed **eagerly**, a
//! `FileObject` owns a sparse **page cache**: frames allocated and filled when a page is first
//! faulted, freed with the object. `sys_memory_map` builds a lazy
//! [`Backing::File`](crate::mm::vmm::Backing) VMA; the `#PF` handler faults pages
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
//! a mount's flush enumerates the cache and writes it back. Dirty is per object, not per page,
//! until `TODO(page-dirty-tracking)`.
//!
//! [`resize`]: FileObject::resize

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::arch::timer::ArchTimer;
use crate::dpc::Dpc;
use crate::libkern::lockrank::LockRank;
use crate::libkern::{AllocError, KBox, KString, KVec, SpinLock};
use crate::mm::frame::{Frame, FrameView};
use crate::mm::{PAGE_SIZE, PhysAddr};
use crate::object::header::{Header, Ref, Shared, kobject};
use crate::object::{DeviceNode, PendingOperation, StoredMsg, UserspaceServerReg};

/// How a [`FileObject`] **fills** a cache page on a fault.
pub enum Producer {
    /// Test producer: fills page `i` with the byte `base + i`, asynchronously via a DPC.
    Stub { base: u8 },
    /// Model B — a **non-block** filesystem: fill a page by sending a `File::ReadRange` to the
    /// resource server through the [`UserspaceServerReg`] `reg`'s forwarding endpoint. `reg` pins
    /// the registration; `suffix` names the file under the mount.
    ///
    FsServer { reg: Ref<UserspaceServerReg>, suffix: KString },
    /// Model A — a **block** filesystem: the kernel owns the file-data path. The run map (under
    /// the object's lock, since a size change replaces it) maps the file's blocks to device LBAs;
    /// a fault reads the page's block **zero-copy** from `device` into the cache frame via a block
    /// IRP. See `docs/architecture/filesystem-data-path.md`.
    ///
    /// `reg` and `file_id` are the file's identity: **a non-zero `file_id` means the object is in
    /// `reg`'s file cache** (its closing takes it out), and they name the file back to its server for
    /// the one thing it cannot learn otherwise — that it was **written**. An in-place overwrite
    /// never reaches the server, so the kernel sends `File::Touch` by id after a write-back
    /// ([`FileObject::writeback`]).
    FsServerBlocks { device: Ref<DeviceNode>, block_size: u32, reg: Ref<UserspaceServerReg>, file_id: u64 },
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
    /// The reply's device is not the one the cached object for its file reads and writes back.
    DeviceChanged,
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
    frame: Frame,
    state: PageState,
    /// While a fault's fill is in flight, **its `PendingOperation`**, which a second faulter
    /// blocks on; taken out when the page [settles](FileObject::settle). Never the last reference
    /// while here — the filler holds its own — so dropping it under the lock only decrements.
    fill: Option<Ref<PendingOperation>>,
}

impl CachePage {
    /// The page's frame, for as long as the page is borrowed.
    fn view(&self) -> FrameView<'_> {
        self.frame.view()
    }
}

pub(crate) enum Fault {
    Hit(PhysAddr),
    Fill(PhysAddr, Ref<PendingOperation>),
    Wait(Ref<PendingOperation>),
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
    header: Header<Self>,
    magic: u64,
    /// Exact file size in bytes; the mapped range is bounded by it. **Changes in place** for a
    /// cached Model A file ([`resize`](Self::resize)), so it is atomic.
    size: AtomicUsize,
    /// What uses its frames now.
    users: io::Users,
    producer: Producer,
    inner: SpinLock<Inner>,
}

kobject!(FileObject, closing = FileObject::leave_cache);
// SAFETY: a `FileObject` changes once shared only in its atomics and behind its lock.
unsafe impl Shared for FileObject {}

struct Inner {
    /// Cached pages. Sparse and unsorted; lookups are a linear scan. Each entry **owns** its
    /// frame, freed as it drops.
    pages: KVec<CachePage>,
    /// Frames a truncation took out of `pages`; a mapping that faulted one in may still point at
    /// it, so they are freed only with the object.
    retired: KVec<Frame>,
    /// A Model A file's run map; empty for any other producer.
    runs: KVec<BlockRun>,
    /// **The object's own reference to itself, held while it is dirty** — from a writable mapping
    /// until a write-back that finds none left. It keeps a file whose writer exited without
    /// syncing alive in its registration's cache (which indexes objects only weakly) until a sync
    /// or unmount writes it. Taken only for a cached object, the kind a sync can find.
    self_pin: Option<Ref<FileObject>>,
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
    forget_answer: Option<Ref<PendingOperation>>,
}

impl Inner {
    /// Where page `index` is if `po` is still its fill: a truncation can retire a page mid-fill.
    fn filling(&self, index: usize, po: &Ref<PendingOperation>) -> Option<usize> {
        self.pages.iter().position(|p| p.index == index && p.fill.as_ref().is_some_and(|f| f.as_ptr() == po.as_ptr()))
    }
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
    Later(Ref<PendingOperation>),
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

mod fault;
/// Page-cache fill and write-back counters, the measurement behind the read-ahead decision.
/// Always on: two atomic adds per fill are nothing beside the block read they bracket.
pub mod fill_stats;
pub mod io;

impl FileObject {
    /// **Out of the registration's cache** before the box is rebuilt, by address (a new object for
    /// the file may have replaced this one): until then, another CPU may upgrade the entry.
    fn leave_cache(&self, _: crate::object::header::Closing) {
        if let Producer::FsServerBlocks { reg, file_id, .. } = &self.producer
            && *file_id != 0
        {
            reg.cache_forget_object(self);
        }
    }

    /// Sentinel written into [`FileObject::magic`] at construction.
    pub const MAGIC: u64 = 0x46_69_6c_65_4f_62_6a_21; // "FileObj!"

    /// Allocate an empty `FileObject` of `size` bytes whose pages `producer` fills on fault.
    pub fn try_new(size: usize, producer: Producer) -> Result<KBox<Self>, AllocError> {
        Self::try_new_with_runs(size, producer, KVec::new())
    }

    /// As [`try_new`](Self::try_new), for a Model A file whose blocks `runs` maps.
    pub fn try_new_with_runs(size: usize, producer: Producer, runs: KVec<BlockRun>) -> Result<KBox<Self>, AllocError> {
        KBox::try_new(Self {
            header: Header::new(),
            magic: Self::MAGIC,
            size: AtomicUsize::new(size),
            users: io::Users::default(),
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

    /// Whether both are block files over one device object.
    fn same_device(&self, other: &FileObject) -> bool {
        match (&self.producer, &other.producer) {
            (Producer::FsServerBlocks { device: a, .. }, Producer::FsServerBlocks { device: b, .. }) => {
                a.as_ptr() == b.as_ptr()
            }
            _ => false,
        }
    }

    /// The registration whose cache holds this object, and its id there, if it is cached.
    pub fn cache_key(&self) -> Option<(&UserspaceServerReg, u64)> {
        match &self.producer {
            Producer::FsServerBlocks { reg, file_id, .. } if *file_id != 0 => Some((reg, *file_id)),
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
    pub fn writable_mapped(file_obj: &Ref<FileObject>) {
        let fo: &FileObject = file_obj;
        let mut g = fo.inner.lock();
        fo.users.writable_maps.fetch_add(1, Ordering::AcqRel);
        g.map_gen = g.map_gen.wrapping_add(1);
        // No sync writes a forgotten object, so a pin on it is one nothing would ever clean.
        if g.self_pin.is_none() && fo.cache_key().is_some() && !g.dead {
            g.self_pin = Some(file_obj.clone());
        }
    }

    /// A writable mapping of this object has gone. Lock-free, since a VMA drops wherever its
    /// address space is torn down; saturating, so a never-counted VMA takes no one's count.
    pub fn writable_unmapped(&self) {
        let _ = self.users.writable_maps.try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
    }

    /// **Taken before a write-back: whether it can leave the object clean.** `Some` only if no
    /// writable mapping exists now — one that does could write after its page's IRP read the
    /// frame, then go away before the write-back ends, and a check made only at the end would call
    /// that write written. The mark is the mapping generation, which any writable mapping made
    /// during the write-back moves; pass it to [`unpin_if_clean`](Self::unpin_if_clean). A writer
    /// that syncs while still mapped leaves its file dirty, so `libfs` and `nxsh` unmap first.
    pub fn clean_mark(&self) -> Option<u64> {
        let g = self.inner.lock();
        if self.users.writable_maps.load(Ordering::Acquire) == 0 { Some(g.map_gen) } else { None }
    }

    /// After a successful write-back that began at `mark`: **clean**, unless a writable mapping
    /// was made since. Clean drops the self-pin outside the lock, as it may be the last reference.
    pub fn unpin_if_clean(file_obj: &Ref<FileObject>, mark: u64) {
        let pin = {
            let mut g = file_obj.inner.lock();
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
                // Reserved above; a `try_push` that failed would drop a frame still mapped.
                let Ok(()) = g.retired.push_within_capacity(p.frame) else { unreachable!("reserved above") };
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
    /// The candidate drops outside the cache's lock, since its closing takes it. `Err` on
    /// allocation failure, with nothing entered or resized; and for a reply whose block size or
    /// device is not the cached object's, with nothing resized: the reply's runs were checked
    /// against its own block size, and its rights were decided by its own device.
    pub fn cache_in(
        reg: &Ref<UserspaceServerReg>,
        candidate: Ref<FileObject>,
    ) -> Result<Ref<FileObject>, CacheInError> {
        let cand: &FileObject = &candidate;
        match reg.cache_get_or_insert(&candidate).map_err(|_| CacheInError::OutOfMemory)? {
            None => Ok(candidate),
            Some(existing) => {
                let ex: &FileObject = &existing;
                if ex.block_size() != cand.block_size() {
                    drop(candidate);
                    return Err(CacheInError::BlockSizeChanged);
                }
                if !ex.same_device(cand) {
                    drop(candidate);
                    return Err(CacheInError::DeviceChanged);
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
        po: &Ref<PendingOperation>,
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
            if Some(block * bs) != (first * bs).checked_add((n * PAGE_SIZE) as u64) || g.pages.try_reserve(1).is_err() {
                break;
            }
            let Some(f) = Frame::alloc() else { break };
            let _ = frames.try_push(f.phys());
            g.pages
                .try_push(CachePage { index: index + n, frame: f, state: PageState::Loading, fill: Some(po.clone()) })
                .expect("slot reserved above");
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
            (p.state == PageState::Ready && block != 0).then_some((at, p.frame.phys(), block))
        };
        let Some((mut at, frame, first)) = target(&g, start, 0) else {
            return WriteStep::Skip;
        };
        // `try_reserve` above guarantees these pushes do not allocate.
        let _ = frames.try_push(frame);
        while frames.len() < max {
            let k = frames.len();
            match target(&g, start + k, at + 1) {
                Some((next, frame, block)) if Some(block * bs) == (first * bs).checked_add((k * PAGE_SIZE) as u64) => {
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
    pub fn end_io(&self) -> Option<Ref<PendingOperation>> {
        let mut g = self.inner.lock();
        g.io_in_flight = g.io_in_flight.saturating_sub(1);
        if g.dead && g.io_in_flight == 0 { g.forget_answer.take() } else { None }
    }

    fn finish_io(&self) {
        if let Some(answer) = self.end_io() {
            crate::sched::complete_pending_op(&answer, 0, 0);
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
    pub fn forget(&self, answer: &Ref<PendingOperation>) -> Forgotten {
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

    /// Look up page `index`: its frame + [`PageState`] if cached, else `None`.
    pub fn lookup(&self, index: usize) -> Option<(PhysAddr, PageState)> {
        let g = self.inner.lock();
        g.pages.iter().find(|p| p.index == index).map(|p| (p.frame.phys(), p.state))
    }

    /// Reserve page `index` for the fault path: [`Reserve::Ready`] on a hit, [`Reserve::Loading`]
    /// for a fill in flight; on a miss a **zeroed** frame (so a tail page's padding is zero)
    /// inserted `Loading`, as [`Reserve::New`] for the caller to fill and
    /// [`mark_ready`](Self::mark_ready). [`Reserve::Oom`] if no frame or slot is free.
    pub fn reserve(&self, index: usize) -> Reserve {
        let mut g = self.inner.lock();
        if let Some(p) = g.pages.iter().find(|p| p.index == index) {
            return match p.state {
                PageState::Ready => Reserve::Ready(p.frame.phys()),
                PageState::Loading => Reserve::Loading(p.frame.phys()),
            };
        }
        // Reserve the slot before allocating the frame, so a `try_push` cannot fail and leak it.
        if g.pages.try_reserve(1).is_err() {
            return Reserve::Oom;
        }
        // Zeroed: a partial tail page's padding reads as zero.
        let Some(frame) = Frame::alloc_zeroed() else {
            return Reserve::Oom;
        };
        let new = Reserve::New(frame.phys());
        g.pages
            .try_push(CachePage { index, frame, state: PageState::Loading, fill: None })
            .expect("slot reserved above");
        new
    }

    /// The fault path's [`reserve`](Self::reserve): a miss also makes the fill's `PendingOperation`
    /// and records it on the page for a second faulter to wait on. Both allocations are under the
    /// lock, which ranks above the allocators.
    pub(crate) fn reserve_fault(&self, index: usize) -> Fault {
        let mut g = self.inner.lock();
        if let Some(p) = g.pages.iter().find(|p| p.index == index) {
            return match (p.state, &p.fill) {
                (PageState::Ready, _) => Fault::Hit(p.frame.phys()),
                (PageState::Loading, Some(po)) => Fault::Wait(po.clone()),
                (PageState::Loading, None) => Fault::Busy,
            };
        }
        if g.pages.try_reserve(1).is_err() {
            return Fault::Oom;
        }
        let po = match PendingOperation::try_new() {
            Ok(p) => Ref::new(p),
            Err(_) => return Fault::Oom,
        };
        let Some(frame) = Frame::alloc_zeroed() else {
            drop(g); // the PO's only reference drops outside the lock
            drop(po);
            return Fault::Oom;
        };
        let phys = frame.phys();
        g.pages
            .try_push(CachePage { index, frame, state: PageState::Loading, fill: Some(po.clone()) })
            .expect("slot reserved above");
        Fault::Fill(phys, po)
    }

    /// **Settle page `index`'s fill `po`** once it has completed: `Ready` if it succeeded,
    /// otherwise out of the cache with its frame freed. Idempotent: the filler or a second faulter
    /// that woke first may call it. Only the page whose fill *is* `po` is touched: a
    /// [`resize`](Self::resize) can retire a page mid-fill and a later fault reserve the index
    /// afresh, and settling by index alone would call that new page ready while its own fill still
    /// ran. Matched by the PO, not the frame: every caller holds a reference to the PO, so its
    /// address cannot be reused meanwhile, where a freed frame's can.
    pub(crate) fn settle(&self, index: usize, po: &Ref<PendingOperation>, ok: bool) {
        let taken = {
            let mut g = self.inner.lock();
            match g.filling(index, po) {
                None => None,
                Some(i) if ok => {
                    g.pages[i].state = PageState::Ready;
                    g.pages[i].fill.take()
                }
                Some(i) => g.pages.remove(i).fill,
            }
        };
        drop(taken);
    }

    /// Transition page `index` from `Loading` to `Ready`; a no-op if absent, ready, or a fault's
    /// fill's to [`settle`](Self::settle), so no ready page has a fill for a settle to free.
    pub fn mark_ready(&self, index: usize) {
        let mut g = self.inner.lock();
        if let Some(p) = g.pages.iter_mut().find(|p| p.index == index && p.fill.is_none()) {
            p.state = PageState::Ready;
        }
    }

    /// **Write page `index` as its fill `po` read it**, if `po` is still its fill; `false` if not.
    pub(crate) fn fill_page(
        &self,
        index: usize,
        po: &Ref<PendingOperation>,
        write: impl FnOnce(FrameView<'_>),
    ) -> bool {
        let g = self.inner.lock();
        let Some(i) = g.filling(index, po) else { return false };
        write(g.pages[i].view());
        true
    }

    /// Page `index`'s frame while it is [`Ready`](PageState::Ready), for the borrow of `self`.
    fn ready_page(&self, index: usize) -> Option<FrameView<'_>> {
        let g = self.inner.lock();
        let p = g.pages.iter().find(|p| p.index == index && p.state == PageState::Ready)?;
        // SAFETY: a ready page has no fill (`mark_ready`), so no `settle` frees it; a truncation
        // retires it, and only `self`'s drop frees it.
        Some(unsafe { FrameView::new(p.frame.phys()) })
    }

    /// Flush every resident, block-backed page to the device (the **Model A** overwrite
    /// writeback), a run of index-adjacent, device-adjacent pages per **block write IRP**
    /// ([`begin_write`](Self::begin_write)), blocking on each; pages over a hole are skipped.
    /// `file_obj` pins the frames across the IRPs. [`WriteBack::Written`] when every write
    /// succeeded, [`WriteBack::Gone`] when the device refused one `PeerClosed` (it has left, and no
    /// later write-back can do better), else [`WriteBack::Failed`].
    pub fn writeback(file_obj: &Ref<FileObject>) -> WriteBack {
        let fo: &FileObject = file_obj;
        // The producer is immutable; the run map is read under the lock with the pages.
        let (device, block_size) = match &fo.producer {
            Producer::FsServerBlocks { device, block_size, .. } => (device.clone(), *block_size),
            _ => return WriteBack::Failed,
        };
        let _running = io::Running::begin(fo);
        let max = run_pages(crate::io::block::max_frags(&device));
        fo.write_runs(block_size, max, |frames, dev_offset, length| {
            let po = match PendingOperation::try_new() {
                Ok(p) => Ref::new(p),
                Err(_) => return crate::syscall::error::KError::OutOfMemory as i32,
            };
            let started = crate::arch::Timer::read_ns();
            if let Err(e) = crate::io::block::dispatch_block_irp_into_frames(
                &device,
                frames,
                file_obj.clone().erase(),
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
                WriteBack::Written => self.written(start, pages),
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
    pub fn let_go(file_obj: &Ref<FileObject>) -> usize {
        let (pin, pages) = {
            let mut g = file_obj.inner.lock();
            let pages = g.pages.iter().filter(|p| p.state == PageState::Ready).count();
            (g.self_pin.take(), pages)
        };
        drop(pin);
        pages
    }

    /// The whole file in a fresh heap buffer, each page faulted in by `fault` first: a spawn gives
    /// [`fault_in_page`](Self::fault_in_page), which blocks on each fill. `AllocError` on a failure.
    pub fn read_to_kvec(&self, mut fault: impl FnMut(usize) -> Option<PhysAddr>) -> Result<KVec<u8>, AllocError> {
        let mut buf = KVec::try_filled(0, self.size())?;
        for (i, piece) in buf.chunks_mut(PAGE_SIZE).enumerate() {
            fault(i).ok_or(AllocError)?;
            // Not ready once faulted in only if a truncation took it since.
            self.ready_page(i).ok_or(AllocError)?.read(0, piece);
        }
        Ok(buf)
    }

    /// Start an asynchronous fill of `frame` for page `index`, completing `po`, by dispatching on
    /// the [`Producer`]. `file_obj` is this object's reference (the deferred fill clones it).
    fn start_fill(
        &self,
        file_obj: &Ref<FileObject>,
        index: usize,
        frame: PhysAddr,
        po: &Ref<PendingOperation>,
        read_ahead: bool,
    ) -> FillStart {
        match &self.producer {
            Producer::Stub { base } => {
                if stub_start_fill(file_obj, index, po, *base) {
                    FillStart::Io
                } else {
                    FillStart::Failed
                }
            }
            Producer::FsServer { reg, suffix } => {
                if self.fs_server_start_fill(file_obj, index, po, reg, suffix) {
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
        file_obj: &Ref<FileObject>,
        index: usize,
        frame: PhysAddr,
        po: &Ref<PendingOperation>,
        device: &Ref<DeviceNode>,
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
                crate::sched::complete_pending_op(po, 0, 0);
                FillStart::Hole
            }
            ReadStep::NoMemory => FillStart::Failed,
            ReadStep::Go(dev_block, pages) => {
                // `file_obj` pins the FileObject (hence the frames) for the IRP's lifetime.
                match crate::io::block::dispatch_block_irp_into_frames(
                    device,
                    &frames,
                    file_obj.clone().erase(),
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
    /// the bytes into the page, marks it ready, and completes `po`. `false` if the request could
    /// not be built or sent (the caller rolls the page back).
    fn fs_server_start_fill(
        &self,
        file_obj: &Ref<FileObject>,
        index: usize,
        po: &Ref<PendingOperation>,
        reg: &Ref<UserspaceServerReg>,
        suffix: &KString,
    ) -> bool {
        let offset = (index * PAGE_SIZE) as u64;
        let remaining = self.size().saturating_sub(index * PAGE_SIZE);
        let len = remaining.min(PAGE_SIZE) as u32;
        // A heap-bounced message, allocated zeroed **in place**: building it on the stack takes 4
        // KiB of kernel stack.
        let mut msg = match KBox::<StoredMsg>::try_new_zeroed() {
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
            crate::sched::us_forward_originate_fill(reg, &mut msg, po, file_obj, index),
            crate::sched::ForwardOutcome::Pending
        )
    }
}

/// Wait until `po` has completed, and answer its status (`0` = success): a barrier for its I/O,
/// so a wait woken with nothing signalled, by the process's exit or for want of room, waits again.
fn wait_status(po: &Ref<PendingOperation>) -> i32 {
    loop {
        let waited = crate::sched::wait_on_io(&[po.as_object()], u64::MAX, 0);
        if let crate::sched::WaitResult::Signaled(_) = waited {
            return crate::sched::pending_op_completion(po).0;
        }
        park_briefly();
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

/// Wait for a page fill's `po` to complete, as [`wait_status`] does; `true` iff it succeeded.
fn wait_for_fill(po: &Ref<PendingOperation>) -> bool {
    wait_status(po) == 0
}

/// Park for a millisecond — a sleep with a deadline on no object, so the CPU runs something
/// else or idles with interrupts on; a `yield_now` from the fault handler returns at once.
fn park_briefly() {
    let now = crate::arch::Timer::read_ns();
    let _ = crate::sched::wait_on_io(&[], now + 1_000_000, now);
}

/// A self-test fill in flight: the DPC and what it needs, heap-boxed so its `Dpc` has a stable
/// address. The DPC fills the page, marks it `Ready`, completes `po`, and frees the box.
struct StubFillBox {
    dpc: Dpc,
    file_obj: Ref<FileObject>,
    po: Ref<PendingOperation>,
    index: usize,
    fill_byte: u8,
}

/// Start a stub fill (page `index` ← the byte `base + index`) by enqueuing a DPC. `false` on
/// box-allocation failure. The box clones `file_obj` and `po` so they outlive the deferred fill.
fn stub_start_fill(file_obj: &Ref<FileObject>, index: usize, po: &Ref<PendingOperation>, base: u8) -> bool {
    let bx = match KBox::try_new(StubFillBox {
        dpc: Dpc::new(stub_fill_dpc, core::ptr::null_mut()),
        file_obj: file_obj.clone(),
        po: po.clone(),
        index,
        fill_byte: base.wrapping_add(index as u8),
    }) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let bx_ptr = KBox::into_raw(bx).as_ptr();
    // SAFETY: `bx_ptr` is a freshly placed, uniquely-owned box; point its DPC at itself (now that
    // it has a stable address) and queue it. The box lives until that DPC's handler frees it.
    unsafe {
        (*bx_ptr).dpc = Dpc::new(stub_fill_dpc, bx_ptr as *mut ());
        crate::dpc::enqueue_raw(NonNull::new_unchecked(&raw mut (*bx_ptr).dpc));
    }
    true
}

/// DPC: write the stub byte into the page, mark it `Ready`, complete the PO.
fn stub_fill_dpc(ctx: *mut ()) {
    // Dropping `bx` here releases two references in DPC context, which the block path stopped
    // doing (decision log, 2026-08-06): a last-reference drop reaches `SlabCache::free` and can
    // deadlock against an allocator holder on the same CPU. Safe *here* only because
    // `Producer::Stub` is built solely from `#[cfg(test)]` code, so this never runs on a boot.
    let bx_ptr = ctx as *mut StubFillBox;
    // SAFETY: `ctx` is the `StubFillBox` we placed in `stub_start_fill`; reclaim it.
    let bx = unsafe { KBox::from_raw(NonNull::new_unchecked(bx_ptr)) };
    bx.file_obj.fill_page(bx.index, &bx.po, |page| page.fill(0, PAGE_SIZE, bx.fill_byte));
    bx.file_obj.settle(bx.index, &bx.po, true);
    crate::sched::complete_pending_op(&bx.po, 0, 0);
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
        p.view().fill(within, PAGE_SIZE - within, 0);
    }
}

#[cfg(test)]
mod tests;
