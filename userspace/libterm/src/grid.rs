//! The cell grid: what [`Op`]s do to a screen, and where lines go when they leave it.
//!
//! The other half of A3's split. The parser decides what a byte *means*; this decides what it
//! *does*, and the two are tested apart so an escape-sequence bug and a wrapping bug cannot
//! present identically.
//!
//! ## Where terminals are subtly wrong, and what this does instead
//!
//! - **Wrapping is deferred.** Writing to the last column leaves the cursor *on* it with a
//!   pending flag; the next character wraps. See [`Grid::print`].
//! - **Cursor addressing clamps, it does not wrap or scroll.** `CUP` past the last row is the
//!   last row.
//! - **Erasing fills with the current background**, not with the default one.
//! - **Scrolling moves a line into scrollback** rather than discarding it, and the scrollback
//!   is a ring with a bound.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use crate::cell::{Attributes, Cell, Colour, Flags};
use crate::parse::{Erase, Op, Sgr};

/// Columns a terminal starts at.
///
/// **A starting size since M9 Part D, not a fixed one.** This said "fixed because M6 owns move
/// and resize", then "fixed, and Part D is where it stops being": [`Grid::resize`] is that, and
/// what made it possible was the thing the old note named as the obstacle — the scrollback now
/// records which of its rows were soft wraps ([`Line::wrapped`]), so it can be re-wrapped rather
/// than merely re-cut.
pub const COLS: usize = 80;
/// Rows a terminal starts at. See [`COLS`].
pub const ROWS: usize = 24;

/// Lines retained above the screen.
///
/// A ring, so a program that prints forever costs a bounded amount rather than growing until
/// something dies. A thousand lines is ~640 KiB of `Cell` at 80 columns, which is the same
/// order as one of the client's frame buffers.
pub const SCROLLBACK: usize = 1000;

/// Columns between tab stops.
///
/// Fixed rather than a settable stop list: nothing in this system emits `HTS`, and a stop list
/// that only ever holds multiples of eight is a table pretending to be a decision.
pub const TAB_WIDTH: usize = 8;

/// A line of cells, and whether the line below it continues the same text.
///
/// **The flag is what makes a reflow possible at all.** A scrollback of already-wrapped rows
/// with no record of *which* wraps were soft is a scrollback that cannot be re-wrapped: joining
/// every adjacent pair would merge paragraphs that were never one line, and joining none would
/// leave the old width's breaks frozen into the history at the new width. M9 Part D is where
/// the flag arrives; before it, `resize` had nothing to be correct with.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Line {
    /// The cells, as wide as the grid was when the line was produced.
    pub cells: Vec<Cell>,
    /// **A soft wrap**: text ran past the right margin and continues on the next line. False
    /// for a line that ended because the program said so.
    pub wrapped: bool,
}

/// How a [`resize`](Grid::resize) moved the lines, so an anchor into them can follow.
///
/// **A view is anchored to an absolute line number** and a reflow changes how many lines exist,
/// so every anchor held outside the grid — the scrolled-back viewport, and nothing else today —
/// is stale the moment the width changes. This maps the old numbering onto the new, which is
/// what lets the visible region go on showing the same *text* across a resize rather than the
/// same arithmetic.
pub struct Reflow {
    /// The absolute number of the oldest line this maps.
    base: u64,
    /// The new absolute number of each old line, from `base` upward. Empty for a resize that
    /// moved nothing, where the mapping is the identity.
    to: Vec<u64>,
}

