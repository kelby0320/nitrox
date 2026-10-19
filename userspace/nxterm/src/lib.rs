//! `nxterm` — the terminal, minus the syscalls.
//!
//! State, `view` and `update` are host-tested; the window, buffers and event pump are the binary's.
//! A key press is encoded by [`libterm::encode`] into an outbox the binary drains to the tty
//! server's backend, and the server's reply arrives at [`App::feed`]. Nothing here translates:
//! `ONLCR` is the line discipline's.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use libdraw::geom::{Rect, Size};
use libkern::abi::BTN_LEFT;
use librsproto::surface::{
    KEY_DOWN, KEY_REPEAT, KeyEvent, POINTER_BUTTON, POINTER_MOTION, POINTER_PRESSED, PointerEvent, RESIZE_BOTTOM,
    RESIZE_RIGHT, WINDOW_STATE_MAXIMIZED, WINDOW_STATE_MINIMIZED, WINDOW_STATE_NORMAL,
};
use libterm::cell::Palette;
use libterm::grid::Grid;
use libterm::parse::{MAX_PER_BYTE, Op, Parser};
use libterm::render::Metrics;
use libui::element::{Edge, Element, Insets, custom, dock, docked, padding, sized};
use libui::menu::{Accel, Item, Menu, MenuState};
use libui::widget::{
    GRIP_W, TITLE_BAR_H, TitleButtons, WINDOW_CONTENT_X, WINDOW_CONTENT_Y, resize_grip, title_bar,
    window_frame_with_grip,
};
use libui::widget::{ScrollState, TAB_STRIP_H, TextFieldState, Theme as UiTheme, WidgetState, scrollbar, text_field};

/// The `custom` node the grid is drawn into.
pub const GRID_KIND: u32 = 0x4772_6964;

/// Where the menu bar's words are keyed from: `MENU_BAR_KEY + i` for menu `i`.
pub const MENU_BAR_KEY: u64 = 10;

/// Where the open popup's rows are keyed from: `MENU_ROW_KEY + i` for item `i`.
pub const MENU_ROW_KEY: u64 = 100;

/// The menu the harness opens with F1, and the one whose rows `check-terminal` clicks.
pub const HARNESS_MENU: usize = 1;

/// The key on the grid, so the window can give it the keyboard on its first frame.
pub const GRID_KEY: u64 = 2;

/// The key on the menu bar, and [`SCROLLBAR_KEY`] on the scrollbar.
pub const BAR_KEY: u64 = 3;
/// The key on the scrollbar — see [`BAR_KEY`].
pub const SCROLLBAR_KEY: u64 = 4;
/// The key on the title bar — see [`BAR_KEY`].
pub const TITLE_KEY: u64 = 5;

/// The resize grip's key, found by tests with [`locate`](libui::layout::locate).
pub const GRIP_KEY: u64 = 6;

/// Height of the menu bar, in pixels.
pub const BAR_H: u32 = 24;

/// The window's title, and what the bar and the shell's window list both show.
pub const TITLE: &str = "nxterm";

/// Width of the scrollbar, in pixels.
pub const SCROLL_W: u32 = 12;

/// What the chrome costs the grid horizontally: the scrollbar, the frame, and the pane's margins.
const CHROME_W: u32 = SCROLL_W + libui::widget::WINDOW_FRAME_W + 2 * GRID_PAD_X;

const CHROME_H: u32 = BARS_H + 2 * GRID_PAD_Y;

/// What stands above and below the pane (title bar, menu bar, tab strip, frame), the box the
/// scrollbar sits beside.
const BARS_H: u32 = BAR_H + TAB_STRIP_H + TITLE_BAR_H + libui::widget::WINDOW_FRAME_H;

/// The margin between the grid's cells and the edge of their pane, across.
const GRID_PAD_X: u32 = 11;

const GRID_PAD_Y: u32 = 9;

/// The element key on the tab strip.
pub const TAB_STRIP_KEY: u64 = 7;

/// The element key on the pane the grid sits in — its ground and its margin.
pub const GRID_PANE_KEY: u64 = 8;

/// The key that selects everything, with Ctrl and Shift held: `a`. Shift keeps `Ctrl+A`
/// (start-of-line) the program's.
pub const SELECT_ALL_KEYCODE: u16 = 30;
/// The key that opens Find, with **Ctrl and Shift**: `f`.
pub const FIND_KEYCODE: u16 = 33;
/// The key that clears the scrollback, with Ctrl and Shift: `k`; `Ctrl+L` clears the screen and is
/// the shell's.
pub const CLEAR_KEYCODE: u16 = 37;

/// The key that copies the selection, with Ctrl and Shift held: `c`. Shift because `Ctrl+C` is the
/// interrupt; pinned against `libinput`'s table by a test.
pub const COPY_KEYCODE: u16 = 46;
/// The key that opens a window: `n`, with Ctrl and Shift.
pub const NEW_WINDOW_KEYCODE: u16 = 49;
/// The key that quits every window: `q`, with Ctrl and Shift (`Ctrl+Q` is flow control).
pub const QUIT_KEYCODE: u16 = 16;

/// The key that opens a tab: `t`, with Ctrl and Shift (`Ctrl+T` is `transpose-chars` in a readline
/// shell).
pub const NEW_TAB_KEYCODE: u16 = 20;
/// The key that closes a tab: `w`, with Ctrl and Shift; closing the last closes the window.
pub const CLOSE_TAB_KEYCODE: u16 = 17;

/// The key that pastes: `v`, with Ctrl and Shift — see [`COPY_KEYCODE`].
pub const PASTE_KEYCODE: u16 = 47;

/// What the chrome can ask the terminal to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Msg {
    /// A word on the menu bar was clicked: open that menu, or close it if it was the open one.
    MenuBar(usize),
    /// Open another window of this terminal — `Ctrl+Shift+N`, or File ▸ New Window.
    NewWindow,
    /// Close every window of this terminal — `Ctrl+Shift+Q`, or File ▸ Quit.
    Quit,
    /// Open a tab — `Ctrl+Shift+T`, or File ▸ New Tab.
    NewTab,
    /// Close the tab with this key — `Ctrl+Shift+W`, or File ▸ Close Tab.
    CloseTab(u64),
    /// Make the tab with this key current — a click on the strip.
    SelectTab(u64),
    /// Select every line the grid holds — `Ctrl+Shift+A`, or Edit ▸ Select All.
    SelectAll,
    /// Open the find field — `Ctrl+Shift+F`, or Edit ▸ Find.
    Find,
    /// Drop the scrollback, keeping the screen — `Ctrl+Shift+K`, or Edit ▸ Clear Scrollback.
    ClearScrollback,
    /// Copy the selection to the clipboard — `Ctrl+Shift+C`, or Edit ▸ Copy.
    Copy,
    /// Fetch the newest clipboard entry and type it at the shell — `Ctrl+Shift+V`, or Edit ▸ Paste.
    Paste,
    /// Erase the screen and home the cursor — `Ctrl-L`'s effect, from the menu.
    Clear,
    /// Erase the screen **and** the scrollback, and reset attributes.
    Reset,
    /// A key reached the grid, raw: whether it types is the terminal's decision (a release does
    /// not, a repeat does).
    Key(KeyEvent),
    /// Something happened to the scrollbar, raw: most events are not a scroll, as motion arrives
    /// with no button held.
    Scroll(PointerEvent),
    /// The wheel turned over the terminal — see [`App::wheel`].
    Wheel(PointerEvent),
    /// The title bar was pressed somewhere that moves the window.
    DragWindow,
    /// The window should close — its own close button, or the shell asking.
    Close,
    /// The resize grip was pressed: the compositor is owed a `StartResize`.
    ResizeWindow(u32),
    /// A title-bar button asking the manager for a window state.
    RequestState(u32),
    /// The manager put this window in this state: the maximise button follows it, not its click.
    WindowState(u32),
    /// The pointer did something over the grid — a selection gesture, or motion that is not one.
    GridPointer(PointerEvent),
}

/// What the terminal wants done with the clipboard, which only `main` can do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ClipRequest {
    /// Push this text onto the ring.
    Copy(alloc::string::String),
    /// Fetch the newest entry and type it at the shell.
    ///
    /// No cycle: a pasted byte has already gone to a program that may not be a line editor, so it
    /// cannot be replaced; cycling lives in the editor.
    Paste,
}

/// What a [`resize`](App::resize) did, beyond changing the size.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Resized {
    /// Logical lines the ring dropped during the rewrap.
    pub evicted: usize,
}

/// The most tabs one window can hold, derived from `MAX_WAIT_HANDLES`.
///
/// `main` waits on the compositor plus one handle per tab in one `sys_wait`, and the kernel rejects
/// a larger count.
pub const MAX_TABS: usize = libkern::abi::MAX_WAIT_HANDLES - 1;

/// Where tab keys start, far from the element keys so a tab and a widget cannot be confused.
pub const TAB_KEY_BASE: u64 = 1 << 62;

/// One tab: a terminal, and everything that is *its* rather than the window's.
///
/// The shell is absent: `main` keeps a backend per tab and reconciles the two by [`key`].
///
/// [`key`]: Self::key
pub struct Term {
    /// Identity across frames; not the index, as closing a tab renumbers the rest.
    key: u64,
    /// The screen, the cursor, and the scrollback.
    pub grid: Grid,
    /// Bytes to grid operations; held across writes because a sequence can be split between them.
    parser: Parser,
    view_top: Option<u64>,
    view_moved: bool,
    outbox: Vec<u8>,
    directory_changed: bool,
}

impl Term {
    fn new(key: u64, cols: usize, rows: usize) -> Self {
        Self {
            key,
            grid: Grid::new(cols, rows),
            parser: Parser::new(),
            view_top: None,
            view_moved: false,
            outbox: Vec::new(),
            directory_changed: false,
        }
    }

    /// This tab's identity, which is what `main` matches a backend against.
    pub fn key(&self) -> u64 {
        self.key
    }

    /// Bytes typed into this tab, taken exactly once.
    pub fn take_outbox(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.outbox)
    }

    /// Where the shell in this tab last said it is, or `None` if it has never said.
    pub fn directory(&self) -> Option<&str> {
        self.parser.directory()
    }

    /// Whether the directory changed since this was last called, clearing it: a log line per `cd`,
    /// not per prompt.
    pub fn take_directory_change(&mut self) -> bool {
        core::mem::take(&mut self.directory_changed)
    }
}

/// Everything the terminal is.
pub struct App {
    /// Counts runs of presses, so a second click means a word and a third a line.
    clicks: libui::click::Clicks,
    scroll_grab: libui::widget::ScrollGrab,
    click_run: u32,
    find: Option<TextFieldState>,
    /// The open tabs, in strip order. Never empty: closing the last closes the window.
    tabs: Vec<Term>,
    current: u64,
    /// The next key to hand out; monotonic, so a stale message never names a replacement tab.
    next_key: u64,
    new_window: bool,
    quit: bool,
    /// Which menu is open, where each bar word sits, and where the keyboard is inside it.
    pub menus: MenuState,
    /// Whether this window holds the keyboard, which the title bar shows; starts `true` like
    /// `libui::Router`'s `window_focused`.
    pub focused: bool,
    /// Cell metrics, so `view` can size the grid node.
    pub metrics: Metrics,
    /// Colours for the cells.
    pub palette: Palette,
    closing: bool,
    state_requested: Option<u32>,
    resize_requested: Option<u32>,
    move_requested: bool,
    /// This window last asked to be maximised, so its button now asks for normal.
    maximized: bool,
    /// The window's size in pixels: what the client commits, not what the grid needs.
    window: Size,
    clip_request: Option<ClipRequest>,
}

