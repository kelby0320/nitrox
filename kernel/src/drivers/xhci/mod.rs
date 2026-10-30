//! **The xHCI USB host controller**: one Tier 1 driver for QEMU's xHCI (the gates attach
//! `nec-usb-xhci`) and the laptop's Sunrise Point-LP controller, matched by PCI class `0C/03/30`.
//!
//! Bring-up is in [`init`], from `drivers::probe`, polled, before the scheduler runs:
//! 1. the function into D0, and bus mastering on;
//! 2. the firmware's hold released through USB Legacy Support, and its SMIs off;
//! 3. halt, then reset: `HCRST`, **a 1 ms pause before the next register access** (Linux does this
//!    for every Intel host, against a rare hang on that read), `HCRST` clear, Controller Not Ready
//!    clear;
//! 4. the device context base array, the scratchpad buffers the controller asks for, the command
//!    ring, and one event-ring segment on interrupter 0;
//! 5. MSI, the controller running, and **a No Op through the command ring, polled**, so the rings,
//!    the doorbell and the event ring are proved before anything depends on them;
//! 6. the interrupter on.
//!
//! **Every wait is bounded, and a failed one declines the function with its reason**: no USB is a
//! diagnosable failure, a hang on a machine with no serial port is not. A decline after the
//! controller started stops it first, or it would go on writing to memory this driver frees.
//!
//! The interrupt's DPC drains the event ring: a port change wakes [`hub`]'s thread, and a command's
//! or a transfer's completion ends the wait the thread is in. See `docs/planning/phase-6-usb.md`.

pub mod caps;
pub mod context;
pub mod desc;
pub mod hid;
mod hub;
pub mod ring;
pub mod storage;

pub use hub::FIRST_ROUND_NS;

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::arch::irq_install::ArchIrqInstall;
use crate::arch::timer::ArchTimer;
use crate::device::{Outcome, Signal};
use crate::dpc::Dpc;
use crate::libkern::handle::KObjectType;
use crate::libkern::lockrank::LockRank;
use crate::libkern::{IrqSpinLock, KBox, KVec, Published};
use crate::mm::dma::{DmaBuffer, Programmed, Quiesced};
use crate::mm::{PhysAddr, kvmap};
use crate::object::device_node::{DeviceNode, ResourceDescriptor};
use crate::object::{InterruptObject, ObjectRef, PendingOperation};
use ring::{Consumer, Producer, RingMem, Trb, code, kind};

/// PCI class, subclass and programming interface of an xHCI controller.
pub const PCI_CLASS: (u8, u8, u8) = (0x0C, 0x03, 0x30);

/// This driver's name in the device table's outcome lines.
const DRIVER: &str = "xhci";

// --- Registers (xHCI 1.2 §5) -------------------------------------------------

// Capability registers, from the start of BAR0.
const CAP_LENGTH_VERSION: u64 = 0x00; // CAPLENGTH in bits 7:0, HCIVERSION in 31:16
const HCSPARAMS1: u64 = 0x04;
const HCSPARAMS2: u64 = 0x08;
const HCCPARAMS1: u64 = 0x10;
const DBOFF: u64 = 0x14;
const RTSOFF: u64 = 0x18;

/// `HCCPARAMS1` bit 0: 64-bit addressing.
const HCC_AC64: u32 = 1 << 0;
/// `HCCPARAMS1` bit 2: contexts are 64 bytes, not 32.
const HCC_CSZ: u32 = 1 << 2;

// Operational registers, from `CAPLENGTH`.
const USBCMD: u64 = 0x00;
const USBSTS: u64 = 0x04;
const PAGESIZE: u64 = 0x08;
const CRCR: u64 = 0x18;
const DCBAAP: u64 = 0x30;
const CONFIG: u64 = 0x38;

const CMD_RUN: u32 = 1 << 0;
const CMD_HCRST: u32 = 1 << 1;
const CMD_INTE: u32 = 1 << 2;

const STS_HCH: u32 = 1 << 0;
const STS_EINT: u32 = 1 << 3;
const STS_CNR: u32 = 1 << 11;

// Interrupter 0, from the runtime registers' base plus `0x20`.
const IR0: u64 = 0x20;
const IMAN: u64 = 0x00;
const IMOD: u64 = 0x04;
const ERSTSZ: u64 = 0x08;
const ERSTBA: u64 = 0x10;
const ERDP: u64 = 0x18;

/// `IMAN` bit 0, Interrupt Pending (write one to clear), and bit 1, Interrupt Enable.
const IMAN_IP: u32 = 1 << 0;
const IMAN_IE: u32 = 1 << 1;
/// `ERDP` bit 3, Event Handler Busy (write one to clear).
const ERDP_EHB: u64 = 1 << 3;
/// The interrupter's moderation interval, in 250 ns units: 40 µs, Linux's default.
const IMOD_INTERVAL: u32 = 160;

