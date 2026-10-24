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
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, Ordering};

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

/// [`AhciDisk::fault`]'s states.
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

/// State for the single supported AHCI disk, leaked to `'static`; the ISR reaches it through
/// [`AHCI`].
pub struct AhciDisk {
    abar: u64,      // mapped ABAR kernel-virtual base
    port_base: u64, // abar + 0x100 + port*0x80
    port: u32,
    cmd_list: DmaBuffer,  // 1 KiB command list; slot 0's header first
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
    /// When slot 0's command issued, for its time on the device and its deadline; written under
    /// [`pending`].
    issued_at: AtomicU64,
    counted: Counted,
    /// [`CLEAR`], [`FAULTED`] or [`RECOVERING`]: while not clear, the ISR retires nothing.
    fault: AtomicU8,
    /// What wakes the watch thread: a fault's DPC signals it. An `InterruptObject`, leaked.
    wake: *mut (),
    /// **The port would not stop**: its command is kept and every other request refused.
    dead: AtomicBool,
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

/// DPC that wakes the watch thread to recover the port after a fault.
static AHCI_FAULT_DPC: Dpc = Dpc::new(ahci_fault_dpc, core::ptr::null_mut());

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

mod bringup;
pub use bringup::init;

const DRIVER: &str = "ahci";

const NO_SATA_DISK: &str = "no SATA disk on any implemented port";

const OUT_OF_MEMORY: &str = "out of memory";

fn declined(why: &'static str) -> Outcome {
    Outcome::Declined { driver: DRIVER, why }
}

/// Stop the port: clear ST then FRE, waiting for CR/FR to clear. Whether they did: a command list
/// still running may still move a command's data.
fn stop_port(pb: u64) -> bool {
    let mut cmd = read32(pb, PX_CMD);
    cmd &= !CMD_ST;
    write32(pb, PX_CMD, cmd);
    wait_clear(pb, PX_CMD, CMD_CR);
    cmd = read32(pb, PX_CMD);
    cmd &= !CMD_FRE;
    write32(pb, PX_CMD, cmd);
    wait_clear(pb, PX_CMD, CMD_FR);
    read32(pb, PX_CMD) & (CMD_CR | CMD_FR) == 0
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
    if !wait_command_polled(disk, COMMAND_DEADLINE_NS) {
        // The command may be running yet: its buffer goes only once the port has stopped it.
        if !stop_port(disk.port_base) {
            core::mem::forget(data);
        }
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

    let cl = disk.cmd_list.virt() as *mut u32;
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

/// Poll slot 0 to completion within `timeout_ns`; `true` on success, `false` on timeout/error.
fn wait_command_polled(disk: &AhciDisk, timeout_ns: u64) -> bool {
    let start = crate::arch::Timer::read_ns();
    loop {
        let ci = read32(disk.port_base, PX_CI);
        if ci & 1 == 0 {
            break;
        }
        if read32(disk.port_base, PX_TFD) & TFD_ERR != 0 {
            return false;
        }
        if crate::arch::Timer::read_ns().wrapping_sub(start) > timeout_ns {
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
    // **A rescan and an eject are refused**, before the disk is touched: a SATA disk's table is read
    // once at boot; partitioning an internal disk is `nxinstall`'s, and a reboot follows.
    // SAFETY: `irp` is a live block IRP during submit.
    let op = unsafe { (*irp).op };
    if op == crate::io::irp::IrpOp::Rescan as u32 || op == crate::io::irp::IrpOp::Eject as u32 {
        // SAFETY: `irp` is uniquely owned during submit; completing it releases its waiter.
        unsafe {
            (*irp).set_completion(crate::syscall::error::KError::Unsupported as i32, 0);
            crate::dpc::enqueue(&(*irp).dpc);
        }
        return;
    }
    let disk = ctx as *mut AhciDisk;
    // SAFETY: `disk` is the live published disk.
    let d = unsafe { &*disk };
    if d.dead.load(Ordering::Acquire) {
        // SAFETY: `irp` is uniquely owned during submit; complete it with an error.
        unsafe {
            (*irp).set_completion(crate::syscall::error::KError::IoError as i32, 0);
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
unsafe fn retire(d: &AhciDisk, irp: *mut Irp, err: bool, moved: u32) {
    let ns = crate::arch::Timer::read_ns().saturating_sub(d.issued_at.load(Ordering::Relaxed));
    // SAFETY: as the contract says.
    unsafe {
        let (status, transferred) = completion(err, (*irp).length, moved);
        d.counted.record((*irp).op, status == IrpStatus::Success as i32, transferred, ns);
        (*irp).set_completion(status, transferred);
        crate::dpc::enqueue(&(*irp).dpc);
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

/// Slot 0 has retired: under the port lock, take the completed IRP out of flight (capturing its
/// error status and the bytes it moved) and mark the slot idle. Returns the IRP (`null` if already
/// idle), whether it errored, and its header's byte count. Does **not** issue the next command:
/// [`drain_queue`] does, from a DPC, so a fast pipelined completion cannot race the interrupt ack.
/// The caller completes the IRP outside any lock.
fn take_retired(d: &AhciDisk) -> (*mut Irp, bool, u32) {
    let _q = d.pending.lock();
    // Stopping the port clears PxCI, which must not read as the command finishing.
    if d.fault.load(Ordering::Acquire) != CLEAR {
        return (core::ptr::null_mut(), false, 0);
    }
    let retired = d.inflight.swap(core::ptr::null_mut(), Ordering::AcqRel);
    let err = read32(d.port_base, PX_TFD) & TFD_ERR != 0;
    // SAFETY: the command list is this disk's, mapped while it lives, and slot 0's header DW1 is
    // only rewritten by `build_command` for the next command, which cannot start under this lock.
    let moved = unsafe { (d.cmd_list.virt() as *const u32).add(1).read_volatile() };
    (retired, err, moved)
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
    on_interrupt(unsafe { &*disk });
}

/// [`isr`]'s body: acknowledge, report a fault to the watch thread, or retire slot 0.
fn on_interrupt(d: &AhciDisk) {
    let pxis = read32(d.port_base, PX_IS);
    write32(d.port_base, PX_IS, pxis);
    write32(d.abar, HBA_IS, 1 << d.port);

    // An error stops the port, PxCI keeping the command's bit until ST is cleared (§6.2.2.1).
    if pxis & PX_FATAL != 0 && d.fault.compare_exchange(CLEAR, FAULTED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        crate::dpc::enqueue(&AHCI_FAULT_DPC);
    }
    // If slot 0 has retired, complete the IRP and queue the drain DPC for the next waiting command.
    if read32(d.port_base, PX_CI) & 1 == 0 {
        let (irp, err, moved) = take_retired(d);
        if !irp.is_null() {
            // SAFETY: `irp` is the retired request; set status + queue its DPC.
            unsafe { retire(d, irp, err, moved) };
            crate::dpc::enqueue(&AHCI_DRAIN_DPC);
        }
    }
    crate::dpc::enqueue(&AHCI_INTR_DPC);
}

fn ahci_fault_dpc(_ctx: *mut ()) {
    let disk = AHCI.load(Ordering::Acquire);
    if !disk.is_null() {
        // SAFETY: live published state; its `wake` a live `InterruptObject`.
        unsafe { crate::sched::signal_interrupt((*disk).wake) };
    }
}

/// **The watch thread**: asleep until slot 0's command is due or a fault wakes it, never longer
/// than a deadline, so no command needs a wake of its own.
extern "C" fn watch(_arg: usize) {
    let disk = AHCI.load(Ordering::Acquire);
    if disk.is_null() {
        return;
    }
    // SAFETY: published once, never withdrawn.
    let d: &'static AhciDisk = unsafe { &*disk };
    while !d.dead.load(Ordering::Acquire) {
        let now = crate::arch::Timer::read_ns();
        // SAFETY: `d.wake` is the disk's `InterruptObject`, leaked with it.
        let woke = unsafe { crate::sched::wait_on(&[d.wake as usize], due_at(d, now), now) };
        if let crate::sched::WaitResult::Signaled(_) = woke {
            // SAFETY: as above.
            unsafe { crate::sched::interrupt_consume(d.wake) };
        }
        if let Some(irp) = claim(d, crate::arch::Timer::read_ns(), false) {
            said(d, recover(d, irp));
        }
    }
}

/// **Start the watch thread**, if a disk was claimed: after the scheduler and the APs are up.
pub fn start() {
    if AHCI.load(Ordering::Acquire).is_null() {
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
    if d.inflight.load(Ordering::Acquire).is_null() {
        return latest;
    }
    d.issued_at.load(Ordering::Relaxed).saturating_add(COMMAND_DEADLINE_NS + 1).min(latest)
}

/// **Claim the port for recovery**, under its lock: a fault reported, slot 0's command past its
/// deadline at `now`, or `forced`. The command, `null` if none; `None` if there is nothing to do.
fn claim(d: &AhciDisk, now: u64, forced: bool) -> Option<*mut Irp> {
    let _q = d.pending.lock();
    let irp = d.inflight.load(Ordering::Acquire);
    let overdue = !irp.is_null() && now.saturating_sub(d.issued_at.load(Ordering::Relaxed)) > COMMAND_DEADLINE_NS;
    match d.fault.load(Ordering::Acquire) {
        RECOVERING => return None,
        CLEAR if !overdue && !forced => return None,
        _ => {}
    }
    d.fault.store(RECOVERING, Ordering::Release);
    Some(irp)
}

/// **Recover the port after slot 0's command `irp` failed or overran** (AHCI 1.3.1 §6.2.2.1), then
/// fail `irp` and issue the next. **A port that will not stop** may still move `irp`'s data, so
/// `irp` is kept and the disk given up. Whether it recovered; the caller says so, as nothing here
/// may print under a host test.
fn recover(d: &AhciDisk, irp: *mut Irp) -> bool {
    let pb = d.port_base;
    if !stop_port(pb) {
        d.dead.store(true, Ordering::Release);
        loop {
            let next = d.pending.lock().pop();
            if next.is_null() {
                return false;
            }
            // SAFETY: `next` was queued, never issued, so uniquely ours.
            unsafe { retire(d, next, true, 0) };
        }
    }
    write32(pb, PX_SERR, 0xFFFF_FFFF);
    write32(pb, PX_IS, 0xFFFF_FFFF);
    write32(d.abar, HBA_IS, 1 << d.port);
    if read32(pb, PX_TFD) & (TFD_BSY | TFD_DRQ) != 0 {
        comreset(pb);
    }
    start_port(pb);
    {
        let _q = d.pending.lock();
        d.inflight.store(core::ptr::null_mut(), Ordering::Release);
        d.fault.store(CLEAR, Ordering::Release);
    }
    if !irp.is_null() {
        // SAFETY: `irp` was slot 0's, taken out of flight above with the port stopped.
        unsafe { retire(d, irp, true, 0) };
    }
    drain_queue(d);
    true
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
fn comreset(pb: u64) {
    write32(pb, PX_SCTL, (read32(pb, PX_SCTL) & !0xF) | 1);
    let start = crate::arch::Timer::read_ns();
    while crate::arch::Timer::read_ns().wrapping_sub(start) < 1_000_000 {
        core::hint::spin_loop();
    }
    write32(pb, PX_SCTL, read32(pb, PX_SCTL) & !0xF);
    let start = crate::arch::Timer::read_ns();
    while read32(pb, PX_SSTS) & 0xF != SSTS_DET_PRESENT
        && crate::arch::Timer::read_ns().wrapping_sub(start) <= POLL_TIMEOUT_NS
    {
        core::hint::spin_loop();
    }
    write32(pb, PX_SERR, 0xFFFF_FFFF);
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
    let finished = wait_command_polled(d, COMMAND_DEADLINE_NS);
    match settle_polled(d, finished) {
        Some(recovered) => {
            if !finished {
                said(d, recovered);
            }
            true
        }
        None => false,
    }
}

/// **Settle slot 0's command after a polled wait**: retired if it `finished`, else recovered and
/// failed, as the watch thread would. `None` if there was none, else whether the port works.
fn settle_polled(d: &AhciDisk, finished: bool) -> Option<bool> {
    let working = if finished {
        let (irp, err, moved) = take_retired(d);
        if irp.is_null() {
            return None;
        }
        // SAFETY: `irp` is the retired request.
        unsafe { retire(d, irp, err, moved) };
        true
    } else {
        recover(d, claim(d, crate::arch::Timer::read_ns(), true)?)
    };
    drain_queue(d);
    crate::dpc::run_pending();
    Some(working)
}

#[cfg(test)]
mod tests;