impl App {
    /// A terminal of `cols` × `rows` cells, drawn with `metrics`.
    pub fn new(cols: usize, rows: usize, metrics: Metrics) -> App {
        let g = metrics.pixel_size(cols, rows);
        App {
            clicks: libui::click::Clicks::new(),
            scroll_grab: libui::widget::ScrollGrab::new(),
            click_run: 1,
            find: None,
            window: Size::new(g.w + CHROME_W, g.h + CHROME_H),
            maximized: false,
            tabs: alloc::vec![Term::new(TAB_KEY_BASE, cols, rows)],
            current: TAB_KEY_BASE,
            next_key: TAB_KEY_BASE + 1,
            new_window: false,
            quit: false,
            // Zero: the length is set every frame by `set_anchors`.
            menus: MenuState::new(0),
            focused: true,
            metrics,
            // `libterm`'s ANSI palette, deliberately not the desktop theme: programs expect it.
            palette: Palette::default(),
            move_requested: false,
            resize_requested: None,
            state_requested: None,
            closing: false,
            clip_request: None,
        }
    }

    /// The size of this terminal's window in pixels.
    pub fn window_size(&self) -> Size {
        self.window
    }

    /// Take the window to `size`, refitting the grid and rewrapping the history (the answer to
    /// `Configure`).
    ///
    /// At least one cell each way, so a degenerate size leaves a valid cursor. `None` if the size
    /// is unchanged; `Some` carries the eviction.
    pub fn resize(&mut self, size: Size) -> Option<Resized> {
        if size == self.window {
            return None;
        }
        self.window = size;
        let cols = (size.w.saturating_sub(CHROME_W) / self.metrics.cell_w).max(1) as usize;
        let rows = (size.h.saturating_sub(CHROME_H) / self.metrics.cell_h).max(1) as usize;
        // Every tab, not the one on screen: a background tab left at the old shape draws from stale
        // numbers.
        let current = self.current;
        let mut evicted = 0;
        for t in &mut self.tabs {
            let reflow = t.grid.resize(cols, rows);
            // Through the map: `view_top` is an absolute line number.
            t.view_top = t.view_top.map(|v| t.grid.clamp_view(reflow.map_line(v)));
            t.view_moved = true;
            if t.key == current {
                evicted = reflow.evicted_lines();
            }
        }
        Some(Resized { evicted })
    }

    /// Where the grid's top-left sits inside the window.
    pub fn grid_origin(&self) -> libdraw::geom::Point {
        libdraw::geom::Point::new(
            (WINDOW_CONTENT_X + GRID_PAD_X) as i32,
            // `TAB_STRIP_H` and the margin: this maps a pointer to a cell.
            (WINDOW_CONTENT_Y + TITLE_BAR_H + BAR_H + TAB_STRIP_H + GRID_PAD_Y) as i32,
        )
    }

    /// Feed bytes from the program on the other end.
    pub fn feed(&mut self, bytes: &[u8]) {
        let key = self.current;
        self.feed_tab(key, bytes);
    }

    /// Queue `bytes` for the program on the other end; typing snaps the view back to the bottom.
    pub fn send(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.snap_to_bottom();
        self.tab_mut().outbox.extend_from_slice(bytes);
    }

    /// Whether a `StartMove` is owed, clearing it.
    pub fn take_move_request(&mut self) -> bool {
        core::mem::take(&mut self.move_requested)
    }

    /// Whether the terminal has been asked to close.
    pub fn closing(&self) -> bool {
        self.closing
    }

    /// The window state a button asked for, clearing it.
    pub fn take_state_request(&mut self) -> Option<u32> {
        self.state_requested.take()
    }

    /// The edges a `StartResize` is owed for, if the grip was pressed. Clears the record.
    pub fn take_resize_request(&mut self) -> Option<u32> {
        self.resize_requested.take()
    }

