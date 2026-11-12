//! AHCI (SATA) block driver: a single controller and SATA disk, one command at a time (slot 0).
//! A second controller is declined before it is touched.
//!
//! Brings up the class-`01.06.01` HBA that PCI enumeration found, maps its ABAR uncached, runs
//! `IDENTIFY DEVICE` on the first SATA disk and publishes it as a block [`DeviceNode`] backed by a
//! [`BlockBackend`]: [`submit`] → controller DMA into the IRP's fragments → [`isr`] → the IRP's
//! completion DPC. MSI where the controller has it, IOAPIC-routed INTx otherwise. NCQ, multi-port
//! and MSI-X are deferred (`docs/rationale/deferred-decisions.md`); see
//! `docs/architecture/drivers-and-irps.md`.

use crate::libkern::block::{BlockKind, MAX_DEVICE_NAME};
use crate::libkern::printable::Printable;

use crate::arch::irq_install::ArchIrqInstall;
use crate::device::{Outcome, Signal};
use crate::dpc::Dpc;
use crate::io::block::BlockBackend;
use crate::io::irp::{Irp, IrpStatus, PhysFrag};
use crate::io::stats::Counted;
use crate::libkern::lockrank::LockRank;
use crate::libkern::{IrqSpinLock, KBox, Published};
use crate::mm::dma::{DmaBuffer, Programmed, Quiesced};
use crate::mm::{Mmio, PhysAddr, kvmap};
use crate::object::InterruptObject;
use crate::object::device_node::{
    BarWindow, BlockGeometry, DeviceIdentity, DeviceNode, InterruptSpec, ResourceDescriptor,
};
use crate::object::header::Ref;

const HBA_CAP: u64 = 0x00;
const HBA_GHC: u64 = 0x04;
const HBA_IS: u64 = 0x08;
const HBA_PI: u64 = 0x0C;
const CAP_S64A: u32 = 1 << 31; // HBA supports 64-bit addressing
const GHC_AE: u32 = 1 << 31; // AHCI enable
const GHC_IE: u32 = 1 << 1; // global interrupt enable

const PORT_BASE: u64 = 0x100; // first port's registers
const PORT_STRIDE: u64 = 0x80;

