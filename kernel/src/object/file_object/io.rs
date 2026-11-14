//! A file's bytes read and written a page at a time through its page cache, for `sys_io_submit`,
//! and what an operation asks of a file: the mount it counts against, and the id that names it.

use super::*;
use crate::object::MemoryObject;
use crate::syscall::error::KError;

/// Where a `Write`'s bytes come from.
pub enum Source {
    /// A memory object's.
    Memory(Ref<MemoryObject>),
    /// Another file's: a copy.
    File(Ref<FileObject>),
}

/// How a page is faulted in: [`FileObject::fault_in_page`], which blocks on its fill.
pub type FaultIn<'a> = &'a mut dyn FnMut(&Ref<FileObject>, usize) -> Option<PhysAddr>;

/// **What uses an object's frames now**: its mappings, and the reads, writes and write-backs running
/// on it, which hold its frames bare. A window's pages go only with none of them.
#[derive(Default)]
pub(super) struct Users {
    /// Writable mappings: raised before the mapping is made, lowered when its VMA drops — atomic,
    /// since a VMA drops wherever its address space does and must take no lock here.
    pub(super) writable_maps: AtomicU32,
    /// Every mapping: raised under the object's lock, lowered after an unmap's shootdown.
    maps: AtomicU32,
    /// Reads, writes and write-backs running: raised under the object's lock.
    ops: AtomicU32,
}

/// **A read, write, write-back or fault running on a file**, counted while it lives: a frame lent
/// to its holder is not let go under it.
pub struct Running<'a>(pub(super) &'a FileObject);

impl<'a> Running<'a> {
    pub(super) fn begin(file: &'a FileObject) -> Self {
        let _g = file.inner.lock();
        file.users.ops.fetch_add(1, Ordering::AcqRel);
        Self(file)
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.users.ops.fetch_sub(1, Ordering::AcqRel);
    }
}

