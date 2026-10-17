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
mod tests {
    use super::*;
    use crate::cell::Ansi;
    use crate::parse::{MAX_PER_BYTE, Parser};

    /// Drive a grid from real bytes through the real parser.
    fn feed(g: &mut Grid, s: &str) {
        let mut p = Parser::new();
        let mut out = [Op::Print('\0'); MAX_PER_BYTE];
        for &b in s.as_bytes() {
            let n = p.feed(b, &mut out);
            g.apply_all(&out[..n]);
        }
    }

    /// `select_all` takes every line the grid holds, scrollback included.
    #[test]
    fn select_all_takes_the_scrollback_too() {
        let mut g = Grid::new(20, 3);
        feed(&mut g, "one\r\ntwo\r\nthree\r\nfour\r\nfive");
        assert!(g.oldest_line() < g.top_line(), "there is scrollback to miss");

        assert!(g.select_all(), "something was selected");
        let text = g.selected_text().expect("a selection has text");
        for line in ["one", "two", "three", "four", "five"] {
            assert!(text.contains(line), "{line:?} is missing from {text:?}");
        }
    }

    /// `find_from` reports cell columns, not bytes (`é` is one cell and two bytes).
    #[test]
    fn find_from_reports_cells_not_bytes() {
        let mut g = Grid::new(30, 2);
        feed(&mut g, "ééxtarget");
        let line = g.top_line();
        assert_eq!(
            g.find_from("target", line, 0),
            Some((line, 3)),
            "two accents and an x are three cells, not five bytes"
        );
        g.select_from(line, 3);
        g.extend(line, 3 + "target".chars().count());
        assert_eq!(g.selected_text().as_deref(), Some("target"));

        assert_eq!(g.find_from("x", line, 1), Some((line, 2)), "the rest of the line is searched");
    }

    #[test]
    fn select_all_on_a_blank_grid_selects_nothing() {
        let mut g = Grid::new(20, 3);
        assert!(!g.select_all(), "an empty grid has nothing to select");
        assert!(!g.has_selection());
        assert_eq!(g.selected_text(), None, "and nothing to copy");
    }

    #[test]
    fn select_all_stops_at_the_text() {
        let mut g = Grid::new(30, 3);
        feed(&mut g, "one\r\ntwo\r\nshort");
        assert!(g.select_all());
        let sel = g.selection().expect("a selection");
        assert_eq!(sel.head.1, 5, "`short` is five cells, and the grid is thirty wide");
        assert_eq!(g.selected_text().as_deref(), Some("one\ntwo\nshort"));
    }

    #[test]
    fn find_from_walks_the_history() {
        let mut g = Grid::new(20, 3);
        feed(&mut g, "alpha\r\nbeta\r\ngamma\r\nbeta again");
        let first = g.find_from("beta", g.oldest_line(), 0).expect("the first beta");
        let next = g.find_from("beta", first.0, first.1 + 1).expect("the second beta");
        assert!(next.0 > first.0, "the second is on a later line: {first:?} then {next:?}");
        assert_eq!(g.find_from("zeta", g.oldest_line(), 0), None, "and what is absent is absent");
    }

    #[test]
    fn clearing_the_scrollback_keeps_the_screen_and_the_numbering() {
        let mut g = Grid::new(20, 3);
        feed(&mut g, "one\r\ntwo\r\nthree\r\nfour\r\nfive");
        let top = g.top_line();
        assert!(g.oldest_line() < top, "there is scrollback to clear");

        assert!(g.clear_scrollback(), "something was dropped");
        assert_eq!(g.top_line(), top, "the screen's absolute number did not move");
        assert_eq!(g.oldest_line(), top, "and the history now starts at the screen");
        assert_eq!(g.line_text(top).as_deref(), Some("three"), "the screen is untouched");
        assert!(!g.clear_scrollback(), "and a second clear has nothing to do");
    }

    #[test]
    fn a_word_is_a_run_of_non_blanks() {
        let mut g = Grid::new(30, 2);
        feed(&mut g, "ls --colour=auto /home");
        let line = g.top_line();

        assert!(g.select_word_at(line, 4), "inside the flag");
        assert_eq!(g.selected_text().as_deref(), Some("--colour=auto"));

        assert!(g.select_word_at(line, 0), "the first word");
        assert_eq!(g.selected_text().as_deref(), Some("ls"));

        assert!(!g.select_word_at(line, 2), "the space between them");
    }

