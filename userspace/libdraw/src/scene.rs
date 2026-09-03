//! The reference scene: the fixed picture both the host test and the guest composite.
//!
//! `docs/design/display-substrate.md` §8e: "Choosing the reference scene matters more
//! than it sounds. It wants overlap, clipping at a screen edge, and a non-trivial
//! stride. A solid-colour fill would hash fine and prove nearly nothing."
//!
//! ## It composites into its own buffer, not the real screen
//!
//! The scene fixes its own geometry ([`SCREEN_WIDTH`] × [`SCREEN_HEIGHT`] at
//! [`SCREEN_PITCH`]) rather than adapting to whatever Limine reports, because the two
//! halves of the gate test different things and only one of them involves the display
//! hardware:
//!
//! - **§8b — the guest proves the compositor.** It runs *this* function over *this*
//!   geometry and reports the hash, which the host asserts as a constant. Composing
//!   into the real framebuffer instead would make the hash depend on the emulator's
//!   resolution, and there would be no constant to assert.
//! - **§8c — `screendump` proves the framebuffer binding**, which is the part a
//!   self-hash structurally cannot check: a compositor can hash its own buffer
//!   correctly while writing to the wrong base address or with the channels swapped.
//!
//! So what §8b actually catches is the compositor behaving differently when compiled
//! for the target than on the host — integer width, endianness, optimisation — which
//! is a real class of bug and a different one from "the pixels never reached the
//! screen".
//!
//! ## Why the surfaces look like this
//!
//! Each element is present to make a specific bug visible in the hash:
//!
//! | Element | Catches |
//! |---|---|
//! | Two overlapping surfaces | stacking order applied backwards |
//! | A surface off the **left** edge | clipping that wraps or underflows a row |
//! | A surface off the **bottom-right** | clipping that overruns the buffer |
//! | A surface entirely off-screen | culling that draws it anyway |
//! | A non-trivial screen **stride** | rows written at `width × bpp` instead of `pitch` |
//! | Surface strides that differ from the screen's | the same bug on the read side |
//! | One surface in the **opposite channel order** | a blit that copies words instead of translating |
//! | One **translucent** surface, overlapping two others | a blend that rounds, saturates or reads the wrong operand differently on the target |
//! | Content varying in **both** axes | transposed rows or columns, which a solid fill cannot detect |
//!
//! That last row is the one worth dwelling on: a solid fill, or a pattern that varies
//! in only one axis, hashes *identically* when rows are swapped. The scene would look
//! thorough and catch nothing.

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
/// Deliberately **not** `SCREEN_WIDTH × 4` (256): the extra 12 bytes are three pixels
/// of row padding, so any code that computes a row offset from the width rather than
/// the pitch skews every row after the first.
pub const SCREEN_PITCH: usize = 268;

/// The background the scene is cleared to before any surface is drawn.
///
/// Not black, so a compositor that skips the clear entirely produces a different hash
/// rather than one that happens to match a zeroed buffer.
///
/// **Taken from the theme since M11 Part B**, and from its `desktop` field rather than its
/// `background` one since Part E. Part B made these one value on the argument that a window's
/// ground and the ground between windows differing shows as a seam; a light theme ends that,
/// because the two stopped being the same kind of thing — one is the paper an application draws
/// on and the other is what a desktop shows when nothing is on it. Still one *source*: a
/// `const fn` constructor is what keeps this a constant, and nothing here writes a literal.
pub const BACKGROUND: Rgb = crate::theme::Theme::light().desktop;

/// The scene's screen geometry.
pub fn screen_geometry() -> Geometry {
    Geometry::with_pitch(SCREEN_WIDTH, SCREEN_HEIGHT, SCREEN_PITCH, PixelFormat::XRGB8888)
        .expect("the reference pitch is wide enough for a row")
}

