//! In-kernel **Tier 1** device drivers (compiled into the kernel ELF).
//!
//! [`probe`] matches discovered [`DeviceNode`]s against the built-in driver
//! table and brings up the ones it recognises; Phase 2 has only [`ahci`]. See
//! `docs/architecture/drivers-and-irps.md` § "Module tiers".

pub mod ahci;
pub mod console;
pub mod gpt;

use crate::arch::cpu::ArchCpu;
use crate::arch::timer::ArchTimer;
use crate::io::block::dispatch_block_irp;
use crate::libkern::handle::KObjectType;
use crate::libkern::io_op::IoOpcode;
use crate::libkern::KBox;
use crate::object::device_node::DeviceClass;
use crate::object::{DeviceNode, MemoryObject, ObjectRef, PendingOperation};

/// PCI class/subclass/prog-if for an AHCI 1.0 controller.
const PCI_CLASS_AHCI: (u8, u8, u8) = (0x01, 0x06, 0x01);

/// Match discovered devices against the Tier 1 driver table and bring up the
/// ones recognised. Boot-time; call after [`crate::device::init`] and the IRQ
/// router. Snapshots the device table, so it never holds the device lock across
/// driver allocation.
pub fn probe() {
    let devices = crate::device::snapshot();
    for node in devices.iter() {
        // SAFETY: each snapshot entry pins a live `DeviceNode`.
        let dn: &DeviceNode = unsafe { &*(node.as_ptr() as *const DeviceNode) };
        let id = &dn.descriptor().identity;
        if (id.class, id.subclass, id.prog_if) == PCI_CLASS_AHCI {
            ahci::init(node);
        }
    }

    // Controllers have published their disks; parse each disk's GPT and publish
    // its partitions. Re-snapshot *now* so the (block-class) disks are visible but
    // the partitions gpt::init creates are not re-scanned.
    let disks = crate::device::snapshot();
    for node in disks.iter() {
        // SAFETY: each entry pins a live `DeviceNode`.
        let dn: &DeviceNode = unsafe { &*(node.as_ptr() as *const DeviceNode) };
        if dn.class() == DeviceClass::Block {
            gpt::init(node);
        }
    }
}

/// Boot self-test: read sector 0 of the first block device and verify the boot
/// signature (`0x55AA` at offset 510). Proves the real driver's read path end to
/// end — `dispatch_block_irp` → controller DMA → completion. Mirrors the IOAPIC
/// PIT self-test: it briefly enables interrupts so a hardware completion IRQ can
/// fire, with a bounded polled fallback if the IRQ does not (e.g. an unrouted
/// GSI). Reports a one-line pass/fail; not a `panic!` path.
pub fn self_test() {
    let devices = crate::device::snapshot();
    let disk = devices.iter().find(|node| {
        // SAFETY: each entry pins a live `DeviceNode`.
        let dn: &DeviceNode = unsafe { &*(node.as_ptr() as *const DeviceNode) };
        dn.class() == DeviceClass::Block
    });
    let Some(disk) = disk else {
        return; // no block device (no AHCI disk) — nothing to test
    };

    let buffer = match MemoryObject::try_new(512) {
        Ok(mo) => adopt(mo, KObjectType::MemoryObject),
        Err(_) => return,
    };
    let po = match PendingOperation::try_new() {
        Ok(po) => adopt(po, KObjectType::PendingOperation),
        Err(_) => return,
    };
    let po_check = po.clone();

    if let Err(e) = dispatch_block_irp(disk, &buffer, &po, IoOpcode::Read, 0, 0, 512) {
        crate::kprintln!("ahci: read self-test FAIL (dispatch err {:?})", e);
        return;
    }

    // Brief interrupt-enabled window for the completion IRQ (only the AHCI line
    // is unmasked; the scheduler is not yet running). Fall back to polling.
    let mut via_irq = false;
    let start = crate::arch::Timer::read_ns();
    // SAFETY: ring-0; IF was 0 here and is restored to 0 below.
    unsafe { crate::arch::Cpu::interrupts_enable() };
    while crate::arch::Timer::read_ns().wrapping_sub(start) < 200_000_000 {
        if crate::sched::pending_op_is_signaled(po_check.as_ptr()) {
            via_irq = true;
            break;
        }
        core::hint::spin_loop();
    }
    // SAFETY: ring-0; restore interrupts-masked.
    let _ = unsafe { crate::arch::Cpu::interrupts_disable() };

    if !via_irq {
        ahci::poll_complete_inflight();
    }

    let (status, result) = crate::sched::pending_op_completion(po_check.as_ptr());
    if status != 0 || result != 512 {
        crate::kprintln!(
            "ahci: read self-test FAIL (status {} result {})",
            status,
            result
        );
        return;
    }

    let sig = read_boot_signature(&buffer);
    let how = if via_irq { "via IRQ" } else { "via poll fallback" };
    if sig == 0xAA55 {
        crate::kprintln!("ahci: read self-test OK (sector 0 boot sig 0x55AA, {})", how);
    } else {
        crate::kprintln!(
            "ahci: read self-test: sector 0 read OK ({}) but sig {:#06x} (no 0x55AA)",
            how,
            sig
        );
    }
}

/// Read the little-endian 16-bit value at offset 510 of `buffer`'s first frame
/// (the boot signature: `0xAA55`).
fn read_boot_signature(buffer: &ObjectRef) -> u16 {
    // SAFETY: `buffer` pins a live `MemoryObject` of ≥ 512 bytes.
    let mo: &MemoryObject = unsafe { &*(buffer.as_ptr() as *const MemoryObject) };
    let frame = mo.frames()[0];
    let va = (frame.as_u64() + crate::mm::heap::hhdm_offset()) as *const u8;
    // SAFETY: offset 510..512 is within the first (HHDM-mapped) buffer frame.
    unsafe { (va.add(510) as *const u16).read_unaligned() }
}

/// Adopt a freshly-created kernel object box into an owning [`ObjectRef`].
fn adopt<T>(obj: KBox<T>, ty: KObjectType) -> ObjectRef {
    // SAFETY: `into_raw` yields the single creation reference of a `ty` object.
    unsafe { ObjectRef::from_raw(KBox::into_raw(obj).as_ptr() as *mut (), ty) }
}
