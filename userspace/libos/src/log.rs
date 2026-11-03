//! The kernel's log: what a program says, on the serial line. Each write is one line at most,
//! atomic, and a line built from several writes can be split by another process's.

use crate::sys;

/// Write `bytes` to the kernel's log.
pub fn write(bytes: &[u8]) {
    sys::kprint(bytes);
}

/// A log line assembled in one buffer and written with one [`write`], `...` at its end if it
/// overflowed: the gates match whole lines.
#[derive(Default)]
pub struct Line(libkern::debug::Line);

impl Line {
    /// An empty line.
    pub fn new() -> Self {
        Line(libkern::debug::Line::new())
    }

    /// Append bytes the image itself carries.
    pub fn s(&mut self, b: &[u8]) -> &mut Self {
        self.0.s(b);
        self
    }

    /// Append bytes from across a trust boundary, a control byte shown as `?`, so they cannot
    /// end this line and start one that seems another's.
    pub fn untrusted(&mut self, b: &[u8]) -> &mut Self {
        self.0.untrusted(b);
        self
    }

    /// Append an unsigned decimal.
    pub fn u(&mut self, v: u64) -> &mut Self {
        self.0.u(v);
        self
    }

    /// Append a signed decimal.
    pub fn i(&mut self, v: i64) -> &mut Self {
        self.0.i(v);
        self
    }

    /// Append a `0x`-prefixed 16-digit hex value.
    pub fn x(&mut self, v: u64) -> &mut Self {
        self.0.x(v);
        self
    }

    /// End the line and write it.
    pub fn end(&mut self) {
        write(self.0.finish());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_one_write_ending_in_a_newline() {
        sys::reset();
        Line::new().s(b"pid=").u(7).s(b" ").untrusted(b"a\nb").end();
        assert_eq!(sys::logged(), b"pid=7 a?b\n");
    }
}
