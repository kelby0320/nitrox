//! Turning a laid-out tree into pixels, within a damage rectangle.
//!
//! This is where the diff's output is finally spent. [`paint`] takes the rectangle
//! [`Tree::update`](crate::diff::Tree::update) returned and draws **only** what intersects
//! it: a one-character edit repaints one row, not a window. Everything before this point —
//! keyed identity, per-buffer accumulation, the union that keeps `Commit` to one rectangle —
//! exists so that this function can be handed a small rectangle.
//!
//! ## The damaged region is cleared first
//!
//! Painting draws ink; it does not erase. A label that got shorter would leave the tail of
//! its previous text on screen, and a widget that moved would leave a copy behind. So the
//! damage rectangle is filled with the background before anything draws into it — which is
//! also why damage must cover where a node *was* as well as where it is, and why the diff
//! reports both.
//!
//! ## Custom widgets paint themselves
//!
//! A [`Node::Custom`](crate::element::Node::Custom) is opaque here: the toolkit hands the
//! application its rectangle and the clip, and gets out of the way. That is not a
//! concession — Milestone 5's terminal grid is an escape-hatch client, so the escape hatch
//! is a first-class path rather than an afterthought.

use libdraw::framebuffer::Framebuffer;
use libdraw::geom::{Point, Rect};
use libdraw::text::Font;

use crate::element::{Element, IconKind, Node};
use crate::layout::{Layout, Metrics};

/// The theme everything is drawn from — **`libdraw`'s, since M11 Part B**.
///
/// Re-exported rather than moved silently, because `libui::paint::Theme` is what every client in
/// the tree names. It lives one crate down now for a reason `libui` cannot serve: the compositor
/// paints chrome too — a cursor, a drag outline, the ground between windows — and it does not
/// link a widget toolkit, deliberately. `libdraw` is what both link.
///
/// It absorbed `Palette` in the same part. The two were split by which *function* needed which,
/// which is not a distinction between kinds of value — and the wrong seam for a milestone whose
/// point is that these arrive together from one place.
pub use libdraw::theme::Theme;

/// A font at a size, as something layout can measure with.
///
/// The bridge between `libdraw`'s glyphs and [`Metrics`]. Layout must measure with the *same*
/// font that paints, or a label is laid out to one width and drawn at another — text that
/// overflows its box by a few pixels and looks like a layout bug.
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
    // `Framebuffer::fill_rect`, not a loop over `put_pixel`. There was a private copy of
    // this here until 2026-08-11; the trait's own doc explains why that is the wrong shape —
    // an earlier open-coded `y * pitch + x * bpp` left one of two copies silently correct
    // when `offset_of` was broken to check the tests were not vacuous. It is also the faster
    // one: `fill_rect` computes a row's start once and writes across it, where `put_pixel`
    // recomputes the geometry and the offset per pixel, and `paint` clears the whole damage
    // rectangle on every frame (PR #185 review, finding 6).
    fb.fill_rect(damage, theme.background);
    draw(fb, font, theme, element, layout, damage, custom);
}

/// Draw a window control inside `rect`, clipped to `clip`.
///
/// **Centred in a square derived from the box** rather than sized in pixels, so the glyphs stay
/// proportionate if the title bar's height ever changes — which it will, the day chrome metrics
/// follow the type scale. The strokes are two pixels so they read at the sizes this chrome
/// actually uses; one pixel disappears against a bevelled face.
fn draw_icon<F: Framebuffer + ?Sized>(
    fb: &mut F,
    kind: IconKind,
    rect: Rect,
    clip: Rect,
    ink: libdraw::format::Rgb,
) {
    const STROKE: u32 = 2;
    // A square glyph box, a little under half the smaller dimension, centred.
    let side = (rect.size.w.min(rect.size.h) * 4 / 10).max(STROKE * 2);
    let x = rect.origin.x + (rect.size.w.saturating_sub(side) / 2) as i32;
    let y = rect.origin.y + (rect.size.h.saturating_sub(side) / 2) as i32;
    let mut bar = |r: Rect| {
        if let Some(c) = r.intersect(&clip) {
            fb.fill_rect(c, ink);
        }
    };
    match kind {
        // A bar along the bottom — the window going down.
        IconKind::Minimise => {
            bar(Rect::new(x, y + (side - STROKE) as i32, side, STROKE));
        }
        // An empty square: the outline of a window at full size.
        IconKind::Maximise => {
            bar(Rect::new(x, y, side, STROKE));
            bar(Rect::new(x, y + (side - STROKE) as i32, side, STROKE));
            bar(Rect::new(x, y, STROKE, side));
            bar(Rect::new(x + (side - STROKE) as i32, y, STROKE, side));
        }
        // Two strokes. Drawn as a run of short horizontal bars stepping across, which is a
        // line-drawing routine written the once rather than a primitive this crate does not
        // otherwise need — the glyph is a dozen pixels on a side.
        IconKind::Close => {
            for i in 0..side {
                let step = i as i32;
                bar(Rect::new(x + step, y + step, STROKE, 1));
                bar(Rect::new(x + step, y + (side - 1) as i32 - step, STROKE, 1));
            }
        }
    }
}

