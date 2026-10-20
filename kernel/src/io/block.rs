//! The block I/O core — building and dispatching a block [`Irp`].
//!
//! A block [`DeviceNode`](crate::object::DeviceNode) carries a [`BlockBackend`]
//! (a submit function + context) installed by its driver. [`dispatch_block_irp`]
//! builds an [`Irp`] in an owning [`IrpBox`], arms its completion DPC and hands
//! it to the backend; the DPC signals the request's `PendingOperation` and leaves
//! the box for thread context to free (see [`irp_complete_dpc`]).
//!
//! See `docs/architecture/drivers-and-irps.md` and `docs/spec/irp-layout.md`.

use crate::dpc::Dpc;
use crate::io::irp::{IRP_BUF_FRAGS, Irp, IrpBuffer, IrpOp, PhysFrag};
use crate::libkern::io_op::IoOpcode;
use crate::libkern::lockrank::LockRank;
use crate::libkern::{IrqSpinLock, KBox, KVec};
use crate::mm::{PAGE_SIZE, PhysAddr};
use crate::object::{DeviceNode, MemoryObject, ObjectRef};
use crate::syscall::error::KError;

/// A block device's I/O entry point, stored on its [`DeviceNode`]. `submit`
/// takes the in-flight `*mut Irp` and the backend's `ctx`, programs the device
/// (the ramdisk transfers and queues the completion DPC) and returns; it must
/// **not** block.
#[derive(Copy, Clone)]
pub struct BlockBackend {
    /// Submit an IRP to the device.
    pub submit: fn(irp: *mut Irp, ctx: *mut ()),
    /// Drive the in-flight command to completion by **polling**, then run its
    /// completion DPC: for synchronous kernel reads at boot, before interrupts
    /// are enabled (see [`read_blocking`]). A partition delegates to its disk.
    pub poll: fn(ctx: *mut ()),
    /// Device context passed back to `submit`/`poll` (e.g. the controller).
    pub ctx: *mut (),
    /// The most physical fragments this device can describe in **one** command:
    /// a device constraint only the driver knows (AHCI's is its command table's
    /// PRDT capacity; a ramdisk has none). [`dispatch_block_irp`] refuses a
    /// transfer needing more, which keeps a driver from writing past the
    /// structure it programs. Counted in fragments because the hardware limit is
    /// a descriptor count.
    pub max_frags: u32,
}

// SAFETY: a `BlockBackend`'s raw `ctx` points at a device structure that lives
// for the kernel's lifetime. `submit` and `poll` dereference it on whichever CPU
// submits, so each device synchronises its own mutable state: AHCI through its
// port lock and single in-flight slot, the RAM disk through its per-transfer
// lock, a partition by being an immutable window onto its disk's backend.
unsafe impl Send for BlockBackend {}
// SAFETY: as above.
unsafe impl Sync for BlockBackend {}

/// The owning wrapper around an in-flight [`Irp`]. `#[repr(C)]` with `irp`
/// **first**, so `*mut IrpBox` and `*mut Irp` share an address and the
/// completion DPC recovers the box from the `irp` pointer. Its `ObjectRef`s keep
/// the raw pointers inside `irp` valid; `frags` backs `irp.buffer`. None of this is the
/// hashed `Irp` layout: a Tier 2 module only ever sees `*mut Irp`.
#[repr(C)]
pub(crate) struct IrpBox {
    irp: Irp,
    /// Backing store for `irp.buffer.frags`.
    frags: KVec<PhysFrag>,
    /// Owning references that pin the objects `irp` points at.
    _po: ObjectRef,
    _buffer: ObjectRef,
    _device: ObjectRef,
    /// Intrusive link for [`RECLAIM`], used between completion and reclamation.
    /// It makes the hand-off in [`irp_complete_dpc`] allocation-free and
    /// unbounded: a fixed parking array would have to drop in place on overflow.
    reclaim_next: *mut IrpBox,
}

/// Build the PRDT-style fragment list covering `[buf_offset, buf_offset +
/// length)` across `frames` (a page each), splitting at page boundaries. The
/// caller has validated the range against `frames.len() * PAGE`.
fn build_frags(frames: &[PhysAddr], buf_offset: u64, length: u64) -> Result<KVec<PhysFrag>, KError> {
    let mut frags: KVec<PhysFrag> = KVec::new();
    let mut pos = buf_offset;
    let mut remaining = length;
    // Worst case one fragment per page plus a partial head; reserved up front so
    // `try_push` never allocates.
    let max_frags = (length / PAGE_SIZE as u64 + 2) as usize;
    frags.try_reserve(max_frags).map_err(|_| KError::OutOfMemory)?;
    while remaining > 0 {
        let page = (pos / PAGE_SIZE as u64) as usize;
        let intra = pos % PAGE_SIZE as u64;
        let in_page = (PAGE_SIZE as u64 - intra).min(remaining);
        frags
            .try_push(PhysFrag { base: frames[page].as_u64() + intra, len: in_page })
            .map_err(|_| KError::OutOfMemory)?;
        pos += in_page;
        remaining -= in_page;
    }
    Ok(frags)
}

