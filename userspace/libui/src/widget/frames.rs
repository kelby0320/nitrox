use super::*;

/// A popup's backing: a one-pixel border, and the face inside it.
///
/// **Because a popup is the one surface with nothing behind it to define its edge.** A window has
/// a frame and a bar has the screen's edge; a menu is a rectangle floating over whatever it
/// covers, and without a line around it the light face bleeds into a light window underneath. The
/// reference desktop draws exactly this, and it is the only reason `border` is a theme colour.
///
/// One helper, so the applications modal and a menu cannot disagree about what a popup looks
/// like — they are the same kind of thing seen twice.
///
/// **Rounded since the desktop refresh's Part B**, because the compositor cuts a popup's corners
/// to [`WINDOW_RADIUS`](libdraw::corner::WINDOW_RADIUS): the border is an
/// [`outline`](crate::element::outline) drawn last, along that curve, rather than a square
/// fill the cut would have taken the corners off.
pub fn popup_frame<Msg>(content: Element<Msg>, theme: &Theme) -> Element<Msg> {
    // **On the window's own ground**, the design's `--bg`, rather than a button's face: a menu is
    // a list of things to read, and it reads as one on paper (desktop refresh, Part B).
    stack(alloc::vec![
        fill(theme.background),
        padding(Insets::all(POPUP_BORDER), content),
        outline(theme.border, libdraw::corner::WINDOW_RADIUS),
    ])
}

/// How thick a popup's border is.
///
/// **Public since the desktop refresh's Part D**, because the greeter builds its card from
/// [`popup_frame`] and sizes its window to the content: the inset between the two is this, and a
/// second 1 written down there would be a second number to keep equal.
pub const POPUP_BORDER: u32 = 1;

/// A window's own frame: an edge, and a margin between its content and that edge.
///
/// **The title bar is flush and the content is not**, which is what the reference desktop does
/// and is not arbitrary: a title bar is the window's edge — it is what you grab to move it — and
/// insetting it would put a strip of frame above a bar that already reads as one. The content
/// below is what wants breathing room.
///
/// Takes the title and the rest separately rather than wrapping a finished tree, because those
/// two are exactly the parts that are treated differently. An application that wants no frame
/// simply does not call this; the greeter does not, having no title bar to be flush with.
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
    // **Both children wrapped, and the zero-inset one is not decoration.** The diff requires a
    // container's children to be all keyed or all unkeyed, and every caller keys its title bar —
    // so docking the title directly beside an unkeyed content pane is a `MixedKeying` error at
    // the first frame. Wrapping both puts the keys one level down, inside their own parents,
    // where they still do their job; the alternative was for this helper to invent a key in the
    // application's own namespace.
    let inner = dock(
        alloc::vec![docked(Edge::Top, padding(Insets::all(0), title))],
        padding(Insets { top: 0, right: WINDOW_FRAME, bottom: WINDOW_FRAME, left: WINDOW_FRAME }, content),
    );
    // **The border is drawn last, and round** (desktop refresh, Part B). It was a square fill
    // under a face inset by a pixel; the compositor now cuts a window's corners to
    // `WINDOW_RADIUS`, which would take a square border's corners off with them. So the edge is
    // an outline along the same curve, painted over the content it curves into.
    let mut layers = alloc::vec![fill(theme.face), padding(Insets::all(WINDOW_BORDER), inner)];
    layers.extend(corner);
    // **The focused window's edge is the accent** (desktop refresh, Part H) — the design's own
    // focus cue, and the one it uses instead of tinting a title bar. We keep the tinted bar as
    // well (the plan's deliberate divergence), so this is additive: a client knows its own focus
    // and draws its own edge, and nothing in the compositor has to say which window is which.
    let edge = if focused { theme.accent } else { theme.border };
    layers.push(outline(edge, libdraw::corner::WINDOW_RADIUS));
    stack(layers)
}

