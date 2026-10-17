//! Surfaces, and compositing as a pure function.
//!
//! `docs/design/display-substrate.md` §8a: compositing needs no screen, as the output bytes are
//! determined by the arguments (§7).

use crate::format::Rgb;
use crate::framebuffer::{Framebuffer, Geometry};
use crate::geom::{Point, Rect};

/// A client's pixel buffer, positioned on screen.
///
/// Borrowed: in the compositor the bytes are a mapped `MemoryObject` the client drew into (§4).
#[derive(Clone, Copy, Debug)]
pub struct SurfaceRef<'a> {
    /// The surface's own size, stride and format, which need not match the framebuffer's.
    pub geometry: Geometry,
    /// Where the surface's top-left corner sits in screen space. May be negative.
    pub origin: Point,
    /// The surface's pixels, at least `geometry.byte_len()` bytes.
    pub pixels: &'a [u8],
    /// The shadow cast around this surface, if any.
    ///
    /// A property of the surface so it can be drawn between surfaces without a parameter on
    /// [`compose`].
    pub shadow: Option<Shadow>,
    /// The radius its corners are rounded to, or zero for square ones.
    ///
    /// Pixels outside the curve are not written and pixels on it blend at its coverage; see
    /// `blit_clipped` and [`corner`](crate::corner). Clamped to half the shorter side.
    pub corner: u32,
}

impl<'a> SurfaceRef<'a> {
    /// A surface at `origin` over `pixels`, square-cornered and casting no shadow.
    pub const fn new(geometry: Geometry, origin: Point, pixels: &'a [u8]) -> Self {
        Self { geometry, origin, pixels, shadow: None, corner: 0 }
    }

    /// The same surface, casting `shadow`.
    pub const fn with_shadow(self, shadow: Shadow) -> Self {
        Self { shadow: Some(shadow), ..self }
    }

    /// The same surface, with its corners rounded to `radius`.
    pub const fn with_corner(self, radius: u32) -> Self {
        Self { corner: radius, ..self }
    }

    /// The corner radius actually drawn: [`corner`](Self::corner) clamped to what the size holds.
    pub const fn radius(&self) -> u32 {
        crate::corner::clamp(self.corner, self.geometry.width, self.geometry.height)
    }

    /// The surface's bounds in screen space.
    pub const fn bounds(&self) -> Rect {
        Rect::new(self.origin.x, self.origin.y, self.geometry.width, self.geometry.height)
    }

    /// Everything this surface paints, shadow included: what damage is computed from, unlike
    /// [`bounds`](Self::bounds), which a click hits.
    pub const fn painted_bounds(&self) -> Rect {
        match self.shadow {
            Some(sh) => sh.around(self.bounds()),
            None => self.bounds(),
        }
    }
}

/// A soft dark edge drawn around a surface, under it and over everything below it.
///
/// Drawn by the compositor so the shadow stays outside `bounds` and hit-testing ignores it. Two
/// layers: a wide soft one for depth (the design's `0 10px 28px` at 22%) and a tight one for
/// contact (`0 2px 6px` at 14%). A layer with zero strength or radius draws nothing, so
/// [`single`](Self::single) says a one-layer shadow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Shadow {
    /// The layers, drawn in order: the wide one first.
    pub layers: [ShadowLayer; 2],
}

impl Shadow {
    /// A shadow of one layer.
    pub const fn single(layer: ShadowLayer) -> Self {
        Self { layers: [layer, ShadowLayer::NONE] }
    }

    /// The rectangle this shadow can paint into around `bounds`: its layers' reach, unioned.
    pub const fn around(&self, bounds: Rect) -> Rect {
        let a = self.layers[0].around(bounds);
        let b = self.layers[1].around(bounds);
        let (l, t) = (min_i32(a.origin.x, b.origin.x), min_i32(a.origin.y, b.origin.y));
        let r_edge = max_i32(a.right() as i32, b.right() as i32);
        let b_edge = max_i32(a.bottom() as i32, b.bottom() as i32);
        Rect::new(l, t, (r_edge - l) as u32, (b_edge - t) as u32)
    }
}

/// One layer of a [`Shadow`]: a dark edge that falls off with distance from the surface.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ShadowLayer {
    /// How far the layer reaches beyond the surface, in pixels.
    pub radius: u32,
    /// How far the layer is displaced; positive `y` drops it downward.
    pub offset: Point,
    /// The layer's colour, blended at a coverage that falls off with distance.
    pub colour: Rgb,
    /// Coverage directly against the surface's edge, where the falloff begins.
    pub strength: u8,
}

impl ShadowLayer {
    /// A layer that draws nothing — the second layer of a [`Shadow::single`].
    pub const NONE: ShadowLayer = ShadowLayer { radius: 0, offset: Point::new(0, 0), colour: Rgb::BLACK, strength: 0 };

    /// Whether this layer paints anything at all.
    pub const fn is_visible(&self) -> bool {
        self.radius > 0 && self.strength > 0
    }

    /// The rectangle this layer can paint into around `bounds`, unioned with `bounds` so a large
    /// offset cannot start it inside the window and leave a band unrepainted after a move.
    pub const fn around(&self, bounds: Rect) -> Rect {
        if !self.is_visible() {
            return bounds;
        }
        let r = self.radius as i32;
        let cast = Rect::new(
            bounds.origin.x + self.offset.x - r,
            bounds.origin.y + self.offset.y - r,
            bounds.size.w + self.radius * 2,
            bounds.size.h + self.radius * 2,
        );
        let (l, t) = (min_i32(cast.origin.x, bounds.origin.x), min_i32(cast.origin.y, bounds.origin.y));
        let r_edge = max_i32(cast.right() as i32, bounds.right() as i32);
        let b_edge = max_i32(cast.bottom() as i32, bounds.bottom() as i32);
        Rect::new(l, t, (r_edge - l) as u32, (b_edge - t) as u32)
    }
}

/// `const` min, which `Ord::min` is not.
const fn min_i32(a: i32, b: i32) -> i32 {
    if a < b { a } else { b }
}

/// `const` max, which `Ord::max` is not.
const fn max_i32(a: i32, b: i32) -> i32 {
    if a > b { a } else { b }
}

/// Draw `shadow` around `bounds` on `fb`, painting only inside `clip`.
///
/// The falloff is `strength * (1 - d/r)²` for edge distance `d`, a faint tail; the cheaper
/// `(r² - d²) / r²` holds three quarters of its opacity half way out and reads as a dark band.
/// Integer arithmetic (`isqrt`), since `check-display` compares host and target pixel for pixel;
/// public because it applies the same shadow.
///
/// Pixels inside the surface are skipped, as it is blitted over them. A rounded surface casts a
/// rounded shadow: distance is measured to the rounded rectangle, and a pixel inside `bounds` is
/// skipped only where the curve covers it entirely, since outside the curve the blit leaves it
/// alone and on the curve it blends over the shadow.
pub fn draw_shadow<F: Framebuffer + ?Sized>(fb: &mut F, bounds: Rect, corner: u32, shadow: &Shadow, clip: &Rect) {
    for layer in &shadow.layers {
        draw_layer(fb, bounds, corner, layer, clip);
    }
}

/// One layer of [`draw_shadow`].
fn draw_layer<F: Framebuffer + ?Sized>(fb: &mut F, bounds: Rect, corner: u32, shadow: &ShadowLayer, clip: &Rect) {
    if !shadow.is_visible() {
        return;
    }
    let c = crate::corner::clamp(corner, bounds.size.w, bounds.size.h);
    // Distance to this, less `c`, is the distance to the rounded shape.
    let inner = Rect::new(
        bounds.origin.x + shadow.offset.x + c as i32,
        bounds.origin.y + shadow.offset.y + c as i32,
        bounds.size.w - 2 * c,
        bounds.size.h - 2 * c,
    );
    let Some(area) = shadow.around(bounds).intersect(clip) else { return };
    let r = shadow.radius as i64;
    let rr = r * r;
    // Empty where the surface is exactly `2c` across: `right() - 1` would precede the origin.
    let (ix0, ix1) = (inner.origin.x, (inner.right() as i32 - 1).max(inner.origin.x));
    let (iy0, iy1) = (inner.origin.y, (inner.bottom() as i32 - 1).max(inner.origin.y));
    let (left, right) = (bounds.origin.x, bounds.right() as i32);
    let (top, bottom) = (bounds.origin.y, bounds.bottom() as i32);
    for y in area.origin.y..area.bottom() as i32 {
        // The span the surface covers whole is skipped, not visited per pixel (the two-layer
        // shadow cost 2.0x the single one); it is where `covered` answers 255.
        let (skip_from, skip_to) = if y >= top && y < bottom {
            let shape = crate::corner::row(c, (y - top).min(bottom - 1 - y) as u32);
            let inset = (shape.clear + shape.partial) as i32;
            (left + inset, right - inset)
        } else {
            (0, 0)
        };
        let dy = (iy0 - y).max(y - iy1).max(0) as i64;
        let mut x = area.origin.x;
        while x < area.right() as i32 {
            if x >= skip_from && x < skip_to {
                x = skip_to;
                continue;
            }
            let dx = (ix0 - x).max(x - ix1).max(0) as i64;
            // `isqrt` of a perfect square is its root, so the root is taken only off the corners;
            // it floors, so a pixel is never darker than its true distance.
            let d = match (dx, dy) {
                (0, d) | (d, 0) => d,
                _ => ((dx * dx + dy * dy) as u64).isqrt() as i64,
            };
            let d = (d - c as i64).max(0);
            if d < r {
                let t = r - d;
                let coverage = (shadow.strength as i64 * t * t / rr) as u8;
                fb.blend_pixel(x as u32, y as u32, shadow.colour, coverage);
            }
            x += 1;
        }
    }
}

