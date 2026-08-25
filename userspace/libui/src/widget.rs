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

/// `EV_KEY` codes for the editing keys, which [`libinput::keymap`] deliberately does not map —
/// it answers "what text does this produce", and these produce none.
const KEY_BACKSPACE: u16 = 14;
/// See [`KEY_BACKSPACE`].
const KEY_DELETE: u16 = 111;
/// See [`KEY_BACKSPACE`].
const KEY_LEFT: u16 = 105;
/// See [`KEY_BACKSPACE`].
const KEY_RIGHT: u16 = 106;
/// See [`KEY_BACKSPACE`].
const KEY_HOME: u16 = 102;
/// See [`KEY_BACKSPACE`].
const KEY_END: u16 = 107;

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

    const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSansMono.ttf");

    fn font() -> libdraw::text::Font {
        libdraw::text::Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses")
    }
}
