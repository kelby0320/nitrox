//! A stable hash of a framebuffer's *visible* pixels.
//!
//! The value `docs/design/display-substrate.md` §8b asserts in two places: the guest composites
//! the reference scene and reports its hash, and the host test asserts the same constant.
//!
//! Row padding is excluded: hashing whole rows would fold never-written memory into the result
//! (§7 forbids that), and a host buffer and a guest framebuffer with different strides must hash
//! identically for the same image.
//!
//! The pixel's own padding is not excluded: in `XRGB8888` the X byte is hashed. Every producer
//! goes through [`PixelFormat::encode`](crate::format::PixelFormat::encode), which leaves it zero,
//! but a client that `memcpy`s ARGB data into an XRGB surface would move `REFERENCE_HASH` with a
//! correct picture. Masking X would cost a pass over every pixel; do it when a client trips this.

use crate::framebuffer::Framebuffer;

/// FNV-1a offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Hash the visible pixels of a framebuffer.
///
/// FNV-1a: a dozen lines with no dependency, and the gate needs a stable fingerprint, not a
/// cryptographic one.
///
/// The geometry is folded in first, so two images differing only in size cannot collide.
pub fn hash_visible<F: Framebuffer + ?Sized>(fb: &F) -> u64 {
    let g = fb.geometry();
    let bytes = fb.bytes();
    let row_bytes = g.width as usize * g.format.bytes_per_pixel();

    let mut h = FNV_OFFSET;
    let mut mix = |b: u8| {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    };
    for b in g.width.to_le_bytes() {
        mix(b);
    }
    for b in g.height.to_le_bytes() {
        mix(b);
    }
    for y in 0..g.height as usize {
        let start = y * g.pitch;
        for &b in &bytes[start..start + row_bytes] {
            mix(b);
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{PixelFormat, Rgb};
    use crate::framebuffer::{Geometry, MemFramebuffer};
    use crate::geom::Rect;

    fn painted(pitch: usize) -> MemFramebuffer {
        let g = Geometry::with_pitch(8, 4, pitch, PixelFormat::XRGB8888).unwrap();
        let mut fb = MemFramebuffer::new(g);
        fb.clear(Rgb::new(0x10, 0x20, 0x30));
        fb.fill_rect(Rect::new(2, 1, 3, 2), Rgb::new(0xAA, 0xBB, 0xCC));
        fb
    }

    #[test]
    fn the_same_image_hashes_the_same() {
        assert_eq!(hash_visible(&painted(32)), hash_visible(&painted(32)));
    }

    #[test]
    fn stride_does_not_change_the_hash() {
        // Same picture, different row padding, same hash: what lets host and guest be compared.
        assert_eq!(hash_visible(&painted(32)), hash_visible(&painted(96)));
    }

    #[test]
    fn padding_contents_do_not_change_the_hash() {
        let mut a = painted(96);
        let g = a.geometry();
        let row_bytes = g.width as usize * 4;
        let bytes = a.bytes_mut();
        for y in 0..g.height as usize {
            for i in row_bytes..g.pitch {
                bytes[y * g.pitch + i] = 0xEE;
            }
        }
        assert_eq!(hash_visible(&a), hash_visible(&painted(96)));
    }

    #[test]
    fn one_changed_pixel_changes_the_hash() {
        let mut a = painted(32);
        let before = hash_visible(&a);
        a.put_pixel(7, 3, Rgb::new(1, 1, 1));
        assert_ne!(hash_visible(&a), before);
    }

    #[test]
    fn transposed_content_changes_the_hash() {
        // The reference scene must vary in both axes, or the gate could not catch a stride bug.
        let mut a = painted(32);
        let g = a.geometry();
        let row = g.width as usize * 4;
        let before = hash_visible(&a);
        let bytes = a.bytes_mut();
        for i in 0..row {
            bytes.swap(i, g.pitch + i);
        }
        assert_ne!(hash_visible(&a), before);
    }

    #[test]
    fn geometry_is_folded_in_so_different_sizes_cannot_collide() {
        let wide = MemFramebuffer::new(Geometry::packed(8, 2, PixelFormat::XRGB8888));
        let tall = MemFramebuffer::new(Geometry::packed(4, 4, PixelFormat::XRGB8888));
        assert_eq!(wide.bytes(), tall.bytes());
        assert_ne!(hash_visible(&wide), hash_visible(&tall));
    }
}
