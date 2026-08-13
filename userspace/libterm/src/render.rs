//! Cells to pixels.
//!
//! The one place `libterm` reaches for `libdraw`, and the reason the crate depends on it at
//! all. Everything else here is a function of values with no notion of a pixel.
//!
//! ## Cell metrics are derived, not chosen
//!
//! A cell is as wide as the font's advance and as tall as its line height, so a grid's pixel
//! size falls out of the font rather than being asserted next to it. Choosing a cell size and
//! hoping the font fits is how a terminal ends up with glyphs clipped on the right or rows
//! that overlap by a pixel — and the two numbers would then have to be kept in step by hand.
//!
//! ## Damage is per cell row
//!
//! [`render_rows`] draws the rows it is given, and [`Grid::take_damage`] is what produces that
//! list. A keystroke dirties one row; the union of dirty rows is the rectangle that reaches
//! `Commit`. Repainting a window per keystroke is exactly what the toolkit's diff exists to
//! avoid one layer up, and a terminal that did it there would undo that work.

use libdraw::framebuffer::Framebuffer;
use libdraw::geom::{Point, Rect, Size};
use libdraw::text::Font;

use crate::cell::{Cell, Flags, Palette};
use crate::grid::Grid;

/// A cell's size in pixels, and where its baseline sits.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Metrics {
    /// Cell width — the font's advance. Monospace, so every glyph has the same one.
    pub cell_w: u32,
    /// Cell height — the font's baseline-to-baseline distance.
    pub cell_h: u32,
    /// Baseline offset from the top of a cell.
    pub ascent: u32,
    /// The size the font was measured at.
    pub px: f32,
}

impl Metrics {
    /// Measure `font` at `px`.
    ///
    /// Width comes from `M`'s advance rather than from an average: in a monospace font every
    /// advance is the same, and taking one glyph's says so. If a proportional font is ever
    /// passed here the grid will be wrong, which is correct — a proportional font has no cell
    /// width, and silently averaging one would hide that.
    ///
    /// Every value is at least 1: a zero-width cell makes a grid of zero-width columns, and
    /// every rectangle below would be empty.
    pub fn new(font: &Font, px: f32) -> Metrics {
        let v = font.v_metrics(px);
        Metrics {
            cell_w: font.advance('M', px).max(1),
            cell_h: (libm::ceilf(v.line_height) as u32).max(1),
            ascent: libm::ceilf(v.ascent).max(0.0) as u32,
            px,
        }
    }

    /// The pixel size of a `cols` × `rows` grid.
    pub fn pixel_size(&self, cols: usize, rows: usize) -> Size {
        Size::new(self.cell_w * cols as u32, self.cell_h * rows as u32)
    }

    /// The rectangle one cell occupies, relative to the grid's top-left.
    pub fn cell_rect(&self, row: usize, col: usize) -> Rect {
        Rect::new(
            (col as u32 * self.cell_w) as i32,
            (row as u32 * self.cell_h) as i32,
            self.cell_w,
            self.cell_h,
        )
    }

    /// The rectangle one row occupies, `cols` wide.
    pub fn row_rect(&self, row: usize, cols: usize) -> Rect {
        Rect::new(0, (row as u32 * self.cell_h) as i32, self.cell_w * cols as u32, self.cell_h)
    }
}

