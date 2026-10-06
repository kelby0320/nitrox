//! **USB HID, bound** (Phase 6 Part B.2): a boot keyboard's or mouse's interrupt-IN endpoint
//! configured, polled by the DPC, and its reports turned into events on an input node.
//!
//! `docs/planning/phase-6-usb.md` § *Part B in detail*. The hub thread binds during enumeration
//! ([`bind`]); from then on the DPC owns the endpoint's polling ([`on_transfer`]), and the hub
//! thread comes back only to reset an endpoint that halted ([`recover`]) or to let a departing
//! device's keyboards and mice go before its slot is disabled ([`depart`]).
//!
//! **What a report means is `drivers::hid`'s**: this module fetches reports and hands them over.
//!
//! **An input node's state is one of [`MAX_NODES`] slots in a static [`Nodes`]**, its
//! `CharBackend` context the slot and the slot's epoch: a static is what lives as long as the device
//! table, which never drops a node, without leaking a box. **A departed device's slot is given
//! back** (Phase 6 Part C) once its node has retired, and its epoch bumped then, so a handle to the
//! old node is refused rather than served the next device's ring. The served index above it is
//! never reissued.

use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};

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

/// A slot no node has had.
const FREE: u8 = 0;
/// A slot whose node is bound to a device.
const BOUND: u8 = 1;
/// A slot whose device has departed (Phase 6 Part C): its node reads what its ring holds and then
/// refuses, and the slot may be given to the next device.
const RETIRED: u8 = 2;

/// **The USB input nodes, by slot**: each slot's state, its epoch, its reader side and a lights write
/// waiting on it, and which slots hold keyboards. A node's `CharBackend` context names its slot and
/// the epoch it was made in ([`context`]).
///
/// One static in a boot, [`NODES`]. **A value so a host test can drive it** (PR #361 review): no
/// boot reuses a slot, which takes sixteen nodes bound at once, so a table of a test's own is where a
/// slot is taken, retired and taken again, and the node it had before read and written through.
struct Nodes {
    /// Each slot's state: [`FREE`], [`BOUND`] or [`RETIRED`]. Changed by the hub thread alone.
    states: [AtomicU8; MAX_NODES],
    /// **Each slot's epoch**, bumped when a retired slot is given to another device (Phase 6 Part
    /// C), so a handle to a node retired from a slot since reused is refused `PeerClosed`, rather
    /// than served the next device's ring.
    epochs: [AtomicU32; MAX_NODES],
    /// Each slot's reader side.
    readers: [IrqSpinLock<Reader>; MAX_NODES],
    /// **A lights write waiting for the hub thread**, by slot (Phase 6 Part B.5): its operation and
    /// the lights.
    lights: IrqSpinLock<[Option<(ObjectRef, u8)>; MAX_NODES]>,
    /// Which slots hold keyboards, one bit each: what takes lights, and what the hardware report
    /// drains.
    keyboards: AtomicU32,
}

const _: () = assert!(MAX_NODES <= 32, "`Nodes::keyboards` has a bit per slot");

/// The boot's USB input nodes.
static NODES: Nodes = Nodes::new();

/// A node's `CharBackend` context: its slot, and the slot's epoch when the node was made.
fn context(slot: usize, epoch: u32) -> *mut () {
    ((epoch as usize) << 8 | slot) as *mut ()
}

/// The slot and the epoch a context names.
fn slot_of(ctx: *mut ()) -> (usize, u32) {
    let c = ctx as usize;
    (c & 0xFF, (c >> 8) as u32)
}

/// **The slot to give a new node**: one no node has had, else one whose device has departed — the
/// first of either — or `None` when every slot is bound.
fn choose(states: &[u8; MAX_NODES]) -> Option<usize> {
    states.iter().position(|&s| s == FREE).or_else(|| states.iter().position(|&s| s == RETIRED))
}

impl Nodes {
    /// Every slot free.
    const fn new() -> Nodes {
        Nodes {
            states: [const { AtomicU8::new(FREE) }; MAX_NODES],
            epochs: [const { AtomicU32::new(0) }; MAX_NODES],
            readers: [const { IrqSpinLock::new(LockRank::Leaf, Reader::new()) }; MAX_NODES],
            lights: IrqSpinLock::new(LockRank::Leaf, [const { None }; MAX_NODES]),
            keyboards: AtomicU32::new(0),
        }
    }

