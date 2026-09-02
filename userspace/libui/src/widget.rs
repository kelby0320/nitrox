//! The widget set, bounded by what Milestone 5's terminal needs.
//!
//! **Widgets are functions returning [`Element`]s**, not node kinds. A button is a fill with
//! a label on it that happens to carry a message; the runtime knows nothing about buttons.
//! That falls out of the declarative model rather than being a decision on top of it — and it
//! means a widget's correctness is the correctness of a pure function returning a tree, which
//! is the cheapest kind of thing to test in this repository.
//!
//! ## Widgets take their state as an argument
//!
//! There is no hover flag hidden inside a `Button`. `view(&state) -> Element` is the model, so
//! whether a button is hovered is *state*, and the application passes it in — reading it from
//! [`Router::inside`](crate::route::Router::inside) if it wants to. The alternative is
//! interaction state retained in the widget tree, which `widget-toolkit.md` §3 reserves for
//! things the application has no opinion about, and a button's appearance is not one of them.
//!
//! That keeps every widget here a pure function of its arguments. It also means a widget
//! cannot animate on its own, which is deferred along with the frame clock (§11).

use alloc::string::String;
use alloc::vec::Vec;
use libdraw::geom::Size;
// **Re-exported, because every widget here takes one.** A caller importing `button` and
// `list_view` from this module should not have to reach into `libdraw` for the third argument
// they all share; `libui::paint::Theme` names the same type for the painting half.
pub use libdraw::theme::Theme;

use librsproto::surface::PointerEvent;

use crate::element::{
    Edge, Element, IconKind, Insets, bevel, column, dock, docked, fill, icon, padding, row, sized,
    stack, text,
};
// The editing keys. **Imported, not re-declared** — `libkern::abi` publishes these and
// `libterm::encode` already imports exactly this set from there, so a second copy is a second
// thing that can disagree about a key. The same argument the `libinput` dependency is
// justified by, applied to the codes as well as the mapping (PR #233 review, finding 3).
// `libinput::keymap` does not map them because it answers "what text does this produce", and
// these produce none.
use libkern::abi::{
    KEY_BACKSPACE, KEY_DELETE, KEY_DOWN, KEY_END, KEY_ENTER, KEY_HOME, KEY_LEFT, KEY_RIGHT,
    KEY_UP,
};
use librsproto::surface::MOD_SHIFT;

/// How a widget should look, given what the application knows about it.
///
/// Passed in rather than remembered, so a widget stays a function of its arguments.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct WidgetState {
    /// The pointer is inside it.
    pub hovered: bool,
    /// A button is held down on it.
    pub pressed: bool,
    /// It holds widget focus **and** its window has the keyboard — see
    /// [`Focus::is_active`](crate::route::Focus::is_active). One bool here rather than two,
    /// because a widget paints the same whichever of the two is missing.
    pub active: bool,
}

/// Space between a button's label and its edge.
const BUTTON_PAD: Insets = Insets { top: 4, right: 8, bottom: 4, left: 8 };

/// A labelled button that sends `msg` when clicked.
///
/// Focusable, because a keyboard user must be able to reach it — and the focus ring is drawn
/// from `state.active` rather than from focus alone, so a button in an unfocused window does
/// not claim the keyboard it does not have.
pub fn button<Msg>(
    label: impl Into<String>,
    msg: Msg,
    state: WidgetState,
    theme: &Theme,
) -> Element<Msg> {
    let face = if state.pressed {
        theme.face_pressed
    } else if state.hovered {
        theme.face_hover
    } else {
        theme.face
    };
    // Bottom to top: the ring, then the face, then the label. A `Stack` gives every layer
    // the whole area, so the ring is only visible because the face above it is inset by the
    // ring's width — and the label sits above both, inset further so it clears the edge.
    let mut layers = alloc::vec::Vec::with_capacity(3);
    if state.active {
        layers.push(fill(theme.focus_ring));
        layers.push(padding(Insets::all(RING), fill(face)));
    } else {
        layers.push(fill(face));
    }
    layers.push(padding(BUTTON_PAD, text(label)));
    stack(layers).on_press(msg).focusable()
}

/// A popup's backing: a one-pixel border, and the face inside it.
///
/// **Because a popup is the one surface with nothing behind it to define its edge.** A window has
/// a frame and a bar has the screen's edge; a menu is a rectangle floating over whatever it
/// covers, and without a line around it the light face bleeds into a light window underneath. The
/// reference desktop draws exactly this, and it is the only reason `border` is a theme colour.
///
/// One helper, so the applications modal and a menu cannot disagree about what a popup looks
/// like — they are the same kind of thing seen twice.
pub fn popup_frame<Msg>(content: Element<Msg>, theme: &Theme) -> Element<Msg> {
    stack(alloc::vec![
        fill(theme.border),
        padding(Insets::all(POPUP_BORDER), fill(theme.face)),
        padding(Insets::all(POPUP_BORDER), content),
    ])
}

/// How thick a popup's border is.
const POPUP_BORDER: u32 = 1;

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
pub fn window_frame<Msg>(title: Element<Msg>, content: Element<Msg>, theme: &Theme) -> Element<Msg> {
    // **Both children wrapped, and the zero-inset one is not decoration.** The diff requires a
    // container's children to be all keyed or all unkeyed, and every caller keys its title bar —
    // so docking the title directly beside an unkeyed content pane is a `MixedKeying` error at
    // the first frame. Wrapping both puts the keys one level down, inside their own parents,
    // where they still do their job; the alternative was for this helper to invent a key in the
    // application's own namespace.
    let inner = dock(
        alloc::vec![docked(Edge::Top, padding(Insets::all(0), title))],
        padding(
            Insets { top: 0, right: WINDOW_FRAME, bottom: WINDOW_FRAME, left: WINDOW_FRAME },
            content,
        ),
    );
    stack(alloc::vec![
        fill(theme.border),
        padding(Insets::all(WINDOW_BORDER), fill(theme.face)),
        padding(Insets::all(WINDOW_BORDER), inner),
    ])
}

/// How thick the line around a window is.
pub const WINDOW_BORDER: u32 = 1;

/// How much frame shows between a window's content and its edge.
pub const WINDOW_FRAME: u32 = 3;

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
pub const DIALOG_BUTTON_CY: i32 =
    (DIALOG_H - WINDOW_BORDER - WINDOW_FRAME - DIALOG_PAD - DIALOG_BUTTON_H / 2) as i32;

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
    theme: &Theme,
) -> Element<Msg> {
    let strip = sized(
        Size::new(0, DIALOG_BUTTON_H + DIALOG_PAD),
        padding(
            Insets { top: 0, right: DIALOG_PAD, bottom: DIALOG_PAD, left: DIALOG_PAD },
            buttons,
        ),
    );
    // **Both dock children wrapped, and the zero-inset one is not decoration** — the same rule
    // [`window_frame`] states and for the same reason: the diff requires a container's children
    // to be all keyed or all unkeyed, and a caller keys its question. Docking the strip this
    // helper built beside it is a `MixedKeying` error on the first frame, which in a client like
    // this means the dialog never appears at all.
    sized(
        Size::new(DIALOG_W, DIALOG_H),
        window_frame(
            title,
            dock(alloc::vec![docked(Edge::Bottom, strip)], padding(Insets::all(0), question)),
            theme,
        ),
    )
}

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
pub fn menu_item<Msg: Clone>(
    label: &str,
    msg: Msg,
    hovered: bool,
    theme: &Theme,
) -> Element<Msg> {
    let mut layers = alloc::vec::Vec::with_capacity(3);
    if hovered {
        layers.push(fill(theme.focus_ring));
        layers.push(padding(Insets::all(1), bevel(theme.selection)));
    }
    layers.push(padding(MENU_ITEM_PAD, text(label)));
    stack(layers).on_press(msg)
}

/// The space around a menu item's label.
///
/// Wider than a button's, because a menu is a column of text rather than a control: the reading
/// is horizontal and the eye needs the gutter.
const MENU_ITEM_PAD: Insets = Insets { top: 3, right: 10, bottom: 3, left: 10 };

/// How wide the focus ring is, in pixels.
const RING: u32 = 2;

/// Where a scrollbar is and how much of its content is visible.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScrollState {
    /// First visible unit — a line, for a terminal.
    pub offset: u32,
    /// How many units are visible at once.
    pub visible: u32,
    /// How many units there are in total.
    pub total: u32,
}

/// The smallest a thumb may get, in pixels.
///
/// A thumb proportional to a long scrollback shrinks to nothing and stops being grabbable;
/// every real scrollbar clamps it, and the clamp is what makes the position arithmetic
/// non-obvious enough to be worth testing.
pub const MIN_THUMB: u32 = 16;

impl ScrollState {
    /// Whether there is anything to scroll.
    pub fn scrollable(&self) -> bool {
        self.total > self.visible
    }

    /// The thumb's length and offset along a track of `track` pixels.
    ///
    /// Returns `(offset, length)`. With nothing to scroll the thumb fills the track, which is
    /// how a user sees "this is all of it" rather than seeing a full-height thumb that
    /// nevertheless moves.
    pub fn thumb(&self, track: u32) -> (u32, u32) {
        if !self.scrollable() || self.visible == 0 || track == 0 {
            return (0, track);
        }
        // Proportional, then clamped — and clamped *before* the position is computed, or the
        // thumb runs past the end of the track by however much the clamp added.
        let proportional = (track as u64 * self.visible as u64 / self.total as u64) as u32;
        let len = proportional.max(MIN_THUMB).min(track);
        let span = track - len;
        let max_offset = self.total - self.visible;
        let pos = (span as u64 * self.offset.min(max_offset) as u64 / max_offset as u64) as u32;
        (pos, len)
    }

    /// The offset that puts the thumb's **centre** at `y` pixels down a track of `track`.
    ///
    /// [`thumb`](Self::thumb)'s inverse, and the half M4 did not ship: the toolkit could say
    /// where a thumb goes for a given offset but not what offset a grab means, so a scrollbar
    /// was a picture of a scrollbar. The terminal is the first thing to want to *use* one,
    /// which is the milestone rule working as intended — see the decision log, 2026-08-12.
    ///
    /// **The thumb's centre rather than its top**, so that clicking a spot on the track jumps
    /// to it and a drag holds the thumb under the cursor. It follows that a grab anywhere on
    /// the thumb re-centres it — the small jump every toolkit without a grab *offset* has, and
    /// removing it needs interaction state the toolkit does not yet keep (`TODO(scroll-grab)`).
    ///
    /// `y` is signed because a drag routinely leaves the widget: the router hands a captured
    /// widget negative coordinates rather than clamping, and this clamps at the ends instead —
    /// which is what makes dragging past the bottom stay at the bottom.
    pub fn offset_at(&self, track: u32, y: i32) -> u32 {
        let max_offset = self.total.saturating_sub(self.visible);
        if max_offset == 0 || track == 0 {
            return 0;
        }
        let (_, len) = self.thumb(track);
        let span = track.saturating_sub(len);
        if span == 0 {
            // A thumb filling its track cannot express a position. Anything but the top would
            // be invented, and `MIN_THUMB` makes this reachable on a short bar.
            return 0;
        }
        let pos = (y - len as i32 / 2).clamp(0, span as i32) as u32;
        // **Truncating, like [`thumb`](Self::thumb)**, which is what keeps the pair consistent:
        // the round trip is exact wherever the division is, and neither end needs help — at the
        // bottom `pos` is `span`, so `span * max / span` is `max` however it rounds. A first
        // version added `span / 2` here for the stated reason that the last line was otherwise
        // unreachable, which is simply not true; deleting it left every test green, so it was
        // half a line of mid-drag accuracy bought with a claim that did not hold.
        ((pos as u64 * max_offset as u64) / span as u64) as u32
    }
}

/// A vertical scrollbar `width` pixels across.
///
/// Built out of layout rather than arithmetic on a canvas: a `Column` of a spacer, the thumb
/// and a filler places the thumb without any node needing to offset its child. That is why
/// the toolkit has no `Offset` primitive — this was the thing that would have wanted one.
///
/// **`height` must be the height the parent will actually give it.** The bar sizes itself
/// `width × 0` — full height of whatever slot it lands in, per `sized`'s zero-axis rule — but
/// `height` is what the thumb's length and position are computed against, and the two are not
/// connected. Pass a smaller number and the thumb stops short of the bottom at the last line;
/// pass a larger one and it runs off the end. A caller in a `Dock` therefore has to subtract
/// whatever the other edges took, which is what [`reference::view`](crate::reference::view)
/// does. Making the widget measure itself would need a second layout pass, which the toolkit
/// does not have; until it does, this is an obligation on the caller rather than a guarantee
/// (PR #185 review, finding 7).
pub fn scrollbar<Msg>(state: ScrollState, width: u32, height: u32, theme: &Theme) -> Element<Msg> {
    let (pos, len) = state.thumb(height);
    sized(
        Size::new(width, 0),
        stack(alloc::vec![
            fill(theme.track),
            column(alloc::vec![
                sized(Size::new(0, pos), fill(theme.track)),
                sized(Size::new(0, len), bevel(theme.thumb)),
                // The remainder, so the thumb does not stretch to the bottom.
                fill(theme.track).flex(1),
            ]),
        ]),
    )
}

/// A horizontal bar of buttons — a menu bar, or a toolbar.
///
/// The popup half of a menu is not here, and is not a widget at all: an open menu is a `popup`
/// **window**, parented to the application's window and clipped by the screen. It was a `stack`
/// layer over the application's content until M6 C3, which works only for a menu that fits
/// inside the window it drops from. This is the part the terminal's chrome needs; the anchor it
/// is dropped from comes from [`layout::locate`](crate::layout::locate).
pub fn menu_bar<Msg>(items: alloc::vec::Vec<Element<Msg>>, height: u32, theme: &Theme) -> Element<Msg> {
    sized(
        Size::new(0, height),
        stack(alloc::vec![fill(theme.face), row(items)]),
    )
}


/// How tall a title bar is, in pixels.
///
/// One number rather than a measurement of the font, because a client sizes its window around
/// it: a bar that grew with the theme would change every window's content area when the theme
/// changed, and M11 is where a theme becomes changeable.
pub const TITLE_BAR_H: u32 = 26;