/// Draw one cell into `fb` at `origin + cell_rect`, inverted if the cursor is on it.
fn draw_cell<F: Framebuffer + ?Sized>(
    fb: &mut F,
    font: &Font,
    m: &Metrics,
    palette: &Palette,
    origin: Point,
    row: usize,
    col: usize,
    cell: Cell,
    cursor: bool,
) {
    let r = m.cell_rect(row, col);
    let rect = Rect::new(origin.x + r.origin.x, origin.y + r.origin.y, r.size.w, r.size.h);
    let (fg, bg) = cell.attrs.resolve(palette);
    // **The cursor is the cell drawn inverted**, not a shape drawn over it. That is what makes
    // the character under it stay readable, and it needs no colour of its own — a cursor with
    // its own colour is a third thing to keep in step with a theme.
    let (fg, bg) = if cursor { (bg, fg) } else { (fg, bg) };

    fb.fill_rect(rect, bg);
    if cell.ch != ' ' {
        let mut buf = [0u8; 4];
        font.draw_str(
            fb,
            Point::new(rect.origin.x, rect.origin.y + m.ascent as i32),
            cell.ch.encode_utf8(&mut buf),
            m.px,
            fg,
            rect,
        );
    }
    if cell.attrs.flags.contains(Flags::UNDERLINE) {
        // One pixel below the baseline, clamped inside the cell so a font whose descent is
        // shallow cannot draw into the row beneath.
        let y = (rect.origin.y + m.ascent as i32 + 1).min(rect.bottom() as i32 - 1);
        fb.fill_rect(Rect::new(rect.origin.x, y, rect.size.w, 1), fg);
    }
}

/// Draw `rows` of `grid` into `fb`, with the grid's top-left at `origin`.
///
/// Rows outside the grid are skipped rather than refused: [`Grid::take_damage`] cannot produce
/// one, but a caller that unions damage across a resize could. **The skip comes from
/// [`Grid::cell`] being total** — it returns `None` outside the screen and the loop below
/// continues on that — rather than from a bounds check of its own. There was one, and a
/// break-test showed it changed nothing: two guards for one condition, only one of them
/// reachable.
pub fn render_rows<F: Framebuffer + ?Sized>(
    fb: &mut F,
    grid: &Grid,
    font: &Font,
    m: &Metrics,
    palette: &Palette,
    origin: Point,
    rows: &[usize],
) {
    render_view(fb, grid, font, m, palette, origin, grid.top_line(), rows);
}

/// Draw `rows` of a view whose first line is absolute line `top`.
///
/// The scrolled-back render. `rows` are **viewport** rows, not screen rows, and everything the
/// difference implies is [`Grid::view_cell`]'s and [`Grid::view_cursor`]'s: which half of the
/// history a row comes from, and whether the cursor is in view at all. That keeps this loop the
/// same loop it was for the live screen, which is what stops the two renders drifting apart.
pub fn render_view<F: Framebuffer + ?Sized>(
    fb: &mut F,
    grid: &Grid,
    font: &Font,
    m: &Metrics,
    palette: &Palette,
    origin: Point,
    top: u64,
    rows: &[usize],
) {
    render_each(fb, grid, font, m, palette, origin, top, rows.iter().copied());
}

/// The shared body of [`render`], [`render_rows`] and [`render_view`].
fn render_each<F: Framebuffer + ?Sized>(
    fb: &mut F,
    grid: &Grid,
    font: &Font,
    m: &Metrics,
    palette: &Palette,
    origin: Point,
    top: u64,
    rows: impl Iterator<Item = usize>,
) {
    let cursor_at = grid.view_cursor(top);
    for row in rows {
        for col in 0..grid.cols() {
            let Some(cell) = grid.view_cell(top, row, col) else { continue };
            let cursor = cursor_at == Some((row, col));
            draw_cell(fb, font, m, palette, origin, row, col, cell, cursor);
        }
    }
}

/// Draw the whole grid.
pub fn render<F: Framebuffer + ?Sized>(
    fb: &mut F,
    grid: &Grid,
    font: &Font,
    m: &Metrics,
    palette: &Palette,
    origin: Point,
) {
    // Iterating rather than collecting a `Vec` of every index: a full repaint is not hot, and
    // an allocation whose only purpose is to be immediately iterated is one a reader has to
    // stop and think about.
    render_each(fb, grid, font, m, palette, origin, grid.top_line(), 0..grid.rows());
}

