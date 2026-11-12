//! The block I/O core — building and dispatching a block [`Irp`].
//!
//! A block [`DeviceNode`](crate::object::DeviceNode) carries a [`BlockBackend`]
//! (a [`BlockDevice`] context and its submit and poll) installed by its driver. [`dispatch_block_irp`]
//! builds an [`Irp`] in an owning [`IrpBox`], sets its `on_complete` and hands it
//! to the backend; once completed it signals the request's `PendingOperation` and
//! leaves the box for thread context to free (see [`finish`]).
//!
//! See `docs/architecture/drivers-and-irps.md` and `docs/spec/irp-layout.md`.

use crate::io::irp::{IRP_BUF_FRAGS, Irp, IrpBuffer, IrpOp, PhysFrag};
use crate::libkern::io_op::IoOpcode;
use crate::libkern::lockrank::LockRank;
use crate::libkern::{IrqSpinLock, KBox, KVec};
use crate::mm::frame::Frame;
use crate::mm::{PAGE_SIZE, PhysAddr};
use crate::object::header::Ref;
use crate::object::{DeviceNode, MemoryObject, ObjectRef, PendingOperation};
use crate::syscall::error::KError;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};

mod in_flight;
pub use in_flight::InFlight;

/// **A block device's driver, as its backend's context**: a reference to the device's state, or a
/// value naming it, which [`BlockBackend`] hands back to its `submit` and `poll`. Each device
/// synchronises its own state, on whichever CPU submits.
pub trait BlockDevice: Copy + Send + Sync + 'static {
    /// Take `irp` in flight: program the device and return, never blocking.
    fn submit(self, irp: InFlight);
    /// Drive the in-flight command to completion by **polling**, then run its
    /// completion DPC: for synchronous kernel reads at boot, before interrupts
    /// are enabled (see [`read_blocking`]).
    fn poll(self);
}

/// A block device's I/O entry point, stored on its [`DeviceNode`]: a [`BlockDevice`] context and
/// the `submit` and `poll` that read it back as its type, paired by [`new`](Self::new), so no
/// backend's context reaches another's functions.
#[derive(Copy, Clone)]
pub struct BlockBackend {
    ctx: MaybeUninit<usize>,
    submit: unsafe fn(MaybeUninit<usize>, InFlight),
    poll: unsafe fn(MaybeUninit<usize>),
    max_frags: u32,
}

impl BlockBackend {
    /// `ctx`'s backend, describing at most `max_frags` fragments in one command.
    pub fn new<C: BlockDevice>(ctx: C, max_frags: u32) -> Self {
        const { assert!(size_of::<C>() <= size_of::<usize>() && align_of::<C>() <= align_of::<usize>()) };
        let mut word = MaybeUninit::<usize>::uninit();
        // SAFETY: a `C` fits the word, as checked above.
        unsafe { word.as_mut_ptr().cast::<C>().write(ctx) };
        BlockBackend { ctx: word, submit: submit_as::<C>, poll: poll_as::<C>, max_frags }
    }

    /// Submit an IRP to the device.
    pub fn submit(&self, irp: InFlight) {
        // SAFETY: `new` wrote `ctx` as the type `submit` reads it back as.
        unsafe { (self.submit)(self.ctx, irp) }
    }

    /// Poll the device's in-flight command to completion ([`BlockDevice::poll`]).
    pub fn poll(&self) {
        // SAFETY: as `submit`.
        unsafe { (self.poll)(self.ctx) }
    }

    /// The most physical fragments this device can describe in **one** command:
    /// a device constraint only the driver knows (AHCI's is its command table's
    /// PRDT capacity; a ramdisk has none). [`dispatch_block_irp`] refuses a
    /// transfer needing more, which keeps a driver from writing past the
    /// structure it programs. Counted in fragments because the hardware limit is
    /// a descriptor count.
    pub fn max_frags(&self) -> u32 {
        self.max_frags
    }
}

