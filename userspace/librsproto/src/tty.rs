//! `Tty` — the terminal's size, which `Size`, `Resize` and `AttachBackend` carry.

/// A terminal's size in cells: two little-endian `u16`s, columns then rows.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Size {
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
}

impl Size {
    /// A backend's that has not said, as the serial console's.
    pub const UNKNOWN: Size = Size { cols: 0, rows: 0 };

    /// Bytes in the encoding.
    pub const LEN: usize = 4;

    /// A grid's size, each side held to what the encoding can carry.
    pub fn of_grid(cols: usize, rows: usize) -> Size {
        let side = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
        Size { cols: side(cols), rows: side(rows) }
    }

    /// Whether the size is known: a side of 0 says it is not.
    pub fn is_known(self) -> bool {
        self.cols > 0 && self.rows > 0
    }

    /// The four bytes on the wire.
    pub fn encode(self) -> [u8; Self::LEN] {
        let [c0, c1] = self.cols.to_le_bytes();
        let [r0, r1] = self.rows.to_le_bytes();
        [c0, c1, r0, r1]
    }

    /// A size from `body`'s first [`LEN`](Self::LEN) bytes; any after them are a later version's,
    /// as a field added to the end of a body is (`rsproto-wire-format.md` § Schema evolution).
    pub fn decode(body: &[u8]) -> Option<Size> {
        let &[c0, c1, r0, r1, ..] = body else { return None };
        Some(Size { cols: u16::from_le_bytes([c0, c1]), rows: u16::from_le_bytes([r0, r1]) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_size_is_columns_then_rows_little_endian() {
        let s = Size { cols: 0x0102, rows: 0x0304 };
        assert_eq!(s.encode(), [0x02, 0x01, 0x04, 0x03]);
        assert_eq!(Size::decode(&s.encode()), Some(s));
    }

    #[test]
    fn a_shorter_body_is_no_size_and_a_longer_one_is_its_first_four_bytes() {
        for body in [&[][..], &[1, 0, 2][..]] {
            assert_eq!(Size::decode(body), None, "{body:?}");
        }
        assert_eq!(Size::decode(&[1, 0, 2, 0, 9]), Some(Size { cols: 1, rows: 2 }), "a later version's field");
    }

    #[test]
    fn a_side_of_zero_is_unknown() {
        assert!(!Size::UNKNOWN.is_known());
        assert!(!Size { cols: 80, rows: 0 }.is_known());
        assert!(!Size { cols: 0, rows: 24 }.is_known());
        assert!(Size { cols: 1, rows: 1 }.is_known());
    }

    #[test]
    fn a_grid_too_big_to_say_is_held_to_the_largest_it_can() {
        assert_eq!(Size::of_grid(80, 24), Size { cols: 80, rows: 24 });
        assert_eq!(Size::of_grid(65_535, 65_536), Size { cols: 65_535, rows: 65_535 });
    }
}
