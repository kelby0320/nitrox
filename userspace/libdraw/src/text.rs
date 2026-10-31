//! Glyphs: real TrueType, rasterised at runtime.
//!
//! The font is a file on the root filesystem, loaded by whoever draws; the compositor never loads
//! one. `ab_glyph`'s 8-bit coverage goes straight to [`Rgb::blend`](crate::format::Rgb::blend) as
//! an argument, not a channel, so text draws the same into opaque and translucent surfaces. Why a
//! rasteriser: `docs/design/display-substrate.md` §6.

use alloc::vec::Vec;

#[cfg(feature = "io")]
use crate::theme::ThemePath;

use ab_glyph::{Font as _, FontVec, PxScale, ScaleFont as _};

use crate::format::Rgb;
use crate::framebuffer::Framebuffer;
use crate::geom::{Point, Rect, Size};

/// A loaded font — and, for a UI face, its bold companion if one was found.
pub struct Font {
    inner: FontVec,
    /// The same family's bold, which [`bold`](Self::bold) answers with. A companion rather than a
    /// second argument everywhere a font is passed: a window title is the only bold text.
    bold: Option<alloc::boxed::Box<Font>>,
    /// The theme's fixed-advance face, which [`mono`](Self::mono) answers with. Beside the bold:
    /// an editor's buffer is set in this one while its chrome stays proportional.
    mono: Option<alloc::boxed::Box<Font>>,
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
    /// Load a font from its file bytes, taking ownership of the bytes read at runtime.
    pub fn from_bytes(data: Vec<u8>) -> Option<Self> {
        FontVec::try_from_vec(data).ok().map(|inner| Self { inner, bold: None, mono: None })
    }

    /// This face with `bold` as its bold companion.
    pub fn with_bold(mut self, bold: Font) -> Self {
        self.bold = Some(alloc::boxed::Box::new(bold));
        self
    }

    /// This face with `mono` as its fixed-advance companion; the theme names it, see [`load_mono`].
    pub fn with_mono(mut self, mono: Font) -> Self {
        self.mono = Some(alloc::boxed::Box::new(mono));
        self
    }

    /// The face fixed-advance text is drawn in: the companion if there is one, this face if not.
    pub fn mono(&self) -> &Font {
        self.mono.as_deref().unwrap_or(self)
    }

    /// The regular face when there is none, rather than a faked weight: a synthetic overstrike
    /// smudged the counters of every `e`.
    pub fn bold(&self) -> &Font {
        self.bold.as_deref().unwrap_or(self)
    }

    /// Vertical metrics at `px`.
    pub fn v_metrics(&self, px: f32) -> VMetrics {
        let s = self.inner.as_scaled(PxScale::from(px));
        VMetrics { ascent: s.ascent(), descent: s.descent(), line_height: s.height() + s.line_gap() }
    }

