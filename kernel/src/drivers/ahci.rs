//! AHCI (SATA) Tier 1 block driver.
//!
//! Brings up the HBA found by PCI enumeration (a class-`01.06.01` function),
//! maps its ABAR uncached, enumerates ports, runs `IDENTIFY DEVICE` on the first
//! SATA disk, and publishes it as a block [`DeviceNode`] backed by a
//! [`BlockBackend`]. Reads flow through the Part 2 I/O spine: `dispatch_block_irp`
//! → [`submit`] issues a `READ DMA EXT` against the IRP's buffer fragments
//! (the controller DMAs straight into the client's `MemoryObject` frames) →
//! the completion IRQ → [`isr`] → the IRP's completion DPC → its
//! `PendingOperation`.
//!
//! Phase 2 scope: a **single controller, single SATA disk**, IOAPIC-routed INTx,
//! one outstanding command (slot 0). Multi-port/multi-controller, NCQ, port
//! multipliers, and MSI are deferred (`docs/rationale/deferred-decisions.md`).
//! See `docs/architecture/drivers-and-irps.md`.

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::dpc::Dpc;
use crate::io::block::BlockBackend;
use crate::io::irp::{Irp, IrpStatus, PhysFrag};
use crate::libkern::handle::KObjectType;
use crate::libkern::KBox;
use crate::mm::dma::DmaBuffer;
use crate::mm::{PhysAddr, kvmap};
use crate::arch::timer::ArchTimer;
use crate::object::device_node::{
    BarWindow, BlockGeometry, DeviceIdentity, DeviceNode, InterruptSpec, ResourceDescriptor,
};
use crate::object::{InterruptObject, ObjectRef};

// --- HBA / port register offsets (AHCI 1.3) ---------------------------------

const HBA_CAP: u64 = 0x00;
const HBA_GHC: u64 = 0x04;
const HBA_IS: u64 = 0x08;
const HBA_PI: u64 = 0x0C;
const GHC_AE: u32 = 1 << 31; // AHCI enable
const GHC_IE: u32 = 1 << 1; // global interrupt enable

const PORT_BASE: u64 = 0x100; // first port's registers
const PORT_STRIDE: u64 = 0x80;

const PX_CLB: u64 = 0x00;
const PX_CLBU: u64 = 0x04;
const PX_FB: u64 = 0x08;
const PX_FBU: u64 = 0x0C;
const PX_IS: u64 = 0x10;
const PX_IE: u64 = 0x14;
const PX_CMD: u64 = 0x18;
const PX_TFD: u64 = 0x20;
const PX_SIG: u64 = 0x24;
const PX_SSTS: u64 = 0x28;
const PX_SERR: u64 = 0x30;
const PX_CI: u64 = 0x38;

const CMD_ST: u32 = 1 << 0; // start
const CMD_FRE: u32 = 1 << 4; // FIS receive enable
const CMD_FR: u32 = 1 << 14; // FIS receive running
const CMD_CR: u32 = 1 << 15; // command list running

const TFD_BSY: u32 = 1 << 7;
const TFD_DRQ: u32 = 1 << 3;
const TFD_ERR: u32 = 1 << 0;

const SIG_SATA: u32 = 0x0000_0101; // a plain SATA disk
const SSTS_DET_PRESENT: u32 = 0x3; // device present + PHY communication

const PXIE_DHRE: u32 = 1 << 0; // D2H register FIS interrupt
const PXIE_TFEE: u32 = 1 << 30; // task file error

// ATA commands.
const ATA_IDENTIFY: u8 = 0xEC;
const ATA_READ_DMA_EXT: u8 = 0x25;

const SECTOR_SIZE: u32 = 512;
/// Bounded poll for command completion / port readiness (~1 s of monotonic time).
const POLL_TIMEOUT_NS: u64 = 1_000_000_000;

// --- Driver state -----------------------------------------------------------

