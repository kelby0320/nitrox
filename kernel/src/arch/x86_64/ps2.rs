//! The **i8042** controller's platform wiring: port I/O and interrupt arming.
//!
//! The x86-only half of `drivers/ps2`: ports `0x60`/`0x64` and ISA IRQs 1 and 12.
//!
//! [`init`] runs with interrupts masked and talks to the controller **synchronously**, and only
//! then does [`arm`] route the interrupts. `0xAA`, a device's passed-self-test byte, is also
//! `0x2A | 0x80`, a Left Shift **release**, indistinguishable in the stream; so every controller
//! response must be consumed before any byte can reach the scancode decoder.
//!
//! The config byte is written once: the mouse enable is a read-modify-write of the byte that
//! carries the keyboard's IRQ-1 enable, and two writers would race and lose a bit.

use super::ioapic;
use super::regs::{inb, outb};

const DATA: u16 = 0x60;
const CMD: u16 = 0x64;

const STATUS_OUTPUT_FULL: u8 = 0x01;
const STATUS_INPUT_FULL: u8 = 0x02;
const STATUS_FROM_AUX: u8 = 0x20;

const CMD_READ_CONFIG: u8 = 0x20;
const CMD_WRITE_CONFIG: u8 = 0x60;
const CMD_DISABLE_KBD: u8 = 0xAD;
const CMD_ENABLE_KBD: u8 = 0xAE;
const CMD_DISABLE_AUX: u8 = 0xA7;
const CMD_ENABLE_AUX: u8 = 0xA8;
const CMD_WRITE_AUX: u8 = 0xD4;
const CMD_PULSE_RESET: u8 = 0xFE;

const CONFIG_KBD_IRQ: u8 = 0x01;
const CONFIG_AUX_IRQ: u8 = 0x02;
/// Config bit: the controller translates set 2 to set 1. **Kept on**: `scancode.rs` decodes set 1.
const CONFIG_TRANSLATE: u8 = 0x40;
const CONFIG_KBD_CLOCK_OFF: u8 = 0x10;
const CONFIG_AUX_CLOCK_OFF: u8 = 0x20;

const DEV_RESET: u8 = 0xFF;
const DEV_ENABLE_REPORTING: u8 = 0xF4;
const DEV_SET_SAMPLE_RATE: u8 = 0xF3;
const DEV_GET_DEVICE_ID: u8 = 0xF2;

/// The sample-rate sequence that asks a mouse to become an IntelliMouse: a device that
/// recognises it changes its id from `0x00` to `0x03` and sends **four**-byte packets.
const WHEEL_KNOCK: [u8; 3] = [200, 100, 80];

const ID_WHEEL: u8 = 0x03;

/// The rate to leave the mouse at, restored after the knock leaves it at 80 Hz.
const SAMPLE_RATE: u8 = 100;

const KBD_IRQ: u8 = 1;
const AUX_IRQ: u8 = 12;

/// Spin bound for a controller handshake: a dead or absent i8042 must not hang the boot.
const SPIN_LIMIT: u32 = 100_000;

/// Which port a byte came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Port {
    /// The keyboard port (IRQ 1).
    Keyboard,
    /// The aux/mouse port (IRQ 12).
    Aux,
}

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

fn command(byte: u8) -> bool {
    if !wait_writable() {
        return false;
    }
    // SAFETY: ring-0 write of an i8042 controller command.
    unsafe { outb(CMD, byte) };
    true
}

fn write_data(byte: u8) -> bool {
    if !wait_writable() {
        return false;
    }
    // SAFETY: ring-0 write of the i8042 data port.
    unsafe { outb(DATA, byte) };
    true
}

/// **Send `byte` to the keyboard**: a device command or its operand, answered through
/// [`read_byte`] as a [`Port::Keyboard`] byte. `false` if the controller did not take it in time.
pub fn send_keyboard(byte: u8) -> bool {
    write_data(byte)
}

fn read_data() -> Option<u8> {
    if !wait_readable() {
        return None;
    }
    // SAFETY: ring-0 read of the i8042 data port.
    Some(unsafe { inb(DATA) })
}

/// Discard every byte the controller holds, bounded so a streaming device cannot wedge the boot.
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

fn aux_command(byte: u8) -> Option<u8> {
    if !command(CMD_WRITE_AUX) || !write_data(byte) {
        return None;
    }
    read_data()
}

/// Read one byte and say which device produced it.
///
/// **Both ports deliver through the same data port**, so the status bit is the only way to tell
/// a scancode from a mouse byte; reading clears the interrupt condition.
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

/// **Pulse the reset line**: the second way a reboot tries, after the FADT's reset register.
///
/// A machine with no 8042 reads `0xFF` from the status port (input-full for ever), so the
/// bounded wait gives up and the write goes nowhere; this therefore runs whatever
/// `IAPC_BOOT_ARCH` says, as firmware gets that bit wrong both ways.
///
/// # Safety
/// Ring 0, with every other processor stopped: when it works, it resets the machine.
pub unsafe fn pulse_reset() {
    for _ in 0..100_000 {
        // SAFETY: ring-0 read of the i8042 status port; reading it has no side effects.
        if unsafe { inb(CMD) } & STATUS_INPUT_FULL == 0 {
            break;
        }
        core::hint::spin_loop();
    }
    // SAFETY: ring 0, and the caller has stopped the machine for exactly this.
    unsafe { outb(CMD, CMD_PULSE_RESET) };
}

