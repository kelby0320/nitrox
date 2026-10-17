//! The title bar and its buttons, and the menu bar.

use super::*;

/// One row of a dropdown menu: a label that highlights under the pointer.
///
/// Hover is state the caller passes in, as for every widget here.
pub fn menu_item<Msg: Clone>(label: &str, msg: Msg, hovered: bool, theme: &Theme) -> Element<Msg> {
    // A wash over the bar, whose own face `menu_bar` paints first.
    let mut layers = alloc::vec::Vec::with_capacity(2);
    if hovered {
        layers.push(wash(theme.accent, theme.scheme.hover_coverage()));
    }
    layers.push(padding(MENU_ITEM_PAD, text(label)));
    stack(layers).on_press(msg)
}

/// The space around a menu item's label, wider than a button's: a menu is a column of text.
const MENU_ITEM_PAD: Insets = Insets { top: 3, right: 10, bottom: 3, left: 10 };

/// A horizontal bar of buttons — a menu bar, or a toolbar.
///
/// An open menu is not here: it is a `popup` window, anchored via
/// [`layout::locate`](crate::layout::locate).
pub fn menu_bar<Msg>(items: alloc::vec::Vec<Element<Msg>>, height: u32, theme: &Theme) -> Element<Msg> {
    // `--faceHi` behind the words and a `--lineSoft` rule drawn inside `height`, not below it:
    // three applications and their gates carry 24 against the design's 25.
    let rule = theme.border.blend(theme.face_hover, 128);
    sized(
        Size::new(0, height),
        dock(
            alloc::vec![docked(Edge::Bottom, sized(Size::new(0, 1), fill(rule)))],
            stack(alloc::vec![fill(theme.face_hover), row(items)]),
        ),
    )
}

/// How tall a title bar is, in pixels, the rule along its bottom included; a fixed number.
pub const TITLE_BAR_H: u32 = 31;

/// How wide each title-bar button is: the design's 23.
pub const TITLE_BUTTON_W: u32 = 23;

/// How tall the face of each is, centred in the bar; the press lands anywhere in its column.
pub const TITLE_BUTTON_H: u32 = 21;

/// The gap between two title-bar buttons: the design's 9.
pub const TITLE_BUTTON_GAP: u32 = 9;

/// The space after the last button, before the window's border: the design's 5.
pub const TITLE_BUTTON_PAD: u32 = 5;

/// The corner of a title-bar button's hover face: the design's 2.
pub(super) const TITLE_BUTTON_RADIUS: u32 = 2;

/// The keys a title bar's buttons carry, so `hovered` can name one: from the top of the key
/// space, clear of an application's keys and tab keys.
pub const TITLE_MINIMISE_KEY: u64 = u64::MAX - 1;

/// See [`TITLE_MINIMISE_KEY`].
pub const TITLE_MAXIMISE_KEY: u64 = u64::MAX - 2;

/// See [`TITLE_MINIMISE_KEY`].
pub const TITLE_CLOSE_KEY: u64 = u64::MAX - 3;

/// The centre of the `nth` title-bar button from the **right** (close is `0`) in a framed window
/// `window_w` wide; for an application's tests, as a gate keeps its own copy.
pub const fn title_button_centre(window_w: u32, nth: u32) -> (i32, i32) {
    let right = window_w - WINDOW_BORDER - TITLE_BUTTON_PAD;
    let x = right - nth * (TITLE_BUTTON_W + TITLE_BUTTON_GAP) - TITLE_BUTTON_W / 2 - 1;
    (x as i32, (WINDOW_CONTENT_Y + (TITLE_BAR_H - 1) / 2) as i32)
}