    /// **Whether a context's node is still its slot's**: its epoch the slot's now, so the slot has
    /// not been given to another device since it was made (Phase 6 Part C).
    fn current(&self, ctx: *mut ()) -> bool {
        let (index, epoch) = slot_of(ctx);
        self.epochs.get(index).is_some_and(|e| e.load(Ordering::Acquire) == epoch)
    }

    /// **Take a slot for a new node** (Phase 6 Part C), a keyboard's or not: a free one, else a
    /// retired one, reset — its epoch first, so a stale handle's next read finds it changed under the
    /// node's lock, then its reader and any lights write still waiting on it. The slot and its epoch,
    /// for the node's context, or `None` when every slot is bound. From the hub thread.
    fn take(&self, keyboard: bool) -> Option<(usize, u32)> {
        let states: [u8; MAX_NODES] = core::array::from_fn(|i| self.states[i].load(Ordering::Acquire));
        let index = choose(&states)?;
        if states[index] == RETIRED {
            self.epochs[index].fetch_add(1, Ordering::AcqRel);
            self.reclaim();
            let old = core::mem::replace(&mut *self.readers[index].lock(), Reader::new());
            // Its references dropped here, with the lock let go.
            drop(old);
            let waiting = self.lights.lock()[index].take();
            if let Some((po, _)) = waiting {
                crate::sched::complete_pending_op(po.as_ptr(), KError::PeerClosed as i32, 0);
                drop(po);
            }
        }
        // Said before the node can be reached: the hardware report drains what this marks.
        let bit = 1u32 << index;
        if keyboard {
            self.keyboards.fetch_or(bit, Ordering::Relaxed);
        } else {
            self.keyboards.fetch_and(!bit, Ordering::Relaxed);
        }
        self.states[index].store(BOUND, Ordering::Release);
        Some((index, self.epochs[index].load(Ordering::Acquire)))
    }

    /// **Retire slot `index`'s node**, its device gone: push `releases`, answer a waiting read with
    /// them, or refuse it if there are none, and complete a waiting lights write `PeerClosed`. Thread
    /// context, with no lock held: completing takes the scheduler's lock, and a drop may reach the
    /// allocator.
    fn retire(&self, index: usize, releases: &[InputEvent], now: u64) {
        let mut scratch = [0u8; DRAIN_MAX];
        let (ready, refused) = {
            let mut r = self.readers[index].lock();
            if !releases.is_empty() {
                r.ring.push_group(releases);
            }
            let ready = r.take_ready(&mut scratch, now);
            (ready, r.retire())
        };
        if let Some((read, n)) = ready {
            input::deliver(&read, &scratch[..n]);
            drop(read);
        }
        if let Some(read) = refused {
            input::refuse(&read, KError::PeerClosed);
            drop(read);
        }
        let waiting = self.lights.lock()[index].take();
        if let Some((po, _)) = waiting {
            crate::sched::complete_pending_op(po.as_ptr(), KError::PeerClosed as i32, 0);
            drop(po);
        }
        self.states[index].store(RETIRED, Ordering::Release);
    }

    /// **Events decoded from slot `index`'s device**, whose node was made in `epoch`: pushed to its
    /// ring, and a read waiting there taken if they answer it. From the DPC.
    ///
    /// **Dropped once the node has retired** (PR #361 review), under the lock retiring takes: the
    /// DPC decodes a report under the bound table's lock and pushes it after letting that go, so a
    /// report decoded before a departure could otherwise land after the releases it pushed — a key
    /// down on a keyboard that has gone, with nothing left to let it up. Its release reaches the
    /// reader without it instead, which lets go of a key never seen pressed. And dropped when the
    /// slot has been given to another device since, whose ring this is not.
    fn push(
        &self,
        index: usize,
        epoch: u32,
        events: &[InputEvent],
        scratch: &mut [u8],
        now: u64,
    ) -> Option<(ParkedRead, usize)> {
        let mut r = self.readers[index].lock();
        if !self.current(context(index, epoch)) || !r.offer(events) {
            return None;
        }
        r.take_ready(scratch, now)
    }

