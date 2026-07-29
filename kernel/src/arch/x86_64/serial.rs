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
/// COM1's legacy ISA interrupt line. A PC-platform fact (like [`COM1_BASE`]) that
/// stays inside the arch layer — neutral code arms it via [`console_arm_rx`].
const COM1_IRQ: u8 = 4;

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
/// Line-status bit: receive data available — a byte can be read from `REG_DATA`.
const LSR_DATA_READY: u8 = 0x01;
/// Interrupt-enable bit: received-data-available — raise IRQ when a byte arrives.
const IER_RX_AVAIL: u8 = 0x01;
/// Modem-control value for normal operation: DTR, RTS, OUT2 (OUT2 gates the UART's
/// interrupt line through to the interrupt controller). Matches [`SerialPort::init`].
const MCR_NORMAL: u8 = 0x0B;
/// Modem-control bit: internal loopback (TX feeds RX) — used by the RX self-test.
const MCR_LOOPBACK: u8 = 0x10;

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

    /// `true` iff the UART has a received byte ready in `REG_DATA`.
    pub fn read_ready(&self) -> bool {
        // SAFETY: `REG_LSR` is a 16550 register at this driver's COM1 base port.
        unsafe { regs::inb(self.base + REG_LSR) & LSR_DATA_READY != 0 }
    }

    /// Read one received byte from `REG_DATA`. The caller must have observed
    /// [`read_ready`](Self::read_ready); reading also clears the UART's
    /// received-data-available interrupt condition.
    pub fn read_byte(&self) -> u8 {
        // SAFETY: `REG_DATA` is a 16550 register at this driver's COM1 base port.
        unsafe { regs::inb(self.base + REG_DATA) }
    }

    /// Enable the received-data-available interrupt (the UART raises its IRQ when a
    /// byte arrives). `OUT2` is already set by [`init`](Self::init), so the IRQ line
    /// reaches the interrupt controller.
    pub fn enable_rx_interrupt(&self) {
        // SAFETY: `REG_IER` is a 16550 register at this driver's COM1 base port.
        unsafe { regs::outb(self.base + REG_IER, IER_RX_AVAIL) };
    }

    /// Set or clear internal loopback (`MCR` bit 4) — TX feeds RX internally,
    /// without touching the wire. Used by the RX self-test.
    pub fn set_loopback(&self, on: bool) {
        let mcr = if on { MCR_NORMAL | MCR_LOOPBACK } else { MCR_NORMAL };
        // SAFETY: `REG_MCR` is a 16550 register at this driver's COM1 base port.
        unsafe { regs::outb(self.base + REG_MCR, mcr) };
    }
}

/// Writes bytes to the UART, translating `\n` into `\r\n` so the output
/// renders correctly on a terminal.
impl fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        // Tee the raw bytes into the kernel log ring (`cat /dev/log` / dmesg). This
        // is the `kprint!` path *and* the panic/exception emergency writer; `push`
        // uses `try_lock`, so it can never deadlock the panic path. Newlines stay
        // bare `\n` here (the log reader's `sys_kprint` translates for the terminal).
        crate::klog::push(s.as_bytes());
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

// --- Console input (COM1 RX) — the neutral surface the console driver uses ----
//
// The console-input driver (`kernel/src/drivers/console.rs`) is neutral kernel
// code; it reaches the COM1 UART only through these free functions (re-exported as
// `crate::arch::serial::*`), never `arch::x86_64` internals.

/// `true` iff COM1 has a received byte ready (the ISR's drain predicate).
pub fn console_rx_ready() -> bool {
    SerialPort::new(COM1_BASE).read_ready()
}

/// Read one received byte from COM1 (also clears the RX interrupt condition).
pub fn console_rx_read() -> u8 {
    SerialPort::new(COM1_BASE).read_byte()
}

/// Arm console receive interrupts: route the console UART's interrupt to `handler`
/// and enable the UART's received-data-available interrupt. Returns the assigned IDT
/// vector. The neutral "arm the console's RX interrupt" operation — it hides the
/// platform wiring (here, COM1 ISA IRQ 4 through the IOAPIC); an aarch64 port would
/// route its UART's GIC interrupt instead. Neutral code calls this rather than naming
/// any ISA/IRQ specifics.
///
/// # Safety
/// Ring-0, after the interrupt router is initialised; `handler` must stay valid for
/// the kernel's lifetime; the caller must be ready to receive the IRQ once interrupts
/// are enabled. Call **after** [`console_rx_loopback_selftest`] (which polls).
pub unsafe fn console_arm_rx(handler: extern "C" fn()) -> u8 {
    // SAFETY: forwarded from this fn's contract (ring-0, post-router-init; `handler`
    // valid for the kernel's lifetime). `install_isa_irq` is the x86 IOAPIC routing
    // primitive, kept inside the arch layer.
    let vector = unsafe { super::ioapic::install_isa_irq(COM1_IRQ, handler) };
    SerialPort::new(COM1_BASE).enable_rx_interrupt();
    vector
}

/// Self-test the COM1 receive path with **internal loopback**: enable loopback,
/// transmit a known byte (which loops back to RX without touching the wire), poll
/// for it, and verify. Returns `true` on a match. Restores normal mode. Boot-only
/// (single-CPU, interrupts masked, no concurrent UART user) — like
/// [`emergency_writer`], it mints a throwaway port. Must run **before** RX
/// interrupts are armed so the polled read, not an ISR, consumes the byte.
pub fn console_rx_loopback_selftest() -> bool {
    const TEST_BYTE: u8 = 0x5A;
    let p = SerialPort::new(COM1_BASE);
    p.set_loopback(true);
    p.write_byte(TEST_BYTE);
    let mut spins: u32 = 0;
    let got = loop {
        if p.read_ready() {
            break Some(p.read_byte());
        }
        spins += 1;
        if spins >= 1_000_000 {
            break None;
        }
        core::hint::spin_loop();
    };
    p.set_loopback(false);
    got == Some(TEST_BYTE)
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
