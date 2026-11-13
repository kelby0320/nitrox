//! Making what was written durable: a file's pages, and a mount's.

use super::*;
use crate::object::userspace_server::QueuedFlush;
use crate::sched::Job;

/// **A mount's flush or held**: an operation's handle, or `WouldBlock` past the process's share.
pub(super) fn mount(ns: &Namespace, path: &[u8], flush: bool, pid: u32) -> SysResult {
    let reg = match ns.resolve(path) {
        Some((ResolvedTarget::UserspaceServer(reg, _), _, _)) => Ok(reg),
        Some(_) => Err(KError::Unsupported),
        None => Err(KError::NotFound),
    };
    let (po, installed, po_h) = operation(pid)?;
    match reg {
        Err(e) => crate::sched::complete_pending_op(&po, e as i32, 0),
        Ok(reg) if flush => admit(reg, QueuedFlush { file: None, po, pid })?,
        Ok(reg) => {
            let held = held(&reg);
            drop(reg);
            match held {
                Ok(n) => crate::sched::complete_pending_op(&po, 0, n),
                Err(e) => crate::sched::complete_pending_op(&po, e as i32, 0),
            }
        }
    }
    installed.commit();
    Ok(po_h.bits() as isize)
}

/// **`sys_io_submit`'s flush of a file**, admitted to its mount's budget. A file with no device
/// behind it has nothing a flush can write: `IoError` at once, as its write-back answers.
pub(super) fn file(file: Ref<FileObject>, pid: u32) -> SysResult {
    let (po, installed, po_h) = operation(pid)?;
    match file.flush_owner() {
        Some(reg) => admit(reg, QueuedFlush { file: Some(file), po, pid })?,
        None => crate::sched::complete_pending_op(&po, KError::IoError as i32, 0),
    }
    installed.commit();
    Ok(po_h.bits() as isize)
}

/// Run `flush` now or queue it; `WouldBlock` past its process's share.
fn admit(reg: Ref<UserspaceServerReg>, flush: QueuedFlush) -> Result<(), KError> {
    let mut job = KBox::try_new(Flush { reg: None, first: None }).map_err(|_| KError::OutOfMemory)?;
    match reg.admit_flush(flush) {
        Ok(None) => Ok(()),
        Ok(Some(flush)) => {
            let pid = flush.pid;
            job.reg = Some(reg.clone());
            job.first = Some(flush);
            if let Err(job) = crate::sched::spawn_job(job) {
                drop(job);
                pass_on(&reg, reg.flush_done(pid));
                return Err(KError::OutOfMemory);
            }
            Ok(())
        }
        Err((e, _)) => Err(e),
    }
}

/// **A place freed with no thread to run its flush**: each flush it passes to is started, or,
/// while no thread can be made, answered `OutOfMemory` and its place passed on in turn.
fn pass_on(reg: &Ref<UserspaceServerReg>, mut next: Option<QueuedFlush>) {
    while let Some(flush) = next.take() {
        let pid = flush.pid;
        let refused = match KBox::try_new(Flush { reg: None, first: None }) {
            Ok(mut job) => {
                job.reg = Some(reg.clone());
                job.first = Some(flush);
                match crate::sched::spawn_job(job) {
                    Ok(()) => return,
                    Err(mut job) => job.first.take(),
                }
            }
            Err(_) => Some(flush),
        };
        let po = refused.map(|f| f.po);
        next = reg.flush_done(pid);
        if let Some(po) = po {
            crate::sched::complete_pending_op(&po, KError::OutOfMemory as i32, 0);
        }
    }
}

fn operation(pid: u32) -> Result<(Ref<PendingOperation>, Installed<'static, 1>, RawHandle), KError> {
    let po = Ref::new(PendingOperation::try_new().map_err(|_| KError::OutOfMemory)?);
    let mut installed = installing::<1>(pid);
    let po_h = installed.install(po.clone().erase(), pending_op_rights()).map_err(refused)?;
    Ok((po, installed, po_h))
}

