//! **The hub thread** (Phase 6 Part A.2): the root hub's ports, enumerated.
//!
//! A kernel thread from `sched::spawn`, and the first long-lived one to block in `sched::wait_on`:
//! on a command's or a transfer's `PendingOperation`, which the DPC completes; on the port-change
//! `InterruptObject`, which the DPC signals; or on nothing until a deadline, to sleep. Enumeration
//! is a sequence with waits between its steps, and a thread lets it read top to bottom.
//!
//! **Coldplug is the first round of hot-plug.** The thread takes the USB 2 debounce once for every
//! port, logs each connected port's state, then enumerates them one at a time — only one device may
//! answer at address 0 — and completes the round the boot waits on. Then it sleeps until a port
//! changes, and handles the change the same way.
//!
//! **Per device**: a USB 2 port is reset (a USB 3 one is enabled by link training), a slot is
//! enabled, the device is addressed, its default endpoint's packet size is read and evaluated, its
//! device and configuration descriptors and its strings are read, and it is matched against the
//! class table — **logged, and not configured**: the class driver that binds does that (Parts B
//! and D). Every command and transfer has a deadline; a device that misses one is logged, and its
//! slot disabled. A command that goes unanswered leaves the command ring in doubt, so nothing more
//! is asked of it.
//!
//! A disconnect disables the device's slot and frees its memory. Departures in the registry are
//! Part C's.

use core::sync::atomic::Ordering;

use super::context::{self, DCI_EP0, speed};
use super::desc;
use super::ring::{Producer, Trb, code, kind};
use super::{Awaited, CommandRing, DmaSlots, PoPtr, Waiting, Xhci, XHCI, read32, write32};
use crate::arch::timer::ArchTimer;
use crate::libkern::handle::KObjectType;
use crate::libkern::KVec;
use crate::libkern::block::MAX_DEVICE_NAME;
use crate::libkern::printable::Printable;
use crate::mm::dma::DmaBuffer;
use crate::object::{ObjectRef, PendingOperation};

/// How long the boot waits for the first round.
pub const FIRST_ROUND_NS: u64 = 2_000_000_000;

/// USB 2.0 §7.1.7.3: 100 ms from a connection before the port is reset.
const DEBOUNCE_NS: u64 = 100_000_000;
/// A port reset's completion.
const RESET_NS: u64 = 500_000_000;
/// USB 2.0 §7.1.7.5: 10 ms after a reset before the device is spoken to.
const RECOVERY_NS: u64 = 10_000_000;
/// A USB 3 port's link to train and enable after a connection.
const LINK_NS: u64 = 100_000_000;
/// A command's completion, and a control transfer's.
const COMMAND_NS: u64 = 1_000_000_000;
const TRANSFER_NS: u64 = 1_000_000_000;

// PORTSC (xHCI 1.2 §5.4.8), at the operational registers' `0x400 + 0x10 × (port − 1)`.
const PORTSC: u64 = 0x400;
const PORT_CCS: u32 = 1 << 0;
const PORT_PED: u32 = 1 << 1;
const PORT_PR: u32 = 1 << 4;
const PORT_CSC: u32 = 1 << 17;
const PORT_PRC: u32 = 1 << 21;
/// The change bits, each cleared by writing it as one.
const PORT_CHANGES: u32 = 0x7F << 17;
/// The bits a write must carry back so as to change nothing else: the read-only ones and the
/// ones that keep their value (Linux's `XHCI_PORT_RO | XHCI_PORT_RWS`). Writing `PED` as one would
/// disable the port, and a change bit as one would clear it.
const PORT_KEEP: u32 = (1 << 0) | (1 << 3) | (0xF << 10) | (1 << 30) | (0xF << 5) | (1 << 9) | (0x3 << 14) | (0x7 << 25);

