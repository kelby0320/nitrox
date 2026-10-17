//! The multi-line text area, with undo.

use super::*;

/// The same inset, for a caller that must aim inside a text area.
pub const TEXT_AREA_PAD: Insets = FIELD_PAD;

/// A multi-line text buffer with a cursor, a selection and a scroll position.
///
/// Lines are logical and unbounded, never wrapped, unlike [`libterm`]'s rewrapping grid.
/// Byte offsets, always on character boundaries, as in [`TextFieldState`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TextAreaState {
    /// The lines. Never empty: an empty buffer is one empty line, so every method can index
    /// without a guard.
    lines: Vec<String>,
    line: usize,
    col: usize,
    /// Where a selection started, or `None` when there is no selection.
    ///
    /// An anchor rather than a range: a selection is directional while it is made, and dragging
    /// back past the anchor selects the other way.
    anchor: Option<(usize, usize)>,
    /// The column vertical movement is aiming for, in **characters**.
    ///
    /// Set by the first Up or Down and cleared by anything horizontal, so moving down through a
    /// short line and back up does not lose the column.
    goal: Option<usize>,
    offset: usize,
    /// What the last [`ensure_visible`](Self::ensure_visible) was for: the cursor's line and
    /// column, the text's revision, and how many lines fitted.
    ///
    /// Everything that could have put the caret out of view, and nothing else: following on
    /// every build made a scrollbar impossible.
    followed: Option<(usize, usize, u64, usize)>,
    /// How many times the *text* has changed.
    ///
    /// Only the state can answer "is this buffer modified?"; movement never bumps it.
    revision: u64,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// What the edits since the last snapshot were, or `None` when the next one starts a group.
    group: Option<EditKind>,
}

/// What kind of edit is in progress, so that adjacent ones of a kind coalesce.
///
/// Per keystroke is unusable and per save is useless; a person expects a word or a line. A run
/// of printable characters is one group, a separator ends it, `Enter` ends it, a run of
/// deletions is a group of its own, and any movement ends whatever was open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EditKind {
    Typing,
    Deleting,
    /// A paste, its own kind so it is its own undo step.
    Pasting,
}

/// A buffer and a cursor, as they were before a group of edits.
///
/// A whole copy, not a delta: a copy cannot be wrong about what it restores, at the cost of
/// memory bounded by [`MAX_UNDO`]. Trigger for deltas: a file where that bound bites,
/// `TODO(undo-deltas)`. The selection is not kept: it is a gesture in progress.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Snapshot {
    lines: Vec<String>,
    line: usize,
    col: usize,
}

