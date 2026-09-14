//! What the console shows, independent of where it is drawn.
//!
//! Plain data and arithmetic — no locks, no pointers, no hardware — for two readers besides the
//! kernel: the host tests, and `xtask`'s `-serial none` gate, which compiles this file and
//! [`glyphs`](super::glyphs) by path so that it reads a screendump back into text with the same
//! palette, geometry and glyphs the kernel drew it with.
//!
//! The byte stream is what COM1 receives, so it is mostly ASCII lines and occasionally something
//! else: UTF-8 (the kernel's messages are full of em dashes), and the escape sequences a
//! program writes to a terminal. The first is drawn when the face has the character and drawn as
//! `?` when it does not; the second is swallowed, since a console with one colour has nothing
//! to do with it, and drawing `[1;32m` into the middle of a line is worse than dropping it.

use super::glyphs::{self, BLANK, GLYPH_H, GLYPH_W};

/// Most columns the console keeps.
///
/// Bounds the cell storage, which is static because the console draws before the allocator
/// exists. With [`MAX_ROWS`] it also picks the scale: 1366×768 is 170×48 cells at scale 1, and
/// 3840×2160 is 240×67 at scale 2 rather than 480×135 cells too small to read.
pub const MAX_COLS: usize = 256;
/// Most rows the console keeps. See [`MAX_COLS`].
pub const MAX_ROWS: usize = 100;
/// Cells of storage: [`MAX_COLS`] × [`MAX_ROWS`].
pub const CELLS: usize = MAX_COLS * MAX_ROWS;

/// The colour text is drawn in, `[r, g, b]`.
pub const INK: [u8; 3] = [0xea, 0xee, 0xf2];
/// The colour behind it — the dark slate the boot screen used.
pub const PAPER: [u8; 3] = [0x0a, 0x18, 0x2c];

/// How far the text jumps when it reaches the bottom, as a fraction of the rows: a quarter.
///
/// **A jump rather than a line at a time**, because scrolling repaints every cell whose glyph
/// changed, which after a scroll is nearly all of them. A quarter makes that a quarter as often
/// and still leaves three quarters of the screen as context above the newest line. The newest
/// line is always drawn before the write that produced it returns — this changes how often the
/// screen repaints, not what it shows when the machine stops.
///
/// Measured 2026-09-14 under TCG, from the first line on screen to `compositor: up` — about 90
/// lines of a release boot, read off `check-fbcon`'s screendumps: 536 and 568 ms jumping a
/// quarter, 773 and 814 ms a line at a time. That is the emulator; a framebuffer mapped without
/// write-combining (Phase 5 Part G) makes every pixel dearer on the laptop, not cheaper.
const JUMP_DIVISOR: usize = 4;

/// The console's layout on one screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Geometry {
    /// Each glyph pixel is drawn as a `scale`×`scale` block.
    pub scale: usize,
    /// Text columns.
    pub cols: usize,
    /// Text rows.
    pub rows: usize,
}

impl Geometry {
    /// The layout a `width`×`height` screen gets: the smallest whole scale at which the grid
    /// fits the cell storage, anchored at the top-left pixel. `None` when not even one cell fits.
    pub const fn for_screen(width: usize, height: usize) -> Option<Self> {
        let mut scale = 1;
        loop {
            let cols = width / (GLYPH_W * scale);
            let rows = height / (GLYPH_H * scale);
            if cols == 0 || rows == 0 {
                return None;
            }
            if cols <= MAX_COLS && rows <= MAX_ROWS {
                return Some(Self { scale, cols, rows });
            }
            scale += 1;
        }
    }

    /// Width of one cell in pixels.
    pub const fn cell_w(&self) -> usize {
        GLYPH_W * self.scale
    }

    /// Height of one cell in pixels.
    pub const fn cell_h(&self) -> usize {
        GLYPH_H * self.scale
    }
}

/// One thing the byte stream asks of the grid.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Draw this glyph at the cursor and advance.
    Glyph(u8),
    /// `\n`: to the start of the next row, scrolling if there is none.
    Newline,
    /// `\r`: to the start of this row.
    Return,
    /// `\t`: to the next multiple-of-eight column.
    Tab,
    /// `0x08`: one column left.
    Backspace,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Ground,
    /// Inside a UTF-8 sequence: the bits so far, and how many continuation bytes are still owed.
    Utf8 { cp: u32, owed: u8 },
    /// After `ESC`.
    Escape,
    /// After `ESC [`, until a final byte.
    Csi,
}

