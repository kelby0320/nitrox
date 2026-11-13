//! A mount's file operations, each on a kernel thread of its own, admitted to the mount's budget.

use super::flush::{flush_mount, write_back};
use super::*;
use crate::object::userspace_server::{FileWork, QueuedOp};
use crate::sched::Job;

/// Run `op` now or queue it; `WouldBlock` past its process's share.
pub(super) fn admit(reg: Ref<UserspaceServerReg>, op: QueuedOp) -> Result<(), KError> {
    let mut job = KBox::try_new(FileOps { reg: None, first: None }).map_err(|_| KError::OutOfMemory)?;
    match reg.admit_op(op) {
        Ok(None) => Ok(()),
        Ok(Some(op)) => {
            let pid = op.pid;
            job.reg = Some(reg.clone());
            job.first = Some(op);
            if let Err(job) = crate::sched::spawn_job(job) {
                drop(job);
                pass_on(&reg, reg.op_done(pid));
                return Err(KError::OutOfMemory);
            }
            Ok(())
        }
        Err((e, _)) => Err(e),
    }
}

/// **A place freed with no thread to run its operation**: each one it passes to is started, or,
/// while no thread can be made, answered `OutOfMemory` and its place passed on in turn.
fn pass_on(reg: &Ref<UserspaceServerReg>, mut next: Option<QueuedOp>) {
    while let Some(op) = next.take() {
        let pid = op.pid;
        let refused = match KBox::try_new(FileOps { reg: None, first: None }) {
            Ok(mut job) => {
                job.reg = Some(reg.clone());
                job.first = Some(op);
                match crate::sched::spawn_job(job) {
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
    let (status, done) = match work {
        FileWork::FlushMount => flush_mount(reg),
        FileWork::Flush(file) => (write_back(&file).err().map_or(0, |e| e as i32), 0),
    };
    (po, status, done, reg.op_done(pid))
}

#[cfg(test)]
pub(super) mod tests;
