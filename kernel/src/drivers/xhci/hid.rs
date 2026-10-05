//! **USB HID, bound** (Phase 6 Part B.2): a boot keyboard's or mouse's interrupt-IN endpoint
//! configured, polled by the DPC, and its reports turned into events on an input node.
//!
//! `docs/planning/phase-6-usb.md` § *Part B in detail*. The hub thread binds during enumeration
//! ([`bind`]); from then on the DPC owns the endpoint's polling ([`on_transfer`]), and the hub
//! thread comes back only to reset an endpoint that halted ([`recover`]) or to take a departing
//! device's endpoints away before its slot is disabled ([`unbind`]).
//!
//! **What a report means is `drivers::hid`'s**: this module fetches reports and hands them over.
//!
//! **An input node is one of [`MAX_NODES`] statics**, its `CharBackend` context the index, as the
//! PS/2 driver's two are: the node lives as long as the device table, which never drops one, and a
//! static is what lives that long without leaking a box. An index is never reused — the served
//! index above it is not either — until Part C retires nodes.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use super::context;
use super::hub::{self, DeviceMem, Failed};
use super::ring::{Producer, Slots, Trb, code, kind};
use super::{RING_TRBS, Xhci, write32};
use crate::arch::timer::ArchTimer;
use crate::drivers::hid::{keyboard, mouse};
use crate::drivers::input::{self, DRAIN_MAX, ParkedRead, ReadNow, Reader};
use crate::libkern::device::DeviceKind;
use crate::libkern::handle::KObjectType;
use crate::libkern::input::{EV_KEY, InputEvent, KEY_PRESS};
use crate::libkern::lockrank::LockRank;
use crate::libkern::IrqSpinLock;
use crate::mm::dma::DmaBuffer;
use crate::object::device_node::{CharBackend, DeviceNode, ResourceDescriptor};
use crate::object::ObjectRef;
use crate::syscall::error::KError;

/// USB input nodes a boot can publish.
pub const MAX_NODES: usize = 16;

/// Endpoints bound at once, across every device.
pub(super) const MAX_BOUND: usize = 16;

/// A third halt in a device's life leaves its endpoint stopped.
const HALTS_MAX: u8 = 3;

/// **Each USB input node's reader side**, by node index: the node's `CharBackend` context.
static NODES: [IrqSpinLock<Reader>; MAX_NODES] =
    [const { IrqSpinLock::new(LockRank::Leaf, Reader::new()) }; MAX_NODES];

/// Which nodes are keyboards, one bit each: what the hardware report drains.
static KEYBOARDS: AtomicU32 = AtomicU32::new(0);

/// The next node index never handed out.
static NEXT_NODE: AtomicUsize = AtomicUsize::new(0);

/// What a bound endpoint's reports are.
#[derive(Copy, Clone, Debug)]
pub(super) enum Decoder {
    /// A boot keyboard, and the last report kept.
    Keyboard { prev: [u8; keyboard::REPORT_LEN] },
    /// A mouse by its layout, and the buttons it holds.
    Mouse { layout: mouse::Layout, buttons: u8 },
}

/// **An endpoint the DPC polls.** Its ring and report buffer are the device's memory, which the hub
/// thread holds; this keeps where they are. [`unbind`] takes the entry out before that memory can
/// go.
pub(super) struct Bound {
    slot: u8,
    dci: u8,
    /// The interface it is, which a keyboard's lights are addressed to.
    interface: u8,
    node: usize,
    decoder: Decoder,
    ring: Producer,
    ring_virt: u64,
    ring_phys: u64,
    buf_phys: u64,
    buf_virt: u64,
    /// What each TRB asks for: the endpoint's maximum packet.
    len: u16,
    /// Halted, and the hub thread asked to reset it.
    halted: bool,
    /// Times it has halted, counted when the DPC sees each: the third leaves it stopped.
    halts: u8,
    /// A lights request timed out on the device's default endpoint, which may still hold it: no
    /// more lights are sent to this keyboard. Its keys go on arriving, on their own endpoint.
    lights_dead: bool,
}

