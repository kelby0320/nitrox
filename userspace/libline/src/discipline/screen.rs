//! The redraw where the width is known: the cursor moved by row and column, across a wrap.
//!
//! **No cursor is left with a wrap pending.** A row filled exactly leaves xterm's and `libterm`'s
//! cursor on its last column, `fbcon`'s at the next row's start; a space and a carriage return
//! after it put both at the next row's start. **Nor a row joined to the next by that wrap** where
//! the text ends there, at a recalled statement's `\n` or Enter: up a row and a line feed, which
//! ends it, or a refit would rewrap what follows into it.
//!
//! **Rows are counted as a terminal lays the text out**, so a refit reflowing it agrees: an earlier
//! row of a recalled statement takes the rows its text fills, and the cursor's the row it is on.

use super::{Discipline, chars};
use alloc::vec::Vec;

/// A row, counted from the prompt's first, and a column.
pub(super) type Cell = (usize, usize);

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(super) struct Geometry {
    /// At least one.
    pub(super) width: usize,
    /// The prompt's columns, after which the line starts.
    pub(super) prompt: usize,
}

fn csi(out: &mut Vec<u8>, n: usize, fin: u8) {
    out.extend_from_slice(b"\x1b[");
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    let mut n = n;
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[i..]);
    out.push(fin);
}

/// By relative moves, so where the line sits on the screen need not be known. `CSI A` stops at the
/// screen's top, so a line taller than the window is edited short of its rows (`TODO(tall-line)`).
pub(super) fn move_cursor(out: &mut Vec<u8>, from: Cell, to: Cell) {
    if to.0 < from.0 {
        csi(out, from.0 - to.0, b'A');
    } else if to.0 > from.0 {
        csi(out, to.0 - from.0, b'B');
    }
    if to.1 == from.1 {
        return;
    }
    if to.1 == 0 {
        out.push(b'\r');
    } else if to.1 > from.1 {
        csi(out, to.1 - from.1, b'C');
    } else {
        csi(out, from.1 - to.1, b'D');
    }
}

impl Discipline {
    /// Where byte `i` of the line is drawn; a recalled statement's `\n` starts a row.
    pub(super) fn cell(&self, g: Geometry, i: usize) -> Cell {
        let (mut row, mut base, mut s) = (0, g.prompt, 0);
        while let Some(nl) = self.line[s..i].iter().position(|&b| b == b'\n') {
            let l = base + chars(&self.line[s..s + nl]);
            row += if l == 0 { 1 } else { l.div_ceil(g.width) };
            base = 0;
            s += nl + 1;
        }
        let l = base + chars(&self.line[s..i]);
        (row + l / g.width, l % g.width)
    }

    /// Whether the text before byte `i` ends exactly on a row's last column, a wrap pending.
    fn fills_its_row(&self, g: Geometry, i: usize) -> bool {
        let s = self.line[..i].iter().rposition(|&b| b == b'\n').map_or(0, |n| n + 1);
        let l = if s == 0 { g.prompt } else { 0 } + chars(&self.line[s..i]);
        l > 0 && l.is_multiple_of(g.width)
    }

    /// Move from `at` to byte `i`'s cell. The line's end, where its text fills a row exactly, is
    /// reached by writing the row's last character again and the space and return after it: moved
    /// to, the terminal would hold the row apart from the next, as a refit leaves one it filled.
    pub(super) fn reach(&self, g: Geometry, out: &mut Vec<u8>, at: Cell, i: usize) {
        let to = self.cell(g, i);
        if at != to && i == self.line.len() && i > self.row_start() && self.fills_its_row(g, i) {
            let last = self.prev(i);
            move_cursor(out, at, self.cell(g, last));
            out.extend_from_slice(&self.line[last..]);
            out.extend_from_slice(b" \r");
        } else {
            move_cursor(out, at, to);
        }
    }

    /// What to write to show the line from byte `from` on, the cursor at `at`, and leave it at the
    /// cursor; `clear` erases what a longer line left.
    pub(super) fn repaint(&self, g: Geometry, at: Cell, from: usize, clear: bool) -> Vec<u8> {
        let mut out = Vec::new();
        move_cursor(&mut out, at, self.cell(g, from));
        let (mut i, mut filled) = (from, false);
        for (n, part) in self.line[from..].split(|&b| b == b'\n').enumerate() {
            if n > 0 {
                out.extend_from_slice(if filled { b"\x1b[A\n" } else { b"\x1b[K\r\n" });
                i += 1;
            }
            out.extend_from_slice(part);
            i += part.len();
            filled = !part.is_empty() && self.fills_its_row(g, i);
            if filled {
                out.extend_from_slice(b" \r");
            }
        }
        if clear {
            out.extend_from_slice(b"\x1b[J");
        }
        move_cursor(&mut out, self.cell(g, self.line.len()), self.cell(g, self.cursor));
        out
    }

    /// What to write to leave the line for a new row below it: to its end, and a new row unless
    /// one is begun.
    pub(super) fn finish(&self, g: Geometry) -> Vec<u8> {
        let mut out = Vec::new();
        self.reach(g, &mut out, self.cell(g, self.cursor), self.line.len());
        if !self.fills_its_row(g, self.line.len()) {
            out.extend_from_slice(b"\r\n");
        } else if self.line.len() > self.row_start() {
            out.extend_from_slice(b"\x1b[A\n");
        }
        // Otherwise a prompt as wide as the terminal filled the row, and its wrap is pending.
        out
    }
}
