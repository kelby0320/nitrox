//! The kernel I/O subsystem — the asynchronous I/O spine.
//!
//! - [`irp`] — the `Irp` (I/O Request Packet) layout (`docs/spec/irp-layout.md`).
//! - [`block`] — building/dispatching a block IRP through a device's
//!   [`BlockBackend`](block::BlockBackend).
//! - [`ramdisk`] — a RAM-backed block device for bring-up.
//! - [`stats`] — what each block device has done, kept by its driver.
//!
//! The async model — `sys_io_submit` → `Irp` → device → completion IRQ → DPC →
//! `PendingOperation` → `sys_wait` — is `docs/architecture/drivers-and-irps.md`.
//! [`self_test`] proves the spine on the ramdisk.

pub mod block;
pub mod irp;
pub mod ramdisk;
pub mod stats;

use crate::dpc::Dpc;
use crate::libkern::KBox;
use crate::libkern::handle::KObjectType;
use crate::libkern::io_op::IoOpcode;
use crate::mm::heap::hhdm_offset;
use crate::object::{InterruptObject, MemoryObject, ObjectRef, PendingOperation};
use core::sync::atomic::{AtomicPtr, Ordering};

/// The [`InterruptObject`] self-test's DPC, a static as every queued DPC must outlive its drain.
static INTR_TEST_DPC: Dpc = Dpc::new(intr_test_dpc, core::ptr::null_mut());
/// The object [`INTR_TEST_DPC`] signals, set by the self-test before it queues it.
static INTR_TEST_OBJ: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// The DPC handler for the [`InterruptObject`] self-test: signal its object, exactly as a device
/// ISR's DPC would.
fn intr_test_dpc(_: *mut ()) {
    // SAFETY: the self-test set its live `InterruptObject` before queuing this, and drains it
    // before letting the object go, on the one CPU there is before the APs come up.
    unsafe { crate::sched::signal_interrupt(INTR_TEST_OBJ.load(Ordering::Acquire)) };
}

/// Boot self-test for the I/O spine. Proves, without real hardware:
///
/// 1. **The block IRP path**: a read against the ramdisk, after draining the DPC
///    queue, completes the request's `PendingOperation` (status 0, `result` =
///    bytes transferred) and fills the destination `MemoryObject`.
/// 2. **`InterruptObject` signalling from a DPC**: the object latches the
///    interrupt, and consuming it clears the latch.
///
/// Logs a one-line pass/fail per check. Call once at boot, after the allocators,
/// the HHDM, the DPC queue and the scheduler waitable machinery are up. It
/// reports and returns rather than panicking (critical-path discipline).
pub fn self_test() {
    block_irp_self_test();
    interrupt_object_self_test();
}

fn block_irp_self_test() {
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
    let device = match ramdisk::try_new_device(rd, b"io self-test ramdisk") {
        Ok(dn) => adopt(dn, KObjectType::DeviceNode),
        Err(_) => {
            crate::kprintln!("io: self-test SKIP (ramdisk device alloc failed)");
            return;
        }
    };

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

    if let Err(e) = block::dispatch_block_irp(&device, &buffer, &po, IoOpcode::Read, 0, 0, LEN as u64) {
        crate::kprintln!("io: self-test FAIL (dispatch err {:?})", e);
        return;
    }

    // The ramdisk queued the completion DPC; drain it in place of the
    // interrupt-dispatch tail.
    crate::dpc::run_pending();

    // SAFETY: `po_check` is a live `PendingOperation` reference held across the call.
    let (status, result) = unsafe { crate::sched::pending_op_completion(po_check.as_ptr()) };
    if status != 0 || result != LEN as u64 {
        crate::kprintln!("io: block-read self-test FAIL (status {} result {})", status, result);
        return;
    }

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
    INTR_TEST_OBJ.store(irq.as_ptr(), Ordering::Release);
    crate::dpc::enqueue(&INTR_TEST_DPC);
    crate::dpc::run_pending();

    // SAFETY: `irq` is a live `InterruptObject` reference held across the call.
    if !unsafe { crate::sched::interrupt_pending(irq.as_ptr()) } {
        crate::kprintln!("io: interrupt self-test FAIL (not latched after DPC)");
        return;
    }
    // SAFETY: `irq` is a live `InterruptObject` reference held across the call.
    unsafe { crate::sched::interrupt_consume(irq.as_ptr()) };
    // SAFETY: `irq` is a live `InterruptObject` reference held across the call.
    if unsafe { crate::sched::interrupt_pending(irq.as_ptr()) } {
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