/// The bound endpoints, for the DPC.
pub(super) struct Table {
    entries: [Option<Bound>; MAX_BOUND],
}

impl Table {
    /// An empty table.
    pub(super) fn new() -> Table {
        Table { entries: core::array::from_fn(|_| None) }
    }
}

/// A ring's TRBs at a virtual address: an interrupt endpoint's, written by the DPC, which holds the
/// address rather than the buffer the hub thread owns.
struct RawSlots(u64);

impl Slots for RawSlots {
    fn len(&self) -> usize {
        RING_TRBS
    }

    fn read(&self, i: usize) -> Trb {
        let p = self.0 as *const u32;
        let mut d = [0u32; 4];
        for (n, dw) in d.iter_mut().enumerate() {
            // SAFETY: `i < RING_TRBS`, and the ring at this address holds that many TRBs for as long
            // as its endpoint is bound.
            *dw = unsafe { core::ptr::read_volatile(p.add(i * 4 + n)) };
        }
        Trb(d)
    }

    fn write(&mut self, i: usize, trb: Trb) {
        let p = self.0 as *mut u32;
        for n in [0, 1, 2, 3] {
            // SAFETY: as for `read`; dword 3, the cycle bit, last.
            unsafe { core::ptr::write_volatile(p.add(i * 4 + n), trb.0[n]) };
        }
    }
}

/// One interface to bind: its number, what it is, and its interrupt-IN endpoint.
struct Candidate {
    interface: u8,
    is_keyboard: bool,
    endpoint: super::desc::Endpoint,
    /// Its report descriptor's length, from its HID descriptor; 0 when it has none.
    report_len: u16,
}

