//! The **i8042** controller driver (Tier 1) — PS/2 keyboard and mouse.
//!
//! `docs/architecture/input-subsystem.md`. One driver: the keyboard and the mouse are two *devices
//! behind one controller*, and enabling the mouse is a read-modify-write of the config byte that
//! carries the keyboard's IRQ-1 enable; two drivers would race on it and boot with a dead keyboard.
//! It publishes `/dev/input/raw/0` (the keyboard) and `/dev/input/raw/1` (the mouse). Port I/O
//! and interrupt arming stay behind `crate::arch::ps2`.

pub mod lights;
pub mod mouse;
pub mod scancode;

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::ps2::Port;
use crate::arch::timer::ArchTimer;
use crate::dpc::Dpc;
use crate::drivers::input::{self, DRAIN_MAX, ParkedRead, ReadNow, Reader, Submitted};
use crate::libkern::IrqSpinLock;
use crate::libkern::KBox;
use crate::libkern::handle::KObjectType;
use crate::libkern::input::InputEvent;
use crate::libkern::lockrank::LockRank;
use crate::object::device_node::{CharBackend, ResourceDescriptor};
use crate::object::{DeviceNode, ObjectRef};
use crate::syscall::error::KError;

/// The two devices this controller publishes, and their `/dev/input/raw/<n>` indices.
pub const DEV_KEYBOARD: usize = 0;
/// Index of the mouse node.
pub const DEV_MOUSE: usize = 1;
/// How many raw nodes exist.
pub const DEV_COUNT: usize = 2;

/// Everything the driver owns, behind one lock for both devices: a single status read decides which
/// port a byte came from.
struct Inner {
    /// Each node's reader side, by index; the DPC owns a read it takes out until it hands it back.
    devices: [Reader; DEV_COUNT],
    keys: scancode::Decoder,
    mouse: mouse::Decoder,
    lights: Option<lights::Exchange>,
    lights_po: Option<ObjectRef>,
    lights_next: Option<(ObjectRef, u8)>,
    lights_done: [Option<LightsDone>; 4],
    lights_unreported: Option<i32>,
    /// What the DPC logs about the exchange that ended, outside this lock (the serial port's lock
    /// ranks above it).
    lights_say: Option<Say>,
}

struct LightsDone {
    po: ObjectRef,
    status: i32,
    state: Completion,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Completion {
    Waiting,
    /// The DPC took it and is completing it **outside this lock** — completing takes the
    /// scheduler's, which ranks above it — so thread context must not drop it yet.
    Completing,
    Completed,
}

#[derive(Copy, Clone)]
enum Say {
    Acknowledged,
    Refused(lights::Failure),
}

impl Inner {
    const fn new() -> Self {
        Self {
            devices: [Reader::new(), Reader::new()],
            keys: scancode::Decoder::new(),
            mouse: mouse::Decoder::new(),
            lights: None,
            lights_po: None,
            lights_next: None,
            lights_done: [const { None }; 4],
            lights_unreported: None,
            lights_say: None,
        }
    }

    /// **A byte from the keyboard port**: the exchange's answer while one is in flight, taken
    /// *ahead of* the decoder (an answer between an `E0` prefix and its code must not become a
    /// key), or what the decoder makes of it.
    fn keyboard_byte(&mut self, byte: u8, now: u64) -> KeyboardByte {
        if let Some(x) = self.lights.as_mut()
            && let Some(action) = x.on_byte(byte, now)
        {
            return KeyboardByte::Answer(action);
        }
        KeyboardByte::Key(self.keys.feed(byte))
    }

    /// **Act on the exchange's `action`**: send, or end it and start the write that waited. Whether
    /// a write is ready for the DPC.
    fn lights_action(&mut self, action: lights::Action, now: u64) -> bool {
        match action {
            lights::Action::Send(byte) => {
                if crate::arch::ps2::send_keyboard(byte) {
                    return false;
                }
                self.lights_end(Err(KError::IoError), now)
            }
            lights::Action::Done(Ok(())) => self.lights_end(Ok(()), now),
            lights::Action::Done(Err(why)) => {
                self.lights_say = Some(Say::Refused(why));
                self.lights_end(Err(KError::TimedOut), now)
            }
        }
    }