// USB Legacy Support, at the extended capability's offset (xHCI 1.2 §7.1).
const LEGSUP_BIOS_OWNED: u32 = 1 << 16;
const LEGSUP_OS_OWNED: u32 = 1 << 24;
/// `USBLEGCTLSTS`, at the capability's `+4`: what is kept when the SMIs are turned off — reserved
/// bits a write must preserve — and the three status bits written as ones to clear them. Linux's
/// `XHCI_LEGACY_DISABLE_SMI` and `XHCI_LEGACY_SMI_EVENTS`.
const LEGCTL_KEEP: u32 = (0x7 << 1) | (0xFF << 5) | (0x7 << 17);
const LEGCTL_CLEAR_EVENTS: u32 = 0x7 << 29;

/// TRBs per ring: one page.
const RING_TRBS: usize = 256;

/// The firmware's release of the controller. Linux waits a second, then takes it.
const HANDOFF_NS: u64 = 1_000_000_000;
/// A halt takes at most 16 ms (xHCI 1.2 §5.4.2); a reset and readiness, longer.
const HALT_NS: u64 = 100_000_000;
const RESET_NS: u64 = 1_000_000_000;
const NO_OP_NS: u64 = 100_000_000;

/// The one controller this driver brought up, for the interrupt and its DPC.
static XHCI: Published<Xhci> = Published::new();

static XHCI_DPC: Dpc = Dpc::new(xhci_dpc, core::ptr::null_mut());

/// **The controller, once brought up**: for a storage submit to wake the hub thread through, with a
/// rescan for it. `None` before then, when no USB disk exists to submit to.
pub(super) fn controller() -> Option<&'static Xhci> {
    XHCI.get()
}

/// A brought-up controller. Leaked to `'static` once published, as AHCI's disk is: a Tier 1
/// controller lives as long as the kernel.
pub struct Xhci {
    /// The operational, runtime and doorbell registers' virtual bases.
    op: u64,
    rt: u64,
    db: u64,
    /// How many ports it has, numbered from 1.
    max_ports: u8,
    layout: context::Layout,
    caps: caps::ExtCaps,
    /// The device context base array, whose slot entries the hub thread writes, and the
    /// scratchpad buffers and their array, which are the controller's from here on.
    dcbaa: DmaBuffer,
    _scratch: Scratchpads,
    /// The command ring. The hub thread writes it; probe's No Op is its first TRB.
    cmd: IrqSpinLock<CommandRing>,
    /// The event ring and its segment table, which only the DPC reads after bring-up.
    events: IrqSpinLock<EventRing>,
    _erst: DmaBuffer,
    port_changes: AtomicU32,
    /// **What wakes the hub thread**: the DPC signals it on every port change. Latching, so a
    /// change that lands while the thread is busy is counted, not lost.
    hub_wake: ObjectRef,
    /// Completed by the hub thread when its first round is done, for the boot to wait on.
    first_round: ObjectRef,
    /// The one command or transfer the hub thread is waiting for, which the DPC completes.
    waiting: IrqSpinLock<Option<Waiting>>,
    /// A command went unanswered: the command ring may be stuck, and nothing more is asked of it.
    wedged: AtomicBool,
    /// The controller's own descriptor, whose address finds its function in the device table: the
    /// parent of every device it enumerates.
    pci: ResourceDescriptor,
    /// **The HID endpoints the DPC polls**, and whether one halted for the hub thread to reset.
    hid: IrqSpinLock<hid::Table>,
    hid_halted: AtomicBool,
    hid_lights: AtomicBool,
}

// SAFETY: shared across CPUs once published. Its register bases and descriptions never change,
// and every field that does is atomic or behind a lock of its own, but for DMA memory: the
// scratchpads and the event ring's segment table are never touched after bring-up, and the device
// context base array is written by the hub thread alone.
unsafe impl Sync for Xhci {}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Awaited {
    /// The completion of the command at this address.
    Command { trb: u64 },
    /// The default endpoint of `slot`: the Status stage at `status`, or an error on any stage of
    /// the transfer. A short Data stage is passed over, since its Status stage follows.
    Transfer { slot: u8, status: u64 },
}

struct Waiting {
    awaited: Awaited,
    po: PoPtr,
    /// **The bytes a short Data stage did not move**, from its own event, which comes before its
    /// Status stage's; the transfer completes with it. `0` for a whole one.
    short: u32,
}

impl Waiting {
    fn new(awaited: Awaited, po: PoPtr) -> Waiting {
        Waiting { awaited, po, short: 0 }
    }
}

/// A `PendingOperation`, by address: the waiting thread's reference pins it until the record is
/// taken back or the operation completes.
struct PoPtr(*mut ());

// SAFETY: the pointer names a kernel object whose reference count is atomic, and it is only
// completed through `sched::complete_pending_op`, which serialises on `SCHED`.
unsafe impl Send for PoPtr {}

