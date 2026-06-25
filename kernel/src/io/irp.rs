//! The `Irp` (I/O Request Packet) — the kernel-internal unit of I/O.
//!
//! Normative layout: `docs/spec/irp-layout.md`; model and rationale:
//! `docs/architecture/drivers-and-irps.md`. An `Irp` is **not** a
//! handle-accessible object (userspace never sees one), but a Tier 2 loadable
//! driver module walks `&mut Irp`, so the layout is a kernel-ABI version-hash
//! input — the field offsets below are pinned by the compile-time asserts at the
//! end of this module.
//!
//! The owning references that keep an in-flight IRP's raw pointers valid (its
//! `PendingOperation`, data buffer, and `DeviceNode`) live in a kernel-internal
//! [`IrpBox`](crate::io::block::IrpBox) wrapper, not in the hashed `Irp` itself.

use crate::dpc::Dpc;

/// `IRP_MAX_STACK` — the deepest driver stack Phase 2/3 builds (GPT-over-AHCI =
/// 2); the headroom avoids a reallocation seam (`docs/spec/irp-layout.md`).
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
    pub const NONE: IrpBuffer = IrpBuffer {
        kind: IRP_BUF_NONE,
        count: 0,
        frags: 0,
    };
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
    pub const EMPTY: IrpStackFrame = IrpStackFrame {
        device: 0,
        completion: 0,
        context: 0,
    };
}

/// `IrpOp` — the operation an IRP carries. Kept numerically aligned with
/// [`IoOpcode`](crate::libkern::io_op::IoOpcode) for a trivial translation.
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IrpOp {
    Read = 0,
    Write = 1,
}

/// `IrpStatus` — the in-flight sentinel and terminal status. A completed IRP
/// carries `Success` (0) or a negative `KError` discriminant, so `Irp::status`
/// maps directly onto `IoResult::status`. `Pending` (1) never reaches userspace.
#[repr(i32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IrpStatus {
    Success = 0,
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
    /// Inline completion DPC — queuing it allocates nothing.
    pub dpc: Dpc,
    /// The per-layer stack frames.
    pub stack: [IrpStackFrame; IRP_MAX_STACK],
}

impl Irp {
    /// Build a single-layer IRP targeting `device`, transferring `length` bytes
    /// at device `offset` against `buffer`, completing `completion` (a
    /// `*mut PendingOperation`). The inline DPC is left unarmed
    /// ([`arm_completion`](Self::arm_completion) sets it once the IRP's final
    /// address is known); `status` starts `Pending`.
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
        stack[0] = IrpStackFrame {
            device: device as u64,
            completion: 0,
            context: 0,
        };
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
            // Placeholder; replaced by `arm_completion` after final placement.
            dpc: Dpc::new(noop_dpc, core::ptr::null_mut()),
            stack,
        }
    }

    /// Record the terminal `status` and `transferred` count on a completed IRP.
    /// Called by a device backend (in DPC or completion context) before the
    /// completion DPC signals the PO.
    pub fn set_completion(&mut self, status: i32, transferred: u64) {
        self.status = status;
        self.transferred = transferred;
    }
}

/// The unarmed-DPC placeholder; never actually queued/run.
fn noop_dpc(_ctx: *mut ()) {}

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
    // `dpc` follows at 80; `stack` after the inline DPC. The exact tail offset
    // depends on `Dpc`'s size, which is kernel-internal — assert the leading
    // hashed fields (through `completion`) and the sub-type sizes.
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
        let mut irp =
            Irp::new_block(IrpOp::Read, core::ptr::null(), 0, 512, IrpBuffer::NONE, core::ptr::null_mut(), 0);
        irp.set_completion(IrpStatus::Success as i32, 512);
        assert_eq!(irp.status, 0);
        assert_eq!(irp.transferred, 512);
    }
}