/// **A host test's backend**: its own `submit` and `poll`, and a context they read back, on the
/// test's thread.
#[cfg(test)]
pub(crate) fn test_backend(
    submit: fn(*mut Irp, *mut ()),
    poll: fn(*mut ()),
    ctx: *mut (),
    max_frags: u32,
) -> BlockBackend {
    struct Raw(fn(*mut Irp, *mut ()), fn(*mut ()), *mut ());
    // SAFETY: a host test's functions and context, which it drives on its own thread.
    unsafe impl Send for Raw {}
    // SAFETY: as above.
    unsafe impl Sync for Raw {}
    impl BlockDevice for &'static Raw {
        fn submit(self, irp: InFlight) {
            (self.0)(irp.into_raw(), self.2)
        }
        fn poll(self) {
            (self.1)(self.2)
        }
    }
    BlockBackend::new(&*std::boxed::Box::leak(std::boxed::Box::new(Raw(submit, poll, ctx))), max_frags)
}

/// # Safety
/// `ctx` holds a `C`, as [`BlockBackend::new`] wrote it.
unsafe fn submit_as<C: BlockDevice>(ctx: MaybeUninit<usize>, irp: InFlight) {
    // SAFETY: the caller's.
    unsafe { ctx.as_ptr().cast::<C>().read() }.submit(irp)
}

/// # Safety
/// As [`submit_as`].
unsafe fn poll_as<C: BlockDevice>(ctx: MaybeUninit<usize>) {
    // SAFETY: the caller's.
    unsafe { ctx.as_ptr().cast::<C>().read() }.poll()
}

