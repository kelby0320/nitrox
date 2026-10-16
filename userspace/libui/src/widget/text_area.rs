use super::*;

/// The same inset, for a caller that must aim inside a text area — the editor's tests, which
/// press at a column and need to know where column nought begins.
pub const TEXT_AREA_PAD: Insets = FIELD_PAD;

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