/// How wide each title-bar button is.
pub const TITLE_BUTTON_W: u32 = 26;

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
    focused: bool,
    drag: Msg,
    buttons: TitleButtons<Msg>,
    theme: &Theme,
) -> Element<Msg> {
    let face = if focused { theme.title_active } else { theme.title_inactive };
    // **A glyph, not a letter** (M11 Part E, batch 2). These were `_`, `[]` and `X` — three
    // characters standing in for three controls, which read as text on a bar full of text. They
    // are drawn now, and the button keeps its size, so nothing a gate clicks has moved.
    let btn = |glyph: IconKind, msg: Msg| {
        sized(
            Size::new(TITLE_BUTTON_W, TITLE_BAR_H),
            stack(alloc::vec![icon(glyph)]).on_press(msg),
        )
    };
    // **A button a caller has no message for is not drawn.** The alternative is a button that
    // does nothing, and a control that looks live and is not is the defect this milestone's
    // predecessor shipped three of (M8's overview). The buttons arrive with the parts that give
    // them somewhere to go: minimise and maximise in Part B, close in Part C.
    let mut controls = alloc::vec::Vec::with_capacity(4);
    controls.push(padding(TITLE_PAD, text(title)).flex(1));
    if let Some(m) = buttons.minimise {
        controls.push(btn(IconKind::Minimise, m));
    }
    if let Some(m) = buttons.maximise {
        controls.push(btn(IconKind::Maximise, m));
    }
    if let Some(m) = buttons.close {
        controls.push(btn(IconKind::Close, m));
    }
    // **The drag is on the bar itself, not on the face underneath the label.** Dispatch walks
    // *up* from whatever was hit to the nearest handler, and the label spans the bar — so a
    // handler on the face below it is never reached, and the first version of this widget
    // produced nothing at all for a press in the middle of its own title. On the bar, a press
    // that lands on the label or on empty space walks up to here, and one that lands on a button
    // stops at the button, because that is where the walk finds a handler first.
    sized(
        Size::new(0, TITLE_BAR_H),
        stack(alloc::vec![fill(face), row(controls)]).on_press_down(drag),
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
        layers.push(padding(
            Insets { top: inset + 2, right: 2, bottom: 2, left: inset + 2 },
            fill(theme.track),
        ));
        layers.push(padding(
            Insets { top: inset + 4, right: 4, bottom: 4, left: inset + 4 },
            fill(theme.face),
        ));
    }
    sized(Size::new(GRIP_W, GRIP_W), stack(layers).on_press_down(msg))
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

/// Space between a title bar's text and its edge.
const TITLE_PAD: Insets = Insets { top: 5, right: 6, bottom: 5, left: 8 };

/// Space between a text field's content and its edge.
const FIELD_PAD: Insets = Insets { top: 4, right: 6, bottom: 4, left: 6 };

/// How wide the caret is, in pixels.
const CARET: u32 = 2;

/// What a masked field shows instead of each character.
///
/// `*` rather than a bullet, deliberately: the vendored DejaVu almost certainly has U+2022,
/// but "almost certainly" is not a property a login screen should depend on, and a missing
/// glyph in a password field is invisible to the person typing — they cannot read what it
/// should have said. ASCII cannot go wrong here.
const MASK_CHAR: char = '*';

/// The editable content of a single-line text field.
///
/// **The state is the application's and the widget is a pure function of it**, which is not a
/// style choice: [`Element::on_key`](crate::element::Element::on_key) is a *function pointer*,
/// so a widget cannot close over anything to mutate. The application owns one of these, hands
/// keys to [`apply`](Self::apply), and passes the result to [`text_field`] — the same shape as
/// every other widget here, where "hovered" is state the caller passes in.
///
/// **A single line, not an editor.** `widget-toolkit.md` §8 keeps the *text area* out of the
/// set until something needs one, and says it "returns when something needs it". A greeter's
/// password box and a launcher's search box are that trigger, and they are narrower than the
/// thing §8 is reserving: no wrapping, no selection, no undo, no multi-line cursor. Building
/// the editor's widget now would be the guess §8 refuses to make.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TextFieldState {
    /// What has been typed.
    text: String,
    /// Where the caret is, as a **byte** offset into `text`.
    ///
    /// Bytes rather than characters because that is what slicing needs, and every mutation
    /// below keeps it on a character boundary — the invariant the whole type rests on.
    cursor: usize,
}

impl TextFieldState {
    /// An empty field.
    pub fn new() -> Self {
        Self::default()
    }

    /// A field holding `text`, caret at the end — where a caller pre-filling a username wants it.
    pub fn with_text(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.len();
        Self { text, cursor }
    }

    /// What has been typed.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The caret's byte offset. Always on a character boundary.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether anything has been typed.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Empty it and put the caret back — what a supervisor does to a password field the
    /// moment it has been read, so a rejected login does not leave it on screen.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// Insert `c` at the caret and step over it.
    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Delete the character before the caret. No-op at the start.
    pub fn backspace(&mut self) -> bool {
        let Some(prev) = self.prev_boundary() else {
            return false;
        };
        self.text.remove(prev);
        self.cursor = prev;
        true
    }

    /// Delete the character after the caret. No-op at the end.
    pub fn delete(&mut self) -> bool {
        if self.cursor >= self.text.len() {
            return false;
        }
        self.text.remove(self.cursor);
        true
    }

    /// Move the caret one character left.
    pub fn left(&mut self) -> bool {
        match self.prev_boundary() {
            Some(prev) => {
                self.cursor = prev;
                true
            }
            None => false,
        }
    }

    /// Move the caret one character right.
    pub fn right(&mut self) -> bool {
        let Some(c) = self.text[self.cursor..].chars().next() else {
            return false;
        };
        self.cursor += c.len_utf8();
        true
    }

    /// Move the caret to the start.
    pub fn home(&mut self) -> bool {
        let moved = self.cursor != 0;
        self.cursor = 0;
        moved
    }

    /// Move the caret to the end.
    pub fn end(&mut self) -> bool {
        let moved = self.cursor != self.text.len();
        self.cursor = self.text.len();
        moved
    }

    /// The byte offset of the character before the caret, if there is one.
    fn prev_boundary(&self) -> Option<usize> {
        self.text[..self.cursor].chars().next_back().map(|c| self.cursor - c.len_utf8())
    }

    /// Apply a key, answering **whether the field changed** so a caller knows to repaint.
    ///
    /// One implementation of "what does this keycode do to a field", rather than one per
    /// caller. The greeter and the applications modal are the two consumers Part A is designed
    /// against, and key dispatch is the part they would otherwise each get subtly wrong —
    /// Home and End are easy to omit, and a field that ignores them is noticeably broken.
    ///
    /// **Keys it does not claim are left alone**, which is the contract
    /// [`Element::on_key`](crate::element::Element::on_key) is built around: Tab, Enter and
    /// Escape belong to whatever is above the field — traversal, submission and dismissal are
    /// not a text field's business — so this answers `false` and lets them bubble.
    ///
    /// ASCII only, because [`libinput::keymap::to_char`] is: the US layout is what the input
    /// stack maps today, and a field that invented its own mapping would disagree with the
    /// terminal about what a key means.
    pub fn apply(&mut self, keycode: u16, modifiers: u16) -> bool {
        match keycode {
            KEY_BACKSPACE => self.backspace(),
            KEY_DELETE => self.delete(),
            KEY_LEFT => self.left(),
            KEY_RIGHT => self.right(),
            KEY_HOME => self.home(),
            KEY_END => self.end(),
            _ => match libinput::keymap::to_char(keycode, modifiers) {
                // Control characters are not text. Ctrl-C folds to 0x03 in the keymap because
                // a terminal needs it to; a field that inserted it would put an unprintable
                // byte in a password.
                Some(b) if b >= 0x20 && b < 0x7F => {
                    self.insert(b as char);
                    true
                }
                _ => false,
            },
        }
    }
}


/// A single-line text field, optionally masked.
///
/// **The caret is a `Row` split at the cursor**, not a measured x-offset: the text before it,
/// a two-pixel fill, then the text after. A `Row` already lays children out left to right by
/// their measured widths, so the caret lands exactly where the glyphs end without this widget
/// measuring anything — and it stays correct for any font, because the same measurement that
/// draws the text places the caret.
///
/// **Masked with [`MASK_CHAR`] per character, not per byte.** A mask built by repeating a byte
/// would leak the encoded length of a multi-byte character and, worse, split one — so the
/// number of stars would not be the number of keys pressed.
///
/// The caret is drawn from `state.active`, like the button's focus ring: a field in an
/// unfocused window must not blink a caret for a keyboard it does not have.
pub fn text_field<Msg>(
    field: &TextFieldState,
    masked: bool,
    state: WidgetState,
    theme: &Theme,
) -> Element<Msg> {
    let render = |s: &str| -> String {
        if masked { core::iter::repeat_n(MASK_CHAR, s.chars().count()).collect() } else { s.into() }
    };
    let (before, after) = field.text.split_at(field.cursor);

    let mut content = alloc::vec::Vec::with_capacity(3);
    content.push(text(render(before)));
    if state.active {
        content.push(sized(Size::new(CARET, 0), fill(theme.focus_ring)));
    }
    content.push(text(render(after)));

    // `track` is the recessed-channel colour the scrollbar uses, and a text field is the same
    // idea: a well the content sits in, rather than a face that stands out of the surface.
    let mut layers = alloc::vec::Vec::with_capacity(3);
    if state.active {
        layers.push(fill(theme.focus_ring));
        layers.push(padding(Insets::all(RING), fill(theme.track)));
    } else {
        layers.push(fill(theme.track));
    }
    layers.push(padding(FIELD_PAD, row(content)));
    stack(layers).focusable()
}


/// A multi-line text buffer with a cursor, a selection and a scroll position.
///
/// **The widget `libui` deliberately did not build until an editor asked for it.** §8 has said
/// since M4 that "building an editor's widget remains a guess at requirements no editor has yet
/// posed"; M10's editor poses them, which is the trigger firing rather than being ignored.
///
/// **Lines are logical and of unbounded length, and nothing here wraps.** That is the whole of
/// what separates this from [`libterm`]'s grid, which is a fixed rectangle of cells that
/// *rewraps* on resize (M9 Part D). The two look similar and are different problems: a grid's
/// line is as wide as the screen by construction, and a text area's line is as long as somebody
/// typed. Sharing code between them is a **non-goal** stated in the plan, so that a later
/// "these could be merged" is argued against something rather than into a vacuum.
///
/// **Byte offsets, always on character boundaries** — the invariant [`TextFieldState`] rests on,
/// for the same reason: slicing needs bytes, and every mutation here keeps them valid.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TextAreaState {
    /// The lines. **Never empty**: an empty buffer is one empty line, so a cursor always has a
    /// line to be on and every method below can index without a guard.
    lines: Vec<String>,
    /// The cursor's line.
    line: usize,
    /// The cursor's byte offset within its line.
    col: usize,
    /// Where a selection started, or `None` when there is no selection.
    ///
    /// **An anchor rather than a range**, because a selection is *directional* while it is being
    /// made: dragging back past the anchor selects the other way, and a stored range would have
    /// to be re-derived every step to know which end is moving.
    anchor: Option<(usize, usize)>,
    /// The column vertical movement is aiming for, in **characters**.
    ///
    /// Set by the first Up or Down and cleared by anything horizontal. Without it, moving down
    /// through a short line and back up leaves the cursor at the short line's end — the column
    /// is lost, and a person who did not touch a horizontal key has had one moved for them.
    goal: Option<usize>,
    /// The first visible line.
    offset: usize,
    /// How many times the *text* has changed.
    ///
    /// **Because "is this buffer modified?" is a question only the state can answer.** An editor
    /// asking it from outside has two bad options: compare the whole text against a copy on
    /// every keystroke, or re-derive which keycodes edit — a second copy of [`apply`]'s dispatch
    /// that goes stale the first time this type learns a key. Byte length cannot stand in for
    /// it either: replacing a one-character selection with one character leaves the length
    /// unchanged.
    ///
    /// Movement never bumps it, and neither does an edit that did nothing — `Backspace` at the
    /// start of the buffer, `Delete` at its end.
    revision: u64,
}

impl Default for TextAreaState {
    fn default() -> Self {
        Self {
            lines: alloc::vec![String::new()],
            line: 0,
            col: 0,
            anchor: None,
            goal: None,
            offset: 0,
            revision: 0,
        }
    }
}

impl TextAreaState {
    /// An empty buffer: one empty line, cursor at its start.
    pub fn new() -> Self {
        Self::default()
    }

    /// A buffer holding `text`, cursor at the start — where an editor opening a file wants it.
    ///
    /// **`\r\n` and `\n` both end a line, and the `\r` is dropped.** A file written elsewhere is
    /// a file this editor should be able to open, and a carriage return kept in the buffer would
    /// be an invisible character at the end of every line that the cursor has to step over.
    pub fn with_text(text: &str) -> Self {
        // **One line minimum, and `split` is what guarantees it** rather than a check here:
        // `str::split('\n')` yields at least one piece for every input, `""` included, so an
        // empty buffer is one empty line. The rest of this type indexes `lines[self.line]`
        // without checking, so the invariant matters — it is just not this function's to
        // enforce. An `is_empty` guard stood here until PR #258's review pointed out that it
        // cannot fire, and a guard that cannot fire reads as protecting an invariant it does
        // not (optional 2).
        let lines: Vec<String> =
            text.split('\n').map(|l| String::from(l.strip_suffix('\r').unwrap_or(l))).collect();
        Self { lines, ..Self::default() }
    }

    /// The buffer as one string, lines joined with `\n`.
    ///
    /// **No trailing newline is added.** What was opened is what is saved: a file that did not
    /// end with one does not gain one, and one that did keeps its final empty line — which
    /// `with_text` produced and this rejoins.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// The lines, for a caller drawing them.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// The cursor, as `(line, byte offset)`.
    pub fn cursor(&self) -> (usize, usize) {
        (self.line, self.col)
    }

    /// The first visible line.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// How many times the text has changed — the buffer's identity for a "modified" marker.
    ///
    /// Compare it against the value taken when the file was last read or written; equal means
    /// what is on screen is what is on disk. Never reset, so a buffer edited, saved and edited
    /// again reads as modified, and one edited back to a saved state still does — an editor
    /// claiming a file is unmodified because the text happens to match again would be claiming
    /// to have diffed it.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The selection as `(start, end)` in document order, or `None`.
    ///
    /// Normalised here rather than at each call site: the anchor may be before or after the
    /// cursor, and every consumer wants them the other way round.
    pub fn selection(&self) -> Option<((usize, usize), (usize, usize))> {
        let a = self.anchor?;
        let c = (self.line, self.col);
        if a == c {
            return None;
        }
        Some(if a <= c { (a, c) } else { (c, a) })
    }

    /// The selected text, or `None` when nothing is selected.
    pub fn selected_text(&self) -> Option<String> {
        let ((sl, sc), (el, ec)) = self.selection()?;
        if sl == el {
            return Some(String::from(&self.lines[sl][sc..ec]));
        }
        let mut out = String::from(&self.lines[sl][sc..]);
        for l in &self.lines[sl + 1..el] {
            out.push('\n');
            out.push_str(l);
        }
        out.push('\n');
        out.push_str(&self.lines[el][..ec]);
        Some(out)
    }

    /// Put the cursor at `(line, col)`, clamped into the buffer, and drop any selection.
    ///
    /// What a press does. `col` is a byte offset and is moved back to a character boundary
    /// rather than refused: it comes from arithmetic on a pixel position, which knows nothing
    /// about encoding.
    pub fn place(&mut self, line: usize, col: usize) {
        self.anchor = None;
        self.goal = None;
        self.line = line.min(self.lines.len() - 1);
        let l = &self.lines[self.line];
        let mut c = col.min(l.len());
        while c > 0 && !l.is_char_boundary(c) {
            c -= 1;
        }
        self.col = c;
    }

    /// Extend the selection to `(line, col)` — what a drag does.
    ///
    /// The anchor is taken from where the cursor is *now* if there is no selection yet, which is
    /// what makes press-then-drag select from the press.
    pub fn extend_to(&mut self, line: usize, col: usize) {
        let from = self.anchor.unwrap_or((self.line, self.col));
        self.place(line, col);
        self.anchor = Some(from);
    }