/// State for the single supported AHCI disk. Leaked to `'static` at bring-up (the
/// hardware lives for the kernel's lifetime); the ISR reaches it through
/// [`AHCI`].
pub struct AhciDisk {
    abar: u64,      // mapped ABAR kernel-virtual base
    port_base: u64, // abar + 0x100 + port*0x80
    port: u32,
    _cmd_list: DmaBuffer, // 1 KiB command list (kept mapped/owned)
    _fis: DmaBuffer,      // 256 B received-FIS area
    cmd_table: DmaBuffer, // CFIS + PRDT for slot 0
    sectors: u64,
    /// The IRP currently issued on slot 0 (`null` when idle), for the ISR to
    /// complete. One outstanding command in Phase 2.
    inflight: AtomicPtr<Irp>,
    /// The controller's IRQ object, signalled by the ISR (exercises the
    /// signal-from-real-ISR path; no waiter in Phase 2).
    intr: *mut (),
}

// SAFETY: the disk state is set up once at boot and thereafter accessed only on
// the single CPU that services the controller (the backend and the ISR);
// `inflight` is the only mutable cross-context field and is atomic.
unsafe impl Send for AhciDisk {}
unsafe impl Sync for AhciDisk {}

/// The active disk (one in Phase 2). Set at bring-up; read by [`isr`] / [`submit`].
static AHCI: AtomicPtr<AhciDisk> = AtomicPtr::new(core::ptr::null_mut());

/// DPC that signals the controller's `InterruptObject` (queued by the ISR).
static AHCI_INTR_DPC: Dpc = Dpc::new(ahci_intr_dpc, core::ptr::null_mut());

// --- MMIO helpers -----------------------------------------------------------

#[inline]
fn read32(base: u64, off: u64) -> u32 {
    // SAFETY: `base + off` is within the mapped uncached ABAR window.
    unsafe { core::ptr::read_volatile((base + off) as *const u32) }
}

#[inline]
fn write32(base: u64, off: u64, val: u32) {
    // SAFETY: as `read32`; AHCI registers are 32-bit MMIO.
    unsafe { core::ptr::write_volatile((base + off) as *mut u32, val) };
}

// --- Bring-up ---------------------------------------------------------------

