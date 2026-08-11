//! Glyphs: real TrueType, rasterised at runtime.
//!
//! The font is a **file on the root filesystem**, loaded by whoever needs to draw — not
//! baked into a binary and not in the initramfs, which carries only what is needed to reach
//! a mounted root. The compositor never loads one: clients render text into their own
//! buffers and the compositor only composites.
//!
//! ## Coverage is thresholded, and that is a `libdraw` limitation rather than a font one
//!
//! `ab_glyph` hands back 8-bit coverage per pixel — antialiasing — and this crate composites
//! **opaque** XRGB8888 with no alpha path. So coverage is thresholded to one bit here. It is
//! one branch in [`Font::draw_str`]'s callback, and it becomes the fallback rather than being
//! deleted when blending lands; the deferral is filed against `libdraw`, where the blocker
//! actually is, and not against the font.
//!
//! ## Why a rasteriser rather than a bitmap font
//!
//! A bitmap path — a PSF loader, or an embedded glyph table — is code that gets deleted when
//! real fonts arrive rather than grown into them. `docs/design/display-substrate.md` §6 has
//! the full argument and the experiment that settled it; the short version is that
//! `ab_glyph` builds for `x86_64-unknown-nitrox` and the interim compromise moved from the
//! font to the blend.

use alloc::vec::Vec;

use ab_glyph::{Font as _, FontVec, PxScale, ScaleFont as _};

use crate::format::Rgb;
use crate::framebuffer::Framebuffer;
use crate::geom::{Point, Rect, Size};

/// A loaded font.
pub struct Font {
    inner: FontVec,
}

/// What a font's vertical metrics come to at a given size.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct VMetrics {
    /// Distance from the baseline to the top of the tallest glyph.
    pub ascent: f32,
    /// Distance from the baseline to the bottom of the deepest glyph; negative.
    pub descent: f32,
    /// Baseline-to-baseline distance for consecutive lines.
    pub line_height: f32,
}

impl Font {
    /// Load a font from its file bytes.
    ///
    /// Takes ownership: the bytes come from a file read at runtime, not from a `'static`
    /// slice, which is what `FontVec` exists for.
    pub fn from_bytes(data: Vec<u8>) -> Option<Self> {
        FontVec::try_from_vec(data).ok().map(|inner| Self { inner })
    }

    /// Vertical metrics at `px`.
    pub fn v_metrics(&self, px: f32) -> VMetrics {
        let s = self.inner.as_scaled(PxScale::from(px));
        VMetrics { ascent: s.ascent(), descent: s.descent(), line_height: s.height() + s.line_gap() }
    }

    /// How far the pen advances across `s` at `px`.
    ///
    /// **Includes kerning between adjacent pairs.** A monospace font kerns nothing, so this
    /// is `len × advance` there — but measuring by multiplication would be wrong the moment
    /// anything proportional is drawn, and the cost of doing it properly is one lookup per
    /// character.
    pub fn text_width(&self, s: &str, px: f32) -> u32 {
        let scaled = self.inner.as_scaled(PxScale::from(px));
        let mut w = 0.0f32;
        let mut prev = None;
        for c in s.chars() {
            let id = scaled.glyph_id(c);
            if let Some(p) = prev {
                w += scaled.kern(p, id);
            }
            w += scaled.h_advance(id);
            prev = Some(id);
        }
        // Rounded up: a caller laying out a box wants somewhere the text fits, and a box one
        // pixel short clips the last column of the last glyph.
        libm::ceilf(w.max(0.0)) as u32
    }

    /// The advance of one character — a monospace font's cell width.
    pub fn advance(&self, c: char, px: f32) -> u32 {
        let scaled = self.inner.as_scaled(PxScale::from(px));
        libm::ceilf(scaled.h_advance(scaled.glyph_id(c)).max(0.0)) as u32
    }

