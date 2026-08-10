//! The **i8042** controller driver (Tier 1) — PS/2 keyboard and mouse.
//!
//! `docs/design/input-subsystem.md`. One driver, because the keyboard and the mouse are two
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
//! This module owns what is portable: the scancode table, the mouse packet framing, and the
//! event rings.

pub mod mouse;
pub mod ring;
pub mod scancode;

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::arch::ps2::Port;
use crate::arch::timer::ArchTimer;
use crate::dpc::Dpc;
use crate::libkern::IrqSpinLock;
use crate::libkern::KBox;
use crate::libkern::handle::KObjectType;
use crate::libkern::input::{INPUT_EVENT_LEN, InputEvent};
use crate::libkern::lockrank::LockRank;
use crate::mm::{PAGE_SIZE, heap};
use crate::object::device_node::{CharBackend, ResourceDescriptor};
use crate::object::{DeviceNode, MemoryObject, ObjectRef};
use crate::syscall::error::KError;
use ring::EventRing;

/// The two devices this controller publishes, and their `/dev/input/raw/<n>` indices.
pub const DEV_KEYBOARD: usize = 0;
/// Index of the mouse node.
pub const DEV_MOUSE: usize = 1;
/// How many raw nodes exist.
pub const DEV_COUNT: usize = 2;

/// A parked `sys_io_submit(Read)` waiting for events on one device.
struct ParkedRead {
    po: ObjectRef,
    buffer: ObjectRef,
    buf_offset: u64,
    max_len: usize,
}

/// Per-device state: its ring and the single reader waiting on it.
struct Device {
    ring: EventRing,
    parked: Option<ParkedRead>,
    /// Set when the DPC has satisfied `parked` and thread context still owes it a drop.
    ///
    /// The `ParkedRead` deliberately stays in place once spent. Taking it out of the DPC
    /// would drop its two `ObjectRef`s there, and a last-reference drop runs
    /// `dispatch_destroy` → `SlabCache::free`, whose lock is a plain `SpinLock` — the
    /// same-CPU deadlock fixed for `io::block` (decision log, 2026-08-06). Leaving it owned
    /// here keeps the objects alive for free until [`reclaim_completed`] can drop them in
    /// thread context.
    spent: bool,
}

impl Device {
    const fn new() -> Self {
        Self { ring: EventRing::new(), parked: None, spent: false }
    }
}

/// Everything the driver owns, behind one lock.
///
/// **One lock for both devices, deliberately.** They share the controller: a single status
/// read decides which port a byte came from, so the two ISRs cannot be made independent
/// anyway, and two locks would only add an ordering rule to get wrong.
struct Inner {
    devices: [Device; DEV_COUNT],
    keys: scancode::Decoder,
    mouse: mouse::Decoder,
}

impl Inner {
    const fn new() -> Self {
        Self {
            devices: [Device::new(), Device::new()],
            keys: scancode::Decoder::new(),
            mouse: mouse::Decoder::new(),
        }
    }
}

static PS2: IrqSpinLock<Inner> = IrqSpinLock::new(LockRank::Leaf, Inner::new());

/// Completes parked reads after an ISR deposits events.
static PS2_DPC: Dpc = Dpc::new(ps2_intr_dpc, core::ptr::null_mut());

/// The leaked-`'static` device nodes, indexed by `DEV_*`. `device_ref` hands out counted
/// references for `/dev/input/raw/<n>` lookups.
static NODES: [AtomicPtr<()>; DEV_COUNT] =
    [AtomicPtr::new(core::ptr::null_mut()), AtomicPtr::new(core::ptr::null_mut())];

/// A counted reference to raw input device `index`, or `None` if absent.
pub fn device_ref(index: usize) -> Option<ObjectRef> {
    let p = NODES.get(index)?.load(Ordering::Acquire);
    if p.is_null() {
        return None;
    }
    // SAFETY: `NODES[index]` points at a leaked-`'static` `DeviceNode` whose creation
    // reference is never released, so the object is live.
    unsafe { ObjectRef::try_acquire(p, KObjectType::DeviceNode) }
}

/// Copy `src` into `buffer`'s frames starting at byte `buf_offset`, via the HHDM. The
/// caller has bounds-checked the range (`sys_io_submit` does). Runs outside the lock.
///
/// # Safety
///
/// `buffer` must point at a live `MemoryObject` — held alive by an `ObjectRef` the caller
/// keeps for the duration. The DPC passes a *borrowed* pointer precisely so it never owns a
/// reference it would then have to drop.
unsafe fn copy_into_memobj(buffer: *const (), buf_offset: u64, src: &[u8]) {
    // SAFETY: the caller guarantees `buffer` pins a live `MemoryObject`.
    let mo: &MemoryObject = unsafe { &*(buffer as *const MemoryObject) };
    let frames = mo.frames();
    let hhdm = heap::hhdm_offset();
    let mut pos = buf_offset as usize;
    for &b in src {
        let page = pos / PAGE_SIZE;
        let intra = pos % PAGE_SIZE;
        if page >= frames.len() {
            break;
        }
        let dst = (frames[page].as_u64() + hhdm) as *mut u8;
        // SAFETY: within an owned, HHDM-mapped buffer frame (bounds pre-checked).
        unsafe { *dst.add(intra) = b };
        pos += 1;
    }
}

/// Scratch for one drain. Sized to the ring so a large read is satisfied in one pass.
const DRAIN_MAX: usize = ring::RING_EVENTS * INPUT_EVENT_LEN;

/// Bytes one interrupt may take from the controller before yielding.
///
/// Generous for a burst — a held key repeats at ~30 Hz and a mouse reports at 100 Hz, so
/// nothing legitimate approaches it — while keeping the interrupts-off window bounded.
const MAX_DRAIN_PER_IRQ: u32 = 64;