/// A window's title bar: its name, and the three things you can do to a window.
///
/// `drag` is sent when a press goes **down** on the bar; the application asks the compositor for
/// an interactive move, as only it knows where the window is. A press on a button does not drag:
/// a nearer `on_press` shadows the bar's `on_press_down`.
pub fn title_bar<Msg: Clone>(
    title: impl Into<String>,
    subtitle: Option<&str>,
    focused: bool,
    drag: Msg,
    buttons: TitleButtons<Msg>,
    hovered: Option<u64>,
    theme: &Theme,
) -> Element<Msg> {
    let face = if focused { theme.title_active } else { theme.title_inactive };
    // Glyphs in the dim ink on no face, as the design's controls are borderless. Hover lights
    // one: `face_pressed` for minimise and maximise, `deny` for close alone, which destroys.
    let btn = |glyph: IconKind, key: u64, danger: bool, msg: Msg| {
        let mut layers = alloc::vec::Vec::with_capacity(2);
        let glyph_ink = match (hovered == Some(key), danger) {
            (false, _) => theme.foreground_dim,
            (true, false) => {
                layers.push(rounded_fill(theme.face_pressed, TITLE_BUTTON_RADIUS));
                theme.foreground
            }
            (true, true) => {
                layers.push(rounded_fill(theme.deny, TITLE_BUTTON_RADIUS));
                // The further of the scheme's two inks from the red; a fixed white suits one.
                furthest_from(theme.deny, theme.background, theme.foreground)
            }
        };
        layers.push(ink(glyph_ink, icon(glyph)));
        sized(Size::new(TITLE_BUTTON_W, 0), center(sized(Size::new(TITLE_BUTTON_W, TITLE_BUTTON_H), stack(layers))))
            .on_press(msg)
            .key(key)
    };
    // A button with no message is not drawn: a control that looks live and is not is a defect.
    let mut glyphs = alloc::vec::Vec::with_capacity(3);
    if let Some(m) = buttons.minimise {
        glyphs.push(btn(IconKind::Minimise, TITLE_MINIMISE_KEY, false, m));
    }
    if let Some(m) = buttons.maximise {
        glyphs.push(btn(IconKind::Maximise, TITLE_MAXIMISE_KEY, false, m));
    }
    if let Some(m) = buttons.close {
        glyphs.push(btn(IconKind::Close, TITLE_CLOSE_KEY, true, m));
    }
    let mut controls = alloc::vec::Vec::with_capacity(3);
    // Bold at the body's size: DejaVu's bold is heavier than the design's semibold already, so a
    // size step overshoots. The dim subtitle is body size, so two words do not read as a heading.
    let mut name = alloc::vec::Vec::with_capacity(2);
    name.push(bold(text(title)));
    if let Some(subtitle) = subtitle.filter(|s| !s.is_empty()) {
        name.push(ink(theme.foreground_dim, text(subtitle)));
    }
    let title = center_v(with_spacing(row(name), TITLE_SUBTITLE_GAP));
    controls.push(padding(TITLE_PAD, title).flex(1));
    controls.push(crate::element::with_spacing(row(glyphs), TITLE_BUTTON_GAP));
    controls.push(sized(Size::new(TITLE_BUTTON_PAD, 0), text("")));
    // The drag is on the bar, not the face under the label: dispatch walks up from the hit to
    // the nearest handler, and the label spans the bar. A button stops the walk first.
    // The rule along the bottom lies inside `TITLE_BAR_H`.
    sized(
        Size::new(0, TITLE_BAR_H),
        dock(
            alloc::vec![docked(Edge::Bottom, sized(Size::new(0, 1), fill(theme.border)))],
            stack(alloc::vec![fill(face), row(controls)]),
        )
        .on_press_down(drag),
    )
}

/// What a title bar's buttons do, for the ones their application has an answer for.
///
/// `None` means the button is not drawn.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TitleButtons<Msg> {
    /// Sent by the minimise button.
    pub minimise: Option<Msg>,
    /// Sent by the maximise button.
    pub maximise: Option<Msg>,
    /// Sent by the close button.
    pub close: Option<Msg>,
}

/// No buttons at all — a bar that shows a title and can be dragged.
///
/// Written out because `derive(Default)` would bound `Msg: Default` for no reason.
impl<Msg> Default for TitleButtons<Msg> {
    fn default() -> Self {
        Self { minimise: None, maximise: None, close: None }
    }
}

/// Space between a title bar's text and its edge: the design's 11 on the left.
const TITLE_PAD: Insets = Insets { top: 0, right: 6, bottom: 0, left: 11 };

/// Between a window's title and the dim subtitle beside it: the design's 9.
const TITLE_SUBTITLE_GAP: u32 = 9;
