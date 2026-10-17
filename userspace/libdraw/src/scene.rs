//! The reference scene: the fixed picture both the host test and the guest composite.
//!
//! `docs/design/display-substrate.md` §8e: the scene wants overlap, clipping at a screen edge and
//! a non-trivial stride; a solid fill would hash fine and prove nearly nothing.
//!
//! The scene composites into its own buffer of fixed geometry ([`SCREEN_WIDTH`] ×
//! [`SCREEN_HEIGHT`] at [`SCREEN_PITCH`]), not the real screen, so the hash does not depend on the
//! emulator's resolution. §8b (the guest runs this function and reports the hash) catches the
//! compositor behaving differently on the target; §8c (`screendump`) catches a wrong base address
//! or swapped channels, which a self-hash cannot.
//!
//! Each element makes a specific bug visible in the hash:
//!
//! | Element | Catches |
//! |---|---|
//! | Two overlapping surfaces | stacking order applied backwards |
//! | A surface off the **left** edge | clipping that wraps or underflows a row |
//! | A surface off the **bottom-right** | clipping that overruns the buffer |
//! | A surface entirely off-screen | culling that draws it anyway |
//! | A non-trivial screen **stride** | rows written at `width × bpp` instead of `pitch` |
//! | Surface strides that differ from the screen's | the same bug on the read side |
//! | One surface in the **opposite channel order** | a blit copying words, not translating |
//! | One **translucent** surface over two others | a blend that rounds differently on the target |
//! | Content varying in **both** axes | transposed rows or columns, which a solid fill hides |
//!
//! A pattern varying in only one axis hashes identically when rows are swapped.

use alloc::vec;
use alloc::vec::Vec;

use crate::compose::{SurfaceRef, compose_full};
use crate::format::{PixelFormat, Rgb};
use crate::framebuffer::{Geometry, MemFramebuffer};
use crate::geom::Point;
use crate::hash::hash_visible;

/// Reference screen width, in pixels.
pub const SCREEN_WIDTH: u32 = 64;
/// Reference screen height, in pixels.
pub const SCREEN_HEIGHT: u32 = 32;
/// Reference screen stride, in bytes.
///
/// Not `SCREEN_WIDTH × 4` (256): the extra 12 bytes are three pixels of row padding, so code
/// computing a row offset from the width rather than the pitch skews every row after the first.
pub const SCREEN_PITCH: usize = 268;

/// The background the scene is cleared to before any surface is drawn.
///
/// Not black, so a compositor that skips the clear does not match a zeroed buffer.
///
/// Taken from the theme's `desktop` field, not `background`: the paper an application draws on
/// and what a desktop shows when nothing is on it are different things. A `const fn` constructor
/// keeps this a constant.
pub const BACKGROUND: Rgb = crate::theme::Theme::light().desktop;

/// The scene's screen geometry.
pub fn screen_geometry() -> Geometry {
    Geometry::with_pitch(SCREEN_WIDTH, SCREEN_HEIGHT, SCREEN_PITCH, PixelFormat::XRGB8888)
        .expect("the reference pitch is wide enough for a row")
}

/// Fill a buffer with a pattern that varies along **both** axes.
///
/// `seed` distinguishes surfaces; the channels use different functions of `(x, y)` so a
/// channel-order mistake changes the picture.
fn fill_pattern(geometry: Geometry, seed: u8) -> Vec<u8> {
    let mut px = vec![0u8; geometry.byte_len()];
    for y in 0..geometry.height {
        for x in 0..geometry.width {
            let colour = Rgb::new(
                seed.wrapping_add((x as u8).wrapping_mul(9)),
                seed.wrapping_add((y as u8).wrapping_mul(23)),
                (x as u8).wrapping_mul(5) ^ (y as u8).wrapping_mul(3) ^ seed,
            );
            // Opacity varies too and reaches both extremes: `blend_pixel` takes a different path at
            // 0, at 255 and between, and a fixed opacity would exercise one. A rounding difference
            // between host and target then changes the hash.
            let alpha = ((x.wrapping_mul(11) ^ y.wrapping_mul(37)) & 0xFF) as u8;
            let off = geometry.offset_of(x, y).expect("in bounds by construction");
            let word = geometry.format.encode_alpha(colour, alpha);
            px[off..off + 4].copy_from_slice(&word.to_le_bytes());
        }
    }
    px
}

/// One surface's shape and placement in the scene.
struct Element {
    geometry: Geometry,
    origin: Point,
    seed: u8,
}