struct CommandRing {
    mem: DmaBuffer,
    ring: Producer,
}

struct EventRing {
    mem: DmaBuffer,
    ring: Consumer,
}

/// The scratchpad buffers: the controller's own working memory, which it asks for and the driver
/// hands it through slot 0 of the device context base array.
struct Scratchpads {
    _array: Option<DmaBuffer>,
    _pages: KVec<DmaBuffer>,
}

#[inline]
fn read32(base: u64, off: u64) -> u32 {
    // SAFETY: `base + off` is inside the controller's mapped, uncached register window.
    unsafe { core::ptr::read_volatile((base + off) as *const u32) }
}

#[inline]
fn write32(base: u64, off: u64, val: u32) {
    // SAFETY: as `read32`.
    unsafe { core::ptr::write_volatile((base + off) as *mut u32, val) };
}

/// A 64-bit register, written as two dwords, low first (xHCI 1.2 §5.1).
fn write64(base: u64, off: u64, val: u64) {
    write32(base, off, val as u32);
    write32(base, off + 4, (val >> 32) as u32);
}

/// **Bring-up's clock.** A host test's moves on as it is read, where the host's reads 0.
#[cfg(not(test))]
fn clock() -> u64 {
    crate::arch::Timer::read_ns()
}

#[cfg(test)]
fn clock() -> u64 {
    std::thread_local!(static NOW: core::cell::Cell<u64> = const { core::cell::Cell::new(0) });
    NOW.with(|now| now.replace(now.get() + 1_000_000))
}

fn wait_for(bound_ns: u64, mut done: impl FnMut() -> bool) -> bool {
    let start = clock();
    loop {
        if done() {
            return true;
        }
        if clock().wrapping_sub(start) > bound_ns {
            return done();
        }
        core::hint::spin_loop();
    }
}

fn pause(ns: u64) {
    let start = clock();
    while clock().wrapping_sub(start) < ns {
        core::hint::spin_loop();
    }
}

fn declined(why: &'static str) -> Outcome {
    crate::kprintln!("xhci: declined: {why}");
    Outcome::Declined { driver: DRIVER, why }
}

