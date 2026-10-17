//! Turning a laid-out tree into pixels, within a damage rectangle.
//!
//! [`paint`] takes the rectangle [`Tree::update`](crate::diff::Tree::update) returned and draws
//! **only** what intersects it.
//!
//! Painting draws ink; it does not erase, so the damage rectangle is filled with the background
//! first — which is also why damage must cover where a node *was*.

use libdraw::format::Rgb;
use libdraw::framebuffer::Framebuffer;
use libdraw::geom::{Point, Rect};
use libdraw::text::Font;

use crate::element::{Element, IconKind, Node, TextStyle};
use crate::layout::{Layout, Metrics};

/// The theme everything is drawn from — `libdraw`'s, re-exported.
pub use libdraw::theme::Theme;

/// A font at a size, as something layout can measure with: layout must use the *same* font that
/// paints, or text overflows its box.
pub struct FontMetrics<'a> {
    font: &'a Font,
    px: f32,
}

impl<'a> FontMetrics<'a> {
    /// Measure with `font` at `px`.
    pub fn new(font: &'a Font, px: f32) -> Self {
        Self { font, px }
    }
}

impl Metrics for FontMetrics<'_> {
    fn text_size(&self, s: &str) -> libdraw::geom::Size {
        self.font.measure(s, self.px)
    }

    fn text_size_as(&self, s: &str, style: TextStyle) -> libdraw::geom::Size {
        // **Fixed-advance first, then weight within it**: a bold word in a mono buffer wants the
        // mono face's bold, and a mono face with no bold companion answers with itself.
        let base = if style.mono { self.font.mono() } else { self.font };
        let face = if style.bold { base.bold() } else { base };
        face.measure(s, style.size.px(self.px))
    }
}

/// Draw `element` into `fb`, repainting only what `damage` covers.
///
/// `custom` is called for each [`Node::Custom`](crate::element::Node::Custom) with its kind,
/// its rectangle and the clip it must respect.
pub fn paint<F, Msg, C>(
    fb: &mut F,
    font: &Font,
    theme: &Theme,
    element: &Element<Msg>,
    layout: &Layout,
    damage: Rect,
    custom: &mut C,
) where
    F: Framebuffer + ?Sized,
    C: FnMut(u32, Rect, Rect, &mut F),
{
    // `Framebuffer::fill_rect`, not a loop over `put_pixel`: it computes a row's start once, and
    // `paint` clears the whole damage every frame.
    fb.fill_rect(damage, theme.background);
    draw(fb, font, theme, element, layout, damage, theme.foreground, TextStyle::default(), custom);
}

/// [`paint`], **without clearing to the theme's ground first**, for a caller that has put something
/// there to keep (the overview's sidebar, translucent so the desktop shows through).
pub fn paint_over<F, Msg, C>(
    fb: &mut F,
    font: &Font,
    theme: &Theme,
    element: &Element<Msg>,
    layout: &Layout,
    damage: Rect,
    custom: &mut C,
) where
    F: Framebuffer + ?Sized,
    C: FnMut(u32, Rect, Rect, &mut F),
{
    draw(fb, font, theme, element, layout, damage, theme.foreground, TextStyle::default(), custom);
}