/// The scene's surfaces, in stacking order (bottom first).
fn elements() -> [Element; 6] {
    let xrgb = PixelFormat::XRGB8888;
    let xbgr = PixelFormat::XBGR8888;
    let argb = PixelFormat::ARGB8888;
    [
        // Fully on-screen, padded stride of its own.
        Element { geometry: Geometry::with_pitch(24, 14, 112, xrgb).unwrap(), origin: Point::new(6, 5), seed: 0x21 },
        // Overlaps the first, and is drawn over it.
        Element { geometry: Geometry::with_pitch(20, 12, 84, xrgb).unwrap(), origin: Point::new(18, 11), seed: 0x8C },
        // Hangs off the left edge: negative origin.
        Element { geometry: Geometry::packed(18, 10, xrgb), origin: Point::new(-7, 20), seed: 0x4D },
        // Hangs off the bottom-right corner, and is in the *opposite* channel order.
        Element { geometry: Geometry::with_pitch(14, 9, 60, xbgr).unwrap(), origin: Point::new(54, 25), seed: 0xB6 },
        // Translucent, topmost so what it blends with is settled: two surfaces and the ground. Its
        // padded stride matters because the per-pixel blend reads its source differently from the
        // row copy.
        Element { geometry: Geometry::with_pitch(22, 13, 100, argb).unwrap(), origin: Point::new(12, 8), seed: 0x5A },
        // Entirely off-screen: must contribute nothing. Last on purpose: the test that drops it
        // drops the final element, so a new element goes above this line.
        Element {
            geometry: Geometry::packed(10, 10, xrgb),
            origin: Point::new(SCREEN_WIDTH as i32 + 4, 2),
            seed: 0xF1,
        },
    ]
}

/// Composite the reference scene into a fresh in-memory framebuffer.
///
/// Pure: the same bytes on every call, on any target (§7).
pub fn render_reference() -> MemFramebuffer {
    let mut fb = MemFramebuffer::new(screen_geometry());
    let elements = elements();
    let pixels: Vec<Vec<u8>> = elements.iter().map(|e| fill_pattern(e.geometry, e.seed)).collect();
    let surfaces: Vec<SurfaceRef<'_>> =
        elements.iter().zip(pixels.iter()).map(|(e, px)| SurfaceRef::new(e.geometry, e.origin, px)).collect();
    compose_full(&mut fb, BACKGROUND, &surfaces);
    fb
}

/// The hash of the reference scene.
///
/// Asserted in two places (§8b): here on the host and by the guest's display self-test. Changing
/// the scene changes this number; make that a deliberate edit with a reason, never a value pasted
/// in to make a test pass.
pub const REFERENCE_HASH: u64 = 0x4c11_d044_5f77_996e;

/// Hash the reference scene.
pub fn reference_hash() -> u64 {
    hash_visible(&render_reference())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framebuffer::Framebuffer;

    #[test]
    fn the_scene_is_deterministic() {
        assert_eq!(render_reference().into_bytes(), render_reference().into_bytes());
    }

    #[test]
    fn the_scene_matches_its_recorded_hash() {
        assert_eq!(
            reference_hash(),
            REFERENCE_HASH,
            "the reference scene changed; update REFERENCE_HASH deliberately, and say why"
        );
    }

    #[test]
    fn the_scene_is_not_a_flat_fill() {
        // A scene that hashes fine and proves nothing (§8e).
        let fb = render_reference();
        let fmt = fb.geometry().format;
        let mut seen = alloc::collections::BTreeSet::new();
        for y in 0..SCREEN_HEIGHT {
            for x in 0..SCREEN_WIDTH {
                seen.insert(fmt.encode(fb.get_pixel(x, y).unwrap()));
            }
        }
        assert!(seen.len() > 500, "only {} distinct colours — too flat to catch much", seen.len());
    }

    #[test]
    fn the_background_shows_where_no_surface_covers() {
        let fb = render_reference();
        assert_eq!(fb.get_pixel(0, 0), Some(BACKGROUND));
    }

    #[test]
    fn the_off_screen_element_contributes_nothing() {
        let mut fb = MemFramebuffer::new(screen_geometry());
        let elements = elements();
        let pixels: Vec<Vec<u8>> = elements.iter().map(|e| fill_pattern(e.geometry, e.seed)).collect();
        let surfaces: Vec<SurfaceRef<'_>> = elements
            .iter()
            .zip(pixels.iter())
            .take(elements.len() - 1) // drop the off-screen one, which is last by construction
            .map(|(e, px)| SurfaceRef::new(e.geometry, e.origin, px))
            .collect();
        compose_full(&mut fb, BACKGROUND, &surfaces);
        assert_eq!(fb.into_bytes(), render_reference().into_bytes());
    }

    #[test]
    fn the_scene_exercises_overlap_and_both_clipped_edges() {
        let fb = render_reference();
        let e = elements();
        assert!(
            SurfaceRef::new(e[0].geometry, e[0].origin, &[])
                .bounds()
                .intersect(&SurfaceRef::new(e[1].geometry, e[1].origin, &[]).bounds())
                .is_some(),
            "the scene must contain overlapping surfaces"
        );
        assert_ne!(fb.get_pixel(0, 22), Some(BACKGROUND), "left-edge clip not exercised");
        assert_ne!(
            fb.get_pixel(SCREEN_WIDTH - 1, SCREEN_HEIGHT - 1),
            Some(BACKGROUND),
            "bottom-right clip not exercised"
        );
        assert_ne!(SCREEN_PITCH, SCREEN_WIDTH as usize * 4, "stride must not be packed");
    }
}
