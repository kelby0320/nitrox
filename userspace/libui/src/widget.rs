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
    Edge, Element, IconKind, Insets, TextSize, bevel, bold, center, center_v, column, dock, docked, fill, icon, ink,
    outline, padding, rounded_fill, row, scaled, sized, stack, text, wash, with_spacing,
};
// The editing keys. **Imported, not re-declared** — `libkern::abi` publishes these and
// `libterm::encode` already imports exactly this set from there, so a second copy is a second
// thing that can disagree about a key. The same argument the `libinput` dependency is
// justified by, applied to the codes as well as the mapping (PR #233 review, finding 3).
// `libinput::keymap` does not map them because it answers "what text does this produce", and
// these produce none.
use libkern::abi::{KEY_BACKSPACE, KEY_DELETE, KEY_DOWN, KEY_END, KEY_ENTER, KEY_HOME, KEY_LEFT, KEY_RIGHT, KEY_UP};
use librsproto::surface::MOD_SHIFT;

mod bars;
mod frames;
mod scroll;
mod status;
mod tabs;

pub use bars::*;
pub use frames::*;
pub use scroll::*;
pub use status::*;
pub use tabs::*;

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
pub fn button<Msg>(label: impl Into<String>, msg: Msg, state: WidgetState, theme: &Theme) -> Element<Msg> {
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
    let (edge, thickness) = if state.active { (theme.accent, RING) } else { (theme.border, BORDER) };
    // **Centred, which is what a button's label is everywhere else in the world.** It was against
    // the top-left corner of the face: `padding` places a child at an inset from the origin, and
    // a face is usually much wider than the word on it, so every button in this toolkit read as
    // a label with a box drawn round it. `center` is the node that was missing.
    let layers = alloc::vec![
        rounded_fill(edge, CONTROL_RADIUS),
        padding(Insets::all(thickness), rounded_fill(face, CONTROL_RADIUS.saturating_sub(thickness))),
        center(padding(BUTTON_PAD, text(label))),
    ];
    stack(layers).on_press(msg).focusable()
}


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

/// Space between a text field's content and its edge.
const FIELD_PAD: Insets = Insets { top: 4, right: 8, bottom: 4, left: 8 };

