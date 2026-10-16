//! The title bar and its buttons, and the menu bar.

use super::*;

/// One row of a dropdown menu: a label that highlights under the pointer.
///
/// **The same treatment a selected list row gets** — the selection colour bevelled inside a
/// one-pixel border in the focus blue — because they are the same thing seen twice: the item
/// that would happen if you acted now. The reference desktop draws them identically.
///
/// **Hover is state the caller passes in**, as it is for every widget here. What is new is that
/// somebody finally passes it: `Router::inside` has reported the widget under the cursor since
/// M4 and `WidgetState::hovered` has existed just as long, and until M11 Part E batch 3 no
/// application ever connected the two — so nothing in this system had ever reacted to the
/// pointer moving over it.
pub fn menu_item<Msg: Clone>(label: &str, msg: Msg, hovered: bool, theme: &Theme) -> Element<Msg> {
    // **A wash over the bar, not a ring round a fill** (desktop refresh, Part B) — the design's
    // `--soft`. The bar's own face is painted beneath this by `menu_bar`, first, which is the
    // ground a wash needs.
    let mut layers = alloc::vec::Vec::with_capacity(2);
    if hovered {
        layers.push(wash(theme.accent, theme.scheme.hover_coverage()));
    }
    layers.push(padding(MENU_ITEM_PAD, text(label)));
    stack(layers).on_press(msg)
}

/// The space around a menu item's label.
///
/// Wider than a button's, because a menu is a column of text rather than a control: the reading
/// is horizontal and the eye needs the gutter.
const MENU_ITEM_PAD: Insets = Insets { top: 3, right: 10, bottom: 3, left: 10 };

/// A horizontal bar of buttons — a menu bar, or a toolbar.
///
/// The popup half of a menu is not here, and is not a widget at all: an open menu is a `popup`
/// **window**, parented to the application's window and clipped by the screen. It was a `stack`
/// layer over the application's content until M6 C3, which works only for a menu that fits
/// inside the window it drops from. This is the part the terminal's chrome needs; the anchor it
/// is dropped from comes from [`layout::locate`](crate::layout::locate).
pub fn menu_bar<Msg>(items: alloc::vec::Vec<Element<Msg>>, height: u32, theme: &Theme) -> Element<Msg> {
    // **The design's menu bar** (desktop refresh, Part B): `--faceHi` behind the words, which
    // sets it apart from the title bar above in the same family of greys, and a `--lineSoft` rule
    // under it. The rule is `border` at half strength over the bar's own face — the derivation
    // Part A measured in both palettes — and is drawn *inside* `height` rather than below it:
    // the design's bar is 25 with its rule, and three applications and their gates carry 24,
    // which is a pixel of divergence rather than a pixel of every aim moving.
    let rule = theme.border.blend(theme.face_hover, 128);
    sized(
        Size::new(0, height),
        dock(
            alloc::vec![docked(Edge::Bottom, sized(Size::new(0, 1), fill(rule)))],
            stack(alloc::vec![fill(theme.face_hover), row(items)]),
        ),
    )
}

/// How tall a title bar is, in pixels — the rule along its bottom included.
///
/// One number rather than a measurement of the font, because a client sizes its window around
/// it: a bar that grew with the theme would change every window's content area when the theme
/// changed, and M11 is where a theme becomes changeable.
///
/// **31 since the desktop refresh's Part B**, the design's own (`height:31px` with a one-pixel
/// `border-bottom`, box-sized). It was 26; the design's metrics are physical sizes on this
/// machine and transfer as written (Part A's metrics decision). The gates carry their own copy,
/// on purpose (M11 decision 2).
pub const TITLE_BAR_H: u32 = 31;

/// How wide each title-bar button is: the design's 23.
pub const TITLE_BUTTON_W: u32 = 23;

/// How tall the face of each is — 21, centred in the bar. The press lands anywhere in the
/// button's column of the bar, which is the easier target and the one a gate aims at.
pub const TITLE_BUTTON_H: u32 = 21;

