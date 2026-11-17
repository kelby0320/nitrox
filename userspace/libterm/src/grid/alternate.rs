//! The alternate screen (`DECSET 1049`): a second grid with no scrollback, which a pager draws on
//! so that leaving it puts back the screen it covered.

use super::{Attributes, Cell, Grid, Reflow};
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

/// The screen not shown: the main one while the alternate one is, with the cursor and attributes
/// `DECSC` would save.
pub(super) struct Screen {
    cells: Vec<Cell>,
    wrapped: Vec<bool>,
    printed: Vec<usize>,
    row: usize,
    col: usize,
    pending_wrap: bool,
    attrs: Attributes,
}

impl Screen {
    fn blank(cols: usize, rows: usize, (row, col): (usize, usize), attrs: Attributes) -> Screen {
        let (wrapped, printed) = (vec![false; rows], vec![0; rows]);
        Screen { cells: vec![Cell::BLANK; cols * rows], wrapped, printed, row, col, pending_wrap: false, attrs }
    }

    /// This screen, `from` columns wide, cut or padded to `cols` by `rows`, not rewrapped: its
    /// program draws it again at the new size.
    fn cropped(self, from: (usize, usize), cols: usize, rows: usize) -> Screen {
        let (old_cols, old_rows) = from;
        let mut s = Screen::blank(cols, rows, (self.row.min(rows - 1), self.col.min(cols - 1)), self.attrs);
        let n = cols.min(old_cols);
        for r in 0..rows.min(old_rows) {
            s.cells[r * cols..r * cols + n].copy_from_slice(&self.cells[r * old_cols..r * old_cols + n]);
            s.printed[r] = self.printed[r].min(cols);
        }
        s
    }
}

impl Grid {
    /// Whether the alternate screen is shown.
    pub fn alternate(&self) -> bool {
        self.main.is_some()
    }

    /// To the alternate screen, cleared, the cursor where it was; or back, the main one's restored.
    pub(super) fn switch_screen(&mut self, to_alternate: bool) {
        if to_alternate == self.alternate() {
            return;
        }
        if to_alternate {
            let main = self.swap(Screen::blank(self.cols, self.rows, (self.row, self.col), self.attrs));
            self.main = Some(Box::new(main));
        } else if let Some(main) = self.main.take() {
            self.swap(*main);
        }
        // A selection's lines are the other screen's.
        self.selection = None;
        self.damage_all();
    }

    /// [`Grid::resize`] while the alternate screen is shown: the main one is rewrapped, as it will
    /// be shown again, and the alternate one cut or padded.
    pub(super) fn resize_alternate(&mut self, cols: usize, rows: usize) -> Reflow {
        let Some(main) = self.main.take() else { return self.resize(cols, rows) };
        let from = (self.cols, self.rows);
        let shown = self.swap(*main);
        let reflow = self.resize(cols, rows);
        let main = self.swap(shown.cropped(from, self.cols, self.rows));
        self.main = Some(Box::new(main));
        self.selection = None;
        self.cursor_drawn = (self.row, self.col);
        self.damage_all();
        reflow
    }

    /// Show `s` and return the screen it replaces; the scrollback stays, being the main screen's.
    fn swap(&mut self, mut s: Screen) -> Screen {
        core::mem::swap(&mut self.cells, &mut s.cells);
        core::mem::swap(&mut self.wrapped, &mut s.wrapped);
        core::mem::swap(&mut self.printed, &mut s.printed);
        core::mem::swap(&mut self.row, &mut s.row);
        core::mem::swap(&mut self.col, &mut s.col);
        core::mem::swap(&mut self.pending_wrap, &mut s.pending_wrap);
        core::mem::swap(&mut self.attrs, &mut s.attrs);
        s
    }
}
