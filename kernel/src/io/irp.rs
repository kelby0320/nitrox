//! The `Irp` (I/O Request Packet) — the kernel-internal unit of I/O.
//!
//! Normative layout: `docs/spec/irp-layout.md`; model:
//! `docs/architecture/drivers-and-irps.md`. An `Irp` is not handle-accessible,
//! but a Tier 2 loadable driver walks `&mut Irp`, so the layout is a kernel-ABI
//! version-hash input, pinned by compile-time asserts at the end of this module.
//! The owning references that keep an in-flight IRP's raw pointers valid live in
//! the kernel-internal [`IrpBox`](crate::io::block::IrpBox), not in the hashed
//! `Irp`.

use crate::dpc::Dpc;
use crate::libkern::IrqSpinLock;
use crate::libkern::lockrank::LockRank;

/// `IRP_MAX_STACK` — the deepest driver stack built (GPT-over-AHCI = 2); the
/// headroom avoids a reallocation seam (`docs/spec/irp-layout.md`).
pub const IRP_MAX_STACK: usize = 4;

/// One physically-contiguous fragment of an IRP's data buffer — the form an AHCI
/// PRDT / NVMe PRP list consumes. `#[repr(C)]`, 16 bytes.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PhysFrag {
    /// Physical base of the fragment.
    pub base: u64,
    /// Length of the fragment in bytes.
    pub len: u64,
}

/// `IrpBuffer::kind` — no data buffer.
pub const IRP_BUF_NONE: u32 = 0;
/// `IrpBuffer::kind` — a `MemoryObject`'s frames, described by a fragment list.
pub const IRP_BUF_FRAGS: u32 = 1;

/// The IRP's data buffer, as a physical-fragment list. `#[repr(C)]`, 16 bytes.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct IrpBuffer {
    /// [`IRP_BUF_NONE`] or [`IRP_BUF_FRAGS`].
    pub kind: u32,
    /// Number of fragments at `frags`.
    pub count: u32,
    /// Kernel pointer to a `[PhysFrag; count]` (0 if `kind == IRP_BUF_NONE`).
    /// Owned by the IRP's allocation context for the IRP's lifetime.
    pub frags: u64,
}

impl IrpBuffer {
    /// An absent buffer.
    pub const NONE: IrpBuffer = IrpBuffer { kind: IRP_BUF_NONE, count: 0, frags: 0 };
}

/// One driver-stack frame: which `DeviceNode` this layer targets, an optional
/// completion routine run (in DPC context) as the IRP unwinds, and per-layer
/// scratch. `#[repr(C)]`, 24 bytes.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct IrpStackFrame {
    /// `*const DeviceNode` this frame drives (kernel pointer).
    pub device: u64,
    /// Completion-routine `fn(*mut Irp)` pointer (0 = none).
    pub completion: u64,
    /// Per-layer scratch (e.g. a partition's base offset).
    pub context: u64,
}

impl IrpStackFrame {
    /// An empty stack frame.
    pub const EMPTY: IrpStackFrame = IrpStackFrame { device: 0, completion: 0, context: 0 };
}

/// `IrpOp` — the operation an IRP carries. Kept numerically aligned with
/// [`IoOpcode`](crate::libkern::io_op::IoOpcode) for a trivial translation.
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IrpOp {
    /// Move data from the device into the buffer.
    Read = 0,
    /// Move data from the buffer onto the device.
    Write = 1,
    /// Write the device's volatile cache to its medium. Carries no buffer and no range.
    Flush = 2,
    /// Read a disk's partition table again. Carries no buffer and no range.
    Rescan = 3,
    /// Stop a removable disk's medium and eject it. Carries no buffer and no range.
    Eject = 4,
}

/// `IrpStatus` — the in-flight sentinel and terminal status. A completed IRP
/// carries `Success` (0) or a negative `KError` discriminant, so `Irp::status`
/// maps directly onto `IoResult::status`. `Pending` (1) never reaches userspace.
#[repr(i32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IrpStatus {
    /// Completed successfully.
    Success = 0,
    /// Still in flight.
    Pending = 1,
}