    /// How far the pen advances across `s` at `px`, kerning between adjacent pairs included.
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
        // Rounded up: a box one pixel short clips the last column of the last glyph.
        libm::ceilf(w.max(0.0)) as u32
    }

    /// Whether the face has a glyph for `c`: a missing one draws `.notdef` silently, so a host test
    /// can pin a decorative character against the shipped face.
    pub fn has_glyph(&self, c: char) -> bool {
        self.inner.glyph_id(c).0 != 0
    }

    /// The advance of one character — a monospace font's cell width.
    pub fn advance(&self, c: char, px: f32) -> u32 {
        let scaled = self.inner.as_scaled(PxScale::from(px));
        libm::ceilf(scaled.h_advance(scaled.glyph_id(c)).max(0.0)) as u32
    }

    /// Draw `s` with its **left edge and baseline** at `at`, clipped to `clip`; returns the pen
    /// position after the last glyph.
    ///
    /// The baseline is the origin a font's metrics are expressed against; [`VMetrics::ascent`]
    /// converts from a box's top.
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
                    // Coverage is blended into what is there (no threshold, no fallback).
                    if coverage <= 0.0 {
                        return;
                    }
                    let x = ox + gx as f32;
                    let y = oy + gy as f32;
                    if x < 0.0 || y < 0.0 {
                        return;
                    }
                    let (x, y) = (x as u32, y as u32);
                    // Rounded, not truncated: truncating `255.0 * c` would leave a fully covered
                    // pixel at 254, and no glyph interior would be exactly its own colour.
                    let a = libm::roundf(coverage.clamp(0.0, 1.0) * 255.0) as u8;
                    // Clipped by the caller's rectangle too: a widget must stay in its slot.
                    if clip.contains(x as i32, y as i32) {
                        fb.blend_pixel(x, y, colour, a);
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

/// The desktop's own font, on the root filesystem rather than in the initramfs, since nothing that
/// draws text runs before the root is mounted (`initramfs-minimisation`).
pub const UI_FONT_PATH: &str = "/system/fonts/DejaVuSans.ttf";

/// Two constants rather than one `SYSTEM_FONT_PATH`, so a call site must choose a role.
pub const MONO_FONT_PATH: &str = "/system/fonts/DejaVuSansMono.ttf";

/// The bold of [`UI_FONT_PATH`], loaded beside it only when the theme uses that face; a theme
/// naming another UI face draws titles in that face's regular weight. Not a theme key.
pub const UI_BOLD_FONT_PATH: &str = "/system/fonts/DejaVuSans-Bold.ttf";

/// The largest font file [`load`] will read; the size comes from a file this process does not own.
pub const MAX_FONT_BYTES: usize = 8 * 1024 * 1024;

/// Why loading a font from the namespace failed.
#[cfg(feature = "io")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoadError {
    /// The path did not resolve — no font at that path in this namespace.
    NoBinding,
    /// The handle resolved but its metadata could not be read.
    Unstattable,
    /// The file is empty, or larger than [`MAX_FONT_BYTES`].
    ImpossibleSize(u64),
    /// The file resolved but could not be mapped.
    Unmappable,
    /// The bytes are not a font this rasteriser can parse.
    NotAFont,
}

#[cfg(feature = "io")]
impl LoadError {
    /// Why, in the words a console line wants; one phrasing for every caller.
    pub const fn why(self) -> &'static [u8] {
        match self {
            LoadError::NoBinding => b"did not resolve (is it staged into the rootfs?)",
            LoadError::Unstattable => b"stat failed",
            LoadError::ImpossibleSize(_) => b"is empty, or larger than the cap",
            LoadError::Unmappable => b"could not be mapped",
            LoadError::NotAFont => b"is not a parseable font",
        }
    }
}

