//! AHCI (SATA) block driver: a single controller and SATA disk, one command at a time (slot 0).
//!
//! Brings up the class-`01.06.01` HBA that PCI enumeration found, maps its ABAR uncached, runs
//! `IDENTIFY DEVICE` on the first SATA disk and publishes it as a block [`DeviceNode`] backed by a
//! [`BlockBackend`]: [`submit`] → controller DMA into the IRP's fragments → [`isr`] → the IRP's
//! completion DPC. MSI where the controller has it, IOAPIC-routed INTx otherwise. NCQ, multi-port
//! and MSI-X are deferred (`docs/rationale/deferred-decisions.md`); see
//! `docs/architecture/drivers-and-irps.md`.

use crate::libkern::block::{BlockKind, MAX_DEVICE_NAME};
use crate::libkern::printable::Printable;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use crate::arch::irq_install::ArchIrqInstall;
use crate::arch::timer::ArchTimer;
use crate::device::{Outcome, Signal};
use crate::dpc::Dpc;
use crate::io::block::BlockBackend;
use crate::io::irp::{Irp, IrpStatus, PhysFrag};
use crate::io::stats::Counted;
use crate::libkern::handle::KObjectType;
use crate::libkern::lockrank::LockRank;
use crate::libkern::{IrqSpinLock, KBox};
use crate::mm::dma::DmaBuffer;
use crate::mm::{PhysAddr, kvmap};
use crate::object::device_node::{
    BarWindow, BlockGeometry, DeviceIdentity, DeviceNode, InterruptSpec, ResourceDescriptor,
};
use crate::object::{InterruptObject, ObjectRef};

const HBA_CAP: u64 = 0x00;
const HBA_GHC: u64 = 0x04;
const HBA_IS: u64 = 0x08;
const HBA_PI: u64 = 0x0C;
const CAP_S64A: u32 = 1 << 31; // HBA supports 64-bit addressing
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

const ATA_IDENTIFY: u8 = 0xEC;
const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
const ATA_FLUSH_CACHE: u8 = 0xE7;
const ATA_FLUSH_CACHE_EXT: u8 = 0xEA;

/// **The ATA command for an IRP op, and whether it writes**; `None` for an op with no command.
/// A flush must never fall through to a `READ DMA EXT` of count 0 (65,536 sectors under LBA48).
fn ata_command(op: u32, flush: u8) -> Option<(u8, bool)> {
    use crate::io::irp::IrpOp;
    match op {
        o if o == IrpOp::Read as u32 => Some((ATA_READ_DMA_EXT, false)),
        o if o == IrpOp::Write as u32 => Some((ATA_WRITE_DMA_EXT, true)),
        o if o == IrpOp::Flush as u32 => Some((flush, false)),
        _ => None,
    }
}

/// **Which flush the drive takes**, from IDENTIFY word 83 bit 13 (`FLUSH CACHE EXT`); without it,
/// `FLUSH CACHE`, which every ATA-6 drive has.
fn flush_command(word83: u16) -> u8 {
    if word83 & (1 << 13) != 0 { ATA_FLUSH_CACHE_EXT } else { ATA_FLUSH_CACHE }
}

const SECTOR_SIZE: u32 = 512;

// A command table is a 64-byte command FIS, a 16-byte ATAPI command and 48 reserved bytes, then
// the PRDT: 16 bytes per entry, one per physical fragment. `MAX_PRDT_ENTRIES` must equal what
// `build_command` writes: an overflow has the HBA read PRDT entries from memory the table does
// not own and DMA to whatever addresses it finds.

const PRDT_OFFSET: usize = 128;
const PRDT_ENTRY: usize = 16;
/// How much memory one command table gets: the allocation `init` makes and the one the capacity
/// below derives from, so the two cannot be edited apart.
const CMD_TABLE_BYTES: usize = crate::mm::PAGE_SIZE;
const MAX_PRDT_ENTRIES: u32 = ((CMD_TABLE_BYTES - PRDT_OFFSET) / PRDT_ENTRY) as u32;
/// Bounded poll for command completion / port readiness (~1 s of monotonic time).
const POLL_TIMEOUT_NS: u64 = 1_000_000_000;

/// Depth of the software queue in front of slot 0, drained FIFO as it retires; a full queue fails
/// the excess submit with `IoError` (backpressure).
const PENDING_DEPTH: usize = 32;

/// A FIFO ring of block IRP pointers waiting for slot 0; the submitter owns the boxes.
struct PendingRing {
    ring: [*mut Irp; PENDING_DEPTH],
    head: usize,
    len: usize,
}

