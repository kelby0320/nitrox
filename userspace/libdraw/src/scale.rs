//! Downscaling, for window thumbnails.
//!
//! **One implementation, and it is specified so that a second end can be built against it.**
//! The compositor scales a window into the shell's buffer. A host-side gate that wanted to say
//! what that buffer should contain would link this rather than write its own — because a
//! comparison between two independent downscales is checking that two roundings agree, which is
//! a weaker claim than it looks and fails for reasons nobody can act on.
//!
//! **No such gate exists today, and the first version of this doc said one did.** Nothing under
//! `tools/` links this module: the shell's buffer never leaves the guest, and the only gate that
//! compares pixels boots an image with no shell in it. What pins the output is the unit tests
//! below, which is why they are written against inputs where averaging and sampling disagree
//! rather than against a re-derivation of the arithmetic (PR #244 review, blocking 1 and 2).
//!
//! **Box average, not nearest.** A thumbnail is where a terminal's text becomes a texture, and
//! dropping seven pixels in eight turns that into aliasing noise that changes with sub-pixel
//! placement — which is exactly the kind of thing a screen comparison then reports as a
//! mismatch. Averaging costs one pass over the source, once per window per overview open
//! (`desktop-shell.md` §6), and it is deterministic in integer arithmetic.

use crate::format::{PixelFormat, Rgb};
use crate::framebuffer::Geometry;
use crate::geom::{Point, Size};

/// Average the `src` rectangle covering one destination pixel.
///
/// Integer arithmetic throughout: the sum of at most `sw * sh` bytes per channel fits a `u32`
/// for any source this can be handed, and the division truncates the same way on every target.
fn average(src: &[u8], src_geom: Geometry, x0: u32, y0: u32, x1: u32, y1: u32) -> Rgb {
    let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
    for y in y0..y1 {
        for x in x0..x1 {
            let off = y as usize * src_geom.pitch + x as usize * 4;
            if off + 4 > src.len() {
                continue;
            }
            let word = u32::from_le_bytes([src[off], src[off + 1], src[off + 2], src[off + 3]]);
            let c = src_geom.format.decode(word);
            r += c.r as u32;
            g += c.g as u32;
            b += c.b as u32;
            n += 1;
        }
    }
    if n == 0 {
        return Rgb::new(0, 0, 0);
    }
    Rgb::new((r / n) as u8, (g / n) as u8, (b / n) as u8)
}

/// Box-downscale `src` into `dst`, both XRGB8888.
///
/// Returns `false` if either geometry is unusable — a zero dimension, a source shorter than its
/// own geometry claims, or a destination larger than the source in either axis. **Refused
/// rather than clamped**: a caller asking to scale *up* has misunderstood what this is for, and
/// silently returning the source at a different size would look like it worked.
pub fn box_downscale(src: &[u8], src_geom: Geometry, dst: &mut [u8], dst_geom: Geometry) -> bool {
    if src_geom.width == 0
        || src_geom.height == 0
        || dst_geom.width == 0
        || dst_geom.height == 0
        || dst_geom.width > src_geom.width
        || dst_geom.height > src_geom.height
        || src.len() < src_geom.byte_len()
        || dst.len() < dst_geom.byte_len()
        || src_geom.format != PixelFormat::XRGB8888
        || dst_geom.format != PixelFormat::XRGB8888
    {
        return false;
    }
    for dy in 0..dst_geom.height {
        // The source band this destination row covers. Computed from the *edges* rather than
        // from a step, so every source row belongs to exactly one band and none is skipped.
        let y0 = dy * src_geom.height / dst_geom.height;
        let y1 = ((dy + 1) * src_geom.height / dst_geom.height).max(y0 + 1);
        for dx in 0..dst_geom.width {
            let x0 = dx * src_geom.width / dst_geom.width;
            let x1 = ((dx + 1) * src_geom.width / dst_geom.width).max(x0 + 1);
            let c = average(src, src_geom, x0, y0, x1, y1);
            let off = dy as usize * dst_geom.pitch + dx as usize * 4;
            let word = dst_geom.format.encode(c).to_le_bytes();
            dst[off..off + 4].copy_from_slice(&word);
        }
    }
    true
}