    fn lights_end(&mut self, result: Result<(), KError>, now: u64) -> bool {
        self.lights = None;
        LIGHTS_IN_FLIGHT.store(false, Ordering::Release);
        if result.is_ok() && !LIGHTS_TAKEN.swap(true, Ordering::Relaxed) {
            self.lights_say = Some(Say::Acknowledged);
        }
        self.lights_unreported = Some(result.err().map_or(0, |e| e as i32));
        self.report_lights();
        if self.lights_unreported.is_none()
            && let Some((po, lights)) = self.lights_next.take()
        {
            self.lights_start(po, lights, now);
        }
        true
    }

    fn report_lights(&mut self) {
        let Some(status) = self.lights_unreported else { return };
        let Some(slot) = self.lights_done.iter_mut().find(|d| d.is_none()) else { return };
        if let Some(po) = self.lights_po.take() {
            *slot = Some(LightsDone { po, status, state: Completion::Waiting });
        }
        self.lights_unreported = None;
    }

    /// Begin an exchange for `po` with `lights`; a failed send ends it at once.
    fn lights_start(&mut self, po: ObjectRef, lights: u8, now: u64) {
        let (exchange, first) = lights::Exchange::start(lights, now);
        self.lights_po = Some(po);
        if crate::arch::ps2::send_keyboard(first) {
            self.lights = Some(exchange);
            LIGHTS_IN_FLIGHT.store(true, Ordering::Release);
        } else {
            self.lights_end(Err(KError::IoError), now);
        }
    }
}

enum KeyboardByte {
    Answer(lights::Action),
    Key(scancode::Decoded),
}

static LIGHTS_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static LIGHTS_TAKEN: AtomicBool = AtomicBool::new(false);

static PS2: IrqSpinLock<Inner> = IrqSpinLock::new(LockRank::Leaf, Inner::new());

static PS2_DPC: Dpc = Dpc::new(ps2_intr_dpc, core::ptr::null_mut());

static PRESENT: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "ps2-hold-gate")]
static HOLD_UNTIL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "ps2-hold-gate")]
const HOLD_NS: u64 = 300_000_000;

fn holding(now: u64) -> bool {
    #[cfg(feature = "ps2-hold-gate")]
    {
        now < HOLD_UNTIL.load(Ordering::Relaxed)
    }
    #[cfg(not(feature = "ps2-hold-gate"))]
    {
        let _ = now;
        false
    }
}

/// Bytes one interrupt may take before yielding, bounding the interrupts-off window.
const MAX_DRAIN_PER_IRQ: u32 = 64;

/// [`CharBackend::submit_read`] for a raw input node; `max_len` is floored to whole records.
fn submit_read(
    buffer: &ObjectRef,
    po: &ObjectRef,
    buf_offset: u64,
    _offset: u64,
    max_len: u64,
    ctx: *mut (),
) -> Result<(), KError> {
    let index = ctx as usize;
    if index >= DEV_COUNT {
        return Err(KError::InvalidArgument);
    }
    read(&PS2, index, buffer, po, buf_offset, input::read_len(max_len)?)
}

fn read(
    ps2: &IrqSpinLock<Inner>,
    index: usize,
    buffer: &ObjectRef,
    po: &ObjectRef,
    buf_offset: u64,
    max_len: usize,
) -> Result<(), KError> {
    let mut tmp = [0u8; DRAIN_MAX];
    let found = input::read_or_park(
        || reclaim(ps2),
        || {
            let now = crate::arch::Timer::read_ns();
            let mut g = ps2.lock();
            let dev = &mut g.devices[index];
            let found = dev.read_now(&mut tmp[..max_len], now);
            if found == ReadNow::Empty {
                dev.park(ParkedRead::new(po, buffer, buf_offset, max_len));
            }
            found
        },
    );
    match found {
        Submitted::Drained(n) => input::deliver_now(buffer, po, buf_offset, &tmp[..n]),
        Submitted::Parked => {}
        Submitted::Busy => return Err(KError::WouldBlock),
        // The i8042's devices never depart.
        Submitted::Gone => return Err(KError::PeerClosed),
    }
    Ok(())
}