/// How many of `reg`'s files something holds, after the finished IRPs' references are released.
pub(super) fn held(reg: &Ref<UserspaceServerReg>) -> Result<u64, KError> {
    crate::io::block::reclaim_completed();
    // Taken under the cache's lock, dropped after: each may be the last and `Drop` takes it.
    let files = reg.cache_objects().map_err(|_| KError::OutOfMemory)?;
    Ok(files.len() as u64)
}

/// **A flush on a kernel thread**, which runs the mount's flushes its place passes to after it.
pub(super) struct Flush {
    reg: Option<Ref<UserspaceServerReg>>,
    first: Option<QueuedFlush>,
}

impl Job for Flush {
    fn run(mut job: KBox<Self>) {
        let (Some(reg), mut next) = (job.reg.take(), job.first.take()) else { return };
        drop(job);
        while let Some(flush) = next {
            let (po, status, written, after) = finish(&reg, flush);
            crate::sched::complete_pending_op(&po, status, written);
            next = after;
        }
    }
}

/// **One flush done**, its file let go and its place passed on before its operation is completed,
/// so a held asked then counts none of it: the operation, its answer, and the flush the place went to.
fn finish(reg: &Ref<UserspaceServerReg>, flush: QueuedFlush) -> (Ref<PendingOperation>, i32, u64, Option<QueuedFlush>) {
    let QueuedFlush { file, po, pid } = flush;
    let (status, written) = match file {
        None => flush_mount(reg),
        Some(file) => (write_back(&file).err().map_or(0, |e| e as i32), 0),
    };
    (po, status, written, reg.flush_done(pid))
}

/// Write back every dirty file of `reg`'s mount, held or not: the last failure, and the count.
fn flush_mount(reg: &Ref<UserspaceServerReg>) -> (i32, u64) {
    let Ok(files) = reg.cache_objects() else { return (KError::OutOfMemory as i32, 0) };
    let (mut status, mut written) = (0, 0);
    for f in files.iter().filter(|f| f.is_dirty()) {
        match write_back(f) {
            Ok(()) => written += 1,
            Err(e) => status = e as i32,
        }
    }
    (status, written)
}

/// Write one file's pages back, tell its server, and unpin it if no writable mapping appeared.
fn write_back(file_obj: &Ref<FileObject>) -> Result<(), KError> {
    let mark = file_obj.clean_mark();
    match FileObject::writeback(file_obj) {
        crate::object::file_object::WriteBack::Written => {}
        crate::object::file_object::WriteBack::Failed => return Err(KError::IoError),
        // **Its device has gone**: nothing will take these pages, so the file is let go.
        crate::object::file_object::WriteBack::Gone => {
            let pages = FileObject::let_go(file_obj);
            crate::kprintln!("file: its device has gone; a dirty file and its {pages} page(s) let go, unwritten");
            return Err(KError::PeerClosed);
        }
    }
    // Under Model A the server does not know the data landed: tell it (best-effort, unwaited).
    notify_file_touched(file_obj);
    if let Some(mark) = mark {
        FileObject::unpin_if_clean(file_obj, mark);
    }
    Ok(())
}

/// Send `File::Touch` for a just-flushed Model A file so its server can stamp `mtime`. Stamps
/// on **sync**, not on the write (no per-page dirty bit, `TODO(page-dirty-tracking)`). Silent
/// on failure: the data is durable, and a dropped touch costs only a stale `mtime`.
fn notify_file_touched(file_obj: &Ref<FileObject>) {
    use crate::libkern::KBox;
    use crate::object::FileObject;

    let Some((reg, file_id)) = FileObject::touch_target(file_obj) else {
        return; // not a cached Model A file — nothing to notify
    };
    // Heap-bounced: a 4 KiB `StoredMsg` does not belong on a measured kernel stack.
    // Zeroed in place: the by-value form would put 4 KiB in this frame, on a sync path that is
    // already among the deepest in the kernel.
    let Ok(mut msg) = KBox::<crate::object::ipc_channel::StoredMsg>::try_new_zeroed() else {
        return;
    };
    let Some(body_len) = crate::rsproto::build_touch_request(&mut msg.payload, file_id) else {
        return;
    };
    msg.header.payload_len = body_len as u32;
    msg.header.handle_count = 0;
    crate::sched::us_forward_notify(&reg, &mut msg);
}

#[cfg(test)]
mod tests;
