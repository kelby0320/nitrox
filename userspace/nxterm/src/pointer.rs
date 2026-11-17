//! The pointer over the grid: which cell it is in, which click a press is, and the selection.

use super::*;

impl App {
    /// A pointer event over the grid: press to anchor, drag to extend, release to end.
    ///
    /// Only the left button, and only while held: the router delivers motion unconditionally.
    pub(crate) fn grid_pointer(&mut self, p: PointerEvent) {
        let Some((line, col)) = self.cell_at(p.x, p.y) else { return };
        if p.kind == POINTER_BUTTON && p.button == BTN_LEFT {
            if p.flags & POINTER_PRESSED != 0 {
                // Past three wraps back to a sweep, as a fourth click has no larger unit.
                match self.click_run {
                    2 => {
                        self.tab_mut().grid.select_word_at(line, col);
                    }
                    3 => {
                        self.tab_mut().grid.select_line_at(line);
                    }
                    _ => self.tab_mut().grid.select_from(line, col),
                }
                self.tab_mut().view_moved = true;
            }
            return;
        }
        if p.kind == POINTER_MOTION && p.buttons & 1 != 0 {
            self.tab_mut().grid.extend(line, col);
            self.tab_mut().view_moved = true;
        }
    }

    /// Record a pointer press at `at`, `at_ms` milliseconds into the monotonic clock, before the
    /// event is routed so it can ask which click it was.
    pub fn note_press(&mut self, at: libdraw::geom::Point, at_ms: u64) {
        let n = self.clicks.press(at, at_ms);
        self.click_run = if n > 3 {
            // Re-seeded, not merely forgotten: a bare `reset` would make this press a first click
            // and the next one too.
            self.clicks.reset();
            self.clicks.press(at, at_ms)
        } else {
            n
        };
    }

    /// The absolute `(line, column)` a grid-local pixel falls in, or `None` outside it.
    pub fn cell_at(&self, x: i32, y: i32) -> Option<(u64, usize)> {
        if x < 0 || y < 0 || self.metrics.cell_w == 0 || self.metrics.cell_h == 0 {
            return None;
        }
        let col = (x as u32 / self.metrics.cell_w) as usize;
        let row = (y as u32 / self.metrics.cell_h) as usize;
        if row >= self.tab().grid.rows() {
            return None;
        }
        // Clamped rather than refused: a drag off the right edge means "to the end of the line".
        Some((self.view_line() + row as u64, col.min(self.tab().grid.cols())))
    }
}