/// [`window_frame`], with a resize grip in its bottom-right corner — **inside the border, and
/// under it**.
///
/// **Why the frame places the grip rather than the application.** All three applications stacked
/// their grip over the finished frame, which painted its face over the last sixteen pixels of the
/// bottom and right border and over the whole bottom-right curve, so the one corner a person
/// reaches for was the one corner whose border was missing (PR #313 review, optional 4). Placed
/// here, the grip is a layer *before* the outline, so the border is drawn over it along the curve,
/// and it sits inside the border rather than on it.
///
/// `window` is the window's own size, which is where the corner is; an `offset` sized to the grip
/// is what keeps it from taking presses anywhere else — a full-size overlay would be the outline's
/// bug again.
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

/// How much frame shows between a window's content and its edge.
///
/// **None, since the desktop refresh's Part B**: the design's content runs flush to the border.
/// It was 3, a margin of the button face around everything below the title bar — the reference
/// desktop's, which the design does not have. Kept as a named zero because applications subtract
/// the frame from their content size through `WINDOW_FRAME_W` and `WINDOW_FRAME_H`, and those
/// still mean what they say.
pub const WINDOW_FRAME: u32 = 0;

/// What [`window_frame`] takes off a window's width before its content sees it.
///
/// **Published because applications compute their own content size**, and they have to compute
/// the same one this draws — a widget built for one height and laid out at another is the bug
/// each of their `*_h()` methods already carries a comment about. Three constants rather than
/// three open-coded sums.
pub const WINDOW_FRAME_W: u32 = 2 * (WINDOW_BORDER + WINDOW_FRAME);

/// What it takes off the height, *in addition* to the title bar: the top border, and the frame
/// and border below the content.
pub const WINDOW_FRAME_H: u32 = WINDOW_BORDER * 2 + WINDOW_FRAME;

/// Where a framed window's content starts, horizontally.
pub const WINDOW_CONTENT_X: u32 = WINDOW_BORDER + WINDOW_FRAME;

/// Where it starts vertically — this, plus [`TITLE_BAR_H`].
pub const WINDOW_CONTENT_Y: u32 = WINDOW_BORDER;

/// A confirmation dialog's width in pixels.
///
/// **A size, not a measurement**, which is the opposite of what a menu does, and the reason is
/// the gate. `check-login` presses buttons in these windows and aims with arithmetic off the
/// origin the shell logs; buttons that resized with the name of the file being asked about would
/// move under it. §11's "chrome metrics are not themeable" is the same argument one level up.
///
/// **Here rather than in an application** since M12 Part B, when `nxfiles` grew the second one.
/// `nxedit` published these five and derived four aim points from them; a browser repeating the
/// arithmetic would give the gate two tables to keep in step, which is the shape that goes wrong
/// silently. The [`dialog_frame`] below is the other half — the measurable frame — and the test
/// beside it is what pins the aim points to a tree that is actually built.
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
/// **Sized rather than measured**, which is what lets it be a [`Child`](crate::window::Child) at
/// all: `Node::Dock` measures as *everything it is offered* — deliberately, since a dock's job is
/// to divide a given area — so a tree containing one has no natural size and `Child::open`
/// refuses it. The fixed wrapper here is what makes the measurement exact.
///
/// `buttons` must be a `row` of exactly two children, **each `.flex(1)`**, or the published
/// centres above name nothing. That contract is not a comment: `dialog_buttons_land_where_the_
/// constants_say` builds one and presses both, so a caller that breaks it fails a host test
/// rather than a three-minute boot.
///
/// Keys stay the caller's. This helper invents none, for the reason [`window_frame`] does not:
/// a key is the application's own numbering, and a widget that assigned one would be reaching
/// into it.
pub fn dialog_frame<Msg>(
    title: Element<Msg>,
    question: Element<Msg>,
    buttons: Element<Msg>,
    focused: bool,
    theme: &Theme,
) -> Element<Msg> {
    dialog_frame_sized(Size::new(DIALOG_W, DIALOG_H), title, question, buttons, focused, theme)
}

