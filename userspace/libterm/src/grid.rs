//! The cell grid: what [`Op`]s do to a screen, and where lines go when they leave it.
//!
//! Where terminals are subtly wrong: wrapping is deferred ([`Grid::print`]); cursor addressing
//! clamps and never wraps or scrolls; erasing fills with the current background; scrolling
//! moves a line into a bounded scrollback ring.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::cell::{Attributes, Cell, Colour, Flags};
use crate::parse::{Erase, Op, Sgr};

/// Columns a terminal starts at.
/// A starting size, not a fixed one: [`Grid::resize`] changes it.
pub const COLS: usize = 80;
/// Rows a terminal starts at. See [`COLS`].
pub const ROWS: usize = 24;

/// Lines retained above the screen.
/// A ring, so a program that prints forever costs a bounded amount.
pub const SCROLLBACK: usize = 1000;

/// Columns between tab stops.
/// Fixed: nothing here emits `HTS`.
pub const TAB_WIDTH: usize = 8;

/// A line of cells, and whether the line below it continues the same text.
/// The flag records which wraps were soft, so the scrollback can be re-wrapped.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Line {
    /// The cells, as wide as the grid was when the line was produced.
    pub cells: Vec<Cell>,
    /// A soft wrap: text ran past the right margin and continues on the next line.
    pub wrapped: bool,
}

/// How a [`resize`](Grid::resize) moved the lines, so an anchor into them can follow.
/// An anchor into the old line numbering follows the text across the new one.
pub struct Reflow {
    /// The absolute number of the oldest line this maps.
    base: u64,
    /// Where each old line's text begins in the new layout: which logical line, and how far
    /// into it. Empty when the resize moved nothing.
    at: Vec<(usize, usize)>,
    /// Where each logical line starts in the new row numbering, and how many rows it takes.
    lines: Vec<(usize, usize)>,
    /// The new width, which is what turns an offset back into a row and a column.
    cols: usize,
    /// Logical lines the scrollback dropped during this resize.
    evicted: usize,
}

impl Reflow {
    /// Where the line that was numbered `old` is now.
    /// Total, clamping at both ends: the scrollback may have evicted the line.
    pub fn map_line(&self, old: u64) -> u64 {
        self.map_position(old, 0).0
    }

    /// Where the *cell* at old `(line, col)` is now; a selection needs the column too.
    pub fn map_position(&self, line: u64, col: usize) -> (u64, usize) {
        if self.at.is_empty() {
            return (line, col);
        }
        let i = (line.saturating_sub(self.base) as usize).min(self.at.len() - 1);
        let (li, into) = self.at[i];
        let (start, height) = self.lines[li];
        let off = into + col;
        // Clamped to the line's last row: a column past the trimmed text belongs at its end.
        let seg = (off / self.cols).min(height.saturating_sub(1));
        (self.base + (start + seg) as u64, (off - seg * self.cols).min(self.cols - 1))
    }

    /// Narrowing makes more rows from the same text, so a full history loses its oldest lines
    /// to the ring; this attributes that to eviction, not the rewrap.
    pub fn evicted_lines(&self) -> usize {
        self.evicted
    }
}

/// A range of text a person has swept out with the pointer.
/// A pair of absolute `(line, column)` positions, so a selection in the history does not slide
/// as output arrives. An anchor and a head rather than a start and an end: dragging back past
/// the press selects the other way; [`ordered`](Selection::ordered) normalises. A position
/// survives a resize as an offset into its logical line ([`Reflow::map_position`]).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Selection {
    /// Where the gesture started: absolute line, and column.
    pub anchor: (u64, usize),
    /// Where the pointer is now.
    pub head: (u64, usize),
}

impl Selection {
    /// `(first, last)` in reading order, whichever way the drag went.
    pub fn ordered(&self) -> ((u64, usize), (u64, usize)) {
        if (self.head.0, self.head.1) < (self.anchor.0, self.anchor.1) {
            (self.head, self.anchor)
        } else {
            (self.anchor, self.head)
        }
    }

