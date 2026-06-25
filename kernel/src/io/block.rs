//! The block I/O core — building and dispatching a block [`Irp`].
//!
//! A block [`DeviceNode`](crate::object::DeviceNode) carries a [`BlockBackend`]
//! (a submit function + context) installed by the driver that claimed it (the
//! ramdisk here; AHCI in Part 3). [`dispatch_block_irp`] builds an [`Irp`] for a
//! read/write, wraps it in an owning [`IrpBox`], arms its completion DPC, and
//! hands it to the backend. The backend completes the IRP asynchronously; the
//! completion DPC signals the request's `PendingOperation` and reclaims the box.
//!
//! See `docs/architecture/drivers-and-irps.md` and `docs/spec/irp-layout.md`.

use crate::dpc::Dpc;
use crate::io::irp::{IRP_BUF_FRAGS, Irp, IrpBuffer, IrpOp, PhysFrag};
use crate::libkern::io_op::IoOpcode;
use crate::libkern::{KBox, KVec};
use crate::mm::{PAGE_SIZE, PhysAddr};
use crate::object::{DeviceNode, MemoryObject, ObjectRef};
use crate::syscall::error::KError;

/// A block device's I/O entry point, stored on its [`DeviceNode`]. `submit`
/// receives the in-flight `*mut Irp` and the backend's own `ctx`; it programs the
/// device (or, for the ramdisk, performs the transfer and queues the IRP's
/// completion DPC) and returns — it must **not** block.
#[derive(Copy, Clone)]
pub struct BlockBackend {
    /// Submit an IRP to the device.
    pub submit: fn(irp: *mut Irp, ctx: *mut ()),
    /// Device context passed back to `submit` (e.g. the controller / ramdisk).
    pub ctx: *mut (),
}

// SAFETY: a `BlockBackend`'s raw `ctx` points at a device structure that lives
// for the kernel's lifetime; it is only ever dereferenced on the single CPU that
// services the device. These impls let a `DeviceNode` holding one be `Send`/`Sync`
// like the rest of the type-erased object graph.
unsafe impl Send for BlockBackend {}
unsafe impl Sync for BlockBackend {}

/// The kernel-internal owning wrapper around an in-flight [`Irp`]. `#[repr(C)]`
/// with `irp` **first**, so `*mut IrpBox` and `*mut Irp` share an address — the
/// completion DPC recovers the box from the `irp` pointer. The `ObjectRef`s keep
/// the raw pointers inside `irp` (its PO, buffer frames, device) valid for the
/// IRP's lifetime; `frags` backs `irp.buffer`. None of this is the hashed `Irp`
/// layout — a Tier 2 module only ever sees `*mut Irp`.
#[repr(C)]
pub(crate) struct IrpBox {
    irp: Irp,
    /// Backing store for `irp.buffer.frags`.
    frags: KVec<PhysFrag>,
    /// Owning references that pin the objects `irp` points at.
    _po: ObjectRef,
    _buffer: ObjectRef,
    _device: ObjectRef,
}

/// Build the fragment list covering the buffer byte range `[buf_offset,
/// buf_offset + length)` across `frames` (each one page), splitting at page
/// boundaries — the PRDT-style description of exactly this transfer's region.
/// The caller has already validated `buf_offset + length <= frames.len() * PAGE`.
fn build_frags(
    frames: &[PhysAddr],
    buf_offset: u64,
    length: u64,
) -> Result<KVec<PhysFrag>, KError> {
    let mut frags: KVec<PhysFrag> = KVec::new();
    let mut pos = buf_offset;
    let mut remaining = length;
    // Worst case one fragment per page touched, plus a partial head — reserve
    // generously so `try_push` never allocates mid-build.
    let max_frags = (length / PAGE_SIZE as u64 + 2) as usize;
    frags.try_reserve(max_frags).map_err(|_| KError::OutOfMemory)?;
    while remaining > 0 {
        let page = (pos / PAGE_SIZE as u64) as usize;
        let intra = pos % PAGE_SIZE as u64;
        let in_page = (PAGE_SIZE as u64 - intra).min(remaining);
        frags
            .try_push(PhysFrag {
                base: frames[page].as_u64() + intra,
                len: in_page,
            })
            .map_err(|_| KError::OutOfMemory)?;
        pos += in_page;
        remaining -= in_page;
    }
    Ok(frags)
}

