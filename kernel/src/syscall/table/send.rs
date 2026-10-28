//! A send's delivery, once its message and the handles it moves are taken in: into the peer's
//! ring, or, on a userspace server's kernel end, as a forwarded reply. The handles close only once
//! the message is committed; a send that fails drops its claims, which give them back.

use super::*;

/// Deliver `bounce` on endpoint `ch` for `pid`, `transfers` moving with it under `claims`.
pub(super) fn deliver(
    ch: &ObjectRef,
    bounce: &StoredMsg,
    transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    claims: &mut [Option<Claim<'_>>],
    send_mode: SendMode,
    deadline: u64,
    pid: u32,
) -> SysResult {
    // A peer that is the kernel's end of a Userspace Server channel means a forwarded-lookup
    // **reply**: complete the waiting lookup inline (nothing receives that ring), whatever
    // `send_mode`. See `docs/spec/rsproto-namespace-ops.md`.
    // SAFETY: `ok.object` is the live `IpcChannel` its lookup type-checked, held across the call.
    if let Some(reg) = unsafe { crate::sched::us_forward_reg_for_send(ch.as_ptr()) } {
        let payload_len = (bounce.header.payload_len as usize).min(IPC_PAYLOAD_SIZE);
        if let Some(file_id) = crate::rsproto::forget_request(&bounce.payload[..payload_len]) {
            // Only `Block`: its result is a PO, and no deadline could apply.
            if send_mode != SendMode::Block || bounce.header.handle_count != 0 {
                return Err(KError::InvalidArgument);
            }
            return answer_forget(&reg, file_id, pid);
        }
        let r = complete_forwarded_reply(&reg, bounce, transfers);
        // A reply is consumed whatever it says.
        commit_all(claims);
        return r;
    }

    match send_mode {
        // `Sent` commits the transfer move; a full ring / dead peer leaves the transfers untaken.
        SendMode::NoBlock => {
            // SAFETY: `ok.object` is the live `IpcChannel` its lookup type-checked, held across the call.
            match unsafe { crate::sched::ipc_send_push(ch.as_ptr(), bounce, transfers) } {
                SendOutcome::Sent { .. } => {
                    commit_all(claims);
                    Ok(0)
                }
                SendOutcome::Full => Err(KError::WouldBlock),
                SendOutcome::PeerClosed => Err(KError::PeerClosed),
                SendOutcome::Cycle => Err(KError::InvalidArgument),
            }
        }
        // Returns a `PendingOperation` that completes on delivery; the message is committed
        // either way, so the transfer move is too. The syscall never parks (async-first).
        SendMode::Block | SendMode::BlockBounded => {
            let deadline_ns = if send_mode == SendMode::BlockBounded { deadline } else { u64::MAX };
            let po = PendingOperation::try_new().map_err(|_| KError::OutOfMemory)?;
            // SAFETY: `into_raw` yields the single creation reference; adopt it.
            let po_ref =
                unsafe { ObjectRef::from_raw(KBox::into_raw(po).as_ptr() as *mut (), KObjectType::PendingOperation) };
            // Its handle before the push: nothing may fail once the message is committed, or a
            // retry would send it twice.
            let po_h = install(pid, po_ref.clone(), pending_op_rights())?;
            // SAFETY: `ch` is the live `IpcChannel` its lookup type-checked, held across the call.
            let refused = match unsafe {
                crate::sched::ipc_send_push_blocking(ch.as_ptr(), bounce, transfers, &po_ref, deadline_ns)
            } {
                BlockSendOutcome::Sent { .. } | BlockSendOutcome::Queued => {
                    commit_all(claims);
                    return Ok(po_h.bits() as isize);
                }
                // Back-pressure / dead peer: nothing committed; the sender keeps its handles.
                BlockSendOutcome::PendingFull => KError::WouldBlock,
                BlockSendOutcome::PeerClosed => KError::PeerClosed,
                BlockSendOutcome::Cycle => KError::InvalidArgument,
            };
            close_and_release(po_h, pid);
            Err(refused)
        }
    }
}

/// **Answer a server's `File::Forget`**: the file `file_id` is about to be freed, and the server
/// frees its blocks only once the kernel says nothing of it is in flight. The file's cached
/// object is forgotten ([`forget_file`](crate::object::UserspaceServerReg::forget_file)): it
/// starts no more device I/O, and no resolve or sync finds it again. The answer is a
/// `PendingOperation` installed in the server's table, complete already if nothing was in
/// flight, else when the last IRP ends; a second `Forget` meanwhile gets the same one. The
/// server `sys_wait`s on it, so a `Forget` must be sent `Block`.
fn answer_forget(reg: &ObjectRef, file_id: u64, pid: u32) -> SysResult {
    use crate::object::{Forgotten, UserspaceServerReg};
    let po = PendingOperation::try_new().map_err(|_| KError::OutOfMemory)?;
    // SAFETY: `into_raw` yields the single creation reference; adopt it.
    let answer = unsafe { ObjectRef::from_raw(KBox::into_raw(po).as_ptr() as *mut (), KObjectType::PendingOperation) };
    // SAFETY: `reg` pins the registration the send arrived through.
    let r: &UserspaceServerReg = unsafe { &*(reg.as_ptr() as *const UserspaceServerReg) };
    let wait_on = match r.forget_file(file_id, &answer) {
        Forgotten::Later(po) => po,
        Forgotten::Now => {
            // SAFETY: `answer` is the `PendingOperation` this created.
            unsafe { crate::sched::complete_pending_op(answer.as_ptr(), 0, 0) };
            answer.clone()
        }
    };
    drop(answer);
    let (op, ot) = wait_on.into_raw();
    match global::get().allocate(pid, op, ot, pending_op_rights()) {
        Ok(h) => Ok(h.bits() as isize),
        Err(e) => {
            // SAFETY: `allocate` did not adopt the reference; reclaim it.
            drop(unsafe { ObjectRef::from_raw(op, ot) });
            Err(map_handle_err(e))
        }
    }
}

/// Complete a forwarded **reply** inline in the server's `sys_channel_send`: a
/// `Namespace::Resolve` or `File::ReadRange` (page-cache fill) reply is routed by its op; one
/// that is neither is consumed (transfers dropped); the caller closes the sender's handles.
/// Runs in syscall context (no `SCHED` held), so the
/// cross-context `allocate` and the drops are outside the lock.
fn complete_forwarded_reply(
    reg: &ObjectRef,
    bounce: &StoredMsg,
    transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
) -> SysResult {
    let payload_len = (bounce.header.payload_len as usize).min(IPC_PAYLOAD_SIZE);
    match crate::rsproto::reply_op(&bounce.payload[..payload_len]) {
        Some(crate::rsproto::RESOLVE_OP) => complete_resolve_reply(reg, bounce, transfers, payload_len),
        Some(crate::rsproto::READ_RANGE_OP) => complete_read_range_reply(reg, bounce, transfers, payload_len),
        _ => Ok(0),
    }
}

#[cfg(test)]
mod tests;