    #[test]
    fn a_line_selection_stops_at_the_text() {
        let mut g = Grid::new(30, 2);
        feed(&mut g, "short");
        let line = g.top_line();
        assert!(g.select_line_at(line));
        let sel = g.selection().expect("a selection");
        assert_eq!(sel.anchor, (line, 0));
        assert_eq!(sel.head, (line, 5), "to the end of `short`, not to column 30");
        assert_eq!(g.selected_text().as_deref(), Some("short"));
    }

    fn line(g: &Grid, row: usize) -> alloc::string::String {
        let s: alloc::string::String = (0..g.cols()).map(|c| g.cell(row, c).unwrap().ch).collect();
        s.trim_end().into()
    }

    fn view(g: &Grid, top: u64, row: usize) -> alloc::string::String {
        let s: alloc::string::String =
            (0..g.cols()).map(|c| g.view_cell(top, row, c).map(|x| x.ch).unwrap_or(' ')).collect();
        s.trim_end().into()
    }

    #[test]
    fn a_wrapped_line_rejoins_on_a_widen_and_two_short_lines_do_not() {
        // The assertion the flag exists for: a rewrap that joined adjacent rows would also
        // rejoin the long line, so the short pair staying two rows separates them.
        let mut g = Grid::new(6, 6);
        feed(&mut g, "abcdefghij\r\n"); // 10 characters at width 6: one soft wrap
        feed(&mut g, "xx\r\n");
        feed(&mut g, "yy\r\n");
        assert_eq!((line(&g, 0), line(&g, 1)), ("abcdef".into(), "ghij".into()));
        assert!(g.row_wrapped(0), "the long line continues below");
        assert!(!g.row_wrapped(2), "and a short one does not");

        g.resize(12, 6);
        assert_eq!(line(&g, 0), "abcdefghij", "the soft wrap was undone");
        assert_eq!(line(&g, 1), "xx", "and the pair that was never one line is still two");
        assert_eq!(line(&g, 2), "yy");
    }

    #[test]
    fn a_rewrap_moves_the_breaks_and_never_the_count_of_lines() {
        let mut g = Grid::new(20, 4);
        for i in 0..10 {
            feed(&mut g, &alloc::format!("line{i} with a tail on it\r\n"));
        }
        let before = g.logical_lines();
        assert!(before >= 10, "ten lines and a partial one: {before}");

        g.resize(60, 4);
        assert_eq!(g.logical_lines(), before, "widening moved breaks, not lines");
        g.resize(9, 4);
        assert_eq!(g.logical_lines(), before, "and narrowing made more rows, not more lines");
        g.resize(20, 4);
        assert_eq!(g.logical_lines(), before, "and back where it started");
    }

    #[test]
    fn what_the_ring_drops_on_a_narrow_is_counted_rather_than_lost() {
        let mut g = Grid::new(20, 3);
        for _ in 0..900 {
            feed(&mut g, "0123456789012345678\r\n"); // 19 characters: one row at width 20
        }
        let before = g.logical_lines();
        assert!(before > 890, "the history is deep enough to hit the cap when halved: {before}");

        let r = g.resize(10, 3); // every line is now two rows
        let after = g.logical_lines();
        assert!(after < before, "the ring really did evict: {before} -> {after}");
        assert_eq!(
            after + r.evicted_lines(),
            before,
            "every line that went is accounted for by the eviction, not by the rewrap"
        );
    }

    #[test]
    fn a_line_feed_clears_a_stale_wrap() {
        // A row that once wrapped and was overwritten must stop being marked, or a widen joins it.
        let mut g = Grid::new(6, 4);
        feed(&mut g, "abcdefghij"); // row 0 "abcdef" wrapped -> row 1 "ghij"
        assert!(g.row_wrapped(0));
        feed(&mut g, "\x1b[1;1H"); // back into the wrapped row
        feed(&mut g, "xx\r\n"); // a short line, ended explicitly
        assert!(!g.row_wrapped(0), "the line feed ended it");

        g.resize(12, 4);
        assert_eq!(line(&g, 0), "xxcdef");
        assert_eq!(line(&g, 1), "ghij", "the row below was not joined onto it");
    }

    #[test]
    fn erasing_to_the_margin_ends_a_wrapped_row() {
        let mut g = Grid::new(6, 4);
        feed(&mut g, "abcdefghij");
        feed(&mut g, "\x1b[1;3H\x1b[K"); // row 0, column 2, erase to the margin
        assert!(!g.row_wrapped(0));

        g.resize(12, 4);
        assert_eq!(line(&g, 0), "ab");
        assert_eq!(line(&g, 1), "ghij");
    }

