//! Downscaling, for window thumbnails.
//!
//! One implementation, so a host-side gate links this rather than writing a second downscale (two
//! independent roundings agreeing is a weak claim); the unit tests pin the output against inputs
//! where averaging and sampling disagree.
//!
//! Box average, not nearest: dropping seven pixels in eight turns a terminal's text into aliasing
//! noise that moves with sub-pixel placement. Deterministic in integer arithmetic.

use crate::format::{PixelFormat, Rgb};
use crate::framebuffer::Geometry;
use crate::geom::{Point, Size};

/// Average the `src` rectangle covering one destination pixel, in integers (the sum fits a `u32`).
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
/// Returns `false` if either geometry is unusable (zero dimension, short source, destination
/// larger than the source): refused rather than clamped, as a caller scaling up is confused.
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
        // Bands come from the edges, not a step, so every source row is in exactly one band.
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

/// [`fit`] scales a picture bigger than the screen down to fit, aspect kept, and draws one that
/// fits at its own size, centred; [`fill`] scales down to cover and crops the overhang. Neither
/// scales up (`TODO(wallpaper-fill)`: needs an upscaler and an interpolation decision). A plan
/// rather than a picture, so the arithmetic is testable without a framebuffer.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Fit {
    /// The size to draw the picture at.
    pub size: Size,
    /// Where its top-left corner goes; negative from [`fill`] where it crops ([`place`] clips).
    pub origin: Point,
    /// Whether reaching [`size`](Self::size) needs a downscale, as the caller's paths differ.
    pub scaled: bool,
}

/// Plan where `image` goes on a `screen`; a zero dimension in either gives a zero-sized fit.
pub fn fit(image: Size, screen: Size) -> Fit {
    if image.w == 0 || image.h == 0 || screen.w == 0 || screen.h == 0 {
        return Fit { size: Size::new(0, 0), origin: Point::new(0, 0), scaled: false };
    }
    let size = if image.w <= screen.w && image.h <= screen.h {
        image
    } else {
        // Compared as `w * sh` against `h * sw` so nothing rounds before the decision.
        let by_width = image.w as u64 * screen.h as u64 >= image.h as u64 * screen.w as u64;
        if by_width {
            // The height is at least 1: a zero-height buffer is one `box_downscale` refuses.
            let h = (image.h as u64 * screen.w as u64 / image.w as u64).max(1) as u32;
            Size::new(screen.w, h.min(screen.h))
        } else {
            let w = (image.w as u64 * screen.h as u64 / image.h as u64).max(1) as u32;
            Size::new(w.min(screen.w), screen.h)
        }
    };
    Fit {
        size,
        // An odd leftover puts the extra pixel on the right and bottom.
        origin: Point::new(((screen.w - size.w) / 2) as i32, ((screen.h - size.h) / 2) as i32),
        scaled: size != image,
    }
}

/// Plan `image` to cover a `screen`: the smallest downscale leaving no part uncovered, centred,
/// the overhang cropped by [`place`].
///
/// Scaling down only: covering would take an upscale (`TODO(wallpaper-fill)`), so such a picture
/// is drawn at its own size, centred, cropped or bordered per axis. The overhang rounds up, never
/// past the picture's size; the tests pin the direction.
pub fn fill(image: Size, screen: Size) -> Fit {
    if image.w == 0 || image.h == 0 || screen.w == 0 || screen.h == 0 {
        return Fit { size: Size::new(0, 0), origin: Point::new(0, 0), scaled: false };
    }
    let (iw, ih, sw, sh) = (image.w as u64, image.h as u64, screen.w as u64, screen.h as u64);
    let size = if iw < sw || ih < sh {
        image
    } else if iw * sh <= ih * sw {
        // Width binds and the height overhangs.
        Size::new(screen.w, (ih * sw).div_ceil(iw).min(ih) as u32)
    } else {
        // Height binds and the width overhangs.
        Size::new((iw * sh).div_ceil(ih).min(iw) as u32, screen.h)
    };
    Fit {
        size,
        // Truncating toward zero puts a negative overhang's extra pixel off the left or top.
        origin: Point::new((screen.w as i32 - size.w as i32) / 2, (screen.h as i32 - size.h as i32) / 2),
        scaled: size != image,
    }
}

