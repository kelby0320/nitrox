//! A mount's file operations, each on a kernel thread of its own, admitted to the mount's budget:
//! its flushes, and `sys_io_submit`'s `Read` and `Write` on a file.

use super::flush::{flush_mount, operation, write_back};
use super::*;
use crate::libkern::io_op::{IO_NOCACHE, MAX_USER_COPY_SIZE};
use crate::object::file_object::io::{Source, same_bytes};
use crate::object::userspace_server::{FileWork, QueuedOp, Span};
use crate::sched::Job;

/// **`Read` or `Write` on `file`, held with `rights`**: `MAP_READ` to read it into a memory object,
/// `MAP_WRITE` to write it from one or from another file, which a copy reads with `MAP_READ`.
pub(super) fn submit(file: Ref<FileObject>, rights: Rights, op: &IoOp, opcode: IoOpcode, pid: u32) -> SysResult {
    let read = opcode == IoOpcode::Read;
    let (need, buf_need) =
        if read { (Rights::MAP_READ, Rights::MAP_WRITE) } else { (Rights::MAP_WRITE, Rights::MAP_READ) };
    if !rights.contains(need) {
        return Err(KError::NoAccess);
    }
    if op.flags & !IO_NOCACHE != 0 || op.length > MAX_USER_COPY_SIZE {
        return Err(KError::InvalidArgument);
    }
    let buf_end = op.buf_offset.checked_add(op.length).ok_or(KError::InvalidArgument)?;
    let buffer = global::get().lookup(RawHandle(op.buffer), pid, buf_need).map_err(map_handle_err)?.object;
    let from = match buffer.downcast::<MemoryObject>() {
        Ok(mo) if buf_end <= super::submit::io_buffer_size(&mo)? => Source::Memory(mo),
        Err(other) if !read => match other.downcast::<FileObject>() {
            Ok(f) if buf_end <= f.size() as u64 => Source::File(f),
            _ => return Err(KError::InvalidArgument),
        },
        _ => return Err(KError::InvalidArgument),
    };
    if !read {
        file.writable_range(op.offset, op.length)?;
    }
    if same_bytes(&file, op.offset, &from, op.buf_offset, op.length) {
        return Err(KError::InvalidArgument);
    }
    // TODO(copy-two-budgets): a copy waits on its source's device too, in this mount's budget.
    let reg = file.op_owner().ok_or(KError::Unsupported)?;
    let nothing = op.length == 0 || (read && op.offset >= file.size() as u64);
    let nocache = op.flags & IO_NOCACHE != 0;
    let span = Span { file, offset: op.offset, buf_offset: op.buf_offset, length: op.length, nocache };
    let work = match from {
        Source::Memory(mo) if read => FileWork::Read(span, mo),
        from => FileWork::Write(span, from),
    };
    let (po, installed, po_h) = operation(pid)?;
    if nothing {
        crate::sched::complete_pending_op(&po, 0, 0);
    } else {
        admit(reg, QueuedOp { work, po, pid })?;
    }
    installed.commit();
    Ok(po_h.bits() as isize)
}

/// How a thread is started for a job: `sched::spawn_job`, the job handed back if none could be.
pub(super) type Start<'a> = &'a mut dyn FnMut(KBox<FileOps>) -> Result<(), KBox<FileOps>>;

/// Run `op` now or queue it; `WouldBlock` past its process's share.
pub(super) fn admit(reg: Ref<UserspaceServerReg>, op: QueuedOp) -> Result<(), KError> {
    admit_with(reg, op, &mut crate::sched::spawn_job)
}

pub(super) fn admit_with(reg: Ref<UserspaceServerReg>, op: QueuedOp, start: Start<'_>) -> Result<(), KError> {
    let mut job = KBox::try_new(FileOps { reg: None, first: None }).map_err(|_| KError::OutOfMemory)?;
    match reg.admit_op(op) {
        Ok(None) => Ok(()),
        Ok(Some(op)) => {
            let pid = op.pid;
            job.reg = Some(reg.clone());
            job.first = Some(op);
            if let Err(job) = start(job) {
                drop(job);
                pass_on(&reg, reg.op_done(pid), start);
                return Err(KError::OutOfMemory);
            }
            Ok(())
        }
        Err((e, _)) => Err(e),
    }
}