    /// Take everything the user has typed since this was last called.
    pub fn take_outbox(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.tab_mut().outbox)
    }

    /// Follow the output again, repainting if that moved the view.
    pub fn snap_to_bottom(&mut self) {
        if self.tab_mut().view_top.is_some_and(|t| self.tab_mut().grid.clamp_view(t) != self.tab_mut().grid.top_line())
        {
            self.tab_mut().view_moved = true;
        }
        self.tab_mut().view_top = None;
    }

    /// The absolute line number of the viewport's first row.
    /// Clamped on every read rather than corrected on eviction.
    pub fn view_line(&self) -> u64 {
        match self.tab().view_top {
            Some(t) => self.tab().grid.clamp_view(t),
            None => self.tab().grid.top_line(),
        }
    }

    /// The viewport rows this frame must repaint, taking the grid's damage as it goes.
    /// The grid reports screen rows; scrolled back by `k`, screen row `s` shows at viewport row `s
    /// + k`.
    pub fn damage_rows(&mut self) -> Vec<usize> {
        let back = (self.tab_mut().grid.top_line() - self.view_line()) as usize;
        let rows = self.tab_mut().grid.rows();
        let moved = core::mem::take(&mut self.tab_mut().view_moved);
        let dirty = self.tab_mut().grid.take_damage();
        if moved {
            return (0..rows).collect();
        }
        dirty.into_iter().filter_map(|s| s.checked_add(back).filter(|v| *v < rows)).collect()
    }

    /// The current tab. Never `None`: `tabs` is never empty.
    fn tab(&self) -> &Term {
        self.tabs.iter().find(|t| t.key == self.current).unwrap_or(&self.tabs[0])
    }

    fn tab_mut(&mut self) -> &mut Term {
        let cur = self.current;
        match self.tabs.iter().position(|t| t.key == cur) {
            Some(i) => &mut self.tabs[i],
            None => &mut self.tabs[0],
        }
    }

    /// The current tab's grid — the screen, the cursor and the scrollback.
    pub fn grid(&self) -> &Grid {
        &self.tab().grid
    }

    /// Every tab, for the binary that owns the ttys behind them.
    pub fn tabs(&self) -> &[Term] {
        &self.tabs
    }

    /// Every tab, mutably — for draining outboxes. See [`tabs`](Self::tabs) for reading.
    pub fn tabs_mut(&mut self) -> &mut [Term] {
        &mut self.tabs
    }

    /// What the strip draws: a label and a key per tab, current first-class.
    pub fn tab_labels(&self) -> Vec<(u64, alloc::string::String)> {
        self.tabs.iter().enumerate().map(|(i, t)| (t.key, alloc::format!("Terminal {}", i + 1))).collect()
    }

    /// Which tab is current.
    pub fn current_tab(&self) -> u64 {
        self.current
    }

    /// The current tab's grid, mutably.
    pub fn grid_mut(&mut self) -> &mut Grid {
        &mut self.tab_mut().grid
    }

    /// Repaint the current tab's grid entirely — what a resize means.
    pub fn damage_all(&mut self) {
        self.tab_mut().grid.damage_all();
    }

    /// Feed bytes from tab `key`'s tty into that tab's parser.
    pub fn feed_tab(&mut self, key: u64, bytes: &[u8]) {
        let Some(i) = self.tabs.iter().position(|t| t.key == key) else { return };
        let t = &mut self.tabs[i];
        let before = t.parser.directory().map(alloc::string::String::from);
        let mut out = [Op::Print('\0'); MAX_PER_BYTE];
        for &b in bytes {
            let n = t.parser.feed(b, &mut out);
            t.grid.apply_all(&out[..n]);
        }
        if t.parser.directory() != before.as_deref() {
            t.directory_changed = true;
        }
    }

    /// Open a tab and make it current, returning its key.
    pub fn open_tab(&mut self) -> u64 {
        if self.tabs.len() >= MAX_TABS {
            // Said rather than silently refused.
            self.feed(b"\r\nnxterm: no more tabs in this window\r\n");
            return self.current;
        }
        let key = self.next_key;
        self.next_key += 1;
        let (cols, rows) = (self.tab().grid.cols(), self.tab().grid.rows());
        self.tabs.push(Term::new(key, cols, rows));
        self.show_tab(key);
        key
    }

    /// Make tab `key` current and repaint the grid, which no diff will do: `diff` fingerprints the
    /// `custom` node by kind and size.
    fn show_tab(&mut self, key: u64) {
        self.current = key;
        self.tab_mut().view_moved = true;
    }

    /// Close tab `key`; closing the last tab closes the window. Returns whether anything was
    /// closed.
    pub fn close_tab(&mut self, key: u64) -> bool {
        let Some(i) = self.tabs.iter().position(|t| t.key == key) else { return false };
        if self.tabs.len() == 1 {
            self.closing = true;
            return true;
        }
        self.tabs.remove(i);
        if self.current == key {
            // Through `show_tab`: a closed tab leaves pixels behind.
            let next = self.tabs[i.min(self.tabs.len() - 1)].key;
            self.show_tab(next);
        }
        true
    }

    /// Apply a message from the chrome.
    pub fn update(&mut self, msg: Msg) {
        // Choosing dismisses the menu, whichever row; a message that is not a row leaves it alone.
        if self
            .menu_table()
            .iter()
            .flat_map(|m| m.items.iter())
            .any(|it| matches!(it, Item::Action { msg: m, .. } if *m == msg))
        {
            self.menus.close();
        }
        match msg {
            Msg::MenuBar(i) => self.menus.toggle(i),
            Msg::NewWindow => self.new_window = true,
            Msg::Quit => self.quit = true,
            Msg::NewTab => {
                self.open_tab();
            }
            Msg::CloseTab(key) => {
                self.close_tab(key);
            }
            Msg::SelectTab(key) => {
                if self.tabs.iter().any(|t| t.key == key) {
                    self.show_tab(key);
                }
            }
            // Nothing selected is not a request: an empty copy would move the ring's serial under
            // clients mid-cycle.
            Msg::Copy => {
                if let Some(text) = self.tab_mut().grid.selected_text() {
                    self.clip_request = Some(ClipRequest::Copy(text));
                }
            }
            Msg::SelectAll => {
                if self.tab_mut().grid.select_all() {
                    self.tab_mut().view_moved = true;
                }
            }
            Msg::Find => {
                if self.find.is_none() {
                    self.find = Some(TextFieldState::new());
                }
            }
            Msg::ClearScrollback => {
                // Snapped before the history goes: afterwards there is nothing above the screen to
                // clamp to, so it repaints nothing.
                self.snap_to_bottom();
                self.tab_mut().grid.clear_scrollback();
            }
            Msg::Paste => self.clip_request = Some(ClipRequest::Paste),
            Msg::DragWindow => self.move_requested = true,
            Msg::ResizeWindow(edges) => self.resize_requested = Some(edges),
            Msg::RequestState(s) => self.state_requested = Some(s),
            Msg::WindowState(s) => {
                if s == WINDOW_STATE_MAXIMIZED || s == WINDOW_STATE_NORMAL {
                    self.maximized = s == WINDOW_STATE_MAXIMIZED;
                }
            }
            Msg::Close => self.closing = true,
            Msg::Clear => {
                self.feed(b"\x1b[2J\x1b[H");
            }
            Msg::Reset => {
                let (cols, rows) = (self.tab_mut().grid.cols(), self.tab_mut().grid.rows());
                self.tab_mut().grid = Grid::new(cols, rows);
                self.tab_mut().parser = Parser::new();
                self.tab_mut().view_top = None;
            }
            Msg::Scroll(p) => self.scroll_to(p),
            Msg::Wheel(p) => self.wheel(p),
            Msg::Key(k) => self.key(k),
            Msg::GridPointer(p) => self.grid_pointer(p),
        }
    }

    /// Whether another window has been asked for. Clears the record.
    pub fn take_new_window(&mut self) -> bool {
        core::mem::take(&mut self.new_window)
    }

    /// Whether a quit has been asked for. Clears the record.
    pub fn take_quit(&mut self) -> bool {
        core::mem::take(&mut self.quit)
    }

    /// What the terminal owes the clipboard, taken exactly once.
    pub fn take_clip_request(&mut self) -> Option<ClipRequest> {
        self.clip_request.take()
    }

    /// Deliver what a paste fetched: type it at the shell.
    ///
    /// Sent as input, not printed: the screen shows the program's output, and the shell must
    /// receive it.
    pub fn pasted(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.send(text.as_bytes());
    }

    /// A pointer event over the grid: press to anchor, drag to extend, release to end.
    ///
    /// Only the left button, and only while held: the router delivers motion unconditionally.
    fn grid_pointer(&mut self, p: PointerEvent) {
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

    /// Type what a key produced, if it is a key that types.
    fn key(&mut self, k: KeyEvent) {
        if k.pressed != KEY_DOWN && k.pressed != KEY_REPEAT {
            return;
        }
        // The menu's chords are taken before anything is encoded: `libterm::encode` folds `Ctrl+C`
        // to the interrupt `0x03`, so they carry Shift.
        if let Some(msg) = libui::menu::accel_match(&self.menu_table(), &k) {
            self.update(msg);
            return;
        }
        // While a find is open the keys are the field's, or they would type into the shell; after
        // the chord check so `Ctrl+Shift+F` can reopen it.
        if self.find.is_some() {
            match k.keycode {
                libkern::abi::KEY_ESC => {
                    self.find = None;
                }
                libkern::abi::KEY_ENTER => self.find_next(),
                code => {
                    if let Some(f) = self.find.as_mut() {
                        f.apply(code, k.modifiers, k.locks);
                    }
                }
            }
            return;
        }
        // Typing clears the selection; the return value is the repaint, so no selection means no
        // damage per keystroke.
        if self.tab_mut().grid.clear_selection() {
            self.tab_mut().view_moved = true;
        }
        let mut out = [0u8; libterm::encode::MAX_ENCODED];
        let n = libterm::encode::encode(k.keycode, k.modifiers, k.locks, &mut out);
        self.send(&out[..n]);
    }

    /// Find the next match and show it, or say there is none.
    fn find_next(&mut self) {
        let Some(needle) = self.find.as_ref().map(|f| alloc::string::String::from(f.text())) else {
            return;
        };
        if needle.is_empty() {
            return;
        }
        let g = &self.tab().grid;
        let (from_line, from_col) = match g.selection() {
            Some(sel) => {
                let (_, head) = sel.ordered();
                (head.0, head.1)
            }
            None => (g.oldest_line(), 0),
        };
        let hit = g.find_from(&needle, from_line, from_col).or_else(|| g.find_from(&needle, g.oldest_line(), 0));
        let Some((line, col)) = hit else { return };
        let n = needle.chars().count();
        let t = self.tab_mut();
        t.grid.select_from(line, col);
        t.grid.extend(line, col + n);
        // Through `scroll_to_line`, which sets `view_moved`: `select_from` damages screen rows,
        // which a view scrolled back a screen does not show.
        self.scroll_to_line(line);
    }

    /// Move the view to where a scrollbar interaction points.
    fn scroll_to(&mut self, p: PointerEvent) {
        let s = self.scroll();
        let Some(offset) = self.scroll_grab.apply(s, self.track_h(), p) else {
            return;
        };
        let line = self.tab().grid.oldest_line() + offset as u64;
        self.scroll_to_line(line);
    }

    /// Scroll the viewport by a turn of the wheel.
    ///
    /// A wheel down to the bottom parks there rather than resuming bottom-follow; typing resumes
    /// following.
    fn wheel(&mut self, p: PointerEvent) {
        if p.wheel == 0 {
            return;
        }
        // Widened before multiplying: `wheel` is an `i16` that saturates.
        let lines = i64::from(p.wheel) * i64::from(libui::click::WHEEL_UNITS);
        let top = self.view_line();
        let want = if lines < 0 { top.saturating_sub(lines.unsigned_abs()) } else { top.saturating_add(lines as u64) };
        self.scroll_to_line(want);
    }

    /// Anchor the viewport at absolute line `top`, clamped, repainting if that moved it.
    pub fn scroll_to_line(&mut self, top: u64) {
        let want = self.tab_mut().grid.clamp_view(top);
        if want != self.view_line() {
            self.tab_mut().view_moved = true;
        }
        // Recorded even at the bottom, so a program printing afterwards does not pull the view
        // along.
        self.tab_mut().view_top = Some(want);
    }

    /// The scrollbar's state: the viewport is a window onto the scrollback plus the screen.
    pub fn scroll(&self) -> ScrollState {
        let rows = self.tab().grid.rows() as u32;
        let oldest = self.tab().grid.oldest_line();
        let back = (self.tab().grid.top_line() - oldest) as u32;
        ScrollState { offset: (self.view_line() - oldest) as u32, visible: rows, total: back + rows }
    }

    /// The height the scrollbar's track is laid out at.
    pub fn track_h(&self) -> u32 {
        // What the `Dock` will leave it, less the grip's square: the grip is the topmost layer, so
        // a track under it cannot be pressed.
        self.window.h.saturating_sub(BARS_H + GRIP_W)
    }

    /// Record where each bar word sits, so an open menu knows where to hang from.
    pub fn place_menus(&mut self, view: &Element<Msg>, l: &libui::layout::Layout) {
        let n = self.menu_count();
        self.menus.set_anchors((0..n).map(|i| libui::layout::locate(view, l, MENU_BAR_KEY + i as u64)).collect());
    }

    /// How many menus the bar has, derived from the table.
    pub fn menu_count(&self) -> usize {
        self.menu_table().len()
    }

    /// The bar's menus, in bar order.
    ///
    /// Built rather than stored; Clear and Reset sit under Edit.
    pub fn menu_table(&self) -> Vec<Menu<Msg>> {
        vec![
            Menu {
                title: "File",
                items: vec![
                    Item::new("New Tab", Accel::ctrl_shift(NEW_TAB_KEYCODE, "T"), Msg::NewTab),
                    Item::new("Close Tab", Accel::ctrl_shift(CLOSE_TAB_KEYCODE, "W"), Msg::CloseTab(self.current)),
                    Item::Separator,
                    Item::new("New Window", Accel::ctrl_shift(NEW_WINDOW_KEYCODE, "N"), Msg::NewWindow),
                    Item::Separator,
                    Item::plain("Close Window", Msg::Close),
                    Item::new("Quit", Accel::ctrl_shift(QUIT_KEYCODE, "Q"), Msg::Quit),
                ],
            },
            Menu {
                title: "Edit",
                items: vec![
                    Item::new("Copy", Accel::ctrl_shift(COPY_KEYCODE, "C"), Msg::Copy)
                        .enabled(self.tab().grid.has_selection()),
                    Item::new("Paste", Accel::ctrl_shift(PASTE_KEYCODE, "V"), Msg::Paste),
                    Item::new("Select All", Accel::ctrl_shift(SELECT_ALL_KEYCODE, "A"), Msg::SelectAll)
                        .enabled(self.tab().grid.line_text(self.tab().grid.top_line()).is_some()),
                    Item::new("Find", Accel::ctrl_shift(FIND_KEYCODE, "F"), Msg::Find),
                    Item::new("Clear Scrollback", Accel::ctrl_shift(CLEAR_KEYCODE, "K"), Msg::ClearScrollback)
                        .enabled(self.tab().grid.oldest_line() < self.tab().grid.top_line()),
                    Item::Separator,
                    Item::plain("Clear", Msg::Clear),
                    Item::plain("Reset", Msg::Reset),
                ],
            },
        ]
    }

    /// The element tree for the current state.
    ///
    /// The theme is the caller's, as it paints this tree.
    pub fn view(&self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        let grid_px = self.metrics.pixel_size(self.tab().grid.cols(), self.tab().grid.rows());

        let bar = libui::menu::bar(&self.menu_table(), &self.menus, MENU_BAR_KEY, hovered, Msg::MenuBar, ui, BAR_H);
        // Always drawn: a strip that appeared on the second tab would move every row of the grid.
        let labels = self.tab_labels();
        let tabs: Vec<libui::widget::Tab<'_>> =
            labels.iter().map(|(key, label)| libui::widget::Tab { key: *key, label, marked: false }).collect();
        // The find field takes the tab strip's row, so the grid's height (which gates assert) does
        // not change.
        let strip = match self.find.as_ref() {
            Some(f) => libui::element::sized(
                Size::new(0, TAB_STRIP_H),
                libui::element::padding(
                    libui::element::Insets { top: 2, right: 6, bottom: 2, left: 6 },
                    text_field(f, false, WidgetState { active: true, ..Default::default() }, ui),
                ),
            ),
            None => libui::widget::tab_strip(
                &tabs,
                self.current,
                hovered,
                Msg::SelectTab,
                Msg::CloseTab,
                libui::widget::TabExtras::new_tab(Msg::NewTab),
                ui,
            ),
        };

        let title = title_bar(
            TITLE,
            self.tab().parser.directory(),
            self.focused,
            Msg::DragWindow,
            TitleButtons {
                minimise: Some(Msg::RequestState(WINDOW_STATE_MINIMIZED)),
                maximise: Some(Msg::RequestState(if self.maximized {
                    WINDOW_STATE_NORMAL
                } else {
                    WINDOW_STATE_MAXIMIZED
                })),
                // Close sends nothing: the client exits and the compositor tears down its windows.
                close: Some(Msg::Close),
            },
            hovered,
            ui,
        )
        .key(TITLE_KEY);
        let body = window_frame_with_grip(
            title,
            dock(
                vec![
                    docked(Edge::Top, bar.key(BAR_KEY)),
                    docked(Edge::Top, strip.key(TAB_STRIP_KEY)),
                    docked(
                        Edge::Right,
                        // Sized so the bar ends where the grip begins, or its bottom `GRIP_W` is
                        // unpressable (see [`track_h`](Self::track_h)). The key goes on the
                        // wrapper: the diff needs all siblings keyed.
                        sized(
                            Size::new(SCROLL_W, self.track_h()),
                            // Drawn only when there is history to reach; the column is kept so the
                            // grid's box does not widen and reflow.
                            if self.scroll().scrollable() {
                                scrollbar(self.scroll(), SCROLL_W, self.track_h(), ui).on_pointer(Msg::Scroll)
                            } else {
                                libui::element::fill(self.palette.background)
                            },
                        )
                        .key(SCROLLBAR_KEY),
                    ),
                ],
                // The pane: the terminal's ground, filled across the whole pane; a custom node
                // covers only its declared cells.
                libui::element::stack(vec![
                    libui::element::fill(self.palette.background),
                    libui::element::padding(
                        libui::element::Insets {
                            top: GRID_PAD_Y,
                            right: GRID_PAD_X,
                            bottom: GRID_PAD_Y,
                            left: GRID_PAD_X,
                        },
                        custom(GRID_KIND, grid_px)
                            .key(GRID_KEY)
                            .on_key(|k| Some(Msg::Key(k)))
                            .on_pointer(Msg::GridPointer),
                    ),
                ])
                .key(GRID_PANE_KEY),
            )
            // The wheel is the whole body's, so turning it over the scrollbar or tab strip scrolls
            // too.
            .on_wheel(Msg::Wheel),
            resize_grip(Msg::ResizeWindow(RESIZE_RIGHT | RESIZE_BOTTOM), ui).key(GRIP_KEY),
            self.window,
            self.focused,
            ui,
        );

        // The menu is not in this tree: a `popup` window of its own, clipped only by the screen.
        body
    }

    /// The menu popup's own element tree — the root of a second window, not a layer in this one.
    pub fn menu_view(&self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        self.menu(ui, hovered)
    }

    /// The popup's contents: whichever menu is open, framed.
    fn menu(&self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        let menus = self.menu_table();
        match self.menus.open().and_then(|i| menus.get(i)) {
            Some(m) => libui::menu::popup(m, &self.menus, MENU_ROW_KEY, hovered, ui),
            None => libui::widget::popup_frame(padding(Insets::all(2), libui::element::text("")), ui),
        }
    }
}

