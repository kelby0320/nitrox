//! The **i8042** controller driver (Tier 1) — PS/2 keyboard and mouse.
//!
//! `docs/architecture/input-subsystem.md`. One driver, because the keyboard and the mouse are two
//! *devices behind one controller*: they share data port `0x60`, are configured through
//! command port `0x64`, and enabling the mouse is a read-modify-write of the same config
//! byte that carries the keyboard's IRQ-1 enable. Two drivers initialising independently
//! race on that byte and produce a machine that intermittently boots with a dead keyboard.
//!
//! It publishes **two** char `DeviceNode`s — `/dev/input/raw/0` (keyboard) and
//! `/dev/input/raw/1` (mouse) — each delivering
//! [`InputEvent`](crate::libkern::input::InputEvent) records.
//!
//! ## What is here and what is in `arch`
//!
//! Port I/O and interrupt arming are x86-only and stay behind `crate::arch::ps2`, exactly as
//! the serial console keeps COM1's registers and IRQ behind `crate::arch::serial`. The
//! decision is older than this driver: `arch/mod.rs` refuses to re-export `install_isa_irq`
//! neutrally because "ISA" is x86 jargon, and "a fixed legacy platform device wires its own
//! interrupt inside the arch layer". The i8042 is exactly that class of device.
//!
//! This module owns what is portable: the scancode table and the mouse packet framing. Each
//! node's ring, the read parked on it and its hand-off to thread context are
//! [`crate::drivers::input`]'s, shared with USB HID since Phase 6 Part B.1.

pub mod lights;
pub mod mouse;
pub mod scancode;

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::ps2::Port;
use crate::arch::timer::ArchTimer;
use crate::dpc::Dpc;
use crate::drivers::input::{self, DRAIN_MAX, ParkedRead, ReadNow, Reader};
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

/// Everything the driver owns, behind one lock.
///
/// **One lock for both devices, deliberately.** They share the controller: a single status
/// read decides which port a byte came from, so the two ISRs cannot be made independent
/// anyway, and two locks would only add an ordering rule to get wrong.
struct Inner {
    /// Each node's reader side, by its index. The DPC owns a read it takes out of one until it
    /// hands it back, and never drops it: see [`crate::drivers::input`].
    devices: [Reader; DEV_COUNT],
    keys: scancode::Decoder,
    mouse: mouse::Decoder,
    /// **The lights exchange in flight** (Phase 6 Part B.5), and the write it serves.
    lights: Option<lights::Exchange>,
    lights_po: Option<ObjectRef>,
    /// The latest write that came while one was in flight: started when that one ends.
    lights_next: Option<(ObjectRef, u8)>,
    /// Finished writes and their status, for the DPC to complete and thread context to drop, as a
    /// read's are. A write whose slot is not yet free waits in `lights_po` with its status here.
    lights_done: [Option<LightsDone>; 4],
    lights_unreported: Option<i32>,
    /// What the DPC is to log about the exchange that ended, outside this lock: the serial port's
    /// lock ranks above it.
    lights_say: Option<Say>,
}

/// A finished lights write: its operation, its status, and how far the DPC has got with it.
struct LightsDone {
    po: ObjectRef,
    status: i32,
    state: Completion,
}

/// How far a finished lights write has got.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Completion {
    /// Waiting for the DPC.
    Waiting,
    /// The DPC took it and is completing it **outside this lock** — completing takes the
    /// scheduler's, which ranks above it — so thread context must not drop it yet.
    Completing,
    /// Completed: thread context drops it.
    Completed,
}

/// What the log says about an exchange.
#[derive(Copy, Clone)]
enum Say {
    /// The keyboard took its lights, for the first time.
    Acknowledged,
    /// The keyboard did not take them.
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