    #[test]
    fn erasing_the_display_ends_the_wrapped_rows_it_blanks() {
        // Each `ED` arm clears the flags of the rows it blanked; `ToStart` leaves this row's own.
        let mut g = Grid::new(6, 4);
        feed(&mut g, "abcdefghij\r\n"); // rows 0-1, row 0 wrapped
        feed(&mut g, "klmnopqrst"); // rows 2-3, row 2 wrapped
        assert!(g.row_wrapped(0) && g.row_wrapped(2));

        feed(&mut g, "\x1b[2;1H\x1b[J");
        assert!(g.row_wrapped(0), "the row above the erase is untouched");
        assert!(!g.row_wrapped(2), "the erased row continues nothing");

        let mut g = Grid::new(6, 4);
        feed(&mut g, "abcdefghij\r\n");
        feed(&mut g, "klmnopqrst");
        feed(&mut g, "\x1b[3;1H\x1b[1J");
        assert!(!g.row_wrapped(0), "an erased row continues nothing");
        assert!(g.row_wrapped(2), "this row's tail was not erased, so it still continues");

        let mut g = Grid::new(6, 4);
        feed(&mut g, "abcdefghij\r\n");
        feed(&mut g, "\x1b[2J");
        assert!(!g.row_wrapped(0), "a cleared screen continues nothing");
    }

    #[test]
    fn narrowing_then_widening_puts_the_text_back() {
        let mut g = Grid::new(20, 6);
        feed(&mut g, "the quick brown fox\r\nshort\r\n");
        g.resize(7, 6);
        g.resize(20, 6);
        assert_eq!(line(&g, 0), "the quick brown fox");
        assert_eq!(line(&g, 1), "short");
        assert_eq!(g.cursor(), (2, 0), "and the cursor is still on the line it was on");
    }

    #[test]
    fn a_reflow_maps_a_scrolled_back_anchor_onto_the_same_text() {
        let mut g = Grid::new(5, 3);
        for i in 0..8 {
            feed(&mut g, &alloc::format!("aaaa{i}bbbb\r\n")); // 9 chars at width 5: two rows
        }
        let anchor = g.oldest_line() + 4;
        let want = view(&g, anchor, 0);
        assert_eq!(want, "aaaa2", "the first half of a line the old width broke in two");

        let r = g.resize(10, 3);
        let now = view(&g, r.map_line(anchor), 0);
        assert!(now.starts_with(&want), "anchor landed on {now:?}, wanted {want:?} first");
        assert_ne!(r.map_line(anchor), anchor, "the numbering really did move");
        assert!(
            !view(&g, anchor, 0).starts_with(&want),
            "and the unmapped number shows something else, so this test can tell them apart"
        );
    }

    #[test]
    fn the_cursor_follows_the_character_it_was_on() {
        let mut g = Grid::new(10, 4);
        feed(&mut g, "hello world"); // wraps: "hello worl" then "d"
        assert_eq!(g.cursor(), (1, 1));
        g.resize(20, 4);
        assert_eq!(line(&g, 0), "hello world");
        assert_eq!(g.cursor(), (0, 11), "still just after the text it was after");
    }

    #[test]
    fn resizing_an_idle_screen_pushes_nothing_into_the_scrollback() {
        let mut g = Grid::new(20, 10);
        feed(&mut g, "/home> ");
        g.resize(40, 5);
        assert_eq!(g.scrollback().len(), 0, "nine blank rows became nine lines of history");
        assert_eq!(line(&g, 0), "/home>");
        assert_eq!(g.cursor(), (0, 7), "the prompt still has the cursor after it");
    }

    #[test]
    fn a_resize_to_the_same_shape_changes_nothing() {
        let mut g = Grid::new(8, 4);
        feed(&mut g, "abc\r\ndef");
        let before = (g.cursor(), g.top_line(), line(&g, 0), line(&g, 1));
        let r = g.resize(8, 4);
        assert_eq!((g.cursor(), g.top_line(), line(&g, 0), line(&g, 1)), before);
        assert_eq!(r.map_line(7), 7, "the identity map, not an empty one that answers zero");
    }