/// Bring up the controller `controller` is, unless `usb_off` or one is already claimed.
pub fn init(controller: &ObjectRef, usb_off: bool) -> Outcome {
    if usb_off {
        return declined("usb=off on the command line");
    }
    if XHCI.get().is_some() {
        return declined("another xHCI controller is already claimed");
    }
    // SAFETY: `controller` pins a live `DeviceNode`.
    let dn: &DeviceNode = unsafe { &*(controller.as_ptr() as *const DeviceNode) };
    let desc = *dn.descriptor();
    let at = Address(&desc);

    // **Power first**: in D3hot the registers read as all ones (the laptop's was in D3 when Linux
    // was asked). Configuration space answers in D3, so the save and the raise go through it.
    let Ok(cfg) = crate::pci::Config::map(&desc) else {
        return declined("its configuration space could not be mapped");
    };
    let (power, saved) = crate::pci::power_up(&cfg);
    match power {
        crate::pci::Power::NoCapability => crate::kprintln!("xhci: {at}: no power management; D0"),
        crate::pci::Power::AlreadyD0 => crate::kprintln!("xhci: {at}: in D0 at probe"),
        crate::pci::Power::Raised { from, kept } => {
            // PCI PM 1.2 §5.4.1: 10 ms from D3hot before the function is touched.
            pause(10_000_000);
            if !kept {
                saved.restore(&cfg);
            }
            crate::kprintln!(
                "xhci: {at}: raised from D{from} to D0, waited 10 ms; its configuration {}",
                if kept { "was kept (No_Soft_Reset)" } else { "was reset, and restored" }
            );
        }
    }
    if crate::pci::enable_bus_master(&cfg) {
        crate::kprintln!("xhci: {at}: bus master already enabled by firmware");
    } else {
        crate::kprintln!("xhci: {at}: bus master enabled by the driver");
    }

    let bar = desc.bars[0];
    if bar.size == 0 {
        return declined("it has no BAR0");
    }
    let pages = bar.size.div_ceil(crate::mm::PAGE_SIZE as u64).max(1);
    // SAFETY: `bar.base` is the controller's MMIO register window, sized by PCI enumeration.
    let base = match unsafe { kvmap::map_mmio(PhysAddr(bar.base), pages) } {
        Ok(va) => va.as_u64() + (bar.base & (crate::mm::PAGE_SIZE as u64 - 1)),
        Err(_) => return declined("its registers could not be mapped"),
    };

    let lv = read32(base, CAP_LENGTH_VERSION);
    if lv == u32::MAX {
        return declined("its registers read as all ones");
    }
    let op = base + (lv & 0xFF) as u64;
    let version = lv >> 16;
    let hcs1 = read32(base, HCSPARAMS1);
    let hcs2 = read32(base, HCSPARAMS2);
    let hcc1 = read32(base, HCCPARAMS1);
    let db = base + (read32(base, DBOFF) & !0x3) as u64;
    let rt = base + (read32(base, RTSOFF) & !0x1F) as u64;
    let max_slots = (hcs1 & 0xFF) as u8;
    let max_ports = (hcs1 >> 24) as u8;
    let scratchpads = ((hcs2 >> 21) & 0x1F) << 5 | (hcs2 >> 27) & 0x1F;
    // The driver writes 64-bit addresses everywhere, of frames anywhere in memory: a controller
    // without them would DMA to the low half of each. Both machines have them.
    if hcc1 & HCC_AC64 == 0 {
        return declined("no 64-bit addressing (AC64 clear)");
    }
    let ext = caps::walk(|o| read32(base, o as u64), hcc1 >> 16, bar.size.min(u32::MAX as u64) as u32);

    // **The firmware lets go.** Without the handoff its SMM handler may go on driving the
    // controller under the kernel.
    match ext.legacy {
        None => crate::kprintln!("xhci: {at}: no legacy support capability; nothing to hand over"),
        Some(leg) => {
            let leg = leg as u64;
            let start = crate::arch::Timer::read_ns();
            write32(base, leg, read32(base, leg) | LEGSUP_OS_OWNED);
            let released = wait_for(HANDOFF_NS, || read32(base, leg) & LEGSUP_BIOS_OWNED == 0);
            if !released {
                // As Linux: the firmware is wrong, and the controller is taken anyway.
                write32(base, leg, read32(base, leg) & !LEGSUP_BIOS_OWNED);
            }
            let ctl = read32(base, leg + 4);
            write32(base, leg + 4, (ctl & LEGCTL_KEEP) | LEGCTL_CLEAR_EVENTS);
            let ms = crate::arch::Timer::read_ns().wrapping_sub(start) / 1_000_000;
            if released {
                crate::kprintln!("xhci: {at}: the firmware handed it over in {ms} ms; its SMIs are off");
            } else {
                crate::kprintln!("xhci: {at}: the firmware held it past {ms} ms; taken anyway, its SMIs off");
            }
        }
    }

    // **Not before it is ready** (xHCI 1.2 §4.2): no operational register may be written while
    // Controller Not Ready is set, which a function just raised from D3 can be — a reset written
    // then would land on nothing, and the poll after it pass at once.
    if !wait_for(RESET_NS, || read32(op, USBSTS) & STS_CNR == 0) {
        return declined("it stayed not ready after power-up");
    }

    // **Halt, then reset.** The pause comes between setting HCRST and the first read, which is the
    // read it guards: no bound on a loop can stand in for it.
    if read32(op, USBSTS) & STS_HCH == 0 {
        write32(op, USBCMD, read32(op, USBCMD) & !CMD_RUN);
        if !wait_for(HALT_NS, || read32(op, USBSTS) & STS_HCH != 0) {
            return declined("it did not halt");
        }
    }
    write32(op, USBCMD, CMD_HCRST);
    pause(1_000_000);
    if !wait_for(RESET_NS, || read32(op, USBCMD) & CMD_HCRST == 0) {
        return declined("its reset did not complete");
    }
    if !wait_for(RESET_NS, || read32(op, USBSTS) & STS_CNR == 0) {
        return declined("it stayed not ready after its reset");
    }
    if read32(op, PAGESIZE) & 1 == 0 {
        return declined("its page size is not 4 KiB");
    }

    // **What the controller needs from memory.** Every structure is a page or more from the buddy
    // allocator, so each is page-aligned and none crosses the 64 KiB boundary rings may not.
    let Ok(dcbaa) = DmaBuffer::alloc((max_slots as usize + 1) * 8) else {
        return declined(OUT_OF_MEMORY);
    };
    let Some(scratch) = scratchpads_for(scratchpads, &dcbaa) else {
        return declined(OUT_OF_MEMORY);
    };
    let (Ok(cmd_mem), Ok(ev_mem), Ok(erst)) =
        (DmaBuffer::alloc(RING_TRBS * 16), DmaBuffer::alloc(RING_TRBS * 16), DmaBuffer::alloc(16))
    else {
        return declined(OUT_OF_MEMORY);
    };
    let cmd_ring = Producer::new(&mut RingMem::of(&cmd_mem), cmd_mem.phys().as_u64());
    // The event ring's one segment: its base and size, in the table the controller reads.
    let e = erst.virt() as *mut u64;
    // SAFETY: `erst` is a page, which holds one 16-byte segment-table entry.
    unsafe {
        core::ptr::write_volatile(e, ev_mem.phys().as_u64());
        core::ptr::write_volatile(e.add(1), RING_TRBS as u64);
    }

    write32(op, CONFIG, max_slots as u32);
    write64(op, DCBAAP, dcbaa.phys().as_u64());
    write64(op, CRCR, cmd_mem.phys().as_u64() | cmd_ring.cycle() as u64);
    let ir = rt + IR0;
    write32(ir, IMOD, IMOD_INTERVAL);
    write32(ir, ERSTSZ, 1);
    write64(ir, ERDP, ev_mem.phys().as_u64());
    write64(ir, ERSTBA, erst.phys().as_u64());

    // **MSI before the controller runs**, so a failure here leaves nothing running to stop.
    let Some(msi) = crate::pci::read_msi(&cfg) else {
        return declined("no MSI capability");
    };
    // SAFETY: ring 0, after the interrupt controller; `isr` lives as long as the kernel, and does
    // nothing until `XHCI` is published below.
    let Some(msg) = (unsafe { crate::arch::IrqInstall::install_msi(isr) }) else {
        return declined("this CPU cannot be named in an MSI message");
    };
    // SAFETY: the handler for `msg.vector` is registered, so the device may raise it.
    if !unsafe { crate::pci::program_msi(&cfg, &msi, msg.address, msg.data) } {
        return declined("its MSI capability cannot hold the message address");
    }
    crate::pci::set_intx_disabled(&cfg, true);

    let (Ok(wake), Ok(first)) = (InterruptObject::try_new(), PendingOperation::try_new()) else {
        return declined(OUT_OF_MEMORY);
    };
    let x = Xhci {
        op,
        rt,
        db,
        max_ports,
        layout: context::Layout::new(hcc1 & HCC_CSZ != 0),
        caps: ext,
        dcbaa,
        _scratch: scratch,
        cmd: IrqSpinLock::new(LockRank::Leaf, CommandRing { mem: cmd_mem, ring: cmd_ring }),
        events: IrqSpinLock::new(LockRank::Leaf, EventRing { mem: ev_mem, ring: Consumer::new() }),
        _erst: erst,
        port_changes: AtomicU32::new(0),
        // SAFETY: a fresh `InterruptObject`, adopted under its own tag.
        hub_wake: unsafe { crate::drivers::adopt(wake, KObjectType::InterruptObject) },
        // SAFETY: a fresh `PendingOperation`, adopted under its own tag.
        first_round: unsafe { crate::drivers::adopt(first, KObjectType::PendingOperation) },
        waiting: IrqSpinLock::new(LockRank::Leaf, None),
        wedged: AtomicBool::new(false),
        pci: desc,
        hid: IrqSpinLock::new(LockRank::Leaf, hid::Table::new()),
        hid_halted: AtomicBool::new(false),
        hid_lights: AtomicBool::new(false),
    };
    // **Boxed before the controller runs**, so no failure after it starts can free what it writes
    // to while it writes: a box that cannot be had is declined here, with nothing running. Boxing
    // after the No Op let a failed allocation drop the rings with the controller still running.
    let Ok(boxed) = KBox::try_new(x) else {
        return declined(OUT_OF_MEMORY);
    };
    let boxed = Programmed::new(boxed);

    // **Run, and prove the rings with a No Op**, polled: interrupts are masked here, and the
    // interrupter is not on yet. Any port's change event that lands first is counted and passed.
    // A decline from here gives the box back only once the controller has stopped.
    write32(op, USBCMD, CMD_RUN);
    if !wait_for(HALT_NS, || read32(op, USBSTS) & STS_HCH == 0) {
        give_back(op, boxed);
        return declined("it did not start running");
    }
    let answer = no_op(&boxed);
    let Some(code) = answer else {
        give_back(op, boxed);
        return declined("its command ring did not answer a No Op");
    };
    if code != code::SUCCESS {
        give_back(op, boxed);
        crate::kprintln!("xhci: {at}: a No Op completed with code {code}");
        return declined("a No Op did not complete with Success");
    }

    // SAFETY: published, it is never freed; refused, it is `Programmed` again at once.
    let x = match XHCI.publish(unsafe { boxed.keep() }) {
        Ok(x) => x,
        Err(boxed) => {
            give_back(op, Programmed::new(boxed));
            return declined("another xHCI controller is already claimed");
        }
    };

    // **The interrupter on, without touching Interrupt Pending**, then the controller's interrupts,
    // then **one more drain**. An event posted after the No Op's drain set IP and Event Handler
    // Busy with the interrupter off, and raised nothing. Writing IP (write-one-to-clear) here would
    // throw that interrupt away, and with EHB still set nothing would raise another for the rest of
    // the boot, since only a DPC clears EHB and only an interrupt runs the DPC. So the pending
    // interrupt is kept, and the drain consumes what landed and clears EHB.
    write32(ir, IMAN, IMAN_IE);
    write32(op, USBCMD, read32(op, USBCMD) | CMD_INTE);
    drain_counting(x);

    crate::kprintln!(
        "xhci: {at} up: xHCI {}.{}, {} ports ({}), {} slots, {}-byte contexts, {} scratchpad(s)",
        version >> 8,
        version & 0xFF,
        max_ports,
        Ranges(&x.caps),
        max_slots,
        if hcc1 & HCC_CSZ != 0 { 64 } else { 32 },
        scratchpads
    );
    crate::kprintln!(
        "xhci: {at}: the command ring answers: a No Op completed; {} port change(s) waiting",
        x.port_changes.load(Ordering::Relaxed)
    );
    crate::kprintln!(
        "xhci: irq via MSI (vec {:#04x}, addr {:#x}, data {:#06x}, {}-bit cap at {:#04x})",
        msg.vector,
        msg.address,
        msg.data,
        if msi.addr64 { 64 } else { 32 },
        msi.off
    );
    Outcome::Claimed { driver: DRIVER, signal: Signal::Msi { vector: msg.vector } }
}