/// Build a block IRP for `opcode` on `device`, moving `length` bytes between
/// device offset `dev_offset` and the `buffer` `MemoryObject`'s range
/// `[buf_offset, ...)`, completing `po`. Handle types, rights and bounds are the
/// caller's responsibility.
///
/// Returns `Err` **before the operation starts**, so the caller rolls back `po`:
/// `OutOfMemory`, `Unsupported` without a block backend, `InvalidArgument` if
/// the transfer needs more fragments than the device can describe in one command.
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
    // **The device's own limit, checked against the exact fragment count.**
    // Refused rather than truncated or split: `sys_io_submit` bounds the range
    // only against the buffer's size, so a caller holding a block device handle
    // could otherwise make the driver write PRDT entries past its table and the
    // controller DMA past it.
    //
    // Splitting across several commands needs one `PendingOperation` to outlive
    // several IRPs. `TODO(block-transfer-split)`.
    if frags.len() > backend.max_frags as usize {
        return Err(KError::InvalidArgument);
    }

    let op = match opcode {
        IoOpcode::Read => IrpOp::Read,
        IoOpcode::Write => IrpOp::Write,
        // Not transfers: `dispatch_block_control`.
        IoOpcode::Flush | IoOpcode::Rescan | IoOpcode::Eject => return Err(KError::InvalidArgument),
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
        reclaim_next: core::ptr::null_mut(),
    })
    .map_err(|_| KError::OutOfMemory)?;
    let bx_ptr = KBox::into_raw(bx).as_ptr();

    // Finish wiring now that the box has a stable address: point `irp.buffer` at
    // `frags` and arm the completion DPC at the box (`irp` is first).
    // SAFETY: `bx_ptr` is a freshly placed, uniquely-owned `IrpBox`.
    let irp_ptr = unsafe {
        let bx = &mut *bx_ptr;
        bx.irp.buffer =
            IrpBuffer { kind: IRP_BUF_FRAGS, count: bx.frags.len() as u32, frags: bx.frags.as_ptr() as u64 };
        bx.irp.dpc = Dpc::new(irp_complete_dpc, bx_ptr as *mut ());
        &mut bx.irp as *mut Irp
    };

    // Completes asynchronously: the backend (or completion ISR) queues `irp.dpc`.
    (backend.submit)(irp_ptr, backend.ctx);
    Ok(())
}

/// **The most fragments `device` describes in one command**
/// ([`max_frags`](BlockBackend::max_frags)), or `None` without a block backend;
/// it sizes the page cache's runs ([`dispatch_block_irp_into_frames`]).
pub fn max_frags(device: &ObjectRef) -> Option<u32> {
    // SAFETY: `device` pins a live `DeviceNode` (type checked by the caller).
    let dn: &DeviceNode = unsafe { &*(device.as_ptr() as *const DeviceNode) };
    dn.block_backend().map(|b| b.max_frags)
}

/// Dispatch a block IRP moving `length` bytes between device offset `dev_offset`
/// and **physical `frames`** (a page each, in order), completing `po`. The DMA
/// target is raw page-cache frames, not a `MemoryObject`: zero-copy fills and
/// write-backs, a run of a file's pages in one command. `pin` keeps the frames'
/// owner (the `FileObject`) alive, held in the box's `_buffer`. `length` must be
/// `≤ frames.len() * PAGE_SIZE`. Errors as [`dispatch_block_irp`]; a caller sizes
/// its runs by [`max_frags`].
pub fn dispatch_block_irp_into_frames(
    device: &ObjectRef,
    frames: &[PhysAddr],
    pin: ObjectRef,
    po: &ObjectRef,
    opcode: IoOpcode,
    dev_offset: u64,
    length: u64,
) -> Result<(), KError> {
    // SAFETY: `device` pins a live `DeviceNode` (type checked by the caller).
    let dn: &DeviceNode = unsafe { &*(device.as_ptr() as *const DeviceNode) };
    let backend = dn.block_backend().ok_or(KError::Unsupported)?;
    if length > (frames.len() * PAGE_SIZE) as u64 {
        return Err(KError::InvalidArgument);
    }

    // A page-sized fragment per frame, refused past the device's limit as above.
    let frags = build_frags(frames, 0, length)?;
    if frags.len() > backend.max_frags as usize {
        return Err(KError::InvalidArgument);
    }

    let op = match opcode {
        IoOpcode::Read => IrpOp::Read,
        IoOpcode::Write => IrpOp::Write,
        IoOpcode::Flush | IoOpcode::Rescan | IoOpcode::Eject => return Err(KError::InvalidArgument), // not transfers
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
        reclaim_next: core::ptr::null_mut(),
    })
    .map_err(|_| KError::OutOfMemory)?;
    let bx_ptr = KBox::into_raw(bx).as_ptr();

    // SAFETY: `bx_ptr` is a freshly placed, uniquely-owned `IrpBox`; wire `irp.buffer` at
    // the now-stable `frags`, arm the completion DPC at the box (irp is the first field).
    let irp_ptr = unsafe {
        let bx = &mut *bx_ptr;
        bx.irp.buffer =
            IrpBuffer { kind: IRP_BUF_FRAGS, count: bx.frags.len() as u32, frags: bx.frags.as_ptr() as u64 };
        bx.irp.dpc = Dpc::new(irp_complete_dpc, bx_ptr as *mut ());
        &mut bx.irp as *mut Irp
    };
    (backend.submit)(irp_ptr, backend.ctx);
    Ok(())
}

