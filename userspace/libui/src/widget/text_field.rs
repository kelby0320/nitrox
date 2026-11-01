//! The single-line text field.

use super::*;

/// Space between a text field's content and its edge.
pub(super) const FIELD_PAD: Insets = Insets { top: 4, right: 8, bottom: 4, left: 8 };

/// How wide the caret is, in pixels.
pub(super) const CARET: u32 = 2;

/// What a masked field shows instead of each character.
///
/// `*` rather than a bullet: a missing glyph in a password field is invisible to the person
/// typing, and ASCII cannot go wrong here.
const MASK_CHAR: char = '*';

/// The editable content of a single-line text field.
///
/// The state is the application's and the widget is a pure function of it:
/// [`Element::on_key`](crate::element::Element::on_key) is a function pointer, so a widget
/// cannot close over anything to mutate. The application hands keys to [`apply`](Self::apply)
/// and passes the result to [`text_field`].
///
/// A single line, not an editor: no wrapping, selection, undo or multi-line cursor.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TextFieldState {
    /// What has been typed.
    text: String,
    /// Where the caret is, as a **byte** offset into `text`.
    ///
    /// Every mutation keeps it on a character boundary, the invariant the type rests on.
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

    /// [`clear`](Self::clear), and zero the allocation, which it leaves as it was. For a password.
    pub fn scrub(&mut self) {
        // SAFETY: empty, the text is valid UTF-8 whatever its spare capacity holds.
        let bytes = unsafe { self.text.as_mut_vec() };
        bytes.clear();
        for byte in bytes.spare_capacity_mut() {
            // SAFETY: `byte` is a valid, exclusive reference to one byte of the allocation.
            unsafe { core::ptr::write_volatile(byte, core::mem::MaybeUninit::new(0)) };
        }
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
    /// Keys it does not claim answer `false` and bubble: Tab, Enter and Escape belong to
    /// whatever is above the field, as [`Element::on_key`](crate::element::Element::on_key)
    /// requires.
    ///
    /// ASCII only, because [`libinput::keymap::text`] is. `locks` are the key's own
    /// (`KeyEvent::locks`): Caps Lock's case, and the keypad's digits under Num Lock.
    pub fn apply(&mut self, keycode: u16, modifiers: u16, locks: u16) -> bool {
        match keycode {
            KEY_BACKSPACE => self.backspace(),
            KEY_DELETE => self.delete(),
            KEY_LEFT => self.left(),
            KEY_RIGHT => self.right(),
            KEY_HOME => self.home(),
            KEY_END => self.end(),
            _ => match libinput::keymap::text(keycode, modifiers, locks) {
                // Control characters are not text: Ctrl-C folds to 0x03 in the keymap, and
                // would put an unprintable byte in a password.
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
/// The caret is a `Row` split at the cursor: the text before it, a two-pixel fill, then the text
/// after. The same measurement that draws the text places the caret, for any font.
///
/// Masked with [`MASK_CHAR`] per character, not per byte, so a multi-byte character neither leaks
/// its length nor is split.
///
/// The caret is drawn from `state.active`: a field in an unfocused window must not blink one.
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

    // A well the content sits in, as the scrollbar's `track` is. Edged at rest: `track` is
    // `--bg` in the light scheme, so an unedged field on a window's own ground is invisible.
    // The ring keeps its second pixel and its accent, so focus reads as a state.
    let (edge, thickness) = if state.active { (theme.accent, RING) } else { (theme.border, BORDER) };
    let layers = alloc::vec![
        rounded_fill(edge, CONTROL_RADIUS),
        padding(Insets::all(thickness), rounded_fill(theme.track, CONTROL_RADIUS.saturating_sub(thickness))),
        padding(FIELD_PAD, row(content)),
    ];
    stack(layers).focusable()
}