    /// Delete the selection, leaving the cursor at where it started. `false` if there was none.
    ///
    /// **The anchor is dropped either way, and that is the whole of it.** A cursor that has
    /// walked back onto its own anchor has no *selection* — [`selection`](Self::selection)
    /// returns `None` for it — but it still has an anchor, and an anchor is a pair of indices
    /// into text this call is about to shorten. Every edit funnels through here (`insert`,
    /// `newline`, `backspace`, `delete` all call it first), so this is the one place that has to
    /// know: whatever it pointed at is not there any more.
    ///
    /// It survived one review because both of its symptoms need two keystrokes to reach.
    /// `Shift+Left` then `Shift+Right` collapses a selection onto its anchor; a `Backspace` after
    /// that used to leave an anchor naming a byte past the end of the line, and the *next frame*
    /// panicked inside `text_area` slicing it. Typing instead of deleting gave the quieter half:
    /// a selection nobody made, over the character just typed, which the keystroke after that
    /// would replace (PR #258 review, blocking 1).
    pub fn delete_selection(&mut self) -> bool {
        let Some(((sl, sc), (el, ec))) = self.selection() else {
            self.anchor = None;
            return false;
        };
        let tail = String::from(&self.lines[el][ec..]);
        self.lines[sl].truncate(sc);
        self.lines[sl].push_str(&tail);
        self.lines.drain(sl + 1..=el);
        self.line = sl;
        self.col = sc;
        self.anchor = None;
        self.goal = None;
        self.revision += 1;
        true
    }

    /// Insert `c` at the cursor, replacing any selection.
    pub fn insert(&mut self, c: char) {
        self.delete_selection();
        self.lines[self.line].insert(self.col, c);
        self.col += c.len_utf8();
        self.goal = None;
        self.revision += 1;
    }

    /// Split the line at the cursor, replacing any selection.
    pub fn newline(&mut self) {
        self.delete_selection();
        let tail = self.lines[self.line].split_off(self.col);
        self.lines.insert(self.line + 1, tail);
        self.line += 1;
        self.col = 0;
        self.goal = None;
        self.revision += 1;
    }

    /// Delete backwards: the selection if there is one, else the character before the cursor,
    /// else join with the previous line.
    pub fn backspace(&mut self) -> bool {
        if self.delete_selection() {
            return true;
        }
        self.goal = None;
        if self.col > 0 {
            let prev = self.prev_boundary();
            self.lines[self.line].remove(prev);
            self.col = prev;
            self.revision += 1;
            return true;
        }
        if self.line == 0 {
            return false;
        }
        // **Joining is the case a single-line field never has**, and the cursor lands where the
        // join happened rather than at the start of the merged line — which is where the text
        // the person was deleting towards now is.
        let cur = self.lines.remove(self.line);
        self.line -= 1;
        self.col = self.lines[self.line].len();
        self.lines[self.line].push_str(&cur);
        self.revision += 1;
        true
    }

    /// Delete forwards: the selection, else the character after the cursor, else join with the
    /// next line.
    pub fn delete(&mut self) -> bool {
        if self.delete_selection() {
            return true;
        }
        self.goal = None;
        if self.col < self.lines[self.line].len() {
            self.lines[self.line].remove(self.col);
            self.revision += 1;
            return true;
        }
        if self.line + 1 >= self.lines.len() {
            return false;
        }
        let next = self.lines.remove(self.line + 1);
        self.lines[self.line].push_str(&next);
        self.revision += 1;
        true
    }

    /// Move left one character, or to the end of the previous line.
    pub fn left(&mut self, extend: bool) -> bool {
        self.before_move(extend);
        self.goal = None;
        if self.col > 0 {
            self.col = self.prev_boundary();
            return true;
        }
        if self.line == 0 {
            return false;
        }
        self.line -= 1;
        self.col = self.lines[self.line].len();
        true
    }

    /// Move right one character, or to the start of the next line.
    pub fn right(&mut self, extend: bool) -> bool {
        self.before_move(extend);
        self.goal = None;
        if let Some(c) = self.lines[self.line][self.col..].chars().next() {
            self.col += c.len_utf8();
            return true;
        }
        if self.line + 1 >= self.lines.len() {
            return false;
        }
        self.line += 1;
        self.col = 0;
        true
    }

    /// Move up one line, keeping the goal column.
    pub fn up(&mut self, extend: bool) -> bool {
        self.before_move(extend);
        if self.line == 0 {
            return false;
        }
        let goal = self.goal_chars();
        self.line -= 1;
        self.col = self.col_for(self.line, goal);
        true
    }

    /// Move down one line, keeping the goal column.
    pub fn down(&mut self, extend: bool) -> bool {
        self.before_move(extend);
        if self.line + 1 >= self.lines.len() {
            return false;
        }
        let goal = self.goal_chars();
        self.line += 1;
        self.col = self.col_for(self.line, goal);
        true
    }

    /// To the start of the line.
    pub fn home(&mut self, extend: bool) -> bool {
        self.before_move(extend);
        self.goal = None;
        let moved = self.col != 0;
        self.col = 0;
        moved
    }

    /// To the end of the line.
    pub fn end(&mut self, extend: bool) -> bool {
        self.before_move(extend);
        self.goal = None;
        let moved = self.col != self.lines[self.line].len();
        self.col = self.lines[self.line].len();
        moved
    }

    /// Set or clear the anchor before a movement.
    ///
    /// **Shift starts a selection from where the cursor is**, and an unshifted movement drops
    /// one. That is the whole of the selection model: there is no separate "selecting" mode to
    /// get out of sync with what is on screen.
    fn before_move(&mut self, extend: bool) {
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some((self.line, self.col));
            }
        } else {
            self.anchor = None;
        }
    }

    /// The goal column in characters, set from the current column the first time.
    fn goal_chars(&mut self) -> usize {
        match self.goal {
            Some(g) => g,
            None => {
                let g = self.lines[self.line][..self.col].chars().count();
                self.goal = Some(g);
                g
            }
        }
    }

    /// The byte offset `chars` characters into `line`, clamped to its end.
    fn col_for(&self, line: usize, chars: usize) -> usize {
        let l = &self.lines[line];
        l.char_indices().nth(chars).map(|(i, _)| i).unwrap_or(l.len())
    }

    /// The byte offset of the character before the cursor, within its line.
    fn prev_boundary(&self) -> usize {
        let l = &self.lines[self.line];
        l[..self.col].chars().next_back().map(|c| self.col - c.len_utf8()).unwrap_or(0)
    }

    /// Scroll so the cursor's line is among the `visible` shown.
    ///
    /// The same shape [`ListState::ensure_visible`] has, and called by [`text_area`] rather than
    /// by the application — a caller that had to remember it would have a cursor that walks off
    /// the bottom of its own window.
    pub fn ensure_visible(&mut self, visible: usize) {
        if visible == 0 {
            return;
        }
        if self.line < self.offset {
            self.offset = self.line;
        } else if self.line >= self.offset + visible {
            self.offset = self.line + 1 - visible;
        }
    }

    /// Apply a key, answering **whether the buffer or the cursor changed**.
    ///
    /// One implementation of "what does this keycode do to a text area", for the reason
    /// [`TextFieldState::apply`] gives. **Shift extends**, which is why this takes the modifiers
    /// rather than only the code.
    ///
    /// **Enter is claimed here and Tab is not.** A text area is the one widget for which Enter
    /// is text rather than submission — that is what multi-line means — while Tab remains
    /// traversal's, because a buffer that swallowed it would trap the keyboard in itself.
    pub fn apply(&mut self, keycode: u16, modifiers: u16) -> bool {
        let extend = modifiers & MOD_SHIFT != 0;
        match keycode {
            KEY_BACKSPACE => self.backspace(),
            KEY_DELETE => self.delete(),
            KEY_LEFT => self.left(extend),
            KEY_RIGHT => self.right(extend),
            KEY_UP => self.up(extend),
            KEY_DOWN => self.down(extend),
            KEY_HOME => self.home(extend),
            KEY_END => self.end(extend),
            KEY_ENTER => {
                self.newline();
                true
            }
            _ => match libinput::keymap::to_char(keycode, modifiers) {
                // Printable ASCII only, the same range a text field takes and for the same
                // reason: `to_char` folds Ctrl-C to 0x03 because a terminal needs it to, and an
                // editor that inserted that would put an unprintable byte in somebody's file.
                Some(b) if (0x20..0x7F).contains(&b) => {
                    self.insert(b as char);
                    true
                }
                _ => false,
            },
        }
    }
}

/// A multi-line editable text view over a [`TextAreaState`].
///
/// **Takes the state by `&mut`, and scrolls it.** `list_view` takes its state by value and
/// returns it scrolled, which put the obligation on the caller — and the caller that dropped it
/// shipped a browser whose selection never left the last visible row (PR #257 review). A widget
/// whose correctness depends on somebody remembering to write something back has the wrong
/// signature, so this one does it itself.
///
/// `height` is what the caller will lay it out at; wrap the result in `sized` to keep the two in
/// step, for the reason [`list_view`] gives.
///
/// **What it draws:** the visible lines, the selection behind the text on each, and the caret
/// when `active`. What it does *not* draw is a scrollbar — that is `scrollbar`'s, composed
/// beside it by an application that wants one, the way the terminal composes its own.
pub fn text_area<Msg>(
    state: &mut TextAreaState,
    height: u32,
    row_height: u32,
    active: bool,
    theme: &Theme,
) -> Element<Msg> {
    let visible = if row_height == 0 { 0 } else { (height / row_height) as usize };
    state.ensure_visible(visible);
    let sel = state.selection();
    let (cur_line, cur_col) = state.cursor();

    let last = (state.offset + visible).min(state.lines.len());
    let mut rows: Vec<Element<Msg>> = Vec::with_capacity(last.saturating_sub(state.offset));
    for i in state.offset..last {
        let l = &state.lines[i];
        // Where this line's selection starts and ends, in bytes. A line wholly inside a
        // multi-line selection is `(0, len)`; one outside it is `None`.
        let span = sel.and_then(|((sl, sc), (el, ec))| {
            if i < sl || i > el {
                return None;
            }
            let from = if i == sl { sc } else { 0 };
            let to = if i == el { ec } else { l.len() };
            // **An empty line inside a multi-line selection draws no highlight**, because
            // `from == to` and there is nothing to put a colour behind. A selected blank line
            // therefore looks unselected. Recorded rather than fixed: showing it means drawing
            // a sliver a space wide, and the widget cannot measure a space — text is measured by
            // the caller's `Metrics` at layout time, not here (PR #258 review, optional 4).
            // **Trigger: the first time a widget can ask for a glyph's advance.**
            (from < to).then_some((from, to))
        });

        let mut pieces: Vec<Element<Msg>> = Vec::with_capacity(5);
        let mut at = 0usize;
        let push_text = |pieces: &mut Vec<Element<Msg>>, from: usize, to: usize| {
            if from < to {
                pieces.push(text(String::from(&l[from..to])));
            }
        };
        // Where the caret goes on this line, if it is on this line at all. The pieces are built
        // left to right, so *when* it is emitted decides where it appears — and a selection's
        // cursor is at its **start** as often as at its end: every `Shift+Left`, `Shift+Up` and
        // backwards drag makes one. The first version emitted it only after the highlight, under
        // a `cur_col >= at` guard that a backwards selection cannot satisfy, so the cursor
        // vanished from the screen for exactly those (PR #258 review, blocking 2).
        let caret = (active && i == cur_line).then_some(cur_col);
        let mut caret_drawn = false;
        let push_caret = |pieces: &mut Vec<Element<Msg>>, at: &mut usize, to: usize| {
            push_text(pieces, *at, to);
            pieces.push(sized(Size::new(CARET, 0), fill(theme.focus_ring)));
            *at = to;
        };
        if let Some(cc) = caret {
            if span.map(|(from, _)| cc <= from).unwrap_or(true) {
                push_caret(&mut pieces, &mut at, cc);
                caret_drawn = true;
            }
        }
        if let Some((from, to)) = span {
            push_text(&mut pieces, at, from);
            // The highlight is a `fill` *under* the run: `fill` measures as zero, so the stack
            // takes the text's size and the colour covers exactly the glyphs' box.
            pieces.push(stack(alloc::vec![
                fill(theme.selection),
                text(String::from(&l[from..to])),
            ]));
            at = to;
        }
        if let Some(cc) = caret {
            if !caret_drawn {
                // **`max`, not a guard, and that is the fix's whole shape.** The cursor is one
                // end of the selection, so on its own line it sits at the highlight's start —
                // drawn above — or at its end, and `cc` is then already past `at`. Should some
                // future arrangement put it inside the run, this draws a caret in the wrong
                // column rather than none at all: a caret nobody can find is the failure this
                // replaces, and it is worse than one a pixel out of place.
                let to = cc.max(at);
                push_caret(&mut pieces, &mut at, to);
            }
        }
        push_text(&mut pieces, at, l.len());
        if pieces.is_empty() {
            // An empty line still needs a row, or the lines below it move up by one.
            pieces.push(text(""));
        }
        rows.push(sized(Size::new(0, row_height), row(pieces)));
    }

    let mut layers = alloc::vec::Vec::with_capacity(2);
    layers.push(fill(theme.track));
    layers.push(padding(FIELD_PAD, column(rows)));
    stack(layers).focusable()
}

/// One row of a [`list_view`].
///
/// **Borrowed, and built fresh each frame from whatever the application already has.** That
/// is the "model" in model-backed: a window list derives these from its window records and a
/// launcher derives them from its filtered program list, neither keeping a parallel array of
/// row widgets to reconcile by hand — which is the hand-rolled diffing `desktop-shell.md` §5
/// says a list widget exists to avoid.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ListRow<'a> {
    /// Identity across frames.
    ///
    /// **Not the index.** [`Element::key`](crate::element::Element::key) exists because the
    /// diff otherwise pairs by position, and its own doc names this failure: insert a row at
    /// the top and "the diff pairs row 2's widget with row 3's element". A window list
    /// reorders on every raise, so position is never stable here. Use the window id, or the
    /// index into the *unfiltered* list — anything that survives the list changing shape.
    ///
    /// **Must be unique among the rows in one list.** A repeat is
    /// [`DiffError::DuplicateKey`](crate::diff::DiffError::DuplicateKey) — a hard error from
    /// the diff, not a degraded pairing — which is worth knowing before reaching for a hash of
    /// a name that two rows could share.
    pub key: u64,
    /// What the row says.
    pub label: &'a str,
}

/// Which row is selected, and how far the list is scrolled.
///
/// Scroll is a row index rather than a pixel offset: a list scrolls by whole rows, and the
/// arithmetic that keeps a selection visible is unreadable in pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ListState {
    /// The selected row, as an index into the rows passed to [`list_view`].
    pub selected: Option<usize>,
    /// The first visible row.
    pub offset: usize,
}

impl ListState {
    /// Move the selection down one row, answering whether anything changed.
    ///
    /// From nothing selected this selects the first row, which is what a launcher wants when
    /// the user presses Down having only typed a query.
    pub fn down(&mut self, len: usize) -> bool {
        if len == 0 {
            return false;
        }
        let next = match self.selected {
            None => 0,
            Some(i) if i + 1 < len => i + 1,
            Some(i) => i,
        };
        let moved = self.selected != Some(next);
        self.selected = Some(next);
        moved
    }

    /// Move the selection up one row, answering whether anything changed.
    pub fn up(&mut self) -> bool {
        let Some(i) = self.selected else {
            return false;
        };
        if i == 0 {
            return false;
        }
        self.selected = Some(i - 1);
        true
    }