/// Why a device was not enumerated, or a step did not complete.
#[derive(Copy, Clone, Debug)]
enum Failed {
    /// The controller completed it with this code.
    Code(u8),
    /// No answer within its bound.
    Timeout,
    /// Memory ran out.
    NoMemory,
    /// The device said something that does not read.
    Device(&'static str),
}

impl core::fmt::Display for Failed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Failed::Code(c) => write!(f, "completion code {c}"),
            Failed::Timeout => f.write_str("no answer within its bound"),
            Failed::NoMemory => f.write_str("out of memory"),
            Failed::Device(why) => f.write_str(why),
        }
    }
}

/// A device on a port: its slot, and the memory the controller reads and writes for it.
struct Attached {
    slot: u8,
    _mem: DeviceMem,
}

/// One device's input and output contexts, its default endpoint's ring, and a page for what its
/// control transfers read.
struct DeviceMem {
    input: DmaBuffer,
    output: DmaBuffer,
    ep0: DmaBuffer,
    ring: Producer,
    data: DmaBuffer,
}

/// The thread.
pub(super) extern "C" fn main(_arg: usize) {
    let x = XHCI.load(Ordering::Acquire);
    if x.is_null() {
        return;
    }
    // SAFETY: published once, never withdrawn.
    let x: &'static Xhci = unsafe { &*x };
    let mut attached: KVec<Option<Attached>> = KVec::new();
    if attached.try_reserve(x.max_ports as usize + 1).is_err() {
        crate::kprintln!("usb: no memory for the hub thread's ports; nothing is enumerated");
        crate::sched::complete_pending_op(x.first_round.as_ptr(), 0, 0);
        return;
    }
    while attached.push_within_capacity(None).is_ok() {}

    // **The first round.** One debounce for every port, then each connected one in turn.
    let start = crate::arch::Timer::read_ns();
    sleep(DEBOUNCE_NS);
    let mut connected = 0;
    for port in 1..=x.max_ports {
        let s = portsc(x, port);
        if s & PORT_CCS != 0 {
            connected += 1;
            crate::kprintln!(
                "xhci: port {} at start: connected, {}{}",
                port,
                speed::name(speed_of(s)),
                if s & PORT_PED != 0 { ", enabled" } else { "" }
            );
        }
    }
    crate::kprintln!("xhci: {} of {} ports connected at start", connected, x.max_ports);
    let mut found = 0;
    for port in 1..=x.max_ports {
        if portsc(x, port) & PORT_CCS != 0 && arrive(x, port, &mut attached) {
            found += 1;
        }
    }
    crate::kprintln!(
        "usb: first round: {} device(s) in {} ms",
        found,
        crate::arch::Timer::read_ns().wrapping_sub(start) / 1_000_000
    );
    crate::sched::complete_pending_op(x.first_round.as_ptr(), 0, found);

    // **Then every later change**, woken by the DPC. The wake latches, so changes that land while
    // a device is being enumerated are found by the scan after it.
    loop {
        if matches!(
            crate::sched::wait_on(&[x.hub_wake.as_ptr() as usize], u64::MAX, 0),
            crate::sched::WaitResult::Signaled(_)
        ) {
            crate::sched::interrupt_consume(x.hub_wake.as_ptr());
        } else {
            sleep(1_000_000);
        }
        for port in 1..=x.max_ports {
            let s = portsc(x, port);
            if s & PORT_CSC == 0 {
                continue;
            }
            if s & PORT_CCS != 0 && attached[port as usize].is_none() {
                sleep(DEBOUNCE_NS);
                if portsc(x, port) & PORT_CCS != 0 {
                    arrive(x, port, &mut attached);
                } else {
                    clear_changes(x, port);
                }
            } else if s & PORT_CCS == 0 {
                clear_changes(x, port);
                depart(x, port, &mut attached);
            } else {
                clear_changes(x, port);
            }
        }
    }
}

/// A device connected at `port`: enumerate it, and keep it if that went through. Whether it did.
fn arrive(x: &Xhci, port: u8, attached: &mut KVec<Option<Attached>>) -> bool {
    clear_changes(x, port);
    if x.wedged.load(Ordering::Relaxed) {
        crate::kprintln!("usb: port {port}: not enumerated: the command ring went unanswered earlier");
        return false;
    }
    match enumerate(x, port) {
        Ok(dev) => {
            attached[port as usize] = Some(dev);
            true
        }
        Err(()) => false,
    }
}

