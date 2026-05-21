//! Raw x86_64 hardware-register access: I/O ports and control registers.
//!
//! Per `kernel/CLAUDE.md`, port I/O and hardware-register reads/writes
//! live behind wrapper functions in this module rather than as `asm!`
//! calls scattered through the codebase. The diagnostics slice needs the
//! port primitives (the 16550 UART speaks port I/O) and a `CR2` read (the
//! page-fault handler reports the faulting linear address from it).

use core::arch::asm;

/// Write a byte to I/O port `port`.
///
/// # Safety
/// Port I/O has arbitrary, device-specific side effects. The caller must
/// own `port` and ensure the write is meaningful for the device behind it.
#[inline]
pub unsafe fn outb(port: u16, val: u8) {
    // SAFETY: `out dx, al` writes `al` to the I/O port named by `dx`. The
    // caller upholds the device-level contract. `nomem`/`preserves_flags`
    // hold: the instruction touches no memory and no arithmetic flags.
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") val,
             options(nomem, nostack, preserves_flags));
    }
}

/// Read a byte from I/O port `port`.
///
/// # Safety
/// See [`outb`] — the caller must own `port`.
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    // SAFETY: `in al, dx` reads the I/O port named by `dx` into `al`. The
    // caller owns the port; the instruction touches no memory or flags.
    unsafe {
        asm!("in al, dx", out("al") val, in("dx") port,
             options(nomem, nostack, preserves_flags));
    }
    val
}

/// Write a 16-bit word to I/O port `port`.
///
/// # Safety
/// See [`outb`] — the caller must own `port`.
#[inline]
pub unsafe fn outw(port: u16, val: u16) {
    // SAFETY: `out dx, ax` writes `ax` to the I/O port named by `dx`. As
    // for `outb`, the caller upholds the device-level contract.
    unsafe {
        asm!("out dx, ax", in("dx") port, in("ax") val,
             options(nomem, nostack, preserves_flags));
    }
}

/// Read a 16-bit word from I/O port `port`.
///
/// # Safety
/// See [`outb`] — the caller must own `port`.
#[inline]
pub unsafe fn inw(port: u16) -> u16 {
    let val: u16;
    // SAFETY: `in ax, dx` reads the I/O port named by `dx` into `ax`.
    unsafe {
        asm!("in ax, dx", out("ax") val, in("dx") port,
             options(nomem, nostack, preserves_flags));
    }
    val
}

/// Write a 32-bit doubleword to I/O port `port`.
///
/// # Safety
/// See [`outb`] — the caller must own `port`.
#[inline]
pub unsafe fn outl(port: u16, val: u32) {
    // SAFETY: `out dx, eax` writes `eax` to the I/O port named by `dx`.
    unsafe {
        asm!("out dx, eax", in("dx") port, in("eax") val,
             options(nomem, nostack, preserves_flags));
    }
}

/// Read a 32-bit doubleword from I/O port `port`.
///
/// # Safety
/// See [`outb`] — the caller must own `port`.
#[inline]
pub unsafe fn inl(port: u16) -> u32 {
    let val: u32;
    // SAFETY: `in eax, dx` reads the I/O port named by `dx` into `eax`.
    unsafe {
        asm!("in eax, dx", out("eax") val, in("dx") port,
             options(nomem, nostack, preserves_flags));
    }
    val
}

/// Read control register `CR2` — the linear address of the most recent
/// page fault.
///
/// Safe: reading `CR2` has no side effects and is always valid in ring 0,
/// which is the only ring the kernel runs in.
#[inline]
pub fn read_cr2() -> u64 {
    let val: u64;
    // SAFETY: `mov reg, cr2` reads CR2 into a general register. It has no
    // side effects, touches no normal memory, and leaves flags untouched.
    unsafe {
        asm!("mov {}, cr2", out(reg) val,
             options(nomem, nostack, preserves_flags));
    }
    val
}