/// Where a picture goes on a screen, and at what size.
///
/// **Fit if larger, centre if smaller** — M12 decision 7, and the whole of it. A picture bigger
/// than the screen is scaled down to fit inside it with its aspect ratio kept; one that already
/// fits is drawn at its own size, centred. Scaling *up* to fill is deferred as a **mode** rather
/// than left out — `TODO(wallpaper-fill)` — because it needs an upscaler and a decision about
/// interpolation, and the maintainer wants both eventually.
///
/// **A plan rather than a picture**, so the arithmetic is testable without a framebuffer. The
/// caller downscales if [`scaled`](Self::scaled) and blits at [`origin`](Self::origin).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Fit {
    /// The size to draw the picture at.
    pub size: Size,
    /// Where its top-left corner goes.
    ///
    /// Never negative: a picture is only ever drawn at a size that fits, so it is inset rather
    /// than cropped. A caller does not have to clip.
    pub origin: Point,
    /// Whether reaching [`size`](Self::size) needs a downscale.
    ///
    /// **A flag rather than "`size != image`"**, because the caller's two paths differ by more
    /// than a comparison: one allocates a second buffer and runs [`box_downscale`], the other
    /// blits the decoded pixels straight through.
    pub scaled: bool,
}

/// Plan where `image` goes on a `screen`.
///
/// A zero dimension in either produces a zero-sized fit at the origin — total rather than
/// panicking, because the image's size comes from a file's header and the screen's from a
/// device.
pub fn fit(image: Size, screen: Size) -> Fit {
    if image.w == 0 || image.h == 0 || screen.w == 0 || screen.h == 0 {
        return Fit { size: Size::new(0, 0), origin: Point::new(0, 0), scaled: false };
    }
    let size = if image.w <= screen.w && image.h <= screen.h {
        image
    } else {
        // **The tighter of the two ratios, in integer arithmetic.** `w * sh` against `h * sw`
        // compares `w/h` with `sw/sh` without dividing, so there is no rounding before the
        // decision — and both products fit a `u64` for any image `MAX_PIXELS` admits.
        let by_width = image.w as u64 * screen.h as u64 >= image.h as u64 * screen.w as u64;
        if by_width {
            // Width is the binding axis. The height is rounded to at least 1: a picture 4000
            // wide and 3 tall scaled to a 1280 screen is 0.96 rows, and a zero-height buffer is
            // one `box_downscale` refuses.
            let h = (image.h as u64 * screen.w as u64 / image.w as u64).max(1) as u32;
            Size::new(screen.w, h.min(screen.h))
        } else {
            let w = (image.w as u64 * screen.h as u64 / image.h as u64).max(1) as u32;
            Size::new(w.min(screen.w), screen.h)
        }
    };
    Fit {
        size,
        // Integer division truncates, so an odd leftover puts the extra pixel on the right and
        // bottom. Consistent, and half a pixel is not a placement anybody can see.
        origin: Point::new(
            ((screen.w - size.w) / 2) as i32,
            ((screen.h - size.h) / 2) as i32,
        ),
        scaled: size != image,
    }
}

