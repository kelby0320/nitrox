//! The reference UI: the fixed toolkit picture both the host gate and the guest client paint.
//!
//! The same idea as [`libdraw::scene`], one layer up: a picture the *toolkit* must reproduce.
//!
//! Host tests render into a buffer this process owns, which does not prove that a **font loaded at
//! runtime** rasterises as the compiled one, that the toolkit behaves the same **compiled for the
//! target**, or that a buffer survives the trip **through the Surface protocol**. So both sides
//! render *this* function and the display gate compares them pixel for pixel; a mismatch names the
//! first differing pixel. One instance of each widget, because a gate that exercises one tells you
//! nothing about the others. The custom node's pattern varies in **both** axes, because a fill
//! varying in one axis hashes identically when rows are swapped.

use alloc::vec;

use libdraw::format::{PixelFormat, Rgb};
use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
use libdraw::geom::{Rect, Size};
use libdraw::text::Font;

use crate::element::{Edge, Element, Insets, custom, dock, docked, padding, text};
use crate::layout::layout;
use crate::paint::{FontMetrics, Theme, paint};
use crate::widget::{
    ListRow, ListState, ScrollState, TextAreaState, TextFieldState, WidgetState, button, list_view, menu_bar,
    menu_item, scrollbar, text_area, text_field,
};

/// The reference UI's width, in pixels.
pub const WIDTH: u32 = 320;
/// The reference UI's height, in pixels.
///
/// The terminal's 180×96 covers this window's top-left corner and the display gate excludes it, so
/// anything above y=96 is barely compared.
pub const HEIGHT: u32 = 300;
/// The reference UI's stride, in bytes.
///
/// **Not** `WIDTH × 4` (1280): the extra 12 bytes of row padding catch code that offsets rows by
/// the width, not the pitch.
pub const PITCH: usize = 1292;

/// The `kind` of the custom node, as the paint callback sees it.
pub const CUSTOM_KIND: u32 = 0x4772_6964;

/// Text size in pixels per em, fixed so the default theme cannot change what the gate compares.
pub const FONT_PX: f32 = 16.0;

/// The reference UI's screen geometry.
pub fn geometry() -> Geometry {
    Geometry::with_pitch(WIDTH, HEIGHT, PITCH, PixelFormat::XRGB8888)
        .expect("the reference pitch is wide enough for a row")
}

/// The messages the reference UI's widgets would send; nothing dispatches them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Msg {
    /// The menu's first item.
    File,
    /// The menu's second item.
    Edit,
    /// The button.
    Run,
    /// A list row was activated. Carries the row's key.
    Row(u64),
}

/// The rows the reference list holds: **more than fit**, so it draws a scrollbar.
const ROWS: [ListRow<'static>; 6] = [
    ListRow { key: 41, label: "alpha", marked: false, cells: &[], swatch: None, button: None },
    ListRow { key: 42, label: "beta", marked: false, cells: &[], swatch: None, button: None },
    ListRow { key: 43, label: "gamma", marked: false, cells: &[], swatch: None, button: None },
    ListRow { key: 44, label: "delta", marked: false, cells: &[], swatch: None, button: None },
    ListRow { key: 45, label: "epsilon", marked: false, cells: &[], swatch: None, button: None },
    ListRow { key: 46, label: "zeta", marked: false, cells: &[], swatch: None, button: None },
];

/// How tall each list row is, and how tall the list is — three rows of six visible.
const ROW_H: u32 = 20;
const LIST_H: u32 = 60;
/// The text area's height: two rows, so the selection's second line is drawn.
const AREA_H: u32 = 2 * ROW_H;

/// The reference UI's element tree.
pub fn view() -> Element<Msg> {
    view_with(&Theme::default())
}

/// [`view`], drawn from `theme` so `cargo xtask preview` can show the dark scheme; the gate
/// compares the light one.
pub fn view_with(theme: &Theme) -> Element<Msg> {
    let theme = *theme;
    // Mid-scroll on purpose: a thumb at offset 0 hides an off-by-one in the position arithmetic.
    let scroll = ScrollState { offset: 30, visible: 25, total: 100 };
    dock(
        vec![
            docked(
                Edge::Top,
                menu_bar(
                    vec![menu_item("File", Msg::File, false, &theme), menu_item("Edit", Msg::Edit, true, &theme)],
                    24,
                    &theme,
                ),
            ),
            docked(Edge::Right, scrollbar(scroll, 12, HEIGHT - 24, &theme)),
        ],
        padding(
            Insets::all(8),
            crate::element::column(vec![
                text("Nitrox"),
                text("widget toolkit"),
                button("Run", Msg::Run, WidgetState { active: true, ..Default::default() }, &theme).flex(0),
                // **An open menu, drawn inline rather than as a window**: pins the accelerator
                // column, the separator's rule and the highlighted row.
                reference_menu(&theme),
                custom(CUSTOM_KIND, Size::new(64, 24)).flex(1),
                // **Masked, with the caret mid-string**, so a per-byte mask or an end-caret shows.
                text_field(&reference_field(), true, WidgetState { active: true, ..Default::default() }, &theme),
                // **Selected row 1, scrolled to 0**: the highlight is visible, not on the first
                // row.
                reference_list(&theme),
                // **A selection spanning a line break, caret at its far end**: wrong highlights
                // show only in a picture.
                reference_area(&theme),
            ]),
        ),
    )
}

