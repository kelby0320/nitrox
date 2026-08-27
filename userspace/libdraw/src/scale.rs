//! Downscaling, for window thumbnails.
//!
//! **One implementation, and it is specified so that a second end can be built against it.**
//! The compositor scales a window into the shell's buffer. A host-side gate that wanted to say
//! what that buffer should contain would link this rather than write its own — because a
//! comparison between two independent downscales is checking that two roundings agree, which is
//! a weaker claim than it looks and fails for reasons nobody can act on.
//!
//! **No such gate exists today, and the first version of this doc said one did.** Nothing under
//! `tools/` links this module: the shell's buffer never leaves the guest, and the only gate that
//! compares pixels boots an image with no shell in it. What pins the output is the unit tests
//! below, which is why they are written against inputs where averaging and sampling disagree
//! rather than against a re-derivation of the arithmetic (PR #244 review, blocking 1 and 2).
//!
//! **Box average, not nearest.** A thumbnail is where a terminal's text becomes a texture, and
//! dropping seven pixels in eight turns that into aliasing noise that changes with sub-pixel
//! placement — which is exactly the kind of thing a screen comparison then reports as a
//! mismatch. Averaging costs one pass over the source, once per window per overview open
//! (`desktop-shell.md` §6), and it is deterministic in integer arithmetic.

use crate::format::{PixelFormat, Rgb};
use crate::framebuffer::Geometry;

/// Average the `src` rectangle covering one destination pixel.
///
/// Integer arithmetic throughout: the sum of at most `sw * sh` bytes per channel fits a `u32`
/// for any source this can be handed, and the division truncates the same way on every target.
fn average(src: &[u8], src_geom: Geometry, x0: u32, y0: u32, x1: u32, y1: u32) -> Rgb {
    let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
    for y in y0..y1 {
        for x in x0..x1 {
            let off = y as usize * src_geom.pitch + x as usize * 4;
            if off + 4 > src.len() {
                continue;
            }
            let word = u32::from_le_bytes([src[off], src[off + 1], src[off + 2], src[off + 3]]);
            let c = src_geom.format.decode(word);
            r += c.r as u32;
            g += c.g as u32;
            b += c.b as u32;
            n += 1;
        }
    }
    if n == 0 {
        return Rgb::new(0, 0, 0);
    }
    Rgb::new((r / n) as u8, (g / n) as u8, (b / n) as u8)
}