const OUT_OF_MEMORY: &str = "out of memory";

/// The scratchpad buffers the controller asked for, and their array in slot 0 of `dcbaa`.
/// `None` when memory ran out.
fn scratchpads_for(count: u32, dcbaa: &DmaBuffer) -> Option<Scratchpads> {
    if count == 0 {
        return Some(Scratchpads { _array: None, _pages: KVec::new() });
    }
    let array = DmaBuffer::alloc(count as usize * 8).ok()?;
    let mut pages: KVec<DmaBuffer> = KVec::new();
    pages.try_reserve(count as usize).ok()?;
    let a = array.virt() as *mut u64;
    for i in 0..count as usize {
        let page = DmaBuffer::alloc(crate::mm::PAGE_SIZE).ok()?;
        // SAFETY: `array` holds `count` entries.
        unsafe { core::ptr::write_volatile(a.add(i), page.phys().as_u64()) };
        pages.try_push(page).ok()?;
    }
    // SAFETY: `dcbaa` holds at least one entry; slot 0 is the scratchpad array's.
    unsafe { core::ptr::write_volatile(dcbaa.virt() as *mut u64, array.phys().as_u64()) };
    Some(Scratchpads { _array: Some(array), _pages: pages })
}

/// Stop a controller that was started, before a decline frees what it writes to: halt it, then
/// reset it, each within its bound. The proof it halted, or `None`: then it is not reset either,
/// since a reset while running is forbidden (xHCI 1.2 §5.4.1), and what it was given is kept.
fn stop(op: u64) -> Option<Quiesced> {
    write32(op, USBCMD, 0);
    if !wait_for(HALT_NS, || read32(op, USBSTS) & STS_HCH != 0) {
        return None;
    }
    write32(op, USBCMD, CMD_HCRST);
    pause(1_000_000);
    let _ = wait_for(RESET_NS, || read32(op, USBCMD) & CMD_HCRST == 0);
    // SAFETY: HCH read set: the controller halted, and a halted one touches no memory.
    Some(unsafe { Quiesced::assume() })
}

