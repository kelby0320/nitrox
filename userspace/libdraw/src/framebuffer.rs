//! The [`Framebuffer`] seam, and the two implementations behind it.
//!
//! This is the trait `docs/design/display-substrate.md` §8a names: "base, width,
//! height, pitch, format — with a real implementation over Limine's mapping and an
//! **in-memory one for tests**". It is what turns "composite these surfaces with this
//! damage" into a pure function, assertable pixel-exactly in milliseconds instead of
//! through a boot, exactly as `BlockReader` did for the ext4 parser and `Host` for the
//! shell's evaluator.

use alloc::vec;
use alloc::vec::Vec;

use crate::format::{PixelFormat, Rgb};
use crate::geom::{Rect, Size};

/// Everything about a pixel buffer's shape that cannot be inferred from its bytes.
///
/// `pitch` is bytes per **row**, not `width × bytes_per_pixel`: firmware routinely
/// pads rows for alignment, and assuming otherwise writes every row after the first
/// at a slightly wrong offset — a skew that a solid-colour test cannot detect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Geometry {
    /// Visible width in pixels.
    pub width: u32,
    /// Visible height in pixels.
    pub height: u32,
    /// Bytes per row, which is `>= width * format.bytes_per_pixel()`.
    pub pitch: usize,
    /// Channel layout.
    pub format: PixelFormat,
}

impl Geometry {
    /// Geometry with the tightest legal pitch for `width`.
    ///
    /// # Panics
    ///
    /// If `format` is not 32 bits per pixel. See [`Geometry::with_pitch`] for why that is
    /// checked here rather than at the write sites.
    pub fn packed(width: u32, height: u32, format: PixelFormat) -> Self {
        Self::with_pitch(width, height, width as usize * format.bytes_per_pixel(), format)
            .expect("a packed pitch always holds a row; only the depth can fail")
    }

    /// Geometry with an explicit row stride.
    ///
    /// Returns `None` if `pitch` cannot hold a row (which would alias rows onto each
    /// other), or if `format` is not 32 bits per pixel.
    ///
    /// **The depth check belongs here**, at the one point every framebuffer passes
    /// through, rather than at each write. `PixelFormat`'s fields are public, so a 16-bpp
    /// format is constructible; [`Framebuffer::put_pixel`] and
    /// [`Framebuffer::fill_rect`] write a 4-byte word while striding by
    /// `bytes_per_pixel()`, so such a format would index past the buffer's end and panic
    /// on the last pixel of a row. Refusing the geometry makes that unreachable instead of
    /// latent. (`from_limine` refuses non-32-bpp too, but nothing forces a `Geometry` to
    /// come from firmware.)
    pub const fn with_pitch(
        width: u32,
        height: u32,
        pitch: usize,
        format: PixelFormat,
    ) -> Option<Self> {
        if format.bits_per_pixel != 32 {
            return None;
        }
        if pitch < width as usize * format.bytes_per_pixel() {
            return None;
        }
        Some(Self { width, height, pitch, format })
    }

    /// The visible area as a rectangle at the origin.
    pub const fn bounds(&self) -> Rect {
        Rect::from_size(Size::new(self.width, self.height))
    }

    /// Bytes needed to store the buffer, including row padding.
    ///
    /// The last row needs only its visible span, but buffers are sized to whole rows
    /// because that is what every producer of one actually allocates.
    pub const fn byte_len(&self) -> usize {
        self.pitch * self.height as usize
    }

    /// Byte offset of pixel `(x, y)`, or `None` if it lies outside the visible area.
    pub const fn offset_of(&self, x: u32, y: u32) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        Some(y as usize * self.pitch + x as usize * self.format.bytes_per_pixel())
    }
}

/// A writable pixel buffer of known geometry.
///
/// Implementors supply geometry and byte access; every drawing operation is a
/// provided method, so the real framebuffer and the test one share one
/// implementation of the logic rather than two that can drift.
pub trait Framebuffer {
    /// The buffer's shape.
    fn geometry(&self) -> Geometry;

    /// The backing bytes, at least [`Geometry::byte_len`] long.
    fn bytes(&self) -> &[u8];

    /// The backing bytes, mutably.
    fn bytes_mut(&mut self) -> &mut [u8];