impl PendingRing {
    const fn new() -> Self {
        PendingRing { ring: [core::ptr::null_mut(); PENDING_DEPTH], head: 0, len: 0 }
    }
    fn push(&mut self, irp: *mut Irp) -> bool {
        if self.len == PENDING_DEPTH {
            return false;
        }
        let tail = (self.head + self.len) % PENDING_DEPTH;
        self.ring[tail] = irp;
        self.len += 1;
        true
    }
    fn pop(&mut self) -> *mut Irp {
        if self.len == 0 {
            return core::ptr::null_mut();
        }
        let irp = self.ring[self.head];
        self.head = (self.head + 1) % PENDING_DEPTH;
        self.len -= 1;
        irp
    }
}

/// State for the single supported AHCI disk, leaked to `'static`; the ISR reaches it through
/// [`AHCI`].
pub struct AhciDisk {
    abar: u64,      // mapped ABAR kernel-virtual base
    port_base: u64, // abar + 0x100 + port*0x80
    port: u32,
    _cmd_list: DmaBuffer, // 1 KiB command list (kept mapped/owned)
    _fis: DmaBuffer,      // 256 B received-FIS area
    cmd_table: DmaBuffer, // CFIS + PRDT for slot 0
    sectors: u64,
    flush_command: u8,
    /// The IRP issued on slot 0 (`null` when idle). Accessed under [`pending`]'s lock; atomic for
    /// the lock-free peek in [`poll_complete_inflight`].
    inflight: AtomicPtr<Irp>,
    /// IRPs waiting for slot 0; IRQ-safe since submit is thread context and completion the ISR.
    pending: IrqSpinLock<PendingRing>,
    intr: *mut (),
    /// When slot 0's command issued, for its time on the device; written under [`pending`], read
    /// once it retires.
    issued_at: AtomicU64,
    counted: Counted,
}

// SAFETY: the disk state is set up once at boot and is immutable thereafter except for
// `pending` and `inflight`. `submit` runs on whichever CPU submits and the ISR on whichever CPU
// takes the interrupt; both take `pending` — the port lock, an `IrqSpinLock` — before touching
// slot 0, and `inflight` is atomic.
unsafe impl Send for AhciDisk {}
// SAFETY: as above.
unsafe impl Sync for AhciDisk {}

static AHCI: AtomicPtr<AhciDisk> = AtomicPtr::new(core::ptr::null_mut());

static AHCI_INTR_DPC: Dpc = Dpc::new(ahci_intr_dpc, core::ptr::null_mut());

/// DPC that issues the next queued command after a completion, off the interrupt path.
static AHCI_DRAIN_DPC: Dpc = Dpc::new(ahci_drain_dpc, core::ptr::null_mut());

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

