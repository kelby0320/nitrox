//! Rounded corners — how much of a pixel near a corner a rounded shape covers.
//!
//! **One function, used by everything that rounds a corner**, and that is the reason this is a
//! module rather than a private helper beside each caller. After the desktop refresh's Part B a
//! window's corner is drawn by two processes: the compositor leaves the outside of the curve
//! unwritten when it blits the window, and the toolkit fills the title bar's corners inside the
//! window's own pixels. If those two computed the curve separately, every corner on the screen
//! would show a thin crescent where one had decided a pixel was in and the other had not
//! (`docs/planning/desktop-refresh.md`, "`radius_px` is a compiled constant"). The radius is the
//! same constant in both; this makes the *curve* the same function too.
//!
//! **Integer, for the reason [`draw_shadow`](crate::compose::draw_shadow) is.** `check-display`
//! compares a host render against the guest's screen pixel for pixel, and a curve computed in
//! floating point is the one thing in compositing that two targets could legitimately round
//! differently.

/// The radius a floating window's corners are rounded to: 8 pixels.
///
/// **A compiled constant, in the crate both sides link**, and that is the whole reason it is
/// here (desktop refresh, "`radius_px` is a compiled constant"). The compositor cuts a window's
/// corner to this curve and the window's own toolkit draws its border along it; the compositor
/// never reads a theme file (M11 decision 1), so a value in the theme would reach one of the two
/// and leave a wedge of mismatch at every corner.
///
/// **Eight is the design's own default**, and a physical size rather than a proportion: a CSS
/// pixel is 1/96 inch and the laptop's panel is about 100 per inch, so the design's 8 px corner is
/// 8 px here (desktop refresh, Part A's metrics decision).
pub const WINDOW_RADIUS: u32 = 8;

/// How much of the pixel at `(dx, dy)` a corner of `radius` covers, 0 to 255.
///
/// `dx` and `dy` count from the corner itself — the pixel in the very corner of the rectangle is
/// `(0, 0)`, and the curve's centre is at `(radius, radius)` — so the one function serves all
/// four corners and a caller mirrors its own coordinates into it. Anything at or beyond `radius`
/// on either axis is fully covered: that is the straight edge, not the curve.
///
/// **The distance from the pixel's centre to the curve, clamped to a pixel**, which is the usual
/// antialiasing approximation and is exact on a straight edge: a pixel whose centre sits on the
/// curve is half covered, one a whole pixel inside is fully covered. Computed in 1/256 of a pixel
/// so the answer has more precision than the 8 bits it is returned in.
pub fn coverage(radius: u32, dx: u32, dy: u32) -> u8 {
    if dx >= radius || dy >= radius {
        return 255;
    }
    // Twice the offset from the curve's centre to the pixel's centre, which keeps the half-pixel
    // in integers: `r - (dx + 0.5)` doubled is `2r - 2dx - 1`, always odd and at least 1.
    let a = (2 * radius - 2 * dx - 1) as u64;
    let b = (2 * radius - 2 * dy - 1) as u64;
    // The distance in 1/256ths: `sqrt(a² + b²) / 2 * 256`, so `sqrt((a² + b²) * 128²)`.
    let d256 = ((a * a + b * b) * 128 * 128).isqrt() as i64;
    // Coverage is how far inside the curve the centre is, plus the half pixel on its far side.
    let inside = (radius as i64 * 256 + 128 - d256).clamp(0, 256);
    ((inside * 255 + 128) / 256) as u8
}

/// How much of the pixel at `(dx, dy)` belongs to a one-pixel border drawn just inside a corner
/// of `radius`, **as a share of the part of the pixel the shape covers** — 0 to 255.
///
/// **For a border on a surface the compositor will cut to the same curve**, and that is why it is
/// a share rather than a coverage. The compositor blends each pixel on the curve at
/// [`coverage`] over what is below; if the surface also blended its border at the ring's own
/// coverage, the edge would be faded twice and every curve would draw lighter than the straight
/// edges beside it. So the border is `outer − inner` of the pixel, and the surface is told what
/// fraction of *its* part that is: a pixel the curve half covers, all of it border, is border
/// here, and the compositor's half is the only fading it gets.
///
/// The border is the band between the shape's own curve and the curve of the shape inset by one
/// pixel with radius `radius − 1` — the same centre, one pixel in. A pixel the shape does not
/// cover at all answers 0: the compositor will not show it, so nothing drawn there matters.
pub fn border_share(radius: u32, dx: u32, dy: u32) -> u8 {
    let outer = coverage(radius, dx, dy) as u32;
    if outer == 0 {
        return 0;
    }
    // The inset shape starts one pixel in, so the outermost row and column are not in it at all.
    let inner = if dx == 0 || dy == 0 || radius == 0 {
        0
    } else {
        coverage(radius - 1, dx - 1, dy - 1) as u32
    };
    let ring = outer.saturating_sub(inner);
    ((ring * 255 + outer / 2) / outer) as u8
}

