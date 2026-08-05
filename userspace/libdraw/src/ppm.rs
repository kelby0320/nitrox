//! Writing a framebuffer out as a PPM image, for looking at a failure.
//!
//! The gate compares a 64-bit hash (§8b), which answers *whether* something changed and
//! nothing about *what*. When the guest and the host disagree, a stride skew, a swapped
//! channel and a wrong stacking order all present identically: two hex numbers. This
//! turns that into a picture.
//!
//! **P6 binary PPM**, chosen because it is a text header followed by raw RGB triples —
//! about ten lines to emit, no dependency, and every image viewer opens it. A PNG would
//! mean a compressor, and the kernel and userspace both forbid pulling in a crate for
//! this.
//!
//! Only the *visible* pixels are written: row padding is never part of the image, for
//! the same reason [`hash_visible`](crate::hash::hash_visible) excludes it.

use alloc::vec::Vec;

use crate::framebuffer::Framebuffer;

/// Encode a framebuffer as a binary PPM (P6) image.
pub fn to_ppm<F: Framebuffer + ?Sized>(fb: &F) -> Vec<u8> {
    let g = fb.geometry();
    let mut out = Vec::new();

    // Header: "P6\n<width> <height>\n255\n", written without `format!` so this stays
    // usable from a `no_std` binary with no formatting machinery.
    out.extend_from_slice(b"P6\n");
    push_decimal(&mut out, g.width as u64);
    out.push(b' ');
    push_decimal(&mut out, g.height as u64);
    out.extend_from_slice(b"\n255\n");

    for y in 0..g.height {
        for x in 0..g.width {
            let c = fb.get_pixel(x, y).unwrap_or_default();
            out.push(c.r);
            out.push(c.g);
            out.push(c.b);
        }
    }
    out
}

/// Append `n` in base 10.
fn push_decimal(out: &mut Vec<u8>, n: u64) {
    if n == 0 {
        out.push(b'0');
        return;
    }
    let mut digits = [0u8; 20];
    let mut i = 0;
    let mut n = n;
    while n > 0 {
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        out.push(digits[i]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{PixelFormat, Rgb};
    use crate::framebuffer::{Geometry, MemFramebuffer};
    use crate::geom::Rect;

    #[test]
    fn the_header_and_body_are_the_documented_size() {
        let g = Geometry::with_pitch(3, 2, 40, PixelFormat::XRGB8888).unwrap();
        let mut fb = MemFramebuffer::new(g);
        fb.clear(Rgb::new(1, 2, 3));
        let ppm = to_ppm(&fb);
        assert!(ppm.starts_with(b"P6\n3 2\n255\n"));
        // Header + 3 bytes per visible pixel. Padding must not appear.
        assert_eq!(ppm.len(), b"P6\n3 2\n255\n".len() + 3 * 3 * 2);
        assert_eq!(&ppm[b"P6\n3 2\n255\n".len()..][..3], &[1, 2, 3]);
    }

    #[test]
    fn pixels_appear_in_row_major_order() {
        let g = Geometry::packed(2, 2, PixelFormat::XRGB8888);
        let mut fb = MemFramebuffer::new(g);
        fb.fill_rect(Rect::new(0, 0, 2, 2), Rgb::BLACK);
        fb.put_pixel(1, 0, Rgb::new(10, 20, 30));
        fb.put_pixel(0, 1, Rgb::new(40, 50, 60));
        let ppm = to_ppm(&fb);
        let body = &ppm[b"P6\n2 2\n255\n".len()..];
        assert_eq!(&body[3..6], &[10, 20, 30], "(1,0) is the second pixel");
        assert_eq!(&body[6..9], &[40, 50, 60], "(0,1) is the third pixel");
    }

    #[test]
    fn a_large_dimension_is_written_in_full() {
        // `push_decimal` is hand-rolled; a truncated dimension would silently produce
        // an image that no viewer can open.
        let mut out = Vec::new();
        push_decimal(&mut out, 1280);
        assert_eq!(&out[..], b"1280");
        out.clear();
        push_decimal(&mut out, 0);
        assert_eq!(&out[..], b"0");
    }
}
