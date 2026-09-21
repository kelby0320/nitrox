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
use libdraw::format::Rgb;
use libdraw::geom::Size;
// **Re-exported, because every widget here takes one.** A caller importing `button` and
// `list_view` from this module should not have to reach into `libdraw` for the third argument
// they all share; `libui::paint::Theme` names the same type for the painting half.
pub use libdraw::theme::Theme;

use librsproto::surface::{POINTER_BUTTON, POINTER_PRESSED, PointerEvent};

use crate::element::{
    Edge, Element, IconKind, Insets, TextSize, bevel, bold, center, center_v, column, dock,
    docked, fill, icon, ink, outline, padding, rounded_fill, row, scaled, sized, stack, text, wash,
    with_spacing,
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
    // Bottom to top: the ring or the border, then the face, then the label. A `Stack` gives
    // every layer the whole area, so the outline is only visible because the face above it is
    // inset — and the label sits above both.
    //
    // **An edge, always** (M15). A face at `#EDECEB` on a window at `#FFFFFF` is a difference of
    // eighteen units per channel: technically not the ground, and in a real window indisputably
    // invisible — the report was that buttons need "a different color background … so you know
    // it's a button". What actually says *button* is the edge, which is what every desktop
    // draws and what this toolkit had only around a focused control.
    //
    // **Rounded since the desktop refresh's Part H**, to `CONTROL_RADIUS` — the curve a window,
    // a popup, a tab and a field all share. A square button beside a rounded field was the last
    // right angle on these surfaces.
    let (edge, thickness) =
        if state.active { (theme.accent, RING) } else { (theme.border, BORDER) };
    let mut layers = alloc::vec::Vec::with_capacity(3);
    layers.push(rounded_fill(edge, CONTROL_RADIUS));
    layers.push(padding(
        Insets::all(thickness),
        rounded_fill(face, CONTROL_RADIUS.saturating_sub(thickness)),
    ));
    // **Centred, which is what a button's label is everywhere else in the world.** It was against
    // the top-left corner of the face: `padding` places a child at an inset from the origin, and
    // a face is usually much wider than the word on it, so every button in this toolkit read as
    // a label with a box drawn round it. `center` is the node that was missing.
    layers.push(center(padding(BUTTON_PAD, text(label))));
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
pub fn window_frame<Msg>(
    title: Element<Msg>,
    content: Element<Msg>,
    focused: bool,
    theme: &Theme,
) -> Element<Msg> {
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
        padding(
            Insets { top: 0, right: WINDOW_FRAME, bottom: WINDOW_FRAME, left: WINDOW_FRAME },
            content,
        ),
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

/// How tall a tab strip is: the design's 30 since the refresh's Part H, where it was 24.
///
/// **A fixed metric, like every other piece of chrome here** (M11's decision 2): the gates aim at
/// tabs, and one whose height followed the theme's text size would move under a gate that had to
/// read a theme file to know where to click.
pub const TAB_STRIP_H: u32 = 30;

/// How tall one tab is, inside that strip: the design's 24.
pub const TAB_H: u32 = 24;

/// The strip's padding above its tabs — the design's 5, which is what leaves a tab sitting on the
/// strip rather than filling it.
pub const TAB_TOP: u32 = 5;

/// The strip's padding at its left end, before the first tab: the design's 6.
pub const TAB_SIDE: u32 = 6;

/// Between one tab and the next: the design's 1. Enough to separate two inactive tabs, which
/// have no face of their own to separate them.
pub const TAB_GAP: u32 = 1;

/// From one tab's left edge to the next's — what a gate multiplies to find tab `n`.
pub const TAB_PITCH: u32 = TAB_W + TAB_GAP;

/// The corner a tab is rounded to, at the top only: a tab is a shape that runs into the content
/// below it, so the bottom corners are square and the bottom edge is not there at all.
pub const TAB_RADIUS: u32 = CONTROL_RADIUS;

/// The `+` at the end of the strip: the design's 22×24.
pub const NEW_TAB_W: u32 = 22;

/// How wide one tab is.
///
/// **Fixed rather than shared out**, which is the decision worth stating. Tabs that divided the
/// strip between them would move every one of them whenever another opened — so the tab a person
/// is reaching for slides away as they reach, and a gate's aim point depends on how many tabs
/// happen to be open. Fixed width means a tab is where it was, and the cost is that enough of
/// them run off the end: `TODO(tab-overflow)` names the scrolling that would fix it, and the
/// trigger is somebody opening more than a window's width of them.
pub const TAB_W: u32 = 120;

/// A tab's box inside the strip: the tab itself, plus the rule it covers when it is the current
/// one.
const TAB_BOX_H: u32 = TAB_H + TAB_RULE;

/// The rule along the bottom of the strip.
const TAB_RULE: u32 = 1;

/// A tab label's inset from the tab's left edge: the design's 9.
const TAB_LABEL_PAD: u32 = 9;

/// Between a tab's label and its close box: the design's 8.
const TAB_CLOSE_GAP: u32 = 8;

/// The key the `+` carries, so the strip's children are all keyed as the diff requires.
const NEW_TAB_KEY: u64 = u64::MAX;

/// How wide the close box at a tab's right end is.
pub const TAB_CLOSE_W: u32 = 20;

/// The centre of a tab's close box, measured from that tab's left edge.
///
/// Published for the same reason [`DIALOG_LEFT_CX`] is: `check-login` presses it from a copy of
/// its own, because a gate aims from its own numbers (M11 decision 2).
pub const TAB_CLOSE_CX: i32 = (TAB_W - TAB_CLOSE_W / 2) as i32;

/// One tab: what it is called, whether it is marked, and what identifies it across frames.
pub struct Tab<'a> {
    /// Identity across frames — see [`ListRow::key`], whose reasoning is the same one.
    ///
    /// **Not the index**, because closing a tab renumbers every one after it and the diff would
    /// pair each surviving tab's widget with its neighbour's element.
    pub key: u64,
    /// What it is called.
    pub label: &'a str,
    /// Shown with a leading mark — an editor's unsaved buffer.
    pub marked: bool,
}

/// A row of tabs, one of them current.
///
/// **`select` and `close` both take the key**, not the index, for the reason the key exists: the
/// message outlives the frame that produced it, and by the time an application acts on it the
/// tab it names may have moved.
///
/// A press on the close box does *not* select the tab, because a nearer `on_press` shadows the
/// one on the tab — the same rule that lets a title bar carry buttons without dragging the
/// window. That rule is the toolkit's rather than this widget's.
pub fn tab_strip<Msg: Clone>(
    tabs: &[Tab<'_>],
    current: u64,
    hovered: Option<u64>,
    select: impl Fn(u64) -> Msg,
    close: impl Fn(u64) -> Msg,
    extras: TabExtras<Msg>,
    theme: &Theme,
) -> Element<Msg> {
    let mut row_items = alloc::vec::Vec::with_capacity(tabs.len() + 1);
    for t in tabs {
        let current = t.key == current;
        let mut label = String::new();
        if t.marked {
            label.push_str("* ");
        }
        label.push_str(t.label);
        // **The current tab is the window's own ground and the others are the strip's**, which is
        // what makes a row of boxes read as tabs rather than as buttons: the current one is a
        // continuation of the content below, and the rest are a strip above it. An inactive tab
        // has no face at all — the strip's is what shows through — so hover is a wash over it,
        // the treatment every other momentary highlight in this toolkit gets.
        let mut layers = alloc::vec::Vec::with_capacity(3);
        if current {
            layers.extend(tab_face(theme.background));
        } else if hovered == Some(t.key) {
            // **A face rather than a wash**, which every other momentary highlight here is: a
            // wash cannot be rounded, and a square highlight behind a rounded tab reads as a
            // second shape. One shade off the strip is enough to say "this one".
            //
            // **Stopping above the rule**, which the current tab covers and no other tab may:
            // running the hover face down to the strip's bottom made an inactive tab read as
            // open at the bottom for as long as the pointer was over it — the one cue that is
            // supposed to mark the current tab alone (PR #319 review, optional 7).
            layers.push(padding(
                Insets { top: 0, right: 0, bottom: TAB_RULE, left: 0 },
                stack(tab_face(theme.face)),
            ));
        }
        let ink_colour = if current { theme.foreground } else { theme.foreground_dim };
        layers.push(row(alloc::vec![
            padding(
                Insets { top: 0, right: TAB_CLOSE_GAP, bottom: 0, left: TAB_LABEL_PAD },
                center_v(ink(ink_colour, text(label))),
            )
            .flex(1),
            // **The icon fills the box rather than being centred in it**: `Node::Icon` measures
            // as nothing and draws into the rect it is given, so a `center` around one hands it
            // a zero rect and the tab loses its close box while still clicking like one.
            sized(
                Size::new(TAB_CLOSE_W, TAB_BOX_H),
                stack(alloc::vec![ink(theme.foreground_dim, icon(IconKind::Close))])
                    .on_press(close(t.key)),
            ),
        ]));
        row_items.push(
            sized(Size::new(TAB_W, TAB_BOX_H), stack(layers).on_press(select(t.key)))
                .key(t.key),
        );
    }
    // **The `+` is the last thing in the row**, where the design puts it: after the tabs rather
    // than at the strip's right end, so it stays beside the tab that was opened last.
    if let Some(new_tab) = extras.new_tab {
        row_items.push(
            sized(
                Size::new(NEW_TAB_W, TAB_BOX_H),
                stack(alloc::vec![center(ink(theme.foreground_dim, text("+")))])
                    .on_press(new_tab),
            )
            .key(NEW_TAB_KEY),
        );
    }
    let mut strip = alloc::vec::Vec::with_capacity(3);
    // The strip's own face, and the rule that separates it from what is below. The current tab
    // is drawn over that rule, which is what "its border is open at the bottom" means here.
    strip.push(fill(theme.face_hover));
    strip.push(dock(
        alloc::vec![docked(Edge::Bottom, sized(Size::new(0, TAB_RULE), fill(theme.border)))],
        text(""),
    ));
    let tabs_row = with_spacing(row(row_items), TAB_GAP);
    strip.push(padding(
        Insets { top: TAB_TOP, right: TAB_SIDE, bottom: 0, left: TAB_SIDE },
        match extras.right {
            // A right-hand slot for a window's own controls — the editor's byte count and its
            // Save. `dock` gives the tabs the rest, so the slot is the size it asks for.
            //
            // **Both sides wrapped**, as `status_bar` wraps its slots and for the same reason:
            // the tabs beside it are keyed, so a caller that keys its own control — and Part J's
            // Save must, for its hover — made this dock `MixedKeying` and the window then drew
            // nothing at all (PR #319 review, worth fixing 6).
            Some(right) => dock(
                alloc::vec![docked(Edge::Right, padding(Insets::all(0), right))],
                padding(Insets::all(0), tabs_row),
            ),
            None => tabs_row,
        },
    ));
    sized(Size::new(0, TAB_STRIP_H), stack(strip))
}

/// What a tab strip carries besides its tabs (desktop refresh, Part H).
///
/// **A struct rather than two more arguments**, because both are optional and an application
/// that wants neither says so once: `TabExtras::none()`.
pub struct TabExtras<Msg> {
    /// What the `+` at the end of the tabs sends. `None` draws no `+`.
    pub new_tab: Option<Msg>,
    /// A window's own controls, at the strip's right end — the editor's byte count and Save.
    pub right: Option<Element<Msg>>,
}

impl<Msg> TabExtras<Msg> {
    /// Tabs and nothing else.
    pub fn none() -> Self {
        Self { new_tab: None, right: None }
    }

    /// Tabs and a `+`.
    pub fn new_tab(msg: Msg) -> Self {
        Self { new_tab: Some(msg), right: None }
    }
}

/// A tab's face: rounded at the top, square where it meets the content.
///
/// **Two layers rather than a per-corner radius**, which the toolkit does not have: a rounded
/// fill over the whole box, then a square one below the curve. The square layer is what covers
/// the strip's rule under the current tab.
fn tab_face<Msg>(colour: Rgb) -> alloc::vec::Vec<Element<Msg>> {
    alloc::vec![
        rounded_fill(colour, TAB_RADIUS),
        padding(Insets { top: TAB_RADIUS, right: 0, bottom: 0, left: 0 }, fill(colour)),
    ]
}

/// How tall a status bar is: the design's 25.
pub const STATUS_BAR_H: u32 = 25;

/// A status bar's inset at either end: the design's 11.
const STATUS_PAD_X: u32 = 11;

/// A window's status bar: what it is doing at one end, where it is at the other.
///
/// **The rule is on the edge that faces the content** — `Edge::Top` for a bar along the bottom of
/// a window, which is where the design puts one and where the editor's is going (Part J), and
/// `Edge::Bottom` for one that sits under the chrome, which is where the editor's is today. A bar
/// with its rule on the wrong side reads as a lid rather than a floor.
///
/// **Not in the mono face, which the design uses.** A window is painted with one face, and the
/// fixed-advance one belongs to a character grid; a status bar set in it would be the toolkit's
/// first two-face surface for the sake of a byte count. It is [`TextSize::Small`] and
/// `foreground_dim` instead — the design's hierarchy by size and ink, which is what carries the
/// "read this second" (desktop refresh, Part H).
pub fn status_bar<Msg>(
    left: Element<Msg>,
    right: Option<Element<Msg>>,
    rule: Edge,
    theme: &Theme,
) -> Element<Msg> {
    // **Both slots wrapped, and the zero-inset one is not decoration** — `frame_layers`'s rule,
    // for its reason: the diff wants a container's children all keyed or none, and a caller that
    // keys its readout would otherwise make this dock `MixedKeying` at the first frame. The
    // wrapper puts the caller's keys one level down, where they still do their job.
    let bare = |e: Element<Msg>| padding(Insets::all(0), e);
    let inner = match right {
        Some(right) => dock(alloc::vec![docked(Edge::Right, bare(right))], bare(left)),
        None => left,
    };
    sized(
        Size::new(0, STATUS_BAR_H),
        stack(alloc::vec![
            fill(theme.face_hover),
            dock(
                alloc::vec![docked(rule, sized(Size::new(0, 1), fill(theme.border)))],
                text(""),
            ),
            padding(
                Insets { top: 0, right: STATUS_PAD_X, bottom: 0, left: STATUS_PAD_X },
                center_v(inner),
            ),
        ]),
    )
}

/// One reading on a status bar: a step below the body, in the ink a second read gets.
pub fn status_text<Msg>(s: impl Into<String>, theme: &Theme) -> Element<Msg> {
    ink(theme.foreground_dim, scaled(TextSize::Small, text(s)))
}

/// The upright between two readings on a status bar — the design's `|`, in the line colour.
pub fn status_separator<Msg>(theme: &Theme) -> Element<Msg> {
    padding(
        Insets { top: 0, right: STATUS_GAP, bottom: 0, left: STATUS_GAP },
        ink(theme.border, scaled(TextSize::Small, text("|"))),
    )
}

/// Between two readings on a status bar — either side of a separator, or between a control and
/// the reading next to it. The design's 16 from one to the next, less the bar it draws between.
pub const STATUS_GAP: u32 = 7;

/// A primary action: the accent as a ground, with its label in the paper colour.
///
/// **The one button in a window that is the answer** — the design's `Save`, and the only place it
/// fills a control with the accent rather than drawing with it. Everything else on a surface is a
/// face with an edge, which is what makes this one read as the action rather than as an option
/// (desktop refresh, Part H).
///
/// Hover and press shade the ground rather than swapping it, because an accent that changed hue
/// under the pointer would read as a different control.
pub fn pill<Msg>(
    label: impl Into<String>,
    msg: Msg,
    state: WidgetState,
    theme: &Theme,
) -> Element<Msg> {
    let ground = if state.pressed {
        theme.accent.shade(-PILL_SHADE)
    } else if state.hovered {
        theme.accent.shade(PILL_SHADE)
    } else {
        theme.accent
    };
    // **The label is whichever of the surface's two inks stands furthest from the ground** — the
    // window's paper in the light scheme, which is the design's white on the accent, and its
    // text colour in the dark one, where the paper is near-black and would sit at 3.8:1 on the
    // same accent (PR #319 review, optional 9). One rule, so a scheme cannot be picked that
    // makes this control unreadable.
    let ink_colour = furthest_from(ground, theme.background, theme.foreground);
    let mut layers = alloc::vec::Vec::with_capacity(4);
    layers.push(rounded_fill(ground, CONTROL_RADIUS));
    if state.active {
        // **A band of the label's colour, inside the edge** — and it has to be a *band*: this
        // drew the ground inset by the ring over a ground of the same colour, which is the same
        // colour on the same colour and put down not one different pixel (review, optional 4).
        layers.push(padding(
            Insets::all(PILL_RING_INSET),
            rounded_fill(ink_colour, CONTROL_RADIUS - PILL_RING_INSET),
        ));
        layers.push(padding(
            Insets::all(PILL_RING_INSET + RING),
            rounded_fill(ground, CONTROL_RADIUS - PILL_RING_INSET - RING),
        ));
    }
    layers.push(center(padding(PILL_PAD, ink(ink_colour, text(label)))));
    stack(layers).on_press(msg).focusable()
}

/// How tall a pill is where a caller sizes one: the design's 21, which is a control sitting on a
/// 24-pixel row rather than filling it.
pub const PILL_H: u32 = 21;

/// How far inside a pill's edge its focus ring is drawn.
const PILL_RING_INSET: u32 = 1;

/// Whichever of `a` and `b` stands further from `ground`, by weighted brightness.
///
/// **Weighted rather than a plain sum**, because green carries most of what an eye reads as
/// light: `0.299r + 0.587g + 0.114b`, the usual integer form, which needs no `powf` — `libdraw`
/// builds for a bare target and has no floating-point maths beyond arithmetic.
///
/// **It picks, and does not promise.** On the accent this system ships, it chooses white in the
/// light scheme (4.6:1) and the near-white ink in the dark one (3.9:1, where the window's own
/// paper would be 3.8:1). The dark figure is under WCAG's 4.5 and cannot be fixed here: `accent`
/// is one colour in both schemes, so a readable pill in the dark scheme is a question about the
/// palette, for whoever revisits it (PR #319 review, optional 9).
fn furthest_from(ground: Rgb, a: Rgb, b: Rgb) -> Rgb {
    let brightness = |c: Rgb| 299 * c.r as i32 + 587 * c.g as i32 + 114 * c.b as i32;
    let (g, a_, b_) = (brightness(ground), brightness(a), brightness(b));
    if (a_ - g).abs() >= (b_ - g).abs() { a } else { b }
}

/// A pill's sides: the design's 12.
const PILL_PAD: Insets = Insets { top: 2, right: 12, bottom: 2, left: 12 };

/// How far a pill's ground moves under the pointer, and the other way when pressed.
const PILL_SHADE: i16 = 12;

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

/// The radius every small control on a surface is rounded to: the design's `--r`, 8 — the same
/// curve a window and a popup are cut to.
///
/// **Public, so the shell's bar shares it** rather than keeping a second 8 of its own. A field in
/// a window and a button on the bar being the same shape is the thing the design is consistent
/// about (desktop refresh, Part D).
pub const CONTROL_RADIUS: u32 = 8;

/// How wide the focus ring is, in pixels.
const RING: u32 = 2;

/// How wide a resting control's edge is, in pixels.
///
/// **One, because it is a line rather than a ring.** The focus ring is two so that it reads as
/// a state; an edge is what makes a shape a control at rest, and a second pixel of it would
/// compete with the ring instead of sitting under it.
const BORDER: u32 = 1;

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
    /// **The thumb's centre rather than its top**, which is what "jump to here" means for a
    /// press on the *track*. It is the wrong answer for a press on the **thumb** — that
    /// re-centres a thumb the person grabbed by its end, so it jumps before the drag begins —
    /// and [`ScrollGrab`] is what tells the two presses apart. This is the track half.
    ///
    /// `y` is signed because a drag routinely leaves the widget: the router hands a captured
    /// widget negative coordinates rather than clamping, and this clamps at the ends instead —
    /// which is what makes dragging past the bottom stay at the bottom.
    pub fn offset_at(&self, track: u32, y: i32) -> u32 {
        let (_, len) = self.thumb(track);
        self.offset_for_thumb_top(track, y - len as i32 / 2)
    }

    /// The offset that puts the thumb's **top** at `top` pixels down a track of `track`.
    ///
    /// [`thumb`](Self::thumb)'s inverse, and the half M4 did not ship: the toolkit could say
    /// where a thumb goes for a given offset but not what offset a grab means, so a scrollbar
    /// was a picture of a scrollbar. The terminal is the first thing to want to *use* one,
    /// which is the milestone rule working as intended — see the decision log, 2026-08-12.
    ///
    /// **The top rather than the centre**, because a drag that keeps the thumb under the cursor
    /// has to keep it under the *same part* of the cursor it was taken by — see [`ScrollGrab`].
    /// It took until M14 Part I to say so: `offset_at` centred, so every grab moved the thumb
    /// before the drag began.
    pub fn offset_for_thumb_top(&self, track: u32, top: i32) -> u32 {
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
        let pos = top.clamp(0, span as i32) as u32;
        // **Truncating, like [`thumb`](Self::thumb)**, which is what keeps the pair consistent:
        // the round trip is exact wherever the division is, and neither end needs help — at the
        // bottom `pos` is `span`, so `span * max / span` is `max` however it rounds. A first
        // version added `span / 2` here for the stated reason that the last line was otherwise
        // unreachable, which is simply not true; deleting it left every test green, so it was
        // half a line of mid-drag accuracy bought with a claim that did not hold.
        ((pos as u64 * max_offset as u64) / span as u64) as u32
    }
}

/// Where within the thumb a scrollbar drag took hold.
///
/// **The interaction state a scrollbar needs and a widget cannot keep.** `widget-toolkit.md` §3
/// reserves retained state for things the application has no opinion about, and this qualifies:
/// nobody has an opinion about where inside a thumb a button landed. It lives in a small value
/// the application holds — the same shape as [`click::Clicks`](crate::click::Clicks), and for
/// the same reason: the toolkit's widgets are rebuilt every frame and have nowhere to put it.
///
/// **What it buys.** Without it the only inverse available is "put the thumb's centre under the
/// cursor", so grabbing a thumb near either end makes it jump by up to half its length before
/// the drag begins — the defect the `scroll-grab` deferral recorded from M4 until M14 Part I.
/// With it,
/// a press on the thumb moves nothing and the thumb then follows the pointer exactly.
///
/// A press on the **track** still jumps, because that is what a track click means; the thumb
/// centres on the cursor and is held there for the rest of that drag.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ScrollGrab {
    /// Pixels between the thumb's top and where the press landed, while a drag is running.
    within: Option<i32>,
    /// Which button took the grab — the only one whose events this drag answers.
    ///
    /// **The router hands a captured widget every button's events**, and drops its own capture
    /// only when *all* of them come up, so a second button tapped mid-drag arrives here as a
    /// press and a release belonging to a gesture this grab is not part of. The compositor keeps
    /// `grab_button` for the identical reason one layer down (PR #248 review, blocking 1).
    button: u16,
}