/// An I/O Request Packet. See the module docs and `docs/spec/irp-layout.md`.
#[repr(C)]
pub struct Irp {
    /// The operation ([`IrpOp`]).
    pub op: u32,
    /// Terminal status ([`IrpStatus`] / negative `KError`); meaningful once the
    /// completion PO is signalled.
    pub status: i32,
    /// Reserved; must be 0.
    pub flags: u32,
    /// Populated stack frames (`≤ IRP_MAX_STACK`).
    pub stack_count: u32,
    /// The frame the IRP is currently at (descends high→low).
    pub stack_index: u32,
    /// Reserved padding.
    pub _pad: u32,
    /// `ProcessId` that submitted the IRP (0 for an internal/self-test IRP).
    pub initiator: u64,
    /// Byte offset within the bottom device.
    pub offset: u64,
    /// Bytes to transfer.
    pub length: u64,
    /// Bytes actually transferred (set on completion).
    pub transferred: u64,
    /// The data buffer (physical fragment list).
    pub buffer: IrpBuffer,
    /// `*mut PendingOperation` signalled on completion (kernel pointer; 0 = none).
    pub completion: u64,
    /// The next completed IRP while this one waits to be finished: [`complete`]'s alone.
    pub next: *mut Irp,
    /// What finishing this IRP does once [`complete`] has recorded its outcome, at the next DPC
    /// drain: its dispatcher's to set. [`Irp::new_block`]'s does nothing.
    pub on_complete: unsafe fn(*mut Irp),
    /// The per-layer stack frames.
    pub stack: [IrpStackFrame; IRP_MAX_STACK],
}

impl Irp {
    /// Build a single-layer IRP targeting `device` for `length` bytes at
    /// `offset` against `buffer`, completing `completion` (a
    /// `*mut PendingOperation`); `status` starts `Pending`, and `on_complete` does nothing until
    /// its dispatcher sets it.
    pub fn new_block(
        op: IrpOp,
        device: *const (),
        offset: u64,
        length: u64,
        buffer: IrpBuffer,
        completion: *mut (),
        initiator: u64,
    ) -> Self {
        let mut stack = [IrpStackFrame::EMPTY; IRP_MAX_STACK];
        stack[0] = IrpStackFrame { device: device as u64, completion: 0, context: 0 };
        Irp {
            op: op as u32,
            status: IrpStatus::Pending as i32,
            flags: 0,
            stack_count: 1,
            stack_index: 0,
            _pad: 0,
            initiator,
            offset,
            length,
            transferred: 0,
            buffer,
            completion: completion as u64,
            next: core::ptr::null_mut(),
            on_complete: finish_nothing,
            stack,
        }
    }

    /// Record the terminal `status` and `transferred` count on a completed IRP: [`complete`]'s
    /// first step.
    pub fn set_completion(&mut self, status: i32, transferred: u64) {
        self.status = status;
        self.transferred = transferred;
    }
}

/// [`Irp::new_block`]'s `on_complete`: an IRP no dispatcher owns, a test's, needs no finishing.
unsafe fn finish_nothing(_: *mut Irp) {}

/// Completed IRPs waiting for [`FINISH`], first to last.
struct Done {
    head: *mut Irp,
    tail: *mut Irp,
}

// SAFETY: the IRPs are reached only under `DONE`, from `complete` until `finish_completed` takes
// the list, and each is the completing driver's to hand on.
unsafe impl Send for Done {}

static DONE: IrqSpinLock<Done> =
    IrqSpinLock::new(LockRank::Leaf, Done { head: core::ptr::null_mut(), tail: core::ptr::null_mut() });

/// **The one DPC that finishes completed IRPs**, so the DPC queue holds static DPCs only and its
/// capacity bounds them, rather than one DPC an IRP completed between two drains.
static FINISH: Dpc = Dpc::new(finish_completed, core::ptr::null_mut());

/// **Complete `irp` with `status` and `transferred`**, from a driver's completion, interrupt or
/// refusal, in any context; its `on_complete` runs at the next DPC drain.
///
/// # Safety
/// `irp` is live and in flight, completed this once, and touched by nothing else until its
/// `on_complete` has run.
pub unsafe fn complete(irp: *mut Irp, status: i32, transferred: u64) {
    // SAFETY: the caller's contract; `DONE` serialises the list's links.
    unsafe {
        (*irp).set_completion(status, transferred);
        (*irp).next = core::ptr::null_mut();
        let mut done = DONE.lock();
        match done.tail.is_null() {
            true => done.head = irp,
            false => (*done.tail).next = irp,
        }
        done.tail = irp;
    }
    crate::dpc::enqueue(&FINISH);
}

