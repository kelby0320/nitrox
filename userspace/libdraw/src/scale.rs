//! Downscaling, for window thumbnails.
//!
//! **One implementation, linked by both ends.** The compositor scales a window into the shell's
//! buffer; a gate on the host has to be able to say what that buffer should contain. If each
//! side had its own downscale the comparison would be checking that two roundings agree, which
//! is a weaker claim than it looks and fails for reasons nobody can act on.
//!
//! **Box average, not nearest.** A thumbnail is where a terminal's text becomes a texture, and
//! dropping seven pixels in eight turns that into aliasing noise that changes with sub-pixel
//! placement — which is exactly the kind of thing a screen comparison then reports as a
//! mismatch. Averaging costs one pass over the source, once per window per overview open
//! (`desktop-shell.md` §6), and it is deterministic in integer arithmetic.

use crate::format::{PixelFormat, Rgb};
use crate::framebuffer::{Framebuffer, Geometry};

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

/// Box-downscale a [`Framebuffer`] into a fresh byte buffer with `dst_geom`.
///
/// The convenience the host end wants: a gate has a rendered reference, not a raw slice.
pub fn downscale_framebuffer<F: Framebuffer + ?Sized>(
    src: &F,
    dst_geom: Geometry,
) -> Option<alloc::vec::Vec<u8>> {
    let src_geom = src.geometry();
    let mut packed = alloc::vec![0u8; src_geom.byte_len()];
    for y in 0..src_geom.height {
        for x in 0..src_geom.width {
            let c = src.get_pixel(x, y).unwrap_or_default();
            let off = y as usize * src_geom.pitch + x as usize * 4;
            packed[off..off + 4].copy_from_slice(&src_geom.format.encode(c).to_le_bytes());
        }
    }
    let mut dst = alloc::vec![0u8; dst_geom.byte_len()];
    box_downscale(&packed, src_geom, &mut dst, dst_geom).then_some(dst)
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

    #[test]
    fn every_source_pixel_belongs_to_exactly_one_band() {
        // **The reason the bands are computed from edges rather than from a step.** A source
        // row that no destination row covers is a row of the window that is simply not in the
        // thumbnail — and with a step it is the *last* rows that vanish, which is where a
        // terminal's most recent output is.
        let (sw, sh) = (7u32, 5u32);
        let (dw, dh) = (3u32, 2u32);
        let mut covered = alloc::vec![0u32; (sw * sh) as usize];
        for dy in 0..dh {
            let y0 = dy * sh / dh;
            let y1 = ((dy + 1) * sh / dh).max(y0 + 1);
            for dx in 0..dw {
                let x0 = dx * sw / dw;
                let x1 = ((dx + 1) * sw / dw).max(x0 + 1);
                for y in y0..y1 {
                    for x in x0..x1 {
                        covered[(y * sw + x) as usize] += 1;
                    }
                }
            }
        }
        assert!(covered.iter().all(|&n| n == 1), "coverage was {covered:?}");
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

    #[test]
    fn a_framebuffer_downscales_through_the_same_path() {
        // What the host end uses. Half of it one colour and half another, so the result cannot
        // be right by accident.
        let mut fb = MemFramebuffer::new(geom(4, 2));
        for x in 0..4 {
            fb.put_pixel(x, 0, Rgb::new(255, 0, 0));
            fb.put_pixel(x, 1, Rgb::new(0, 0, 255));
        }
        let dg = geom(2, 2);
        let out = downscale_framebuffer(&fb, dg).expect("downscale");
        let px = |i: usize| {
            let off = (i / 2) * dg.pitch + (i % 2) * 4;
            let w = u32::from_le_bytes([out[off], out[off + 1], out[off + 2], out[off + 3]]);
            dg.format.decode(w)
        };
        assert_eq!(px(0), Rgb::new(255, 0, 0), "the top row is the red one");
        assert_eq!(px(2), Rgb::new(0, 0, 255), "and the bottom the blue");
    }
}