    /// Scroll so the selected row is on screen, given how many rows fit.
    ///
    /// **The thing both callers would get wrong.** A selection moved with the keyboard walks
    /// off the visible window and the list appears to stop responding — the state changed and
    /// nothing on screen did. Called by [`list_view`] itself rather than left to the caller,
    /// because a widget that can compute this and makes its callers do it is a widget that
    /// will have two implementations of it.
    pub fn ensure_visible(&mut self, visible: usize) {
        let Some(i) = self.selected else {
            return;
        };
        if visible == 0 {
            return;
        }
        if i < self.offset {
            self.offset = i;
        } else if i >= self.offset + visible {
            self.offset = i + 1 - visible;
        }
    }

    /// Apply a key, answering whether the list changed.
    ///
    /// Declines everything it does not claim, for the reason
    /// [`TextFieldState::apply`](TextFieldState::apply) does: Enter activates the selection
    /// and Escape dismisses the list, and neither belongs to the list itself. A launcher's
    /// query field and its results list are both focusable, and arrow keys have to reach the
    /// list while the field holds focus — so a caller routes these itself rather than relying
    /// on widget focus, which is why this takes a keycode instead of being wired to `on_key`.
    pub fn apply(&mut self, keycode: u16, len: usize) -> bool {
        match keycode {
            KEY_DOWN => self.down(len),
            KEY_UP => self.up(),
            _ => false,
        }
    }
}

/// How much space a row's label gets around it.
const ROW_PAD: Insets = Insets { top: 2, right: 6, bottom: 2, left: 6 };

impl ListState {
    /// Move the offset to where a drag on the scrollbar's track points.
    ///
    /// **The conversion belongs here, not in each caller**, because the widget already knows the
    /// arithmetic and the caller only knows the numbers it passed in. `nxterm` does the same
    /// conversion with the same [`ScrollState::offset_at`], from a grid's coordinates — this is
    /// that for a list, so the two cannot drift apart on rounding.
    ///
    /// Takes the same `height`, `row_height` and row count [`list_view`] was given: a drag
    /// converted against a different geometry from the one drawn puts the thumb where the pointer
    /// is not (M11 Part E batch 6).
    pub fn drag_to(&mut self, height: u32, row_height: u32, total: usize, y: i32) {
        let visible = if row_height == 0 { 0 } else { height / row_height };
        let bar = ScrollState { offset: self.offset as u32, visible, total: total as u32 };
        self.offset = bar.offset_at(height, y) as usize;
    }
}

