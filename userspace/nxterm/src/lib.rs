//! `nxterm` — the terminal, minus the syscalls.
//!
//! The split every subsystem in this tree uses: state, `view` and `update` are functions of
//! values and host-test in milliseconds; the window, the buffers and the event pump are the
//! binary's.
//!
//! ## Where the bytes come from
//!
//! **A loopback, until Part C.** A key press is encoded by [`libterm::encode`] and fed straight
//! back through [`libterm::parse`] into the grid, so typing works on screen with nothing else
//! running. That is not a stub for its own sake: it exercises the whole of Part A — encoder,
//! parser, grid, render — driven by real key events, and Part C replaces exactly one function
//! call with a write to the tty server's backend channel.
//!
//! **The loopback translates `\r` to `\r\n`.** A terminal receives `\r` from its own Enter key
//! and `LF` is *index*, so without this typing Enter would return to column 0 and overwrite the
//! line just typed. That translation is Unix's `ONLCR` and it belongs to the **line
//! discipline**, which is what Part C puts between these two halves — so it lives here, in one
//! obvious place, to be deleted rather than moved.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use libdraw::geom::{Rect, Size};
use libterm::cell::Palette;
use libterm::grid::Grid;
use libterm::parse::{MAX_PER_BYTE, Op, Parser};
use libterm::render::Metrics;
use libui::element::{Edge, Element, Insets, custom, dock, docked, offset, padding, stack};
use libui::widget::{Palette as UiPalette, ScrollState, WidgetState, button, menu_bar, scrollbar};

/// The `custom` node the grid is drawn into.
pub const GRID_KIND: u32 = 0x4772_6964;

/// The key on the menu bar's one item, so [`libui::layout::locate`] can find where it landed
/// and the popup can be placed under it.
pub const MENU_ITEM_KEY: u64 = 1;

/// Height of the menu bar, in pixels.
pub const BAR_H: u32 = 24;

/// Width of the scrollbar, in pixels.
pub const SCROLL_W: u32 = 12;

/// What the chrome can ask the terminal to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Msg {
    /// The menu bar's item was clicked.
    ToggleMenu,
    /// Erase the screen and home the cursor — `Ctrl-L`'s effect, from the menu.
    Clear,
    /// Erase the screen **and** the scrollback, and reset attributes.
    Reset,
}

/// Everything the terminal is.
pub struct App {
    /// The screen, the cursor, and the scrollback.
    pub grid: Grid,
    /// Bytes to grid operations. Held across writes because a sequence can be split across
    /// them — which is the ordinary case once a real backend is delivering in chunks.
    parser: Parser,
    /// Whether the menu's popup is showing.
    pub menu_open: bool,
    /// Where the menu item was laid out last frame, so the popup can go under it.
    ///
    /// **Last frame's**, which costs one frame of lag the very first time the menu opens and
    /// is invisible because the item does not move. The alternative is laying the tree out
    /// twice per frame — once to find the anchor, once with the popup in it — to remove a lag
    /// nobody can see.
    pub menu_anchor: Option<Rect>,
    /// Cell metrics, so `view` can size the grid node.
    pub metrics: Metrics,
    /// Colours for the cells.
    pub palette: Palette,
}

impl App {
    /// A terminal of `cols` × `rows` cells, drawn with `metrics`.
    pub fn new(cols: usize, rows: usize, metrics: Metrics) -> App {
        App {
            grid: Grid::new(cols, rows),
            parser: Parser::new(),
            menu_open: false,
            menu_anchor: None,
            metrics,
            palette: Palette::default(),
        }
    }

    /// The window size this terminal wants: the grid, plus its chrome.
    pub fn window_size(&self) -> Size {
        let g = self.metrics.pixel_size(self.grid.cols(), self.grid.rows());
        Size::new(g.w + SCROLL_W, g.h + BAR_H)
    }

    /// Where the grid's top-left sits inside the window.
    pub fn grid_origin(&self) -> libdraw::geom::Point {
        libdraw::geom::Point::new(0, BAR_H as i32)
    }