/// The field the reference UI draws: masked, with the caret two characters from the end.
fn reference_field() -> TextFieldState {
    let mut f = TextFieldState::with_text("passw");
    f.left();
    f.left();
    f
}

/// The list the reference UI draws, with a local state (a fixed picture is rebuilt each call).
fn reference_list(theme: &Theme) -> Element<Msg> {
    let mut state = ListState::at(Some(1), 0);
    let e = list_view(&ROWS, &[], &mut state, LIST_H, ROW_H, Msg::Row, None, None, None, None, theme);
    // Fixed height: otherwise the list takes what is left, making the picture depend on `HEIGHT`.
    crate::element::sized(Size::new(0, LIST_H), e)
}

/// The text area the reference UI draws: two lines, mid-selection, caret at the far end; **the
/// selection is a forward one** (both directions are counted in `widget/tests.rs`).
fn reference_area(theme: &Theme) -> Element<Msg> {
    let mut a = TextAreaState::with_text("select me\nand me");
    a.apply(libkern::abi::KEY_RIGHT, 0, 0);
    a.apply(libkern::abi::KEY_RIGHT, 0, 0);
    a.apply(libkern::abi::KEY_RIGHT, 0, 0);
    for _ in 0..4 {
        a.apply(libkern::abi::KEY_RIGHT, librsproto::surface::MOD_SHIFT, 0);
    }
    a.apply(libkern::abi::KEY_DOWN, librsproto::surface::MOD_SHIFT, 0);
    // **One coloured run, so the picture contains ink that is not the theme's foreground** — the
    // only pixel check on the ink path — **on `me`, under the selection**.
    let ink = [crate::widget::InkRun { line: 0, start: 7, end: 9, colour: theme.syntax_keyword }];
    crate::element::sized(Size::new(0, AREA_H), text_area(&mut a, AREA_H, ROW_H, true, &ink, None, theme))
}

/// The open menu the reference draws: two rows with chords, a separator, and a disabled row; **row
/// 1 is highlighted**, as an unhighlighted menu paints the same without the highlight code.
fn reference_menu(theme: &Theme) -> Element<Msg> {
    use crate::menu::{Accel, Item, Menu, MenuState, popup};
    let m: Menu<Msg> = Menu {
        title: "File",
        items: vec![
            Item::new("New Tab", Accel::ctrl_shift(20, "T"), Msg::File),
            Item::new("Close", Accel::ctrl(17, "W"), Msg::Edit),
            Item::Separator,
            Item::plain("Quit", Msg::Run).enabled(false),
        ],
    };
    let mut st = MenuState::new(1);
    st.toggle(0);
    popup(&m, &st, MENU_ROW_KEY, Some(MENU_ROW_KEY), theme)
}

/// Where the reference menu's row keys start. Above the other widgets' so nothing collides.
const MENU_ROW_KEY: u64 = 900;

/// Paint the custom node: a pattern varying along both axes (a function of the position *within*
/// `rect`), clipped to `clip`.
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
pub fn render(font: &Font) -> MemFramebuffer {
    render_with(font, &Theme::default())
}

/// [`render`], in `theme`. See [`view_with`].
pub fn render_with(font: &Font, theme: &Theme) -> MemFramebuffer {
    let mut fb = MemFramebuffer::new(geometry());
    let ui = view_with(theme);
    let bounds = Rect::new(0, 0, WIDTH, HEIGHT);
    let metrics = FontMetrics::new(font, FONT_PX);
    let l = layout(&ui, bounds, &metrics);
    let theme = Theme { font_px: FONT_PX, ..*theme };
    paint(&mut fb, font, &theme, &ui, &l, bounds, &mut draw_custom);
    fb
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSans.ttf");

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
        let fb = render(&font());
        let first = Framebuffer::get_pixel(&fb, 0, 0).unwrap();
        let varied = (0..HEIGHT).any(|y| (0..WIDTH).any(|x| Framebuffer::get_pixel(&fb, x, y).unwrap() != first));
        assert!(varied, "the reference UI painted a single colour");
    }

    #[test]
    fn the_text_actually_rasterised() {
        // **Counted as "not the background"**, as antialiasing moves most of a glyph's pixels off
        // the solid foreground.
        let f = font();
        let fb = render(&f);
        let theme = Theme::default();
        let chrome = [Theme::default().background, theme.face, theme.accent, theme.track, theme.thumb];
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
        let fb = render(&font());
        let theme = Theme::default();
        let known = [theme.background, theme.foreground, theme.face, theme.accent, theme.track, theme.thumb];
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