/// Turns bytes into [`Action`]s, carrying state across writes — a `kprintln!` arrives as several
/// `write_str` calls, and nothing stops one of them ending partway through a character.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Decoder {
    state: State,
}

impl Decoder {
    /// A decoder at the start of a stream.
    pub const fn new() -> Self {
        Self { state: State::Ground }
    }

    /// Feed one byte, passing whatever it completes to `out` — nothing, one action, or two when a
    /// byte both ends a broken sequence and starts something of its own.
    pub fn feed(&mut self, byte: u8, out: &mut impl FnMut(Action)) {
        match self.state {
            State::Ground => self.ground(byte, out),
            State::Utf8 { cp, owed } => {
                if byte & 0xC0 == 0x80 {
                    let cp = (cp << 6) | (byte & 0x3F) as u32;
                    if owed == 1 {
                        self.state = State::Ground;
                        out(Action::Glyph(char::from_u32(cp).map_or(glyphs::REPLACEMENT, glyphs::index_or_replacement)));
                    } else {
                        self.state = State::Utf8 { cp, owed: owed - 1 };
                    }
                } else {
                    // Cut short: the character is lost, the byte that cut it is not.
                    self.state = State::Ground;
                    out(Action::Glyph(glyphs::REPLACEMENT));
                    self.ground(byte, out);
                }
            }
            State::Escape => {
                self.state = if byte == b'[' { State::Csi } else { State::Ground };
            }
            State::Csi => {
                if (0x40..=0x7E).contains(&byte) {
                    self.state = State::Ground;
                } else if byte < 0x20 {
                    // A control inside a sequence ends it, and still does what it says.
                    self.state = State::Ground;
                    self.ground(byte, out);
                }
            }
        }
    }

    fn ground(&mut self, byte: u8, out: &mut impl FnMut(Action)) {
        match byte {
            b'\n' => out(Action::Newline),
            b'\r' => out(Action::Return),
            b'\t' => out(Action::Tab),
            0x08 => out(Action::Backspace),
            0x1B => self.state = State::Escape,
            0x20..=0x7E => out(Action::Glyph(byte)),
            0xC2..=0xDF => self.state = State::Utf8 { cp: (byte & 0x1F) as u32, owed: 1 },
            0xE0..=0xEF => self.state = State::Utf8 { cp: (byte & 0x0F) as u32, owed: 2 },
            0xF0..=0xF4 => self.state = State::Utf8 { cp: (byte & 0x07) as u32, owed: 3 },
            // A stray continuation byte, or one no UTF-8 encoder writes.
            0x80..=0xFF => out(Action::Glyph(glyphs::REPLACEMENT)),
            // Every other C0 control, and DEL: nothing to draw.
            _ => {}
        }
    }
}

/// Rows of text: what each cell holds, where the cursor is, and which rows changed.
///
/// Rows are a **ring**: logical row 0 (the top of the screen) is physical row `top`, so a scroll
/// moves `top` and clears the rows it exposes instead of copying every cell up. That matters
/// after userspace has the screen, when every line anyone prints still lands here (a later stop
/// repaints from it) and nothing is drawn.
pub struct Grid {
    geometry: Geometry,
    /// Glyph indices, `MAX_COLS` to a physical row.
    cells: [u8; CELLS],
    top: usize,
    row: usize,
    col: usize,
    decoder: Decoder,
    /// Logical rows changed since [`take_damage`](Self::take_damage), inclusive.
    damage: Option<(usize, usize)>,
}

impl Grid {
    /// A grid with no rows, which ignores every write until [`reset`](Self::reset).
    pub const fn new() -> Self {
        Self {
            geometry: Geometry { scale: 1, cols: 0, rows: 0 },
            cells: [BLANK; CELLS],
            top: 0,
            row: 0,
            col: 0,
            decoder: Decoder::new(),
            damage: None,
        }
    }

    /// Clear to `geometry`, with the cursor at the top-left. A geometry larger than the storage
    /// is clamped to it.
    pub fn reset(&mut self, geometry: Geometry) {
        self.geometry = Geometry {
            scale: geometry.scale,
            cols: geometry.cols.min(MAX_COLS),
            rows: geometry.rows.min(MAX_ROWS),
        };
        self.cells = [BLANK; CELLS];
        self.top = 0;
        self.row = 0;
        self.col = 0;
        self.decoder = Decoder::new();
        self.damage = None;
    }