impl ScrollGrab {
    /// A tracker holding nothing.
    pub const fn new() -> Self {
        Self { within: None, button: 0 }
    }

    /// Whether a drag is in progress.
    pub fn dragging(&self) -> bool {
        self.within.is_some()
    }

    /// Apply one pointer event over the bar; answer where the bar should now be.
    ///
    /// `None` means this event is not part of a drag — a hover, a crossing, or the release that
    /// ends one — and the caller should leave its offset alone.
    ///
    /// `state` is the bar as it is *now* and `track` the height it was drawn at; both are the
    /// numbers the caller passed to [`scrollbar`], and a drag converted against a different
    /// geometry from the one drawn puts the thumb where the pointer is not.
    pub fn apply(&mut self, state: ScrollState, track: u32, ev: PointerEvent) -> Option<u32> {
        let is_button = ev.kind == POINTER_BUTTON;
        // **Only the button that took the grab is answered.** Pressing a second one mid-drag
        // would otherwise re-take the grab from wherever the cursor happens to be — a jump, if
        // that is off the thumb — and its *release* would end a gesture the person is still
        // making, leaving the bar stranded while the router goes on delivering to it. The old
        // `if p.buttons != 0` test in each application kept following (PR #288 review, 1).
        if is_button && self.within.is_some() && ev.button != self.button {
            return None;
        }
        if is_button && ev.flags & POINTER_PRESSED == 0 {
            // The release, which the router delivers to the captured widget wherever the
            // cursor has got to. Nothing moves; the grab ends.
            self.within = None;
            return None;
        }
        if is_button {
            let (pos, len) = state.thumb(track);
            let (top, bottom) = (pos as i32, pos as i32 + len as i32);
            if ev.y >= top && ev.y < bottom {
                // **On the thumb: nothing moves.** Remembering where within it the press
                // landed is the whole of this type, and returning the offset unchanged —
                // rather than one recomputed from the thumb's position — keeps a grab exact
                // where the position arithmetic truncates.
                self.within = Some(ev.y - top);
                self.button = ev.button;
                return Some(state.offset);
            }
            // On the track: jump, then hold the thumb centred for the rest of the drag.
            self.within = Some(len as i32 / 2);
            self.button = ev.button;
            return Some(state.offset_at(track, ev.y));
        }
        let within = self.within?;
        if ev.buttons == 0 {
            // A motion with nothing held after a grab we never saw released — the router
            // suppresses crossings mid-capture, so this is a lost release rather than a hover.
            // Ending the drag beats following a pointer nobody is pressing.
            self.within = None;
            return None;
        }
        Some(state.offset_for_thumb_top(track, ev.y - within))
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
            fill(theme.groove),
            column(alloc::vec![
                sized(Size::new(0, pos), fill(theme.groove)),
                sized(Size::new(0, len), bevel(theme.thumb)),
                // The remainder, so the thumb does not stretch to the bottom.
                fill(theme.groove).flex(1),
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
    theme: &Theme,
) -> Element<Msg> {
    let face = if focused { theme.title_active } else { theme.title_inactive };
    // **A glyph, not a letter** (M11 Part E, batch 2). These were `_`, `[]` and `X` — three
    // characters standing in for three controls, which read as text on a bar full of text.
    //
    // **In the dim ink, on no face of their own** — the design's controls are borderless and
    // `--fgdim`, and read as part of the bar rather than as three buttons on it. Each takes its
    // whole column of the bar as a target and draws its glyph in the middle of the design's 23x21.
    let btn = |glyph: IconKind, msg: Msg| {
        sized(
            Size::new(TITLE_BUTTON_W, 0),
            center(sized(Size::new(TITLE_BUTTON_W, TITLE_BUTTON_H), stack(alloc::vec![icon(glyph)]))),
        )
        .on_press(msg)
    };
    // **A button a caller has no message for is not drawn.** The alternative is a button that
    // does nothing, and a control that looks live and is not is the defect this milestone's
    // predecessor shipped three of (M8's overview). The buttons arrive with the parts that give
    // them somewhere to go: minimise and maximise in Part B, close in Part C.
    let mut glyphs = alloc::vec::Vec::with_capacity(3);
    if let Some(m) = buttons.minimise {
        glyphs.push(btn(IconKind::Minimise, m));
    }
    if let Some(m) = buttons.maximise {
        glyphs.push(btn(IconKind::Maximise, m));
    }
    if let Some(m) = buttons.close {
        glyphs.push(btn(IconKind::Close, m));
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
    controls.push(ink(
        theme.foreground_dim,
        crate::element::with_spacing(row(glyphs), TITLE_BUTTON_GAP),
    ));
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

/// Space between a title bar's text and its edge: the design's 11 on the left, and the text
/// centred down the bar.
const TITLE_PAD: Insets = Insets { top: 0, right: 6, bottom: 0, left: 11 };

/// Between a window's title and the dim subtitle beside it: the design's 9.
const TITLE_SUBTITLE_GAP: u32 = 9;

/// Space between a text field's content and its edge.
const FIELD_PAD: Insets = Insets { top: 4, right: 8, bottom: 4, left: 8 };

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
        content.push(sized(Size::new(CARET, 0), fill(theme.accent)));
    }
    content.push(text(render(after)));

    // `track` is the recessed-channel colour the scrollbar uses, and a text field is the same
    // idea: a well the content sits in, rather than a face that stands out of the surface.
    //
    // **Rounded, and edged at rest since the desktop refresh's Part D.** It was a flat fill of
    // `track` with no edge until focused — and `track` is `--bg` in the light scheme, so a
    // resting field on a window's own ground was *invisible*: the greeter's password box was a
    // white rectangle on a white card. The design draws every field as a ground inside a
    // one-pixel `--line`, which is what makes it read as somewhere to type before it is typed in.
    // The ring keeps its second pixel and its accent, so focus still reads as a state rather than
    // as an edge.
    let (edge, thickness) =
        if state.active { (theme.accent, RING) } else { (theme.border, BORDER) };
    let mut layers = alloc::vec::Vec::with_capacity(3);
    layers.push(rounded_fill(edge, CONTROL_RADIUS));
    layers.push(padding(
        Insets::all(thickness),
        rounded_fill(theme.track, CONTROL_RADIUS.saturating_sub(thickness)),
    ));
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
    /// What the last [`ensure_visible`](Self::ensure_visible) was for: the cursor's line and
    /// column, the text's revision, and how many lines fitted.
    ///
    /// **Everything that could have put the caret out of view, and nothing else.** A keystroke
    /// moves the caret, an edit changes the revision, and a window that got shorter changes the
    /// count — any of those means the view should follow again. A *repaint* changes none of
    /// them, which is the whole point: following on every build is what made a scrollbar
    /// impossible (M15 Part D).
    ///
    /// **Derived rather than a flag somebody sets.** A "please scroll to the caret" boolean has
    /// to be set by every mutator, and a missed one is silent — which is exactly the bug this
    /// field's first version had: it keyed on the *line* alone, so typing (which does not change
    /// the line) left the person typing into a document they could not see (PR #290 review, 1).
    followed: Option<(usize, usize, u64, usize)>,
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
    /// States to go back to, oldest first.
    undo: Vec<Snapshot>,
    /// States to come forward to, cleared by any new edit.
    redo: Vec<Snapshot>,
    /// What the edits since the last snapshot were, or `None` when the next one starts a group.
    group: Option<EditKind>,
}

/// What kind of edit is in progress, so that adjacent ones of a kind coalesce.
///
/// **The grouping is the decision, not the stack** (M12 Part C). Per keystroke is unusable —
/// undoing a sentence becomes forty presses — and per save is useless, because the thing a person
/// wants back is usually the last word they typed. What they expect is a word or a line, so a run
/// of printable characters is one group, a separator ends it, `Enter` ends it, a run of deletions
/// is a group of its own, and **any movement ends whatever was open**: the cursor moving means
/// what comes next is a different edit, wherever it lands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EditKind {
    /// Characters going in.
    Typing,
    /// Characters coming out.
    Deleting,
    /// A whole insertion arriving at once — a paste.
    ///
    /// **Its own kind so it is its own undo step.** Grouped with `Typing` a paste would merge
    /// into whatever word was being typed before it, and one undo would take back both. A
    /// person who pastes and then undoes means "not that", and the text they had typed is not
    /// part of "that".
    Pasting,
}

/// A buffer and a cursor, as they were before a group of edits.
///
/// **A whole copy, not a delta**, and the trade is worth stating. A delta stack is what a large
/// editor keeps: it costs the size of the change rather than the size of the file, and it costs a
/// separate inverse for every kind of edit — an insert, a join, a split, and a replace that is
/// two of those at once. Each is a way to be subtly wrong, and none of them is checkable by
/// reading. A copy cannot be wrong about what it restores; what it costs is memory, bounded here
/// by [`MAX_UNDO`] groups. **Trigger for deltas: a file where that bound bites** —
/// `TODO(undo-deltas)`.
///
/// The selection is deliberately not kept. A selection is a gesture in progress rather than part
/// of the text, and restoring one would make undo re-select something the person has since moved
/// away from.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Snapshot {
    lines: Vec<String>,
    line: usize,
    col: usize,
}

/// How many groups of edits a buffer can go back through.
///
/// Each is a copy of the whole buffer (see [`Snapshot`]), so this is the memory bound as well as
/// the depth one. Sixty-four is far more than a person reaches for between saves and small enough
/// that even a large file's history stays a fraction of what the editor already holds to draw it.
pub const MAX_UNDO: usize = 64;

impl Default for TextAreaState {
    fn default() -> Self {
        Self {
            lines: alloc::vec![String::new()],
            line: 0,
            col: 0,
            anchor: None,
            goal: None,
            offset: 0,
            followed: None,
            revision: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            group: None,
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

    /// Whether anything is selected.
    ///
    /// **The cheap half of [`selected_text`](Self::selected_text)**, which allocates the whole
    /// selection as a `String` — and which callers used to ask this question with (PR #280 review,
    /// worth fixing 6). Here `selection` is already `None` for a collapsed range, so this is a
    /// straight delegation; `libterm::Grid::has_selection` is the one that has to say more.
    pub fn has_selection(&self) -> bool {
        self.selection().is_some()
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
        // A click is a movement, and ends a group for the same reason an arrow key does.
        self.group = None;
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
        self.begin(EditKind::Typing);
        self.delete_selection();
        self.lines[self.line].insert(self.col, c);
        self.col += c.len_utf8();
        self.goal = None;
        self.revision += 1;
        // **A separator ends the group it is part of**, so a word and the space after it undo
        // together and the next word is a step of its own. Ending the group *before* the space
        // would make every space a group of one, and undo would hand the space back alone.
        if !c.is_alphanumeric() {
            self.group = None;
        }
    }

    /// Insert `text` at the cursor, replacing any selection, as **one** undo step.
    ///
    /// The paste primitive (M12 Part E). `insert` per character would be usable and wrong in two
    /// ways: a newline is not a character it accepts, and the undo grouping would break the
    /// pasted text into words.
    ///
    /// Returns the range it occupies afterwards, `(line, col)` to `(line, col)` — which is what
    /// makes **cycling** possible at all: M12 decision 3 says a cycle *replaces what was just
    /// inserted*, so the caller has to be told where that is. Deriving it from the cursor and
    /// the text's shape at the call site is the same arithmetic done in a place with less to
    /// check it against.
    pub fn insert_text(&mut self, text: &str) -> ((usize, usize), (usize, usize)) {
        self.begin(EditKind::Pasting);
        self.delete_selection();
        let from = (self.line, self.col);
        // The tail of the current line moves to the end of what is being inserted, exactly as
        // `newline` moves it — a paste ending mid-line must not swallow what was after it.
        let tail = self.lines[self.line].split_off(self.col);
        let mut parts = text.split('\n');
        // `split` always yields at least one piece, so this cannot be `None`.
        let first = parts.next().unwrap_or("");
        self.lines[self.line].push_str(first);
        self.col += first.len();
        for part in parts {
            self.line += 1;
            self.lines.insert(self.line, String::from(part));
            self.col = part.len();
        }
        let to = (self.line, self.col);
        self.lines[self.line].push_str(&tail);
        self.goal = None;
        self.revision += 1;
        // **A paste is a complete step**, so the next keystroke starts a new group rather than
        // being undone together with it.
        self.group = None;
        (from, to)
    }

    /// The (line, column) a pointer at widget-local `(x, y)` is over.
    ///
    /// **Given a way to measure text rather than a font**, for [`Metrics`](crate::layout::Metrics)'
    /// reason one layer down: this crate has no glyphs, and the application that draws the area
    /// already holds the metrics it was laid out with. Pass `|s| metrics.text_size(s).w`.
    ///
    /// **The column is the *nearest boundary*, not the character under the cursor.** Clicking the
    /// right half of a letter puts the caret after it, which is what every editor does and what
    /// makes clicking at the end of a word land after the word rather than inside it.
    ///
    /// Coordinates are the ones [`text_area`] hands its pointer handler, so this subtracts the
    /// padding the widget draws with — a caller cannot know that number and should not have to.
    pub fn at_point(&self, x: i32, y: i32, row_height: u32, width: impl Fn(&str) -> u32)
        -> (usize, usize)
    {
        let y = y - FIELD_PAD.top as i32;
        let row = if row_height == 0 { 0 } else { (y.max(0) as u32 / row_height) as usize };
        // **Clamped rather than refused.** A press below the last line means its end, which is
        // what dragging off the bottom of a selection has to mean.
        let line = (self.offset + row).min(self.lines.len().saturating_sub(1));
        let text = &self.lines[line];
        let x = x - FIELD_PAD.left as i32;
        if x <= 0 {
            return (line, 0);
        }
        // Walk the boundaries, keeping the one whose drawn width is nearest the cursor. Linear
        // in the line's length and measured once per boundary, which is what a proportional font
        // costs: there is no arithmetic that turns a pixel into a column when every glyph is a
        // different width.
        let mut best = (0usize, u32::MAX);
        for col in text
            .char_indices()
            .map(|(i, _)| i)
            .chain(core::iter::once(text.len()))
        {
            let w = width(&text[..col]);
            let d = w.abs_diff(x as u32);
            if d < best.1 {
                best = (col, d);
            }
        }
        (line, best.0)
    }

    /// This buffer as a scrollbar's state, for `visible` lines on screen.
    ///
    /// The same shape [`ListState::bar`] has, and for the same reason: the widget owns the
    /// arithmetic, so a bar and the text beside it cannot disagree about where a thumb points.
    pub fn bar(&self, visible: usize) -> ScrollState {
        ScrollState {
            offset: self.offset as u32,
            visible: visible as u32,
            total: self.lines.len() as u32,
        }
    }

    /// Scroll so `offset` is the first visible line, clamped to what there is.
    ///
    /// **The cursor does not move.** A scrollbar drag changes what is *shown*; an editor that
    /// dragged the caret along with the view would lose the place the person was working at.
    pub fn scroll_to(&mut self, offset: usize, visible: usize) {
        self.offset = offset.min(self.lines.len().saturating_sub(visible.max(1)));
    }

    /// Select from `from` to `to`, putting the cursor at `to`.
    ///
    /// **Total, and clamped**, because the caller's coordinates may be stale: the range a paste
    /// returned is only valid until the next edit, and a cycle that arrived after one would
    /// otherwise index out of the buffer. Clamping means a stale range selects something
    /// harmless rather than panicking in an editor holding somebody's unsaved work.
    pub fn select_range(&mut self, from: (usize, usize), to: (usize, usize)) {
        let clamp = |(line, col): (usize, usize), lines: &Vec<String>| {
            let line = line.min(lines.len() - 1);
            let mut col = col.min(lines[line].len());
            while col > 0 && !lines[line].is_char_boundary(col) {
                col -= 1;
            }
            (line, col)
        };
        let from = clamp(from, &self.lines);
        let to = clamp(to, &self.lines);
        self.anchor = Some(from);
        self.line = to.0;
        self.col = to.1;
        self.goal = None;
        self.group = None;
    }

    /// Split the line at the cursor, replacing any selection.
    pub fn newline(&mut self) {
        self.begin(EditKind::Typing);
        self.delete_selection();
        let tail = self.lines[self.line].split_off(self.col);
        self.lines.insert(self.line + 1, tail);
        self.line += 1;
        self.col = 0;
        self.goal = None;
        self.revision += 1;
        // A line is a group, which is the coarser half of "a word or a line".
        self.group = None;
    }

    /// Start a group of `kind` if one is not already open, and forget the way forward.
    ///
    /// **Called before the edit, because the snapshot is of what came before it.** An edit that
    /// will not happen must not call this: a snapshot pushed for a `Backspace` at the start of
    /// the buffer is an undo step that visibly does nothing, which is worse than no step at all.
    fn begin(&mut self, kind: EditKind) {
        if self.group != Some(kind) {
            let here =
                Snapshot { lines: self.lines.clone(), line: self.line, col: self.col };
            if self.undo.len() == MAX_UNDO {
                self.undo.remove(0);
            }
            self.undo.push(here);
            self.group = Some(kind);
        }
        // **Any edit abandons the way forward**, which is what makes redo a branch rather than a
        // second history: typing after an undo means the undone text is not coming back, and a
        // stack that kept it would offer to restore something since replaced.
        self.redo.clear();
    }

    /// Close whatever group is open, so the next edit starts a new one.
    ///
    /// **For the boundaries this type cannot see.** Movement, a separator and `Enter` are edits,
    /// and it closes groups on all three by itself — but *saving* is an application's event, and
    /// it is exactly the boundary a person means: what they want back after a save is what they
    /// have typed since it, not everything since the file was opened. Without this, typing a
    /// word, saving, and typing another word left one group, and one undo emptied the buffer
    /// back to the file's original contents. `check-login` found it, by byte count, from outside.
    pub fn end_group(&mut self) {
        self.group = None;
    }

    /// Go back one group of edits. `false` when there is nothing to go back to.
    ///
    /// **The revision moves**, so a buffer undone back to what is on disk still reads as
    /// modified. That is deliberate and it is the safe direction: the alternative is comparing
    /// the whole text against the file on every keystroke, and over-reporting only means a person
    /// is asked before closing something that turned out to match — `TODO(undo-clean-revision)`.
    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else { return false };
        self.redo.push(Snapshot { lines: self.lines.clone(), line: self.line, col: self.col });
        self.restore(prev);
        true
    }

    /// Come forward one group. `false` when there is nothing ahead.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else { return false };
        self.undo.push(Snapshot { lines: self.lines.clone(), line: self.line, col: self.col });
        self.restore(next);
        true
    }