/// Draw `image` onto `dst` where a [`fit`] or [`fill`] plan says, filling the rest with `ground`
/// and cropping what falls outside.
///
/// The destination pitch is the device's, not `width * 4`. Both geometries must name the same
/// pixel format: the ground is encoded into `dst`'s format and the picture copied word for word,
/// so a mismatch would split one buffer; refused because "every caller is XRGB8888" is a property
/// of today. Downscales internally when the plan says so. Returns `false` if a geometry is
/// unusable, the formats differ, or `dst` is short.
pub fn place(image: &[u8], image_geom: Geometry, plan: Fit, ground: Rgb, dst: &mut [u8], dst_geom: Geometry) -> bool {
    if dst_geom.width == 0 || dst_geom.height == 0 || image_geom.format != dst_geom.format {
        return false;
    }
    if dst.len() < dst_geom.pitch * dst_geom.height as usize {
        return false;
    }
    // The ground first, everywhere: a letterbox leaves bars and the buffer holds stale pixels.
    let word = dst_geom.format.encode(ground).to_le_bytes();
    for y in 0..dst_geom.height as usize {
        let row = &mut dst[y * dst_geom.pitch..y * dst_geom.pitch + dst_geom.width as usize * 4];
        for px in row.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&word);
        }
    }
    if plan.size.w == 0 || plan.size.h == 0 {
        // Nothing to draw; the ground is the answer.
        return true;
    }

    // Materialised only when scaled, so a picture already the right size allocates nothing.
    let scaled;
    let (src, src_geom) = if plan.scaled {
        let Some(g) = Geometry::with_pitch(plan.size.w, plan.size.h, plan.size.w as usize * 4, image_geom.format)
        else {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w`x`h` buffer of one colour.
    fn flat(w: u32, h: u32, c: Rgb) -> (alloc::vec::Vec<u8>, Geometry) {
        let g = Geometry::with_pitch(w, h, w as usize * 4, PixelFormat::XRGB8888).unwrap();
        let word = g.format.encode(c).to_le_bytes();
        let mut v = alloc::vec![0u8; g.pitch * h as usize];
        for px in v.as_chunks_mut::<4>().0 {
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
        assert_eq!(at(&dst, dg, 2, 2), Rgb::new(255, 0, 0));
        assert_eq!(at(&dst, dg, 3, 3), Rgb::new(255, 0, 0));
        assert_eq!(at(&dst, dg, 0, 0), Rgb::new(0, 0, 128));
        assert_eq!(at(&dst, dg, 1, 2), Rgb::new(0, 0, 128));
        assert_eq!(at(&dst, dg, 4, 4), Rgb::new(0, 0, 128));
    }

    #[test]
    fn place_honours_a_destination_pitch_that_is_not_the_width() {
        // The device's pitch, not `width * 4`: assuming otherwise shears the picture diagonally.
        let (img, ig) = flat(2, 2, Rgb::new(0, 255, 0));
        let dg = Geometry::with_pitch(2, 2, 64, PixelFormat::XRGB8888).unwrap();
        let mut dst = alloc::vec![0u8; dg.pitch * 2];
        let plan = fit(Size::new(2, 2), Size::new(2, 2));
        assert!(place(&img, ig, plan, Rgb::new(0, 0, 0), &mut dst, dg));
        assert_eq!(at(&dst, dg, 0, 1), Rgb::new(0, 255, 0), "row 1 is one pitch down");
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
        // The unscaled path had no format check of its own.
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
        let f = fit(Size::new(2560, 1280), Size::new(1280, 800));
        assert_eq!(f.size, Size::new(1280, 640));
        assert_eq!(f.origin, Point::new(0, 80));
        assert!(f.scaled);
    }

    #[test]
    fn a_taller_picture_is_bound_by_height_and_pillarboxed() {
        let f = fit(Size::new(1000, 2000), Size::new(1280, 800));
        assert_eq!(f.size, Size::new(400, 800));
        assert_eq!(f.origin, Point::new(440, 0));
        assert!(f.scaled);
    }

    #[test]
    fn the_aspect_ratio_is_kept_rather_than_stretched() {
        let f = fit(Size::new(4000, 1000), Size::new(1280, 800));
        assert_eq!(f.size.w, 1280);
        assert_eq!(f.size.h, 320, "4:1 stays 4:1");
        assert_ne!(f.size, Size::new(1280, 800), "not stretched to fill");
    }

    #[test]
    fn a_picture_larger_in_one_axis_only_is_still_scaled_to_fit() {
        // A rule firing only when both axes are over would draw this off the bottom.
        let f = fit(Size::new(200, 1600), Size::new(1280, 800));
        assert_eq!(f.size, Size::new(100, 800));
        assert!(f.scaled);
        assert!(f.size.h <= 800 && f.size.w <= 1280, "inside the screen in both axes");
    }

    #[test]
    fn an_extreme_ratio_still_produces_a_buffer_box_downscale_will_take() {
        // 4000 x 3 into 1280 wide is 0.96 rows; a zero-height destination is refused.
        let f = fit(Size::new(4000, 3), Size::new(1280, 800));
        assert!(f.size.w > 0 && f.size.h > 0, "got {:?}", f.size);
        let mut dst = alloc::vec![0u8; (f.size.w * f.size.h * 4) as usize];
        let src = alloc::vec![0u8; 4000 * 3 * 4];
        let sg = Geometry::with_pitch(4000, 3, 4000 * 4, PixelFormat::XRGB8888).unwrap();
        let dg = Geometry::with_pitch(f.size.w, f.size.h, (f.size.w * 4) as usize, PixelFormat::XRGB8888).unwrap();
        assert!(box_downscale(&src, sg, &mut dst, dg), "the planned size is one it accepts");
    }

    #[test]
    fn a_sixteen_by_ten_picture_fills_a_sixteen_by_nine_screen_by_cropping_top_and_bottom() {
        let gate = fill(Size::new(1920, 1200), Size::new(1360, 768));
        assert_eq!(gate, Fit { size: Size::new(1360, 850), origin: Point::new(0, -41), scaled: true });
        let laptop = fill(Size::new(1920, 1200), Size::new(1366, 768));
        assert_eq!(laptop.size, Size::new(1366, 854), "853.75 rounds up; 853 would cover too");
        assert_eq!(laptop.origin, Point::new(0, -43));
    }

    #[test]
    fn a_sixteen_by_nine_picture_fills_a_sixteen_by_ten_screen_by_cropping_the_sides() {
        let f = fill(Size::new(1920, 1080), Size::new(1280, 800));
        assert_eq!(f.size, Size::new(1423, 800), "1422.2 rounds up");
        assert_eq!(f.origin, Point::new(-71, 0));
        assert!(f.scaled);
    }

    #[test]
    fn a_fill_that_would_need_an_upscale_draws_the_picture_at_its_own_size() {
        let big = fill(Size::new(1920, 1200), Size::new(2560, 1440));
        assert_eq!(big, Fit { size: Size::new(1920, 1200), origin: Point::new(320, 120), scaled: false });
        let tall = fill(Size::new(1000, 2000), Size::new(1360, 768));
        assert_eq!(tall, Fit { size: Size::new(1000, 2000), origin: Point::new(180, -616), scaled: false });
        assert_eq!(fill(Size::new(1360, 768), Size::new(1360, 768)).origin, Point::new(0, 0));
    }

    #[test]
    fn every_scaled_fill_covers_the_screen_and_never_grows_the_picture() {
        let pictures = [(1920, 1200), (1920, 1080), (4000, 1000), (1000, 4000), (1367, 769), (3001, 1999)];
        let screens = [(1024, 768), (1280, 800), (1360, 768), (1366, 768), (1920, 1080), (7, 5)];
        for (iw, ih) in pictures {
            for (sw, sh) in screens {
                let f = fill(Size::new(iw, ih), Size::new(sw, sh));
                assert!(f.size.w <= iw && f.size.h <= ih, "{iw}x{ih} on {sw}x{sh}: grew to {:?}", f.size);
                if f.scaled {
                    let (x, y) = (f.origin.x, f.origin.y);
                    assert!(x <= 0 && y <= 0, "{iw}x{ih} on {sw}x{sh}: origin {x},{y} leaves a margin");
                    assert!(x + f.size.w as i32 >= sw as i32, "{iw}x{ih} on {sw}x{sh}: a bare column");
                    assert!(y + f.size.h as i32 >= sh as i32, "{iw}x{ih} on {sw}x{sh}: a bare row");
                }
            }
        }
    }

    #[test]
    fn place_crops_a_fill_plan_to_the_screen() {
        let ig = Geometry::with_pitch(4, 2, 16, PixelFormat::XRGB8888).unwrap();
        let cols = [Rgb::new(10, 0, 0), Rgb::new(20, 0, 0), Rgb::new(30, 0, 0), Rgb::new(40, 0, 0)];
        let img = packed(ig, |x, _| cols[x as usize]);
        let dg = Geometry::with_pitch(2, 2, 12, PixelFormat::XRGB8888).unwrap();
        let mut dst = alloc::vec![0xEEu8; dg.pitch * 2];
        let plan = fill(Size::new(4, 2), Size::new(2, 2));
        assert_eq!(plan, Fit { size: Size::new(4, 2), origin: Point::new(-1, 0), scaled: false });
        assert!(place(&img, ig, plan, Rgb::new(0, 0, 0), &mut dst, dg));
        assert_eq!((at(&dst, dg, 0, 0), at(&dst, dg, 1, 1)), (cols[1], cols[2]));
        assert_eq!(&dst[8..12], &[0xEE; 4], "the destination's row padding is untouched");
    }

    #[test]
    fn a_zero_dimension_is_answered_rather_than_panicking() {
        assert_eq!(fit(Size::new(0, 100), Size::new(1280, 800)).size, Size::new(0, 0));
        assert_eq!(fit(Size::new(100, 100), Size::new(0, 800)).size, Size::new(0, 0));
        assert_eq!(fill(Size::new(0, 100), Size::new(1280, 800)).size, Size::new(0, 0));
        assert_eq!(fill(Size::new(100, 100), Size::new(1280, 0)).size, Size::new(0, 0));
    }

    fn geom(w: u32, h: u32) -> Geometry {
        Geometry::packed(w, h, PixelFormat::XRGB8888)
    }

    #[test]
    fn a_flat_source_downscales_to_the_same_colour() {
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
        // Black, black, white into one: the mean is 85, point sampling gives 0 or 255.
        let sg = geom(3, 1);
        let src = packed(sg, |x, _| if x == 2 { Rgb::new(255, 255, 255) } else { Rgb::new(0, 0, 0) });
        let dg = geom(1, 1);
        let mut dst = alloc::vec![0u8; dg.byte_len()];
        assert!(box_downscale(&src, sg, &mut dst, dg));
        assert_eq!(at(&dst, dg, 0, 0), Rgb::new(85, 85, 85), "nearest-neighbour would give 0 or 255");
    }

    #[test]
    fn the_last_source_row_reaches_the_thumbnail() {
        // Bands derive from edges, not a step: with `sh / dh` the last rows (a terminal's newest
        // output) fall outside every band. The source is black but for a white last row.
        let sg = geom(4, 5);
        let src = packed(sg, |_, y| if y == 4 { Rgb::new(255, 255, 255) } else { Rgb::new(0, 0, 0) });
        let dg = geom(2, 2);
        let mut dst = alloc::vec![0u8; dg.byte_len()];
        assert!(box_downscale(&src, sg, &mut dst, dg));
        let bottom = at(&dst, dg, 0, 1);
        assert_ne!(bottom, Rgb::new(0, 0, 0), "the last source row never reached the thumbnail");
        assert_eq!(bottom, Rgb::new(85, 85, 85));
        assert_eq!(at(&dst, dg, 0, 0), Rgb::new(0, 0, 0), "and the top band is all black");
    }

    #[test]
    fn an_uneven_split_covers_every_column_too() {
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
