//! Glyphs: real TrueType, rasterised at runtime.
//!
//! The font is a **file on the root filesystem**, loaded by whoever needs to draw — not
//! baked into a binary and not in the initramfs, which carries only what is needed to reach
//! a mounted root. The compositor never loads one: clients render text into their own
//! buffers and the compositor only composites.
//!
//! ## Coverage is blended, since 2026-08-12
//!
//! `ab_glyph` hands back 8-bit coverage per pixel — antialiasing — and until M5 Part A this
//! crate had no way to mix two colours, so coverage was thresholded to one bit. It is not any
//! more: [`Rgb::blend`](crate::format::Rgb::blend) composites a covered source over what is
//! already in the buffer, and [`Font::draw_str`] passes the rasteriser's coverage straight
//! through to it.
//!
//! **Glyph coverage is still an argument rather than a channel**, and that is the point of the
//! shape — M13 Part B added [`PixelFormat::ARGB8888`](crate::format::PixelFormat::ARGB8888) for
//! surfaces that want to be translucent, and it changed nothing here. A rasterised glyph computes
//! its coverage per pixel and blends immediately; storing it would mean carrying a channel from
//! the rasteriser to the compositor to answer a question already answered. Text draws the same
//! into an opaque surface and a translucent one; coverage is an argument to a blend, not a
//! fourth byte in a pixel. What made the threshold necessary was the absence of the operation,
//! not the absence of a channel.
//!
//! ## Why a rasteriser rather than a bitmap font
//!
//! A bitmap path — a PSF loader, or an embedded glyph table — is code that gets deleted when
//! real fonts arrive rather than grown into them. `docs/design/display-substrate.md` §6 has
//! the full argument and the experiment that settled it; the short version is that
//! `ab_glyph` builds for `x86_64-unknown-nitrox` and the interim compromise moved from the
//! font to the blend.

use alloc::vec::Vec;

#[cfg(feature = "io")]
use crate::theme::ThemePath;

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
                    // **Blended, since 2026-08-12.** `ab_glyph` hands back 8-bit coverage and
                    // this used to threshold it at 0.5, because `libdraw` had no way to mix
                    // two colours. `Rgb::blend` is that way, and the coverage now reaches the
                    // pixel it was always about.
                    //
                    // The `<= 0.5` branch is **gone rather than kept as a fallback**, which is
                    // a correction to the plan and to `display-substrate.md` §6. A fallback is
                    // for a caller that cannot take the good path; thresholding is not that,
                    // it is only worse output. The one caller that could genuinely want it is
                    // a surface that cannot be read back — and such a surface cannot blend
                    // *anything*, so that is a `Framebuffer` capability question rather than a
                    // second glyph loop kept alive against a case nothing has.
                    if coverage <= 0.0 {
                        return;
                    }
                    let x = ox + gx as f32;
                    let y = oy + gy as f32;
                    if x < 0.0 || y < 0.0 {
                        return;
                    }
                    let (x, y) = (x as u32, y as u32);
                    // Rounded, not truncated: coverage is `0.0..=1.0`, and `as u8` on `255.0 *
                    // c` truncates, so a fully-covered pixel would come out 254 and no glyph
                    // interior would ever be exactly its own colour.
                    let a = libm::roundf(coverage.clamp(0.0, 1.0) * 255.0) as u8;
                    // Clipped by the caller's rectangle as well as the framebuffer's own
                    // bounds: a widget must not draw outside its slot even when the buffer
                    // would accept the write.
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

/// Where the proportional face is bound on the root filesystem — the desktop's own font.
///
/// **On the root filesystem, not in the initramfs.** The boot image carries only what cannot
/// come from a filesystem — see `docs/rationale/deferred-decisions.md`
/// (`initramfs-minimisation`) — and nothing that draws text runs before the root is mounted.
/// A 343 KiB font would have been larger than every program in the boot image put together,
/// and this one is larger still.
pub const UI_FONT_PATH: &str = "/system/fonts/DejaVuSans.ttf";

/// Where the fixed-advance face is bound — the terminal's.
///
/// **Two constants rather than one `SYSTEM_FONT_PATH`, which is the whole of M11 Part D at the
/// bottom of the stack.** There was one path and every client loaded it, so every label in every
/// window was monospaced. Deleting the old name rather than keeping it as an alias is the point:
/// a call site that did not choose a role no longer compiles.
pub const MONO_FONT_PATH: &str = "/system/fonts/DejaVuSansMono.ttf";

/// The largest font file [`load`] will read.
///
/// A bound rather than trust: the size comes from `sys_handle_stat` on a file this process
/// does not own, and it is used to size an allocation.
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
    /// Why, in the words a console line wants.
    ///
    /// **One phrasing for every caller.** Five programs load fonts and each had spelled these
    /// out itself, so the same failure read differently depending on which one hit it.
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
/// The counterpart to [`acquire`](crate::acquire) for the other half of drawing: authority is
/// the namespace binding, so this either finds the file or does not, and there is no font
/// capability to ask for.
///
/// It maps, copies and unmaps rather than holding the mapping, because [`Font`] owns its
/// bytes — `ab_glyph`'s parser keeps references into them, and a mapping is a thing another
/// process could have unmapped underneath us.
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
    let obj = block_on(unsafe {
        ns.lookup::<Memory, MapRead>(path, Rights::MAP_READ | Rights::INSPECT)
    })
    .map_err(|_| LoadError::NoBinding)?;

    // The exact byte length. Mapping is page-granular, so without this the parser would be
    // handed however much zero padding the last page carries.
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

    let addr = obj.map(size).map_err(|_| LoadError::Unmappable)?;
    let mut data = Vec::new();
    // Reserve up front: growing a 343 KiB vector by doubling would map, copy and unmap the
    // whole thing several times over, which on a demand-paged file means faulting it in twice.
    data.try_reserve_exact(size).map_err(|_| LoadError::ImpossibleSize(info.size))?;
    // SAFETY: `map` returned a mapping of at least `size` readable bytes.
    data.extend_from_slice(unsafe { core::slice::from_raw_parts(addr as *const u8, size) });
    let _ = obj.unmap(addr as *mut u8, size);

    Font::from_bytes(data).ok_or(LoadError::NotAFont)
}