/// Box-downscale `src` into `dst`, both XRGB8888.
///
/// Returns `false` if either geometry is unusable — a zero dimension, a source shorter than its
/// own geometry claims, or a destination larger than the source in either axis. **Refused
/// rather than clamped**: a caller asking to scale *up* has misunderstood what this is for, and
/// silently returning the source at a different size would look like it worked.
pub fn box_downscale(src: &[u8], src_geom: Geometry, dst: &mut [u8], dst_geom: Geometry) -> bool {
    if src_geom.width == 0
        || src_geom.height == 0
        || dst_geom.width == 0
        || dst_geom.height == 0
        || dst_geom.width > src_geom.width
        || dst_geom.height > src_geom.height
        || src.len() < src_geom.byte_len()
        || dst.len() < dst_geom.byte_len()
        || src_geom.format != PixelFormat::XRGB8888
        || dst_geom.format != PixelFormat::XRGB8888
    {
        return false;
    }
    for dy in 0..dst_geom.height {
        // The source band this destination row covers. Computed from the *edges* rather than
        // from a step, so every source row belongs to exactly one band and none is skipped.
        let y0 = dy * src_geom.height / dst_geom.height;
        let y1 = ((dy + 1) * src_geom.height / dst_geom.height).max(y0 + 1);
        for dx in 0..dst_geom.width {
            let x0 = dx * src_geom.width / dst_geom.width;
            let x1 = ((dx + 1) * src_geom.width / dst_geom.width).max(x0 + 1);
            let c = average(src, src_geom, x0, y0, x1, y1);
            let off = dy as usize * dst_geom.pitch + dx as usize * 4;
            let word = dst_geom.format.encode(c).to_le_bytes();
            dst[off..off + 4].copy_from_slice(&word);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framebuffer::MemFramebuffer;

    fn geom(w: u32, h: u32) -> Geometry {
        Geometry::packed(w, h, PixelFormat::XRGB8888)
    }

    #[test]
    fn a_flat_source_downscales_to_the_same_colour() {
        // The property that makes a thumbnail recognisable: averaging equal pixels cannot
        // introduce a colour that was not there.
        let g = geom(8, 8);
        let mut src = alloc::vec![0u8; g.byte_len()];
        for y in 0..8 {
            for x in 0..8 {
                let off = y * g.pitch + x * 4;
                src[off..off + 4].copy_from_slice(&g.format.encode(Rgb::new(9, 40, 200)).to_le_bytes());
            }
        }
        let dg = geom(2, 2);
        let mut dst = alloc::vec![0u8; dg.byte_len()];
        assert!(box_downscale(&src, g, &mut dst, dg));
        for i in 0..4 {
            let off = (i / 2) * dg.pitch + (i % 2) * 4;
            let word = u32::from_le_bytes([dst[off], dst[off + 1], dst[off + 2], dst[off + 3]]);
            assert_eq!(dg.format.decode(word), Rgb::new(9, 40, 200), "pixel {i}");
        }
    }

    /// Fill `g` from a per-pixel closure, and return the packed bytes.
    fn packed(g: Geometry, f: impl Fn(u32, u32) -> Rgb) -> alloc::vec::Vec<u8> {
        let mut v = alloc::vec![0u8; g.byte_len()];
        for y in 0..g.height {
            for x in 0..g.width {
                let off = y as usize * g.pitch + x as usize * 4;
                v[off..off + 4].copy_from_slice(&g.format.encode(f(x, y)).to_le_bytes());
            }
        }
        v
    }

    /// Read destination pixel `(x, y)`.
    fn at(dst: &[u8], g: Geometry, x: u32, y: u32) -> Rgb {
        let off = y as usize * g.pitch + x as usize * 4;
        g.format.decode(u32::from_le_bytes([dst[off], dst[off + 1], dst[off + 2], dst[off + 3]]))
    }

    #[test]
    fn it_averages_rather_than_sampling() {
        // **Chosen so the two answers differ.** Three source pixels — black, black, white —
        // into one: the mean is 85 and any form of point sampling gives 0 or 255. A uniform
        // source cannot tell these apart, which is why the first version of this suite could
        // not: averaging equal pixels and picking one of them are the same answer.
        let sg = geom(3, 1);
        let src = packed(sg, |x, _| if x == 2 { Rgb::new(255, 255, 255) } else { Rgb::new(0, 0, 0) });
        let dg = geom(1, 1);
        let mut dst = alloc::vec![0u8; dg.byte_len()];
        assert!(box_downscale(&src, sg, &mut dst, dg));
        assert_eq!(at(&dst, dg, 0, 0), Rgb::new(85, 85, 85), "nearest-neighbour would give 0 or 255");
    }

    #[test]
    fn the_last_source_row_reaches_the_thumbnail() {
        // **The bands are derived from edges, not from a step**, and this is what that buys.
        // With a step of `sh / dh` the last rows fall outside every band — in a terminal, the
        // most recent output. Five rows into two does not divide, so a step would cover rows
        // 0..1 and 2..3 and drop row 4 entirely.
        //
        // The source is black except for its **last** row, which is white. If the last row is
        // dropped the bottom destination pixel is pure black; averaged, it is not.
        let sg = geom(4, 5);
        let src = packed(sg, |_, y| if y == 4 { Rgb::new(255, 255, 255) } else { Rgb::new(0, 0, 0) });
        let dg = geom(2, 2);
        let mut dst = alloc::vec![0u8; dg.byte_len()];
        assert!(box_downscale(&src, sg, &mut dst, dg));
        let bottom = at(&dst, dg, 0, 1);
        assert_ne!(bottom, Rgb::new(0, 0, 0), "the last source row never reached the thumbnail");
        // Rows 2,3,4 average to (0 + 0 + 255) / 3 = 85.
        assert_eq!(bottom, Rgb::new(85, 85, 85));
        assert_eq!(at(&dst, dg, 0, 0), Rgb::new(0, 0, 0), "and the top band is all black");
    }

    #[test]
    fn an_uneven_split_covers_every_column_too() {
        // The same claim on the other axis, and with a width that does not divide: seven
        // columns into three. Column 6 is the one a step would drop.
        let sg = geom(7, 1);
        let src = packed(sg, |x, _| if x == 6 { Rgb::new(0, 0, 240) } else { Rgb::new(0, 0, 0) });
        let dg = geom(3, 1);
        let mut dst = alloc::vec![0u8; dg.byte_len()];
        assert!(box_downscale(&src, sg, &mut dst, dg));
        assert_ne!(at(&dst, dg, 2, 0), Rgb::new(0, 0, 0), "the last source column was dropped");
    }

    #[test]
    fn scaling_up_is_refused_rather_than_clamped() {
        let g = geom(4, 4);
        let src = alloc::vec![0u8; g.byte_len()];
        let dg = geom(8, 8);
        let mut dst = alloc::vec![0u8; dg.byte_len()];
        assert!(!box_downscale(&src, g, &mut dst, dg), "a caller asking to scale up is confused");
    }

    #[test]
    fn a_short_destination_is_refused_rather_than_written_partially() {
        let g = geom(8, 8);
        let src = alloc::vec![0u8; g.byte_len()];
        let dg = geom(4, 4);
        let mut dst = alloc::vec![0u8; dg.byte_len() - 1];
        assert!(!box_downscale(&src, g, &mut dst, dg));
    }

}
