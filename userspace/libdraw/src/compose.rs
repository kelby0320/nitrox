//! Surfaces, and compositing as a pure function.
//!
//! `docs/design/display-substrate.md` §8a: "The piece that *looks* like it needs a
//! screen is compositing, and it does not." Given surfaces, geometry, damage and
//! stacking, the output bytes are determined — so this is an ordinary function with
//! ordinary tests, and §7's determinism requirement is what keeps it that way.

use crate::format::Rgb;
use crate::framebuffer::{Framebuffer, Geometry};
use crate::geom::{Point, Rect};

/// A client's pixel buffer, positioned on screen.
///
/// Borrowed rather than owned: in the compositor these bytes are a mapped
/// `MemoryObject` the client drew into (§4), and in tests they are an ordinary
/// allocation. Neither case wants this type to own them.
#[derive(Clone, Copy, Debug)]
pub struct SurfaceRef<'a> {
    /// The surface's own shape — its size, stride and format, which need not match
    /// the framebuffer's.
    pub geometry: Geometry,
    /// Where the surface's top-left corner sits in screen space. May be negative.
    pub origin: Point,
    /// The surface's pixels, at least `geometry.byte_len()` bytes.
    pub pixels: &'a [u8],
}

impl<'a> SurfaceRef<'a> {
    /// A surface at `origin` over `pixels`.
    pub const fn new(geometry: Geometry, origin: Point, pixels: &'a [u8]) -> Self {
        Self { geometry, origin, pixels }
    }

    /// The surface's bounds in screen space.
    pub const fn bounds(&self) -> Rect {
        Rect::new(self.origin.x, self.origin.y, self.geometry.width, self.geometry.height)
    }
}