/// Build a block IRP for `opcode` on the block `device`, transferring `length`
/// bytes between device offset `dev_offset` and the `buffer` `MemoryObject`'s
/// byte range `[buf_offset, ...)`, completing `po` when done. The three handles'
/// types/rights/bounds are the caller's responsibility (the syscall validates
/// them synchronously; the boot self-test passes valid ones). Returns `Err` only
/// on allocation failure — the caller rolls back the `po` handle.
pub fn dispatch_block_irp(
    device: &ObjectRef,
    buffer: &ObjectRef,
    po: &ObjectRef,
    opcode: IoOpcode,
    dev_offset: u64,
    buf_offset: u64,
    length: u64,
) -> Result<(), KError> {
    // SAFETY: `device` pins a live `DeviceNode` (type checked by the caller).
    let dn: &DeviceNode = unsafe { &*(device.as_ptr() as *const DeviceNode) };
    let backend = dn.block_backend().ok_or(KError::Unsupported)?;

    // SAFETY: `buffer` pins a live `MemoryObject` (type checked by the caller).
    let mo: &MemoryObject = unsafe { &*(buffer.as_ptr() as *const MemoryObject) };
    let frags = build_frags(mo.frames(), buf_offset, length)?;

    let op = match opcode {
        IoOpcode::Read => IrpOp::Read,
        IoOpcode::Write => IrpOp::Write,
    };
    let irp = Irp::new_block(
        op,
        device.as_ptr() as *const (),
        dev_offset,
        length,
        IrpBuffer::NONE, // patched to point at `frags` after placement
        po.as_ptr(),
        0,
    );

    let bx = KBox::try_new(IrpBox {
        irp,
        frags,
        _po: po.clone(),
        _buffer: buffer.clone(),
        _device: device.clone(),
    })
    .map_err(|_| KError::OutOfMemory)?;
    let bx_ptr = KBox::into_raw(bx).as_ptr();

    // Finish wiring now that the box (and its `frags` buffer) have a stable
    // address: point `irp.buffer` at `frags`, and arm the completion DPC at the
    // box. `irp` is the first field, so `bx_ptr` is also the `*mut Irp`.
    // SAFETY: `bx_ptr` is a freshly placed, uniquely-owned `IrpBox`.
    let irp_ptr = unsafe {
        let bx = &mut *bx_ptr;
        bx.irp.buffer = IrpBuffer {
            kind: IRP_BUF_FRAGS,
            count: bx.frags.len() as u32,
            frags: bx.frags.as_ptr() as u64,
        };
        bx.irp.dpc = Dpc::new(irp_complete_dpc, bx_ptr as *mut ());
        &mut bx.irp as *mut Irp
    };

    // Hand the IRP to the device. It completes asynchronously: the backend (or,
    // for real hardware, the completion ISR) queues `irp.dpc`, drained at the
    // interrupt-dispatch tail.
    (backend.submit)(irp_ptr, backend.ctx);
    Ok(())
}

/// The IRP completion DPC: signal the request's `PendingOperation` with the
/// IRP's terminal status (and bytes transferred), then reclaim the `IrpBox`
/// (dropping its owning references). `ctx` is the `*mut IrpBox` (== `*mut Irp`).
fn irp_complete_dpc(ctx: *mut ()) {
    let bx_ptr = ctx as *mut IrpBox;
    // SAFETY: `ctx` is the box pointer armed in `dispatch_block_irp`; the box is
    // live until this handler reclaims it (the IRP completes exactly once).
    let (status, transferred, po) = unsafe {
        let irp = &(*bx_ptr).irp;
        (irp.status, irp.transferred, irp.completion as *mut ())
    };
    // Deliver the outcome through the PO (result payload = bytes transferred).
    crate::sched::complete_pending_op(po, status, transferred);
    // Reclaim the box: drops `_po`/`_buffer`/`_device` and the `frags` backing.
    // SAFETY: the box was `KBox::into_raw`'d in `dispatch_block_irp` and is
    // reclaimed exactly once here.
    drop(unsafe { KBox::from_raw(core::ptr::NonNull::new_unchecked(bx_ptr)) });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frags_cover_aligned_range_one_per_page() {
        let frames = [PhysAddr(0x1000), PhysAddr(0x5000), PhysAddr(0x9000)];
        let frags = build_frags(&frames, 0, 3 * PAGE_SIZE as u64).unwrap();
        assert_eq!(frags.len(), 3);
        assert_eq!(frags[0], PhysFrag { base: 0x1000, len: PAGE_SIZE as u64 });
        assert_eq!(frags[1], PhysFrag { base: 0x5000, len: PAGE_SIZE as u64 });
        assert_eq!(frags[2], PhysFrag { base: 0x9000, len: PAGE_SIZE as u64 });
    }

    #[test]
    fn frags_split_unaligned_head_and_tail() {
        let frames = [PhysAddr(0x1000), PhysAddr(0x2000)];
        // 512 bytes starting 256 into the first page: one fragment.
        let frags = build_frags(&frames, 256, 512).unwrap();
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0], PhysFrag { base: 0x1000 + 256, len: 512 });
        // A range crossing the page boundary: head to end of page 0, then page 1.
        let frags = build_frags(&frames, PAGE_SIZE as u64 - 100, 200).unwrap();
        assert_eq!(frags.len(), 2);
        assert_eq!(frags[0], PhysFrag { base: 0x1000 + PAGE_SIZE as u64 - 100, len: 100 });
        assert_eq!(frags[1], PhysFrag { base: 0x2000, len: 100 });
    }
}