    /// The layout in use.
    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    /// Apply a byte stream.
    pub fn write(&mut self, bytes: &[u8]) {
        if self.geometry.cols == 0 || self.geometry.rows == 0 {
            return;
        }
        let mut decoder = self.decoder;
        for &b in bytes {
            decoder.feed(b, &mut |action| self.apply(action));
        }
        self.decoder = decoder;
    }

    /// The glyph in `col` of logical row `row` (row 0 is the top of the screen).
    pub fn glyph_at(&self, row: usize, col: usize) -> u8 {
        if row >= self.geometry.rows || col >= self.geometry.cols {
            return BLANK;
        }
        self.cells[self.physical(row) * MAX_COLS + col]
    }

    /// The cursor, as `(row, col)`. `col` can equal the column count: the next glyph wraps.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    /// The logical rows changed since the last call, inclusive, and forget them.
    pub fn take_damage(&mut self) -> Option<(usize, usize)> {
        self.damage.take()
    }

    fn physical(&self, row: usize) -> usize {
        (self.top + row) % self.geometry.rows
    }

    fn mark(&mut self, first: usize, last: usize) {
        self.damage = Some(match self.damage {
            Some((a, b)) => (a.min(first), b.max(last)),
            None => (first, last),
        });
    }

    fn apply(&mut self, action: Action) {
        let cols = self.geometry.cols;
        match action {
            Action::Glyph(g) => {
                if self.col >= cols {
                    self.newline();
                }
                let at = self.physical(self.row) * MAX_COLS + self.col;
                self.cells[at] = g;
                self.mark(self.row, self.row);
                self.col += 1;
            }
            Action::Newline => self.newline(),
            Action::Return => self.col = 0,
            Action::Tab => self.col = ((self.col / 8 + 1) * 8).min(cols),
            Action::Backspace => self.col = self.col.min(cols).saturating_sub(1),
        }
    }