/// Composite `surfaces` onto `fb` within `damage`.
///
/// `surfaces` is bottom first. Each damage rectangle is repainted from scratch: `background`, then
/// every surface that intersects it; overlapping damage is painted twice. Deterministic (§7),
/// which the gate rests on: output depends only on the arguments.
pub fn compose<F: Framebuffer + ?Sized>(fb: &mut F, background: Rgb, surfaces: &[SurfaceRef<'_>], damage: &[Rect]) {
    let screen = fb.geometry().bounds();
    for area in damage {
        let Some(area) = area.intersect(&screen) else { continue };
        fb.fill_rect(area, background);
        for surface in surfaces {
            paint_surface(fb, surface, &area);
        }
    }
}

/// Composite `surfaces` over `background`, filling only the background nobody covers.
///
/// The same output as [`compose`] with less work: a window drag writes most pixels twice there.
/// A surface covers only if it is opaque ([`covers`]) and its buffer holds its geometry, else its
/// skipped pixels would show stale framebuffer contents. The fill is also the flicker's cause: a
/// frame that never paints background where a window is about to be leaves nothing to scan out.
pub fn compose_exposed<F: Framebuffer + ?Sized>(
    fb: &mut F,
    background: Rgb,
    surfaces: &[SurfaceRef<'_>],
    damage: &[Rect],
) {
    let screen = fb.geometry().bounds();
    for area in damage {
        let Some(area) = area.intersect(&screen) else { continue };
        // The damage rectangle with every covering surface cut out (up to four pieces per cut).
        let mut exposed: [Option<Rect>; MAX_EXPOSED] = [None; MAX_EXPOSED];
        exposed[0] = Some(area);
        for surface in surfaces {
            if !covers(surface) {
                continue;
            }
            // A rounded surface's corner squares still need background: the blit leaves outside
            // the curve unwritten.
            for cut in covered_by(surface).iter().flatten() {
                subtract_from(&mut exposed, cut);
            }
        }
        for piece in exposed.iter().flatten() {
            fb.fill_rect(*piece, background);
        }
        for surface in surfaces {
            paint_surface(fb, surface, &area);
        }
    }
}

/// One surface and the shadow it casts. The shadow goes down first, so it falls on what is below
/// and the surface covers the part under it; a separate shadow pass would put every shadow under
/// or over every window.
fn paint_surface<F: Framebuffer + ?Sized>(fb: &mut F, surface: &SurfaceRef<'_>, area: &Rect) {
    if let Some(shadow) = &surface.shadow {
        draw_shadow(fb, surface.bounds(), surface.radius(), shadow, area);
    }
    blit_clipped(fb, surface, area);
}

/// How many rectangles the exposed region may become. A bound, not a guarantee: [`subtract_from`]
/// abandons a subtraction that will not fit, which over-fills and is always correct, so lowering
/// this can only cost speed.
const MAX_EXPOSED: usize = 16;

/// Whether skipping the fill underneath `surface` is safe: its blit must write every pixel of the
/// intersection (a short buffer breaks this) without depending on what was there (alpha does).
fn covers(surface: &SurfaceRef<'_>) -> bool {
    !surface.geometry.format.has_alpha() && surface.pixels.len() >= surface.geometry.byte_len()
}

/// The rectangles a [`covers`] surface paints every pixel of: its bounds, or three bands when
/// rounded. Any subset is safe, as [`subtract_from`] refuses a cut that does not fit.
fn covered_by(surface: &SurfaceRef<'_>) -> [Option<Rect>; 3] {
    let b = surface.bounds();
    let r = surface.radius();
    if r == 0 {
        return [Some(b), None, None];
    }
    let (x, y, w, h) = (b.origin.x, b.origin.y, b.size.w, b.size.h);
    [
        Some(Rect::new(x, y + r as i32, w, h - 2 * r)),
        Some(Rect::new(x + r as i32, y, w - 2 * r, r)),
        Some(Rect::new(x + r as i32, y + (h - r) as i32, w - 2 * r, r)),
    ]
}

/// Cut `cut` out of every rectangle in `region`, in place, or leave `region` untouched.
///
/// All or nothing: running out of room is an over-fill, never a hole. A bound enforced two ways
/// that disagreed once dropped pieces, leaving background nothing fills and no surface covers.
fn subtract_from(region: &mut [Option<Rect>], cut: &Rect) {
    // The capacity is the region's length, so a test can run this at every size.
    let cap = region.len().min(MAX_EXPOSED);
    let mut out: [Option<Rect>; MAX_EXPOSED] = [None; MAX_EXPOSED];
    let mut n = 0usize;
    // `false` when the piece will not fit; an empty band is skipped, not refused.
    let push = |r: Rect, n: &mut usize, out: &mut [Option<Rect>; MAX_EXPOSED]| -> bool {
        if r.size.w == 0 || r.size.h == 0 {
            return true;
        }
        if *n == cap {
            return false;
        }
        out[*n] = Some(r);
        *n += 1;
        true
    };
    for piece in region.iter().flatten() {
        let Some(hit) = piece.intersect(cut) else {
            if !push(*piece, &mut n, &mut out) {
                return;
            }
            continue;
        };
        let (l, t) = (piece.origin.x, piece.origin.y);
        let (r, b) = (piece.right() as i32, piece.bottom() as i32);
        let (hl, ht) = (hit.origin.x, hit.origin.y);
        let (hr, hb) = (hit.right() as i32, hit.bottom() as i32);
        // Above, below and the two side bands: a partition, so no pixel is doubled or missed.
        let bands = [
            Rect::new(l, t, (r - l) as u32, (ht - t).max(0) as u32),
            Rect::new(l, hb, (r - l) as u32, (b - hb).max(0) as u32),
            Rect::new(l, ht, (hl - l).max(0) as u32, (hb - ht) as u32),
            Rect::new(hr, ht, (r - hr).max(0) as u32, (hb - ht) as u32),
        ];
        for band in bands {
            if !push(band, &mut n, &mut out) {
                return;
            }
        }
    }
    for (i, slot) in region.iter_mut().enumerate() {
        *slot = out.get(i).copied().flatten();
    }
}

/// Equivalent to [`compose`] with one full-screen damage rectangle.
pub fn compose_full<F: Framebuffer + ?Sized>(fb: &mut F, background: Rgb, surfaces: &[SurfaceRef<'_>]) {
    let screen = fb.geometry().bounds();
    compose(fb, background, surfaces, &[screen]);
}

/// Draw the part of `surface` inside `area`, through `Rgb` since formats may differ in order.
fn blit_clipped<F: Framebuffer + ?Sized>(fb: &mut F, surface: &SurfaceRef<'_>, area: &Rect) {
    let Some(visible) = surface.bounds().intersect(area) else { return };
    let r = surface.radius();
    if r == 0 {
        blit_span(fb, surface, visible);
        return;
    }
    // The masked blit: a plain middle band (the `memcpy` path), and per corner row a span between
    // the curves plus the pixels on each curve blended at their coverage.
    let b = surface.bounds();
    let (top, bottom) = (b.origin.y, b.bottom() as i32);
    let middle = Rect::new(b.origin.x, top + r as i32, b.size.w, b.size.h - 2 * r);
    if let Some(m) = middle.intersect(&visible) {
        blit_span(fb, surface, m);
    }
    for y in (top..top + r as i32).chain(bottom - r as i32..bottom) {
        if y < visible.origin.y || y >= visible.bottom() as i32 {
            continue;
        }
        let dy = (y - top).min(bottom - 1 - y) as u32;
        let shape = crate::corner::row(r, dy);
        let inset = shape.clear + shape.partial;
        let span = Rect::new(b.origin.x + inset as i32, y, b.size.w - 2 * inset, 1);
        if let Some(s) = span.intersect(&visible) {
            blit_span(fb, surface, s);
        }
        for i in shape.clear..inset {
            let a = crate::corner::coverage(r, i, dy);
            for x in [b.origin.x + i as i32, b.right() as i32 - 1 - i as i32] {
                if visible.contains(x, y) {
                    blend_one(fb, surface, x, y, a);
                }
            }
        }
    }
}

/// One pixel of `surface` at screen `(x, y)` blended at `coverage`, the edge of a rounded corner;
/// the surface's own alpha is multiplied in.
fn blend_one<F: Framebuffer + ?Sized>(fb: &mut F, surface: &SurfaceRef<'_>, x: i32, y: i32, coverage: u8) {
    let src = surface.geometry;
    let (sx, sy) = ((x - surface.origin.x) as u32, (y - surface.origin.y) as u32);
    let Some(off) = src.offset_of(sx, sy) else { return };
    let Some(bytes) = surface.pixels.get(off..off + 4) else { return };
    let word = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let alpha = src.format.alpha_of(word) as u32;
    let a = ((alpha * coverage as u32 + 127) / 255) as u8;
    fb.blend_pixel(x as u32, y as u32, src.format.decode(word), a);
}

/// Draw the whole of `visible` from `surface` by whichever path its format takes.
fn blit_span<F: Framebuffer + ?Sized>(fb: &mut F, surface: &SurfaceRef<'_>, visible: Rect) {
    let src = surface.geometry;

    // On every real path the formats match and a row is a `memcpy`; the per-pixel path is what a
    // drag spends its time on (halving pixel writes bought 6%).
    if src.format.has_alpha() {
        blit_blended(fb, surface, visible);
    } else if src.format == fb.geometry().format {
        blit_rows(fb, surface, visible);
    } else {
        blit_pixels(fb, surface, visible);
    }
}

/// The slow path, for surfaces with an alpha channel: blending reads the destination, so it is not
/// a row copy (hence alpha is opt-in). [`Framebuffer::blend_pixel`] skips transparent pixels and
/// stores opaque ones.
fn blit_blended<F: Framebuffer + ?Sized>(fb: &mut F, surface: &SurfaceRef<'_>, visible: Rect) {
    let src = surface.geometry;
    let bpp = src.format.bytes_per_pixel();
    for row in 0..visible.size.h {
        let dst_y = visible.origin.y + row as i32;
        let src_y = (dst_y - surface.origin.y) as u32;
        for col in 0..visible.size.w {
            let dst_x = visible.origin.x + col as i32;
            let src_x = (dst_x - surface.origin.x) as u32;
            let Some(off) = src.offset_of(src_x, src_y) else { continue };
            if off + bpp > surface.pixels.len() {
                continue;
            }
            let word = u32::from_le_bytes([
                surface.pixels[off],
                surface.pixels[off + 1],
                surface.pixels[off + 2],
                surface.pixels[off + 3],
            ]);
            fb.blend_pixel(dst_x as u32, dst_y as u32, src.format.decode(word), src.format.alpha_of(word));
        }
    }
}

/// The general case, and the definition [`blit_rows`] must agree with: every caller takes the fast
/// path, so a test compares them.
fn blit_pixels<F: Framebuffer + ?Sized>(fb: &mut F, surface: &SurfaceRef<'_>, visible: Rect) {
    let src = surface.geometry;
    let src_bpp = src.format.bytes_per_pixel();
    for row in 0..visible.size.h {
        let dst_y = visible.origin.y + row as i32;
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

/// Copy `visible` from `surface` to `fb` a row at a time, for surfaces sharing the pixel format.
///
/// A short surface is clamped to the bytes held, rounded down to a whole pixel, matching the
/// per-pixel path. Padding bits ride along; nothing reads them.
fn blit_rows<F: Framebuffer + ?Sized>(fb: &mut F, surface: &SurfaceRef<'_>, visible: Rect) {
    let src = surface.geometry;
    let dst = fb.geometry();
    let bpp = src.format.bytes_per_pixel();
    let src_x = (visible.origin.x - surface.origin.x) as u32;

    for row in 0..visible.size.h {
        let dst_y = visible.origin.y + row as i32;
        let src_y = (dst_y - surface.origin.y) as u32;
        let (Some(soff), Some(doff)) =
            (src.offset_of(src_x, src_y), dst.offset_of(visible.origin.x as u32, dst_y as u32))
        else {
            continue;
        };
        let want = visible.size.w as usize * bpp;
        let have = surface.pixels.len().saturating_sub(soff).min(want);
        let have = have - have % bpp;
        if have == 0 {
            continue;
        }
        let bytes = fb.bytes_mut();
        if doff + have > bytes.len() {
            continue;
        }
        bytes[doff..doff + have].copy_from_slice(&surface.pixels[soff..soff + have]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::PixelFormat;

    /// How much of screen pixel `(x, y)` a surface at `bounds` with corners of `radius` covers: the
    /// per-pixel definition `draw_layer_reference` and the tests use.
    fn covered(bounds: Rect, radius: u32, x: i32, y: i32) -> u8 {
        if radius == 0 {
            return 255;
        }
        let dx = (x - bounds.origin.x).min(bounds.right() as i32 - 1 - x) as u32;
        let dy = (y - bounds.origin.y).min(bounds.bottom() as i32 - 1 - y) as u32;
        crate::corner::coverage(radius, dx, dy)
    }

    /// A framebuffer that counts background pixels filled (a count, not a duration, which would
    /// measure this machine). Only fills and `put_pixel`s count: the blits are identical and the
    /// fill is the whole difference between `compose` and `compose_exposed`.
    struct Counting {
        inner: crate::framebuffer::MemFramebuffer,
        writes: core::cell::Cell<usize>,
    }

    impl Counting {
        fn new(g: Geometry) -> Self {
            Self { inner: crate::framebuffer::MemFramebuffer::new(g), writes: 0.into() }
        }
    }

    impl Framebuffer for Counting {
        fn geometry(&self) -> Geometry {
            self.inner.geometry()
        }
        fn bytes(&self) -> &[u8] {
            self.inner.bytes()
        }
        fn bytes_mut(&mut self) -> &mut [u8] {
            self.inner.bytes_mut()
        }
        fn put_pixel(&mut self, x: u32, y: u32, colour: Rgb) {
            self.writes.set(self.writes.get() + 1);
            self.inner.put_pixel(x, y, colour);
        }
        fn fill_rect(&mut self, r: Rect, colour: Rgb) {
            self.writes.set(self.writes.get() + (r.size.w * r.size.h) as usize);
            self.inner.fill_rect(r, colour);
        }
    }

    fn screen_geom() -> Geometry {
        Geometry::with_pitch(64, 48, 64 * 4, PixelFormat::XRGB8888).unwrap()
    }

    fn shade() -> Shadow {
        Shadow::single(ShadowLayer { radius: 6, offset: Point::new(0, 2), colour: Rgb::new(0, 0, 0), strength: 160 })
    }

    #[test]
    fn a_shadow_darkens_outside_the_surface_and_never_inside_it() {
        let g = screen_geom();
        let bg = Rgb::new(120, 120, 120);
        let ink = Rgb::new(200, 30, 30);
        let (sg, px, o) = surface(20, 16, 12, 10, ink);
        let surfaces = [SurfaceRef::new(sg, o, &px).with_shadow(shade())];

        let mut fb = crate::framebuffer::MemFramebuffer::new(g);
        compose(&mut fb, bg, &surfaces, &[g.bounds()]);

        assert_eq!(fb.get_pixel(25, 20), Some(ink));
        let left = fb.get_pixel(19, 20).unwrap();
        assert!(left.r < bg.r, "no shadow beside the surface: {left:?}");
        assert_eq!(fb.get_pixel(5, 5), Some(bg));
    }

    #[test]
    fn the_shadow_falls_off_fast_near_the_surface_and_leaves_a_faint_tail() {
        // The shape, not the constants: the replaced `(r² - d²) / r²` passed every other test.
        // Ratios against the edge value, so only a change of curve breaks them: that curve gives
        // 94% / 75% / 44% at these points, this one 56% / 25% / 6%.
        let g = screen_geom();
        let bg = Rgb::new(255, 255, 255);
        let sh = Shadow::single(ShadowLayer {
            radius: 16,
            offset: Point::new(0, 0),
            colour: Rgb::new(0, 0, 0),
            strength: 200,
        });
        let (sg, px, o) = surface(24, 8, 8, 8, Rgb::new(200, 30, 30));
        let mut fb = crate::framebuffer::MemFramebuffer::new(g);
        compose(&mut fb, bg, &[SurfaceRef::new(sg, o, &px).with_shadow(sh)], &[g.bounds()]);

        let cov = |d: i32| 255 - fb.get_pixel((24 - 1 - d) as u32, 12).unwrap().r as i32;
        let edge = cov(0);
        assert!(edge > 150, "the premise: the edge is dark, got {edge}");
        let at = |d: i32| cov(d) * 100 / edge;
        assert!(at(4) < 70, "a quarter out should be well under three quarters, got {}%", at(4));
        assert!(at(8) < 40, "half way out should be a faint tail, got {}%", at(8));
        assert!(at(12) < 15, "three quarters out is nearly nothing, got {}%", at(12));
    }

    #[test]
    fn a_shadow_fades_with_distance_and_stops_at_its_radius() {
        let g = screen_geom();
        let bg = Rgb::new(120, 120, 120);
        let (sg, px, o) = surface(20, 16, 12, 10, Rgb::new(200, 30, 30));
        let mut fb = crate::framebuffer::MemFramebuffer::new(g);
        compose(&mut fb, bg, &[SurfaceRef::new(sg, o, &px).with_shadow(shade())], &[g.bounds()]);

        let mut last = 0u8;
        for (i, x) in (14..20).rev().enumerate() {
            let v = fb.get_pixel(x, 20).unwrap().r;
            if i > 0 {
                assert!(v > last, "shadow did not lighten at x={x}: {v} after {last}");
            }
            last = v;
        }
        assert_eq!(fb.get_pixel(13, 20), Some(bg), "the shadow reached past its radius");
    }

    #[test]
    fn a_shadow_falls_on_the_window_below_it_and_under_its_own() {
        // The ordering rule: all shadows before all surfaces would put this under the lower
        // window, all after would put it over the upper.
        let g = screen_geom();
        let bg = Rgb::new(120, 120, 120);
        let under = Rgb::new(240, 240, 240);
        let over = Rgb::new(200, 30, 30);
        let (ug, upx, uo) = surface(4, 4, 40, 30, under);
        let (og, opx, oo) = surface(20, 16, 12, 10, over);
        let surfaces = [SurfaceRef::new(ug, uo, &upx), SurfaceRef::new(og, oo, &opx).with_shadow(shade())];

        let mut fb = crate::framebuffer::MemFramebuffer::new(g);
        compose(&mut fb, bg, &surfaces, &[g.bounds()]);

        let on_under = fb.get_pixel(19, 20).unwrap();
        assert!(on_under.r < under.r, "the shadow did not fall on the window below: {on_under:?}");
        assert_eq!(fb.get_pixel(25, 20), Some(over));
    }

    #[test]
    fn compose_exposed_draws_the_same_picture_as_compose_with_shadows() {
        let g = screen_geom();
        let bg = Rgb::new(120, 120, 120);
        let stale = Rgb::new(240, 0, 240);
        let (ag, apx, ao) = surface(4, 4, 20, 16, Rgb::new(80, 160, 80));
        let (bg2, bpx, bo) = surface(26, 18, 20, 16, Rgb::new(200, 30, 30));
        let surfaces =
            [SurfaceRef::new(ag, ao, &apx).with_shadow(shade()), SurfaceRef::new(bg2, bo, &bpx).with_shadow(shade())];

        let mut plain = crate::framebuffer::MemFramebuffer::new(g);
        plain.clear(stale);
        compose(&mut plain, bg, &surfaces, &[g.bounds()]);
        let mut exposed = crate::framebuffer::MemFramebuffer::new(g);
        exposed.clear(stale);
        compose_exposed(&mut exposed, bg, &surfaces, &[g.bounds()]);

        assert_eq!(plain.bytes(), exposed.bytes(), "the ground under a shadow was skipped");
        assert_ne!(plain.get_pixel(2, 20), Some(stale), "nothing was drawn to compare");
    }

    #[test]
    fn damage_bounds_a_shadow_the_way_it_bounds_a_surface() {
        let g = screen_geom();
        let bg = Rgb::new(120, 120, 120);
        let (sg, px, o) = surface(20, 16, 12, 10, Rgb::new(200, 30, 30));
        let mut fb = crate::framebuffer::MemFramebuffer::new(g);
        fb.clear(bg);
        compose(&mut fb, bg, &[SurfaceRef::new(sg, o, &px).with_shadow(shade())], &[Rect::new(20, 16, 12, 10)]);
        assert_eq!(fb.get_pixel(19, 20), Some(bg), "the shadow escaped the damage rectangle");
    }

    #[test]
    fn a_shadow_offset_past_its_radius_still_covers_the_surface() {
        // `around` must never start inside the window, or a move leaves a band unrepainted.
        let bounds = Rect::new(40, 30, 20, 16);
        let far = Shadow::single(ShadowLayer {
            radius: 4,
            offset: Point::new(0, 30),
            colour: Rgb::new(0, 0, 0),
            strength: 80,
        });
        let r = far.around(bounds);
        assert!(
            r.origin.x <= bounds.origin.x
                && r.origin.y <= bounds.origin.y
                && r.right() >= bounds.right()
                && r.bottom() >= bounds.bottom(),
            "{r:?} does not contain {bounds:?}"
        );
        assert!(r.bottom() >= bounds.bottom() + 30 + 4);
    }

    #[test]
    fn painted_bounds_includes_the_shadow_and_bounds_does_not() {
        let (sg, px, o) = surface(20, 16, 12, 10, Rgb::new(1, 2, 3));
        let plain = SurfaceRef::new(sg, o, &px);
        assert_eq!(plain.painted_bounds(), plain.bounds());

        let shadowed = plain.with_shadow(shade());
        assert_eq!(shadowed.bounds(), Rect::new(20, 16, 12, 10), "bounds must not grow");
        assert_eq!(shadowed.painted_bounds(), Rect::new(14, 12, 24, 22));
    }

    fn translucent(x: i32, y: i32, w: u32, h: u32, c: Rgb, alpha: u8) -> (Geometry, alloc::vec::Vec<u8>, Point) {
        let g = Geometry::with_pitch(w, h, w as usize * 4, PixelFormat::ARGB8888).unwrap();
        let word = g.format.encode_alpha(c, alpha).to_le_bytes();
        let mut px = alloc::vec![0u8; g.pitch * h as usize];
        for p in px.as_chunks_mut::<4>().0 {
            p.copy_from_slice(&word);
        }
        (g, px, Point::new(x, y))
    }

    #[test]
    fn a_translucent_surface_blends_with_what_is_under_it() {
        let g = screen_geom();
        let bg = Rgb::new(0, 0, 0);
        let (sg, px, o) = translucent(4, 4, 10, 8, Rgb::new(200, 100, 50), 128);
        let surfaces = [SurfaceRef::new(sg, o, &px)];

        let mut fb = crate::framebuffer::MemFramebuffer::new(g);
        compose(&mut fb, bg, &surfaces, &[g.bounds()]);

        // Asserted against `blend` to pin the plumbing (channels, operands), not the mixing rule.
        assert_eq!(fb.get_pixel(5, 5), Some(Rgb::new(200, 100, 50).blend(bg, 128)));
        assert_ne!(fb.get_pixel(5, 5), Some(Rgb::new(200, 100, 50)));
        assert_ne!(fb.get_pixel(5, 5), Some(bg));
        assert_eq!(fb.get_pixel(20, 20), Some(bg));
    }

    #[test]
    fn a_fully_opaque_argb_surface_draws_what_an_xrgb_one_draws() {
        let g = screen_geom();
        let bg = Rgb::new(9, 9, 9);
        let colour = Rgb::new(200, 100, 50);

        let (ag, apx, ao) = translucent(4, 4, 10, 8, colour, 255);
        let mut argb = crate::framebuffer::MemFramebuffer::new(g);
        compose(&mut argb, bg, &[SurfaceRef::new(ag, ao, &apx)], &[g.bounds()]);

        let (xg, xpx, xo) = surface(4, 4, 10, 8, colour);
        let mut xrgb = crate::framebuffer::MemFramebuffer::new(g);
        compose(&mut xrgb, bg, &[SurfaceRef::new(xg, xo, &xpx)], &[g.bounds()]);

        assert_eq!(argb.bytes(), xrgb.bytes());
        assert_eq!(argb.get_pixel(5, 5), Some(colour), "neither drew the surface");
    }

    #[test]
    fn a_fully_transparent_surface_leaves_the_background_showing() {
        let g = screen_geom();
        let bg = Rgb::new(9, 9, 9);
        let (sg, px, o) = translucent(4, 4, 10, 8, Rgb::new(200, 100, 50), 0);
        let mut fb = crate::framebuffer::MemFramebuffer::new(g);
        compose(&mut fb, bg, &[SurfaceRef::new(sg, o, &px)], &[g.bounds()]);
        assert_eq!(fb.get_pixel(5, 5), Some(bg));
    }

    #[test]
    fn a_translucent_surface_blends_against_the_background_not_the_last_frame() {
        // A translucent surface must not count as covering: skipping the fill would blend it
        // against stale pixels. The buffers start holding a previous frame to make that visible.
        let g = screen_geom();
        let bg = Rgb::new(9, 9, 9);
        let stale = Rgb::new(240, 0, 240);
        let (sg, px, o) = translucent(4, 4, 10, 8, Rgb::new(200, 100, 50), 128);
        let surfaces = [SurfaceRef::new(sg, o, &px)];

        let mut plain = crate::framebuffer::MemFramebuffer::new(g);
        plain.clear(stale);
        compose(&mut plain, bg, &surfaces, &[g.bounds()]);
        let mut exposed = crate::framebuffer::MemFramebuffer::new(g);
        exposed.clear(stale);
        compose_exposed(&mut exposed, bg, &surfaces, &[g.bounds()]);

        assert_eq!(plain.bytes(), exposed.bytes(), "the fill under a translucent surface was skipped");
        assert_ne!(exposed.get_pixel(5, 5), Some(Rgb::new(200, 100, 50).blend(stale, 128)));
    }

    #[test]
    fn a_short_translucent_surface_blends_what_it_has_and_skips_the_rest() {
        // Defensive: the compositor drops short surfaces first, but an untested skip could read
        // past a client's buffer.
        let g = screen_geom();
        let bg = Rgb::new(9, 9, 9);
        let ink = Rgb::new(200, 100, 50);
        let (sg, px, o) = translucent(2, 2, 10, 8, ink, 128);

        let short = &px[..sg.pitch * 2 + 5];
        let mut fb = crate::framebuffer::MemFramebuffer::new(g);
        compose(&mut fb, bg, &[SurfaceRef::new(sg, o, short)], &[g.bounds()]);

        assert_eq!(fb.get_pixel(3, 2), Some(ink.blend(bg, 128)));
        assert_eq!(fb.get_pixel(3, 6), Some(bg));
        let mut full = crate::framebuffer::MemFramebuffer::new(g);
        compose(&mut full, bg, &[SurfaceRef::new(sg, o, &px)], &[g.bounds()]);
        assert_eq!(full.get_pixel(3, 6), Some(ink.blend(bg, 128)));
    }

    #[test]
    fn an_opaque_surface_still_covers_and_a_translucent_one_does_not() {
        let (og, opx, oo) = surface(0, 0, 8, 8, Rgb::new(1, 2, 3));
        assert!(covers(&SurfaceRef::new(og, oo, &opx)));
        let (tg, tpx, to) = translucent(0, 0, 8, 8, Rgb::new(1, 2, 3), 255);
        assert!(!covers(&SurfaceRef::new(tg, to, &tpx)), "opaque *pixels* are not an opaque format");
        assert!(!covers(&SurfaceRef::new(og, oo, &opx[..4])), "a short buffer covers nothing");
    }

    /// `subtract_from` never loses a pixel and never invents one, at any capacity, including the
    /// overflow path the shipped bound of 16 rarely reaches.
    #[test]
    fn subtracting_never_loses_a_pixel_at_any_capacity() {
        fn covered(region: &[Option<Rect>]) -> alloc::vec::Vec<(i32, i32)> {
            let mut out = alloc::vec::Vec::new();
            for y in 0..24 {
                for x in 0..24 {
                    let inside =
                        |r: &Rect| x >= r.origin.x && y >= r.origin.y && x < r.right() as i32 && y < r.bottom() as i32;
                    if region.iter().flatten().any(inside) {
                        out.push((x, y));
                    }
                }
            }
            out
        }

        let cuts = [
            Rect::new(4, 4, 6, 6),
            Rect::new(12, 2, 5, 9),
            Rect::new(2, 14, 9, 5),
            Rect::new(15, 15, 6, 6),
            Rect::new(8, 9, 4, 4),
            Rect::new(18, 3, 3, 3),
        ];

        for cap in 1..=MAX_EXPOSED {
            let mut region: [Option<Rect>; MAX_EXPOSED] = [None; MAX_EXPOSED];
            region[0] = Some(Rect::new(0, 0, 24, 24));
            let mut cut_so_far = alloc::vec::Vec::new();

            for cut in &cuts {
                let before = covered(&region[..cap]);
                subtract_from(&mut region[..cap], cut);
                let after = covered(&region[..cap]);
                cut_so_far.push(*cut);

                for p in &before {
                    let taken = p.0 >= cut.origin.x
                        && p.1 >= cut.origin.y
                        && p.0 < cut.right() as i32
                        && p.1 < cut.bottom() as i32;
                    assert!(after.contains(p) || taken, "cap {cap}: {p:?} was dropped — background nothing will paint");
                }
                for p in &after {
                    assert!(before.contains(p), "cap {cap}: {p:?} appeared from nowhere");
                }
            }
            if cap == MAX_EXPOSED {
                assert!(
                    covered(&region[..cap]).len() < 24 * 24,
                    "nothing was ever subtracted, so neither direction was tested"
                );
            }
        }
    }

    /// Enough surfaces to overflow the exposed region: it must over-fill, not leave a hole (five
    /// interior surfaces once left a 9x4 block of stale pixels at (55,14)).
    #[test]
    fn overflowing_the_exposed_region_over_fills_rather_than_leaving_a_hole() {
        let g = screen_geom();
        let bg = Rgb::new(0x2A, 0x55, 0x70);
        let never = Rgb::new(1, 2, 3);
        let boxes = [(42, 2, 8, 16), (7, 12, 6, 21), (33, 14, 22, 5), (28, 27, 5, 6), (6, 36, 6, 11)];
        let made: alloc::vec::Vec<_> =
            boxes.iter().map(|&(x, y, w, h)| surface(x, y, w, h, Rgb::new(200, 30, 30))).collect();
        let surfaces: alloc::vec::Vec<SurfaceRef<'_>> =
            made.iter().map(|(sg, px, o)| SurfaceRef::new(*sg, *o, px)).collect();
        let damage = [g.bounds()];

        let mut plain = crate::framebuffer::MemFramebuffer::new(g);
        plain.clear(never);
        compose(&mut plain, bg, &surfaces, &damage);
        let mut exposed = crate::framebuffer::MemFramebuffer::new(g);
        exposed.clear(never);
        compose_exposed(&mut exposed, bg, &surfaces, &damage);

        assert_eq!(plain.bytes(), exposed.bytes(), "a region was left neither filled nor covered");
        assert_eq!(plain.get_pixel(55, 14), Some(bg));
        assert_ne!(exposed.get_pixel(55, 14), Some(never), "stale pixels at the reported spot");
    }

    /// The row-copy fast path and the per-pixel reference agree, over surfaces off each edge, a
    /// narrow strip, a buffer a byte short of a pixel and one so short most rows copy nothing.
    #[test]
    fn the_row_copy_agrees_with_the_per_pixel_blit() {
        let g = screen_geom();
        let bg = Rgb::new(0x2A, 0x55, 0x70);
        let mk = |w: u32, h: u32| {
            let sg = Geometry::with_pitch(w, h, w as usize * 4 + 12, PixelFormat::XRGB8888).unwrap();
            let mut px = alloc::vec![0u8; sg.pitch * h as usize];
            for y in 0..h {
                for x in 0..w {
                    let c = Rgb::new((x * 7 % 256) as u8, (y * 11 % 256) as u8, 0x40);
                    let off = sg.offset_of(x, y).unwrap();
                    px[off..off + 4].copy_from_slice(&sg.format.encode(c).to_le_bytes());
                }
            }
            (sg, px)
        };

        let (sg, full) = mk(20, 16);
        let short_pixel = &full[..full.len() - 1]; // a byte shy of the last whole pixel
        let short_rows = &full[..sg.pitch * 3 + 5]; // three rows and a fragment

        let cases: &[(&str, Point, Rect, &[u8])] = &[
            ("centred", Point::new(10, 8), Rect::new(0, 0, 64, 48), &full),
            ("off the left edge", Point::new(-7, 8), Rect::new(0, 0, 64, 48), &full),
            ("off the top edge", Point::new(10, -5), Rect::new(0, 0, 64, 48), &full),
            ("off the right edge", Point::new(50, 8), Rect::new(0, 0, 64, 48), &full),
            ("off the bottom edge", Point::new(10, 40), Rect::new(0, 0, 64, 48), &full),
            ("clipped to a strip", Point::new(10, 8), Rect::new(14, 10, 3, 40), &full),
            ("damage misses it", Point::new(10, 8), Rect::new(40, 30, 8, 8), &full),
            ("a pixel short", Point::new(10, 8), Rect::new(0, 0, 64, 48), short_pixel),
            ("rows short", Point::new(10, 8), Rect::new(0, 0, 64, 48), short_rows),
        ];

        for (name, origin, damage, pixels) in cases {
            let surface = SurfaceRef::new(sg, *origin, pixels);
            let Some(visible) = surface.bounds().intersect(damage) else {
                continue;
            };

            let mut fast = crate::framebuffer::MemFramebuffer::new(g);
            fast.fill_rect(g.bounds(), bg);
            blit_rows(&mut fast, &surface, visible);

            let mut slow = crate::framebuffer::MemFramebuffer::new(g);
            slow.fill_rect(g.bounds(), bg);
            blit_pixels(&mut slow, &surface, visible);

            assert_eq!(fast.bytes(), slow.bytes(), "{name}: the two blits disagree");
            if damage.intersect(&g.bounds()).is_some() && !pixels.is_empty() {
                let mut plain = crate::framebuffer::MemFramebuffer::new(g);
                plain.fill_rect(g.bounds(), bg);
                assert_ne!(fast.bytes(), plain.bytes(), "{name}: drew nothing to compare");
            }
        }
    }

    fn surface(x: i32, y: i32, w: u32, h: u32, c: Rgb) -> (Geometry, alloc::vec::Vec<u8>, Point) {
        let g = Geometry::with_pitch(w, h, w as usize * 4, PixelFormat::XRGB8888).unwrap();
        let word = g.format.encode(c).to_le_bytes();
        let mut px = alloc::vec![0u8; g.pitch * h as usize];
        for p in px.as_chunks_mut::<4>().0 {
            p.copy_from_slice(&word);
        }
        (g, px, Point::new(x, y))
    }

    #[test]
    fn compose_exposed_draws_the_same_picture_as_compose() {
        let g = screen_geom();
        let bg = Rgb::new(0x2A, 0x55, 0x70);
        let (sg, spx, _) = surface(0, 0, 20, 16, Rgb::new(200, 30, 30));
        let (tg, tpx, _) = surface(0, 0, 24, 24, Rgb::new(30, 200, 30));

        for (origin_a, origin_b, damage) in [
            (Point::new(4, 4), Point::new(40, 40), alloc::vec![Rect::new(4, 4, 20, 16)]),
            (Point::new(9, 4), Point::new(40, 40), alloc::vec![Rect::new(8, 4, 20, 16), Rect::new(9, 4, 20, 16),]),
            (Point::new(4, 4), Point::new(40, 40), alloc::vec![Rect::new(50, 30, 10, 10)]),
            (Point::new(4, 4), Point::new(14, 10), alloc::vec![Rect::new(0, 0, 40, 40)]),
            (Point::new(-6, -4), Point::new(56, 40), alloc::vec![Rect::new(0, 0, 64, 48)]),
        ] {
            let surfaces = [SurfaceRef::new(sg, origin_a, &spx), SurfaceRef::new(tg, origin_b, &tpx)];
            let mut a = crate::framebuffer::MemFramebuffer::new(g);
            let mut b = crate::framebuffer::MemFramebuffer::new(g);
            compose(&mut a, bg, &surfaces, &damage);
            compose_exposed(&mut b, bg, &surfaces, &damage);
            assert_eq!(a.bytes(), b.bytes(), "arrangement {origin_a:?}/{origin_b:?} {damage:?}");
        }
    }

    #[test]
    fn a_two_layer_shadow_is_its_layers_drawn_wide_first() {
        let g = screen_geom();
        let bg = Rgb::new(120, 120, 120);
        let bounds = Rect::new(20, 12, 20, 14);
        let wide = ShadowLayer { radius: 12, offset: Point::new(0, 5), colour: Rgb::BLACK, strength: 60 };
        let contact = ShadowLayer { radius: 3, offset: Point::new(0, 1), colour: Rgb::new(200, 0, 0), strength: 120 };
        let draw = |layers: &[ShadowLayer]| {
            let mut fb = crate::framebuffer::MemFramebuffer::filled(g, bg);
            for l in layers {
                draw_shadow(&mut fb, bounds, 0, &Shadow::single(*l), &g.bounds());
            }
            fb
        };
        let mut both = crate::framebuffer::MemFramebuffer::filled(g, bg);
        draw_shadow(&mut both, bounds, 0, &Shadow { layers: [wide, contact] }, &g.bounds());
        assert_eq!(both, draw(&[wide, contact]), "the pair is its layers, wide first");
        assert_ne!(both, draw(&[contact, wide]), "and the order is observable");

        let sh = Shadow { layers: [wide, contact] };
        assert_eq!(sh.around(bounds), wide.around(bounds));
        // A layer sticking out past the other must widen `around`; the wide layer contains the
        // contact one in both shipped schemes, so only a different layer shows it.
        let dropped = ShadowLayer { radius: 3, offset: Point::new(0, 30), colour: Rgb::BLACK, strength: 60 };
        let sticks_out = Shadow { layers: [wide, dropped] };
        let reach = sticks_out.around(bounds);
        assert!(reach.bottom() > wide.around(bounds).bottom(), "the dropped layer's strip is missing");
        assert_eq!(reach.bottom(), dropped.around(bounds).bottom());
        assert_eq!(reach.origin, wide.around(bounds).origin, "and the wide layer still sets the rest");
        assert_eq!(Shadow::single(wide).around(bounds), wide.around(bounds));
        assert_eq!(ShadowLayer::NONE.around(bounds), bounds);
    }

    #[test]
    fn the_span_skipping_shadow_draws_exactly_what_the_per_pixel_one_did() {
        // Byte for byte: every gate's expected picture is drawn by this function. Cases: corners
        // of every size, an offset past and inside the radius, a clip inside the skipped span, a
        // window exactly `2c` across, one partly off screen.
        let g = screen_geom();
        let (pg, ground) = patterned(64, 48, 64 * 4, 0x33);
        for corner in [0u32, 1, 3, 6, 8, 20] {
            for layer in [
                ShadowLayer { radius: 6, offset: Point::new(0, 2), colour: Rgb::BLACK, strength: 160 },
                ShadowLayer { radius: 12, offset: Point::new(3, 7), colour: Rgb::new(40, 0, 90), strength: 90 },
                ShadowLayer { radius: 3, offset: Point::new(0, 9), colour: Rgb::BLACK, strength: 255 },
            ] {
                for bounds in [Rect::new(14, 10, 30, 20), Rect::new(20, 12, 12, 12), Rect::new(-5, 30, 24, 25)] {
                    for clip in
                        [g.bounds(), Rect::new(20, 0, 9, 48), Rect::new(0, 15, 64, 4), Rect::new(30, 22, 30, 30)]
                    {
                        let draw = |f: fn(&mut crate::framebuffer::MemFramebuffer, Rect, u32, &ShadowLayer, &Rect)| {
                            let mut fb = crate::framebuffer::MemFramebuffer::new(pg);
                            fb.bytes_mut().copy_from_slice(&ground);
                            f(&mut fb, bounds, corner, &layer, &clip);
                            fb
                        };
                        assert_eq!(
                            draw(draw_layer),
                            draw(draw_layer_reference),
                            "corner {corner}, {layer:?}, {bounds:?} through {clip:?}"
                        );
                    }
                }
            }
        }
    }

    fn draw_layer_reference<F: Framebuffer + ?Sized>(
        fb: &mut F,
        bounds: Rect,
        corner: u32,
        shadow: &ShadowLayer,
        clip: &Rect,
    ) {
        if !shadow.is_visible() {
            return;
        }
        let c = crate::corner::clamp(corner, bounds.size.w, bounds.size.h);
        let inner = Rect::new(
            bounds.origin.x + shadow.offset.x + c as i32,
            bounds.origin.y + shadow.offset.y + c as i32,
            bounds.size.w - 2 * c,
            bounds.size.h - 2 * c,
        );
        let Some(area) = shadow.around(bounds).intersect(clip) else { return };
        let r = shadow.radius as i64;
        let rr = r * r;
        for y in area.origin.y..area.bottom() as i32 {
            for x in area.origin.x..area.right() as i32 {
                if x >= bounds.origin.x
                    && y >= bounds.origin.y
                    && x < bounds.right() as i32
                    && y < bounds.bottom() as i32
                    && covered(bounds, c, x, y) == 255
                {
                    continue;
                }
                let (ix0, ix1) = (inner.origin.x, (inner.right() as i32 - 1).max(inner.origin.x));
                let (iy0, iy1) = (inner.origin.y, (inner.bottom() as i32 - 1).max(inner.origin.y));
                let dx = (ix0 - x).max(x - ix1).max(0) as i64;
                let dy = (iy0 - y).max(y - iy1).max(0) as i64;
                let d = ((dx * dx + dy * dy) as u64).isqrt() as i64 - c as i64;
                let d = d.max(0);
                if d >= r {
                    continue;
                }
                let t = r - d;
                let coverage = (shadow.strength as i64 * t * t / rr) as u8;
                fb.blend_pixel(x as u32, y as u32, shadow.colour, coverage);
            }
        }
    }

    /// A screen whose every pixel differs from its neighbours, so a wrong corner cannot match.
    fn patterned_ground() -> crate::framebuffer::MemFramebuffer {
        let (pg, ground) = patterned(64, 48, 64 * 4, 0x33);
        let mut fb = crate::framebuffer::MemFramebuffer::new(pg);
        fb.bytes_mut().copy_from_slice(&ground);
        fb
    }

    /// What a rounded surface should put at every pixel of its bounds, the slow obvious way.
    fn rounded_expectation(
        under: &crate::framebuffer::MemFramebuffer,
        surface: &SurfaceRef<'_>,
    ) -> crate::framebuffer::MemFramebuffer {
        let mut want = under.clone();
        let b = surface.bounds();
        let r = surface.radius();
        for y in b.origin.y.max(0)..(b.bottom() as i32).min(under.geometry().height as i32) {
            for x in b.origin.x.max(0)..(b.right() as i32).min(under.geometry().width as i32) {
                let (sx, sy) = ((x - b.origin.x) as u32, (y - b.origin.y) as u32);
                let off = surface.geometry.offset_of(sx, sy).unwrap();
                let word = u32::from_le_bytes(surface.pixels[off..off + 4].try_into().unwrap());
                let colour = surface.geometry.format.decode(word);
                let alpha = surface.geometry.format.alpha_of(word) as u32;
                let a = (alpha * covered(b, r, x, y) as u32 + 127) / 255;
                want.blend_pixel(x as u32, y as u32, colour, a as u8);
            }
        }
        want
    }

    #[test]
    fn a_rounded_surface_blends_its_curve_and_leaves_the_outside_to_what_is_below() {
        let g = screen_geom();
        let under = patterned_ground();
        let (sg, spx) = patterned(30, 20, 30 * 4 + 8, 0x90);
        let surface = SurfaceRef::new(sg, Point::new(9, 7), &spx).with_corner(8);

        let mut got = under.clone();
        blit_clipped(&mut got, &surface, &g.bounds());
        assert_eq!(got, rounded_expectation(&under, &surface));
        assert_eq!(got.get_pixel(9, 7), under.get_pixel(9, 7));
        assert_ne!(got.get_pixel(9, 15), under.get_pixel(9, 15), "the straight edge was drawn");
    }

    #[test]
    fn every_blit_path_rounds_the_same_curve() {
        // The mask sits in front of three paths; each must cut the same corner.
        let g = screen_geom();
        let under = patterned_ground();
        let (sg, spx) = patterned(24, 18, 24 * 4, 0x90);
        let bgr = PixelFormat {
            red: crate::format::Channel::new(0, 8),
            blue: crate::format::Channel::new(16, 8),
            ..PixelFormat::XRGB8888
        };
        let mut translucent = spx.clone();
        for (i, p) in translucent.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            p[3] = [255u8, 200, 90, 0][i % 4];
        }
        let cases: [(&str, Geometry, &[u8]); 3] = [
            ("row copy", sg, &spx),
            ("per pixel", Geometry { format: bgr, ..sg }, &spx),
            ("blended", Geometry { format: PixelFormat::ARGB8888, ..sg }, &translucent),
        ];
        for (name, geometry, pixels) in cases {
            for (origin, damage) in [
                (Point::new(20, 15), g.bounds()),
                (Point::new(-5, -3), g.bounds()),
                (Point::new(50, 38), g.bounds()),
                (Point::new(20, 15), Rect::new(24, 18, 5, 40)),
            ] {
                let surface = SurfaceRef::new(geometry, origin, pixels).with_corner(6);
                let mut got = under.clone();
                blit_clipped(&mut got, &surface, &damage);
                let mut want = rounded_expectation(&under, &surface);
                for y in 0..48 {
                    for x in 0..64 {
                        if !damage.contains(x, y) {
                            let u = under.get_pixel(x as u32, y as u32).unwrap();
                            want.put_pixel(x as u32, y as u32, u);
                        }
                    }
                }
                assert_eq!(got, want, "{name} at {origin:?} through {damage:?}");
            }
        }
    }

    #[test]
    fn compose_exposed_draws_the_same_picture_as_compose_with_rounded_corners() {
        // Cutting the whole rectangle would leave the corners stale, which the stale colour shows.
        let g = screen_geom();
        let bg = Rgb::new(0x2A, 0x55, 0x70);
        let stale = Rgb::new(240, 0, 240);
        let (sg, spx) = patterned(20, 16, 20 * 4, 0x60);
        let (tg, tpx) = patterned(24, 24, 24 * 4, 0xC0);
        for (a_at, b_at, damage) in [
            (Point::new(4, 4), Point::new(40, 40), alloc::vec![Rect::new(4, 4, 20, 16)]),
            (Point::new(9, 4), Point::new(40, 40), alloc::vec![Rect::new(8, 4, 20, 16), Rect::new(9, 4, 20, 16),]),
            (Point::new(4, 4), Point::new(14, 10), alloc::vec![Rect::new(0, 0, 40, 40)]),
            (Point::new(-6, -4), Point::new(56, 40), alloc::vec![Rect::new(0, 0, 64, 48)]),
        ] {
            for shadow in [None, Some(shade())] {
                let mut surfaces =
                    [SurfaceRef::new(sg, a_at, &spx).with_corner(6), SurfaceRef::new(tg, b_at, &tpx).with_corner(6)];
                if let Some(sh) = shadow {
                    surfaces = surfaces.map(|s| s.with_shadow(sh));
                }
                let mut plain = crate::framebuffer::MemFramebuffer::new(g);
                plain.clear(stale);
                compose(&mut plain, bg, &surfaces, &damage);
                let mut exposed = crate::framebuffer::MemFramebuffer::new(g);
                exposed.clear(stale);
                compose_exposed(&mut exposed, bg, &surfaces, &damage);
                assert_eq!(plain, exposed, "{a_at:?}/{b_at:?} {damage:?}, shadow {shadow:?}");
            }
        }
    }

    #[test]
    fn a_rounded_window_over_another_shows_that_window_in_its_corner() {
        let g = screen_geom();
        let bg = Rgb::new(0x2A, 0x55, 0x70);
        let below = Rgb::new(30, 200, 30);
        let above = Rgb::new(200, 30, 30);
        let (lg, lpx, _) = surface(0, 0, 40, 30, below);
        let (ug, upx, _) = surface(0, 0, 20, 16, above);
        let surfaces = [
            SurfaceRef::new(lg, Point::new(0, 0), &lpx).with_corner(8),
            SurfaceRef::new(ug, Point::new(10, 10), &upx).with_corner(8),
        ];
        let mut fb = crate::framebuffer::MemFramebuffer::new(g);
        compose_exposed(&mut fb, bg, &surfaces, &[g.bounds()]);
        assert_eq!(fb.get_pixel(10, 10), Some(below), "the upper window's corner shows the lower");
        assert_eq!(fb.get_pixel(0, 0), Some(bg), "the lower window's corner shows the desktop");
        assert_eq!(fb.get_pixel(20, 18), Some(above));
    }

    #[test]
    fn a_rounded_surface_repainted_in_pieces_is_the_same_picture() {
        let g = screen_geom();
        let bg = Rgb::new(0x2A, 0x55, 0x70);
        let (sg, spx) = patterned(30, 22, 30 * 4, 0x60);
        let surfaces = [SurfaceRef::new(sg, Point::new(7, 5), &spx).with_corner(8).with_shadow(shade())];
        let mut whole = crate::framebuffer::MemFramebuffer::new(g);
        compose_exposed(&mut whole, bg, &surfaces, &[g.bounds()]);
        let mut pieces = crate::framebuffer::MemFramebuffer::new(g);
        let mut damage = alloc::vec::Vec::new();
        for y in (0..48).step_by(5) {
            for x in (0..64).step_by(3) {
                damage.push(Rect::new(x, y, 3, 5));
            }
        }
        compose_exposed(&mut pieces, bg, &surfaces, &damage);
        assert_eq!(pieces, whole);
    }

    #[test]
    fn a_rounded_surface_casts_a_rounded_shadow_into_its_own_corners() {
        let g = screen_geom();
        let bg = Rgb::new(120, 120, 120);
        let (sg, spx, o) = surface(20, 16, 20, 14, Rgb::new(200, 30, 30));
        let sh = Shadow::single(ShadowLayer {
            radius: 6,
            offset: Point::new(0, 0),
            colour: Rgb::new(0, 0, 0),
            strength: 160,
        });
        let surfaces = [SurfaceRef::new(sg, o, &spx).with_corner(6).with_shadow(sh)];
        let mut fb = crate::framebuffer::MemFramebuffer::new(g);
        compose(&mut fb, bg, &surfaces, &[g.bounds()]);
        let corner = fb.get_pixel(20, 16).unwrap();
        assert!(corner.r < bg.r, "the very corner is shadowed, not bare ground: {corner:?}");

        // Compared against the square shadow: beside a straight edge they are equally far, off the
        // corner the rounded one is further.
        let draw = |corner: u32| {
            let mut fb = crate::framebuffer::MemFramebuffer::filled(g, bg);
            draw_shadow(&mut fb, Rect::new(20, 16, 20, 14), corner, &sh, &g.bounds());
            fb
        };
        let (round, square) = (draw(6), draw(0));
        assert_eq!(round.get_pixel(30, 13), square.get_pixel(30, 13), "beside the top edge");
        let (r, q) = (round.get_pixel(17, 13).unwrap(), square.get_pixel(17, 13).unwrap());
        assert!(r.r > q.r, "off the corner, rounded {r:?} is not lighter than square {q:?}");
    }

    #[test]
    fn a_square_surface_draws_what_it_drew_before_corners_existed() {
        // `corner: 0` is every gate's expected picture, so check it against the definitions that
        // predate corners; comparing `compose` to itself cannot differ.
        let g = screen_geom();
        let bg = Rgb::new(0x2A, 0x55, 0x70);
        let (sg, spx) = patterned(20, 16, 20 * 4, 0x60);
        let surface = SurfaceRef::new(sg, Point::new(9, 7), &spx).with_shadow(shade());
        let mut got = crate::framebuffer::MemFramebuffer::new(g);
        compose(&mut got, bg, &[surface], &[g.bounds()]);

        let mut want = crate::framebuffer::MemFramebuffer::filled(g, bg);
        for layer in &shade().layers {
            draw_layer_reference(&mut want, surface.bounds(), 0, layer, &g.bounds());
        }
        blit_span(&mut want, &surface, surface.bounds().intersect(&g.bounds()).unwrap());
        assert_eq!(got, want);
        assert_eq!(covered_by(&surface), [Some(surface.bounds()), None, None]);
    }

    #[test]
    fn compose_exposed_fills_far_less_background_for_the_same_picture() {
        let g = screen_geom();
        let bg = Rgb::new(0x2A, 0x55, 0x70);
        let (sg, spx, _) = surface(0, 0, 20, 16, Rgb::new(200, 30, 30));
        let damage = [Rect::new(8, 4, 20, 16), Rect::new(9, 4, 20, 16)];
        let surfaces = [SurfaceRef::new(sg, Point::new(9, 4), &spx)];

        let mut plain = Counting::new(g);
        compose(&mut plain, bg, &surfaces, &damage);
        let mut exposed = Counting::new(g);
        compose_exposed(&mut exposed, bg, &surfaces, &damage);

        assert_eq!(plain.inner.bytes(), exposed.inner.bytes(), "same picture");
        let (p, e) = (plain.writes.get(), exposed.writes.get());
        // Plain fills both rectangles whole (2 x 20 x 16 = 640), exposed one column. A twentieth
        // fails if any whole rectangle is filled, which `e < p` would not.
        assert_eq!(p, 640, "plain should fill both damage rectangles whole");
        assert!(e * 20 < p, "expected a sliver, got {e} filled against {p}");
    }

    /// A short surface must not count as covering: its unread pixels would be stale.
    #[test]
    fn a_short_surface_still_gets_its_background() {
        let g = screen_geom();
        let bg = Rgb::new(0x2A, 0x55, 0x70);
        let (sg, spx, _) = surface(0, 0, 20, 16, Rgb::new(200, 30, 30));
        let truncated = &spx[..spx.len() / 2];
        let surfaces = [SurfaceRef::new(sg, Point::new(4, 4), truncated)];
        let damage = [Rect::new(4, 4, 20, 16)];

        let mut a = crate::framebuffer::MemFramebuffer::new(g);
        let mut b = crate::framebuffer::MemFramebuffer::new(g);
        a.fill_rect(g.bounds(), Rgb::new(1, 2, 3));
        b.fill_rect(g.bounds(), Rgb::new(1, 2, 3));
        compose(&mut a, bg, &surfaces, &damage);
        compose_exposed(&mut b, bg, &surfaces, &damage);
        assert_eq!(a.bytes(), b.bytes(), "a short surface must not change the picture");
        assert_eq!(b.get_pixel(4, 14), Some(bg), "the unwritten rows are background, not stale");
    }
    use crate::framebuffer::MemFramebuffer;
    use alloc::vec;
    use alloc::vec::Vec;

    /// Varies in both axes: a solid or one-axis pattern hashes identically when transposed.
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
                assert_eq!(fb.get_pixel(x + 2, y + 1), Some(g.format.decode(word)), "surface pixel ({x},{y})");
            }
        }
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
            &[SurfaceRef::new(ga, Point::new(0, 0), &pa), SurfaceRef::new(gb, Point::new(3, 3), &pb)],
        );
        let off = gb.offset_of(0, 0).unwrap();
        let word = u32::from_le_bytes([pb[off], pb[off + 1], pb[off + 2], pb[off + 3]]);
        assert_eq!(fb.get_pixel(3, 3), Some(gb.format.decode(word)));
        let off = ga.offset_of(0, 0).unwrap();
        let word = u32::from_le_bytes([pa[off], pa[off + 1], pa[off + 2], pa[off + 3]]);
        assert_eq!(fb.get_pixel(0, 0), Some(ga.format.decode(word)));
    }

    #[test]
    fn stacking_order_is_not_symmetric() {
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

        let off = g.offset_of(3, 2).unwrap();
        let word = u32::from_le_bytes([px[off], px[off + 1], px[off + 2], px[off + 3]]);
        assert_eq!(fb.get_pixel(0, 0), Some(g.format.decode(word)));
        assert_eq!(fb.get_pixel(31, 0), Some(Rgb::BLACK));
    }

    #[test]
    fn a_surface_off_the_bottom_right_is_clipped() {
        let (g, px) = patterned(8, 8, 32, 0x77);
        let mut fb = screen();
        compose_full(&mut fb, Rgb::BLACK, &[SurfaceRef::new(g, Point::new(28, 12), &px)]);
        assert!(fb.get_pixel(31, 15).is_some());
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

        assert_eq!(partial.get_pixel(2, 2), full.get_pixel(2, 2));
        assert_ne!(partial.get_pixel(6, 2), full.get_pixel(6, 2));
        assert_eq!(partial.get_pixel(6, 2), Some(Rgb::BLACK));
    }

    #[test]
    fn compositing_the_same_inputs_twice_produces_identical_bytes() {
        let (ga, pa) = patterned(9, 7, 40, 0x21);
        let (gb, pb) = patterned(5, 11, 20, 0x8C);
        let surfaces = [SurfaceRef::new(ga, Point::new(-2, 3), &pa), SurfaceRef::new(gb, Point::new(27, 9), &pb)];
        let mut a = screen();
        let mut b = screen();
        compose_full(&mut a, Rgb::new(3, 5, 7), &surfaces);
        compose_full(&mut b, Rgb::new(3, 5, 7), &surfaces);
        assert_eq!(a.bytes(), b.bytes());
    }

    #[test]
    fn a_surface_in_a_different_channel_order_is_translated_not_copied() {
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
        compose(&mut fb, Rgb::new(1, 2, 3), &[SurfaceRef::new(g, Point::new(0, 0), &px)], &[Rect::new(200, 200, 8, 8)]);
        assert_eq!(fb, before);
    }
}