/// Port `port`'s registers, if the ABAR holds them.
fn port_regs(abar: Mmio<'static>, port: u32) -> Option<Mmio<'static>> {
    abar.sub(PORT_BASE + port as u64 * PORT_STRIDE, PORT_STRIDE)
}

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
const PX_SCTL: u64 = 0x2C;
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
/// The errors that stop the port: a task file error, a host bus fatal or data error, an interface
/// fatal error. The same bits enable each in PxIE and report it in PxIS.
const PX_FATAL: u32 = PXIE_TFEE | (1 << 29) | (1 << 28) | (1 << 27);

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

/// **A command's deadline**: what Linux's `sd` gives one, as xHCI's storage does.
const COMMAND_DEADLINE_NS: u64 = 30_000_000_000;

/// [`PortState::fault`]'s states.
const CLEAR: u8 = 0;
const FAULTED: u8 = 1;
const RECOVERING: u8 = 2;

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

/// **Slot 0 and the queue in front of it**, read and changed only in a hold of the port lock.
struct PortState {
    /// The 1 KiB command list, slot 0's header first.
    cmd_list: Programmed,
    /// Slot 0's command table: its FIS, then its PRDT.
    cmd_table: Programmed,
    queue: PendingRing,
    /// The IRP issued on slot 0, `null` when idle.
    inflight: *mut Irp,
    /// When slot 0's command issued, for its time on the device and its deadline.
    issued_at: u64,
    /// [`CLEAR`], [`FAULTED`] or [`RECOVERING`]: while not clear, the ISR retires nothing.
    fault: u8,
    /// **The port would not stop**: its command kept, all else refused.
    dead: bool,
}

impl PortState {
    fn new(cmd_list: Programmed, cmd_table: Programmed) -> Self {
        PortState {
            cmd_list,
            cmd_table,
            queue: PendingRing::new(),
            inflight: core::ptr::null_mut(),
            issued_at: 0,
            fault: CLEAR,
            dead: false,
        }
    }
}

/// State for the single supported AHCI disk, leaked to `'static`; the ISR reaches it through
/// [`AHCI`].
pub struct AhciDisk {
    abar: Mmio<'static>,
    /// Its port's registers, inside the ABAR.
    port_base: Mmio<'static>,
    port: u32,
    _fis: Programmed, // 256 B received-FIS area
    sectors: u64,
    flush_command: u8,
    /// The port lock: slot 0 and its queue, taken from thread context and the ISR alike.
    state: IrqSpinLock<PortState>,
    intr: Ref<InterruptObject>,
    counted: Counted,
    /// What wakes the watch thread: a fault's DPC signals it. An `InterruptObject`.
    wake: Ref<InterruptObject>,
}

// SAFETY: the disk state is set up once at boot and is immutable thereafter except for `state`,
// the port lock, which `submit`, the ISR and the watch thread take, on whichever CPU, before
// reading or changing slot 0, whose memory it holds.
unsafe impl Send for AhciDisk {}
// SAFETY: as above.
unsafe impl Sync for AhciDisk {}

impl AhciDisk {
    /// Its port stopped: the memory it gave the port is freed with it.
    fn release(&mut self, stopped: &Quiesced) {
        let p = &mut *self.state.lock();
        for b in [&mut p.cmd_list, &mut self._fis, &mut p.cmd_table] {
            b.release(stopped);
        }
    }
}

/// **Decline a disk whose port started**: its memory freed with it once the port has stopped, and
/// kept for good if it will not.
fn give_back(disk: &mut AhciDisk) {
    if let Some(stopped) = stop_port(disk.port_base) {
        disk.release(&stopped);
    }
}

static AHCI: Published<AhciDisk> = Published::new();

static AHCI_INTR_DPC: Dpc = Dpc::new(ahci_intr_dpc, core::ptr::null_mut());

/// DPC that issues the next queued command after a completion, off the interrupt path.
static AHCI_DRAIN_DPC: Dpc = Dpc::new(ahci_drain_dpc, core::ptr::null_mut());

/// DPC that wakes the watch thread to recover the port after a fault.
static AHCI_FAULT_DPC: Dpc = Dpc::new(ahci_fault_dpc, core::ptr::null_mut());

mod bringup;
#[cfg(test)]
use bringup::claim_port;
pub use bringup::init;

const DRIVER: &str = "ahci";

const NO_SATA_DISK: &str = "no SATA disk on any implemented port";

const OUT_OF_MEMORY: &str = "out of memory";

fn declined(why: &'static str) -> Outcome {
    Outcome::Declined { driver: DRIVER, why }
}

/// Stop the port: clear ST then FRE, waiting for CR/FR to clear. The proof they did, or `None`: a
/// command list still running may still move a command's data.
fn stop_port(pb: Mmio) -> Option<Quiesced> {
    let mut cmd = pb.read32(PX_CMD);
    cmd &= !CMD_ST;
    pb.write32(PX_CMD, cmd);
    wait_clear(pb, PX_CMD, CMD_CR);
    cmd = pb.read32(PX_CMD);
    cmd &= !CMD_FRE;
    pb.write32(PX_CMD, cmd);
    wait_clear(pb, PX_CMD, CMD_FR);
    // SAFETY: CR and FR read clear: the port fetches no command and receives no FIS.
    (pb.read32(PX_CMD) & (CMD_CR | CMD_FR) == 0).then(|| unsafe { Quiesced::assume() })
}

/// Start the port: set FRE then ST (FRE must precede ST).
fn start_port(pb: Mmio) {
    wait_clear(pb, PX_TFD, TFD_BSY | TFD_DRQ);
    let mut cmd = pb.read32(PX_CMD);
    cmd |= CMD_FRE;
    pb.write32(PX_CMD, cmd);
    cmd |= CMD_ST;
    pb.write32(PX_CMD, cmd);
}

/// **The port's clock.** A host test's moves on as it is read, where the host's reads 0.
#[cfg(not(test))]
fn clock() -> u64 {
    use crate::arch::timer::ArchTimer;
    crate::arch::Timer::read_ns()
}

#[cfg(test)]
use tests::clock;

fn wait_clear(pb: Mmio, off: u64, mask: u32) {
    let start = clock();
    while pb.read32(off) & mask != 0 {
        if clock().wrapping_sub(start) > POLL_TIMEOUT_NS {
            return;
        }
        core::hint::spin_loop();
    }
}

/// Run `IDENTIFY DEVICE` (polled) into `data`, a sector's page, and return the LBA48 sector count,
/// the length of the identity written to `name`, and the flush command the drive takes.
///
/// # Safety
/// `disk` is the live, brought-up disk state; called at bring-up with no IRQ.
unsafe fn identify(
    disk: &AhciDisk,
    mut data: Programmed,
    name: &mut [u8; MAX_DEVICE_NAME],
) -> Option<(u64, usize, u8)> {
    let frags = [PhysFrag { base: data.phys().as_u64(), len: SECTOR_SIZE as u64 }];
    build_command(&disk.state.lock(), ATA_IDENTIFY, 0, 0, &frags, false);
    issue(disk);
    if !wait_command_polled(disk, COMMAND_DEADLINE_NS) {
        // The command may be running yet: its buffer goes only once the port has stopped it.
        if let Some(stopped) = stop_port(disk.port_base) {
            data.release(&stopped);
        }
        return None;
    }
    // SAFETY: the command retired, PxCI read clear, and the next one rewrites the table.
    data.release(&unsafe { Quiesced::assume() });
    identity(&data.read::<[u16; 256]>(0), name)
}

/// **What an IDENTIFY result says**: its LBA48 sector count, the length of the identity written into
/// `name`, and its flush. `None` for no sectors, or more than LBA48 addresses, which overflowed.
fn identity(words: &[u16; 256], name: &mut [u8; MAX_DEVICE_NAME]) -> Option<(u64, usize, u8)> {
    let n = words[100..=103].iter().rev().fold(0u64, |n, &w| n << 16 | w as u64);
    let len = format_identity(words, name);
    (n != 0 && n < 1 << 48).then_some((n, len, flush_command(words[83])))
}

/// Write `model (serial)` from an IDENTIFY result into `out`, returning its length. Words 10..=19
/// are the serial and 27..=46 the model, ASCII in **byte-swapped pairs** (the wire order of the
/// old 16-bit register interface); a person about to lose a disk confirms it by this string.
fn format_identity(words: &[u16; 256], out: &mut [u8; MAX_DEVICE_NAME]) -> usize {
    let field = |first: usize, words_len: usize, dst: &mut [u8]| -> usize {
        let mut n = 0;
        for i in 0..words_len {
            let w = words[first + i];
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

/// Write slot 0's command into `p`, the held port lock's state.
fn build_command(p: &PortState, command: u8, lba: u64, count: u16, frags: &[PhysFrag], write: bool) {
    let ct = &p.cmd_table;
    // **The fragments fit the table**: `frags.len()` is
    // `<= MAX_PRDT_ENTRIES` — refused by [`submit`] before the port lock is taken, and again by
    // `dispatch_block_irp` against the `max_frags` this driver publishes. The check is not here
    // because here is under the port lock, where a failure cannot be reported (a `debug_assert!`
    // there panics on a lock-order violation while formatting, and vanishes in release). Those
    // refusals are all that keep a caller from the panic a write past the table's room makes, here.
    ct.fill(0, PRDT_OFFSET + frags.len() * PRDT_ENTRY, 0);

    // Command FIS — H2D Register FIS (type 0x27), C=1 (command), the device in LBA mode.
    let [l0, l1, l2, l3, l4, l5, ..] = lba.to_le_bytes();
    ct.copy_in(0, &[0x27, 0x80, command, 0, l0, l1, l2, 0x40, l3, l4, l5, 0, count as u8, (count >> 8) as u8]);

    for (i, f) in frags.iter().enumerate() {
        // DBC = byte count - 1 (bits 21:0); bit 31 = interrupt on completion.
        let dbc = ((f.len as u32 - 1) & 0x003F_FFFF) | (1 << 31);
        ct.write(PRDT_OFFSET + i * PRDT_ENTRY, [f.base as u32, (f.base >> 32) as u32, 0, dbc]);
    }

    // Slot 0's header — DW0: CFL = 5 dwords (H2D FIS), W bit, PRDTL = frag count; DW1: PRDBC.
    let dw0 = 5u32 | ((write as u32) << 6) | ((frags.len() as u32) << 16);
    let table = ct.phys().as_u64();
    p.cmd_list.write(0, [dw0, 0, table as u32, (table >> 32) as u32]);
}

fn issue(disk: &AhciDisk) {
    disk.port_base.write32(PX_CI, 1);
}

/// Poll slot 0 to completion within `timeout_ns`; `true` on success, `false` on timeout/error.
fn wait_command_polled(disk: &AhciDisk, timeout_ns: u64) -> bool {
    let start = clock();
    loop {
        let ci = disk.port_base.read32(PX_CI);
        if ci & 1 == 0 {
            break;
        }
        if disk.port_base.read32(PX_TFD) & TFD_ERR != 0 {
            return false;
        }
        if clock().wrapping_sub(start) > timeout_ns {
            return false;
        }
        core::hint::spin_loop();
    }
    disk.port_base.read32(PX_TFD) & (TFD_ERR | TFD_BSY | TFD_DRQ) == 0
}

/// **The disk's block node**, its backend pointing at `disk`, published before the node is.
fn disk_node(controller: &DeviceNode, sectors: u64, name: &[u8], disk: &AhciDisk) -> Option<KBox<DeviceNode>> {
    let cdesc = controller.descriptor();
    let backend = BlockBackend {
        submit,
        poll: ahci_poll,
        ctx: disk as *const AhciDisk as *mut (),
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
    DeviceNode::try_new_block(descriptor, geometry, BlockKind::Disk, name, backend).ok()
}

/// [`BlockBackend::submit`] for an AHCI disk: builds a `READ DMA EXT` (or write or flush) against
/// the IRP's fragments and issues it. Completion arrives via the IRQ or the boot polled fallback.
fn submit(irp: *mut Irp, ctx: *mut ()) {
    // **A rescan and an eject are refused**, before the disk is touched: a SATA disk's table is read
    // once at boot; partitioning an internal disk is `nxinstall`'s, and a reboot follows.
    // SAFETY: `irp` is a live block IRP during submit.
    let op = unsafe { (*irp).op };
    if op == crate::io::irp::IrpOp::Rescan as u32 || op == crate::io::irp::IrpOp::Eject as u32 {
        refuse(irp, crate::syscall::error::KError::Unsupported);
        return;
    }
    let disk = ctx as *mut AhciDisk;
    // SAFETY: `disk` is the live published disk.
    let d = unsafe { &*disk };

    // **More fragments than one command table can describe: refuse, before the lock.**
    // `dispatch_block_irp` already rejects this against the published `max_frags`; this is the
    // backstop for another route. It precedes the port lock because failing an IRP means completing
    // it, unavailable under the lock.
    // SAFETY: `irp` is a live block IRP during submit.
    if unsafe { (*irp).buffer.count } as usize > MAX_PRDT_ENTRIES as usize {
        refuse(irp, crate::syscall::error::KError::InvalidArgument);
        return;
    }

    // Take the port lock: it masks interrupts, so the completion ISR cannot run on
    // this CPU while we decide whether slot 0 is free.
    let mut p = d.state.lock();
    // Under the lock a disk is given up under, so nothing is queued behind its kept command.
    let refused = if p.dead {
        Some(crate::syscall::error::KError::IoError)
    } else if p.inflight.is_null() && p.fault == CLEAR {
        // SAFETY: `irp` is a live block IRP.
        let issued = unsafe { issue_locked(d, &mut p, irp) };
        (!issued).then_some(crate::syscall::error::KError::InvalidArgument)
    } else {
        (!p.queue.push(irp)).then_some(crate::syscall::error::KError::IoError)
    };
    drop(p);
    if let Some(error) = refused {
        refuse(irp, error);
    }
}

/// Build slot 0's command for `irp`, issue it and put it in flight in `p`, the held port lock's
/// state, whose slot 0 is idle. `false`, issuing nothing, for an op with no ATA command
/// ([`ata_command`]): the caller fails the IRP once it has let go of the lock.
///
/// # Safety
/// `irp` is a live block IRP whose `buffer.frags` is a valid `[PhysFrag; count]`, or has a
/// count of `0` — a flush carries no buffer, and its pointer is null.
unsafe fn issue_locked(d: &AhciDisk, p: &mut PortState, irp: *mut Irp) -> bool {
    debug_assert!(p.inflight.is_null(), "slot 0 is building or running another command");
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
        build_command(p, command, 0, 0, &[], false);
    } else {
        build_command(p, command, lba, count, frags, write);
    }
    p.inflight = irp;
    p.issued_at = clock();
    issue(d);
    true
}

/// **Complete the IRP slot 0 retired, and count it**: its status, the bytes it moved, and its time
/// on the device since `issued_at`, when [`issue_locked`] stamped it.
///
/// # Safety
/// `irp` is the IRP [`take_retired`] just took out of flight: live, and uniquely this driver's to
/// complete.
unsafe fn retire(d: &AhciDisk, irp: *mut Irp, err: bool, moved: u32, issued_at: u64) {
    let ns = clock().saturating_sub(issued_at);
    // SAFETY: as the contract says.
    unsafe {
        let (status, transferred) = completion(err, (*irp).length, moved);
        d.counted.record((*irp).op, status == IrpStatus::Success as i32, transferred, ns);
        crate::io::irp::complete(irp, status, transferred);
    }
}

/// A retired command's status and the bytes it moved: nothing on an error, or when the byte count
/// the HBA wrote back falls short, since the pages a read did not fill would be read as data.
fn completion(err: bool, length: u64, moved: u32) -> (i32, u64) {
    match err || u64::from(moved) < length {
        true => (crate::syscall::error::KError::IoError as i32, 0),
        false => (IrpStatus::Success as i32, length),
    }
}

/// If slot 0 has retired, take its IRP out of flight in `p`, the held port lock's state: the IRP
/// (`null` if idle or still running), its error, the bytes it moved and its issue time. The next
/// command issues from [`drain_queue`]'s DPC, and the caller completes the IRP once unlocked.
fn take_retired(d: &AhciDisk, p: &mut PortState) -> (*mut Irp, bool, u32, u64) {
    // Stopping the port clears PxCI, which must not read as the command finishing.
    if p.fault != CLEAR {
        return (core::ptr::null_mut(), false, 0, 0);
    }
    // **PxCI is read under the lock a submit issues under**: read before it, slot 0 could have
    // taken a new command since, retired here while the HBA still runs it.
    if d.port_base.read32(PX_CI) & 1 != 0 {
        return (core::ptr::null_mut(), false, 0, 0);
    }
    let retired = core::mem::replace(&mut p.inflight, core::ptr::null_mut());
    let err = d.port_base.read32(PX_TFD) & TFD_ERR != 0;
    // Slot 0's header DW1, which only `build_command` for the next command rewrites, under this lock.
    let moved = p.cmd_list.read::<u32>(4);
    (retired, err, moved, p.issued_at)
}

/// If slot 0 is idle and the port not faulted, issue the next queued IRP. Called from the
/// completion DPC and the boot poll path, not the ISR.
fn drain_queue(d: &AhciDisk) {
    loop {
        let refused = {
            let mut p = d.state.lock();
            if !p.inflight.is_null() || p.fault != CLEAR {
                return;
            }
            let next = p.queue.pop();
            if next.is_null() {
                return;
            }
            // SAFETY: `next` is a live queued IRP.
            if unsafe { issue_locked(d, &mut p, next) } {
                return;
            }
            next
        };
        refuse(refused, crate::syscall::error::KError::InvalidArgument);
    }
}

/// Complete `irp` with `error`, outside the port lock.
fn refuse(irp: *mut Irp, error: crate::syscall::error::KError) {
    // SAFETY: `irp` is a live IRP not in flight or queued, so uniquely ours to complete.
    unsafe { crate::io::irp::complete(irp, error as i32, 0) };
}

/// The AHCI completion ISR (device-IRQ context): acknowledges the controller, completes the
/// in-flight IRP and queues the `InterruptObject` signal.
extern "C" fn isr() {
    if let Some(d) = AHCI.get() {
        on_interrupt(d);
    }
}

/// [`isr`]'s body: acknowledge, report a fault to the watch thread, or retire slot 0.
fn on_interrupt(d: &AhciDisk) {
    let pxis = d.port_base.read32(PX_IS);
    d.port_base.write32(PX_IS, pxis);
    d.abar.write32(HBA_IS, 1 << d.port);

    // **One hold**: the fault report and the retire decide on state no submit can change meanwhile.
    let (faulted, (irp, err, moved, issued_at)) = {
        let mut p = d.state.lock();
        // An error stops the port, PxCI keeping the command's bit until ST is cleared (§6.2.2.1).
        let faulted = pxis & PX_FATAL != 0 && p.fault == CLEAR;
        if faulted {
            p.fault = FAULTED;
        }
        (faulted, take_retired(d, &mut p))
    };
    if faulted {
        crate::dpc::enqueue(&AHCI_FAULT_DPC);
    }
    // If slot 0 has retired, complete the IRP and queue the drain DPC for the next waiting command.
    if !irp.is_null() {
        // SAFETY: `irp` is the retired request, completed here once.
        unsafe { retire(d, irp, err, moved, issued_at) };
        crate::dpc::enqueue(&AHCI_DRAIN_DPC);
    }
    crate::dpc::enqueue(&AHCI_INTR_DPC);
}

fn ahci_fault_dpc(_ctx: *mut ()) {
    if let Some(d) = AHCI.get() {
        crate::sched::signal_interrupt(&d.wake);
    }
}

/// **The watch thread**: asleep until slot 0's command is due or a fault wakes it, never longer
/// than a deadline, so no command needs a wake of its own.
extern "C" fn watch(_arg: usize) {
    let Some(d) = AHCI.get() else {
        return;
    };
    while !d.state.lock().dead {
        let now = clock();
        let woke = crate::sched::wait_on(&[d.wake.as_object()], due_at(d, now), now);
        if let crate::sched::WaitResult::Signaled(_) = woke {
            crate::sched::interrupt_consume(&d.wake);
        }
        if let Some(irp) = claim(d, clock(), false) {
            said(d, recover(d, irp));
        }
    }
}

/// **Start the watch thread**, if a disk was claimed: after the scheduler and the APs are up.
pub fn start() {
    if AHCI.get().is_none() {
        return;
    }
    if crate::sched::spawn(watch, 0).is_err() {
        crate::kprintln!("ahci: no watch thread; a failed or stalled command will not be recovered");
    }
}

/// When the watch thread wakes at `now` at the latest: slot 0's command's deadline, or a
/// deadline's length on, whichever is first.
fn due_at(d: &AhciDisk, now: u64) -> u64 {
    let latest = now.saturating_add(COMMAND_DEADLINE_NS);
    let p = d.state.lock();
    if p.inflight.is_null() {
        return latest;
    }
    p.issued_at.saturating_add(COMMAND_DEADLINE_NS + 1).min(latest)
}

/// **Claim the port for recovery**, under its lock: a fault reported, slot 0's command past its
/// deadline at `now`, or `forced`. The command, `null` if none; `None` if there is nothing to do.
fn claim(d: &AhciDisk, now: u64, forced: bool) -> Option<*mut Irp> {
    let mut p = d.state.lock();
    let irp = p.inflight;
    let overdue = !irp.is_null() && now.saturating_sub(p.issued_at) > COMMAND_DEADLINE_NS;
    match p.fault {
        RECOVERING => return None,
        CLEAR if !overdue && !forced => return None,
        _ => {}
    }
    p.fault = RECOVERING;
    Some(irp)
}

/// **Recover the port after slot 0's command `irp` failed or overran** (AHCI 1.3.1 §6.2.2.1), then
/// fail `irp` and issue the next. **A port that will not stop** may still move `irp`'s data, so
/// `irp` is kept and the disk given up. Whether it recovered; the caller says so, as nothing here
/// may print under a host test.
fn recover(d: &AhciDisk, irp: *mut Irp) -> bool {
    let pb = d.port_base;
    if stop_port(pb).is_none() {
        let mut refused = give_up(&mut d.state.lock());
        loop {
            let next = refused.pop();
            if next.is_null() {
                return false;
            }
            // SAFETY: `next` was queued, never issued, so uniquely ours.
            unsafe { retire(d, next, true, 0, clock()) };
        }
    }
    pb.write32(PX_SERR, 0xFFFF_FFFF);
    pb.write32(PX_IS, 0xFFFF_FFFF);
    d.abar.write32(HBA_IS, 1 << d.port);
    if pb.read32(PX_TFD) & (TFD_BSY | TFD_DRQ) != 0 {
        comreset(pb);
    }
    start_port(pb);
    let issued_at = {
        let mut p = d.state.lock();
        p.inflight = core::ptr::null_mut();
        p.fault = CLEAR;
        p.issued_at
    };
    if !irp.is_null() {
        // SAFETY: `irp` was slot 0's, taken out of flight above with the port stopped.
        unsafe { retire(d, irp, true, 0, issued_at) };
    }
    drain_queue(d);
    true
}

/// **Give the disk up**, in `p`: refuse all from here, and hand back the queue.
fn give_up(p: &mut PortState) -> PendingRing {
    p.dead = true;
    core::mem::replace(&mut p.queue, PendingRing::new())
}

/// Say what a recovery did.
fn said(d: &AhciDisk, recovered: bool) {
    if recovered {
        crate::kprintln!("ahci: port {} recovered after a failed or stalled command", d.port);
    } else {
        crate::kprintln!("ahci: port {} would not stop after a failed command; the disk is given up", d.port);
    }
}

/// **Reset the link** (COMRESET, AHCI 1.3.1 §10.4.2), for a device still busy once stopped.
fn comreset(pb: Mmio) {
    pb.write32(PX_SCTL, (pb.read32(PX_SCTL) & !0xF) | 1);
    let start = clock();
    while clock().wrapping_sub(start) < 1_000_000 {
        core::hint::spin_loop();
    }
    pb.write32(PX_SCTL, pb.read32(PX_SCTL) & !0xF);
    let start = clock();
    while pb.read32(PX_SSTS) & 0xF != SSTS_DET_PRESENT && clock().wrapping_sub(start) <= POLL_TIMEOUT_NS {
        core::hint::spin_loop();
    }
    pb.write32(PX_SERR, 0xFFFF_FFFF);
}

/// [`BlockBackend::poll`] for the AHCI disk: complete the in-flight command by polling (boot path).
fn ahci_poll(_ctx: *mut ()) {
    poll_complete_inflight();
}

fn ahci_intr_dpc(_ctx: *mut ()) {
    if let Some(d) = AHCI.get() {
        crate::sched::signal_interrupt(&d.intr);
    }
}

fn ahci_drain_dpc(_ctx: *mut ()) {
    if let Some(d) = AHCI.get() {
        drain_queue(d);
    }
}

/// Poll the in-flight command to completion and complete its IRP — the boot self-test's fallback
/// when the IRQ does not fire. Returns `true` if a command was completed this way.
pub fn poll_complete_inflight() -> bool {
    let Some(d) = AHCI.get() else {
        return false;
    };
    let settled = poll_disk(d);
    if let Some((false, working)) = settled {
        said(d, working);
    }
    settled.is_some()
}

/// **Poll slot 0's command to its end, then run every completion queued**, a refusal's too. `None`
/// if there was none to wait for, else whether it finished and whether the port works.
fn poll_disk(d: &AhciDisk) -> Option<(bool, bool)> {
    let idle = {
        let p = d.state.lock();
        p.dead || p.inflight.is_null()
    };
    let settled = if idle {
        None
    } else {
        let finished = wait_command_polled(d, COMMAND_DEADLINE_NS);
        settle_polled(d, finished).map(|working| (finished, working))
    };
    crate::dpc::run_pending();
    settled
}

/// **Settle slot 0's command after a polled wait**: retired if it `finished`, else recovered and
/// failed, as the watch thread would. `None` if there was none, else whether the port works.
fn settle_polled(d: &AhciDisk, finished: bool) -> Option<bool> {
    let working = if finished {
        let (irp, err, moved, issued_at) = take_retired(d, &mut d.state.lock());
        if irp.is_null() {
            return None;
        }
        // SAFETY: `irp` is the retired request.
        unsafe { retire(d, irp, err, moved, issued_at) };
        true
    } else {
        recover(d, claim(d, clock(), true)?)
    };
    drain_queue(d);
    Some(working)
}

#[cfg(test)]
mod tests;