/// The fixed grid the display gate compares, and the render of it.
///
/// The same idea as [`libdraw::scene`] and `libui::reference`, for the third layer: both sides
/// render *this* and compare pixel for pixel, so a target build that rasterises or lays out
/// differently from the host shows up as a named pixel rather than as a picture someone has to
/// judge.
///
/// **On screen since M5 Part B**: `ui-testclient` presents it in a window of its own, between
/// the toolkit's and the scene's, and `cargo xtask check-display` compares that region against
/// this module rendered on the host. Until then it was a fixture with tests on it rather than a
/// gate (PR #190 review, finding 3).
///
/// **This rather than `nxterm`'s own window**, which is also on screen and is deterministic on
/// its first frame. A live terminal showing a boot banner exercises one plain line of text; this
/// stream is built so that each of its lines fails differently, and a gate should compare the
/// picture that discriminates rather than the one that happens to be there.
pub mod reference {
    use super::*;
    use crate::parse::{MAX_PER_BYTE, Op, Parser};

    /// Columns in the reference grid.
    pub const COLS: usize = 20;
    /// Rows in it.
    pub const ROWS: usize = 6;
    /// The size it is rendered at.
    pub const FONT_PX: f32 = 16.0;

    /// The byte stream the reference grid is built from.
    ///
    /// Chosen so each line fails differently:
    ///
    /// | Line | What it catches |
    /// |---|---|
    /// | plain text | the baseline and the advance |
    /// | every attribute | bold's brightening, underline's rule, reverse's swap |
    /// | a coloured background then `EL` | the erase fill taking the current background |
    /// | text longer than the row | deferred wrap, at the exact column |
    /// | a `CUP` and an overwrite | cursor addressing being one-based on the wire |
    ///
    /// The cursor is left mid-row on the last line, so the inverted cell is somewhere a
    /// whole-row comparison must notice.
    pub const SOURCE: &str = concat!(
        "Nitrox\r\n",
        "\x1b[1mbold\x1b[m \x1b[4mund\x1b[m \x1b[7mrev\x1b[m\r\n",
        "\x1b[44m\x1b[K\x1b[m\r\n",
        "wrapping past the edge\r\n",
        "\x1b[6;4Hxy",
    );

    /// The reference grid, built by feeding [`SOURCE`] through the real parser.
    ///
    /// Through the parser rather than by calling `Grid` directly: this is the only place all
    /// three of A3, A4 and A5 are exercised together, and a mismatch in what an `Op` *means*
    /// would otherwise survive both halves being individually right.
    pub fn grid() -> Grid {
        let mut g = Grid::new(COLS, ROWS);
        let mut p = Parser::new();
        let mut out = [Op::Print('\0'); MAX_PER_BYTE];
        for &b in SOURCE.as_bytes() {
            let n = p.feed(b, &mut out);
            g.apply_all(&out[..n]);
        }
        g
    }

    /// The size the reference renders at with `font`.
    ///
    /// A function rather than a constant, unlike `libui::reference::WIDTH`: a terminal's size
    /// *is* its cell metrics, so the number cannot exist before the font does. Both ends of the
    /// gate have the font — the client reads it off the disk, the host reads it out of
    /// `assets/` — so this is what they agree through.
    pub fn size(font: &Font) -> libdraw::geom::Size {
        Metrics::new(font, FONT_PX).pixel_size(COLS, ROWS)
    }

    /// The stride [`render_with`] uses, for `font`.
    ///
    /// **Padded**, for the reason `libdraw::scene::SCREEN_PITCH` is: code that computes a row
    /// offset from the width rather than the pitch skews every row after the first, and equal
    /// numbers hide it.
    pub fn pitch(font: &Font) -> usize {
        size(font).w as usize * 4 + 12
    }