/// The device at `port` left: disable its slot and free what the controller had of it.
fn depart(x: &Xhci, port: u8, attached: &mut KVec<Option<Attached>>) {
    let Some(dev) = attached[port as usize].take() else {
        return;
    };
    let freed = command(x, Trb::disable_slot(dev.slot)).is_ok();
    set_slot_context(x, dev.slot, 0);
    if freed {
        crate::kprintln!("usb: port {port}: disconnected; slot {} disabled", dev.slot);
    } else {
        crate::kprintln!("usb: port {port}: disconnected; slot {} did not disable", dev.slot);
    }
}

/// **Enumerate the device at `port`**, logging the step that fails if one does. On failure its
/// slot, if it got one, is disabled again.
fn enumerate(x: &Xhci, port: u8) -> Result<Attached, ()> {
    if !enable_port(x, port) {
        return Err(());
    }
    let speed_id = speed_of(portsc(x, port));
    let slot = match command(x, Trb::of_kind(kind::ENABLE_SLOT)) {
        Ok(slot) if slot != 0 => slot,
        Ok(_) => {
            crate::kprintln!("usb: port {port}: Enable Slot named no slot");
            return Err(());
        }
        Err(e) => {
            crate::kprintln!("usb: port {port}: Enable Slot failed: {e}");
            return Err(());
        }
    };
    match address_and_read(x, port, slot, speed_id) {
        Ok(mem) => Ok(Attached { slot, _mem: mem }),
        Err((step, e)) => {
            crate::kprintln!("usb: port {port}: {step} failed: {e}");
            let _ = command(x, Trb::disable_slot(slot));
            set_slot_context(x, slot, 0);
            Err(())
        }
    }
}

