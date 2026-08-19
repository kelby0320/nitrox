//! The **i8042** controller's platform wiring: port I/O and interrupt arming.
//!
//! The x86-only half of `drivers/ps2`. Ports `0x60`/`0x64` and ISA IRQs 1 and 12 are as
//! arch-specific as COM1's registers, so they live here for the same reason and behind the
//! same shape of interface: `arch/mod.rs` refuses to re-export `install_isa_irq` neutrally
//! because "ISA" is x86 jargon, and *"a fixed legacy platform device wires its own interrupt
//! inside the arch layer"*. The neutral verb is "arm the keyboard's interrupt", not "install
//! ISA IRQ 1".
//!
//! ## Bring-up order, and why it is polled
//!
//! [`init`] runs with interrupts masked and talks to the controller **synchronously**:
//! disable both ports, flush, program the config byte once, enable the ports, reset each
//! device and consume its response. Only then does [`arm`] route the interrupts.
//!
//! That ordering is not stylistic. `0xAA` — the byte a device sends to report a passed
//! self-test — is also `0x2A | 0x80`, an ordinary Left Shift **release**. The two are
//! indistinguishable in the stream, so the *only* way to tell them apart is time: consume
//! every controller response before any byte can reach the scancode decoder. Arming first
//! would deliver a phantom Shift release on every boot, and worse, `drivers/ps2` would have
//! to special-case `0xAA` forever and swallow every real one.
//!
//! ## One config byte, written once
//!
//! Enabling the mouse is a read-modify-write of the same byte that carries the keyboard's
//! IRQ-1 enable. Two drivers doing that independently race and lose one of the bits — a
//! machine that intermittently boots with a dead keyboard. There is one driver and one
//! write.

use super::ioapic;
use super::regs::{inb, outb};

/// Data port: scancodes and mouse packets in, device commands out.
const DATA: u16 = 0x60;
/// Status (read) / command (write) port.
const CMD: u16 = 0x64;

/// Status bit: output buffer full — a byte is waiting in [`DATA`].
const STATUS_OUTPUT_FULL: u8 = 0x01;
/// Status bit: input buffer full — the controller has not consumed our last write.
const STATUS_INPUT_FULL: u8 = 0x02;
/// Status bit: the waiting byte came from the **aux** (mouse) port, not the keyboard.
const STATUS_FROM_AUX: u8 = 0x20;

/// Controller command: read the configuration byte.
const CMD_READ_CONFIG: u8 = 0x20;
/// Controller command: write the configuration byte.
const CMD_WRITE_CONFIG: u8 = 0x60;
/// Controller command: disable the keyboard port.
const CMD_DISABLE_KBD: u8 = 0xAD;
/// Controller command: enable the keyboard port.
const CMD_ENABLE_KBD: u8 = 0xAE;
/// Controller command: disable the aux port.
const CMD_DISABLE_AUX: u8 = 0xA7;
/// Controller command: enable the aux port.
const CMD_ENABLE_AUX: u8 = 0xA8;
/// Controller command: the **next** data-port write goes to the aux device.
const CMD_WRITE_AUX: u8 = 0xD4;

/// Config bit: raise IRQ 1 when the keyboard has a byte.
const CONFIG_KBD_IRQ: u8 = 0x01;
/// Config bit: raise IRQ 12 when the aux device has a byte.
const CONFIG_AUX_IRQ: u8 = 0x02;
/// Config bit: the controller translates set 2 to set 1 for us. **Kept on** — `scancode.rs`
/// decodes set 1, which is what every PC BIOS leaves enabled.
const CONFIG_TRANSLATE: u8 = 0x40;
/// Config bit: the keyboard clock is *disabled* when set.
const CONFIG_KBD_CLOCK_OFF: u8 = 0x10;
/// Config bit: the aux clock is *disabled* when set.
const CONFIG_AUX_CLOCK_OFF: u8 = 0x20;

/// Device command: reset and run the built-in self-test.
const DEV_RESET: u8 = 0xFF;
/// Device command: enable reporting (the mouse needs it; it boots silent).
const DEV_ENABLE_REPORTING: u8 = 0xF4;

/// ISA IRQ the keyboard port raises.
const KBD_IRQ: u8 = 1;
/// ISA IRQ the aux port raises.
const AUX_IRQ: u8 = 12;