/// [`FINISH`]'s handler: take every completed IRP and run its `on_complete`, in completion order.
fn finish_completed(_: *mut ()) {
    let mut irp = {
        let mut done = DONE.lock();
        done.tail = core::ptr::null_mut();
        core::mem::replace(&mut done.head, core::ptr::null_mut())
    };
    while !irp.is_null() {
        // SAFETY: `irp` came off the list `complete` built, `next` written before it was pushed,
        // and is this drain's alone; its `on_complete` may end it, so `next` is read first.
        unsafe {
            let next = (*irp).next;
            ((*irp).on_complete)(irp);
            irp = next;
        }
    }
}

// --- Layout pinning (docs/spec/irp-layout.md) ---------------------------------

const _: () = {
    use core::mem::{align_of, offset_of, size_of};
    assert!(offset_of!(Irp, op) == 0);
    assert!(offset_of!(Irp, status) == 4);
    assert!(offset_of!(Irp, flags) == 8);
    assert!(offset_of!(Irp, stack_count) == 12);
    assert!(offset_of!(Irp, stack_index) == 16);
    assert!(offset_of!(Irp, initiator) == 24);
    assert!(offset_of!(Irp, offset) == 32);
    assert!(offset_of!(Irp, length) == 40);
    assert!(offset_of!(Irp, transferred) == 48);
    assert!(offset_of!(Irp, buffer) == 56);
    assert!(offset_of!(Irp, completion) == 72);
    assert!(offset_of!(Irp, next) == 80);
    assert!(offset_of!(Irp, on_complete) == 88);
    assert!(offset_of!(Irp, stack) == 96);
    assert!(size_of::<PhysFrag>() == 16);
    assert!(size_of::<IrpBuffer>() == 16);
    assert!(size_of::<IrpStackFrame>() == 24);
    assert!(align_of::<Irp>() == 8);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn irp_op_and_status_discriminants() {
        assert_eq!(IrpOp::Read as u32, 0);
        assert_eq!(IrpOp::Write as u32, 1);
        assert_eq!(IrpOp::Flush as u32, 2);
        use crate::libkern::io_op::IoOpcode;
        assert_eq!(IrpOp::Flush as u32, IoOpcode::Flush as u32);
        assert_eq!(IrpOp::Rescan as u32, IoOpcode::Rescan as u32);
        assert_eq!(IrpOp::Eject as u32, IoOpcode::Eject as u32);
        assert_eq!(IrpStatus::Success as i32, 0);
        assert_eq!(IrpStatus::Pending as i32, 1);
    }

    #[test]
    fn new_block_initialises_single_layer_pending() {
        let dev = 0xdead_beefusize as *const ();
        let po = 0xfeed_face_usize as *mut ();
        let irp = Irp::new_block(IrpOp::Read, dev, 0x1000, 0x2000, IrpBuffer::NONE, po, 7);
        assert_eq!(irp.op, IrpOp::Read as u32);
        assert_eq!(irp.status, IrpStatus::Pending as i32);
        assert_eq!(irp.stack_count, 1);
        assert_eq!(irp.stack_index, 0);
        assert_eq!(irp.offset, 0x1000);
        assert_eq!(irp.length, 0x2000);
        assert_eq!(irp.transferred, 0);
        assert_eq!(irp.completion, po as u64);
        assert_eq!(irp.initiator, 7);
        assert_eq!(irp.stack[0].device, dev as u64);
    }

    #[test]
    fn set_completion_records_status_and_count() {
        let mut irp = Irp::new_block(IrpOp::Read, core::ptr::null(), 0, 512, IrpBuffer::NONE, core::ptr::null_mut(), 0);
        irp.set_completion(IrpStatus::Success as i32, 512);
        assert_eq!(irp.status, 0);
        assert_eq!(irp.transferred, 512);
    }
}