/// Address the device in `slot` and read what it is. The step that failed, and why, if one did.
fn address_and_read(x: &Xhci, port: u8, slot: u8, speed_id: u8) -> Result<DeviceMem, (&'static str, Failed)> {
    let alloc = |len| DmaBuffer::alloc(len).map_err(|_| ("allocating its contexts", Failed::NoMemory));
    let input = alloc(x.layout.input_len())?;
    let output = alloc(x.layout.output_len())?;
    let ep0 = alloc(super::RING_TRBS * 16)?;
    let data = alloc(crate::mm::PAGE_SIZE)?;
    let ring = Producer::new(&mut DmaSlots(&ep0), ep0.phys().as_u64());
    let mut mem = DeviceMem { input, output, ep0, ring, data };
    set_slot_context(x, slot, mem.output.phys().as_u64());

    let default = speed::default_max_packet0(speed_id);
    let ring_at = mem.ep0.phys().as_u64();
    let cycle = mem.ring.cycle();
    context::address_device(input_bytes(&mut mem.input, x), x.layout, port, speed_id, ring_at, cycle, default);
    command(x, Trb::with_input(kind::ADDRESS_DEVICE, mem.input.phys().as_u64(), slot))
        .map_err(|e| ("Address Device", e))?;

    // **The default endpoint's packet**, from the first eight bytes, which every packet size can
    // carry. At SuperSpeed it is an exponent.
    let raw = read_descriptor(x, slot, &mut mem, desc::kind::DEVICE, 0, 0, 8)
        .and_then(|b| desc::device_prefix(b).ok_or(Failed::Device("its first eight bytes are not a device descriptor")))
        .map_err(|e| ("reading its packet size", e))?;
    let max = desc::max_packet0(speed::is_super(speed_id), raw)
        .ok_or(("reading its packet size", Failed::Device("bMaxPacketSize0 is not one this speed allows")))?;
    if max != default {
        context::evaluate_ep0(input_bytes(&mut mem.input, x), x.layout, ring_at, cycle, max);
        command(x, Trb::with_input(kind::EVALUATE_CONTEXT, mem.input.phys().as_u64(), slot))
            .map_err(|e| ("Evaluate Context", e))?;
        // Said, because nothing else shows it: QEMU's controller reads the packet size only for a
        // debug message, so a gate cannot tell an evaluated endpoint from one left at the default.
        crate::kprintln!("usb: port {port}: its default endpoint takes {max}-byte packets, not {default}; evaluated");
    }

    let dev = read_descriptor(x, slot, &mut mem, desc::kind::DEVICE, 0, 0, 18)
        .and_then(|b| desc::device(b).ok_or(Failed::Device("its device descriptor does not read")))
        .map_err(|e| ("reading its device descriptor", e))?;
    let total = read_descriptor(x, slot, &mut mem, desc::kind::CONFIGURATION, 0, 0, 9)
        .and_then(|b| desc::configuration_total(b).ok_or(Failed::Device("its configuration does not read")))
        .map_err(|e| ("reading its configuration", e))?;
    let len = (total as usize).min(crate::mm::PAGE_SIZE) as u16;
    let mut config = [0u8; 512];
    let config_len = {
        let b = read_descriptor(x, slot, &mut mem, desc::kind::CONFIGURATION, 0, 0, len)
            .map_err(|e| ("reading its configuration", e))?;
        let n = b.len().min(config.len());
        config[..n].copy_from_slice(&b[..n]);
        n
    };
    let config = &config[..config_len];

    // **Its name**: the product string, and the serial beside it, as a disk's is its model and
    // serial. A device with no strings is named by its IDs in the log.
    let mut name = [0u8; MAX_DEVICE_NAME];
    let name_len = name_of(x, slot, &mut mem, &dev, &mut name);
    let matched = desc::class_match(&dev, config);
    let class = desc::record_class(&dev, config);
    if name_len > 0 {
        crate::kprintln!(
            "usb: port {}: {:04x}:{:04x} class {:02x}/{:02x}/{:02x}, {}, \"{}\": {}",
            port,
            dev.vendor,
            dev.product,
            class.0,
            class.1,
            class.2,
            speed::name(speed_id),
            Printable(&name[..name_len]),
            matched.says()
        );
    } else {
        crate::kprintln!(
            "usb: port {}: {:04x}:{:04x} class {:02x}/{:02x}/{:02x}, {}, no name: {}",
            port,
            dev.vendor,
            dev.product,
            class.0,
            class.1,
            class.2,
            speed::name(speed_id),
            matched.says()
        );
    }
    Ok(mem)
}

/// The product string, then ` (serial)` if it fits, into `out`. The length written; zero when the
/// device names itself with no string.
fn name_of(x: &Xhci, slot: u8, mem: &mut DeviceMem, dev: &desc::Device, out: &mut [u8]) -> usize {
    if dev.product_string == 0 && dev.serial == 0 {
        return 0;
    }
    let Some(lang) = read_descriptor(x, slot, mem, desc::kind::STRING, 0, 0, 255).ok().and_then(desc::first_language)
    else {
        return 0;
    };
    let mut n = 0;
    if dev.product_string != 0 {
        if let Ok(b) = read_descriptor(x, slot, mem, desc::kind::STRING, dev.product_string, lang, 255) {
            n = desc::string_into(b, out).unwrap_or(0);
        }
    }
    if dev.serial != 0 {
        let mut serial = [0u8; 32];
        let s = read_descriptor(x, slot, mem, desc::kind::STRING, dev.serial, lang, 255)
            .ok()
            .and_then(|b| desc::string_into(b, &mut serial))
            .unwrap_or(0);
        if s > 0 && n + s + 3 <= out.len() {
            if n > 0 {
                out[n..n + 2].copy_from_slice(b" (");
                out[n + 2..n + 2 + s].copy_from_slice(&serial[..s]);
                out[n + 2 + s] = b')';
                n += s + 3;
            } else {
                out[..s].copy_from_slice(&serial[..s]);
                n = s;
            }
        }
    }
    n
}