    /// Put a snapshot back, and end whatever group was open.
    ///
    /// The cursor is clamped rather than trusted: it came from this buffer, but restoring the
    /// *other* stack's entry after a sequence of undos and redos is one arithmetic slip away
    /// from an index that panics inside `text_area` on the next frame.
    fn restore(&mut self, s: Snapshot) {
        self.lines = s.lines;
        self.line = s.line.min(self.lines.len().saturating_sub(1));
        self.col = s.col.min(self.lines[self.line].len());
        self.anchor = None;
        self.goal = None;
        self.group = None;
        self.revision += 1;
    }

    /// Find `needle` after the cursor, wrapping once, and select it. `false` if it is nowhere.
    ///
    /// **After the cursor and wrapping**, which is what makes pressing the same key again walk
    /// through every occurrence and come back to the first. An empty needle matches nothing
    /// rather than everything: it is what the field holds before anything is typed, and a search
    /// that jumped on each keystroke of an empty one would move the buffer under the person.
    ///
    /// The match is **selected**, not merely scrolled to. A cursor sitting silently at a hit
    /// leaves the person to find it; a highlight says which of several this one is. The scrolling
    /// is the widget's, which follows the cursor on the next frame.
    pub fn find(&mut self, needle: &str) -> bool {
        if needle.is_empty() {
            return false;
        }
        // **From the next character after the cursor**, or a second press finds the match it is
        // already sitting on.
        //
        // **Character, not byte.** `col + 1` lands *inside* a multi-byte character whenever the
        // cursor is on one, and `str::get` of a range that starts there yields `None` — so the
        // search silently skipped the rest of that line and wrapped to a match behind the
        // cursor. It looked like "find went backwards", and the test below is what found it.
        let text = &self.lines[self.line];
        let mut from = (self.col + 1).min(text.len());
        while from < text.len() && !text.is_char_boundary(from) {
            from += 1;
        }
        let start = (self.line, from);
        let hit = self.search_from(needle, start).or_else(|| self.search_from(needle, (0, 0)));
        let Some((line, col)) = hit else { return false };
        self.line = line;
        self.col = col;
        self.anchor = Some((line, col + needle.len()));
        self.goal = None;
        self.group = None;
        true
    }

    /// The first occurrence of `needle` at or after `(line, col)`, searching to the end only.
    ///
    /// **Never spans a line break**, which is a limit rather than an oversight: lines are
    /// separate `String`s here, and a needle containing one would have to be split and matched
    /// piecewise. Nothing can type a newline into the find field, so it is unreachable from the
    /// editor — stated so that the next caller does not assume otherwise.
    fn search_from(&self, needle: &str, (line, col): (usize, usize)) -> Option<(usize, usize)> {
        for (i, text) in self.lines.iter().enumerate().skip(line) {
            let from = if i == line { col.min(text.len()) } else { 0 };
            // `get` rather than a slice: `from` can land inside a multi-byte character, and this
            // is reached with `col + 1` on every search.
            if let Some(at) = text.get(from..).and_then(|tail| tail.find(needle)) {
                return Some((i, from + at));
            }
        }
        None
    }

    /// Delete backwards: the selection if there is one, else the character before the cursor,
    /// else join with the previous line.
    pub fn backspace(&mut self) -> bool {
        // **A selection first, and it is its own kind of edit.** Replacing one is not a run of
        // deletions: whatever comes next — usually the character being typed over it — starts a
        // group of its own.
        if self.selection().is_some() {
            self.begin(EditKind::Deleting);
            self.delete_selection();
            self.group = None;
            return true;
        }
        // **And `delete_selection` still has work to do with no selection**, which is the reason
        // this call is here and not folded into the branch above. It does two jobs: it removes a
        // selection, and it clears an anchor the cursor has *walked back onto* — one that leaves
        // no selection but would otherwise survive this edit and name a byte past the end of a
        // line it has just shortened. That is PR #258's blocking 1, and skipping this call
        // brought it straight back; the test written for it then is what caught that.
        self.delete_selection();
        // **One guard, before the group**, so a `Backspace` that cannot do anything leaves no
        // undo step that visibly does nothing — and so that everything below can rely on there
        // being something behind the cursor. It is load-bearing twice, which is why the inner
        // repeats of it are gone: they could not fire, and a guard that cannot fire reads as
        // protecting an invariant it does not (PR #269 review, optional 2).
        if self.col == 0 && self.line == 0 {
            return false;
        }
        self.begin(EditKind::Deleting);
        self.goal = None;
        if self.col > 0 {
            let prev = self.prev_boundary();
            self.lines[self.line].remove(prev);
            self.col = prev;
        } else {
            // **Joining is the case a single-line field never has**, and the cursor lands where
            // the join happened rather than at the start of the merged line — which is where the
            // text the person was deleting towards now is. `col == 0` here, and the guard above
            // ruled out `line == 0`, so there is a line before this one.
            let cur = self.lines.remove(self.line);
            self.line -= 1;
            self.col = self.lines[self.line].len();
            self.lines[self.line].push_str(&cur);
        }
        self.revision += 1;
        true
    }

    /// Delete forwards: the selection, else the character after the cursor, else join with the
    /// next line.
    pub fn delete(&mut self) -> bool {
        // The mirror of `backspace`, with the same one guard and for the same two reasons.
        if self.selection().is_some() {
            self.begin(EditKind::Deleting);
            self.delete_selection();
            self.group = None;
            return true;
        }
        // As in `backspace`: this clears a collapsed anchor and reports that it deleted nothing.
        self.delete_selection();
        if self.col >= self.lines[self.line].len() && self.line + 1 >= self.lines.len() {
            return false;
        }
        self.begin(EditKind::Deleting);
        self.goal = None;
        if self.col < self.lines[self.line].len() {
            self.lines[self.line].remove(self.col);
        } else {
            // At the end of a line that is not the last: the guard above says so.
            let next = self.lines.remove(self.line + 1);
            self.lines[self.line].push_str(&next);
        }
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
    /// **A movement ends whatever group was open**, wherever it lands: the cursor moving means
    /// what is typed next is a different edit, and a group that spanned one would undo two
    /// separate pieces of text at once.
    fn before_move(&mut self, extend: bool) {
        self.group = None;
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
        // Once per *reason to follow*, for [`ListState::ensure_visible`]'s reason: a document
        // that followed its caret on every build could not be scrolled away from it, and one
        // that followed only on a line change let you type into a screen you were not looking at.
        let now = (self.line, self.col, self.revision, visible);
        if self.followed == Some(now) {
            return;
        }
        self.followed = Some(now);
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

/// A stretch of one line of a [`text_area`], drawn in a colour of its own.
///
/// **Colours rather than token kinds**, which is what keeps this toolkit out of the business of
/// knowing what a language is: the application scans its own text and looks the colour up in the
/// theme, and the widget merely draws what it is handed. `nxedit::syntax` is the first producer.
///
/// **Byte offsets, and the widget does not trust them.** A cache of these is computed from the
/// buffer as it was a moment ago, so an edit can leave one naming bytes past the end of a
/// shortened line, or inside a character — which must be a wrong colour for one frame rather
/// than a panic. A run's bounds become split points only where they are character boundaries of
/// the line as it is *now*, which refuses both.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InkRun {
    /// Which line, as an index into the state's lines.
    pub line: usize,
    /// First byte within that line, inclusive.
    pub start: usize,
    /// Last byte, exclusive.
    pub end: usize,
    /// What to draw it in.
    pub colour: Rgb,
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
/// **`pointer` is what makes it clickable** (M15). The state has had `place` and `extend_to`
/// since M10 — documented, then, as "what a press does" and "what a drag does" — and no widget
/// ever handed them anything: this took no pointer events at all, so an editor's caret could
/// only be moved with the arrow keys. The handler receives widget-local coordinates; the
/// application turns them into a line and a column with
/// [`TextAreaState::at_point`](TextAreaState::at_point), because that needs to measure text and
/// this crate has no glyphs.
///
/// **What it draws:** the visible lines, the selection behind the text on each, the caret when
/// `active`, and each line in the colours `ink` gives it. What it does *not* draw is a scrollbar
/// — that is `scrollbar`'s, composed beside it by an application that wants one, the way the
/// terminal composes its own.
///
/// `ink` may be empty, which is a text area in one colour and what every caller had before
/// M14 Part G. Runs for lines that are not on screen are ignored, so a caller may hand over
/// whatever it has cached.
pub fn text_area<Msg>(
    state: &mut TextAreaState,
    height: u32,
    row_height: u32,
    active: bool,
    ink: &[InkRun],
    pointer: Option<fn(PointerEvent) -> Msg>,
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

        // Where the caret goes on this line, if it is on this line at all.
        //
        // **Clamped into the line**, because it is drawn at a *cut* below and a cut outside the
        // line would be dropped — taking the caret with it. A cursor column past the end is not
        // supposed to happen; a caret nobody can find is the failure that follows if it does
        // (PR #258 review, blocking 2, which this rewrite subsumes).
        let caret = (active && i == cur_line).then_some(cur_col.min(l.len()));

        // **Every boundary on this line, in one sorted list**, and then one pass over it. The
        // selection, the caret and the syntax runs each split the line, and the version that
        // handled them in sequence had to decide *when* to emit the caret relative to the
        // highlight — a question with two right answers, since a selection's cursor is at its
        // start as often as at its end. As cuts there is no ordering left to get wrong: the
        // caret is a boundary like any other and comes out where it sits (M14 Part G).
        let mut cuts: Vec<usize> = Vec::with_capacity(8);
        cuts.push(0);
        cuts.push(l.len());
        if let Some((from, to)) = span {
            cuts.push(from);
            cuts.push(to);
        }
        if let Some(cc) = caret {
            cuts.push(cc);
        }
        // **Not trusted: a cut has to be a character boundary of *this* line.** These come from
        // a scan of the buffer as it was, so an edit that shortened this line leaves runs naming
        // bytes past its end, and a cut inside a multi-byte character would panic on the slice
        // below. `is_char_boundary` refuses both — it is false for every index past the end —
        // which is the whole of the defence. A `min(l.len())` beside it reads like the guard and
        // is dead: the only index it changes is one already in `cuts` (PR #289 review, 4).
        let runs = ink.iter().filter(|r| r.line == i);
        for r in runs.clone() {
            for b in [r.start, r.end] {
                if l.is_char_boundary(b) {
                    cuts.push(b);
                }
            }
        }
        cuts.sort_unstable();
        cuts.dedup();

        let mut pieces: Vec<Element<Msg>> = Vec::with_capacity(cuts.len());
        for (k, &from) in cuts.iter().enumerate() {
            if caret == Some(from) {
                pieces.push(sized(Size::new(CARET, 0), fill(theme.accent)));
            }
            let Some(&to) = cuts.get(k + 1) else { break };
            let mut piece = text(String::from(&l[from..to]));
            // The innermost wrapper wins, so a selected keyword keeps its own colour.
            if let Some(r) = runs.clone().find(|r| r.start <= from && from < r.end) {
                piece = crate::element::ink(r.colour, piece);
            }
            if span.is_some_and(|(f, t)| from >= f && to <= t) {
                // The highlight is a `fill` *under* the run: `fill` measures as zero, so the
                // stack takes the text's size and the colour covers exactly the glyphs' box.
                piece = stack(alloc::vec![fill(theme.selection()), piece]);
            }
            pieces.push(piece);
        }
        if pieces.is_empty() {
            // An empty line still needs a row, or the lines below it move up by one.
            pieces.push(text(""));
        }
        rows.push(sized(Size::new(0, row_height), row(pieces)));
    }

    let mut layers = alloc::vec::Vec::with_capacity(2);
    layers.push(fill(theme.track));
    layers.push(padding(FIELD_PAD, column(rows)));
    let mut e = stack(layers).focusable();
    if let Some(f) = pointer {
        // **On the whole area, including its padding.** A press in the margin beside a line is a
        // press on that line — `at_point` subtracts the padding itself, which is the number a
        // caller cannot know.
        e = e.on_pointer(f);
    }
    e
}

/// One row of a [`list_view`].
///
/// **Borrowed, and built fresh each frame from whatever the application already has.** That
/// is the "model" in model-backed: a window list derives these from its window records and a
/// launcher derives them from its filtered program list, neither keeping a parallel array of
/// row widgets to reconcile by hand — which is the hand-rolled diffing `desktop-shell.md` §5
/// says a list widget exists to avoid.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[derive(Default)]
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
    /// Whether this row is part of a **multiple** selection, beside `ListState::selected`.
    ///
    /// **A property of the row, not a second selection in the state** — the same shape [`Tab`]
    /// uses. `ListState` is `Copy` and every list in the system builds its rows each frame
    /// anyway, so a caller that knows a row is picked can just say so; a set inside the state
    /// would cost every caller a `Vec` for a thing only the file browser has.
    ///
    /// Drawn exactly as `selected` is, because to a person they are the same thing: the rows an
    /// action will act on.
    pub marked: bool,
    /// The trailing cells, one per column the list declares (desktop refresh, Part I).
    ///
    /// **Empty for every list but one.** A browser's listing is a name and three facts about it;
    /// a window list, a launcher's results and a chooser's rows are a name and nothing else, and
    /// they say so by leaving this `&[]` — which is what `Default` gives them.
    ///
    /// Fewer cells than columns is fine: the rest are blank. More are ignored.
    pub cells: &'a [&'a str],
    /// A small square before the label — the browser's folder-or-file mark.
    pub swatch: Option<Rgb>,
}

/// A trailing column in a list: how wide it is, and which end its text sits against.
///
/// **Fixed widths, and the label takes the rest.** The design's browser is `Name` flexible with
/// `Size`, `Kind` and `Modified` at 70, 60 and 96 — columns that shared the width out would move
/// under each other as a listing changed, which is the argument `TAB_W` already makes for tabs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ListColumn {
    /// How wide, in pixels.
    pub width: u32,
    /// Whether the text sits against the column's left edge or its right.
    ///
    /// **A size reads right-aligned** — the digits line up and the eye compares them — and
    /// everything else reads left.
    pub align: ColumnAlign,
}

/// Which end of a column its text sits against. See [`ListColumn::align`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColumnAlign {
    /// Against the left edge.
    Left,
    /// Against the right edge.
    Right,
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
    /// What the last [`ensure_visible`](Self::ensure_visible) was for: the selected row, and how
    /// many rows fitted.
    ///
    /// **What stops "keep the selection on screen" from meaning "never scroll anywhere else".**
    /// `list_view` calls `ensure_visible` on every build, so without this a list whose selection
    /// is row 0 snaps its offset back to 0 on every repaint — and since an application repaints
    /// after every event, a scrollbar drag computed the right offset and had it thrown away
    /// before anything was drawn. The bar moved and the list did not (M15 Part D).
    ///
    /// **The row count is part of it**, so a window that got shorter brings the selection back
    /// into view rather than waiting for it to move. `TextAreaState` carries the text's revision
    /// here too; a list has no equivalent, because everything that changes a list's rows also
    /// replaces its selection.
    followed: Option<(usize, usize)>,
}

impl ListState {
    /// A list showing `offset` first, with `selected` picked.
    ///
    /// **A constructor rather than a literal**, since M15 Part D: the state carries one piece of
    /// bookkeeping ([`followed`](Self::followed)) that no caller has an opinion about, and a
    /// private field cannot be written in a struct literal from another module — which is the
    /// point. `selected` and `offset` stay public, because a caller does have opinions about
    /// those.
    pub fn at(selected: Option<usize>, offset: usize) -> Self {
        Self { selected, offset, followed: None }
    }

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
        // **Once per selection, not once per build.** Following a selection is what a *changed*
        // selection asks for; doing it every time the widget is built makes every other way of
        // scrolling impossible, because the next repaint undoes it.
        if self.followed == Some((i, visible)) {
            return;
        }
        self.followed = Some((i, visible));
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
    /// Scroll by a turn of the wheel, `dz` detents, positive **down**.
    ///
    /// **No upper clamp here**, deliberately: [`list_view`] already clamps the offset against
    /// the rows it is given every time it builds, and it is the one that knows how many fit.
    /// A second clamp would need the caller to pass a count it has no other reason to have,
    /// and two clamps disagreeing is how a list ends up unable to reach its last row.
    ///
    /// **Widened before multiplying**, because `dz` saturates at the compositor: a consumer
    /// that stalled while somebody scrolled receives thousands of detents, and `i16`
    /// arithmetic on that scrolls the other way.
    pub fn wheel(&mut self, dz: i16) {
        let rows = i64::from(dz) * i64::from(crate::click::WHEEL_UNITS);
        self.offset = if rows < 0 {
            self.offset.saturating_sub(rows.unsigned_abs() as usize)
        } else {
            self.offset.saturating_add(rows as usize)
        };
    }

