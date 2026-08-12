//! The reference UI: the fixed toolkit picture both the host gate and the guest client paint.
//!
//! The same idea as [`libdraw::scene`], one layer up. That module fixes a picture the
//! *compositor* must reproduce; this one fixes a picture the *toolkit* must reproduce, and it
//! exists for a reason the milestone otherwise had no answer to.
//!
//! ## What it is for
//!
//! Everything in this crate is host-tested, and every one of those tests renders into a
//! buffer this process owns. That proves layout and painting agree with themselves. It does
//! not prove:
//!
//! - that a **font loaded from a file at runtime** rasterises the same glyphs as one compiled
//!   into the host tool — the guest reads `/system/fonts/DejaVuSansMono.ttf` through
//!   `fs-server-ext4`, and until this existed nothing on the target had ever loaded a font;
//! - that the toolkit behaves the same **compiled for the target** — different integer
//!   widths in a layout calculation, a different float path in the rasterizer;
//! - that a toolkit-painted buffer survives the trip **through the Surface protocol** to the
//!   screen at all.
//!
//! So both sides render *this* function and the display gate compares them pixel for pixel.
//! A mismatch names the first differing pixel, which distinguishes "the text is in the wrong
//! place" from "the text is absent" from "everything is shifted".
//!
//! ## Why the picture looks like this
//!
//! One instance of each thing Part C built, because a gate that exercises one widget tells
//! you nothing about the other three:
//!
//! | Element | Catches |
//! |---|---|
//! | A [`menu_bar`] docked to the top | a `Dock` edge that takes the wrong slice |
//! | A [`scrollbar`] docked right, mid-scroll | thumb position arithmetic, and `sized`'s zero-axis rule |
//! | A [`button`] with `active` set | the focus ring, drawn from the layer order in `Stack` |
//! | Two [`text`] runs at different lengths | glyph advance and the measured extent disagreeing |
//! | A [`custom`] node with a two-axis pattern | the escape hatch's clip rect, and transposed rows |
//!
//! The custom node's pattern varies in **both** axes for the reason `scene` gives: a fill that
//! varies in one axis hashes and compares identically when rows are swapped.
//!
//! ## Why it is here and not in the client
//!
//! Because the host has to render it too, and a picture defined in the guest binary is one the
//! host would have to reimplement — which is the bug the gate is supposed to catch, written
//! twice and agreeing with itself. `libdraw::scene` learned this first.

use alloc::vec;

use libdraw::format::{PixelFormat, Rgb};
use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
use libdraw::geom::{Rect, Size};
use libdraw::text::Font;

use crate::element::{Edge, Element, Insets, custom, docked, dock, padding, text};
use crate::layout::layout;
use crate::paint::{FontMetrics, Theme, paint};
use crate::widget::{Palette, ScrollState, WidgetState, button, menu_bar, scrollbar};

/// The reference UI's width, in pixels.
pub const WIDTH: u32 = 320;
/// The reference UI's height, in pixels.
///
/// Tall enough that the flexed custom node gets real space after the two labels and the
/// button have taken their measured heights. At 120 it was squeezed to nothing, the callback
/// drew zero pixels, and every other assertion here still passed.
pub const HEIGHT: u32 = 160;
/// The reference UI's stride, in bytes.
///
/// **Not** `WIDTH × 4` (1280): the extra 12 bytes are three pixels of row padding, for the
/// reason [`libdraw::scene::SCREEN_PITCH`] is padded — any code computing a row offset from
/// the width rather than the pitch skews every row after the first, and equal numbers hide it.
pub const PITCH: usize = 1292;

/// The `kind` of the custom node, as the paint callback sees it.
pub const CUSTOM_KIND: u32 = 0x4772_6964;

/// Text size, in pixels per em. Fixed here rather than taken from [`Theme::default`] so that
/// changing the default theme cannot silently change what the gate compares.
pub const FONT_PX: f32 = 16.0;

/// The reference UI's screen geometry.
pub fn geometry() -> Geometry {
    Geometry::with_pitch(WIDTH, HEIGHT, PITCH, PixelFormat::XRGB8888)
        .expect("the reference pitch is wide enough for a row")
}

