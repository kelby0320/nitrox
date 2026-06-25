//! A RAM-backed block device — bring-up scaffolding for the I/O spine.
//!
//! The ramdisk is the first [`BlockBackend`] producer: it proves
//! initiate → DPC → `PendingOperation` → `sys_wait` end-to-end without real
//! hardware or interrupts (Part 2). Its `submit` performs the transfer (a
//! `memcpy` against its backing) and queues the IRP's completion DPC, standing in
//! for a controller's DMA + completion ISR. AHCI (Part 3) plugs a real driver
//! into the same [`BlockBackend`] seam.

use crate::io::block::BlockBackend;
use crate::io::irp::{Irp, IrpOp, IrpStatus, PhysFrag};
use crate::libkern::{AllocError, KBox, KVec};
use crate::object::device_node::{
    BarWindow, BlockGeometry, DeviceIdentity, DeviceNode, InterruptSpec, ResourceDescriptor,
};
use crate::syscall::error::KError;

/// Backing size of the bring-up ramdisk (64 KiB).
pub const RAMDISK_BYTES: usize = 64 * 1024;
/// Logical block size the ramdisk reports.
pub const RAMDISK_BLOCK: u32 = 512;

/// A RAM-backed block device. Owns its backing as a heap [`KVec`] (the device
/// lives for the kernel's lifetime, like real hardware).
pub struct RamDisk {
    backing: KVec<u8>,
    block_size: u32,
}

// SAFETY: the backing is owned by this device and accessed only on the single
// CPU that services it; no aliasing across threads.
unsafe impl Send for RamDisk {}
unsafe impl Sync for RamDisk {}

impl RamDisk {
    /// The deterministic backing byte at offset `i` — the pattern a read should
    /// return, so a test can predict it.
    pub fn pattern_byte(i: usize) -> u8 {
        (i as u8).wrapping_mul(31).wrapping_add(7)
    }

    /// Allocate a ramdisk with its backing filled with [`pattern_byte`]. Built
    /// by appending into a [`KVec`] (never a large stack temporary, which would
    /// overflow the kernel stack).
    ///
    /// [`pattern_byte`]: RamDisk::pattern_byte
    pub fn try_new() -> Result<KBox<Self>, AllocError> {
        let mut backing: KVec<u8> = KVec::new();
        backing.try_reserve(RAMDISK_BYTES)?;
        for i in 0..RAMDISK_BYTES {
            backing
                .try_push(Self::pattern_byte(i))
                .expect("within reserved ramdisk capacity");
        }
        KBox::try_new(RamDisk {
            backing,
            block_size: RAMDISK_BLOCK,
        })
    }

    /// A raw pointer to the backing (the device owns it; single-CPU access).
    fn base(&self) -> *mut u8 {
        self.backing.as_ptr() as *mut u8
    }

    /// Capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.backing.len()
    }

    /// Logical block size.
    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Perform the IRP's transfer against the backing, returning
    /// `(status, transferred)`. A `memcpy` either direction across the buffer's
    /// physical fragments (reached through the HHDM). No blocking, no allocation.
    fn transfer(&self, irp: &Irp) -> (i32, u64) {
        let dev_off = irp.offset;
        let len = irp.length;
        if dev_off
            .checked_add(len)
            .map_or(true, |end| end > self.capacity() as u64)
        {
            return (KError::InvalidArgument as i32, 0);
        }
        let base = self.base();
        let is_read = irp.op == IrpOp::Read as u32;
        // SAFETY: `irp.buffer.frags` points at a `[PhysFrag; count]` owned by the
        // IRP's box for the IRP's lifetime (see `io::block`).
        let frags = unsafe {
            core::slice::from_raw_parts(
                irp.buffer.frags as *const PhysFrag,
                irp.buffer.count as usize,
            )
        };
        let hhdm = crate::mm::heap::hhdm_offset();
        let mut dev_pos = dev_off;
        for f in frags {
            let buf_va = (f.base + hhdm) as *mut u8;
            // SAFETY: `dev_pos + f.len <= len` (the total was bounds-checked and
            // the frags sum to `len`); `buf_va` is the HHDM alias of an owned
            // buffer frame; the regions do not overlap.
            unsafe {
                let dev_ptr = base.add(dev_pos as usize);
                if is_read {
                    core::ptr::copy_nonoverlapping(dev_ptr, buf_va, f.len as usize);
                } else {
                    core::ptr::copy_nonoverlapping(buf_va as *const u8, dev_ptr, f.len as usize);
                }
            }
            dev_pos += f.len;
        }
        (IrpStatus::Success as i32, len)
    }
}

/// The ramdisk's [`BlockBackend::submit`]: transfer now (the ramdisk has no DMA
/// engine), then queue the IRP's completion DPC — exercising the IRQ→DPC→thread
/// completion path that real hardware drives from its ISR.
fn ramdisk_submit(irp: *mut Irp, ctx: *mut ()) {
    // SAFETY: `ctx` is the `*const RamDisk` installed in the backend; the device
    // outlives the IRP.
    let rd = unsafe { &*(ctx as *const RamDisk) };
    // SAFETY: `irp` is the live in-flight IRP from `dispatch_block_irp`.
    let (status, transferred) = rd.transfer(unsafe { &*irp });
    // SAFETY: as above; record the outcome before queuing completion.
    unsafe { (*irp).set_completion(status, transferred) };
    // SAFETY: the inline DPC was armed in `dispatch_block_irp`; the IRP outlives
    // the drain (it is reclaimed by the completion handler).
    crate::dpc::enqueue(unsafe { &(*irp).dpc });
}

/// Build a block [`DeviceNode`] for `rd`. `rd` must outlive every IRP submitted
/// to it (it does — the ramdisk is leaked/'static).
pub fn try_new_device(rd: &'static RamDisk) -> Result<KBox<DeviceNode>, AllocError> {
    let backend = BlockBackend {
        submit: ramdisk_submit,
        ctx: rd as *const RamDisk as *mut (),
    };
    let geometry = BlockGeometry {
        logical_block_size: rd.block_size(),
        block_count: (rd.capacity() / rd.block_size() as usize) as u64,
    };
    let descriptor = ResourceDescriptor {
        identity: DeviceIdentity {
            vendor: 0,
            device: 0,
            class: 0,
            subclass: 0,
            prog_if: 0,
            revision: 0,
        },
        bars: [BarWindow::ZERO; 6],
        interrupt: InterruptSpec::NONE,
        seg: 0,
        bus: 0,
        dev: 0,
        func: 0,
        _pad: [0; 3],
    };
    DeviceNode::try_new_block(descriptor, geometry, backend)
}