    fn newline(&mut self) {
        let rows = self.geometry.rows;
        self.col = 0;
        if self.row + 1 < rows {
            self.row += 1;
            return;
        }
        let jump = (rows / JUMP_DIVISOR).max(1);
        self.top = (self.top + jump) % rows;
        for row in rows - jump..rows {
            let at = self.physical(row) * MAX_COLS;
            self.cells[at..at + MAX_COLS].fill(BLANK);
        }
        self.row = rows - jump;
        self.mark(0, rows - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(cols: usize, rows: usize) -> Box<Grid> {
        let mut g = Box::new(Grid::new());
        g.reset(Geometry { scale: 1, cols, rows });
        g
    }

    /// Logical row `row` as text, trailing blanks trimmed.
    fn line(g: &Grid, row: usize) -> String {
        let s: String = (0..g.geometry().cols)
            .map(|c| glyphs::char_of(g.glyph_at(row, c)).unwrap_or('\u{FFFD}'))
            .collect();
        s.trim_end().to_string()
    }

    fn actions(bytes: &[u8]) -> Vec<Action> {
        let mut d = Decoder::new();
        let mut out = Vec::new();
        for &b in bytes {
            d.feed(b, &mut |a| out.push(a));
        }
        out
    }

    #[test]
    fn the_geometry_scales_up_only_when_the_storage_would_not_hold_the_screen() {
        assert_eq!(Geometry::for_screen(1280, 800), Some(Geometry { scale: 1, cols: 160, rows: 50 }));
        assert_eq!(Geometry::for_screen(1366, 768), Some(Geometry { scale: 1, cols: 170, rows: 48 }));
        assert_eq!(Geometry::for_screen(2048, 1600), Some(Geometry { scale: 1, cols: 256, rows: 100 }));
        assert_eq!(Geometry::for_screen(2056, 1600), Some(Geometry { scale: 2, cols: 128, rows: 50 }));
        assert_eq!(Geometry::for_screen(3840, 2160), Some(Geometry { scale: 2, cols: 240, rows: 67 }));
        assert_eq!(Geometry::for_screen(7, 800), None);
        assert_eq!(Geometry::for_screen(0, 0), None);
    }

    #[test]
    fn utf8_is_one_cell_per_character_even_split_across_writes() {
        let dash = glyphs::index_of('—').unwrap();
        let mut g = grid(20, 3);
        let bytes = "a—b".as_bytes();
        // Every split point, including inside the three-byte dash.
        for split in 0..=bytes.len() {
            g.reset(g.geometry());
            g.write(&bytes[..split]);
            g.write(&bytes[split..]);
            assert_eq!([g.glyph_at(0, 0), g.glyph_at(0, 1), g.glyph_at(0, 2)], [b'a', dash, b'b'], "split at {split}");
            assert_eq!(g.cursor(), (0, 3));
        }
    }

    #[test]
    fn a_broken_sequence_costs_its_character_and_nothing_after_it() {
        use Action::Glyph;
        let r = glyphs::REPLACEMENT;
        assert_eq!(actions(b"\xE2\x80x"), [Glyph(r), Glyph(b'x')], "cut short by an ASCII byte");
        assert_eq!(actions(b"\xE2\x80\n"), [Glyph(r), Action::Newline], "cut short by a control");
        assert_eq!(actions(b"\x80y"), [Glyph(r), Glyph(b'y')], "a stray continuation");
        assert_eq!(actions(b"\xF0\x9F\x98\x80"), [Glyph(r)], "well-formed, but not in the face");
    }

    #[test]
    fn escape_sequences_are_swallowed_and_controls_are_actions() {
        use Action::*;
        assert_eq!(actions(b"\x1b[1;32mok\x1b[0m"), [Glyph(b'o'), Glyph(b'k')]);
        assert_eq!(actions(b"\x1b7a"), [Glyph(b'a')], "a two-byte escape");
        assert_eq!(actions(b"\x1b[12\nz"), [Newline, Glyph(b'z')], "a control ends a sequence and still acts");
        assert_eq!(actions(b"\r\t\x08\x07\x7f"), [Return, Tab, Backspace], "BEL and DEL draw nothing");
    }

    #[test]
    fn a_full_row_wraps_on_the_next_glyph_not_on_reaching_the_edge() {
        let mut g = grid(4, 3);
        g.write(b"abcd");
        assert_eq!(g.cursor(), (0, 4), "the cursor waits at the edge");
        g.write(b"\n");
        assert_eq!(g.cursor(), (1, 0), "so a line exactly as wide as the screen costs no blank row");
        g.write(b"efghi");
        assert_eq!((line(&g, 1).as_str(), line(&g, 2).as_str()), ("efgh", "i"));
    }

    #[test]
    fn tab_return_and_backspace_move_the_cursor_within_the_row() {
        let mut g = grid(20, 2);
        g.write(b"ab\tc");
        assert_eq!(line(&g, 0), "ab      c");
        g.write(b"\rX\x08Y");
        assert_eq!(line(&g, 0), "Yb      c");
        g.write(b"\r\t\t\t");
        assert_eq!(g.cursor(), (0, 20), "a tab stops at the edge");
    }

    #[test]
    fn reaching_the_bottom_jumps_a_quarter_and_keeps_the_newest_line_on_screen() {
        let mut g = grid(10, 8);
        for i in 0..8 {
            g.write(format!("line{i}\n").as_bytes());
        }
        // The eighth newline wanted a ninth row: two rows (a quarter of eight) scrolled off.
        assert_eq!(line(&g, 0), "line2");
        assert_eq!(line(&g, 5), "line7");
        assert_eq!((line(&g, 6).as_str(), line(&g, 7).as_str()), ("", ""), "the exposed rows are clear");
        assert_eq!(g.cursor(), (6, 0));
        g.write(b"next");
        assert_eq!(line(&g, 6), "next");
        // Far past one lap of the ring.
        for i in 0..1000 {
            g.write(format!("n{i}\n").as_bytes());
        }
        assert_eq!(line(&g, g.cursor().0 - 1), "n999");
    }

    #[test]
    fn damage_names_the_rows_written_and_all_of_them_after_a_scroll() {
        let mut g = grid(10, 8);
        assert_eq!(g.take_damage(), None);
        g.write(b"a\n\nb");
        assert_eq!(g.take_damage(), Some((0, 2)));
        assert_eq!(g.take_damage(), None, "taking it clears it");
        g.write(b"\n\n\n\n\n");
        assert_eq!(g.cursor(), (7, 0));
        assert_eq!(g.take_damage(), None, "a newline with a row to spare changes no cell");
        g.write(b"\n");
        assert_eq!(g.take_damage(), Some((0, 7)), "a scroll moves every row");
    }

    #[test]
    fn a_grid_with_no_geometry_ignores_writes() {
        let mut g = Box::new(Grid::new());
        g.write(b"hello\n");
        assert_eq!(g.cursor(), (0, 0));
        assert_eq!(g.take_damage(), None);
    }
}