/// The messages the reference UI's widgets would send. Nothing dispatches them — the picture
/// is what is under test — but `button` takes one, and `()` would let a handler-less tree
/// compile just as well, which is not what the terminal will look like.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Msg {
    /// The menu's first item.
    File,
    /// The menu's second item.
    Edit,
    /// The button.
    Run,
}

/// The reference UI's element tree.
///
/// Public because the client renders it and the host renders it, and because a test that wants
/// to assert about the tree should not have to rebuild it from this module's prose.
pub fn view() -> Element<Msg> {
    let palette = Palette::default();
    // Mid-scroll on purpose: a thumb at offset 0 is where an off-by-one in the position
    // arithmetic is invisible, because zero times anything is zero.
    let scroll = ScrollState { offset: 30, visible: 25, total: 100 };
    dock(
        vec![
            docked(
                Edge::Top,
                menu_bar(
                    vec![
                        button("File", Msg::File, WidgetState::default(), &palette),
                        button("Edit", Msg::Edit, WidgetState::default(), &palette),
                    ],
                    24,
                    &palette,
                ),
            ),
            docked(Edge::Right, scrollbar(scroll, 12, HEIGHT - 24, &palette)),
        ],
        padding(
            Insets::all(8),
            crate::element::column(vec![
                text("Nitrox"),
                text("widget toolkit"),
                // `active`, so the focus ring is drawn. A button at rest would paint the same
                // whether or not the ring code exists.
                button("Run", Msg::Run, WidgetState { active: true, ..Default::default() }, &palette)
                    .flex(0),
                custom(CUSTOM_KIND, Size::new(64, 24)).flex(1),
            ]),
        ),
    )
}

/// Paint the custom node: a pattern varying along both axes, clipped to `clip`.
///
/// `rect` is the node's own rectangle and `clip` what the damage left of it; the pattern is a
/// function of the position *within* `rect`, so a callback that draws relative to the clip
/// instead of the rect produces a visibly different picture rather than an identical one.
pub fn draw_custom<F: Framebuffer + ?Sized>(kind: u32, rect: Rect, clip: Rect, fb: &mut F) {
    if kind != CUSTOM_KIND {
        return;
    }
    let Some(area) = rect.intersect(&clip) else { return };
    for y in area.origin.y..area.bottom() as i32 {
        for x in area.origin.x..area.right() as i32 {
            let (u, v) = ((x - rect.origin.x) as u8, (y - rect.origin.y) as u8);
            fb.put_pixel(
                x as u32,
                y as u32,
                Rgb::new(
                    0x30u8.wrapping_add(u.wrapping_mul(7)),
                    0x30u8.wrapping_add(v.wrapping_mul(11)),
                    u.wrapping_mul(3) ^ v.wrapping_mul(5),
                ),
            );
        }
    }
}