/// Fill a buffer with a pattern that varies along **both** axes.
///
/// `seed` distinguishes one surface from another. The three channels use different
/// functions of `(x, y)` so that a channel-order mistake also changes the picture.
fn fill_pattern(geometry: Geometry, seed: u8) -> Vec<u8> {
    let mut px = vec![0u8; geometry.byte_len()];
    for y in 0..geometry.height {
        for x in 0..geometry.width {
            let colour = Rgb::new(
                seed.wrapping_add((x as u8).wrapping_mul(9)),
                seed.wrapping_add((y as u8).wrapping_mul(23)),
                (x as u8).wrapping_mul(5) ^ (y as u8).wrapping_mul(3) ^ seed,
            );
            // **Opacity varies across the surface too, and reaches both extremes** (M13 Part B).
            // `blend_pixel` takes a different path at 0, at 255 and in between, so a surface at
            // one fixed opacity would exercise one of the three and hash identically whichever
            // it was. Sweeping 0..=255 along a diagonal puts all three in the picture and makes
            // a rounding difference between host and target show up as a changed hash.
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
        Element {
            geometry: Geometry::with_pitch(24, 14, 112, xrgb).unwrap(),
            origin: Point::new(6, 5),
            seed: 0x21,
        },
        // Overlaps the first, and is drawn over it.
        Element {
            geometry: Geometry::with_pitch(20, 12, 84, xrgb).unwrap(),
            origin: Point::new(18, 11),
            seed: 0x8C,
        },
        // Hangs off the left edge: negative origin.
        Element {
            geometry: Geometry::packed(18, 10, xrgb),
            origin: Point::new(-7, 20),
            seed: 0x4D,
        },
        // Hangs off the bottom-right corner, and is in the *opposite* channel order.
        Element {
            geometry: Geometry::with_pitch(14, 9, 60, xbgr).unwrap(),
            origin: Point::new(54, 25),
            seed: 0xB6,
        },
        // **Translucent, and placed over two other surfaces and the background** (M13 Part B).
        // Topmost so that what it blends with is settled: the pixels beneath it are the first
        // two elements and the ground, so this one number covers blending against a surface and
        // blending against the background. It also has a padded stride of its own, because the
        // per-pixel blend path reads its source with different arithmetic from the row copy and
        // would not inherit the stride coverage the opaque elements give.
        Element {
            geometry: Geometry::with_pitch(22, 13, 100, argb).unwrap(),
            origin: Point::new(12, 8),
            seed: 0x5A,
        },
        // Entirely off-screen: must contribute nothing. **Last on purpose** — the test that
        // drops it drops the final element, so a new element goes above this line.
        Element {
            geometry: Geometry::packed(10, 10, xrgb),
            origin: Point::new(SCREEN_WIDTH as i32 + 4, 2),
            seed: 0xF1,
        },
    ]
}

/// Composite the reference scene into a fresh in-memory framebuffer.
///
/// Pure: same output bytes on every call, on any target. That is §7's determinism
/// requirement, and the gate exists only because it holds.
pub fn render_reference() -> MemFramebuffer {
    let mut fb = MemFramebuffer::new(screen_geometry());
    let elements = elements();
    let pixels: Vec<Vec<u8>> =
        elements.iter().map(|e| fill_pattern(e.geometry, e.seed)).collect();
    let surfaces: Vec<SurfaceRef<'_>> = elements
        .iter()
        .zip(pixels.iter())
        .map(|(e, px)| SurfaceRef::new(e.geometry, e.origin, px))
        .collect();
    compose_full(&mut fb, BACKGROUND, &surfaces);
    fb
}

/// The hash of the reference scene.
///
/// **Asserted in two places on purpose** (§8b): here on the host, and by the guest's
/// display self-test once it exists (plan M1 Part C). If they ever disagree, the
/// commit that broke it is the one that fails, rather than the two quietly diverging.
///
/// **Last changed 2026-09-03**, when M13 Part B added a translucent element to the scene — the
/// sixth in the table above, blending over two opaque surfaces and the background at an opacity
/// that varies across it. The number moved because the picture did.
///
/// Changing the scene changes this number. That is expected — but it should be a
/// deliberate edit accompanied by a reason, never a value pasted in to make a red
/// test go green.
///
/// **Moved 2026-09-01 (M11 Part E, batch 1): the desktop turned light.** The ground this scene
/// is cleared to comes from the theme, and the theme's ground between windows went from
/// `#0E141B` to `#2A5570` — so every pixel of the scene not covered by a surface changed. That
/// is the reason, and it is the whole reason: no surface moved.
pub const REFERENCE_HASH: u64 = 0xbe4c_6dbe_8ed2_8ecd;

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
        // The failure mode §8e warns about: a scene that hashes fine and proves
        // nothing. Assert it actually contains varied content.
        let fb = render_reference();
        let fmt = fb.geometry().format;
        // Keyed on the encoded word: colours have no meaningful ordering of their own.
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
        // Top-left corner is outside every element.
        assert_eq!(fb.get_pixel(0, 0), Some(BACKGROUND));
    }

    #[test]
    fn the_off_screen_element_contributes_nothing() {
        // Re-render without the last element and confirm the picture is identical.
        let mut fb = MemFramebuffer::new(screen_geometry());
        let elements = elements();
        let pixels: Vec<Vec<u8>> =
            elements.iter().map(|e| fill_pattern(e.geometry, e.seed)).collect();
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
        // Guards the scene itself against being weakened by a later edit.
        let fb = render_reference();
        let e = elements();
        // Overlap: elements 0 and 1 share pixels.
        assert!(
            SurfaceRef::new(e[0].geometry, e[0].origin, &[])
                .bounds()
                .intersect(&SurfaceRef::new(e[1].geometry, e[1].origin, &[]).bounds())
                .is_some(),
            "the scene must contain overlapping surfaces"
        );
        // Left-edge clip: something is drawn at x = 0 that is not background.
        assert_ne!(fb.get_pixel(0, 22), Some(BACKGROUND), "left-edge clip not exercised");
        // Bottom-right clip: something is drawn in the far corner.
        assert_ne!(
            fb.get_pixel(SCREEN_WIDTH - 1, SCREEN_HEIGHT - 1),
            Some(BACKGROUND),
            "bottom-right clip not exercised"
        );
        // Non-trivial stride.
        assert_ne!(SCREEN_PITCH, SCREEN_WIDTH as usize * 4, "stride must not be packed");
    }
}