/// **Decline a controller that was started**: `mem`, what it was given, freed once it has stopped,
/// and kept for good if it did not.
fn give_back<T>(op: u64, mem: Programmed<T>) {
    if let Some(stopped) = stop(op) {
        drop(mem.release(stopped));
    }
}

/// Send a No Op through the command ring and poll the event ring for its completion code, or
/// `None` if no answer came within the bound. Port changes that arrive first are counted.
fn no_op(x: &Xhci) -> Option<u8> {
    let sent = {
        let mut c = x.cmd.lock();
        let CommandRing { mem, ring } = &mut *c;
        let slot = ring.push(&mut RingMem::of(mem), Trb::of_kind(kind::NO_OP_COMMAND));
        mem.phys().as_u64() + slot as u64 * 16
    };
    // Doorbell 0 is the command ring's, and its target is zero.
    write32(x.db, 0, 0);
    let mut answer = None;
    let done = wait_for(NO_OP_NS, || {
        drain(x, |trb| {
            if trb.kind() == kind::COMMAND_COMPLETION && trb.pointer() == sent {
                answer = Some(trb.completion_code());
            }
        });
        answer.is_some()
    });
    if done { answer } else { None }
}

/// **Drain the event ring at bring-up**, counting port changes and passing everything else, then
/// clear Event Handler Busy. Polled, before the scheduler runs: nothing here may wake a thread.
fn drain_counting(x: &Xhci) {
    drain(x, |_| {});
}

/// Drain the event ring, handing each event to `seen` after counting port changes, and write the
/// dequeue pointer back with Event Handler Busy cleared.
fn drain(x: &Xhci, mut seen: impl FnMut(&Trb)) {
    let mut ev = x.events.lock();
    let EventRing { mem, ring } = &mut *ev;
    while let Some(trb) = ring.next(&RingMem::of(mem)) {
        if trb.kind() == kind::PORT_STATUS_CHANGE {
            x.port_changes.fetch_add(1, Ordering::Relaxed);
        }
        seen(&trb);
    }
    write64(x.rt + IR0, ERDP, (mem.phys().as_u64() + ring.dequeue() as u64 * 16) | ERDP_EHB);
}

