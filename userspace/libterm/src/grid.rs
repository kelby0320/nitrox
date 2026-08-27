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

/// Columns in the default grid.
///
/// **Fixed, and M9 Part D is where it stops being.** This said "fixed because M6 owns move and
/// resize"; M6 shipped with neither, and nothing since could resize a window at all — `nxterm`
/// declines every `Configure` on purpose. The reasoning underneath it still holds and is the
/// reason that part is the milestone's largest: reflowing scrollback is a different problem, not
/// a parameter of this one, and the scrollback does not yet record which of its rows were soft
/// wraps.
pub const COLS: usize = 80;
/// Rows in the milestone's fixed grid.
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

/// The screen, the cursor, and the lines that have scrolled off it.
pub struct Grid {
    cols: usize,
    rows: usize,
    /// Row-major, `rows * cols`.
    cells: Vec<Cell>,
    /// Lines that have scrolled off the top, oldest first.
    scrollback: VecDeque<Vec<Cell>>,
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
    pub fn scrollback(&self) -> impl ExactSizeIterator<Item = &Vec<Cell>> {
        self.scrollback.iter()
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
            self.scrollback.get(i).and_then(|l| l.get(col)).copied()
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
        self.line_feed_scrolling();
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
        let line = self.cells[0..self.cols].to_vec();
        self.scrollback.push_back(line);
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
            }
            Erase::ToStart => {
                self.cells[..row * self.cols].fill(blank);
                self.cells[row * self.cols..row * self.cols + col + 1].fill(blank);
            }
            // **`ED 2` does not scroll.** `Ctrl-L` in a shell clears the screen and leaves the
            // scrollback alone; a version that scrolled the screen into history would fill
            // someone's scrollback with blank lines every time they cleared.
            Erase::All => self.cells.fill(blank),
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
            g.scrollback().next().unwrap().iter().map(|c| c.ch).collect();
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