    /// Feed bytes from the program on the other end.
    ///
    /// The one call Part C replaces: today the loopback hands it what the keyboard produced,
    /// and then it will be what the tty server wrote.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut out = [Op::Print('\0'); MAX_PER_BYTE];
        for &b in bytes {
            let n = self.parser.feed(b, &mut out);
            self.grid.apply_all(&out[..n]);
        }
    }

    /// Send what a key press produced back to ourselves.
    ///
    /// **`\r` becomes `\r\n`** — see the module docs. Deleting this line is most of what
    /// Part C's line discipline has to take over.
    pub fn loopback(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if b == b'\r' {
                self.feed(b"\r\n");
            } else {
                self.feed(&[b]);
            }
        }
    }

    /// Apply a message from the chrome.
    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::ToggleMenu => self.menu_open = !self.menu_open,
            Msg::Clear => {
                // Through the parser, so "clear" means exactly what `Ctrl-L` means and there is
                // one implementation of it rather than a menu-shaped second one.
                self.feed(b"\x1b[2J\x1b[H");
                self.menu_open = false;
            }
            Msg::Reset => {
                let (cols, rows) = (self.grid.cols(), self.grid.rows());
                self.grid = Grid::new(cols, rows);
                self.parser = Parser::new();
                self.menu_open = false;
            }
        }
    }

    /// The scrollbar's state: the screen is a window onto the screen plus the scrollback.
    pub fn scroll(&self) -> ScrollState {
        let rows = self.grid.rows() as u32;
        let back = self.grid.scrollback().len() as u32;
        ScrollState { offset: back, visible: rows, total: back + rows }
    }

    /// The element tree for the current state.
    pub fn view(&self) -> Element<Msg> {
        let ui = UiPalette::default();
        let grid_px = self.metrics.pixel_size(self.grid.cols(), self.grid.rows());

        let bar = menu_bar(
            vec![
                button("Terminal", Msg::ToggleMenu, WidgetState::default(), &ui)
                    .key(MENU_ITEM_KEY),
            ],
            BAR_H,
            &ui,
        );
        let body = dock(
            vec![
                docked(Edge::Top, bar),
                docked(Edge::Right, scrollbar(self.scroll(), SCROLL_W, grid_px.h, &ui)),
            ],
            custom(GRID_KIND, grid_px),
        );

        // **The popup is a layer over the whole window, not a child of the bar.** A `Stack`
        // layer inside a 24-pixel bar would be clipped to 24 pixels; the menu has to escape its
        // parent, and the window is the first ancestor big enough to hold it.
        let Some(anchor) = self.menu_anchor.filter(|_| self.menu_open) else {
            return body;
        };
        stack(vec![
            body,
            offset(anchor.origin.x, anchor.bottom() as i32, self.menu(&ui)),
        ])
    }

    /// The popup's contents.
    fn menu(&self, ui: &UiPalette) -> Element<Msg> {
        use libui::element::{column, fill};
        let items = column(vec![
            button("Clear", Msg::Clear, WidgetState::default(), ui),
            button("Reset", Msg::Reset, WidgetState::default(), ui),
        ]);
        // A backing fill under the items, so the menu is opaque over whatever it covers.
        stack(vec![fill(ui.face), padding(Insets::all(2), items)])
    }
}

