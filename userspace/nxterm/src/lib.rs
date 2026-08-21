//! `nxterm` — the terminal, minus the syscalls.
//!
//! The split every subsystem in this tree uses: state, `view` and `update` are functions of
//! values and host-test in milliseconds; the window, the buffers and the event pump are the
//! binary's.
//!
//! ## Where the bytes come from
//!
//! **A tty, since Part C.** A key press is encoded by [`libterm::encode`] into an *outbox*; the
//! binary drains it to the tty server's backend channel, and what the server sends back —
//! echo, and the shell's output — arrives at [`App::feed`] and goes through [`libterm::parse`]
//! into the grid.
//!
//! Nothing here translates. A loopback stood in until Part C and turned `\r` into `\r\n`,
//! because `LF` is *index* and Enter alone would overwrite the line just typed. **That is
//! `ONLCR`, and it is the line discipline's** — which is now genuinely on the other side of the
//! channel, so the translation left with it. The one comment worth keeping from that period:
//! it lived in a single obvious place precisely so it could be deleted rather than found.

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
use librsproto::surface::{KEY_DOWN, KEY_REPEAT, KeyEvent, PointerEvent};
use libui::element::{Edge, Element, Insets, custom, dock, docked, offset, padding, stack};
use libui::widget::{Palette as UiPalette, ScrollState, WidgetState, button, menu_bar, scrollbar};

/// The `custom` node the grid is drawn into.
pub const GRID_KIND: u32 = 0x4772_6964;

/// The key on the menu bar's one item, so [`libui::layout::locate`] can find where it landed
/// and the popup can be placed under it.
pub const MENU_ITEM_KEY: u64 = 1;

/// The key on the grid, so the window can give it the keyboard on its first frame.
///
/// Focus has to start somewhere and the toolkit will not guess: `focus_next` would land on the
/// menu button, being first in tree order, and a terminal whose first keystroke opens a menu is
/// not a terminal. [`Tree::find_by_key`](libui::diff::Tree::find_by_key) turns this into the
/// widget id the router takes.
pub const GRID_KEY: u64 = 2;