fn ps2_intr_dpc(_ctx: *mut ()) {
    let now = crate::arch::Timer::read_ns();
    for index in 0..DEV_COUNT {
        let mut tmp = [0u8; DRAIN_MAX];
        // Exclusive ownership while delivering; handed back after (see `drivers::input`).
        let ready = PS2.lock().devices[index].take_ready(&mut tmp, now);
        if let Some((read, n)) = ready {
            input::deliver(&read, &tmp[..n]);
            PS2.lock().devices[index].owe(read);
        }
    }
    // **Finished lights writes**: taken under the lock and completed outside it (completing takes
    // the scheduler's lock, which ranks above this leaf); `Completing` keeps thread context from
    // dropping them first.
    let mut taken: [Option<(usize, *mut (), i32)>; 4] = [None; 4];
    let say = {
        let mut g = PS2.lock();
        for (i, d) in g.lights_done.iter_mut().enumerate() {
            if let Some(d) = d.as_mut().filter(|d| d.state == Completion::Waiting) {
                d.state = Completion::Completing;
                taken[i] = Some((i, d.po.as_ptr(), d.status));
            }
        }
        g.lights_say.take()
    };
    for &(_, po, status) in taken.iter().flatten() {
        // SAFETY: the slot holds `po`'s reference, `Completing` until thread context reclaims it after this.
        unsafe { crate::sched::complete_pending_op(po, status, (status == 0) as u64) };
    }
    if taken.iter().any(Option::is_some) {
        let mut g = PS2.lock();
        for &(i, _, _) in taken.iter().flatten() {
            if let Some(d) = g.lights_done[i].as_mut() {
                d.state = Completion::Completed;
            }
        }
    }
    match say {
        Some(Say::Acknowledged) => crate::kprintln!("ps2: keyboard lights acknowledged"),
        Some(Say::Refused(why)) => crate::kprintln!("ps2: the keyboard did not take its lights ({:?})", why),
        None => {}
    }
}

/// Drop any read the DPC has finished with, from `sched::reap_pending` and before a new read parks
/// — **thread context only**, being the frees `ps2_intr_dpc` must not do.
pub fn reclaim_completed() {
    reclaim(&PS2);
}

fn reclaim(ps2: &IrqSpinLock<Inner>) {
    for index in 0..DEV_COUNT {
        // Take under the lock, drop outside it: a drop reaches the allocator.
        let owed = ps2.lock().devices[index].take_owed();
        drop(owed);
    }
    let mut done: [Option<LightsDone>; 4] = [const { None }; 4];
    let started = {
        let mut g = ps2.lock();
        for (slot, out) in g.lights_done.iter_mut().zip(done.iter_mut()) {
            if slot.as_ref().is_some_and(|d| d.state == Completion::Completed) {
                *out = slot.take();
            }
        }
        let waited = g.lights_unreported.is_some();
        g.report_lights();
        if waited
            && g.lights_unreported.is_none()
            && g.lights.is_none()
            && let Some((po, lights)) = g.lights_next.take()
        {
            let now = crate::arch::Timer::read_ns();
            g.lights_start(po, lights, now);
        }
        waited
    };
    drop(done);
    if started {
        crate::dpc::enqueue(&PS2_DPC);
    }
}

/// [`CharBackend::submit_write`] for the keyboard's node: **its lights**, one byte in HID's order.
/// A write arriving while one is in flight waits, replacing any already waiting, which completes as
/// done.
fn submit_write(buffer: &ObjectRef, po: &ObjectRef, buf_offset: u64, len: u64, ctx: *mut ()) -> Result<(), KError> {
    if ctx as usize != DEV_KEYBOARD {
        return Err(KError::Unsupported);
    }
    let lights = input::lights_from(buffer, buf_offset, len)?;
    reclaim_completed();
    let now = crate::arch::Timer::read_ns();
    let superseded = {
        let mut g = PS2.lock();
        if g.lights.is_none() && g.lights_po.is_none() {
            g.lights_start(po.clone(), lights, now);
            None
        } else {
            g.lights_next.replace((po.clone(), lights))
        }
    };
    if let Some((old, _)) = superseded {
        // SAFETY: `old` is the superseded write's `PendingOperation` reference.
        unsafe { crate::sched::complete_pending_op(old.as_ptr(), 0, 1) };
        drop(old);
    }
    crate::dpc::enqueue(&PS2_DPC);
    Ok(())
}