impl Reflow {
    /// Where the line that was numbered `old` is now.
    ///
    /// **Total, and clamping at both ends**, because an anchor is not guaranteed to name a line
    /// that still exists — the scrollback evicts, and a caller holding a number from before an
    /// eviction is the ordinary case [`clamp_view`](Grid::clamp_view) already exists for.
    pub fn map_line(&self, old: u64) -> u64 {
        if self.to.is_empty() {
            return old;
        }
        let i = old.saturating_sub(self.base) as usize;
        self.to[i.min(self.to.len() - 1)]
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
    ///
    /// Parallel to the rows rather than stored with the cells, for the same reason `dirty` is:
    /// `cells` is one flat buffer that `copy_within` shifts on a scroll, and a per-row field
    /// inside it would have to be threaded through every index calculation in this file.
    wrapped: Vec<bool>,
    /// Cursor row, always `< rows`.
    row: usize,
    /// Cursor column, always `< cols`.
    col: usize,
    /// **Deferred wrap.** Set when a character lands in the last column; the *next* character
    /// wraps. See [`Grid::print`] for why this exists rather than wrapping on write.
    pending_wrap: bool,
    /// Attributes the next printed character takes.
    attrs: Attributes,
    /// Rows changed since [`Grid::take_damage`].
    dirty: Vec<bool>,
    /// Where the cursor was when damage was last taken.
    ///
    /// The render paints the cursor **into its cell**, so moving it changes two cells' pixels
    /// while changing no cell's *contents*. Remembering the reported position is what lets
    /// [`take_damage`](Grid::take_damage) name the row that has to un-invert.
    cursor_drawn: (usize, usize),
    /// How many lines have ever scrolled off the top — **the absolute line number of the
    /// screen's first row**.
    ///
    /// This is what a scrolled-back view is anchored to, and the reason it is anchored to an
    /// absolute number rather than to "n lines above the bottom": output arriving while the
    /// user is reading history pushes lines into the scrollback, so a bottom-relative offset
    /// would make the text creep upward under them at exactly the moment they are trying to
    /// read it. Anchored here, a view showing lines 40..64 goes on showing lines 40..64
    /// however much arrives below.
    ///
    /// It counts *lines produced*, not lines retained, so it does not move when the bounded
    /// scrollback evicts its oldest line. [`oldest_line`](Grid::oldest_line) is the other end.
    scrolled: u64,
}

/// Drop the run of untouched cells at the end of a logical line.
///
/// **Only exactly-blank cells**, never a space a program wrote with a background colour: a
/// coloured region reaching the right margin is content, and trimming it would repaint a bar the
/// program drew. `Cell::BLANK` is the one value that means "never written".
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
    /// An empty grid of `cols` × `rows`, cursor at the origin.
    ///
    /// Both are clamped to at least 1: a zero-column grid has no valid cursor position, and
    /// every operation below would need a special case for a screen that cannot hold anything.
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
        if row < self.rows && col < self.cols {
            Some(self.cells[row * self.cols + col])
        } else {
            None
        }
    }

    /// Lines that have scrolled off, oldest first.
    pub fn scrollback(&self) -> impl ExactSizeIterator<Item = &Line> {
        self.scrollback.iter()
    }

    /// Whether screen row `row`'s text continues on the row below.
    ///
    /// The screen's half of [`Line::wrapped`], which is what a reflow reads before the rows
    /// ever reach the scrollback.
    pub fn row_wrapped(&self, row: usize) -> bool {
        self.wrapped.get(row).copied().unwrap_or(false)
    }

    /// The absolute line number of the screen's first row.
    ///
    /// A view anchored here is at the bottom — following the output, which is what a terminal
    /// does unless the user has scrolled away from it.
    pub fn top_line(&self) -> u64 {
        self.scrolled
    }

    /// The absolute line number of the oldest line still retained.
    ///
    /// It moves as the bounded scrollback evicts, which is why a view holds a line number and
    /// asks [`clamp_view`](Grid::clamp_view) rather than assuming its anchor survives.
    pub fn oldest_line(&self) -> u64 {
        self.scrolled - self.scrollback.len() as u64
    }

    /// `top` brought inside the history that still exists.
    pub fn clamp_view(&self, top: u64) -> u64 {
        top.clamp(self.oldest_line(), self.top_line())
    }

    /// The cell at viewport `(row, col)` when the viewport's first line is absolute line `top`.
    ///
    /// The one place the two halves of the history meet: a viewport row is served from the
    /// scrollback or from the live screen depending only on where its line number falls.
    /// `None` outside the grid's width, or for a line that no longer exists — total, like
    /// [`cell`](Grid::cell), so a caller with a stale anchor draws blanks rather than panicking.
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

    /// Where the cursor falls in a viewport whose first line is `top`, if it is in view at all.
    ///
    /// **`None` when it is not**, and that is the point: scrolling back does not move the
    /// cursor, so a render that drew it at its screen row regardless would invert a cell of
    /// somebody's history, several screens above where the cursor actually is.
    pub fn view_cursor(&self, top: u64) -> Option<(usize, usize)> {
        let row = (self.scrolled + self.row as u64).checked_sub(top)?;
        (row < self.rows as u64).then_some((row as usize, self.col))
    }

    /// Which rows changed since this was last called, and clear the record.
    ///
    /// **Per row, not one rectangle.** A keystroke dirties one row, and the render unions what
    /// it is given — so a grid that reported a bounding box would make a status line at the
    /// bottom and a prompt at the top repaint everything between them.
    ///
    /// **A moved cursor damages two rows**: the one it left and the one it arrived in. The
    /// render paints the cursor by inverting the cell it sits on, so moving it changes pixels
    /// in a cell whose *contents* did not change — and the row it left must repaint or the
    /// inversion stays behind as a phantom block. That is the same rule the compositor follows
    /// for the pointer, and for the same reason: something drawn *over* a surface rather than
    /// composited into it owns both of its positions.
    ///
    /// An earlier version reported nothing for cursor movement, with a rationale that would
    /// have held if the cursor were a separate overlay pass. A5 made it per-cell and the
    /// rationale stopped being true — each half defensible, the pair wrong (PR #190 review).
    ///
    /// **Calling this means "I am about to draw these rows."** The cursor bookkeeping updates
    /// here, so a caller that takes damage and does not render leaves the record ahead of the
    /// screen — the same contract the dirty flags themselves have always had.
    pub fn take_damage(&mut self) -> Vec<usize> {
        if self.cursor_drawn != (self.row, self.col) {
            let (was_row, _) = self.cursor_drawn;
            self.touch(was_row);
            self.touch(self.row);
            self.cursor_drawn = (self.row, self.col);
        }
        let rows: Vec<usize> =
            self.dirty.iter().enumerate().filter(|(_, d)| **d).map(|(i, _)| i).collect();
        self.dirty.iter_mut().for_each(|d| *d = false);
        rows
    }

    /// Re-lay the whole terminal at `cols` × `rows`, rewrapping the history.
    ///
    /// **The constant this file opens with stops being one here** (M9 Part D). It is the largest
    /// operation in this crate and the reason the part is the milestone's largest, so what it
    /// does is worth stating in order:
    ///
    /// 1. **Everything becomes logical lines again.** The scrollback and the screen are one
    ///    sequence of rows; consecutive rows joined by [`Line::wrapped`] were one line before the
    ///    old width broke them, and are one line again. Trailing blanks go with the break they
    ///    padded — a line that ends is trimmed, so narrowing does not push a run of spaces onto
    ///    a row of its own.
    /// 2. **Each logical line is re-broken at the new width**, every piece but the last marked
    ///    wrapped. This is the step that a `resize` without the flag could not do correctly in
    ///    either direction: joining every pair merges paragraphs, joining none freezes the old
    ///    width's breaks into the history for ever.
    /// 3. **The last `rows` of the result are the screen**, the rest is scrollback, and the
    ///    cursor follows the character it was on — clamped into the screen if the rewrap pushed
    ///    its line above the top, which only a shrink with content below the cursor can do.
    ///
    /// **The screen's empty tail is not history.** Rows below the last one carrying anything —
    /// and below the cursor — are dropped rather than rewrapped, or every resize of an idle
    /// terminal would push a screenful of blank lines into the scrollback.
    ///
    /// The returned [`Reflow`] maps old absolute line numbers onto new ones. A caller holding a
    /// scrolled-back anchor **must** put it through that, or its viewport shows a different part
    /// of the history after a resize than before it.
    ///
    /// Both dimensions are clamped to at least 1, as [`Grid::new`] clamps them.
    pub fn resize(&mut self, cols: usize, rows: usize) -> Reflow {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if cols == self.cols && rows == self.rows {
            return Reflow { base: self.oldest_line(), to: Vec::new() };
        }
        let base = self.oldest_line();

        // 1. Every line there is, oldest first. Old absolute number `base + i` for index `i`,
        //    which is what makes the map below a plain vector.
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
            // The last row wrapped and there is nothing after it — a line the cursor is still
            // in the middle of writing. It is a logical line like any other.
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

        // Where each old line's first character now is. A line whose text was trimmed away maps
        // to the last row its logical line produced, which is where that text now ends.
        let screen_start = lines.len().saturating_sub(rows);
        let to: Vec<u64> = pos
            .iter()
            .map(|&(li, off)| {
                let seg = (off / cols).min(height[li].saturating_sub(1));
                base + (start[li] + seg) as u64
            })
            .collect();

        // 4. The bottom `rows` become the screen; the rest is history.
        self.cells = vec![blank; cols * rows];
        self.wrapped = vec![false; rows];
        for (r, l) in lines[screen_start..].iter().enumerate() {
            self.cells[r * cols..(r + 1) * cols].copy_from_slice(&l.cells);
            self.wrapped[r] = l.wrapped;
        }
        // The row *below* the last one taken from `lines` continues nothing: it is blank screen,
        // not a continuation of the text above it.
        if let Some(last) = lines.len().checked_sub(screen_start).and_then(|n| n.checked_sub(1))
            && last + 1 < rows
        {
            self.wrapped[last] = false;
        }
        lines.truncate(screen_start);
        self.scrollback = lines.into_iter().collect();
        self.scrolled = base + screen_start as u64;
        while self.scrollback.len() > SCROLLBACK {
            self.scrollback.pop_front();
            // `scrolled` counts lines produced and the eviction moves the *other* end, exactly
            // as in `scroll_up`.
        }

        // 5. The cursor follows the character it was on.
        let (cl, coff) = pos.get(cursor_row).copied().unwrap_or((0, 0));
        let off = coff + self.col;
        let seg = (off / cols).min(height.get(cl).copied().unwrap_or(1).saturating_sub(1));
        let line_index = start.get(cl).copied().unwrap_or(0) + seg;
        self.row = line_index.saturating_sub(screen_start).min(rows - 1);
        self.col = off.saturating_sub(seg * cols).min(cols - 1);

        self.cols = cols;
        self.rows = rows;
        // A deferred wrap belongs to a margin that has moved.
        self.pending_wrap = false;
        self.cursor_drawn = (self.row, self.col);
        self.dirty = vec![true; rows];

        Reflow { base, to }
    }

    /// How many *logical* lines the terminal holds — history and screen, wraps not counted.
    ///
    /// **The invariant a rewrap must not break, and the one a caller can check without reading
    /// anybody's text.** Re-breaking lines at a new width moves where the breaks are; it does
    /// not create or destroy *lines*. So this number is the same before and after a
    /// [`resize`](Grid::resize), and an implementation that ignored [`Line::wrapped`] and joined
    /// every adjacent row would collapse the whole history to one — visibly, in a single
    /// number, with nothing about the content in it. That is what lets a gate on a release image
    /// assert the reflow at all: a terminal's rows are somebody's session and the serial log is
    /// not the place for them.
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
                // Stops at the left margin rather than wrapping to the previous line. A
                // backspace that wrapped would let a program erase text it never wrote.
                self.col = self.col.saturating_sub(1);
                self.pending_wrap = false;
            }
            Op::Tab => {
                let next = (self.col / TAB_WIDTH + 1) * TAB_WIDTH;
                // A tab in the last cell moves *to* the last column and stops there. It does
                // not wrap, and it does not set the pending flag: a tab is movement, and the
                // deferred-wrap flag belongs to a character having been written.
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

    /// What an erase writes: a space in the **current background**, with nothing else set.
    ///
    /// **Not [`Cell::BLANK`].** `ED` after `SGR 44` fills blue, which is how a program paints a
    /// coloured region, and a fill with the default background would make that impossible. The
    /// other attributes go: an erased cell has no character, so bold and underline have nothing
    /// to apply to, and carrying them would make a later `reverse` repaint a region that looks
    /// erased.
    ///
    /// The foreground goes too, for the same reason — with one consequence worth knowing:
    /// under `SGR 7`, [`Attributes::resolve`] swaps, so an erase while reversed fills with the
    /// *foreground*. That is what xterm does and it is what makes "reverse, erase line" paint a
    /// solid bar.
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

    /// Write one character at the cursor and advance it.
    ///
    /// **Deferred wrap, not wrap-on-write.** A character landing in the last column leaves the
    /// cursor *on* that column with [`Grid::pending_wrap`] set, and only the next character
    /// moves to the next line. Wrapping immediately is the naive implementation and it is
    /// wrong in a way people notice: printing exactly `cols` characters and then a newline
    /// produces a blank line, because the wrap already happened and the newline moves again.
    /// Every real terminal defers, and every program that draws a full-width line depends on
    /// it.
    fn print(&mut self, c: char) {
        if self.pending_wrap {
            self.col = 0;
            // **The row being left is a soft wrap**, and this is the only moment that is known:
            // afterwards it is just two adjacent full rows, indistinguishable from a program
            // that printed exactly `cols` characters and a newline. `scroll_up` carries the
            // flag into the scrollback with the line, and `resize` reads it back.
            let leaving = self.row;
            self.set_wrapped(leaving, true);
            self.line_feed_scrolling();
        }
        // **Cleared unconditionally**, before the trailing branch decides whether to set it
        // again. Clearing it only inside the wrap arm leaves it set after a wrap on any grid
        // wider than one column — so the *second* character of the wrapped line wraps too, and
        // a paragraph reaching the right margin then descends one line per character. Nothing
        // caught that until a break-test removed the assignment and every test stayed green:
        // the wrap test wrote exactly one character past the margin and stopped.
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

    /// Move down one line, scrolling if that leaves the screen.
    ///
    /// **The column does not change.** `LF` is *index* — down only — and returning to column 0
    /// is `CR`'s job, which is why a program that wants a new line emits `\r\n`. A terminal
    /// that folded the two would make `\r\n` return to column 0 twice, harmlessly, and make
    /// the sequence `abc\ndef` (which should stairstep) look like two aligned lines — hiding a
    /// producer's bug rather than showing it.
    ///
    /// **Translating a bare `\n` into `\r\n` is the line discipline's job**, not the
    /// terminal's: it is Unix's `ONLCR`, and this system already has a tty server that owns
    /// the discipline. `tty-server` writes `\r\n` explicitly where it echoes and does **not**
    /// translate on the `Tty::Write` path — so a program emitting bare `\n` will stairstep in
    /// the GUI terminal, which is a Part C item and is recorded as one in the plan.
    fn line_feed(&mut self) {
        self.pending_wrap = false;
        // **An explicit line feed ends the line it happens on**, whatever a previous paragraph
        // left behind there. Without this a row that once wrapped stays marked after the text
        // that wrapped it has been overwritten, and a reflow would join it to the row below —
        // which is exactly what makes two deliberately short adjacent lines become one, the
        // failure the reflow gate is built around.
        self.set_wrapped(self.row, false);
        self.line_feed_scrolling();
    }

    /// Record whether `row`'s text continues below, if `row` is on the screen.
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
        // The flags move with the rows they describe, and the row opened at the bottom is a
        // fresh line that continues nothing.
        self.wrapped.rotate_left(1);
        if let Some(last) = self.wrapped.last_mut() {
            *last = false;
        }
        // Counts lines *produced*, so it is incremented here and not adjusted by the eviction
        // below: it is the top of the history, and the eviction moves the bottom.
        self.scrolled += 1;
        while self.scrollback.len() > SCROLLBACK {
            self.scrollback.pop_front();
        }
        self.cells.copy_within(self.cols.., 0);
        let blank = self.blank();
        let start = (self.rows - 1) * self.cols;
        self.cells[start..].fill(blank);
        // Everything moved, so everything repaints. A scroll is the one operation that dirties
        // the whole screen, and pretending otherwise would be the bug a damage system exists
        // to avoid on the *other* side.
        self.damage_all();
    }

    /// `CUP`: clamp into the screen. Never scrolls — addressing a row past the bottom means
    /// the bottom row, not a screen that moves under the program.
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
            // Inclusive of the cursor, which is what `EL 1` means — an exclusive range leaves
            // the character under the cursor behind.
            Erase::ToStart => (0, col + 1),
            Erase::All => (0, self.cols),
        };
        let blank = self.blank();
        self.cells[row * self.cols + from..row * self.cols + to].fill(blank);
        if to == self.cols {
            // Its tail is gone, so it does not continue below any more — the same reasoning as
            // `erase_in_display`, for the one row this touches.
            self.set_wrapped(row, false);
        }
        self.touch(row);
        // Erasing does not move the cursor, but it does end a pending wrap: the character that
        // set it is gone.
        self.pending_wrap = false;
    }

    fn erase_in_display(&mut self, e: Erase) {
        let blank = self.blank();
        let (row, col) = (self.row, self.col);
        match e {
            Erase::ToEnd => {
                self.cells[row * self.cols + col..(row + 1) * self.cols].fill(blank);
                self.cells[(row + 1) * self.cols..].fill(blank);
                // **Erased text cannot still be continued below.** A row whose tail is gone
                // ends there, and so does every row under it — leaving the flags set would let
                // a later reflow join rows whose joining text no longer exists.
                self.wrapped[row..].fill(false);
            }
            Erase::ToStart => {
                self.cells[..row * self.cols].fill(blank);
                self.cells[row * self.cols..row * self.cols + col + 1].fill(blank);
                // The rows above are empty now, so nothing continues into anything; this row
                // keeps its own flag, because its tail — past the cursor — is untouched.
                self.wrapped[..row].fill(false);
            }
            // **`ED 2` does not scroll.** `Ctrl-L` in a shell clears the screen and leaves the
            // scrollback alone; a version that scrolled the screen into history would fill
            // someone's scrollback with blank lines every time they cleared.
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

    /// Drive a grid from real bytes, through the real parser.
    ///
    /// The two halves are tested apart *and* joined here, because "the parser emits the right
    /// ops" and "the grid does the right thing with them" together still permit a mismatch in
    /// what the ops mean.
    fn feed(g: &mut Grid, s: &str) {
        let mut p = Parser::new();
        let mut out = [Op::Print('\0'); MAX_PER_BYTE];
        for &b in s.as_bytes() {
            let n = p.feed(b, &mut out);
            g.apply_all(&out[..n]);
        }
    }

    /// A row's characters, trailing blanks trimmed.
    fn line(g: &Grid, row: usize) -> alloc::string::String {
        let s: alloc::string::String =
            (0..g.cols()).map(|c| g.cell(row, c).unwrap().ch).collect();
        s.trim_end().into()
    }

    /// The text of a viewport row, from wherever it is served, trailing blanks trimmed.
    fn view(g: &Grid, top: u64, row: usize) -> alloc::string::String {
        let s: alloc::string::String =
            (0..g.cols()).map(|c| g.view_cell(top, row, c).map(|x| x.ch).unwrap_or(' ')).collect();
        s.trim_end().into()
    }

    #[test]
    fn a_wrapped_line_rejoins_on_a_widen_and_two_short_lines_do_not() {
        // **The assertion the whole flag exists for**, and the shape M9 Part D's gate uses. A
        // rewrap that ignored `wrapped` and simply joined adjacent rows would put the long line
        // back together too — so "the long line is now one row" holds for the broken version as
        // well, and it is the *short pair staying two rows* that separates them.
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
        // **The invariant a gate can check without reading anyone's text**, which is what makes
        // the reflow assertable on a release image at all. An implementation that joined every
        // adjacent row — the failure `Line::wrapped` exists to prevent — collapses the whole
        // history into one logical line, and says so in this one number.
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
        // **The property a person notices**: after a resize the viewport shows the same words,
        // not the same line arithmetic. `scrolled` counts lines and a rewrap changes how many
        // there are, so an anchor carried across unmapped is off by the number of joins above
        // it — which is why the history here is *wrapped*. A first version of this test used
        // lines that fitted at both widths, where nothing moves and carrying the number across
        // unmapped passes too (found by running that control).
        let mut g = Grid::new(5, 3);
        for i in 0..8 {
            feed(&mut g, &alloc::format!("aaaa{i}bbbb\r\n")); // 9 chars at width 5: two rows
        }
        let anchor = g.oldest_line() + 4;
        let want = view(&g, anchor, 0);
        assert_eq!(want, "aaaa2", "the first half of a line the old width broke in two");

        let r = g.resize(10, 3);
        // **Starts with**, not equals: the row at the new number holds the *rejoined* line, so
        // the text that began at the anchor now has the rest of its own line after it. What
        // must not happen is landing on a different line, which is what carrying the number
        // across unmapped does.
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
        // A terminal showing a prompt is mostly empty screen. Rewrapping the blank tail would
        // put a screenful of nothing into the history on every resize — and a person who
        // maximises and restores would find their scrollback full of blank lines.
        // **Shrinking, because that is where it shows.** Growing has room for the blank tail
        // and nothing spills whether or not it is trimmed — the first version of this test grew
        // the screen and passed with the trim removed (found by running that control).
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
        // `trim_trailing_blanks` drops only never-written cells. A program that painted a bar
        // to the margin — reverse video, or an erase under a background colour — wrote those
        // spaces on purpose, and a rewrap that trimmed them would erase the bar.
        let mut g = Grid::new(6, 3);
        feed(&mut g, "\x1b[44m\x1b[K"); // erase the line under blue: six blue spaces
        g.resize(6, 3);
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
        // **The whole reason wrapping is deferred.** Wrap-on-write moves to row 1 as the last
        // character lands, and the newline then moves to row 2 — so a program printing exactly
        // `cols` characters per line double-spaces. Every real terminal defers; the naive
        // implementation is wrong in a way people see immediately.
        let mut g = Grid::new(4, 4);
        feed(&mut g, "abcd\r\nxy");
        assert_eq!(line(&g, 0), "abcd");
        assert_eq!(line(&g, 1), "xy", "a blank line appeared between them");
        assert_eq!(line(&g, 2), "");
    }

    #[test]
    fn a_line_feed_moves_down_and_leaves_the_column_alone() {
        // **`LF` is index, not newline.** Returning to column 0 is `CR`'s job, which is why a
        // program emits `\r\n`. Folding the two here would hide a producer's stairstepping
        // bug rather than showing it — and translation is the *line discipline's* job
        // (Unix `ONLCR`), which this system has a tty server for.
        let mut g = Grid::new(10, 3);
        feed(&mut g, "abc\ndef");
        assert_eq!(line(&g, 0), "abc");
        assert_eq!(line(&g, 1), "   def", "the line feed also returned to column 0");
    }

    #[test]
    fn the_character_after_a_full_line_wraps() {
        // The other half: deferring must not mean *not* wrapping.
        let mut g = Grid::new(4, 4);
        feed(&mut g, "abcde");
        assert_eq!(line(&g, 0), "abcd");
        assert_eq!(line(&g, 1), "e");
        assert_eq!(g.cursor(), (1, 1));
    }

    #[test]
    fn a_wrapped_line_keeps_filling_rather_than_wrapping_every_character() {
        // The test the wrap coverage was missing: it wrote one character past the margin and
        // stopped, so a stale pending flag — which makes every subsequent character wrap —
        // looked identical. A paragraph would have descended one line per character.
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
        // A pending wrap belongs to the character that set it. Any explicit movement replaces
        // that position, so the flag must not survive — or a `\r` after a full line would
        // still wrap the next character to row 1.
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
        // Three characters into four columns, deliberately: "three" would have wrapped and
        // scrolled a second time, which is a different thing from what this test is about and
        // is what the first version of it accidentally measured.
        let mut g = Grid::new(4, 2);
        feed(&mut g, "one\r\ntwo\r\nsix");
        assert_eq!(line(&g, 0), "two");
        assert_eq!(line(&g, 1), "six", "the third line did not land on the bottom row");
        assert_eq!(g.scrollback().len(), 1);
        let first: alloc::string::String =
            g.scrollback().next().unwrap().cells.iter().map(|c| c.ch).collect();
        assert_eq!(first.trim_end(), "one");
    }

    #[test]
    fn scrollback_is_bounded() {
        // A program that prints forever must cost a bounded amount. Sized small here and
        // driven past it, because a ring that never evicts passes every test that only fills
        // it partway.
        let mut g = Grid::new(4, 1);
        for i in 0..(SCROLLBACK + 50) {
            feed(&mut g, &alloc::format!("{}\r\n", i % 10));
        }
        assert_eq!(g.scrollback().len(), SCROLLBACK, "the ring grew past its bound");
    }

    #[test]
    fn a_scrolled_back_view_reads_from_the_scrollback_and_the_screen_at_once() {
        // The seam. A viewport straddling the boundary must serve its top rows from history
        // and its bottom rows from the live screen, and the two halves are stored differently.
        let mut g = Grid::new(4, 2);
        for i in 0..6 {
            feed(&mut g, &alloc::format!("{i}\r\n"));
        }
        // Six lines produced on a two-row screen: lines 0..4 are history, 4..6 on screen.
        assert_eq!(g.top_line(), 5, "five lines scrolled off");
        assert_eq!(g.oldest_line(), 0, "and none evicted");

        let at = |top: u64, row: usize| g.view_cell(top, row, 0).map(|c| c.ch);
        assert_eq!((at(5, 0), at(5, 1)), (Some('5'), Some(' ')), "the bottom: the live screen");
        assert_eq!((at(3, 0), at(3, 1)), (Some('3'), Some('4')), "wholly in the scrollback");
        assert_eq!(
            (at(4, 0), at(4, 1)),
            (Some('4'), Some('5')),
            "straddling: history above, screen below",
        );
    }

    #[test]
    fn the_view_is_anchored_to_a_line_so_output_does_not_drag_it() {
        // **Why the anchor is an absolute line number.** A view expressed as "n lines above
        // the bottom" shows different text every time a line arrives — the reader's page
        // creeping upward exactly while they are trying to read it.
        let mut g = Grid::new(4, 2);
        for i in 0..6 {
            feed(&mut g, &alloc::format!("{i}\r\n"));
        }
        let before: alloc::vec::Vec<Option<char>> =
            (0..2).map(|r| g.view_cell(2, r, 0).map(|c| c.ch)).collect();

        for i in 6..9 {
            feed(&mut g, &alloc::format!("{i}\r\n"));
        }
        let after: alloc::vec::Vec<Option<char>> =
            (0..2).map(|r| g.view_cell(2, r, 0).map(|c| c.ch)).collect();
        assert_eq!(before, after, "the anchored view moved when output arrived");
        assert_eq!(before, [Some('2'), Some('3')]);
    }

    #[test]
    fn an_anchor_the_scrollback_has_evicted_clamps_to_the_oldest_line_kept() {
        // The bounded ring drops the oldest line, so an anchor that was valid stops being so.
        // Clamping is what a real terminal does; the alternative is a viewport of blanks.
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
        // Scrolling back does not move the cursor. A render that drew it at its *screen* row
        // regardless would invert a cell of somebody's history, several screens above where
        // the cursor really is.
        let mut g = Grid::new(4, 2);
        for i in 0..6 {
            feed(&mut g, &alloc::format!("{i}\r\n"));
        }
        assert_eq!(g.cursor(), (1, 0));
        assert_eq!(g.view_cursor(g.top_line()), Some((1, 0)), "following: where it always was");
        // One line back: the cursor's line is 6, the viewport covers 4 and 5 — just below it,
        // which is the boundary case a `<=` here would get wrong.
        assert_eq!(g.view_cursor(4), None, "one row past the bottom is still out of view");
        assert_eq!(g.view_cursor(5), Some((1, 0)), "and the row before it is the last in view");
        assert_eq!(g.view_cursor(0), None, "scrolled to the top: nowhere near it");
    }

    #[test]
    fn cursor_addressing_clamps_and_never_scrolls() {
        // A program that addresses row 999 gets the last row. Scrolling instead would move the
        // screen under a program that was only trying to position a cursor.
        let mut g = Grid::new(10, 3);
        feed(&mut g, "\x1b[99;99H");
        assert_eq!(g.cursor(), (2, 9));
        assert_eq!(g.scrollback().len(), 0, "addressing the cursor scrolled the screen");

        // **One past the end, not far past it.** `99` on a 3-row grid clamps to 2 *and*
        // wraps to 2 (`98 % 3`), so it cannot tell clamping from a modulo — which a break-test
        // proved by leaving this assertion green. Row 4 (zero-based 3) separates them: clamp
        // gives 2, modulo gives 0.
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
        // Wrapping to the previous line would let a program erase text it never wrote.
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
        // Against the right edge it stops rather than wrapping.
        let mut g = Grid::new(10, 2);
        feed(&mut g, "\t\t\t\t");
        assert_eq!(g.cursor(), (0, 9));
    }

    #[test]
    fn erasing_fills_with_the_current_background_not_the_default() {
        // **The A2 gap this part was expected to force.** `ED`/`EL` after `SGR 44` fill blue —
        // it is how a program paints a coloured region — and `Cell::BLANK` cannot express that.
        // A2's `BLANK` is still right for a *never-written* cell; what was missing is that an
        // erased one is a different thing.
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
        // A space has nothing to embolden or underline, and carrying those would make a later
        // reverse repaint a region that looks erased.
        let mut g = Grid::new(4, 2);
        feed(&mut g, "\x1b[1;4;44m\x1b[2K");
        let cell = g.cell(0, 0).unwrap();
        assert!(!cell.attrs.flags.contains(Flags::BOLD));
        assert!(!cell.attrs.flags.contains(Flags::UNDERLINE));
        assert_eq!(cell.attrs.fg, Colour::Default);
    }

    #[test]
    fn erasing_while_reversed_fills_with_the_foreground() {
        // **`blank()` keeps `REVERSE` while dropping `BOLD` and `UNDERLINE`**, and that
        // retention is the whole of "reverse, erase line paints a solid bar". It was documented
        // in three places and asserted in none: adding `.without(Flags::REVERSE)` — deleting the
        // behaviour outright — left every test green (PR #190 review, finding 4).
        //
        // Worse than an ordinary gap, because `blank()`'s own summary reads "a space in the
        // current background, with nothing else set" — so a reader tidying the flags to match
        // the summary would break a documented behaviour and see a passing suite.
        let p = crate::cell::Palette::default();
        let mut g = Grid::new(4, 1);
        feed(&mut g, "\x1b[7m\x1b[2K");
        let (fg, bg) = g.cell(0, 0).unwrap().attrs.resolve(&p);
        assert_eq!(bg, p.foreground, "an erase under reverse did not paint a bar");
        assert_eq!(fg, p.background);

        // And without reverse it is the ordinary background, or the assertion above would hold
        // for any erase at all.
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
        // `Ctrl-L` clears and leaves history alone. A version that scrolled would fill
        // someone's scrollback with blank lines every time they cleared.
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
        // Attributes are per-cell, not per-screen: changing them later must not recolour text
        // already on the grid.
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

        // Writing where the cursor already is stays one row; it is *moving* that costs two, and
        // that is what the two tests below are about.
        feed(&mut g, "y");
        assert_eq!(g.take_damage(), alloc::vec![2]);
    }

    #[test]
    fn a_scroll_damages_every_row() {
        // Everything moved. A grid that reported only the bottom row here would leave the
        // screen showing the pre-scroll contents everywhere else.
        let mut g = Grid::new(4, 3);
        feed(&mut g, "a\r\nb\r\nc");
        g.take_damage();
        feed(&mut g, "\r\nd");
        assert_eq!(g.take_damage(), alloc::vec![0, 1, 2]);
    }

    #[test]
    fn erasing_the_display_damages_every_row_it_touched() {
        // `ED` writes to rows the cursor is not on, so reporting the cursor's row alone would
        // leave the rest of the screen showing what was erased. Caught by a break-test: the
        // extent test above asserts *contents* and says nothing about damage.
        let mut g = Grid::new(4, 3);
        feed(&mut g, "aaaa\r\nbbbb\r\ncccc");
        g.take_damage();
        feed(&mut g, "\x1b[2J");
        assert_eq!(g.take_damage(), alloc::vec![0, 1, 2]);

        // And the partial forms too — `ED 0` from the middle of row 1 touches rows 1 and 2.
        let mut g = Grid::new(4, 3);
        feed(&mut g, "aaaa\r\nbbbb\r\ncccc\x1b[2;3H");
        g.take_damage();
        feed(&mut g, "\x1b[J");
        let d = g.take_damage();
        assert!(d.contains(&1) && d.contains(&2), "rows below the cursor were not damaged: {d:?}");
    }

    #[test]
    fn moving_the_cursor_damages_the_row_it_left_and_the_one_it_reached() {
        // **This test used to assert the opposite**, on the rationale that the render draws the
        // cursor over whatever cell it sits on — which would hold if it were a separate overlay
        // pass. A5 paints it *into* the cell, so the row the cursor left keeps an inverted
        // block until something repaints it (PR #190 review, finding 1).
        let mut g = Grid::new(10, 4);
        g.take_damage();
        feed(&mut g, "\x1b[2;5H");
        assert_eq!(g.take_damage(), alloc::vec![0, 1], "left row 0, arrived in row 1");

        // Within one row, one row.
        feed(&mut g, "\x1b[C");
        assert_eq!(g.take_damage(), alloc::vec![1]);

        // And a cursor that did not move damages nothing, which is what keeps a still screen
        // free.
        assert_eq!(g.take_damage(), alloc::vec![]);
    }

    #[test]
    fn a_wrap_damages_the_row_the_cursor_left() {
        // The ordinary print path, no cursor keys involved: `abcd` rests the cursor on the last
        // column of row 0 (inverted), and `e` wraps it to row 1. Reporting only row 1 leaves a
        // phantom block at the end of row 0 — one per line of any paragraph reaching the
        // margin.
        let mut g = Grid::new(4, 3);
        feed(&mut g, "abcd");
        g.take_damage();
        feed(&mut g, "e");
        assert_eq!(g.take_damage(), alloc::vec![0, 1], "the row the cursor left was not repainted");
    }

    #[test]
    fn a_degenerate_size_is_clamped_rather_than_panicking() {
        // `Grid::new(0, 0)` has no valid cursor position, and every operation would need a
        // special case for a screen that cannot hold anything.
        let mut g = Grid::new(0, 0);
        feed(&mut g, "abc\r\n\x1b[9;9H\x1b[2J");
        assert_eq!(g.cols(), 1);
        assert_eq!(g.rows(), 1);
        assert_eq!(g.cursor(), (0, 0));
    }
}