/// The shape of one row of a rounded corner: how many pixels are wholly outside the curve, and
/// how many more are only partly inside it.
///
/// **What makes a masked blit cheap.** A row of a rounded rectangle is `clear` pixels of nothing,
/// then `partial` pixels to blend, then a run to copy whole — and the run is a `memcpy` exactly
/// as the unrounded row was. Only the partial pixels pay for a read of what is underneath, and a
/// corner of radius 8 has a couple per row.
///
/// `dy` counts from the edge the row is nearest, as for [`coverage`]; a row at or past `radius`
/// is `(0, 0)`.
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
    // Coverage only rises as `dx` moves away from the corner along a row, so the row is three
    // runs in order and two scans find their ends.
    let clear = (0..radius).take_while(|&dx| coverage(radius, dx, dy) == 0).count() as u32;
    let full = (clear..radius).find(|&dx| coverage(radius, dx, dy) == 255).unwrap_or(radius);
    Row { clear, partial: full - clear }
}

/// The largest radius a `w × h` rectangle can take: half its shorter side.
///
/// **Clamped rather than refused**, because the thing being rounded is often a window mid-resize
/// and a window 10 pixels tall is still a window. Past half the shorter side the four curves
/// would overlap, and a shape that rounds more than that is a different shape.
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
                    // A corner is the same curve from either axis.
                    assert_eq!(c, coverage(r, dy, dx), "r {r} ({dx},{dy}) is not symmetric");
                    // And coverage never falls moving inward along a row, which is what lets
                    // `row` describe a row as three runs.
                    assert!(c >= last, "r {r} row {dy} falls at {dx}: {last} then {c}");
                    last = c;
                }
            }
        }
    }

    #[test]
    fn the_very_corner_is_empty_and_the_straight_edge_is_full() {
        // At radius 8 the corner pixel's centre is 10.6 pixels from the curve's centre, against
        // a radius of 8 — well outside. A radius-1 corner is too small to leave
        // any pixel empty: its one corner pixel's centre is 0.7 from the curve's, so it is
        // partly covered, which is right — a one-pixel curve is a softened corner, not a notch.
        assert_eq!(coverage(8, 0, 0), 0);
        assert!(coverage(1, 0, 0) > 0 && coverage(1, 0, 0) < 255);
        for r in [0, 1, 8, 16] {
            assert_eq!(coverage(r, r, 0), 255, "column r of radius {r}");
            assert_eq!(coverage(r, 0, r), 255, "row r of radius {r}");
        }
    }

    #[test]
    fn the_integer_curve_matches_the_real_one_to_within_a_level() {
        // The arithmetic is integer so the guest and the host agree; the *answer* is still meant
        // to be the real distance, and a slip in the half-pixel bookkeeping would move every
        // corner by half a pixel while all the shape tests above kept passing.
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
        // Radius 8's first row, as a reader can check against a picture: the curve enters it
        // well away from the corner, and **never reaches full coverage before the straight
        // edge** — a top-row pixel's centre is half a pixel inside the edge, so the last one
        // before the straight part is still a sliver short. (This test first asserted the
        // opposite, which is the guess the float comparison above exists to replace.)
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
        // The straight parts, which is most of any window's border: the outermost row and column
        // are the line, the next one in is not.
        for r in [2u32, 8] {
            assert_eq!(border_share(r, 0, r + 3), 255, "the left edge at radius {r}");
            assert_eq!(border_share(r, r + 3, 0), 255, "the top edge at radius {r}");
            assert_eq!(border_share(r, 1, r + 3), 0, "one pixel in at radius {r}");
            assert_eq!(border_share(r, r + 3, r + 3), 0, "the middle at radius {r}");
        }
        // Outside the curve the compositor shows nothing, so the answer is nothing.
        assert_eq!(border_share(8, 0, 0), 0);
    }

    #[test]
    fn a_border_on_the_curve_is_a_share_of_what_the_shape_covers() {
        // **The property the share exists for**: the compositor blends the surface's pixel at
        // `coverage`, and the surface blends its border at `border_share` — so what reaches the
        // screen as border is their product, and it must equal the real band's area in the pixel,
        // `outer − inner`, to within rounding. Blending the border at the band's own coverage
        // instead would square it and every curve would draw paler than the straight edge.
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
