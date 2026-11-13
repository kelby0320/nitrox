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

impl FileObject {
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
            let Some(page) = file.ready_page(index).filter(|_| at < size) else { break };
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
            let pieces = match source_pieces(src, src_at + done, n) {
                Ok(pieces) => pieces,
                Err(e) => break e as i32,
            };
            if let Err(e) = Self::write_page(file, index, in_page, &pieces, fault) {
                break e as i32;
            }
            done += n as u64;
        };
        file.writable_unmapped();
        (status, done)
    }

    /// **Write `pieces` into page `index` from `at`**: a whole page not resident is reserved zeroed
    /// and written before it is ready, with no fill; any other is faulted in first.
    fn write_page(
        file: &Ref<Self>,
        index: usize,
        at: usize,
        pieces: &Pieces<'_>,
        fault: FaultIn<'_>,
    ) -> Result<(), KError> {
        if at == 0 && pieces.len() == PAGE_SIZE && matches!(file.reserve(index), Reserve::New(_)) {
            let written = {
                let g = file.inner.lock();
                let page =
                    g.pages.iter().find(|p| p.index == index && p.state == PageState::Loading && p.fill.is_none());
                page.map(|p| pieces.copy_into(p.view(), 0)).is_some()
            };
            file.mark_ready(index);
            return if written { Ok(()) } else { Err(KError::IoError) };
        }
        fault(file, index).ok_or(KError::IoError)?;
        pieces.copy_into(file.ready_page(index).ok_or(KError::InvalidArgument)?, at);
        Ok(())
    }
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

/// `src`'s pieces of `n` bytes from `at`, `n` at most a page.
fn source_pieces(src: &Source, at: u64, n: usize) -> Result<Pieces<'_>, KError> {
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
            Source::File(from) => from.ready_page(index),
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