/// **A place freed with no thread to run its operation**: each one it passes to is started, or,
/// while no thread can be made, answered `OutOfMemory` and its place passed on in turn.
pub(super) fn pass_on(reg: &Ref<UserspaceServerReg>, mut next: Option<QueuedOp>, start: Start<'_>) {
    while let Some(op) = next.take() {
        let pid = op.pid;
        let refused = match KBox::try_new(FileOps { reg: None, first: None }) {
            Ok(mut job) => {
                job.reg = Some(reg.clone());
                job.first = Some(op);
                match start(job) {
                    Ok(()) => return,
                    Err(mut job) => job.first.take(),
                }
            }
            Err(_) => Some(op),
        };
        let po = refused.map(|f| f.po);
        next = reg.op_done(pid);
        if let Some(po) = po {
            crate::sched::complete_pending_op(&po, KError::OutOfMemory as i32, 0);
        }
    }
}

/// **A file operation on a kernel thread**, then each one its place passes to.
pub(super) struct FileOps {
    pub(super) reg: Option<Ref<UserspaceServerReg>>,
    pub(super) first: Option<QueuedOp>,
}

impl Job for FileOps {
    fn run(mut job: KBox<Self>) {
        let (Some(reg), mut next) = (job.reg.take(), job.first.take()) else { return };
        drop(job);
        while let Some(op) = next {
            let (po, status, written, after) = finish(&reg, op);
            crate::sched::complete_pending_op(&po, status, written);
            next = after;
        }
    }
}

/// **One operation done**, its file and place let go before it completes, for a held asked then.
pub(super) fn finish(
    reg: &Ref<UserspaceServerReg>,
    op: QueuedOp,
) -> (Ref<PendingOperation>, i32, u64, Option<QueuedOp>) {
    let QueuedOp { work, po, pid } = op;
    let mut fault = |file: &Ref<FileObject>, index| FileObject::fault_in_page(file, index);
    let (status, done) = match work {
        FileWork::FlushMount => flush_mount(reg),
        FileWork::Flush(file) => (write_back(&file).err().map_or(0, |e| e as i32), 0),
        FileWork::Read(s, into) => {
            let (status, done) = FileObject::read_into(&s.file, s.offset, s.length, &into, s.buf_offset, &mut fault);
            if s.nocache && status == 0 {
                let_clean_go(&s.file, s.offset, done);
            }
            (status, done)
        }
        FileWork::Write(s, from) => {
            let (status, done) = FileObject::write_from(&s.file, s.offset, s.length, &from, s.buf_offset, &mut fault);
            match status {
                0 => (uncache_written(&s, &from, done).err().map_or(0, |e| e as i32), done),
                failed => (failed, done),
            }
        }
    };
    (po, status, done, reg.op_done(pid))
}

/// **`IO_NOCACHE` after a write**: the file written back, its window let go, and a copy's source's
/// window if it is clean. The write-back's failure is the op's, which wrote nothing durable.
fn uncache_written(s: &Span, from: &Source, done: u64) -> Result<(), KError> {
    if !s.nocache {
        return Ok(());
    }
    if let Source::File(source) = from {
        let_clean_go(source, s.buf_offset, done);
    }
    let mark = s.file.clean_mark();
    write_back(&s.file)?;
    if let Some(mark) = mark {
        s.file.let_window_go(window(s.offset, done), mark);
    }
    Ok(())
}

/// **A read's window, or a copy's source's, let go if its file is clean**, and written back by no
/// one: a holder of `MAP_READ` alone may not have a file written. A file whose dirt is not tracked,
/// one its server gave no id, is kept.
fn let_clean_go(file: &Ref<FileObject>, at: u64, done: u64) {
    if file.cache_key().is_none() || file.is_dirty() {
        return;
    }
    if let Some(mark) = file.clean_mark() {
        file.let_window_go(window(at, done), mark);
    }
}

/// The pages `done` bytes from `at` touch.
fn window(at: u64, done: u64) -> core::ops::Range<usize> {
    let page = PAGE_SIZE as u64;
    (at / page) as usize..at.saturating_add(done).div_ceil(page) as usize
}

#[cfg(test)]
pub(super) mod tests;