/// The key on the menu bar, and [`SCROLLBAR_KEY`] on the scrollbar.
///
/// **Keyed because [`GRID_KEY`] is.** The diff pairs a parent's children either all by key or
/// all by position and refuses a mixture — deliberately, since a half-keyed list has two
/// answers for which child is which. Naming one of the window's three regions therefore means
/// naming all three, which they earn: they are fixed roles for the window's whole life.
pub const BAR_KEY: u64 = 3;
/// The key on the scrollbar — see [`BAR_KEY`].
pub const SCROLLBAR_KEY: u64 = 4;

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
    /// A key reached the grid. Raw, because whether it types is the terminal's decision and
    /// not the toolkit's: a release does not type, a **repeat does**, and `on_key` is a plain
    /// function pointer with nothing to decide it with.
    Key(KeyEvent),
    /// Something happened to the scrollbar. Carries the raw event because most of them are not
    /// a scroll: the router delivers motion whether or not a button is held, and a bar that
    /// moved on hover would be unusable.
    Scroll(PointerEvent),
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
    /// Bytes the user typed, waiting for the binary to send them to the tty.
    ///
    /// **An outbox rather than a call**, because this half has no syscalls: `update` is a
    /// function of values, and "send this to a channel" is not one. It is the same shape as
    /// the toolkit's messages — the application says what happened, the shell of a `main`
    /// performs it.
    outbox: Vec<u8>,
    /// Where the viewport is, or `None` to follow the output.
    ///
    /// **`None` is not "line zero".** Following the bottom and being anchored at whatever the
    /// bottom currently is are different states: the first stays at the bottom as output
    /// arrives, the second is exactly what the user asked for when they scrolled away from it.
    /// Collapsing them is how a terminal ends up scrolling itself back to where you were
    /// reading half a second ago.
    view_top: Option<u64>,
    /// The viewport moved, so all of it repaints.
    ///
    /// **Not [`Grid::damage_all`], which was the first attempt** and is wrong for a reason
    /// worth keeping: the grid's dirty flags name *screen* rows, and a view scrolled far
    /// enough back holds none of them. Scrolling to the top of the history therefore damaged
    /// every row the grid could name and not one row the user could see — the screen stayed on
    /// the old page under a thumb that had moved. The two damage spaces meet in
    /// [`damage_rows`](App::damage_rows) and nowhere else, so this is the half that lives here.
    view_moved: bool,
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
            outbox: Vec::new(),
            view_top: None,
            view_moved: false,
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

    /// Queue `bytes` for the program on the other end.
    ///
    /// **Typing snaps the view back to the bottom**, which is what every terminal does and what
    /// makes scrollback usable rather than a trap: the alternative is a prompt that answers
    /// somewhere you cannot see, and a user who concludes the terminal has hung.
    pub fn send(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.snap_to_bottom();
        self.outbox.extend_from_slice(bytes);
    }

    /// Take everything the user has typed since this was last called.
    pub fn take_outbox(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.outbox)
    }

    /// Follow the output again, repainting if that moved the view.
    pub fn snap_to_bottom(&mut self) {
        if self.view_top.is_some_and(|t| self.grid.clamp_view(t) != self.grid.top_line()) {
            self.view_moved = true;
        }
        self.view_top = None;
    }

    /// The absolute line number of the viewport's first row.
    ///
    /// Clamped on every read rather than corrected when the scrollback evicts: the grid is
    /// where lines go, and an anchor kept in step by a callback is an anchor that is wrong
    /// whenever somebody adds a second path into the grid.
    pub fn view_line(&self) -> u64 {
        match self.view_top {
            Some(t) => self.grid.clamp_view(t),
            None => self.grid.top_line(),
        }
    }

    /// The **viewport** rows this frame must repaint, taking the grid's damage as it goes.
    ///
    /// The grid reports *screen* rows, which are the same thing only while the view is
    /// following the bottom. Scrolled back by `k`, screen row `s` is on show at viewport row
    /// `s + k` — and at `s + k >= rows` it is below the viewport entirely and repaints nothing.
    ///
    /// A moved viewport repaints all of itself, and the grid's record is still taken in that
    /// case: [`Grid::take_damage`]'s contract is "I am about to draw these rows", which a full
    /// repaint satisfies, and leaving it unread would carry stale rows into the next frame.
    pub fn damage_rows(&mut self) -> Vec<usize> {
        let back = (self.grid.top_line() - self.view_line()) as usize;
        let rows = self.grid.rows();
        let moved = core::mem::take(&mut self.view_moved);
        let dirty = self.grid.take_damage();
        if moved {
            return (0..rows).collect();
        }
        dirty.into_iter().filter_map(|s| s.checked_add(back).filter(|v| *v < rows)).collect()
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
                self.view_top = None;
            }
            Msg::Scroll(p) => self.scroll_to(p),
            Msg::Key(k) => self.key(k),
        }
    }

    /// Type what a key produced, if it is a key that types.
    ///
    /// **A repeat types and a release does not.** A terminal sends on the way down, and a
    /// repeat is that press continuing — which is the whole of what the compositor's key-repeat
    /// generator is *for*, and the point at which it reaches something that acts on it rather
    /// than a test client that prints it.
    fn key(&mut self, k: KeyEvent) {
        if k.pressed != KEY_DOWN && k.pressed != KEY_REPEAT {
            return;
        }
        let mut out = [0u8; libterm::encode::MAX_ENCODED];
        let n = libterm::encode::encode(k.keycode, k.modifiers, &mut out);
        self.send(&out[..n]);
    }

    /// Move the view to where a scrollbar interaction points.
    ///
    /// Only while a button is held: the router delivers motion and crossings unconditionally,
    /// and a bar that tracked the cursor on hover would scroll whenever the pointer passed over
    /// it on its way somewhere else.
    fn scroll_to(&mut self, p: PointerEvent) {
        if p.buttons == 0 {
            return;
        }
        let s = self.scroll();
        self.scroll_to_line(self.grid.oldest_line() + s.offset_at(self.track_h(), p.y) as u64);
    }

    /// Anchor the viewport at absolute line `top`, clamped, repainting if that moved it.
    ///
    /// The one place the view changes, so that the mouse wheel and a `Shift-PageUp` are a
    /// coordinate conversion away from working rather than a second copy of this.
    pub fn scroll_to_line(&mut self, top: u64) {
        let want = self.grid.clamp_view(top);
        if want != self.view_line() {
            self.view_moved = true;
        }
        // **Recorded even at the bottom**, rather than becoming `None` again: dragging the thumb
        // to the end means "show me the last screen", and a program printing afterwards must not
        // pull the view along with it. Following resumes when the user types.
        self.view_top = Some(want);
    }

    /// The scrollbar's state: the viewport is a window onto the scrollback plus the screen.
    pub fn scroll(&self) -> ScrollState {
        let rows = self.grid.rows() as u32;
        let oldest = self.grid.oldest_line();
        let back = (self.grid.top_line() - oldest) as u32;
        ScrollState {
            offset: (self.view_line() - oldest) as u32,
            visible: rows,
            total: back + rows,
        }
    }

    /// The height the scrollbar's track is laid out at.
    ///
    /// The bar sizes itself to whatever the `Dock` leaves it, and `scrollbar` is told a height
    /// separately for the thumb arithmetic — so this is the one number both the view and the
    /// drag have to agree on, and it exists so they cannot disagree.
    pub fn track_h(&self) -> u32 {
        self.metrics.pixel_size(self.grid.cols(), self.grid.rows()).h
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
                docked(Edge::Top, bar.key(BAR_KEY)),
                docked(
                    Edge::Right,
                    scrollbar(self.scroll(), SCROLL_W, self.track_h(), &ui)
                        .on_pointer(Msg::Scroll)
                        .key(SCROLLBAR_KEY),
                ),
            ],
            custom(GRID_KIND, grid_px).key(GRID_KEY).on_key(|k| Some(Msg::Key(k))),
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
    use librsproto::surface::KEY_UP;
    use libui::layout::{FixedCell, layout, locate};

    const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSansMono.ttf");

    fn app() -> App {
        let f = Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses");
        App::new(20, 6, Metrics::new(&f, 16.0))
    }

    /// The characters **on show** at viewport `row`, trailing blanks trimmed.
    ///
    /// Through the view rather than the screen, so that a test about a scrolled-back terminal
    /// asks what the user can see. The two are the same thing while the view follows the
    /// bottom, which is every test written before scrollback existed.
    fn line(a: &App, row: usize) -> alloc::string::String {
        let s: alloc::string::String = (0..a.grid.cols())
            .map(|c| a.grid.view_cell(a.view_line(), row, c).map_or(' ', |x| x.ch))
            .collect();
        s.trim_end().into()
    }

    /// Type `code` and return what the terminal would send to the tty.
    fn typed(a: &mut App, code: u16) -> alloc::vec::Vec<u8> {
        a.update(Msg::Key(key_ev(code, KEY_DOWN)));
        a.take_outbox()
    }

    #[test]
    fn a_key_press_becomes_bytes_in_the_outbox_and_nothing_on_the_grid() {
        // **What Part C changed.** A keystroke goes *out*; what appears on screen is whatever
        // comes back. Before Part C this looped internally, which made the two halves
        // indistinguishable — and a terminal that echoed locally would double every character
        // the moment a real shell echoed too.
        let mut a = app();
        assert_eq!(typed(&mut a, 35), b"h", "h");
        assert_eq!(line(&a, 0), "", "the keystroke drew itself without being echoed");

        // ...and the bytes coming back are what lands.
        a.feed(b"h");
        assert_eq!(line(&a, 0), "h");
    }

    #[test]
    fn the_terminal_translates_nothing_on_the_way_out() {
        // Enter sends `\r` and only `\r`. The `\r\n` a user sees is the line discipline's
        // `ONLCR`, which is on the other side of the channel since Part C — a terminal that
        // translated here would send two bytes where a program expects one, and `read` of a
        // single keystroke would return a spurious newline.
        let mut a = app();
        assert_eq!(typed(&mut a, libkern::abi::KEY_ENTER), b"\r");
    }

    #[test]
    fn a_key_with_no_encoding_sends_nothing() {
        let mut a = app();
        assert!(typed(&mut a, libkern::abi::KEY_LEFTSHIFT).is_empty());
        assert_eq!(line(&a, 0), "");
    }

    #[test]
    fn output_from_the_program_is_what_reaches_the_grid() {
        // The other direction, end to end through the parser: this is what `Tty::Output`
        // delivers.
        let mut a = app();
        a.feed(b"hi\r\n\x1b[1mbold\x1b[m");
        assert_eq!(line(&a, 0), "hi");
        assert_eq!(line(&a, 1), "bold");
        assert!(a.grid.cell(1, 0).unwrap().attrs.flags.contains(libterm::cell::Flags::BOLD));
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

    /// A press-and-drag on the scrollbar to `y`, in widget-local coordinates.
    fn grab(a: &mut App, y: i32) {
        a.update(Msg::Scroll(PointerEvent {
            kind: librsproto::surface::POINTER_BUTTON,
            button: 0x110,
            buttons: 1,
            flags: librsproto::surface::POINTER_PRESSED,
            y,
            ..Default::default()
        }));
    }

    /// Fill the scrollback with numbered lines.
    fn produce(a: &mut App, n: usize) {
        for i in 0..n {
            a.feed(alloc::format!("{i}\r\n").as_bytes());
        }
    }

    #[test]
    fn dragging_the_scrollbar_moves_the_view_into_the_scrollback() {
        let mut a = app();
        produce(&mut a, 40);
        assert_eq!(a.view_line(), a.grid.top_line(), "starts at the bottom");

        grab(&mut a, 0);
        assert_eq!(a.view_line(), a.grid.oldest_line(), "the top of the track is the oldest");
        assert_eq!(line(&a, 0), "0", "and the oldest line is on show");

        let bottom = a.track_h() as i32;
        grab(&mut a, bottom);
        assert_eq!(a.view_line(), a.grid.top_line(), "the bottom is the live screen again");
    }

    #[test]
    fn a_pointer_over_the_scrollbar_with_no_button_held_does_not_scroll() {
        // The router delivers motion and crossings unconditionally. A bar that tracked the
        // cursor would scroll whenever the pointer crossed it on its way somewhere else.
        let mut a = app();
        produce(&mut a, 40);
        grab(&mut a, 0);
        let parked = a.view_line();

        a.update(Msg::Scroll(PointerEvent {
            kind: librsproto::surface::POINTER_MOTION,
            buttons: 0,
            y: a.track_h() as i32,
            ..Default::default()
        }));
        assert_eq!(a.view_line(), parked, "a hover moved the view");
    }

    #[test]
    fn typing_snaps_the_view_back_to_the_bottom() {
        // What makes scrollback usable rather than a trap: the alternative is a prompt that
        // answers somewhere off screen, and a user who concludes the terminal has hung.
        let mut a = app();
        produce(&mut a, 40);
        grab(&mut a, 0);
        assert_ne!(a.view_line(), a.grid.top_line(), "the premise: scrolled away");

        typed(&mut a, 35); // h
        assert_eq!(a.view_line(), a.grid.top_line(), "typing did not come back to the bottom");

        // ...and a key that encodes to nothing does not, because it is not typing.
        grab(&mut a, 0);
        let scrolled = a.view_line();
        typed(&mut a, libkern::abi::KEY_LEFTSHIFT);
        assert_eq!(a.view_line(), scrolled, "pressing Shift snapped the view");
    }

    #[test]
    fn output_arriving_does_not_drag_a_scrolled_back_view_with_it() {
        // **The reason the anchor is a line number.** With an offset counted from the bottom,
        // every line a program prints moves the reader's page up by one.
        let mut a = app();
        produce(&mut a, 40);
        grab(&mut a, 0);
        let showing: alloc::vec::Vec<alloc::string::String> =
            (0..a.grid.rows()).map(|r| line(&a, r)).collect();

        a.feed(b"more\r\nand more\r\n");
        let after: alloc::vec::Vec<alloc::string::String> =
            (0..a.grid.rows()).map(|r| line(&a, r)).collect();
        assert_eq!(showing, after, "the view moved under the reader");
    }

    #[test]
    fn the_scrollbar_reports_where_the_view_is_not_only_how_much_there_is() {
        // A bar whose offset was always the bottom would draw its thumb at the end while the
        // user reads the top — the state and the picture disagreeing.
        let mut a = app();
        produce(&mut a, 40);
        let s = a.scroll();
        assert_eq!(s.offset, s.total - s.visible, "following: the thumb is at the end");

        grab(&mut a, 0);
        assert_eq!(a.scroll().offset, 0, "scrolled to the top: so is the thumb");
    }

    #[test]
    fn damage_is_reported_in_viewport_rows_and_drops_what_is_below_the_view() {
        // The grid names *screen* rows and the viewport may be somewhere else entirely. Getting
        // this wrong repaints the wrong row — or, scrolled far back, repaints rows for changes
        // that are not on screen at all.
        let mut a = app();
        produce(&mut a, 40);
        let _ = a.damage_rows();

        // Scrolled back by exactly one line: screen row 0 shows at viewport row 1.
        let back_one = a.grid.top_line() - 1;
        a.scroll_to_line(back_one);
        let _ = a.damage_rows(); // the scroll itself damaged everything
        a.feed(b"\x1b[1;1Hz"); // touch screen row 0
        let d = a.damage_rows();
        assert!(d.contains(&1), "screen row 0 should show at viewport row 1, got {d:?}");
        assert!(!d.contains(&0), "and not at viewport row 0: {d:?}");

        // Scrolled far back: the change is below the viewport and repaints nothing.
        grab(&mut a, 0);
        let _ = a.damage_rows();
        a.feed(b"\x1b[1;1Hy");
        assert!(a.damage_rows().is_empty(), "damage for a row that is not on screen");
    }

    #[test]
    fn moving_the_view_repaints_all_of_it() {
        // Every row shows different text afterwards, so a scroll that damaged nothing would
        // leave the old page on screen under a thumb that had moved.
        let mut a = app();
        produce(&mut a, 40);
        let _ = a.damage_rows();
        assert!(a.damage_rows().is_empty(), "the premise: nothing outstanding");

        grab(&mut a, 0);
        assert_eq!(a.damage_rows().len(), a.grid.rows(), "a scroll must repaint the viewport");

        // ...and snapping back does too.
        let _ = a.damage_rows();
        a.snap_to_bottom();
        assert_eq!(a.damage_rows().len(), a.grid.rows());

        // A scroll that lands where the view already is repaints nothing.
        let _ = a.damage_rows();
        a.snap_to_bottom();
        assert!(a.damage_rows().is_empty(), "an idempotent snap still repainted");
    }

    /// A key event as the compositor sends it.
    /// A key record for these tests. The window id is arbitrary: by the time a record reaches
    /// the application, `libsurface` has already established it belongs to this window.
    fn key_ev(code: u16, pressed: u16) -> KeyEvent {
        KeyEvent::new(1, code, pressed, 0)
    }

    /// The tree, layout and router of a live window, with the grid focused as `main` does it.
    fn window(a: &App) -> (libui::diff::Tree, libui::layout::Layout, libui::route::Router) {
        let bounds = Rect::new(0, 0, a.window_size().w, a.window_size().h);
        let e = a.view();
        let l = layout(&e, bounds, &FixedCell { w: 8, h: 16 });
        let mut t = libui::diff::Tree::new();
        t.update(&e, &l).expect("a clean frame");
        let mut r = libui::route::Router::new();
        let id = t.find_by_key(GRID_KEY).expect("the grid is keyed");
        assert!(r.focus(&t, &e, id), "the grid must be able to take focus");
        (t, l, r)
    }

    #[test]
    fn a_key_repeat_reaches_the_grid_through_the_router_and_types() {
        // **B4.** Repeat is generated compositor-side and already had a consumer that *prints*
        // it; what is new is a repeat reaching a widget through `libui::route` and being
        // encoded exactly like the press it continues. Held-down keys are how anyone deletes a
        // line, so this is the difference between a demo and a terminal.
        let mut a = app();
        let (t, _l, r) = window(&a);
        let e = a.view();

        for msg in [
            r.key(&t, &e, key_ev(35, KEY_DOWN)),   // h, pressed
            r.key(&t, &e, key_ev(35, KEY_REPEAT)), // ...and held
            r.key(&t, &e, key_ev(35, KEY_REPEAT)),
        ] {
            a.update(msg.expect("the focused grid claims the key"));
        }
        // Observed at the outbox, not on the grid: since Part C what a keystroke produces is
        // bytes on their way to the shell, and what reaches the grid is whatever comes back.
        assert_eq!(a.take_outbox(), b"hhh", "a repeat did not type");
    }

    #[test]
    fn a_key_release_reaching_the_grid_types_nothing() {
        // The other half, and the one that goes wrong silently: a terminal sends on the way
        // down, so acting on the release too doubles every character typed.
        let mut a = app();
        let (t, _l, r) = window(&a);
        let e = a.view();
        a.update(r.key(&t, &e, key_ev(35, KEY_DOWN)).unwrap());
        a.update(r.key(&t, &e, key_ev(35, KEY_UP)).unwrap());
        assert_eq!(a.take_outbox(), b"h", "the release typed as well");
    }

    #[test]
    fn the_menu_bars_accelerator_path_still_works_while_the_grid_holds_the_keyboard() {
        // The grid claims every key it is given, so a key can only reach the chrome by the
        // chrome being focused. Asserted because the alternative — a grid that declines some
        // keys — is a decision, and this records that it was not taken: the menu is reached
        // with the pointer, and `route`'s bubbling is there for a keyboard menu that M6 owns.
        let a = app();
        let (t, _l, r) = window(&a);
        let e = a.view();
        assert!(matches!(r.key(&t, &e, key_ev(35, KEY_DOWN)), Some(Msg::Key(_))));
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
