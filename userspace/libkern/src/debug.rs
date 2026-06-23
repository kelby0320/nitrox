//! Debug output helpers: `kprint` (write to the kernel serial log) and `exit`,
//! plus small no-alloc integer formatters.
//!
//! These sit just above the raw syscall line but every early/critical-path crate
//! (init, eshell, the demos) needs them with identical bodies, so they live here.
//! The formatting (`fmt_u64`/`fmt_hex`) is pure and host-testable; `kprint`/`exit`
//! issue syscalls and are exercised under QEMU.

use crate::syscall::{SYS_DEBUG_KPRINT, SYS_PROCESS_EXIT, syscall4};
use core::arch::asm;

/// Write `msg` to the kernel serial log (`sys_kprint`).
pub fn kprint(msg: &[u8]) {
    // SAFETY: passes a valid (ptr, len) pair the kernel copies from; no handles.
    unsafe {
        syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0);
    }
}

/// Terminate the calling process with `status`. Diverges (never returns).
pub fn exit(status: i64) -> ! {
    // SAFETY: `sys_process_exit` diverges in the kernel; control never returns,
    // so `options(noreturn)` is sound.
    unsafe {
        asm!(
            "syscall",
            in("rax") SYS_PROCESS_EXIT,
            in("rdi") status,
            options(noreturn, nostack),
        );
    }
}

/// Format `v` as decimal into `buf`, returning the written suffix. No alloc.
pub fn fmt_u64(mut v: u64, buf: &mut [u8; 20]) -> &[u8] {
    if v == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    &buf[i..]
}

/// Format `v` as `0x`-prefixed, 16-digit lowercase hex into `buf`. No alloc.
pub fn fmt_hex(v: u64, buf: &mut [u8; 18]) -> &[u8] {
    buf[0] = b'0';
    buf[1] = b'x';
    let mut i = 0;
    while i < 16 {
        let nib = ((v >> ((15 - i) * 4)) & 0xf) as u8;
        buf[2 + i] = if nib < 10 { b'0' + nib } else { b'a' + (nib - 10) };
        i += 1;
    }
    &buf[..]
}

/// Print a small unsigned decimal (pids/codes) to the kernel log. No alloc.
pub fn kprint_u64(v: u64) {
    let mut buf = [0u8; 20];
    kprint(fmt_u64(v, &mut buf));
}

/// Print a 64-bit value as `0x`-prefixed 16-digit hex to the kernel log. No alloc.
pub fn kprint_hex(v: u64) {
    let mut buf = [0u8; 18];
    kprint(fmt_hex(v, &mut buf));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_u64_cases() {
        let mut b = [0u8; 20];
        assert_eq!(fmt_u64(0, &mut b), b"0");
        assert_eq!(fmt_u64(7, &mut b), b"7");
        assert_eq!(fmt_u64(12345, &mut b), b"12345");
        assert_eq!(fmt_u64(u64::MAX, &mut b), b"18446744073709551615");
    }

    #[test]
    fn fmt_hex_cases() {
        let mut b = [0u8; 18];
        assert_eq!(fmt_hex(0, &mut b), b"0x0000000000000000");
        assert_eq!(fmt_hex(0xc0ffee, &mut b), b"0x0000000000c0ffee");
        assert_eq!(fmt_hex(u64::MAX, &mut b), b"0xffffffffffffffff");
    }
}