    /// Read a single pixel, or `None` outside the visible area.
    fn get_pixel(&self, x: u32, y: u32) -> Option<Rgb> {
        let g = self.geometry();
        let off = g.offset_of(x, y)?;
        let b = self.bytes();
        let word = u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]]);
        Some(g.format.decode(word))
    }

    /// Write a single pixel. Writes outside the visible area are dropped.
    fn put_pixel(&mut self, x: u32, y: u32, colour: Rgb) {
        let g = self.geometry();
        let Some(off) = g.offset_of(x, y) else { return };
        let word = g.format.encode(colour).to_le_bytes();
        self.bytes_mut()[off..off + 4].copy_from_slice(&word);
    }

    /// Composite `colour` over what is already at `(x, y)` at `coverage`.
    ///
    /// A read-modify-write, which is why it is a method here rather than arithmetic at the
    /// call site: the read must go through [`get_pixel`](Self::get_pixel) so a buffer whose
    /// format decodes differently from the writer's assumption cannot silently blend against
    /// the wrong colour.
    ///
    /// **The endpoints short-circuit, and that is purely an optimisation** — it skips a read,
    /// which on a mapped framebuffer aperture is uncached memory. An earlier version of this
    /// comment claimed it was also needed for correctness on formats with fewer than 8 bits
    /// per channel, which is false: [`blend`](Rgb::blend) is the identity at both endpoints,
    /// and a narrow channel does round-trip `decode` → `encode` (`decode` replicates the high
    /// bits and `encode` truncates them back). Verified over all 65 536 words of a 5-6-5
    /// format before the claim was removed rather than reasoned about a second time.
    fn blend_pixel(&mut self, x: u32, y: u32, colour: Rgb, coverage: u8) {
        match coverage {
            0 => {}
            255 => self.put_pixel(x, y, colour),
            a => {
                let Some(under) = self.get_pixel(x, y) else { return };
                self.put_pixel(x, y, colour.blend(under, a));
            }
        }
    }

    /// Fill `rect`, clipped to the visible area, with a solid colour.
    ///
    /// The row's starting offset comes from [`Geometry::offset_of`] rather than being
    /// recomputed here. That is deliberate: an earlier version open-coded
    /// `y * pitch + x * bpp`, and breaking `offset_of` to check the tests were not
    /// vacuous left this path silently correct — two copies of the same arithmetic,
    /// only one of them covered.
    fn fill_rect(&mut self, rect: Rect, colour: Rgb) {
        // **One loop, not two.** `encode(c)` *is* `encode_alpha(c, 255)`, so this and
        // [`fill_rect_alpha`](Self::fill_rect_alpha) were byte-identical arithmetic in two
        // places — the shape this file's own history warns about, where one copy was silently
        // correct while the other was broken (PR #276 review, optional 7).
        self.fill_rect_alpha(rect, colour, 255);
    }

    /// Fill `rect`, clipped to the visible area, with `colour` at `alpha`.
    ///
    /// **A stored opacity, not a blend.** [`blend_pixel`](Self::blend_pixel) mixes a colour *into*
    /// what is already there and leaves an opaque pixel; this writes a pixel that is still
    /// translucent afterwards, for something further down the line to composite. The distinction
    /// only exists for a buffer whose format has an alpha channel — for any other, `alpha` is
    /// discarded and this is [`fill_rect`](Self::fill_rect), which is the right answer rather than
    /// a refusal: a surface with no channel has no way to be anything but opaque.
    ///
    /// Written for the overview, which fills a translucent ground and then draws opaque content
    /// over it (M13 Part C).
    fn fill_rect_alpha(&mut self, rect: Rect, colour: Rgb, alpha: u8) {
        let g = self.geometry();
        let Some(clipped) = rect.intersect(&g.bounds()) else { return };
        let word = g.format.encode_alpha(colour, alpha).to_le_bytes();
        let bpp = g.format.bytes_per_pixel();
        let bytes = self.bytes_mut();
        for row in 0..clipped.size.h {
            let y = clipped.origin.y as u32 + row;
            let Some(start) = g.offset_of(clipped.origin.x as u32, y) else { continue };
            for col in 0..clipped.size.w as usize {
                let off = start + col * bpp;
                bytes[off..off + 4].copy_from_slice(&word);
            }
        }
    }

    /// Fill `rect` with a vertical gradient — `mid` lightened at the top, darkened at the
    /// bottom by `bevel` — painting only inside `clip`.
    ///
    /// **Two rectangles rather than one, and that is the whole subtlety.** The gradient's stops
    /// are fixed by `rect`, which is the shape being drawn; `clip` is how much of it this frame
    /// repaints. Computing the ramp from the clip instead would make a partial repaint draw a
    /// *different* picture from a full one — the class of bug that only shows when a damage
    /// rectangle happens to be small, which is to say the one that reaches a screenshot last.
    ///
    /// A one-pixel-high rect is `mid` exactly: there is no top and bottom to span.
    fn fill_rect_bevel(&mut self, rect: Rect, clip: Rect, mid: Rgb, bevel: u8) {
        let Some(paint) = rect.intersect(&clip) else { return };
        if rect.size.h <= 1 || bevel == 0 {
            self.fill_rect(paint, mid);
            return;
        }
        let span = (rect.size.h - 1) as i32;
        let top = bevel as i32;
        for row in 0..paint.size.h {
            let y = paint.origin.y + row as i32;
            // Linear from +bevel at the rect's top row to -bevel at its last, rounded to
            // nearest so the two halves are symmetric about the middle.
            let t = y - rect.origin.y;
            let delta = top - (2 * top * t + span / 2) / span;
            let line = Rect::new(paint.origin.x, y, paint.size.w, 1);
            self.fill_rect(line, mid.shade(delta as i16));
        }
    }

    /// Fill the entire visible area.
    fn clear(&mut self, colour: Rgb) {
        let bounds = self.geometry().bounds();
        self.fill_rect(bounds, colour);
    }
}