/// Probe and initialise an AHCI controller `DeviceNode`. On success, publishes
/// the first SATA disk found as a block `DeviceNode` (registered in the device
/// table) and installs the completion ISR. Logs progress; returns whether a disk
/// was published.
pub fn init(controller: &ObjectRef) -> bool {
    // SAFETY: `controller` pins a live `DeviceNode`.
    let dn: &DeviceNode = unsafe { &*(controller.as_ptr() as *const DeviceNode) };
    let desc = dn.descriptor();

    // BAR5 is the ABAR (AHCI 1.3). Map it uncached.
    let bar = desc.bars[5];
    if bar.size == 0 {
        crate::kprintln!("ahci: controller has no ABAR (BAR5)");
        return false;
    }
    let pages = bar.size.div_ceil(crate::mm::PAGE_SIZE as u64).max(1);
    // SAFETY: `bar.base` is the controller's MMIO ABAR from PCI BAR sizing.
    let abar = match unsafe { kvmap::map_mmio(PhysAddr(bar.base), pages) } {
        Ok(va) => va.as_u64() + (bar.base & (crate::mm::PAGE_SIZE as u64 - 1)),
        Err(_) => {
            crate::kprintln!("ahci: ABAR map failed");
            return false;
        }
    };

    // Enable AHCI mode, then find an implemented port with a SATA disk.
    write32(abar, HBA_GHC, read32(abar, HBA_GHC) | GHC_AE);
    let pi = read32(abar, HBA_PI);
    let cap = read32(abar, HBA_CAP);
    crate::kprintln!("ahci: HBA up (CAP {:#010x}, PI {:#010x})", cap, pi);

    let mut port = u32::MAX;
    for p in 0..32u32 {
        if pi & (1 << p) == 0 {
            continue;
        }
        let pb = abar + PORT_BASE + p as u64 * PORT_STRIDE;
        if read32(pb, PX_SSTS) & 0xF == SSTS_DET_PRESENT && read32(pb, PX_SIG) == SIG_SATA {
            port = p;
            break;
        }
    }
    if port == u32::MAX {
        crate::kprintln!("ahci: no SATA disk on any implemented port");
        return false;
    }
    let port_base = abar + PORT_BASE + port as u64 * PORT_STRIDE;

    // Allocate the per-port DMA structures (zeroed, contiguous, page-aligned).
    let cmd_list = match DmaBuffer::alloc(1024) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let fis = match DmaBuffer::alloc(256) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let cmd_table = match DmaBuffer::alloc(crate::mm::PAGE_SIZE) {
        Ok(b) => b,
        Err(_) => return false,
    };

    // Stop the port, point it at our structures, clear errors, restart.
    stop_port(port_base);
    write32(port_base, PX_CLB, cmd_list.phys().as_u64() as u32);
    write32(port_base, PX_CLBU, (cmd_list.phys().as_u64() >> 32) as u32);
    write32(port_base, PX_FB, fis.phys().as_u64() as u32);
    write32(port_base, PX_FBU, (fis.phys().as_u64() >> 32) as u32);
    write32(port_base, PX_SERR, 0xFFFF_FFFF); // clear errors (write-1-to-clear)
    write32(port_base, PX_IS, 0xFFFF_FFFF);
    write32(port_base, PX_IE, PXIE_DHRE | PXIE_TFEE);
    start_port(port_base);

    let intr = match InterruptObject::try_new() {
        Ok(io) => unsafe {
            ObjectRef::from_raw(
                KBox::into_raw(io).as_ptr() as *mut (),
                KObjectType::InterruptObject,
            )
        },
        Err(_) => return false,
    };
    let intr_ptr = intr.as_ptr();
    // Leak the IRQ object: the controller holds it for the kernel's lifetime.
    core::mem::forget(intr);

    let disk = AhciDisk {
        abar,
        port_base,
        port,
        _cmd_list: cmd_list,
        _fis: fis,
        cmd_table,
        sectors: 0,
        inflight: AtomicPtr::new(core::ptr::null_mut()),
        intr: intr_ptr,
    };
    let disk = match KBox::try_new(disk) {
        Ok(b) => KBox::into_raw(b).as_ptr(), // leak to 'static
        Err(_) => return false,
    };
    AHCI.store(disk, Ordering::Release);

    // IDENTIFY the disk (polled — bring-up runs with interrupts masked).
    // SAFETY: `disk` is the just-published live state.
    let sectors = match unsafe { identify(&mut *disk) } {
        Some(s) => s,
        None => {
            crate::kprintln!("ahci: IDENTIFY failed on port {}", port);
            return false;
        }
    };
    // SAFETY: exclusive at bring-up (no IRQ, no other CPU).
    unsafe { (*disk).sectors = sectors };
    crate::kprintln!(
        "ahci: port {} disk ready ({} sectors, {} MiB)",
        port,
        sectors,
        sectors * SECTOR_SIZE as u64 / (1024 * 1024)
    );

    // Route the controller's INTx and install the completion ISR. The GSI comes
    // from the PCI interrupt line register (firmware-programmed on QEMU; ACPI
    // _PRT routing is deferred).
    let gsi = desc.interrupt.line as u32;
    // SAFETY: ring-0, post-IrqRouter::init; `isr` stays valid for the kernel's
    // lifetime.
    let vec = unsafe { crate::arch::install_pci_irq(gsi, isr) };
    // Enable HBA-level interrupt delivery.
    write32(abar, HBA_GHC, read32(abar, HBA_GHC) | GHC_IE);
    write32(abar, HBA_IS, read32(abar, HBA_IS)); // clear stale
    crate::kprintln!("ahci: INTx GSI{} -> vec {:#x}", gsi, vec);

    publish_disk(controller, sectors, disk)
}

/// Stop the port: clear ST then FRE, waiting for CR/FR to clear.
fn stop_port(pb: u64) {
    let mut cmd = read32(pb, PX_CMD);
    cmd &= !CMD_ST;
    write32(pb, PX_CMD, cmd);
    wait_clear(pb, PX_CMD, CMD_CR);
    cmd = read32(pb, PX_CMD);
    cmd &= !CMD_FRE;
    write32(pb, PX_CMD, cmd);
    wait_clear(pb, PX_CMD, CMD_FR);
}