/// Dispatch a **flush** of the block `device`: an IRP with no buffer and no range
/// that completes `po` once the device has written its volatile cache to its
/// medium (a partition passes it to its disk, a RAM disk completes it at once).
/// `Err` before anything starts: `Unsupported` without a block backend,
/// `OutOfMemory` if the IRP cannot be allocated.
pub fn dispatch_block_flush(device: &ObjectRef, po: &ObjectRef) -> Result<(), KError> {
    dispatch_block_control(device, po, IrpOp::Flush)
}

/// Dispatch an IRP that moves no data — a [`Flush`](IrpOp::Flush), a [`Rescan`](IrpOp::Rescan) of
/// a disk's table or an [`Eject`](IrpOp::Eject) of its medium — completing `po`. Errors as
/// [`dispatch_block_flush`]'s.
pub fn dispatch_block_control(device: &ObjectRef, po: &ObjectRef, op: IrpOp) -> Result<(), KError> {
    // SAFETY: `device` pins a live `DeviceNode` (type checked by the caller).
    let dn: &DeviceNode = unsafe { &*(device.as_ptr() as *const DeviceNode) };
    let backend = dn.block_backend().ok_or(KError::Unsupported)?;
    let irp = Irp::new_block(op, device.as_ptr() as *const (), 0, 0, IrpBuffer::NONE, po.as_ptr(), 0);
    let bx = KBox::try_new(IrpBox {
        irp,
        frags: KVec::new(),
        _po: po.clone(),
        _buffer: device.clone(), // no buffer to pin; the slot holds the device again
        _device: device.clone(),
        reclaim_next: core::ptr::null_mut(),
    })
    .map_err(|_| KError::OutOfMemory)?;
    let bx_ptr = KBox::into_raw(bx).as_ptr();
    // SAFETY: `bx_ptr` is a freshly placed, uniquely-owned `IrpBox`; arm the completion DPC at
    // the box (`irp` is the first field). `irp.buffer` stays `NONE`: nothing to transfer.
    let irp_ptr = unsafe {
        let bx = &mut *bx_ptr;
        bx.irp.dpc = Dpc::new(irp_complete_dpc, bx_ptr as *mut ());
        &mut bx.irp as *mut Irp
    };
    (backend.submit)(irp_ptr, backend.ctx);
    Ok(())
}

/// Boxes whose IRP has completed, awaiting reclamation in **thread** context. An
/// `IrqSpinLock` because [`irp_complete_dpc`] pushes from the interrupt-dispatch
/// tail; masking interrupts is allocation-free.
static RECLAIM: IrqSpinLock<ReclaimHead> = IrqSpinLock::new(LockRank::Leaf, ReclaimHead(core::ptr::null_mut()));

/// Head of the intrusive reclaim list.
struct ReclaimHead(*mut IrpBox);

// SAFETY: the pointer is an `IrpBox` this module allocated and owns exclusively from the
// moment its IRP completes; it is reachable only under `RECLAIM`, and the thread-context
// drain takes the whole list before touching any of it. Same reasoning as `BlockBackend`
// above — a raw pointer into memory this module owns for a well-defined interval.
unsafe impl Send for ReclaimHead {}

/// Drop every completed IRP box. **Thread context only**: the frees
/// [`irp_complete_dpc`] must not do happen here, called from
/// `sched::reap_pending`. Each parked box pins its `_buffer`, so until a drain a
/// machine holds every completed transfer's buffer object; this is bounded by
/// completions since the last `reap_pending`, which `idle_body` keeps small.
pub fn reclaim_completed() {
    // Take the whole list in one swap and drop outside the lock: a drop reaches
    // the allocator, which an IRQ-masking lock must not be held across.
    let mut head = {
        let mut g = RECLAIM.lock();
        core::mem::replace(&mut g.0, core::ptr::null_mut())
    };
    while !head.is_null() {
        // SAFETY: `head` came off the reclaim list, where `irp_complete_dpc` put a box it
        // had finished with and nothing else references. `reclaim_next` was written before
        // the push.
        let next = unsafe { (*head).reclaim_next };
        // SAFETY: the box was `KBox::into_raw`'d in `dispatch_block_irp` and is reclaimed
        // exactly once — the completion path pushes it here exactly once.
        drop(unsafe { KBox::from_raw(core::ptr::NonNull::new_unchecked(head)) });
        head = next;
    }
}

