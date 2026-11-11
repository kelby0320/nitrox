//! A server's reply to a page-cache fill: its bytes copied into the cache frame, the fill settled.

use super::*;

/// Complete a forwarded `File::ReadRange` reply: correlate it to the registration's pending
/// fill, copy the replied bytes (a transferred ≤1-page `MemoryObject`) into the cache page,
/// mark it ready, and complete the fill PO (waking the parked faulter). An error or
/// uncorrelated reply fails the PO / is dropped (transfers released, handles closed).
pub(super) fn complete_read_range_reply(
    reg: &Ref<UserspaceServerReg>,
    bounce: &StoredMsg,
    transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    payload_len: usize,
) -> SysResult {
    use crate::rsproto::{RangeReplyKind, parse_read_range_reply};

    let reply = parse_read_range_reply(&bounce.payload[..payload_len]);
    let pf = match reply {
        Some(r) => crate::sched::us_forward_take_pending_fill(reg, r.request_id),
        None => None,
    };
    let Some(pf) = pf else {
        return Ok(0);
    };
    let reply = reply.expect("pending taken ⇒ reply parsed");

    let status: i32 = match reply.kind {
        RangeReplyKind::Error { kerror } => kerror,
        RangeReplyKind::Malformed => KError::KernelError as i32,
        RangeReplyKind::Success { content_len } => match transfers[0].take() {
            None => KError::InvalidArgument as i32, // success but no bytes
            Some(tr) => match copy_fill(&pf.file_obj, pf.index, &pf.po, &tr.obj, content_len as usize) {
                Ok(()) => {
                    pf.file_obj.settle(pf.index, &pf.po, true);
                    drop(tr.obj);
                    0
                }
                Err(e) => {
                    drop(tr.obj);
                    e as i32
                }
            },
        },
    };

    // Complete the fill PO (one-shot); release the kernel's clones outside `SCHED`.
    crate::sched::complete_pending_op(&pf.po, status, 0);
    drop(pf);
    Ok(0)
}

/// Copy `n` bytes (at most a page) of the transferred fill `MemoryObject` `src` into page `index`
/// of `file`, if `po` is still its fill. `Unsupported` for any other kind of object, or a borrowed one.
pub(super) fn copy_fill(
    file: &FileObject,
    index: usize,
    po: &Ref<PendingOperation>,
    src: &ObjectRef,
    n: usize,
) -> Result<(), KError> {
    let mo = src.downcast_ref::<crate::object::MemoryObject>().ok_or(KError::Unsupported)?;
    if !mo.kernel_reachable() {
        return Err(KError::Unsupported);
    }
    let from = mo.page(0).expect("a memory object has a frame");
    let n = n.min(crate::mm::PAGE_SIZE);
    file.fill_page(index, po, |to| to.copy_from(0, from, 0, n));
    Ok(())
}