/// **Bind every boot keyboard and boot mouse interface of the device in `slot`** (Part B.2):
/// configure their endpoints, set the configuration, set each to boot protocol, ask keyboards to
/// report on change only, register a node for each, and start polling. `config` is its
/// configuration descriptor, `parent` its `UsbDevice` record. Logged; a failure is the device's or
/// the interface's (`phase-6-usb.md` § *Part B in detail*).
pub(super) fn bind(x: &Xhci, port: u8, slot: u8, speed_id: u8, mem: &mut DeviceMem, config: &[u8], parent: Option<u32>) {
    let mut found: [Option<Candidate>; 4] = [None, None, None, None];
    let mut n = 0;
    for h in super::desc::hid_interfaces(config) {
        let is_keyboard = match h.class {
            (0x03, 0x01, 0x01) => true,
            (0x03, 0x01, 0x02) => false,
            _ => continue,
        };
        let Some(endpoint) = h.endpoint else {
            crate::kprintln!("usb: port {port}: interface {} has no interrupt-IN endpoint; not bound", h.number);
            continue;
        };
        if n < found.len() {
            found[n] = Some(Candidate { interface: h.number, is_keyboard, endpoint, report_len: h.report_len });
            n += 1;
        }
    }
    if n == 0 {
        return;
    }
    let Some(config_value) = super::desc::configuration_value(config) else {
        return;
    };

    // **The device's steps**: memory for each endpoint, Configure Endpoint for all, then
    // SET_CONFIGURATION. A failure here ends the whole binding.
    let mut prepared: [Option<Prepared>; 4] = [None, None, None, None];
    let mut eps = [context::Interrupt::default(); 4];
    for (i, c) in found.into_iter().flatten().enumerate() {
        let (Ok(ring), Ok(buf)) = (DmaBuffer::alloc(RING_TRBS * 16), DmaBuffer::alloc(crate::mm::PAGE_SIZE)) else {
            crate::kprintln!("usb: port {port}: no memory for its endpoints; not bound");
            return;
        };
        let ring_virt = ring.virt() as u64;
        let ring_phys = ring.phys().as_u64();
        let producer = Producer::new(&mut RawSlots(ring_virt), ring_phys);
        eps[i] = context::Interrupt {
            dci: c.endpoint.dci(),
            max_packet: c.endpoint.max_packet,
            burst: c.endpoint.burst,
            interval: context::interval(speed_id, c.endpoint.interval),
            ring: ring_phys,
            cycle: producer.cycle(),
        };
        prepared[i] =
            Some(Prepared { c, producer, ring_virt, ring_phys, buf_phys: buf.phys().as_u64(), buf_virt: buf.virt() as u64 });
        // **The device's memory**: freed with it, and only after its slot is disabled.
        if mem.hid.try_push(ring).is_err() || mem.hid.try_push(buf).is_err() {
            crate::kprintln!("usb: port {port}: no memory for its endpoints; not bound");
            return;
        }
    }
    context::configure_endpoints(hub::input_bytes(&mut mem.input, x), x.layout, port, speed_id, &eps[..n]);
    if let Err(e) = hub::command(x, Trb::with_input(kind::CONFIGURE_ENDPOINT, mem.input.phys().as_u64(), slot)) {
        crate::kprintln!("usb: port {port}: Configure Endpoint failed: {e}; not bound");
        return;
    }
    if let Err(e) = hub::control_out(x, slot, mem, [0x00, 9, config_value, 0, 0, 0, 0, 0]) {
        crate::kprintln!("usb: port {port}: SET_CONFIGURATION failed: {e}; not bound");
        return;
    }

    // **Each interface's steps**: a stall ends that interface alone, and the others go on. **Any
    // other failure on the default endpoint ends the binding there** (PR #358 review): the request
    // may still be on the endpoint's ring, so nothing more is asked of it. That interface is not
    // bound, nor any after it, and those before it stay bound, on endpoints of their own — **and
    // take no lights** (Part B.5), since a lights request is the default endpoint's too.
    let mut in_doubt_at = false;
    for p in prepared.into_iter().flatten() {
        let c = &p.c;
        let what = if c.is_keyboard { "keyboard" } else { "mouse" };
        // **A mouse's layout, from its report descriptor** (Part B.3): report protocol when it
        // describes a plain mouse, boot protocol otherwise.
        let layout = if c.is_keyboard {
            None
        } else {
            match report_layout(x, slot, mem, c) {
                Ok(l) => l,
                Err(e) => {
                    crate::kprintln!(
                        "usb: port {port}: its {what}'s report descriptor did not read ({e}); \
                         not bound, nor anything after it"
                    );
                    in_doubt_at = true;
                    break;
                }
            }
        };
        if layout.is_some() {
            // A stall is passed over: a device is in report protocol after a reset anyway.
            if let Err(e) = interface_request(x, slot, mem, [0x21, 0x0B, 1, 0, c.interface, 0, 0, 0])
                && in_doubt(&e)
            {
                crate::kprintln!(
                    "usb: port {port}: its {what}'s SET_PROTOCOL failed ({e}); not bound, nor anything after it"
                );
                in_doubt_at = true;
                break;
            }
        } else if let Err(e) = interface_request(x, slot, mem, [0x21, 0x0B, 0, 0, c.interface, 0, 0, 0]) {
            if in_doubt(&e) {
                crate::kprintln!(
                    "usb: port {port}: its {what}'s SET_PROTOCOL failed ({e}); not bound, nor anything after it"
                );
                in_doubt_at = true;
                break;
            }
            crate::kprintln!("usb: port {port}: its {what} refused boot protocol ({e}); not bound");
            continue;
        }
        // A keyboard whose SET_IDLE leaves the endpoint in doubt is bound, being past its last
        // request, and is the last bound.
        let mut last = false;
        if c.is_keyboard
            && let Err(e) = interface_request(x, slot, mem, [0x21, 0x0A, 0, 0, c.interface, 0, 0, 0])
        {
            if in_doubt(&e) {
                crate::kprintln!("usb: port {port}: its keyboard's SET_IDLE failed ({e}); bound, and nothing after it");
                last = true;
                in_doubt_at = true;
            } else {
                // Harmless: an unchanged report decodes to nothing.
                crate::kprintln!("usb: port {port}: its keyboard refused SET_IDLE ({e}); bound anyway");
            }
        }
        let decoder = if c.is_keyboard {
            Decoder::Keyboard { prev: [0; keyboard::REPORT_LEN] }
        } else {
            Decoder::Mouse { layout: layout.unwrap_or(mouse::Layout::BOOT), buttons: 0 }
        };
        let kind = if c.is_keyboard { DeviceKind::Keyboard } else { DeviceKind::Mouse };
        let Some((node, served)) = publish(kind, parent) else {
            crate::kprintln!("usb: port {port}: no input node for its {what}; not bound");
            if last {
                break;
            }
            continue;
        };
        let bound = Bound {
            slot,
            dci: c.endpoint.dci(),
            interface: c.interface,
            node,
            decoder,
            ring: p.producer,
            ring_virt: p.ring_virt,
            ring_phys: p.ring_phys,
            buf_phys: p.buf_phys,
            buf_virt: p.buf_virt,
            len: c.endpoint.max_packet.max(layout.map_or(0, |l| report_bytes(&l))).min(REPORT_MAX as u16),
            halted: false,
            halts: 0,
            lights_dead: false,
        };
        if !start(x, bound) {
            crate::kprintln!("usb: port {port}: no room to poll its {what}; not bound");
            if last {
                break;
            }
            continue;
        }
        let how = match layout {
            Some(l) if l.wheel.is_some() => ", report protocol, with a wheel",
            Some(_) => ", report protocol, with no wheel",
            None if c.is_keyboard => ", boot protocol",
            None => ", boot protocol: its report descriptor does not describe a plain mouse",
        };
        crate::kprintln!("usb: port {port}: {what} at /dev/input/raw/{served}{how}");
        if last {
            break;
        }
    }
    if in_doubt_at {
        lights_dead(x, slot);
    }
}

