//! The block I/O core — building and dispatching a block [`Irp`].
//!
//! A block [`DeviceNode`](crate::object::DeviceNode) carries a [`BlockBackend`]
//! (a submit function + context) installed by its driver. [`dispatch_block_irp`]
//! builds an [`Irp`] in an owning [`IrpBox`], sets its `on_complete` and hands it
//! to the backend; once completed it signals the request's `PendingOperation` and
//! leaves the box for thread context to free (see [`finish`]).
//!
//! See `docs/architecture/drivers-and-irps.md` and `docs/spec/irp-layout.md`.

use crate::io::irp::{IRP_BUF_FRAGS, Irp, IrpBuffer, IrpOp, PhysFrag};
use crate::libkern::io_op::IoOpcode;
use crate::libkern::lockrank::LockRank;
use crate::libkern::{IrqSpinLock, KBox, KVec};
use crate::mm::{PAGE_SIZE, PhysAddr};
use crate::object::{DeviceNode, MemoryObject, ObjectRef};
use crate::syscall::error::KError;
use core::sync::atomic::{AtomicUsize, Ordering};

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
/// completion's [`finish`] recovers the box from the `irp` pointer. Its `ObjectRef`s keep
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
    /// It makes the hand-off in [`finish`] allocation-free and
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

/// **Whether `dn` takes a transfer of `length` bytes at `offset` as whole logical blocks**
/// (`docs/spec/io-operation.md`); no driver sees part of a block.
fn whole_blocks(dn: &DeviceNode, offset: u64, length: u64) -> Result<(), KError> {
    let size = dn.block_info().logical_block_size as u64;
    if size == 0 || !offset.is_multiple_of(size) || !length.is_multiple_of(size) {
        return Err(KError::InvalidArgument);
    }
    Ok(())
}

/// **Submit `irp` to `backend`, or complete it `InvalidArgument` if its range is past `dn`'s end**,
/// an outcome, not an argument failure (`docs/spec/io-operation.md`).
fn submit_within(dn: &DeviceNode, backend: BlockBackend, irp: *mut Irp) {
    let info = dn.block_info();
    // SAFETY: `irp` is freshly placed and uniquely owned until it is submitted or completed.
    let (offset, length) = unsafe { ((*irp).offset, (*irp).length) };
    let span = info.block_count.checked_mul(info.logical_block_size as u64);
    if offset.checked_add(length).zip(span).is_some_and(|(end, span)| end <= span) {
        (backend.submit)(irp, backend.ctx);
    } else {
        // SAFETY: as above; completed as a refusing driver completes it.
        unsafe { crate::io::irp::complete(irp, KError::InvalidArgument as i32, 0) };
    }
}

/// Build a block IRP for `opcode` on `device`, moving `length` bytes between
/// device offset `dev_offset` and the `buffer` `MemoryObject`'s range
/// `[buf_offset, ...)`, completing `po`. Handle types, rights and the buffer's bounds are the
/// caller's responsibility; **the device's are checked here**.
///
/// Returns `Err` **before the operation starts**, so the caller rolls back `po`:
/// `OutOfMemory`, `Unsupported` without a block backend, `InvalidArgument` if
/// the transfer is not whole blocks or needs more fragments than the device can describe in one
/// command. A range past the device's end completes `po` `InvalidArgument`.
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
    whole_blocks(dn, dev_offset, length)?;

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
    // `frags` and have its completion finish the box (`irp` is first).
    // SAFETY: `bx_ptr` is a freshly placed, uniquely-owned `IrpBox`.
    let irp_ptr = unsafe {
        let bx = &mut *bx_ptr;
        bx.irp.buffer =
            IrpBuffer { kind: IRP_BUF_FRAGS, count: bx.frags.len() as u32, frags: bx.frags.as_ptr() as u64 };
        bx.irp.on_complete = finish;
        &mut bx.irp as *mut Irp
    };

    // Completes asynchronously: the backend, or its completion interrupt, calls `irp::complete`.
    submit_within(dn, backend, irp_ptr);
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
    whole_blocks(dn, dev_offset, length)?;
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
    // the now-stable `frags`, and have its completion finish the box (irp is the first field).
    let irp_ptr = unsafe {
        let bx = &mut *bx_ptr;
        bx.irp.buffer =
            IrpBuffer { kind: IRP_BUF_FRAGS, count: bx.frags.len() as u32, frags: bx.frags.as_ptr() as u64 };
        bx.irp.on_complete = finish;
        &mut bx.irp as *mut Irp
    };
    submit_within(dn, backend, irp_ptr);
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
    // SAFETY: `bx_ptr` is a freshly placed, uniquely-owned `IrpBox`; have its completion finish
    // the box (`irp` is the first field). `irp.buffer` stays `NONE`: nothing to transfer.
    let irp_ptr = unsafe {
        let bx = &mut *bx_ptr;
        bx.irp.on_complete = finish;
        &mut bx.irp as *mut Irp
    };
    (backend.submit)(irp_ptr, backend.ctx);
    Ok(())
}

/// Boxes whose IRP has completed, awaiting reclamation in **thread** context. An
/// `IrqSpinLock` because [`finish`] pushes from the interrupt-dispatch
/// tail; masking interrupts is allocation-free.
static RECLAIM: IrqSpinLock<ReclaimHead> = IrqSpinLock::new(LockRank::Leaf, ReclaimHead(core::ptr::null_mut()));