/// The interrupt: acknowledge it, and leave the event ring to the DPC.
extern "C" fn isr() {
    let Some(x) = XHCI.get() else {
        return;
    };
    write32(x.rt + IR0, IMAN, IMAN_IP | IMAN_IE);
    write32(x.op, USBSTS, STS_EINT);
    crate::dpc::enqueue(&XHCI_DPC);
}

/// Drain the event ring a batch at a time, telling the controller where each drain stopped, and
/// act on each event **with the ring's lock released**: completing an operation takes the
/// scheduler's lock, which no leaf lock may be held across. No allocation, and no freeing: a DPC
/// may do neither.
fn xhci_dpc(_ctx: *mut ()) {
    let Some(x) = XHCI.get() else {
        return;
    };
    const BATCH: usize = 16;
    loop {
        let mut batch = [Trb::default(); BATCH];
        let mut n = 0;
        {
            let mut ev = x.events.lock();
            let EventRing { mem, ring } = &mut *ev;
            while n < BATCH {
                let Some(trb) = ring.next(&RingMem::of(mem)) else { break };
                batch[n] = trb;
                n += 1;
            }
            let at = mem.phys().as_u64() + ring.dequeue() as u64 * 16;
            write64(x.rt + IR0, ERDP, at | ERDP_EHB);
        }
        for trb in &batch[..n] {
            on_event(x, trb);
        }
        if n < BATCH {
            break;
        }
    }
}

/// One event: a port change wakes the hub thread, and a completion ends the wait it is in.
fn on_event(x: &Xhci, trb: &Trb) {
    match trb.kind() {
        kind::PORT_STATUS_CHANGE => {
            x.port_changes.fetch_add(1, Ordering::Relaxed);
            // SAFETY: `hub_wake` is the controller's `InterruptObject`, held for its life.
            unsafe { crate::sched::signal_interrupt(x.hub_wake.as_ptr()) };
        }
        kind::COMMAND_COMPLETION => complete_if(
            x,
            |a| a == Awaited::Command { trb: trb.pointer() },
            trb.completion_code(),
            trb.slot_id() as u64,
        ),
        // **A bound endpoint's transfer**: the default endpoint's transfers are the hub thread's
        // waits; every other endpoint's is a storage device's or a HID one's.
        kind::TRANSFER_EVENT if trb.endpoint_id() != context::DCI_EP0 => {
            if !storage::on_transfer(x, trb) {
                hid::on_transfer(x, trb);
            }
        }
        kind::TRANSFER_EVENT => {
            let done =
                ep0_event(&mut x.waiting.lock(), trb.slot_id(), trb.pointer(), trb.completion_code(), trb.residual());
            if let Some((w, code, result)) = done {
                // SAFETY: the waiting thread's reference pins `w.po` until its operation completes.
                unsafe { crate::sched::complete_pending_op(w.po.0, code as i32, result) };
            }
        }
        _ => {}
    }
}

/// **An event on a default endpoint, against the waiting record**: the record taken, with the
/// code and result to complete it with, when the event ends its transfer. **A short Data stage's
/// own event** keeps the bytes it did not move on the record and ends nothing: its Status stage
/// follows, and completes the transfer with them. The Status stage's event, or an error on any
/// stage, ends it; an error's result is its own TRB's residual. An event for another slot, or
/// with no transfer waiting, changes nothing.
fn ep0_event(
    waiting: &mut Option<Waiting>,
    slot: u8,
    pointer: u64,
    code: u8,
    residual: u32,
) -> Option<(Waiting, u8, u64)> {
    let w = waiting.as_mut()?;
    let Awaited::Transfer { slot: awaited, status } = w.awaited else { return None };
    if awaited != slot {
        return None;
    }
    match code {
        code::SHORT_PACKET if pointer != status => {
            w.short = residual;
            None
        }
        code::SUCCESS if pointer != status => None,
        _ => {
            let w = waiting.take()?;
            let result = if pointer == status { w.short } else { residual };
            Some((w, code, result as u64))
        }
    }
}

/// **Take the waiting record if `matches` it, then complete its operation** with `code` and
/// `result`. Taken under the lock, so the thread can tell a record the DPC holds from one it may
/// take back; completed after it, since that takes the scheduler's lock.
fn complete_if(x: &Xhci, matches: impl Fn(Awaited) -> bool, code: u8, result: u64) {
    let taken = {
        let mut w = x.waiting.lock();
        if w.as_ref().is_some_and(|w| matches(w.awaited)) { w.take() } else { None }
    };
    if let Some(w) = taken {
        // SAFETY: the waiting thread's reference pins `w.po` until its operation completes.
        unsafe { crate::sched::complete_pending_op(w.po.0, code as i32, result) };
    }
}