/// Load a font from `path` in `root_ns`.
///
/// Authority is the namespace binding, as for [`acquire`](crate::acquire). It maps, copies and
/// unmaps rather than holding the mapping: [`Font`] owns its bytes, and `ab_glyph`'s parser keeps
/// references into them.
///
/// # Safety
///
/// `root_ns` must be a live namespace handle owned by the caller for the duration of the
/// call. It is borrowed, never closed.
#[cfg(feature = "io")]
pub unsafe fn load(root_ns: u64, path: &str) -> Result<Font, LoadError> {
    use libkern::abi::HandleInfo;
    use libkern::handle::{RawHandle, Rights};
    use libkern::{SYS_HANDLE_STAT, syscall2};
    use libos::{Handle, MapRead, Memory, Namespace, NsReadOnly, block_on};

    // SAFETY: the caller guarantees `root_ns` is live and owned; `borrow` yields a
    // non-owning view that never closes it.
    let ns = unsafe { Handle::<Namespace, NsReadOnly>::borrow(RawHandle(root_ns), Rights::LOOKUP) };
    // SAFETY: the path resolves to a read-mappable file object, or it does not resolve.
    let obj = block_on(unsafe { ns.lookup::<Memory, MapRead>(path, Rights::MAP_READ | Rights::INSPECT) })
        .map_err(|_| LoadError::NoBinding)?;

    // The exact byte length: mapping is page-granular and would pad the last page with zeros.
    let mut info = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
    // SAFETY: `&mut info` is a valid 24-byte `HandleInfo` out-param, and `obj` is live.
    let sr = unsafe { syscall2(SYS_HANDLE_STAT, obj.raw().0, (&mut info as *mut HandleInfo) as u64) };
    if sr < 0 {
        return Err(LoadError::Unstattable);
    }
    if info.size == 0 || info.size as usize > MAX_FONT_BYTES {
        return Err(LoadError::ImpossibleSize(info.size));
    }
    let size = info.size as usize;

    let mut data = Vec::new();
    // Reserve up front, before the map, so a refusal leaves no mapping behind: growing by doubling
    // would fault the demand-paged file in twice.
    data.try_reserve_exact(size).map_err(|_| LoadError::ImpossibleSize(info.size))?;
    let addr = obj.map(size).map_err(|_| LoadError::Unmappable)?;
    // SAFETY: `map` returned a mapping of at least `size` readable bytes.
    data.extend_from_slice(unsafe { core::slice::from_raw_parts(addr as *const u8, size) });
    // SAFETY: `addr` is the mapping made above, and the slice copied out of it is done with.
    let _ = unsafe { obj.unmap(addr, size) };

    Font::from_bytes(data).ok_or(LoadError::NotAFont)
}

/// Load the proportional face a theme names, falling back to the built-in one.
///
/// Returns the face and the path it actually came from, which may not be the theme's; a caller
/// reporting the font in use must report this one. `who` prefixes the console line a fallback
/// prints.
///
/// # Safety
///
/// As [`load`]: `root_ns` must be a live namespace handle owned by the caller.
#[cfg(feature = "io")]
pub unsafe fn load_ui(root_ns: u64, theme: &crate::theme::Theme, who: &[u8]) -> Result<(Font, ThemePath), LoadError> {
    let builtin = crate::theme::Theme::light().font_ui;
    // SAFETY: forwarded from this function's own contract.
    let (face, from) = unsafe { load_themed(root_ns, theme.font_ui, builtin, who) }?;
    // The built-in face gets its bold; a missing bold costs only the titles their weight.
    if from != builtin {
        return Ok((face, from));
    }
    // SAFETY: as above.
    match unsafe { load(root_ns, UI_BOLD_FONT_PATH) } {
        Ok(bold) => Ok((face.with_bold(bold), from)),
        Err(e) => {
            libkern::debug::Line::new()
                .s(who)
                .s(b": bold face ")
                .s(UI_BOLD_FONT_PATH.as_bytes())
                .s(b" ")
                .s(e.why())
                .s(b"; titles in the regular weight")
                .end();
            Ok((face, from))
        }
    }
}

/// Load the fixed-advance face a theme names, falling back to the built-in one; returns the path
/// it came from, as [`load_ui`] does.
///
/// # Safety
///
/// As [`load`]: `root_ns` must be a live namespace handle owned by the caller.
#[cfg(feature = "io")]
pub unsafe fn load_mono(root_ns: u64, theme: &crate::theme::Theme, who: &[u8]) -> Result<(Font, ThemePath), LoadError> {
    // SAFETY: forwarded from this function's own contract.
    unsafe { load_themed(root_ns, theme.font_mono, crate::theme::Theme::light().font_mono, who) }
}

