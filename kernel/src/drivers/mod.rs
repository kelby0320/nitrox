//! In-kernel **Tier 1** device drivers (compiled into the kernel ELF): [`probe`] matches discovered
//! [`DeviceNode`]s against the built-in driver table and brings up the ones it recognises. See
//! `docs/architecture/drivers-and-irps.md` § "Module tiers".

pub mod ahci;
pub mod console;
pub mod gpt;
pub mod hid;
pub mod input;
pub mod partitions;
pub mod ps2;
pub mod xhci;

use crate::arch::cpu::ArchCpu;
use crate::arch::timer::ArchTimer;
use crate::io::block::dispatch_block_irp;
use crate::libkern::io_op::IoOpcode;
use crate::object::device_node::DeviceClass;
use crate::object::header::Ref;
use crate::object::{DeviceNode, MemoryObject, PendingOperation};

const PCI_CLASS_AHCI: (u8, u8, u8) = (0x01, 0x06, 0x01);

/// Match discovered devices against the Tier 1 driver table and bring up the ones recognised.
/// Boot-time, after [`crate::device::init`] and the IRQ router. Snapshots the device table, so it
/// never holds the device lock across driver allocation. `flags` is the command line: `usb=off`
/// leaves the USB host controller to no driver.
pub fn probe(flags: &crate::cmdline::Flags) {
    let devices = crate::device::snapshot();
    for node in devices.iter() {
        let id = &node.descriptor().identity;
        let class = (id.class, id.subclass, id.prog_if);
        if class == PCI_CLASS_AHCI {
            crate::device::record_outcome(node.descriptor(), ahci::init(node));
        } else if class == xhci::PCI_CLASS {
            crate::device::record_outcome(node.descriptor(), xhci::init(node, flags.usb_off));
        }
    }

    // Limine modules after the initramfs are disks too: publish them now, so the GPT pass below
    // scans them as it scans a SATA disk.
    crate::io::ramdisk::publish_modules();

    // Controllers have published their disks; parse each disk's table and publish its partitions.
    // Re-snapshot *now* so the disks are visible but the partitions `gpt::init` creates are not
    // re-scanned.
    let disks = crate::device::snapshot();
    for node in disks.iter() {
        if node.class() == DeviceClass::Block {
            gpt::init(node);
        }
    }

    crate::device::log_outcomes();
}

/// **Start what drivers do after boot**: the USB hub thread and AHCI's watch thread, once the
/// scheduler and APs are up.
pub fn start() {
    xhci::start();
    ahci::start();
}

/// **Wait, bounded, for what [`start`] began to settle**: the USB hub thread's first round, so
/// the report and the registry list what is attached before userspace reads them.
pub fn settle() {
    xhci::settle(xhci::FIRST_ROUND_NS);
}

/// Boot self-test: read sector 0 of the first block device and verify the boot signature (`0x55AA`
/// at offset 510), proving `dispatch_block_irp` → controller DMA → completion. Briefly enables
/// interrupts so the completion IRQ can fire, with a bounded polled fallback. Reports a one-line
/// pass/fail; not a `panic!` path.
pub fn self_test() {
    let devices = crate::device::snapshot();
    let disk = devices.iter().find(|node| node.class() == DeviceClass::Block);
    let Some(disk) = disk else {
        return; // no block device (no AHCI disk) — nothing to test
    };

    // **First, because it shares nothing with the read below but the handle**: an earlier failure
    // here must not silently skip it, or the gate that reads its absence blames the wrong thing.
    oversize_refused(disk);

    let buffer = match MemoryObject::try_new(512) {
        Ok(mo) => Ref::new(mo),
        Err(_) => return,
    };
    let po = match PendingOperation::try_new() {
        Ok(po) => Ref::new(po),
        Err(_) => return,
    };
    let po_check = po.clone();

    if let Err(e) = dispatch_block_irp(disk, &buffer, &po, IoOpcode::Read, 0, 0, 512) {
        crate::kprintln!("ahci: read self-test FAIL (dispatch err {:?})", e);
        return;
    }

    // Brief interrupt-enabled window for the completion IRQ (only the AHCI line is unmasked).
    let mut via_irq = false;
    let start = crate::arch::Timer::read_ns();
    // SAFETY: ring-0; IF was 0 here and is restored to 0 below.
    unsafe { crate::arch::Cpu::interrupts_enable() };
    while crate::arch::Timer::read_ns().wrapping_sub(start) < 200_000_000 {
        if crate::sched::pending_op_is_signaled(&po_check) {
            via_irq = true;
            break;
        }
        core::hint::spin_loop();
    }
    let _ = crate::arch::Cpu::interrupts_disable();

    if !via_irq {
        ahci::poll_complete_inflight();
    }

    let (status, result) = crate::sched::pending_op_completion(&po_check);
    if status != 0 || result != 512 {
        crate::kprintln!("ahci: read self-test FAIL (status {} result {})", status, result);
        return;
    }

    let sig = read_boot_signature(&buffer);
    let how = if via_irq { "via IRQ" } else { "via poll fallback" };
    if sig == 0xAA55 {
        crate::kprintln!("ahci: read self-test OK (sector 0 boot sig 0x55AA, {})", how);
    } else {
        crate::kprintln!("ahci: read self-test: sector 0 read OK ({}) but sig {:#06x} (no 0x55AA)", how, sig);
    }
}

/// Boot self-test: a transfer needing more fragments than the device can describe in one command is
/// **refused**, not attempted.
///
/// **The only test of that guard through the real path**: the host tests check its halves and pass
/// with the check in `dispatch_block_irp` deleted, which would let the driver write PRDT entries
/// past its one-page command table. Runs before the sector-0 read, as the two share only the disk
/// handle.
fn oversize_refused(disk: &Ref<DeviceNode>) {
    let Some(backend) = disk.block_backend() else {
        return;
    };
    // **Ask the device what it can do, then ask for one fragment more**: a driver that published
    // the wrong `max_frags` fails here. A backend with no limit (the ramdisk) has nothing to
    // exceed.
    if backend.max_frags == u32::MAX {
        return;
    }
    let pages = backend.max_frags as usize + 1;
    let bytes = pages * crate::mm::PAGE_SIZE;
    let big = match MemoryObject::try_new(bytes) {
        Ok(mo) => Ref::new(mo),
        Err(_) => return,
    };
    let po = match PendingOperation::try_new() {
        Ok(po) => Ref::new(po),
        Err(_) => return,
    };
    match dispatch_block_irp(disk, &big, &po, IoOpcode::Read, 0, 0, bytes as u64) {
        Err(crate::syscall::error::KError::InvalidArgument) => {
            crate::kprintln!("ahci: oversize self-test OK ({} frags > {} refused)", pages, backend.max_frags)
        }
        Err(e) => crate::kprintln!("ahci: oversize self-test FAIL (refused, but with {:?} not InvalidArgument)", e),
        Ok(()) => crate::kprintln!(
            "ahci: oversize self-test FAIL ({} frags accepted, table holds {})",
            pages,
            backend.max_frags
        ),
    }
}

/// Read the little-endian 16-bit boot signature (`0xAA55`) at offset 510 of `buffer`'s first frame.
fn read_boot_signature(buffer: &MemoryObject) -> u16 {
    let mut sig = [0u8; 2];
    buffer.copy_out(510, &mut sig);
    u16::from_le_bytes(sig)
}
