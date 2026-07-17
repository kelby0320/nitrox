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
    /// Drive the in-flight command to completion by **polling** (no interrupts),
    /// then run its completion DPC — for synchronous kernel reads at boot, before
    /// interrupts are enabled (see [`read_blocking`]). A partition delegates to
    /// its disk's poll; the disk driver polls its hardware.
    pub poll: fn(ctx: *mut ()),
    /// Device context passed back to `submit`/`poll` (e.g. the controller).
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

/// Dispatch a block IRP that transfers `length` bytes between device byte offset
/// `dev_offset` and a **single physical `frame`** (a page-cache frame), completing `po`.
/// Unlike [`dispatch_block_irp`] the DMA target is a raw frame, not a `MemoryObject` — the
/// **Model A** page-cache fill (a disk read straight into the cache page, zero-copy). `pin`
/// is an owning reference that keeps the frame's owner (the `FileObject`) alive for the
/// IRP's lifetime — the box holds it in the `_buffer` slot. `length` must be `≤ PAGE_SIZE`.
/// Returns `Err` only on allocation failure (the caller rolls back `po`).
pub fn dispatch_block_irp_into_frame(
    device: &ObjectRef,
    frame: PhysAddr,
    pin: ObjectRef,
    po: &ObjectRef,
    opcode: IoOpcode,
    dev_offset: u64,
    length: u64,
) -> Result<(), KError> {
    // SAFETY: `device` pins a live `DeviceNode` (type checked by the caller).
    let dn: &DeviceNode = unsafe { &*(device.as_ptr() as *const DeviceNode) };
    let backend = dn.block_backend().ok_or(KError::Unsupported)?;

    // One page-sized fragment covering `[0, length)` of the single frame.
    let frags = build_frags(&[frame], 0, length)?;

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
        _buffer: pin, // pins the frame owner (the FileObject), not a MemoryObject
        _device: device.clone(),
    })
    .map_err(|_| KError::OutOfMemory)?;
    let bx_ptr = KBox::into_raw(bx).as_ptr();

    // SAFETY: `bx_ptr` is a freshly placed, uniquely-owned `IrpBox`; wire `irp.buffer` at
    // the now-stable `frags`, arm the completion DPC at the box (irp is the first field).
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

/// Read `count` 512-byte sectors starting at `lba` from the block `device`
/// **synchronously** into `dst`, by polling — for kernel-internal reads at boot,
/// before interrupts are enabled (the GPT driver). Returns `false` on any
/// failure (allocation, bad device, short `dst`, or a device error).
///
/// Submits a normal IRP, then drives it to completion via the device's
/// [`BlockBackend::poll`] (no IRQ needed), and copies the result out.
pub fn read_blocking(device: &ObjectRef, lba: u64, count: u64, dst: &mut [u8]) -> bool {
    // SAFETY: `device` pins a live `DeviceNode`.
    let dn: &DeviceNode = unsafe { &*(device.as_ptr() as *const DeviceNode) };
    let Some(backend) = dn.block_backend() else {
        return false;
    };
    let len = (count * 512) as usize;
    if dst.len() < len {
        return false;
    }
    let Ok(buf) = MemoryObject::try_new(len) else {
        return false;
    };
    // SAFETY: adopt the creation references into owning `ObjectRef`s.
    let buf_ref = unsafe {
        ObjectRef::from_raw(KBox::into_raw(buf).as_ptr() as *mut (), crate::libkern::handle::KObjectType::MemoryObject)
    };
    let Ok(po) = crate::object::PendingOperation::try_new() else {
        return false;
    };
    let po_ref = unsafe {
        ObjectRef::from_raw(KBox::into_raw(po).as_ptr() as *mut (), crate::libkern::handle::KObjectType::PendingOperation)
    };

    if dispatch_block_irp(device, &buf_ref, &po_ref, IoOpcode::Read, lba * 512, 0, len as u64)
        .is_err()
    {
        return false;
    }
    // Poll the in-flight command to completion + run its DPC (no interrupts).
    (backend.poll)(backend.ctx);

    let (status, _result) = crate::sched::pending_op_completion(po_ref.as_ptr());
    if status != 0 {
        return false;
    }

    // Copy the buffer's frames out through the HHDM.
    // SAFETY: `buf_ref` pins the live `MemoryObject`.
    let mo: &MemoryObject = unsafe { &*(buf_ref.as_ptr() as *const MemoryObject) };
    let hhdm = crate::mm::heap::hhdm_offset();
    let mut copied = 0usize;
    for &frame in mo.frames() {
        let n = PAGE_SIZE.min(len - copied);
        let src = (frame.as_u64() + hhdm) as *const u8;
        // SAFETY: `src..src+n` is an owned, HHDM-mapped buffer frame; `dst` fits.
        unsafe { core::ptr::copy_nonoverlapping(src, dst.as_mut_ptr().add(copied), n) };
        copied += n;
        if copied >= len {
            break;
        }
    }
    true
}

// --- Partitions: the second IRP layer -----------------------------------------