/// A framebuffer backed by an owned `Vec`, for host tests.
///
/// The point of the whole trait: this is the implementation the compositing tests
/// run against, so a wrong pixel is a failing assertion in milliseconds rather than a
/// screenshot someone squints at.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MemFramebuffer {
    geometry: Geometry,
    bytes: Vec<u8>,
}

impl MemFramebuffer {
    /// A zeroed buffer of the given geometry.
    pub fn new(geometry: Geometry) -> Self {
        Self { geometry, bytes: vec![0u8; geometry.byte_len()] }
    }

    /// A zeroed buffer of the given geometry, or `None` if it will not fit.
    ///
    /// **`new` is right for a test and wrong for a screen.** `vec![0; n]` aborts the process when
    /// the allocation fails, which is tolerable for a 64x48 fixture and is not for a full display
    /// — a 1280x800 shadow buffer is 4 MB, asked for on a machine with 256 MB, and a compositor
    /// that dies there takes the session with it. The caller that can carry on without the buffer
    /// should be able to find out that it has to.
    pub fn try_new(geometry: Geometry) -> Option<Self> {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(geometry.byte_len()).ok()?;
        bytes.resize(geometry.byte_len(), 0);
        Some(Self { geometry, bytes })
    }

    /// A buffer of the given geometry, filled with `colour`.
    pub fn filled(geometry: Geometry, colour: Rgb) -> Self {
        let mut fb = Self::new(geometry);
        fb.clear(colour);
        fb
    }

    /// Consume the framebuffer, yielding its bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl Framebuffer for MemFramebuffer {
    fn geometry(&self) -> Geometry {
        self.geometry
    }
    fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
}

/// A framebuffer over memory this crate does not own — a mapped device aperture.
///
/// This is the shape the compositor uses once the kernel hands the framebuffer to
/// userspace (plan M1 Part B). It is kept here, and host-tested over an ordinary
/// allocation, so that the raw-pointer path is exercised before any kernel work
/// exists to hide a mistake in it.
pub struct RawFramebuffer {
    geometry: Geometry,
    base: *mut u8,
    len: usize,
}

impl RawFramebuffer {
    /// Wrap a mapped framebuffer.
    ///
    /// # Safety
    ///
    /// `base` must point to at least `geometry.byte_len()` bytes that stay mapped,
    /// writable, and un-aliased for the lifetime of the returned value. The caller is
    /// the process holding the framebuffer binding; nothing else may write the same
    /// mapping concurrently, since compositing reads back what it wrote.
    pub const unsafe fn new(geometry: Geometry, base: *mut u8) -> Self {
        Self { geometry, base, len: geometry.byte_len() }
    }
}

