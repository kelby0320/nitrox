//! The kernel I/O subsystem — the asynchronous I/O spine.
//!
//! - [`irp`] — the `Irp` (I/O Request Packet) layout (`docs/spec/irp-layout.md`).
//! - [`block`] — building/dispatching a block IRP through a device's
//!   [`BlockBackend`](block::BlockBackend).
//! - [`ramdisk`] — a RAM-backed block device for bring-up.
//!
//! The async model — `sys_io_submit` → `Irp` → device → completion IRQ → DPC →
//! `PendingOperation` → `sys_wait` — is `docs/architecture/drivers-and-irps.md`.
//! Part 2 builds the spine and proves it on the ramdisk via [`self_test`]; the
//! AHCI driver (Part 3) and the `/dev/blk` resource server (Part 4) follow.

pub mod block;
pub mod irp;
pub mod ramdisk;

use crate::dpc::Dpc;
use crate::libkern::handle::KObjectType;
use crate::libkern::io_op::IoOpcode;
use crate::libkern::KBox;
use crate::mm::heap::hhdm_offset;
use crate::object::{InterruptObject, MemoryObject, ObjectRef, PendingOperation};

/// The DPC handler for the [`InterruptObject`] self-test: signal the object whose
/// pointer is its `ctx`, exactly as a device ISR's completion DPC would.
fn intr_test_dpc(ctx: *mut ()) {
    crate::sched::signal_interrupt(ctx);
}

/// Boot self-test for the I/O spine (Phase 2 slice 5, Part 2). Proves, without
/// real hardware:
///
/// 1. **The block IRP path** — submit a read against the ramdisk, drain the DPC
///    queue, and confirm the request's `PendingOperation` completed (status 0,
///    `result` = bytes transferred) and the destination `MemoryObject` holds the
///    ramdisk's data.
/// 2. **`InterruptObject` signalling from a DPC** — queue a DPC that signals an
///    `InterruptObject`, drain it, and confirm the object latched the interrupt
///    (and that consuming it clears the latch).
///
/// Logs a one-line pass/fail per check. Call once at boot, after the allocators,
/// the HHDM, the DPC queue, and the scheduler waitable machinery are up. Not a
/// `panic!` path — it reports and returns (critical-path discipline).
pub fn self_test() {
    block_irp_self_test();
    interrupt_object_self_test();
}

fn block_irp_self_test() {
    // A leaked 'static ramdisk (a device persists for the kernel's lifetime).
    let rd = match ramdisk::RamDisk::try_new() {
        Ok(rd) => {
            // SAFETY: leak the box to obtain a 'static reference for the device.
            unsafe { &*(KBox::into_raw(rd).as_ptr()) }
        }
        Err(_) => {
            crate::kprintln!("io: self-test SKIP (ramdisk alloc failed)");
            return;
        }
    };
    let device = match ramdisk::try_new_device(rd) {
        Ok(dn) => adopt(dn, KObjectType::DeviceNode),
        Err(_) => {
            crate::kprintln!("io: self-test SKIP (ramdisk device alloc failed)");
            return;
        }
    };

    // An 8 KiB destination buffer.
    const LEN: usize = 2 * crate::mm::PAGE_SIZE;
    let buffer = match MemoryObject::try_new(LEN) {
        Ok(mo) => adopt(mo, KObjectType::MemoryObject),
        Err(_) => {
            crate::kprintln!("io: self-test SKIP (buffer alloc failed)");
            return;
        }
    };
    let po = match PendingOperation::try_new() {
        Ok(po) => adopt(po, KObjectType::PendingOperation),
        Err(_) => {
            crate::kprintln!("io: self-test SKIP (PO alloc failed)");
            return;
        }
    };
    let po_check = po.clone();

    // Submit a read of the first `LEN` bytes into the buffer.
    if let Err(e) =
        block::dispatch_block_irp(&device, &buffer, &po, IoOpcode::Read, 0, 0, LEN as u64)
    {
        crate::kprintln!("io: self-test FAIL (dispatch err {:?})", e);
        return;
    }

    // The ramdisk queued the completion DPC; drain it (stands in for the
    // interrupt-dispatch tail).
    crate::dpc::run_pending();

    let (status, result) = crate::sched::pending_op_completion(po_check.as_ptr());
    if status != 0 || result != LEN as u64 {
        crate::kprintln!(
            "io: block-read self-test FAIL (status {} result {})",
            status,
            result
        );
        return;
    }

    // Verify the buffer received the ramdisk's pattern.
    if buffer_matches_pattern(&buffer, LEN) {
        crate::kprintln!("io: block-read self-test OK ({} bytes via IRP+DPC+PO)", LEN);
    } else {
        crate::kprintln!("io: block-read self-test FAIL (buffer content mismatch)");
    }
}

/// `true` iff the first `len` bytes of `buffer`'s frames equal the ramdisk
/// pattern (`RamDisk::pattern_byte`).
fn buffer_matches_pattern(buffer: &ObjectRef, len: usize) -> bool {
    // SAFETY: `buffer` pins a live `MemoryObject`.
    let mo: &MemoryObject = unsafe { &*(buffer.as_ptr() as *const MemoryObject) };
    let hhdm = hhdm_offset();
    let mut checked = 0usize;
    for &frame in mo.frames() {
        let va = (frame.as_u64() + hhdm) as *const u8;
        let n = crate::mm::PAGE_SIZE.min(len - checked);
        for i in 0..n {
            // SAFETY: `va + i` is within an owned, HHDM-mapped buffer frame.
            let got = unsafe { va.add(i).read() };
            if got != ramdisk::RamDisk::pattern_byte(checked + i) {
                return false;
            }
        }
        checked += n;
        if checked >= len {
            break;
        }
    }
    checked == len
}

fn interrupt_object_self_test() {
    let irq = match InterruptObject::try_new() {
        Ok(io) => adopt(io, KObjectType::InterruptObject),
        Err(_) => {
            crate::kprintln!("io: interrupt self-test SKIP (alloc failed)");
            return;
        }
    };
    // A DPC that signals the object, as a device ISR's completion DPC would.
    let dpc = Dpc::new(intr_test_dpc, irq.as_ptr());
    crate::dpc::enqueue(&dpc);
    crate::dpc::run_pending();

    if !crate::sched::interrupt_pending(irq.as_ptr()) {
        crate::kprintln!("io: interrupt self-test FAIL (not latched after DPC)");
        return;
    }
    crate::sched::interrupt_consume(irq.as_ptr());
    if crate::sched::interrupt_pending(irq.as_ptr()) {
        crate::kprintln!("io: interrupt self-test FAIL (still latched after consume)");
        return;
    }
    crate::kprintln!("io: interrupt self-test OK (DPC signal -> latch -> consume)");
}

/// Adopt a freshly-created kernel object box into an owning [`ObjectRef`].
fn adopt<T>(obj: KBox<T>, ty: KObjectType) -> ObjectRef {
    // SAFETY: `into_raw` yields the single creation reference of a `ty` object.
    unsafe { ObjectRef::from_raw(KBox::into_raw(obj).as_ptr() as *mut (), ty) }
}