    #[test]
    fn a_coloured_run_to_the_right_margin_is_not_trimmed_away() {
        // `trim_trailing_blanks` drops only never-written cells: a bar painted to the margin
        // was written on purpose.
        let mut g = Grid::new(6, 3);
        feed(&mut g, "\x1b[44m\x1b[K"); // erase the line under blue: six blue spaces
        // A real resize: an unchanged shape takes `resize`'s early return and skips the trim.
        g.resize(8, 3);
        let blue = (0..6).filter(|c| g.cell(0, *c).unwrap().attrs.bg == Colour::Ansi(Ansi::Blue));
        assert_eq!(blue.count(), 6, "the painted run survived the rewrap");
    }

    #[test]
    fn printing_advances_the_cursor_and_stores_the_characters() {
        let mut g = Grid::new(10, 3);
        feed(&mut g, "hi");
        assert_eq!(line(&g, 0), "hi");
        assert_eq!(g.cursor(), (0, 2));
    }

    #[test]
    fn a_full_line_then_a_newline_does_not_leave_a_blank_one() {
        // The reason wrapping is deferred: wrap-on-write double-spaces a program printing exactly
        // `cols` characters per line.
        let mut g = Grid::new(4, 4);
        feed(&mut g, "abcd\r\nxy");
        assert_eq!(line(&g, 0), "abcd");
        assert_eq!(line(&g, 1), "xy", "a blank line appeared between them");
        assert_eq!(line(&g, 2), "");
    }

    #[test]
    fn a_line_feed_moves_down_and_leaves_the_column_alone() {
        let mut g = Grid::new(10, 3);
        feed(&mut g, "abc\ndef");
        assert_eq!(line(&g, 0), "abc");
        assert_eq!(line(&g, 1), "   def", "the line feed also returned to column 0");
    }

    #[test]
    fn the_character_after_a_full_line_wraps() {
        let mut g = Grid::new(4, 4);
        feed(&mut g, "abcde");
        assert_eq!(line(&g, 0), "abcd");
        assert_eq!(line(&g, 1), "e");
        assert_eq!(g.cursor(), (1, 1));
    }

    #[test]
    fn a_wrapped_line_keeps_filling_rather_than_wrapping_every_character() {
        let mut g = Grid::new(4, 4);
        feed(&mut g, "abcdefg");
        assert_eq!(line(&g, 0), "abcd");
        assert_eq!(line(&g, 1), "efg", "the wrapped line did not keep filling");
        assert_eq!(line(&g, 2), "");
        assert_eq!(g.cursor(), (1, 3));
    }

    #[test]
    fn the_cursor_rests_on_the_last_column_rather_than_past_it() {
        let mut g = Grid::new(4, 4);
        feed(&mut g, "abcd");
        assert_eq!(g.cursor(), (0, 3), "the cursor left the screen");
    }

    #[test]
    fn movement_cancels_a_pending_wrap() {
        let mut g = Grid::new(4, 4);
        feed(&mut g, "abcd\rZ");
        assert_eq!(line(&g, 0), "Zbcd");
        assert_eq!(line(&g, 1), "", "the write wrapped after a carriage return");

        let mut g = Grid::new(4, 4);
        feed(&mut g, "abcd\x1b[1;1HZ");
        assert_eq!(line(&g, 0), "Zbcd");
    }

    #[test]
    fn a_newline_at_the_bottom_scrolls_and_the_line_goes_to_scrollback() {
        let mut g = Grid::new(4, 2);
        feed(&mut g, "one\r\ntwo\r\nsix");
        assert_eq!(line(&g, 0), "two");
        assert_eq!(line(&g, 1), "six", "the third line did not land on the bottom row");
        assert_eq!(g.scrollback().len(), 1);
        let first: alloc::string::String = g.scrollback().next().unwrap().cells.iter().map(|c| c.ch).collect();
        assert_eq!(first.trim_end(), "one");
    }

    #[test]
    fn scrollback_is_bounded() {
        let mut g = Grid::new(4, 1);
        for i in 0..(SCROLLBACK + 50) {
            feed(&mut g, &alloc::format!("{}\r\n", i % 10));
        }
        assert_eq!(g.scrollback().len(), SCROLLBACK, "the ring grew past its bound");
    }

