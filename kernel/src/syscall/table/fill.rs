use super::*;

/// Complete a forwarded `File::ReadRange` reply: correlate it to the registration's pending
/// fill, copy the replied bytes (a transferred ≤1-page `MemoryObject`) into the cache frame,
/// mark the page ready, and complete the fill PO (waking the parked faulter). An error or
/// uncorrelated reply fails the PO / is dropped (transfers released, handles closed).
pub(super) fn complete_read_range_reply(
    reg: &ObjectRef,
    bounce: &StoredMsg,
    transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    payload_len: usize,
) -> SysResult {
    use crate::rsproto::{RangeReplyKind, parse_read_range_reply};

    let reply = parse_read_range_reply(&bounce.payload[..payload_len]);
    let pf = match reply {
        // SAFETY: `reg` pins the registration the send arrived through.
        Some(r) => unsafe { crate::sched::us_forward_take_pending_fill(reg.as_ptr(), r.request_id) },
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
            Some(tr) if tr.obj.object_type() != KObjectType::MemoryObject => {
                drop(tr.obj);
                KError::Unsupported as i32
            }
            Some(tr) => {
                copy_fill_into_frame(&tr.obj, pf.frame, content_len as usize);
                // SAFETY: `pf.file_obj` pins the live `FileObject` (header at 0).
                let fo: &crate::object::FileObject =
                    unsafe { &*(pf.file_obj.as_ptr() as *const crate::object::FileObject) };
                fo.settle(pf.index, &pf.po, true);
                drop(tr.obj);
                0
            }
        },
    };

    // Complete the fill PO (one-shot); release the kernel's clones outside `SCHED`.
    // SAFETY: `pf` owns the fill's `PendingOperation` reference.
    unsafe { crate::sched::complete_pending_op(pf.po.as_ptr(), status, 0) };
    drop(pf);
    Ok(0)
}

/// Copy `n` bytes (clamped to one page) from the first frame of the transferred fill
/// `MemoryObject` `src` into the cache frame `dst`, both via the HHDM; a no-op if `src` has no
/// frames or `n == 0`.
fn copy_fill_into_frame(src: &ObjectRef, dst: crate::mm::PhysAddr, n: usize) {
    use crate::mm::{PAGE_SIZE, heap};
    // SAFETY: `src` pins the live `MemoryObject` (header at offset 0).
    let mo: &crate::object::MemoryObject = unsafe { &*(src.as_ptr() as *const crate::object::MemoryObject) };
    let frames = mo.frames();
    let n = n.min(PAGE_SIZE);
    if frames.is_empty() || n == 0 {
        return;
    }
    let src_va = (frames[0].as_u64() + heap::hhdm_offset()) as *const u8;
    let dst_va = (dst.as_u64() + heap::hhdm_offset()) as *mut u8;
    // SAFETY: both frames are page-aligned, HHDM-mapped, and distinct (one is the
    // server's just-transferred object, the other the cache frame); `n ≤ PAGE`.
    unsafe {
        core::ptr::copy_nonoverlapping(src_va, dst_va, n);
    }
}