/// **A GET_DESCRIPTOR** of `kind`, index `index`, in language `lang`, up to `len` bytes, on the
/// device's default endpoint. The bytes, which are as many as asked: a device that sent fewer
/// left the rest zero, and the descriptor's own lengths say how much is real.
fn read_descriptor<'m>(x: &Xhci, slot: u8, mem: &'m mut DeviceMem, kind: u8, index: u8, lang: u16, len: u16) -> Result<&'m [u8], Failed> {
    let request = [
        0x80,
        6,
        index,
        kind,
        lang as u8,
        (lang >> 8) as u8,
        len as u8,
        (len >> 8) as u8,
    ];
    control_in(x, slot, mem, request, len)?;
    // SAFETY: `data` is a page, and `len` is at most a page; the transfer that wrote it is done.
    let bytes = unsafe { core::slice::from_raw_parts(mem.data.virt() as *const u8, len as usize) };
    Ok(bytes)
}

/// **A control transfer reading `len` bytes** into the device's data page: Setup, Data, Status,
/// and a wait for the Status stage's completion or an error on any stage.
fn control_in(x: &Xhci, slot: u8, mem: &mut DeviceMem, request: [u8; 8], len: u16) -> Result<(), Failed> {
    // SAFETY: `data` is a page the controller writes only during a transfer, and none is running.
    unsafe { core::ptr::write_bytes(mem.data.virt(), 0, len as usize) };
    let base = mem.ep0.phys().as_u64();
    let po = new_operation()?;
    // **Registered before the TRBs are written**, and the Status stage's address known first, so
    // no completion can come before the record it ends.
    let status_at = {
        let mut probe = mem.ring.next_slot();
        // Setup, then Data: the Status stage is two slots on, past the Link if the ring wraps.
        for _ in 0..2 {
            probe = if probe + 1 == super::RING_TRBS - 1 { 0 } else { probe + 1 };
        }
        base + probe as u64 * 16
    };
    *x.waiting.lock() = Some(Waiting { awaited: Awaited::Transfer { slot, status: status_at }, po: PoPtr(po.as_ptr()) });
    let mut slots = DmaSlots(&mem.ep0);
    mem.ring.push(&mut slots, Trb::setup(request, true));
    mem.ring.push(&mut slots, Trb::data_in(mem.data.phys().as_u64(), len as u32));
    let at = mem.ring.push(&mut slots, Trb::status(true));
    debug_assert_eq!(base + at as u64 * 16, status_at);
    write32(x.db, 4 * slot as u64, DCI_EP0 as u32);
    match wait(x, &po, TRANSFER_NS) {
        Some((code::SUCCESS, _)) => Ok(()),
        Some((c, _)) => Err(Failed::Code(c)),
        None => Err(Failed::Timeout),
    }
}

/// **A command, and its completion**: the slot it names. A command left unanswered marks the
/// controller wedged.
fn command(x: &Xhci, trb: Trb) -> Result<u8, Failed> {
    let po = new_operation()?;
    let sent = {
        let c = x.cmd.lock();
        c.mem.phys().as_u64() + c.ring.next_slot() as u64 * 16
    };
    *x.waiting.lock() = Some(Waiting { awaited: Awaited::Command { trb: sent }, po: PoPtr(po.as_ptr()) });
    {
        let mut c = x.cmd.lock();
        let CommandRing { mem, ring } = &mut *c;
        ring.push(&mut DmaSlots(mem), trb);
    }
    write32(x.db, 0, 0);
    match wait(x, &po, COMMAND_NS) {
        Some((code::SUCCESS, slot)) => Ok(slot as u8),
        Some((c, _)) => Err(Failed::Code(c)),
        None => {
            if !x.wedged.swap(true, Ordering::Relaxed) {
                crate::kprintln!("usb: a command went unanswered for a second; nothing more is asked of the controller");
            }
            Err(Failed::Timeout)
        }
    }
}