/// Probe and initialise an AHCI controller `DeviceNode`, publishing its first SATA disk; returns
/// its fate (claimed, or declined with why).
pub fn init(controller: &ObjectRef) -> Outcome {
    // SAFETY: `controller` pins a live `DeviceNode`.
    let dn: &DeviceNode = unsafe { &*(controller.as_ptr() as *const DeviceNode) };
    let desc = dn.descriptor();

    // BAR5 is the ABAR (AHCI 1.3). Map it uncached.
    let bar = desc.bars[5];
    if bar.size == 0 {
        crate::kprintln!("ahci: controller has no ABAR (BAR5)");
        return declined("the controller has no ABAR (BAR5)");
    }
    let pages = bar.size.div_ceil(crate::mm::PAGE_SIZE as u64).max(1);
    // SAFETY: `bar.base` is the controller's MMIO ABAR from PCI BAR sizing.
    let abar = match unsafe { kvmap::map_mmio(PhysAddr(bar.base), pages) } {
        Ok(va) => va.as_u64() + (bar.base & (crate::mm::PAGE_SIZE as u64 - 1)),
        Err(_) => {
            crate::kprintln!("ahci: ABAR map failed");
            return declined("its ABAR could not be mapped");
        }
    };

    write32(abar, HBA_GHC, read32(abar, HBA_GHC) | GHC_AE);
    let pi = read32(abar, HBA_PI);
    let cap = read32(abar, HBA_CAP);
    crate::kprintln!("ahci: HBA up (CAP {:#010x}, PI {:#010x})", cap, pi);

    // The driver writes 64-bit addresses into the command list, the FIS base and every PRDT entry.
    // An HBA without `S64A` would take the low half and DMA to the wrong place, so decline it.
    if cap & CAP_S64A == 0 {
        crate::kprintln!("ahci: HBA lacks 64-bit addressing (CAP.S64A clear) — declining");
        return declined("no 64-bit addressing (CAP.S64A clear)");
    }

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
        return declined(NO_SATA_DISK);
    }
    let port_base = abar + PORT_BASE + port as u64 * PORT_STRIDE;

    let cfg = match crate::pci::Config::map(desc) {
        Ok(c) => Some(c),
        Err(e) => {
            crate::kprintln!(
                "ahci: config space unreachable ({:?}) — INTx, and DMA is left relying on \
                 whatever the firmware set",
                e
            );
            None
        }
    };

    // **Bus mastering must be enabled before anything DMAs**: `start_port` sets FRE (the HBA then
    // writes received FISes to memory) and `identify` has it fetch a command list and a PRDT and
    // write 512 bytes back. Firmware that left the bit clear would otherwise time `identify` out.
    // Say which happened: leaving it on is a property of the firmware.
    if let Some(cfg) = &cfg {
        if crate::pci::enable_bus_master(cfg) {
            crate::kprintln!("ahci: bus master already enabled by firmware");
        } else {
            crate::kprintln!("ahci: bus master enabled by the driver");
        }
    }

    let cmd_list = match DmaBuffer::alloc(1024) {
        Ok(b) => b,
        Err(_) => return declined(OUT_OF_MEMORY),
    };
    let fis = match DmaBuffer::alloc(256) {
        Ok(b) => b,
        Err(_) => return declined(OUT_OF_MEMORY),
    };
    let cmd_table = match DmaBuffer::alloc(CMD_TABLE_BYTES) {
        Ok(b) => b,
        Err(_) => return declined(OUT_OF_MEMORY),
    };

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
        // SAFETY: `into_raw` yields the box's single creation reference, which this adopts.
        Ok(io) => unsafe { ObjectRef::from_raw(KBox::into_raw(io).as_ptr() as *mut (), KObjectType::InterruptObject) },
        Err(_) => return declined(OUT_OF_MEMORY),
    };
    let intr_ptr = intr.as_ptr();
    core::mem::forget(intr);

    let disk = AhciDisk {
        abar,
        port_base,
        port,
        _cmd_list: cmd_list,
        _fis: fis,
        cmd_table,
        sectors: 0,
        flush_command: ATA_FLUSH_CACHE,
        inflight: AtomicPtr::new(core::ptr::null_mut()),
        pending: IrqSpinLock::new(LockRank::Leaf, PendingRing::new()),
        intr: intr_ptr,
        issued_at: AtomicU64::new(0),
        counted: Counted::new(),
    };
    let disk = match KBox::try_new(disk) {
        Ok(b) => KBox::into_raw(b).as_ptr(), // leak to 'static
        Err(_) => return declined(OUT_OF_MEMORY),
    };
    AHCI.store(disk, Ordering::Release);

    let mut name = [0u8; MAX_DEVICE_NAME];
    // SAFETY: `disk` is the just-published live state.
    let (sectors, name_len, flush) = match unsafe { identify(&mut *disk, &mut name) } {
        Some(s) => s,
        None => {
            crate::kprintln!("ahci: IDENTIFY failed on port {}", port);
            return declined("IDENTIFY failed");
        }
    };
    // SAFETY: exclusive at bring-up (no IRQ, no other CPU).
    unsafe {
        (*disk).sectors = sectors;
        (*disk).flush_command = flush;
    }
    crate::kprintln!(
        "ahci: port {} disk ready ({} sectors, {} MiB): {}",
        port,
        sectors,
        sectors * SECTOR_SIZE as u64 / (1024 * 1024),
        Printable(&name[..name_len])
    );
    // Its own line: the one above is matched by a gate.
    crate::kprintln!(
        "ahci: port {} flushes with {}",
        port,
        if flush == ATA_FLUSH_CACHE_EXT { "FLUSH CACHE EXT" } else { "FLUSH CACHE" }
    );

    // Prefer MSI: it does not depend on the PCI interrupt-line register, which QEMU's firmware
    // programs and real UEFI often leaves meaningless. INTx is the fallback.
    let mut signal = None;
    if let Some(cfg) = &cfg
        && let Some(msi) = crate::pci::read_msi(cfg)
    {
        // SAFETY: ring-0, post-Irq::init; `isr` stays valid for the kernel's
        // lifetime, and nothing can be delivered until the message is written.
        match unsafe { crate::arch::IrqInstall::install_msi(isr) } {
            // SAFETY: the handler for `msg.vector` was just registered, so
            // the device may write the message from here on.
            Some(msg) if unsafe { crate::pci::program_msi(cfg, &msi, msg.address, msg.data) } => {
                // MSI does not suppress INTx, and a device able to do both
                // would assert a line nothing is listening on.
                crate::pci::set_intx_disabled(cfg, true);
                crate::kprintln!(
                    "ahci: irq via MSI (vec {:#04x}, addr {:#x}, data {:#06x}, {}-bit cap at {:#04x})",
                    msg.vector,
                    msg.address,
                    msg.data,
                    if msi.addr64 { 64 } else { 32 },
                    msi.off
                );
                signal = Some(Signal::Msi { vector: msg.vector });
            }
            Some(_) => crate::kprintln!(
                "ahci: MSI declined — the capability cannot hold the message address; \
                     falling back"
            ),
            None => crate::kprintln!("ahci: MSI declined — this CPU cannot be named in a message; falling back"),
        }
    }
    let signal = match signal {
        Some(signal) => signal,
        None => {
            // The GSI comes from the PCI interrupt-line register (firmware-programmed
            // on QEMU; ACPI `_PRT` routing is deferred).
            let gsi = desc.interrupt.line as u32;
            // SAFETY: ring-0, post-IrqRouter::init; `isr` stays valid for the
            // kernel's lifetime.
            let vec = unsafe { crate::arch::IrqInstall::install_intx(gsi, isr) };
            if let Some(cfg) = &cfg {
                crate::pci::set_intx_disabled(cfg, false);
            }
            crate::kprintln!("ahci: irq via INTx (GSI {}, vec {:#04x})", gsi, vec);
            Signal::Intx { gsi, vector: vec }
        }
    };

    write32(abar, HBA_GHC, read32(abar, HBA_GHC) | GHC_IE);
    write32(abar, HBA_IS, read32(abar, HBA_IS)); // clear stale

    if publish_disk(controller, sectors, &name[..name_len], disk) {
        Outcome::Claimed { driver: DRIVER, signal }
    } else {
        declined("the disk's device node could not be allocated")
    }
}