    #[test]
    fn a_scrolled_back_view_reads_from_the_scrollback_and_the_screen_at_once() {
        // The seam: a viewport straddling the boundary serves top rows from history, bottom
        // rows from the screen, which are stored differently.
        let mut g = Grid::new(4, 2);
        for i in 0..6 {
            feed(&mut g, &alloc::format!("{i}\r\n"));
        }
        assert_eq!(g.top_line(), 5, "five lines scrolled off");
        assert_eq!(g.oldest_line(), 0, "and none evicted");

        let at = |top: u64, row: usize| g.view_cell(top, row, 0).map(|c| c.ch);
        assert_eq!((at(5, 0), at(5, 1)), (Some('5'), Some(' ')), "the bottom: the live screen");
        assert_eq!((at(3, 0), at(3, 1)), (Some('3'), Some('4')), "wholly in the scrollback");
        assert_eq!((at(4, 0), at(4, 1)), (Some('4'), Some('5')), "straddling: history above, screen below",);
    }

    #[test]
    fn the_view_is_anchored_to_a_line_so_output_does_not_drag_it() {
        let mut g = Grid::new(4, 2);
        for i in 0..6 {
            feed(&mut g, &alloc::format!("{i}\r\n"));
        }
        let before: alloc::vec::Vec<Option<char>> = (0..2).map(|r| g.view_cell(2, r, 0).map(|c| c.ch)).collect();

        for i in 6..9 {
            feed(&mut g, &alloc::format!("{i}\r\n"));
        }
        let after: alloc::vec::Vec<Option<char>> = (0..2).map(|r| g.view_cell(2, r, 0).map(|c| c.ch)).collect();
        assert_eq!(before, after, "the anchored view moved when output arrived");
        assert_eq!(before, [Some('2'), Some('3')]);
    }

    #[test]
    fn an_anchor_the_scrollback_has_evicted_clamps_to_the_oldest_line_kept() {
        let mut g = Grid::new(4, 1);
        for i in 0..(SCROLLBACK + 50) {
            feed(&mut g, &alloc::format!("{}\r\n", i % 10));
        }
        assert_eq!(g.oldest_line(), (SCROLLBACK + 50 - SCROLLBACK) as u64, "50 evicted");
        assert_eq!(g.clamp_view(0), g.oldest_line(), "an evicted anchor");
        assert_eq!(g.clamp_view(u64::MAX), g.top_line(), "and one past the bottom");
        assert_eq!(g.clamp_view(60), 60, "one that is still there is left alone");
    }

    #[test]
    fn the_cursor_is_not_in_a_view_scrolled_away_from_it() {
        // Scrolling back does not move the cursor; drawing it at its screen row would invert
        // a cell of history.
        let mut g = Grid::new(4, 2);
        for i in 0..6 {
            feed(&mut g, &alloc::format!("{i}\r\n"));
        }
        assert_eq!(g.cursor(), (1, 0));
        assert_eq!(g.view_cursor(g.top_line()), Some((1, 0)), "following: where it always was");
        assert_eq!(g.view_cursor(4), None, "one row past the bottom is still out of view");
        assert_eq!(g.view_cursor(5), Some((1, 0)), "and the row before it is the last in view");
        assert_eq!(g.view_cursor(0), None, "scrolled to the top: nowhere near it");
    }

    #[test]
    fn cursor_addressing_clamps_and_never_scrolls() {
        let mut g = Grid::new(10, 3);
        feed(&mut g, "\x1b[99;99H");
        assert_eq!(g.cursor(), (2, 9));
        assert_eq!(g.scrollback().len(), 0, "addressing the cursor scrolled the screen");

        // One past the end, not far past: `99` clamps to 2 and `98 % 3` is also 2, so only row 4
        // separates clamping from a modulo.
        let mut g = Grid::new(10, 3);
        feed(&mut g, "\x1b[4;11H");
        assert_eq!(g.cursor(), (2, 9), "the cursor wrapped instead of clamping");
    }

    #[test]
    fn relative_movement_clamps_at_every_edge() {
        let mut g = Grid::new(10, 3);
        feed(&mut g, "\x1b[10A\x1b[10D");
        assert_eq!(g.cursor(), (0, 0), "moving up and left past the origin");
        feed(&mut g, "\x1b[10B\x1b[10C");
        assert_eq!(g.cursor(), (2, 9), "moving down and right past the corner");
        assert_eq!(g.scrollback().len(), 0, "relative movement scrolled");
    }

    #[test]
    fn backspace_stops_at_the_left_margin() {
        let mut g = Grid::new(4, 2);
        feed(&mut g, "ab\r\n\x08\x08\x08");
        assert_eq!(g.cursor(), (1, 0));
    }