/// Composite `surfaces` onto `fb` within `damage`.
///
/// `surfaces` is in stacking order, **bottom first**; later entries paint over
/// earlier ones. Each damage rectangle is repainted from scratch: `background`, then
/// every surface that intersects it. Rectangles outside the screen contribute
/// nothing, and overlapping damage rectangles are simply painted twice, which is
/// wasteful but not wrong.
///
/// **Determinism** (§7) is a property of this function, and it is what the gate rests
/// on: output depends only on the arguments, in the order given. No clock, no
/// allocation order, no dependence on which client committed first. The one subtlety
/// is that a damaged region is *cleared before* the surfaces are drawn, so a pixel no
/// surface covers is `background` rather than whatever it previously held.
pub fn compose<F: Framebuffer + ?Sized>(
    fb: &mut F,
    background: Rgb,
    surfaces: &[SurfaceRef<'_>],
    damage: &[Rect],
) {
    let screen = fb.geometry().bounds();
    for area in damage {
        let Some(area) = area.intersect(&screen) else { continue };
        fb.fill_rect(area, background);
        for surface in surfaces {
            blit_clipped(fb, surface, &area);
        }
    }
}

/// Composite over the whole screen. Equivalent to [`compose`] with one full-screen
/// damage rectangle; the shape a first frame takes, before anything is incremental.
pub fn compose_full<F: Framebuffer + ?Sized>(
    fb: &mut F,
    background: Rgb,
    surfaces: &[SurfaceRef<'_>],
) {
    let screen = fb.geometry().bounds();
    compose(fb, background, surfaces, &[screen]);
}

/// Draw the part of `surface` that falls inside `area`.
///
/// Pixels are translated through `Rgb` rather than copied as words, because a surface
/// need not share the framebuffer's channel order. Copying raw words would be faster
/// and would silently swap channels the moment the two formats differ.
fn blit_clipped<F: Framebuffer + ?Sized>(fb: &mut F, surface: &SurfaceRef<'_>, area: &Rect) {
    let Some(visible) = surface.bounds().intersect(area) else { return };
    let src = surface.geometry;
    let src_bpp = src.format.bytes_per_pixel();

    for row in 0..visible.size.h {
        let dst_y = visible.origin.y + row as i32;
        // Where this screen row sits inside the surface.
        let src_y = (dst_y - surface.origin.y) as u32;
        for col in 0..visible.size.w {
            let dst_x = visible.origin.x + col as i32;
            let src_x = (dst_x - surface.origin.x) as u32;
            let Some(off) = src.offset_of(src_x, src_y) else { continue };
            if off + src_bpp > surface.pixels.len() {
                continue;
            }
            let word = u32::from_le_bytes([
                surface.pixels[off],
                surface.pixels[off + 1],
                surface.pixels[off + 2],
                surface.pixels[off + 3],
            ]);
            fb.put_pixel(dst_x as u32, dst_y as u32, src.format.decode(word));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::PixelFormat;
    use crate::framebuffer::MemFramebuffer;
    use alloc::vec;
    use alloc::vec::Vec;

    /// A surface whose pixel content varies in **both** axes.
    ///
    /// This matters more than it looks: a solid fill, or a pattern that varies in
    /// only one axis, hashes identically when rows or columns are transposed — so it
    /// cannot catch a stride bug, which is the most likely blit defect.
    fn patterned(w: u32, h: u32, pitch: usize, seed: u8) -> (Geometry, Vec<u8>) {
        let g = Geometry::with_pitch(w, h, pitch, PixelFormat::XRGB8888).unwrap();
        let mut px = vec![0u8; g.byte_len()];
        for y in 0..h {
            for x in 0..w {
                let c = Rgb::new(
                    seed.wrapping_add((x * 7) as u8),
                    seed.wrapping_add((y * 11) as u8),
                    seed ^ (x as u8).wrapping_mul(y as u8),
                );
                let off = g.offset_of(x, y).unwrap();
                px[off..off + 4].copy_from_slice(&g.format.encode(c).to_le_bytes());
            }
        }
        (g, px)
    }

    fn screen() -> MemFramebuffer {
        MemFramebuffer::new(Geometry::with_pitch(32, 16, 140, PixelFormat::XRGB8888).unwrap())
    }

    #[test]
    fn a_surface_lands_at_its_origin_pixel_for_pixel() {
        let (g, px) = patterned(4, 3, 16, 0x40);
        let mut fb = screen();
        compose_full(&mut fb, Rgb::BLACK, &[SurfaceRef::new(g, Point::new(2, 1), &px)]);

        for y in 0..3u32 {
            for x in 0..4u32 {
                let off = g.offset_of(x, y).unwrap();
                let word = u32::from_le_bytes([px[off], px[off + 1], px[off + 2], px[off + 3]]);
                assert_eq!(
                    fb.get_pixel(x + 2, y + 1),
                    Some(g.format.decode(word)),
                    "surface pixel ({x},{y})"
                );
            }
        }
        // Outside the surface stays background.
        assert_eq!(fb.get_pixel(1, 1), Some(Rgb::BLACK));
        assert_eq!(fb.get_pixel(6, 1), Some(Rgb::BLACK));
    }

    #[test]
    fn later_surfaces_paint_over_earlier_ones() {
        let (ga, pa) = patterned(6, 6, 24, 0x10);
        let (gb, pb) = patterned(6, 6, 24, 0x90);
        let mut fb = screen();
        compose_full(
            &mut fb,
            Rgb::BLACK,
            &[
                SurfaceRef::new(ga, Point::new(0, 0), &pa),
                SurfaceRef::new(gb, Point::new(3, 3), &pb),
            ],
        );
        // In the overlap, the top surface wins.
        let off = gb.offset_of(0, 0).unwrap();
        let word = u32::from_le_bytes([pb[off], pb[off + 1], pb[off + 2], pb[off + 3]]);
        assert_eq!(fb.get_pixel(3, 3), Some(gb.format.decode(word)));
        // Outside the overlap, the bottom one is intact.
        let off = ga.offset_of(0, 0).unwrap();
        let word = u32::from_le_bytes([pa[off], pa[off + 1], pa[off + 2], pa[off + 3]]);
        assert_eq!(fb.get_pixel(0, 0), Some(ga.format.decode(word)));
    }

    #[test]
    fn stacking_order_is_not_symmetric() {
        // A guard against the previous test passing because both surfaces happen to
        // agree: swapping the order must change the result.
        let (ga, pa) = patterned(6, 6, 24, 0x10);
        let (gb, pb) = patterned(6, 6, 24, 0x90);
        let a = SurfaceRef::new(ga, Point::new(0, 0), &pa);
        let b = SurfaceRef::new(gb, Point::new(3, 3), &pb);

        let mut lower_first = screen();
        compose_full(&mut lower_first, Rgb::BLACK, &[a, b]);
        let mut upper_first = screen();
        compose_full(&mut upper_first, Rgb::BLACK, &[b, a]);
        assert_ne!(lower_first, upper_first);
    }

    #[test]
    fn a_surface_off_the_left_and_top_edges_is_clipped_not_wrapped() {
        let (g, px) = patterned(8, 8, 32, 0x55);
        let mut fb = screen();
        compose_full(&mut fb, Rgb::BLACK, &[SurfaceRef::new(g, Point::new(-3, -2), &px)]);

        // Screen (0,0) shows the surface's (3,2), not its (0,0).
        let off = g.offset_of(3, 2).unwrap();
        let word = u32::from_le_bytes([px[off], px[off + 1], px[off + 2], px[off + 3]]);
        assert_eq!(fb.get_pixel(0, 0), Some(g.format.decode(word)));
        // The clipped-away columns must not have wrapped to the far edge.
        assert_eq!(fb.get_pixel(31, 0), Some(Rgb::BLACK));
    }

    #[test]
    fn a_surface_off_the_bottom_right_is_clipped() {
        let (g, px) = patterned(8, 8, 32, 0x77);
        let mut fb = screen();
        compose_full(&mut fb, Rgb::BLACK, &[SurfaceRef::new(g, Point::new(28, 12), &px)]);
        assert!(fb.get_pixel(31, 15).is_some());
        // Nothing was written past the visible area — the buffer is exactly as long
        // as its geometry says, so an overrun would have panicked.
        assert_eq!(fb.bytes().len(), fb.geometry().byte_len());
    }

    #[test]
    fn damage_bounds_what_is_repainted() {
        let (g, px) = patterned(8, 8, 32, 0x33);
        let surfaces = [SurfaceRef::new(g, Point::new(0, 0), &px)];

        let mut full = screen();
        compose_full(&mut full, Rgb::BLACK, &surfaces);

        let mut partial = screen();
        compose(&mut partial, Rgb::BLACK, &surfaces, &[Rect::new(0, 0, 4, 8)]);

        // Inside the damage the two agree; outside it, only the full composite drew.
        assert_eq!(partial.get_pixel(2, 2), full.get_pixel(2, 2));
        assert_ne!(partial.get_pixel(6, 2), full.get_pixel(6, 2));
        assert_eq!(partial.get_pixel(6, 2), Some(Rgb::BLACK));
    }

    #[test]
    fn compositing_the_same_inputs_twice_produces_identical_bytes() {
        // §7 in one assertion: the gate cannot exist without this.
        let (ga, pa) = patterned(9, 7, 40, 0x21);
        let (gb, pb) = patterned(5, 11, 20, 0x8C);
        let surfaces = [
            SurfaceRef::new(ga, Point::new(-2, 3), &pa),
            SurfaceRef::new(gb, Point::new(27, 9), &pb),
        ];
        let mut a = screen();
        let mut b = screen();
        compose_full(&mut a, Rgb::new(3, 5, 7), &surfaces);
        compose_full(&mut b, Rgb::new(3, 5, 7), &surfaces);
        assert_eq!(a.bytes(), b.bytes());
    }

    #[test]
    fn a_surface_in_a_different_channel_order_is_translated_not_copied() {
        // The compositor must not memcpy words between mismatched formats.
        let g_bgr = Geometry::packed(2, 1, PixelFormat::XBGR8888);
        let colour = Rgb::new(0x10, 0x20, 0x30);
        let mut px = vec![0u8; g_bgr.byte_len()];
        for x in 0..2u32 {
            let off = g_bgr.offset_of(x, 0).unwrap();
            px[off..off + 4].copy_from_slice(&g_bgr.format.encode(colour).to_le_bytes());
        }
        let mut fb = screen(); // XRGB
        compose_full(&mut fb, Rgb::BLACK, &[SurfaceRef::new(g_bgr, Point::new(0, 0), &px)]);
        assert_eq!(fb.get_pixel(0, 0), Some(colour), "channels must survive translation");
    }

    #[test]
    fn a_damage_rect_outside_the_screen_changes_nothing() {
        let (g, px) = patterned(4, 4, 16, 0x11);
        let mut fb = screen();
        let before = fb.clone();
        compose(
            &mut fb,
            Rgb::new(1, 2, 3),
            &[SurfaceRef::new(g, Point::new(0, 0), &px)],
            &[Rect::new(200, 200, 8, 8)],
        );
        assert_eq!(fb, before);
    }
}
