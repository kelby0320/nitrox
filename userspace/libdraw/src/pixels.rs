//! A surface's pixels, and the reading of a buffer another process shares.
//!
//! **A reference promises that nothing writes its bytes while it lives**, and a client may write
//! its buffer at any time, so a shared buffer is never borrowed: its bytes are copied out a piece
//! at a time, and only the copy is read.

use core::fmt;

/// Bytes another process may write at any time, so read only by copying them out.
pub trait SharedBytes {
    /// How many there are.
    fn len(&self) -> usize;
    /// Whether there are none.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Copy `out.len()` of them from `offset` into `out`: `false`, with nothing copied, past the
    /// end.
    fn copy_out(&self, offset: usize, out: &mut [u8]) -> bool;
}

#[cfg(any(test, feature = "io"))]
impl<M> SharedBytes for libos::Mapping<M> {
    fn len(&self) -> usize {
        libos::Mapping::len(self)
    }
    fn copy_out(&self, offset: usize, out: &mut [u8]) -> bool {
        self.read_into(offset, out).is_ok()
    }
}

/// A surface's pixels: bytes this process alone writes, or a buffer shared with another.
#[derive(Clone, Copy)]
pub enum Pixels<'a> {
    /// Bytes nothing else writes while they are borrowed.
    Own(&'a [u8]),
    /// Bytes another process shares, never borrowed.
    Shared(&'a dyn SharedBytes),
}

/// The most of a shared buffer copied out at once: a piece is on the stack, so a row costs no
/// allocation.
const PIECE: usize = 1024;

impl Pixels<'_> {
    /// How many bytes there are.
    pub fn len(&self) -> usize {
        match self {
            Pixels::Own(b) => b.len(),
            Pixels::Shared(s) => s.len(),
        }
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copy `out.len()` bytes from `offset` into `out`: `false`, with nothing copied, past the end.
    pub fn copy_out(&self, offset: usize, out: &mut [u8]) -> bool {
        match self {
            Pixels::Own(b) => match b.get(offset..offset.saturating_add(out.len())) {
                Some(src) => {
                    out.copy_from_slice(src);
                    true
                }
                None => false,
            },
            Pixels::Shared(s) => s.copy_out(offset, out),
        }
    }

    /// Hand `f` the whole `unit`s there are of the `len` bytes at `offset`, in order, each piece
    /// with its offset: borrowed where this process alone writes them, copied where they are shared.
    pub fn each_piece(&self, offset: usize, len: usize, unit: usize, mut f: impl FnMut(usize, &[u8])) {
        let unit = unit.max(1);
        let have = self.len().saturating_sub(offset).min(len);
        let have = have - have % unit;
        match self {
            Pixels::Own(b) if have > 0 => f(offset, &b[offset..offset + have]),
            Pixels::Own(_) => {}
            Pixels::Shared(s) => {
                let mut buf = [0u8; PIECE];
                let step = PIECE - PIECE % unit;
                let mut at = 0;
                while step > 0 && at < have {
                    let n = step.min(have - at);
                    if !s.copy_out(offset + at, &mut buf[..n]) {
                        return;
                    }
                    f(offset + at, &buf[..n]);
                    at += n;
                }
            }
        }
    }
}

impl fmt::Debug for Pixels<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pixels::Own(b) => write!(f, "Own({} bytes)", b.len()),
            Pixels::Shared(s) => write!(f, "Shared({} bytes)", s.len()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libos::{Handle, MapReadWrite, Mapping, Memory, mock};
    use std::vec::Vec;

    fn shared(bytes: &[u8]) -> Mapping<MapReadWrite> {
        let mem = Handle::<Memory, MapReadWrite>::create(bytes.len()).unwrap();
        let mut map = mem.map(bytes.len()).unwrap();
        map.write_from(0, bytes).unwrap();
        map
    }

    fn pieces(px: Pixels<'_>, offset: usize, len: usize, unit: usize) -> Vec<(usize, Vec<u8>)> {
        let mut got = Vec::new();
        px.each_piece(offset, len, unit, |at, b| got.push((at, b.to_vec())));
        got
    }

    #[test]
    fn a_shared_buffer_gives_the_bytes_its_owner_would_in_pieces_of_whole_units() {
        mock::reset();
        let bytes: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
        let map = shared(&bytes);
        let (own, shared) = (Pixels::Own(&bytes), Pixels::Shared(&map));
        let joined = |p: Vec<(usize, Vec<u8>)>| p.into_iter().flat_map(|(_, b)| b).collect::<Vec<u8>>();
        assert_eq!(pieces(own, 8, 2500, 4).len(), 1, "borrowed whole");
        let got = pieces(shared, 8, 2500, 4);
        assert_eq!(
            got.iter().map(|(at, b)| (*at, b.len())).collect::<Vec<_>>(),
            [(8, 1024), (1032, 1024), (2056, 452)]
        );
        assert_eq!(joined(got), joined(pieces(own, 8, 2500, 4)));
        assert_eq!(joined(pieces(shared, 2990, 100, 4)), &bytes[2990..2998], "past the end: the whole units held");
        assert!(pieces(shared, 2999, 4, 4).is_empty() && pieces(own, 2999, 4, 4).is_empty(), "no part of a unit");
    }

    /// A unit that does not divide a piece, three bytes a pixel, is never split between two.
    #[test]
    fn a_unit_that_does_not_divide_a_piece_stays_whole() {
        mock::reset();
        let bytes: Vec<u8> = (0..3000u32).map(|i| (i * 7) as u8).collect();
        let map = shared(&bytes);
        let got = pieces(Pixels::Shared(&map), 0, 2500, 3);
        assert_eq!(
            got.iter().map(|(at, b)| (*at, b.len())).collect::<Vec<_>>(),
            [(0, 1023), (1023, 1023), (2046, 453)]
        );
        assert_eq!(got.into_iter().flat_map(|(_, b)| b).collect::<Vec<u8>>(), &bytes[..2499]);
    }

    /// A shared surface draws what the same bytes owned do, on every blit path: rows, blended,
    /// another format's, and a rounded corner's edge.
    #[test]
    fn a_shared_surface_composes_as_the_same_bytes_owned() {
        use crate::compose::{SurfaceRef, compose_full};
        use crate::format::{PixelFormat, Rgb};
        use crate::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
        use crate::geom::Point;
        mock::reset();
        let screen = Geometry::packed(300, 40, PixelFormat::XRGB8888);
        for (format, corner) in [
            (PixelFormat::XRGB8888, 0),
            (PixelFormat::ARGB8888, 0),
            (PixelFormat::XBGR8888, 0),
            (PixelFormat::XRGB8888, 9),
        ] {
            let g = Geometry::packed(290, 30, format);
            let bytes: Vec<u8> = (0..g.byte_len()).map(|i| (i * 7 + i / 5) as u8).collect();
            let map = shared(&bytes);
            let draw = |s: SurfaceRef<'_>| {
                let mut fb = MemFramebuffer::new(screen);
                compose_full(&mut fb, Rgb::new(9, 9, 9), &[s.with_corner(corner)]);
                fb.bytes().to_vec()
            };
            let own = draw(SurfaceRef::new(g, Point::new(5, 3), &bytes));
            assert_eq!(draw(SurfaceRef::shared(g, Point::new(5, 3), &map)), own, "{format:?} corner {corner}");
            assert_ne!(own, MemFramebuffer::new(screen).bytes(), "it drew something");
        }
    }

    #[test]
    fn a_copy_past_the_end_copies_nothing() {
        mock::reset();
        let bytes = [1u8, 2, 3, 4, 5];
        let map = shared(&bytes);
        for px in [Pixels::Own(&bytes), Pixels::Shared(&map)] {
            let mut out = [9u8; 4];
            assert!(px.copy_out(1, &mut out) && out == [2, 3, 4, 5]);
            assert!(!px.copy_out(2, &mut out) && out == [2, 3, 4, 5], "{px:?}");
        }
    }
}