/// A scrolling list of rows, with one selected.
///
/// **The one model-backed widget** `desktop-shell.md` §5 settles on — "an explicit toolkit
/// *plus one model-backed list widget*" covering the window list, the desktop previews and the
/// launcher results, "which is essentially all of the churn, for a fraction of a diffing
/// engine's machinery". Designed against two of those callers rather than one, because a model
/// API drawn for a single consumer is the failure mode §5 was avoiding: a window list is
/// **reordered and mutated in place** while a launcher's results are **replaced wholesale on
/// every keystroke**, and those stress different halves — the first the keying, the second the
/// selection and scroll surviving a list that changed length.
///
/// **Only the visible rows become elements.** That is the point of the widget rather than an
/// optimisation: a list of a hundred windows costs as many elements as fit on screen, and the
/// diff walks that many.
///
/// **`height` must be the height the parent will actually give it** — the same obligation
/// [`scrollbar`] states, and this widget does not size itself to it, so a caller that lets the
/// list flex will show a different number of rows than it built. Wrapping the result in
/// `sized` is what [`crate::reference`] does and is the reliable way to keep the two in step.
/// (An earlier version of this sentence claimed a caller "cannot get it out of step", which is
/// exactly backwards; PR #233 review.)
///
/// **`state` is taken by `&mut` and scrolled in place** to follow the selection — see
/// [`ensure_visible`](ListState::ensure_visible).
///
/// It returned the scrolled state instead until M10 Part C, and the difference is not
/// stylistic: **nothing in the type system could make a caller keep it.** `#[must_use]` fires on
/// an *unused* return, and `let (e, _) = list_view(…)` uses the tuple; putting it on `ListState`
/// does not help either, since binding to `_` is the documented way to silence exactly that
/// lint. `ListState` is `Copy`, which removed the last chance — a caller passing `self.list`
/// by value and dropping the result kept a perfectly valid stale copy, where a non-`Copy` state
/// would have been a move-out error. `nxfiles` shipped precisely that: an offset re-derived
/// from zero every frame, so the selection never left the last visible row (PR #257 review).
///
/// The obligation also *propagated*: `desktop-shell` grew `(T, ListState)` returns three
/// functions deep to carry state none of them used. In-place update is what Rust uses for this
/// — `Vec::sort`, `Vec::retain`, `read_line(&mut String)` — and a returned value is for when a
/// caller may genuinely decline it. There is no correct program that ignores a scroll offset.
pub fn list_view<Msg>(
    rows: &[ListRow<'_>],
    state: &mut ListState,
    height: u32,
    row_height: u32,
    activate: fn(u64) -> Msg,
    grab: Option<fn(u64) -> Msg>,
    scroll: Option<fn(PointerEvent) -> Msg>,
    hovered: Option<u64>,
    theme: &Theme,
) -> Element<Msg> {
    let visible = if row_height == 0 { 0 } else { (height / row_height) as usize };
    // **The selection is clamped first, because it is an index into a list that may have just
    // been replaced.** A launcher rebuilds its results on every keystroke, so a selection made
    // against twenty of them is not an index into the three that remain — and a stale one is
    // worse than useless: nothing paints as selected, `down` sees `i + 1 < len` fail and
    // returns the same index so the key is *dead*, and a caller reading `selected` to decide
    // what Enter activates gets an out-of-range index. Clamped to the last surviving row
    // rather than cleared, so something is highlighted and the arrows work on the next press
    // (PR #233 review, finding 2).
    if let Some(i) = state.selected {
        if i >= rows.len() {
            state.selected = rows.len().checked_sub(1);
        }
    }
    state.ensure_visible(visible);
    // Never scrolled past the end: a list that shrinks under a stale offset would otherwise
    // render blank while holding rows.
    let max_offset = rows.len().saturating_sub(visible);
    state.offset = state.offset.min(max_offset);

    let last = (state.offset + visible).min(rows.len());
    let mut items = alloc::vec::Vec::with_capacity(last.saturating_sub(state.offset));
    for (i, r) in rows.iter().enumerate().take(last).skip(state.offset) {
        let selected = state.selected == Some(i);
        // **A selection is blue with a darker edge**, not a lighter grey (M11 Part E, batch 2).
        // The reference draws a one-pixel border in the same blue the focus ring uses and fills
        // the inside with a gradient, and that border is what separates a selected row from the
        // row above it — without it two adjacent selections would merge into one block.
        //
        // **Hover is quieter than selection and loses to it** (batch 3) — *unless nothing is
        // selected*, in which case it is the answer and gets the blue (batch 5). The rule is
        // still "one primary highlight": two of equal weight is two answers to "what happens if
        // I act now", and where there is no keyboard selection to compete with, the pointer's is
        // not competing. The applications modal is exactly that list — it keeps no selection at
        // all, so every hover landed on the quiet branch and the menu highlighted in grey.
        let primary = selected || (hovered == Some(r.key) && state.selected.is_none());
        let row_el = if primary {
            stack(alloc::vec![
                fill(theme.focus_ring),
                padding(Insets::all(1), bevel(theme.selection)),
                padding(ROW_PAD, text(r.label)),
            ])
        } else {
            let ground = if hovered == Some(r.key) { theme.face_hover } else { theme.track };
            stack(alloc::vec![fill(ground), padding(ROW_PAD, text(r.label))])
        };
        let mut item =
            sized(Size::new(0, row_height), row_el).key(r.key).on_press(activate(r.key));
        // **A press *down* on a row, for the caller that needs the gesture rather than the
        // click.** Dragging a row somewhere is decided the moment the button lands on it — by
        // the time it comes up the drag is over — and the row a press landed on is a fact this
        // widget has and its caller would otherwise recompute from the pointer's y, the row
        // height and the scroll offset. Three numbers to keep in step with this function is how
        // two implementations of "which row is that" come to disagree.
        //
        // `on_press` on the same element does **not** shadow it: the router's shadowing rule
        // compares depth, and these are the same element (M10 Part E).
        if let Some(f) = grab {
            item = item.on_press_down(f(r.key));
        }
        items.push(item);
    }

    let list = column(items);
    let body = if rows.len() > visible {
        let bar = ScrollState {
            offset: state.offset as u32,
            visible: visible as u32,
            total: rows.len() as u32,
        };
        // **The bar takes the pointer when the caller has somewhere to send it** (M11 Part E
        // batch 6). It was built without a handler, so a list's scrollbar showed a position and
        // could not be dragged — a control that looks live and is not, which is the defect this
        // toolkit's own notes keep naming. `nxterm` builds its scrollbar directly and has always
        // wired this; a list's was simply never offered.
        let mut bar_el = scrollbar(bar, SCROLLBAR_W, height, theme);
        if let Some(f) = scroll {
            bar_el = bar_el.on_pointer(f);
        }
        row(alloc::vec![list.flex(1), bar_el])
    } else {
        list
    };
    stack(alloc::vec![fill(theme.track), body]).focusable()
}

/// How wide a list's scrollbar is, in pixels.
const SCROLLBAR_W: u32 = 10;

#[cfg(test)]
mod list_view_tests {
    use super::*;
    use libdraw::format::Rgb;
    use crate::element::Node;

    fn rows<'a>(labels: &'a [(u64, &'a str)]) -> alloc::vec::Vec<ListRow<'a>> {
        labels.iter().map(|&(key, label)| ListRow { key, label }).collect()
    }

    /// The whole point of the widget: a hundred rows cost as many elements as fit.
    #[test]
    fn only_the_visible_rows_become_elements() {
        let data: alloc::vec::Vec<(u64, &str)> = (0..100u64).map(|i| (i, "row")).collect();
        let r = rows(&data);
        let e: Element<u64> =
            list_view(&r, &mut ListState::default(), 100, 20, |k| k, None, None, None, &Theme::default());
        assert_eq!(keys(&e).len(), 5, "the list built rows it cannot show");
    }

    /// Without keys the diff pairs by position, and its own doc names the failure: insert at
    /// the top and "row 2's widget" pairs with "row 3's element".
    #[test]
    fn every_row_carries_its_key_not_its_index() {
        let data = [(70u64, "a"), (80, "b"), (90, "c")];
        let e: Element<u64> =
            list_view(&rows(&data), &mut ListState::default(), 100, 20, |k| k, None, None, None, &Theme::default());
        assert_eq!(keys(&e), alloc::vec![70, 80, 90], "rows are keyed by position");
    }

    /// The window-list caller: rows reorder in place on every raise.
    #[test]
    fn a_reordered_window_list_keeps_each_rows_identity() {
        let before = [(1u64, "term"), (2, "editor")];
        let after = [(2u64, "editor"), (1, "term")];
        let a: Element<u64> =
            list_view(&rows(&before), &mut ListState::default(), 100, 20, |k| k, None, None, None, &Theme::default());
        let b: Element<u64> =
            list_view(&rows(&after), &mut ListState::default(), 100, 20, |k| k, None, None, None, &Theme::default());
        assert_eq!(keys(&a), alloc::vec![1, 2]);
        assert_eq!(keys(&b), alloc::vec![2, 1], "the reorder did not move the keys");
    }

    /// The launcher caller: results are replaced wholesale on every keystroke, so the list
    /// gets shorter under a scroll offset that was valid a frame ago.
    #[test]
    fn a_list_that_shrinks_under_a_stale_offset_still_renders() {
        let long: alloc::vec::Vec<(u64, &str)> = (0..20u64).map(|i| (i, "hit")).collect();
        let mut state = ListState { selected: Some(19), offset: 0 };
        let _: Element<u64> =
            list_view(&rows(&long), &mut state, 100, 20, |k| k, None, None, None, &Theme::default());
        assert_eq!(state.offset, 15, "the scroll did not follow the selection");
        let short = [(0u64, "hit"), (1, "hit"), (2, "hit")];
        let e: Element<u64> =
            list_view(&rows(&short), &mut state, 100, 20, |k| k, None, None, None, &Theme::default());
        assert_eq!(state.offset, 0, "a stale offset survived the list shrinking");
        assert_eq!(keys(&e).len(), 3, "the list rendered blank");

        // **The selection half, which the first version of this test did not assert and so
        // passed against a widget that left it dangling** (PR #233 review, finding 2). A
        // selection of 19 into three rows highlights nothing, and `down` is *dead*: it takes
        // `Some(i) if i + 1 < len` and otherwise returns `i` unchanged, so a stale index never
        // comes back into range on its own.
        assert_eq!(state.selected, Some(2), "the selection still indexes the longer list");
        assert!(
            row_bevels(&e).iter().any(|f| *f == Some(Theme::default().selection)),
            "no row is painted as selected"
        );
        assert!(!state.down(3), "the selection is already on the last row");
        assert!(state.up(), "the arrow keys are dead after the shrink");
        assert_eq!(state.selected, Some(1));
    }

    /// Shrinking to nothing leaves nothing selected, rather than row `-1`.
    #[test]
    fn a_list_that_empties_clears_the_selection() {
        let mut state = ListState { selected: Some(3), offset: 2 };
        let _: Element<u64> =
            list_view(&[], &mut state, 100, 20, |k| k, None, None, None, &Theme::default());
        assert_eq!(state.selected, None, "an empty list kept a selection");
        assert_eq!(state.offset, 0);
    }

    /// A selection moved with the keyboard must stay on screen.
    #[test]
    fn the_scroll_follows_the_selection_in_both_directions() {
        let mut s = ListState { selected: Some(7), offset: 0 };
        s.ensure_visible(5);
        assert_eq!(s.offset, 3, "scrolling down did not bring the selection into view");
        s.selected = Some(1);
        s.ensure_visible(5);
        assert_eq!(s.offset, 1, "scrolling up did not bring the selection into view");
        s.selected = Some(3);
        s.ensure_visible(5);
        assert_eq!(s.offset, 1, "a visible selection scrolled anyway");
    }

    /// Down from nothing selects the first row.
    #[test]
    fn down_from_nothing_selects_the_first_row() {
        let mut s = ListState::default();
        assert!(s.down(3));
        assert_eq!(s.selected, Some(0));
        assert!(s.down(3));
        assert_eq!(s.selected, Some(1));
    }

    /// Both ends stop rather than wrapping, and report no change.
    #[test]
    fn the_selection_stops_at_both_ends() {
        let mut s = ListState { selected: Some(2), offset: 0 };
        assert!(!s.down(3), "the selection moved past the last row");
        s.selected = Some(0);
        assert!(!s.up(), "the selection moved above the first row");
        let mut empty = ListState::default();
        assert!(!empty.up());
        assert_eq!(empty.selected, None);
    }

    /// Enter and Escape belong to whatever owns the list.
    #[test]
    fn declines_the_keys_that_belong_above_it() {
        for key in [28u16, 1, 15] {
            let mut s = ListState { selected: Some(1), offset: 0 };
            assert!(!s.apply(key, 4), "keycode {key} was claimed by the list");
            assert_eq!(s.selected, Some(1));
        }
        let mut s = ListState { selected: Some(1), offset: 0 };
        assert!(s.apply(108, 4), "Down was not claimed");
        assert_eq!(s.selected, Some(2));
    }

    #[test]
    fn a_menu_item_highlights_under_the_pointer_and_is_flat_otherwise() {
        let p = Theme::default();
        let hot: Element<u8> = menu_item("Clear", 1, true, &p);
        let cold: Element<u8> = menu_item("Clear", 1, false, &p);

        let fills = |e: &Element<u8>| {
            let mut out = alloc::vec::Vec::new();
            walk(e, &mut |n| {
                if let Node::Fill(c) = &n.node {
                    out.push(*c);
                }
            });
            out
        };
        let bevels = |e: &Element<u8>| {
            let mut out = alloc::vec::Vec::new();
            walk(e, &mut |n| {
                if let Node::Bevel(c) = &n.node {
                    out.push(*c);
                }
            });
            out
        };

        // The same two layers a selected list row gets: a border in the focus blue, and the
        // selection colour bevelled inside it.
        assert_eq!(fills(&hot), alloc::vec![p.focus_ring], "no border on the hovered item");
        assert_eq!(bevels(&hot), alloc::vec![p.selection], "no selection fill on the hovered item");

        // **And nothing at all otherwise**, which is the half that fails if a highlight sticks:
        // an item that paints a face when it is not hovered is a menu with every row lit.
        assert!(fills(&cold).is_empty() && bevels(&cold).is_empty(), "an idle item drew a face");
    }

    #[test]
    fn the_scrollbar_takes_the_pointer_only_when_the_caller_offered_somewhere_to_send_it() {
        // **The missing half was the handler, not the arithmetic.** `ScrollState::offset_at` has
        // been right since M5 and `nxterm` has always dragged with it; a *list's* bar was built
        // without an `on_pointer` at all, so the events never left the router. This asserts the
        // wiring, and the `None` case is the control — without it the test would pass for a
        // widget that attached a handler unconditionally, which is a different bug.
        let p = Theme::default();
        let many: alloc::vec::Vec<(u64, &str)> = (0..20u64).map(|i| (i, "x")).collect();
        let handlers = |e: &Element<u64>| {
            let mut n = 0;
            walk(e, &mut |c| {
                if c.on_pointer.is_some() {
                    n += 1;
                }
            });
            n
        };
        let with: Element<u64> = list_view(
            &rows(&many),
            &mut ListState::default(),
            100,
            20,
            |k| k,
            None,
            Some(|_| 0),
            None,
            &p,
        );
        assert_eq!(handlers(&with), 1, "the scrollbar took no pointer handler");
        let without: Element<u64> =
            list_view(&rows(&many), &mut ListState::default(), 100, 20, |k| k, None, None, None, &p);
        assert_eq!(handlers(&without), 0, "a handler appeared with nowhere to send it");
    }

    #[test]
    fn a_drag_on_the_track_moves_the_offset_and_a_release_does_not() {
        // **The scrollbar was decoration.** `list_view` built one and gave it no pointer handler,
        // so a list showed its position and could not be dragged — a control that looks live and
        // is not, which is the defect this crate's own notes keep naming. `nxterm` builds its
        // scrollbar directly and has always wired this (M11 Part E batch 6).
        let mut st = ListState::default();
        // Twenty rows of 20px in a 100px viewport: five visible, fifteen of travel.
        st.drag_to(100, 20, 20, 100);
        assert!(st.offset > 0, "a drag to the bottom of the track moved nothing");
        let bottom = st.offset;
        st.drag_to(100, 20, 20, 0);
        assert_eq!(st.offset, 0, "a drag to the top did not come back");
        assert!(bottom <= 15, "the offset ran past the last full screen of rows");

        // A list that fits has nowhere to go, and must not be moved by a drag on a track that is
        // not drawn — the case `offset_at` returns zero for.
        let mut st = ListState::default();
        st.drag_to(100, 20, 3, 100);
        assert_eq!(st.offset, 0, "a list shorter than its viewport scrolled");
    }

    #[test]
    fn with_nothing_selected_the_hovered_row_is_the_highlight() {
        // **The applications modal is this list**, and it keeps no selection at all — Enter takes
        // the first filtered entry — so before batch 5 every hover landed on the quiet branch and
        // a menu that is nothing *but* hover highlighted in grey. One primary highlight is the
        // rule; where there is no selection to compete with, the pointer's is not competing.
        let p = Theme::default();
        let data = [(1u64, "a"), (2, "b")];
        let e: Element<u64> =
            list_view(&rows(&data), &mut ListState::default(), 100, 20, |k| k, None, None, Some(2), &p);
        assert_eq!(row_faces(&e)[1], p.focus_ring, "the hovered row has no border");
        assert_eq!(row_bevels(&e)[1], Some(p.selection), "the hovered row is not the blue");
        assert_eq!(row_faces(&e)[0], p.track, "an untouched row reacted");
    }

    #[test]
    fn a_hovered_row_is_quieter_than_a_selected_one_and_loses_to_it() {
        let p = Theme::default();
        let data = [(1u64, "a"), (2, "b")];
        // Row 1 selected, row 0 hovered: two different highlights, and they must not be the
        // same weight — two answers to "what happens if I act now" is one too many.
        let e: Element<u64> = list_view(
            &rows(&data),
            &mut ListState { selected: Some(1), offset: 0 },
            100,
            20,
            |k| k,
            None,
            None,
            Some(1),
            &p,
        );
        let faces = row_faces(&e);
        assert_eq!(faces[0], p.face_hover, "the hovered row did not react");
        assert_eq!(faces[1], p.focus_ring, "the selected row lost its border");
        assert_eq!(row_bevels(&e)[1], Some(p.selection), "the selected row lost its fill");

        // And hovering the *selected* row leaves it selected rather than downgrading it.
        let e: Element<u64> = list_view(
            &rows(&data),
            &mut ListState { selected: Some(1), offset: 0 },
            100,
            20,
            |k| k,
            None,
            None,
            Some(2),
            &p,
        );
        assert_eq!(row_faces(&e)[1], p.focus_ring, "selection lost to hover");
    }

    /// The selected row paints differently, or selection is invisible.
    #[test]
    fn the_selected_row_is_painted_differently() {
        let data = [(1u64, "a"), (2, "b")];
        let p = Theme::default();
        let e: Element<u64> =
            list_view(&rows(&data), &mut ListState { selected: Some(1), offset: 0 }, 100, 20, |k| k, None, None, None, &p);
        let faces = row_faces(&e);
        assert_eq!(faces.len(), 2);
        assert_ne!(faces[0], faces[1], "the selected row looks like the others");
        // **Two layers, and both are the claim** (M11 Part E, batch 2): a one-pixel border in
        // the focus blue, and the selection colour bevelled inside it. Asserting only the fill
        // would pass for a selection with no edge, which is the thing that makes two adjacent
        // selected rows read as one block.
        assert_eq!(faces[1], p.focus_ring, "the selected row has no border");
        assert_eq!(faces[0], p.track, "an unselected row is the list's own ground");
        let bevels = row_bevels(&e);
        assert_eq!(bevels[1], Some(p.selection), "the selected row is not the selection colour");
        assert_eq!(bevels[0], None, "an unselected row is a flat fill, not a gradient");
    }

    /// A scrollbar that is always there wastes width; one that never appears strands rows.
    #[test]
    fn the_scrollbar_appears_only_when_there_is_more_than_fits() {
        let p = Theme::default();
        let few = [(1u64, "a"), (2, "b")];
        let e: Element<u64> =
            list_view(&rows(&few), &mut ListState::default(), 100, 20, |k| k, None, None, None, &p);
        assert!(!has_row_node(&e), "a list that fits drew a scrollbar");
        let many: alloc::vec::Vec<(u64, &str)> = (0..20u64).map(|i| (i, "x")).collect();
        let e: Element<u64> =
            list_view(&rows(&many), &mut ListState::default(), 100, 20, |k| k, None, None, None, &p);
        assert!(has_row_node(&e), "a list that overflows drew no scrollbar");
    }

    /// Each row's activation message carries that row's key.
    #[test]
    fn a_rows_message_carries_its_own_key() {
        let data = [(11u64, "a"), (22, "b")];
        let e: Element<u64> =
            list_view(&rows(&data), &mut ListState::default(), 100, 20, |k| k, None, None, None, &Theme::default());
        assert_eq!(presses(&e), alloc::vec![11, 22], "a row sent another row's message");
    }

    /// Zero row height must not divide by zero.
    #[test]
    fn a_degenerate_row_height_is_not_a_division() {
        let data = [(1u64, "a")];
        let e: Element<u64> =
            list_view(&rows(&data), &mut ListState::default(), 100, 0, |k| k, None, None, None, &Theme::default());
        assert_eq!(keys(&e).len(), 0);
    }

    fn walk<Msg>(e: &Element<Msg>, f: &mut impl FnMut(&Element<Msg>)) {
        f(e);
        for c in e.children() {
            walk(c, f);
        }
    }

    fn keys<Msg>(e: &Element<Msg>) -> alloc::vec::Vec<u64> {
        let mut out = alloc::vec::Vec::new();
        walk(e, &mut |n| {
            if let Some(k) = n.key {
                out.push(k);
            }
        });
        out
    }

    fn presses(e: &Element<u64>) -> alloc::vec::Vec<u64> {
        let mut out = alloc::vec::Vec::new();
        walk(e, &mut |n| {
            if let Some(m) = n.on_press {
                out.push(m);
            }
        });
        out
    }

    /// The bevelled fill each row carries, if any — the visible face of a selected row since
    /// M11 Part E, where `row_faces` now reports the one-pixel border drawn behind it.
    fn row_bevels<Msg>(e: &Element<Msg>) -> alloc::vec::Vec<Option<Rgb>> {
        let mut out = alloc::vec::Vec::new();
        walk(e, &mut |n| {
            if n.key.is_none() {
                return;
            }
            let mut found = None;
            walk(n, &mut |c| {
                if found.is_none()
                    && let Node::Bevel(rgb) = &c.node
                {
                    found = Some(*rgb);
                }
            });
            out.push(found);
        });
        out
    }

    fn row_faces<Msg>(e: &Element<Msg>) -> alloc::vec::Vec<Rgb> {
        let mut out = alloc::vec::Vec::new();
        walk(e, &mut |n| {
            if n.key.is_none() {
                return;
            }
            let mut first = None;
            walk(n, &mut |c| {
                if first.is_none() {
                    if let Node::Fill(rgb) = &c.node {
                        first = Some(*rgb);
                    }
                }
            });
            if let Some(rgb) = first {
                out.push(rgb);
            }
        });
        out
    }

    fn has_row_node<Msg>(e: &Element<Msg>) -> bool {
        let mut found = false;
        walk(e, &mut |n| {
            if matches!(n.node, Node::Row { .. }) {
                found = true;
            }
        });
        found
    }
}

#[cfg(test)]
mod text_field_tests {
    use super::*;
    use crate::element::Node;

    /// Tab, Enter and Escape are the whole reason `on_key` returns an `Option`. A field that
    /// claimed them would make traversal, submission and dismissal impossible from a focused
    /// field — the exact failure `Element::on_key`'s doc names.
    #[test]
    fn declines_the_keys_that_belong_above_it() {
        const KEY_TAB: u16 = 15;
        const KEY_ENTER: u16 = 28;
        const KEY_ESC: u16 = 1;
        for key in [KEY_TAB, KEY_ENTER, KEY_ESC] {
            let mut f = TextFieldState::with_text("abc");
            assert!(!f.apply(key, 0), "keycode {key} was claimed by the field");
            assert_eq!(f.text(), "abc", "keycode {key} changed the text");
        }
    }

    /// Negative control for the test above: a key the field *does* claim must answer `true`,
    /// or "declines everything" would pass it.
    #[test]
    fn claims_the_keys_it_handles() {
        const KEY_A: u16 = 30;
        let mut f = TextFieldState::new();
        assert!(f.apply(KEY_A, 0), "a letter was not claimed");
        assert_eq!(f.text(), "a");
    }

    /// Ctrl-C folds to `0x03` in the keymap because a terminal needs it to. A field that
    /// inserted what `to_char` returned would put an unprintable byte in a password.
    #[test]
    fn control_characters_are_not_text() {
        const KEY_C: u16 = 46;
        let mut f = TextFieldState::new();
        assert!(!f.apply(KEY_C, librsproto::surface::MOD_CTRL), "Ctrl-C was treated as text");
        assert_eq!(f.text(), "");
        // Negative control: the same key without Ctrl *is* text.
        assert!(f.apply(KEY_C, 0));
        assert_eq!(f.text(), "c");
    }

    /// The caret is a byte offset that must never land inside a character. Every mutation
    /// keeps it on a boundary, and slicing at it is what would panic if one did not.
    #[test]
    fn the_caret_stays_on_character_boundaries() {
        let mut f = TextFieldState::new();
        f.insert('é'); // two bytes
        f.insert('x');
        assert_eq!(f.cursor(), 3);
        assert!(f.left());
        assert_eq!(f.cursor(), 2, "left stopped inside the two-byte character");
        assert!(f.left());
        assert_eq!(f.cursor(), 0);
        assert!(!f.left(), "left at the start reported a move");
        // Slicing at the cursor is what a caret-splitting render does; it panics off-boundary.
        let _ = f.text().split_at(f.cursor());
        assert!(f.right());
        assert_eq!(f.cursor(), 2, "right stopped inside the two-byte character");
    }

    /// Backspace deletes a *character*, not a byte.
    #[test]
    fn backspace_removes_a_whole_character() {
        let mut f = TextFieldState::with_text("aé");
        assert!(f.backspace());
        assert_eq!(f.text(), "a");
        assert!(f.backspace());
        assert_eq!(f.text(), "");
        assert!(!f.backspace(), "backspace on an empty field reported a change");
    }

    /// `delete` is the other direction, and is the one an implementation is most likely to
    /// omit or alias to backspace.
    #[test]
    fn delete_removes_forward_and_backspace_removes_back() {
        let mut f = TextFieldState::with_text("abc");
        f.home();
        assert!(f.delete());
        assert_eq!(f.text(), "bc", "delete removed the wrong side");
        assert_eq!(f.cursor(), 0, "delete moved the caret");
        assert!(!f.backspace(), "backspace at the start reported a change");
        f.end();
        assert!(!f.delete(), "delete at the end reported a change");
    }

    /// Home and End answer whether they moved, so a caller can skip a repaint. Pressing Home
    /// twice must not report a change the second time.
    #[test]
    fn home_and_end_report_only_real_movement() {
        let mut f = TextFieldState::with_text("abc");
        assert!(f.home());
        assert!(!f.home(), "Home at the start reported a move");
        assert!(f.end());
        assert!(!f.end(), "End at the end reported a move");
    }

    /// A mask must count characters. Repeating a byte would print three stars for a
    /// two-character string containing one multi-byte character.
    #[test]
    fn masking_counts_characters_not_bytes() {
        let f = TextFieldState::with_text("aé");
        assert_eq!(f.text().len(), 3, "the fixture is not multi-byte");
        let e: Element<()> = text_field(&f, true, WidgetState { active: true, ..Default::default() }, &Theme::default());
        assert_eq!(rendered(&e), "**", "the mask leaked the byte length");
        // Negative control: unmasked shows the real text.
        let e: Element<()> = text_field(&f, false, WidgetState::default(), &Theme::default());
        assert_eq!(rendered(&e), "aé");
    }

    /// The caret is drawn from `active`, so a field in an unfocused window does not blink one.
    #[test]
    fn the_caret_appears_only_when_active() {
        let f = TextFieldState::with_text("ab");
        let active: Element<()> =
            text_field(&f, false, WidgetState { active: true, ..Default::default() }, &Theme::default());
        let idle: Element<()> = text_field(&f, false, WidgetState::default(), &Theme::default());
        assert_eq!(row_children(&active), 3, "no caret between the two text runs");
        assert_eq!(row_children(&idle), 2, "an inactive field drew a caret");
    }

    /// The split is *at the cursor*, which is what puts the caret in the middle of the text
    /// rather than always at the end.
    #[test]
    fn the_caret_splits_the_text_at_the_cursor() {
        let mut f = TextFieldState::with_text("abcd");
        f.home();
        f.right();
        let e: Element<()> =
            text_field(&f, false, WidgetState { active: true, ..Default::default() }, &Theme::default());
        assert_eq!(runs(&e), alloc::vec!["a", "bcd"], "the caret was not placed at the cursor");
    }

    /// Every `Text` run in the tree, in order.
    fn runs<Msg>(e: &Element<Msg>) -> alloc::vec::Vec<alloc::string::String> {
        let mut out = alloc::vec::Vec::new();
        walk(e, &mut out);
        out
    }

    fn walk<Msg>(e: &Element<Msg>, out: &mut alloc::vec::Vec<alloc::string::String>) {
        if let Node::Text(s) = &e.node {
            out.push(s.clone());
        }
        for c in e.children() {
            walk(c, out);
        }
    }

    /// The concatenated text, which is what a reader of the field would see.
    fn rendered<Msg>(e: &Element<Msg>) -> alloc::string::String {
        runs(e).concat()
    }

    /// How many children the content `Row` has — two text runs, plus the caret when active.
    fn row_children<Msg>(e: &Element<Msg>) -> usize {
        fn find<Msg>(e: &Element<Msg>) -> Option<usize> {
            if let Node::Row { children, .. } = &e.node {
                return Some(children.len());
            }
            e.children().find_map(find)
        }
        find(e).expect("the field has a content row")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libdraw::format::Rgb;
    use crate::layout::{FixedCell, layout};
    use crate::paint::{Theme, paint};
    use alloc::vec;
    use libdraw::format::PixelFormat;
    use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
    use libdraw::geom::Rect;

    type Msg = ();

    const CELL: FixedCell = FixedCell { w: 8, h: 16 };

    #[test]
    fn a_thumb_is_proportional_to_what_is_visible() {
        let s = ScrollState { offset: 0, visible: 25, total: 100 };
        let (pos, len) = s.thumb(200);
        assert_eq!(len, 50, "a quarter visible is a quarter of the track");
        assert_eq!(pos, 0, "at the top");
    }

    #[test]
    fn a_thumb_reaches_the_bottom_exactly_at_the_last_line() {
        // Off by one here leaves a gap at the bottom that says "there is more" when there is
        // not, or runs the thumb past the end.
        let s = ScrollState { offset: 75, visible: 25, total: 100 };
        let (pos, len) = s.thumb(200);
        assert_eq!(pos + len, 200, "flush with the end of the track");
    }

    #[test]
    fn a_short_thumb_is_clamped_and_still_reaches_both_ends() {
        // A thumb proportional to a long scrollback shrinks below grabbing size. Clamping it
        // *after* computing the position runs it past the track's end by whatever the clamp
        // added — which is why the clamp comes first.
        let s = ScrollState { offset: 0, visible: 1, total: 10_000 };
        let (pos, len) = s.thumb(200);
        assert_eq!(len, MIN_THUMB, "clamped to something grabbable");
        assert_eq!(pos, 0);

        let s = ScrollState { offset: 9_999, visible: 1, total: 10_000 };
        let (pos, len) = s.thumb(200);
        assert_eq!(len, MIN_THUMB);
        assert_eq!(pos + len, 200, "and the clamped thumb still reaches the bottom");
    }

    #[test]
    fn nothing_to_scroll_fills_the_track() {
        // How a user sees "this is all of it". A full-height thumb that nevertheless moves is
        // the alternative, and it is nonsense.
        for s in [
            ScrollState { offset: 0, visible: 100, total: 100 },
            ScrollState { offset: 0, visible: 200, total: 100 },
        ] {
            assert_eq!(s.thumb(200), (0, 200), "{s:?}");
            assert!(!s.scrollable());
        }
    }

    #[test]
    fn degenerate_scroll_states_do_not_divide_by_zero() {
        // `total == visible` makes `max_offset` zero and `visible == 0` makes the proportion
        // zero; a scrollbar for an empty document is an ordinary thing to ask for.
        assert_eq!(ScrollState { offset: 0, visible: 0, total: 0 }.thumb(200), (0, 200));
        assert_eq!(ScrollState { offset: 5, visible: 0, total: 10 }.thumb(200), (0, 200));
        assert_eq!(ScrollState { offset: 0, visible: 10, total: 10 }.thumb(0), (0, 0));
    }

    #[test]
    fn an_offset_past_the_end_is_clamped_rather_than_overflowing() {
        // An application that scrolled past the end must not produce a thumb outside the
        // track — and the arithmetic is unsigned, so getting it wrong wraps rather than
        // going negative.
        let s = ScrollState { offset: 10_000, visible: 25, total: 100 };
        let (pos, len) = s.thumb(200);
        assert_eq!(pos + len, 200);
    }

    #[test]
    fn a_grab_maps_back_to_the_offset_the_thumb_was_drawn_for() {
        // `offset_at` is `thumb`'s inverse, and the two are the only pair in the toolkit that
        // *must* agree: one decides where the thumb is painted, the other what grabbing it
        // there means. A drift between them is a thumb that jumps away from the cursor.
        //
        // Over the whole range rather than one offset, because the arithmetic is two
        // divisions and a clamp, and the ends are where each of them goes wrong.
        //
        // **Within a pixel's worth of lines, not exactly** — and that is a fact about
        // scrollbars, not a weak assertion. A thousand lines over a ~390-pixel span puts
        // roughly three lines on every pixel, so a thumb drawn for line 500 is at the same
        // pixel as one drawn for 501, and no inverse can tell them apart. Demanding equality
        // here would be demanding the impossible; **the ends are exact**, which is the part
        // that matters and the part off-by-one errors break.
        let s = ScrollState { offset: 0, visible: 24, total: 1024 };
        let track = 400;
        let (_, len) = s.thumb(track);
        let per_pixel = (1000u32).div_ceil(track - len);
        for offset in [0, 1, 7, 500, 999, 1000] {
            let s = ScrollState { offset, ..s };
            let (pos, len) = s.thumb(track);
            let got = s.offset_at(track, pos as i32 + len as i32 / 2);
            assert!(
                got.abs_diff(offset) <= per_pixel,
                "the thumb drawn for {offset} grabs as {got}, more than {per_pixel} lines out",
            );
        }
        assert_eq!(ScrollState { offset: 0, ..s }.offset_at(track, {
            let (p, l) = ScrollState { offset: 0, ..s }.thumb(track);
            p as i32 + l as i32 / 2
        }), 0, "the top is exactly the top");
        assert_eq!(ScrollState { offset: 1000, ..s }.offset_at(track, {
            let (p, l) = ScrollState { offset: 1000, ..s }.thumb(track);
            p as i32 + l as i32 / 2
        }), 1000, "and the bottom exactly the bottom");

        // And where the arithmetic *can* be exact — fewer lines than pixels of span — it is,
        // so the tolerance above is the quantisation and not a bug hiding inside it.
        let s = ScrollState { offset: 0, visible: 10, total: 40 };
        for offset in 0..=30 {
            let s = ScrollState { offset, ..s };
            let (pos, len) = s.thumb(400);
            assert_eq!(s.offset_at(400, pos as i32 + len as i32 / 2), offset);
        }
    }

    #[test]
    fn dragging_past_either_end_of_the_track_stays_at_that_end() {
        // The router hands a captured widget coordinates outside itself rather than clamping
        // them — deliberately, so a drag is not indistinguishable from a drag that stopped at
        // the edge. Something has to clamp, and this is it.
        let s = ScrollState { offset: 300, visible: 24, total: 1024 };
        assert_eq!(s.offset_at(400, -900), 0, "dragged far above the bar");
        assert_eq!(s.offset_at(400, 9000), 1000, "and far below it");
        // The bottom of the track is the last line, not one short of it: the rounding.
        assert_eq!(s.offset_at(400, 400), 1000, "the very bottom is the end of the document");
    }

    #[test]
    fn a_bar_with_nothing_to_scroll_reports_nothing_wherever_it_is_grabbed() {
        // Otherwise a short document scrolls when its full-height thumb is dragged, which is
        // the visible form of dividing by a zero span.
        let s = ScrollState { offset: 0, visible: 24, total: 24 };
        assert!(!s.scrollable());
        for y in [-5, 0, 100, 4000] {
            assert_eq!(s.offset_at(400, y), 0, "at y={y}");
        }
        // And a track so short that `MIN_THUMB` fills it — reachable, not hypothetical.
        let s = ScrollState { offset: 5, visible: 24, total: 1024 };
        assert_eq!(s.thumb(MIN_THUMB).1, MIN_THUMB, "the premise: the thumb fills the track");
        assert_eq!(s.offset_at(MIN_THUMB, 8), 0, "a position it cannot express");
    }

    #[test]
    fn a_scrollbar_lays_its_thumb_out_where_the_arithmetic_says() {
        // The layout half: a `Column` of spacer, thumb and filler places the thumb without
        // any node needing to offset a child.
        let p = Theme::default();
        let s = ScrollState { offset: 50, visible: 25, total: 100 };
        let e: Element<Msg> = scrollbar(s, 12, 400, &p);
        let l = layout(&e, Rect::new(0, 0, 12, 400), &CELL);
        let (pos, len) = s.thumb(400);

        // stack -> [track fill, column] ; column -> [spacer, thumb, filler]
        let col = &l.children[0].children[1];
        assert_eq!(col.children[0].rect.size.h, pos, "the spacer is the thumb's offset");
        assert_eq!(col.children[1].rect.size.h, len, "and then the thumb");
        assert_eq!(col.children[1].rect.origin.y, pos as i32);
        assert_eq!(l.rect.size.w, 12, "the bar is as wide as asked");
    }

    #[test]
    fn a_button_carries_its_message_and_takes_focus() {
        let p = Theme::default();
        let e = button("OK", (), WidgetState::default(), &p);
        assert!(e.on_press.is_some(), "a button that sends nothing is not a button");
        assert!(e.focusable, "a keyboard user must be able to reach it");
    }

    #[test]
    fn a_buttons_face_follows_the_state_it_is_given() {
        // Widgets take their state as an argument rather than remembering it, so this is the
        // whole of a button's appearance logic and it is a pure function.
        let p = Theme::default();
        let face_of = |st: WidgetState| {
            let e: Element<Msg> = button("OK", (), st, &p);
            // The first layer is the face at rest and the ring when active; either way the
            // painted face is the last `Fill` before the label.
            let fills: vec::Vec<Rgb> = e
                .children()
                .filter_map(|c| match &c.node {
                    crate::element::Node::Fill(c) => Some(*c),
                    crate::element::Node::Padding { child, .. } => match &child.node {
                        crate::element::Node::Fill(c) => Some(*c),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            *fills.last().expect("a button has a face")
        };
        assert_eq!(face_of(WidgetState::default()), p.face);
        assert_eq!(face_of(WidgetState { hovered: true, ..Default::default() }), p.face_hover);
        assert_eq!(
            face_of(WidgetState { hovered: true, pressed: true, ..Default::default() }),
            p.face_pressed,
            "pressed wins over hovered — a held button is held wherever the pointer is"
        );
    }

    #[test]
    fn a_focused_button_draws_a_ring_that_the_face_does_not_cover() {
        // Painted rather than inspected: the ring is the outermost layer, so a face drawn
        // over the whole area would hide it and the tree would still look right.
        let p = Theme::default();
        let t = Theme::default();
        let mut fb =
            MemFramebuffer::new(Geometry::packed(80, 40, PixelFormat::XRGB8888));
        let e: Element<Msg> =
            button("OK", (), WidgetState { active: true, ..Default::default() }, &p);
        let l = layout(&e, Rect::new(0, 0, 80, 40), &CELL);
        paint(&mut fb, &font(), &t, &e, &l, Rect::new(0, 0, 80, 40), &mut |_, _, _, _: &mut MemFramebuffer| {});
        assert_eq!(fb.get_pixel(0, 0), Some(p.focus_ring), "the ring is on the edge");
        assert_eq!(fb.get_pixel(40, 20), Some(p.face), "and the face is inside it");
    }

    #[test]
    fn an_unfocused_button_draws_no_ring() {
        let p = Theme::default();
        let t = Theme::default();
        let mut fb = MemFramebuffer::new(Geometry::packed(80, 40, PixelFormat::XRGB8888));
        let e: Element<Msg> = button("OK", (), WidgetState::default(), &p);
        let l = layout(&e, Rect::new(0, 0, 80, 40), &CELL);
        paint(&mut fb, &font(), &t, &e, &l, Rect::new(0, 0, 80, 40), &mut |_, _, _, _: &mut MemFramebuffer| {});
        assert_eq!(fb.get_pixel(0, 0), Some(p.face), "face all the way to the edge");
    }

    #[test]
    fn a_menu_bar_is_as_tall_as_asked_and_no_taller() {
        let p = Theme::default();
        let e: Element<Msg> = menu_bar(vec![text("File"), text("Edit")], 24, &p);
        let l = layout(&e, Rect::new(0, 0, 200, 100), &CELL);
        assert_eq!(l.rect.size.h, 24);
        assert_eq!(l.rect.size.w, 200, "and spans the width it is given");
    }

    #[test]
    fn a_menu_bars_items_each_get_their_own_width() {
        // **The test this file was missing.** Every other one here lays *one* widget into a
        // rectangle of its own, where a widget that measures to "everything available" and one
        // that measures to its content are indistinguishable. Put two beside each other and
        // they are not: until 2026-08-11 `Node::Fill` measured to `c.max`, so the first
        // `button` — a `Stack` over a `Fill` — took the whole row and the second laid out at
        // zero width, off the right edge. The bar looked correct in every assertion above.
        let p = Theme::default();
        let items: vec::Vec<Element<Msg>> = vec![
            button("File", (), WidgetState::default(), &p),
            button("Edit", (), WidgetState::default(), &p),
        ];
        let e = menu_bar(items, 24, &p);
        let l = layout(&e, Rect::new(0, 0, 200, 100), &CELL);
        // sized -> stack -> [face fill, row] ; row -> the two buttons
        let row = &l.children[0].children[1];
        let (a, b) = (row.children[0].rect, row.children[1].rect);
        assert!(a.size.w > 0, "the first item measured to nothing");
        assert!(b.size.w > 0, "the second item got no width — the first one ate the row");
        assert_eq!(b.origin.x, a.origin.x + a.size.w as i32, "and they sit side by side");
        assert!(
            b.right() <= 200,
            "the second item runs off the bar: {:?}",
            b
        );
    }

    #[test]
    fn a_button_measures_to_its_label_not_to_the_room_it_is_given() {
        // The same defect stated as a property rather than as a composition. A button in a
        // 400-pixel-tall column is not a 400-pixel-tall button.
        let p = Theme::default();
        let e: Element<Msg> = button("OK", (), WidgetState::default(), &p);
        let big = crate::layout::measure(
            &e,
            crate::layout::Constraints::loose(libdraw::geom::Size::new(400, 400)),
            &CELL,
        );
        let small = crate::layout::measure(
            &e,
            crate::layout::Constraints::loose(libdraw::geom::Size::new(100, 100)),
            &CELL,
        );
        assert_eq!(big, small, "a button's size must not depend on the room around it");
        assert!(big.h < 100, "a two-line-tall button: {big:?}");
    }

    #[test]
    fn a_title_bars_buttons_sit_at_its_right_edge_and_the_drag_face_takes_the_rest() {
        // **The layout property a person notices**, and the one that decides where a press
        // lands: the buttons are at the right, the title runs from the left, and everything
        // between them is draggable. A `flex(1)` label that measured to its text instead would
        // leave the buttons floating next to the title in the middle of the bar.
        #[derive(Clone, PartialEq, Debug)]
        enum M {
            Drag,
            Min,
            Max,
            Close,
        }
        let p = Theme::default();
        let e = title_bar(
            "a terminal",
            true,
            M::Drag,
            TitleButtons {
                minimise: Some(M::Min),
                maximise: Some(M::Max),
                close: Some(M::Close),
            },
            &p,
        );
        let l = layout(&e, Rect::new(0, 0, 400, 100), &CELL);
        // sized -> stack -> [face, row] ; row -> [label, min, max, close]
        let stack = &l.children[0];
        let face = stack.children[0].rect;
        let row = &stack.children[1];
        assert_eq!(face.size.h, TITLE_BAR_H, "the draggable face is the bar");
        assert_eq!(face.size.w, 400, "and it spans it");

        let close = row.children[3].rect;
        assert_eq!(close.right(), 400, "the close button is at the right edge");
        assert_eq!(close.size.w, TITLE_BUTTON_W);
        let label = row.children[0].rect;
        assert_eq!(label.origin.x, 0, "the title starts at the left");
        assert!(
            label.right() <= row.children[1].rect.origin.x as i64,
            "the title overlaps the buttons: {label:?} vs {:?}",
            row.children[1].rect
        );
    }

    // ---- the text area (M10 Part C) ----

    /// `abc` / `de` / `fghi`, cursor at the start.
    fn area() -> TextAreaState {
        TextAreaState::with_text("abc\nde\nfghi")
    }

    /// How many `Fill`s of `colour` the tree holds — the caret is one, and a selection's
    /// highlight is one per line it covers.
    ///
    /// **What `text_area` draws had no host coverage in either direction** until PR #258's
    /// review; `check-display` renders one arrangement of it and a picture cannot count.
    fn fills<M>(e: &Element<M>, colour: Rgb) -> usize {
        fn walk<M>(e: &Element<M>, colour: Rgb, n: &mut usize) {
            if matches!(&e.node, crate::element::Node::Fill(rgb) if *rgb == colour) {
                *n += 1;
            }
            for c in e.children() {
                walk(c, colour, n);
            }
        }
        let mut n = 0;
        walk(e, colour, &mut n);
        n
    }

    const KEY_A: u16 = 30;
    const KEY_X: u16 = 45;

    #[test]
    fn a_buffer_round_trips_its_text_and_keeps_a_trailing_empty_line() {
        // **What was opened is what is saved.** A file that ended with a newline has a final
        // empty line; one that did not, does not — and an editor that "helpfully" added one
        // would rewrite every file it touched on the first save.
        for src in ["abc\nde", "abc\nde\n", "", "\n"] {
            assert_eq!(TextAreaState::with_text(src).text(), src, "round trip of {src:?}");
        }
        // A carriage return is dropped rather than kept: an invisible character at the end of
        // every line is one the cursor has to step over and nobody can see.
        assert_eq!(TextAreaState::with_text("a\r\nb").text(), "a\nb");
    }

    #[test]
    fn an_empty_buffer_is_one_empty_line_so_the_cursor_always_has_somewhere_to_be() {
        let a = TextAreaState::new();
        assert_eq!(a.lines().len(), 1);
        assert_eq!(a.cursor(), (0, 0));
    }

    #[test]
    fn typing_inserts_and_enter_splits_the_line() {
        let mut a = TextAreaState::new();
        for k in [KEY_A, KEY_A] {
            a.apply(k, 0);
        }
        a.apply(KEY_ENTER, 0);
        a.apply(KEY_X, 0);
        assert_eq!(a.text(), "aa\nx");
        assert_eq!(a.cursor(), (1, 1));
    }

    #[test]
    fn backspace_at_the_start_of_a_line_joins_it_to_the_one_above() {
        // The case a single-line field never has. The cursor lands **where the join happened**,
        // which is where the text the person was deleting towards now is — not at the start of
        // the merged line.
        let mut a = area();
        a.apply(KEY_DOWN, 0);
        assert_eq!(a.cursor(), (1, 0));
        assert!(a.apply(KEY_BACKSPACE, 0));
        assert_eq!(a.text(), "abcde\nfghi");
        assert_eq!(a.cursor(), (0, 3), "at the join, not at the start of the line");
    }

    #[test]
    fn delete_at_the_end_of_a_line_pulls_the_next_one_up() {
        let mut a = area();
        a.apply(KEY_END, 0);
        assert!(a.apply(KEY_DELETE, 0));
        assert_eq!(a.text(), "abcde\nfghi");
        assert_eq!(a.cursor(), (0, 3));
    }

    #[test]
    fn backspace_at_the_very_start_and_delete_at_the_very_end_do_nothing() {
        let mut a = area();
        assert!(!a.apply(KEY_BACKSPACE, 0));
        let mut a = area();
        for _ in 0..2 {
            a.apply(KEY_DOWN, 0);
        }
        a.apply(KEY_END, 0);
        assert!(!a.apply(KEY_DELETE, 0));
        assert_eq!(a.text(), "abc\nde\nfghi", "and neither changed the buffer");
    }

    #[test]
    fn vertical_movement_keeps_the_column_it_was_aiming_for() {
        // **The goal column.** Down from column 3 of `abc` onto `de` (length 2) clamps to 2 —
        // and coming back up must return to 3, not stay at 2. Without it a person who pressed
        // only Down and Up has had their column moved for them.
        let mut a = area();
        a.apply(KEY_END, 0);
        assert_eq!(a.cursor(), (0, 3));
        a.apply(KEY_DOWN, 0);
        assert_eq!(a.cursor(), (1, 2), "clamped to the short line's end");
        a.apply(KEY_DOWN, 0);
        assert_eq!(a.cursor(), (2, 3), "and back out to the goal on a line long enough");
        a.apply(KEY_UP, 0);
        a.apply(KEY_UP, 0);
        assert_eq!(a.cursor(), (0, 3), "all the way back to where it started");
    }

    #[test]
    fn a_horizontal_move_gives_up_the_goal_column() {
        // Otherwise the goal outlives the intent that set it: press Down, Left, Down, and the
        // second Down would jump back out to a column the person just moved away from.
        let mut a = area();
        a.apply(KEY_END, 0);
        a.apply(KEY_DOWN, 0);
        a.apply(KEY_LEFT, 0);
        assert_eq!(a.cursor(), (1, 1));
        a.apply(KEY_DOWN, 0);
        assert_eq!(a.cursor(), (2, 1), "the new column, not the old goal");
    }

    #[test]
    fn shift_extends_a_selection_and_an_unshifted_move_drops_it() {
        let mut a = area();
        for _ in 0..2 {
            a.apply(KEY_RIGHT, MOD_SHIFT);
        }
        assert_eq!(a.selection(), Some(((0, 0), (0, 2))));
        assert_eq!(a.selected_text().as_deref(), Some("ab"));

        a.apply(KEY_RIGHT, 0);
        assert_eq!(a.selection(), None, "an unshifted move drops it");
    }

    #[test]
    fn a_selection_reads_the_same_whichever_way_it_was_made() {
        // The anchor may be before or after the cursor; every consumer wants document order.
        let mut a = area();
        a.apply(KEY_END, 0);
        for _ in 0..2 {
            a.apply(KEY_LEFT, MOD_SHIFT);
        }
        assert_eq!(a.selection(), Some(((0, 1), (0, 3))));
        assert_eq!(a.selected_text().as_deref(), Some("bc"));
    }

    #[test]
    fn a_selection_spanning_lines_reads_the_newlines_back() {
        let mut a = area();
        a.apply(KEY_RIGHT, 0);
        a.apply(KEY_DOWN, MOD_SHIFT);
        a.apply(KEY_DOWN, MOD_SHIFT);
        assert_eq!(a.selected_text().as_deref(), Some("bc\nde\nf"));
    }

    #[test]
    fn typing_over_a_selection_replaces_it() {
        // **The rule that makes a selection worth having.** An editor where typing appends
        // beside a highlighted run rather than replacing it is one nobody can use.
        let mut a = area();
        a.apply(KEY_DOWN, MOD_SHIFT);
        a.apply(KEY_END, MOD_SHIFT);
        assert_eq!(a.selected_text().as_deref(), Some("abc\nde"));
        a.apply(KEY_X, 0);
        assert_eq!(a.text(), "x\nfghi");
        assert_eq!(a.selection(), None);
        assert_eq!(a.cursor(), (0, 1));
    }

    #[test]
    fn backspace_over_a_selection_deletes_the_selection_and_not_a_character() {
        let mut a = area();
        for _ in 0..2 {
            a.apply(KEY_RIGHT, MOD_SHIFT);
        }
        assert!(a.apply(KEY_BACKSPACE, 0));
        assert_eq!(a.text(), "c\nde\nfghi", "the two selected characters, not three");
    }

    #[test]
    fn the_view_scrolls_to_follow_the_cursor_and_the_widget_does_it() {
        // **`ensure_visible` is the widget's to call, not the application's.** A caller that had
        // to remember it would have a cursor that walks off the bottom of its own window — and
        // the widget takes `&mut` precisely so it cannot be forgotten (PR #257 review).
        let mut a = TextAreaState::with_text("0\n1\n2\n3\n4\n5\n6\n7");
        let p = Theme::default();
        let _: Element<()> = text_area(&mut a, 3 * 16, 16, true, &p);
        assert_eq!(a.offset(), 0);

        for _ in 0..5 {
            a.apply(KEY_DOWN, 0);
        }
        let _: Element<()> = text_area(&mut a, 3 * 16, 16, true, &p);
        assert_eq!(a.offset(), 3, "line 5 is visible in a three-line window");

        for _ in 0..5 {
            a.apply(KEY_UP, 0);
        }
        let _: Element<()> = text_area(&mut a, 3 * 16, 16, true, &p);
        assert_eq!(a.offset(), 0, "and it scrolls back the other way");
    }

    #[test]
    fn the_revision_counts_edits_and_not_movement() {
        // **What an editor's "modified" marker is made of.** The alternatives it replaces are
        // both wrong: comparing byte length misses replacing a one-character selection with one
        // character, and re-deriving which keycodes edit is a second copy of `apply`'s dispatch.
        let mut a = area();
        let start = a.revision();

        for k in [KEY_RIGHT, KEY_DOWN, KEY_END, KEY_HOME, KEY_UP, KEY_LEFT] {
            a.apply(k, 0);
        }
        assert_eq!(a.revision(), start, "moving is not editing");

        // **Typed with nothing selected, and that is not incidental.** The first version made a
        // selection first and then typed, so the insert's own bump was covered by the deletion
        // of the selection — the assertion passed with `insert` not counting at all.
        a.apply(KEY_X, 0);
        assert!(a.revision() > start, "typing is");

        let mut sel = area();
        let quiet = sel.revision();
        for _ in 0..2 {
            sel.apply(KEY_RIGHT, MOD_SHIFT);
        }
        assert_eq!(sel.revision(), quiet, "nor is selecting");

        // The case a length comparison cannot see: one character selected, one typed.
        let mut b = TextAreaState::with_text("abc");
        b.apply(KEY_RIGHT, MOD_SHIFT);
        let before = b.revision();
        let len = b.text().len();
        b.apply(KEY_X, 0);
        assert_eq!(b.text().len(), len, "the fixture must keep its length, or it proves nothing");
        assert!(b.revision() > before, "replacing a selection is an edit");

        // And an edit that does nothing is not one.
        let mut c = TextAreaState::with_text("abc");
        let quiet = c.revision();
        assert!(!c.apply(KEY_BACKSPACE, 0), "backspace at the start of the buffer does nothing");
        assert_eq!(c.revision(), quiet, "so it is not an edit");
    }

    #[test]
    fn every_edit_path_counts_its_own_edit() {
        // **One case per bump, because the bumps are not one line.** `insert` has its own and so
        // does the `delete_selection` it calls first; `backspace` and `delete` each have two, a
        // character and a join. A test that only typed left five of the seven uncontrolled, and
        // the one that matters most is `delete_selection`'s: `backspace` and `delete` both
        // return the moment it reports it deleted something, so without its bump a person could
        // select a word, press Delete, and watch the buffer change while the title bar kept
        // saying the file was saved (PR #259 review, finding 2).
        let bumps = |setup: &dyn Fn(&mut TextAreaState)| -> u64 {
            let mut a = area();
            let before = a.revision();
            setup(&mut a);
            a.revision() - before
        };

        // A selection, deleted by each of the two keys that delete one.
        let select_two = |a: &mut TextAreaState| {
            for _ in 0..2 {
                a.apply(KEY_RIGHT, MOD_SHIFT);
            }
        };
        assert!(
            bumps(&|a| {
                select_two(a);
                assert!(a.apply(KEY_BACKSPACE, 0));
            }) > 0,
            "backspace over a selection"
        );
        assert!(
            bumps(&|a| {
                select_two(a);
                assert!(a.apply(KEY_DELETE, 0));
            }) > 0,
            "delete over a selection"
        );

        assert!(bumps(&|a| a.newline()) > 0, "enter splits a line");

        // Backspace's two paths: a character, and the join at the start of a line.
        assert!(
            bumps(&|a| {
                a.apply(KEY_RIGHT, 0);
                assert!(a.apply(KEY_BACKSPACE, 0));
            }) > 0,
            "backspace over a character"
        );
        assert!(
            bumps(&|a| {
                a.apply(KEY_DOWN, 0);
                a.apply(KEY_HOME, 0);
                assert!(a.apply(KEY_BACKSPACE, 0));
            }) > 0,
            "backspace joining two lines"
        );

        // Delete's two, the same shape from the other side.
        assert!(bumps(&|a| assert!(a.apply(KEY_DELETE, 0))) > 0, "delete over a character");
        assert!(
            bumps(&|a| {
                a.apply(KEY_END, 0);
                assert!(a.apply(KEY_DELETE, 0));
            }) > 0,
            "delete joining two lines"
        );

        // And the two that change nothing still count nothing, so "an edit" means an edit.
        let mut ends = TextAreaState::with_text("a");
        let quiet = ends.revision();
        assert!(!ends.apply(KEY_BACKSPACE, 0), "backspace at the buffer's start");
        ends.apply(KEY_END, 0);
        assert!(!ends.apply(KEY_DELETE, 0), "delete at its end");
        assert_eq!(ends.revision(), quiet);
    }

    #[test]
    fn the_caret_is_drawn_at_either_end_of_a_selection() {
        // **The half a picture cannot check.** `check-display`'s reference builds its selection
        // with `Shift+Right`, so the gate compares a *forward* one — and the caret was drawn
        // only after the highlight, which a forward selection satisfies and a backward one
        // never does. Both directions here, counted in the tree (PR #258 review, blocking 2).
        let p = Theme::default();
        let draw = |a: &mut TextAreaState| -> usize {
            let e: Element<()> = text_area(a, 3 * 16, 16, true, &p);
            fills(&e, p.focus_ring)
        };

        let mut a = area();
        a.apply(KEY_END, 0);
        assert_eq!(draw(&mut a), 1, "no selection at all");

        let mut a = area();
        for _ in 0..2 {
            a.apply(KEY_RIGHT, MOD_SHIFT);
        }
        assert_eq!(draw(&mut a), 1, "forward: the cursor is at the highlight's end");

        let mut a = area();
        a.apply(KEY_END, 0);
        for _ in 0..2 {
            a.apply(KEY_LEFT, MOD_SHIFT);
        }
        assert_eq!(draw(&mut a), 1, "backward: the cursor is at the highlight's start");

        let mut a = area();
        a.apply(KEY_DOWN, 0);
        a.apply(KEY_UP, MOD_SHIFT);
        assert_eq!(draw(&mut a), 1, "backward across a line break");

        let mut a = area();
        a.apply(KEY_END, 0);
        let e: Element<()> = text_area(&mut a, 3 * 16, 16, false, &p);
        assert_eq!(fills(&e, p.focus_ring), 0, "and none at all when the widget is not active");
    }

    #[test]
    fn a_selection_is_highlighted_on_every_line_it_covers() {
        // The other half of what `text_area` draws, and the reason the count is per *line*: a
        // multi-line selection is one highlight per row, not one rectangle.
        let p = Theme::default();
        let mut a = area();
        a.apply(KEY_RIGHT, 0);
        a.apply(KEY_DOWN, MOD_SHIFT);
        let e: Element<()> = text_area(&mut a, 3 * 16, 16, true, &p);
        assert_eq!(fills(&e, p.selection), 2, "the tail of line 0 and the head of line 1");

        let mut a = area();
        let e: Element<()> = text_area(&mut a, 3 * 16, 16, true, &p);
        assert_eq!(fills(&e, p.selection), 0, "and nothing when nothing is selected");
    }

    #[test]
    fn a_collapsed_selection_leaves_no_anchor_behind_an_edit() {
        // **Two keystrokes to arm and one to fire**, which is why it survived a review: walking
        // the cursor back onto its own anchor leaves no *selection* but does leave an anchor,
        // and the next edit shortens the text it names (PR #258 review, blocking 1).
        let mut a = area();
        a.apply(KEY_END, 0);
        a.apply(KEY_LEFT, MOD_SHIFT);
        a.apply(KEY_RIGHT, MOD_SHIFT);
        assert_eq!(a.selection(), None, "the cursor is back on its anchor");
        assert!(a.apply(KEY_BACKSPACE, 0));
        assert_eq!(a.text(), "ab\nde\nfghi");
        assert_eq!(a.selection(), None, "and the anchor went with the character");
        // This is where it used to panic: the anchor named byte 3 of a line now 2 long.
        let e: Element<()> = text_area(&mut a, 3 * 16, 16, true, &Theme::default());
        assert_eq!(fills(&e, Theme::default().selection), 0, "nothing is selected, so nothing \
            is highlighted");

        // The quieter symptom of the same defect: typing instead of deleting used to leave a
        // selection over the character just typed, which the next keystroke would replace.
        let mut a = area();
        a.apply(KEY_RIGHT, MOD_SHIFT);
        a.apply(KEY_LEFT, MOD_SHIFT);
        a.insert('x');
        assert_eq!(a.text(), "xabc\nde\nfghi");
        assert_eq!(a.selection(), None, "typing selects nothing");

        // And by pointer, which arms it the same way: a press and a release that never moved.
        let mut a = area();
        a.place(0, 3);
        a.extend_to(0, 3);
        assert!(a.apply(KEY_BACKSPACE, 0));
        assert_eq!(a.text(), "ab\nde\nfghi");
        let _: Element<()> = text_area(&mut a, 3 * 16, 16, true, &Theme::default());
    }

    #[test]
    fn a_press_places_the_cursor_and_a_drag_selects_from_where_it_landed() {
        // The pointer half, which the state owns because the pixel-to-cell arithmetic is the
        // application's — it knows its own metrics — and what a press *means* is not.
        let mut a = area();
        a.place(2, 2);
        assert_eq!(a.cursor(), (2, 2));
        assert_eq!(a.selection(), None, "a press starts no selection");

        a.extend_to(0, 1);
        assert_eq!(a.selection(), Some(((0, 1), (2, 2))), "the drag selects from the press");
        assert_eq!(a.selected_text().as_deref(), Some("bc\nde\nfg"));
    }

    #[test]
    fn a_press_past_the_end_of_a_line_lands_on_its_last_character_boundary() {
        // The coordinates come from arithmetic on a pixel position, which knows nothing about
        // encoding or line lengths. Clamping is the widget's job, not the caller's.
        let mut a = area();
        a.place(1, 99);
        assert_eq!(a.cursor(), (1, 2));
        a.place(99, 0);
        assert_eq!(a.cursor(), (2, 0), "and past the last line lands on the last line");
    }

    #[test]
    fn enter_is_the_text_areas_and_tab_is_not() {
        // **The one widget for which Enter is text rather than submission** — that is what
        // multi-line means. Tab stays traversal's: a buffer that swallowed it would trap the
        // keyboard inside itself with no way out.
        const KEY_TAB: u16 = 15;
        let mut a = TextAreaState::new();
        assert!(a.apply(KEY_ENTER, 0));
        assert_eq!(a.lines().len(), 2);
        assert!(!a.apply(KEY_TAB, 0), "Tab is not claimed");
        assert_eq!(a.text(), "\n", "and it inserted nothing");
    }

    #[test]
    fn a_control_chord_is_not_text() {
        // `to_char` folds Ctrl-C to 0x03 because a terminal needs it to; an editor that
        // inserted that would put an unprintable byte in somebody's file.
        const KEY_C: u16 = 46;
        let mut a = TextAreaState::new();
        assert!(!a.apply(KEY_C, librsproto::surface::MOD_CTRL));
        assert_eq!(a.text(), "");
    }

    #[test]
    fn a_grip_is_a_square_that_reports_its_press_rather_than_its_click() {
        // **At the press, like the title bar's drag**, because a resize is a gesture that
        // *begins* there: a grip that waited for the click would hand the compositor a drag
        // whose button was already up. And it measures its own square, so a caller placing it
        // in a corner has a number to place it by.
        use crate::diff::Tree;
        use crate::layout::{Constraints, measure};
        use crate::route::Router;
        use librsproto::surface::{POINTER_BUTTON, POINTER_PRESSED, PointerEvent};

        #[derive(Clone, PartialEq, Debug)]
        struct Resize;
        let p = Theme::default();
        let e = resize_grip(Resize, &p);
        assert_eq!(
            measure(&e, Constraints::loose(Size::new(400, 400)), &CELL),
            Size::new(GRIP_W, GRIP_W)
        );

        let l = layout(&e, Rect::new(0, 0, 400, 400), &CELL);
        let mut tree = Tree::new();
        tree.update(&e, &l).expect("a clean frame");
        let mut r = Router::new();
        let at = |pressed: bool| PointerEvent {
            kind: POINTER_BUTTON,
            button: 0x110,
            buttons: u16::from(pressed),
            flags: if pressed { POINTER_PRESSED } else { 0 },
            x: GRIP_W as i32 / 2,
            y: GRIP_W as i32 / 2,
            ..Default::default()
        };
        assert_eq!(r.pointer(&tree, &e, &l, at(true)).0, alloc::vec![Resize], "at the press");
        assert!(r.pointer(&tree, &e, &l, at(false)).0.is_empty(), "and not again at the click");
    }

    #[test]
    fn a_press_on_the_bars_face_is_a_drag_and_a_press_on_a_button_is_not() {
        // The discrimination the whole widget exists for. The buttons sit *above* the face in
        // the stack, so a press on one must produce its own message and not also a drag —
        // otherwise every click on close would move the window a little first.
        use crate::diff::Tree;
        use crate::route::Router;
        use librsproto::surface::{POINTER_BUTTON, POINTER_PRESSED, PointerEvent};

        #[derive(Clone, PartialEq, Debug)]
        enum M {
            Drag,
            Min,
            Max,
            Close,
        }
        let p = Theme::default();
        let e = title_bar(
            "a terminal",
            true,
            M::Drag,
            TitleButtons {
                minimise: Some(M::Min),
                maximise: Some(M::Max),
                close: Some(M::Close),
            },
            &p,
        );
        let l = layout(&e, Rect::new(0, 0, 400, 100), &CELL);
        let mut tree = Tree::new();
        tree.update(&e, &l).expect("a clean frame");

        let at = |x: i32, pressed: bool| PointerEvent {
            kind: POINTER_BUTTON,
            button: 0x110,
            buttons: u16::from(pressed),
            flags: if pressed { POINTER_PRESSED } else { 0 },
            x,
            y: 8,
            ..Default::default()
        };
        // The press and the release, kept apart: a drag is decided by the first and a click by
        // the second, and this widget carries one of each.
        let down = |r: &mut Router, x: i32| r.pointer(&tree, &e, &l, at(x, true)).0;
        let up = |r: &mut Router, x: i32| r.pointer(&tree, &e, &l, at(x, false)).0;

        let mut r = Router::new();
        assert_eq!(down(&mut r, 200), vec![M::Drag], "the bar moves the window on the press…");
        assert_eq!(up(&mut r, 200), vec![], "…and the release adds nothing");

        let mut r = Router::new();
        assert_eq!(
            down(&mut r, 390),
            vec![],
            "a press on close must not also drag: the window would move under the pointer while \
             the user is aiming at a button"
        );
        assert_eq!(up(&mut r, 390), vec![M::Close], "and the click is the close");

        let mut r = Router::new();
        assert_eq!(down(&mut r, 364), vec![], "the same for maximise");
        assert_eq!(up(&mut r, 364), vec![M::Max]);

        // **And a second button pressed mid-drag is not a second drag.** While a capture is held
        // the router routes to the *captured* widget, so every later press was reaching the bar
        // — including one over a button, where the shadowing rule cannot help because it walks
        // the captured node's path rather than the pointer's. A window jumped by the drag's
        // accumulated distance on each extra click (PR #248 review, blocking 1).
        let mut r = Router::new();
        assert_eq!(down(&mut r, 200), vec![M::Drag], "the left press starts the drag");
        let other = librsproto::surface::PointerEvent {
            kind: librsproto::surface::POINTER_BUTTON,
            button: 0x111,
            buttons: 3,
            flags: librsproto::surface::POINTER_PRESSED,
            x: 200,
            y: 8,
            ..Default::default()
        };
        assert_eq!(
            r.pointer(&tree, &e, &l, other).0,
            vec![],
            "a second button while the first is held must not start a second drag"
        );
        let over_close = librsproto::surface::PointerEvent { x: 390, ..other };
        assert_eq!(
            r.pointer(&tree, &e, &l, over_close).0,
            vec![],
            "nor one pressed over a button, where the capture is still the bar"
        );
    }

    const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSans.ttf");

    fn font() -> libdraw::text::Font {
        libdraw::text::Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses")
    }

    #[test]
    fn dialog_buttons_land_where_the_constants_say() {
        // **The four numbers `check-login` types**, asserted against a tree that is actually
        // built. They were `nxedit`'s until `nxfiles` grew a second confirmation; deriving them
        // beside the frame and checking them *here* is what stops two applications and one gate
        // drifting apart in three separate places.
        //
        // The literals matter as much as the derivation. Comparing derived constants against a
        // tree built from the same constants pins nothing — both sides move together, and the
        // gate's own table is linked to neither (PR #267 review, finding 2).
        assert_eq!((DIALOG_W, DIALOG_H), (340, 132));
        assert_eq!((DIALOG_LEFT_CX, DIALOG_RIGHT_CX, DIALOG_BUTTON_CY), (91, 249, 103));

        #[derive(Clone, PartialEq, Eq, Debug)]
        enum M {
            Left,
            Right,
            Drag,
        }
        let theme = Theme::default();
        let answer = |label: &str, msg: M, key: u64| {
            button(label, msg, WidgetState::default(), &theme).key(key).flex(1)
        };
        let ui: Element<M> = dialog_frame(
            title_bar(
                "Question",
                true,
                M::Drag,
                TitleButtons { minimise: None, maximise: None, close: None },
                &theme,
            )
            .key(1),
            padding(Insets::all(DIALOG_PAD), text("Really?")).key(2),
            crate::element::with_spacing(
                row(alloc::vec![answer("yes", M::Left, 3), answer("no", M::Right, 4)]),
                DIALOG_GAP,
            )
            .key(5),
            &theme,
        );

        // **It measures to exactly what it declares**, which is what lets it be a `Child`:
        // `Node::Dock` measures as everything it is offered, so without the fixed wrapper this
        // is a window a thousand screens wide and `Child::open` refuses it.
        let cell = crate::layout::FixedCell { w: 8, h: 16 };
        assert_eq!(
            crate::layout::measure(
                &ui,
                crate::layout::Constraints::loose(Size::new(u32::MAX / 4, u32::MAX / 4)),
                &cell,
            ),
            Size::new(DIALOG_W, DIALOG_H)
        );

        let l = crate::layout::layout(&ui, Rect::new(0, 0, DIALOG_W, DIALOG_H), &cell);
        let mut tree = crate::diff::Tree::new();
        tree.update(&ui, &l).expect("a dialog is diffable");
        let mut router = crate::route::Router::new();
        let click = |r: &mut crate::route::Router, x: i32, y: i32| {
            let at = |flags: u16, buttons: u16| PointerEvent {
                kind: librsproto::surface::POINTER_BUTTON,
                button: 0x110,
                buttons,
                flags,
                x,
                y,
                ..Default::default()
            };
            r.pointer(&tree, &ui, &l, at(librsproto::surface::POINTER_PRESSED, 1));
            r.pointer(&tree, &ui, &l, at(0, 0)).0
        };
        assert_eq!(click(&mut router, DIALOG_LEFT_CX, DIALOG_BUTTON_CY), alloc::vec![M::Left]);
        assert_eq!(click(&mut router, DIALOG_RIGHT_CX, DIALOG_BUTTON_CY), alloc::vec![M::Right]);

        // **And the aim point is the button's *centre*, not merely a point inside it.** Padding
        // the strip on four sides instead of three halves the buttons, and a centre stays inside
        // a box that shrank around it — so the row's own height is bracketed here.
        let half = DIALOG_BUTTON_H as i32 / 2;
        for edge in [DIALOG_BUTTON_CY - half + 1, DIALOG_BUTTON_CY + half - 1] {
            assert_eq!(
                click(&mut router, DIALOG_LEFT_CX, edge),
                alloc::vec![M::Left],
                "the button does not reach {edge}, so {DIALOG_BUTTON_CY} is not its centre"
            );
        }
        assert!(
            click(&mut router, DIALOG_LEFT_CX, DIALOG_BUTTON_CY - DIALOG_BUTTON_H as i32)
                .is_empty(),
            "a whole button above the aim point is not the button"
        );
    }
}