/// **Wait for `po`**, up to `bound_ns`: its completion code and result, or `None` if it did not
/// complete. On a timeout the waiting record is taken back, so the DPC cannot complete an operation
/// this thread is about to drop; if the DPC already took it, its completion is on the way, and is
/// waited for.
fn wait(x: &Xhci, po: &ObjectRef, bound_ns: u64) -> Option<(u8, u64)> {
    let now = crate::arch::Timer::read_ns();
    let done = matches!(
        crate::sched::wait_on(&[po.as_ptr() as usize], now + bound_ns, now),
        crate::sched::WaitResult::Signaled(_)
    );
    if !done {
        if x.waiting.lock().take().is_some() {
            return None;
        }
        while !matches!(crate::sched::wait_on(&[po.as_ptr() as usize], u64::MAX, 0), crate::sched::WaitResult::Signaled(_)) {
            sleep(1_000_000);
        }
    }
    let (status, result) = crate::sched::pending_op_completion(po.as_ptr());
    Some((status as u8, result))
}

/// A fresh operation for one wait.
fn new_operation() -> Result<ObjectRef, Failed> {
    let po = PendingOperation::try_new().map_err(|_| Failed::NoMemory)?;
    Ok(crate::drivers::adopt(po, KObjectType::PendingOperation))
}

/// **Enable `port`**: a USB 2 port is reset and given its recovery time; a USB 3 port is enabled
/// by its link training, which is waited for. Whether it is enabled, logged when not.
fn enable_port(x: &Xhci, port: u8) -> bool {
    if x.caps.major_of(port) == Some(3) {
        if poll(LINK_NS, || portsc(x, port) & PORT_PED != 0) {
            return true;
        }
        let s = portsc(x, port);
        crate::kprintln!("usb: port {port}: its USB 3 link did not enable (link state {})", (s >> 5) & 0xF);
        return false;
    }
    let s = portsc(x, port);
    write_portsc(x, port, (s & PORT_KEEP) | PORT_PR);
    if !poll(RESET_NS, || portsc(x, port) & PORT_PRC != 0) {
        crate::kprintln!("usb: port {port}: its reset did not complete");
        return false;
    }
    clear_changes(x, port);
    sleep(RECOVERY_NS);
    if portsc(x, port) & PORT_PED == 0 {
        crate::kprintln!("usb: port {port}: not enabled after its reset");
        return false;
    }
    true
}

fn portsc(x: &Xhci, port: u8) -> u32 {
    read32(x.op, PORTSC + 0x10 * (port as u64 - 1))
}

fn write_portsc(x: &Xhci, port: u8, val: u32) {
    write32(x.op, PORTSC + 0x10 * (port as u64 - 1), val);
}

/// Clear whichever of `port`'s change bits are set, changing nothing else.
fn clear_changes(x: &Xhci, port: u8) {
    let s = portsc(x, port);
    write_portsc(x, port, (s & PORT_KEEP) | (s & PORT_CHANGES));
}

/// A port's speed, from `PORTSC` bits 13:10.
fn speed_of(s: u32) -> u8 {
    ((s >> 10) & 0xF) as u8
}

/// Point slot `slot`'s entry of the device context base array at `output`, or clear it with 0.
fn set_slot_context(x: &Xhci, slot: u8, output: u64) {
    // SAFETY: the array holds an entry for every slot up to `max_slots`, and the controller only
    // names slots up to that.
    unsafe { core::ptr::write_volatile((x.dcbaa.virt() as *mut u64).add(slot as usize), output) };
}

/// The input context's bytes, cleared for a new command.
fn input_bytes<'a>(input: &'a mut DmaBuffer, x: &Xhci) -> &'a mut [u8] {
    let len = x.layout.input_len();
    let bytes = &mut input.as_mut_slice()[..len];
    bytes.fill(0);
    bytes
}

/// Sleep for `ns`: a wait on nothing with a deadline, so the CPU runs something else meanwhile.
fn sleep(ns: u64) {
    let now = crate::arch::Timer::read_ns();
    let _ = crate::sched::wait_on(&[], now + ns, now);
}

/// Look every millisecond until `done`, or `bound_ns` passes. Whether it was done.
fn poll(bound_ns: u64, mut done: impl FnMut() -> bool) -> bool {
    let start = crate::arch::Timer::read_ns();
    loop {
        if done() {
            return true;
        }
        if crate::arch::Timer::read_ns().wrapping_sub(start) > bound_ns {
            return done();
        }
        sleep(1_000_000);
    }
}