fn draw<F, Msg, C>(
    fb: &mut F,
    font: &Font,
    theme: &Theme,
    e: &Element<Msg>,
    l: &Layout,
    damage: Rect,
    custom: &mut C,
) where
    F: Framebuffer + ?Sized,
    C: FnMut(u32, Rect, Rect, &mut F),
{
    // **Skipped whole, subtree and all.** A node's children are arranged inside it — the
    // invariant `every_child_is_contained_by_its_parent` holds — so a node that misses the
    // damage has nothing beneath it that could hit. This is where a small damage rectangle
    // turns into a small amount of work rather than merely a small `Commit`.
    let Some(clip) = l.rect.intersect(&damage) else {
        return;
    };

    match &e.node {
        Node::Text(s) => {
            // Positioned by the baseline, which is `ascent` below the box's top: a font's
            // metrics are expressed against the baseline and two runs of different heights
            // line up on it, not on their tops.
            let v = font.v_metrics(theme.font_px);
            let baseline = l.rect.origin.y + libm::ceilf(v.ascent) as i32;
            font.draw_str(
                fb,
                Point::new(l.rect.origin.x, baseline),
                s,
                theme.font_px,
                theme.foreground,
                clip,
            );
        }
        Node::Fill(colour) => fb.fill_rect(clip, *colour),
        // **The node's own rect, not the clip.** The ramp is a property of the shape being
        // drawn; passing the clip would make a one-row repaint paint a one-row gradient.
        Node::Bevel(colour) => fb.fill_rect_bevel(l.rect, clip, *colour, theme.bevel),
        Node::Icon(kind) => draw_icon(fb, *kind, l.rect, clip, theme.foreground),
        Node::Custom { kind, .. } => custom(*kind, l.rect, clip, fb),
        // Containers draw nothing of their own; their children are the picture. Painted in
        // `children()` order, so a `Stack`'s last layer lands on top — the reverse of the
        // order `hit_test` walks, which is what makes the thing you can see the thing you
        // can click.
        _ => {
            for (ce, cl) in e.children().zip(l.children.iter()) {
                draw(fb, font, theme, ce, cl, damage, custom);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libdraw::format::Rgb;
    use crate::diff::Tree;
    use crate::element::{column, custom as custom_node, fill as fill_node, sized, stack, text};
    use crate::layout::layout;
    use alloc::vec;
    use alloc::vec::Vec;
    use libdraw::format::PixelFormat;
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

    /// Pixels not equal to the theme background.
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

    /// Paint with no custom widgets.
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
    fn nothing_outside_the_damage_rectangle_is_touched() {
        // The whole point of the diff. A one-character edit must repaint one row, and the
        // proof is that every pixel elsewhere keeps whatever it held.
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
        // **The gap the test above leaves.** It proves containment using only `text`, whose
        // glyphs cover a fraction of their box — so a node that overdrew its clip by a few
        // pixels might well not reach the assertions. `Fill` is the primitive that covers
        // large areas, and the one a partial repaint would visibly overdraw with.
        //
        // Changing `fb.fill_rect(clip, ..)` to `fb.fill_rect(l.rect, ..)` in `draw` left the
        // whole suite green (PR #185 review, finding 4): a `Fill` whose rect extends past the
        // damage would repaint pixels the diff had just reported clean, which for a partial
        // repaint means painting over a neighbour that did not change.
        let (f, t) = (font(), Theme::default());
        let mut b = fb();
        let marker = Rgb::new(0xFF, 0x00, 0xFF);
        for y in 0..H {
            for x in 0..W {
                b.put_pixel(x, y, marker);
            }
        }

        // One `Fill` taking the whole buffer, damaged over only its top half.
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
        // Painting draws ink; it does not erase. A label that got shorter would otherwise
        // leave the tail of its previous text on screen.
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
        // Not merely clipped: the subtree is never walked. Children are contained by their
        // parents, so a parent that misses the damage has nothing beneath it that could hit.
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
        // The reverse of the order `hit_test` walks, which is what makes the thing you can
        // see the thing you can click.
        let (f, t) = (font(), Theme::default());
        let mut b = fb();
        let e: Element<Msg> = stack(vec![
            custom_node(1, Size::new(0, 0)),
            custom_node(2, Size::new(0, 0)),
        ]);
        let l = layout(&e, Rect::new(0, 0, W, H), &FontMetrics::new(&f, t.font_px));
        let mut order: Vec<u32> = Vec::new();
        paint(&mut b, &f, &t, &e, &l, Rect::new(0, 0, W, H), &mut |k, _, _, _: &mut MemFramebuffer| {
            order.push(k);
        });
        assert_eq!(order, [1, 2], "painted bottom-first");
    }

    #[test]
    fn an_empty_damage_rectangle_paints_nothing_at_all() {
        // What a caller does with the `None` the diff returns is its own business, but a
        // zero-extent rectangle reaching here must still be inert. It is, without a guard:
        // `fill`'s loops run zero times and `intersect` gives `None` at the root. An earlier
        // version had an early return justified as stopping a corner pixel from being
        // cleared — removing it changed nothing, which is how the justification was found to
        // be false.
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
        // Measuring with one font and drawing with another gives text that overflows its box
        // by a few pixels and reads as a layout bug. `FontMetrics` is the seam that stops it.
        let (f, t) = (font(), Theme::default());
        let e: Element<Msg> = column(vec![text("Hello")]);
        let l = layout(&e, Rect::new(0, 0, W, H), &FontMetrics::new(&f, t.font_px));
        assert_eq!(l.children[0].rect.size.h, f.measure("Hello", t.font_px).h);
    }

    #[test]
    fn painting_after_a_diff_repaints_exactly_what_the_diff_reported() {
        // The two halves meeting: the rectangle `Tree::update` returns is the rectangle
        // `paint` is handed, and nothing outside it changes.
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