/// The rows of the grid a window-space rectangle covers.
///
/// The bridge between the toolkit's damage — one rectangle in window coordinates — and
/// `libterm`'s, which is a list of cell rows. The custom node's paint callback is handed a
/// clip and has to turn it into rows, and doing that arithmetic at the call site is how the
/// two disagree about which row a pixel is in.
pub fn rows_in(clip: Rect, grid_origin: libdraw::geom::Point, m: &Metrics, rows: usize) -> Vec<usize> {
    let top = clip.origin.y - grid_origin.y;
    let bottom = clip.bottom() as i32 - grid_origin.y;
    if bottom <= 0 || m.cell_h == 0 {
        return Vec::new();
    }
    let first = (top.max(0) as u32 / m.cell_h) as usize;
    // `bottom` is exclusive, so the last covered row is the one holding `bottom - 1`.
    let last = ((bottom - 1).max(0) as u32 / m.cell_h) as usize;
    (first..=last.min(rows.saturating_sub(1))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use libdraw::text::Font;
    use libui::layout::{FixedCell, layout, locate};

    const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSansMono.ttf");

    fn app() -> App {
        let f = Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses");
        App::new(20, 6, Metrics::new(&f, 16.0))
    }

    /// The grid's characters on `row`, trailing blanks trimmed.
    fn line(a: &App, row: usize) -> alloc::string::String {
        let s: alloc::string::String =
            (0..a.grid.cols()).map(|c| a.grid.cell(row, c).unwrap().ch).collect();
        s.trim_end().into()
    }

    #[test]
    fn typing_goes_round_the_loopback_and_lands_on_the_grid() {
        // The whole of Part A in one line: keycode -> bytes -> ops -> cells.
        let mut a = app();
        let mut out = [0u8; libterm::encode::MAX_ENCODED];
        for code in [35u16, 23, 18] {
            // h, i, e — arbitrary letters from the US table
            let n = libterm::encode::encode(code, 0, &mut out);
            a.loopback(&out[..n]);
        }
        assert_eq!(line(&a, 0), "hie");
    }

    #[test]
    fn enter_starts_a_new_line_rather_than_overwriting_the_last_one() {
        // **The `ONLCR` the loopback owns.** Enter sends `\r`, and `LF` is index — so without
        // the translation the cursor returns to column 0 and the next character overwrites
        // what was just typed, on the same line.
        let mut a = app();
        let mut out = [0u8; libterm::encode::MAX_ENCODED];
        let mut key = |a: &mut App, code: u16| {
            let n = libterm::encode::encode(code, 0, &mut out);
            a.loopback(&out[..n]);
        };
        key(&mut a, 35); // h
        key(&mut a, libkern::abi::KEY_ENTER);
        key(&mut a, 23); // i
        assert_eq!(line(&a, 0), "h");
        assert_eq!(line(&a, 1), "i", "the newline did not reach the second row");
    }

    #[test]
    fn a_key_with_no_encoding_types_nothing() {
        let mut a = app();
        let mut out = [0u8; libterm::encode::MAX_ENCODED];
        let n = libterm::encode::encode(libkern::abi::KEY_LEFTSHIFT, 0, &mut out);
        a.loopback(&out[..n]);
        assert_eq!(line(&a, 0), "");
    }

    #[test]
    fn clear_erases_the_screen_and_homes_the_cursor() {
        let mut a = app();
        a.feed(b"hello\r\nworld");
        a.update(Msg::Clear);
        assert_eq!(line(&a, 0), "");
        assert_eq!(line(&a, 1), "");
        assert_eq!(a.grid.cursor(), (0, 0));
    }

    #[test]
    fn clear_keeps_the_scrollback_and_reset_does_not() {
        // The distinction that makes two menu items rather than one: `Ctrl-L` clears what you
        // can see, and a reset throws away the history too.
        let mut a = app();
        for _ in 0..10 {
            a.feed(b"x\r\n");
        }
        assert!(a.grid.scrollback().len() > 0, "nothing scrolled off");

        a.update(Msg::Clear);
        assert!(a.grid.scrollback().len() > 0, "clear threw away the scrollback");

        a.update(Msg::Reset);
        assert_eq!(a.grid.scrollback().len(), 0, "reset kept the scrollback");
    }

    #[test]
    fn reset_forgets_attributes_that_were_in_force() {
        // A reset that only cleared cells would leave the *next* character bold and red.
        let mut a = app();
        a.feed(b"\x1b[1;31m");
        a.update(Msg::Reset);
        a.feed(b"x");
        assert_eq!(a.grid.cell(0, 0).unwrap().attrs, libterm::cell::Attributes::default());
    }

    #[test]
    fn reset_also_abandons_a_half_finished_sequence() {
        // The parser is state too. A reset that kept it would apply the tail of a sequence
        // from before the reset to whatever came after.
        let mut a = app();
        a.feed(b"\x1b[1"); // mid-sequence
        a.update(Msg::Reset);
        a.feed(b"m");
        assert_eq!(line(&a, 0), "m", "the parser was still mid-sequence after a reset");
    }

    #[test]
    fn the_menu_opens_and_closes() {
        let mut a = app();
        assert!(!a.menu_open);
        a.update(Msg::ToggleMenu);
        assert!(a.menu_open);
        a.update(Msg::ToggleMenu);
        assert!(!a.menu_open);
    }

    #[test]
    fn choosing_an_item_closes_the_menu() {
        // A menu that stayed open after its item was chosen would cover the thing it just
        // acted on.
        for msg in [Msg::Clear, Msg::Reset] {
            let mut a = app();
            a.update(Msg::ToggleMenu);
            a.update(msg);
            assert!(!a.menu_open, "{msg:?} left the menu open");
        }
    }

    #[test]
    fn the_popup_appears_only_when_the_menu_is_open_and_anchored() {
        let a0 = app();
        // Closed: the tree is the body, a `Dock`.
        assert!(matches!(a0.view().node, libui::element::Node::Dock { .. }));

        let mut a = app();
        a.menu_open = true;
        a.menu_anchor = Some(Rect::new(4, 0, 60, BAR_H));
        assert!(matches!(a.view().node, libui::element::Node::Stack(_)), "no popup layer");

        // Open but never laid out: no anchor, so no popup rather than one at the origin.
        let mut a = app();
        a.menu_open = true;
        a.menu_anchor = None;
        assert!(matches!(a.view().node, libui::element::Node::Dock { .. }));
    }

    #[test]
    fn the_popup_lands_under_the_menu_item() {
        // `locate` finds the item, `offset` puts the menu beneath it — the two toolkit
        // additions of B1a, doing their job through the real view.
        let mut a = app();
        let cell = FixedCell { w: 8, h: 16 };
        let bounds = Rect::new(0, 0, a.window_size().w, a.window_size().h);

        let closed = a.view();
        let l = layout(&closed, bounds, &cell);
        let item = locate(&closed, &l, MENU_ITEM_KEY).expect("the menu item is keyed");
        a.menu_anchor = Some(item);
        a.menu_open = true;

        let open = a.view();
        let l = layout(&open, bounds, &cell);
        let popup = l.children[1].rect;
        assert_eq!(popup.origin.x, item.origin.x, "the popup is not aligned with its item");
        assert_eq!(popup.origin.y, item.bottom() as i32, "the popup is not below its item");
        assert!(popup.size.h > 0 && popup.size.w > 0);
        // And it escapes the bar it hangs from, which is the whole reason it is a window-level
        // layer rather than a child of the menu bar.
        assert!(popup.bottom() > BAR_H as i64, "the popup was clipped to the bar");
    }

    #[test]
    fn the_window_is_the_grid_plus_its_chrome() {
        let a = app();
        let g = a.metrics.pixel_size(20, 6);
        assert_eq!(a.window_size(), Size::new(g.w + SCROLL_W, g.h + BAR_H));
    }

    #[test]
    fn the_scrollbar_reports_the_screen_within_the_history() {
        let mut a = app();
        let s = a.scroll();
        assert_eq!(s.visible, 6);
        assert_eq!(s.total, 6, "an empty terminal has nothing above it");
        assert!(!s.scrollable());

        for _ in 0..10 {
            a.feed(b"x\r\n");
        }
        let s = a.scroll();
        assert!(s.scrollable(), "ten lines scrolled off and the bar says nothing to scroll");
        assert_eq!(s.total, a.grid.scrollback().len() as u32 + 6);
    }

    #[test]
    fn a_clip_maps_to_the_rows_it_covers() {
        let a = app();
        let m = &a.metrics;
        let o = a.grid_origin();
        // The whole grid.
        let all = rows_in(
            Rect::new(0, o.y, 100, m.cell_h * 6),
            o,
            m,
            6,
        );
        assert_eq!(all, alloc::vec![0, 1, 2, 3, 4, 5]);

        // Exactly one row, and the *right* one: a clip on row 2's first pixel is row 2.
        let one = rows_in(Rect::new(0, o.y + (m.cell_h * 2) as i32, 100, m.cell_h), o, m, 6);
        assert_eq!(one, alloc::vec![2]);

        // A clip that stops one pixel short of the next row must not include it — the
        // off-by-one that repaints twice as much as it needs to.
        let short = rows_in(Rect::new(0, o.y, 100, m.cell_h), o, m, 6);
        assert_eq!(short, alloc::vec![0]);

        // A clip straddling a boundary covers both.
        let two = rows_in(Rect::new(0, o.y + m.cell_h as i32 - 1, 100, 2), o, m, 6);
        assert_eq!(two, alloc::vec![0, 1]);
    }

    #[test]
    fn a_clip_above_or_beyond_the_grid_maps_to_nothing_out_of_range() {
        let a = app();
        let m = &a.metrics;
        let o = a.grid_origin();
        // Entirely in the menu bar, above the grid.
        assert!(rows_in(Rect::new(0, 0, 100, BAR_H), o, m, 6).is_empty());
        // Past the bottom: clamped to the last row rather than naming rows that do not exist.
        let past = rows_in(Rect::new(0, o.y, 100, m.cell_h * 99), o, m, 6);
        assert_eq!(*past.last().unwrap(), 5);
    }
}