/// [`irp_complete_dpc`], for a host test in another module to complete an IRP its
/// recording device took.
///
/// # Safety
///
/// `irp` is the in-flight IRP of a box `dispatch_block_irp` armed, completed once.
#[cfg(test)]
pub(crate) unsafe fn complete_for_test(irp: *mut Irp) {
    irp_complete_dpc(irp as *mut ());
}

/// The IRP completion DPC: signal the request's `PendingOperation` with the
/// terminal status and bytes transferred, then **hand the `IrpBox` to
/// [`reclaim_completed`]**; it must not drop it. `ctx` is the `*mut IrpBox`
/// (== `*mut Irp`).
fn irp_complete_dpc(ctx: *mut ()) {
    let bx_ptr = ctx as *mut IrpBox;
    // SAFETY: `ctx` is the box pointer armed in `dispatch_block_irp`; the box is
    // live until this handler reclaims it (the IRP completes exactly once).
    let (status, transferred, po) = unsafe {
        let irp = &(*bx_ptr).irp;
        (irp.status, irp.transferred, irp.completion as *mut ())
    };
    // SAFETY: the box's `_po` pins the `PendingOperation` until thread context reclaims the box.
    unsafe { crate::sched::complete_pending_op(po, status, transferred) };
    // **Hand the box to thread context; do not drop it here.** Dropping releases
    // `_po`/`_buffer`/`_device` and `frags`, each of which reaches
    // `SlabCache::free`, whose lock is a plain `SpinLock` (preemption off,
    // interrupts on). This DPC runs on top of whatever the CPU was doing, so if it
    // interrupted a thread already holding that lock the free would spin forever
    // on a lock the frame beneath it can never release. `kernel/CLAUDE.md`:
    // "freeing counts as allocating".
    // SAFETY: the box is live and owned by this completion; nothing else references it
    // once the IRP has completed, so writing its link and publishing it is sound.
    unsafe {
        let mut g = RECLAIM.lock();
        (*bx_ptr).reclaim_next = g.0;
        g.0 = bx_ptr;
    }
}