    /// This list as a scrollbar's state.
    ///
    /// **The conversion belongs here, not in each caller**, because the widget already knows the
    /// arithmetic and the caller only knows the numbers it passed in. `nxterm` builds the same
    /// thing from a grid's line numbers — this is that for a list, so the two cannot drift apart
    /// on rounding.
    ///
    /// Takes the same `height`, `row_height` and row count [`list_view`] was given: a drag
    /// converted against a different geometry from the one drawn puts the thumb where the pointer
    /// is not (M11 Part E batch 6).
    pub fn bar(&self, height: u32, row_height: u32, total: usize) -> ScrollState {
        let visible = if row_height == 0 { 0 } else { height / row_height };
        ScrollState { offset: self.offset as u32, visible, total: total as u32 }
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
/// **`ground` is the colour behind the rows**, or `None` for the ordinary list ground. The one
/// caller that passes something is a *sidebar*: a panel beside content has to be told from the
/// content at a glance, and drawn in the list's own ground it reads as a list with a gap in it
/// (M15). A colour rather than a flag, because what a panel is depends on the theme.
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
    columns: &[ListColumn],
    state: &mut ListState,
    height: u32,
    row_height: u32,
    activate: fn(u64) -> Msg,
    grab: Option<fn(u64) -> Msg>,
    scroll: Option<fn(PointerEvent) -> Msg>,
    hovered: Option<u64>,
    ground: Option<Rgb>,
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

    // **The surface the rows sit on.** A row used to fill `theme.track` whatever the list's
    // ground was, so a panel with a ground of its own had list-coloured tiles painted over it and
    // the panel showed only below the last row (M15 Part F).
    //
    // **And a highlight is a wash over it** (desktop refresh, Part B): the accent at the
    // selection's 20%, or at the scheme's hover coverage. That is the design's model, and it is
    // why the hover no longer has to be derived per ground — "this surface, lit" was what the
    // derivation approximated, and a wash over whatever surface is under it is that exactly.
    let ground = ground.unwrap_or(theme.track);
    let hover = theme.scheme.hover_coverage();
    let last = (state.offset + visible).min(rows.len());
    let mut items = alloc::vec::Vec::with_capacity(last.saturating_sub(state.offset));
    for (i, r) in rows.iter().enumerate().take(last).skip(state.offset) {
        let selected = state.selected == Some(i) || r.marked;
        // **A selection is the accent washed over the ground**, flat, since the desktop refresh's
        // Part B. It was a one-pixel accent border around a bevelled fill (M11 Part E, batch 2),
        // the reference desktop's, and the border was what kept two adjacent selections from
        // merging into one block; the design lets them merge, as a block is what a run of
        // selected rows is.
        //
        // **Hover is quieter than selection and loses to it** (batch 3) — *unless nothing is
        // selected*, in which case it is the answer and gets the blue (batch 5). The rule is
        // still "one primary highlight": two of equal weight is two answers to "what happens if
        // I act now", and where there is no keyboard selection to compete with, the pointer's is
        // not competing. The applications modal is exactly that list — it keeps no selection at
        // all, so every hover landed on the quiet branch and the menu highlighted in grey.
        let primary = selected || (hovered == Some(r.key) && state.selected.is_none());
        let mut layers = alloc::vec![fill(ground)];
        if primary {
            layers.push(wash(theme.accent, libdraw::theme::SELECTION_COVERAGE));
        } else if hovered == Some(r.key) {
            layers.push(wash(theme.accent, hover));
        }
        // **The label, then whatever trailing cells the list declares** (desktop refresh,
        // Part I). A row with no columns is what it always was: one padded label. With them,
        // the label takes the space the columns leave — so a long name is what gets cut short,
        // never a size or a date, which are the same width in every row and are read as a
        // column rather than as text.
        // **A plain row stays exactly what it was**: one padded label, with no row wrapper around
        // it. Every list in the system but the browser's has no columns and no swatch, and
        // wrapping those in a `Row` would put a node per row in every window list and launcher
        // result for nothing.
        if columns.is_empty() && r.swatch.is_none() {
            layers.push(padding(ROW_PAD, text(r.label)));
            let row_el = stack(layers);
            let mut item =
                sized(Size::new(0, row_height), row_el).key(r.key).on_press(activate(r.key));
            if let Some(f) = grab {
                item = item.on_press_down(f(r.key));
            }
            items.push(item);
            continue;
        }
        let mut cells = alloc::vec::Vec::with_capacity(columns.len() + 2);
        if let Some(colour) = r.swatch {
            cells.push(center_v(sized(
                Size::new(SWATCH_W, SWATCH_H),
                rounded_fill(colour, SWATCH_RADIUS),
            )));
            cells.push(sized(Size::new(SWATCH_GAP, 0), text("")));
        }
        cells.push(center_v(text(r.label)).flex(1));
        for (i, c) in columns.iter().enumerate() {
            let cell = r.cells.get(i).copied().unwrap_or("");
            let text_el = center_v(ink(theme.foreground_dim, scaled(TextSize::Small, text(cell))));
            cells.push(sized(
                Size::new(c.width, 0),
                match c.align {
                    ColumnAlign::Left => text_el,
                    // **Pushed rather than aligned**: a `Row` lays its children out left to
                    // right, so the way to put text against the right edge is to give the space
                    // before it to something that takes what is left.
                    ColumnAlign::Right => {
                        row(alloc::vec![text("").flex(1), text_el, sized(Size::new(CELL_PAD, 0), text(""))])
                    }
                },
            ));
        }
        layers.push(padding(ROW_PAD, row(cells)));
        let row_el = stack(layers);
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
    stack(alloc::vec![fill(ground), body]).focusable()
}

/// A row swatch: the design's 11×9, with the barest corner on it.
pub const SWATCH_W: u32 = 11;
/// See [`SWATCH_W`].
pub const SWATCH_H: u32 = 9;
/// See [`SWATCH_W`].
const SWATCH_RADIUS: u32 = 1;
/// Between a swatch and the name beside it: the design's 8.
const SWATCH_GAP: u32 = 8;
/// The space after a right-aligned cell's text, so a column of sizes does not sit hard against
/// the next column's edge.
const CELL_PAD: u32 = 6;

/// How tall a list's column header is: the design's 25.
pub const LIST_HEADER_H: u32 = 25;

/// The header over a list's columns: the primary heading, then one per column.
///
/// **Here rather than in the application**, because a heading that did not sit exactly over its
/// column would be worse than none — and the insets that put it there are this module's: a row's
/// padding, a swatch's width and the gap after it. One source, two readers, which is the
/// arrangement every pair of numbers in this refresh ended up with (desktop refresh, Part I).
///
/// The headings are `Small` and dim, like the cells under them, on the `face_hover` ground every
/// strip of chrome in a window uses.
pub fn list_header<Msg>(
    primary: &str,
    headings: &[&str],
    columns: &[ListColumn],
    swatched: bool,
    theme: &Theme,
) -> Element<Msg> {
    let heading = |s: &str| center_v(ink(theme.foreground_dim, scaled(TextSize::Small, text(s))));
    let mut cells = alloc::vec::Vec::with_capacity(columns.len() + 2);
    // The same lead-in the rows have, so `Name` starts where a name does.
    if swatched {
        cells.push(sized(Size::new(SWATCH_W + SWATCH_GAP, 0), text("")));
    }
    cells.push(heading(primary).flex(1));
    for (i, c) in columns.iter().enumerate() {
        let label = heading(headings.get(i).copied().unwrap_or(""));
        cells.push(sized(
            Size::new(c.width, 0),
            match c.align {
                ColumnAlign::Left => label,
                ColumnAlign::Right => {
                    row(alloc::vec![text("").flex(1), label, sized(Size::new(CELL_PAD, 0), text(""))])
                }
            },
        ));
    }
    sized(
        Size::new(0, LIST_HEADER_H),
        stack(alloc::vec![
            fill(theme.face_hover),
            dock(
                alloc::vec![docked(Edge::Bottom, sized(Size::new(0, 1), fill(theme.border)))],
                text(""),
            ),
            padding(ROW_PAD, row(cells)),
        ]),
    )
}

/// How wide a list's scrollbar is, in pixels.
const SCROLLBAR_W: u32 = 12;

#[cfg(test)]
mod list_view_tests {
    use super::*;
    use libdraw::format::Rgb;
    use crate::element::Node;

    fn rows<'a>(labels: &'a [(u64, &'a str)]) -> alloc::vec::Vec<ListRow<'a>> {
        labels.iter().map(|&(key, label)| ListRow { key, label, marked: false, ..Default::default() }).collect()
    }

    /// A row's columns are where the list declares them, and its swatch is drawn.
    ///
    /// **Painted, and written before the screendump this time.** Three parts running have shipped
    /// a widget that was in the tree, routed correctly and drew nothing — so what this asserts is
    /// ink: a swatch's colour at the row's left end, a right-aligned cell's text inside its own
    /// column and not the one beside it, and the name giving way to the columns rather than the
    /// other way round (desktop refresh, Part I).
    #[test]
    fn a_rows_cells_land_in_their_columns_and_its_swatch_is_drawn() {
        use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
        use libdraw::format::PixelFormat;
        const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSans.ttf");
        let font = libdraw::text::Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font");
        let t = Theme::default();
        let (w, h) = (400u32, 25u32);
        let columns = [
            ListColumn { width: 70, align: ColumnAlign::Right },
            ListColumn { width: 60, align: ColumnAlign::Left },
        ];
        let mark = t.accent;
        let row = ListRow {
            key: 1,
            label: "a-very-long-file-name-that-would-run-past-the-columns.txt",
            marked: false,
            cells: &["848", "toml"],
            swatch: Some(mark),
        };
        let mut st = ListState::default();
        let e: Element<u64> =
            list_view(&[row], &columns, &mut st, h, h, |k| k, None, None, None, None, &t);
        let all = libdraw::geom::Rect::new(0, 0, w, h);
        let l = crate::layout::layout(&e, all, &crate::paint::FontMetrics::new(&font, t.font_px));
        let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
        fb.clear(t.background);
        crate::paint::paint(&mut fb, &font, &t, &e, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
        let count = |x0: u32, x1: u32, c: Rgb| {
            (x0..x1)
                .flat_map(|x| (0..h).map(move |y| (x, y)))
                .filter(|(x, y)| fb.get_pixel(*x, *y) == Some(c))
                .count()
        };
        // The swatch, at the row's left end and in its colour.
        assert!(count(0, ROW_PAD.left + SWATCH_W + 2, mark) >= (SWATCH_W * SWATCH_H / 2) as usize,
            "the swatch is drawn where the row starts");
        assert_eq!(count(120, w, mark), 0, "and nowhere else");
        // Ink in each column's own band. The columns are the last 130 pixels: 70 then 60.
        let (size_from, kind_from) = (w - ROW_PAD.right - 130, w - ROW_PAD.right - 60);
        let ink_in = |x0: u32, x1: u32| {
            (x0..x1)
                .flat_map(|x| (0..h).map(move |y| (x, y)))
                .filter(|(x, y)| {
                    let c = fb.get_pixel(*x, *y);
                    c != Some(t.background) && c != Some(mark)
                })
                .count()
        };
        assert!(ink_in(size_from, kind_from) > 10, "the size cell is drawn in its column");
        assert!(ink_in(kind_from, w - ROW_PAD.right) > 10, "and the kind cell in its own");
        // **The name gives way, not the columns.** A long name is clipped at the column's edge
        // rather than pushing it along — asserted by *which ink* is in the columns' band, since
        // the name is body ink and a cell is the dim step: the darkest pixel there must be no
        // darker than `foreground_dim`. (A first version asserted a blank gap before the
        // columns, which a name is entitled to fill.)
        let darkness = |c: Rgb| c.r as u32 + c.g as u32 + c.b as u32;
        let darkest = (size_from..w)
            .flat_map(|x| (0..h).map(move |y| (x, y)))
            .filter_map(|(x, y)| fb.get_pixel(x, y))
            .map(darkness)
            .min()
            .expect("pixels");
        assert!(
            darkest >= darkness(t.foreground_dim),
            "something in the columns is body ink at {darkest}: the name has run into them"
        );
    }

    /// The wheel moves the offset by whole rows, and stops at the top.
    ///
    /// **The bottom is not clamped here and that is deliberate** — `list_view` does it against
    /// the rows it is handed, which is the only place the count is known. This checks the half
    /// that *is* here: the direction, the multiplier, and that scrolling up past the first row
    /// stops rather than wrapping to an enormous offset (`offset` is unsigned, so the failure
    /// would be a list showing nothing at all).
    #[test]
    fn the_wheel_moves_whole_rows_and_stops_at_the_top() {
        let mut st = ListState::at(None, 0);
        st.wheel(2);
        assert_eq!(st.offset, 2 * crate::click::WHEEL_UNITS as usize, "positive is down");
        st.wheel(-1);
        assert_eq!(st.offset, crate::click::WHEEL_UNITS as usize);
        st.wheel(-100);
        assert_eq!(st.offset, 0, "a scroll off the top stops there");
    }

    /// A saturated delta scrolls a long way down rather than wrapping upward.
    ///
    /// **20 000 rather than `i16::MAX`**: `32767 * 3` wraps back round to a *positive* `i16`, so
    /// the largest value passes against the broken arithmetic this exists to catch. The value
    /// has to be one whose overflow is visible — the same trap `nxterm`'s copy of this test hit.
    #[test]
    fn a_huge_wheel_delta_does_not_wrap_upward() {
        let mut st = ListState::at(None, 100);
        st.wheel(20_000);
        assert!(st.offset > 100, "scrolled down, not back to the top: {}", st.offset);
    }

    /// A row rests on the list's own ground, and is lit from it.
    ///
    /// **A row used to fill `theme.track` whatever the list's ground was** (M15 Part F), so a
    /// panel with a ground of its own had list-coloured tiles painted over it — the panel showed
    /// only in the gap below the last row, which is not what a panel is. The hover is a wash of
    /// the accent over that ground since the desktop refresh's Part B — "this surface, lit",
    /// which a hover derived by shading the ground only approximated.
    #[test]
    fn a_rows_ground_is_the_lists_and_its_hover_is_a_wash_over_it() {
        use crate::element::Node;
        let p = Theme::default();
        let data = [(1u64, "alpha"), (2, "beta")];
        let panel = Rgb::new(0xDD, 0xDA, 0xD6);
        // **A selection is kept, and the hover is on a *different* row.** With nothing selected
        // a hovered row is the primary highlight, so a fixture without a selection never reaches
        // the hover branch at all (M11 Part E, batch 5).
        //
        // Each row's fill and wash, in order.
        let rows_of = |ground: Option<Rgb>, hovered: Option<u64>| {
            let mut st = ListState::at(Some(0), 0);
            let e: Element<u64> =
                list_view(&rows(&data), &[], &mut st, 100, 20, |k| k, None, None, hovered, ground, &p);
            let mut out: Vec<(Option<Rgb>, Option<(Rgb, u8)>)> = Vec::new();
            walk(&e, &mut |n| {
                if n.key.is_none() {
                    return;
                }
                let (mut f, mut w) = (None, None);
                walk(n, &mut |c| match &c.node {
                    Node::Fill(rgb) if f.is_none() => f = Some(*rgb),
                    Node::Wash { colour, coverage } => w = Some((*colour, *coverage)),
                    _ => {}
                });
                out.push((f, w));
            });
            out
        };
        let hover = p.scheme.hover_coverage();
        let sel = libdraw::theme::SELECTION_COVERAGE;

        // The default list: rows on `track`; the selected row washed at 20%, the hovered one at
        // the hover's coverage, both over their own ground.
        assert_eq!(
            rows_of(None, Some(2)),
            [(Some(p.track), Some((p.accent, sel))), (Some(p.track), Some((p.accent, hover)))]
        );
        // A list on a panel rests on the panel, and the same washes land on it.
        assert_eq!(
            rows_of(Some(panel), Some(2)),
            [(Some(panel), Some((p.accent, sel))), (Some(panel), Some((p.accent, hover)))]
        );
        // And a row nobody is pointing at is its ground and nothing more.
        assert_eq!(rows_of(Some(panel), None)[1], (Some(panel), None));
    }

    /// A **marked** row is drawn as a selected one, and an unmarked one is not.
    ///
    /// **Asserted on the tree, because nothing else can be.** `check-display` renders the guest's
    /// expected picture with this same code, so a broken highlight is identical on both sides by
    /// construction; `check-login` presses rows and reads no pixels. Breaking `|| r.marked` left
    /// the entire host suite green — 309 tests — until this existed (PR #285 review, worth fixing
    /// 5).
    #[test]
    fn a_marked_row_is_drawn_like_a_selected_one() {
        fn nodes<M>(e: &Element<M>) -> usize {
            1 + e.children().map(nodes).sum::<usize>()
        }
        let label = [(1u64, "alpha"), (2, "beta")];
        let build = |marked: bool, selected: Option<usize>| {
            let mut r = rows(&label);
            r[1].marked = marked;
            let mut st = ListState { selected, offset: 0, ..Default::default() };
            nodes(&list_view(&r, &[], &mut st, 100, 20, |k| k, None, None, None, None, &Theme::default()))
        };
        let plain = build(false, None);
        let marked = build(true, None);
        let selected = build(false, Some(1));
        assert!(marked > plain, "a marked row must cost more layers than a resting one");
        assert_eq!(marked, selected, "and be drawn as the selected row is");
    }

    /// The whole point of the widget: a hundred rows cost as many elements as fit.
    #[test]
    fn only_the_visible_rows_become_elements() {
        let data: alloc::vec::Vec<(u64, &str)> = (0..100u64).map(|i| (i, "row")).collect();
        let r = rows(&data);
        let e: Element<u64> =
            list_view(&r, &[], &mut ListState::default(), 100, 20, |k| k, None, None, None, None, &Theme::default());
        assert_eq!(keys(&e).len(), 5, "the list built rows it cannot show");
    }

    /// Without keys the diff pairs by position, and its own doc names the failure: insert at
    /// the top and "row 2's widget" pairs with "row 3's element".
    #[test]
    fn every_row_carries_its_key_not_its_index() {
        let data = [(70u64, "a"), (80, "b"), (90, "c")];
        let e: Element<u64> =
            list_view(&rows(&data), &[], &mut ListState::default(), 100, 20, |k| k, None, None, None, None, &Theme::default());
        assert_eq!(keys(&e), alloc::vec![70, 80, 90], "rows are keyed by position");
    }

    /// The window-list caller: rows reorder in place on every raise.
    #[test]
    fn a_reordered_window_list_keeps_each_rows_identity() {
        let before = [(1u64, "term"), (2, "editor")];
        let after = [(2u64, "editor"), (1, "term")];
        let a: Element<u64> =
            list_view(&rows(&before), &[], &mut ListState::default(), 100, 20, |k| k, None, None, None, None, &Theme::default());
        let b: Element<u64> =
            list_view(&rows(&after), &[], &mut ListState::default(), 100, 20, |k| k, None, None, None, None, &Theme::default());
        assert_eq!(keys(&a), alloc::vec![1, 2]);
        assert_eq!(keys(&b), alloc::vec![2, 1], "the reorder did not move the keys");
    }

    /// The launcher caller: results are replaced wholesale on every keystroke, so the list
    /// gets shorter under a scroll offset that was valid a frame ago.
    #[test]
    fn a_list_that_shrinks_under_a_stale_offset_still_renders() {
        let long: alloc::vec::Vec<(u64, &str)> = (0..20u64).map(|i| (i, "hit")).collect();
        let mut state = ListState::at(Some(19), 0);
        let _: Element<u64> =
            list_view(&rows(&long), &[], &mut state, 100, 20, |k| k, None, None, None, None, &Theme::default());
        assert_eq!(state.offset, 15, "the scroll did not follow the selection");
        let short = [(0u64, "hit"), (1, "hit"), (2, "hit")];
        let e: Element<u64> =
            list_view(&rows(&short), &[], &mut state, 100, 20, |k| k, None, None, None, None, &Theme::default());
        assert_eq!(state.offset, 0, "a stale offset survived the list shrinking");
        assert_eq!(keys(&e).len(), 3, "the list rendered blank");

        // **The selection half, which the first version of this test did not assert and so
        // passed against a widget that left it dangling** (PR #233 review, finding 2). A
        // selection of 19 into three rows highlights nothing, and `down` is *dead*: it takes
        // `Some(i) if i + 1 < len` and otherwise returns `i` unchanged, so a stale index never
        // comes back into range on its own.
        assert_eq!(state.selected, Some(2), "the selection still indexes the longer list");
        assert!(
            row_washes(&e)
                .iter()
                .any(|w| *w == Some((Theme::default().accent, libdraw::theme::SELECTION_COVERAGE))),
            "no row is painted as selected"
        );
        assert!(!state.down(3), "the selection is already on the last row");
        assert!(state.up(), "the arrow keys are dead after the shrink");
        assert_eq!(state.selected, Some(1));
    }

    /// Shrinking to nothing leaves nothing selected, rather than row `-1`.
    #[test]
    fn a_list_that_empties_clears_the_selection() {
        let mut state = ListState::at(Some(3), 2);
        let _: Element<u64> =
            list_view(&[], &[], &mut state, 100, 20, |k| k, None, None, None, None, &Theme::default());
        assert_eq!(state.selected, None, "an empty list kept a selection");
        assert_eq!(state.offset, 0);
    }

    /// A selection moved with the keyboard must stay on screen.
    /// Following the selection does not undo a deliberate scroll.
    ///
    /// **The bug the maintainer reported as "the scrollbar doesn't work"** (M15 Part D). Both
    /// `list_view` and `text_area` call `ensure_visible` on every build, and an application
    /// repaints after every event — so with the selection on row 0, a scrollbar drag computed
    /// the right offset and the very next frame put it back. The bar moved and the list did not.
    /// `nxterm` was unaffected because its grid follows nothing: it keeps a `view_top` instead.
    #[test]
    fn following_a_selection_happens_once_per_selection_not_once_per_build() {
        let mut s = ListState::at(Some(0), 0);
        s.ensure_visible(5);
        assert_eq!(s.offset, 0);

        // A scrollbar drag, then the repaint that follows every event.
        s.offset = 20;
        s.ensure_visible(5);
        assert_eq!(s.offset, 20, "the repaint dragged the list back to its selection");
        s.ensure_visible(5);
        assert_eq!(s.offset, 20, "…and again on the frame after that");

        // **But a selection that *moves* is still followed**, which is what the rule is for.
        s.down(40);
        s.ensure_visible(5);
        assert_eq!(s.offset, 1, "the selection moved and the view did not follow it");
    }

    /// A window that gets shorter brings the selection back into view.
    ///
    /// **The case a key of "the selection we last followed" alone gets wrong**, and the one
    /// flagged when Part D landed: a shrink can put the selection off screen without the
    /// selection moving, so a rule that waits for it to move leaves a list highlighting a row
    /// nobody can see (PR #290 review, 1).
    #[test]
    fn a_shorter_window_brings_the_selection_back() {
        let mut s = ListState::at(Some(9), 0);
        s.ensure_visible(10);
        assert_eq!(s.offset, 0, "precondition: ten rows fit, so nothing moved");

        // Four fit now, and the selection is past the bottom.
        s.ensure_visible(4);
        assert_eq!(s.offset, 6, "the selection was left below a window that shrank");
    }

    /// The same rule for a document's caret.
    #[test]
    fn a_document_scrolls_away_from_its_caret_and_stays_there() {
        let mut a = TextAreaState::with_text("0\n1\n2\n3\n4\n5\n6\n7\n8\n9");
        a.ensure_visible(3);
        assert_eq!(a.offset(), 0);

        a.scroll_to(6, 3);
        a.ensure_visible(3);
        assert_eq!(a.offset(), 6, "the repaint dragged the document back to its caret");

        // Typing moves the caret, and the view follows it again.
        a.apply(KEY_DOWN, 0);
        a.ensure_visible(3);
        assert_eq!(a.offset(), 1, "the caret moved to line 1 and the view did not follow it");
    }

    #[test]
    fn the_scroll_follows_the_selection_in_both_directions() {
        let mut s = ListState::at(Some(7), 0);
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
        let mut s = ListState::at(Some(2), 0);
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
            let mut s = ListState::at(Some(1), 0);
            assert!(!s.apply(key, 4), "keycode {key} was claimed by the list");
            assert_eq!(s.selected, Some(1));
        }
        let mut s = ListState::at(Some(1), 0);
        assert!(s.apply(108, 4), "Down was not claimed");
        assert_eq!(s.selected, Some(2));
    }

    #[test]
    fn a_menu_item_highlights_under_the_pointer_and_is_flat_otherwise() {
        let p = Theme::default();
        let hot: Element<u8> = menu_item("Clear", 1, true, &p);
        let cold: Element<u8> = menu_item("Clear", 1, false, &p);

        // Every painting node in the item, whatever its kind.
        let paints = |e: &Element<u8>| {
            let mut out = alloc::vec::Vec::new();
            walk(e, &mut |n| match &n.node {
                Node::Fill(c) | Node::Bevel(c) => out.push((*c, 255)),
                Node::Wash { colour, coverage } => out.push((*colour, *coverage)),
                _ => {}
            });
            out
        };

        // **One layer: the accent washed over the bar at the hover's coverage** (desktop refresh,
        // Part B) — the design's `--soft`. It was a border in the accent and the selection
        // bevelled inside it.
        assert_eq!(
            paints(&hot),
            alloc::vec![(p.accent, p.scheme.hover_coverage())],
            "the hovered item is not the hover wash"
        );

        // **And nothing at all otherwise**, which is the half that fails if a highlight sticks:
        // an item that paints a face when it is not hovered is a menu with every row lit.
        assert!(paints(&cold).is_empty(), "an idle item drew a face");
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
            &[],
            &mut ListState::default(),
            100,
            20,
            |k| k,
            None,
            Some(|_| 0),
            None,
            None,
            &p,
        );
        assert_eq!(handlers(&with), 1, "the scrollbar took no pointer handler");
        let without: Element<u64> =
            list_view(&rows(&many), &[], &mut ListState::default(), 100, 20, |k| k, None, None, None, None, &p);
        assert_eq!(handlers(&without), 0, "a handler appeared with nowhere to send it");
    }

    /// A press at `y` on a bar, and the offset it asks for.
    fn press_bar(g: &mut ScrollGrab, st: ScrollState, track: u32, y: i32) -> Option<u32> {
        g.apply(st, track, PointerEvent {
            kind: POINTER_BUTTON,
            button: 0x110,
            buttons: 1,
            flags: POINTER_PRESSED,
            y,
            ..Default::default()
        })
    }

    /// A motion to `y` with the button still held.
    fn drag_bar(g: &mut ScrollGrab, st: ScrollState, track: u32, y: i32) -> Option<u32> {
        g.apply(st, track, PointerEvent {
            kind: librsproto::surface::POINTER_MOTION,
            buttons: 1,
            y,
            ..Default::default()
        })
    }

    #[test]
    fn a_drag_on_the_track_moves_the_offset_and_a_release_does_not() {
        // **The scrollbar was decoration.** `list_view` built one and gave it no pointer handler,
        // so a list showed its position and could not be dragged — a control that looks live and
        // is not, which is the defect this crate's own notes keep naming. `nxterm` builds its
        // scrollbar directly and has always wired this (M11 Part E batch 6).
        let mut st = ListState::default();
        let mut g = ScrollGrab::new();
        // Twenty rows of 20px in a 100px viewport: five visible, fifteen of travel.
        let bar = |st: &ListState| st.bar(100, 20, 20);
        st.offset = press_bar(&mut g, bar(&st), 100, 100).expect("a press on the track scrolls") as usize;
        assert!(st.offset > 0, "a drag to the bottom of the track moved nothing");
        let bottom = st.offset;
        st.offset = drag_bar(&mut g, bar(&st), 100, 0).expect("still dragging") as usize;
        assert_eq!(st.offset, 0, "a drag to the top did not come back");
        assert!(bottom <= 15, "the offset ran past the last full screen of rows");

        // The release moves nothing and ends the drag.
        let released = g.apply(bar(&st), 100, PointerEvent {
            kind: POINTER_BUTTON,
            button: 0x110,
            buttons: 0,
            flags: 0,
            y: 100,
            ..Default::default()
        });
        assert_eq!(released, None, "a release is not a position");
        assert!(!g.dragging());
        assert_eq!(drag_bar(&mut g, bar(&st), 100, 90), None, "and the bar stopped following");

        // A list that fits has nowhere to go, and must not be moved by a drag on a track that is
        // not drawn — the case the position arithmetic returns zero for.
        let mut st = ListState::default();
        let mut g = ScrollGrab::new();
        st.offset = press_bar(&mut g, st.bar(100, 20, 3), 100, 100).unwrap_or(0) as usize;
        assert_eq!(st.offset, 0, "a list shorter than its viewport scrolled");
    }

    /// Grabbing the thumb by its end does not move it — the `scroll-grab` deferral, closed.
    ///
    /// **The defect this exists for is a *jump* on the press**, before the drag has begun: with
    /// only "put the thumb's centre under the cursor" available, a press on the thumb's top edge
    /// moved the content by half a thumb, so aiming at the thumb threw away the position you
    /// were looking at. Every toolkit that avoids it remembers where within the thumb the press
    /// landed.
    #[test]
    fn a_press_on_the_thumb_does_not_move_it_and_the_drag_then_tracks_the_pointer() {
        // A long scrollback: 1000 units, 20 visible, in a 200px track. The thumb is clamped to
        // `MIN_THUMB`, so it is *much* shorter than the travel — which is when a re-centring
        // grab is least noticeable and a wrong one still throws the view a long way.
        let track = 200;
        let st = ScrollState { offset: 400, visible: 20, total: 1000 };
        let (pos, len) = st.thumb(track);
        assert!(len >= MIN_THUMB, "premise: the thumb is at its floor");

        for (what, y) in [("its top edge", pos as i32), ("its bottom edge", pos as i32 + len as i32 - 1)] {
            let mut g = ScrollGrab::new();
            assert_eq!(
                press_bar(&mut g, st, track, y),
                Some(400),
                "{what}: taking hold of the thumb moved the content"
            );
            assert!(g.dragging());

            // And from there it follows the pointer: ten pixels down the track is ten pixels of
            // thumb travel, whichever end it was taken by.
            let moved = drag_bar(&mut g, st, track, y + 10).expect("dragging");
            let expected = st.offset_for_thumb_top(track, pos as i32 + 10);
            assert_eq!(moved, expected, "{what}: the thumb did not follow by the distance moved");
        }
    }

    /// A second button tapped mid-drag neither ends the drag nor re-takes it.
    ///
    /// **The router delivers every button's events to the captured widget**, and clears its own
    /// capture only when *all* of them come up (`released && event.buttons == 0`) — so pressing
    /// the right button without letting go of the left arrives here as a press and a release of
    /// a button this grab knows nothing about. Answering either of them would strand the bar:
    /// the router still believes the gesture is live, so the person goes on dragging and
    /// nothing follows. The code this replaced (`if p.buttons != 0` in each application) kept
    /// following, so this is a regression the review caught (PR #288, finding 1).
    #[test]
    fn a_second_buttons_press_and_release_do_not_disturb_the_drag() {
        const RIGHT: u16 = 0x111;
        let track = 200;
        let st = ScrollState { offset: 400, visible: 20, total: 1000 };
        let (pos, _) = st.thumb(track);
        let mut g = ScrollGrab::new();
        press_bar(&mut g, st, track, pos as i32);
        assert!(g.dragging(), "precondition: the left button took the thumb");

        // The right button goes down while the left is still held: two buttons in the mask.
        let other = |flags: u16, buttons: u16| PointerEvent {
            kind: POINTER_BUTTON,
            button: RIGHT,
            buttons,
            flags,
            // Far from the thumb, so re-taking the grab here would visibly jump the bar.
            y: track as i32 - 1,
            ..Default::default()
        };
        assert_eq!(g.apply(st, track, other(POINTER_PRESSED, 0b011)), None, "the press answered");
        assert_eq!(g.apply(st, track, other(0, 0b001)), None, "the release answered");
        assert!(g.dragging(), "a button this grab never took ended it");

        // And the drag still tracks the pointer, from the offset it was taken at.
        let moved = drag_bar(&mut g, st, track, pos as i32 + 10).expect("still dragging");
        assert_eq!(moved, st.offset_for_thumb_top(track, pos as i32 + 10));
    }

    /// A press on the track still jumps, and holds the thumb centred afterwards.
    ///
    /// **The other half of the rule**, and the reason the grab is not simply "always keep the
    /// press offset": a click on empty track means "go there", and there is no thumb under the
    /// cursor to preserve a relationship with.
    #[test]
    fn a_press_on_the_track_jumps_and_then_holds_the_thumb_centred() {
        let track = 200;
        let st = ScrollState { offset: 0, visible: 20, total: 1000 };
        let (_, len) = st.thumb(track);
        let mut g = ScrollGrab::new();

        let jumped = press_bar(&mut g, st, track, 150).expect("a track press scrolls");
        assert_eq!(jumped, st.offset_at(track, 150), "the thumb's centre came to the cursor");
        assert!(jumped > 0);

        let after = drag_bar(&mut g, st, track, 160).expect("dragging");
        assert_eq!(
            after,
            st.offset_for_thumb_top(track, 160 - len as i32 / 2),
            "and it stayed centred as the drag continued"
        );
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
            list_view(&rows(&data), &[], &mut ListState::default(), 100, 20, |k| k, None, None, Some(2), None, &p);
        // The primary highlight is the selection's wash, not the quieter hover's.
        let sel = Some((p.accent, libdraw::theme::SELECTION_COVERAGE));
        assert_eq!(row_washes(&e)[1], sel, "the hovered row is not the primary highlight");
        assert_eq!(row_washes(&e)[0], None, "an untouched row reacted");
        assert_eq!(row_faces(&e)[0], p.track, "and it is the list's ground");
    }

    #[test]
    fn a_hovered_row_is_quieter_than_a_selected_one_and_loses_to_it() {
        let p = Theme::default();
        let data = [(1u64, "a"), (2, "b")];
        // Row 1 selected, row 0 hovered: two different highlights, and they must not be the
        // same weight — two answers to "what happens if I act now" is one too many.
        let e: Element<u64> = list_view(
            &rows(&data),
            &[],
            &mut ListState::at(Some(1), 0),
            100,
            20,
            |k| k,
            None,
            None,
            Some(1),
            None,
            &p,
        );
        // Quieter: the hover's coverage against the selection's — 10% against 20% in the light
        // scheme, which is the whole difference in weight.
        let washes = row_washes(&e);
        assert_eq!(washes[0], Some((p.accent, p.scheme.hover_coverage())), "the hovered row did not react");
        assert_eq!(washes[1], Some((p.accent, libdraw::theme::SELECTION_COVERAGE)), "the selected row lost it");
        assert!(p.scheme.hover_coverage() < libdraw::theme::SELECTION_COVERAGE, "the hover is not quieter");

        // And hovering the *selected* row leaves it selected rather than downgrading it.
        let e: Element<u64> = list_view(
            &rows(&data),
            &[],
            &mut ListState::at(Some(1), 0),
            100,
            20,
            |k| k,
            None,
            None,
            Some(2),
            None,
            &p,
        );
        assert_eq!(
            row_washes(&e)[1],
            Some((p.accent, libdraw::theme::SELECTION_COVERAGE)),
            "selection lost to hover"
        );
    }

    /// The selected row paints differently, or selection is invisible.
    #[test]
    fn the_selected_row_is_painted_differently() {
        let data = [(1u64, "a"), (2, "b")];
        let p = Theme::default();
        let e: Element<u64> =
            list_view(&rows(&data), &[], &mut ListState::at(Some(1), 0), 100, 20, |k| k, None, None, None, None, &p);
        // **The ground and a wash over it** (desktop refresh, Part B): both rows rest on the
        // list's ground, and the selected one carries the accent at the selection's 20%. It was a
        // one-pixel accent border and the selection bevelled inside it (M11 Part E, batch 2); the
        // design is flat, so a gradient on either row is now the regression.
        let faces = row_faces(&e);
        assert_eq!(faces, [p.track, p.track], "a row is not on the list's own ground");
        let washes = row_washes(&e);
        assert_eq!(washes[1], Some((p.accent, libdraw::theme::SELECTION_COVERAGE)), "the selected row looks like the others");
        assert_eq!(washes[0], None, "an unselected row is washed too");
        assert_eq!(row_bevels(&e), [None, None], "a row is drawn with a gradient");
    }

    /// A scrollbar that is always there wastes width; one that never appears strands rows.
    #[test]
    fn the_scrollbar_appears_only_when_there_is_more_than_fits() {
        let p = Theme::default();
        let few = [(1u64, "a"), (2, "b")];
        let e: Element<u64> =
            list_view(&rows(&few), &[], &mut ListState::default(), 100, 20, |k| k, None, None, None, None, &p);
        assert!(!has_row_node(&e), "a list that fits drew a scrollbar");
        let many: alloc::vec::Vec<(u64, &str)> = (0..20u64).map(|i| (i, "x")).collect();
        let e: Element<u64> =
            list_view(&rows(&many), &[], &mut ListState::default(), 100, 20, |k| k, None, None, None, None, &p);
        assert!(has_row_node(&e), "a list that overflows drew no scrollbar");
    }

    /// Each row's activation message carries that row's key.
    #[test]
    fn a_rows_message_carries_its_own_key() {
        let data = [(11u64, "a"), (22, "b")];
        let e: Element<u64> =
            list_view(&rows(&data), &[], &mut ListState::default(), 100, 20, |k| k, None, None, None, None, &Theme::default());
        assert_eq!(presses(&e), alloc::vec![11, 22], "a row sent another row's message");
    }

    /// Zero row height must not divide by zero.
    #[test]
    fn a_degenerate_row_height_is_not_a_division() {
        let data = [(1u64, "a")];
        let e: Element<u64> =
            list_view(&rows(&data), &[], &mut ListState::default(), 100, 0, |k| k, None, None, None, None, &Theme::default());
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

    /// The wash each row carries, if any — `(colour, coverage)` — which is how a selected or a
    /// hovered row is drawn since the desktop refresh's Part B.
    fn row_washes<Msg>(e: &Element<Msg>) -> alloc::vec::Vec<Option<(Rgb, u8)>> {
        let mut out = alloc::vec::Vec::new();
        walk(e, &mut |n| {
            if n.key.is_none() {
                return;
            }
            let mut found = None;
            walk(n, &mut |c| {
                if found.is_none()
                    && let Node::Wash { colour, coverage } = &c.node
                {
                    found = Some((*colour, *coverage));
                }
            });
            out.push(found);
        });
        out
    }

    /// The bevelled fill each row carries, if any — the visible face of a selected row from M11
    /// Part E until the desktop refresh's Part B, and the negative a flat design has to keep.
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
                    // A button's layers are rounded fills since Part H; a square `Fill` is still
                    // matched so this reads whichever shape the widget is built from.
                    crate::element::Node::Fill(c)
                    | crate::element::Node::RoundedFill { colour: c, .. } => Some(*c),
                    crate::element::Node::Padding { child, .. } => match &child.node {
                        crate::element::Node::Fill(c)
                        | crate::element::Node::RoundedFill { colour: c, .. } => Some(*c),
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
        // **Down the left edge rather than at the corner**: the button is rounded to
        // `CONTROL_RADIUS` since Part H, so `(0, 0)` is the ground the curve was cut out of.
        assert_eq!(fb.get_pixel(0, 20), Some(p.accent), "the ring is on the edge");
        // **Inside the ring but away from the label**, which is centred since M15: the middle
        // of the button is where the word is, so a sample taken there is a glyph.
        assert_eq!(fb.get_pixel(6, 20), Some(p.face), "and the face is inside it");
    }

    /// A resting button has an edge, and it is not the focus ring.
    ///
    /// **What makes a shape read as a control** (M15). The face is eighteen units per channel
    /// from a white window, which is a difference nobody can see — the report from running it
    /// was that buttons "need different color background … so you know it's a button". An edge
    /// is what every desktop draws and what this toolkit had only around a focused control.
    #[test]
    fn a_resting_button_has_a_border_and_a_focused_one_keeps_the_ring() {
        let p = Theme::default();
        let t = Theme::default();
        let mut fb = MemFramebuffer::new(Geometry::packed(80, 40, PixelFormat::XRGB8888));
        let e: Element<Msg> = button("OK", (), WidgetState::default(), &p);
        let l = layout(&e, Rect::new(0, 0, 80, 40), &CELL);
        paint(&mut fb, &font(), &t, &e, &l, Rect::new(0, 0, 80, 40), &mut |_, _, _, _: &mut MemFramebuffer| {});
        assert_eq!(fb.get_pixel(0, 20), Some(p.border), "a resting button has no edge at all");
        assert_ne!(p.border, p.face, "…and the edge is not the face");
        assert_eq!(fb.get_pixel(6, 20), Some(p.face), "the face is inside the edge");
    }

    /// A resting field has an edge, and on a white card that edge is the only thing that says
    /// there is a field there at all.
    ///
    /// **The greeter is what found this** (desktop refresh, Part D): a field was a flat fill of
    /// `track`, `track` is the same white as a window's own ground in the light scheme, and so a
    /// password box at rest was invisible. Painted rather than inspected, because the ground is
    /// drawn over the edge if the layers are the wrong way round and the tree still looks right.
    #[test]
    fn a_resting_field_has_an_edge_against_the_ground_it_sits_on() {
        let p = Theme::default();
        let mut fb = MemFramebuffer::new(Geometry::packed(80, 40, PixelFormat::XRGB8888));
        fb.clear(p.background);
        let state = TextFieldState::new();
        let e: Element<Msg> = text_field(&state, false, WidgetState::default(), &p);
        let l = layout(&e, Rect::new(0, 0, 80, 40), &CELL);
        let all = Rect::new(0, 0, 80, 40);
        paint(&mut fb, &font(), &p, &e, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
        // Down the left edge, clear of the rounded corners.
        let mid = l.rect.size.h / 2;
        assert_eq!(fb.get_pixel(0, mid), Some(p.border), "a resting field has an edge");
        assert_ne!(p.border, p.track, "…and it is not the colour of the well inside it");
        assert_ne!(p.border, p.background, "…nor of the surface it sits on");
        assert_eq!(fb.get_pixel(4, mid), Some(p.track), "the well is inside the edge");
        // Focused, the edge is the accent instead — a state, not a second kind of edge.
        let mut lit = MemFramebuffer::new(Geometry::packed(80, 40, PixelFormat::XRGB8888));
        lit.clear(p.background);
        let f: Element<Msg> =
            text_field(&state, false, WidgetState { active: true, ..Default::default() }, &p);
        let fl = layout(&f, Rect::new(0, 0, 80, 40), &CELL);
        paint(&mut lit, &font(), &p, &f, &fl, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
        assert_eq!(lit.get_pixel(0, mid), Some(p.accent), "a focused field rings in the accent");
    }

    /// The label is in the middle of the button, not against its corner.
    ///
    /// **Measured as a distance from each edge**, because the exact pixels a glyph lands on are
    /// the font's business: what this asserts is that the ink is not hard against the left,
    /// which is where `padding` alone put it — a button 80 wide with a word 20 wide had 58
    /// pixels of face to the right of its label (M15).
    #[test]
    fn a_buttons_label_is_centred_in_its_face() {
        let p = Theme::default();
        let mut fb = MemFramebuffer::new(Geometry::packed(80, 40, PixelFormat::XRGB8888));
        let e: Element<Msg> = button("OK", (), WidgetState::default(), &p);
        let l = layout(&e, Rect::new(0, 0, 80, 40), &CELL);
        paint(&mut fb, &font(), &p, &e, &l, Rect::new(0, 0, 80, 40), &mut |_, _, _, _: &mut MemFramebuffer| {});
        let inked: Vec<u32> = (0..80)
            .filter(|x| {
                (0..40).any(|y| {
                    let c = fb.get_pixel(*x, y);
                    c != Some(p.face) && c != Some(p.border)
                })
            })
            .collect();
        let (first, last) = (inked[0], inked[inked.len() - 1]);
        let (left, right) = (first, 79 - last);
        assert!(
            left.abs_diff(right) <= 2,
            "the label sits {left} from the left and {right} from the right"
        );
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
        // sized -> dock -> [the rule, stack] ; stack -> [face fill, row] ; row -> the buttons.
        // (One level deeper since the desktop refresh's Part B put a rule under the bar.)
        let row = &l.children[0].children[1].children[1];
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
            None,
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
        let mut tree = crate::diff::Tree::new();
        tree.update(&e, &l).expect("a clean frame");
        // **Asked of the router, not read off the layout tree**, which this used to walk by child
        // index and which changed shape when the bar gained a rule and its buttons an ink
        // (desktop refresh, Part B). Where a press *lands* is the property; the tree's shape was
        // only ever a way of guessing it.
        let click = |x: i32, y: i32| {
            let at = |pressed: bool| librsproto::surface::PointerEvent {
                kind: librsproto::surface::POINTER_BUTTON,
                button: 0x110,
                buttons: u16::from(pressed),
                flags: if pressed { librsproto::surface::POINTER_PRESSED } else { 0 },
                x,
                y,
                ..Default::default()
            };
            let mut r = crate::route::Router::new();
            let mut got = r.pointer(&tree, &e, &l, at(true)).0;
            got.extend(r.pointer(&tree, &e, &l, at(false)).0);
            got
        };
        assert_eq!(l.rect.size.h, TITLE_BAR_H, "the bar is its height");
        // The buttons sit at the right edge, `TITLE_BUTTON_PAD` in, each answering across its
        // whole column and not a pixel beyond it.
        let right = (400 - TITLE_BUTTON_PAD) as i32;
        let step = (TITLE_BUTTON_W + TITLE_BUTTON_GAP) as i32;
        for (nth, want) in [(0, M::Close), (1, M::Max), (2, M::Min)] {
            let end = right - nth * step;
            let start = end - TITLE_BUTTON_W as i32;
            assert_eq!(click(start, 12), vec![want.clone()], "{want:?}'s first column");
            assert_eq!(click(end - 1, 12), vec![want.clone()], "{want:?}'s last column");
            assert_eq!(click(end, 12), vec![M::Drag], "just past {want:?} is the bar");
        }
        // And the face takes the rest: the title's end of the bar, the gaps, the padding.
        assert_eq!(click(1, 12), vec![M::Drag], "the title starts at the left and drags");
        assert_eq!(click(399, 12), vec![M::Drag], "the padding after close drags");
    }

    // ---- the text area (M10 Part C) ----

    /// `abc` / `de` / `fghi`, cursor at the start.
    fn area() -> TextAreaState {
        TextAreaState::with_text("abc\nde\nfghi")
    }

    /// Type `text` into `a`, one character at a time, the way a person does.
    fn type_text(a: &mut TextAreaState, text: &str) {
        for c in text.chars() {
            if c == '\n' {
                a.newline();
            } else {
                a.insert(c);
            }
        }
    }

    // ---- paste and its range (M12 Part E) ----

    #[test]
    fn insert_text_returns_the_range_it_occupies() {
        // **The return value is what makes cycling possible at all.** M12 decision 3 says a
        // cycle *replaces what was just inserted*, so the caller has to be told where that is —
        // and deriving it at the call site from the cursor and the text's shape would be the
        // same arithmetic done somewhere with less to check it against.
        let mut a = area();
        a.apply(KEY_RIGHT, 0); // between `a` and `bc`
        let (from, to) = a.insert_text("XY");
        assert_eq!(a.text(), "aXYbc\nde\nfghi");
        assert_eq!(from, (0, 1));
        assert_eq!(to, (0, 3), "one line, so the range is columns 1..3");

        // …and a multi-line paste ends on the line it made, not the one it started on.
        let mut a = area();
        let (from, to) = a.insert_text("one\ntwo");
        assert_eq!(from, (0, 0));
        assert_eq!(to, (1, 3));
        assert_eq!(a.text(), "one\ntwoabc\nde\nfghi", "and the tail followed it down");
    }

    #[test]
    fn select_range_then_insert_text_is_the_cycle() {
        // The two halves together are what `nxedit::App::cycled` does. Doing it here as well
        // pins the *primitives*: `nxedit`'s own test would pass against a `select_range` that
        // selected nothing, because `insert_text` inserts at the cursor either way.
        let mut a = area();
        let (from, to) = a.insert_text("FIRST");
        assert_eq!(a.text(), "FIRSTabc\nde\nfghi");
        a.select_range(from, to);
        assert_eq!(a.selected_text().as_deref(), Some("FIRST"), "the range names what went in");
        a.insert_text("SECOND");
        assert_eq!(a.text(), "SECONDabc\nde\nfghi", "which the next paste replaced");
    }

    #[test]
    fn select_range_clamps_a_stale_range_rather_than_panicking() {
        // **The guard is documented and was not covered** (PR #271 review, optional 7). A range
        // is only valid until the next edit, and a cycle that arrived after one would otherwise
        // index out of the buffer — in an editor holding somebody's unsaved work. Clamping
        // makes a stale range select something harmless instead.
        let mut a = area();
        a.select_range((99, 99), (99, 99));
        assert_eq!(a.selected_text(), None, "collapsed onto the end of the last line");
        assert_eq!(a.cursor(), (2, 4), "and the cursor is inside the buffer");
        assert_eq!(a.text(), "abc\nde\nfghi", "nothing was changed by asking");
    }

    #[test]
    fn select_range_lands_on_a_character_boundary() {
        // Columns are byte offsets, and a stale one can point into the middle of a multi-byte
        // character — where every `String` operation below panics. Clamping walks back to the
        // boundary rather than trusting the number.
        let mut a = TextAreaState::with_text("aéb");
        // `é` is two bytes, so 2 is inside it.
        a.select_range((0, 0), (0, 2));
        assert_eq!(a.selected_text().as_deref(), Some("a"));
    }

    // ---- undo and redo (M12 Part C) ----

    #[test]
    fn a_word_is_one_undo_step_and_so_is_a_line() {
        // **The grouping is the decision, not the stack.** Per keystroke is unusable — undoing a
        // sentence becomes forty presses — and per save is useless. A run of printable characters
        // is one group, a separator ends it, and `Enter` ends it.
        let mut a = TextAreaState::new();
        type_text(&mut a, "hello world");
        assert!(a.undo(), "there is something to undo");
        assert_eq!(a.text(), "hello ", "the last word, and not the last letter");
        assert!(a.undo());
        assert_eq!(a.text(), "", "and the word before it, with its separator");
        assert!(!a.undo(), "and then there is nothing");
    }

    #[test]
    fn a_line_ends_a_group() {
        // **`Enter` joins the group it lands in and closes it**, so the line you just typed —
        // its text *and* its break — comes back in one step, and what you type on the next line
        // is a step of its own. Splitting the break out would make undoing a line take two
        // presses that look identical.
        let mut a = TextAreaState::new();
        type_text(&mut a, "one\ntwo");
        assert!(a.undo());
        assert_eq!(a.text(), "one\n", "the second line's word");
        assert!(a.undo());
        assert_eq!(a.text(), "", "then the first line and its break, together");
        assert!(!a.undo());

        // And a break typed after a movement is its own group, because the movement closed the
        // one before it.
        let mut a = TextAreaState::with_text("ab");
        a.end(false);
        a.newline();
        a.insert('c');
        assert!(a.undo());
        assert_eq!(a.text(), "ab\n");
        assert!(a.undo());
        assert_eq!(a.text(), "ab");
    }

    #[test]
    fn moving_ends_a_group_so_two_edits_undo_separately() {
        // **Wherever it lands.** Without this, typing at one end of a line, moving to the other
        // and typing again undoes both at once — two pieces of text a person put in two places.
        let mut a = TextAreaState::with_text("ab");
        a.end(false);
        a.insert('X');
        a.home(false);
        a.insert('Y');
        assert_eq!(a.text(), "YabX");
        assert!(a.undo());
        assert_eq!(a.text(), "abX", "only the edit after the move");
        assert!(a.undo());
        assert_eq!(a.text(), "ab");
    }

    #[test]
    fn deleting_is_its_own_kind_of_group() {
        // A run of deletions coalesces, and typing after them starts a new group rather than
        // extending the one that was removing.
        let mut a = TextAreaState::with_text("abcdef");
        a.end(false);
        a.backspace();
        a.backspace();
        assert_eq!(a.text(), "abcd");
        a.insert('Z');
        assert_eq!(a.text(), "abcdZ");
        assert!(a.undo());
        assert_eq!(a.text(), "abcd", "the typing, on its own");
        assert!(a.undo());
        assert_eq!(a.text(), "abcdef", "then both deletions together");
    }

    #[test]
    fn an_edit_that_does_nothing_is_not_a_step() {
        // A `Backspace` at the start of the buffer and a `Delete` at its end change nothing, and
        // a snapshot for either is an undo press that visibly does nothing — which reads as a
        // broken undo rather than an empty history.
        let mut a = TextAreaState::with_text("ab");
        assert!(!a.backspace(), "nothing before the cursor");
        assert!(!a.undo(), "and nothing to undo");
        a.end(false);
        a.down(false);
        a.end(false);
        assert!(!a.delete(), "nothing after it");
        assert!(!a.undo());
    }

    #[test]
    fn placing_the_cursor_ends_a_group() {
        // **The fifth boundary, and the one with no test until PR #269's review said so.** A
        // click is a movement and closes a group for the same reason an arrow key does — and it
        // is forward-looking here: nothing outside this crate's own tests wires a press to
        // `place` yet, so the editor cannot reach it. That is a reason for a test rather than
        // against one: the day `text_area` grows click-to-place, this rule has to already hold.
        let mut a = TextAreaState::with_text("ab");
        a.end(false);
        a.insert('X');
        a.place(0, 0);
        a.insert('Y');
        assert_eq!(a.text(), "YabX");
        assert!(a.undo());
        assert_eq!(a.text(), "abX", "only the edit after the click");
        assert!(a.undo());
        assert_eq!(a.text(), "ab");
    }

    #[test]
    fn a_boundary_the_buffer_cannot_see_can_still_be_drawn() {
        // A save is the boundary this exists for: what a person wants back afterwards is what
        // they have typed *since*, and without an explicit end the whole session is one group.
        let mut a = TextAreaState::new();
        type_text(&mut a, "first");
        a.end_group();
        type_text(&mut a, "second");
        assert!(a.undo());
        assert_eq!(a.text(), "first", "only what came after the boundary");
        assert!(a.undo());
        assert_eq!(a.text(), "");
    }

    #[test]
    fn redo_comes_forward_and_a_new_edit_abandons_it() {
        let mut a = TextAreaState::new();
        type_text(&mut a, "one two");
        a.undo();
        assert_eq!(a.text(), "one ");
        assert!(a.redo(), "and forward again");
        assert_eq!(a.text(), "one two");

        // **Typing after an undo is a branch**, so what was undone is not coming back.
        a.undo();
        assert_eq!(a.text(), "one ");
        a.insert('X');
        assert!(!a.redo(), "the way forward is gone");
        assert_eq!(a.text(), "one X");
    }

    #[test]
    fn the_history_is_bounded_and_drops_the_oldest() {
        // Each step is a copy of the whole buffer, so the depth bound is the memory bound.
        let mut a = TextAreaState::new();
        for i in 0..MAX_UNDO + 10 {
            a.insert(char::from(b'a' + (i % 26) as u8));
            // A separator between each, so every character is its own group.
            a.insert(' ');
        }
        let mut steps = 0;
        while a.undo() {
            steps += 1;
        }
        assert_eq!(steps, MAX_UNDO, "the oldest go, and the bound holds");
        assert_ne!(a.text(), "", "so the very beginning is not reachable, which the cap means");
    }

    #[test]
    fn undoing_marks_the_buffer_changed() {
        // `revision` is what an editor derives "modified" from, and undo has to move it or a
        // buffer taken back to something else would read as matching the file.
        let mut a = TextAreaState::new();
        a.insert('x');
        let after_typing = a.revision();
        a.undo();
        assert_ne!(a.revision(), after_typing);
    }

    // ---- find (M12 Part C) ----

    #[test]
    fn find_walks_forward_through_every_match_and_wraps() {
        let mut a = TextAreaState::with_text("one two
three two
two");
        assert!(a.find("two"));
        assert_eq!(a.cursor(), (0, 4), "the first, after the cursor");
        assert!(a.find("two"));
        assert_eq!(a.cursor(), (1, 6), "the next");
        assert!(a.find("two"));
        assert_eq!(a.cursor(), (2, 0));
        assert!(a.find("two"), "and round again");
        assert_eq!(a.cursor(), (0, 4));
    }

    #[test]
    fn find_selects_what_it_found() {
        // A cursor sitting silently at a hit leaves the person to spot it; a highlight says which
        // of several this one is.
        let mut a = TextAreaState::with_text("alpha beta");
        assert!(a.find("beta"));
        assert_eq!(a.selected_text().as_deref(), Some("beta"));
    }

    #[test]
    fn find_answers_no_for_what_is_not_there_and_for_nothing() {
        let mut a = TextAreaState::with_text("alpha");
        assert!(!a.find("omega"), "absent");
        assert_eq!(a.cursor(), (0, 0), "and the cursor did not move");
        // An empty needle is what the field holds before anything is typed; matching everything
        // would move the buffer under the person on the way to their first character.
        assert!(!a.find(""));
        assert_eq!(a.cursor(), (0, 0));
    }

    #[test]
    fn find_does_not_split_a_multi_byte_character() {
        // The search starts one byte past the cursor, and that byte can be inside a character.
        let mut a = TextAreaState::with_text("é and é");
        assert!(a.find("é"), "the second one");
        assert_eq!(a.cursor(), (0, 7));
        assert!(a.find("é"), "and back to the first");
        assert_eq!(a.cursor(), (0, 0));
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
        let _: Element<()> = text_area(&mut a, 3 * 16, 16, true, &[], None, &p);
        assert_eq!(a.offset(), 0);

        for _ in 0..5 {
            a.apply(KEY_DOWN, 0);
        }
        let _: Element<()> = text_area(&mut a, 3 * 16, 16, true, &[], None, &p);
        assert_eq!(a.offset(), 3, "line 5 is visible in a three-line window");

        for _ in 0..5 {
            a.apply(KEY_UP, 0);
        }
        let _: Element<()> = text_area(&mut a, 3 * 16, 16, true, &[], None, &p);
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
            let e: Element<()> = text_area(a, 3 * 16, 16, true, &[], None, &p);
            fills(&e, p.accent)
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
        let e: Element<()> = text_area(&mut a, 3 * 16, 16, false, &[], None, &p);
        assert_eq!(fills(&e, p.accent), 0, "and none at all when the widget is not active");
    }

    /// Every `(text, ink)` pair in a tree, in order — an ink node's text, or `None` for plain.
    fn inked<M>(e: &Element<M>) -> Vec<(String, Option<Rgb>)> {
        fn walk<M>(e: &Element<M>, under: Option<Rgb>, out: &mut Vec<(String, Option<Rgb>)>) {
            match &e.node {
                crate::element::Node::Text(t) if !t.is_empty() => {
                    out.push((t.clone(), under));
                }
                crate::element::Node::Ink { colour, child } => walk(child, Some(*colour), out),
                _ => {
                    for c in e.children() {
                        walk(c, under, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(e, None, &mut out);
        out
    }

    const KEYWORD: Rgb = Rgb::new(0x11, 0x22, 0x33);

    /// A run splits its line and colours exactly its own bytes.
    #[test]
    fn an_ink_run_splits_a_line_and_colours_only_itself() {
        let p = Theme::default();
        let mut a = TextAreaState::with_text("let x = 1");
        let e: Element<()> = text_area(
            &mut a,
            16,
            16,
            false,
            &[InkRun { line: 0, start: 0, end: 3, colour: KEYWORD }],
            None,
            &p,
        );
        assert_eq!(
            inked(&e),
            alloc::vec![
                (String::from("let"), Some(KEYWORD)),
                (String::from(" x = 1"), None),
            ],
            "the line was not split at the run's edge"
        );
    }

    /// A pixel becomes a line and a column, and the column is the nearest boundary.
    ///
    /// **Nearest, not the character under the cursor**: clicking the right half of a letter puts
    /// the caret after it, which is what every editor does and what makes clicking past the end
    /// of a word land after the word rather than inside it.
    #[test]
    fn a_point_becomes_the_nearest_line_and_column() {
        // Eight pixels a character, which is what the fixed metric these tests use gives.
        let w = |s: &str| (s.chars().count() * 8) as u32;
        let a = TextAreaState::with_text("abcdef\nghi");
        let px = |col: usize| FIELD_PAD.left as i32 + col as i32 * 8;
        let py = |row: usize| FIELD_PAD.top as i32 + row as i32 * 16 + 4;

        assert_eq!(a.at_point(px(0), py(0), 16, w), (0, 0));
        assert_eq!(a.at_point(px(3), py(0), 16, w), (0, 3), "a boundary is itself");
        assert_eq!(a.at_point(px(3) + 6, py(0), 16, w), (0, 4), "past the middle is the next");
        assert_eq!(a.at_point(px(3) + 2, py(0), 16, w), (0, 3), "before it is this one");
        assert_eq!(a.at_point(px(0), py(1), 16, w), (1, 0), "the second row is the second line");

        // **Off the ends is clamped, in both directions.** A drag runs off a widget routinely —
        // the router hands a captured widget negative coordinates rather than clamping them.
        assert_eq!(a.at_point(-40, py(0), 16, w), (0, 0));
        assert_eq!(a.at_point(px(99), py(0), 16, w), (0, 6), "past the end of the line is its end");
        assert_eq!(a.at_point(px(0), py(9), 16, w), (1, 0), "past the last line is the last line");
        assert_eq!(a.at_point(px(0), -80, 16, w), (0, 0));
    }

    /// A scrolled area maps a point to the line that is *on screen* there.
    ///
    /// **The offset is the whole of it**, and leaving it out is the bug that makes clicking work
    /// perfectly until the first time somebody scrolls — after which every click lands the same
    /// number of lines too high.
    #[test]
    fn a_point_is_read_against_what_is_on_screen() {
        let w = |s: &str| (s.chars().count() * 8) as u32;
        let mut a = TextAreaState::with_text("0\n1\n2\n3\n4\n5\n6\n7\n8\n9");
        a.scroll_to(4, 3);
        assert_eq!(a.offset(), 4, "precondition: scrolled");
        let y = FIELD_PAD.top as i32 + 4;
        assert_eq!(a.at_point(FIELD_PAD.left as i32, y, 16, w).0, 4, "the top row is line 4");
    }

    /// The scrollbar's state, and a scroll that leaves the cursor where it was.
    #[test]
    fn a_text_area_reports_a_bar_and_scrolls_without_moving_the_cursor() {
        let mut a = TextAreaState::with_text("0\n1\n2\n3\n4\n5\n6\n7\n8\n9");
        let bar = a.bar(4);
        assert_eq!((bar.offset, bar.visible, bar.total), (0, 4, 10));
        assert!(bar.scrollable());

        let before = a.cursor();
        a.scroll_to(3, 4);
        assert_eq!(a.offset(), 3);
        assert_eq!(a.cursor(), before, "a scrollbar drag moved the caret");
        // Clamped to what is left, so a drag past the end shows the last screen rather than
        // scrolling into blank space.
        a.scroll_to(99, 4);
        assert_eq!(a.offset(), 6, "ten lines, four visible");
    }

    /// The caret is drawn where the cursor is in the middle of a line.
    ///
    /// **The commonest caret position there is, and the one the rewrite newly depends on `cuts`
    /// for.** Every other test puts it at column 0, at the line's end, or at an end of a
    /// selection — each of which is already a cut for another reason, so deleting the caret's
    /// own cut left all 240 tests green while a person typing mid-line saw no caret at all
    /// (PR #289 review, 2).
    #[test]
    fn the_caret_is_drawn_in_the_middle_of_a_line() {
        let p = Theme::default();
        let mut a = TextAreaState::with_text("abcdef");
        for _ in 0..2 {
            a.apply(KEY_RIGHT, 0);
        }
        assert_eq!(a.cursor(), (0, 2), "precondition: mid-line, with no selection");
        let e: Element<()> = text_area(&mut a, 16, 16, true, &[], None, &p);
        assert_eq!(fills(&e, p.accent), 1, "no caret while typing in the middle of a line");
    }

    /// A coloured run under a selection keeps its colour.
    ///
    /// **The reason ink is a wrapper rather than a property of the text node.** The selection is
    /// a `fill` beneath the glyphs, so a selected keyword is a stack of a fill and an inked text
    /// — and a version that dropped the ink when the piece was selected would make selecting a
    /// line turn all of it black, which is the moment a person is most likely to be reading it.
    #[test]
    fn a_selected_run_keeps_its_ink() {
        let p = Theme::default();
        let mut a = TextAreaState::with_text("let x = 1");
        for _ in 0..3 {
            a.apply(KEY_RIGHT, MOD_SHIFT);
        }
        let e: Element<()> = text_area(
            &mut a,
            16,
            16,
            true,
            &[InkRun { line: 0, start: 0, end: 3, colour: KEYWORD }],
            None,
            &p,
        );
        assert_eq!(fills(&e, p.selection()), 1, "precondition: the keyword is selected");
        assert!(
            inked(&e).contains(&(String::from("let"), Some(KEYWORD))),
            "the selection took the colour with it: {:?}",
            inked(&e)
        );
    }

    /// A stale run past the end of a line is clamped, not indexed with.
    ///
    /// **Reachable on every keystroke.** The runs are scanned from the buffer as it was a moment
    /// ago, so deleting the end of a line leaves one naming bytes that are gone — and a run whose
    /// bounds landed inside a multi-byte character would panic on the slice. A wrong colour for
    /// one frame is the right failure; a crashed editor is not.
    #[test]
    fn a_stale_or_misaligned_run_neither_panics_nor_colours_past_the_end() {
        let p = Theme::default();
        let mut a = TextAreaState::with_text("ab");
        let e: Element<()> = text_area(
            &mut a,
            16,
            16,
            false,
            &[InkRun { line: 0, start: 1, end: 99, colour: KEYWORD }],
            None,
            &p,
        );
        assert_eq!(
            inked(&e),
            alloc::vec![(String::from("a"), None), (String::from("b"), Some(KEYWORD))],
            "a run past the end was not clamped to it"
        );

        // A boundary inside a character is dropped rather than split on.
        let mut a = TextAreaState::with_text("\u{4e2d}\u{6587}");
        let e: Element<()> = text_area(
            &mut a,
            16,
            16,
            false,
            &[InkRun { line: 0, start: 1, end: 2, colour: KEYWORD }],
            None,
            &p,
        );
        assert_eq!(inked(&e).len(), 1, "the line was split inside a character: {:?}", inked(&e));

        // And a run naming a line that is not on screen colours nothing.
        let mut a = TextAreaState::with_text("only");
        let e: Element<()> = text_area(
            &mut a,
            16,
            16,
            false,
            &[InkRun { line: 40, start: 0, end: 2, colour: KEYWORD }],
            None,
            &p,
        );
        assert!(inked(&e).iter().all(|(_, c)| c.is_none()));
    }

    #[test]
    fn a_selection_is_highlighted_on_every_line_it_covers() {
        // The other half of what `text_area` draws, and the reason the count is per *line*: a
        // multi-line selection is one highlight per row, not one rectangle.
        let p = Theme::default();
        let mut a = area();
        a.apply(KEY_RIGHT, 0);
        a.apply(KEY_DOWN, MOD_SHIFT);
        let e: Element<()> = text_area(&mut a, 3 * 16, 16, true, &[], None, &p);
        assert_eq!(fills(&e, p.selection()), 2, "the tail of line 0 and the head of line 1");

        let mut a = area();
        let e: Element<()> = text_area(&mut a, 3 * 16, 16, true, &[], None, &p);
        assert_eq!(fills(&e, p.selection()), 0, "and nothing when nothing is selected");
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
        let e: Element<()> = text_area(&mut a, 3 * 16, 16, true, &[], None, &Theme::default());
        assert_eq!(fills(&e, Theme::default().selection()), 0, "nothing is selected, so nothing \
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
        let _: Element<()> = text_area(&mut a, 3 * 16, 16, true, &[], None, &Theme::default());
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
            None,
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

        // The buttons' centres in a bare 400-wide bar, from the constants — the close button ends
        // `TITLE_BUTTON_PAD` in from the right, each is `TITLE_BUTTON_W` wide, `TITLE_BUTTON_GAP`
        // apart. These were 390 and 364 in contiguous 26-pixel slots until the desktop refresh's
        // Part B, and 364 now falls in a gap, which drags.
        let step = (TITLE_BUTTON_W + TITLE_BUTTON_GAP) as i32;
        let close_x = (400 - TITLE_BUTTON_PAD - TITLE_BUTTON_W / 2 - 1) as i32;
        let max_x = close_x - step;

        let mut r = Router::new();
        assert_eq!(down(&mut r, 200), vec![M::Drag], "the bar moves the window on the press…");
        assert_eq!(up(&mut r, 200), vec![], "…and the release adds nothing");

        let mut r = Router::new();
        assert_eq!(
            down(&mut r, close_x),
            vec![],
            "a press on close must not also drag: the window would move under the pointer while \
             the user is aiming at a button"
        );
        assert_eq!(up(&mut r, close_x), vec![M::Close], "and the click is the close");

        let mut r = Router::new();
        assert_eq!(down(&mut r, max_x), vec![], "the same for maximise");
        assert_eq!(up(&mut r, max_x), vec![M::Max]);

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

    /// Every `Text` node in a tree, joined — enough to say what a widget shows without pinning
    /// where in its tree it sits.
    fn all_text<M>(e: &Element<M>) -> String {
        fn walk<M>(e: &Element<M>, out: &mut String) {
            if let crate::element::Node::Text(t) = &e.node {
                out.push_str(t);
            }
            for c in e.children() {
                walk(c, out);
            }
        }
        let mut out = String::new();
        walk(e, &mut out);
        out
    }

    #[test]
    fn a_tab_selects_where_it_is_and_its_close_box_does_not_select_it() {
        // **The published metrics, and the shadowing rule between them.** `check-login` presses
        // a tab at `TAB_SIDE + i * TAB_PITCH + something` and its close box at `TAB_CLOSE_CX`
        // from its own copies, because a gate that aimed from the toolkit could agree with a
        // toolkit that had stopped drawing where it says (M11 decision 2) — so the numbers are
        // asserted here against a tree that is actually built, the way the dialog's aim points
        // are, and `xtask`'s `the_gates_chrome_table_is_the_toolkits` compares the two sets.
        assert_eq!((TAB_W, TAB_STRIP_H, TAB_H, TAB_CLOSE_W), (120, 30, 24, 20));
        assert_eq!((TAB_SIDE, TAB_TOP, TAB_GAP, TAB_PITCH), (6, 5, 1, 121));
        assert_eq!(TAB_CLOSE_CX, 110);

        #[derive(Clone, PartialEq, Eq, Debug)]
        enum M {
            Select(u64),
            Close(u64),
            New,
        }
        let theme = Theme::default();
        let tabs = [
            Tab { key: 7, label: "one", marked: false },
            Tab { key: 9, label: "two", marked: true },
        ];
        let ui: Element<M> =
            tab_strip(&tabs, 7, None, M::Select, M::Close, TabExtras::new_tab(M::New), &theme);

        let cell = crate::layout::FixedCell { w: 8, h: 16 };
        let l = crate::layout::layout(&ui, Rect::new(0, 0, 400, TAB_STRIP_H), &cell);
        let mut tree = crate::diff::Tree::new();
        tree.update(&ui, &l).expect("a tab strip is diffable");
        let mut router = crate::route::Router::new();
        let click = |r: &mut crate::route::Router, x: i32| {
            let at = |flags: u16, buttons: u16| PointerEvent {
                kind: librsproto::surface::POINTER_BUTTON,
                button: 0x110,
                buttons,
                flags,
                x,
                y: (TAB_STRIP_H / 2) as i32,
                ..Default::default()
            };
            r.pointer(&tree, &ui, &l, at(librsproto::surface::POINTER_PRESSED, 1));
            r.pointer(&tree, &ui, &l, at(0, 0)).0
        };

        // **Tab `i` begins at `TAB_SIDE + i * TAB_PITCH`** since the refresh's Part H: the strip
        // has padding at its left end and a pixel between tabs, where it used to start hard
        // against the content's edge with none.
        let tab_x = |i: i32| TAB_SIDE as i32 + i * TAB_PITCH as i32;
        // The second tab's label area selects it, by **key** and not by position.
        assert_eq!(click(&mut router, tab_x(1) + 20), alloc::vec![M::Select(9)]);
        // Its close box closes it and does *not* also select it: a nearer `on_press` shadows the
        // one on the tab, which is the same rule that lets a title bar carry buttons.
        assert_eq!(click(&mut router, tab_x(1) + TAB_CLOSE_CX), alloc::vec![M::Close(9)]);
        // And the first tab is still where it was, which is what a fixed width buys.
        assert_eq!(click(&mut router, tab_x(0) + 20), alloc::vec![M::Select(7)]);
        // The `+` is after the last tab, and opens one rather than selecting anything.
        assert_eq!(click(&mut router, tab_x(2) + (NEW_TAB_W / 2) as i32), alloc::vec![M::New]);
    }

    /// How far off an exact ink a glyph's darkest pixel may land. A stem covers a pixel almost
    /// but not quite completely, so even body ink comes out a few units light.
    const ANTIALIAS_SLACK: u32 = 12;

    /// A focused pill shows a ring, and its label reads in either scheme.
    ///
    /// **Two failures in one control** (PR #319 review, optionals 4 and 9). The ring was the
    /// ground drawn over the ground — nought pixels of the 2400 differed between an active pill
    /// and a resting one — and the label was the window's paper, which in the dark scheme is
    /// near-black on the accent. Painted, and compared *between states* rather than against a
    /// colour, because "a ring is visible" is exactly the claim a same-colour ring passes.
    #[test]
    fn a_pill_rings_when_focused_and_its_label_reads_in_either_scheme() {
        let (w, h) = (80, 30);
        let draw = |t: &Theme, state: WidgetState| {
            let e: Element<Msg> = pill("Save", (), state, t);
            let all = Rect::new(0, 0, w, h);
            let l = layout(&e, all, &CELL);
            let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
            fb.clear(t.face);
            paint(&mut fb, &font(), t, &e, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
            fb
        };
        let t = Theme::default();
        let resting = draw(&t, WidgetState::default());
        let active = draw(&t, WidgetState { active: true, ..Default::default() });
        let differing = (0..w)
            .flat_map(|x| (0..h).map(move |y| (x, y)))
            .filter(|(x, y)| resting.get_pixel(*x, *y) != active.get_pixel(*x, *y))
            .count();
        assert!(differing > 40, "a focused pill looks no different: {differing} pixels");
        // **The label is the better-contrasting of the surface's two inks, in each scheme** —
        // asserted as the *choice* rather than as a distance, because a distance passes for the
        // wrong ink too: the dark scheme's near-black paper is a long way from the accent by any
        // arithmetic and still unreadable on it (review, optional 9). In the light scheme that
        // is the paper, in the dark scheme the text colour.
        let brightness = |c: Rgb| 299 * c.r as i32 + 587 * c.g as i32 + 114 * c.b as i32;
        for t in [Theme::light(), Theme::dark()] {
            let fb = draw(&t, WidgetState::default());
            let ground = brightness(t.accent);
            let (paper, text_ink) = (brightness(t.background), brightness(t.foreground));
            let wanted =
                if (paper - ground).abs() >= (text_ink - ground).abs() { paper } else { text_ink };
            // The label's extreme pixel on the middle row is the ink it is drawn in.
            let row: alloc::vec::Vec<i32> =
                (0..w).filter_map(|x| fb.get_pixel(x, h / 2)).map(brightness).collect();
            let reached = if wanted > ground {
                *row.iter().max().expect("pixels")
            } else {
                *row.iter().min().expect("pixels")
            };
            let got = (reached - wanted).abs();
            let other = (reached - if wanted == paper { text_ink } else { paper }).abs();
            assert!(
                got < other,
                "the label is not the better-contrasting ink: reached {reached}, wanted \
                 {wanted}, the other is {}",
                if wanted == paper { text_ink } else { paper }
            );
        }
    }

    /// What a tab strip *looks* like: the current tab is the window's ground and covers the rule,
    /// the others have no face and their labels are dim, and a keyed right slot is drawable.
    ///
    /// **The description's central claim, and nothing tested it** (PR #319 review, worth fixing
    /// 5): every one of these could be broken with all 80 of this module's tests still passing,
    /// which is the same "present and invisible" lesson as the close box, one level up.
    #[test]
    fn a_tab_strip_draws_the_current_tab_as_the_windows_own_ground() {
        #[derive(Clone, PartialEq, Eq, Debug)]
        enum M {
            Select(u64),
            Close(u64),
        }
        let t = Theme::default();
        let (w, h) = (300, TAB_STRIP_H);
        let tabs = [
            Tab { key: 1, label: "one", marked: false },
            Tab { key: 2, label: "two", marked: false },
        ];
        let paint_strip = |extras: TabExtras<M>, hovered: Option<u64>| {
            let ui: Element<M> = tab_strip(&tabs, 1, hovered, M::Select, M::Close, extras, &t);
            let all = Rect::new(0, 0, w, h);
            let l = layout(&ui, all, &CELL);
            let mut tree = crate::diff::Tree::new();
            let diffable = tree.update(&ui, &l).is_ok();
            let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
            fb.clear(t.background);
            paint(&mut fb, &font(), &t, &ui, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
            (fb, diffable)
        };
        let (fb, _) = paint_strip(TabExtras::none(), None);
        // Down the middle of each tab, clear of its label and its close box.
        let (first, second) = (TAB_SIDE + 4, TAB_SIDE + TAB_PITCH + 4);
        assert_eq!(
            fb.get_pixel(first, TAB_TOP + TAB_H / 2),
            Some(t.background),
            "the current tab is the window's own ground"
        );
        assert_eq!(
            fb.get_pixel(first, TAB_STRIP_H - 1),
            Some(t.background),
            "…and it covers the rule, so it runs into the content below"
        );
        assert_eq!(
            fb.get_pixel(second, TAB_TOP + TAB_H / 2),
            Some(t.face_hover),
            "another tab has no face of its own: the strip's shows through"
        );
        assert_eq!(
            fb.get_pixel(second, TAB_STRIP_H - 1),
            Some(t.border),
            "…and the rule runs under it"
        );
        // The inactive label is dim, and the current one is not.
        let darkness = |c: Rgb| c.r as u32 + c.g as u32 + c.b as u32;
        let darkest = |x0: u32, x1: u32| {
            (x0..x1)
                .flat_map(|x| (0..h).map(move |y| (x, y)))
                .filter_map(|(x, y)| fb.get_pixel(x, y))
                .map(darkness)
                .min()
                .expect("pixels")
        };
        assert!(
            darkest(TAB_SIDE, TAB_SIDE + TAB_W - TAB_CLOSE_W) <= darkness(t.foreground) + 12,
            "the current tab's label is body ink"
        );
        assert!(
            darkest(second, second + TAB_W - TAB_CLOSE_W - 4) >= darkness(t.foreground_dim),
            "another tab's label is dim"
        );
        // Hovering one of the others must not open it at the bottom: the rule stays.
        let (hover, _) = paint_strip(TabExtras::none(), Some(2));
        assert_eq!(
            hover.get_pixel(second, TAB_STRIP_H - 1),
            Some(t.border),
            "a hovered tab still has the rule under it"
        );
        assert_ne!(
            hover.get_pixel(second, TAB_TOP + TAB_H / 2),
            Some(t.face_hover),
            "…and it does light up"
        );
        // A keyed right-hand slot is drawable: the diff refuses a half-keyed container.
        let keyed = TabExtras {
            new_tab: None,
            right: Some(ink(t.foreground, text("848 bytes")).key(99)),
        };
        let (_, diffable) = paint_strip(keyed, None);
        assert!(diffable, "a caller that keys its right-hand control makes the window undiffable");
    }

    /// A dialog's edge follows *its* window's focus, not the fact that it is up.
    ///
    /// **Two windows edged in the accent is the failure** (PR #319 review, blocking 1): a dialog
    /// that has lost the keyboard to another window is a click away — the compositor raises any
    /// listed window, and a dialog is listed — and every caller already tracks that flag for the
    /// dialog's own title bar. This passed `true` unconditionally, so the edge said "the
    /// keyboard is here" while the title bar beside it said otherwise.
    #[test]
    fn a_dialogs_edge_follows_its_own_focus() {
        let t = Theme::default();
        let edge = |focused: bool| {
            let ui: Element<Msg> = dialog_frame(
                sized(Size::new(0, TITLE_BAR_H), fill(t.face)).key(1),
                padding(Insets::all(DIALOG_PAD), text("Really?")).key(2),
                sized(Size::new(0, 20), text("")).key(5),
                focused,
                &t,
            );
            let all = Rect::new(0, 0, DIALOG_W, DIALOG_H);
            let l = layout(&ui, all, &CELL);
            let mut fb =
                MemFramebuffer::new(Geometry::packed(DIALOG_W, DIALOG_H, PixelFormat::XRGB8888));
            fb.clear(t.background);
            paint(&mut fb, &font(), &t, &ui, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
            fb.get_pixel(0, DIALOG_H / 2)
        };
        assert_eq!(edge(true), Some(t.accent), "a dialog holding the keyboard is edged in it");
        assert_eq!(edge(false), Some(t.border), "and one that has lost it is not");
    }

    /// A pill is the accent filled in, with its label in the paper colour.
    ///
    /// **The inverse of every other control here**, which is the whole point of it: a button is a
    /// face with an edge and ink on it, and this is the accent with the window's own ground as
    /// ink. Painted, because "filled with the accent" is a statement about pixels, and because a
    /// label drawn in `foreground` on an accent ground is the failure worth catching — it reads
    /// as a smudge rather than as a word (desktop refresh, Part H).
    #[test]
    fn a_pill_is_the_accent_with_the_paper_as_its_ink() {
        let t = Theme::default();
        let (w, h) = (80, 30);
        let e: Element<Msg> = pill("Save", (), WidgetState::default(), &t);
        let all = Rect::new(0, 0, w, h);
        let l = layout(&e, all, &CELL);
        let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
        fb.clear(t.face);
        paint(&mut fb, &font(), &t, &e, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
        // The ground, sampled down the left edge and clear of the label in the middle.
        assert_eq!(fb.get_pixel(2, h / 2), Some(t.accent), "a pill is filled with the accent");
        // **Measured as lightness along the middle row**, where the pill is full width so its
        // cut corners cannot be mistaken for the label. Counting pixels at exactly `background`
        // counts almost none: a glyph at this size is mostly antialiased, which is the same
        // thing the status bar's and the greeter's tests ran into.
        let light = |c: Rgb| c.r as u32 + c.g as u32 + c.b as u32;
        let row: alloc::vec::Vec<u32> =
            (0..w).filter_map(|x| fb.get_pixel(x, h / 2)).map(light).collect();
        let (lightest, darkest) = (
            *row.iter().max().expect("pixels"),
            *row.iter().min().expect("pixels"),
        );
        let (ground, paper) = (light(t.accent), light(t.background));
        assert!(
            lightest > ground + (paper - ground) / 2,
            "the label is drawn toward the paper colour: {lightest} against a ground of {ground}"
        );
        assert!(
            darkest >= ground - ANTIALIAS_SLACK,
            "something on the pill is darker than its ground ({darkest} against {ground}) — a \
             label in body ink on the accent reads as a smudge"
        );
        assert!(light(t.foreground) < ground - ANTIALIAS_SLACK, "and body ink would fail that");
    }

    /// A focused window's edge is the accent; an unfocused one's is the line colour.
    ///
    /// **The design's own focus cue** (desktop refresh, Part H), and painted rather than read off
    /// the tree: the edge is the last layer over everything else, so a frame that drew it first
    /// would have the right colour in the wrong place and the tree would look the same.
    #[test]
    fn a_focused_windows_edge_is_the_accent() {
        let t = Theme::default();
        let (w, h) = (120, 60);
        let edge = |focused: bool| {
            let e: Element<Msg> = window_frame(
                sized(Size::new(0, TITLE_BAR_H), fill(t.face)).key(1),
                sized(Size::new(0, 0), text("")).key(2),
                focused,
                &t,
            );
            let all = Rect::new(0, 0, w, h);
            let l = layout(&e, all, &CELL);
            let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
            fb.clear(t.background);
            paint(&mut fb, &font(), &t, &e, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
            // Down the left edge, clear of the rounded corners.
            fb.get_pixel(0, h / 2)
        };
        assert_eq!(edge(true), Some(t.accent), "the focused window is edged in the accent");
        assert_eq!(edge(false), Some(t.border), "and an unfocused one in the line colour");
        assert_ne!(t.accent, t.border);
    }

    /// A title bar's subtitle is beside the title, dimmer than it, and absent when there is none.
    ///
    /// **Painted, and in that order**: the pair is a bold name and a second word that must read
    /// as secondary, so what this asserts is that the darkest ink in the subtitle's half of the
    /// bar is lighter than the title's — not merely that two strings are in the tree (desktop
    /// refresh, Part H).
    #[test]
    fn a_titles_subtitle_is_beside_it_and_dimmer_than_it() {
        let t = Theme::default();
        let (w, h) = (300, TITLE_BAR_H);
        let draw = |subtitle: Option<&str>| {
            let e: Element<Msg> =
                title_bar("Files", subtitle, true, (), TitleButtons::default(), &t);
            let all = Rect::new(0, 0, w, h);
            let l = layout(&e, all, &CELL);
            let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
            fb.clear(t.background);
            paint(&mut fb, &font(), &t, &e, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
            fb
        };
        let darkness = |c: Rgb| c.r as u32 + c.g as u32 + c.b as u32;
        let darkest = |fb: &MemFramebuffer, x0: u32, x1: u32| {
            (x0..x1)
                .flat_map(|x| (0..h).map(move |y| (x, y)))
                .filter_map(|(x, y)| fb.get_pixel(x, y))
                .map(darkness)
                .min()
                .expect("pixels")
        };
        // Where each word falls: the title starts at `TITLE_PAD`, and "Files" in the bold face
        // is comfortably inside 60 pixels at the default size.
        let (title_half, sub_half) = (60, 200);
        let with = draw(Some("/home"));
        let without = draw(None);
        assert!(
            darkest(&with, title_half, sub_half) < darkest(&without, title_half, sub_half),
            "the subtitle draws where there was nothing"
        );
        // **Against the dim ink itself, not merely against the title.** A first version compared
        // the two halves and passed with the subtitle in *body* ink: antialiasing left it three
        // units lighter than the title, and "darker than" was true of a difference nobody could
        // see. What matters is which ink it is, so the threshold is that ink.
        let (title_ink, sub_ink) =
            (darkest(&with, 0, title_half), darkest(&with, title_half, sub_half));
        assert!(
            sub_ink >= darkness(t.foreground_dim),
            "the subtitle is {sub_ink}, darker than `foreground_dim` — it is not a second read"
        );
        assert!(
            title_ink <= darkness(t.foreground) + ANTIALIAS_SLACK,
            "and the title is body ink at {title_ink}"
        );
    }

    /// A status bar is a ground, a rule on the edge that faces the content, and dim readings.
    ///
    /// **Painted**, because every part of this is a colour in a place: a bar whose rule is on the
    /// wrong edge reads as a lid rather than a floor, and a reading in body ink is not a second
    /// read (desktop refresh, Part H).
    #[test]
    fn a_status_bar_grounds_itself_and_rules_the_edge_it_is_given() {
        let t = Theme::default();
        let (w, h) = (200, STATUS_BAR_H);
        let draw = |rule: Edge| {
            let ui: Element<Msg> = status_bar(
                status_text("opened", &t),
                Some(status_text("ln 6", &t)),
                rule,
                &t,
            );
            let all = Rect::new(0, 0, w, h);
            let l = layout(&ui, all, &CELL);
            let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
            fb.clear(t.background);
            paint(&mut fb, &font(), &t, &ui, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
            fb
        };
        let top = draw(Edge::Top);
        assert_eq!(top.get_pixel(w / 2, 0), Some(t.border), "the rule is along the top edge");
        assert_eq!(top.get_pixel(w / 2, h - 1), Some(t.face_hover), "and the other edge is ground");
        let bottom = draw(Edge::Bottom);
        assert_eq!(bottom.get_pixel(w / 2, h - 1), Some(t.border), "or along the bottom");
        assert_eq!(bottom.get_pixel(w / 2, 0), Some(t.face_hover), "…and then the top is ground");
        // **Both readings are there, and neither is body ink — measured by darkness, not by the
        // exact colour.** At the small step a glyph is mostly antialiased, so counting pixels
        // equal to `foreground_dim` counts almost nothing; what separates the two inks is how
        // dark the darkest pixel gets. Dim is 91/103/102 and body is 22/32/31, so nothing drawn
        // in dim reaches the threshold and anything drawn in body sails past it.
        let darkness = |c: Rgb| c.r as u32 + c.g as u32 + c.b as u32;
        let darkest = (0..w)
            .flat_map(|x| (0..h).map(move |y| (x, y)))
            .filter_map(|(x, y)| top.get_pixel(x, y))
            .map(darkness)
            .min()
            .expect("the bar has pixels");
        // **Each end counted separately** (PR #319 review, blocking 2). One count over the whole
        // bar passes on the left reading alone, so dropping the right slot — which is where the
        // editor's line and column live — failed nothing. Each half must have ink of its own.
        let ink_between = |x0: u32, x1: u32| {
            (x0..x1)
                .flat_map(|x| (0..h).map(move |y| (x, y)))
                .filter(|(x, y)| {
                    let c = top.get_pixel(*x, *y);
                    c != Some(t.face_hover) && c != Some(t.border)
                })
                .count()
        };
        let (left_ink, right_ink) = (ink_between(0, w / 2), ink_between(w / 2, w));
        assert!(left_ink > 20, "the left reading is drawn: {left_ink} pixels");
        assert!(right_ink > 20, "the right reading is drawn: {right_ink} pixels");
        let floor = darkness(t.foreground_dim);
        assert!(
            darkest >= floor,
            "something is darker than `foreground_dim` ({darkest} against {floor}) — a reading \
             on a status bar is a second read, not body ink"
        );
        assert!(darkness(t.foreground) < floor, "and body ink would fail that");
    }

    /// A tab's close box is drawn, not merely clickable.
    ///
    /// **`Node::Icon` measures as nothing** and paints into the rect it is handed, so any wrapper
    /// that sizes to its child's measurement — `center`, and it was a `center` — gives the glyph
    /// a zero rect. The tab then still *clicks* like a tab with a close box, which is why the
    /// routing test above passed while the × was missing from a screendump (desktop refresh,
    /// Part H). Painted, because that is the only place the difference exists.
    #[test]
    fn a_tabs_close_box_puts_ink_on_the_screen() {
        #[derive(Clone, PartialEq, Eq, Debug)]
        enum M {
            Select(u64),
            Close(u64),
        }
        let t = Theme::default();
        let tabs = [Tab { key: 1, label: "one", marked: false }];
        let ui: Element<M> =
            tab_strip(&tabs, 1, None, M::Select, M::Close, TabExtras::none(), &t);
        let all = Rect::new(0, 0, 200, TAB_STRIP_H);
        let l = layout(&ui, all, &CELL);
        let mut fb = MemFramebuffer::new(Geometry::packed(200, TAB_STRIP_H, PixelFormat::XRGB8888));
        fb.clear(t.background);
        paint(&mut fb, &font(), &t, &ui, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
        // The close box is the last `TAB_CLOSE_W` of the first tab.
        let (from, to) = (TAB_SIDE + TAB_W - TAB_CLOSE_W, TAB_SIDE + TAB_W);
        let inked = |x0: u32, x1: u32| {
            (x0..x1)
                .flat_map(|x| (0..TAB_STRIP_H).map(move |y| (x, y)))
                .filter(|(x, y)| fb.get_pixel(*x, *y) == Some(t.foreground_dim))
                .count()
        };
        assert!(inked(from, to) > 8, "the close box drew nothing");
        // And the ink is the box's, not the label's spilling into it.
        assert!(inked(to, to + TAB_GAP + 4) == 0, "and it stays inside the tab");
    }

    #[test]
    fn a_marked_tab_says_so_in_its_label() {
        // The editor's unsaved mark, which is the only thing distinguishing two tabs on the same
        // file name — and the reason `Tab` carries a flag rather than the caller pre-marking the
        // string: two applications would otherwise spell the mark two ways.
        #[derive(Clone, PartialEq, Eq, Debug)]
        enum M {
            Select(u64),
            Close(u64),
        }
        let theme = Theme::default();
        let quiet = [Tab { key: 1, label: "notes", marked: false }];
        let dirty = [Tab { key: 1, label: "notes", marked: true }];
        let extras = || TabExtras::none();
        let a: Element<M> = tab_strip(&quiet, 1, None, M::Select, M::Close, extras(), &theme);
        let b: Element<M> = tab_strip(&dirty, 1, None, M::Select, M::Close, extras(), &theme);
        assert_eq!(all_text(&a), "notes");
        assert_eq!(all_text(&b), "* notes");
    }

    #[test]
    fn the_title_buttons_land_where_the_gates_aim() {
        // **The literals the gates' `chrome` table types**, asserted against a framed window that
        // is actually built — the same guard `dialog_buttons_land_where_the_constants_say` gives
        // the dialog, and for its reason: `title_button_centre` moves with the constants, so a
        // change to the padding moved it and this crate's own tests together and left the gates
        // aiming four pixels off with nothing saying so (PR #313 review, optional 6). In a window
        // 400 wide: close 18 in from the right, maximise 50, minimise 82, all 16 down.
        assert_eq!(title_button_centre(400, 0), (382, 16));
        assert_eq!(title_button_centre(400, 1), (350, 16));
        assert_eq!(title_button_centre(400, 2), (318, 16));

        #[derive(Clone, PartialEq, Eq, Debug)]
        enum M {
            Drag,
            Min,
            Max,
            Close,
        }
        let theme = Theme::default();
        let bar = title_bar(
            "a window",
            None,
            true,
            M::Drag,
            TitleButtons { minimise: Some(M::Min), maximise: Some(M::Max), close: Some(M::Close) },
            &theme,
        )
        .key(1);
        let e: Element<M> =
            window_frame(bar, sized(Size::new(0, 0), text("")).key(2), true, &theme);
        let cell = crate::layout::FixedCell { w: 8, h: 16 };
        let l = crate::layout::layout(&e, Rect::new(0, 0, 400, 200), &cell);
        let mut tree = crate::diff::Tree::new();
        tree.update(&e, &l).expect("a clean frame");
        let click = |(x, y): (i32, i32)| {
            let at = |pressed: bool| PointerEvent {
                kind: POINTER_BUTTON,
                button: 0x110,
                buttons: u16::from(pressed),
                flags: if pressed { POINTER_PRESSED } else { 0 },
                x,
                y,
                ..Default::default()
            };
            let mut r = crate::route::Router::new();
            let _ = r.pointer(&tree, &e, &l, at(true));
            r.pointer(&tree, &e, &l, at(false)).0
        };
        assert_eq!(click((382, 16)), vec![M::Close]);
        assert_eq!(click((350, 16)), vec![M::Max]);
        assert_eq!(click((318, 16)), vec![M::Min]);
    }

    #[test]
    fn a_framed_windows_grip_is_inside_its_border_and_under_it() {
        // **The one corner a person reaches for was the one whose border was missing**: all three
        // applications stacked the grip over the finished frame (PR #313 review, optional 4). The
        // frame places it now — inside the border, as a layer before the border — and takes no
        // presses anywhere but the grip, which a full-size overlay would.
        #[derive(Clone, PartialEq, Eq, Debug)]
        enum M {
            Content,
            Grip,
        }
        let theme = Theme::default();
        let window = Size::new(200, 120);
        let content: Element<M> = sized(Size::new(0, 0), crate::element::custom(1, Size::new(0, 0)))
            .on_press(M::Content)
            .key(2);
        let e = window_frame_with_grip(
            sized(Size::new(0, TITLE_BAR_H), fill(theme.face)).key(1),
            content,
            resize_grip(M::Grip, &theme).key(3),
            window,
            true,
            &theme,
        );
        let crate::element::Node::Stack(layers) = &e.node else { panic!("the frame is a stack") };
        assert!(
            matches!(layers.last().map(|l| &l.node), Some(crate::element::Node::Outline { .. })),
            "the border is not the top layer, so the grip is painted over it"
        );
        let cell = crate::layout::FixedCell { w: 8, h: 16 };
        let l = crate::layout::layout(&e, Rect::new(0, 0, window.w, window.h), &cell);
        let grip = crate::layout::locate(&e, &l, 3).expect("the grip is keyed");
        assert_eq!(
            (grip.right(), grip.bottom()),
            ((window.w - WINDOW_BORDER) as i64, (window.h - WINDOW_BORDER) as i64),
            "the grip is not in the corner inside the border: {grip:?}"
        );
        let mut tree = crate::diff::Tree::new();
        tree.update(&e, &l).expect("a clean frame");
        let click = |x: i32, y: i32| {
            let at = |pressed: bool| PointerEvent {
                kind: POINTER_BUTTON,
                button: 0x110,
                buttons: u16::from(pressed),
                flags: if pressed { POINTER_PRESSED } else { 0 },
                x,
                y,
                ..Default::default()
            };
            let mut r = crate::route::Router::new();
            let mut got = r.pointer(&tree, &e, &l, at(true)).0;
            got.extend(r.pointer(&tree, &e, &l, at(false)).0);
            got
        };
        assert_eq!(click(window.w as i32 - 8, window.h as i32 - 8), vec![M::Grip], "the grip's middle");
        assert_eq!(click(window.w as i32 - 30, window.h as i32 - 8), vec![M::Content], "beside it is content");
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
        //
        // **Moved by the desktop refresh's Part B**, from (91, 249, 103): the window's content now
        // runs flush to its border (`WINDOW_FRAME` 3 → 0), so the strip is three pixels wider on
        // each side and three pixels lower. `check-login`'s `CONFIRM_*` moved in the same change.
        assert_eq!((DIALOG_W, DIALOG_H), (340, 132));
        assert_eq!((DIALOG_LEFT_CX, DIALOG_RIGHT_CX, DIALOG_BUTTON_CY), (89, 250, 106));

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
                None,
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
            true,
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