impl FileObject {
    /// **This file in use while the guard lives**, so no frame it lends is let go: a fault holds one
    /// from fetching its page until its PTE is in.
    pub fn running(&self) -> Running<'_> {
        Running::begin(self)
    }

    /// Page `index`'s frame while it is [`Ready`](PageState::Ready), lent to `running`'s holder.
    pub(super) fn ready_page<'a>(&'a self, running: &Running<'_>, index: usize) -> Option<FrameView<'a>> {
        debug_assert!(core::ptr::eq(running.0, self), "a page lent on another file's Running");
        let g = self.inner.lock();
        let p = g.pages.iter().find(|p| p.index == index && p.state == PageState::Ready)?;
        // SAFETY: a ready page has no fill (`mark_ready`), so no `settle` frees it; a truncation
        // retires it; `let_window_go` frees it only with no `Running`, and the caller holds one;
        // only `self`'s drop frees it otherwise.
        Some(unsafe { FrameView::new(p.frame.phys()) })
    }

    /// **A mapping of the object is about to be made**: counted before the address space's lock,
    /// as the writable among them are.
    pub fn mapped(&self) {
        let _g = self.inner.lock();
        self.users.maps.fetch_add(1, Ordering::AcqRel);
    }

    /// A mapping of the object has gone, after its shootdown. Lock-free and saturating, as
    /// [`writable_unmapped`](Self::writable_unmapped) is.
    pub fn unmapped(&self) {
        let _ = self.users.maps.try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
    }

    #[cfg(test)]
    pub(crate) fn writable_maps(&self) -> u32 {
        self.users.writable_maps.load(Ordering::Acquire)
    }

    /// **Pages a write-back put on the device from block `lba` are written**: their blocks' `UNWRITTEN`
    /// mark is cleared, so a page let go fills from them again rather than reading as zero. Only
    /// where the run map still puts them there: a resize during the write may have moved them.
    pub(super) fn written(&self, first: usize, pages: usize, lba: u64) {
        let Some(bs) = self.block_size() else { return };
        let block = |page: usize| (page * PAGE_SIZE) as u64 / bs as u64;
        let mut g = self.inner.lock();
        if let Some(runs) = clear_unwritten(&g.runs, block(first), block(first + pages), lba) {
            g.runs = runs;
        }
    }

    /// **`IO_NOCACHE`: let pages `pages` go**, if nothing uses the frames and the file is clean:
    /// no mapping, no read, write or write-back running, and no write begun since `mark`, taken
    /// before the write-back that cleaned it. How many went; a loading page stays, as does one over
    /// blocks still marked unwritten, which a fill would read as zero though they may hold its bytes.
    pub fn let_window_go(&self, pages: core::ops::Range<usize>, mark: u64) -> usize {
        let mut gone: KVec<Frame> = KVec::new();
        if gone.try_reserve(pages.len()).is_err() {
            return 0;
        }
        {
            let mut g = self.inner.lock();
            let used = self.users.maps.load(Ordering::Acquire) != 0 || self.users.ops.load(Ordering::Acquire) != 0;
            if used || g.map_gen != mark || g.self_pin.is_some() {
                return 0;
            }
            let bs = self.block_size().map(u64::from);
            let refills = |runs: &KVec<BlockRun>, index: usize| {
                bs.is_none_or(|bs| {
                    let first = (index * PAGE_SIZE) as u64 / bs;
                    let blocks = first..first + (PAGE_SIZE as u64 / bs).max(1);
                    !runs.iter().any(|r| {
                        let held = r.file_block..r.file_block + r.length as u64;
                        r.flags & RUN_UNWRITTEN != 0
                            && r.device_lba != 0
                            && held.start < blocks.end
                            && blocks.start < held.end
                    })
                })
            };
            let mut i = 0;
            while i < g.pages.len() {
                let p = &g.pages[i];
                let ready = p.state == PageState::Ready && p.fill.is_none();
                if pages.contains(&p.index) && ready && refills(&g.runs, p.index) {
                    let Ok(()) = gone.push_within_capacity(g.pages.remove(i).frame) else { unreachable!("reserved") };
                } else {
                    i += 1;
                }
            }
        }
        gone.len()
    }

    /// **The mount whose budget a read or write of this file counts in**: its producer's
    /// registration, for a file a server serves; `None` for one no server does.
    pub fn op_owner(&self) -> Option<Ref<UserspaceServerReg>> {
        match &self.producer {
            Producer::FsServerBlocks { reg, .. } | Producer::FsServer { reg, .. } => Some(reg.clone()),
            Producer::Stub { .. } => None,
        }
    }

    /// **The mount whose flush budget this file's flush counts in**: its producer's registration, for
    /// a file with a device behind it, forgotten or not; `None` for one with none.
    pub fn flush_owner(&self) -> Option<Ref<UserspaceServerReg>> {
        match &self.producer {
            Producer::FsServerBlocks { reg, .. } => Some(reg.clone()),
            _ => None,
        }
    }

    /// The `(registration, file id)` naming this file to its filesystem server, for a cached
    /// Model A file; `None` for any other. For the post-writeback `File::Touch`, which must reach
    /// a server *without* a resolve — by id, since a cached object outlives the resolve that named
    /// it. The clone pins the registration across the send.
    pub fn touch_target(file_obj: &Ref<FileObject>) -> Option<(Ref<UserspaceServerReg>, u64)> {
        // A forgotten file's id may already name another file.
        if file_obj.inner.lock().dead {
            return None;
        }
        match &file_obj.producer {
            Producer::FsServerBlocks { reg, file_id, .. } if *file_id != 0 => Some((reg.clone(), *file_id)),
            _ => None,
        }
    }

    /// **Whether a `Write` of `len` bytes from `offset` can land**: `InvalidArgument` past the end,
    /// as growth is a resolve's; `Unsupported` on a file with no blocks, a forgotten one, or over a
    /// hole, where write-back would skip the page and the count would claim bytes no block took.
    pub fn writable_range(&self, offset: u64, len: u64) -> Result<(), KError> {
        let end = offset.checked_add(len).ok_or(KError::InvalidArgument)?;
        if end > self.size() as u64 {
            return Err(KError::InvalidArgument);
        }
        let bs = self.block_size().ok_or(KError::Unsupported)? as u64;
        let g = self.inner.lock();
        if g.dead {
            return Err(KError::Unsupported);
        }
        let page = PAGE_SIZE as u64;
        let mut index = offset / page;
        while index * page < end {
            if device_block_in(&g.runs, index * page / bs) == 0 {
                return Err(KError::Unsupported);
            }
            index += 1;
        }
        Ok(())
    }

    /// **Read `len` bytes from `offset` into `dst` from `dst_at`**, a page at a time: the status and
    /// the bytes done, short with status `0` at the file's end, as a truncate may make it.
    pub fn read_into(
        file: &Ref<FileObject>,
        offset: u64,
        len: u64,
        dst: &MemoryObject,
        dst_at: u64,
        fault: FaultIn<'_>,
    ) -> (i32, u64) {
        let running = Running::begin(file);
        let mut done = 0;
        while done < len {
            let at = offset + done;
            let index = (at / PAGE_SIZE as u64) as usize;
            if at >= file.size() as u64 {
                break;
            }
            if fault(file, index).is_none() {
                return (KError::IoError as i32, done);
            }
            let size = file.size() as u64;
            let Some(page) = file.ready_page(&running, index).filter(|_| at < size) else { break };
            let in_page = (at % PAGE_SIZE as u64) as usize;
            let n = (PAGE_SIZE - in_page).min((len - done).min(size - at) as usize);
            memory_pieces(dst, dst_at + done, n, |frame, frame_at, from, k| {
                frame.copy_from(frame_at, page, in_page + from, k)
            });
            done += n as u64;
        }
        (0, done)
    }

    /// **Write `len` bytes at `offset` from `src` at `src_at`**, a page at a time: the status and the
    /// bytes done, which stay in the cache, dirty, when a page fails. It counts as a writable
    /// mapping while it runs, so a write-back beside it leaves the file dirty rather than cleaning
    /// it under the copy. A whole page skips its fill.
    pub fn write_from(
        file: &Ref<FileObject>,
        offset: u64,
        len: u64,
        src: &Source,
        src_at: u64,
        fault: FaultIn<'_>,
    ) -> (i32, u64) {
        if same_bytes(file, offset, src, src_at, len) {
            return (KError::InvalidArgument as i32, 0);
        }
        let running = Running::begin(file);
        // **A copy's source is held too**, so another op's `IO_NOCACHE` lets none of its frames go
        // while this copies from them.
        let source = match src {
            Source::File(from) => Some(Running::begin(from)),
            Source::Memory(_) => None,
        };
        FileObject::writable_mapped(file);
        let mut done = 0;
        let status = loop {
            if done == len {
                break 0;
            }
            let at = offset + done;
            let index = (at / PAGE_SIZE as u64) as usize;
            let in_page = (at % PAGE_SIZE as u64) as usize;
            let n = (PAGE_SIZE - in_page).min((len - done) as usize);
            // **The source's pages before the destination's**: two copies opposite ways then never
            // each hold a page the other waits on.
            if let Source::File(from) = src
                && let Err(e) = fault_range(from, src_at + done, n, fault)
            {
                break e as i32;
            }
            if let Err(e) = file.writable_range(at, n as u64) {
                break e as i32;
            }
            let pieces = match source_pieces(src, source.as_ref(), src_at + done, n) {
                Ok(pieces) => pieces,
                Err(e) => break e as i32,
            };
            if let Err(e) = Self::write_page(file, &running, index, in_page, &pieces, fault) {
                break e as i32;
            }
            done += n as u64;
        };
        file.writable_unmapped();
        (status, done)
    }

    /// **Write `pieces` into page `index` from `at`**: a whole page not resident is made, written
    /// and ready under one hold, with no fill; any other is faulted in first.
    fn write_page(
        file: &Ref<Self>,
        running: &Running<'_>,
        index: usize,
        at: usize,
        pieces: &Pieces<'_>,
        fault: FaultIn<'_>,
    ) -> Result<(), KError> {
        if at == 0
            && pieces.len() == PAGE_SIZE
            && let Some(written) = file.write_whole(index, pieces)
        {
            return written;
        }
        fault(file, index).ok_or(KError::IoError)?;
        pieces.copy_into(file.ready_page(running, index).ok_or(KError::InvalidArgument)?, at);
        Ok(())
    }

    /// Page `index` written whole if it is not resident, so no fault sees it before it is ready:
    /// `None` if it is; `OutOfMemory` if no frame or slot is free.
    fn write_whole(&self, index: usize, pieces: &Pieces<'_>) -> Option<Result<(), KError>> {
        let mut g = self.inner.lock();
        if g.pages.iter().any(|p| p.index == index) {
            return None;
        }
        let Some(frame) = g.pages.try_reserve(1).ok().and_then(|()| Frame::alloc()) else {
            return Some(Err(KError::OutOfMemory));
        };
        pieces.copy_into(frame.view(), 0);
        let page = CachePage { index, frame, state: PageState::Ready, fill: None };
        let Ok(()) = g.pages.push_within_capacity(page) else { unreachable!("reserved") };
        Some(Ok(()))
    }
}