/// A block-device partition: a window `[start_lba, start_lba + block_count)` on a
/// parent block device. Leaked to `'static` by its creator (a partition lives for
/// the kernel's lifetime, like a disk).
pub struct Partition {
    /// The parent disk's backend — where rebased IRPs are forwarded.
    disk: BlockBackend,
    start_lba: u64,
    block_count: u64,
    sector_size: u64,
}

// SAFETY: a `Partition` is set up once and accessed only on the CPU servicing the
// device; it holds no interior-mutable cross-context state.
unsafe impl Send for Partition {}
unsafe impl Sync for Partition {}

/// [`BlockBackend::submit`] for a partition: bounds-check the partition-relative
/// request, **rebase** the IRP's offset to disk-absolute, and forward it to the
/// parent disk's backend. This is the two-layer block IRP stack (partition → disk)
/// realised by backend delegation. `ctx` is the `*const Partition`.
fn partition_submit(irp: *mut Irp, ctx: *mut ()) {
    // SAFETY: `ctx` is the live `Partition`; `irp` is the in-flight request.
    let p = unsafe { &*(ctx as *const Partition) };
    let (offset, length) = unsafe { ((*irp).offset, (*irp).length) };
    match partition_rebase(offset, length, p.start_lba, p.block_count, p.sector_size) {
        Some(disk_offset) => {
            // Rebase partition-relative → disk-absolute, then forward down a layer.
            // SAFETY: `irp` is in flight and uniquely owned during submit.
            unsafe {
                (*irp).offset = disk_offset;
                (p.disk.submit)(irp, p.disk.ctx);
            }
        }
        None => {
            // Out of the partition's bounds — complete the IRP with an error.
            // SAFETY: `irp` is in flight; set its status and queue its completion.
            unsafe {
                (*irp).set_completion(KError::InvalidArgument as i32, 0);
                crate::dpc::enqueue(&(*irp).dpc);
            }
        }
    }
}

/// Translate a partition-relative byte `offset` (length `length`) into a
/// disk-absolute offset, or `None` if the request falls outside the partition
/// `[0, block_count * sector_size)`. The core of the two-layer block stack.
fn partition_rebase(
    offset: u64,
    length: u64,
    start_lba: u64,
    block_count: u64,
    sector_size: u64,
) -> Option<u64> {
    let span = block_count.checked_mul(sector_size)?;
    let end = offset.checked_add(length)?;
    if end > span {
        return None;
    }
    offset.checked_add(start_lba.checked_mul(sector_size)?)
}

/// [`BlockBackend::poll`] for a partition: delegate to the parent disk's poll.
fn partition_poll(ctx: *mut ()) {
    // SAFETY: `ctx` is the live `Partition`.
    let p = unsafe { &*(ctx as *const Partition) };
    (p.disk.poll)(p.disk.ctx);
}

/// Build a [`BlockBackend`] for a partition window over `disk`. The returned
/// backend borrows `partition` (which must outlive every IRP submitted to it —
/// it is leaked `'static`).
pub fn partition_backend(partition: &'static Partition) -> BlockBackend {
    BlockBackend {
        submit: partition_submit,
        poll: partition_poll,
        ctx: partition as *const Partition as *mut (),
    }
}

impl Partition {
    /// Create a partition window `[start_lba, start_lba + block_count)` over the
    /// block device `disk` (a `DeviceNode` with a backend). Returns the leaked
    /// `'static` partition + a backend over it, or `None` if `disk` is not a block
    /// device. The caller publishes a block `DeviceNode` backed by the backend.
    pub fn new(
        disk: &ObjectRef,
        start_lba: u64,
        block_count: u64,
        sector_size: u64,
    ) -> Option<&'static Partition> {
        // SAFETY: `disk` pins a live `DeviceNode`.
        let dn: &DeviceNode = unsafe { &*(disk.as_ptr() as *const DeviceNode) };
        let disk_backend = dn.block_backend()?;
        let boxed = KBox::try_new(Partition {
            disk: disk_backend,
            start_lba,
            block_count,
            sector_size,
        })
        .ok()?;
        // Leak: the partition persists for the kernel's lifetime.
        // SAFETY: `into_raw` yields a stable, uniquely-owned pointer.
        Some(unsafe { &*KBox::into_raw(boxed).as_ptr() })
    }

    /// Total sectors in the partition.
    pub fn block_count(&self) -> u64 {
        self.block_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_rebase_maps_relative_to_absolute() {
        // A partition starting at LBA 2048 (1 MiB), 1000 sectors, 512-byte blocks.
        let start = 2048u64;
        let count = 1000u64;
        let ss = 512u64;
        // Partition LBA 0 → disk LBA 2048.
        assert_eq!(partition_rebase(0, 512, start, count, ss), Some(2048 * 512));
        // Partition byte offset 8192 → disk (2048 * 512) + 8192.
        assert_eq!(partition_rebase(8192, 512, start, count, ss), Some(2048 * 512 + 8192));
        // The last sector is in bounds.
        assert_eq!(
            partition_rebase((count - 1) * ss, ss, start, count, ss),
            Some((start + count - 1) * ss)
        );
        // One byte past the end is rejected.
        assert_eq!(partition_rebase((count - 1) * ss, ss + 1, start, count, ss), None);
        assert_eq!(partition_rebase(count * ss, ss, start, count, ss), None);
    }

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