#[cfg(feature = "fbcon-gate")]
const CRASH_KEY: u16 = crate::libkern::input::KEY_F1 + 9;

/// Drain the controller into the rings, for both ISRs: **both ports deliver through the same data
/// port**, so whichever line fires the byte might be either device's and the status bit is the only
/// discriminator; returns whether any device now has a parked reader to wake.
fn drain_controller() -> bool {
    let now = crate::arch::Timer::read_ns();
    let mut g = PS2.lock();
    // **Bounded**: the lock masks interrupts, so an unbounded drain is an unbounded interrupts-off
    // window (a stuck repeating key would hold this CPU forever).
    //
    // **Anything left waiting is collected by [`poll`]**: the controller raises only when a byte
    // *becomes* available, so one already in the output buffer produces no further edge.
    let mut budget = MAX_DRAIN_PER_IRQ;
    let mut lights_finished = false;
    // A `ps2-hold-gate` hold reads nothing, so injected input queues in the host's device.
    while budget > 0
        && !holding(now)
        && let Some((port, byte)) = crate::arch::ps2::read_byte()
    {
        budget -= 1;
        match port {
            Port::Keyboard => {
                let decoded = match g.keyboard_byte(byte, now) {
                    KeyboardByte::Answer(action) => {
                        lights_finished |= g.lights_action(action, now);
                        continue;
                    }
                    KeyboardByte::Key(d) => d,
                };
                if let scancode::Decoded::Key { code, pressed } = decoded {
                    #[cfg(feature = "fbcon-gate")]
                    if pressed && code == CRASH_KEY {
                        // **With this leaf lock held, deliberately**: the message must still reach
                        // the screen. `check-fbcon` is the regression test for `lock-order
                        // violation: acquiring Klog (rank 72) while holding Leaf (rank 90)` from
                        // the panic path's `try_lock` tee.
                        panic!("fbcon-gate: F10 pressed, and this kernel was built to stop on it");
                    }
                    if pressed {
                        input::note_key_press();
                    }
                    // F9, the ninth of the consecutive function keys; F10 is `fbcon-gate`'s.
                    #[cfg(feature = "ps2-hold-gate")]
                    if pressed && code == crate::libkern::input::KEY_F1 + 8 {
                        HOLD_UNTIL.store(now + HOLD_NS, Ordering::Relaxed);
                    }
                    let value =
                        if pressed { crate::libkern::input::KEY_PRESS } else { crate::libkern::input::KEY_RELEASE };
                    let group = [InputEvent::key(code, value, now), InputEvent::syn(now)];
                    g.devices[DEV_KEYBOARD].ring.push_group(&group);
                }
            }
            Port::Aux => {
                if let Some(packet) = g.mouse.feed(byte) {
                    let mut out = [InputEvent::default(); mouse::Decoder::MAX_EVENTS];
                    let n = g.mouse.events(packet, now, &mut out);
                    if n > 0 {
                        g.devices[DEV_MOUSE].ring.push_group(&out[..n]);
                    }
                }
            }
        }
    }
    lights_finished || g.devices.iter().any(|d| d.has_parked())
}

