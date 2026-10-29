//! `sys_io_submit`: an I/O request on a device, its buffer a memory object the device reaches.

use super::*;

/// `sys_io_submit(resource, op)` — initiate the [`IoOp`] `*op` against `resource` (a block
/// [`DeviceNode`](crate::object::DeviceNode) or char device) and return a `PendingOperation`
/// handle. Never blocks: the outcome (status + bytes transferred) arrives through the PO.
/// Argument / permission / allocation failures return a negative `KError` synchronously with no
/// PO; device/medium failures arrive through the PO (`docs/spec/io-operation.md`).
pub fn sys_io_submit(resource_h: u64, op_ptr: u64) -> SysResult {
    let pid = crate::sched::current_owner_pid();

    let optr = UserPtr::<IoOp>::new(op_ptr).map_err(from_user_access)?;
    let op = copy_from_user(optr).map_err(from_user_access)?;
    if op.flags != 0 {
        return Err(KError::InvalidArgument);
    }
    let opcode = IoOpcode::from_u32(op.opcode).ok_or(KError::InvalidArgument)?;

    let (dev_right, buf_right) = match opcode {
        IoOpcode::Read => (Rights::READ, Rights::MAP_WRITE),
        IoOpcode::Write => (Rights::WRITE, Rights::MAP_READ),
        IoOpcode::Flush => return submit_control(resource_h, &op, pid, crate::io::irp::IrpOp::Flush),
        IoOpcode::Rescan => return submit_control(resource_h, &op, pid, crate::io::irp::IrpOp::Rescan),
        IoOpcode::Eject => return submit_control(resource_h, &op, pid, crate::io::irp::IrpOp::Eject),
    };
    let dev_ok = lookup_typed(resource_h, pid, dev_right, KObjectType::DeviceNode)?;
    let buf_ok = lookup_typed(op.buffer, pid, buf_right, KObjectType::MemoryObject)?;
    let buffer_size = io_buffer_size(&buf_ok.object)?;
    let buf_end = op.buf_offset.checked_add(op.length).ok_or(KError::InvalidArgument)?;
    if buf_end > buffer_size {
        return Err(KError::InvalidArgument);
    }

    let po_box = PendingOperation::try_new().map_err(|_| KError::OutOfMemory)?;
    // SAFETY: adopt the single creation reference.
    let po_ref =
        unsafe { ObjectRef::from_raw(KBox::into_raw(po_box).as_ptr() as *mut (), KObjectType::PendingOperation) };
    let (tptr, tty) = po_ref.clone().into_raw();
    let po_h = match global::get().allocate(pid, tptr, tty, pending_op_rights()) {
        Ok(h) => h,
        Err(e) => {
            // SAFETY: `allocate` did not adopt this reference; reclaim it. Then
            // `po_ref` drops at return, destroying the PO.
            drop(unsafe { ObjectRef::from_raw(tptr, tty) });
            return Err(map_handle_err(e));
        }
    };

    if op.length == 0 {
        // SAFETY: `po_ref` pins the `PendingOperation`.
        unsafe { crate::sched::complete_pending_op(po_ref.as_ptr(), 0, 0) };
        return Ok(po_h.bits() as isize);
    }

    // A block device goes through the IRP path; a char device through its backend. On failure
    // the PO handle is rolled back.
    // SAFETY: `dev_ok.object` pins a live `DeviceNode` (type-checked above).
    let dn: &DeviceNode = unsafe { &*(dev_ok.object.as_ptr() as *const DeviceNode) };
    let dispatched: Result<(), KError> = match dn.class() {
        DeviceClass::Block => crate::io::block::dispatch_block_irp(
            &dev_ok.object,
            &buf_ok.object,
            &po_ref,
            opcode,
            op.offset,
            op.buf_offset,
            op.length,
        ),
        DeviceClass::Char if opcode == IoOpcode::Write => {
            match dn.char_backend().and_then(|b| Some((b.submit_write?, b.ctx))) {
                Some((write, ctx)) => write(&buf_ok.object, &po_ref, op.buf_offset, op.length, ctx),
                None => Err(KError::Unsupported),
            }
        }
        DeviceClass::Char if opcode == IoOpcode::Read => match dn.char_backend() {
            Some(backend) => {
                (backend.submit_read)(&buf_ok.object, &po_ref, op.buf_offset, op.offset, op.length, backend.ctx)
            }
            None => Err(KError::Unsupported),
        },
        DeviceClass::Char | DeviceClass::Other => Err(KError::Unsupported),
    };
    match dispatched {
        Ok(()) => Ok(po_h.bits() as isize),
        Err(e) => {
            close_and_release(po_h, pid);
            Err(e)
        }
    }
}

/// `sys_io_submit`'s **flush**: make what has been written to the block device `resource_h`
/// durable, completing the returned `PendingOperation` once the device has written its cache.
/// Needs `WRITE` on the device and names no buffer and no range, so each of those fields must
/// be `0`. Not a zero-length transfer, so it never takes that path's pre-signalled no-op.
///
/// **And its rescan and its eject**, `irp_op` saying which, with the same rules (`WRITE`). A
/// partition and every disk but a USB one answer `Unsupported`, through the IRP.
/// An I/O buffer's size; a borrowed one is refused, as the kernel and the device reach its frames.
pub(super) fn io_buffer_size(buf: &ObjectRef) -> Result<u64, KError> {
    assert_eq!(buf.object_type(), KObjectType::MemoryObject);
    // SAFETY: `buf` pins a live `MemoryObject`, its type just checked.
    let mo = unsafe { &*(buf.as_ptr() as *const MemoryObject) };
    if !mo.kernel_reachable() {
        return Err(KError::InvalidArgument);
    }
    Ok(mo.size() as u64)
}

fn submit_control(resource_h: u64, op: &IoOp, pid: u32, irp_op: crate::io::irp::IrpOp) -> SysResult {
    if op.buffer != 0 || op.buf_offset != 0 || op.offset != 0 || op.length != 0 {
        return Err(KError::InvalidArgument);
    }
    let dev_ok = lookup_typed(resource_h, pid, Rights::WRITE, KObjectType::DeviceNode)?;
    // SAFETY: `dev_ok.object` pins a live `DeviceNode` (type-checked above).
    if unsafe { &*(dev_ok.object.as_ptr() as *const DeviceNode) }.class() != DeviceClass::Block {
        return Err(KError::Unsupported);
    }
    let po_box = PendingOperation::try_new().map_err(|_| KError::OutOfMemory)?;
    // SAFETY: adopt the single creation reference.
    let po_ref =
        unsafe { ObjectRef::from_raw(KBox::into_raw(po_box).as_ptr() as *mut (), KObjectType::PendingOperation) };
    let (tptr, tty) = po_ref.clone().into_raw();
    let po_h = match global::get().allocate(pid, tptr, tty, pending_op_rights()) {
        Ok(h) => h,
        Err(e) => {
            // SAFETY: `allocate` did not adopt this reference; reclaim it.
            drop(unsafe { ObjectRef::from_raw(tptr, tty) });
            return Err(map_handle_err(e));
        }
    };
    match crate::io::block::dispatch_block_control(&dev_ok.object, &po_ref, irp_op) {
        Ok(()) => Ok(po_h.bits() as isize),
        Err(e) => {
            close_and_release(po_h, pid);
            Err(e)
        }
    }
}