/// Draw `image` onto `dst` where [`fit`] says, filling the rest with `ground`.
///
/// **The whole of "put a wallpaper on a screen", so the arithmetic is here rather than in the
/// shell.** Two things in it are easy to get wrong and invisible when they are — the destination
/// pitch, which is the device's and not `width * 4`, and the origin, which is where a letterbox
/// comes from. Both are testable without a framebuffer, and neither is worth discovering by
/// booting.
///
/// **Both geometries must name the same pixel format.** The ground is *encoded* into `dst`'s
/// format and the picture is copied word for word, so a mismatch would put the two halves of one
/// buffer in different formats — invisible in the arithmetic and unmistakable on a screen. The
/// scaled path was safe by accident, because [`box_downscale`] refuses anything but XRGB8888 on
/// both sides; the unscaled path had no check at all (PR #272 review, optional 5). Refused here
/// rather than documented, because "every caller today is XRGB8888" is a property of today.
///
/// Downscales internally when the plan says so, allocating the intermediate. Returns `false` if
/// a geometry is unusable, the formats differ, or `dst` is shorter than its own geometry claims
/// — the same refusal [`box_downscale`] makes, and for the same reason.
pub fn place(
    image: &[u8],
    image_geom: Geometry,
    plan: Fit,
    ground: Rgb,
    dst: &mut [u8],
    dst_geom: Geometry,
) -> bool {
    if dst_geom.width == 0 || dst_geom.height == 0 || image_geom.format != dst_geom.format {
        return false;
    }
    if dst.len() < dst_geom.pitch * dst_geom.height as usize {
        return false;
    }
    // The ground first, everywhere. A letterboxed picture leaves bars, and a buffer the
    // compositor hands back holds whatever was in it last.
    let word = dst_geom.format.encode(ground).to_le_bytes();
    for y in 0..dst_geom.height as usize {
        let row = &mut dst[y * dst_geom.pitch..y * dst_geom.pitch + dst_geom.width as usize * 4];
        for px in row.chunks_exact_mut(4) {
            px.copy_from_slice(&word);
        }
    }
    if plan.size.w == 0 || plan.size.h == 0 {
        // Nothing to draw, and the ground is drawn — which is the right answer for a picture
        // that planned to nothing rather than a reason to report failure.
        return true;
    }

    // **The scaled copy is materialised only when it is needed.** A picture already the right
    // size is blitted from the caller's buffer, so the ordinary "wallpaper matches the screen"
    // case allocates nothing.
    let scaled;
    let (src, src_geom) = if plan.scaled {
        let Some(g) = Geometry::with_pitch(
            plan.size.w,
            plan.size.h,
            plan.size.w as usize * 4,
            image_geom.format,
        ) else {
            return false;
        };
        let mut buf = alloc::vec![0u8; g.pitch * g.height as usize];
        if !box_downscale(image, image_geom, &mut buf, g) {
            return false;
        }
        scaled = buf;
        (&scaled[..], g)
    } else {
        (image, image_geom)
    };

    for y in 0..plan.size.h as usize {
        let dy = plan.origin.y + y as i32;
        if dy < 0 || dy >= dst_geom.height as i32 {
            continue;
        }
        for x in 0..plan.size.w as usize {
            let dx = plan.origin.x + x as i32;
            if dx < 0 || dx >= dst_geom.width as i32 {
                continue;
            }
            let so = y * src_geom.pitch + x * 4;
            let dof = dy as usize * dst_geom.pitch + dx as usize * 4;
            if so + 4 > src.len() || dof + 4 > dst.len() {
                continue;
            }
            dst[dof..dof + 4].copy_from_slice(&src[so..so + 4]);
        }
    }
    true
}

