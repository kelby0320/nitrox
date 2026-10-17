//! Window, popup and dialog frames.

use super::*;

/// A popup's backing: a one-pixel border, and the face inside it.
///
/// A popup has nothing behind it to define its edge, so without a line its light face bleeds
/// into a light window. The border is an outline drawn last, along the curve the compositor
/// cuts to [`WINDOW_RADIUS`](libdraw::corner::WINDOW_RADIUS).
pub fn popup_frame<Msg>(content: Element<Msg>, theme: &Theme) -> Element<Msg> {
    // On the window's own ground, the design's `--bg`: a menu is a list to read, as on paper.
    stack(alloc::vec![
        fill(theme.background),
        padding(Insets::all(POPUP_BORDER), content),
        outline(theme.border, libdraw::corner::WINDOW_RADIUS),
    ])
}

/// How thick a popup's border is; public so the greeter's card can size its window from it.
pub const POPUP_BORDER: u32 = 1;

/// A window's own frame: an edge, and a margin between its content and that edge.
///
/// The title bar is flush and the content is not: insetting the bar would put a strip of frame
/// above what you grab to move the window.
pub fn window_frame<Msg>(title: Element<Msg>, content: Element<Msg>, focused: bool, theme: &Theme) -> Element<Msg> {
    frame_layers(title, content, None, focused, theme)
}

/// [`window_frame`]'s layers, with an optional corner layer drawn before the border.
fn frame_layers<Msg>(
    title: Element<Msg>,
    content: Element<Msg>,
    corner: Option<Element<Msg>>,
    focused: bool,
    theme: &Theme,
) -> Element<Msg> {
    // Both children wrapped, and the zero-inset one is not decoration: the diff requires a
    // container's children to be all keyed or all unkeyed, and every caller keys its title bar,
    // so an unkeyed content pane beside it is a `MixedKeying` error at the first frame.
    let inner = dock(
        alloc::vec![docked(Edge::Top, padding(Insets::all(0), title))],
        padding(Insets { top: 0, right: WINDOW_FRAME, bottom: WINDOW_FRAME, left: WINDOW_FRAME }, content),
    );
    // The border is drawn last and round: the compositor cuts a window's corners to
    // `WINDOW_RADIUS`, which would take a square border's corners off.
    let mut layers = alloc::vec![fill(theme.face), padding(Insets::all(WINDOW_BORDER), inner)];
    layers.extend(corner);
    // The focused window's edge is the accent, in addition to the tinted bar. A client knows its
    // own focus, so nothing in the compositor has to say which window is which.
    let edge = if focused { theme.accent } else { theme.border };
    layers.push(outline(edge, libdraw::corner::WINDOW_RADIUS));
    stack(layers)
}

/// [`window_frame`], with a resize grip in its bottom-right corner, inside the border.
///
/// The grip is a layer before the outline, so the border is drawn over it; an `offset` sized to
/// the grip keeps it from taking presses elsewhere.
pub fn window_frame_with_grip<Msg>(
    title: Element<Msg>,
    content: Element<Msg>,
    grip: Element<Msg>,
    window: Size,
    focused: bool,
    theme: &Theme,
) -> Element<Msg> {
    let at = |len: u32| len.saturating_sub(WINDOW_BORDER + GRIP_W) as i32;
    let corner = crate::element::offset(at(window.w), at(window.h), grip);
    frame_layers(title, content, Some(corner), focused, theme)
}

/// How thick the line around a window is.
pub const WINDOW_BORDER: u32 = 1;

/// How much frame shows between a window's content and its edge: none, kept as a named zero
/// for the `WINDOW_FRAME_W` and `WINDOW_FRAME_H` sums.
pub const WINDOW_FRAME: u32 = 0;

/// What [`window_frame`] takes off a window's width, published so applications' sizes match.
pub const WINDOW_FRAME_W: u32 = 2 * (WINDOW_BORDER + WINDOW_FRAME);

/// What it takes off the height, in addition to the title bar.
pub const WINDOW_FRAME_H: u32 = WINDOW_BORDER * 2 + WINDOW_FRAME;

/// Where a framed window's content starts, horizontally.
pub const WINDOW_CONTENT_X: u32 = WINDOW_BORDER + WINDOW_FRAME;

/// Where it starts vertically — this, plus [`TITLE_BAR_H`].
pub const WINDOW_CONTENT_Y: u32 = WINDOW_BORDER;

