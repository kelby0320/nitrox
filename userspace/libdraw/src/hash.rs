//! A stable hash of a framebuffer's *visible* pixels.
//!
//! This is the value `docs/design/display-substrate.md` §8b asserts in two places:
//! the guest composites the reference scene and reports its hash, and the host test
//! asserts the same constant against its in-memory composite. "If the host and the
//! guest disagree, one of them is wrong and the commit that broke it is the one that
//! fails."
//!
//! **Row padding is excluded, and that is the whole design.** Hashing whole rows
//! would fold `pitch - width × bpp` bytes of never-written memory into the result, so
//! the same picture would hash differently depending on what happened to be in the
//! allocation. §7 forbids exactly that. It also means a host buffer and a guest
//! framebuffer with *different* strides hash identically for the same image, which is
//! what lets the two sides be compared at all.

use crate::framebuffer::Framebuffer;

/// FNV-1a offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Hash the visible pixels of a framebuffer.
///
/// FNV-1a, chosen because it is a dozen lines with no dependencies — the kernel and
/// userspace both forbid pulling in a crate for this — and because the gate needs a
/// *stable* fingerprint, not a cryptographic one. Nothing here defends against an
/// adversary; it detects accidental change.
///
/// The geometry is folded in first, so two images that differ only in size cannot
/// collide by having the same pixel bytes.
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
        // The property that lets the host's buffer and the guest's framebuffer be
        // compared at all: same picture, different row padding, same hash.
        assert_eq!(hash_visible(&painted(32)), hash_visible(&painted(96)));
    }

    #[test]
    fn padding_contents_do_not_change_the_hash() {
        let mut a = painted(96);
        // Scribble in the row padding, which no correct writer ever touches.
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
        // The reason the reference scene must vary in both axes: if swapping two rows
        // did not change the hash, the gate could not catch a stride bug.
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
        // Identical pixel bytes (all zero), different shape.
        assert_eq!(wide.bytes(), tall.bytes());
        assert_ne!(hash_visible(&wide), hash_visible(&tall));
    }
}
