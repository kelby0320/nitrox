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

/// How many bytes a [`Line`] can hold, including its newline.
///
/// Stack-allocated, so this is a per-call cost paid by init and the critical path too. The
/// longest line in the tree today is `service-mgr`'s restart announcement at ~100 bytes.
pub const LINE_MAX: usize = 256;

/// A log line assembled in one buffer and emitted with a **single** `kprint`.
///
/// `kprint` is atomic per call and nothing more: the console is shared, so a line built from
/// several calls — `kprint(b"... exit "); kprint_u64(n); kprint(b")\n")` — can be split down
/// the middle by any other process that logs in between. That is not hypothetical.
/// `session-mgr`'s session-ended line came back from CI as `session ended (shell exit
/// tty-server: terminal closed` / `3)`, and `cargo xtask check-input` was 40% flaky for a
/// milestone because its six-call lines arrived shredded — misdiagnosed as a guest bug first,
/// which is what the deferral's trigger meant by "costs debugging time".
///
/// ```ignore
/// Line::new().s(b"init: reaped pid=").u(pid).s(b" code=").i(code).end();
/// ```
///
/// **A truncated line says so.** Overflow is marked with a trailing `...` rather than
/// silently dropped, because these lines are what the QEMU gates match on and a short line
/// that looks complete is worse than one that admits it is not.
pub struct Line {
    buf: [u8; LINE_MAX],
    len: usize,
    overflowed: bool,
}

impl Default for Line {
    /// Delegates to [`Line::new`].
    fn default() -> Self {
        Self::new()
    }
}

impl Line {
    /// An empty line.
    pub fn new() -> Self {
        Self { buf: [0; LINE_MAX], len: 0, overflowed: false }
    }

    /// Append raw bytes.
    pub fn s(&mut self, b: &[u8]) -> &mut Self {
        for &c in b {
            // One byte is always held back for the newline, so `finish` cannot be the thing
            // that overflows and a line always terminates.
            if self.len < LINE_MAX - 1 {
                self.buf[self.len] = c;
                self.len += 1;
            } else {
                self.overflowed = true;
            }
        }
        self
    }

    /// Append an unsigned decimal.
    pub fn u(&mut self, v: u64) -> &mut Self {
        let mut digits = [0u8; 20];
        // No copy dance: `digits` is a local, so the slice `fmt_u64` hands back does not
        // borrow `self` and can be passed straight to `s`. A first version shifted the
        // bytes down on the assumption that `fmt_u64` always returns a *suffix* — it
        // returns the front for zero — and turned `0` into a NUL byte.
        let d = fmt_u64(v, &mut digits);
        let mut tmp = [0u8; 20];
        let n = d.len();
        tmp[..n].copy_from_slice(d);
        self.s(&tmp[..n])
    }

    /// Append a signed decimal.
    pub fn i(&mut self, v: i64) -> &mut Self {
        if v < 0 {
            self.s(b"-");
            // `unsigned_abs` rather than `-v`: negating `i64::MIN` overflows, and an exit
            // status or a window-local coordinate is exactly the kind of value that reaches
            // this.
            self.u(v.unsigned_abs())
        } else {
            self.u(v as u64)
        }
    }

    /// Append a `0x`-prefixed 16-digit hex value.
    pub fn x(&mut self, v: u64) -> &mut Self {
        let mut buf = [0u8; 18];
        let mut tmp = [0u8; 18];
        tmp.copy_from_slice(fmt_hex(v, &mut buf));
        self.s(&tmp)
    }

    /// Terminate the line and return its bytes, newline included.
    ///
    /// Separate from [`Line::end`] so the formatting is a pure function the host tests can
    /// inspect — the same split `fmt_u64` and `kprint_u64` already use.
    pub fn finish(&mut self) -> &[u8] {
        if self.overflowed {
            // Overwrite the tail rather than append: there is no room to append.
            let cut = self.len.saturating_sub(3);
            self.buf[cut..self.len].copy_from_slice(b"...");
        }
        self.buf[self.len] = b'\n';
        &self.buf[..self.len + 1]
    }

    /// Terminate the line and emit it with one `kprint`.
    pub fn end(&mut self) {
        kprint(self.finish());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_assembles_in_order_with_one_newline() {
        let mut l = Line::new();
        assert_eq!(
            l.s(b"init: reaped pid=").u(7).s(b" code=").i(-1).finish(),
            b"init: reaped pid=7 code=-1\n"
        );
    }

    #[test]
    fn multi_digit_values_are_not_reversed_or_truncated() {
        // `fmt_u64` writes a *suffix* of its buffer for non-zero values and the *front* for
        // zero, so anything that assumes one shape yields NULs or a wrong length. Both
        // shapes are covered here and in `hex_and_zero_and_the_signed_extreme`.
        assert_eq!(Line::new().u(1234567890).finish(), b"1234567890\n");
        assert_eq!(Line::new().u(u64::MAX).finish(), b"18446744073709551615\n");
        assert_eq!(Line::new().s(b"a=").u(42).s(b" b=").u(7).finish(), b"a=42 b=7\n");
    }

    #[test]
    fn hex_and_zero_and_the_signed_extreme() {
        assert_eq!(Line::new().x(0xdead_beef).finish(), b"0x00000000deadbeef\n");
        assert_eq!(Line::new().u(0).finish(), b"0\n");
        // `-v` on `i64::MIN` overflows; an exit status is exactly the kind of value that
        // reaches `i`, so this is the case that must not panic.
        assert_eq!(Line::new().i(i64::MIN).finish(), b"-9223372036854775808\n");
    }

    #[test]
    fn an_overflowing_line_admits_it_rather_than_looking_complete() {
        // These lines are what the QEMU gates match on. A silently truncated one that still
        // reads as a whole line is worse than one that says it was cut.
        let mut l = Line::new();
        for _ in 0..LINE_MAX {
            l.s(b"x");
        }
        let out = l.finish();
        assert_eq!(out.len(), LINE_MAX, "one byte was held back for the newline");
        assert_eq!(&out[out.len() - 4..], b"...\n");
    }

    #[test]
    fn a_line_that_exactly_fills_the_buffer_is_not_marked_cut() {
        // The boundary the truncation marker must not claim: `LINE_MAX - 1` bytes plus the
        // newline is a complete line, and marking it would make a correct line look broken.
        let mut l = Line::new();
        for _ in 0..LINE_MAX - 1 {
            l.s(b"y");
        }
        let out = l.finish();
        assert_eq!(out.len(), LINE_MAX);
        assert!(!out.ends_with(b"...\n"), "nothing was dropped");
    }

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