/// Head of the intrusive reclaim list.
struct ReclaimHead(*mut IrpBox);

/// The boxes on [`RECLAIM`]: written under it, read without it.
static RECLAIM_LEN: AtomicUsize = AtomicUsize::new(0);

/// The backlog at which a completion wakes the reaper to drain [`RECLAIM`]; an idle CPU drains it
/// sooner.
pub const RECLAIM_WAKE_AT: usize = 32;

/// How many completed boxes wait for [`reclaim_completed`].
pub fn reclaim_backlog() -> usize {
    RECLAIM_LEN.load(Ordering::Acquire)
}

// SAFETY: the pointer is an `IrpBox` this module allocated and owns exclusively from the
// moment its IRP completes; it is reachable only under `RECLAIM`, and the thread-context
// drain takes the whole list before touching any of it. Same reasoning as `BlockBackend`
// above — a raw pointer into memory this module owns for a well-defined interval.
unsafe impl Send for ReclaimHead {}

/// Drop every completed IRP box. **Thread context only**: the frees
/// [`finish`] must not do happen here, called from
/// `sched::reap_pending`. Each parked box pins its `_buffer`, so until a drain a
/// machine holds every completed transfer's buffer object: an idle CPU drains them, and with
/// none idle, the reaper once [`RECLAIM_WAKE_AT`] wait.
pub fn reclaim_completed() {
    // Take the whole list in one swap and drop outside the lock: a drop reaches
    // the allocator, which an IRQ-masking lock must not be held across.
    let mut head = {
        let mut g = RECLAIM.lock();
        RECLAIM_LEN.store(0, Ordering::Release);
        core::mem::replace(&mut g.0, core::ptr::null_mut())
    };
    while !head.is_null() {
        // SAFETY: `head` came off the reclaim list, where `finish` put a box it
        // had finished with and nothing else references. `reclaim_next` was written before
        // the push.
        let next = unsafe { (*head).reclaim_next };
        // SAFETY: the box was `KBox::into_raw`'d in `dispatch_block_irp` and is reclaimed
        // exactly once — the completion path pushes it here exactly once.
        drop(unsafe { KBox::from_raw(core::ptr::NonNull::new_unchecked(head)) });
        head = next;
    }
}

/// [`finish`], for a host test in another module to complete an IRP its
/// recording device took.
///
/// # Safety
///
/// `irp` is the in-flight IRP of a box `dispatch_block_irp` armed, completed once.
#[cfg(test)]
pub(crate) unsafe fn complete_for_test(irp: *mut Irp) {
    // SAFETY: forwarded.
    unsafe { finish(irp) };
}

/// A block IRP's `on_complete`, at a DPC drain: signal the request's `PendingOperation` with the
/// terminal status and bytes transferred, then **hand the `IrpBox` to [`reclaim_completed`]**; it
/// must not drop it. `irp` is the box's (`irp` is its first field).
///
/// # Safety
/// `irp` is the IRP of a box a dispatch here made, completed once.
unsafe fn finish(irp: *mut Irp) {
    let bx_ptr = irp as *mut IrpBox;
    // SAFETY: the box is live until this reclaims it (the IRP completes exactly once).
    let (status, transferred, po) = unsafe {
        let irp = &(*bx_ptr).irp;
        (irp.status, irp.transferred, irp.completion as *mut ())
    };
    // SAFETY: the box's `_po` pins the `PendingOperation` until thread context reclaims the box.
    unsafe { crate::sched::complete_pending_op(po, status, transferred) };
    // **Hand the box to thread context; do not drop it here.** Dropping releases
    // `_po`/`_buffer`/`_device` and `frags`, each of which reaches
    // `SlabCache::free`, whose lock is a plain `SpinLock` (preemption off,
    // interrupts on). A DPC runs on top of whatever the CPU was doing, so if it
    // interrupted a thread already holding that lock the free would spin forever
    // on a lock the frame beneath it can never release. `kernel/CLAUDE.md`:
    // "freeing counts as allocating".
    // SAFETY: the box is live and owned by this completion; nothing else references it
    // once the IRP has completed, so writing its link and publishing it is sound.
    let backlog = unsafe {
        let mut g = RECLAIM.lock();
        (*bx_ptr).reclaim_next = g.0;
        g.0 = bx_ptr;
        RECLAIM_LEN.fetch_add(1, Ordering::AcqRel) + 1
    };
    if backlog == RECLAIM_WAKE_AT {
        crate::sched::wake_reaper_to_reclaim();
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
    let done = unsafe { crate::sched::pending_op_is_signaled(po_ref.as_ptr()) };
    // SAFETY: as above.
    let (status, _result) = unsafe { crate::sched::pending_op_completion(po_ref.as_ptr()) };
    // **Not completed is not read**: a command the device kept, or a refusal whose DPC no poll ran,
    // reads status 0 too.
    if !done || status != 0 {
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
        // SAFETY: `irp` is in flight, and completed here once.
        unsafe { crate::io::irp::complete(irp, e as i32, 0) };
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
            // SAFETY: `irp` is in flight, and completed here once.
            unsafe { crate::io::irp::complete(irp, KError::InvalidArgument as i32, 0) };
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
mod tests;
