//! Bytes someone else wrote, printed without trusting them.
//!
//! Firmware and bootloaders hand the kernel fixed-width and NUL-terminated byte strings —
//! ACPI's space-padded OEM fields, Limine's bootloader name and command line — and the kernel
//! log is drawn on a terminal and on the framebuffer console. [`Printable`] is the one way
//! those bytes reach a log line.

use core::fmt;

/// A byte string formatted for a log line.
///
/// Stops at the first NUL, drops trailing spaces (ACPI pads its OEM fields with them), and
/// shows every byte outside printable ASCII as `.`, so a corrupt or hostile field cannot put a
/// control sequence on a terminal. An empty result prints as `-`: a blank field is still
/// visibly a field, and two blanks in a row do not read as one.
pub struct Printable<'a>(pub &'a [u8]);

impl Printable<'_> {
    /// The bytes that will be shown: up to the first NUL, trailing spaces removed.
    fn shown(&self) -> &[u8] {
        let end = self.0.iter().position(|&b| b == 0).unwrap_or(self.0.len());
        let mut s = &self.0[..end];
        while let [rest @ .., b' '] = s {
            s = rest;
        }
        s
    }
}

impl fmt::Display for Printable<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.shown();
        if s.is_empty() {
            return f.write_str("-");
        }
        for &b in s {
            let c = if (0x20..=0x7E).contains(&b) { b as char } else { '.' };
            fmt::Write::write_char(f, c)?;
        }
        Ok(())
    }
}

/// The bytes of the NUL-terminated string at `ptr`, not counting the NUL, and never more than
/// `max`. A null `ptr` is an empty string.
///
/// # Safety
/// A non-null `ptr` must be readable up to its first NUL or `max` bytes, whichever is first.
pub unsafe fn c_bytes<'a>(ptr: *const u8, max: usize) -> &'a [u8] {
    if ptr.is_null() {
        return &[];
    }
    let mut n = 0;
    // SAFETY: the caller guarantees every byte before the NUL, up to `max`, is readable; the
    // loop reads no further than either.
    while n < max && unsafe { *ptr.add(n) } != 0 {
        n += 1;
    }
    // SAFETY: the `n` bytes just read are readable, per the same contract.
    unsafe { core::slice::from_raw_parts(ptr, n) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_acpi_oem_field_loses_its_padding_and_nothing_else() {
        assert_eq!(format!("{}", Printable(b"BOCHS ")), "BOCHS");
        assert_eq!(format!("{}", Printable(b"BXPC    ")), "BXPC");
        // Interior spaces are part of the name.
        assert_eq!(format!("{}", Printable(b"ACER  AB")), "ACER  AB");
    }

    #[test]
    fn a_control_byte_is_shown_as_a_dot_rather_than_sent_to_the_terminal() {
        assert_eq!(format!("{}", Printable(b"a\x1b[2Jb")), "a.[2Jb");
        assert_eq!(format!("{}", Printable(&[b'x', 0xFF, b'y'])), "x.y");
    }

    #[test]
    fn a_nul_ends_the_string_and_a_blank_field_is_still_visible() {
        assert_eq!(format!("{}", Printable(b"Limine\0junk")), "Limine");
        assert_eq!(format!("{}", Printable(b"      ")), "-");
        assert_eq!(format!("{}", Printable(b"")), "-");
    }

    #[test]
    fn c_bytes_stops_at_the_nul_and_at_the_bound() {
        let s = b"hwreport=5\0tail";
        // SAFETY: `s` is readable for its whole length, which covers both bounds used.
        assert_eq!(unsafe { c_bytes(s.as_ptr(), 64) }, b"hwreport=5");
        // SAFETY: as above.
        assert_eq!(unsafe { c_bytes(s.as_ptr(), 3) }, b"hwr");
        // SAFETY: null is explicitly allowed.
        assert_eq!(unsafe { c_bytes(core::ptr::null(), 8) }, b"");
    }
}