/// Load the proportional face a theme names, falling back to the built-in one.
///
/// Returns the face **and the path it actually came from**, which is not always the one the
/// theme asked for — see [`load_themed`]. A caller that reports which font it is using must
/// report this one, or it names a file it never opened.
///
/// `who` prefixes the console line a fallback prints — the calling program's name, as every
/// other line it writes is prefixed.
///
/// # Safety
///
/// As [`load`]: `root_ns` must be a live namespace handle owned by the caller.
#[cfg(feature = "io")]
pub unsafe fn load_ui(
    root_ns: u64,
    theme: &crate::theme::Theme,
    who: &[u8],
) -> Result<(Font, ThemePath), LoadError> {
    // SAFETY: forwarded from this function's own contract.
    unsafe { load_themed(root_ns, theme.font_ui, crate::theme::Theme::light().font_ui, who) }
}

/// Load the fixed-advance face a theme names, falling back to the built-in one.
///
/// Returns the face and the path it came from, as [`load_ui`] does.
///
/// # Safety
///
/// As [`load`]: `root_ns` must be a live namespace handle owned by the caller.
#[cfg(feature = "io")]
pub unsafe fn load_mono(
    root_ns: u64,
    theme: &crate::theme::Theme,
    who: &[u8],
) -> Result<(Font, ThemePath), LoadError> {
    // SAFETY: forwarded from this function's own contract.
    unsafe { load_themed(root_ns, theme.font_mono, crate::theme::Theme::light().font_mono, who) }
}

/// `wanted`, or `builtin` if `wanted` will not load — the rule written once for both roles, with
/// the path that was actually opened.
///
/// **A theme that names a font this machine does not have must not cost the desktop its text.**
/// That is the same stance the schema takes on every other field: a value that cannot be read
/// leaves that one thing at its default and the rest of the file still counts. It cannot be
/// enforced where the rest is, though — `Theme::from_config` runs in the shell, which has no way
/// to know whether a path resolves in the *application's* namespace, and the answer can differ
/// between two applications. So the check is here, at the only place that has the namespace.
///
/// The fallback is said out loud rather than swallowed: a desktop quietly ignoring the font it
/// was asked for looks exactly like a theme that never arrived.
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
        // The built-in path itself, so there is nothing to fall back *to*: hand back the error
        // rather than retrying a lookup, stat and map that can only reach the same answer
        // (PR #264 review, optional 1).
        Err(e) if wanted == builtin => Err(e),
        Err(e) => {
            // `s` rather than `untrusted`: a `ThemePath` cannot hold a control byte, which is
            // most of why it validates at all.
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

    /// The font this system ships, loaded from the tree rather than from the host — the
    /// image build uses the same file, so a test cannot pass against a different one.
    const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSansMono.ttf");

    fn font() -> Font {
        Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses")
    }

    fn fb(w: u32, h: u32) -> MemFramebuffer {
        MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888))
    }

    /// Pixels the glyph touched at all — any coverage, not only full.
    ///
    /// **The measure `lit` stopped being** when glyphs started blending (2026-08-12). A stroke
    /// thinner than a pixel now reaches *no* pixel at full coverage, so counting exact-colour
    /// matches answers "is any part of this glyph solid" rather than "was anything drawn". The
    /// `.notdef` box is exactly that case and it is what caught this: its outline is one pixel
    /// wide at 16px, it renders correctly, and the exact-match count is zero.
    ///
    /// Buffers here start zeroed, so "not black" is "touched".
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

        assert!(inked(&b) > 0, "something was drawn");
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
        assert_eq!(inked(&blank), 0, "a space has no outline");

        let mut missing = fb(60, 40);
        f.draw_str(&mut missing, Point::new(0, 20), "\u{10FFFF}", 16.0, ink, clip);
        assert!(inked(&missing) > 0, "an unmapped codepoint shows .notdef");
    }

    #[test]
    fn glyph_edges_land_between_the_ink_and_the_background() {
        // **The test that says antialiasing is happening.** Every other test here counts
        // pixels and would pass identically against the thresholded path this replaced —
        // including `something was drawn`, which is why the change needed its own assertion.
        //
        // A curved glyph on a non-black background, so a partially-covered pixel is a colour
        // that is neither and cannot be produced by any threshold.
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
        assert!(
            partial > solid / 4,
            "only {partial} partial pixels against {solid} solid: this looks thresholded"
        );
    }

    #[test]
    fn a_glyph_blends_against_what_is_already_there() {
        // The destination is *read*, not assumed: a blend hardcoded against black would still
        // produce partial coverage and pass the test above, while being wrong on any
        // background but black.
        //
        // **Compares only the partially-covered pixels**, which is the correction that made
        // this test mean anything. A first version compared whole buffers and passed with
        // thresholding reinstated — of course it did: the two backgrounds differ, so the
        // pixels the glyph never touched differ, and the assertion was satisfied by the
        // background rather than by the glyph.
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
                // Partially covered in the first render: neither the ink nor its background.
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