/// Whether the controller has a byte nobody has collected (a side-effect-free status read).
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
    /// That mouse answered the IntelliMouse knock and sends **four**-byte packets; the decoder
    /// must be told, as a three-byte one reads every packet out of phase.
    pub wheel: bool,
}

/// Bring the controller up, synchronously and with interrupts still masked.
///
/// Returns which devices answered. Never panics: a machine with no i8042 reports both absent.
///
/// # Safety
///
/// Ring-0, at boot, before [`arm`] and before any other code touches ports `0x60`/`0x64`.
pub unsafe fn init() -> Present {
    let mut found = Present::default();

    // Silence both ports so no device deposits a byte mid-handshake.
    command(CMD_DISABLE_KBD);
    command(CMD_DISABLE_AUX);
    flush();

    // Interrupts stay off until `arm`; the clocks go on now, as a device cannot answer its
    // reset with its clock disabled.
    if !command(CMD_READ_CONFIG) {
        return found;
    }
    let Some(config) = read_data() else { return found };
    let new = (config | CONFIG_TRANSLATE)
        & !(CONFIG_KBD_IRQ | CONFIG_AUX_IRQ)
        & !(CONFIG_KBD_CLOCK_OFF | CONFIG_AUX_CLOCK_OFF);
    if !command(CMD_WRITE_CONFIG) || !write_data(new) {
        return found;
    }

    command(CMD_ENABLE_KBD);
    command(CMD_ENABLE_AUX);

    if write_data(DEV_RESET) {
        // Ack (0xFA) and self-test (0xAA), in either order on some controllers.
        let a = read_data();
        let b = read_data();
        found.keyboard = matches!(a, Some(0xFA) | Some(0xAA)) || matches!(b, Some(0xAA));
    }

    if let Some(ack) = aux_command(DEV_RESET) {
        let a = read_data();
        let b = read_data();
        found.mouse = ack == 0xFA && (matches!(a, Some(0xAA)) || matches!(b, Some(0xAA)));
        if found.mouse {
            // Ask for the wheel **before reporting is enabled**, or movement packets would
            // interleave with the knock and id query.
            found.wheel = knock_for_wheel();
            let _ = aux_command(DEV_ENABLE_REPORTING);
        }
    }

    flush();
    found
}

/// Send the [`WHEEL_KNOCK`] rates, read the device id back, and say whether the mouse has a wheel.
///
/// A device that recognises the sequence switches to four-byte packets when the third rate is
/// acknowledged, not when its id is read, so a later failure would leave a four-byte mouse
/// behind a three-byte decoder. Hence **anything but a confirmed [`ID_WHEEL`] after that point
/// resets the device**, restoring three-byte packets and the default rate.
fn knock_for_wheel() -> bool {
    for rate in WHEEL_KNOCK {
        if aux_command(DEV_SET_SAMPLE_RATE) != Some(0xFA) || aux_command(rate) != Some(0xFA) {
            return false;
        }
    }
    if aux_command(DEV_GET_DEVICE_ID) != Some(0xFA) || read_data() != Some(ID_WHEEL) {
        return reset_to_three_bytes();
    }
    // Undo the knock's 80 Hz; every other outcome was reset to the default rate.
    if aux_command(DEV_SET_SAMPLE_RATE) == Some(0xFA) {
        let _ = aux_command(SAMPLE_RATE);
    }
    true
}

/// Reset the mouse (three-byte packets, default rate, reporting off) and consume its replies.
/// Always `false`: "no wheel".
fn reset_to_three_bytes() -> bool {
    if aux_command(DEV_RESET) == Some(0xFA) {
        // The self-test byte and the device id; a `0xAA` reaching the decoder would be a
        // phantom Left Shift release.
        let _ = read_data();
        let _ = read_data();
    }
    false
}

/// Route the keyboard and aux interrupts to `kbd`/`aux` and enable them in the controller,
/// returning the assigned IDT vectors, `None` for a line that cannot be routed (polled instead).
///
/// # Safety
///
/// Ring-0, after the interrupt router is initialised and after [`init`]; both handlers must
/// stay valid for the kernel's lifetime, and the caller must be ready to receive the
/// interrupts once they are unmasked.
pub unsafe fn arm(kbd: extern "C" fn(), aux: extern "C" fn()) -> (Option<u8>, Option<u8>) {
    // SAFETY: forwarded from this fn's contract. `install_isa_irq` is the x86 IOAPIC
    // routing primitive, kept inside the arch layer (`arch/mod.rs` refuses to surface it).
    let kbd_vec = unsafe { ioapic::install_isa_irq(KBD_IRQ, kbd) };
    // SAFETY: as above, for the aux line.
    let aux_vec = unsafe { ioapic::install_isa_irq(AUX_IRQ, aux) };

    // Enable the controller's interrupt generation last, so neither line fires before both
    // handlers are routed.
    //
    // **Skipped under `no-ps2-irq`**: the controller then never asserts either line, so every
    // byte is recovered by the tick-driven `ps2::poll` sweep, which exists because the i8042's
    // one-byte output buffer loses edges. `cargo xtask check-input --no-ps2-irq` uses it.
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