/// Start the port: set FRE then ST (FRE must precede ST).
fn start_port(pb: u64) {
    wait_clear(pb, PX_TFD, TFD_BSY | TFD_DRQ);
    let mut cmd = read32(pb, PX_CMD);
    cmd |= CMD_FRE;
    write32(pb, PX_CMD, cmd);
    cmd |= CMD_ST;
    write32(pb, PX_CMD, cmd);
}

/// Spin until `mask` bits at `pb+off` are clear, or the poll timeout elapses.
fn wait_clear(pb: u64, off: u64, mask: u32) {
    let start = crate::arch::Timer::read_ns();
    while read32(pb, off) & mask != 0 {
        if crate::arch::Timer::read_ns().wrapping_sub(start) > POLL_TIMEOUT_NS {
            return;
        }
        core::hint::spin_loop();
    }
}

/// Run `IDENTIFY DEVICE` (polled) and return the LBA48 sector count.
///
/// # Safety
/// `disk` is the live, brought-up disk state; called at bring-up with no IRQ.
unsafe fn identify(disk: &mut AhciDisk) -> Option<u64> {
    let data = DmaBuffer::alloc(SECTOR_SIZE as usize).ok()?;
    let frags = [PhysFrag {
        base: data.phys().as_u64(),
        len: SECTOR_SIZE as u64,
    }];
    build_command(disk, ATA_IDENTIFY, 0, 0, &frags, false);
    issue(disk);
    if !wait_command_polled(disk) {
        return None;
    }
    // IDENTIFY words 100..104 hold the 48-bit max-LBA sector count.
    let words = data.virt() as *const u16;
    // SAFETY: `data` holds the 512-byte IDENTIFY result; word 100..=103 in range.
    let n = unsafe {
        (words.add(100).read() as u64)
            | (words.add(101).read() as u64) << 16
            | (words.add(102).read() as u64) << 32
            | (words.add(103).read() as u64) << 48
    };
    // `data` drops here (IDENTIFY is one-shot); the command is complete.
    if n == 0 { None } else { Some(n) }
}

/// Fill slot 0's command header + table for an ATA command transferring the
/// `frags` region. `write` selects the transfer direction.
fn build_command(disk: &AhciDisk, command: u8, lba: u64, count: u16, frags: &[PhysFrag], write: bool) {
    let ct = disk.cmd_table.virt();
    // Zero the command FIS + PRDT region we touch.
    // SAFETY: `ct` is our owned page-sized command-table buffer.
    unsafe { core::ptr::write_bytes(ct, 0, 128 + frags.len() * 16) };

    // Command FIS — H2D Register FIS (type 0x27), command set.
    // SAFETY: `ct` addresses the 64-byte CFIS area.
    unsafe {
        ct.add(0).write(0x27);
        ct.add(1).write(0x80); // C=1 (command)
        ct.add(2).write(command);
        ct.add(4).write(lba as u8);
        ct.add(5).write((lba >> 8) as u8);
        ct.add(6).write((lba >> 16) as u8);
        ct.add(7).write(0x40); // device: LBA mode
        ct.add(8).write((lba >> 24) as u8);
        ct.add(9).write((lba >> 32) as u8);
        ct.add(10).write((lba >> 40) as u8);
        ct.add(12).write(count as u8);
        ct.add(13).write((count >> 8) as u8);
    }

    // PRDT entries (16 bytes each) at offset 128.
    for (i, f) in frags.iter().enumerate() {
        let e = (ct as u64 + 128 + (i as u64) * 16) as *mut u32;
        // SAFETY: `e` is within the owned command-table page (frags bounded).
        unsafe {
            e.add(0).write(f.base as u32);
            e.add(1).write((f.base >> 32) as u32);
            e.add(2).write(0);
            // DBC = byte count - 1 (bits 21:0); bit 31 = interrupt on completion.
            e.add(3).write(((f.len as u32 - 1) & 0x003F_FFFF) | (1 << 31));
        }
    }

    // Command header (slot 0) at the start of the command list.
    let cl = disk._cmd_list.virt() as *mut u32;
    // DW0: CFL = 5 dwords (H2D FIS), W bit, PRDTL = frag count.
    let dw0 = 5u32 | ((write as u32) << 6) | ((frags.len() as u32) << 16);
    // SAFETY: `cl` addresses the 1 KiB command list; slot 0 is its first 32 bytes.
    unsafe {
        cl.add(0).write(dw0);
        cl.add(1).write(0); // PRDBC
        cl.add(2).write(disk.cmd_table.phys().as_u64() as u32);
        cl.add(3).write((disk.cmd_table.phys().as_u64() >> 32) as u32);
    }
}