/// The gap between two title-bar buttons: the design's 9.
pub const TITLE_BUTTON_GAP: u32 = 9;

/// The space after the last button, before the window's border: the design's 5.
pub const TITLE_BUTTON_PAD: u32 = 5;

/// The corner of a title-bar button's hover face: the design's 2.
pub(super) const TITLE_BUTTON_RADIUS: u32 = 2;

/// The keys a title bar's buttons carry, so a caller's `hovered` can say which one the pointer is
/// over (desktop refresh, after Part K).
///
/// **The toolkit's, from the top of the key space down**, beside the `+`'s key at `u64::MAX`:
/// an application's own keys are small numbers and its tab keys start at `1 << 62` or `1 << 63`
/// with a small offset, so nothing a caller writes comes within reach of these. Each window is its
/// own tree, so two windows' title bars carrying the same three keys is not a collision.
pub const TITLE_MINIMISE_KEY: u64 = u64::MAX - 1;

/// See [`TITLE_MINIMISE_KEY`].
pub const TITLE_MAXIMISE_KEY: u64 = u64::MAX - 2;

/// See [`TITLE_MINIMISE_KEY`].
pub const TITLE_CLOSE_KEY: u64 = u64::MAX - 3;

/// The centre of the `nth` title-bar button counting from the **right** — the close button is
/// `0` — in a framed window `window_w` wide, in the window's own coordinates.
///
/// **For an application's tests, not for a gate**: a gate keeps its own copy of these numbers
/// (M11 decision 2), and this exists so the three applications stop each re-deriving the
/// arithmetic, which is how one of them came to aim a slot to the left.
pub const fn title_button_centre(window_w: u32, nth: u32) -> (i32, i32) {
    let right = window_w - WINDOW_BORDER - TITLE_BUTTON_PAD;
    let x = right - nth * (TITLE_BUTTON_W + TITLE_BUTTON_GAP) - TITLE_BUTTON_W / 2 - 1;
    (x as i32, (WINDOW_CONTENT_Y + (TITLE_BAR_H - 1) / 2) as i32)
}

