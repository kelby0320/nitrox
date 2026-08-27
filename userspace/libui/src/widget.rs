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
use libdraw::format::Rgb;
use libdraw::geom::Size;

use crate::element::{Element, Insets, column, fill, padding, row, sized, stack, text};
// The editing keys. **Imported, not re-declared** — `libkern::abi` publishes these and
// `libterm::encode` already imports exactly this set from there, so a second copy is a second
// thing that can disagree about a key. The same argument the `libinput` dependency is
// justified by, applied to the codes as well as the mapping (PR #233 review, finding 3).
// `libinput::keymap` does not map them because it answers "what text does this produce", and
// these produce none.
use libkern::abi::{
    KEY_BACKSPACE, KEY_DELETE, KEY_DOWN, KEY_END, KEY_HOME, KEY_LEFT, KEY_RIGHT, KEY_UP,
};

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

/// The colours a widget draws itself in.
///
/// Constants rather than a theming system, for the reason `widget-toolkit.md` §11 gives —
/// and shaped as a struct so that becoming one later is a change of provenance rather than a
/// change of every call site.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Palette {
    /// A button's face at rest.
    pub face: Rgb,
    /// Its face under the pointer.
    pub face_hover: Rgb,
    /// Its face while held.
    pub face_pressed: Rgb,
    /// The ring drawn around the focused widget.
    pub focus_ring: Rgb,
    /// A scrollbar's groove.
    pub track: Rgb,
    /// A scrollbar's thumb.
    pub thumb: Rgb,
    /// A title bar's face while its window holds the keyboard.
    pub title_active: Rgb,
    /// A title bar's face while it does not.
    ///
    /// **Two faces rather than one**, because a title bar is the only chrome that says which
    /// window is focused. The compositor announces focus and the window list marks it, but a
    /// person looking at two overlapping windows reads it here.
    pub title_inactive: Rgb,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            face: Rgb::new(0x24, 0x2C, 0x36),
            face_hover: Rgb::new(0x30, 0x3A, 0x46),
            face_pressed: Rgb::new(0x18, 0x1E, 0x26),
            focus_ring: Rgb::new(0x5A, 0x9F, 0xD4),
            track: Rgb::new(0x18, 0x1E, 0x26),
            thumb: Rgb::new(0x3A, 0x46, 0x54),
            title_active: Rgb::new(0x2E, 0x3A, 0x4A),
            title_inactive: Rgb::new(0x1C, 0x22, 0x2A),
        }
    }
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
    palette: &Palette,
) -> Element<Msg> {
    let face = if state.pressed {
        palette.face_pressed
    } else if state.hovered {
        palette.face_hover
    } else {
        palette.face
    };
    // Bottom to top: the ring, then the face, then the label. A `Stack` gives every layer
    // the whole area, so the ring is only visible because the face above it is inset by the
    // ring's width — and the label sits above both, inset further so it clears the edge.
    let mut layers = alloc::vec::Vec::with_capacity(3);
    if state.active {
        layers.push(fill(palette.focus_ring));
        layers.push(padding(Insets::all(RING), fill(face)));
    } else {
        layers.push(fill(face));
    }
    layers.push(padding(BUTTON_PAD, text(label)));
    stack(layers).on_press(msg).focusable()
}

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
pub fn scrollbar<Msg>(state: ScrollState, width: u32, height: u32, palette: &Palette) -> Element<Msg> {
    let (pos, len) = state.thumb(height);
    sized(
        Size::new(width, 0),
        stack(alloc::vec![
            fill(palette.track),
            column(alloc::vec![
                sized(Size::new(0, pos), fill(palette.track)),
                sized(Size::new(0, len), fill(palette.thumb)),
                // The remainder, so the thumb does not stretch to the bottom.
                fill(palette.track).flex(1),
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
pub fn menu_bar<Msg>(items: alloc::vec::Vec<Element<Msg>>, height: u32, palette: &Palette) -> Element<Msg> {
    sized(
        Size::new(0, height),
        stack(alloc::vec![fill(palette.face), row(items)]),
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
    palette: &Palette,
) -> Element<Msg> {
    let face = if focused { palette.title_active } else { palette.title_inactive };
    let btn = |label: &str, msg: Msg| {
        sized(
            Size::new(TITLE_BUTTON_W, TITLE_BAR_H),
            stack(alloc::vec![padding(BUTTON_PAD, text(label))]).on_press(msg),
        )
    };
    // **A button a caller has no message for is not drawn.** The alternative is a button that
    // does nothing, and a control that looks live and is not is the defect this milestone's
    // predecessor shipped three of (M8's overview). The buttons arrive with the parts that give
    // them somewhere to go: minimise and maximise in Part B, close in Part C.
    let mut controls = alloc::vec::Vec::with_capacity(4);
    controls.push(padding(TITLE_PAD, text(title)).flex(1));
    if let Some(m) = buttons.minimise {
        controls.push(btn("_", m));
    }
    if let Some(m) = buttons.maximise {
        controls.push(btn("[]", m));
    }
    if let Some(m) = buttons.close {
        controls.push(btn("X", m));
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
    palette: &Palette,
) -> Element<Msg> {
    let render = |s: &str| -> String {
        if masked { core::iter::repeat_n(MASK_CHAR, s.chars().count()).collect() } else { s.into() }
    };
    let (before, after) = field.text.split_at(field.cursor);

    let mut content = alloc::vec::Vec::with_capacity(3);
    content.push(text(render(before)));
    if state.active {
        content.push(sized(Size::new(CARET, 0), fill(palette.focus_ring)));
    }
    content.push(text(render(after)));

    // `track` is the recessed-channel colour the scrollbar uses, and a text field is the same
    // idea: a well the content sits in, rather than a face that stands out of the surface.
    let mut layers = alloc::vec::Vec::with_capacity(3);
    if state.active {
        layers.push(fill(palette.focus_ring));
        layers.push(padding(Insets::all(RING), fill(palette.track)));
    } else {
        layers.push(fill(palette.track));
    }
    layers.push(padding(FIELD_PAD, row(content)));
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
/// `state` is taken by value and scrolled to follow the selection — see
/// [`ensure_visible`](ListState::ensure_visible). A caller that wants the scroll to persist
/// reads it back from the returned state.
pub fn list_view<Msg>(
    rows: &[ListRow<'_>],
    state: ListState,
    height: u32,
    row_height: u32,
    activate: fn(u64) -> Msg,
    palette: &Palette,
) -> (Element<Msg>, ListState) {
    let mut state = state;
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
        let face = if selected { palette.face_hover } else { palette.track };
        let row_el = stack(alloc::vec![fill(face), padding(ROW_PAD, text(r.label))]);
        items.push(sized(Size::new(0, row_height), row_el).key(r.key).on_press(activate(r.key)));
    }

    let list = column(items);
    let body = if rows.len() > visible {
        let bar = ScrollState {
            offset: state.offset as u32,
            visible: visible as u32,
            total: rows.len() as u32,
        };
        row(alloc::vec![
            list.flex(1),
            scrollbar(bar, SCROLLBAR_W, height, palette),
        ])
    } else {
        list
    };
    (stack(alloc::vec![fill(palette.track), body]).focusable(), state)
}

/// How wide a list's scrollbar is, in pixels.
const SCROLLBAR_W: u32 = 10;

#[cfg(test)]
mod list_view_tests {
    use super::*;
    use crate::element::Node;

    fn rows<'a>(labels: &'a [(u64, &'a str)]) -> alloc::vec::Vec<ListRow<'a>> {
        labels.iter().map(|&(key, label)| ListRow { key, label }).collect()
    }

    /// The whole point of the widget: a hundred rows cost as many elements as fit.
    #[test]
    fn only_the_visible_rows_become_elements() {
        let data: alloc::vec::Vec<(u64, &str)> = (0..100u64).map(|i| (i, "row")).collect();
        let r = rows(&data);
        let (e, _): (Element<u64>, _) =
            list_view(&r, ListState::default(), 100, 20, |k| k, &Palette::default());
        assert_eq!(keys(&e).len(), 5, "the list built rows it cannot show");
    }

    /// Without keys the diff pairs by position, and its own doc names the failure: insert at
    /// the top and "row 2's widget" pairs with "row 3's element".
    #[test]
    fn every_row_carries_its_key_not_its_index() {
        let data = [(70u64, "a"), (80, "b"), (90, "c")];
        let (e, _): (Element<u64>, _) =
            list_view(&rows(&data), ListState::default(), 100, 20, |k| k, &Palette::default());
        assert_eq!(keys(&e), alloc::vec![70, 80, 90], "rows are keyed by position");
    }

    /// The window-list caller: rows reorder in place on every raise.
    #[test]
    fn a_reordered_window_list_keeps_each_rows_identity() {
        let before = [(1u64, "term"), (2, "editor")];
        let after = [(2u64, "editor"), (1, "term")];
        let (a, _): (Element<u64>, _) =
            list_view(&rows(&before), ListState::default(), 100, 20, |k| k, &Palette::default());
        let (b, _): (Element<u64>, _) =
            list_view(&rows(&after), ListState::default(), 100, 20, |k| k, &Palette::default());
        assert_eq!(keys(&a), alloc::vec![1, 2]);
        assert_eq!(keys(&b), alloc::vec![2, 1], "the reorder did not move the keys");
    }

    /// The launcher caller: results are replaced wholesale on every keystroke, so the list
    /// gets shorter under a scroll offset that was valid a frame ago.
    #[test]
    fn a_list_that_shrinks_under_a_stale_offset_still_renders() {
        let long: alloc::vec::Vec<(u64, &str)> = (0..20u64).map(|i| (i, "hit")).collect();
        let (_, state): (Element<u64>, _) = list_view(
            &rows(&long),
            ListState { selected: Some(19), offset: 0 },
            100, 20, |k| k, &Palette::default(),
        );
        assert_eq!(state.offset, 15, "the scroll did not follow the selection");
        let short = [(0u64, "hit"), (1, "hit"), (2, "hit")];
        let (e, mut state): (Element<u64>, _) =
            list_view(&rows(&short), state, 100, 20, |k| k, &Palette::default());
        assert_eq!(state.offset, 0, "a stale offset survived the list shrinking");
        assert_eq!(keys(&e).len(), 3, "the list rendered blank");

        // **The selection half, which the first version of this test did not assert and so
        // passed against a widget that left it dangling** (PR #233 review, finding 2). A
        // selection of 19 into three rows highlights nothing, and `down` is *dead*: it takes
        // `Some(i) if i + 1 < len` and otherwise returns `i` unchanged, so a stale index never
        // comes back into range on its own.
        assert_eq!(state.selected, Some(2), "the selection still indexes the longer list");
        assert!(
            row_faces(&e).iter().any(|f| *f == Palette::default().face_hover),
            "no row is painted as selected"
        );
        assert!(!state.down(3), "the selection is already on the last row");
        assert!(state.up(), "the arrow keys are dead after the shrink");
        assert_eq!(state.selected, Some(1));
    }

    /// Shrinking to nothing leaves nothing selected, rather than row `-1`.
    #[test]
    fn a_list_that_empties_clears_the_selection() {
        let (_, state): (Element<u64>, _) = list_view(
            &[],
            ListState { selected: Some(3), offset: 2 },
            100, 20, |k| k, &Palette::default(),
        );
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

    /// The selected row paints differently, or selection is invisible.
    #[test]
    fn the_selected_row_is_painted_differently() {
        let data = [(1u64, "a"), (2, "b")];
        let p = Palette::default();
        let (e, _): (Element<u64>, _) =
            list_view(&rows(&data), ListState { selected: Some(1), offset: 0 }, 100, 20, |k| k, &p);
        let faces = row_faces(&e);
        assert_eq!(faces.len(), 2);
        assert_ne!(faces[0], faces[1], "the selected row looks like the others");
        assert_eq!(faces[1], p.face_hover, "the selected row is not the selected colour");
    }

    /// A scrollbar that is always there wastes width; one that never appears strands rows.
    #[test]
    fn the_scrollbar_appears_only_when_there_is_more_than_fits() {
        let p = Palette::default();
        let few = [(1u64, "a"), (2, "b")];
        let (e, _): (Element<u64>, _) =
            list_view(&rows(&few), ListState::default(), 100, 20, |k| k, &p);
        assert!(!has_row_node(&e), "a list that fits drew a scrollbar");
        let many: alloc::vec::Vec<(u64, &str)> = (0..20u64).map(|i| (i, "x")).collect();
        let (e, _): (Element<u64>, _) =
            list_view(&rows(&many), ListState::default(), 100, 20, |k| k, &p);
        assert!(has_row_node(&e), "a list that overflows drew no scrollbar");
    }

    /// Each row's activation message carries that row's key.
    #[test]
    fn a_rows_message_carries_its_own_key() {
        let data = [(11u64, "a"), (22, "b")];
        let (e, _): (Element<u64>, _) =
            list_view(&rows(&data), ListState::default(), 100, 20, |k| k, &Palette::default());
        assert_eq!(presses(&e), alloc::vec![11, 22], "a row sent another row's message");
    }

    /// Zero row height must not divide by zero.
    #[test]
    fn a_degenerate_row_height_is_not_a_division() {
        let data = [(1u64, "a")];
        let (e, _): (Element<u64>, _) =
            list_view(&rows(&data), ListState::default(), 100, 0, |k| k, &Palette::default());
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
        let e: Element<()> = text_field(&f, true, WidgetState { active: true, ..Default::default() }, &Palette::default());
        assert_eq!(rendered(&e), "**", "the mask leaked the byte length");
        // Negative control: unmasked shows the real text.
        let e: Element<()> = text_field(&f, false, WidgetState::default(), &Palette::default());
        assert_eq!(rendered(&e), "aé");
    }

    /// The caret is drawn from `active`, so a field in an unfocused window does not blink one.
    #[test]
    fn the_caret_appears_only_when_active() {
        let f = TextFieldState::with_text("ab");
        let active: Element<()> =
            text_field(&f, false, WidgetState { active: true, ..Default::default() }, &Palette::default());
        let idle: Element<()> = text_field(&f, false, WidgetState::default(), &Palette::default());
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
            text_field(&f, false, WidgetState { active: true, ..Default::default() }, &Palette::default());
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
        let p = Palette::default();
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
        let p = Palette::default();
        let e = button("OK", (), WidgetState::default(), &p);
        assert!(e.on_press.is_some(), "a button that sends nothing is not a button");
        assert!(e.focusable, "a keyboard user must be able to reach it");
    }

    #[test]
    fn a_buttons_face_follows_the_state_it_is_given() {
        // Widgets take their state as an argument rather than remembering it, so this is the
        // whole of a button's appearance logic and it is a pure function.
        let p = Palette::default();
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
        let p = Palette::default();
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
        let p = Palette::default();
        let t = Theme::default();
        let mut fb = MemFramebuffer::new(Geometry::packed(80, 40, PixelFormat::XRGB8888));
        let e: Element<Msg> = button("OK", (), WidgetState::default(), &p);
        let l = layout(&e, Rect::new(0, 0, 80, 40), &CELL);
        paint(&mut fb, &font(), &t, &e, &l, Rect::new(0, 0, 80, 40), &mut |_, _, _, _: &mut MemFramebuffer| {});
        assert_eq!(fb.get_pixel(0, 0), Some(p.face), "face all the way to the edge");
    }

    #[test]
    fn a_menu_bar_is_as_tall_as_asked_and_no_taller() {
        let p = Palette::default();
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
        let p = Palette::default();
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
        let p = Palette::default();
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
        let p = Palette::default();
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
        let p = Palette::default();
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
        let mut down = |r: &mut Router, x: i32| r.pointer(&tree, &e, &l, at(x, true)).0;
        let mut up = |r: &mut Router, x: i32| r.pointer(&tree, &e, &l, at(x, false)).0;

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

    const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSansMono.ttf");

    fn font() -> libdraw::text::Font {
        libdraw::text::Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses")
    }
}