/// **Start the hub thread**, if a controller was claimed. After the scheduler and the APs are up.
pub fn start() {
    if XHCI.get().is_none() {
        return;
    }
    if crate::sched::spawn(hub::main, 0).is_err() {
        crate::kprintln!("xhci: no memory for the hub thread; nothing will be enumerated");
    }
}

/// **Wait for the hub thread's first round**, up to `bound_ns`, so the hardware report lists what
/// is attached and the registry holds it before userspace reads it. A bound that passes is said;
/// whatever is still enumerating arrives later.
pub fn settle(bound_ns: u64) {
    let Some(x) = XHCI.get() else {
        return;
    };
    let now = crate::arch::Timer::read_ns();
    let po = x.first_round.as_ptr();
    // SAFETY: `x.first_round` pins the `PendingOperation` at `po` for the controller's life.
    let waited = unsafe { crate::sched::wait_on(&[po as usize], now + bound_ns, now) };
    if !matches!(waited, crate::sched::WaitResult::Signaled(_)) {
        crate::kprintln!(
            "usb: the first round was not done in {} ms; what is still enumerating arrives later",
            bound_ns / 1_000_000
        );
    }
}

/// A function's PCI address, as the log shows it.
struct Address<'a>(&'a ResourceDescriptor);

impl fmt::Display for Address<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.0.bus, self.0.dev, self.0.func)
    }
}

/// `USB 2: 1-8, USB 3: 9-16`: which ports speak which version.
struct Ranges<'a>(&'a caps::ExtCaps);

impl fmt::Display for Ranges<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for p in self.0.protocols.iter().flatten() {
            if !first {
                f.write_str(", ")?;
            }
            first = false;
            match p.count {
                0 => write!(f, "USB {}: none", p.major)?,
                n => write!(f, "USB {}: {}-{}", p.major, p.first, p.first as u16 + n as u16 - 1)?,
            }
        }
        if first {
            f.write_str("no supported protocol named")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A started controller declined is given back its memory only once it halted**: one that
    /// will not halt keeps it, and is not reset while it runs.
    #[test]
    fn a_declined_controller_frees_its_memory_only_once_halted() {
        crate::mm::test_support::init_global_heap();
        for halts in [false, true] {
            let mut regs = std::boxed::Box::new([0u32; 4]);
            let op = regs.as_mut_ptr() as u64;
            write32(op, USBSTS, if halts { STS_HCH } else { 0 });
            let freed = crate::mm::heap::frames_freed_by_this_thread();
            give_back(op, Programmed::new(DmaBuffer::alloc(crate::mm::PAGE_SIZE).unwrap()));
            let gave = crate::mm::heap::frames_freed_by_this_thread() - freed;
            assert_eq!(gave, halts as usize, "halts {halts}: its memory freed");
            assert_eq!(read32(op, USBCMD), if halts { CMD_HCRST } else { 0 }, "halts {halts}: reset");
        }
    }

    /// A transfer on `slot`'s default endpoint waiting for its Status stage at `status`.
    fn transfer(slot: u8, status: u64) -> Option<Waiting> {
        Some(Waiting::new(Awaited::Transfer { slot, status }, PoPtr(core::ptr::null_mut())))
    }

    /// **A short Data stage's residual is its transfer's result**: its own event ends nothing and
    /// the Status stage's carries it; an error on the Data stage ends there with its own residual.
    #[test]
    fn a_short_data_stage_ends_its_transfer_with_what_it_did_not_move() {
        let (data, status) = (0x1010, 0x1020);
        let mut w = transfer(3, status);
        let (_, code, result) = ep0_event(&mut w, 3, status, code::SUCCESS, 0).expect("the Status stage ends it");
        assert_eq!((code, result, w.is_none()), (code::SUCCESS, 0, true), "whole");

        let mut w = transfer(3, status);
        assert!(ep0_event(&mut w, 3, data, code::SHORT_PACKET, 64).is_none(), "the Data stage's own event");
        assert!(w.is_some(), "still waiting for the Status stage");
        let (_, code, result) = ep0_event(&mut w, 3, status, code::SUCCESS, 0).expect("the Status stage ends it");
        assert_eq!((code, result), (code::SUCCESS, 64), "short by a packet");

        let mut w = transfer(3, status);
        let (_, code, result) = ep0_event(&mut w, 3, data, code::STALL, 18).expect("an error ends it");
        assert_eq!((code, result), (code::STALL, 18));

        let mut w = transfer(3, status);
        assert!(ep0_event(&mut w, 4, status, code::SUCCESS, 0).is_none(), "another slot's");
        assert!(w.is_some());
        let mut c = Some(Waiting::new(Awaited::Command { trb: status }, PoPtr(core::ptr::null_mut())));
        assert!(ep0_event(&mut c, 3, status, code::SUCCESS, 0).is_none(), "a command's wait");
        assert!(ep0_event(&mut None, 3, status, code::SUCCESS, 0).is_none(), "nothing waiting");
    }
}