/// **Whether a failure on the default endpoint leaves it in doubt**: any failure but a stall may
/// leave its request on the ring, to complete into the device's data page whenever it does, so
/// nothing more may be asked of that endpoint — `hub::control_in`'s rule, which enumeration keeps by
/// ending the device. A stall is the request's answer, and the endpoint is recovered after it.
fn in_doubt(e: &Failed) -> bool {
    !matches!(e, Failed::Code(code::STALL))
}

/// **A mouse's layout from its report descriptor** (Part B.3): `None` when it has none, or one that
/// does not describe a plain mouse, or the device stalled the request, which leaves boot protocol;
/// an error for any other failure, which may leave the transfer on the default endpoint's ring.
fn report_layout(x: &Xhci, slot: u8, mem: &mut DeviceMem, c: &Candidate) -> Result<Option<mouse::Layout>, Failed> {
    if c.report_len == 0 {
        return Ok(None);
    }
    match hub::read_interface_descriptor(x, slot, mem, super::desc::kind::REPORT, c.interface, c.report_len) {
        Ok(desc) => Ok(crate::drivers::hid::descriptor::mouse_layout(&desc[..c.report_len.min(desc.len() as u16) as usize])),
        Err(Failed::Code(code::STALL)) => {
            hub::recover_ep0(x, slot, mem)?;
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// The bytes a report in `layout` takes: its last field's end, and the report ID before it.
fn report_bytes(layout: &mouse::Layout) -> u16 {
    let end = [Some(layout.x), Some(layout.y), layout.wheel]
        .into_iter()
        .chain(layout.buttons)
        .flatten()
        .map(|f| f.offset as u32 + f.size as u32)
        .max()
        .unwrap_or(0);
    (end.div_ceil(8) + layout.report_id.is_some() as u32) as u16
}

/// An interface's endpoint with its memory placed, before the device is configured.
struct Prepared {
    c: Candidate,
    producer: Producer,
    ring_virt: u64,
    ring_phys: u64,
    buf_phys: u64,
    buf_virt: u64,
}

/// The longest report read: a TRB asks for the endpoint's maximum packet, up to this.
const REPORT_MAX: usize = 64;

/// A class request to an interface, with no data stage; a stall recovers the default endpoint and
/// is the request's failure.
fn interface_request(x: &Xhci, slot: u8, mem: &mut DeviceMem, request: [u8; 8]) -> Result<(), Failed> {
    match hub::control_out(x, slot, mem, request) {
        Err(Failed::Code(code::STALL)) => {
            hub::recover_ep0(x, slot, mem)?;
            Err(Failed::Code(code::STALL))
        }
        r => r,
    }
}

/// **A node for a USB keyboard or mouse**, registered under `parent`: its index here, and the index
/// `/dev/input/raw` serves it at. `None` when every node is taken or the table cannot hold it.
fn publish(kind: DeviceKind, parent: Option<u32>) -> Option<(usize, u32)> {
    let index = NEXT_NODE.fetch_add(1, Ordering::Relaxed);
    if index >= MAX_NODES {
        return None;
    }
    let backend = CharBackend { submit_read, submit_write: Some(submit_write), ctx: index as *mut () };
    let node = DeviceNode::try_new_char(ResourceDescriptor::ZERO, backend).ok()?;
    let node = crate::drivers::adopt(node, KObjectType::DeviceNode);
    let served = crate::device::register_input(node, kind, parent, "usb-hid")?;
    if kind == DeviceKind::Keyboard {
        KEYBOARDS.fetch_or(1 << index, Ordering::Relaxed);
    }
    Some((index, served))
}

/// Put `bound` in the table and queue its first TRB. Whether there was room.
fn start(x: &Xhci, mut bound: Bound) -> bool {
    let mut t = x.hid.lock();
    let Some(free) = t.entries.iter_mut().find(|e| e.is_none()) else {
        return false;
    };
    queue(x, &mut bound);
    *free = Some(bound);
    true
}

/// Queue `bound`'s TRB and ring its doorbell.
fn queue(x: &Xhci, b: &mut Bound) {
    b.ring.push(&mut RawSlots(b.ring_virt), Trb::normal(b.buf_phys, b.len as u32));
    write32(x.db, 4 * b.slot as u64, b.dci as u32);
}

/// **A Transfer Event for an endpoint other than the default one**, from the DPC: a report to
/// decode, or a halt to hand to the hub thread.
pub(super) fn on_transfer(x: &Xhci, trb: &Trb) {
    let now = crate::arch::Timer::read_ns();
    let mut events = [InputEvent::default(); keyboard::EVENTS_MAX];
    let mut halted = false;
    let (node, n) = {
        let mut t = x.hid.lock();
        let Some(b) = t.entries.iter_mut().flatten().find(|b| b.slot == trb.slot_id() && b.dci == trb.endpoint_id()) else {
            return;
        };
        match trb.completion_code() {
            code::SUCCESS | code::SHORT_PACKET => {
                let got = (b.len as u32).saturating_sub(trb.residual()) as usize;
                let mut report = [0u8; REPORT_MAX];
                let got = got.min(report.len());
                for (i, r) in report[..got].iter_mut().enumerate() {
                    // SAFETY: the report buffer is a page the controller finished writing with this
                    // event, and `got` is at most what this TRB asked for.
                    *r = unsafe { core::ptr::read_volatile((b.buf_virt as *const u8).add(i)) };
                }
                let n = decode(&mut b.decoder, &report[..got], now, &mut events);
                queue(x, b);
                (b.node, n)
            }
            _ => {
                if !b.halted {
                    b.halted = true;
                    b.halts = b.halts.saturating_add(1);
                    halted = true;
                }
                (b.node, 0)
            }
        }
    };
    // **The hub thread is woken with the table's lock let go**: waking takes the scheduler's
    // lock, which ranks above this leaf, and the lock-order tracker in every kernel this
    // project builds panics on the other order (PR #358 review, probed with a halt simulated).
    if halted {
        x.hid_halted.store(true, Ordering::Release);
        crate::sched::signal_interrupt(x.hub_wake.as_ptr());
    }
    if n == 0 {
        return;
    }
    for e in &events[..n] {
        if e.kind == EV_KEY && e.value == KEY_PRESS && e.code < crate::libkern::input::BTN_LEFT {
            input::note_key_press();
        }
    }
    let mut scratch = [0u8; DRAIN_MAX];
    let ready = {
        let mut r = NODES[node].lock();
        r.ring.push_group(&events[..n]);
        r.take_ready(&mut scratch, now)
    };
    if let Some((read, len)) = ready {
        input::deliver(&read, &scratch[..len]);
        NODES[node].lock().owe(read);
    }
}

/// Decode `report` with `decoder` into `out`, keeping what the next report is decoded against.
fn decode(decoder: &mut Decoder, report: &[u8], now: u64, out: &mut [InputEvent; keyboard::EVENTS_MAX]) -> usize {
    match decoder {
        Decoder::Keyboard { prev } => {
            let mut events = [InputEvent::default(); keyboard::EVENTS_MAX];
            match keyboard::decode(prev, report, now, &mut events) {
                Some(n) => {
                    prev.copy_from_slice(&report[..keyboard::REPORT_LEN]);
                    out[..n].copy_from_slice(&events[..n]);
                    n
                }
                None => 0,
            }
        }
        Decoder::Mouse { layout, buttons } => {
            let mut events = [InputEvent::default(); mouse::EVENTS_MAX];
            match mouse::decode(layout, *buttons, report, now, &mut events) {
                Some((n, held)) => {
                    *buttons = held;
                    out[..n].copy_from_slice(&events[..n]);
                    n
                }
                None => 0,
            }
        }
    }
}

/// **Reset every halted endpoint**, from the hub thread: Reset Endpoint, then Set TR Dequeue Pointer
/// to its enqueue point, then its TRB again. No `SYN_DROPPED`: the next report, decoded against the
/// last, says what changed (PR #357 review). A third halt leaves it stopped, as does a reset that
/// fails; either is the hub thread's for good, so a later pass, woken by another endpoint's halt,
/// passes it over.
///
/// **Nothing is printed with the table's lock held**: the serial port's lock ranks above this leaf
/// (PR #358 review). Each entry's outcome is settled under the lock and said after it.
pub(super) fn recover(x: &Xhci) {
    if !x.hid_halted.swap(false, Ordering::AcqRel) {
        return;
    }
    for i in 0..MAX_BOUND {
        let target = {
            let mut t = x.hid.lock();
            match t.entries[i].as_mut() {
                Some(b) if b.halted => {
                    // Taken: the DPC will not see it halt again, since nothing is queued on it.
                    b.halted = false;
                    Some((b.slot, b.dci, b.halts, b.ring_phys + b.ring.next_slot() as u64 * 16, b.ring.cycle()))
                }
                _ => None,
            }
        };
        let Some((slot, dci, halts, at, cycle)) = target else {
            continue;
        };
        if halts >= HALTS_MAX {
            crate::kprintln!("usb: slot {slot}: endpoint {dci} halted a third time; left stopped");
            continue;
        }
        let reset = hub::command(x, Trb::endpoint_command(kind::RESET_ENDPOINT, slot, dci))
            .and_then(|_| hub::command(x, Trb::set_dequeue(at, cycle, slot, dci)));
        let restarted = {
            let mut t = x.hid.lock();
            // Gone meanwhile: its device departed, and `unbind` took it.
            let Some(b) = t.entries[i].as_mut().filter(|b| b.slot == slot && b.dci == dci) else {
                continue;
            };
            if reset.is_ok() {
                queue(x, b);
            }
            reset
        };
        match restarted {
            Ok(_) => crate::kprintln!("usb: slot {slot}: endpoint {dci} halted and was reset"),
            Err(e) => crate::kprintln!("usb: slot {slot}: endpoint {dci} halted and did not reset: {e}; left stopped"),
        }
    }
}

/// **Take a departing device's endpoints out of the DPC's table**, before its slot is disabled and
/// its memory freed: from here the DPC cannot touch a buffer that is about to go.
pub(super) fn unbind(x: &Xhci, slot: u8) {
    let mut t = x.hid.lock();
    for e in t.entries.iter_mut() {
        if e.as_ref().is_some_and(|b| b.slot == slot) {
            *e = None;
        }
    }
}

/// [`CharBackend::submit_read`] for a USB input node, as PS/2's is.
fn submit_read(
    buffer: &ObjectRef,
    po: &ObjectRef,
    buf_offset: u64,
    _offset: u64,
    max_len: u64,
    ctx: *mut (),
) -> Result<(), KError> {
    let index = ctx as usize;
    if index >= MAX_NODES {
        return Err(KError::InvalidArgument);
    }
    let max_len = input::read_len(max_len)?;
    reclaim_completed();
    let now = crate::arch::Timer::read_ns();
    let mut tmp = [0u8; DRAIN_MAX];
    let found = {
        let mut r = NODES[index].lock();
        let found = r.read_now(&mut tmp[..max_len], now);
        if found == ReadNow::Empty {
            r.park(ParkedRead::new(po, buffer, buf_offset, max_len));
        }
        found
    };
    match found {
        ReadNow::Drained(n) => input::deliver_now(buffer, po, buf_offset, &tmp[..n]),
        ReadNow::Empty => {}
        ReadNow::Busy => return Err(KError::WouldBlock),
    }
    Ok(())
}

/// **A lights write waiting for the hub thread**, by node (Phase 6 Part B.5): its operation and the
/// lights.
static LIGHTS: IrqSpinLock<[Option<(ObjectRef, u8)>; MAX_NODES]> =
    IrqSpinLock::new(LockRank::Leaf, [const { None }; MAX_NODES]);

/// Which keyboards have taken lights once: what the log says, the first time.
static LIGHTS_TAKEN: AtomicU32 = AtomicU32::new(0);

/// [`CharBackend::submit_write`] for a USB keyboard's node: **its lights** (Phase 6 Part B.5), one
/// byte in HID's order, sent by the hub thread as a `SET_REPORT`, which owns the default endpoint. A
/// write to a keyboard that has left is refused at once, `PeerClosed`, and to one that takes no more
/// lights, `IoError`; one that comes while another waits replaces it, which is completed as done,
/// its lights being older than the ones that will be set.
fn submit_write(buffer: &ObjectRef, po: &ObjectRef, buf_offset: u64, len: u64, ctx: *mut ()) -> Result<(), KError> {
    let index = ctx as usize;
    if index >= MAX_NODES || KEYBOARDS.load(Ordering::Relaxed) & (1 << index) == 0 {
        return Err(KError::Unsupported);
    }
    let lights = input::lights_from(buffer, buf_offset, len)?;
    let x = super::XHCI.load(Ordering::Acquire);
    if x.is_null() {
        return Err(KError::PeerClosed);
    }
    // SAFETY: published once, never withdrawn.
    let x: &Xhci = unsafe { &*x };
    match x.hid.lock().entries.iter().flatten().find(|b| b.node == index).map(|b| b.lights_dead) {
        None => return Err(KError::PeerClosed),
        Some(true) => return Err(KError::IoError),
        Some(false) => {}
    }
    let superseded = LIGHTS.lock()[index].replace((po.clone(), lights));
    if let Some((old, _)) = superseded {
        crate::sched::complete_pending_op(old.as_ptr(), 0, 1);
        drop(old);
    }
    x.hid_lights.store(true, Ordering::Release);
    crate::sched::signal_interrupt(x.hub_wake.as_ptr());
    Ok(())
}

/// Where a keyboard's waiting lights go.
pub(super) enum LightsTo {
    /// To its slot's default endpoint, addressed to its interface.
    Send { slot: u8, interface: u8 },
    /// Nowhere: a request on its default endpoint is in doubt, so it takes no more lights.
    Dead,
    /// Nowhere: it has left.
    Gone,
}

/// **The lights waiting for node `node`**, and where they go. From the hub thread.
pub(super) fn take_lights(x: &Xhci, node: usize) -> Option<(ObjectRef, u8, LightsTo)> {
    let (po, lights) = LIGHTS.lock()[node].take()?;
    let to = match x.hid.lock().entries.iter().flatten().find(|b| b.node == node) {
        None => LightsTo::Gone,
        Some(b) if b.lights_dead => LightsTo::Dead,
        Some(b) => LightsTo::Send { slot: b.slot, interface: b.interface },
    };
    Some((po, lights, to))
}

/// Whether the hub thread has lights to send, clearing the flag.
pub(super) fn lights_waiting(x: &Xhci) -> bool {
    x.hid_lights.swap(false, Ordering::AcqRel)
}

/// **A lights request that timed out** on `slot`'s default endpoint: its keyboards take no more.
pub(super) fn lights_dead(x: &Xhci, slot: u8) {
    for b in x.hid.lock().entries.iter_mut().flatten().filter(|b| b.slot == slot) {
        b.lights_dead = true;
    }
}

/// Say the first time keyboard `node` took its lights.
pub(super) fn lights_taken(node: usize) -> bool {
    LIGHTS_TAKEN.fetch_or(1 << node, Ordering::Relaxed) & (1 << node) == 0
}

/// **The `SET_REPORT` that sets a keyboard's lights** (HID 1.11 §7.2.2): a class request to
/// `interface`, for an Output report — type 2, in `wValue`'s high byte — with no report ID, one
/// byte long. The byte is the node's as written: HID's order is the wire format's.
pub(super) const fn set_lights_request(interface: u8) -> [u8; 8] {
    [0x21, 0x09, 0x00, 0x02, interface, 0, 1, 0]
}

/// Drop every read the DPC has finished with. **Thread context only**: from `sched::reap_pending`,
/// and before a read parks.
pub fn reclaim_completed() {
    let used = NEXT_NODE.load(Ordering::Relaxed).min(MAX_NODES);
    for node in NODES.iter().take(used) {
        let owed = node.lock().take_owed();
        drop(owed);
    }
}

/// Throw away every USB keyboard's waiting events, as the PS/2 driver's `drain_keyboard` does: for
/// the hardware report's page turns.
pub fn drain_keyboards() {
    let now = crate::arch::Timer::read_ns();
    let keyboards = KEYBOARDS.load(Ordering::Relaxed);
    let mut scratch = [0u8; DRAIN_MAX];
    for (i, node) in NODES.iter().enumerate() {
        if keyboards & (1 << i) != 0 {
            while node.lock().ring.drain_into(&mut scratch, now) > 0 {}
        }
    }
}

const _: () = assert!(MAX_NODES <= 32, "KEYBOARDS has a bit per node");

#[cfg(test)]
mod tests {
    use super::*;

    /// **Only a stall leaves the default endpoint to be asked again** (PR #358 review): a stall is
    /// the request's answer, and every other failure may have left the request on the ring — a
    /// transaction error (4) and babble (3) halt the endpoint mid-transfer, and a timeout says
    /// nothing at all.
    #[test]
    fn only_a_stall_leaves_the_default_endpoint_to_ask_again() {
        assert!(!in_doubt(&Failed::Code(code::STALL)));
        for e in [Failed::Timeout, Failed::Code(4), Failed::Code(3), Failed::NoMemory, Failed::Device("short")] {
            assert!(in_doubt(&e), "{e}");
        }
    }

    /// **Each field where HID puts it** (HID 1.11 §7.2, USB 2.0 §9.3), read back as a device would.
    /// QEMU's keyboard reads none of them — it takes any `SET_REPORT` as its lights — so a swapped
    /// `wValue` would pass every gate and reach a real keyboard as an Input report with ID 2.
    #[test]
    fn the_lights_request_is_an_output_report_to_its_interface() {
        let r = set_lights_request(3);
        assert_eq!(r[0], 0b0_01_00001, "host to device, class, to an interface");
        assert_eq!(r[1], 0x09, "SET_REPORT");
        let w_value = u16::from_le_bytes([r[2], r[3]]);
        assert_eq!(w_value >> 8, 2, "an Output report");
        assert_eq!(w_value & 0xFF, 0, "no report ID: a boot keyboard has none");
        assert_eq!(u16::from_le_bytes([r[4], r[5]]), 3, "the interface");
        assert_eq!(u16::from_le_bytes([r[6], r[7]]), 1, "one byte");
    }
}