const DRIVER: &str = "ahci";

const NO_SATA_DISK: &str = "no SATA disk on any implemented port";

const OUT_OF_MEMORY: &str = "out of memory";

fn declined(why: &'static str) -> Outcome {
    Outcome::Declined { driver: DRIVER, why }
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

fn wait_clear(pb: u64, off: u64, mask: u32) {
    let start = crate::arch::Timer::read_ns();
    while read32(pb, off) & mask != 0 {
        if crate::arch::Timer::read_ns().wrapping_sub(start) > POLL_TIMEOUT_NS {
            return;
        }
        core::hint::spin_loop();
    }
}

/// Run `IDENTIFY DEVICE` (polled) and return the LBA48 sector count, the length of the
/// identity written to `name`, and the flush command the drive takes.
///
/// # Safety
/// `disk` is the live, brought-up disk state; called at bring-up with no IRQ.
unsafe fn identify(disk: &mut AhciDisk, name: &mut [u8; MAX_DEVICE_NAME]) -> Option<(u64, usize, u8)> {
    let data = DmaBuffer::alloc(SECTOR_SIZE as usize).ok()?;
    let frags = [PhysFrag { base: data.phys().as_u64(), len: SECTOR_SIZE as u64 }];
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
    // The model and serial: words 10..=19 are the serial and 27..=46 the model, ASCII in
    // **byte-swapped pairs** (the wire order of the old 16-bit register interface); a person about
    // to lose a disk confirms it by this string.
    // SAFETY: as above; words 10..=46 are inside the 512-byte result.
    let len = unsafe { format_identity(words, name) };
    // SAFETY: as above; word 83 is inside the 512-byte result.
    let flush = flush_command(unsafe { words.add(83).read() });
    if n == 0 { None } else { Some((n, len, flush)) }
}

/// Write `model (serial)` from an IDENTIFY result into `out`, returning its length.
///
/// # Safety
/// `words` must address a complete 512-byte IDENTIFY result.
unsafe fn format_identity(words: *const u16, out: &mut [u8; MAX_DEVICE_NAME]) -> usize {
    let field = |first: usize, words_len: usize, dst: &mut [u8]| -> usize {
        let mut n = 0;
        for i in 0..words_len {
            // SAFETY: `first + i` is within the 256-word result, per the caller's contract.
            let w = unsafe { words.add(first + i).read() };
            for b in [(w >> 8) as u8, w as u8] {
                if n < dst.len() {
                    dst[n] = if b.is_ascii_graphic() || b == b' ' { b } else { b'?' };
                    n += 1;
                }
            }
        }
        // **ATA pads with spaces, and not always on the right**: the laptop's Seagate
        // right-justifies its serial, and trimming only the tail left interior spaces in a name the
        // installer asks a person to type back. QEMU never showed this.
        let mut start = 0;
        while start < n && dst[start] == b' ' {
            start += 1;
        }
        while n > start && dst[n - 1] == b' ' {
            n -= 1;
        }
        dst.copy_within(start..n, 0);
        n - start
    };
    let mut model = [0u8; 40];
    let mut serial = [0u8; 20];
    let m = field(27, 20, &mut model);
    let s = field(10, 10, &mut serial);
    let mut n = 0;
    let push = |bytes: &[u8], out: &mut [u8; MAX_DEVICE_NAME], n: &mut usize| {
        for &b in bytes {
            if *n < MAX_DEVICE_NAME {
                out[*n] = b;
                *n += 1;
            }
        }
    };
    push(&model[..m], out, &mut n);
    if s > 0 {
        push(b" (", out, &mut n);
        push(&serial[..s], out, &mut n);
        push(b")", out, &mut n);
    }
    n
}

fn build_command(disk: &AhciDisk, command: u8, lba: u64, count: u16, frags: &[PhysFrag], write: bool) {
    let ct = disk.cmd_table.virt();
    // SAFETY: `ct` is our owned page-sized command-table buffer, and `frags.len()` is
    // `<= MAX_PRDT_ENTRIES` — refused by [`submit`] before the port lock is taken, and again by
    // `dispatch_block_irp` against the `max_frags` this driver publishes. The check is not here
    // because here is under the port lock, where a failure cannot be reported (a `debug_assert!`
    // there panics on a lock-order violation while formatting, and vanishes in release).
    unsafe { core::ptr::write_bytes(ct, 0, PRDT_OFFSET + frags.len() * PRDT_ENTRY) };

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

    for (i, f) in frags.iter().enumerate() {
        let e = (ct as u64 + PRDT_OFFSET as u64 + (i as u64) * PRDT_ENTRY as u64) as *mut u32;
        // SAFETY: `e` is within the owned command-table page (frags bounded).
        unsafe {
            e.add(0).write(f.base as u32);
            e.add(1).write((f.base >> 32) as u32);
            e.add(2).write(0);
            // DBC = byte count - 1 (bits 21:0); bit 31 = interrupt on completion.
            e.add(3).write(((f.len as u32 - 1) & 0x003F_FFFF) | (1 << 31));
        }
    }

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

fn publish_disk(controller: &ObjectRef, sectors: u64, name: &[u8], disk: *mut AhciDisk) -> bool {
    // SAFETY: `controller` pins a live `DeviceNode`.
    let cdesc = unsafe { &*(controller.as_ptr() as *const DeviceNode) }.descriptor();
    let backend = BlockBackend {
        submit,
        poll: ahci_poll,
        ctx: disk as *mut (),
        // One command table, one page, so one command describes this many fragments.
        max_frags: MAX_PRDT_ENTRIES,
    };
    let geometry = BlockGeometry { logical_block_size: SECTOR_SIZE, block_count: sectors };
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
    match DeviceNode::try_new_block(descriptor, geometry, BlockKind::Disk, name, backend) {
        Ok(node) => {
            // SAFETY: adopt the creation reference into the device table.
            let r = unsafe { ObjectRef::from_raw(KBox::into_raw(node).as_ptr() as *mut (), KObjectType::DeviceNode) };
            crate::device::register(r.clone(), "ahci");
            // SAFETY: `disk` is the live published state; `r` was just registered, and the table
            // holds it for the kernel's lifetime.
            unsafe { (*disk).counted.attach(&r) };
            true
        }
        Err(_) => false,
    }
}

/// [`BlockBackend::submit`] for an AHCI disk: builds a `READ DMA EXT` (or write or flush) against
/// the IRP's fragments and issues it. Completion arrives via the IRQ or the boot polled fallback.
fn submit(irp: *mut Irp, ctx: *mut ()) {
    let disk = ctx as *mut AhciDisk;
    // SAFETY: `disk` is the live published disk.
    let d = unsafe { &*disk };

    // **A rescan is refused**: a SATA disk's table is read once at boot, by polling; partitioning
    // an internal disk is `nxinstall`'s, and a reboot follows.
    // SAFETY: `irp` is a live block IRP during submit.
    if unsafe { (*irp).op } == crate::io::irp::IrpOp::Rescan as u32 {
        // SAFETY: `irp` is uniquely owned during submit; completing it releases its waiter.
        unsafe {
            (*irp).set_completion(crate::syscall::error::KError::Unsupported as i32, 0);
            crate::dpc::enqueue(&(*irp).dpc);
        }
        return;
    }

    // **More fragments than one command table can describe: refuse, before the lock.**
    // `dispatch_block_irp` already rejects this against the published `max_frags`; this is the
    // backstop for another route. It precedes the port lock because failing an IRP means completing
    // it, unavailable under the lock.
    // SAFETY: `irp` is a live block IRP during submit.
    if unsafe { (*irp).buffer.count } as usize > MAX_PRDT_ENTRIES as usize {
        // SAFETY: `irp` is uniquely owned during submit; complete it with an error so
        // its waiter is released rather than parked forever.
        unsafe {
            (*irp).set_completion(crate::syscall::error::KError::InvalidArgument as i32, 0);
            crate::dpc::enqueue(&(*irp).dpc);
        }
        return;
    }

    // Take the port lock: it masks interrupts, so the completion ISR cannot run on
    // this CPU while we decide whether slot 0 is free — `inflight` is stable here.
    let mut q = d.pending.lock();
    if d.inflight.load(Ordering::Acquire).is_null() {
        d.inflight.store(irp, Ordering::Release);
        // SAFETY: slot 0 is free; `irp` is a live block IRP.
        if !unsafe { issue_locked(d, irp) } {
            d.inflight.store(core::ptr::null_mut(), Ordering::Release);
            drop(q);
            fail_irp(irp);
        }
    } else if !q.push(irp) {
        drop(q);
        // SAFETY: `irp` is uniquely owned during submit; complete it with an error.
        unsafe {
            (*irp).set_completion(crate::syscall::error::KError::IoError as i32, 0);
            crate::dpc::enqueue(&(*irp).dpc);
        }
    }
}

/// Build slot 0's command for `irp` and issue it; the caller holds [`AhciDisk::pending`] and has
/// established that slot 0 is idle. `false`, issuing nothing, for an op with no ATA command
/// ([`ata_command`]): the caller fails the IRP once it has let go of the lock.
///
/// # Safety
/// `irp` is a live block IRP whose `buffer.frags` is a valid `[PhysFrag; count]`, or has a
/// count of `0` — a flush carries no buffer, and its pointer is null; slot 0 is idle (no other
/// command building/running).
unsafe fn issue_locked(d: &AhciDisk, irp: *mut Irp) -> bool {
    // SAFETY: `irp` is live.
    let (op, offset, length, buffer) = unsafe { ((*irp).op, (*irp).offset, (*irp).length, (*irp).buffer) };
    // **No buffer is an empty slice**, not one built from a null pointer, which `from_raw_parts`
    // forbids.
    let frags: &[PhysFrag] = if buffer.count == 0 {
        &[]
    } else {
        // SAFETY: a non-empty buffer's `frags` array is owned by the IRP box.
        unsafe { core::slice::from_raw_parts(buffer.frags as *const PhysFrag, buffer.count as usize) }
    };
    let lba = offset / SECTOR_SIZE as u64;
    let count = (length / SECTOR_SIZE as u64) as u16;
    let Some((command, write)) = ata_command(op, d.flush_command) else {
        return false;
    };
    // A flush is non-data — no LBA, no count, no PRDT — and completes with a D2H register FIS,
    // which `PXIE_DHRE` already interrupts on.
    if op == crate::io::irp::IrpOp::Flush as u32 {
        build_command(d, command, 0, 0, &[], false);
    } else {
        build_command(d, command, lba, count, frags, write);
    }
    d.issued_at.store(crate::arch::Timer::read_ns(), Ordering::Relaxed);
    issue(d);
    true
}

/// **Complete the IRP slot 0 retired, and count it**: its status, the bytes it moved, and its time
/// on the device since [`issue_locked`] stamped it — read before the next command can issue.
///
/// # Safety
/// `irp` is the IRP [`take_retired`] just took out of flight: live, and uniquely this driver's
/// until its DPC runs.
unsafe fn retire(d: &AhciDisk, irp: *mut Irp, err: bool) {
    let ns = crate::arch::Timer::read_ns().saturating_sub(d.issued_at.load(Ordering::Relaxed));
    // SAFETY: as the contract says.
    unsafe {
        let status = if err { crate::syscall::error::KError::IoError as i32 } else { IrpStatus::Success as i32 };
        let transferred = if err { 0 } else { (*irp).length };
        d.counted.record((*irp).op, !err, transferred, ns);
        (*irp).set_completion(status, transferred);
        crate::dpc::enqueue(&(*irp).dpc);
    }
}

/// Slot 0 has retired: under the port lock, take the completed IRP out of flight (capturing its
/// error status) and mark the slot idle. Returns the IRP (`null` if already idle) and whether it
/// errored. Does **not** issue the next command: [`drain_queue`] does, from a DPC, so a fast
/// pipelined completion cannot race the interrupt ack. The caller completes the IRP outside any
/// lock.
fn take_retired(d: &AhciDisk) -> (*mut Irp, bool) {
    let _q = d.pending.lock();
    let retired = d.inflight.swap(core::ptr::null_mut(), Ordering::AcqRel);
    let err = read32(d.port_base, PX_TFD) & TFD_ERR != 0;
    (retired, err)
}

/// If slot 0 is idle, issue the next queued IRP. Called from the completion DPC and the boot poll
/// path, not the ISR.
fn drain_queue(d: &AhciDisk) {
    loop {
        let refused = {
            let mut q = d.pending.lock();
            if !d.inflight.load(Ordering::Acquire).is_null() {
                return;
            }
            let next = q.pop();
            if next.is_null() {
                return;
            }
            d.inflight.store(next, Ordering::Release);
            // SAFETY: slot 0 is idle; `next` is a live queued IRP.
            if unsafe { issue_locked(d, next) } {
                return;
            }
            d.inflight.store(core::ptr::null_mut(), Ordering::Release);
            next
        };
        fail_irp(refused);
    }
}

/// Complete `irp` with `InvalidArgument` (an op with no command), outside the port lock.
fn fail_irp(irp: *mut Irp) {
    // SAFETY: `irp` is a live IRP no longer in flight or queued, so uniquely ours.
    unsafe {
        (*irp).set_completion(crate::syscall::error::KError::InvalidArgument as i32, 0);
        crate::dpc::enqueue(&(*irp).dpc);
    }
}

/// The AHCI completion ISR (device-IRQ context): acknowledges the controller, completes the
/// in-flight IRP via its DPC and queues the `InterruptObject` signal.
extern "C" fn isr() {
    let disk = AHCI.load(Ordering::Acquire);
    if disk.is_null() {
        return;
    }
    // SAFETY: `disk` was leaked to `'static` before its `Release` store, which this `Acquire`
    // load pairs with; `AhciDisk` is `Sync`, so a shared borrow on any CPU is sound.
    let d = unsafe { &*disk };

    let pxis = read32(d.port_base, PX_IS);
    write32(d.port_base, PX_IS, pxis);
    write32(d.abar, HBA_IS, 1 << d.port);

    // If slot 0 has retired, complete the IRP and queue the drain DPC for the next waiting command.
    if read32(d.port_base, PX_CI) & 1 == 0 {
        let (irp, err) = take_retired(d);
        if !irp.is_null() {
            // SAFETY: `irp` is the retired request; set status + queue its DPC.
            unsafe { retire(d, irp, err) };
            crate::dpc::enqueue(&AHCI_DRAIN_DPC);
        }
    }
    crate::dpc::enqueue(&AHCI_INTR_DPC);
}

/// [`BlockBackend::poll`] for the AHCI disk: complete the in-flight command by polling (boot path).
fn ahci_poll(_ctx: *mut ()) {
    poll_complete_inflight();
}

fn ahci_intr_dpc(_ctx: *mut ()) {
    let disk = AHCI.load(Ordering::Acquire);
    if !disk.is_null() {
        // SAFETY: `disk` is live published state, and its `intr` a live `InterruptObject`.
        unsafe { crate::sched::signal_interrupt((*disk).intr) };
    }
}

fn ahci_drain_dpc(_ctx: *mut ()) {
    let disk = AHCI.load(Ordering::Acquire);
    if !disk.is_null() {
        // SAFETY: live published state.
        drain_queue(unsafe { &*disk });
    }
}

/// Poll the in-flight command to completion and complete its IRP — the boot self-test's fallback
/// when the IRQ does not fire. Returns `true` if a command was completed this way.
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
    // Spin until slot 0 retires or times out; `take_retired` completes it even on timeout, with its
    // error status, so its waiter is not left stuck. Boot polling is synchronous: issue the next
    // directly.
    let _ = wait_command_polled(d);
    let (irp, err) = take_retired(d);
    if irp.is_null() {
        return false;
    }
    // SAFETY: `irp` is the retired request.
    unsafe { retire(d, irp, err) };
    drain_queue(d);
    crate::dpc::run_pending();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_op_has_its_command_and_nothing_else_does() {
        use crate::io::irp::IrpOp;
        assert_eq!(ata_command(IrpOp::Read as u32, 0xEA), Some((ATA_READ_DMA_EXT, false)));
        assert_eq!(ata_command(IrpOp::Write as u32, 0xEA), Some((ATA_WRITE_DMA_EXT, true)));
        assert_eq!(ata_command(IrpOp::Flush as u32, 0xEA), Some((0xEA, false)), "the drive's flush");
        assert_eq!(ata_command(IrpOp::Flush as u32, 0xE7), Some((0xE7, false)));
        assert_eq!(ata_command(3, 0xEA), None, "not a read, which a count of 0 makes 65,536 sectors");
    }

    #[test]
    fn a_drive_with_flush_cache_ext_is_flushed_with_it() {
        assert_eq!(flush_command(1 << 13), ATA_FLUSH_CACHE_EXT);
        assert_eq!(flush_command(0xFFFF), ATA_FLUSH_CACHE_EXT);
        assert_eq!(flush_command(!(1 << 13)), ATA_FLUSH_CACHE, "every other bit set, not this one");
        assert_eq!(flush_command(0), ATA_FLUSH_CACHE);
    }

    #[test]
    fn the_prdt_capacity_is_what_fits_in_a_command_table() {
        assert_eq!(MAX_PRDT_ENTRIES, 248);
        assert!(PRDT_OFFSET + MAX_PRDT_ENTRIES as usize * PRDT_ENTRY <= CMD_TABLE_BYTES);
        assert!(PRDT_OFFSET + (MAX_PRDT_ENTRIES as usize + 1) * PRDT_ENTRY > CMD_TABLE_BYTES);
    }

    /// What that capacity is worth as a transfer, pinning the figure in `deferred-decisions.md`.
    #[test]
    fn one_command_covers_just_under_a_megabyte() {
        let bytes = MAX_PRDT_ENTRIES as usize * crate::mm::PAGE_SIZE;
        assert_eq!(bytes, 1_015_808);
        assert_eq!(bytes / 1024, 992);
    }

    /// Lay `text` into an IDENTIFY field as a drive does: byte-swapped within each word,
    /// space-padded.
    fn put_field(words: &mut [u16; 256], first: usize, width_words: usize, text: &[u8]) {
        let mut bytes = [b' '; 40];
        bytes[..text.len()].copy_from_slice(text);
        for i in 0..width_words {
            words[first + i] = u16::from(bytes[i * 2]) << 8 | u16::from(bytes[i * 2 + 1]);
        }
    }

    fn identity_of(model: &[u8], serial: &[u8]) -> ([u8; MAX_DEVICE_NAME], usize) {
        let mut words = [0u16; 256];
        put_field(&mut words, 27, 20, model);
        put_field(&mut words, 10, 10, serial);
        let mut out = [0u8; MAX_DEVICE_NAME];
        // SAFETY: `words` is a full 256-word IDENTIFY result, which is the contract.
        let n = unsafe { format_identity(words.as_ptr(), &mut out) };
        (out, n)
    }

    fn assert_identity(model: &[u8], serial: &[u8], want: &[u8]) {
        let (out, n) = identity_of(model, serial);
        assert_eq!(
            &out[..n],
            want,
            "model {:?} serial {:?} produced {:?}",
            core::str::from_utf8(model),
            core::str::from_utf8(serial),
            core::str::from_utf8(&out[..n])
        );
    }

    /// **Padding is trimmed at both ends**, because the standard does not say which end a drive
    /// pads.
    #[test]
    fn a_right_justified_field_loses_its_padding_too() {
        assert_identity(b"ST1000LM035-1RK172", b"            WDELGVZ3", b"ST1000LM035-1RK172 (WDELGVZ3)");
        assert_identity(b"QEMU HARDDISK", b"QM00001", b"QEMU HARDDISK (QM00001)");
        // Padding on both ends of both fields, interior spaces kept: `QEMU HARDDISK` has one.
        assert_identity(b"  A B  ", b"   C D  ", b"A B (C D)");
    }

    /// A field that is nothing but padding is empty, and a disk with no serial is named by its
    /// model alone.
    #[test]
    fn an_all_padding_field_comes_back_empty() {
        assert_identity(b"ST1000LM035", b"                    ", b"ST1000LM035");
        assert_identity(b"                                        ", b"", b"");
    }
}