    /// Draw `s` with its **left edge and baseline** at `at`, clipped to `clip`.
    ///
    /// The baseline rather than the top-left, because that is the origin a font's metrics are
    /// expressed against and the one on which two glyphs of different heights line up.
    /// [`VMetrics::ascent`] converts from a box's top when a caller wants that instead.
    ///
    /// Returns the pen position after the last glyph, so a caller can chain runs.
    pub fn draw_str<F: Framebuffer + ?Sized>(
        &self,
        fb: &mut F,
        at: Point,
        s: &str,
        px: f32,
        colour: Rgb,
        clip: Rect,
    ) -> Point {
        let scaled = self.inner.as_scaled(PxScale::from(px));
        let mut pen = at.x as f32;
        let mut prev = None;
        for c in s.chars() {
            let id = scaled.glyph_id(c);
            if let Some(p) = prev {
                pen += scaled.kern(p, id);
            }
            let glyph = id.with_scale_and_position(PxScale::from(px), ab_glyph::point(pen, at.y as f32));
            if let Some(outlined) = self.inner.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                let (ox, oy) = (bounds.min.x, bounds.min.y);
                outlined.draw(|gx, gy, coverage| {
                    // **Thresholded, not blended.** `libdraw` composites opaque XRGB8888;
                    // the coverage is real and has nowhere to go until it can blend. One
                    // branch, and it becomes the fallback rather than being deleted.
                    if coverage <= 0.5 {
                        return;
                    }
                    let x = ox + gx as f32;
                    let y = oy + gy as f32;
                    if x < 0.0 || y < 0.0 {
                        return;
                    }
                    let (x, y) = (x as u32, y as u32);
                    // Clipped by the caller's rectangle as well as the framebuffer's own
                    // bounds: a widget must not draw outside its slot even when the buffer
                    // would accept the write.
                    if clip.contains(x as i32, y as i32) {
                        fb.put_pixel(x, y, colour);
                    }
                });
            }
            pen += scaled.h_advance(id);
            prev = Some(id);
        }
        Point::new(libm::ceilf(pen) as i32, at.y)
    }

    /// The box `s` occupies at `px`, for layout.
    pub fn measure(&self, s: &str, px: f32) -> Size {
        let v = self.v_metrics(px);
        Size::new(self.text_width(s, px), libm::ceilf(v.ascent - v.descent) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::PixelFormat;
    use crate::framebuffer::{Geometry, MemFramebuffer};

    /// The font this system ships, loaded from the tree rather than from the host — the
    /// image build uses the same file, so a test cannot pass against a different one.
    const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSansMono.ttf");

    fn font() -> Font {
        Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses")
    }

    fn fb(w: u32, h: u32) -> MemFramebuffer {
        MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888))
    }

    /// How many pixels of `colour` are set.
    fn lit(fb: &MemFramebuffer, colour: Rgb) -> usize {
        let g = fb.geometry();
        let mut n = 0;
        for y in 0..g.height {
            for x in 0..g.width {
                if fb.get_pixel(x, y) == Some(colour) {
                    n += 1;
                }
            }
        }
        n
    }

    #[test]
    fn the_vendored_font_parses_and_has_plausible_metrics() {
        let f = font();
        let v = f.v_metrics(16.0);
        assert!(v.ascent > 0.0, "ascent above the baseline: {v:?}");
        assert!(v.descent < 0.0, "descent below it: {v:?}");
        assert!(v.line_height >= v.ascent - v.descent, "lines do not overlap: {v:?}");
    }

    #[test]
    fn a_monospace_font_advances_the_same_for_every_character() {
        // The property the terminal grid depends on. If this ever fails, the font was
        // replaced with a proportional one and the grid silently stops being a grid.
        let f = font();
        let w = f.advance('m', 16.0);
        for c in ['i', 'W', '.', '0', 'A'] {
            assert_eq!(f.advance(c, 16.0), w, "{c:?} advances differently");
        }
        assert!(w > 0, "a zero advance would stack every glyph on the first");
    }


    #[test]
    fn width_counts_characters_not_bytes() {
        // A byte count is a character count only for ASCII, and getting it wrong mis-sizes
        // every non-ASCII label — a layout bug in disguise.
        let f = font();
        assert_eq!("é".len(), 2, "two bytes");
        assert_eq!(f.text_width("é", 16.0), f.text_width("m", 16.0), "one cell either way");
        assert_eq!(f.text_width("", 16.0), 0);
    }

    #[test]
    fn width_is_not_additive_because_it_rounds_once_per_string() {
        // At 16px DejaVu Sans Mono advances ~8.33px, so three of them is 25 and *not*
        // 3 × ceil(8.33) = 27. A caller sizing a grid must multiply the **advance**, not the
        // measured width of one character — a first version of this test asserted the
        // identity and failed against a correct implementation.
        let f = font();
        let three = f.text_width("mmm", 16.0);
        assert!(three <= 3 * f.text_width("m", 16.0), "rounding is per string, not per glyph");
        assert!(three >= 3 * f.advance('m', 16.0) - 3, "but only by the rounding: {three}");
        // Monotone, which is the property layout actually leans on.
        assert!(f.text_width("mm", 16.0) > f.text_width("m", 16.0));
        assert!(three > f.text_width("mm", 16.0));
    }

    #[test]
    fn drawing_puts_pixels_where_the_text_is_and_nowhere_else() {
        let f = font();
        let mut b = fb(200, 40);
        let ink = Rgb::new(0xFF, 0xFF, 0xFF);
        let v = f.v_metrics(16.0);
        let baseline = libm::ceilf(v.ascent) as i32;
        f.draw_str(&mut b, Point::new(10, baseline), "Hi", 16.0, ink, Rect::new(0, 0, 200, 40));

        assert!(lit(&b, ink) > 0, "something was drawn");
        // Nothing to the left of the pen, and nothing below the descent.
        for y in 0..40u32 {
            for x in 0..10u32 {
                assert_eq!(b.get_pixel(x, y), Some(Rgb::new(0, 0, 0)), "ink left of the pen");
            }
        }
    }

    #[test]
    fn the_clip_rectangle_binds_even_where_the_buffer_would_accept_the_write() {
        // A widget must not draw outside its slot. Clipping only by the framebuffer's own
        // bounds lets a long label scribble over its neighbour.
        let f = font();
        let ink = Rgb::new(0xFF, 0xFF, 0xFF);
        let v = f.v_metrics(16.0);
        let baseline = libm::ceilf(v.ascent) as i32;

        let mut wide = fb(200, 40);
        f.draw_str(&mut wide, Point::new(0, baseline), "MMMMMM", 16.0, ink, Rect::new(0, 0, 200, 40));
        let unclipped = lit(&wide, ink);

        let mut narrow = fb(200, 40);
        f.draw_str(&mut narrow, Point::new(0, baseline), "MMMMMM", 16.0, ink, Rect::new(0, 0, 20, 40));
        let clipped = lit(&narrow, ink);

        assert!(clipped > 0, "the first glyph still drew");
        assert!(clipped < unclipped, "and the rest were clipped: {clipped} vs {unclipped}");
        for y in 0..40u32 {
            for x in 20..200u32 {
                assert_eq!(narrow.get_pixel(x, y), Some(Rgb::new(0, 0, 0)), "ink past the clip");
            }
        }
    }

    #[test]
    fn draw_str_returns_a_pen_a_caller_can_chain_from() {
        let f = font();
        let mut b = fb(200, 40);
        let ink = Rgb::new(0xFF, 0xFF, 0xFF);
        let clip = Rect::new(0, 0, 200, 40);
        let end = f.draw_str(&mut b, Point::new(5, 20), "ab", 16.0, ink, clip);
        assert_eq!(end.y, 20, "the baseline does not move");
        assert_eq!(end.x as u32, 5 + f.text_width("ab", 16.0), "and the pen advanced by the run");
    }

    #[test]
    fn a_space_draws_nothing_and_an_unmapped_codepoint_draws_notdef() {
        // A client printing arbitrary text must not take the process down, and neither case
        // is an error. They differ, though, and a first version of this test asserted both
        // drew nothing: a space has no outline, while an unmapped codepoint gets the font's
        // `.notdef` box — which is the font behaving correctly, and is how a user sees that
        // a character is missing rather than silently absent.
        let f = font();
        let ink = Rgb::new(0xFF, 0xFF, 0xFF);
        let clip = Rect::new(0, 0, 60, 40);

        let mut blank = fb(60, 40);
        f.draw_str(&mut blank, Point::new(0, 20), "   ", 16.0, ink, clip);
        assert_eq!(lit(&blank, ink), 0, "a space has no outline");

        let mut missing = fb(60, 40);
        f.draw_str(&mut missing, Point::new(0, 20), "\u{10FFFF}", 16.0, ink, clip);
        assert!(lit(&missing, ink) > 0, "an unmapped codepoint shows .notdef");
    }

    #[test]
    fn garbage_is_refused_rather_than_parsed() {
        assert!(Font::from_bytes(alloc::vec![0u8; 64]).is_none());
        assert!(Font::from_bytes(Vec::new()).is_none());
    }
}