    /// **A byte from the keyboard port**: the lights exchange's answer while one is in flight —
    /// taken here, *ahead of* the decoder, so an answer between an `E0` prefix and its code cannot
    /// become a key (PR #357 review) — or what the decoder makes of it.
    fn keyboard_byte(&mut self, byte: u8, now: u64) -> KeyboardByte {
        if let Some(x) = self.lights.as_mut()
            && let Some(action) = x.on_byte(byte, now)
        {
            return KeyboardByte::Answer(action);
        }
        KeyboardByte::Key(self.keys.feed(byte))
    }

    /// **Act on the exchange's `action`**: send what it says, or end it — reporting the write, and
    /// starting the one that waited. Whether a write is now there for the DPC to complete.
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

    /// End the exchange in flight with `result`, and start the next write if one waited.
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

    /// Move a finished write into a free slot for the DPC.
    fn report_lights(&mut self) {
        let Some(status) = self.lights_unreported else { return };
        let Some(slot) = self.lights_done.iter_mut().find(|d| d.is_none()) else { return };
        if let Some(po) = self.lights_po.take() {
            *slot = Some(LightsDone { po, status, state: Completion::Waiting });
        }
        self.lights_unreported = None;
    }

    /// Begin an exchange for `po` with `lights`. A keyboard the controller would not send to ends it
    /// at once, failed.
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

/// What a byte from the keyboard port was.
enum KeyboardByte {
    /// The lights exchange's answer.
    Answer(lights::Action),
    /// What the scancode decoder made of it.
    Key(scancode::Decoded),
}

/// An exchange is in flight: what the tick checks before taking the lock for its bound.
static LIGHTS_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
/// The keyboard took lights once: what the log says, the first time.
static LIGHTS_TAKEN: AtomicBool = AtomicBool::new(false);

static PS2: IrqSpinLock<Inner> = IrqSpinLock::new(LockRank::Leaf, Inner::new());

/// Completes parked reads after an ISR deposits events.
static PS2_DPC: Dpc = Dpc::new(ps2_intr_dpc, core::ptr::null_mut());

/// Set once a device has answered and the handlers are armed. Guards [`poll`] — see there for
/// why an absent controller would otherwise be drained on every tick.
static PRESENT: AtomicBool = AtomicBool::new(false);

/// `ps2-hold-gate` only: nothing is drained from the controller before this monotonic time.
#[cfg(feature = "ps2-hold-gate")]
static HOLD_UNTIL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// How long an F9 press holds the drain, in a `ps2-hold-gate` kernel: long enough for the host to
/// inject a walk and a click into it, short enough that a gate spends little time waiting.
#[cfg(feature = "ps2-hold-gate")]
const HOLD_NS: u64 = 300_000_000;

/// Whether the drain is held at `now` — never, in a kernel without `ps2-hold-gate`.
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

/// Bytes one interrupt may take from the controller before yielding.
///
/// Generous for a burst — a held key repeats at ~30 Hz and a mouse reports at 100 Hz, so
/// nothing legitimate approaches it — while keeping the interrupts-off window bounded.
const MAX_DRAIN_PER_IRQ: u32 = 64;

/// [`CharBackend::submit_read`] for a raw input node: satisfy immediately if the ring has
/// anything, else park until the next interrupt. `max_len` is floored to whole records
/// ([`input::read_len`]).
fn submit_read(
    buffer: &ObjectRef,
    po: &ObjectRef,
    buf_offset: u64,
    max_len: u64,
    ctx: *mut (),
) -> Result<(), KError> {
    let index = ctx as usize;
    if index >= DEV_COUNT {
        return Err(KError::InvalidArgument);
    }
    let max_len = input::read_len(max_len)?;
    // Thread context: release anything a previous completion left owed before parking.
    reclaim_completed();
    let now = crate::arch::Timer::read_ns();
    let mut tmp = [0u8; DRAIN_MAX];
    let found = {
        let mut g = PS2.lock();
        let dev = &mut g.devices[index];
        let found = dev.read_now(&mut tmp[..max_len], now);
        if found == ReadNow::Empty {
            dev.park(ParkedRead::new(po, buffer, buf_offset, max_len));
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

/// DPC: complete any parked read whose device has events. Queued by the ISRs.
fn ps2_intr_dpc(_ctx: *mut ()) {
    let now = crate::arch::Timer::read_ns();
    for index in 0..DEV_COUNT {
        let mut tmp = [0u8; DRAIN_MAX];
        // **Take exclusive ownership for the duration**, and hand it back after: see
        // `drivers::input`.
        let ready = PS2.lock().devices[index].take_ready(&mut tmp, now);
        if let Some((read, n)) = ready {
            input::deliver(&read, &tmp[..n]);
            PS2.lock().devices[index].owe(read);
        }
    }
    // **Finished lights writes**, completed here and dropped in thread context, as reads are: the
    // operation is completed by pointer, and the reference stays in its slot until reclaimed.
    // **Taken under the lock and completed outside it**, as a read is delivered: completing takes
    // the scheduler's lock, which ranks above this leaf. The slot says `Completing` meanwhile, so
    // thread context cannot drop the operation before it is completed.
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
        crate::sched::complete_pending_op(po, status, (status == 0) as u64);
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

/// Drop any read the DPC has finished with. **Thread context only.**
///
/// This is where the two `ObjectRef`s a completed read pinned are actually released — the
/// frees that `ps2_intr_dpc` must not do. Called from `sched::reap_pending` (so any CPU
/// going idle reclaims) and from [`submit_read`] before parking a new read.
///
/// Safe to run concurrently with a DPC on another CPU: the DPC owns its entry outright until
/// it publishes it, so there is nothing here to race with.
pub fn reclaim_completed() {
    for index in 0..DEV_COUNT {
        // Take under the lock, drop outside it: a drop reaches the allocator.
        let owed = PS2.lock().devices[index].take_owed();
        drop(owed);
    }
    // And the lights writes the DPC completed; then a write that waited for a slot, and the
    // write that waited behind it.
    let mut done: [Option<LightsDone>; 4] = [const { None }; 4];
    let started = {
        let mut g = PS2.lock();
        for (slot, out) in g.lights_done.iter_mut().zip(done.iter_mut()) {
            if slot.as_ref().is_some_and(|d| d.state == Completion::Completed) {
                *out = slot.take();
            }
        }
        let waited = g.lights_unreported.is_some();
        g.report_lights();
        if waited && g.lights_unreported.is_none() && g.lights.is_none()
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

/// [`CharBackend::submit_write`] for the keyboard's node: **its lights** (Phase 6 Part B.5), one
/// byte in HID's order. Completed when the keyboard has acknowledged them, or failed. A write that
/// comes while another is in flight waits for it, and replaces one already waiting, which is then
/// completed as done: the lights it would have set are older than the ones that will be.
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
        crate::sched::complete_pending_op(old.as_ptr(), 0, 1);
        drop(old);
    }
    // A start that failed at once has a write for the DPC to complete.
    crate::dpc::enqueue(&PS2_DPC);
    Ok(())
}

/// The key the `fbcon-gate` feature panics on: F10, the tenth of the consecutive function keys.
#[cfg(feature = "fbcon-gate")]
const CRASH_KEY: u16 = crate::libkern::input::KEY_F1 + 9;

/// Drain the controller into the rings. Shared by both ISRs, because **both ports deliver
/// through the same data port**: whichever line fires, the byte waiting might belong to
/// either device, and the status bit is the only discriminator. Draining everything from
/// either handler is therefore both correct and necessary.
///
/// Returns whether any device now has a parked reader to wake.
fn drain_controller() -> bool {
    let now = crate::arch::Timer::read_ns();
    let mut g = PS2.lock();
    // **Bounded.** The lock masks interrupts, so an unbounded drain is an unbounded
    // interrupts-off window: a device that streams without pause — a mouse left reporting
    // after a bad reset, a keyboard repeating a stuck key — would hold this CPU forever and
    // the machine simply stops. The console's equivalent loop is unbounded and gets away
    // with it because a UART with nothing arriving stops immediately; an input controller
    // with two attached devices is not that.
    //
    // **Anything still waiting is collected by [`poll`], not by the next interrupt.** This
    // comment used to say the opposite — "the controller will raise because the byte is still
    // there" — and that is false for this device: it raises when a byte *becomes* available,
    // so a byte already sitting in the output buffer produces no further edge, and the buffer
    // stays full forever. That sentence cost three investigations; the tick-driven sweep is
    // what actually recovers the byte.
    let mut budget = MAX_DRAIN_PER_IRQ;
    let mut lights_finished = false;
    // A `ps2-hold-gate` hold reads nothing at all, so everything injected meanwhile queues in
    // the host's device — the condition that gate exists to build.
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
                        // **With this leaf lock held, deliberately**: a driver that panics
                        // usually holds its own lock, and the panic's message must still reach
                        // the screen. Until the rank tracker stopped ordering `try_lock`, the
                        // panic path's tee into the log ring tripped it here and the screen
                        // showed `lock-order violation: acquiring Klog (rank 72) while holding
                        // Leaf (rank 90)` instead — so `check-fbcon` is that fix's regression
                        // test as well as the console's.
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
                    let value = if pressed {
                        crate::libkern::input::KEY_PRESS
                    } else {
                        crate::libkern::input::KEY_RELEASE
                    };
                    let group =
                        [InputEvent::key(code, value, now), InputEvent::syn(now)];
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

/// Collect anything the interrupt path missed. Called from the timer IRQ dispatcher, ahead of
/// the DPC drain so a byte recovered here wakes its reader on this tick rather than the next.
///
/// **This exists because an i8042 interrupt can be lost, and the loss is unrecoverable
/// without it.** The controller has a *one-byte* output buffer and its IRQ line is a level
/// that the interrupt controller turns into an edge. A byte that arrives after a drain's last
/// status read but while the line is still asserted produces **no new edge** — and because
/// nobody then reads the buffer, the line never drops, so no later byte can produce one
/// either. The device and the driver deadlock: a byte sits in the buffer forever and every
/// keystroke and mouse movement after it is discarded by a full controller.
///
/// That is not hypothetical. It is what made `check-input` and `check-terminal` intermittent
/// through Milestones 5 and 6, and the state was captured directly: `status=0x1d` (output full,
/// keyboard byte) with the ISR counters frozen and the last drain having exited on
/// `status=0x1c` (output *empty*) — the driver did everything right and was never called again.
///
/// **The race cannot be closed by re-checking harder**: however many times the drain re-reads
/// the status, there is a last read, and a byte can always arrive after it. A periodic sweep is
/// the standard answer — Linux's i8042 carries a polling timer for the same class of fault —
/// and one `inb` per tick is not a cost worth optimising.
pub fn poll() {
    // **A machine with no i8042 must not pay for one.** An absent controller floats its
    // status port high, so `output_pending` reads `0xFF` — which has `STATUS_OUTPUT_FULL`
    // set — and an unguarded sweep would take the lock and run the full drain budget on
    // every tick, forever: 128 port reads per tick per CPU with interrupts masked, buying
    // nothing. Worse, `0xFF` also has the mouse packet's sync bit set, so three of them
    // decode as a valid three-button-down packet. `arch::ps2::init` promises such a machine
    // "reports both absent and the boot continues"; this is what keeps that true.
    if !PRESENT.load(Ordering::Acquire) {
        return;
    }
    // **A lights exchange the keyboard has not answered** fails on the tick after its bound.
    if LIGHTS_IN_FLIGHT.load(Ordering::Acquire) {
        let now = crate::arch::Timer::read_ns();
        // The DPC is queued after the lock is let go: its queue's lock is a leaf too.
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

/// `true` once a keyboard has answered and its interrupt is armed — the only case in which this
/// driver can move [`input::key_presses`].
pub fn keyboard_present() -> bool {
    PRESENT.load(Ordering::Acquire) && crate::device::has(crate::libkern::device::DeviceKind::Keyboard)
}

/// Throw away every keyboard event waiting in the ring. Returns how many bytes of records went.
///
/// For keys pressed before anyone could have meant them for a program — the hardware report's
/// page turns — so the first reader of `/dev/input/raw/0` does not receive them. Thread context;
/// no reader is parked this early, so there is nothing to complete.
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

/// Bring up the i8042 and publish its device nodes. Call once at boot, after the interrupt
/// router is initialised and with interrupts masked — [`crate::arch::ps2::init`] polls, and
/// must consume every controller response before the scancode decoder can see a byte.
///
/// Logs a one-line result; not a `panic!` path. A machine with no i8042 publishes no nodes
/// and boots normally.
pub fn init() {
    // SAFETY: ring-0 at boot, before `arm`, and nothing else touches 0x60/0x64.
    let present = unsafe { crate::arch::ps2::init() };
    if !present.keyboard && !present.mouse {
        crate::kprintln!("ps2: no i8042 devices answered (no /dev/input/raw/*)");
        return;
    }
    if present.wheel {
        // **The decoder cannot work this out for itself**, and it is not a feature flag: a
        // mouse that answered the knock sends four-byte packets from now on, so a decoder
        // still framing three would read every one of them at an offset. Done here rather
        // than inside `arch::ps2::init` because the packet layout is this module's — the arch
        // layer owns ports and interrupts, not what the bytes mean.
        //
        // Safe without ordering care: `PRESENT` is still false and the interrupts are not
        // armed, so nothing can be feeding the decoder yet.
        PS2.lock().mouse.enable_wheel();
    }

    for (index, present) in [(DEV_KEYBOARD, present.keyboard), (DEV_MOUSE, present.mouse)] {
        if !present {
            continue;
        }
        let backend = CharBackend { submit_read, submit_write: Some(submit_write), ctx: index as *mut () };
        match DeviceNode::try_new_char(ResourceDescriptor::ZERO, backend) {
            Ok(node) => {
                // **Owned by the device table**, which `/dev/input/raw/<n>` resolves through:
                // the index is this driver's, and the table records it as the node's served
                // index, which is also what `/dev/registry` reports (administration Part B).
                // SAFETY: `into_raw` yields the single creation reference; the table adopts it.
                let r = unsafe {
                    ObjectRef::from_raw(KBox::into_raw(node).as_ptr() as *mut (), KObjectType::DeviceNode)
                };
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
    // controller's and not a floating bus.
    //
    // **Last, and that ordering is load-bearing in the other direction.** Setting this before
    // `arch::ps2::init`'s polled handshake would let a tick's sweep consume the controller's
    // own `0xFA`/`0xAA` replies as if they were scancodes — the phantom-modifier failure this
    // module's header warns about. Today no tick can land here at all (the periodic timer
    // starts in `sched_bringup`, after this runs), so the window is unreachable rather than
    // merely harmless; storing last is what keeps it unreachable if that ever changes.
    PRESENT.store(true, Ordering::Release);
    crate::kprintln!(
        "ps2: {}{}{}armed (kbd vec{:#x}, aux vec{:#x})",
        if present.keyboard { "keyboard " } else { "" },
        if present.mouse { "mouse " } else { "" },
        // Logged because it changes how every mouse packet is *framed*, so a boot where the
        // knock silently failed and one where it worked must not look the same in a transcript.
        if present.wheel { "wheel " } else { "" },
        kbd_vec,
        aux_vec
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libkern::input::{KEY_UP, LIGHT_CAPS};

    fn decoded(b: KeyboardByte) -> Option<scancode::Decoded> {
        match b {
            KeyboardByte::Key(d) => Some(d),
            KeyboardByte::Answer(_) => None,
        }
    }

    /// **An acknowledgement between an `E0` prefix and its code is the exchange's, not a key**
    /// (Phase 6 Part B.5; PR #357 review). Taken by the decoder, `E0 FA 48` — Up with an answer in
    /// the middle — is keypad 8 pressed and never released, which is what the second half shows
    /// the decoder alone still does.
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