/// Issue slot 0 (set PxCI bit 0).
fn issue(disk: &AhciDisk) {
    write32(disk.port_base, PX_CI, 1);
}

/// Poll slot 0 to completion; `true` on success, `false` on timeout/error.
fn wait_command_polled(disk: &AhciDisk) -> bool {
    let start = crate::arch::Timer::read_ns();
    loop {
        let ci = read32(disk.port_base, PX_CI);
        if ci & 1 == 0 {
            break;
        }
        if read32(disk.port_base, PX_TFD) & TFD_ERR != 0 {
            return false;
        }
        if crate::arch::Timer::read_ns().wrapping_sub(start) > POLL_TIMEOUT_NS {
            return false;
        }
        core::hint::spin_loop();
    }
    read32(disk.port_base, PX_TFD) & (TFD_ERR | TFD_BSY | TFD_DRQ) == 0
}

/// Publish the disk as a block `DeviceNode` in the device table.
fn publish_disk(controller: &ObjectRef, sectors: u64, disk: *mut AhciDisk) -> bool {
    // SAFETY: `controller` pins a live `DeviceNode`.
    let cdesc = unsafe { &*(controller.as_ptr() as *const DeviceNode) }.descriptor();
    let backend = BlockBackend {
        submit,
        poll: ahci_poll,
        ctx: disk as *mut (),
    };
    let geometry = BlockGeometry {
        logical_block_size: SECTOR_SIZE,
        block_count: sectors,
    };
    let descriptor = ResourceDescriptor {
        identity: DeviceIdentity {
            vendor: cdesc.identity.vendor,
            device: cdesc.identity.device,
            class: 0x01,
            subclass: 0x06,
            prog_if: 0x01,
            revision: cdesc.identity.revision,
        },
        bars: [BarWindow::ZERO; 6],
        interrupt: InterruptSpec::NONE,
        seg: cdesc.seg,
        bus: cdesc.bus,
        dev: cdesc.dev,
        func: cdesc.func,
        _pad: [0; 3],
    };
    match DeviceNode::try_new_block(descriptor, geometry, backend) {
        Ok(node) => {
            // SAFETY: adopt the creation reference into the device table.
            let r = unsafe {
                ObjectRef::from_raw(
                    KBox::into_raw(node).as_ptr() as *mut (),
                    KObjectType::DeviceNode,
                )
            };
            crate::device::register(r);
            true
        }
        Err(_) => false,
    }
}

// --- The block backend: issue a read/write IRP ------------------------------

/// [`BlockBackend::submit`] for an AHCI disk. Builds a `READ DMA EXT` (or write)
/// against the IRP's buffer fragments and issues it; completion arrives via the
/// IRQ (or the boot self-test's polled fallback). `ctx` is the `*mut AhciDisk`.
fn submit(irp: *mut Irp, ctx: *mut ()) {
    let disk = ctx as *mut AhciDisk;
    // SAFETY: `disk` is the live published disk; `irp` is the in-flight request.
    let (op, offset, length) = unsafe { ((*irp).op, (*irp).offset, (*irp).length) };
    // SAFETY: `irp.buffer.frags` is a `[PhysFrag; count]` owned by the IRP box.
    let frags = unsafe {
        core::slice::from_raw_parts(
            (*irp).buffer.frags as *const PhysFrag,
            (*irp).buffer.count as usize,
        )
    };

    let lba = offset / SECTOR_SIZE as u64;
    let count = (length / SECTOR_SIZE as u64) as u16;
    let is_write = op == crate::io::irp::IrpOp::Write as u32;
    let command = if is_write {
        ATA_READ_DMA_EXT.wrapping_add(0x10) // WRITE DMA EXT = 0x35
    } else {
        ATA_READ_DMA_EXT
    };

    // SAFETY: single outstanding command in Phase 2; record it for the ISR.
    unsafe {
        (*disk).inflight.store(irp, Ordering::Release);
        build_command(&*disk, command, lba, count, frags, is_write);
        issue(&*disk);
    }
}