    #[test]
    fn tabs_advance_to_the_next_stop_and_stop_at_the_edge() {
        let mut g = Grid::new(20, 2);
        feed(&mut g, "a\t");
        assert_eq!(g.cursor(), (0, TAB_WIDTH));
        feed(&mut g, "\t");
        assert_eq!(g.cursor(), (0, TAB_WIDTH * 2));
        let mut g = Grid::new(10, 2);
        feed(&mut g, "\t\t\t\t");
        assert_eq!(g.cursor(), (0, 9));
    }

    #[test]
    fn erasing_fills_with_the_current_background_not_the_default() {
        let mut g = Grid::new(4, 2);
        feed(&mut g, "\x1b[44m\x1b[2K");
        for c in 0..4 {
            let cell = g.cell(0, c).unwrap();
            assert_eq!(cell.ch, ' ');
            assert_eq!(cell.attrs.bg, Colour::Ansi(Ansi::Blue), "column {c} kept the default");
        }
    }

    #[test]
    fn an_erased_cell_carries_no_ink_attributes() {
        let mut g = Grid::new(4, 2);
        feed(&mut g, "\x1b[1;4;44m\x1b[2K");
        let cell = g.cell(0, 0).unwrap();
        assert!(!cell.attrs.flags.contains(Flags::BOLD));
        assert!(!cell.attrs.flags.contains(Flags::UNDERLINE));
        assert_eq!(cell.attrs.fg, Colour::Default);
    }

    #[test]
    fn erasing_while_reversed_fills_with_the_foreground() {
        // `blank()` keeps `REVERSE` and drops `BOLD`/`UNDERLINE`: that retention is what makes
        // "reverse, erase line" paint a bar, and removing it would otherwise leave tests green.
        let p = crate::cell::Palette::default();
        let mut g = Grid::new(4, 1);
        feed(&mut g, "\x1b[7m\x1b[2K");
        let (fg, bg) = g.cell(0, 0).unwrap().attrs.resolve(&p);
        assert_eq!(bg, p.foreground, "an erase under reverse did not paint a bar");
        assert_eq!(fg, p.background);

        let mut g = Grid::new(4, 1);
        feed(&mut g, "\x1b[2K");
        assert_eq!(g.cell(0, 0).unwrap().attrs.resolve(&p).1, p.background);
    }

    #[test]
    fn erase_in_line_covers_exactly_its_extent() {
        let mut g = Grid::new(6, 2);
        feed(&mut g, "abcdef\x1b[1;3H\x1b[K"); // cursor at column 2, erase to end
        assert_eq!(line(&g, 0), "ab");

        let mut g = Grid::new(6, 2);
        feed(&mut g, "abcdef\x1b[1;3H\x1b[1K"); // erase to start, inclusive of the cursor
        assert_eq!(line(&g, 0), "   def", "`EL 1` left the cell under the cursor");

        let mut g = Grid::new(6, 2);
        feed(&mut g, "abcdef\x1b[1;3H\x1b[2K");
        assert_eq!(line(&g, 0), "");
    }

    #[test]
    fn erase_in_display_covers_exactly_its_extent() {
        let mut g = Grid::new(4, 3);
        feed(&mut g, "aaaa\r\nbbbb\r\ncccc\x1b[2;3H\x1b[J"); // row 1 col 2, erase to end
        assert_eq!(line(&g, 0), "aaaa");
        assert_eq!(line(&g, 1), "bb");
        assert_eq!(line(&g, 2), "");

        let mut g = Grid::new(4, 3);
        feed(&mut g, "aaaa\r\nbbbb\r\ncccc\x1b[2;3H\x1b[1J");
        assert_eq!(line(&g, 0), "");
        assert_eq!(line(&g, 1), "   b");
        assert_eq!(line(&g, 2), "cccc");
    }

    #[test]
    fn clearing_the_screen_does_not_push_it_into_scrollback() {
        let mut g = Grid::new(4, 3);
        feed(&mut g, "aaaa\r\nbbbb\x1b[2J");
        assert_eq!(g.scrollback().len(), 0);
        assert_eq!(line(&g, 0), "");
    }

    #[test]
    fn attributes_accumulate_and_reset_clears_all_of_them() {
        let mut g = Grid::new(4, 2);
        feed(&mut g, "\x1b[1m\x1b[4m\x1b[31m\x1b[44m");
        let a = g.attributes();
        assert!(a.flags.contains(Flags::BOLD.with(Flags::UNDERLINE)));
        assert_eq!(a.fg, Colour::Ansi(Ansi::Red));
        assert_eq!(a.bg, Colour::Ansi(Ansi::Blue));
        feed(&mut g, "\x1b[m");
        assert_eq!(g.attributes(), Attributes::default(), "reset left something behind");
    }