/// The same face at a size the caller picks.
///
/// **A dialog that is not a question needs its own size**, and the fixed one above is the reason
/// this exists: [`DIALOG_W`]×[`DIALOG_H`] is 340×132, which fits a two-line question and two
/// buttons and nothing else. The file chooser put a list and a name field inside it and got a
/// list one and a half rows tall and a field with no pixels at all — the widget honoured neither
/// height it was built for, because the frame had already decided the window was 132 tall
/// (PR #284 review, blocking 1).
///
/// The *sizing* is what must not be dropped: `Node::Dock` measures as everything it is offered,
/// so a tree containing one has no natural size and `Child::open` refuses it. What varies is the
/// number, not whether there is one.
///
/// The published aim points — [`DIALOG_LEFT_CX`] and friends — describe the fixed size only. A
/// caller of this owns its own geometry, and the chooser's buttons are right-aligned rather than
/// two halves, so they would name nothing here.
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
    // **Both dock children wrapped, and the zero-inset one is not decoration** — the same rule
    // [`window_frame`] states and for the same reason: the diff requires a container's children
    // to be all keyed or all unkeyed, and a caller keys its question. Docking the strip this
    // helper built beside it is a `MixedKeying` error on the first frame, which in a client like
    // this means the dialog never appears at all.
    sized(
        size,
        window_frame(
            title,
            dock(alloc::vec![docked(Edge::Bottom, strip)], padding(Insets::all(0), question)),
            // **A dialog's focus is the caller's to report**, like any other window's. This
            // passed `true`, on the reasoning that a dialog is up because something is waiting
            // on it — but the state that matters is a dialog that has *lost* the keyboard to
            // another window, which is a click away and which every caller already tracks for
            // its own title bar. Two windows edged in the accent is exactly what those flags
            // exist to prevent (PR #319 review, blocking 1).
            focused,
            theme,
        ),
    )
}

/// The side of the square a window's resize grip occupies, in pixels.
///
/// Big enough to hit without aiming — the corner is the one place on a window where a person
/// expects to be able to grab roughly — and small enough not to swallow the content under it,
/// since the grip is drawn *over* the window's own bottom-right rather than reserving a strip.
pub const GRIP_W: u32 = 16;

/// A resize grip: a corner a window can be dragged bigger by.
///
/// **Client-side, like every other piece of chrome here** (decision 1 of Milestone 9), and like
/// the title bar it *asks*: the message it sends becomes `Surface::StartResize`, the compositor
/// runs the gesture, and the manager sends the `Configure` at the end. This widget draws a
/// corner and reports a press; it knows nothing about rectangles.
///
/// **`on_press_down`, not `on_press`**, for the reason the title bar's drag uses it: the gesture
/// begins at the press. A grip that waited for the click would hand the compositor a drag whose
/// button was already up.
///
/// Positioned by its caller — an application stacks it over its own bottom-right corner, which
/// is the one place this widget cannot work out for itself.
pub fn resize_grip<Msg: Clone>(msg: Msg, theme: &Theme) -> Element<Msg> {
    // Three nested corner bands — the conventional grip — drawn as squares of the groove
    // colour rather than as glyphs: a grip that needed a font would need a theme, and this has
    // neither. Each pair paints a band and then punches its middle back out.
    let mut layers = alloc::vec::Vec::with_capacity(4);
    layers.push(fill(theme.face));
    for i in 0..3u32 {
        let inset = i * 5;
        layers.push(padding(Insets { top: inset + 2, right: 2, bottom: 2, left: inset + 2 }, fill(theme.track)));
        layers.push(padding(Insets { top: inset + 4, right: 4, bottom: 4, left: inset + 4 }, fill(theme.face)));
    }
    sized(Size::new(GRIP_W, GRIP_W), stack(layers).on_press_down(msg))
}