/// `wanted`, or `builtin` if `wanted` will not load, with the path actually opened.
///
/// A theme naming a font this machine lacks must not cost the desktop its text. That cannot be
/// checked in the shell (`Theme::from_config`), which does not know what resolves in the
/// application's namespace, so the check is here. The fallback is said aloud: silence looks like
/// a theme that never arrived.
#[cfg(feature = "io")]
unsafe fn load_themed(
    root_ns: u64,
    wanted: ThemePath,
    builtin: ThemePath,
    who: &[u8],
) -> Result<(Font, ThemePath), LoadError> {
    // SAFETY: forwarded from the caller's contract.
    match unsafe { load(root_ns, wanted.as_str()) } {
        Ok(f) => Ok((f, wanted)),
        // The built-in path itself: nothing to fall back to, and a retry reaches the same answer.
        Err(e) if wanted == builtin => Err(e),
        Err(e) => {
            // `s` rather than `untrusted`: a `ThemePath` cannot hold a control byte.
            libkern::debug::Line::new()
                .s(who)
                .s(b": theme font ")
                .s(wanted.as_str().as_bytes())
                .s(b" ")
                .s(e.why())
                .s(b"; using ")
                .s(builtin.as_str().as_bytes())
                .end();
            // SAFETY: as above.
            unsafe { load(root_ns, builtin.as_str()) }.map(|f| (f, builtin))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::PixelFormat;
    use crate::framebuffer::{Geometry, MemFramebuffer};

    /// The shipped font, from the tree, so a test cannot pass against a different file.
    const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSansMono.ttf");

    fn font() -> Font {
        Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses")
    }

    #[test]
    fn bold_is_the_companion_or_the_face_itself() {
        const REGULAR: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSans.ttf");
        const BOLD: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSans-Bold.ttf");
        let plain = Font::from_bytes(REGULAR.to_vec()).unwrap();
        assert!(core::ptr::eq(plain.bold(), &plain), "no companion: bold text is the regular face");
        let bold = Font::from_bytes(BOLD.to_vec()).unwrap();
        let paired = Font::from_bytes(REGULAR.to_vec()).unwrap().with_bold(bold);
        assert!(!core::ptr::eq(paired.bold(), &paired));
        let heavy = paired.bold().text_width("Files", 12.0);
        assert!(heavy > paired.text_width("Files", 12.0), "and it is wider");
    }

    fn fb(w: u32, h: u32) -> MemFramebuffer {
        MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888))
    }

    /// Pixels the glyph touched at all, any coverage: a stroke thinner than a pixel (the `.notdef`
    /// box at 16px) never reaches full coverage. Buffers start zeroed, so "not black" is touched.
    fn inked(fb: &MemFramebuffer) -> usize {
        let g = fb.geometry();
        let mut n = 0;
        for y in 0..g.height {
            for x in 0..g.width {
                if fb.get_pixel(x, y) != Some(Rgb::BLACK) {
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
        let f = font();
        let w = f.advance('m', 16.0);
        for c in ['i', 'W', '.', '0', 'A'] {
            assert_eq!(f.advance(c, 16.0), w, "{c:?} advances differently");
        }
        assert!(w > 0, "a zero advance would stack every glyph on the first");
    }

    #[test]
    fn width_counts_characters_not_bytes() {
        let f = font();
        assert_eq!("é".len(), 2, "two bytes");
        assert_eq!(f.text_width("é", 16.0), f.text_width("m", 16.0), "one cell either way");
        assert_eq!(f.text_width("", 16.0), 0);
    }

    #[test]
    fn width_is_not_additive_because_it_rounds_once_per_string() {
        // At 16px DejaVu Sans Mono advances ~8.33px, so three is 25, not 3 × ceil(8.33) = 27: a
        // caller sizing a grid must multiply the advance, not one character's measured width.
        let f = font();
        let three = f.text_width("mmm", 16.0);
        assert!(three <= 3 * f.text_width("m", 16.0), "rounding is per string, not per glyph");
        assert!(three >= 3 * f.advance('m', 16.0) - 3, "but only by the rounding: {three}");
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

        assert!(inked(&b) > 0, "something was drawn");
        for y in 0..40u32 {
            for x in 0..10u32 {
                assert_eq!(b.get_pixel(x, y), Some(Rgb::new(0, 0, 0)), "ink left of the pen");
            }
        }
    }

    #[test]
    fn the_clip_rectangle_binds_even_where_the_buffer_would_accept_the_write() {
        let f = font();
        let ink = Rgb::new(0xFF, 0xFF, 0xFF);
        let v = f.v_metrics(16.0);
        let baseline = libm::ceilf(v.ascent) as i32;

        let mut wide = fb(200, 40);
        f.draw_str(&mut wide, Point::new(0, baseline), "MMMMMM", 16.0, ink, Rect::new(0, 0, 200, 40));
        let unclipped = inked(&wide);

        let mut narrow = fb(200, 40);
        f.draw_str(&mut narrow, Point::new(0, baseline), "MMMMMM", 16.0, ink, Rect::new(0, 0, 20, 40));
        let clipped = inked(&narrow);

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
        // A space has no outline; an unmapped codepoint gets `.notdef`.
        let f = font();
        let ink = Rgb::new(0xFF, 0xFF, 0xFF);
        let clip = Rect::new(0, 0, 60, 40);

        let mut blank = fb(60, 40);
        f.draw_str(&mut blank, Point::new(0, 20), "   ", 16.0, ink, clip);
        assert_eq!(inked(&blank), 0, "a space has no outline");

        let mut missing = fb(60, 40);
        f.draw_str(&mut missing, Point::new(0, 20), "\u{10FFFF}", 16.0, ink, clip);
        assert!(inked(&missing) > 0, "an unmapped codepoint shows .notdef");
    }

    #[test]
    fn glyph_edges_land_between_the_ink_and_the_background() {
        // Every other test counts pixels and would pass against thresholding.
        let f = font();
        let ink = Rgb::new(0xFF, 0xFF, 0xFF);
        let bg = Rgb::new(0x0E, 0x14, 0x1B);
        let mut b = fb(60, 40);
        b.clear(bg);
        f.draw_str(&mut b, Point::new(2, 30), "S", 32.0, ink, Rect::new(0, 0, 60, 40));

        let mut solid = 0usize;
        let mut partial = 0usize;
        for y in 0..40 {
            for x in 0..60 {
                match b.get_pixel(x, y) {
                    Some(p) if p == ink => solid += 1,
                    Some(p) if p != bg => partial += 1,
                    _ => {}
                }
            }
        }
        assert!(solid > 0, "no pixel reached full coverage — the glyph did not draw");
        assert!(partial > solid / 4, "only {partial} partial pixels against {solid} solid: this looks thresholded");
    }

    #[test]
    fn a_glyph_blends_against_what_is_already_there() {
        // The destination is read, not assumed. Only partially covered pixels are compared: whole
        // buffers differ by their backgrounds and would pass with thresholding reinstated.
        let f = font();
        let ink = Rgb::new(0xFF, 0xFF, 0xFF);
        let draw = |bg: Rgb| {
            let mut b = fb(60, 40);
            b.clear(bg);
            f.draw_str(&mut b, Point::new(2, 30), "S", 32.0, ink, Rect::new(0, 0, 60, 40));
            b
        };
        let (bg_a, bg_b) = (Rgb::new(0x0E, 0x14, 0x1B), Rgb::new(0x80, 0x20, 0x60));
        let (a, b) = (draw(bg_a), draw(bg_b));

        let mut compared = 0usize;
        for y in 0..40 {
            for x in 0..60 {
                let (pa, pb) = (a.get_pixel(x, y).unwrap(), b.get_pixel(x, y).unwrap());
                if pa == ink || pa == bg_a {
                    continue;
                }
                compared += 1;
                assert_ne!(
                    pa, pb,
                    "({x},{y}) is partially covered and identical over both backgrounds — \
                     the blend ignored the destination"
                );
            }
        }
        assert!(compared > 0, "no partially-covered pixel to compare");
    }

    #[test]
    fn garbage_is_refused_rather_than_parsed() {
        assert!(Font::from_bytes(alloc::vec![0u8; 64]).is_none());
        assert!(Font::from_bytes(Vec::new()).is_none());
    }
}