    #[test]
    fn a_character_carries_the_attributes_in_force_when_it_was_written() {
        let mut g = Grid::new(4, 2);
        feed(&mut g, "\x1b[31ma\x1b[32mb");
        assert_eq!(g.cell(0, 0).unwrap().attrs.fg, Colour::Ansi(Ansi::Red));
        assert_eq!(g.cell(0, 1).unwrap().attrs.fg, Colour::Ansi(Ansi::Green));
    }

    #[test]
    fn damage_is_the_rows_that_changed_and_clears_when_taken() {
        let mut g = Grid::new(10, 4);
        feed(&mut g, "\x1b[3;1H");
        g.take_damage(); // drop the initial full-screen damage *and* settle the cursor on row 2
        feed(&mut g, "x");
        assert_eq!(g.take_damage(), alloc::vec![2], "only the written row should be dirty");
        assert_eq!(g.take_damage(), alloc::vec![], "damage was not cleared");

        feed(&mut g, "y");
        assert_eq!(g.take_damage(), alloc::vec![2]);
    }

    #[test]
    fn a_scroll_damages_every_row() {
        let mut g = Grid::new(4, 3);
        feed(&mut g, "a\r\nb\r\nc");
        g.take_damage();
        feed(&mut g, "\r\nd");
        assert_eq!(g.take_damage(), alloc::vec![0, 1, 2]);
    }

    #[test]
    fn erasing_the_display_damages_every_row_it_touched() {
        let mut g = Grid::new(4, 3);
        feed(&mut g, "aaaa\r\nbbbb\r\ncccc");
        g.take_damage();
        feed(&mut g, "\x1b[2J");
        assert_eq!(g.take_damage(), alloc::vec![0, 1, 2]);

        let mut g = Grid::new(4, 3);
        feed(&mut g, "aaaa\r\nbbbb\r\ncccc\x1b[2;3H");
        g.take_damage();
        feed(&mut g, "\x1b[J");
        let d = g.take_damage();
        assert!(d.contains(&1) && d.contains(&2), "rows below the cursor were not damaged: {d:?}");
    }

    #[test]
    fn moving_the_cursor_damages_the_row_it_left_and_the_one_it_reached() {
        // The render paints the cursor into its cell, so the row it left keeps an inverted block
        // until repainted.
        let mut g = Grid::new(10, 4);
        g.take_damage();
        feed(&mut g, "\x1b[2;5H");
        assert_eq!(g.take_damage(), alloc::vec![0, 1], "left row 0, arrived in row 1");

        feed(&mut g, "\x1b[C");
        assert_eq!(g.take_damage(), alloc::vec![1]);

        assert_eq!(g.take_damage(), alloc::vec![]);
    }

    #[test]
    fn a_wrap_damages_the_row_the_cursor_left() {
        // `abcd` rests the cursor on the last column and `e` wraps it; reporting only row 1 would
        // leave a phantom block at the end of row 0.
        let mut g = Grid::new(4, 3);
        feed(&mut g, "abcd");
        g.take_damage();
        feed(&mut g, "e");
        assert_eq!(g.take_damage(), alloc::vec![0, 1], "the row the cursor left was not repainted");
    }

    #[test]
    fn a_degenerate_size_is_clamped_rather_than_panicking() {
        let mut g = Grid::new(0, 0);
        feed(&mut g, "abc\r\n\x1b[9;9H\x1b[2J");
        assert_eq!(g.cols(), 1);
        assert_eq!(g.rows(), 1);
        assert_eq!(g.cursor(), (0, 0));
    }

    #[test]
    fn a_press_selects_nothing_until_it_is_dragged() {
        let mut g = Grid::new(20, 4);
        feed(&mut g, "hello world");
        g.select_from(0, 0);
        assert!(g.selection().is_some(), "the gesture is in progress");
        assert_eq!(g.selected_text(), None);
        g.extend(0, 5);
        assert_eq!(g.selected_text().as_deref(), Some("hello"));
    }

    #[test]
    fn dragging_backwards_selects_the_same_text() {
        let mut g = Grid::new(20, 4);
        feed(&mut g, "hello world");
        g.select_from(0, 5);
        g.extend(0, 0);
        assert_eq!(g.selected_text().as_deref(), Some("hello"));
    }