// --- Interrupt + completion -------------------------------------------------

/// The AHCI completion ISR (device-IRQ context). Acknowledges the controller,
/// completes the in-flight IRP via its DPC, and queues the InterruptObject
/// signal. The dispatcher EOIs after this returns.
extern "C" fn isr() {
    let disk = AHCI.load(Ordering::Acquire);
    if disk.is_null() {
        return;
    }
    // SAFETY: `disk` is the live published state; ISR runs single-CPU, IF=0.
    let d = unsafe { &*disk };

    // Acknowledge: clear the port's interrupt status, then the HBA's.
    let pxis = read32(d.port_base, PX_IS);
    write32(d.port_base, PX_IS, pxis);
    write32(d.abar, HBA_IS, 1 << d.port);

    // If slot 0's command has retired, complete the in-flight IRP.
    if read32(d.port_base, PX_CI) & 1 == 0 {
        let irp = d.inflight.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if !irp.is_null() {
            let err = read32(d.port_base, PX_TFD) & TFD_ERR != 0;
            // SAFETY: `irp` is the in-flight request; set status + queue its DPC.
            unsafe {
                let status = if err {
                    crate::syscall::error::KError::IoError as i32
                } else {
                    IrpStatus::Success as i32
                };
                let transferred = if err { 0 } else { (*irp).length };
                (*irp).set_completion(status, transferred);
                crate::dpc::enqueue(&(*irp).dpc);
            }
        }
    }
    // Exercise the signal-from-real-ISR path (no waiter in Phase 2).
    crate::dpc::enqueue(&AHCI_INTR_DPC);
}

/// [`BlockBackend::poll`] for the AHCI disk: drive the single in-flight command
/// to completion by polling and run its DPC (the `read_blocking` boot path; `ctx`
/// is unused — Phase 2 has one disk, tracked in [`AHCI`]).
fn ahci_poll(_ctx: *mut ()) {
    poll_complete_inflight();
}

/// DPC queued by [`isr`]: signal the controller's `InterruptObject`.
fn ahci_intr_dpc(_ctx: *mut ()) {
    let disk = AHCI.load(Ordering::Acquire);
    if !disk.is_null() {
        // SAFETY: live published state; `intr` pins a live InterruptObject.
        crate::sched::signal_interrupt(unsafe { (*disk).intr });
    }
}

/// Poll the in-flight command to completion and complete its IRP — the boot
/// self-test's fallback when the IRQ does not fire (e.g. an unrouted GSI).
/// Returns `true` if a command was completed this way.
pub fn poll_complete_inflight() -> bool {
    let disk = AHCI.load(Ordering::Acquire);
    if disk.is_null() {
        return false;
    }
    // SAFETY: live published state.
    let d = unsafe { &*disk };
    if d.inflight.load(Ordering::Acquire).is_null() {
        return false;
    }
    if !wait_command_polled(d) {
        // leave inflight for the caller to observe the failure
    }
    let irp = d.inflight.swap(core::ptr::null_mut(), Ordering::AcqRel);
    if irp.is_null() {
        return false;
    }
    let err = read32(d.port_base, PX_TFD) & TFD_ERR != 0;
    // SAFETY: `irp` is the in-flight request.
    unsafe {
        let status = if err {
            crate::syscall::error::KError::IoError as i32
        } else {
            IrpStatus::Success as i32
        };
        (*irp).set_completion(status, if err { 0 } else { (*irp).length });
        crate::dpc::enqueue(&(*irp).dpc);
    }
    crate::dpc::run_pending();
    true
}