    /// Render the reference grid with `font`.
    pub fn render_with(font: &Font) -> libdraw::framebuffer::MemFramebuffer {
        use libdraw::format::PixelFormat;
        use libdraw::framebuffer::{Geometry, MemFramebuffer};

        let g = grid();
        let m = Metrics::new(font, FONT_PX);
        let size = size(font);
        let pitch = pitch(font);
        let geometry = Geometry::with_pitch(size.w, size.h, pitch, PixelFormat::XRGB8888)
            .expect("a padded pitch always holds a row");
        let mut fb = MemFramebuffer::new(geometry);
        let palette = Palette::default();
        // Cleared first: `render` draws each cell's background, but a caller comparing the
        // whole buffer needs the padding-adjacent bytes deterministic too.
        fb.clear(palette.background);
        render(&mut fb, &g, font, &m, &palette, Point::new(0, 0));
        fb
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{Ansi, Colour};
    use crate::parse::{Op, Sgr};
    use libdraw::format::{PixelFormat, Rgb};
    use libdraw::framebuffer::{Geometry, MemFramebuffer};

    const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSansMono.ttf");

    fn font() -> Font {
        Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses")
    }

    fn fb_for(m: &Metrics, cols: usize, rows: usize) -> MemFramebuffer {
        let s = m.pixel_size(cols, rows);
        MemFramebuffer::new(Geometry::packed(s.w, s.h, PixelFormat::XRGB8888))
    }

    /// Whether a cell is drawn *inverted* — the cursor's rendering.
    ///
    /// **Not "contains the foreground colour"**, which is what the first version of the phantom
    /// test asked and which is true of any cell holding a glyph: ink is the foreground. An
    /// inverted cell is foreground almost everywhere, so counting is what tells the two apart.
    fn is_inverted(fb: &MemFramebuffer, r: Rect, p: &Palette) -> bool {
        let mut fg = 0usize;
        for y in r.origin.y..r.bottom() as i32 {
            for x in r.origin.x..r.right() as i32 {
                if fb.get_pixel(x as u32, y as u32) == Some(p.foreground) {
                    fg += 1;
                }
            }
        }
        fg * 2 > (r.size.w * r.size.h) as usize
    }

    /// Every distinct colour in a rectangle.
    fn colours(fb: &MemFramebuffer, r: Rect) -> alloc::vec::Vec<Rgb> {
        let mut v = alloc::vec::Vec::new();
        for y in r.origin.y..r.bottom() as i32 {
            for x in r.origin.x..r.right() as i32 {
                if let Some(c) = fb.get_pixel(x as u32, y as u32)
                    && !v.contains(&c)
                {
                    v.push(c);
                }
            }
        }
        v
    }

    #[test]
    fn cell_metrics_come_from_the_font() {
        // Derived, not chosen. A hardcoded 8x16 would be wrong for this font at this size, and
        // wrong in a way that clips glyphs rather than one that fails a build.
        let f = font();
        let m = Metrics::new(&f, 16.0);
        assert_eq!(m.cell_w, f.advance('M', 16.0));
        assert!(m.cell_h >= m.ascent, "the baseline is below the cell: {m:?}");
        assert!(m.cell_w > 0 && m.cell_h > 0);
        // A monospace font gives every glyph the same advance, which is the assumption the
        // grid rests on. Checked rather than believed.
        for c in ['i', 'M', 'W', '.', '0'] {
            assert_eq!(f.advance(c, 16.0), m.cell_w, "{c:?} has a different advance");
        }
    }

    #[test]
    fn metrics_scale_with_the_size() {
        let f = font();
        let small = Metrics::new(&f, 12.0);
        let large = Metrics::new(&f, 24.0);
        assert!(large.cell_w > small.cell_w && large.cell_h > small.cell_h);
    }

    #[test]
    fn a_rendered_cell_paints_its_own_background() {
        let f = font();
        let m = Metrics::new(&f, 16.0);
        let p = Palette::default();
        // Three columns, and the cursor parked on the middle one: the cell *after* a print is
        // where the cursor lands, and it is drawn inverted — so checking "the neighbour kept
        // the default" one cell along measures the cursor, not the neighbour. The first
        // version of this test did exactly that and failed for the right reason.
        let mut g = Grid::new(3, 1);
        let mut fb = fb_for(&m, 3, 1);
        g.apply_all(&[Op::Attr(Sgr::Background(Colour::Ansi(Ansi::Blue))), Op::Print(' ')]);
        render(&mut fb, &g, &f, &m, &p, Point::new(0, 0));
        assert_eq!(
            fb.get_pixel(1, 1),
            Some(p.ansi[Ansi::Blue.index()]),
            "the cell's background did not reach the pixels"
        );
        assert_eq!(g.cursor(), (0, 1), "the cursor is not where this test assumes");
        // Column 2: past the cursor, so its default background is its own.
        assert_eq!(fb.get_pixel(m.cell_w * 2 + 1, 1), Some(p.background));
    }

    #[test]
    fn the_cursor_cell_is_drawn_inverted() {
        // Inverted rather than overdrawn, so the character under it stays readable and the
        // cursor needs no colour of its own.
        let f = font();
        let m = Metrics::new(&f, 16.0);
        let p = Palette::default();
        let mut g = Grid::new(2, 1);
        g.apply(Op::Print('a')); // cursor now on column 1
        let mut fb = fb_for(&m, 2, 1);
        render(&mut fb, &g, &f, &m, &p, Point::new(0, 0));

        assert!(
            is_inverted(&fb, m.cell_rect(0, 1), &p),
            "the cursor cell is not drawn inverted"
        );
        let elsewhere = colours(&fb, m.cell_rect(0, 0));
        assert!(elsewhere.contains(&p.background), "the non-cursor cell lost its background");
    }

    #[test]
    fn only_the_rows_asked_for_are_drawn() {
        // The whole point of per-row damage. A render that ignored its row list would repaint
        // the window on every keystroke, undoing the toolkit's diff one layer up.
        let f = font();
        let m = Metrics::new(&f, 16.0);
        let p = Palette::default();
        let mut g = Grid::new(4, 3);
        g.apply_all(&[Op::Print('x'), Op::CarriageReturn, Op::LineFeed, Op::Print('y')]);

        let mut fb = fb_for(&m, 4, 3);
        let marker = Rgb::new(0xFF, 0x00, 0xFF);
        fb.clear(marker);
        render_rows(&mut fb, &g, &f, &m, &p, Point::new(0, 0), &[1]);

        assert_eq!(fb.get_pixel(1, 1), Some(marker), "row 0 was drawn and should not have been");
        assert_ne!(
            fb.get_pixel(1, m.cell_h + 1),
            Some(marker),
            "row 1 was not drawn and should have been"
        );
        assert_eq!(
            fb.get_pixel(1, m.cell_h * 2 + 1),
            Some(marker),
            "row 2 was drawn and should not have been"
        );
    }

    #[test]
    fn the_damage_pairing_leaves_no_phantom_cursor() {
        // **The bug that lived between A4 and A5**, and the level it had to be tested at.
        // Each half was defensible on its own: the grid reported no damage for a cursor that
        // moved, and the render painted the cursor into its cell. Driving the documented
        // `take_damage` -> `render_rows` pairing is what shows the pair is wrong — a phantom
        // inverted block left at the end of every line a paragraph wraps on.
        let f = font();
        let m = Metrics::new(&f, 16.0);
        let p = Palette::default();
        let mut g = Grid::new(4, 3);
        let mut fb = fb_for(&m, 4, 3);

        g.apply_all(&[Op::Print('a'), Op::Print('b'), Op::Print('c'), Op::Print('d')]);
        let d = g.take_damage();
        render_rows(&mut fb, &g, &f, &m, &p, Point::new(0, 0), &d);
        assert!(is_inverted(&fb, m.cell_rect(0, 3), &p), "the cursor is not drawn where it rests");

        // One more character wraps it to row 1.
        g.apply(Op::Print('e'));
        let d = g.take_damage();
        render_rows(&mut fb, &g, &f, &m, &p, Point::new(0, 0), &d);
        assert!(
            is_inverted(&fb, m.cell_rect(1, 1), &p),
            "the cursor did not appear on the row it wrapped to"
        );
        assert!(
            !is_inverted(&fb, m.cell_rect(0, 3), &p),
            "a phantom cursor block was left at the end of row 0"
        );
    }

    #[test]
    fn a_cursor_key_moves_the_block_on_screen() {
        // Scenario A: no printing at all, just movement. The row the cursor left has to
        // repaint or the block does not move.
        let f = font();
        let m = Metrics::new(&f, 16.0);
        let p = Palette::default();
        let mut g = Grid::new(4, 1);
        let mut fb = fb_for(&m, 4, 1);

        g.apply_all(&[Op::Print('h'), Op::Print('i')]);
        let d = g.take_damage();
        render_rows(&mut fb, &g, &f, &m, &p, Point::new(0, 0), &d);
        assert!(is_inverted(&fb, m.cell_rect(0, 2), &p));

        g.apply(Op::MoveBy { rows: 0, cols: -1 });
        let d = g.take_damage();
        render_rows(&mut fb, &g, &f, &m, &p, Point::new(0, 0), &d);
        assert!(
            !is_inverted(&fb, m.cell_rect(0, 2), &p),
            "the cursor block stayed where it was"
        );
        assert!(is_inverted(&fb, m.cell_rect(0, 1), &p), "the cursor did not arrive");
    }

    #[test]
    fn a_row_out_of_range_is_skipped_rather_than_panicking() {
        let f = font();
        let m = Metrics::new(&f, 16.0);
        let g = Grid::new(4, 2);
        let mut fb = fb_for(&m, 4, 2);
        render_rows(&mut fb, &g, &f, &m, &Palette::default(), Point::new(0, 0), &[0, 7, 99]);
    }

    #[test]
    fn the_origin_offsets_everything() {
        // The grid is drawn inside a window with chrome around it, so it never starts at the
        // buffer's corner in practice.
        let f = font();
        let m = Metrics::new(&f, 16.0);
        let p = Palette::default();
        let mut g = Grid::new(2, 1);
        g.apply_all(&[
            Op::Attr(Sgr::Background(Colour::Ansi(Ansi::Red))),
            Op::Print(' '),
        ]);
        let s = m.pixel_size(3, 2);
        let mut fb = MemFramebuffer::new(Geometry::packed(s.w, s.h, PixelFormat::XRGB8888));
        let marker = Rgb::new(0x00, 0xFF, 0x00);
        fb.clear(marker);
        render(&mut fb, &g, &f, &m, &p, Point::new(m.cell_w as i32, m.cell_h as i32));

        assert_eq!(fb.get_pixel(1, 1), Some(marker), "the origin was ignored");
        assert_eq!(
            fb.get_pixel(m.cell_w + 1, m.cell_h + 1),
            Some(p.ansi[Ansi::Red.index()]),
            "the grid was not drawn at the origin it was given"
        );
    }

    #[test]
    fn underline_draws_below_the_baseline_and_inside_the_cell() {
        let f = font();
        let m = Metrics::new(&f, 16.0);
        let p = Palette::default();
        // **The cursor is parked two rows away**, which the first version of this test did not
        // do — and on a one-column grid the cursor stays on the cell it just wrote, so the
        // underlined cell was drawn *inverted* and filled with the foreground. The test then
        // passed on the inversion rather than on the underline, and stayed green when the rule
        // was moved into the next row. Found by a break-test.
        let mut g = Grid::new(1, 3);
        g.apply_all(&[
            Op::Attr(Sgr::Underline),
            Op::Print(' '),
            Op::CarriageReturn,
            Op::LineFeed,
            Op::CarriageReturn,
            Op::LineFeed,
        ]);
        assert_eq!(g.cursor(), (2, 0), "the cursor is not where this test needs it");
        let mut fb = fb_for(&m, 1, 3);
        render(&mut fb, &g, &f, &m, &p, Point::new(0, 0));

        let row0 = colours(&fb, m.cell_rect(0, 0));
        assert!(row0.contains(&p.foreground), "no underline was drawn: {row0:?}");
        assert!(row0.contains(&p.background), "the cell was filled, not underlined: {row0:?}");
        // It must not bleed into the row beneath, which would look like the next line's text
        // having a stray rule over it.
        let row1 = colours(&fb, m.cell_rect(1, 0));
        assert!(!row1.contains(&p.foreground), "the underline reached the next row");
    }

    #[test]
    fn the_underline_clamp_keeps_a_tight_cell_in_bounds() {
        // The clamp on the underline's `y` never engages for DejaVu at 16px — `ascent + 1` is
        // comfortably inside the cell — so removing it fails nothing. That makes it *unproven*
        // rather than redundant, which is the opposite of the bounds check removed from
        // `render_rows`: a font whose line height equals its ascent reaches it.
        //
        // Constructed rather than found, because no font in the tree has those metrics.
        let f = font();
        let mut m = Metrics::new(&f, 16.0);
        m.cell_h = m.ascent; // baseline flush with the bottom of the cell
        let p = Palette::default();
        let mut g = Grid::new(1, 2);
        g.apply_all(&[
            Op::Attr(Sgr::Underline),
            Op::Print(' '),
            Op::CarriageReturn,
            Op::LineFeed,
        ]);
        let mut fb = fb_for(&m, 1, 2);
        render_rows(&mut fb, &g, &f, &m, &p, Point::new(0, 0), &[0]);
        // Row 1 was not drawn at all, so any ink there came from row 0's underline.
        let row1 = colours(&fb, m.cell_rect(1, 0));
        assert!(!row1.contains(&p.foreground), "the underline escaped its cell: {row1:?}");
    }

    #[test]
    fn the_reference_grid_exercises_what_it_claims() {
        // The gate compares pixels; this asserts the *picture* is worth comparing. A reference
        // that lost its attributes to a typo in `SOURCE` would still compare equal on both
        // sides and prove nothing — the same vacuity `libdraw::scene` guards against.
        let g = reference::grid();
        let mut bold = 0;
        let mut underline = 0;
        let mut reverse = 0;
        let mut coloured_blank = 0;
        for r in 0..g.rows() {
            for c in 0..g.cols() {
                let cell = g.cell(r, c).unwrap();
                if cell.attrs.flags.contains(Flags::BOLD) {
                    bold += 1;
                }
                if cell.attrs.flags.contains(Flags::UNDERLINE) {
                    underline += 1;
                }
                if cell.attrs.flags.contains(Flags::REVERSE) {
                    reverse += 1;
                }
                if cell.ch == ' ' && cell.attrs.bg != Colour::Default {
                    coloured_blank += 1;
                }
            }
        }
        assert!(bold >= 4, "only {bold} bold cells");
        assert!(underline >= 3, "only {underline} underlined cells");
        assert!(reverse >= 3, "only {reverse} reversed cells");
        assert!(coloured_blank >= 10, "the erase-with-background line is not there");
        // The wrap: line 4 is longer than the grid is wide, so it must have continued.
        assert_ne!(g.cell(4, 0).unwrap().ch, ' ', "the long line did not wrap onto row 4");
        // The cursor was addressed to the last row, mid-line.
        assert_eq!(g.cursor(), (5, 5));
    }

    #[test]
    fn the_reference_render_is_deterministic_and_not_a_flat_fill() {
        let f = font();
        assert_eq!(
            reference::render_with(&f).into_bytes(),
            reference::render_with(&f).into_bytes()
        );
        let fb = reference::render_with(&f);
        let g = fb.geometry();
        let first = fb.get_pixel(0, 0).unwrap();
        let varied =
            (0..g.height).any(|y| (0..g.width).any(|x| fb.get_pixel(x, y).unwrap() != first));
        assert!(varied, "the reference render is one colour");
    }

    #[test]
    fn the_reference_render_is_the_size_its_metrics_say() {
        let f = font();
        let m = Metrics::new(&f, reference::FONT_PX);
        let fb = reference::render_with(&f);
        assert_eq!(fb.geometry().width, m.pixel_size(reference::COLS, reference::ROWS).w);
        assert_eq!(fb.geometry().height, m.pixel_size(reference::COLS, reference::ROWS).h);
        // The padded stride is the point of padding it.
        assert!(fb.geometry().pitch > fb.geometry().width as usize * 4);
    }
}