/// A confirmation dialog's width in pixels.
///
/// A size, not a measurement: `check-login` aims with arithmetic off the origin the shell logs.
/// `dialog_buttons_land_where_the_constants_say` pins the aim points to a tree actually built.
pub const DIALOG_W: u32 = 340;

/// A confirmation dialog's height in pixels.
pub const DIALOG_H: u32 = 132;

/// The margin between a dialog's frame and the button strip inside it.
pub const DIALOG_PAD: u32 = 12;

/// The gap between a dialog's two buttons.
pub const DIALOG_GAP: u32 = 8;

/// How tall each of a dialog's buttons is.
pub const DIALOG_BUTTON_H: u32 = 26;

/// How wide each is — half of what is left after the frame, the margins and the gap.
pub const DIALOG_BUTTON_W: u32 = (DIALOG_W - WINDOW_FRAME_W - 2 * DIALOG_PAD - DIALOG_GAP) / 2;

/// The centre of a dialog's **left** button, in the dialog window's own coordinates.
pub const DIALOG_LEFT_CX: i32 = (WINDOW_CONTENT_X + DIALOG_PAD + DIALOG_BUTTON_W / 2) as i32;

/// The centre of its **right** button, likewise.
pub const DIALOG_RIGHT_CX: i32 =
    (WINDOW_CONTENT_X + DIALOG_PAD + DIALOG_BUTTON_W + DIALOG_GAP + DIALOG_BUTTON_W / 2) as i32;

/// The vertical centre of both, measured up from the dialog's bottom edge.
pub const DIALOG_BUTTON_CY: i32 = (DIALOG_H - WINDOW_BORDER - WINDOW_FRAME - DIALOG_PAD - DIALOG_BUTTON_H / 2) as i32;

/// A confirmation dialog's whole face: a title bar, a question, and two answers.
///
/// Sized rather than measured, so it can be a [`Child`](crate::window::Child): `Node::Dock`
/// measures as everything it is offered, and `Child::open` refuses a tree with no natural size.
/// `buttons` must be a `row` of exactly two children, each `.flex(1)`, or the published centres
/// name nothing; `dialog_buttons_land_where_the_constants_say` fails a caller that breaks it.
pub fn dialog_frame<Msg>(
    title: Element<Msg>,
    question: Element<Msg>,
    buttons: Element<Msg>,
    focused: bool,
    theme: &Theme,
) -> Element<Msg> {
    dialog_frame_sized(Size::new(DIALOG_W, DIALOG_H), title, question, buttons, focused, theme)
}

/// The same face at a size the caller picks; the published aim points describe the fixed size.
pub fn dialog_frame_sized<Msg>(
    size: Size,
    title: Element<Msg>,
    question: Element<Msg>,
    buttons: Element<Msg>,
    focused: bool,
    theme: &Theme,
) -> Element<Msg> {
    let strip = sized(
        Size::new(0, DIALOG_BUTTON_H + DIALOG_PAD),
        padding(Insets { top: 0, right: DIALOG_PAD, bottom: DIALOG_PAD, left: DIALOG_PAD }, buttons),
    );
    sized(
        size,
        window_frame(
            title,
            dock(alloc::vec![docked(Edge::Bottom, strip)], padding(Insets::all(0), question)),
            // A dialog's focus is the caller's to report: one that has lost the keyboard to
            // another window must not be edged in the accent too.
            focused,
            theme,
        ),
    )
}

/// The side of the square a window's resize grip occupies, in pixels.
pub const GRIP_W: u32 = 16;

/// A resize grip: a corner a window can be dragged bigger by.
///
/// Client-side: the message becomes `Surface::StartResize` and the compositor runs the gesture.
/// It is `on_press_down` because the gesture begins at the press.
pub fn resize_grip<Msg: Clone>(msg: Msg, theme: &Theme) -> Element<Msg> {
    // Three nested corner bands drawn as squares, so the grip needs no font or theme. Each pair
    // paints a band and then punches its middle back out.
    let mut layers = alloc::vec::Vec::with_capacity(4);
    layers.push(fill(theme.face));
    for i in 0..3u32 {
        let inset = i * 5;
        layers.push(padding(Insets { top: inset + 2, right: 2, bottom: 2, left: inset + 2 }, fill(theme.track)));
        layers.push(padding(Insets { top: inset + 4, right: 4, bottom: 4, left: inset + 4 }, fill(theme.face)));
    }
    sized(Size::new(GRIP_W, GRIP_W), stack(layers).on_press_down(msg))
}