/// Collect anything the interrupt path missed. Called from the timer IRQ dispatcher, ahead of the
/// DPC drain so a byte recovered here wakes its reader on this tick.
///
/// **An i8042 interrupt can be lost, and nothing else recovers it.** The controller has a
/// *one-byte* output buffer and its IRQ line is a level the interrupt controller turns into an
/// edge: a byte arriving after a drain's last status read, while the line is asserted, produces
/// **no new edge**, and since nobody reads the buffer the line never drops. Re-checking cannot
/// close it (there is always a last read), so a periodic sweep is the answer.
pub fn poll() {
    // **A machine with no i8042 must not pay for one**: an absent controller floats its status port
    // high, so `output_pending` reads `0xFF`, an unguarded sweep would run the full drain budget
    // every tick with interrupts masked, and three `0xFF` bytes decode as a valid mouse packet.
    if !PRESENT.load(Ordering::Acquire) {
        return;
    }
    // **A lights exchange the keyboard has not answered** fails on the tick after its bound.
    if LIGHTS_IN_FLIGHT.load(Ordering::Acquire) {
        let now = crate::arch::Timer::read_ns();
        let ended = {
            let mut g = PS2.lock();
            match g.lights.as_ref().and_then(|x| x.on_tick(now)) {
                Some(action) => g.lights_action(action, now),
                None => false,
            }
        };
        if ended {
            crate::dpc::enqueue(&PS2_DPC);
        }
    }
    if !crate::arch::ps2::output_pending() {
        return;
    }
    if drain_controller() {
        crate::dpc::enqueue(&PS2_DPC);
    }
}

/// `true` once a keyboard has answered and its interrupt is armed, the only case in which this
/// driver can move [`input::key_presses`].
pub fn keyboard_present() -> bool {
    PRESENT.load(Ordering::Acquire) && crate::device::has(crate::libkern::device::DeviceKind::Keyboard)
}

/// Throw away every keyboard event waiting in the ring (thread context), returning how many bytes
/// went: keys pressed before anyone could mean them for a program.
pub fn drain_keyboard() -> usize {
    let now = crate::arch::Timer::read_ns();
    let mut scratch = [0u8; DRAIN_MAX];
    let mut total = 0;
    loop {
        let n = PS2.lock().devices[DEV_KEYBOARD].ring.drain_into(&mut scratch, now);
        if n == 0 {
            return total;
        }
        total += n;
    }
}

/// Keyboard interrupt (IRQ 1).
extern "C" fn kbd_isr() {
    if drain_controller() {
        crate::dpc::enqueue(&PS2_DPC);
    }
}

/// Aux/mouse interrupt (IRQ 12).
extern "C" fn aux_isr() {
    if drain_controller() {
        crate::dpc::enqueue(&PS2_DPC);
    }
}

/// Bring up the i8042 and publish its device nodes: call once at boot with interrupts masked, since
/// [`crate::arch::ps2::init`] polls and must consume every controller response before the decoder
/// sees a byte.
pub fn init() {
    // SAFETY: ring-0 at boot, before `arm`, and nothing else touches 0x60/0x64.
    let present = unsafe { crate::arch::ps2::init() };
    if !present.keyboard && !present.mouse {
        crate::kprintln!("ps2: no i8042 devices answered (no /dev/input/raw/*)");
        return;
    }
    if present.wheel {
        // **The decoder cannot work this out itself**: a mouse that answered the knock sends
        // four-byte packets from now on, so a decoder still framing three would read every one at
        // an offset. Done here because the packet layout is this module's. Needs no ordering care:
        // `PRESENT` is false and the interrupts are not armed.
        PS2.lock().mouse.enable_wheel();
    }

    for (index, present) in [(DEV_KEYBOARD, present.keyboard), (DEV_MOUSE, present.mouse)] {
        if !present {
            continue;
        }
        let backend = CharBackend { submit_read, submit_write: Some(submit_write), ctx: index as *mut () };
        match DeviceNode::try_new_char(ResourceDescriptor::ZERO, backend) {
            Ok(node) => {
                // **Owned by the device table**, which `/dev/input/raw/<n>` resolves through.
                // SAFETY: `into_raw` yields the single creation reference; the table adopts it.
                let r =
                    unsafe { ObjectRef::from_raw(KBox::into_raw(node).as_ptr() as *mut (), KObjectType::DeviceNode) };
                let kind = if index == DEV_KEYBOARD {
                    crate::libkern::device::DeviceKind::Keyboard
                } else {
                    crate::libkern::device::DeviceKind::Mouse
                };
                crate::device::register_char(r, kind, index as u32, "i8042");
            }
            Err(_) => crate::kprintln!("ps2: device-node alloc FAIL for index {}", index),
        }
    }

    // SAFETY: ring-0, after `IrqRouter::init` and after `arch::ps2::init`; both handlers
    // are valid for the kernel's lifetime and the rings are ready to receive.
    let (kbd_vec, aux_vec) = unsafe { crate::arch::ps2::arm(kbd_isr, aux_isr) };
    // Only now may `poll` touch the ports: a device answered, so the status port is the
    // controller's and not a floating bus. **Last, deliberately**: set before `arch::ps2::init`'s
    // polled handshake, a tick's sweep could consume its `0xFA`/`0xAA` replies as scancodes. No
    // tick can land here today (the periodic timer starts after this), so storing last keeps that
    // unreachable.
    PRESENT.store(true, Ordering::Release);
    crate::kprintln!(
        "ps2: {}{}{}armed (kbd {}, aux {})",
        if present.keyboard { "keyboard " } else { "" },
        if present.mouse { "mouse " } else { "" },
        // Logged because it changes how every mouse packet is *framed*.
        if present.wheel { "wheel " } else { "" },
        Routed(kbd_vec),
        Routed(aux_vec)
    );
}