/// [`CharBackend::submit_read`] for a raw input node: satisfy immediately if the ring has
/// anything, else park until the next interrupt.
///
/// **`max_len` is floored to a whole number of records.** A reader asking for 24 bytes gets
/// one 16-byte event, not one and a half — the record has no sync word, so a partial tail
/// would misalign everything after it (`input-subsystem.md` §3a).
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
    let max_len = ((max_len as usize) / INPUT_EVENT_LEN * INPUT_EVENT_LEN).min(DRAIN_MAX);
    if max_len == 0 {
        // Too small for even one record. Completing with zero would look like end-of-stream
        // to a reader that simply passed a short buffer.
        return Err(KError::InvalidArgument);
    }
    // Thread context: release anything a previous completion left owed before parking.
    reclaim_completed();
    let now = crate::arch::Timer::read_ns();
    let mut tmp = [0u8; DRAIN_MAX];
    let drained = {
        let mut g = PS2.lock();
        let dev = &mut g.devices[index];
        if !dev.ring.is_empty() {
            Some(dev.ring.drain_into(&mut tmp[..max_len], now))
        } else if dev.parked.is_some() {
            return Err(KError::WouldBlock); // single reader per device
        } else {
            dev.parked = Some(ParkedRead {
                po: po.clone(),
                buffer: buffer.clone(),
                buf_offset,
                max_len,
            });
            None
        }
    };
    if let Some(n) = drained {
        // SAFETY: `buffer` is the caller's live `MemoryObject` reference, held across this.
        unsafe { copy_into_memobj(buffer.as_ptr(), buf_offset, &tmp[..n]) };
        crate::sched::complete_pending_op(po.as_ptr(), 0, n as u64);
    }
    Ok(())
}

/// DPC: complete any parked read whose device has events. Queued by the ISRs.
fn ps2_intr_dpc(_ctx: *mut ()) {
    let now = crate::arch::Timer::read_ns();
    for index in 0..DEV_COUNT {
        let mut tmp = [0u8; DRAIN_MAX];
        // **Borrow, never take.** The `ParkedRead` stays owned by `Device` and only its raw
        // pointers come out, so this DPC drops nothing — see `Device::spent`.
        let completed = {
            let mut g = PS2.lock();
            let dev = &mut g.devices[index];
            match dev.parked.as_ref() {
                Some(pr) if !dev.spent && !dev.ring.is_empty() => {
                    let (po, buffer, off, max) =
                        (pr.po.as_ptr(), pr.buffer.as_ptr(), pr.buf_offset, pr.max_len);
                    let n = dev.ring.drain_into(&mut tmp[..max.min(DRAIN_MAX)], now);
                    dev.spent = true;
                    Some((po, buffer, off, n))
                }
                _ => None,
            }
        };
        if let Some((po, buffer, buf_offset, n)) = completed {
            // SAFETY: `buffer` is still pinned by the `ObjectRef` left in `dev.parked`,
            // which only thread context removes.
            unsafe { copy_into_memobj(buffer, buf_offset, &tmp[..n]) };
            crate::sched::complete_pending_op(po, 0, n as u64);
        }
    }
}

/// Drop any `ParkedRead` the DPC has finished with. **Thread context only.**
///
/// This is where the two `ObjectRef`s a completed read pinned are actually released — the
/// frees that `ps2_intr_dpc` must not do. Called from `sched::reap_pending` (so any CPU
/// going idle reclaims) and from [`submit_read`] before parking a new read.
pub fn reclaim_completed() {
    for index in 0..DEV_COUNT {
        // Take under the lock, drop outside it: a drop reaches the allocator.
        let spent = {
            let mut g = PS2.lock();
            let dev = &mut g.devices[index];
            if dev.spent {
                dev.spent = false;
                dev.parked.take()
            } else {
                None
            }
        };
        drop(spent);
    }
}

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
    // with two attached devices is not that. Anything still waiting is picked up by the
    // next interrupt, which the controller will raise because the byte is still there.
    let mut budget = MAX_DRAIN_PER_IRQ;
    while budget > 0
        && let Some((port, byte)) = crate::arch::ps2::read_byte()
    {
        budget -= 1;
        match port {
            Port::Keyboard => {
                if let scancode::Decoded::Key { code, pressed } = g.keys.feed(byte) {
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
    g.devices.iter().any(|d| d.parked.is_some())
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

    for (index, present) in [(DEV_KEYBOARD, present.keyboard), (DEV_MOUSE, present.mouse)] {
        if !present {
            continue;
        }
        let backend = CharBackend { submit_read, ctx: index as *mut () };
        match DeviceNode::try_new_char(ResourceDescriptor::ZERO, backend) {
            Ok(node) => {
                // Leaked: the controller lives for the kernel's lifetime, and `device_ref`
                // hands out counted references off this pointer.
                let ptr = KBox::into_raw(node).as_ptr() as *mut ();
                NODES[index].store(ptr, Ordering::Release);
            }
            Err(_) => crate::kprintln!("ps2: device-node alloc FAIL for index {}", index),
        }
    }

    // SAFETY: ring-0, after `IrqRouter::init` and after `arch::ps2::init`; both handlers
    // are valid for the kernel's lifetime and the rings are ready to receive.
    let (kbd_vec, aux_vec) = unsafe { crate::arch::ps2::arm(kbd_isr, aux_isr) };
    crate::kprintln!(
        "ps2: {}{}armed (kbd vec{:#x}, aux vec{:#x})",
        if present.keyboard { "keyboard " } else { "" },
        if present.mouse { "mouse " } else { "" },
        kbd_vec,
        aux_vec
    );
}