/// The rows of the grid a window-space rectangle covers.
pub fn rows_in(clip: Rect, grid_origin: libdraw::geom::Point, m: &Metrics, rows: usize) -> Vec<usize> {
    let top = clip.origin.y - grid_origin.y;
    let bottom = clip.bottom() as i32 - grid_origin.y;
    if bottom <= 0 || m.cell_h == 0 {
        return Vec::new();
    }
    let first = (top.max(0) as u32 / m.cell_h) as usize;
    let last = ((bottom - 1).max(0) as u32 / m.cell_h) as usize;
    (first..=last.min(rows.saturating_sub(1))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use libdraw::text::Font;
    use librsproto::surface::{KEY_UP, MOD_CTRL, MOD_SHIFT};
    use libui::layout::{FixedCell, layout, locate};

    const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSansMono.ttf");

    fn app() -> App {
        let f = Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses");
        App::new(20, 6, Metrics::new(&f, 16.0))
    }

    fn ptr(kind: u16, buttons: u16, flags: u16, x: i32, y: i32) -> PointerEvent {
        PointerEvent { kind, button: BTN_LEFT, buttons, flags, x, y, ..Default::default() }
    }

    fn press(a: &mut App, keycode: u16, modifiers: u16) {
        a.update(Msg::Key(KeyEvent::new(1, keycode, KEY_DOWN, modifiers)));
    }

    #[test]
    fn a_drag_over_the_grid_selects_and_a_copy_asks_for_that_text() {
        let mut a = app();
        a.feed(b"hello world");
        let (w, h) = (a.metrics.cell_w as i32, a.metrics.cell_h as i32);
        a.update(Msg::GridPointer(ptr(POINTER_BUTTON, 1, POINTER_PRESSED, 0, h / 2)));
        a.update(Msg::GridPointer(ptr(POINTER_MOTION, 1, 0, 5 * w, h / 2)));
        press(&mut a, COPY_KEYCODE, MOD_CTRL | MOD_SHIFT);
        assert_eq!(a.take_clip_request(), Some(ClipRequest::Copy(alloc::string::String::from("hello"))));
        assert!(a.take_outbox().is_empty(), "the chord was consumed, not typed");
    }

    #[test]
    fn motion_after_a_release_does_not_go_on_selecting() {
        // Select first: with no anchor `Grid::extend` returns early either way, so a motion-only
        // test would pin libterm's guard, not this one.
        let mut a = app();
        a.feed(b"hello world");
        let (w, h) = (a.metrics.cell_w as i32, a.metrics.cell_h as i32);
        a.update(Msg::GridPointer(ptr(POINTER_BUTTON, 1, POINTER_PRESSED, 0, h / 2)));
        a.update(Msg::GridPointer(ptr(POINTER_MOTION, 1, 0, 5 * w, h / 2)));
        a.update(Msg::GridPointer(ptr(POINTER_BUTTON, 0, 0, 5 * w, h / 2)));
        assert_eq!(a.grid().selected_text().as_deref(), Some("hello"), "the drag selected");
        a.update(Msg::GridPointer(ptr(POINTER_MOTION, 0, 0, 9 * w, h / 2)));
        assert_eq!(
            a.grid().selected_text().as_deref(),
            Some("hello"),
            "and a pointer merely passing over it afterwards did not extend it"
        );
    }

    #[test]
    fn copying_nothing_is_not_a_request() {
        let mut a = app();
        a.feed(b"hello");
        press(&mut a, COPY_KEYCODE, MOD_CTRL | MOD_SHIFT);
        assert_eq!(a.take_clip_request(), None);
    }

    #[test]
    fn ctrl_c_without_shift_is_still_the_interrupt() {
        let mut a = app();
        a.feed(b"hello");
        a.grid_mut().select_from(0, 0);
        a.grid_mut().extend(0, 5);
        press(&mut a, COPY_KEYCODE, MOD_CTRL);
        assert_eq!(a.take_clip_request(), None, "a plain Ctrl+C is not a copy");
        assert_eq!(a.take_outbox(), alloc::vec![0x03], "it is the interrupt byte");
    }

    #[test]
    fn a_paste_is_typed_at_the_shell_rather_than_drawn() {
        let mut a = app();
        a.pasted("ls\n");
        assert_eq!(a.take_outbox(), alloc::vec![b'l', b's', b'\n']);
    }

    #[test]
    fn typing_clears_the_selection() {
        let mut a = app();
        a.feed(b"hello");
        a.grid_mut().select_from(0, 0);
        a.grid_mut().extend(0, 5);
        press(&mut a, 30, 0); // `a`
        assert_eq!(a.grid().selection(), None);
    }

    #[test]
    fn a_selection_made_in_the_scrollback_repaints_what_is_being_looked_at() {
        // `select_from` damages screen rows; a view scrolled into history shows none of them, so
        // `damage_rows` must still repaint.
        let mut a = app();
        for i in 0..40 {
            a.feed(alloc::format!("line {i}\r\n").as_bytes());
        }
        a.scroll_to_line(a.grid().oldest_line());
        let _ = a.damage_rows();
        let h = a.metrics.cell_h as i32;
        a.update(Msg::GridPointer(ptr(POINTER_BUTTON, 1, POINTER_PRESSED, 0, h / 2)));
        let rows = a.damage_rows();
        assert!(rows.contains(&0), "the top viewport row is scrollback and has to repaint; got {rows:?}");
    }

    #[test]
    fn the_copy_and_paste_keycodes_are_the_letters_they_claim() {
        assert_eq!(libinput::keymap::to_char(SELECT_ALL_KEYCODE, 0), Some(b'a'));
        assert_eq!(libinput::keymap::to_char(FIND_KEYCODE, 0), Some(b'f'));
        assert_eq!(libinput::keymap::to_char(CLEAR_KEYCODE, 0), Some(b'k'));
        assert_eq!(libinput::keymap::to_char(COPY_KEYCODE, 0), Some(b'c'));
        assert_eq!(libinput::keymap::to_char(PASTE_KEYCODE, 0), Some(b'v'));
        assert_eq!(libinput::keymap::to_char(NEW_TAB_KEYCODE, 0), Some(b't'));
        assert_eq!(libinput::keymap::to_char(CLOSE_TAB_KEYCODE, 0), Some(b'w'));
    }

    #[test]
    fn the_grip_does_not_cover_any_part_of_the_scrollbar() {
        // The grip is the topmost layer, so any track under it cannot be pressed; a following view
        // puts the thumb at the bottom, under it.
        let f = Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses");
        let mut a = App::new(80, 24, Metrics::new(&f, 16.0));
        for i in 0..1200 {
            a.feed(alloc::format!("line{i}\r\n").as_bytes());
        }
        assert!(a.scroll().scrollable(), "precondition: there is a thumb to press");

        let e = a.view(&UiTheme::default(), None);
        let bounds = Rect::new(0, 0, a.window_size().w, a.window_size().h);
        let l = layout(&e, bounds, &FixedCell { w: 8, h: 16 });
        let bar = locate(&e, &l, SCROLLBAR_KEY).expect("the scrollbar is keyed");
        let grip = locate(&e, &l, GRIP_KEY).expect("the grip is keyed");
        assert!(bar.intersect(&grip).is_none(), "the grip {grip:?} overlaps the scrollbar {bar:?}");
        assert_eq!(bar.bottom(), grip.origin.y as i64, "the scrollbar {bar:?} stops short of the grip {grip:?}");
        let border = libui::widget::WINDOW_BORDER as i64;
        assert_eq!(grip.bottom(), bounds.bottom() - border, "and the grip is still in the corner");
        assert_eq!(grip.right(), bounds.right() - border);
    }

    /// Painted and measured rather than read off the tree: a tree test only shows the string
    /// reached an argument.
    #[test]
    fn the_title_bar_shows_where_the_shell_in_this_tab_says_it_is() {
        use libdraw::format::PixelFormat;
        use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};

        let f = Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses");
        let mut a = App::new(40, 8, Metrics::new(&f, 16.0));
        let ui = UiTheme::default();
        let (w, h) = (a.window_size().w, a.window_size().h);

        let reach = |a: &App| {
            let e = a.view(&ui, None);
            let l = layout(&e, Rect::new(0, 0, w, h), &FixedCell { w: 8, h: 16 });
            let bar = locate(&e, &l, TITLE_KEY).expect("the title bar is keyed");
            let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
            fb.clear(ui.background);
            let mut custom = |_: u32, _: Rect, _: Rect, _: &mut MemFramebuffer| {};
            libui::paint::paint(&mut fb, &f, &ui, &e, &l, Rect::new(0, 0, w, h), &mut custom);
            let (minimise, _) = libui::widget::title_button_centre(w, 2);
            let right = (minimise - libui::widget::TITLE_BUTTON_W as i32) as u32;
            // The text's own rows: the bar's bottom rule spans the full width and would give every
            // subtitle the same answer.
            let inset = bar.size.h / 4;
            let band = (bar.origin.y as u32 + inset)..(bar.bottom() as u32 - inset);
            (bar.origin.x as u32..right)
                .rev()
                .find(|x| {
                    band.clone().any(|y| {
                        let c = fb.get_pixel(*x, y);
                        c.is_some() && c != Some(ui.title_active) && c != Some(ui.title_inactive)
                    })
                })
                .expect("the window's name is drawn")
        };

        let bare = reach(&a);
        a.feed(b"\x1b]7;/home/alice\x07");
        let with_place = reach(&a);
        assert!(with_place > bare, "the bar reached {with_place} with a directory and {bare} without — no subtitle");

        a.update(Msg::NewTab);
        assert_eq!(reach(&a), bare, "a fresh tab inherited the first tab's directory");
        a.feed(b"\x1b]7;/home/alice/Documents\x07");
        let deeper = reach(&a);
        assert!(deeper > with_place, "the second tab's own, deeper, path did not reach the bar");
    }

    /// Only a different answer is a change, so the console line is one per `cd`; `check-terminal`
    /// cannot assert a line's absence.
    #[test]
    fn a_repeated_announcement_is_not_a_change() {
        let mut a = app();
        let key = a.current_tab();
        fn tab(a: &mut App, key: u64) -> &mut Term {
            a.tabs_mut().iter_mut().find(|t| t.key() == key).expect("the tab is there")
        }
        assert!(!tab(&mut a, key).take_directory_change(), "nothing said, nothing owed");

        a.feed(b"\x1b]7;/home\x07");
        assert!(tab(&mut a, key).take_directory_change(), "the first answer is a change");
        assert!(!tab(&mut a, key).take_directory_change(), "and it is owed exactly once");

        a.feed(b"\x1b]7;/home\x07");
        assert!(!tab(&mut a, key).take_directory_change(), "the same place is not a change");

        a.feed(b"\x1b]7;/home/alice\x07");
        assert!(tab(&mut a, key).take_directory_change(), "a different place is a change");

        a.feed(b"\x1b]0;a title\x07");
        assert!(!tab(&mut a, key).take_directory_change(), "a window title moved the terminal");
    }

    /// Painted and scanned rather than read off the tree: the widget is in the slot either way.
    /// Both halves in one test, each the other's control.
    #[test]
    fn the_scrollbar_appears_only_once_there_is_something_to_scroll() {
        use libdraw::format::PixelFormat;
        use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};

        let f = Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses");
        let mut a = App::new(40, 8, Metrics::new(&f, 16.0));
        let ui = UiTheme::default();
        let (w, h) = (a.window_size().w, a.window_size().h);

        let bar_ink = |a: &App| {
            let e = a.view(&ui, None);
            let l = layout(&e, Rect::new(0, 0, w, h), &FixedCell { w: 8, h: 16 });
            let bar = locate(&e, &l, SCROLLBAR_KEY).expect("the scrollbar's slot is keyed");
            let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
            fb.clear(ui.background);
            let mut custom = |_: u32, _: Rect, _: Rect, _: &mut MemFramebuffer| {};
            libui::paint::paint(&mut fb, &f, &ui, &e, &l, Rect::new(0, 0, w, h), &mut custom);
            let mut n = 0;
            for y in bar.origin.y as u32..bar.bottom() as u32 {
                for x in bar.origin.x as u32..bar.right() as u32 {
                    let c = fb.get_pixel(x, y);
                    if c.is_some() && c != Some(a.palette.background) && c != Some(ui.background) && c != Some(ui.face)
                    {
                        n += 1;
                    }
                }
            }
            (bar, n)
        };

        assert!(!a.scroll().scrollable(), "precondition: nothing has scrolled away yet");
        let (bar, ink) = bar_ink(&a);
        assert_eq!(ink, 0, "an empty terminal drew a bar in {bar:?}");

        produce(&mut a, 40);
        assert!(a.scroll().scrollable(), "precondition: there is history now");
        let (bar2, ink2) = bar_ink(&a);
        assert_eq!(bar2, bar, "the slot itself must not move, or the grid reflows");
        assert!(ink2 > 0, "a terminal with history drew no bar in {bar2:?}");
    }

    #[test]
    fn the_grip_asks_the_binary_for_a_resize_and_asks_once() {
        let mut a = app();
        assert_eq!(a.take_resize_request(), None, "nothing owed before the corner is touched");
        a.update(Msg::ResizeWindow(RESIZE_RIGHT | RESIZE_BOTTOM));
        assert_eq!(a.take_resize_request(), Some(RESIZE_RIGHT | RESIZE_BOTTOM));
        assert_eq!(a.take_resize_request(), None, "and owed exactly once");
    }

    #[test]
    fn pressing_the_corner_produces_the_resize_and_not_a_move() {
        let mut a = app();
        let (t, l, mut r) = window(&a);
        let e = a.view(&UiTheme::default(), None);
        let g = locate(&e, &l, GRIP_KEY).expect("the grip is keyed");
        let p = PointerEvent {
            window: 1,
            kind: librsproto::surface::POINTER_BUTTON,
            x: g.origin.x + g.size.w as i32 / 2,
            y: g.origin.y + g.size.h as i32 / 2,
            buttons: 1,
            flags: librsproto::surface::POINTER_PRESSED,
            ..Default::default()
        };
        for m in r.pointer(&t, &e, &l, p).0 {
            a.update(m);
        }
        assert_eq!(a.take_resize_request(), Some(RESIZE_RIGHT | RESIZE_BOTTOM));
        assert!(!a.take_move_request(), "and the press did not also start a move");
    }

    #[test]
    fn the_maximise_button_asks_and_follows_what_the_manager_says() {
        let mut a = app();
        click_maximise(&mut a);
        assert_eq!(a.take_state_request(), Some(WINDOW_STATE_MAXIMIZED));
        click_maximise(&mut a);
        assert_eq!(a.take_state_request(), Some(WINDOW_STATE_MAXIMIZED), "asking changed nothing");
        a.update(Msg::WindowState(WINDOW_STATE_MAXIMIZED));
        click_maximise(&mut a);
        assert_eq!(a.take_state_request(), Some(WINDOW_STATE_NORMAL));
        a.update(Msg::WindowState(WINDOW_STATE_NORMAL));
        click_maximise(&mut a);
        assert_eq!(a.take_state_request(), Some(WINDOW_STATE_MAXIMIZED), "and back, when the manager says");
    }

    #[test]
    fn minimising_does_not_disturb_which_way_the_maximise_button_points() {
        // Three buttons, one message type: treating every `RequestState` as about maximisation
        // would flip on a minimise.
        let mut a = app();
        a.update(Msg::WindowState(WINDOW_STATE_MAXIMIZED));
        a.update(Msg::WindowState(WINDOW_STATE_MINIMIZED));
        a.update(Msg::RequestState(WINDOW_STATE_MINIMIZED));
        let _ = a.take_state_request();
        click_maximise(&mut a);
        assert_eq!(a.take_state_request(), Some(WINDOW_STATE_NORMAL));
    }

    #[test]
    fn a_configure_takes_the_window_exactly_and_fits_the_grid_inside_it() {
        let mut a = app();
        let m = a.metrics;
        let want = Size::new(1280, 752);
        assert!(a.resize(want).is_some(), "a new size is a change");
        assert_eq!(a.window_size(), want, "committed at exactly what was asked for");
        assert_eq!(a.grid().cols(), ((1280 - CHROME_W) / m.cell_w) as usize);
        assert_eq!(a.grid().rows(), ((752 - CHROME_H) / m.cell_h) as usize);
        let g = m.pixel_size(a.grid().cols(), a.grid().rows());
        assert!(g.w + CHROME_W <= want.w && g.h + CHROME_H <= want.h);
    }

    #[test]
    fn a_configure_repeating_the_current_size_is_not_a_resize() {
        let mut a = app();
        let want = a.window_size();
        assert!(a.resize(want).is_none(), "same size, nothing to do");
    }

    #[test]
    fn a_degenerate_configure_still_leaves_a_usable_grid() {
        let mut a = app();
        assert!(a.resize(Size::new(1, 1)).is_some());
        assert_eq!((a.grid().cols(), a.grid().rows()), (1, 1));
        assert_eq!(a.window_size(), Size::new(1, 1));
    }

    #[test]
    fn a_scrolled_back_view_shows_the_same_text_after_a_resize() {
        let mut a = app();
        for i in 0..30 {
            a.feed(alloc::format!("line{i} and some more text here\r\n").as_bytes());
        }
        // An even line: each 29-character line is two rows at 20 columns, and a continuation row
        // would make the assertion wrong.
        a.scroll_to_line(a.grid().oldest_line() + 4);
        let want = line(&a, 0);
        assert!(want.starts_with("line2"), "the anchor is where it is thought to be: {want:?}");

        a.resize(Size::new(600, 400));
        assert!(
            line(&a, 0).starts_with(&want),
            "after the resize the viewport shows {:?}, which does not begin the text it showed \
             before ({want:?})",
            line(&a, 0)
        );
    }

    fn line(a: &App, row: usize) -> alloc::string::String {
        let s: alloc::string::String =
            (0..a.grid().cols()).map(|c| a.grid().view_cell(a.view_line(), row, c).map_or(' ', |x| x.ch)).collect();
        s.trim_end().into()
    }

    fn typed(a: &mut App, code: u16) -> alloc::vec::Vec<u8> {
        a.update(Msg::Key(key_ev(code, KEY_DOWN)));
        a.take_outbox()
    }

    #[test]
    fn a_key_press_becomes_bytes_in_the_outbox_and_nothing_on_the_grid() {
        let mut a = app();
        assert_eq!(typed(&mut a, 35), b"h", "h");
        assert_eq!(line(&a, 0), "", "the keystroke drew itself without being echoed");

        a.feed(b"h");
        assert_eq!(line(&a, 0), "h");
    }

    #[test]
    fn the_terminal_translates_nothing_on_the_way_out() {
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
        let mut a = app();
        a.feed(b"hi\r\n\x1b[1mbold\x1b[m");
        assert_eq!(line(&a, 0), "hi");
        assert_eq!(line(&a, 1), "bold");
        assert!(a.grid().cell(1, 0).unwrap().attrs.flags.contains(libterm::cell::Flags::BOLD));
    }

    #[test]
    fn clear_erases_the_screen_and_homes_the_cursor() {
        let mut a = app();
        a.feed(b"hello\r\nworld");
        a.update(Msg::Clear);
        assert_eq!(line(&a, 0), "");
        assert_eq!(line(&a, 1), "");
        assert_eq!(a.grid().cursor(), (0, 0));
    }

    #[test]
    fn clear_keeps_the_scrollback_and_reset_does_not() {
        let mut a = app();
        for _ in 0..10 {
            a.feed(b"x\r\n");
        }
        assert!(a.grid().scrollback().len() > 0, "nothing scrolled off");

        a.update(Msg::Clear);
        assert!(a.grid().scrollback().len() > 0, "clear threw away the scrollback");

        a.update(Msg::Reset);
        assert_eq!(a.grid().scrollback().len(), 0, "reset kept the scrollback");
    }

    #[test]
    fn reset_forgets_attributes_that_were_in_force() {
        let mut a = app();
        a.feed(b"\x1b[1;31m");
        a.update(Msg::Reset);
        a.feed(b"x");
        assert_eq!(a.grid().cell(0, 0).unwrap().attrs, libterm::cell::Attributes::default());
    }

    #[test]
    fn reset_also_abandons_a_half_finished_sequence() {
        let mut a = app();
        a.feed(b"\x1b[1"); // mid-sequence
        a.update(Msg::Reset);
        a.feed(b"m");
        assert_eq!(line(&a, 0), "m", "the parser was still mid-sequence after a reset");
    }

    #[test]
    fn the_menu_opens_and_closes() {
        let mut a = app();
        assert_eq!(a.menus.open(), None);
        a.update(Msg::MenuBar(1));
        assert_eq!(a.menus.open(), Some(1));
        a.update(Msg::MenuBar(1));
        assert_eq!(a.menus.open(), None, "the same word again closes it");
        a.update(Msg::MenuBar(0));
        a.update(Msg::MenuBar(1));
        assert_eq!(a.menus.open(), Some(1));
    }

    #[test]
    fn choosing_an_item_closes_the_menu() {
        let names = app().menu_table();
        let rows: Vec<Msg> = names
            .iter()
            .flat_map(|m| m.items.iter())
            .filter_map(|it| match it {
                Item::Action { msg, .. } => Some(*msg),
                Item::Separator => None,
            })
            .collect();
        assert!(rows.len() >= 5, "the table is not empty, or this test proves nothing");
        for msg in rows {
            let mut a = app();
            a.update(Msg::MenuBar(1));
            a.update(msg);
            assert_eq!(a.menus.open(), None, "{msg:?} left the menu open");
        }
        // Negative control: the bar's own word is not a row, or no menu could ever open.
        let mut a = app();
        a.update(Msg::MenuBar(0));
        assert_eq!(a.menus.open(), Some(0));
    }

    /// Arrowing sideways changes both where the popup hangs and how big it must be, so the binary
    /// rebuilds the window on a change.
    #[test]
    fn the_two_menus_hang_from_different_words_and_measure_differently() {
        let mut a = app();
        let cell = FixedCell { w: 8, h: 16 };
        let bounds = Rect::new(0, 0, a.window_size().w, a.window_size().h);
        let view = a.view(&UiTheme::default(), None);
        let l = layout(&view, bounds, &cell);
        a.menus.set_anchors((0..a.menu_count()).map(|i| locate(&view, &l, MENU_BAR_KEY + i as u64)).collect());

        let measure = |a: &App| {
            libui::layout::measure(
                &a.menu_view(&UiTheme::default(), None),
                libui::layout::Constraints::loose(bounds.size),
                &cell,
            )
        };
        a.menus.toggle(0);
        let (file_at, file_size) = (a.menus.anchor(), measure(&a));
        a.menus.toggle(1);
        let (edit_at, edit_size) = (a.menus.anchor(), measure(&a));

        assert!(file_at.is_some() && edit_at.is_some(), "both words were laid out");
        assert_ne!(file_at, edit_at, "the two menus hang from different words");
        assert_ne!(edit_size, file_size, "the two menus measure the same, so a rebuild would be undetectable here");
    }

    /// Drives `App::key`, not `accel_match` on a row's own `Accel` (both sides of one table); an
    /// equivalence, not a second list of what each action does.
    #[test]
    fn every_advertised_chord_does_what_its_row_says() {
        fn digest(a: &mut App) -> alloc::string::String {
            let clip = a.take_clip_request();
            // Every outbox, tab count and current tab, selection extent and whether a find is open:
            // each was something the negative control below caught the digest unable to see.
            let (nw, q) = (a.take_new_window(), a.take_quit());
            let finding = a.find.as_ref().map(|f| alloc::string::String::from(f.text()));
            alloc::format!(
                "{clip:?}|{}|{}|{:?}|{:?}|{}|{}|{nw}{q}|{finding:?}",
                line(a, 0),
                a.closing(),
                a.grid().selection(),
                a.menus.open(),
                a.tab_labels().len(),
                a.current_tab(),
            )
        }
        fn selected() -> App {
            let mut a = app();
            a.feed(b"hello world");
            let (w, h) = (a.metrics.cell_w as i32, a.metrics.cell_h as i32);
            a.update(Msg::GridPointer(ptr(POINTER_BUTTON, 1, POINTER_PRESSED, 0, h / 2)));
            a.update(Msg::GridPointer(ptr(POINTER_MOTION, 1, 0, 5 * w, h / 2)));
            a
        }
        let table = selected().menu_table();
        let mut checked = 0;
        for it in table.iter().flat_map(|m| m.items.iter()) {
            let Item::Action { accel: Some(acc), msg, label, enabled: true, .. } = it else {
                continue;
            };
            checked += 1;
            let ev = KeyEvent::new(1, acc.key(), KEY_DOWN, acc.mods());
            assert_eq!(
                libui::menu::accel_match(&table, &ev).as_ref(),
                Some(msg),
                "{label} advertises {} and the table hands it to another row",
                acc.label()
            );
            let (mut by_row, mut by_chord) = (selected(), selected());
            by_row.update(*msg);
            by_chord.update(Msg::Key(ev));
            let (by_row, by_chord, untouched) = (digest(&mut by_row), digest(&mut by_chord), digest(&mut selected()));
            assert_eq!(by_chord, by_row, "{label}: {} does not do what choosing the row does", acc.label());
            // Negative control: the two agreeing is evidence only if the row moved something.
            assert_ne!(by_row, untouched, "{label} changes nothing, so this row proves nothing about routing");
        }
        assert_eq!(checked, 8, "the four tab and clipboard rows, Select All and Find, plus New Window and Quit");
    }

    #[test]
    fn a_find_takes_the_keys_and_walks_the_matches() {
        let mut a = app();
        a.feed(b"hello world");
        a.update(Msg::Find);
        assert!(a.find.is_some(), "the field is open");

        for c in "hello".bytes() {
            let code = (1u16..=90).find(|k| libinput::keymap::to_char(*k, 0) == Some(c)).expect("a keycode");
            a.update(Msg::Key(KeyEvent::new(1, code, KEY_DOWN, 0)));
        }
        assert_eq!(a.find.as_ref().map(|f| f.text()), Some("hello"), "the keys went to the field");

        a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_ENTER, KEY_DOWN, 0)));
        assert_eq!(a.grid().selected_text().as_deref(), Some("hello"), "Enter selected the match");

        a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_ESC, KEY_DOWN, 0)));
        assert!(a.find.is_none(), "Esc closed it");
    }

    /// `select_from` damages screen rows and `damage_rows` drops those off the bottom, so a view
    /// scrolled back a screen would select a match nobody redrew.
    #[test]
    fn a_find_that_scrolls_back_repaints_the_viewport() {
        let mut a = app();
        a.feed(b"needle\r\n");
        for _ in 0..10 {
            a.feed(b"filler\r\n");
        }
        assert!(a.grid().oldest_line() < a.grid().top_line(), "the needle is in the history");
        let _ = a.damage_rows();

        a.update(Msg::Find);
        for c in "needle".bytes() {
            let code = (1u16..=90).find(|k| libinput::keymap::to_char(*k, 0) == Some(c)).expect("a keycode");
            a.update(Msg::Key(KeyEvent::new(1, code, KEY_DOWN, 0)));
        }
        a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_ENTER, KEY_DOWN, 0)));

        assert_eq!(a.grid().selected_text().as_deref(), Some("needle"), "it found the match");
        assert_ne!(a.view_line(), a.grid().top_line(), "and scrolled back to show it");
        assert_eq!(a.damage_rows().len(), a.grid().rows(), "a viewport that moved repaints all of itself");
    }

    #[test]
    fn click_runs_take_a_word_then_a_line() {
        let mut a = app();
        a.feed(b"hello world");
        let at = libdraw::geom::Point::new(4, 4);
        let press = |a: &mut App| {
            let h = a.metrics.cell_h as i32;
            a.update(Msg::GridPointer(ptr(POINTER_BUTTON, 1, POINTER_PRESSED, 0, h / 2)));
        };

        a.note_press(at, 1_000);
        press(&mut a);
        assert_eq!(a.grid().selected_text(), None, "one press starts a sweep and selects nothing");

        a.note_press(at, 1_100);
        press(&mut a);
        assert_eq!(a.grid().selected_text().as_deref(), Some("hello"), "two take the word");

        a.note_press(at, 1_200);
        press(&mut a);
        assert_eq!(a.grid().selected_text().as_deref(), Some("hello world"), "three take the line");

        // A fourth selects nothing either way; the fifth proves the run restarted, being a word
        // again only then.
        a.note_press(at, 1_300);
        press(&mut a);
        assert_eq!(a.grid().selected_text(), None, "a fourth selects nothing either way");

        a.note_press(at, 1_400);
        press(&mut a);
        assert_eq!(
            a.grid().selected_text().as_deref(),
            Some("hello"),
            "the fifth is the second of a new run, so it takes a word"
        );
    }

    #[test]
    fn clearing_the_scrollback_returns_the_view() {
        let mut a = app();
        for _ in 0..8 {
            a.feed(b"line\r\n");
        }
        let top = a.grid().top_line();
        assert!(a.grid().oldest_line() < top, "there is history");

        // Scrolled back first, or the test passes with the view handling deleted.
        a.tab_mut().view_top = Some(a.grid().oldest_line());
        assert_ne!(a.view_line(), top, "the view is in the history");

        let _ = a.damage_rows();

        a.update(Msg::ClearScrollback);
        assert_eq!(a.grid().oldest_line(), top, "the history is gone");
        assert_eq!(a.view_line(), top, "and the view came back to the screen");
        // The repaint is the assertion: `view_line` clamps on read, so it reports the screen either
        // way.
        assert_eq!(a.damage_rows().len(), a.grid().rows(), "moving the viewport repaints all of it");
    }

    /// `check-terminal` clicks the Edit menu's `Clear` by number; this is that number, as the gate
    /// cannot link this crate.
    #[test]
    fn the_gate_clicks_the_row_it_means() {
        // The constant in `tools/xtask/src/gates/terminal.rs`, as `MENU_ROW_KEY + n`.
        const CLEAR_ROW: usize = 6;
        let a = app();
        let edit = a.menu_table().into_iter().find(|m| m.title == "Edit").expect("an Edit menu");
        match &edit.items[CLEAR_ROW] {
            Item::Action { label, msg, .. } => {
                assert_eq!(*label, "Clear", "check-terminal's CLEAR_ROW is not Clear any more");
                assert_eq!(*msg, Msg::Clear);
            }
            Item::Separator => panic!("check-terminal's CLEAR_ROW is a rule, not a row"),
        }
    }

    #[test]
    fn the_menu_is_not_a_layer_in_the_windows_tree() {
        for (open, anchor) in [(false, None), (true, Some(Rect::new(4, 0, 60, BAR_H))), (true, None)] {
            let mut a = app();
            if open {
                a.menus.toggle(0);
            }
            a.menus.set_anchors(vec![anchor, None]);
            let libui::element::Node::Stack(layers) = &a.view(&UiTheme::default(), None).node else {
                panic!("open={open}: the window's tree is the framed window");
            };
            assert_eq!(layers.len(), 4, "open={open}: face, content, grip and border, and nothing else");
            assert!(
                matches!(layers[3].node, libui::element::Node::Outline { .. }),
                "open={open}: the border is the top layer, over the grip"
            );
        }
    }

    #[test]
    fn the_anchor_is_under_the_menu_item_and_escapes_the_bar() {
        let mut a = app();
        let cell = FixedCell { w: 8, h: 16 };
        let bounds = Rect::new(0, 0, a.window_size().w, a.window_size().h);

        let view = a.view(&UiTheme::default(), None);
        let l = layout(&view, bounds, &cell);
        let item = locate(&view, &l, MENU_BAR_KEY).expect("the menu's first word is keyed");
        a.menus.set_anchors(vec![Some(item), None]);
        a.menus.toggle(0);
        assert_eq!(a.menus.anchor(), Some((item.origin.x, item.bottom() as i32)));

        let bar_top = (WINDOW_CONTENT_Y + TITLE_BAR_H) as i32;
        assert!(item.origin.y >= bar_top, "the item is below the title bar");
        assert!(item.bottom() <= (bar_top as u32 + BAR_H) as i64, "the item is inside the menu bar");

        let menu = a.menu_view(&UiTheme::default(), None);
        let size = libui::layout::measure(&menu, libui::layout::Constraints::loose(bounds.size), &cell);
        assert!(size.w > 0 && size.h > 0, "the menu measures to something");
        assert!(
            item.bottom() + size.h as i64 > BAR_H as i64,
            "and it extends past the bar it hangs from — which only a window can do"
        );
    }

    #[test]
    fn the_window_is_the_grid_plus_its_chrome() {
        let a = app();
        let g = a.metrics.pixel_size(20, 6);
        assert_eq!(a.window_size(), Size::new(g.w + CHROME_W, g.h + CHROME_H));
        assert_eq!(
            a.grid_origin(),
            libdraw::geom::Point::new(
                (WINDOW_CONTENT_X + GRID_PAD_X) as i32,
                (WINDOW_CONTENT_Y + TITLE_BAR_H + BAR_H + TAB_STRIP_H + GRID_PAD_Y) as i32
            ),
            "the grid starts below the bars and the tab strip, inside the frame and the pane's margin"
        );
        assert_eq!(
            a.window_size().h - a.grid_origin().y as u32 - g.h,
            GRID_PAD_Y + libui::widget::WINDOW_BORDER + libui::widget::WINDOW_FRAME,
            "what is left below the grid is the margin, the frame and the border, and nothing else"
        );
    }

    #[test]
    fn dragging_the_title_bar_asks_the_binary_for_a_move_and_asks_once() {
        let mut a = app();
        assert!(!a.take_move_request(), "nothing is owed before the bar is touched");
        a.update(Msg::DragWindow);
        assert!(a.take_move_request(), "the press is owed to the compositor");
        assert!(!a.take_move_request(), "and owed exactly once");
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
        assert_eq!(s.total, a.grid().scrollback().len() as u32 + 6);
    }

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

    fn produce(a: &mut App, n: usize) {
        for i in 0..n {
            a.feed(alloc::format!("{i}\r\n").as_bytes());
        }
    }

    #[test]
    fn dragging_the_scrollbar_moves_the_view_into_the_scrollback() {
        let mut a = app();
        produce(&mut a, 40);
        assert_eq!(a.view_line(), a.grid().top_line(), "starts at the bottom");

        grab(&mut a, 0);
        assert_eq!(a.view_line(), a.grid().oldest_line(), "the top of the track is the oldest");
        assert_eq!(line(&a, 0), "0", "and the oldest line is on show");

        let bottom = a.track_h() as i32;
        grab(&mut a, bottom);
        assert_eq!(a.view_line(), a.grid().top_line(), "the bottom is the live screen again");
    }

    /// The terminal uses `ScrollGrab`: re-centring the thumb on press would throw away the screen
    /// being read.
    #[test]
    fn taking_hold_of_the_thumb_does_not_move_the_view() {
        let mut a = app();
        produce(&mut a, 200);
        let mid = a.track_h() as i32 / 2;
        grab(&mut a, mid);
        let parked = a.view_line();
        assert_ne!(parked, a.grid().top_line(), "premise: scrolled back");
        assert_ne!(parked, a.grid().oldest_line(), "premise: not at the top either");

        let (pos, len) = a.scroll().thumb(a.track_h());
        for (what, y) in [("its top", pos as i32), ("its bottom", pos as i32 + len as i32 - 1)] {
            grab(&mut a, y);
            assert_eq!(a.view_line(), parked, "the press on {what} moved the view");
        }

        let before = a.view_line();
        a.update(Msg::Scroll(PointerEvent {
            kind: librsproto::surface::POINTER_MOTION,
            buttons: 1,
            y: pos as i32 + len as i32 - 1 + 20,
            ..Default::default()
        }));
        assert!(a.view_line() > before, "the drag did not follow the pointer down");
    }

    #[test]
    fn a_pointer_over_the_scrollbar_with_no_button_held_does_not_scroll() {
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

    fn turn(a: &mut App, dz: i16) {
        a.update(Msg::Wheel(PointerEvent {
            kind: librsproto::surface::POINTER_WHEEL,
            wheel: dz,
            ..Default::default()
        }));
    }

    #[test]
    fn the_wheel_scrolls_three_lines_a_detent_in_the_direction_it_turned() {
        let mut a = app();
        produce(&mut a, 40);
        let bottom = a.view_line();

        turn(&mut a, -1);
        assert_eq!(a.view_line(), bottom - 3, "up one detent is three lines back");
        turn(&mut a, -2);
        assert_eq!(a.view_line(), bottom - 9, "and detents multiply");
        turn(&mut a, 1);
        assert_eq!(a.view_line(), bottom - 6, "positive is down, back toward the live screen");
    }

    /// Scrolling changes which cells are on show, not the grid's own damage, so the wheel must
    /// repaint the viewport explicitly.
    #[test]
    fn scrolling_by_wheel_repaints_the_viewport() {
        let mut a = app();
        produce(&mut a, 40);
        let _ = a.damage_rows(); // start from a clean frame

        turn(&mut a, -1);
        assert_eq!(a.damage_rows().len(), a.grid().rows(), "the viewport moved and nothing was repainted");
    }

    #[test]
    fn the_wheel_stops_at_both_ends_rather_than_running_past_them() {
        let mut a = app();
        produce(&mut a, 40);

        turn(&mut a, -1000);
        assert_eq!(a.view_line(), a.grid().oldest_line(), "scrolled past the top of history");
        turn(&mut a, 1000);
        assert_eq!(a.view_line(), a.grid().top_line(), "and past the live screen");
    }

    #[test]
    fn a_wheel_record_with_no_detents_does_not_move_the_view() {
        let mut a = app();
        produce(&mut a, 40);
        turn(&mut a, -2);
        let parked = a.view_line();
        turn(&mut a, 0);
        assert_eq!(a.view_line(), parked);
    }

    /// `wheel` is `i16` and saturates, so a stalled consumer gets thousands of detents. 20 000, not
    /// `i16::MAX`: `32767 * 3` wraps to a still-positive `32765` and would pass against the broken
    /// arithmetic.
    #[test]
    fn a_huge_wheel_delta_scrolls_down_rather_than_wrapping_upward() {
        let mut a = app();
        produce(&mut a, 40);
        turn(&mut a, -20); // somewhere in the middle of the history
        assert_ne!(a.view_line(), a.grid().top_line(), "the premise: scrolled back");

        turn(&mut a, 20_000);
        assert_eq!(a.view_line(), a.grid().top_line(), "20 000 detents down is the live screen");
    }

    #[test]
    fn typing_snaps_the_view_back_to_the_bottom() {
        let mut a = app();
        produce(&mut a, 40);
        grab(&mut a, 0);
        assert_ne!(a.view_line(), a.grid().top_line(), "the premise: scrolled away");

        typed(&mut a, 35); // h
        assert_eq!(a.view_line(), a.grid().top_line(), "typing did not come back to the bottom");

        grab(&mut a, 0);
        let scrolled = a.view_line();
        typed(&mut a, libkern::abi::KEY_LEFTSHIFT);
        assert_eq!(a.view_line(), scrolled, "pressing Shift snapped the view");
    }

    #[test]
    fn output_arriving_does_not_drag_a_scrolled_back_view_with_it() {
        let mut a = app();
        produce(&mut a, 40);
        grab(&mut a, 0);
        let showing: alloc::vec::Vec<alloc::string::String> = (0..a.grid().rows()).map(|r| line(&a, r)).collect();

        a.feed(b"more\r\nand more\r\n");
        let after: alloc::vec::Vec<alloc::string::String> = (0..a.grid().rows()).map(|r| line(&a, r)).collect();
        assert_eq!(showing, after, "the view moved under the reader");
    }

    #[test]
    fn the_scrollbar_reports_where_the_view_is_not_only_how_much_there_is() {
        let mut a = app();
        produce(&mut a, 40);
        let s = a.scroll();
        assert_eq!(s.offset, s.total - s.visible, "following: the thumb is at the end");

        grab(&mut a, 0);
        assert_eq!(a.scroll().offset, 0, "scrolled to the top: so is the thumb");
    }

    #[test]
    fn damage_is_reported_in_viewport_rows_and_drops_what_is_below_the_view() {
        let mut a = app();
        produce(&mut a, 40);
        let _ = a.damage_rows();

        let back_one = a.grid().top_line() - 1;
        a.scroll_to_line(back_one);
        let _ = a.damage_rows(); // the scroll itself damaged everything
        a.feed(b"\x1b[1;1Hz"); // touch screen row 0
        let d = a.damage_rows();
        assert!(d.contains(&1), "screen row 0 should show at viewport row 1, got {d:?}");
        assert!(!d.contains(&0), "and not at viewport row 0: {d:?}");

        grab(&mut a, 0);
        let _ = a.damage_rows();
        a.feed(b"\x1b[1;1Hy");
        assert!(a.damage_rows().is_empty(), "damage for a row that is not on screen");
    }

    #[test]
    fn moving_the_view_repaints_all_of_it() {
        let mut a = app();
        produce(&mut a, 40);
        let _ = a.damage_rows();
        assert!(a.damage_rows().is_empty(), "the premise: nothing outstanding");

        grab(&mut a, 0);
        assert_eq!(a.damage_rows().len(), a.grid().rows(), "a scroll must repaint the viewport");

        let _ = a.damage_rows();
        a.snap_to_bottom();
        assert_eq!(a.damage_rows().len(), a.grid().rows());

        let _ = a.damage_rows();
        a.snap_to_bottom();
        assert!(a.damage_rows().is_empty(), "an idempotent snap still repainted");
    }

    fn key_ev(code: u16, pressed: u16) -> KeyEvent {
        KeyEvent::new(1, code, pressed, 0)
    }

    #[test]
    fn the_grid_sits_in_a_margin_of_the_terminals_own_ground() {
        // A custom node covers only the cells it declares, so the pane is an element of its own;
        // this pins its three properties.
        let a = app();
        let (t, _, _) = window(&a);
        fn find(w: &libui::diff::Widget, key: u64) -> Option<&libui::diff::Widget> {
            if w.key == Some(key) {
                return Some(w);
            }
            w.children.iter().find_map(|c| find(c, key))
        }
        let root = t.root().expect("a frame");
        let grid = find(root, GRID_KEY).expect("the grid is keyed");
        let pane = find(root, GRID_PANE_KEY).expect("the pane is keyed");
        assert_eq!(grid.rect.origin, a.grid_origin(), "the cells are not where the origin says");
        assert_eq!(grid.rect.origin.x - pane.rect.origin.x, GRID_PAD_X as i32);
        assert_eq!(grid.rect.origin.y - pane.rect.origin.y, GRID_PAD_Y as i32);
        let ground = pane.children.first().expect("the pane has its ground");
        assert_eq!(ground.print, libui::diff::Fingerprint::Fill(a.palette.background));
        assert_eq!(ground.rect, pane.rect, "the ground does not fill the pane");
    }

    fn window(a: &App) -> (libui::diff::Tree, libui::layout::Layout, libui::route::Router) {
        let bounds = Rect::new(0, 0, a.window_size().w, a.window_size().h);
        let e = a.view(&UiTheme::default(), None);
        let l = layout(&e, bounds, &FixedCell { w: 8, h: 16 });
        let mut t = libui::diff::Tree::new();
        t.update(&e, &l).expect("a clean frame");
        let mut r = libui::route::Router::new();
        let id = t.find_by_key(GRID_KEY).expect("the grid is keyed");
        assert!(r.focus(&t, &e, id), "the grid must be able to take focus");
        (t, l, r)
    }

    /// Through the router: reading the state instead would pass for a toggle that never reached the
    /// bar.
    fn click_maximise(a: &mut App) {
        let (t, l, mut r) = window(a);
        let e = a.view(&UiTheme::default(), None);
        let (x, y) = libui::widget::title_button_centre(a.window_size().w, 1);
        let mut msgs = alloc::vec::Vec::new();
        for (flags, held) in [(librsproto::surface::POINTER_PRESSED, 1), (0, 0)] {
            let p = PointerEvent {
                window: 1,
                kind: librsproto::surface::POINTER_BUTTON,
                x,
                y,
                buttons: held,
                flags,
                ..Default::default()
            };
            msgs.extend(r.pointer(&t, &e, &l, p).0);
        }
        assert!(!msgs.is_empty(), "nothing under the maximise button at ({x}, {y})");
        for m in msgs {
            a.update(m);
        }
    }

    /// Through the real tree and router: direct-`update` tests cannot see `on_wheel` being deleted.
    /// Over the grid and the scrollbar, as hanging it on the body is what makes the second work.
    #[test]
    fn a_wheel_over_the_window_scrolls_it_through_the_router() {
        for (what, x) in [
            ("over the grid", 40),
            ("over the scrollbar", -(SCROLL_W as i32) / 2), // measured from the right edge
        ] {
            let mut a = app();
            produce(&mut a, 40);
            let bottom = a.view_line();
            let (t, l, mut r) = window(&a);
            let e = a.view(&UiTheme::default(), None);
            let x = if x < 0 { a.window_size().w as i32 + x } else { x };
            let p = PointerEvent {
                window: 1,
                kind: librsproto::surface::POINTER_WHEEL,
                wheel: -1,
                x,
                y: (a.window_size().h / 2) as i32,
                ..Default::default()
            };
            let msgs = r.pointer(&t, &e, &l, p).0;
            assert!(!msgs.is_empty(), "{what}: nothing took the wheel at x={x}");
            for m in msgs {
                a.update(m);
            }
            assert_eq!(
                a.view_line(),
                bottom - u64::from(libui::click::WHEEL_UNITS),
                "{what}: the wheel did not reach the viewport"
            );
        }
    }

    #[test]
    fn a_key_repeat_reaches_the_grid_through_the_router_and_types() {
        let mut a = app();
        let (t, _l, r) = window(&a);
        let e = a.view(&UiTheme::default(), None);

        for msg in [
            r.key(&t, &e, key_ev(35, KEY_DOWN)),   // h, pressed
            r.key(&t, &e, key_ev(35, KEY_REPEAT)), // ...and held
            r.key(&t, &e, key_ev(35, KEY_REPEAT)),
        ] {
            a.update(msg.expect("the focused grid claims the key"));
        }
        assert_eq!(a.take_outbox(), b"hhh", "a repeat did not type");
    }

    #[test]
    fn a_key_release_reaching_the_grid_types_nothing() {
        let mut a = app();
        let (t, _l, r) = window(&a);
        let e = a.view(&UiTheme::default(), None);
        a.update(r.key(&t, &e, key_ev(35, KEY_DOWN)).unwrap());
        a.update(r.key(&t, &e, key_ev(35, KEY_UP)).unwrap());
        assert_eq!(a.take_outbox(), b"h", "the release typed as well");
    }

    #[test]
    fn the_menu_bars_accelerator_path_still_works_while_the_grid_holds_the_keyboard() {
        let a = app();
        let (t, _l, r) = window(&a);
        let e = a.view(&UiTheme::default(), None);
        assert!(matches!(r.key(&t, &e, key_ev(35, KEY_DOWN)), Some(Msg::Key(_))));
    }

    #[test]
    fn a_clip_maps_to_the_rows_it_covers() {
        let a = app();
        let m = &a.metrics;
        let o = a.grid_origin();
        let all = rows_in(Rect::new(0, o.y, 100, m.cell_h * 6), o, m, 6);
        assert_eq!(all, alloc::vec![0, 1, 2, 3, 4, 5]);

        let one = rows_in(Rect::new(0, o.y + (m.cell_h * 2) as i32, 100, m.cell_h), o, m, 6);
        assert_eq!(one, alloc::vec![2]);

        let short = rows_in(Rect::new(0, o.y, 100, m.cell_h), o, m, 6);
        assert_eq!(short, alloc::vec![0]);

        let two = rows_in(Rect::new(0, o.y + m.cell_h as i32 - 1, 100, 2), o, m, 6);
        assert_eq!(two, alloc::vec![0, 1]);
    }

    #[test]
    fn a_clip_above_or_beyond_the_grid_maps_to_nothing_out_of_range() {
        let a = app();
        let m = &a.metrics;
        let o = a.grid_origin();
        assert!(rows_in(Rect::new(0, 0, 100, BAR_H), o, m, 6).is_empty());
        let past = rows_in(Rect::new(0, o.y, 100, m.cell_h * 99), o, m, 6);
        assert_eq!(*past.last().unwrap(), 5);
    }
    /// `diff` rejects a parent whose children are partly keyed, which makes the whole window
    /// undiffable where no layout or paint test can see it.
    #[test]
    fn the_window_tree_diffs_across_the_menu_states() {
        let mut a = app();
        let theme = UiTheme::default();
        let mut tree = libui::diff::Tree::new();
        let cell = libui::layout::FixedCell { w: 8, h: 16 };
        let check = |a: &mut App, hovered, tree: &mut libui::diff::Tree, what: &str| {
            let size = a.window_size();
            let e = a.view(&theme, hovered);
            let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &cell);
            tree.update(&e, &l).unwrap_or_else(|err| panic!("{what}: {err:?}"));
        };
        check(&mut a, None, &mut tree, "with the menu shut");
        check(&mut a, Some(MENU_BAR_KEY), &mut tree, "with a bar word hovered");
        a.update(Msg::MenuBar(0));
        check(&mut a, Some(MENU_BAR_KEY), &mut tree, "with File open and hovered");
        a.update(Msg::MenuBar(1));
        check(&mut a, None, &mut tree, "with Edit open and nothing hovered");
        a.update(Msg::MenuBar(1));
        check(&mut a, None, &mut tree, "back to shut");
    }

    /// `libui` keys a tab's close box as the tab's key with `TAB_CLOSE_BIT` set, so tab keys must
    /// leave that bit clear.
    #[test]
    fn a_tabs_close_box_can_have_a_key_of_its_own() {
        let mut a = app();
        for _ in 0..3 {
            a.update(Msg::NewTab);
        }
        for t in a.tabs() {
            assert_eq!(t.key() & libui::widget::TAB_CLOSE_BIT, 0, "tab key {:#x} sets the close bit", t.key());
            assert_ne!(libui::widget::tab_close_key(t.key()), t.key());
        }
        assert_eq!((TAB_KEY_BASE + (1 << 32)) & libui::widget::TAB_CLOSE_BIT, 0);
    }

    #[test]
    fn a_second_tab_shares_nothing_with_the_first() {
        let mut a = app();
        a.feed(b"first");
        let one = a.current_tab();

        a.update(Msg::NewTab);
        let two = a.current_tab();
        assert_ne!(two, one, "a new tab is current, and is not the old one");
        assert_eq!(line(&a, 0), "", "and its grid is empty");

        a.feed(b"second");
        assert_eq!(line(&a, 0), "second");
        a.update(Msg::SelectTab(one));
        assert_eq!(line(&a, 0), "first", "the first tab kept its own screen");

        a.update(Msg::Key(KeyEvent::new(1, 30, KEY_DOWN, 0))); // `a`
        let outboxes: alloc::vec::Vec<(u64, usize)> =
            a.tabs_mut().iter_mut().map(|t| (t.key(), t.take_outbox().len())).collect();
        assert_eq!(outboxes, alloc::vec![(one, 1), (two, 0)], "only the current tab was typed at");
    }

    #[test]
    fn closing_the_last_tab_closes_the_window() {
        let mut a = app();
        let one = a.current_tab();
        a.update(Msg::NewTab);
        let two = a.current_tab();

        a.update(Msg::CloseTab(two));
        assert!(!a.closing(), "one tab left, so the window stays");
        assert_eq!(a.current_tab(), one, "and the survivor is current");
        assert_eq!(a.tab_labels().len(), 1);

        a.update(Msg::CloseTab(one));
        assert!(a.closing(), "the last tab takes the window with it");
    }

    #[test]
    fn closing_the_current_tab_falls_to_a_neighbour() {
        let mut a = app();
        let one = a.current_tab();
        a.update(Msg::NewTab);
        let two = a.current_tab();
        a.update(Msg::NewTab);
        let three = a.current_tab();

        a.update(Msg::SelectTab(two));
        a.update(Msg::CloseTab(two));
        assert_eq!(a.current_tab(), three, "the tab that took its place");

        a.update(Msg::SelectTab(three));
        a.update(Msg::CloseTab(three));
        assert_eq!(a.current_tab(), one, "the end falls back rather than off");
        assert!(!a.closing());
    }

    #[test]
    fn a_message_naming_a_closed_tab_does_nothing() {
        let mut a = app();
        let one = a.current_tab();
        a.update(Msg::NewTab);
        let two = a.current_tab();
        a.update(Msg::CloseTab(two));
        assert_eq!(a.current_tab(), one);

        a.update(Msg::SelectTab(two));
        assert_eq!(a.current_tab(), one, "selecting a closed tab changes nothing");
        a.update(Msg::CloseTab(two));
        assert!(!a.closing(), "closing a closed tab does not close the window");
        assert_eq!(a.tab_labels().len(), 1);
    }

    /// The grid is outside the diffed tree, so a switch reports no damage and the application must
    /// declare it.
    #[test]
    fn switching_tabs_repaints_the_grid() {
        let mut a = app();
        let one = a.current_tab();
        a.feed(b"first");
        let _ = a.damage_rows(); // the frame that drew tab one

        a.update(Msg::NewTab);
        a.feed(b"second");
        let _ = a.damage_rows(); // …and the frame that drew tab two

        a.update(Msg::SelectTab(one));
        assert_eq!(a.damage_rows().len(), a.grid().rows(), "switching tabs left the other tab's pixels on screen");
    }

    #[test]
    fn closing_the_current_tab_repaints_the_survivor() {
        let mut a = app();
        a.feed(b"first");
        let _ = a.damage_rows();
        a.update(Msg::NewTab);
        let two = a.current_tab();
        a.feed(b"second");
        let _ = a.damage_rows();

        a.update(Msg::CloseTab(two));
        assert_eq!(a.damage_rows().len(), a.grid().rows(), "the survivor was not repainted");
    }

    /// A background tab left at the old shape is drawn from stale numbers; shrinking is worse, and
    /// `open_tab` takes its shape from the current tab.
    #[test]
    fn a_resize_reshapes_every_tab() {
        let mut a = app();
        let one = a.current_tab();
        a.update(Msg::NewTab);
        let before = (a.grid().cols(), a.grid().rows());

        let bigger = Size::new(a.window_size().w + 200, a.window_size().h + 200);
        assert!(a.resize(bigger).is_some(), "a new size is a change");
        let now = (a.grid().cols(), a.grid().rows());
        assert_ne!(now, before, "the current tab grew, or this test proves nothing");

        a.update(Msg::SelectTab(one));
        assert_eq!(
            (a.grid().cols(), a.grid().rows()),
            now,
            "a background tab kept the old shape and would be drawn from stale numbers"
        );
        a.update(Msg::NewTab);
        assert_eq!((a.grid().cols(), a.grid().rows()), now);
    }

    /// The cap is `MAX_WAIT_HANDLES - 1`: past it the kernel rejects the wait and the render loop
    /// spins. Holding `Ctrl+Shift+T` reaches it.
    #[test]
    fn a_window_holds_at_most_max_tabs() {
        let mut a = app();
        for _ in 0..MAX_TABS * 2 {
            a.update(Msg::NewTab);
        }
        assert_eq!(a.tabs().len(), MAX_TABS, "the cap did not hold");
        const { assert!(MAX_TABS < libkern::abi::MAX_WAIT_HANDLES, "…and it leaves room for the window") };
        assert!((0..a.grid().rows()).any(|r| line(&a, r).contains("no more tabs")), "the refusal was silent");
    }

    #[test]
    fn every_menu_in_the_bar_has_somewhere_to_hang_from() {
        let mut a = app();
        let cell = FixedCell { w: 8, h: 16 };
        let size = a.window_size();
        let view = a.view(&UiTheme::default(), None);
        let l = layout(&view, Rect::new(0, 0, size.w, size.h), &cell);
        let n = a.menu_count();
        assert!(n >= 2, "a bar with fewer than two menus is not this window's");
        a.place_menus(&view, &l);
        for i in 0..n {
            a.menus.toggle(i);
            assert_eq!(a.menus.open(), Some(i));
            assert!(
                a.menus.anchor().is_some(),
                "menu {i} of {n} opened with nowhere to hang from, so nothing would be drawn"
            );
            a.menus.close();
        }
    }
}
