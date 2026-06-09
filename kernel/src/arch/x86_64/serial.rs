//! Polled 16550 UART driver for the COM1 serial port.
//!
//! The diagnostics slice's primary kernel output surface. The driver is
//! deliberately minimal: no interrupts, no allocation, no buffering.
//! `write_byte` busy-polls the line-status register and pushes one byte
//! at a time, which keeps it usable from the earliest point in boot and
//! from inside the panic and exception handlers.
//!
//! Two access paths exist:
//!
//! * [`SERIAL`] — the normal path: an [`IrqSpinLock`]-guarded port that the
//!   `kprint!` / `kprintln!` macros drive. It is an `IrqSpinLock` (masks
//!   interrupts while held) because a thread can be printing when the timer IRQ
//!   fires; the mask makes the print uninterruptible, so the handler can never
//!   find the lock held by the context it interrupted.
//! * [`emergency_writer`] — an *unsynchronised* path for the panic and
//!   exception handlers. The lock cannot be force-unlocked, so a handler that
//!   ran while [`SERIAL`] was locked would deadlock trying to take it. The
//!   emergency path constructs a throwaway [`SerialPort`] to the fixed COM1 port
//!   instead. This is sound only because Phase 1 is single-CPU: at fault time no
//!   other context can be driving the UART. It must be revisited at SMP.

use core::fmt;

use crate::arch::x86_64::regs;
use crate::libkern::IrqSpinLock;

/// COM1 base I/O port. Fixed by the PC platform.
const COM1_BASE: u16 = 0x3F8;

// 16550 register offsets from the base port. DATA and IER double as the
// divisor-latch low/high bytes while the LCR's DLAB bit is set.
const REG_DATA: u16 = 0;
const REG_IER: u16 = 1;
const REG_FCR: u16 = 2;
const REG_LCR: u16 = 3;
const REG_MCR: u16 = 4;
const REG_LSR: u16 = 5;

/// Line-status bit: transmit-holding register empty — the UART can accept
/// another byte.
const LSR_THR_EMPTY: u8 = 0x20;

/// A 16550 UART addressed by its base I/O port.
///
/// `SerialPort` is `Copy` and holds nothing but the port number, so the
/// emergency path can mint a fresh one for free.
#[derive(Clone, Copy)]
pub struct SerialPort {
    base: u16,
}

impl SerialPort {
    /// Bind a `SerialPort` to the UART at `base`. Touches no hardware, so
    /// it is usable in `const` context (see [`SERIAL`]).
    pub const fn new(base: u16) -> Self {
        SerialPort { base }
    }

    /// Program the UART: 115200 baud, 8N1, FIFOs on, interrupts off.
    pub fn init(&self) {
        // SAFETY: every write targets a 16550 register at the COM1 base
        // port, which this driver exclusively owns. The values and their
        // ordering follow the standard 16550 programming sequence.
        unsafe {
            regs::outb(self.base + REG_IER, 0x00); // interrupts off — polled
            regs::outb(self.base + REG_LCR, 0x80); // DLAB on: expose divisor
            regs::outb(self.base + REG_DATA, 0x01); // divisor low  = 1 (115200)
            regs::outb(self.base + REG_IER, 0x00); // divisor high = 0
            regs::outb(self.base + REG_LCR, 0x03); // DLAB off, 8 bits, no parity, 1 stop
            regs::outb(self.base + REG_FCR, 0xC7); // FIFO on, cleared, 14-byte trigger
            regs::outb(self.base + REG_MCR, 0x0B); // DTR, RTS, OUT2
        }
    }

    /// Write one byte, busy-polling until the UART can accept it.
    ///
    /// The poll is capped: a dead or absent UART must never wedge the
    /// kernel — least of all the panic path, which must always reach
    /// `halt_loop`. A byte not accepted before the cap is dropped.
    pub fn write_byte(&self, byte: u8) {
        // Far longer than one character time at 115200 baud, short enough
        // never to feel hung.
        const POLL_CAP: u32 = 10_000_000;
        let mut spins: u32 = 0;
        // SAFETY: `REG_LSR` and `REG_DATA` are 16550 registers at the
        // COM1 base port, owned exclusively by this driver.
        unsafe {
            while regs::inb(self.base + REG_LSR) & LSR_THR_EMPTY == 0 {
                spins += 1;
                if spins >= POLL_CAP {
                    return;
                }
                core::hint::spin_loop();
            }
            regs::outb(self.base + REG_DATA, byte);
        }
    }
}

/// Writes bytes to the UART, translating `\n` into `\r\n` so the output
/// renders correctly on a terminal.
impl fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &byte in s.as_bytes() {
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
        Ok(())
    }
}

/// The kernel's COM1 serial port behind a spin lock. The `kprint!` /
/// `kprintln!` macros drive this; the panic and exception handlers use
/// [`emergency_writer`] instead (see the module docs).
pub static SERIAL: IrqSpinLock<SerialPort> = IrqSpinLock::new(SerialPort::new(COM1_BASE));

/// Initialise the COM1 UART. Call once, early in boot, before the first
/// `kprintln!`.
pub fn init() {
    SERIAL.lock().init();
}

/// An *unsynchronised* [`SerialPort`] for the panic and exception
/// handlers.
///
/// See the module docs: this bypasses [`SERIAL`]'s lock so a handler can
/// print even if the lock was held when the fault occurred. Sound only
/// under Phase 1's single-CPU, interrupts-masked model.
pub fn emergency_writer() -> SerialPort {
    SerialPort::new(COM1_BASE)
}

/// Print formatted output to the kernel serial console, without a
/// trailing newline.
///
/// Formats directly into the locked [`SERIAL`] port via
/// `core::fmt::Write` — no heap allocation. **Not** for use in panic or
/// exception handlers: it takes [`SERIAL`]'s lock and would deadlock if
/// the lock were already held at fault time. Those paths must use
/// [`emergency_writer`] directly.
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {{
        let mut __serial = $crate::arch::serial::SERIAL.lock();
        let _ = ::core::fmt::Write::write_fmt(
            &mut *__serial,
            ::core::format_args!($($arg)*),
        );
    }};
}

/// Like [`kprint!`], with a trailing newline. The same restriction
/// against use in panic and exception handlers applies.
#[macro_export]
macro_rules! kprintln {
    () => {
        $crate::kprint!("\n")
    };
    ($($arg:tt)*) => {{
        let mut __serial = $crate::arch::serial::SERIAL.lock();
        let _ = ::core::fmt::Write::write_fmt(
            &mut *__serial,
            ::core::format_args!($($arg)*),
        );
        let _ = ::core::fmt::Write::write_str(&mut *__serial, "\n");
    }};
}