/// Draw a glyph — a window control, or the power symbol — inside `rect`, clipped to `clip`.
///
/// **Centred in a square derived from the box** so the glyphs stay proportionate; two-pixel
/// strokes, as one disappears against a bevelled face.
fn draw_icon<F: Framebuffer + ?Sized>(fb: &mut F, kind: IconKind, rect: Rect, clip: Rect, ink: libdraw::format::Rgb) {
    const STROKE: u32 = 2;
    let side = (rect.size.w.min(rect.size.h) * 4 / 10).max(STROKE * 2);
    let x = rect.origin.x + (rect.size.w.saturating_sub(side) / 2) as i32;
    let y = rect.origin.y + (rect.size.h.saturating_sub(side) / 2) as i32;
    let mut bar = |r: Rect| {
        if let Some(c) = r.intersect(&clip) {
            fb.fill_rect(c, ink);
        }
    };
    match kind {
        IconKind::Minimise => {
            bar(Rect::new(x, y + (side - STROKE) as i32, side, STROKE));
        }
        IconKind::Maximise => {
            bar(Rect::new(x, y, side, STROKE));
            bar(Rect::new(x, y + (side - STROKE) as i32, side, STROKE));
            bar(Rect::new(x, y, STROKE, side));
            bar(Rect::new(x + (side - STROKE) as i32, y, STROKE, side));
        }
        IconKind::Close => {
            for i in 0..side {
                let step = i as i32;
                bar(Rect::new(x + step, y + step, STROKE, 1));
                bar(Rect::new(x + step, y + (side - 1) as i32 - step, STROKE, 1));
            }
        }
        // **The power symbol**: a ring `STROKE` thick, open over the top where the bar comes down
        // to the centre, worked in doubled coordinates so the ring is symmetric.
        IconKind::Power => {
            // **Even**, so the two-pixel bar has a centre to straddle; only this glyph rounds.
            let s = (side & !1) as i32;
            let (outer, inner) = (s * s, (s - 2 * STROKE as i32) * (s - 2 * STROKE as i32));
            for py in 0..s {
                for px in 0..s {
                    let (dx, dy) = (2 * px + 1 - s, 2 * py + 1 - s);
                    let d = dx * dx + dy * dy;
                    let in_gap = dy < 0 && dx.abs() * 7 < s * 4;
                    let ring = d <= outer && d > inner && !in_gap;
                    let stem = dx.abs() < STROKE as i32 && dy < 0;
                    if ring || stem {
                        bar(Rect::new(x + px, y + py, 1, 1));
                    }
                }
            }
        }
    }
}