/// Read `count` 512-byte sectors from `lba` of the block `device`
/// **synchronously** into `dst`, by polling, for kernel-internal reads at boot
/// before interrupts are enabled (the GPT driver). Returns `false` on any
/// failure (allocation, bad device, short `dst`, device error).
pub fn read_blocking(device: &ObjectRef, lba: u64, count: u64, dst: &mut [u8]) -> bool {
    // SAFETY: `device` pins a live `DeviceNode`.
    let dn: &DeviceNode = unsafe { &*(device.as_ptr() as *const DeviceNode) };
    let Some(backend) = dn.block_backend() else {
        return false;
    };
    // **Checked**: a table's header names where its entries are, and is the disk's bytes, not ours.
    let (Some(offset), Some(len)) = (lba.checked_mul(512), count.checked_mul(512)) else {
        return false;
    };
    let len = len as usize;
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
    // SAFETY: `into_raw` yields the box's single creation reference, which this adopts.
    let po_ref = unsafe {
        ObjectRef::from_raw(
            KBox::into_raw(po).as_ptr() as *mut (),
            crate::libkern::handle::KObjectType::PendingOperation,
        )
    };

    if dispatch_block_irp(device, &buf_ref, &po_ref, IoOpcode::Read, offset, 0, len as u64).is_err() {
        return false;
    }
    (backend.poll)(backend.ctx);

    // SAFETY: `po_ref` is a live `PendingOperation` reference held across the call.
    let (status, _result) = unsafe { crate::sched::pending_op_completion(po_ref.as_ptr()) };
    if status != 0 {
        return false;
    }

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

/// A block-device partition: a window `[start_lba, start_lba + block_count)` on a
/// parent block device, leaked to `'static` for the kernel's lifetime (a USB
/// disk's departs and keeps its windows, `TODO(usb-departed-records)`).
pub struct Partition {
    /// The parent disk's backend — where rebased IRPs are forwarded.
    disk: BlockBackend,
    start_lba: u64,
    block_count: u64,
    sector_size: u64,
    /// **Retired** by a rescan of its disk: its record departed and a new table
    /// replaced it, so every IRP is refused `PeerClosed`; a window still
    /// forwarding would reach whatever the new table put there.
    retired: core::sync::atomic::AtomicBool,
    /// **Submits between the retired check and the forward**: a rescan waits for
    /// this to reach zero ([`window_busy`]) after retiring the window, so no IRP
    /// that passed the check reaches the disk after the new table is published.
    entering: core::sync::atomic::AtomicU32,
}

// SAFETY: a `Partition` is immutable after setup but for `retired` and `entering`, which
// are atomic, and its `disk` backend is `Send + Sync` (its device synchronises itself), so
// it may be used from whichever CPU submits.
unsafe impl Send for Partition {}
// SAFETY: as above.
unsafe impl Sync for Partition {}

/// [`BlockBackend::submit`] for a partition: bounds-check, **rebase** the offset
/// to disk-absolute and forward to the parent disk's backend. `ctx` is the
/// `*const Partition`.
fn partition_submit(irp: *mut Irp, ctx: *mut ()) {
    use core::sync::atomic::Ordering::SeqCst;
    // SAFETY: `ctx` is the live `Partition`; `irp` is the in-flight request.
    let p = unsafe { &*(ctx as *const Partition) };
    // **Counted in before the retired check, out after the forward**, each `SeqCst` as
    // `retire_window`'s store and `window_busy`'s load are: either this sees the window retired,
    // or the rescan retiring it sees this submit and waits for it to reach the disk first.
    p.entering.fetch_add(1, SeqCst);
    forward(p, irp);
    p.entering.fetch_sub(1, SeqCst);
}

/// [`partition_submit`]'s body, between its count in and out: refuse, or rebase and forward.
fn forward(p: &Partition, irp: *mut Irp) {
    // SAFETY: `irp` is in flight and uniquely owned during submit.
    if let Some(e) = window_refusal(p, unsafe { (*irp).op }) {
        // SAFETY: `irp` is in flight; set its status and queue its completion.
        unsafe {
            (*irp).set_completion(e as i32, 0);
            crate::dpc::enqueue(&(*irp).dpc);
        }
        return;
    }
    // **A flush names no range**: it is the disk's cache, not the partition's
    // blocks, so it goes down unchanged.
    // SAFETY: `irp` is in flight and uniquely owned during submit.
    if unsafe { (*irp).op } == IrpOp::Flush as u32 {
        (p.disk.submit)(irp, p.disk.ctx);
        return;
    }
    // SAFETY: as above, `irp` is in flight and uniquely owned during submit.
    let (offset, length) = unsafe { ((*irp).offset, (*irp).length) };
    match partition_rebase(offset, length, p.start_lba, p.block_count, p.sector_size) {
        Some(disk_offset) => {
            // SAFETY: `irp` is in flight and uniquely owned during submit.
            unsafe {
                (*irp).offset = disk_offset;
                (p.disk.submit)(irp, p.disk.ctx);
            }
        }
        None => {
            // SAFETY: `irp` is in flight; set its status and queue its completion.
            unsafe {
                (*irp).set_completion(KError::InvalidArgument as i32, 0);
                crate::dpc::enqueue(&(*irp).dpc);
            }
        }
    }
}

/// **What a partition's window refuses `op` with**, before anything is forwarded: `PeerClosed`
/// once retired, and `Unsupported` for a rescan or an eject, the disk's alone, with which one
/// partition's holder (every filesystem server) could take its siblings away.
fn window_refusal(p: &Partition, op: u32) -> Option<KError> {
    if p.retired.load(core::sync::atomic::Ordering::SeqCst) {
        Some(KError::PeerClosed)
    } else if op == IrpOp::Rescan as u32 || op == IrpOp::Eject as u32 {
        Some(KError::Unsupported)
    } else {
        None
    }
}

/// Translate a partition-relative byte `offset` into a disk-absolute one, or
/// `None` if `[offset, offset + length)` falls outside the partition.
fn partition_rebase(offset: u64, length: u64, start_lba: u64, block_count: u64, sector_size: u64) -> Option<u64> {
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

/// A [`BlockBackend`] for a partition window; `partition` must outlive every IRP
/// submitted to it (it is leaked `'static`).
pub fn partition_backend(partition: &'static Partition) -> BlockBackend {
    BlockBackend {
        submit: partition_submit,
        poll: partition_poll,
        ctx: partition as *const Partition as *mut (),
        // **Inherited, not restated**: a partition forwards the same IRP to the
        // same hardware, so the disk's limit applies; its own number could disagree.
        max_frags: partition.disk.max_frags,
    }
}

impl Partition {
    /// Create a partition window `[start_lba, start_lba + block_count)` over block
    /// device `disk`, returning the leaked `'static` partition, or `None` if
    /// `disk` is not a block device.
    pub fn new(disk: &ObjectRef, start_lba: u64, block_count: u64, sector_size: u64) -> Option<&'static Partition> {
        // SAFETY: `disk` pins a live `DeviceNode`.
        let dn: &DeviceNode = unsafe { &*(disk.as_ptr() as *const DeviceNode) };
        let disk_backend = dn.block_backend()?;
        let boxed = KBox::try_new(Partition {
            disk: disk_backend,
            start_lba,
            block_count,
            sector_size,
            retired: core::sync::atomic::AtomicBool::new(false),
            entering: core::sync::atomic::AtomicU32::new(0),
        })
        .ok()?;
        // SAFETY: `into_raw` yields a stable, uniquely-owned pointer.
        Some(unsafe { &*KBox::into_raw(boxed).as_ptr() })
    }

    /// Total sectors in the partition.
    pub fn block_count(&self) -> u64 {
        self.block_count
    }
}

/// The partition window behind `node`, if `node` is one: a disk's node is not.
fn window_of(node: &ObjectRef) -> Option<&'static Partition> {
    // SAFETY: `node` pins a live `DeviceNode`.
    let dn: &DeviceNode = unsafe { &*(node.as_ptr() as *const DeviceNode) };
    let backend = dn.block_backend()?;
    if backend.submit as *const () != partition_submit as *const () {
        return None;
    }
    // SAFETY: a backend whose `submit` is `partition_submit` has a leaked `'static` `Partition` as
    // its context (`partition_backend`).
    Some(unsafe { &*(backend.ctx as *const Partition) })
}