/// The same inset, for a caller that must aim inside a text area — the editor's tests, which
/// press at a column and need to know where column nought begins.
pub const TEXT_AREA_PAD: Insets = FIELD_PAD;

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
    /// ASCII only, because [`libinput::keymap::text`] is: the US layout is what the input
    /// stack maps today, and a field that invented its own mapping would disagree with the
    /// terminal about what a key means. `locks` are the key's own (`KeyEvent::locks`): Caps Lock's
    /// case, and the keypad's digits under Num Lock (Phase 6 Part B.5).
    pub fn apply(&mut self, keycode: u16, modifiers: u16, locks: u16) -> bool {
        match keycode {
            KEY_BACKSPACE => self.backspace(),
            KEY_DELETE => self.delete(),
            KEY_LEFT => self.left(),
            KEY_RIGHT => self.right(),
            KEY_HOME => self.home(),
            KEY_END => self.end(),
            _ => match libinput::keymap::text(keycode, modifiers, locks) {
                // Control characters are not text. Ctrl-C folds to 0x03 in the keymap because
                // a terminal needs it to; a field that inserted it would put an unprintable
                // byte in a password.
                Some(b) if (0x20..0x7F).contains(&b) => {
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
pub fn text_field<Msg>(field: &TextFieldState, masked: bool, state: WidgetState, theme: &Theme) -> Element<Msg> {
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
    let (edge, thickness) = if state.active { (theme.accent, RING) } else { (theme.border, BORDER) };
    let layers = alloc::vec![
        rounded_fill(edge, CONTROL_RADIUS),
        padding(Insets::all(thickness), rounded_fill(theme.track, CONTROL_RADIUS.saturating_sub(thickness))),
        padding(FIELD_PAD, row(content)),
    ];
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
        let lines: Vec<String> = text.split('\n').map(|l| String::from(l.strip_suffix('\r').unwrap_or(l))).collect();
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
    pub fn at_point(&self, x: i32, y: i32, row_height: u32, width: impl Fn(&str) -> u32) -> (usize, usize) {
        let y = y - FIELD_PAD.top as i32;
        let row = (y.max(0) as u32).checked_div(row_height).unwrap_or(0) as usize;
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
        for col in text.char_indices().map(|(i, _)| i).chain(core::iter::once(text.len())) {
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
        ScrollState { offset: self.offset as u32, visible: visible as u32, total: self.lines.len() as u32 }
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
            let here = Snapshot { lines: self.lines.clone(), line: self.line, col: self.col };
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
    pub fn apply(&mut self, keycode: u16, modifiers: u16, locks: u16) -> bool {
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
            _ => match libinput::keymap::text(keycode, modifiers, locks) {
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
    let visible = height.checked_div(row_height).unwrap_or(0) as usize;
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

    let layers = alloc::vec![fill(theme.track), padding(FIELD_PAD, column(rows))];
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
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
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
    /// A small shape before the label — the browser's folder-or-file mark, or a place's dot.
    pub swatch: Option<Swatch>,
    /// A control at the row's trailing end — a drive's eject (Phase 6 Part F) — or `None`.
    pub button: Option<RowButton<'a>>,
}

/// A control at a row's trailing end: a glyph that does something to the row's subject rather
/// than opening it, as a tab's `×` closes its tab (Phase 6 Part F).
///
/// **Pressed through the list's own `activate`, with a key of its own**, so a list with buttons
/// takes no more parameters than one without: the caller tells a press on the button from one on
/// the row by the key, as it tells rows apart. The key must differ from every row's key in the
/// list, the row's own included.
///
/// **Over the button is still over the row**: the row stays lit while the pointer is on its
/// button, as a tab does over its `×`. And a press on the button is not a grab of the row, by the
/// router's rule that a nearer `on_press` shadows an `on_press_down`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RowButton<'a> {
    /// Its key — what `activate` is handed when it is pressed, and what `hovered` names over it.
    pub key: u64,
    /// What it shows: one glyph, drawn dim and in full ink under the pointer.
    pub glyph: &'a str,
}

/// How wide a row's button is, in pixels.
pub const ROW_BUTTON_W: u32 = 24;
/// The face behind a row's button under the pointer: the tab close box's.
const ROW_BUTTON_HOVER: u32 = TAB_CLOSE_HOVER + 2;

/// The shape drawn before a row's label. See [`ListRow::swatch`].
///
/// **Two shapes, named rather than described at each call site**: the design marks a listing's
/// rows with a small block and a sidebar's places with a dot, and a caller that spelled out
/// widths and radii would be free to invent a third (desktop refresh, Part I).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Swatch {
    /// What colour it is drawn in.
    pub colour: Rgb,
    /// How wide, in pixels.
    pub width: u32,
    /// How tall, in pixels.
    pub height: u32,
    /// The corner it is rounded to; half the height makes it round.
    pub radius: u32,
}

impl Swatch {
    /// The listing's mark: the design's 11×9 with the barest corner on it.
    pub fn block(colour: Rgb) -> Self {
        Self { colour, width: SWATCH_W, height: SWATCH_H, radius: 1 }
    }

    /// A place's dot in a sidebar: the design's 5, round.
    pub fn dot(colour: Rgb) -> Self {
        Self { colour, width: DOT_W, height: DOT_W, radius: DOT_W / 2 }
    }
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

/// How much space a row's label gets around it — public so a heading over a list's rows can
/// start where they do (Files' Drives, Phase 6 Part F).
pub const ROW_PAD: Insets = Insets { top: 2, right: 6, bottom: 2, left: 6 };

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
        let visible = height.checked_div(row_height).unwrap_or(0);
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
#[allow(
    clippy::too_many_arguments,
    reason = "the list's rows, handlers and appearance are separate inputs of one constructor"
)]
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
    let visible = height.checked_div(row_height).unwrap_or(0) as usize;
    // **The selection is clamped first, because it is an index into a list that may have just
    // been replaced.** A launcher rebuilds its results on every keystroke, so a selection made
    // against twenty of them is not an index into the three that remain — and a stale one is
    // worse than useless: nothing paints as selected, `down` sees `i + 1 < len` fail and
    // returns the same index so the key is *dead*, and a caller reading `selected` to decide
    // what Enter activates gets an out-of-range index. Clamped to the last surviving row
    // rather than cleared, so something is highlighted and the arrows work on the next press
    // (PR #233 review, finding 2).
    if let Some(i) = state.selected
        && i >= rows.len()
    {
        state.selected = rows.len().checked_sub(1);
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
        // Over a row's button is over the row (see [`RowButton`]).
        let over = hovered == Some(r.key) || r.button.is_some_and(|b| hovered == Some(b.key));
        let primary = selected || (over && state.selected.is_none());
        let mut layers = alloc::vec![fill(ground)];
        if primary {
            layers.push(wash(theme.accent, libdraw::theme::SELECTION_COVERAGE));
        } else if over {
            layers.push(wash(theme.accent, hover));
        } else if r.button.is_some() {
            // **A row with a button keeps its layers in place, lit or not** (Phase 6 Part F). The
            // router finds the widget a press captured by its id, and an id survives a repaint
            // only while its siblings keep their positions: a wash inserted here when the press
            // lit the row moved the body and its button along one, and the release that followed
            // the repaint was lost — or, in `check-media`, opened the drive instead of ejecting
            // it. A row with no button has its handler on the row itself, which no layer moves.
            layers.push(text(""));
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
        if columns.is_empty() && r.swatch.is_none() && r.button.is_none() {
            layers.push(padding(ROW_PAD, text(r.label)));
            let row_el = stack(layers);
            let mut item = sized(Size::new(0, row_height), row_el).key(r.key).on_press(activate(r.key));
            if let Some(f) = grab {
                item = item.on_press_down(f(r.key));
            }
            items.push(item);
            continue;
        }
        let mut cells = alloc::vec::Vec::with_capacity(columns.len() + 2);
        if let Some(sw) = r.swatch {
            // **The lead-in is the same width whatever the shape**, so a sidebar's dots and a
            // listing's blocks both leave their labels on one edge.
            cells.push(center_v(sized(
                Size::new(SWATCH_W, sw.height),
                center(sized(Size::new(sw.width, sw.height), rounded_fill(sw.colour, sw.radius))),
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
        let body = padding(ROW_PAD, row(cells));
        match r.button {
            None => layers.push(body),
            // **Both halves keyed**, since the diff wants a container's children all keyed or none
            // and the button needs a key for hover to name it: the body takes the row's own, as a
            // tab's label takes its tab's, so `hovered` over it still names the row.
            Some(b) => {
                // The face may come and go: it is inside the button, which is the widget a press
                // captures, and that keeps its id however its own children move.
                let mut face = alloc::vec::Vec::with_capacity(2);
                let glyph_ink = if hovered == Some(b.key) {
                    face.push(center(sized(
                        Size::new(ROW_BUTTON_HOVER, ROW_BUTTON_HOVER),
                        rounded_fill(theme.face_pressed, TITLE_BUTTON_RADIUS),
                    )));
                    theme.foreground
                } else {
                    theme.foreground_dim
                };
                face.push(center(ink(glyph_ink, text(b.glyph))));
                layers.push(row(alloc::vec![
                    body.flex(1).key(r.key),
                    sized(Size::new(ROW_BUTTON_W, 0), stack(face).on_press(activate(b.key))).key(b.key),
                ]));
            }
        }
        let row_el = stack(layers);
        let mut item = sized(Size::new(0, row_height), row_el).key(r.key).on_press(activate(r.key));
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
        let bar = ScrollState { offset: state.offset as u32, visible: visible as u32, total: rows.len() as u32 };
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
/// A place's dot in a sidebar: the design's 5.
pub const DOT_W: u32 = 5;
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
    scrolling: bool,
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
            dock(alloc::vec![docked(Edge::Bottom, sized(Size::new(0, 1), fill(theme.border)))], text(""),),
            padding(
                // **The scrollbar's width, when there is one** (PR #320 review, worth fixing 5): a
                // list that overflows puts its rows beside a scrollbar and they lose that width,
                // while this spans the whole strip — so every column sat twelve pixels left of its
                // heading in any directory long enough to scroll, which is most of them.
                Insets { right: ROW_PAD.right + if scrolling { SCROLLBAR_W } else { 0 }, ..ROW_PAD },
                row(cells),
            ),
        ]),
    )
}

/// Whether a list of `rows` at `row_height` overflows `height` — and so draws a scrollbar.
///
/// **The rule `list_view` uses, published**, because a caller drawing a header above one needs
/// the same answer and computing it twice is how the two come to disagree.
pub fn list_scrolls(rows: usize, height: u32, row_height: u32) -> bool {
    let visible = height.checked_div(row_height).unwrap_or(0) as usize;
    rows > visible
}

#[cfg(test)]
mod list_view_tests;

#[cfg(test)]
mod text_field_tests;

#[cfg(test)]
mod tests;