/// A window's title bar: its name, and the three things you can do to a window.
///
/// **This is what makes decorations client-side.** The compositor draws no chrome and knows no
/// theme (M9 decision 1); a window's title bar is part of the pixels its own client commits,
/// like every other pixel, so nothing about "the window's rectangle" has to mean two things.
///
/// The `drag` message is sent when a press goes **down** on the bar — not on the click, which is
/// a release, and by then the user has finished the gesture. An application answers it by asking
/// the compositor for an interactive move: the compositor already holds the pointer grab the
/// press opened, so all the client contributes is "that press was on a part of me that moves the
/// window". It cannot compute the move itself — it does not know where it is on screen, and
/// `Place` is deliberately a manager op.
///
/// A press on one of the buttons does *not* drag, because a nearer `on_press` shadows the bar's
/// `on_press_down`. That rule is the toolkit's, not this widget's.
///
/// **The buttons are ordinary buttons and the bar is an ordinary `stack`.** A title bar is
/// chrome by convention rather than by mechanism, which is what lets any client draw one — and
/// what would let a client draw something else entirely, which is a property of client-side
/// decorations and not a defect in this widget.
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
    // **A glyph, not a letter** (M11 Part E, batch 2). These were `_`, `[]` and `X` — three
    // characters standing in for three controls, which read as text on a bar full of text.
    //
    // **In the dim ink, on no face of their own** — the design's controls are borderless and
    // `--fgdim`, and read as part of the bar rather than as three buttons on it. Each takes its
    // whole column of the bar as a target and draws its glyph in the middle of the design's 23x21.
    //
    // **Until the pointer is over one** (desktop refresh, after Part K). A borderless glyph gives
    // no sign it is a control, so the design lights it on hover: minimise and maximise on
    // `--faceLo` (`face_pressed`) with the glyph in full ink, close on `--deny` with a light one.
    // **Red for close alone** — the one button of the three that destroys something, and the one
    // a person should see coming before they press it.
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
                // The further of the scheme's two inks from the red — the dark scheme's paper is
                // near-black, so a fixed white would be right in one scheme only (the `pill`'s
                // reasoning, Part H).
                furthest_from(theme.deny, theme.background, theme.foreground)
            }
        };
        layers.push(ink(glyph_ink, icon(glyph)));
        sized(Size::new(TITLE_BUTTON_W, 0), center(sized(Size::new(TITLE_BUTTON_W, TITLE_BUTTON_H), stack(layers))))
            .on_press(msg)
            .key(key)
    };
    // **A button a caller has no message for is not drawn.** The alternative is a button that
    // does nothing, and a control that looks live and is not is the defect this milestone's
    // predecessor shipped three of (M8's overview). The buttons arrive with the parts that give
    // them somewhere to go: minimise and maximise in Part B, close in Part C.
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
    // **Bold, at the body's size** (desktop refresh, Part G): the design heads a window with its
    // title at the size of the text inside it and a heavier weight. DejaVu's bold is heavier than
    // the design's semibold already, so a size step on top overshoots — measured on a screendump
    // against the menu's "File", the title is 1.62× its width at the body size and 1.75× a step
    // up, where the design's is 1.47×.
    //
    // **And a dim subtitle beside it** (Part H): the design says *what* a window is showing next
    // to what it is — a browser's directory, an editor's kind. It is the body size in the dim
    // ink rather than a size below, which is what the design does and what keeps two words on
    // one line from reading as a heading and a footnote.
    let mut name = alloc::vec::Vec::with_capacity(2);
    name.push(bold(text(title)));
    if let Some(subtitle) = subtitle.filter(|s| !s.is_empty()) {
        name.push(ink(theme.foreground_dim, text(subtitle)));
    }
    let title = center_v(with_spacing(row(name), TITLE_SUBTITLE_GAP));
    controls.push(padding(TITLE_PAD, title).flex(1));
    // Each button sets its own ink now, since a hovered one differs from its neighbours.
    controls.push(crate::element::with_spacing(row(glyphs), TITLE_BUTTON_GAP));
    controls.push(sized(Size::new(TITLE_BUTTON_PAD, 0), text("")));
    // **The drag is on the bar itself, not on the face underneath the label.** Dispatch walks
    // *up* from whatever was hit to the nearest handler, and the label spans the bar — so a
    // handler on the face below it is never reached, and the first version of this widget
    // produced nothing at all for a press in the middle of its own title. On the bar, a press
    // that lands on the label or on empty space walks up to here, and one that lands on a button
    // stops at the button, because that is where the walk finds a handler first.
    // **A rule along the bottom**, the design's `border-bottom: 1px solid var(--line)`: it is what
    // separates a title bar from a menu bar the same colour as it, and it is inside `TITLE_BAR_H`.
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
/// `None` is "do not draw it". See [`title_bar`] for why that is not the same as a button that
/// does nothing.
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
/// **Written out rather than derived.** `#[derive(Default)]` on a generic struct demands
/// `Msg: Default`, which is a bound on the *application's* message type for no reason: every
/// field here is an `Option` and its default is `None` whatever `Msg` is.
impl<Msg> Default for TitleButtons<Msg> {
    fn default() -> Self {
        Self { minimise: None, maximise: None, close: None }
    }
}

/// Space between a title bar's text and its edge: the design's 11 on the left, and the text
/// centred down the bar.
const TITLE_PAD: Insets = Insets { top: 0, right: 6, bottom: 0, left: 11 };

/// Between a window's title and the dim subtitle beside it: the design's 9.
const TITLE_SUBTITLE_GAP: u32 = 9;