    #[test]
    fn a_selection_across_a_hard_break_keeps_the_newline() {
        let mut g = Grid::new(20, 4);
        feed(&mut g, "one\r\ntwo");
        g.select_from(0, 0);
        g.extend(1, 3);
        assert_eq!(g.selected_text().as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn a_soft_wrap_does_not_become_a_newline() {
        // A soft wrap is one line of text, and pasting it with a newline inserts a break the
        // program never wrote.
        let mut g = Grid::new(4, 4);
        feed(&mut g, "abcdefgh");
        assert!(g.row_wrapped(0), "precondition: the first row is a soft wrap");
        g.select_from(0, 0);
        g.extend(1, 4);
        assert_eq!(g.selected_text().as_deref(), Some("abcdefgh"));
    }

    #[test]
    fn trailing_blanks_are_not_selected() {
        // Cells past the end of a line were never written; keeping them pastes a screen's width
        // of spaces.
        let mut g = Grid::new(20, 4);
        feed(&mut g, "hi\r\nthere");
        g.select_from(0, 0);
        g.extend(1, 5);
        assert_eq!(g.selected_text().as_deref(), Some("hi\nthere"));
    }

    #[test]
    fn a_selection_in_the_history_names_the_same_text_as_output_arrives() {
        // Absolute positions, so a selection in the history does not drift while output arrives.
        let mut g = Grid::new(20, 2);
        feed(&mut g, "first\r\nsecond\r\nthird\r\n");
        let top = g.top_line();
        g.select_from(top - 2, 0);
        g.extend(top - 2, 5);
        assert_eq!(g.selected_text().as_deref(), Some("first"));
        feed(&mut g, "fourth\r\nfifth\r\n");
        assert_eq!(
            g.selected_text().as_deref(),
            Some("first"),
            "two more lines scrolled past and the selection still names the same text"
        );
    }

    #[test]
    fn a_selection_survives_a_rewrap() {
        // A logical line's text is invariant across a rewrap, so an offset into it survives. At
        // width 3 "defg" sits in row 1 at a different column than at width 4, which
        // `map_position` follows and `map_line` alone cannot.
        let mut g = Grid::new(4, 4);
        feed(&mut g, "abcdefgh");
        g.select_from(0, 3);
        g.extend(1, 3);
        assert_eq!(g.selected_text().as_deref(), Some("defg"));
        g.resize(3, 4);
        assert_eq!(
            g.selected_text().as_deref(),
            Some("defg"),
            "the same characters, at a width where they sit in different cells"
        );
    }

    #[test]
    fn clearing_a_selection_that_is_not_there_damages_nothing() {
        let mut g = Grid::new(20, 4);
        feed(&mut g, "hello");
        let _ = g.take_damage();
        assert!(!g.clear_selection());
        assert!(g.take_damage().is_empty(), "nothing was selected, so nothing changed");
        g.select_from(0, 0);
        g.extend(0, 3);
        let _ = g.take_damage();
        assert!(g.clear_selection());
        assert!(!g.take_damage().is_empty(), "the highlight has to be painted out");
    }

    #[test]
    fn extending_without_an_anchor_selects_nothing() {
        let mut g = Grid::new(20, 4);
        feed(&mut g, "hello");
        g.extend(0, 3);
        assert_eq!(g.selection(), None);
        assert_eq!(g.selected_text(), None);
    }

    #[test]
    fn a_selection_reaching_past_the_history_stops_at_what_exists() {
        let mut g = Grid::new(10, 2);
        for i in 0..(SCROLLBACK + 10) {
            feed(&mut g, "x\r\n");
            let _ = i;
        }
        g.select_from(0, 0);
        g.extend(2, 1);
        let _ = g.selected_text();
    }
    /// `has_selection` agrees with `selected_text().is_some()` in every state. An empty range
    /// is the case they could disagree in.
    #[test]
    fn has_selection_is_the_cheap_half_of_selected_text() {
        let mut g = Grid::new(20, 4);
        feed(&mut g, "hello world");
        assert!(!g.has_selection(), "nothing swept out");
        assert_eq!(g.selected_text(), None);
        g.select_from(0, 3);
        assert_eq!(g.selected_text(), None, "a press selects no text");
        assert!(!g.has_selection(), "…and this must agree, which selection().is_some() would not");
        assert!(g.selection().is_some(), "the range does exist — that is the trap");
        g.extend(0, 8);
        assert!(g.selected_text().is_some());
        assert!(g.has_selection());
        g.clear_selection();
        assert_eq!(g.selected_text(), None);
        assert!(!g.has_selection());
    }
}