/// The owning wrapper around an in-flight [`Irp`]. `#[repr(C)]` with `irp`
/// **first**, so `*mut IrpBox` and `*mut Irp` share an address and the
/// completion's [`finish`] recovers the box from the `irp` pointer, which a dispatch makes from
/// the box's, so that it reaches the whole box. Its `ObjectRef`s keep
/// the raw pointers inside `irp` valid; `frags` backs `irp.buffer`. None of this is the
/// hashed `Irp` layout, which is all a Tier 2 module would see.
#[repr(C)]
pub(crate) struct IrpBox {
    irp: Irp,
    /// Backing store for `irp.buffer.frags`.
    frags: KVec<PhysFrag>,
    /// Owning references that pin the objects `irp` points at.
    _po: Ref<PendingOperation>,
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
fn submit_within(dn: &DeviceNode, backend: BlockBackend, irp: InFlight) {
    let info = dn.block_info();
    let span = info.block_count.checked_mul(info.logical_block_size as u64);
    if irp.offset().checked_add(irp.length()).zip(span).is_some_and(|(end, span)| end <= span) {
        backend.submit(irp);
    } else {
        irp.complete(KError::InvalidArgument as i32, 0);
    }
}

/// **Place `bx` and mint its IRP in flight**: its buffer pointed at its fragments, and its
/// completion finishing the box. `InvalidArgument`, the box freed, if the fragments do not hold the
/// IRP's length.
fn arm(bx: KBox<IrpBox>) -> Result<InFlight, KError> {
    let bx_ptr = KBox::into_raw(bx).as_ptr();
    // SAFETY: `bx_ptr` is a freshly placed, uniquely-owned `IrpBox`; `irp` is its first field.
    let irp = unsafe {
        let bx = &mut *bx_ptr;
        if !bx.frags.is_empty() {
            bx.irp.buffer =
                IrpBuffer { kind: IRP_BUF_FRAGS, count: bx.frags.len() as u32, frags: bx.frags.as_ptr() as u64 };
        }
        bx.irp.on_complete = finish;
        core::ptr::NonNull::new_unchecked(&raw mut (*bx_ptr).irp)
    };
    // SAFETY: the box's IRP, submitted nowhere yet; the box keeps its fragments until reclaimed,
    // after the completion.
    match unsafe { InFlight::mint(irp) } {
        Some(irp) => Ok(irp),
        None => {
            // SAFETY: the box placed above, which nothing else has reached.
            drop(unsafe { KBox::from_raw(core::ptr::NonNull::new_unchecked(bx_ptr)) });
            Err(KError::InvalidArgument)
        }
    }
}

/// Build a block IRP for `opcode` on `device`, moving `length` bytes between
/// device offset `dev_offset` and the `buffer` `MemoryObject`'s range
/// `[buf_offset, ...)`, completing `po`. Handle types, rights and the buffer's bounds are the
/// caller's responsibility; **the device's are checked here**.
///
/// Returns `Err` **before the operation starts**, so the caller rolls back `po`:
/// `OutOfMemory`, `Unsupported` without a block backend, `InvalidArgument` if
/// the transfer is not whole blocks, starts at an odd `buf_offset`, or needs more fragments than
/// the device can describe in one command. A range past the device's end completes `po`
/// `InvalidArgument`.
pub fn dispatch_block_irp(
    device: &Ref<DeviceNode>,
    buffer: &Ref<MemoryObject>,
    po: &Ref<PendingOperation>,
    opcode: IoOpcode,
    dev_offset: u64,
    buf_offset: u64,
    length: u64,
) -> Result<(), KError> {
    let dn: &DeviceNode = device;
    let backend = dn.block_backend().ok_or(KError::Unsupported)?;
    whole_blocks(dn, dev_offset, length)?;
    // **A disk moves 16-bit words**: an odd offset makes an odd address and count, which no PRDT holds.
    if !buf_offset.is_multiple_of(2) {
        return Err(KError::InvalidArgument);
    }

    let frags = build_frags(Frame::addrs(buffer.frames()), buf_offset, length)?;
    // **The device's own limit, checked against the exact fragment count.**
    // Refused rather than truncated or split: `sys_io_submit` bounds the range
    // only against the buffer's size, so a caller holding a block device handle
    // could otherwise make the driver write PRDT entries past its table and the
    // controller DMA past it.
    //
    // Splitting across several commands needs one `PendingOperation` to outlive
    // several IRPs. `TODO(block-transfer-split)`.
    if frags.len() > backend.max_frags() as usize {
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
        po.as_object().as_ptr(),
        0,
    );

    let bx = KBox::try_new(IrpBox {
        irp,
        frags,
        _po: po.clone(),
        _buffer: buffer.as_object().clone(),
        _device: device.as_object().clone(),
        reclaim_next: core::ptr::null_mut(),
    })
    .map_err(|_| KError::OutOfMemory)?;
    // Completes asynchronously: the backend, or its completion interrupt, completes it.
    submit_within(dn, backend, arm(bx)?);
    Ok(())
}

/// **The most fragments `device` describes in one command**
/// ([`max_frags`](BlockBackend::max_frags)), or `None` without a block backend;
/// it sizes the page cache's runs ([`dispatch_block_irp_into_frames`]).
pub fn max_frags(device: &DeviceNode) -> Option<u32> {
    device.block_backend().map(|b| b.max_frags())
}

/// Dispatch a block IRP moving `length` bytes between device offset `dev_offset`
/// and **physical `frames`** (a page each, in order), completing `po`. The DMA
/// target is raw page-cache frames, not a `MemoryObject`: zero-copy fills and
/// write-backs, a run of a file's pages in one command. `pin` keeps the frames'
/// owner (the `FileObject`) alive, held in the box's `_buffer`. `length` must be
/// `≤ frames.len() * PAGE_SIZE`. Errors as [`dispatch_block_irp`]; a caller sizes
/// its runs by [`max_frags`].
pub fn dispatch_block_irp_into_frames(
    device: &Ref<DeviceNode>,
    frames: &[PhysAddr],
    pin: ObjectRef,
    po: &Ref<PendingOperation>,
    opcode: IoOpcode,
    dev_offset: u64,
    length: u64,
) -> Result<(), KError> {
    let dn: &DeviceNode = device;
    let backend = dn.block_backend().ok_or(KError::Unsupported)?;
    whole_blocks(dn, dev_offset, length)?;
    if length > (frames.len() * PAGE_SIZE) as u64 {
        return Err(KError::InvalidArgument);
    }

    // A page-sized fragment per frame, refused past the device's limit as above.
    let frags = build_frags(frames, 0, length)?;
    if frags.len() > backend.max_frags() as usize {
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
        po.as_object().as_ptr(),
        0,
    );
    let bx = KBox::try_new(IrpBox {
        irp,
        frags,
        _po: po.clone(),
        _buffer: pin, // pins the frame owner (the FileObject), not a MemoryObject
        _device: device.as_object().clone(),
        reclaim_next: core::ptr::null_mut(),
    })
    .map_err(|_| KError::OutOfMemory)?;
    submit_within(dn, backend, arm(bx)?);
    Ok(())
}

/// Dispatch a **flush** of the block `device`: an IRP with no buffer and no range
/// that completes `po` once the device has written its volatile cache to its
/// medium (a partition passes it to its disk, a RAM disk completes it at once).
/// `Err` before anything starts: `Unsupported` without a block backend,
/// `OutOfMemory` if the IRP cannot be allocated.
pub fn dispatch_block_flush(device: &Ref<DeviceNode>, po: &Ref<PendingOperation>) -> Result<(), KError> {
    dispatch_block_control(device, po, IrpOp::Flush)
}

/// Dispatch an IRP that moves no data — a [`Flush`](IrpOp::Flush), a [`Rescan`](IrpOp::Rescan) of
/// a disk's table or an [`Eject`](IrpOp::Eject) of its medium — completing `po`. Errors as
/// [`dispatch_block_flush`]'s.
pub fn dispatch_block_control(device: &Ref<DeviceNode>, po: &Ref<PendingOperation>, op: IrpOp) -> Result<(), KError> {
    let backend = device.block_backend().ok_or(KError::Unsupported)?;
    let irp = Irp::new_block(op, device.as_ptr() as *const (), 0, 0, IrpBuffer::NONE, po.as_object().as_ptr(), 0);
    let bx = KBox::try_new(IrpBox {
        irp,
        frags: KVec::new(),
        _po: po.clone(),
        _buffer: device.as_object().clone(), // no buffer to pin; the slot holds the device again
        _device: device.as_object().clone(),
        reclaim_next: core::ptr::null_mut(),
    })
    .map_err(|_| KError::OutOfMemory)?;
    // `irp.buffer` stays `NONE`: nothing to transfer.
    backend.submit(arm(bx)?);
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

/// The backlog at which a completion wakes the reaper to drain [`RECLAIM`].
pub const RECLAIM_WAKE_AT: usize = 32;

/// How many completed boxes wait for [`reclaim_completed`].
pub fn reclaim_backlog() -> usize {
    RECLAIM_LEN.load(Ordering::Acquire)
}

// SAFETY: the pointer is an `IrpBox` this module allocated and owns exclusively from the
// moment its IRP completes; it is reachable only under `RECLAIM`, and the thread-context
// drain takes the whole list before touching any of it, so whichever CPU holds the lock may hold
// the pointer.
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
unsafe extern "C" fn finish(irp: *mut Irp) {
    let bx_ptr = irp as *mut IrpBox;
    // SAFETY: the box is live until this reclaims it (the IRP completes exactly once).
    let (status, transferred, po) = unsafe {
        let irp = &(*bx_ptr).irp;
        (irp.status, irp.transferred, irp.completion as *mut ())
    };
    // SAFETY: the box's `_po` pins the `PendingOperation` at `po` until thread context reclaims the
    // box.
    let po = unsafe { &*po.cast::<crate::object::PendingOperation>() };
    crate::sched::complete_pending_op(po, status, transferred);
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
/// failure (allocation, bad device, short `dst`, device error). **Before the other CPUs come up**:
/// another CPU's drain could take its IRP first.
pub fn read_blocking(device: &Ref<DeviceNode>, lba: u64, count: u64, dst: &mut [u8]) -> bool {
    debug_assert!(crate::sched::online_mask().count_ones() <= 1, "read_blocking with other CPUs online");
    let Some(backend) = device.block_backend() else {
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
    let buf_ref = Ref::new(buf);
    let Ok(po) = crate::object::PendingOperation::try_new() else {
        return false;
    };
    let po_ref = Ref::new(po);

    if dispatch_block_irp(device, &buf_ref, &po_ref, IoOpcode::Read, offset, 0, len as u64).is_err() {
        return false;
    }
    backend.poll();

    let done = crate::sched::pending_op_is_signaled(&po_ref);
    let (status, _result) = crate::sched::pending_op_completion(&po_ref);
    // **Not completed is not read**: a command the device kept, or a refusal whose DPC no poll ran,
    // reads status 0 too.
    if !done || status != 0 {
        return false;
    }

    buf_ref.copy_out(0, &mut dst[..len]) == len
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

impl BlockDevice for &'static Partition {
    /// Bounds-check, **rebase** the offset to disk-absolute and forward to the parent disk's backend.
    fn submit(self, irp: InFlight) {
        use core::sync::atomic::Ordering::SeqCst;
        // **Counted in before the retired check, out after the forward**, each `SeqCst` as
        // `retire_window`'s store and `window_busy`'s load are: either this sees the window retired,
        // or the rescan retiring it sees this submit and waits for it to reach the disk first.
        self.entering.fetch_add(1, SeqCst);
        forward(self, irp);
        self.entering.fetch_sub(1, SeqCst);
    }

    fn poll(self) {
        self.disk.poll();
    }
}

/// A partition's submit's body, between its count in and out: refuse, or rebase and forward.
fn forward(p: &Partition, mut irp: InFlight) {
    if let Some(e) = window_refusal(p, irp.op()) {
        irp.complete(e as i32, 0);
        return;
    }
    // **A flush names no range**: it is the disk's cache, not the partition's
    // blocks, so it goes down unchanged.
    if irp.op() == IrpOp::Flush as u32 {
        p.disk.submit(irp);
        return;
    }
    match partition_rebase(irp.offset(), irp.length(), p.start_lba, p.block_count, p.sector_size) {
        Some(disk_offset) => {
            irp.rebase(disk_offset);
            p.disk.submit(irp);
        }
        None => irp.complete(KError::InvalidArgument as i32, 0),
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

/// A [`BlockBackend`] for a partition window, leaked `'static`.
pub fn partition_backend(partition: &'static Partition) -> BlockBackend {
    // **Inherited, not restated**: a partition forwards the same IRP to the
    // same hardware, so the disk's limit applies; its own number could disagree.
    BlockBackend::new(partition, partition.disk.max_frags())
}

impl Partition {
    /// Create a partition window `[start_lba, start_lba + block_count)` over block
    /// device `disk`, returning the leaked `'static` partition, or `None` if
    /// `disk` is not a block device.
    pub fn new(disk: &DeviceNode, start_lba: u64, block_count: u64, sector_size: u64) -> Option<&'static Partition> {
        let disk_backend = disk.block_backend()?;
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
fn window_of(node: &DeviceNode) -> Option<&'static Partition> {
    node.window()
}

/// **Retire the partition window behind `node`**: every IRP is refused from
/// here, for a rescan that departs its record; a disk's node is left alone
/// (`false`). A submit already past the check may still be forwarding:
/// [`window_busy`] says when none is.
pub fn retire_window(node: &DeviceNode) -> bool {
    let Some(p) = window_of(node) else { return false };
    p.retired.store(true, core::sync::atomic::Ordering::SeqCst);
    true
}

/// **Whether a submit is between `node`'s window's retired check and its
/// forward**: what a rescan waits out after [`retire_window`], so every IRP the
/// window let through is at its disk, and none arrives after.
pub fn window_busy(node: &DeviceNode) -> bool {
    window_of(node).is_some_and(|p| p.entering.load(core::sync::atomic::Ordering::SeqCst) != 0)
}

#[cfg(test)]
mod tests;