impl Framebuffer for RawFramebuffer {
    fn geometry(&self) -> Geometry {
        self.geometry
    }
    fn bytes(&self) -> &[u8] {
        // SAFETY: `new` requires `base` to address `len` readable bytes that remain
        // mapped for this value's lifetime, and `&self` borrows it for no longer.
        unsafe { core::slice::from_raw_parts(self.base, self.len) }
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, plus `&mut self` is the exclusive borrow the mutable
        // slice needs. `new`'s contract forbids any other writer to this mapping.
        unsafe { core::slice::from_raw_parts_mut(self.base, self.len) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geom() -> Geometry {
        Geometry::packed(8, 4, PixelFormat::XRGB8888)
    }

    #[test]
    fn a_new_buffer_is_zeroed_and_sized_to_whole_rows() {
        let fb = MemFramebuffer::new(Geometry::with_pitch(8, 4, 40, PixelFormat::XRGB8888).unwrap());
        assert_eq!(fb.bytes().len(), 160);
        assert!(fb.bytes().iter().all(|&b| b == 0));
    }

    #[test]
    fn with_pitch_refuses_a_stride_too_narrow_for_a_row() {
        // 8 px × 4 bytes needs 32; 31 would alias row n+1 over row n.
        assert!(Geometry::with_pitch(8, 4, 31, PixelFormat::XRGB8888).is_none());
        assert!(Geometry::with_pitch(8, 4, 32, PixelFormat::XRGB8888).is_some());
    }

    #[test]
    fn a_geometry_refuses_a_depth_the_write_paths_cannot_handle() {
        // `put_pixel`/`fill_rect` write 4 bytes while striding by `bytes_per_pixel()`, so
        // a 16-bpp geometry would index past the end of the last pixel in a row. Refused
        // at construction rather than left to panic later.
        let rgb565 = PixelFormat {
            bits_per_pixel: 16,
            red: crate::format::Channel::new(11, 5),
            green: crate::format::Channel::new(5, 6),
            blue: crate::format::Channel::new(0, 5),
            alpha: None,
        };
        assert!(Geometry::with_pitch(4, 2, 16, rgb565).is_none());
        assert!(Geometry::with_pitch(4, 2, 16, PixelFormat::XRGB8888).is_some());
    }

    #[test]
    fn pixels_round_trip_through_put_and_get() {
        let mut fb = MemFramebuffer::new(geom());
        let c = Rgb::new(0x11, 0x22, 0x33);
        fb.put_pixel(3, 2, c);
        assert_eq!(fb.get_pixel(3, 2), Some(c));
        assert_eq!(fb.get_pixel(0, 0), Some(Rgb::BLACK));
    }

    #[test]
    fn out_of_bounds_access_is_dropped_rather_than_panicking() {
        let mut fb = MemFramebuffer::new(geom());
        fb.put_pixel(8, 0, Rgb::new(1, 2, 3));
        fb.put_pixel(0, 4, Rgb::new(1, 2, 3));
        assert_eq!(fb.get_pixel(8, 0), None);
        assert_eq!(fb.get_pixel(0, 4), None);
        assert!(fb.bytes().iter().all(|&b| b == 0), "nothing should have been written");
    }

    #[test]
    fn padding_bytes_are_never_written_by_drawing() {
        // A row-padded buffer: the pad must stay zero, because the gate hashes only
        // visible pixels and a writer that runs into the pad has the wrong stride.
        let g = Geometry::with_pitch(4, 2, 24, PixelFormat::XRGB8888).unwrap();
        let mut fb = MemFramebuffer::new(g);
        fb.clear(Rgb::new(0xFF, 0xFF, 0xFF));
        for row in 0..2usize {
            let pad = &fb.bytes()[row * 24 + 16..row * 24 + 24];
            assert!(pad.iter().all(|&b| b == 0), "row {row} padding was written");
        }
    }

    #[test]
    fn a_bevel_ramps_from_lighter_to_darker_and_a_partial_repaint_agrees() {
        let mut fb = MemFramebuffer::new(Geometry::packed(4, 9, PixelFormat::XRGB8888));
        let r = Rect::new(0, 0, 4, 9);
        let mid = Rgb::new(0x80, 0x80, 0x80);
        fb.fill_rect_bevel(r, r, mid, 8);
        assert_eq!(fb.get_pixel(0, 0), Some(mid.shade(8)), "the top row is the light end");
        assert_eq!(fb.get_pixel(0, 4), Some(mid), "the middle row is the colour itself");
        assert_eq!(fb.get_pixel(0, 8), Some(mid.shade(-8)), "the last row is the dark end");

        // **A partial repaint must draw the same picture.** The ramp is fixed by the shape, not
        // by how much of it this frame touches — get that wrong and a small damage rectangle
        // silently paints a different gradient from a full one.
        let mut part = MemFramebuffer::new(Geometry::packed(4, 9, PixelFormat::XRGB8888));
        for band in 0..9 {
            part.fill_rect_bevel(r, Rect::new(0, band, 4, 1), mid, 8);
        }
        assert_eq!(part.into_bytes(), fb.into_bytes(), "nine one-row repaints equal one full one");
    }

    #[test]
    fn a_bevel_of_zero_and_a_one_row_bevel_are_flat_fills() {
        // Both are real cases: a theme may set the bevel to nothing, and a scrollbar thumb at
        // its smallest is one row. Dividing by `h - 1` would panic on the second.
        for (h, bevel) in [(9u32, 0u8), (1, 12)] {
            let mut fb = MemFramebuffer::new(Geometry::packed(2, h, PixelFormat::XRGB8888));
            let r = Rect::new(0, 0, 2, h);
            let mid = Rgb::new(0x40, 0x50, 0x60);
            fb.fill_rect_bevel(r, r, mid, bevel);
            for y in 0..h {
                assert_eq!(fb.get_pixel(0, y), Some(mid), "h={h} bevel={bevel} row {y}");
            }
        }
    }

    #[test]
    fn a_bevel_clamps_rather_than_wrapping_at_both_ends() {
        // Near-white lightened and near-black darkened both saturate; wrapping would put a dark
        // band across the top of a light title bar.
        assert_eq!(Rgb::new(0xFA, 0xFA, 0xFA).shade(12), Rgb::new(0xFF, 0xFF, 0xFF));
        assert_eq!(Rgb::new(4, 4, 4).shade(-12), Rgb::BLACK);
    }

    #[test]
    fn fill_rect_clips_at_every_edge() {
        let mut fb = MemFramebuffer::new(geom());
        let c = Rgb::new(9, 9, 9);
        // Straddles the left and top edges.
        fb.fill_rect(Rect::new(-2, -1, 4, 3), c);
        assert_eq!(fb.get_pixel(0, 0), Some(c));
        assert_eq!(fb.get_pixel(1, 1), Some(c));
        assert_eq!(fb.get_pixel(2, 0), Some(Rgb::BLACK));
        // Entirely off-screen writes nothing.
        let before = fb.clone();
        fb.fill_rect(Rect::new(100, 100, 4, 4), Rgb::new(7, 7, 7));
        assert_eq!(fb, before);
    }

    #[test]
    fn a_raw_framebuffer_over_an_allocation_matches_the_in_memory_one() {
        // Exercises the raw-pointer path before any kernel plumbing exists to hide a
        // mistake in it.
        let g = Geometry::with_pitch(6, 3, 32, PixelFormat::XRGB8888).unwrap();
        let mut backing = vec![0u8; g.byte_len()];
        // SAFETY: `backing` outlives `raw`, is exactly `byte_len()` bytes, is
        // writable, and is not aliased while `raw` holds it.
        let mut raw = unsafe { RawFramebuffer::new(g, backing.as_mut_ptr()) };
        let mut mem = MemFramebuffer::new(g);

        for fb in [&mut raw as &mut dyn Framebuffer, &mut mem as &mut dyn Framebuffer] {
            fb.clear(Rgb::new(0x20, 0x30, 0x40));
            fb.fill_rect(Rect::new(1, 1, 3, 1), Rgb::new(0xAA, 0xBB, 0xCC));
        }
        assert_eq!(raw.bytes(), mem.bytes());
    }

    #[test]
    fn a_translucent_fill_stores_the_opacity_it_was_given() {
        // **The single primitive the translucent overview rests on, and it had no test** (PR #276
        // review, finding 4). Replacing `encode_alpha(colour, alpha)` with `encode(colour)` here
        // makes every overview opaque again — half of M13 Part C reverted — and passed all 549
        // host tests, because `desktop-shell` is a bin with no unit tests and nothing between
        // them covered it.
        use crate::format::PixelFormat;
        let g = Geometry::with_pitch(8, 4, 40, PixelFormat::ARGB8888).unwrap();
        let mut fb = MemFramebuffer::new(g);
        let c = Rgb::new(0x20, 0x40, 0x60);
        fb.fill_rect_alpha(Rect::new(1, 1, 4, 2), c, 0x99);

        let word = |x: u32, y: u32| {
            let o = g.offset_of(x, y).unwrap();
            u32::from_le_bytes([fb.bytes()[o], fb.bytes()[o + 1], fb.bytes()[o + 2], fb.bytes()[o + 3]])
        };
        assert_eq!(g.format.alpha_of(word(1, 1)), 0x99, "the opacity was not stored");
        assert_eq!(g.format.decode(word(1, 1)), c, "…and the colour still is");
        assert_eq!(word(0, 1), 0, "outside the rectangle is untouched");

        // A format with no alpha channel discards it and this *is* `fill_rect` — the claim the
        // doc makes, and what lets `fill_rect` delegate here.
        let og = Geometry::with_pitch(8, 4, 40, PixelFormat::XRGB8888).unwrap();
        let mut a = MemFramebuffer::new(og);
        let mut b = MemFramebuffer::new(og);
        a.fill_rect_alpha(Rect::new(1, 1, 4, 2), c, 0x99);
        b.fill_rect(Rect::new(1, 1, 4, 2), c);
        assert_eq!(a.bytes(), b.bytes());
    }

    #[test]
    fn blend_pixel_covers_all_three_arms() {
        // **`blend_pixel` had no test in its own module** when it landed (PR #188 review): all
        // three arms were exercised only through `Font::draw_str`, and one of them not at all.
        let mut fb = MemFramebuffer::new(geom());
        let bg = Rgb::new(0x10, 0x20, 0x30);
        let ink = Rgb::new(0xF0, 0xE0, 0xD0);
        fb.clear(bg);

        // Zero coverage: the pixel is not touched.
        fb.blend_pixel(0, 0, ink, 0);
        assert_eq!(fb.get_pixel(0, 0), Some(bg), "zero coverage wrote something");

        // Full coverage: the pixel is the source, bit-exactly.
        fb.blend_pixel(1, 0, ink, 255);
        assert_eq!(fb.get_pixel(1, 0), Some(ink), "full coverage is not the source");

        // Partial: strictly between, per channel, and reads the destination that is there.
        fb.blend_pixel(2, 0, ink, 128);
        let mid = fb.get_pixel(2, 0).unwrap();
        for (m, (a, b)) in [(mid.r, (bg.r, ink.r)), (mid.g, (bg.g, ink.g)), (mid.b, (bg.b, ink.b))] {
            assert!(m > a && m < b, "{m} is not between {a} and {b}");
        }
    }

    #[test]
    fn blend_pixel_outside_the_buffer_writes_nothing() {
        // **The arm nothing covered**, because every `draw_str` test clips inside its buffer.
        // A `get_pixel` that returns `None` must end the operation — reordering this arm to
        // fall through to `put_pixel` would paint a glyph fringe at full intensity along a
        // surface edge, and no other test in the tree would object.
        let mut fb = MemFramebuffer::new(geom());
        fb.clear(Rgb::new(0x10, 0x20, 0x30));
        let before = fb.bytes().to_vec();
        let g = fb.geometry();
        for (x, y) in [(g.width, 0), (0, g.height), (g.width, g.height), (u32::MAX, 0)] {
            // Every coverage class, since each takes a different path to the bounds check.
            for a in [0u8, 1, 128, 254, 255] {
                fb.blend_pixel(x, y, Rgb::new(0xFF, 0xFF, 0xFF), a);
            }
        }
        assert_eq!(fb.bytes(), before.as_slice(), "an out-of-bounds blend wrote to the buffer");
    }

    #[test]
    fn a_narrow_channel_format_round_trips_at_the_endpoints() {
        // The claim the short-circuit's doc used to make, checked rather than repeated: a
        // format with fewer than 8 bits per channel survives `decode` → blend → `encode` at
        // both endpoints, so skipping the blend there is an optimisation and not a correction.
        //
        // 5-6-5 inside a 32-bit word, which `Geometry` accepts because the *word* is 32 bits.
        let f = PixelFormat {
            bits_per_pixel: 32,
            red: crate::format::Channel::new(11, 5),
            green: crate::format::Channel::new(5, 6),
            blue: crate::format::Channel::new(0, 5),
            alpha: None,
        };
        let src = Rgb::new(0x9C, 0x41, 0x7B);
        for w in 0..=0xFFFFu32 {
            let under = f.decode(w);
            assert_eq!(f.encode(src.blend(under, 0)), f.encode(under), "zero coverage at {w:#x}");
            assert_eq!(f.encode(src.blend(under, 255)), f.encode(src), "full coverage at {w:#x}");
        }
    }
}