/// Draw one node and its subtree, in `ink`.
#[allow(clippy::too_many_arguments, reason = "a paint pass carries the frame's drawing state as separate values")]
fn draw<F, Msg, C>(
    fb: &mut F,
    font: &Font,
    theme: &Theme,
    e: &Element<Msg>,
    l: &Layout,
    damage: Rect,
    ink: Rgb,
    at: TextStyle,
    custom: &mut C,
) where
    F: Framebuffer + ?Sized,
    C: FnMut(u32, Rect, Rect, &mut F),
{
    // **Skipped whole, subtree and all**: children are arranged inside their parent
    // (`every_child_is_contained_by_its_parent`), so a node that misses the damage has nothing
    // beneath it that could hit.
    let Some(clip) = l.rect.intersect(&damage) else {
        return;
    };

    match &e.node {
        Node::Text(s) => {
            // Positioned by the baseline, `ascent` below the box's top, at the step the enclosing
            // `Scale` set (the size layout measured this run at).
            let px = at.size.px(theme.font_px);
            let base = if at.mono { font.mono() } else { font };
            let face = if at.bold { base.bold() } else { base };
            let v = face.v_metrics(px);
            let baseline = l.rect.origin.y + libm::ceilf(v.ascent) as i32;
            face.draw_str(fb, Point::new(l.rect.origin.x, baseline), s, px, ink, clip);
        }
        Node::Fill(colour) => fb.fill_rect(clip, *colour),
        // **The node's own rect, not the clip.** The ramp is a property of the shape being
        // drawn; passing the clip would make a one-row repaint paint a one-row gradient.
        Node::Bevel(colour) => fb.fill_rect_bevel(l.rect, clip, *colour, theme.bevel),
        Node::Outline { colour, radius } => fb.outline_rounded_rect(l.rect, clip, *colour, *radius),
        // The node's own rect for the shape and the clip for how much of it this frame repaints,
        // for `Bevel`'s reason: a curve taken from the clip would round a partial repaint's edges.
        Node::RoundedFill { colour, radius } => fb.fill_rounded_rect(l.rect, clip, *colour, *radius),
        Node::Wash { colour, coverage } => fb.blend_rect(clip, *colour, *coverage),
        Node::Icon(kind) => draw_icon(fb, *kind, l.rect, clip, ink),
        // **Not a container arm**, which would paint its child correctly and ignore the colour.
        Node::Ink { colour, child } => {
            if let Some(cl) = l.children.first() {
                draw(fb, font, theme, child, cl, damage, *colour, at, custom);
            }
        }
        Node::Scale { size, child } => {
            if let Some(cl) = l.children.first() {
                let at = TextStyle { size: *size, ..at };
                draw(fb, font, theme, child, cl, damage, ink, at, custom);
            }
        }
        Node::Bold { child } => {
            if let Some(cl) = l.children.first() {
                let at = TextStyle { bold: true, ..at };
                draw(fb, font, theme, child, cl, damage, ink, at, custom);
            }
        }
        Node::Mono { child } => {
            if let Some(cl) = l.children.first() {
                let at = TextStyle { mono: true, ..at };
                draw(fb, font, theme, child, cl, damage, ink, at, custom);
            }
        }
        Node::Custom { kind, .. } => custom(*kind, l.rect, clip, fb),
        // Containers draw nothing of their own; painted in `children()` order, a `Stack`'s last
        // layer lands on top, the reverse of `hit_test`'s walk.
        _ => {
            for (ce, cl) in e.children().zip(l.children.iter()) {
                draw(fb, font, theme, ce, cl, damage, ink, at, custom);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::Tree;
    use crate::element::{column, custom as custom_node, fill as fill_node, sized, stack, text};
    use crate::layout::layout;
    use alloc::vec;
    use alloc::vec::Vec;
    use libdraw::format::PixelFormat;
    use libdraw::format::Rgb;
    use libdraw::framebuffer::{Geometry, MemFramebuffer};
    use libdraw::geom::Size;

    type Msg = ();

    const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSans.ttf");
    const W: u32 = 200;
    const H: u32 = 100;

    fn font() -> Font {
        Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses")
    }

    fn fb() -> MemFramebuffer {
        MemFramebuffer::new(Geometry::packed(W, H, PixelFormat::XRGB8888))
    }

    fn ink(fb: &MemFramebuffer, theme: &Theme) -> usize {
        let mut n = 0;
        for y in 0..H {
            for x in 0..W {
                if fb.get_pixel(x, y) != Some(theme.background) {
                    n += 1;
                }
            }
        }
        n
    }

    fn go(b: &mut MemFramebuffer, f: &Font, t: &Theme, e: &Element<Msg>, damage: Rect) -> Layout {
        let l = layout(e, Rect::new(0, 0, W, H), &FontMetrics::new(f, t.font_px));
        paint(b, f, t, e, &l, damage, &mut |_, _, _, _: &mut MemFramebuffer| {});
        l
    }

    #[test]
    fn text_draws_ink_inside_its_own_rectangle() {
        let (f, t) = (font(), Theme::default());
        let mut b = fb();
        let e: Element<Msg> = column(vec![text("Hi")]);
        let l = go(&mut b, &f, &t, &e, Rect::new(0, 0, W, H));
        assert!(ink(&b, &t) > 0, "something was drawn");

        let row = l.children[0].rect;
        for y in 0..H {
            for x in 0..W {
                if fb_has_ink(&b, &t, x, y) {
                    assert!(row.contains(x as i32, y as i32), "ink at ({x},{y}) outside {row:?}");
                }
            }
        }
    }

    fn fb_has_ink(b: &MemFramebuffer, t: &Theme, x: u32, y: u32) -> bool {
        b.get_pixel(x, y) != Some(t.background)
    }

    #[test]
    fn scaled_text_measures_and_draws_at_its_step() {
        use crate::element::{TextSize, scaled};
        let (f, t) = (font(), Theme::default());
        let m = FontMetrics::new(&f, t.font_px);
        let at = |e: &Element<Msg>| crate::layout::measure(e, crate::layout::Constraints::loose(Size::new(W, H)), &m);
        let body = at(&column(vec![text("Hello")]));
        let small = at(&column(vec![scaled(TextSize::Small, text("Hello"))]));
        let large = at(&column(vec![scaled(TextSize::Large, text("Hello"))]));
        assert!(small.w < body.w && small.h < body.h, "small {small:?} against body {body:?}");
        assert!(large.w > body.w && large.h > body.h, "large {large:?} against body {body:?}");
        assert_eq!(small, f.measure("Hello", TextSize::Small.px(t.font_px)));
        let inner = scaled(TextSize::Small, text("Hello"));
        let nested = at(&column(vec![scaled(TextSize::Large, inner)]));
        assert_eq!(nested, small);

        // **What is drawn is drawn at the step**, read off the ink in a box far wider than the
        // text, so the clip cannot be what limits it.
        let mut widths = Vec::new();
        for step in [TextSize::Small, TextSize::Body, TextSize::Large] {
            let e: Element<Msg> = sized(Size::new(W, 40), scaled(step, text("Hello")));
            let mut b = fb();
            b.clear(t.background);
            go(&mut b, &f, &t, &e, Rect::new(0, 0, W, H));
            let (mut left, mut right) = (u32::MAX, 0);
            for y in 0..H {
                for x in 0..W {
                    if fb_has_ink(&b, &t, x, y) {
                        left = left.min(x);
                        right = right.max(x);
                    }
                }
            }
            widths.push(right + 1 - left);
        }
        let ordered = widths[0] < widths[1] && widths[1] < widths[2];
        assert!(ordered, "the steps draw at one size: {widths:?}");
        let ratio = widths[0] as f32 / widths[1] as f32;
        let want = TextSize::SMALL;
        assert!((ratio - want).abs() < 0.08, "small is {ratio} of body, not {want}");
    }

    #[test]
    fn mono_text_uses_the_companion_face_and_falls_back_without_one() {
        use crate::element::mono;
        const MONO: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSansMono.ttf");
        let t = Theme::default();
        let plain = font();
        let paired = font().with_mono(Font::from_bytes(MONO.to_vec()).expect("the mono face"));
        let measured = |f: &Font, e: &Element<Msg>| {
            let loose = crate::layout::Constraints::loose(Size::new(W * 4, H));
            crate::layout::measure(e, loose, &FontMetrics::new(f, t.font_px)).w
        };
        let narrow = || -> Element<Msg> { mono(text("iiii")) };
        let wide = || -> Element<Msg> { mono(text("mmmm")) };
        assert_eq!(
            measured(&paired, &narrow()),
            measured(&paired, &wide()),
            "every glyph takes the same room in the companion"
        );
        assert!(
            measured(&plain, &narrow()) < measured(&plain, &wide()),
            "with no companion it is the proportional face"
        );
        let bare = |s: &'static str| -> Element<Msg> { text(s) };
        assert!(measured(&paired, &bare("iiii")) < measured(&paired, &bare("mmmm")));

        // **And the glyphs are *painted* in it, which measuring cannot say**: dropping the face
        // selection in `draw` would leave every column crooked.
        let inked = |f: &Font, e: Element<Msg>| {
            let mut b = fb();
            b.clear(t.background);
            go(&mut b, f, &t, &sized(Size::new(W, 40), e), Rect::new(0, 0, W, H));
            ink(&b, &t)
        };
        let narrow_run = || -> Element<Msg> { mono(text("iiiiiiii")) };
        assert!(
            inked(&paired, narrow_run()) > inked(&plain, narrow_run()),
            "the companion's wider advances put down more ink than the proportional face"
        );
    }

    #[test]
    fn bold_text_uses_the_companion_face_and_falls_back_without_one() {
        use crate::element::bold;
        const BOLD: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSans-Bold.ttf");
        let t = Theme::default();
        let plain = font();
        let paired = font().with_bold(Font::from_bytes(BOLD.to_vec()).expect("the bold face"));
        let words = || -> Element<Msg> { text("Files") };
        let measured = |f: &Font, e: &Element<Msg>| {
            let loose = crate::layout::Constraints::loose(Size::new(W, H));
            crate::layout::measure(e, loose, &FontMetrics::new(f, t.font_px)).w
        };
        let inked = |f: &Font, e: Element<Msg>| {
            let mut b = fb();
            b.clear(t.background);
            go(&mut b, f, &t, &sized(Size::new(W, 40), e), Rect::new(0, 0, W, H));
            ink(&b, &t)
        };
        let (heavy, regular) = (measured(&paired, &bold(words())), measured(&paired, &words()));
        assert!(heavy > regular, "bold measures wider");
        assert!(inked(&paired, bold(words())) > inked(&paired, words()), "bold puts down more ink");
        assert_eq!(measured(&plain, &bold(words())), measured(&plain, &words()));
        assert_eq!(inked(&plain, bold(words())), inked(&plain, words()));
    }

    #[test]
    fn a_change_of_step_or_weight_alone_repaints() {
        use crate::diff::Tree;
        use crate::element::{TextSize, scaled};
        let (f, t) = (font(), Theme::default());
        let m = FontMetrics::new(&f, t.font_px);
        let view = |step| -> Element<Msg> { sized(Size::new(120, 30), scaled(step, text("status"))) };
        let mut tree = Tree::new();
        let a = view(TextSize::Body);
        let _ = tree.update(&a, &layout(&a, Rect::new(0, 0, W, H), &m));
        let b = view(TextSize::Small);
        let damage = tree.update(&b, &layout(&b, Rect::new(0, 0, W, H), &m)).expect("diffs");
        assert!(damage.is_some(), "a smaller status line must be redrawn");
        let c = view(TextSize::Body);
        let _ = tree.update(&c, &layout(&c, Rect::new(0, 0, W, H), &m));
        let d: Element<Msg> = sized(Size::new(120, 30), crate::element::bold(text("status")));
        let damage = tree.update(&d, &layout(&d, Rect::new(0, 0, W, H), &m)).expect("diffs");
        assert!(damage.is_some(), "a bold status line must be redrawn");
    }

    #[test]
    fn the_power_glyph_is_an_open_ring_with_a_bar() {
        use crate::element::{IconKind, icon};
        let (f, t) = (font(), Theme::default());
        let glyph = |kind: IconKind| {
            let mut b = fb();
            let e: Element<Msg> = sized(Size::new(40, 40), icon(kind));
            go(&mut b, &f, &t, &e, Rect::new(0, 0, W, H));
            let inked: Vec<(u32, u32)> =
                (0..H).flat_map(|y| (0..W).map(move |x| (x, y))).filter(|&(x, y)| fb_has_ink(&b, &t, x, y)).collect();
            (b, inked)
        };
        let (b, inked) = glyph(IconKind::Power);
        assert!(!inked.is_empty(), "the power glyph drew nothing");
        let (x0, x1) = (inked.iter().map(|p| p.0).min().unwrap(), inked.iter().map(|p| p.0).max().unwrap());
        let (y0, y1) = (inked.iter().map(|p| p.1).min().unwrap(), inked.iter().map(|p| p.1).max().unwrap());
        let on = |x: u32, y: u32| fb_has_ink(&b, &t, x, y);
        assert_eq!(x1 - x0, y1 - y0, "not square: {x0}..{x1} by {y0}..{y1}");
        for &(x, y) in &inked {
            assert!(on(x0 + x1 - x, y), "not symmetric at ({x},{y})");
        }
        let (cx, cy) = ((x0 + x1) / 2, (y0 + y1) / 2);
        // The bar at the top centre — `cx` is its left column, the box being even — and **the gap
        // beside it, a row down**, which the top row alone cannot show.
        assert!(on(cx, y0) && on(cx + 1, y0), "no bar at the top centre");
        assert!(!on(cx - 2, y0 + 1) && !on(cx + 3, y0 + 1), "the ring is closed at the top");
        assert!(on(x0, cy) && on(x1, cy), "no ring at the sides");
        assert!(on(cx, y1), "the ring is open at the bottom");
        assert!(!on(cx, cy + (y1 - cy) / 2), "filled below the centre");
        let (bc, _) = glyph(IconKind::Close);
        assert!(fb_has_ink(&bc, &t, 20, 20), "the close cross should be inked at its centre");
        let (bm, maxi) = glyph(IconKind::Maximise);
        let top = maxi.iter().map(|p| p.1).min().unwrap();
        let left = maxi.iter().map(|p| p.0).min().unwrap();
        assert!(fb_has_ink(&bm, &t, left + 1, top), "the square's top edge runs its whole width");
    }

    #[test]
    fn a_rounded_fill_is_rounded_at_its_own_corners_and_nowhere_else() {
        use crate::element::rounded_fill;
        let (f, t) = (font(), Theme::default());
        let red = Rgb::new(200, 0, 0);
        let e: Element<Msg> = sized(Size::new(40, 30), rounded_fill(red, 8));
        let mut b = fb();
        b.clear(t.background);
        go(&mut b, &f, &t, &e, Rect::new(0, 0, W, H));
        assert_eq!(b.get_pixel(0, 0), Some(t.background), "the corner is cut");
        assert_eq!(b.get_pixel(39, 29), Some(t.background), "every corner is cut");
        assert_eq!(b.get_pixel(20, 0), Some(red), "the straight top edge is solid");
        assert_eq!(b.get_pixel(0, 15), Some(red), "the straight left edge is solid");
        assert_eq!(b.get_pixel(20, 15), Some(red), "the middle is the colour");
        let curve = (0..8).map(|i| b.get_pixel(i, 8 - 1 - i)).find(|p| *p != Some(red) && *p != Some(t.background));
        assert!(curve.is_some(), "the curve is antialiased, not stepped");

        let mut b = fb();
        b.clear(t.background);
        go(&mut b, &f, &t, &e, Rect::new(0, 10, W, 10));
        assert_eq!(b.get_pixel(0, 10), Some(red), "the band's end is not a corner of the shape");
        assert_eq!(b.get_pixel(39, 19), Some(red), "nor is its other end");
        assert_eq!(b.get_pixel(0, 0), Some(t.background), "and nothing outside the band was drawn");
    }

    /// Every pixel drawn, as a set of colours: **a set, never an exact match against the ink**, as
    /// antialiasing leaves what survives blending as the **hue**.
    fn painted(b: &MemFramebuffer, t: &Theme) -> Vec<Rgb> {
        let mut out = Vec::new();
        for y in 0..H {
            for x in 0..W {
                if let Some(c) = b.get_pixel(x, y)
                    && c != t.background
                    && !out.contains(&c)
                {
                    out.push(c);
                }
            }
        }
        out
    }

    fn all_theme_ink(c: &[Rgb], t: &Theme) -> bool {
        c.iter().all(|p| (0..=255u8).any(|a| t.foreground.blend(t.background, a) == *p))
    }

    fn any_reddish(c: &[Rgb]) -> bool {
        c.iter().any(|p| p.r > p.g && p.g == p.b)
    }

    fn any_greenish(c: &[Rgb]) -> bool {
        c.iter().any(|p| p.g > p.r && p.r == p.b)
    }

    const RED: Rgb = Rgb::new(0xFF, 0x00, 0x00);
    const GREEN: Rgb = Rgb::new(0x00, 0xFF, 0x00);

    #[test]
    fn text_inside_an_ink_node_is_drawn_in_that_colour() {
        let (f, t) = (font(), Theme::default());

        let mut plain = fb();
        go(&mut plain, &f, &t, &column(vec![text("Hi")]), Rect::new(0, 0, W, H));
        let before = painted(&plain, &t);
        assert!(!before.is_empty(), "nothing was drawn at all");
        assert!(all_theme_ink(&before, &t), "the theme's ink is what text uses by default");

        let mut coloured = fb();
        let e: Element<Msg> = column(vec![crate::element::ink(RED, text("Hi"))]);
        go(&mut coloured, &f, &t, &e, Rect::new(0, 0, W, H));
        let after = painted(&coloured, &t);
        assert!(any_reddish(&after), "the wrapper's colour never reached the glyphs");
        assert!(
            !all_theme_ink(&after, &t) && !after.contains(&t.foreground),
            "the theme's ink was drawn as well, so the leaf ignored what it was given"
        );
    }

    #[test]
    fn ink_is_inherited_and_the_innermost_wins() {
        let (f, t) = (font(), Theme::default());
        let mut b = fb();
        let e: Element<Msg> = crate::element::ink(
            RED,
            column(vec![text("outer"), crate::element::ink(GREEN, column(vec![text("inner")]))]),
        );
        go(&mut b, &f, &t, &e, Rect::new(0, 0, W, H));
        let c = painted(&b, &t);
        assert!(any_reddish(&c), "the outer text lost its ink through the column");
        assert!(any_greenish(&c), "the inner ink did not override the outer");
        assert!(!c.contains(&t.foreground), "something was still drawn in the theme's ink");
    }

    #[test]
    fn an_ink_node_lays_out_exactly_as_its_child() {
        let (f, t) = (font(), Theme::default());
        let m = FontMetrics::new(&f, t.font_px);
        let bare: Element<Msg> = column(vec![text("measure me")]);
        let wrapped: Element<Msg> = column(vec![crate::element::ink(RED, text("measure me"))]);
        let (a, b) = (layout(&bare, Rect::new(0, 0, W, H), &m), layout(&wrapped, Rect::new(0, 0, W, H), &m));
        assert_eq!(a.children[0].rect, b.children[0].rect, "the wrapper is not its child's size");
        assert_eq!(a.children[0].rect, b.children[0].children[0].rect, "the text inside the wrapper moved or resized");
    }

    #[test]
    fn nothing_outside_the_damage_rectangle_is_touched() {
        let (f, t) = (font(), Theme::default());
        let mut b = fb();
        let marker = Rgb::new(0xFF, 0x00, 0xFF);
        for y in 0..H {
            for x in 0..W {
                b.put_pixel(x, y, marker);
            }
        }

        let e: Element<Msg> = column(vec![text("Hi"), text("There")]);
        let damage = Rect::new(0, 0, W, 20);
        go(&mut b, &f, &t, &e, damage);

        for y in 20..H {
            for x in 0..W {
                assert_eq!(b.get_pixel(x, y), Some(marker), "({x},{y}) outside the damage");
            }
        }
    }

    #[test]
    fn a_fill_straddling_the_damage_edge_stops_at_it() {
        // **The gap the test above leaves**: `text` covers only a fraction of its box, so an
        // overdrawn clip might not reach the assertions; `Fill` would.
        let (f, t) = (font(), Theme::default());
        let mut b = fb();
        let marker = Rgb::new(0xFF, 0x00, 0xFF);
        for y in 0..H {
            for x in 0..W {
                b.put_pixel(x, y, marker);
            }
        }

        let colour = Rgb::new(0x20, 0x90, 0x40);
        let e: Element<Msg> = fill_node(colour);
        let half = H / 2;
        go(&mut b, &f, &t, &e, Rect::new(0, 0, W, half));

        assert_eq!(b.get_pixel(0, 0), Some(colour), "the damaged half was not filled");
        assert_eq!(
            b.get_pixel(0, half),
            Some(marker),
            "the fill ran past the damage rectangle and repainted a clean row"
        );
        assert_eq!(b.get_pixel(W - 1, H - 1), Some(marker), "and past it at the far corner");
    }

    #[test]
    fn the_damaged_region_is_cleared_before_anything_draws_into_it() {
        let (f, t) = (font(), Theme::default());
        let mut b = fb();
        let e: Element<Msg> = column(vec![text("MMMMMMMMMMMM")]);
        let damage = Rect::new(0, 0, W, H);
        go(&mut b, &f, &t, &e, damage);
        let long = ink(&b, &t);
        assert!(long > 0);

        let e: Element<Msg> = column(vec![text("M")]);
        go(&mut b, &f, &t, &e, damage);
        let short = ink(&b, &t);
        assert!(short > 0, "the short label drew");
        assert!(short < long, "and the long one was erased: {short} vs {long}");
    }

    #[test]
    fn a_custom_node_is_handed_its_rectangle_and_the_clip() {
        let (f, t) = (font(), Theme::default());
        let mut b = fb();
        let e: Element<Msg> = column(vec![sized(Size::new(40, 30), custom_node(7, Size::new(0, 0)))]);
        let l = layout(&e, Rect::new(0, 0, W, H), &FontMetrics::new(&f, t.font_px));

        let mut seen: Vec<(u32, Rect, Rect)> = Vec::new();
        let damage = Rect::new(0, 0, 20, H);
        paint(&mut b, &f, &t, &e, &l, damage, &mut |k, rect, clip, _: &mut MemFramebuffer| {
            seen.push((k, rect, clip));
        });
        assert_eq!(seen.len(), 1);
        let (kind, rect, clip) = seen[0];
        assert_eq!(kind, 7);
        assert_eq!(rect, Rect::new(0, 0, 40, 30), "its whole rectangle, damaged or not");
        assert_eq!(clip, Rect::new(0, 0, 20, 30), "and the part it may draw in");
    }

    #[test]
    fn a_node_outside_the_damage_is_skipped_entirely() {
        let (f, t) = (font(), Theme::default());
        let mut b = fb();
        let e: Element<Msg> = column(vec![
            sized(Size::new(W, 20), custom_node(1, Size::new(0, 0))),
            sized(Size::new(W, 20), custom_node(2, Size::new(0, 0))),
        ]);
        let l = layout(&e, Rect::new(0, 0, W, H), &FontMetrics::new(&f, t.font_px));

        let mut kinds: Vec<u32> = Vec::new();
        paint(&mut b, &f, &t, &e, &l, Rect::new(0, 0, W, 10), &mut |k, _, _, _: &mut MemFramebuffer| {
            kinds.push(k);
        });
        assert_eq!(kinds, [1], "the second row never ran");
    }

    #[test]
    fn a_stacks_last_layer_paints_on_top() {
        let (f, t) = (font(), Theme::default());
        let mut b = fb();
        let e: Element<Msg> = stack(vec![custom_node(1, Size::new(0, 0)), custom_node(2, Size::new(0, 0))]);
        let l = layout(&e, Rect::new(0, 0, W, H), &FontMetrics::new(&f, t.font_px));
        let mut order: Vec<u32> = Vec::new();
        paint(&mut b, &f, &t, &e, &l, Rect::new(0, 0, W, H), &mut |k, _, _, _: &mut MemFramebuffer| {
            order.push(k);
        });
        assert_eq!(order, [1, 2], "painted bottom-first");
    }

    #[test]
    fn an_empty_damage_rectangle_paints_nothing_at_all() {
        // A zero-extent rectangle reaching here must be inert without a guard: `fill`'s loops run
        // zero times and `intersect` gives `None` at the root.
        let (f, t) = (font(), Theme::default());
        let mut b = fb();
        let marker = Rgb::new(0xFF, 0x00, 0xFF);
        b.put_pixel(0, 0, marker);
        let e: Element<Msg> = column(vec![text("Hi")]);
        go(&mut b, &f, &t, &e, Rect::new(0, 0, 0, 0));
        assert_eq!(b.get_pixel(0, 0), Some(marker));
    }

    #[test]
    fn layout_measures_with_the_font_that_paints() {
        let (f, t) = (font(), Theme::default());
        let e: Element<Msg> = column(vec![text("Hello")]);
        let l = layout(&e, Rect::new(0, 0, W, H), &FontMetrics::new(&f, t.font_px));
        assert_eq!(l.children[0].rect.size.h, f.measure("Hello", t.font_px).h);
    }

    #[test]
    fn painting_after_a_diff_repaints_exactly_what_the_diff_reported() {
        let (f, t) = (font(), Theme::default());
        let m = FontMetrics::new(&f, t.font_px);
        let mut b = fb();
        let mut tree = Tree::new();

        let before: Element<Msg> = column(vec![text("a"), text("b")]);
        let l0 = layout(&before, Rect::new(0, 0, W, H), &m);
        let d0 = tree.update(&before, &l0).expect("ok").expect("first frame is full");
        paint(&mut b, &f, &t, &before, &l0, d0, &mut |_, _, _, _: &mut MemFramebuffer| {});

        let marker = Rgb::new(0xFF, 0x00, 0xFF);
        b.put_pixel(0, H - 1, marker);

        let after: Element<Msg> = column(vec![text("a"), text("Z")]);
        let l1 = layout(&after, Rect::new(0, 0, W, H), &m);
        let d1 = tree.update(&after, &l1).expect("ok").expect("the second row changed");
        assert!(d1.size.h < H, "the diff reported a partial repaint: {d1:?}");
        paint(&mut b, &f, &t, &after, &l1, d1, &mut |_, _, _, _: &mut MemFramebuffer| {});
        assert_eq!(b.get_pixel(0, H - 1), Some(marker), "the untouched row stayed untouched");
    }
}