    /// Whether the range covers no cells at all.
    /// A press with no drag is empty yet in progress, so the selection is not `None`.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Whether the cell at absolute `(line, col)` falls inside the range.
    /// Half-open at the end: the cell under the pointer is not yet swept.
    pub fn contains(&self, line: u64, col: usize) -> bool {
        let (first, last) = self.ordered();
        (line, col) >= first && (line, col) < last
    }
}

/// The screen, the cursor, and the lines that have scrolled off it.
pub struct Grid {
    cols: usize,
    rows: usize,
    /// Row-major, `rows * cols`.
    cells: Vec<Cell>,
    /// Lines that have scrolled off the top, oldest first.
    scrollback: VecDeque<Line>,
    /// Per screen row: its text continues on the row below. See [`Line::wrapped`].
    wrapped: Vec<bool>,
    /// Cursor row, always `< rows`.
    row: usize,
    /// Cursor column, always `< cols`.
    col: usize,
    /// Deferred wrap: set when a character lands in the last column; the next one wraps.
    pending_wrap: bool,
    /// Attributes the next printed character takes.
    attrs: Attributes,
    /// Rows changed since [`Grid::take_damage`].
    dirty: Vec<bool>,
    /// Where the cursor was when damage was last taken: the render paints it into its cell, so
    /// moving it must damage the row it left.
    cursor_drawn: (usize, usize),
    /// Lines ever scrolled off the top: the absolute line number of the screen's first row.
    /// It counts lines produced, so eviction does not move it; a view anchored here does not
    /// creep as output arrives.
    scrolled: u64,
    /// The range a person has swept out, if any. The grid's, since reflow and eviction both
    /// invalidate it.
    selection: Option<Selection>,
}

/// Only exactly-blank cells: a space written with a background colour is content.
fn trim_trailing_blanks(cells: &mut Vec<Cell>) {
    while cells.last() == Some(&Cell::BLANK) {
        cells.pop();
    }
}

impl Default for Grid {
    /// An 80×24 grid.
    fn default() -> Self {
        Self::new(COLS, ROWS)
    }
}

impl Grid {
    /// An empty grid of `cols` × `rows`, each clamped to at least 1.
    pub fn new(cols: usize, rows: usize) -> Grid {
        let (cols, rows) = (cols.max(1), rows.max(1));
        Grid {
            cols,
            rows,
            cells: vec![Cell::BLANK; cols * rows],
            scrollback: VecDeque::new(),
            wrapped: vec![false; rows],
            row: 0,
            col: 0,
            pending_wrap: false,
            attrs: Attributes::default(),
            dirty: vec![true; rows],
            cursor_drawn: (0, 0),
            scrolled: 0,
            selection: None,
        }
    }

    /// Width in columns.
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Height in rows.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Cursor position as `(row, col)`, both zero-based.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    /// The attributes the next printed character will take.
    pub fn attributes(&self) -> Attributes {
        self.attrs
    }

    /// The cell at `(row, col)`, or `None` outside the screen.
    pub fn cell(&self, row: usize, col: usize) -> Option<Cell> {
        if row < self.rows && col < self.cols { Some(self.cells[row * self.cols + col]) } else { None }
    }

    /// Lines that have scrolled off, oldest first.
    pub fn scrollback(&self) -> impl ExactSizeIterator<Item = &Line> {
        self.scrollback.iter()
    }

    /// Whether screen row `row`'s text continues on the row below.
    pub fn row_wrapped(&self, row: usize) -> bool {
        self.wrapped.get(row).copied().unwrap_or(false)
    }

    /// The absolute line number of the screen's first row.
    pub fn top_line(&self) -> u64 {
        self.scrolled
    }

    /// The absolute line number of the oldest line still retained; eviction moves it.
    pub fn oldest_line(&self) -> u64 {
        self.scrolled - self.scrollback.len() as u64
    }

    /// `top` brought inside the history that still exists.
    pub fn clamp_view(&self, top: u64) -> u64 {
        top.clamp(self.oldest_line(), self.top_line())
    }