/// **Retire the partition window behind `node`**: every IRP is refused from
/// here, for a rescan that departs its record; a disk's node is left alone
/// (`false`). A submit already past the check may still be forwarding:
/// [`window_busy`] says when none is.
pub fn retire_window(node: &ObjectRef) -> bool {
    let Some(p) = window_of(node) else { return false };
    p.retired.store(true, core::sync::atomic::Ordering::SeqCst);
    true
}

/// **Whether a submit is between `node`'s window's retired check and its
/// forward**: what a rescan waits out after [`retire_window`], so every IRP the
/// window let through is at its disk, and none arrives after.
pub fn window_busy(node: &ObjectRef) -> bool {
    window_of(node).is_some_and(|p| p.entering.load(core::sync::atomic::Ordering::SeqCst) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;

    #[test]
    fn partition_rebase_maps_relative_to_absolute() {
        let start = 2048u64;
        let count = 1000u64;
        let ss = 512u64;
        assert_eq!(partition_rebase(0, 512, start, count, ss), Some(2048 * 512));
        assert_eq!(partition_rebase(8192, 512, start, count, ss), Some(2048 * 512 + 8192));
        assert_eq!(partition_rebase((count - 1) * ss, ss, start, count, ss), Some((start + count - 1) * ss));
        assert_eq!(partition_rebase((count - 1) * ss, ss + 1, start, count, ss), None);
        assert_eq!(partition_rebase(count * ss, ss, start, count, ss), None);
    }

    /// A window refuses a rescan itself, and once retired refuses everything with
    /// `PeerClosed`.
    #[test]
    fn a_window_refuses_a_rescan_and_once_retired_everything() {
        use core::sync::atomic::{AtomicBool, Ordering};
        fn disk_submit(_: *mut Irp, _: *mut ()) {}
        fn disk_poll(_: *mut ()) {}
        let p = Partition {
            disk: BlockBackend { submit: disk_submit, poll: disk_poll, ctx: core::ptr::null_mut(), max_frags: 8 },
            start_lba: 2048,
            block_count: 100,
            sector_size: 512,
            retired: AtomicBool::new(false),
            entering: core::sync::atomic::AtomicU32::new(0),
        };
        let answer = |op: IrpOp| window_refusal(&p, op as u32);
        assert_eq!(answer(IrpOp::Read), None, "a read goes down");
        assert_eq!(answer(IrpOp::Write), None);
        assert_eq!(answer(IrpOp::Flush), None, "and a flush");
        assert_eq!(answer(IrpOp::Rescan), Some(KError::Unsupported), "a rescan is the disk's alone");
        assert_eq!(answer(IrpOp::Eject), Some(KError::Unsupported), "and an eject");
        p.retired.store(true, Ordering::Release);
        for op in [IrpOp::Read, IrpOp::Write, IrpOp::Flush, IrpOp::Rescan, IrpOp::Eject] {
            assert_eq!(answer(op), Some(KError::PeerClosed), "retired: {op:?}");
        }
    }

    /// The window's own submit and retirement, driven through `partition_submit`:
    /// a read reaches the disk rebased and a rescan does not; once retired,
    /// nothing more reaches the disk and no submit is left counted in.
    #[test]
    fn a_window_node_refuses_a_rescan_and_once_retired_forwards_nothing() {
        use crate::libkern::block::BlockKind;
        use crate::object::device_node::{BlockGeometry, ResourceDescriptor};
        use core::cell::RefCell;
        init_global_heap();
        // The global queue's reserve, as the boot makes it: a refusal completes through a DPC.
        crate::dpc::init().unwrap();
        /// What reached the disk: each IRP's op and disk-absolute offset.
        struct Seen(RefCell<std::vec::Vec<(u32, u64)>>);
        fn disk_submit(irp: *mut Irp, ctx: *mut ()) {
            // SAFETY: `ctx` is the test's `Seen`; `irp` is the test's live IRP.
            unsafe { (*(ctx as *const Seen)).0.borrow_mut().push(((*irp).op, (*irp).offset)) };
        }
        fn disk_poll(_: *mut ()) {}
        let seen = Seen(RefCell::new(std::vec::Vec::new()));
        let adopt = |node: KBox<DeviceNode>| {
            // SAFETY: `into_raw` yields the single creation reference, which the test adopts.
            unsafe {
                ObjectRef::from_raw(
                    KBox::into_raw(node).as_ptr() as *mut (),
                    crate::libkern::handle::KObjectType::DeviceNode,
                )
            }
        };
        let geometry = |blocks| BlockGeometry { logical_block_size: 512, block_count: blocks };
        let backend =
            BlockBackend { submit: disk_submit, poll: disk_poll, ctx: &seen as *const Seen as *mut (), max_frags: 8 };
        let disk = adopt(
            DeviceNode::try_new_block(ResourceDescriptor::ZERO, geometry(8192), BlockKind::Disk, b"disk", backend)
                .unwrap(),
        );
        let window = Partition::new(&disk, 2048, 100, 512).unwrap();
        let wb = partition_backend(window);
        let part = adopt(
            DeviceNode::try_new_block(ResourceDescriptor::ZERO, geometry(100), BlockKind::Partition, b"p", wb).unwrap(),
        );
        // An IRP through the partition node's backend; returns its status after
        // any refusal's completion has run.
        let submit = |op: IrpOp, offset: u64| {
            let mut irp = Irp::new_block(op, core::ptr::null(), offset, 512, IrpBuffer::NONE, core::ptr::null_mut(), 0);
            // SAFETY: `part` pins the node; its backend's context is the leaked window.
            let b = unsafe { &*(part.as_ptr() as *const DeviceNode) }.block_backend().unwrap();
            (b.submit)(&mut irp, b.ctx);
            crate::dpc::run_pending();
            irp.status
        };
        let pending = crate::io::irp::IrpStatus::Pending as i32;
        assert_eq!(submit(IrpOp::Read, 512), pending, "forwarded, for the disk to complete");
        assert_eq!(seen.0.borrow().as_slice(), &[(IrpOp::Read as u32, 2049 * 512)], "rebased to the disk");
        assert_eq!(submit(IrpOp::Rescan, 0), KError::Unsupported as i32, "a rescan is the disk's alone");
        assert_eq!(submit(IrpOp::Eject, 0), KError::Unsupported as i32, "and an eject");
        assert_eq!(seen.0.borrow().len(), 1, "and neither reached the disk");

        assert!(!retire_window(&disk), "a disk's node is no window");
        assert!(retire_window(&part));
        for op in [IrpOp::Read, IrpOp::Write, IrpOp::Flush, IrpOp::Rescan] {
            assert_eq!(submit(op, 0), KError::PeerClosed as i32, "retired: {op:?}");
        }
        assert_eq!(seen.0.borrow().len(), 1, "nothing more reached the disk");
        assert!(!window_busy(&part), "every submit counted out");
        assert!(!window_busy(&disk));
    }

    /// Page-cache frames go to the device as one IRP, in order; more than it takes
    /// is refused `InvalidArgument` before anything reaches it.
    #[test]
    fn frames_go_to_the_device_as_one_irp_and_more_than_it_takes_are_refused() {
        use crate::libkern::block::BlockKind;
        use crate::libkern::handle::KObjectType;
        use crate::object::PendingOperation;
        use crate::object::device_node::{BlockGeometry, ResourceDescriptor};
        use core::cell::RefCell;
        init_global_heap();
        /// Each IRP that reached the device, with its fragments.
        struct Seen(RefCell<std::vec::Vec<(*mut Irp, std::vec::Vec<PhysFrag>)>>);
        fn disk_submit(irp: *mut Irp, ctx: *mut ()) {
            // SAFETY: `ctx` is the test's `Seen`; `irp` is a live IRP whose buffer describes `count`
            // fragments.
            unsafe {
                let b = (*irp).buffer;
                let frags = core::slice::from_raw_parts(b.frags as *const PhysFrag, b.count as usize).to_vec();
                (*(ctx as *const Seen)).0.borrow_mut().push((irp, frags));
            }
        }
        fn disk_poll(_: *mut ()) {}
        let seen = Seen(RefCell::new(std::vec::Vec::new()));
        let backend =
            BlockBackend { submit: disk_submit, poll: disk_poll, ctx: &seen as *const Seen as *mut (), max_frags: 8 };
        let geometry = BlockGeometry { logical_block_size: 512, block_count: 8192 };
        let node =
            DeviceNode::try_new_block(ResourceDescriptor::ZERO, geometry, BlockKind::Disk, b"disk", backend).unwrap();
        // SAFETY: `into_raw` yields each single creation reference, which the test adopts.
        let (disk, po) = unsafe {
            (
                ObjectRef::from_raw(KBox::into_raw(node).as_ptr() as *mut (), KObjectType::DeviceNode),
                ObjectRef::from_raw(
                    KBox::into_raw(PendingOperation::try_new().unwrap()).as_ptr() as *mut (),
                    KObjectType::PendingOperation,
                ),
            )
        };
        let frames: std::vec::Vec<PhysAddr> = (0..9).map(|i| PhysAddr(0x10_0000 + i * 0x3000)).collect();
        let page = PAGE_SIZE as u64;
        let write = |frames: &[PhysAddr], length| {
            dispatch_block_irp_into_frames(&disk, frames, disk.clone(), &po, IoOpcode::Write, 4096, length)
        };
        assert_eq!(write(&frames, 9 * page), Err(KError::InvalidArgument), "nine frames, eight fragments");
        assert_eq!(write(&frames[..8], 9 * page), Err(KError::InvalidArgument), "a length past the frames");
        assert!(seen.0.borrow().is_empty(), "nothing reached the device");
        write(&frames[..8], 8 * page).unwrap();
        let (irp, frags) = seen.0.borrow_mut().pop().unwrap();
        let expected: std::vec::Vec<PhysFrag> = frames[..8].iter().map(|f| PhysFrag { base: f.0, len: page }).collect();
        assert_eq!(frags, expected, "a fragment a frame, in order");
        // SAFETY: the IRP is live until it is completed below.
        unsafe { assert_eq!(((*irp).op, (*irp).offset, (*irp).length), (IrpOp::Write as u32, 4096, 8 * page)) };
        // Completed as the device would complete it, and its box reclaimed.
        irp_complete_dpc(irp as *mut ());
        reclaim_completed();
    }

    #[test]
    fn frags_cover_aligned_range_one_per_page() {
        init_global_heap();
        let frames = [PhysAddr(0x1000), PhysAddr(0x5000), PhysAddr(0x9000)];
        let frags = build_frags(&frames, 0, 3 * PAGE_SIZE as u64).unwrap();
        assert_eq!(frags.len(), 3);
        assert_eq!(frags[0], PhysFrag { base: 0x1000, len: PAGE_SIZE as u64 });
        assert_eq!(frags[1], PhysFrag { base: 0x5000, len: PAGE_SIZE as u64 });
        assert_eq!(frags[2], PhysFrag { base: 0x9000, len: PAGE_SIZE as u64 });
    }

    #[test]
    fn frags_split_unaligned_head_and_tail() {
        init_global_heap();
        let frames = [PhysAddr(0x1000), PhysAddr(0x2000)];
        let frags = build_frags(&frames, 256, 512).unwrap();
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0], PhysFrag { base: 0x1000 + 256, len: 512 });
        let frags = build_frags(&frames, PAGE_SIZE as u64 - 100, 200).unwrap();
        assert_eq!(frags.len(), 2);
        assert_eq!(frags[0], PhysFrag { base: 0x1000 + PAGE_SIZE as u64 - 100, len: 100 });
        assert_eq!(frags[1], PhysFrag { base: 0x2000, len: 100 });
    }

    /// One fragment per page, with **no ceiling of its own**, which is why the
    /// caller must check the device's: `sys_io_submit` bounds the range only
    /// against the buffer's size.
    #[test]
    fn the_fragment_count_grows_with_the_transfer_and_is_never_clamped() {
        init_global_heap();
        let frames: KVec<PhysAddr> = {
            let mut v = KVec::new();
            v.try_reserve(600).unwrap();
            for i in 0..600u64 {
                v.try_push(PhysAddr(0x10_0000 + i * PAGE_SIZE as u64)).unwrap();
            }
            v
        };
        for pages in [1usize, 248, 249, 600] {
            let frags = build_frags(&frames, 0, pages as u64 * PAGE_SIZE as u64).unwrap();
            assert_eq!(frags.len(), pages, "{pages} pages should be {pages} fragments");
        }
    }

    /// An unaligned start costs **one extra fragment**: why the dispatch check
    /// counts *fragments*, not a byte ceiling.
    #[test]
    fn an_unaligned_start_costs_an_extra_fragment() {
        init_global_heap();
        let frames: KVec<PhysAddr> = {
            let mut v = KVec::new();
            v.try_reserve(4).unwrap();
            for i in 0..4u64 {
                v.try_push(PhysAddr(0x10_0000 + i * PAGE_SIZE as u64)).unwrap();
            }
            v
        };
        let two_pages = 2 * PAGE_SIZE as u64;
        assert_eq!(build_frags(&frames, 0, two_pages).unwrap().len(), 2);
        assert_eq!(build_frags(&frames, 1, two_pages).unwrap().len(), 3);
    }
}
