//! Rounded corners — how much of a pixel near a corner a rounded shape covers.
//!
//! The one curve for every rounded corner: the compositor leaves the outside of a window's curve
//! unwritten while the toolkit fills the title bar's corners, so two computations would leave a
//! crescent. Integer arithmetic, since `check-display` compares host and guest pixel for pixel.

/// The radius a floating window's corners are rounded to: 8 pixels.
///
/// A compiled constant in the crate both sides link, as the compositor never reads a theme file.
/// A physical size, not a proportion: a CSS pixel is 1/96 inch and the panel about 100 per inch.
pub const WINDOW_RADIUS: u32 = 8;

/// How much of the pixel at `(dx, dy)` a corner of `radius` covers, 0 to 255.
///
/// `dx` and `dy` count from the corner itself ((0, 0) is the corner pixel, the curve's centre is
/// `(radius, radius)`), so a caller mirrors coordinates for all four corners; at or beyond
/// `radius` the pixel is on the straight edge. Coverage is the distance from the pixel's centre
/// to the curve, clamped to a pixel, computed in 1/256 pixel.
pub fn coverage(radius: u32, dx: u32, dy: u32) -> u8 {
    if dx >= radius || dy >= radius {
        return 255;
    }
    // Twice the offset from the curve's centre to the pixel's: the half-pixel stays integral.
    let a = (2 * radius - 2 * dx - 1) as u64;
    let b = (2 * radius - 2 * dy - 1) as u64;
    // The distance in 1/256ths: `sqrt(a² + b²) / 2 * 256`.
    let d256 = ((a * a + b * b) * 128 * 128).isqrt() as i64;
    // How far inside the curve the centre is, plus the half pixel on its far side.
    let inside = (radius as i64 * 256 + 128 - d256).clamp(0, 256);
    ((inside * 255 + 128) / 256) as u8
}

/// How much of the pixel at `(dx, dy)` belongs to a one-pixel border drawn just inside a corner
/// of `radius`, **as a share of the part of the pixel the shape covers** — 0 to 255.
///
/// A share rather than a coverage: the compositor already blends pixels on the curve at
/// [`coverage`], so a border at the ring's own coverage would fade twice. The border is
/// `outer − inner` of the pixel (the inner shape: same centre, radius `radius − 1`); a pixel the
/// shape does not cover answers 0.
pub fn border_share(radius: u32, dx: u32, dy: u32) -> u8 {
    let outer = coverage(radius, dx, dy) as u32;
    if outer == 0 {
        return 0;
    }
    // The inset shape starts one pixel in.
    let inner = if dx == 0 || dy == 0 || radius == 0 { 0 } else { coverage(radius - 1, dx - 1, dy - 1) as u32 };
    let ring = outer.saturating_sub(inner);
    ((ring * 255 + outer / 2) / outer) as u8
}

/// The shape of one row of a rounded corner: how many pixels are wholly outside the curve, and
/// how many more are only partly inside it.
///
/// A masked blit is `clear` pixels of nothing, `partial` pixels to blend, then a run copied
/// whole. `dy` counts from the nearest edge; a row at or past `radius` is `(0, 0)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Row {
    /// Pixels from the edge that the shape does not touch at all.
    pub clear: u32,
    /// Pixels after those that it covers only partly.
    pub partial: u32,
}

/// The [`Row`] a corner of `radius` has `dy` rows from its edge.
pub fn row(radius: u32, dy: u32) -> Row {
    if dy >= radius {
        return Row { clear: 0, partial: 0 };
    }
    // Coverage only rises along a row, so it is three runs in order.
    let clear = (0..radius).take_while(|&dx| coverage(radius, dx, dy) == 0).count() as u32;
    let full = (clear..radius).find(|&dx| coverage(radius, dx, dy) == 255).unwrap_or(radius);
    Row { clear, partial: full - clear }
}