/// **`runs` with blocks `from..to`, written from device block `lba`, no longer marked unwritten**,
/// a run split where the range starts or ends inside it; a run putting them elsewhere is left
/// marked. `None` if none was marked or the runs cannot grow by the two splits.
pub(super) fn clear_unwritten(runs: &KVec<BlockRun>, from: u64, to: u64, lba: u64) -> Option<KVec<BlockRun>> {
    let same_place = |r: &BlockRun| r.device_lba.wrapping_sub(r.file_block) == lba.wrapping_sub(from);
    let touched = |r: &BlockRun| {
        r.flags & RUN_UNWRITTEN != 0 && r.file_block < to && from < r.file_block + r.length as u64 && same_place(r)
    };
    if !runs.iter().any(touched) {
        return None;
    }
    let mut out = KVec::new();
    out.try_reserve(runs.len() + 2).ok()?;
    for r in runs.iter() {
        if !touched(r) {
            out.push_within_capacity(*r).ok()?;
            continue;
        }
        let end = r.file_block + r.length as u64;
        let (a, b) = (from.max(r.file_block), to.min(end));
        for (start, stop, flags) in [(r.file_block, a, r.flags), (a, b, r.flags & !RUN_UNWRITTEN), (b, end, r.flags)] {
            if start < stop {
                let device_lba = if r.device_lba == 0 { 0 } else { r.device_lba + (start - r.file_block) };
                let piece = BlockRun { file_block: start, device_lba, length: (stop - start) as u32, flags };
                out.push_within_capacity(piece).ok()?;
            }
        }
    }
    Some(out)
}