/// Draw `src` onto `dst` with black composited over it at `coverage`.
///
/// **The overlay a system with no alpha channel can have.** `desktop-shell`'s overview is a
/// full-screen *opaque* window — it does not sit over the desktop, it replaces it — so reading as
/// an overlay means drawing what is behind it, darkened. [`Rgb::blend`] is the primitive, and
/// nothing gains a stored channel.
///
/// **Here rather than in the shell**, which is where it was written and is the same argument
/// [`place`] carries: the destination pitch and the source pitch are exactly the arithmetic that
/// is invisible when wrong and expensive to discover by booting, and `tools/CLAUDE.md` asks for a
/// host test wherever the answer does not need a guest (PR #273 review, optional 5).
///
/// Both geometries must name the same size and format; returns `false` otherwise, or if either
/// buffer is shorter than its geometry claims.
pub fn dim(src: &[u8], src_geom: Geometry, coverage: u8, dst: &mut [u8], dst_geom: Geometry) -> bool {
    if src_geom.width != dst_geom.width
        || src_geom.height != dst_geom.height
        || src_geom.format != dst_geom.format
        || src_geom.width == 0
        || src_geom.height == 0
    {
        return false;
    }
    if src.len() < src_geom.pitch * src_geom.height as usize
        || dst.len() < dst_geom.pitch * dst_geom.height as usize
    {
        return false;
    }
    let black = Rgb::new(0, 0, 0);
    for y in 0..src_geom.height as usize {
        for x in 0..src_geom.width as usize {
            let so = y * src_geom.pitch + x * 4;
            let word = u32::from_le_bytes([src[so], src[so + 1], src[so + 2], src[so + 3]]);
            let under = src_geom.format.decode(word);
            let out = dst_geom.format.encode(black.blend(under, coverage)).to_le_bytes();
            let dof = y * dst_geom.pitch + x * 4;
            dst[dof..dof + 4].copy_from_slice(&out);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- fit or centre (M12 Part F) ----

    // ---- dimming an overlay ----

    #[test]
    fn dim_darkens_every_pixel_towards_black() {
        let (src, sg) = flat(2, 2, Rgb::new(200, 100, 50));
        let dg = sg;
        let mut dst = alloc::vec![0u8; dg.pitch * 2];
        assert!(dim(&src, sg, 128, &mut dst, dg));
        // Half coverage of black: each channel roughly halves, rounded to nearest.
        let c = at(&dst, dg, 0, 0);
        assert_eq!(c, Rgb::new(100, 50, 25), "got {c:?}");
        assert_eq!(at(&dst, dg, 1, 1), c, "every pixel, not only the first");
    }

    #[test]
    fn dim_at_zero_is_the_picture_and_at_full_is_black() {
        // The endpoints, because a coverage that is silently inverted looks plausible at 128 and
        // is exactly wrong at both ends.
        let (src, sg) = flat(2, 1, Rgb::new(10, 200, 30));
        let mut dst = alloc::vec![0u8; sg.pitch];
        assert!(dim(&src, sg, 0, &mut dst, sg));
        assert_eq!(at(&dst, sg, 0, 0), Rgb::new(10, 200, 30), "0 is invisible black");
        assert!(dim(&src, sg, 255, &mut dst, sg));
        assert_eq!(at(&dst, sg, 0, 0), Rgb::new(0, 0, 0), "255 is opaque black");
    }

    #[test]
    fn dim_honours_pitches_that_are_not_the_width() {
        // Both of them, independently: a source read at the wrong stride shears the picture and a
        // destination written at the wrong one shears the result, and neither is visible in the
        // arithmetic.
        let sg = Geometry::with_pitch(2, 2, 32, PixelFormat::XRGB8888).unwrap();
        let mut src = alloc::vec![0u8; sg.pitch * 2];
        let word = sg.format.encode(Rgb::new(80, 80, 80)).to_le_bytes();
        for y in 0..2usize {
            for x in 0..2usize {
                let o = y * sg.pitch + x * 4;
                src[o..o + 4].copy_from_slice(&word);
            }
        }
        let dg = Geometry::with_pitch(2, 2, 64, PixelFormat::XRGB8888).unwrap();
        let mut dst = alloc::vec![0u8; dg.pitch * 2];
        assert!(dim(&src, sg, 128, &mut dst, dg));
        assert_eq!(at(&dst, dg, 1, 1), Rgb::new(40, 40, 40), "row 1 is one destination pitch down");
        // The padding between destination rows is untouched.
        assert_eq!(&dst[8..64], &alloc::vec![0u8; 56][..]);
    }

    #[test]
    fn dim_refuses_mismatched_geometries_and_short_buffers() {
        let (src, sg) = flat(2, 2, Rgb::new(1, 2, 3));
        let mut dst = alloc::vec![0u8; sg.pitch * 2];
        let other = Geometry::with_pitch(4, 2, 16, PixelFormat::XRGB8888).unwrap();
        assert!(!dim(&src, sg, 128, &mut dst, other), "a different size");
        let bgr = Geometry::with_pitch(2, 2, 8, PixelFormat::XBGR8888).unwrap();
        assert!(!dim(&src, sg, 128, &mut dst, bgr), "a different format");
        let mut short = alloc::vec![0u8; 4];
        assert!(!dim(&src, sg, 128, &mut short, sg), "a destination shorter than it claims");
        assert!(!dim(&src[..4], sg, 128, &mut dst, sg), "a source shorter than it claims");
    }


    /// A `w`×`h` image whose every pixel is `c`.
    fn flat(w: u32, h: u32, c: Rgb) -> (alloc::vec::Vec<u8>, Geometry) {
        let g = Geometry::with_pitch(w, h, w as usize * 4, PixelFormat::XRGB8888).unwrap();
        let word = g.format.encode(c).to_le_bytes();
        let mut v = alloc::vec![0u8; g.pitch * h as usize];
        for px in v.chunks_exact_mut(4) {
            px.copy_from_slice(&word);
        }
        (v, g)
    }

    #[test]
    fn place_centres_a_small_picture_and_grounds_the_rest() {
        let (img, ig) = flat(2, 2, Rgb::new(255, 0, 0));
        let dg = Geometry::with_pitch(6, 6, 6 * 4, PixelFormat::XRGB8888).unwrap();
        let mut dst = alloc::vec![0u8; dg.pitch * 6];
        let plan = fit(Size::new(2, 2), Size::new(6, 6));
        assert!(place(&img, ig, plan, Rgb::new(0, 0, 128), &mut dst, dg));
        // The picture lands at (2, 2).
        assert_eq!(at(&dst, dg, 2, 2), Rgb::new(255, 0, 0));
        assert_eq!(at(&dst, dg, 3, 3), Rgb::new(255, 0, 0));
        // …and everything outside it is the ground.
        assert_eq!(at(&dst, dg, 0, 0), Rgb::new(0, 0, 128));
        assert_eq!(at(&dst, dg, 1, 2), Rgb::new(0, 0, 128));
        assert_eq!(at(&dst, dg, 4, 4), Rgb::new(0, 0, 128));
    }

    #[test]
    fn place_honours_a_destination_pitch_that_is_not_the_width() {
        // **The device's pitch, not `width * 4`.** A framebuffer's rows are padded, and a blit
        // that assumed otherwise shears the picture diagonally — the classic display bug, and
        // the one `check-display` exists to catch a boot later than this does.
        let (img, ig) = flat(2, 2, Rgb::new(0, 255, 0));
        let dg = Geometry::with_pitch(2, 2, 64, PixelFormat::XRGB8888).unwrap();
        let mut dst = alloc::vec![0u8; dg.pitch * 2];
        let plan = fit(Size::new(2, 2), Size::new(2, 2));
        assert!(place(&img, ig, plan, Rgb::new(0, 0, 0), &mut dst, dg));
        assert_eq!(at(&dst, dg, 0, 1), Rgb::new(0, 255, 0), "row 1 is one pitch down");
        // The padding between rows is untouched, which is what a pitch is for.
        assert_eq!(&dst[8..64], &alloc::vec![0u8; 56][..]);
    }

    #[test]
    fn place_downscales_a_larger_picture_and_letterboxes_it() {
        let (img, ig) = flat(8, 4, Rgb::new(200, 100, 50));
        let dg = Geometry::with_pitch(4, 4, 16, PixelFormat::XRGB8888).unwrap();
        let mut dst = alloc::vec![0u8; dg.pitch * 4];
        let plan = fit(Size::new(8, 4), Size::new(4, 4));
        assert_eq!(plan.size, Size::new(4, 2));
        assert_eq!(plan.origin, Point::new(0, 1));
        assert!(place(&img, ig, plan, Rgb::new(0, 0, 0), &mut dst, dg));
        assert_eq!(at(&dst, dg, 0, 0), Rgb::new(0, 0, 0), "the letterbox above");
        assert_eq!(at(&dst, dg, 0, 1), Rgb::new(200, 100, 50), "the picture");
        assert_eq!(at(&dst, dg, 3, 2), Rgb::new(200, 100, 50));
        assert_eq!(at(&dst, dg, 0, 3), Rgb::new(0, 0, 0), "and below");
    }

    #[test]
    fn place_refuses_two_geometries_in_different_formats() {
        // The ground is encoded into `dst`'s format and the picture is copied word for word, so
        // a mismatch puts the two halves of one buffer in different formats. The scaled path was
        // safe only because `box_downscale` refuses anything but XRGB8888; the unscaled path —
        // a picture already the right size, which is the ordinary case — had no check.
        let (img, ig) = flat(2, 2, Rgb::new(1, 2, 3));
        let dg = Geometry::with_pitch(2, 2, 8, PixelFormat::XBGR8888).unwrap();
        let mut dst = alloc::vec![0u8; dg.pitch * 2];
        let plan = fit(Size::new(2, 2), Size::new(2, 2));
        assert!(!plan.scaled, "the unscaled path is the one with no other check");
        assert!(!place(&img, ig, plan, Rgb::new(0, 0, 0), &mut dst, dg));
    }

    #[test]
    fn place_refuses_a_destination_shorter_than_its_geometry() {
        let (img, ig) = flat(2, 2, Rgb::new(1, 2, 3));
        let dg = Geometry::with_pitch(4, 4, 16, PixelFormat::XRGB8888).unwrap();
        let mut dst = alloc::vec![0u8; 8];
        assert!(!place(&img, ig, fit(Size::new(2, 2), Size::new(4, 4)), Rgb::new(0, 0, 0), &mut dst, dg));
    }


    #[test]
    fn a_picture_smaller_than_the_screen_is_centred_at_its_own_size() {
        let f = fit(Size::new(400, 300), Size::new(1280, 800));
        assert_eq!(f.size, Size::new(400, 300));
        assert_eq!(f.origin, Point::new(440, 250));
        assert!(!f.scaled, "nothing to scale");
    }

    #[test]
    fn a_picture_the_size_of_the_screen_is_neither_scaled_nor_moved() {
        let f = fit(Size::new(1280, 800), Size::new(1280, 800));
        assert_eq!(f, Fit { size: Size::new(1280, 800), origin: Point::new(0, 0), scaled: false });
    }

    #[test]
    fn a_wider_picture_is_bound_by_width_and_letterboxed() {
        // 2:1 into 16:10 — width is the tighter axis, so the height comes out short and the
        // leftover is split top and bottom.
        let f = fit(Size::new(2560, 1280), Size::new(1280, 800));
        assert_eq!(f.size, Size::new(1280, 640));
        assert_eq!(f.origin, Point::new(0, 80));
        assert!(f.scaled);
    }

    #[test]
    fn a_taller_picture_is_bound_by_height_and_pillarboxed() {
        // 1:2 into 16:10 — height binds, and the leftover is split left and right.
        let f = fit(Size::new(1000, 2000), Size::new(1280, 800));
        assert_eq!(f.size, Size::new(400, 800));
        assert_eq!(f.origin, Point::new(440, 0));
        assert!(f.scaled);
    }

    #[test]
    fn the_aspect_ratio_is_kept_rather_than_stretched() {
        // **The point of fitting rather than filling.** Stretching to the screen is one line
        // shorter and makes every face on a wallpaper the wrong shape.
        let f = fit(Size::new(4000, 1000), Size::new(1280, 800));
        assert_eq!(f.size.w, 1280);
        assert_eq!(f.size.h, 320, "4:1 stays 4:1");
        assert_ne!(f.size, Size::new(1280, 800), "not stretched to fill");
    }

    #[test]
    fn a_picture_larger_in_one_axis_only_is_still_scaled_to_fit() {
        // Taller than the screen but narrower. A rule that only fired when *both* axes were
        // over would draw this one off the bottom.
        let f = fit(Size::new(200, 1600), Size::new(1280, 800));
        assert_eq!(f.size, Size::new(100, 800));
        assert!(f.scaled);
        assert!(f.size.h <= 800 && f.size.w <= 1280, "inside the screen in both axes");
    }

    #[test]
    fn an_extreme_ratio_still_produces_a_buffer_box_downscale_will_take() {
        // 4000 x 3 into 1280 wide is 0.96 rows. A zero-height destination is one
        // `box_downscale` refuses, so the fit would be a wallpaper that silently did not
        // appear.
        let f = fit(Size::new(4000, 3), Size::new(1280, 800));
        assert!(f.size.w > 0 && f.size.h > 0, "got {:?}", f.size);
        let mut dst = alloc::vec![0u8; (f.size.w * f.size.h * 4) as usize];
        let src = alloc::vec![0u8; 4000 * 3 * 4];
        let sg = Geometry::with_pitch(4000, 3, 4000 * 4, PixelFormat::XRGB8888).unwrap();
        let dg = Geometry::with_pitch(f.size.w, f.size.h, (f.size.w * 4) as usize, PixelFormat::XRGB8888)
            .unwrap();
        assert!(box_downscale(&src, sg, &mut dst, dg), "the planned size is one it accepts");
    }

    #[test]
    fn a_zero_dimension_is_answered_rather_than_panicking() {
        // The image's size comes from a file's header and the screen's from a device.
        assert_eq!(fit(Size::new(0, 100), Size::new(1280, 800)).size, Size::new(0, 0));
        assert_eq!(fit(Size::new(100, 100), Size::new(0, 800)).size, Size::new(0, 0));
    }

    fn geom(w: u32, h: u32) -> Geometry {
        Geometry::packed(w, h, PixelFormat::XRGB8888)
    }

    #[test]
    fn a_flat_source_downscales_to_the_same_colour() {
        // The property that makes a thumbnail recognisable: averaging equal pixels cannot
        // introduce a colour that was not there.
        let g = geom(8, 8);
        let mut src = alloc::vec![0u8; g.byte_len()];
        for y in 0..8 {
            for x in 0..8 {
                let off = y * g.pitch + x * 4;
                src[off..off + 4].copy_from_slice(&g.format.encode(Rgb::new(9, 40, 200)).to_le_bytes());
            }
        }
        let dg = geom(2, 2);
        let mut dst = alloc::vec![0u8; dg.byte_len()];
        assert!(box_downscale(&src, g, &mut dst, dg));
        for i in 0..4 {
            let off = (i / 2) * dg.pitch + (i % 2) * 4;
            let word = u32::from_le_bytes([dst[off], dst[off + 1], dst[off + 2], dst[off + 3]]);
            assert_eq!(dg.format.decode(word), Rgb::new(9, 40, 200), "pixel {i}");
        }
    }

    /// Fill `g` from a per-pixel closure, and return the packed bytes.
    fn packed(g: Geometry, f: impl Fn(u32, u32) -> Rgb) -> alloc::vec::Vec<u8> {
        let mut v = alloc::vec![0u8; g.byte_len()];
        for y in 0..g.height {
            for x in 0..g.width {
                let off = y as usize * g.pitch + x as usize * 4;
                v[off..off + 4].copy_from_slice(&g.format.encode(f(x, y)).to_le_bytes());
            }
        }
        v
    }

    /// Read destination pixel `(x, y)`.
    fn at(dst: &[u8], g: Geometry, x: u32, y: u32) -> Rgb {
        let off = y as usize * g.pitch + x as usize * 4;
        g.format.decode(u32::from_le_bytes([dst[off], dst[off + 1], dst[off + 2], dst[off + 3]]))
    }

    #[test]
    fn it_averages_rather_than_sampling() {
        // **Chosen so the two answers differ.** Three source pixels — black, black, white —
        // into one: the mean is 85 and any form of point sampling gives 0 or 255. A uniform
        // source cannot tell these apart, which is why the first version of this suite could
        // not: averaging equal pixels and picking one of them are the same answer.
        let sg = geom(3, 1);
        let src = packed(sg, |x, _| if x == 2 { Rgb::new(255, 255, 255) } else { Rgb::new(0, 0, 0) });
        let dg = geom(1, 1);
        let mut dst = alloc::vec![0u8; dg.byte_len()];
        assert!(box_downscale(&src, sg, &mut dst, dg));
        assert_eq!(at(&dst, dg, 0, 0), Rgb::new(85, 85, 85), "nearest-neighbour would give 0 or 255");
    }

    #[test]
    fn the_last_source_row_reaches_the_thumbnail() {
        // **The bands are derived from edges, not from a step**, and this is what that buys.
        // With a step of `sh / dh` the last rows fall outside every band — in a terminal, the
        // most recent output. Five rows into two does not divide, so a step would cover rows
        // 0..1 and 2..3 and drop row 4 entirely.
        //
        // The source is black except for its **last** row, which is white. If the last row is
        // dropped the bottom destination pixel is pure black; averaged, it is not.
        let sg = geom(4, 5);
        let src = packed(sg, |_, y| if y == 4 { Rgb::new(255, 255, 255) } else { Rgb::new(0, 0, 0) });
        let dg = geom(2, 2);
        let mut dst = alloc::vec![0u8; dg.byte_len()];
        assert!(box_downscale(&src, sg, &mut dst, dg));
        let bottom = at(&dst, dg, 0, 1);
        assert_ne!(bottom, Rgb::new(0, 0, 0), "the last source row never reached the thumbnail");
        // Rows 2,3,4 average to (0 + 0 + 255) / 3 = 85.
        assert_eq!(bottom, Rgb::new(85, 85, 85));
        assert_eq!(at(&dst, dg, 0, 0), Rgb::new(0, 0, 0), "and the top band is all black");
    }

    #[test]
    fn an_uneven_split_covers_every_column_too() {
        // The same claim on the other axis, and with a width that does not divide: seven
        // columns into three. Column 6 is the one a step would drop.
        let sg = geom(7, 1);
        let src = packed(sg, |x, _| if x == 6 { Rgb::new(0, 0, 240) } else { Rgb::new(0, 0, 0) });
        let dg = geom(3, 1);
        let mut dst = alloc::vec![0u8; dg.byte_len()];
        assert!(box_downscale(&src, sg, &mut dst, dg));
        assert_ne!(at(&dst, dg, 2, 0), Rgb::new(0, 0, 0), "the last source column was dropped");
    }

    #[test]
    fn scaling_up_is_refused_rather_than_clamped() {
        let g = geom(4, 4);
        let src = alloc::vec![0u8; g.byte_len()];
        let dg = geom(8, 8);
        let mut dst = alloc::vec![0u8; dg.byte_len()];
        assert!(!box_downscale(&src, g, &mut dst, dg), "a caller asking to scale up is confused");
    }

    #[test]
    fn a_short_destination_is_refused_rather_than_written_partially() {
        let g = geom(8, 8);
        let src = alloc::vec![0u8; g.byte_len()];
        let dg = geom(4, 4);
        let mut dst = alloc::vec![0u8; dg.byte_len() - 1];
        assert!(!box_downscale(&src, g, &mut dst, dg));
    }

}