/// A line's vector for the log, or that it could not be routed.
struct Routed(Option<u8>);

impl core::fmt::Display for Routed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(vector) => write!(f, "vec{vector:#x}"),
            None => f.write_str("unrouted"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libkern::input::{INPUT_EVENT_LEN, KEY_PRESS, KEY_UP, LIGHT_CAPS};

    #[test]
    fn a_keyboard_read_waits_out_the_delivery_before_it() {
        input::test_support::a_read_waits_out_the_delivery_before_it(
            IrqSpinLock::new(LockRank::Leaf, Inner::new()),
            |ps2, buf, po| read(ps2, DEV_KEYBOARD, buf, po, 0, INPUT_EVENT_LEN),
            |ps2| {
                let mut g = ps2.lock();
                g.devices[DEV_KEYBOARD].ring.push(InputEvent::key(30, KEY_PRESS, 1));
                g.devices[DEV_KEYBOARD].take_ready(&mut [0u8; DRAIN_MAX], 0).unwrap().0
            },
            |ps2, first| ps2.lock().devices[DEV_KEYBOARD].owe(first),
            |ps2| ps2.lock().devices[DEV_KEYBOARD].has_parked(),
        );
    }

    fn decoded(b: KeyboardByte) -> Option<scancode::Decoded> {
        match b {
            KeyboardByte::Key(d) => Some(d),
            KeyboardByte::Answer(_) => None,
        }
    }

    /// **An acknowledgement between an `E0` prefix and its code is the exchange's, not a key**: the
    /// decoder alone reads `E0 FA 48` as keypad 8 pressed and never released.
    #[test]
    fn an_answer_between_e0_and_its_code_is_the_exchanges() {
        let mut g = Inner::new();
        g.lights = Some(lights::Exchange::start(LIGHT_CAPS as u8, 0).0);
        assert_eq!(decoded(g.keyboard_byte(0xE0, 1)), Some(scancode::Decoded::Consumed), "the prefix");
        assert!(
            matches!(g.keyboard_byte(lights::ACK, 1), KeyboardByte::Answer(lights::Action::Send(0b100))),
            "the answer, sending the mask"
        );
        assert_eq!(
            decoded(g.keyboard_byte(0x48, 1)),
            Some(scancode::Decoded::Key { code: KEY_UP, pressed: true }),
            "Up, as it was pressed"
        );

        let mut alone = Inner::new();
        alone.keyboard_byte(0xE0, 1);
        alone.keyboard_byte(lights::ACK, 1);
        assert_eq!(
            decoded(alone.keyboard_byte(0x48, 1)),
            Some(scancode::Decoded::Key { code: 72, pressed: true }),
            "with no exchange in flight the decoder sees the answer, and Up becomes keypad 8"
        );
    }
}