/// Render the reference UI with `font`.
///
/// Deterministic: the same font bytes give the same buffer, which is the property the gate
/// rests on.
pub fn render(font: &Font) -> MemFramebuffer {
    let mut fb = MemFramebuffer::new(geometry());
    let ui = view();
    let bounds = Rect::new(0, 0, WIDTH, HEIGHT);
    let metrics = FontMetrics::new(font, FONT_PX);
    let l = layout(&ui, bounds, &metrics);
    let theme = Theme { font_px: FONT_PX, ..Theme::default() };
    paint(&mut fb, font, &theme, &ui, &l, bounds, &mut draw_custom);
    fb
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSansMono.ttf");

    fn font() -> Font {
        Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses")
    }

    #[test]
    fn the_reference_ui_is_deterministic() {
        let f = font();
        assert_eq!(render(&f).into_bytes(), render(&f).into_bytes());
    }

    #[test]
    fn the_reference_ui_is_not_a_flat_fill() {
        // The failure `scene` warns about: a picture that compares fine and proves nothing.
        let fb = render(&font());
        let first = Framebuffer::get_pixel(&fb, 0, 0).unwrap();
        let varied = (0..HEIGHT)
            .any(|y| (0..WIDTH).any(|x| Framebuffer::get_pixel(&fb, x, y).unwrap() != first));
        assert!(varied, "the reference UI painted a single colour");
    }

    #[test]
    fn the_text_actually_rasterised() {
        // Layout would place a label of the right size whether or not a glyph was ever drawn,
        // and `Theme::background` everywhere would still be "not a flat fill" thanks to the
        // widgets. So look for ink, which only `draw_str` produces.
        //
        // **Counted as "not the background", not as "exactly the foreground"** — the same
        // correction `libdraw` made when glyphs started blending (PR #188 review). Exact-match
        // counting asks "is any part of this glyph *solid*", and antialiasing moves most of a
        // glyph's pixels off that value: the count here fell from 656 to 105 against a floor of
        // 100 when blending landed, leaving a 5% margin on an assertion whose failure message
        // says "the labels did not rasterise". Any later nudge to the font size, the theme or a
        // label would have turned it red while the labels rasterised perfectly.
        let f = font();
        let fb = render(&f);
        let palette = Palette::default();
        // The widgets' own flat colours are not ink.
        let chrome = [
            Theme::default().background,
            palette.face,
            palette.focus_ring,
            palette.track,
            palette.thumb,
        ];
        // **And nor is the custom node's pattern**, which is 18,688 of the 20,012 non-chrome
        // pixels in this picture. Counting "not chrome" without excluding it gives a test that
        // passes with no text drawn at all — which the first attempt at this fix did.
        let custom = custom_rect(&f);
        let n = (0..HEIGHT)
            .map(|y| {
                (0..WIDTH)
                    .filter(|&x| {
                        if custom.contains(x as i32, y as i32) {
                            return false;
                        }
                        let c = Framebuffer::get_pixel(&fb, x, y).unwrap();
                        !chrome.contains(&c)
                    })
                    .count()
            })
            .sum::<usize>();
        assert!(n > 400, "only {n} ink pixels — the labels did not rasterise");
    }

    /// Where the custom node was laid out, so a test counting ink can exclude its pattern.
    fn custom_rect(font: &Font) -> Rect {
        fn find<Msg>(e: &Element<Msg>, l: &crate::layout::Layout) -> Option<Rect> {
            if matches!(e.node, crate::element::Node::Custom { .. }) {
                return Some(l.rect);
            }
            e.children().zip(l.children.iter()).find_map(|(c, cl)| find(c, cl))
        }
        let ui = view();
        let bounds = Rect::new(0, 0, WIDTH, HEIGHT);
        let l = layout(&ui, bounds, &FontMetrics::new(font, FONT_PX));
        find(&ui, &l).expect("the reference UI has a custom node")
    }

    #[test]
    fn the_custom_node_drew_something() {
        // Its pattern is unique to it: no widget paints a colour that varies per pixel, so
        // finding two different non-theme colours on one row means the callback ran.
        let fb = render(&font());
        let theme = Theme::default();
        let palette = Palette::default();
        let known = [
            theme.background,
            theme.foreground,
            palette.face,
            palette.focus_ring,
            palette.track,
            palette.thumb,
        ];
        let n = (0..HEIGHT)
            .map(|y| {
                (0..WIDTH)
                    .filter(|&x| {
                        let c = Framebuffer::get_pixel(&fb, x, y).unwrap();
                        !known.contains(&c)
                    })
                    .count()
            })
            .sum::<usize>();
        assert!(n > 200, "only {n} pattern pixels — the custom callback did not draw");
    }

    #[test]
    fn a_different_font_gives_a_different_picture() {
        // The property the gate depends on: if the guest loaded the *wrong* file, the pixels
        // differ. Rendering at a different size stands in for a different face — it changes
        // only the glyphs, so a difference here cannot come from the widgets.
        let f = font();
        let mut fb = MemFramebuffer::new(geometry());
        let ui = view();
        let bounds = Rect::new(0, 0, WIDTH, HEIGHT);
        let theme = Theme { font_px: 11.0, ..Theme::default() };
        let l = layout(&ui, bounds, &FontMetrics::new(&f, 11.0));
        paint(&mut fb, &f, &theme, &ui, &l, bounds, &mut draw_custom);
        assert_ne!(fb.into_bytes(), render(&f).into_bytes());
    }
}