/// Spin bound for a controller handshake. The i8042 is a microcontroller on a slow bus; a
/// dead or absent one must not hang the boot, so every wait is bounded and failure is
/// reported rather than retried forever.
const SPIN_LIMIT: u32 = 100_000;

/// Which port a byte came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Port {
    /// The keyboard port (IRQ 1).
    Keyboard,
    /// The aux/mouse port (IRQ 12).
    Aux,
}

/// Wait until the controller has consumed our previous write. Returns `false` on timeout.
fn wait_writable() -> bool {
    for _ in 0..SPIN_LIMIT {
        // SAFETY: ring-0 read of the i8042 status port.
        if unsafe { inb(CMD) } & STATUS_INPUT_FULL == 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// Wait until a byte is waiting to be read. Returns `false` on timeout.
fn wait_readable() -> bool {
    for _ in 0..SPIN_LIMIT {
        // SAFETY: ring-0 read of the i8042 status port.
        if unsafe { inb(CMD) } & STATUS_OUTPUT_FULL != 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// Write a controller command to [`CMD`].
fn command(byte: u8) -> bool {
    if !wait_writable() {
        return false;
    }
    // SAFETY: ring-0 write of an i8042 controller command.
    unsafe { outb(CMD, byte) };
    true
}

/// Write a byte to [`DATA`] (a device command, or the operand of a controller command).
fn write_data(byte: u8) -> bool {
    if !wait_writable() {
        return false;
    }
    // SAFETY: ring-0 write of the i8042 data port.
    unsafe { outb(DATA, byte) };
    true
}

/// Read [`DATA`], waiting for a byte. `None` on timeout.
fn read_data() -> Option<u8> {
    if !wait_readable() {
        return None;
    }
    // SAFETY: ring-0 read of the i8042 data port.
    Some(unsafe { inb(DATA) })
}

/// Discard every byte the controller currently holds. Bounded, so a device that streams
/// continuously cannot wedge the boot here.
fn flush() {
    for _ in 0..16 {
        // SAFETY: ring-0 read of the i8042 status port.
        if unsafe { inb(CMD) } & STATUS_OUTPUT_FULL == 0 {
            return;
        }
        // SAFETY: ring-0 read of the i8042 data port, discarding the byte.
        let _ = unsafe { inb(DATA) };
    }
}

/// Send a command to the aux device (prefixed with [`CMD_WRITE_AUX`]) and read its ack.
fn aux_command(byte: u8) -> Option<u8> {
    if !command(CMD_WRITE_AUX) || !write_data(byte) {
        return None;
    }
    read_data()
}

/// Read one byte and say which device produced it.
///
/// Called from the ISRs, where the status bit is the only way to tell a scancode from a
/// mouse packet byte: **both ports deliver through the same data port**, and reading it
/// clears the interrupt condition for whichever one it was.
pub fn read_byte() -> Option<(Port, u8)> {
    // SAFETY: ring-0 read of the i8042 status port.
    let status = unsafe { inb(CMD) };
    if status & STATUS_OUTPUT_FULL == 0 {
        return None;
    }
    let port = if status & STATUS_FROM_AUX != 0 { Port::Aux } else { Port::Keyboard };
    // SAFETY: ring-0 read of the i8042 data port; a byte is present per the status read.
    Some((port, unsafe { inb(DATA) }))
}

/// Whether the controller has a byte nobody has collected.
///
/// A status read with no side effects — it neither consumes the byte nor acknowledges
/// anything, so it is safe to call from any context that can afford one `inb`.
pub fn output_pending() -> bool {
    // SAFETY: ring-0 read of the i8042 status port; reading it has no side effects.
    unsafe { inb(CMD) & STATUS_OUTPUT_FULL != 0 }
}

/// What [`init`] found.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Present {
    /// A keyboard port that answered its reset.
    pub keyboard: bool,
    /// An aux port that answered its reset — a mouse.
    pub mouse: bool,
}

/// Bring the controller up, synchronously and with interrupts still masked.
///
/// Returns which devices answered. Never panics: a machine with no i8042 at all (or a
/// virtualised one that ignores the port) reports both absent and the boot continues.
///
/// # Safety
///
/// Ring-0, at boot, before [`arm`] and before any other code touches ports `0x60`/`0x64`.
pub unsafe fn init() -> Present {
    let mut found = Present::default();

    // 1. Silence both ports before touching anything else, so no device can deposit a byte
    //    into the middle of the configuration handshake below.
    command(CMD_DISABLE_KBD);
    command(CMD_DISABLE_AUX);
    flush();

    // 2. Program the config byte **once**. Interrupts stay off until `arm`; the clocks go
    //    on now because a device cannot answer its reset with its clock disabled.
    if !command(CMD_READ_CONFIG) {
        return found;
    }
    let Some(config) = read_data() else { return found };
    let new = (config | CONFIG_TRANSLATE) & !(CONFIG_KBD_IRQ | CONFIG_AUX_IRQ)
        & !(CONFIG_KBD_CLOCK_OFF | CONFIG_AUX_CLOCK_OFF);
    if !command(CMD_WRITE_CONFIG) || !write_data(new) {
        return found;
    }

    // 3. Enable the ports and reset each device, consuming its response **here** — the
    //    whole reason bring-up is polled. `0xAA` (self-test passed) is byte-identical to a
    //    Left Shift release, so it must never reach the scancode decoder.
    command(CMD_ENABLE_KBD);
    command(CMD_ENABLE_AUX);

    if write_data(DEV_RESET) {
        // Ack (0xFA) then self-test result (0xAA), in either order on some controllers.
        let a = read_data();
        let b = read_data();
        found.keyboard = matches!(a, Some(0xFA) | Some(0xAA)) || matches!(b, Some(0xAA));
    }

    if let Some(ack) = aux_command(DEV_RESET) {
        let a = read_data();
        let b = read_data();
        // 0xFA ack, then 0xAA self-test, then the device id (0x00 for a plain mouse).
        found.mouse = ack == 0xFA && (matches!(a, Some(0xAA)) || matches!(b, Some(0xAA)));
        if found.mouse {
            // A PS/2 mouse boots with reporting **off** and says nothing until told.
            let _ = aux_command(DEV_ENABLE_REPORTING);
        }
    }

    flush();
    found
}

/// Route the keyboard and aux interrupts to `kbd`/`aux` and enable them in the controller.
///
/// Returns the assigned IDT vectors. The neutral operation is "arm the input controller's
/// interrupts" — the platform wiring (which ISA IRQ, how routed) stays here, exactly as
/// `console_arm_rx` hides COM1's IRQ 4.
///
/// # Safety
///
/// Ring-0, after the interrupt router is initialised and after [`init`]; both handlers must
/// stay valid for the kernel's lifetime, and the caller must be ready to receive the
/// interrupts once they are unmasked.
pub unsafe fn arm(kbd: extern "C" fn(), aux: extern "C" fn()) -> (u8, u8) {
    // SAFETY: forwarded from this fn's contract. `install_isa_irq` is the x86 IOAPIC
    // routing primitive, kept inside the arch layer (`arch/mod.rs` refuses to surface it).
    let kbd_vec = unsafe { ioapic::install_isa_irq(KBD_IRQ, kbd) };
    // SAFETY: as above, for the aux line.
    let aux_vec = unsafe { ioapic::install_isa_irq(AUX_IRQ, aux) };

    // Enable the controller's interrupt generation last, so neither line can fire before
    // both handlers are routed.
    //
    // **Skipped under `no-ps2-irq`**, which is the whole of that feature: the vectors above are
    // still routed and both devices are still enabled, but the controller never asserts either
    // line, so every byte has to be recovered by the tick-driven `ps2::poll` sweep. That sweep
    // exists because the i8042's one-byte output buffer loses edges (2026-08-13), and nothing
    // else in the tree can fail when it is deleted — on healthy hardware the interrupt path
    // carries every byte and the sweep is pure redundancy. `cargo xtask check-input
    // --no-ps2-irq` makes the redundancy load-bearing so its absence is observable.
    #[cfg(not(feature = "no-ps2-irq"))]
    if command(CMD_READ_CONFIG)
        && let Some(config) = read_data()
    {
        let with_irqs = config | CONFIG_KBD_IRQ | CONFIG_AUX_IRQ;
        if command(CMD_WRITE_CONFIG) {
            write_data(with_irqs);
        }
    }
    (kbd_vec, aux_vec)
}
