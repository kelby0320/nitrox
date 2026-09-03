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

/// Composite `surfaces` over `background`, **filling only the background nobody covers**.
///
/// The same output as [`compose`], and measurably less work. `compose` fills every damage
/// rectangle with the background and *then* blits the surfaces over it, so in a window drag most
/// pixels are written twice — once as background and once as the window. This fills only the
/// *exposed* part: the damage rectangle minus every surface that is going to cover it.
///
/// **The two are the same picture, and a test asserts exactly that** rather than asserting either
/// one's pixels: the property that matters is that removing work removed no output.
///
/// ## Why this is sound, and the one case where it is not
///
/// A surface in a format with no alpha channel is opaque, and [`blit_clipped`] writes every pixel
/// of the intersection it is given. So background under such a surface is background nobody can
/// see. A translucent surface is neither of those things and is excluded by [`covers`].
///
/// The exception is a **malformed surface**: one whose `pixels` are shorter than its `geometry`
/// claims. `blit_clipped` skips those pixels rather than reading past the buffer, and today the
/// fill underneath is what the viewer sees instead. Skipping the fill would turn that into
/// whatever the framebuffer held before — stale pixels rather than background. So a surface only
/// counts as covering when its buffer is long enough to make the blit's guarantee real, which is
/// the check [`covers`] makes.
///
/// ## Where the flicker went
///
/// This is also the flicker's cause, seen from the other side (M13 Part A): the fill *is* the
/// flash. A frame that never paints background over a region a window is about to occupy has no
/// intermediate state for a scanout to catch there.
pub fn compose_exposed<F: Framebuffer + ?Sized>(
    fb: &mut F,
    background: Rgb,
    surfaces: &[SurfaceRef<'_>],
    damage: &[Rect],
) {
    let screen = fb.geometry().bounds();
    for area in damage {
        let Some(area) = area.intersect(&screen) else { continue };
        // The exposed region: the damage rectangle with every covering surface cut out of it.
        // Accumulated as a small list of rectangles, because subtracting one rectangle from
        // another leaves up to four.
        let mut exposed: [Option<Rect>; MAX_EXPOSED] = [None; MAX_EXPOSED];
        exposed[0] = Some(area);
        for surface in surfaces {
            if !covers(surface) {
                continue;
            }
            subtract_from(&mut exposed, &surface.bounds());
        }
        for piece in exposed.iter().flatten() {
            fb.fill_rect(*piece, background);
        }
        for surface in surfaces {
            blit_clipped(fb, surface, &area);
        }
    }
}

/// How many rectangles the exposed region is allowed to become.
///
/// **A fixed array rather than a `Vec`, and a bound rather than a guarantee.** Subtracting one
/// rectangle from another leaves up to four, so the region can in principle grow without limit as
/// surfaces are cut out of it. When a subtraction will not fit, [`subtract_from`] abandons *that
/// subtraction entirely* and leaves the region as it found it — which fills more background than
/// strictly necessary and is therefore always correct, just less of a saving. Sixteen covers any
/// arrangement a drag produces; a desktop full of overlapping windows falls back toward
/// [`compose`]'s behaviour rather than toward being wrong.
///
/// **The value is a tuning choice and nothing rests on it**, which is the property to preserve:
/// `compose_exposed` must draw `compose`'s picture at *any* bound, so lowering this can only cost
/// speed. It did not always hold — see [`subtract_from`].
const MAX_EXPOSED: usize = 16;

/// Whether `surface` is one that skipping the fill underneath is safe for — see
/// [`compose_exposed`]'s doc.
///
/// **Two conditions, and the second arrived with alpha.** A surface hides what is under it only
/// if its blit writes every pixel of the intersection *and* those writes do not depend on what
/// was there. A short buffer breaks the first — [`blit_clipped`] skips the pixels it cannot read.
/// An alpha channel breaks the second: a blended pixel reads the destination, so skipping the
/// background fill would blend the window against whatever the framebuffer last held rather than
/// against the background. That is a colour-shifted window instead of a hole — the same class of
/// defect, harder to see and harder to trace.
///
/// The PR #274 review asked for this to be named here before Part B landed, on the grounds that
/// the Part B author would be standing in this function. They were; it was.
fn covers(surface: &SurfaceRef<'_>) -> bool {
    !surface.geometry.format.has_alpha() && surface.pixels.len() >= surface.geometry.byte_len()
}

/// Cut `cut` out of every rectangle in `region`, in place — **or leave `region` untouched**.
///
/// Each surviving piece is one of the up-to-four bands around the removed part.
///
/// **All of the subtraction, or none of it.** The result is written into a scratch array and only
/// installed once every piece has fitted; running out of room returns with `region` as it was.
/// That makes the failure mode an over-fill — the caller paints background a surface is about to
/// cover — which costs the work this function exists to save and cannot be wrong.
///
/// **This was a hole, and the shape of the bug is worth keeping.** The bound used to be enforced
/// two ways that did not agree: an `n + 4 > MAX_EXPOSED` guard before splitting a piece, and a
/// `push` that silently discarded when the array was full. Between them `n` could reach exactly
/// `MAX_EXPOSED`, and every remaining piece was then **dropped** rather than kept — background
/// that nothing fills and no surface covers, so the framebuffer keeps whatever it last held. Five
/// interior windows on a bare desktop were enough (PR #274 review). Two mechanisms guarding one
/// invariant is how an invariant gets lost; there is now one, and it is the array's own capacity.
fn subtract_from(region: &mut [Option<Rect>], cut: &Rect) {
    // The capacity is the region's own length rather than the constant, so a test can run this
    // at every size and pin "any bound is safe" as a property instead of a claim about 16.
    let cap = region.len().min(MAX_EXPOSED);
    let mut out: [Option<Rect>; MAX_EXPOSED] = [None; MAX_EXPOSED];
    let mut n = 0usize;
    // `false` when the piece will not fit, which abandons the subtraction. An empty band is not a
    // piece and is skipped rather than refused — the four bands around a cut are usually not all
    // present, and treating a zero-width one as a failure would over-fill almost every time.
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
        // Above, below, and the two side bands between them — a partition, so no pixel is in
        // two pieces and none is missed.
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

    // **The formats match on every real path, and then a row is a `memcpy`.** A client's surface
    // and the framebuffer both come from the same Limine-reported mode, so `decode` followed by
    // `encode` is a round trip through `Rgb` that lands on the bytes it started from. Doing it a
    // pixel at a time — two `offset_of`s, six bounds checks, an unpack and a repack — is what the
    // compositor actually spends a drag's time on, which is not obvious until it is measured:
    // removing *half the pixel writes* from a frame (`compose_exposed`) bought 6%, because the
    // writes were never the cost. See the M13 Part A entry in `docs/decision-log.md`.
    if src.format.has_alpha() {
        blit_blended(fb, surface, visible);
    } else if src.format == fb.geometry().format {
        blit_rows(fb, surface, visible);
    } else {
        blit_pixels(fb, surface, visible);
    }
}

/// Composite `visible` from a surface that carries its own opacity, a pixel at a time.
///
/// **The slow path, taken only by surfaces that asked for it** (M13 Part B). Blending has to read
/// the destination before writing it, so it cannot be a row copy and cannot be reordered with the
/// surfaces below — which is exactly why the alpha channel is opt-in and `XRGB8888` remains the
/// default. A desktop of ordinary windows never reaches this function.
///
/// [`Framebuffer::blend_pixel`] already handles the three coverage classes, and the two extremes
/// matter here rather than being tidiness: a fully transparent pixel writes nothing, and a fully
/// opaque one is a plain store rather than a read-modify-write. A surface that is mostly one or
/// the other — the usual shape, a translucent panel carrying opaque text — pays the blend only on
/// the pixels that need it.
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

/// Copy `visible` from `surface` to `fb` a pixel at a time, converting each through [`Rgb`].
///
/// The general case, and — since it is the one that handles a format change — the definition of
/// what [`blit_rows`] must agree with. A test runs the two against each other over the arrangements
/// where the row maths could go wrong, which is the only thing standing between the fast path and
/// a silent wrong picture: every caller takes the fast path, so no test of `compose`'s *output*
/// can see this one.
fn blit_pixels<F: Framebuffer + ?Sized>(fb: &mut F, surface: &SurfaceRef<'_>, visible: Rect) {
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

/// Copy `visible` from `surface` to `fb` a row at a time, for the case where the two share a
/// pixel format.
///
/// **The one behaviour worth stating is the short surface**, because it is the same hazard
/// [`compose_exposed`] guards: a client whose buffer is shorter than its geometry claims. The
/// per-pixel path copies the pixels that are there and skips the rest, leaving whatever was
/// underneath, so this clamps each row's copy to the bytes the buffer actually holds and rounds
/// down to a whole pixel. A row that starts past the end copies nothing.
///
/// The padding bits ride along. Where the per-pixel path rebuilds each word through `Rgb` and so
/// writes zero into an `XRGB8888` alpha byte, this copies the source's. Nothing reads those bits
/// — the format's name is what says so — and the picture is identical either way.
fn blit_rows<F: Framebuffer + ?Sized>(fb: &mut F, surface: &SurfaceRef<'_>, visible: Rect) {
    let src = surface.geometry;
    let dst = fb.geometry();
    let bpp = src.format.bytes_per_pixel();
    let src_x = (visible.origin.x - surface.origin.x) as u32;

    for row in 0..visible.size.h {
        let dst_y = visible.origin.y + row as i32;
        let src_y = (dst_y - surface.origin.y) as u32;
        let (Some(soff), Some(doff)) = (
            src.offset_of(src_x, src_y),
            dst.offset_of(visible.origin.x as u32, dst_y as u32),
        ) else {
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

    // ---- compose_exposed (M13 Part A) ----

    /// A framebuffer that counts the **background pixels filled** through it.
    ///
    /// **The saving is a count, not a duration**: a timing here would measure this machine, and
    /// what the change claims is that it fills less background for the same picture.
    ///
    /// It counts fills and `put_pixel`s, which since the row-copy fast path landed means fills
    /// alone — [`blit_rows`] writes through `bytes_mut` and is invisible here. That is the right
    /// quantity anyway: `compose` and `compose_exposed` blit *identically*, and the fill is the
    /// entire difference between them. A counter that also saw the blits would dilute the ratio
    /// with a term the change does not move.
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

    /// The geometry the tests below compose into. Named apart from the module's existing
    /// `screen()`, which hands back a framebuffer rather than its shape.
    fn screen_geom() -> Geometry {
        Geometry::with_pitch(64, 48, 64 * 4, PixelFormat::XRGB8888).unwrap()
    }

    // ---- alpha (M13 Part B) ----

    /// A `w`x`h` `ARGB8888` surface at `(x, y)`, one colour at one opacity.
    fn translucent(
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        c: Rgb,
        alpha: u8,
    ) -> (Geometry, alloc::vec::Vec<u8>, Point) {
        let g = Geometry::with_pitch(w, h, w as usize * 4, PixelFormat::ARGB8888).unwrap();
        let word = g.format.encode_alpha(c, alpha).to_le_bytes();
        let mut px = alloc::vec![0u8; g.pitch * h as usize];
        for p in px.chunks_exact_mut(4) {
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

        // Half of the source over a black background — the same arithmetic `Rgb::blend` does,
        // asserted against `blend` rather than against hand-computed numbers so this pins the
        // *plumbing* (channel positions, which operand is which) and not the mixing rule.
        assert_eq!(fb.get_pixel(5, 5), Some(Rgb::new(200, 100, 50).blend(bg, 128)));
        // Not the source, and not the background: a wrong alpha position would give one of those.
        assert_ne!(fb.get_pixel(5, 5), Some(Rgb::new(200, 100, 50)));
        assert_ne!(fb.get_pixel(5, 5), Some(bg));
        // Outside the surface, background as usual.
        assert_eq!(fb.get_pixel(20, 20), Some(bg));
    }

    #[test]
    fn a_fully_opaque_argb_surface_draws_what_an_xrgb_one_draws() {
        // **The equivalence that catches a misplaced alpha channel.** At alpha 255 the two
        // formats describe the same picture, so any disagreement is plumbing rather than blending
        // — and `blend_pixel`'s 255 arm makes this a plain store, so it also checks that the fast
        // arm of the slow path writes what the fast path would.
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
        // **The trap PR #274's review predicted for this part, made a test.** `compose_exposed`
        // skips the background fill where a surface will cover it — sound only while the blit
        // ignores what was there. A blended pixel does not, so a translucent surface must *not*
        // count as covering: with the fill skipped it would mix with whatever the framebuffer
        // last held, which is a plausible-looking colour rather than an obvious hole.
        //
        // Both framebuffers start holding a previous frame, which is what makes the difference
        // visible at all — on a fresh buffer the stale pixels would be black and the bug could
        // pass for a dark blend.
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
        // **The one line of the alpha path with no negative control** (PR #275 review, optional
        // 5). `blit_blended` mirrors `blit_pixels`' short-buffer skip, and the compositor drops
        // short surfaces before compose ever sees them, so this is defensive — but "defensive"
        // is a claim about a code path, and an untested one reads past a client's buffer if the
        // comparison is ever written the wrong way round.
        let g = screen_geom();
        let bg = Rgb::new(9, 9, 9);
        let ink = Rgb::new(200, 100, 50);
        let (sg, px, o) = translucent(2, 2, 10, 8, ink, 128);

        // Two whole rows and a fragment: enough that some pixels blend and the rest do not.
        let short = &px[..sg.pitch * 2 + 5];
        let mut fb = crate::framebuffer::MemFramebuffer::new(g);
        compose(&mut fb, bg, &[SurfaceRef::new(sg, o, short)], &[g.bounds()]);

        // A pixel the buffer covers blended; one past its end is the background the fill left,
        // not a read past the end and not an untouched pixel.
        assert_eq!(fb.get_pixel(3, 2), Some(ink.blend(bg, 128)));
        assert_eq!(fb.get_pixel(3, 6), Some(bg));
        // The control: with the whole buffer that second pixel *does* blend, so the assertion
        // above is about the truncation rather than about the geometry.
        let mut full = crate::framebuffer::MemFramebuffer::new(g);
        compose(&mut full, bg, &[SurfaceRef::new(sg, o, &px)], &[g.bounds()]);
        assert_eq!(full.get_pixel(3, 6), Some(ink.blend(bg, 128)));
    }

    #[test]
    fn an_opaque_surface_still_covers_and_a_translucent_one_does_not() {
        // `covers` decides whether the fill underneath can be skipped, and it is the whole reason
        // the test above passes. Asserted directly so a change to either condition is visible
        // here rather than only as a picture two functions away.
        let (og, opx, oo) = surface(0, 0, 8, 8, Rgb::new(1, 2, 3));
        assert!(covers(&SurfaceRef::new(og, oo, &opx)));
        let (tg, tpx, to) = translucent(0, 0, 8, 8, Rgb::new(1, 2, 3), 255);
        assert!(!covers(&SurfaceRef::new(tg, to, &tpx)), "opaque *pixels* are not an opaque format");
        assert!(!covers(&SurfaceRef::new(og, oo, &opx[..4])), "a short buffer covers nothing");
    }

    /// `subtract_from` never loses a pixel, at any capacity — the invariant the hole broke.
    ///
    /// **Two directions, and both matter.** A point that was exposed must still be exposed unless
    /// the cut took it: losing one is background nothing paints, which is the stale-pixel hole
    /// that PR #274's review found. And no point may *appear*: the region is what gets filled with
    /// background, so a piece growing past where it started would paint over a window.
    ///
    /// Run at every capacity from 1 up, because the bug lived entirely in the overflow path and
    /// the shipped bound of 16 is generous enough that ordinary arrangements never reach it. A
    /// capacity of 1 can barely represent any cut at all and must still be correct — it simply
    /// declines to subtract, which is the over-fill the design allows.
    #[test]
    fn subtracting_never_loses_a_pixel_at_any_capacity() {
        // Every point of a small grid that the region covers.
        fn covered(region: &[Option<Rect>]) -> alloc::vec::Vec<(i32, i32)> {
            let mut out = alloc::vec::Vec::new();
            for y in 0..24 {
                for x in 0..24 {
                    let inside = |r: &Rect| {
                        x >= r.origin.x
                            && y >= r.origin.y
                            && x < r.right() as i32
                            && y < r.bottom() as i32
                    };
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
                    assert!(
                        after.contains(p) || taken,
                        "cap {cap}: {p:?} was dropped — background nothing will paint"
                    );
                }
                for p in &after {
                    assert!(before.contains(p), "cap {cap}: {p:?} appeared from nowhere");
                }
            }
            // The control: at a generous capacity the subtraction actually happened, so the
            // assertions above were not all trivially satisfied by a region that never changed.
            if cap == MAX_EXPOSED {
                assert!(
                    covered(&region[..cap]).len() < 24 * 24,
                    "nothing was ever subtracted, so neither direction was tested"
                );
            }
        }
    }

    /// Enough surfaces to overflow the exposed region, and the picture is still `compose`'s.
    ///
    /// **The bound has to fail toward over-filling.** `subtract_from` represents the un-covered
    /// background as at most [`MAX_EXPOSED`] rectangles, and cutting one surface out of one
    /// rectangle leaves up to four — so a busy enough arrangement runs out of room. Filling a
    /// rectangle a surface is about to cover anyway costs only the work this function saves;
    /// *dropping* one leaves background that nothing paints and nothing covers, and the
    /// framebuffer keeps whatever it last held there. Those are not two shades of the same
    /// mistake: one is slower, the other is a hole.
    ///
    /// This arrangement is from the PR #274 review, which found the second happening — five
    /// interior surfaces on this module's own screen left a 9x4 block of stale pixels at (55,14).
    /// Both framebuffers start filled with a colour that is neither the background nor any
    /// surface, so a pixel nobody wrote is visible as itself rather than passing for a fill.
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
        // The control the equality needs: `never` must be gone, or two blank screens would agree.
        assert_eq!(plain.get_pixel(55, 14), Some(bg));
        assert_ne!(exposed.get_pixel(55, 14), Some(never), "stale pixels at the reported spot");
    }

    /// The row-copy fast path and the per-pixel reference produce identical bytes.
    ///
    /// **Every caller takes the fast path**, so no test of `compose`'s output can tell the two
    /// apart — this is the only thing between a wrong row calculation and a silently wrong
    /// picture. The arrangements are the ones where that calculation can go wrong: a surface
    /// hanging off each edge (so the copy starts mid-row and mid-surface), one clipped to a
    /// narrow strip, one whose buffer is a byte short of a whole pixel, and one so short that
    /// most rows have nothing to copy at all.
    #[test]
    fn the_row_copy_agrees_with_the_per_pixel_blit() {
        let g = screen_geom();
        let bg = Rgb::new(0x2A, 0x55, 0x70);
        // A gradient rather than a flat colour: a flat surface would survive almost any wrong
        // offset, which is the whole failure mode this test exists to catch.
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
            // A negative control on the case itself: if the blit drew nothing, the comparison
            // above is two identical background fills and proves nothing.
            if damage.intersect(&g.bounds()).is_some() && !pixels.is_empty() {
                let mut plain = crate::framebuffer::MemFramebuffer::new(g);
                plain.fill_rect(g.bounds(), bg);
                assert_ne!(fast.bytes(), plain.bytes(), "{name}: drew nothing to compare");
            }
        }
    }

    /// A `w`x`h` surface at `(x, y)`, every pixel the same colour.
    fn surface(x: i32, y: i32, w: u32, h: u32, c: Rgb) -> (Geometry, alloc::vec::Vec<u8>, Point) {
        let g = Geometry::with_pitch(w, h, w as usize * 4, PixelFormat::XRGB8888).unwrap();
        let word = g.format.encode(c).to_le_bytes();
        let mut px = alloc::vec![0u8; g.pitch * h as usize];
        for p in px.chunks_exact_mut(4) {
            p.copy_from_slice(&word);
        }
        (g, px, Point::new(x, y))
    }

    /// The two produce the same picture, over arrangements that exercise every band of the
    /// subtraction: fully covered, partly covered, untouched, and two overlapping surfaces.
    #[test]
    fn compose_exposed_draws_the_same_picture_as_compose() {
        let g = screen_geom();
        let bg = Rgb::new(0x2A, 0x55, 0x70);
        let (sg, spx, _) = surface(0, 0, 20, 16, Rgb::new(200, 30, 30));
        let (tg, tpx, _) = surface(0, 0, 24, 24, Rgb::new(30, 200, 30));

        for (origin_a, origin_b, damage) in [
            // The surface exactly covers the damage: the fill should vanish entirely.
            (Point::new(4, 4), Point::new(40, 40), alloc::vec![Rect::new(4, 4, 20, 16)]),
            // A drag: where it was, and where it is.
            (Point::new(9, 4), Point::new(40, 40), alloc::vec![
                Rect::new(8, 4, 20, 16),
                Rect::new(9, 4, 20, 16),
            ]),
            // Damage nothing covers.
            (Point::new(4, 4), Point::new(40, 40), alloc::vec![Rect::new(50, 30, 10, 10)]),
            // Two surfaces overlapping each other inside one damage rectangle.
            (Point::new(4, 4), Point::new(14, 10), alloc::vec![Rect::new(0, 0, 40, 40)]),
            // Partly off-screen, so the clip and the subtraction interact.
            (Point::new(-6, -4), Point::new(56, 40), alloc::vec![Rect::new(0, 0, 64, 48)]),
        ] {
            let surfaces = [
                SurfaceRef::new(sg, origin_a, &spx),
                SurfaceRef::new(tg, origin_b, &tpx),
            ];
            let mut a = crate::framebuffer::MemFramebuffer::new(g);
            let mut b = crate::framebuffer::MemFramebuffer::new(g);
            compose(&mut a, bg, &surfaces, &damage);
            compose_exposed(&mut b, bg, &surfaces, &damage);
            assert_eq!(a.bytes(), b.bytes(), "arrangement {origin_a:?}/{origin_b:?} {damage:?}");
        }
    }

    /// And it is less work — which is the whole point, and would otherwise be a refactor.
    #[test]
    fn compose_exposed_fills_far_less_background_for_the_same_picture() {
        let g = screen_geom();
        let bg = Rgb::new(0x2A, 0x55, 0x70);
        let (sg, spx, _) = surface(0, 0, 20, 16, Rgb::new(200, 30, 30));
        // A one-pixel drag, which is the workload the flicker was reported from.
        let damage = [Rect::new(8, 4, 20, 16), Rect::new(9, 4, 20, 16)];
        let surfaces = [SurfaceRef::new(sg, Point::new(9, 4), &spx)];

        let mut plain = Counting::new(g);
        compose(&mut plain, bg, &surfaces, &damage);
        let mut exposed = Counting::new(g);
        compose_exposed(&mut exposed, bg, &surfaces, &damage);

        assert_eq!(plain.inner.bytes(), exposed.inner.bytes(), "same picture");
        let (p, e) = (plain.writes.get(), exposed.writes.get());
        // Plain fills both rectangles whole: 2 x 20 x 16 = 640. Exposed fills only the sliver of
        // the old rectangle the window has moved off — one column, 16 tall. A bound of a twentieth
        // is loose enough to survive a clipping change and tight enough that filling any whole
        // rectangle fails it, which a `e < p` would not.
        assert_eq!(p, 640, "plain should fill both damage rectangles whole");
        assert!(e * 20 < p, "expected a sliver, got {e} filled against {p}");
    }

    /// **A surface whose buffer is short does not count as covering.**
    ///
    /// `blit_clipped` skips the pixels it cannot read, and today the fill underneath is what the
    /// viewer sees. Treating such a surface as opaque would replace that background with whatever
    /// the framebuffer happened to hold — the one case where skipping the fill changes the
    /// picture, and the reason `covers` exists.
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
        // Both start from something that is neither the background nor the surface, so a pixel
        // left untouched is visible as itself rather than as a coincidence.
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