/// How many groups of edits a buffer can go back through; each is a copy of the buffer.
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

    /// A buffer holding `text`, cursor at the start; `\r\n` and `\n` both end a line.
    pub fn with_text(text: &str) -> Self {
        // One line minimum, which `split` guarantees for every input, `""` included.
        let lines: Vec<String> = text.split('\n').map(|l| String::from(l.strip_suffix('\r').unwrap_or(l))).collect();
        Self { lines, ..Self::default() }
    }

    /// The buffer as one string, lines joined with `\n`, with no trailing newline added.
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

    /// How many times the text has changed — the identity for a "modified" marker.
    ///
    /// Never reset, so a buffer edited back to a saved state still reads as modified.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The selection as `(start, end)` in document order, or `None`.
    pub fn selection(&self) -> Option<((usize, usize), (usize, usize))> {
        let a = self.anchor?;
        let c = (self.line, self.col);
        if a == c {
            return None;
        }
        Some(if a <= c { (a, c) } else { (c, a) })
    }

    /// Whether anything is selected; cheaper than [`selected_text`](Self::selected_text).
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

    /// Put the cursor at `(line, col)`, clamped, and drop any selection.
    ///
    /// `col` is moved back to a character boundary rather than refused.
    pub fn place(&mut self, line: usize, col: usize) {
        self.anchor = None;
        self.goal = None;
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
    pub fn extend_to(&mut self, line: usize, col: usize) {
        let from = self.anchor.unwrap_or((self.line, self.col));
        self.place(line, col);
        self.anchor = Some(from);
    }

    /// Delete the selection, leaving the cursor at where it started. `false` if there was none.
    ///
    /// The anchor is dropped either way: one the cursor has walked back onto indexes text this
    /// call is about to shorten. Every edit funnels through here.
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
        // A separator ends the group it is part of, so a word and the space after it undo
        // together; ending it before the space would hand the space back alone.
        if !c.is_alphanumeric() {
            self.group = None;
        }
    }

    /// Insert `text` at the cursor, replacing any selection, as **one** undo step.
    ///
    /// Returns the range it occupies afterwards, so a cycle that replaces what was just inserted
    /// is told where that is.
    pub fn insert_text(&mut self, text: &str) -> ((usize, usize), (usize, usize)) {
        self.begin(EditKind::Pasting);
        self.delete_selection();
        let from = (self.line, self.col);
        let tail = self.lines[self.line].split_off(self.col);
        let mut parts = text.split('\n');
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
        self.group = None;
        (from, to)
    }

    /// The (line, column) a pointer at widget-local `(x, y)` is over.
    ///
    /// Takes a way to measure text, as this crate has no glyphs: pass
    /// `|s| metrics.text_size(s).w`. The column is the nearest boundary, and the widget's padding
    /// is subtracted here.
    pub fn at_point(&self, x: i32, y: i32, row_height: u32, width: impl Fn(&str) -> u32) -> (usize, usize) {
        let y = y - FIELD_PAD.top as i32;
        let row = (y.max(0) as u32).checked_div(row_height).unwrap_or(0) as usize;
        let line = (self.offset + row).min(self.lines.len().saturating_sub(1));
        let text = &self.lines[line];
        let x = x - FIELD_PAD.left as i32;
        if x <= 0 {
            return (line, 0);
        }
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
    pub fn bar(&self, visible: usize) -> ScrollState {
        ScrollState { offset: self.offset as u32, visible: visible as u32, total: self.lines.len() as u32 }
    }

    /// Scroll so `offset` is the first visible line, clamped; the cursor does not move.
    pub fn scroll_to(&mut self, offset: usize, visible: usize) {
        self.offset = offset.min(self.lines.len().saturating_sub(visible.max(1)));
    }

    /// Select from `from` to `to`, putting the cursor at `to`.
    ///
    /// Clamped, because a range a paste returned is stale after the next edit.
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
        self.group = None;
    }

    /// Start a group of `kind` if none is open, and forget the way forward.
    ///
    /// Called before the edit, since the snapshot is of what came before it.
    fn begin(&mut self, kind: EditKind) {
        if self.group != Some(kind) {
            let here = Snapshot { lines: self.lines.clone(), line: self.line, col: self.col };
            if self.undo.len() == MAX_UNDO {
                self.undo.remove(0);
            }
            self.undo.push(here);
            self.group = Some(kind);
        }
        // Any edit abandons the way forward, so redo is a branch rather than a second history.
        self.redo.clear();
    }

    /// Close whatever group is open, so the next edit starts a new one.
    ///
    /// Saving is an event this type cannot see: without it a word, a save and another word
    /// undid as one group.
    pub fn end_group(&mut self) {
        self.group = None;
    }

    /// Go back one group of edits. `false` when there is nothing to go back to.
    ///
    /// The revision moves, so a buffer undone to what is on disk still reads as modified,
    /// `TODO(undo-clean-revision)`.
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

    /// Put a snapshot back and end the open group; the cursor is clamped, not trusted.
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
    /// An empty needle matches nothing, so a search does not jump on each keystroke.
    pub fn find(&mut self, needle: &str) -> bool {
        if needle.is_empty() {
            return false;
        }
        // From the next character after the cursor, or a second press finds the match it sits
        // on. Character, not byte: `col + 1` can land inside a multi-byte character.
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
    /// Never spans a line break; nothing can type one into the find field.
    fn search_from(&self, needle: &str, (line, col): (usize, usize)) -> Option<(usize, usize)> {
        for (i, text) in self.lines.iter().enumerate().skip(line) {
            let from = if i == line { col.min(text.len()) } else { 0 };
            if let Some(at) = text.get(from..).and_then(|tail| tail.find(needle)) {
                return Some((i, from + at));
            }
        }
        None
    }

    /// Delete backwards: the selection, else the character before the cursor, else join lines.
    pub fn backspace(&mut self) -> bool {
        if self.selection().is_some() {
            self.begin(EditKind::Deleting);
            self.delete_selection();
            self.group = None;
            return true;
        }
        // `delete_selection` still has work with no selection: it clears an anchor the cursor
        // has walked back onto, which would otherwise name a byte past the end of a line this
        // edit shortens.
        self.delete_selection();
        // One guard, before the group, so a `Backspace` that cannot do anything leaves no undo
        // step and everything below can rely on something being behind the cursor.
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
            // Joining: the cursor lands where the join happened.
            let cur = self.lines.remove(self.line);
            self.line -= 1;
            self.col = self.lines[self.line].len();
            self.lines[self.line].push_str(&cur);
        }
        self.revision += 1;
        true
    }

    /// Delete forwards: the selection, else the character after the cursor, else join lines.
    pub fn delete(&mut self) -> bool {
        if self.selection().is_some() {
            self.begin(EditKind::Deleting);
            self.delete_selection();
            self.group = None;
            return true;
        }
        self.delete_selection();
        if self.col >= self.lines[self.line].len() && self.line + 1 >= self.lines.len() {
            return false;
        }
        self.begin(EditKind::Deleting);
        self.goal = None;
        if self.col < self.lines[self.line].len() {
            self.lines[self.line].remove(self.col);
        } else {
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

    /// Set or clear the anchor before a movement: shift starts a selection from the cursor, an
    /// unshifted movement drops one. A movement ends whatever group was open.
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

    fn col_for(&self, line: usize, chars: usize) -> usize {
        let l = &self.lines[line];
        l.char_indices().nth(chars).map(|(i, _)| i).unwrap_or(l.len())
    }

    fn prev_boundary(&self) -> usize {
        let l = &self.lines[self.line];
        l[..self.col].chars().next_back().map(|c| self.col - c.len_utf8()).unwrap_or(0)
    }

    /// Scroll so the cursor's line is among the `visible` shown, as [`ListState::ensure_visible`].
    pub fn ensure_visible(&mut self, visible: usize) {
        if visible == 0 {
            return;
        }
        // Once per reason to follow, as in [`ListState::ensure_visible`]: following on every build
        // could not be scrolled away from, and on a line change alone let you type unseen.
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
    /// Shift extends. Enter is text here and Tab is not: swallowing it would trap the keyboard.
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
/// Colours rather than token kinds: the application scans its own text. Byte offsets the widget
/// does not trust, since a stale cache can name bytes past a shortened line or inside a
/// character; a run's bounds split the line only where they are boundaries now.
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
/// Takes the state by `&mut` and scrolls it, so no caller has to write it back. `height` is
/// what the caller will lay it out at; wrap the result in `sized` to keep the two in step.
///
/// `pointer` receives widget-local coordinates, which the application turns into a line and
/// column with [`TextAreaState::at_point`]. A scrollbar is composed beside it by the
/// application. `ink` may be empty, and runs for lines not on screen are ignored.
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
        let span = sel.and_then(|((sl, sc), (el, ec))| {
            if i < sl || i > el {
                return None;
            }
            let from = if i == sl { sc } else { 0 };
            let to = if i == el { ec } else { l.len() };
            // An empty line inside a multi-line selection draws no highlight: the widget cannot
            // measure a space. Trigger: the first time a widget can ask for a glyph's advance.
            (from < to).then_some((from, to))
        });

        // The caret is clamped into the line: a cut outside it would be dropped, caret and all.
        let caret = (active && i == cur_line).then_some(cur_col.min(l.len()));

        // Every boundary on this line in one sorted list, then one pass: as cuts, the selection,
        // caret and syntax runs have no ordering left to get wrong.
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
        // A cut has to be a character boundary of *this* line, as runs come from a scan of the
        // buffer as it was; `is_char_boundary` is false past the end, so a `min` would be dead.
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
            if let Some(r) = runs.clone().find(|r| r.start <= from && from < r.end) {
                piece = crate::element::ink(r.colour, piece);
            }
            if span.is_some_and(|(f, t)| from >= f && to <= t) {
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
        e = e.on_pointer(f);
    }
    e
}
