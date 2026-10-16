use super::*;

/// Space between a text field's content and its edge.
pub(super) const FIELD_PAD: Insets = Insets { top: 4, right: 8, bottom: 4, left: 8 };

/// How wide the caret is, in pixels.
pub(super) const CARET: u32 = 2;

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