    /// **A read of a node** at `now`: [`CharBackend::submit_read`]'s, as PS/2's is.
    fn read(
        &self,
        buffer: &ObjectRef,
        po: &ObjectRef,
        buf_offset: u64,
        max_len: u64,
        ctx: *mut (),
        now: u64,
    ) -> Result<(), KError> {
        let (index, _) = slot_of(ctx);
        if index >= MAX_NODES {
            return Err(KError::InvalidArgument);
        }
        let max_len = input::read_len(max_len)?;
        self.reclaim();
        let mut tmp = [0u8; DRAIN_MAX];
        let found = {
            let mut r = self.readers[index].lock();
            // **Under the node's lock**, against which a reused slot's reset is ordered (Phase 6 Part
            // C): a node from an earlier epoch reads none of the next device's ring.
            if !self.current(ctx) {
                ReadNow::Gone
            } else {
                let found = r.read_now(&mut tmp[..max_len], now);
                if found == ReadNow::Empty {
                    r.park(ParkedRead::new(po, buffer, buf_offset, max_len));
                }
                found
            }
        };
        match found {
            ReadNow::Drained(n) => input::deliver_now(buffer, po, buf_offset, &tmp[..n]),
            ReadNow::Empty => {}
            ReadNow::Busy => return Err(KError::WouldBlock),
            ReadNow::Gone => return Err(KError::PeerClosed),
        }
        Ok(())
    }

    /// **A lights write to a node**: [`CharBackend::submit_write`]'s, with the controller `x` —
    /// `None` before one is published.
    fn write(
        &self,
        x: Option<&Xhci>,
        buffer: &ObjectRef,
        po: &ObjectRef,
        buf_offset: u64,
        len: u64,
        ctx: *mut (),
    ) -> Result<(), KError> {
        let (index, _) = slot_of(ctx);
        if index >= MAX_NODES {
            return Err(KError::Unsupported);
        }
        // A node from an earlier epoch is gone, whatever its slot holds now (Phase 6 Part C).
        if !self.current(ctx) {
            return Err(KError::PeerClosed);
        }
        if self.keyboards.load(Ordering::Relaxed) & (1 << index) == 0 {
            return Err(KError::Unsupported);
        }
        let lights = input::lights_from(buffer, buf_offset, len)?;
        let Some(x) = x else {
            return Err(KError::PeerClosed);
        };
        match x.hid.lock().entries.iter().flatten().find(|b| b.node == index).map(|b| b.lights_dead) {
            None => return Err(KError::PeerClosed),
            Some(true) => return Err(KError::IoError),
            Some(false) => {}
        }
        let superseded = self.lights.lock()[index].replace((po.clone(), lights));
        if let Some((old, _)) = superseded {
            crate::sched::complete_pending_op(old.as_ptr(), 0, 1);
            drop(old);
        }
        x.hid_lights.store(true, Ordering::Release);
        crate::sched::signal_interrupt(x.hub_wake.as_ptr());
        Ok(())
    }

    /// Drop every read the DPC has finished with. **Thread context only**.
    fn reclaim(&self) {
        for (i, node) in self.readers.iter().enumerate() {
            if self.states[i].load(Ordering::Acquire) == FREE {
                continue;
            }
            let owed = node.lock().take_owed();
            drop(owed);
        }
    }

    /// Throw away every keyboard's waiting events.
    fn drain_keyboards(&self, now: u64) {
        let keyboards = self.keyboards.load(Ordering::Relaxed);
        let mut scratch = [0u8; DRAIN_MAX];
        for (i, node) in self.readers.iter().enumerate() {
            if keyboards & (1 << i) != 0 {
                while node.lock().ring.drain_into(&mut scratch, now) > 0 {}
            }
        }
    }
}

/// What a bound endpoint's reports are.
#[derive(Copy, Clone, Debug)]
pub(super) enum Decoder {
    /// A boot keyboard, and the last report kept.
    Keyboard { prev: [u8; keyboard::REPORT_LEN] },
    /// A mouse by its layout, and the buttons it holds.
    Mouse { layout: mouse::Layout, buttons: u8 },
}

