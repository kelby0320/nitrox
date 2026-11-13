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
        IoOpcode::Flush => return submit_flush(resource_h, &op, pid),
        IoOpcode::Rescan => return submit_control(resource_h, &op, pid, crate::io::irp::IrpOp::Rescan),
        IoOpcode::Eject => return submit_control(resource_h, &op, pid, crate::io::irp::IrpOp::Eject),
    };
    let device = lookup_as::<DeviceNode>(resource_h, pid, dev_right)?;
    let buffer = lookup_as::<MemoryObject>(op.buffer, pid, buf_right)?;
    let buffer_size = io_buffer_size(&buffer)?;
    let buf_end = op.buf_offset.checked_add(op.length).ok_or(KError::InvalidArgument)?;
    if buf_end > buffer_size {
        return Err(KError::InvalidArgument);
    }

    let po_box = PendingOperation::try_new().map_err(|_| KError::OutOfMemory)?;
    let po_ref = Ref::new(po_box);
    let mut installed = installing::<1>(pid);
    let po_h = installed.install(po_ref.clone().erase(), pending_op_rights()).map_err(refused)?;

    if op.length == 0 {
        crate::sched::complete_pending_op(&po_ref, 0, 0);
        installed.commit();
        return Ok(po_h.bits() as isize);
    }

    // A block device goes through the IRP path; a char device through its backend. On failure
    // the PO handle is closed again.
    let dispatched: Result<(), KError> = match device.class() {
        DeviceClass::Block => {
            crate::io::block::dispatch_block_irp(&device, &buffer, &po_ref, opcode, op.offset, op.buf_offset, op.length)
        }
        DeviceClass::Char if opcode == IoOpcode::Write => {
            match device.char_backend().and_then(|b| Some((b.submit_write?, b.ctx))) {
                Some((write, ctx)) => write(&buffer, &po_ref, op.buf_offset, op.length, ctx),
                None => Err(KError::Unsupported),
            }
        }
        DeviceClass::Char if opcode == IoOpcode::Read => match device.char_backend() {
            Some(backend) => (backend.submit_read)(&buffer, &po_ref, op.buf_offset, op.offset, op.length, backend.ctx),
            None => Err(KError::Unsupported),
        },
        DeviceClass::Char | DeviceClass::Other => Err(KError::Unsupported),
    };
    dispatched?;
    installed.commit();
    Ok(po_h.bits() as isize)
}

/// `sys_io_submit`'s **flush**: make what has been written to the block device `resource_h`
/// durable, completing the returned `PendingOperation` once the device has written its cache.
/// Needs `WRITE` on the device and names no buffer and no range, so each of those fields must
/// be `0`. Not a zero-length transfer, so it never takes that path's pre-signalled no-op.
///
/// **And its rescan and its eject**, `irp_op` saying which, with the same rules (`WRITE`). A
/// partition and every disk but a USB one answer `Unsupported`, through the IRP.
fn submit_control(resource_h: u64, op: &IoOp, pid: u32, irp_op: crate::io::irp::IrpOp) -> SysResult {
    if op.buffer != 0 || op.buf_offset != 0 || op.offset != 0 || op.length != 0 {
        return Err(KError::InvalidArgument);
    }
    let device = lookup_as::<DeviceNode>(resource_h, pid, Rights::WRITE)?;
    if device.class() != DeviceClass::Block {
        return Err(KError::Unsupported);
    }
    let po_box = PendingOperation::try_new().map_err(|_| KError::OutOfMemory)?;
    let po_ref = Ref::new(po_box);
    let mut installed = installing::<1>(pid);
    let po_h = installed.install(po_ref.clone().erase(), pending_op_rights()).map_err(refused)?;
    crate::io::block::dispatch_block_control(&device, &po_ref, irp_op)?;
    installed.commit();
    Ok(po_h.bits() as isize)
}

/// A file's flush is its pages' ([`flush::file`]); anything else's, the device's own cache's.
pub(super) fn submit_flush(resource_h: u64, op: &IoOp, pid: u32) -> SysResult {
    let device = || submit_control(resource_h, op, pid, crate::io::irp::IrpOp::Flush);
    let Ok(found) = global::get().lookup(RawHandle(resource_h), pid, Rights::empty()) else { return device() };
    let Ok(file) = found.object.downcast::<FileObject>() else { return device() };
    if op.buffer != 0 || op.buf_offset != 0 || op.offset != 0 || op.length != 0 {
        return Err(KError::InvalidArgument);
    }
    if !found.rights.contains(Rights::MAP_WRITE) {
        return Err(KError::NoAccess);
    }
    super::flush::file(file, pid)
}

/// An I/O buffer's size; a borrowed one is refused, as the kernel and the device reach its frames.
pub(super) fn io_buffer_size(buf: &MemoryObject) -> Result<u64, KError> {
    if !buf.kernel_reachable() {
        return Err(KError::InvalidArgument);
    }
    Ok(buf.size() as u64)
}