/// The largest radius a `w × h` rectangle can take: half its shorter side.
///
/// Clamped rather than refused: a window mid-resize may be 10 pixels tall; past half the shorter
/// side the curves overlap.
pub const fn clamp(radius: u32, w: u32, h: u32) -> u32 {
    let half = if w < h { w / 2 } else { h / 2 };
    if radius < half { radius } else { half }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_curve_is_symmetric_and_rises_away_from_the_corner() {
        for r in [1, 2, 3, 6, 8, 14, 16] {
            for dy in 0..r {
                let mut last = 0u8;
                for dx in 0..r {
                    let c = coverage(r, dx, dy);
                    assert_eq!(c, coverage(r, dy, dx), "r {r} ({dx},{dy}) is not symmetric");
                    // Coverage never falls inward along a row, so `row` can be three runs.
                    assert!(c >= last, "r {r} row {dy} falls at {dx}: {last} then {c}");
                    last = c;
                }
            }
        }
    }

    #[test]
    fn the_very_corner_is_empty_and_the_straight_edge_is_full() {
        // Radius 8's corner pixel centre is 10.6 from the curve's centre, well outside; a radius-1
        // corner pixel's is 0.7, partly covered: a softened corner, not a notch.
        assert_eq!(coverage(8, 0, 0), 0);
        assert!(coverage(1, 0, 0) > 0 && coverage(1, 0, 0) < 255);
        for r in [0, 1, 8, 16] {
            assert_eq!(coverage(r, r, 0), 255, "column r of radius {r}");
            assert_eq!(coverage(r, 0, r), 255, "row r of radius {r}");
        }
    }

    #[test]
    fn the_integer_curve_matches_the_real_one_to_within_a_level() {
        // A slip in the half-pixel bookkeeping would move every corner by half a pixel while the
        // shape tests kept passing.
        for r in [1u32, 2, 5, 8, 16, 32] {
            for dy in 0..r {
                for dx in 0..r {
                    let (cx, cy) = (r as f64 - dx as f64 - 0.5, r as f64 - dy as f64 - 0.5);
                    let want = ((r as f64 + 0.5 - cx.hypot(cy)).clamp(0.0, 1.0) * 255.0).round();
                    let got = coverage(r, dx, dy) as f64;
                    assert!((got - want).abs() <= 1.0, "r {r} ({dx},{dy}): {got} against {want}");
                }
            }
        }
    }

    #[test]
    fn a_row_is_clear_then_partial_then_full() {
        for r in [2, 6, 8, 16] {
            for dy in 0..r {
                let Row { clear, partial } = row(r, dy);
                for dx in 0..r {
                    let c = coverage(r, dx, dy);
                    if dx < clear {
                        assert_eq!(c, 0, "r {r} row {dy}: {dx} should be clear");
                    } else if dx < clear + partial {
                        assert!(c > 0 && c < 255, "r {r} row {dy}: {dx} should be partial, is {c}");
                    } else {
                        assert_eq!(c, 255, "r {r} row {dy}: {dx} should be full");
                    }
                }
            }
            assert_eq!(row(r, r), Row { clear: 0, partial: 0 });
        }
        // Row 0 of radius 8 never reaches full coverage before the straight edge: a top-row pixel's
        // centre is half a pixel inside it.
        let top = row(8, 0);
        assert!(top.clear >= 2, "row 0 of radius 8 is {top:?}");
        assert_eq!(top.clear + top.partial, 8, "row 0 of radius 8 is {top:?}");
    }

    #[test]
    fn a_radius_is_clamped_to_half_the_shorter_side() {
        assert_eq!(clamp(8, 100, 100), 8);
        assert_eq!(clamp(8, 10, 100), 5);
        assert_eq!(clamp(8, 100, 3), 1);
        assert_eq!(clamp(8, 0, 0), 0);
    }

    #[test]
    fn a_border_is_all_border_on_the_edge_and_none_inside() {
        // The straight parts: the outermost row and column are the line, the next one in is not.
        for r in [2u32, 8] {
            assert_eq!(border_share(r, 0, r + 3), 255, "the left edge at radius {r}");
            assert_eq!(border_share(r, r + 3, 0), 255, "the top edge at radius {r}");
            assert_eq!(border_share(r, 1, r + 3), 0, "one pixel in at radius {r}");
            assert_eq!(border_share(r, r + 3, r + 3), 0, "the middle at radius {r}");
        }
        // Outside the curve the compositor shows nothing.
        assert_eq!(border_share(8, 0, 0), 0);
    }

    #[test]
    fn a_border_on_the_curve_is_a_share_of_what_the_shape_covers() {
        // The border reaching the screen is the compositor's coverage times the share, and must
        // equal the band's area `outer − inner` to within rounding.
        for r in [3u32, 8, 16] {
            for dy in 0..r {
                for dx in 0..r {
                    let outer = coverage(r, dx, dy) as u32;
                    if outer == 0 {
                        continue;
                    }
                    let inner = if dx == 0 || dy == 0 { 0 } else { coverage(r - 1, dx - 1, dy - 1) as u32 };
                    let band = outer.saturating_sub(inner);
                    let shown = border_share(r, dx, dy) as u32 * outer / 255;
                    assert!(shown.abs_diff(band) <= 1, "r {r} ({dx},{dy}): shown {shown}, band {band}");
                }
            }
        }
    }
}