/// **An endpoint the DPC polls.** Its ring and report buffer are the device's memory, which the hub
/// thread holds; this keeps where they are. [`depart`] takes the entry out before that memory can
/// go.
pub(super) struct Bound {
    slot: u8,
    dci: u8,
    /// The interface it is, which a keyboard's lights are addressed to.
    interface: u8,
    /// Its node's slot, and the slot's epoch when the node was made.
    node: usize,
    epoch: u32,
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
        let Some((node, epoch, served)) = publish(kind, parent) else {
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
            epoch,
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

/// **A node for a USB keyboard or mouse**, registered under `parent`: its slot here, the slot's
/// epoch, and the index `/dev/input/raw` serves it at. `None` when every slot is bound or the table
/// cannot hold it.
fn publish(kind: DeviceKind, parent: Option<u32>) -> Option<(usize, u32, u32)> {
    let (index, epoch) = NODES.take(kind == DeviceKind::Keyboard)?;
    LIGHTS_TAKEN.fetch_and(!(1u32 << index), Ordering::Relaxed);
    let backend = CharBackend { submit_read, submit_write: Some(submit_write), ctx: context(index, epoch) };
    let served = DeviceNode::try_new_char(ResourceDescriptor::ZERO, backend).ok().and_then(|node| {
        let node = crate::drivers::adopt(node, KObjectType::DeviceNode);
        crate::device::register_input(node, kind, parent, "usb-hid")
    });
    if served.is_none() {
        // No node reached anyone: the slot is the next device's.
        NODES.states[index].store(RETIRED, Ordering::Release);
    }
    Some((index, epoch, served?))
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
    let (node, epoch, n) = {
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
                (b.node, b.epoch, n)
            }
            _ => {
                if !b.halted {
                    b.halted = true;
                    b.halts = b.halts.saturating_add(1);
                    halted = true;
                }
                (b.node, b.epoch, 0)
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
    if let Some((read, len)) = NODES.push(node, epoch, &events[..n], &mut scratch, now) {
        input::deliver(&read, &scratch[..len]);
        NODES.readers[node].lock().owe(read);
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
            // Gone meanwhile: its device departed, and `depart` took it.
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

/// **A departing device's keyboards and mice, let go** (Phase 6 Part C), before its slot is
/// disabled and its memory freed. In this order:
/// 1. **what each holds is released**: a keyboard's last report decoded against an empty one, a
///    mouse's held buttons from its decoder's state;
/// 2. **its endpoints leave the DPC's table**, so the DPC cannot touch a buffer about to go;
/// 3. **each node retires**: the releases reach a read waiting on it, or the next read; after them a
///    read is refused, `PeerClosed`; a lights write still waiting is completed `PeerClosed`.
///
/// From the hub thread. The records depart after this, as one change.
pub(super) fn depart(x: &Xhci, slot: u8) {
    let now = crate::arch::Timer::read_ns();
    // `bind` binds at most four interfaces a device.
    let mut gone: [Option<(usize, [InputEvent; keyboard::EVENTS_MAX], usize)>; 4] = [const { None }; 4];
    {
        let mut t = x.hid.lock();
        let mut k = 0;
        for e in t.entries.iter_mut() {
            let Some(b) = e.take_if(|b| b.slot == slot) else {
                continue;
            };
            let mut events = [InputEvent::default(); keyboard::EVENTS_MAX];
            let n = match &b.decoder {
                Decoder::Keyboard { prev } => keyboard::release_all(prev, now, &mut events),
                Decoder::Mouse { buttons, .. } => {
                    let mut released = [InputEvent::default(); mouse::EVENTS_MAX];
                    let n = mouse::release_all(*buttons, now, &mut released);
                    events[..n].copy_from_slice(&released[..n]);
                    n
                }
            };
            if let Some(g) = gone.get_mut(k) {
                *g = Some((b.node, events, n));
                k += 1;
            }
        }
    }
    for (node, events, n) in gone.into_iter().flatten() {
        NODES.retire(node, &events[..n], now);
    }
}

/// [`CharBackend::submit_read`] for a USB input node: [`Nodes::read`], on the boot's.
fn submit_read(
    buffer: &ObjectRef,
    po: &ObjectRef,
    buf_offset: u64,
    _offset: u64,
    max_len: u64,
    ctx: *mut (),
) -> Result<(), KError> {
    NODES.read(buffer, po, buf_offset, max_len, ctx, crate::arch::Timer::read_ns())
}

/// Which keyboards have taken lights once: what the log says, the first time.
static LIGHTS_TAKEN: AtomicU32 = AtomicU32::new(0);

/// [`CharBackend::submit_write`] for a USB keyboard's node: **its lights** (Phase 6 Part B.5), one
/// byte in HID's order, sent by the hub thread as a `SET_REPORT`, which owns the default endpoint. A
/// write to a keyboard that has left is refused at once, `PeerClosed`, and to one that takes no more
/// lights, `IoError`; one that comes while another waits replaces it, which is completed as done,
/// its lights being older than the ones that will be set. [`Nodes::write`], on the boot's.
fn submit_write(buffer: &ObjectRef, po: &ObjectRef, buf_offset: u64, len: u64, ctx: *mut ()) -> Result<(), KError> {
    // SAFETY: the controller is published once and never withdrawn; null before.
    let x = unsafe { super::XHCI.load(Ordering::Acquire).as_ref() };
    NODES.write(x, buffer, po, buf_offset, len, ctx)
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
    let (po, lights) = NODES.lights.lock()[node].take()?;
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
    NODES.reclaim();
}

/// Throw away every USB keyboard's waiting events, as the PS/2 driver's `drain_keyboard` does: for
/// the hardware report's page turns.
pub fn drain_keyboards() {
    NODES.drain_keyboards(crate::arch::Timer::read_ns());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libkern::input::{INPUT_EVENT_LEN, KEY_RELEASE, REL_X};
    use crate::mm::PAGE_SIZE;
    use crate::mm::test_support::init_global_heap;
    use crate::object::{MemoryObject, PendingOperation};

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

    /// **A new node takes a slot no node has had, then one whose device has departed** (Phase 6 Part
    /// C), and none while every slot is bound: sixteen bound devices, not sixteen ever attached.
    #[test]
    fn a_new_node_takes_a_free_slot_then_a_retired_one() {
        let mut states = [BOUND; MAX_NODES];
        assert_eq!(choose(&states), None, "every slot bound");
        states[5] = RETIRED;
        assert_eq!(choose(&states), Some(5), "a departed device's slot");
        states[9] = FREE;
        assert_eq!(choose(&states), Some(9), "a free one first");
        assert_eq!(choose(&[FREE; MAX_NODES]), Some(0));
    }

    /// **A context names its slot and the epoch it was made in** (Phase 6 Part C), so a node made
    /// before its slot was reused is told apart from the one after: same slot, another epoch.
    #[test]
    fn a_context_carries_its_slot_and_epoch() {
        assert_eq!(slot_of(context(7, 0)), (7, 0));
        assert_eq!(slot_of(context(15, 3)), (15, 3));
        let (old, new) = (context(4, 1), context(4, 2));
        assert_ne!(old, new, "a reused slot's node is another");
        assert_eq!(slot_of(old).0, slot_of(new).0);
    }

    /// **A slot taken, retired and taken by another device serves the node it had before nothing of
    /// the new one** (PR #361 review): `take` bumps the epoch, replaces the reader and completes a
    /// lights write left waiting, and `read` and `write` refuse a node from an earlier epoch. No boot
    /// reaches it — a slot is reused only once all sixteen are bound — so this is its guard.
    #[test]
    fn a_reused_slot_serves_its_old_node_nothing_of_the_new_device() {
        init_global_heap();
        let nodes = Nodes::new();
        let mut made = [core::ptr::null_mut(); MAX_NODES];
        for (i, ctx) in made.iter_mut().enumerate() {
            let (index, epoch) = nodes.take(true).expect("a free slot");
            assert_eq!(index, i);
            *ctx = context(index, epoch);
        }
        assert_eq!(nodes.take(true), None, "every slot bound");

        // The keyboard in slot 5 departs holding a key, and a lights write is left waiting.
        nodes.retire(5, &[InputEvent::key(30, KEY_RELEASE, 1)], 1);
        let lights = crate::drivers::adopt(PendingOperation::try_new().unwrap(), KObjectType::PendingOperation);
        nodes.lights.lock()[5] = Some((lights.clone(), 1));
        // A mouse takes its slot, and moves.
        let (index, epoch) = nodes.take(false).expect("the retired slot");
        assert_eq!(index, 5);
        let now = context(index, epoch);
        assert_ne!(now, made[5], "another epoch");
        assert!(nodes.lights.lock()[5].is_none(), "the waiting lights write is let go");
        assert_eq!(crate::sched::pending_op_completion(lights.as_ptr()), (KError::PeerClosed as i32, 0));
        nodes.readers[5].lock().ring.push(InputEvent::rel(REL_X, 3, 2));

        let buf = crate::drivers::adopt(MemoryObject::try_new(PAGE_SIZE).unwrap(), KObjectType::MemoryObject);
        let po = crate::drivers::adopt(PendingOperation::try_new().unwrap(), KObjectType::PendingOperation);
        let len = 4 * INPUT_EVENT_LEN as u64;
        assert_eq!(nodes.read(&buf, &po, 0, len, made[5], 3), Err(KError::PeerClosed), "the keyboard's node");
        assert!(!nodes.readers[5].lock().ring.is_empty(), "took none of the mouse's motion");
        assert_eq!(nodes.write(None, &buf, &po, 0, 1, made[5]), Err(KError::PeerClosed), "nor set its lights");
        assert_eq!(nodes.write(None, &buf, &po, 0, 1, now), Err(KError::Unsupported), "a mouse takes none");
        assert_eq!(nodes.read(&buf, &po, 0, len, now, 3), Ok(()), "the mouse's node reads it");
        assert_eq!(
            crate::sched::pending_op_completion(po.as_ptr()),
            (0, INPUT_EVENT_LEN as u64),
            "the motion alone: none of the keyboard's release",
        );
        assert!(nodes.current(made[4]) && nodes.current(made[6]), "the other slots' nodes are theirs");
        assert!(!nodes.current(context(MAX_NODES + 1, 0)), "no such slot");
    }

    /// **A report the DPC pushes after its device departed is dropped** (PR #361 review), under the
    /// node's lock: once the node has retired, so its releases stay the last thing its ring holds, and
    /// once the slot is another device's, whose ring it is not.
    #[test]
    fn a_report_pushed_after_its_device_departed_is_dropped() {
        init_global_heap();
        let nodes = Nodes::new();
        let mut scratch = [0u8; DRAIN_MAX];
        let (index, epoch) = nodes.take(true).expect("a free slot");
        // Every other slot bound, so the next device takes this one once it retires.
        while nodes.take(false).is_some() {}
        let press = [InputEvent::key(30, KEY_PRESS, 1), InputEvent::syn(1)];
        assert!(nodes.push(index, epoch, &press, &mut scratch, 1).is_none(), "no read waiting");
        assert!(!nodes.readers[index].lock().ring.is_empty(), "pushed while the keyboard is here");

        nodes.retire(index, &[InputEvent::key(30, KEY_RELEASE, 2), InputEvent::syn(2)], 2);
        // What the ring holds, drained: how many events, and the last key among them.
        let held = |n: &Nodes| {
            let mut out = [0u8; DRAIN_MAX];
            let len = n.readers[index].lock().ring.drain_into(&mut out, 3);
            let events = out[..len].chunks(INPUT_EVENT_LEN).filter_map(InputEvent::read);
            let last = events.clone().filter(|e| e.kind == EV_KEY).last().map(|e| (e.code, e.value));
            (events.count(), last)
        };
        nodes.push(index, epoch, &press, &mut scratch, 2);
        assert_eq!(held(&nodes), (4, Some((30, KEY_RELEASE))), "the press and the release, and nothing after");

        let (again, now) = nodes.take(false).expect("the retired slot");
        assert_eq!(again, index);
        nodes.push(index, epoch, &press, &mut scratch, 4);
        assert!(nodes.readers[index].lock().ring.is_empty(), "the old node's report reaches no new ring");
        nodes.push(index, now, &[InputEvent::rel(REL_X, 3, 5)], &mut scratch, 5);
        assert_eq!(held(&nodes), (1, None), "the new device's own does");
    }
}