    /// The cell at viewport `(row, col)` when its first line is absolute line `top`; `None`
    /// outside the grid or for an evicted line, so a stale anchor draws blanks.
    pub fn view_cell(&self, top: u64, row: usize, col: usize) -> Option<Cell> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        let line = top.checked_add(row as u64)?;
        if line >= self.scrolled {
            self.cell((line - self.scrolled) as usize, col)
        } else {
            let i = line.checked_sub(self.oldest_line())? as usize;
            self.scrollback.get(i).and_then(|l| l.cells.get(col)).copied()
        }
    }

    /// Where the cursor falls in a viewport whose first line is `top`, or `None` if out of
    /// view: scrolling back does not move it.
    pub fn view_cursor(&self, top: u64) -> Option<(usize, usize)> {
        let row = (self.scrolled + self.row as u64).checked_sub(top)?;
        (row < self.rows as u64).then_some((row as usize, self.col))
    }

    /// The range a person has swept out, if any.
    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// Whether anything is selected: a range that exists and covers cells. Not
    /// `selection().is_some()`, which is true after a bare press.
    pub fn has_selection(&self) -> bool {
        self.selection.is_some_and(|s| !s.is_empty())
    }

    /// Begin a selection at absolute `(line, col)` — a press; both ends start here.
    pub fn select_from(&mut self, line: u64, col: usize) {
        self.selection = Some(Selection { anchor: (line, col), head: (line, col) });
        self.damage_all();
    }

    /// Move the head of an in-progress selection — a drag; without an anchor it does nothing.
    pub fn extend(&mut self, line: u64, col: usize) {
        if let Some(sel) = self.selection.as_mut() {
            if sel.head == (line, col) {
                return;
            }
            sel.head = (line, col);
            self.damage_all();
        }
    }

    /// Drop the selection, if there is one. Returns whether anything changed.
    pub fn clear_selection(&mut self) -> bool {
        if self.selection.take().is_some() {
            self.damage_all();
            true
        } else {
            false
        }
    }

    /// Select every line the grid holds, scrollback included; returns whether anything was.
    pub fn select_all(&mut self) -> bool {
        let oldest = self.oldest_line();
        let last = self.top_line() + self.rows() as u64 - 1;
        // "Nothing to select" means every line is blank, not a line-number comparison.
        let mut end = None;
        let mut at = oldest;
        while at <= last {
            if let Some(text) = self.line_text(at)
                && !text.is_empty()
            {
                end = Some((at, text.chars().count()));
            }
            at += 1;
        }
        let Some((last_line, last_col)) = end else { return false };
        self.select_from(oldest, 0);
        // To the end of the last line with text: `extend` does not clamp.
        self.extend(last_line, last_col);
        true
    }

    /// The text of one absolute line, trailing blanks trimmed; `None` outside the history.
    pub fn line_text(&self, line: u64) -> Option<String> {
        if line < self.oldest_line() || line > self.top_line() + self.rows as u64 - 1 {
            return None;
        }
        let mut out = String::new();
        for col in 0..self.cols {
            out.push(self.view_cell(line, 0, col)?.ch);
        }
        while out.ends_with(' ') {
            out.pop();
        }
        Some(out)
    }

    /// The first `needle` at or after `(line, col)`, searching towards the newest line. Never
    /// spans a line break; columns are character offsets into the line's text.
    pub fn find_from(&self, needle: &str, line: u64, col: usize) -> Option<(u64, usize)> {
        if needle.is_empty() {
            return None;
        }
        let last = self.top_line() + self.rows as u64 - 1;
        let mut at = line.max(self.oldest_line());
        while at <= last {
            if let Some(text) = self.line_text(at) {
                // Cells in, cells out: a column is a character, `str::find` works in bytes.
                let skip = if at == line { col } else { 0 };
                let from: usize = text.char_indices().nth(skip).map_or(text.len(), |(b, _)| b);
                if let Some(hit) = text.get(from..).and_then(|tail| tail.find(needle)) {
                    return Some((at, text[..from + hit].chars().count()));
                }
            }
            at += 1;
        }
        None
    }

    /// Drop the lines that have scrolled off, keeping the screen and the absolute numbering.
    /// Returns whether anything was dropped.
    pub fn clear_scrollback(&mut self) -> bool {
        if self.scrollback.is_empty() {
            return false;
        }
        self.scrollback.clear();
        // A selection reaching into what was just dropped names lines that are gone.
        if let Some(sel) = self.selection
            && sel.ordered().0.0 < self.top_line()
        {
            self.selection = None;
        }
        true
    }

    /// Select the word under `(line, col)`; `false` if there is none. A word is a run of
    /// non-spaces, so `--colour=auto` stays one.
    pub fn select_word_at(&mut self, line: u64, col: usize) -> bool {
        let Some(text) = self.line_text(line) else { return false };
        let chars: Vec<char> = text.chars().collect();
        if col >= chars.len() || chars[col] == ' ' {
            return false;
        }
        let mut from = col;
        while from > 0 && chars[from - 1] != ' ' {
            from -= 1;
        }
        let mut to = col;
        while to + 1 < chars.len() && chars[to + 1] != ' ' {
            to += 1;
        }
        self.select_from(line, from);
        self.extend(line, to + 1);
        true
    }

    /// Select the whole of `line`; `false` if that line is not there.
    pub fn select_line_at(&mut self, line: u64) -> bool {
        let Some(text) = self.line_text(line) else { return false };
        self.select_from(line, 0);
        // To the end of the text, not the row: trailing blanks were never written.
        self.extend(line, text.chars().count());
        true
    }

    /// The selected text, or `None` when nothing is selected. Trailing blanks are dropped per
    /// line and a soft wrap does not become a newline ([`Line::wrapped`]).
    pub fn selected_text(&self) -> Option<String> {
        let sel = self.selection?;
        if sel.is_empty() {
            return None;
        }
        let (first, last) = sel.ordered();
        let mut out = String::new();
        let mut line = first.0;
        while line <= last.0 {
            let from = if line == first.0 { first.1 } else { 0 };
            let to = if line == last.0 { last.1 } else { self.cols };
            let mut row = String::new();
            for col in from..to.min(self.cols) {
                match self.view_cell(line, 0, col) {
                    Some(c) => row.push(c.ch),
                    // Past the end of the history: the anchor named a line the ring evicted.
                    None => break,
                }
            }
            while row.ends_with(' ') {
                row.pop();
            }
            out.push_str(&row);
            // The wrap flag belongs to the line being left, and only a *hard* end is a newline.
            if line < last.0 && !self.line_wrapped(line) {
                out.push('\n');
            }
            line += 1;
        }
        Some(out)
    }

    /// Whether absolute `line`'s text continues on the line below, in history or on screen.
    pub fn line_wrapped(&self, line: u64) -> bool {
        if line >= self.scrolled {
            self.row_wrapped((line - self.scrolled) as usize)
        } else {
            match line.checked_sub(self.oldest_line()) {
                Some(i) => self.scrollback.get(i as usize).is_some_and(|l| l.wrapped),
                None => false,
            }
        }
    }

    /// Which rows changed since this was last called, and clear the record. A moved cursor
    /// damages two rows: the render inverts the cell under it, so the row it left must repaint.
    /// Calling this means "I am about to draw these rows".
    pub fn take_damage(&mut self) -> Vec<usize> {
        if self.cursor_drawn != (self.row, self.col) {
            let (was_row, _) = self.cursor_drawn;
            self.touch(was_row);
            self.touch(self.row);
            self.cursor_drawn = (self.row, self.col);
        }
        let rows: Vec<usize> = self.dirty.iter().enumerate().filter(|(_, d)| **d).map(|(i, _)| i).collect();
        self.dirty.iter_mut().for_each(|d| *d = false);
        rows
    }

    /// Re-lay the whole terminal at `cols` × `rows`, rewrapping the history.
    ///
    /// Rows joined by [`Line::wrapped`] become logical lines again, each is re-broken at the
    /// new width, and the last `rows` become the screen with the cursor following its
    /// character. The screen's empty tail is dropped, not rewrapped, or an idle terminal would
    /// fill the scrollback with blanks. A scrolled-back anchor must go through the returned
    /// [`Reflow`]. Both dimensions are clamped to at least 1.
    pub fn resize(&mut self, cols: usize, rows: usize) -> Reflow {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if cols == self.cols && rows == self.rows {
            return Reflow { base: self.oldest_line(), at: Vec::new(), lines: Vec::new(), cols, evicted: 0 };
        }
        let base = self.oldest_line();

        // 1. Every line there is, oldest first: old absolute number `base + i` for index `i`.
        let mut old: Vec<Line> = Vec::with_capacity(self.scrollback.len() + self.rows);
        let sb_len = self.scrollback.len();
        old.extend(self.scrollback.drain(..));
        let keep = self.content_rows();
        for r in 0..keep {
            let cells = self.cells[r * self.cols..(r + 1) * self.cols].to_vec();
            old.push(Line { cells, wrapped: self.wrapped[r] });
        }
        let cursor_row = sb_len + self.row;

        // 2. Join. `pos[i]` is where old line `i` starts: which logical line, and how far in.
        let mut logical: Vec<Vec<Cell>> = Vec::new();
        let mut pos: Vec<(usize, usize)> = Vec::with_capacity(old.len());
        let mut cur: Vec<Cell> = Vec::new();
        let mut open = false;
        for l in &old {
            pos.push((logical.len(), cur.len()));
            cur.extend_from_slice(&l.cells);
            open = true;
            if !l.wrapped {
                trim_trailing_blanks(&mut cur);
                logical.push(core::mem::take(&mut cur));
                open = false;
            }
        }
        if open {
            // The last row wrapped with nothing after it: a line still being written.
            trim_trailing_blanks(&mut cur);
            logical.push(cur);
        }

        // 3. Re-break at the new width. `start[li]` is where logical line `li` begins.
        let blank = Cell::BLANK;
        let mut lines: Vec<Line> = Vec::with_capacity(logical.len());
        let mut start: Vec<usize> = Vec::with_capacity(logical.len());
        let mut height: Vec<usize> = Vec::with_capacity(logical.len());
        for lg in &logical {
            start.push(lines.len());
            let mut i = 0;
            loop {
                let end = (i + cols).min(lg.len());
                let mut cells = lg[i..end].to_vec();
                cells.resize(cols, blank);
                lines.push(Line { cells, wrapped: end < lg.len() });
                i = end;
                if i >= lg.len() {
                    break;
                }
            }
            height.push(lines.len() - start[start.len() - 1]);
        }

        // Kept as offsets: resolving loses the column. [`Reflow::map_position`] divides.
        let screen_start = lines.len().saturating_sub(rows);
        let at = pos.clone();
        let line_spans: Vec<(usize, usize)> = start.iter().zip(height.iter()).map(|(&s, &h)| (s, h)).collect();

        // 4. The bottom `rows` become the screen; the rest is history.
        self.cells = vec![blank; cols * rows];
        self.wrapped = vec![false; rows];
        for (r, l) in lines[screen_start..].iter().enumerate() {
            self.cells[r * cols..(r + 1) * cols].copy_from_slice(&l.cells);
            self.wrapped[r] = l.wrapped;
        }
        // No fix-up of the last row: the final `lines` element ends a logical line.
        lines.truncate(screen_start);
        self.scrollback = lines.into_iter().collect();
        self.scrolled = base + screen_start as u64;
        // A terminal near the cap loses its oldest lines to the ring, not the rewrap; counted so
        // the two can be told apart. `scrolled` is unchanged: eviction moves the other end.
        let mut evicted = 0;
        while self.scrollback.len() > SCROLLBACK {
            if self.scrollback.pop_front().is_some_and(|l| !l.wrapped) {
                evicted += 1;
            }
        }

        // 5. The cursor follows its character through the same map a selection uses.
        let reflow = Reflow { base, at, lines: line_spans, cols, evicted: 0 };
        let (cursor_line, cursor_col) = reflow.map_position(base + cursor_row as u64, self.col);
        let line_index = (cursor_line - base) as usize;
        self.row = line_index.saturating_sub(screen_start).min(rows - 1);
        self.col = cursor_col;

        // 5b. The selection follows too; `map_position` clamps an evicted line, harmlessly.
        self.selection = self.selection.map(|sel| Selection {
            anchor: reflow.map_position(sel.anchor.0, sel.anchor.1),
            head: reflow.map_position(sel.head.0, sel.head.1),
        });

        self.cols = cols;
        self.rows = rows;
        // A deferred wrap belongs to a margin that has moved.
        self.pending_wrap = false;
        self.cursor_drawn = (self.row, self.col);
        self.dirty = vec![true; rows];

        Reflow { evicted, ..reflow }
    }

    /// How many *logical* lines the terminal holds. A rewrap moves breaks but creates no lines,
    /// so this is unchanged by a [`resize`](Grid::resize), less [`Reflow::evicted_lines`].
    pub fn logical_lines(&self) -> usize {
        let sb = self.scrollback.iter().filter(|l| !l.wrapped).count();
        let screen = (0..self.content_rows()).filter(|r| !self.wrapped[*r]).count();
        sb + screen
    }

    /// How many screen rows carry content — at least enough to include the cursor's row.
    fn content_rows(&self) -> usize {
        let mut last = self.row;
        for r in 0..self.rows {
            if self.cells[r * self.cols..(r + 1) * self.cols].iter().any(|c| *c != Cell::BLANK) {
                last = last.max(r);
            }
        }
        last + 1
    }

    /// Mark every row as needing a repaint — after a resize, or a first paint.
    pub fn damage_all(&mut self) {
        self.dirty.iter_mut().for_each(|d| *d = true);
    }

    /// Apply one parser operation.
    pub fn apply(&mut self, op: Op) {
        match op {
            Op::Print(c) => self.print(c),
            Op::MoveTo { row, col } => self.move_to(row as usize, col as usize),
            Op::MoveBy { rows, cols } => self.move_by(rows, cols),
            Op::CarriageReturn => {
                self.col = 0;
                self.pending_wrap = false;
            }
            Op::LineFeed => self.line_feed(),
            Op::Backspace => {
                // Stops at the left margin, or a program could erase text it never wrote.
                self.col = self.col.saturating_sub(1);
                self.pending_wrap = false;
            }
            Op::Tab => {
                let next = (self.col / TAB_WIDTH + 1) * TAB_WIDTH;
                // Movement to the last column does not set the pending-wrap flag.
                self.col = next.min(self.cols - 1);
                self.pending_wrap = false;
            }
            Op::EraseInDisplay(e) => self.erase_in_display(e),
            Op::EraseInLine(e) => self.erase_in_line(e),
            Op::Attr(s) => self.attr(s),
        }
    }

    /// Feed a whole sequence of operations.
    pub fn apply_all(&mut self, ops: &[Op]) {
        for op in ops {
            self.apply(*op);
        }
    }

    /// Not [`Cell::BLANK`]: `ED` after `SGR 44` fills blue. Bold and underline go; the
    /// foreground goes too, so under `SGR 7` an erase fills with the foreground, as xterm does.
    fn blank(&self) -> Cell {
        Cell {
            ch: ' ',
            attrs: Attributes {
                fg: Colour::Default,
                bg: self.attrs.bg,
                flags: self.attrs.flags.without(Flags::BOLD).without(Flags::UNDERLINE),
            },
        }
    }

    fn touch(&mut self, row: usize) {
        if let Some(d) = self.dirty.get_mut(row) {
            *d = true;
        }
    }

    /// **Deferred wrap, not wrap-on-write.** A character in the last column leaves the cursor on
    /// it with [`Grid::pending_wrap`] set; only the next one wraps. Wrapping at once makes
    /// `cols` characters then a newline print a blank line.
    fn print(&mut self, c: char) {
        if self.pending_wrap {
            self.col = 0;
            // The row being left is a soft wrap, known only now.
            let leaving = self.row;
            self.set_wrapped(leaving, true);
            self.line_feed_scrolling();
        }
        // Cleared unconditionally, or a wrapped paragraph descends one line per character.
        self.pending_wrap = false;
        let (row, col) = (self.row, self.col);
        let cell = Cell::new(c, self.attrs);
        self.cells[row * self.cols + col] = cell;
        self.touch(row);
        if self.col + 1 >= self.cols {
            self.pending_wrap = true;
        } else {
            self.col += 1;
        }
    }

    /// The column does not change: `LF` is index, `CR` returns to column 0. Translating a bare
    /// `\n` is the line discipline's `ONLCR`.
    fn line_feed(&mut self) {
        self.pending_wrap = false;
        // An explicit line feed ends the line, or a reflow joins a stale wrap to the row below.
        self.set_wrapped(self.row, false);
        self.line_feed_scrolling();
    }

    fn set_wrapped(&mut self, row: usize, to: bool) {
        if let Some(w) = self.wrapped.get_mut(row) {
            *w = to;
        }
    }

    fn line_feed_scrolling(&mut self) {
        if self.row + 1 < self.rows {
            self.row += 1;
        } else {
            self.scroll_up();
        }
    }

    /// Move the top line into scrollback and open a blank one at the bottom.
    fn scroll_up(&mut self) {
        let cells = self.cells[0..self.cols].to_vec();
        self.scrollback.push_back(Line { cells, wrapped: self.wrapped[0] });
        // The flags move with their rows.
        self.wrapped.rotate_left(1);
        if let Some(last) = self.wrapped.last_mut() {
            *last = false;
        }
        // Counts lines produced; eviction below does not adjust it.
        self.scrolled += 1;
        while self.scrollback.len() > SCROLLBACK {
            self.scrollback.pop_front();
        }
        self.cells.copy_within(self.cols.., 0);
        let blank = self.blank();
        let start = (self.rows - 1) * self.cols;
        self.cells[start..].fill(blank);
        self.damage_all();
    }

    /// `CUP`: clamp into the screen; never scrolls.
    fn move_to(&mut self, row: usize, col: usize) {
        self.row = row.min(self.rows - 1);
        self.col = col.min(self.cols - 1);
        self.pending_wrap = false;
    }

    /// Relative movement, clamped at every edge and never scrolling.
    fn move_by(&mut self, rows: i32, cols: i32) {
        let r = (self.row as i64 + rows as i64).clamp(0, self.rows as i64 - 1);
        let c = (self.col as i64 + cols as i64).clamp(0, self.cols as i64 - 1);
        self.row = r as usize;
        self.col = c as usize;
        self.pending_wrap = false;
    }

    fn erase_in_line(&mut self, e: Erase) {
        let (row, col) = (self.row, self.col);
        let (from, to) = match e {
            Erase::ToEnd => (col, self.cols),
            Erase::ToStart => (0, col + 1),
            Erase::All => (0, self.cols),
        };
        let blank = self.blank();
        self.cells[row * self.cols + from..row * self.cols + to].fill(blank);
        if to == self.cols {
            self.set_wrapped(row, false);
        }
        self.touch(row);
        // Erasing ends a pending wrap: the character that set it is gone.
        self.pending_wrap = false;
    }

    fn erase_in_display(&mut self, e: Erase) {
        let blank = self.blank();
        let (row, col) = (self.row, self.col);
        match e {
            Erase::ToEnd => {
                self.cells[row * self.cols + col..(row + 1) * self.cols].fill(blank);
                self.cells[(row + 1) * self.cols..].fill(blank);
                // Erased text cannot be continued below, or a reflow would join rows.
                self.wrapped[row..].fill(false);
            }
            Erase::ToStart => {
                self.cells[..row * self.cols].fill(blank);
                self.cells[row * self.cols..row * self.cols + col + 1].fill(blank);
                self.wrapped[..row].fill(false);
            }
            // `ED 2` does not scroll: `Ctrl-L` must not fill the scrollback with blank lines.
            Erase::All => {
                self.cells.fill(blank);
                self.wrapped.fill(false);
            }
        }
        self.damage_all();
        self.pending_wrap = false;
    }

    fn attr(&mut self, s: Sgr) {
        match s {
            Sgr::Reset => self.attrs = Attributes::default(),
            Sgr::Bold => self.attrs.flags = self.attrs.flags.with(Flags::BOLD),
            Sgr::NoBold => self.attrs.flags = self.attrs.flags.without(Flags::BOLD),
            Sgr::Underline => self.attrs.flags = self.attrs.flags.with(Flags::UNDERLINE),
            Sgr::NoUnderline => self.attrs.flags = self.attrs.flags.without(Flags::UNDERLINE),
            Sgr::Reverse => self.attrs.flags = self.attrs.flags.with(Flags::REVERSE),
            Sgr::NoReverse => self.attrs.flags = self.attrs.flags.without(Flags::REVERSE),
            Sgr::Foreground(c) => self.attrs.fg = c,
            Sgr::Background(c) => self.attrs.bg = c,
        }
    }
}

#[cfg(test)]
mod tests;