/// **Whether a copy would read bytes it writes first**: a file onto itself, its ranges overlapping
/// and not the same. A copy goes forward a word at a time, so it would read what it had just
/// written; the same range is a copy of nothing.
pub fn same_bytes(file: &FileObject, offset: u64, src: &Source, src_at: u64, len: u64) -> bool {
    let Source::File(from) = src else { return false };
    let overlap = offset < src_at.saturating_add(len) && src_at < offset.saturating_add(len);
    core::ptr::eq(&**from, file) && overlap && offset != src_at
}

/// Fault in every page of `file` that `n` bytes from `at` touch.
fn fault_range(file: &Ref<FileObject>, at: u64, n: usize, fault: FaultIn<'_>) -> Result<(), KError> {
    let page = PAGE_SIZE as u64;
    let end = at + n as u64;
    if end > file.size() as u64 {
        return Err(KError::InvalidArgument);
    }
    let mut index = at / page;
    while index * page < end {
        fault(file, index as usize).ok_or(KError::IoError)?;
        index += 1;
    }
    Ok(())
}

/// **At most two frames' bytes**, a page's worth or less of a source: each a view, the offset in
/// it, and how many. Gathered before the destination's lock, which no other object's lock nests in.
struct Pieces<'a>([Option<(FrameView<'a>, usize, usize)>; 2]);

impl Pieces<'_> {
    fn len(&self) -> usize {
        self.0.iter().flatten().map(|&(_, _, k)| k).sum()
    }

    fn copy_into(&self, page: FrameView<'_>, at: usize) {
        let mut to = at;
        for &(frame, from, k) in self.0.iter().flatten() {
            page.copy_from(to, frame, from, k);
            to += k;
        }
    }
}

/// `src`'s pieces of `n` bytes from `at`, `n` at most a page; a file's lent on `running`.
fn source_pieces<'a>(src: &'a Source, running: Option<&Running<'_>>, at: u64, n: usize) -> Result<Pieces<'a>, KError> {
    let mut pieces = Pieces([None, None]);
    let mut done = 0;
    let mut slot = 0;
    while done < n {
        let pos = at + done as u64;
        let frame_at = (pos % PAGE_SIZE as u64) as usize;
        let k = (PAGE_SIZE - frame_at).min(n - done);
        let index = (pos / PAGE_SIZE as u64) as usize;
        let frame = match src {
            Source::Memory(mo) => mo.page(index),
            // Faulted in before; gone only if a truncate took it since.
            Source::File(from) => running.and_then(|r| from.ready_page(r, index)),
        };
        pieces.0[slot] = Some((frame.ok_or(KError::InvalidArgument)?, frame_at, k));
        slot += 1;
        done += k;
    }
    Ok(pieces)
}

/// Hand `step` each of `mo`'s frames that `n` bytes from `at` touch: the frame, the offset in it,
/// the offset in the `n` bytes, and how many.
fn memory_pieces(mo: &MemoryObject, at: u64, n: usize, mut step: impl FnMut(FrameView<'_>, usize, usize, usize)) {
    let mut done = 0;
    while done < n {
        let pos = at as usize + done;
        let Some(frame) = mo.page(pos / PAGE_SIZE) else { return };
        let frame_at = pos % PAGE_SIZE;
        let k = (PAGE_SIZE - frame_at).min(n - done);
        step(frame, frame_at, done, k);
        done += k;
    }
}
