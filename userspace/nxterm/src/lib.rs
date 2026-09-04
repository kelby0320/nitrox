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
use libkern::abi::BTN_LEFT;
use librsproto::surface::{
    KEY_DOWN, KEY_REPEAT, KeyEvent, POINTER_BUTTON, POINTER_MOTION,
    POINTER_PRESSED, PointerEvent, RESIZE_BOTTOM, RESIZE_RIGHT, WINDOW_STATE_MAXIMIZED,
    WINDOW_STATE_MINIMIZED, WINDOW_STATE_NORMAL,
};
use libui::element::{Edge, Element, Insets, custom, dock, docked, offset, padding, sized, stack};
use libui::widget::{
    GRIP_W, TITLE_BAR_H, TitleButtons, WINDOW_CONTENT_X, WINDOW_CONTENT_Y, resize_grip,
    title_bar, window_frame,
};
use libui::menu::{Accel, Item, Menu, MenuState};
use libui::widget::{Theme as UiTheme, ScrollState, scrollbar};

/// The `custom` node the grid is drawn into.
pub const GRID_KIND: u32 = 0x4772_6964;

/// Where the menu bar's words are keyed from: `MENU_BAR_KEY + i` for menu `i`.
///
/// [`libui::layout::locate`] turns each into the rectangle its popup hangs under. **Ten rather
/// than one** so the range stays clear of [`GRID_KEY`], which shares this window.
pub const MENU_BAR_KEY: u64 = 10;

/// Where the open popup's rows are keyed from: `MENU_ROW_KEY + i` for item `i`.
///
/// **Keys are what make hover possible**: `Router::inside` reports the id of the keyed widget
/// under the pointer, so an unkeyed item is one the router cannot name (M11 Part E batch 3).
/// A different range from the bar's because a row and a word are different widgets, even though
/// they live in different windows and could not collide.
pub const MENU_ROW_KEY: u64 = 100;

/// The menu the harness opens with F1, and the one whose rows `check-terminal` clicks.
///
/// Named rather than spelled `1` at the two sites that need it: the gate's expectations are
/// written against *this* menu's first row, and a reordering that moved it would otherwise
/// break the gate somewhere that never mentions menus.
pub const HARNESS_MENU: usize = 1;

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
/// The key on the title bar — see [`BAR_KEY`].
pub const TITLE_KEY: u64 = 5;

/// The resize grip's key.
///
/// Read by `the_grip_does_not_cover_any_part_of_the_scrollbar` and
/// `pressing_the_corner_produces_the_resize_and_not_a_move`, which find it with
/// [`locate`](libui::layout::locate) rather than by walking the tree — the same reason the
/// scrollbar and the menu item carry keys.
pub const GRIP_KEY: u64 = 6;

/// Height of the menu bar, in pixels.
pub const BAR_H: u32 = 24;

/// The window's title, and what the bar and the shell's window list both show.
pub const TITLE: &str = "nxterm";

/// Width of the scrollbar, in pixels.
pub const SCROLL_W: u32 = 12;

/// What the chrome costs the grid horizontally: the scrollbar, and the window's frame.
///
/// **One pair of constants, because three places compute this** — `resize` fits the cells,
/// `grid_origin` places them, and `track_h` sizes the scrollbar against the same content box.
/// They were three open-coded sums of `BAR_H + TITLE_BAR_H`, which agreed only because nothing
/// had ever been added between the window's edge and its content. M11 Part E batch 2b added
/// something (PR #265).
const CHROME_W: u32 = SCROLL_W + libui::widget::WINDOW_FRAME_W;

/// And vertically: the title bar, the menu bar, and the frame.
const CHROME_H: u32 = BAR_H + TITLE_BAR_H + libui::widget::WINDOW_FRAME_H;

/// The key that copies the selection, with **Ctrl and Shift** held: `c`.
///
/// **Shift is not decoration here** — M12 decision 6. `Ctrl+C` means *interrupt* in a terminal
/// and always will; it is the one binding this system cannot take for copy, and the reason
/// every real terminal emulator spells the pair with Shift. A literal for the reason
/// `nxedit::SAVE_KEYCODE` is one, and pinned against `libinput`'s table by a test rather than
/// by this comment.
pub const COPY_KEYCODE: u16 = 46;
/// The key that pastes: `v`, with Ctrl and Shift — see [`COPY_KEYCODE`].
pub const PASTE_KEYCODE: u16 = 47;

/// What the chrome can ask the terminal to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Msg {
    /// A word on the menu bar was clicked: open that menu, or close it if it was the open one.
    MenuBar(usize),
    /// Copy the selection to the clipboard — `Ctrl+Shift+C`, or Edit ▸ Copy.
    ///
    /// **A message rather than a branch inside `key`**, since M14 Part A: the menu declares the
    /// chord and [`libui::menu::accel_match`] routes it, so the label and the binding are one
    /// statement. Before, they were two and could drift apart silently.
    Copy,
    /// Fetch the newest clipboard entry and type it at the shell — `Ctrl+Shift+V`, or Edit ▸
    /// Paste. See [`Copy`](Msg::Copy) for why it is a message.
    Paste,
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
    /// The title bar was pressed somewhere that moves the window.
    ///
    /// The answer is one request and no arithmetic: the compositor already holds the grab this
    /// press opened and knows where the window is, and this terminal knows neither.
    DragWindow,
    /// The window should close — its own close button, or the shell asking.
    ///
    /// **One message for both**, because they mean the same thing to this terminal: somebody
    /// wants it gone. Which of them it was is not something it has to act on differently.
    Close,
    /// The resize grip was pressed: the compositor is owed a `StartResize`.
    ///
    /// **Carries the edges** for the same reason [`RequestState`](Msg::RequestState) carries the
    /// state: which corner a grip is, is the *grip's* business, and this crate would only be
    /// deciding it a second time.
    ResizeWindow(u32),
    /// A title-bar button asking the manager for a window state.
    ///
    /// **Carries the state rather than being three messages**, because the terminal does nothing
    /// with it but pass it on: which of them a button means is the *bar's* business, and this
    /// crate would only be re-deciding it.
    RequestState(u32),
    /// The pointer did something over the grid — a selection gesture, or motion that is not one.
    ///
    /// Raw for [`Scroll`](Msg::Scroll)'s reason: most of what arrives here is not a selection,
    /// because the router delivers motion whether or not a button is held, and a grid that
    /// selected on hover would be unusable.
    GridPointer(PointerEvent),
}

/// What the terminal wants done with the clipboard, which only `main` can do.
///
/// **An outbox, like every other syscall this crate does not make.** `update` is a function of
/// values; talking to `/dev/clipboard` is IPC, so the app records what it wants and the binary
/// that owns the namespace performs it — the same shape `nxedit`'s `take_save` has.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ClipRequest {
    /// Push this text onto the ring.
    Copy(alloc::string::String),
    /// Fetch the newest entry and type it at the shell.
    ///
    /// **There is no cycle here, and that is a decision rather than an omission.** M12 decision
    /// 3 makes cycling a *continuation* of a paste that **replaces what was just inserted** — and
    /// a terminal cannot: a paste is bytes already sent down the pty to a program that has read
    /// them. Taking them back would mean sending backspaces to something that may not be a line
    /// editor at all. So cycling lives in the editor, where the buffer is the client's own, and
    /// `Ctrl+Shift+V` here always pastes the newest.
    Paste,
}

/// What a [`resize`](App::resize) did, beyond changing the size.
///
/// **Only the eviction, because only the eviction is not derivable.** Everything else about the
/// new shape is readable from the grid afterwards; how many lines the bounded scrollback dropped
/// on the way is not, and it is the difference between "the rewrap lost lines" and "the history
/// was already full" — see [`libterm::grid::Reflow::evicted_lines`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Resized {
    /// Logical lines the ring dropped during the rewrap.
    pub evicted: usize,
}

/// How many menus the bar carries. `File` and `Edit`.
pub const MENU_COUNT: usize = 2;

/// Everything the terminal is.
pub struct App {
    /// The screen, the cursor, and the scrollback.
    pub grid: Grid,
    /// Bytes to grid operations. Held across writes because a sequence can be split across
    /// them — which is the ordinary case once a real backend is delivering in chunks.
    parser: Parser,
    /// Which menu is open, where each bar word sits, and where the keyboard is inside it.
    ///
    /// **The toolkit's, since M14 Part A.** This was a `bool` and a `Rect` here, a two-element
    /// array and an enum in `nxfiles`, and the same open/close/anchor/dismiss logic in both.
    pub menus: MenuState,
    /// Whether this window holds the keyboard, which the title bar shows.
    ///
    /// **The compositor's answer, not a guess.** `FocusEvent` says so on every change; a title
    /// bar that inferred focus from the last click would disagree with the compositor the first
    /// time focus moved by a chord.
    ///
    /// **Starts `true`, matching `libui::Router`'s own `window_focused`** — which starts that
    /// way deliberately, because starting `false` makes a client's first paint dim. Two pieces
    /// of the same state disagreeing for one frame is worse than either answer (PR #248 review,
    /// finding 7).
    pub focused: bool,
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
    /// The terminal has been asked to close, and the binary owes an exit.
    ///
    /// **A flag rather than an `exit` here.** `update` is a function of values — it has no
    /// syscalls and no way to tear down a session — and a terminal that vanished mid-frame would
    /// leave its buffers mapped and its window undestroyed.
    closing: bool,
    /// A title-bar button was pressed, and the binary owes the compositor a `RequestState`.
    ///
    /// **The last one wins, and there is only ever one.** The buttons are mutually exclusive
    /// answers to "what should this window be", so a frame that saw two presses has had the
    /// second one supersede the first.
    state_requested: Option<u32>,
    /// The grip was pressed, and the binary owes the compositor a `StartResize` for these edges.
    ///
    /// **An outbox of one**, like [`move_requested`](Self::move_requested): `update` is a
    /// function of values and "send a request on a channel" is a syscall.
    resize_requested: Option<u32>,
    /// The title bar was dragged, and the binary owes the compositor a `StartMove`.
    ///
    /// **An outbox of one**, for the reason `outbox` is one: `update` is a function of values
    /// and "send a request on a channel" is a syscall. The application says what happened; the
    /// `main` that owns the session performs it.
    move_requested: bool,
    /// Where the viewport is, or `None` to follow the output.
    ///
    /// **`None` is not "line zero".** Following the bottom and being anchored at whatever the
    /// bottom currently is are different states: the first stays at the bottom as output
    /// arrives, the second is exactly what the user asked for when they scrolled away from it.
    /// Collapsing them is how a terminal ends up scrolling itself back to where you were
    /// reading half a second ago.
    view_top: Option<u64>,
    /// This window last asked to be maximised, so its maximise button now asks for normal.
    ///
    /// See [`Msg::RequestState`]'s arm in [`update`](App::update) for why it is what was
    /// *asked* rather than what the window is.
    maximized: bool,
    /// The window's size in pixels — what the client commits, not what the grid needs.
    ///
    /// **These are not the same number once a manager is involved** (M9 Part D). A maximised
    /// terminal is exactly the work area; the grid inside it is the largest whole number of
    /// cells that fits, and the few pixels left over are background. Deriving the window from
    /// the grid instead — which is what [`window_size`](App::window_size) did until Part D, and
    /// still does at startup — would make a maximised window a cell smaller than it was asked
    /// for in each axis, so "the window is the work area" would be false by a rounding error
    /// and the shell's own geometry log would say so.
    window: Size,
    /// What the terminal owes the clipboard — see [`ClipRequest`].
    clip_request: Option<ClipRequest>,
    /// The whole viewport repaints — it moved, or the selection changed.
    ///
    /// **Not [`Grid::damage_all`], which was the first attempt** and is wrong for a reason
    /// worth keeping: the grid's dirty flags name *screen* rows, and a view scrolled far
    /// enough back holds none of them. Scrolling to the top of the history therefore damaged
    /// every row the grid could name and not one row the user could see — the screen stayed on
    /// the old page under a thumb that had moved. The two damage spaces meet in
    /// [`damage_rows`](App::damage_rows) and nowhere else, so this is the half that lives here.
    ///
    /// **A selection change is the second cause, since M12 Part E**, and it is the *same*
    /// mistake seen from a new direction: `Grid::select_from` damages every screen row, which
    /// for a view scrolled back into the history is every row but the ones being looked at. A
    /// drag in the scrollback would have highlighted nothing.
    view_moved: bool,
}

impl App {
    /// A terminal of `cols` × `rows` cells, drawn with `metrics`.
    pub fn new(cols: usize, rows: usize, metrics: Metrics) -> App {
        let g = metrics.pixel_size(cols, rows);
        App {
            // The same two sums `resize` subtracts, so the window this opens at is a window
            // whose grid is exactly `cols` x `rows` — and stays so after the first `Configure`.
            window: Size::new(g.w + CHROME_W, g.h + CHROME_H),
            maximized: false,
            grid: Grid::new(cols, rows),
            parser: Parser::new(),
            menus: MenuState::new(MENU_COUNT),
            focused: true,
            metrics,
            // `libterm`'s ANSI palette — the sixteen colours a program addresses with
            // `ESC[31m` — and deliberately *not* the desktop theme. M11 Part B collapsed
            // `libui`'s `Palette` into the shared `Theme`; this one stays where it is, because
            // it is defined by what programs expect rather than by how this system looks.
            palette: Palette::default(),
            outbox: Vec::new(),
            move_requested: false,
            resize_requested: None,
            state_requested: None,
            closing: false,
            view_top: None,
            view_moved: false,
            clip_request: None,
        }
    }

    /// The size of this terminal's window in pixels.
    ///
    /// At startup it is the grid plus its chrome — the title bar is part of this window like
    /// every other pixel, because client-side decorations mean the chrome is in the buffer.
    /// After a [`resize`](App::resize) it is whatever the manager asked for, and the grid is
    /// what fits inside it.
    pub fn window_size(&self) -> Size {
        self.window
    }

    /// Take the window to `size`, refitting the grid and rewrapping the history.
    ///
    /// **This is the client's answer to `Configure`**, and it accepts rather than declines — the
    /// thing M9 Part D exists to change. The window becomes exactly the size asked for; the grid
    /// becomes the largest whole number of cells that fits inside the chrome, which is at least
    /// one of each so that a hostile or degenerate size cannot produce a grid with no valid
    /// cursor position.
    ///
    /// **The scrolled-back viewport is carried across the rewrap**, because a resize changes how
    /// many lines the history has: a reader who has scrolled up sees the same text after the
    /// resize as before it, not the same line number.
    ///
    /// `None` if nothing changed, so a caller can skip reallocating buffers for a `Configure`
    /// that repeats the size a window already has — which is every `Configure` that follows a
    /// move. `Some` carries what the resize cost the history: see [`Resized`].
    pub fn resize(&mut self, size: Size) -> Option<Resized> {
        if size == self.window {
            return None;
        }
        self.window = size;
        let cols = (size.w.saturating_sub(CHROME_W) / self.metrics.cell_w).max(1) as usize;
        let rows = (size.h.saturating_sub(CHROME_H) / self.metrics.cell_h).max(1) as usize;
        let reflow = self.grid.resize(cols, rows);
        // **Through the map, not around it.** `view_top` is an absolute line number and the
        // rewrap changed how many lines exist above it.
        self.view_top = self.view_top.map(|t| self.grid.clamp_view(reflow.map_line(t)));
        self.view_moved = true;
        Some(Resized { evicted: reflow.evicted_lines() })
    }

    /// Where the grid's top-left sits inside the window.
    pub fn grid_origin(&self) -> libdraw::geom::Point {
        libdraw::geom::Point::new(
            WINDOW_CONTENT_X as i32,
            (WINDOW_CONTENT_Y + TITLE_BAR_H + BAR_H) as i32,
        )
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
        // **Choosing dismisses the menu, whichever row it was** — a menu that stayed open would
        // cover the thing it just acted on. Asked of the table rather than repeated in the arms,
        // because before M14 Part A exactly two of them remembered to do it and the rest were a
        // row away from forgetting. A message that is not a row leaves it alone, which is what
        // keeps `MenuBar` able to open one.
        if self.menu_table().iter().flat_map(|m| m.items.iter()).any(
            |it| matches!(it, Item::Action { msg: m, .. } if *m == msg),
        ) {
            self.menus.close();
        }
        match msg {
            Msg::MenuBar(i) => self.menus.toggle(i),
            // **Nothing selected is not a request.** A copy that pushed an empty entry would move
            // the ring's serial under every client that was mid-cycle, for a gesture that had
            // nothing to copy. The Edit row is disabled in that state too, so this is the second
            // of two guards rather than the only one — the chord reaches here with no menu open.
            Msg::Copy => {
                if let Some(text) = self.grid.selected_text() {
                    self.clip_request = Some(ClipRequest::Copy(text));
                }
            }
            Msg::Paste => self.clip_request = Some(ClipRequest::Paste),
            // The compositor is the one holding the grab this press opened, so all this does is
            // record that the binary owes it a request.
            Msg::DragWindow => self.move_requested = true,
            // Likewise for the corner: the compositor holds the grab, and this records the ask.
            Msg::ResizeWindow(edges) => self.resize_requested = Some(edges),
            // Likewise: minimising and maximising are the manager's, and this records the ask.
            Msg::RequestState(s) => {
                // **The maximise button is a toggle**, and this bit is the whole of what makes
                // it one: a window that asked to be maximised asks to be *normal* next, which
                // is what reaches the shell's restore path. Before M9 Part D the button was
                // one-way because maximising did nothing visible — the client declined the
                // `Configure` — so a window could not be un-maximised and nothing on the system
                // ever sent `WINDOW_STATE_NORMAL`.
                //
                // **What was asked for, not what happened**, like every other half of this
                // exchange: a shell that ignored the request leaves this bit wrong for exactly
                // one click, which the next one corrects.
                if s == WINDOW_STATE_MAXIMIZED || s == WINDOW_STATE_NORMAL {
                    self.maximized = s == WINDOW_STATE_MAXIMIZED;
                }
                self.state_requested = Some(s);
            }
            Msg::Close => self.closing = true,
            Msg::Clear => {
                // Through the parser, so "clear" means exactly what `Ctrl-L` means and there is
                // one implementation of it rather than a menu-shaped second one.
                self.feed(b"\x1b[2J\x1b[H");
            }
            Msg::Reset => {
                let (cols, rows) = (self.grid.cols(), self.grid.rows());
                self.grid = Grid::new(cols, rows);
                self.parser = Parser::new();
                self.view_top = None;
            }
            Msg::Scroll(p) => self.scroll_to(p),
            Msg::Key(k) => self.key(k),
            Msg::GridPointer(p) => self.grid_pointer(p),
        }
    }

    /// What the terminal owes the clipboard, taken exactly once.
    pub fn take_clip_request(&mut self) -> Option<ClipRequest> {
        self.clip_request.take()
    }

    /// Deliver what a paste fetched: type it at the shell.
    ///
    /// **Sent as input, not printed into the grid.** What is on screen is the *program's*
    /// output, and a terminal that drew pasted text itself would show something the shell never
    /// received. Sending it down the pty is what makes a pasted command a command.
    pub fn pasted(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.send(text.as_bytes());
    }

    /// A pointer event over the grid: press to anchor, drag to extend, release to end.
    ///
    /// **Only the left button, and only while it is held.** The router delivers motion and
    /// crossings unconditionally — `scroll_to` makes the same check for the same reason — so a
    /// grid that extended on every motion would select whatever the pointer passed over on its
    /// way to the scrollbar.
    fn grid_pointer(&mut self, p: PointerEvent) {
        let Some((line, col)) = self.cell_at(p.x, p.y) else { return };
        if p.kind == POINTER_BUTTON && p.button == BTN_LEFT {
            if p.flags & POINTER_PRESSED != 0 {
                // A press with no drag selects nothing and *clears* what was selected — which
                // is what a click anywhere else in the window means too.
                self.grid.select_from(line, col);
                // **The viewport, not the screen** — see [`view_moved`](Self::view_moved).
                self.view_moved = true;
            }
            return;
        }
        if p.kind == POINTER_MOTION && p.buttons & 1 != 0 {
            self.grid.extend(line, col);
            self.view_moved = true;
        }
    }

    /// The absolute `(line, column)` a grid-local pixel falls in, or `None` outside it.
    ///
    /// **Absolute, because a selection is** — see [`libterm::grid::Selection`]. The conversion
    /// is here rather than at the call site for `rows_in`'s reason: two places doing this
    /// arithmetic is two places to disagree about which cell a pixel is in.
    pub fn cell_at(&self, x: i32, y: i32) -> Option<(u64, usize)> {
        if x < 0 || y < 0 || self.metrics.cell_w == 0 || self.metrics.cell_h == 0 {
            return None;
        }
        let col = (x as u32 / self.metrics.cell_w) as usize;
        let row = (y as u32 / self.metrics.cell_h) as usize;
        if row >= self.grid.rows() {
            return None;
        }
        // Clamped rather than refused: a drag that runs off the right edge means "to the end of
        // the line", which is what every text selection does and what a `None` here would turn
        // into a gesture that stops moving.
        Some((self.view_line() + row as u64, col.min(self.grid.cols())))
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
        // **The menu's chords are taken before anything is encoded**, which is the whole reason
        // they carry Shift: `libterm::encode` folds `Ctrl+C` to `0x03`, the interrupt, and a
        // terminal that copied on it would have taken the one binding a terminal cannot give away
        // (M12 decision 6). Checked here rather than in `update` for the reason the two
        // applications settled in Part D: a chord that acts on the *window* is the window's, and
        // typing is what the grid does with everything else.
        //
        // **Matched against the menu table rather than against a `match` on keycodes** (M14
        // decision 2). The pair of constants that used to be tested here are still the source of
        // truth — the table names them — but there is now one statement of "Ctrl+Shift+C copies"
        // instead of a label in the menu and a branch here that could stop agreeing with it.
        if let Some(msg) = libui::menu::accel_match(&self.menu_table(), &k) {
            self.update(msg);
            return;
        }
        // **Typing clears the selection.** It is the same rule the kill ring's cycle follows —
        // any other action ends the gesture — and without it a highlight stays on screen over
        // text that has since scrolled away under it.
        //
        // The return value is the repaint: clearing nothing must not damage the viewport, or
        // every keystroke in a terminal with no selection would repaint every row.
        if self.grid.clear_selection() {
            self.view_moved = true;
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
        // **What the `Dock` will actually leave it**, which since M9 Part D is not the grid's
        // pixel height: a window is whatever size the manager asked for, and the grid is the
        // whole cells that fit inside it. Sizing the thumb against the grid instead would make
        // it a fraction of a cell short in a maximised window, and the error would grow with
        // the leftover.
        //
        // **Less the grip's square, which the bar must stop above** (PR #253 review, finding 4).
        // `GRIP_W` is wider than `SCROLL_W`, the grip is the topmost layer, and hit-testing
        // takes the topmost — so any part of the track under it is a part of the track that
        // cannot be pressed. That is not hypothetical: `MIN_THUMB` is 16 and a following view
        // puts the thumb at the very bottom, so a 24-row terminal with a full scrollback had
        // its thumb *entirely* under the grip.
        self.window.h.saturating_sub(CHROME_H + GRIP_W)
    }

    /// The element tree for the current state.
    ///
    /// **The theme is the caller's**, because the caller paints this tree — and a tree built from
    /// one theme and painted with another is two themes in one frame, which one type makes easy
    /// to write and the old `Theme`/`Palette` split made impossible (PR #262 review, optional 5).
    /// The bar's menus, in bar order.
    ///
    /// **Built rather than stored**, because it is a function of almost nothing: a cached copy
    /// would be a second place for the same constants, and this is cheap. It takes `&self`
    /// because Copy is only offered when there is a selection — the one row here whose
    /// availability is a fact about the terminal rather than about the menu.
    ///
    /// **Clear and Reset are under Edit rather than in a Terminal menu of their own.** Two menus
    /// is what M14 Part A scopes, and of the two they are edits to what is on screen. GNOME
    /// Terminal puts them under `Terminal`; if a third menu arrives they move there, and the
    /// accelerator table does not care which menu an item is in.
    pub fn menu_table(&self) -> Vec<Menu<Msg>> {
        vec![
            Menu {
                title: "File",
                // No Close Tab: the terminal has no tabs until Part B. No Quit: decision 4
                // settles what "quit" means to an application with unsaved work, and settling
                // it here as "the window goes" would be one of two answers in two places.
                items: vec![Item::plain("Close Window", Msg::Close)],
            },
            Menu {
                title: "Edit",
                items: vec![
                    // **Greyed with nothing selected**, which is what `enabled` is for: the
                    // copy path already declines an empty selection, and a row that looks
                    // available but declines is the thing that reads as a broken menu.
                    Item::new("Copy", Accel::ctrl_shift(COPY_KEYCODE, "C"), Msg::Copy)
                        .enabled(self.grid.has_selection()),
                    Item::new("Paste", Accel::ctrl_shift(PASTE_KEYCODE, "V"), Msg::Paste),
                    Item::Separator,
                    Item::plain("Clear", Msg::Clear),
                    Item::plain("Reset", Msg::Reset),
                ],
            },
        ]
    }

    /// It is also the shape Part C needs: a theme read from a file arrives in `main` and is
    /// handed down, rather than being fetched from a default in the middle of a view.
    pub fn view(&self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {

        let grid_px = self.metrics.pixel_size(self.grid.cols(), self.grid.rows());

        // **The bar is `libui::menu`'s, and so is what hangs off it.** It was one hand-rolled
        // `button` labelled "Terminal"; the word a menu bar carries is not a button, and the
        // popup under it was a second copy of `nxfiles`'s.
        let bar = libui::menu::bar(
            &self.menu_table(),
            &self.menus,
            MENU_BAR_KEY,
            hovered,
            Msg::MenuBar,
            &ui,
            BAR_H,
        );
        // **The title bar is the terminal's own chrome** (M9 Part A), and since Part C all three
        // of its buttons do something: minimise and maximise ask the shell, close is this
        // client's own answer.
        let title = title_bar(
            TITLE,
            self.focused,
            Msg::DragWindow,
            TitleButtons {
                minimise: Some(Msg::RequestState(WINDOW_STATE_MINIMIZED)),
                maximise: Some(Msg::RequestState(if self.maximized {
                    WINDOW_STATE_NORMAL
                } else {
                    WINDOW_STATE_MAXIMIZED
                })),
                // **Close sends nothing.** It is the one button whose answer is entirely this
                // client's: it exits, and the compositor tears down its windows with its
                // session. A `Manage::Close` exists for a client that will *not* do this, and it
                // is the shell's to reach for (M9 Part C).
                close: Some(Msg::Close),
            },
            &ui,
        )
        .key(TITLE_KEY);
        let body = window_frame(
            title,
            dock(
                vec![
                docked(Edge::Top, bar.key(BAR_KEY)),
                docked(
                    Edge::Right,
                    // **Sized, so the bar ends where the grip begins.** The dock's right slot
                    // is the full remaining height; without this the scrollbar is laid out
                    // under the grip and the bottom `GRIP_W` of its track is unpressable — see
                    // [`track_h`](Self::track_h).
                    // The key goes on the wrapper, because the diff requires a container's
                    // children to be all keyed or all unkeyed and this one's siblings are keyed.
                    sized(
                        Size::new(SCROLL_W, self.track_h()),
                        scrollbar(self.scroll(), SCROLL_W, self.track_h(), &ui)
                            .on_pointer(Msg::Scroll),
                    )
                    .key(SCROLLBAR_KEY),
                ),
                ],
                custom(GRID_KIND, grid_px)
                    .key(GRID_KEY)
                    .on_key(|k| Some(Msg::Key(k)))
                    // **The grid takes the pointer since M12 Part E.** It had none: nothing in
                    // the terminal reacted to the pointer except the scrollbar and the chrome.
                    .on_pointer(Msg::GridPointer),
            ),
            &ui,
        );

        // **The grip sits over the bottom-right corner, not beside it** (M9 Part E). A strip
        // reserved for it would take a row of cells from every terminal for a control that is
        // only ever aimed at; stacked over the corner it costs nothing and is exactly where a
        // person reaches. `offset` places it, because a `stack` layer otherwise gets the whole
        // area and `sized` alone would put it top-left.
        let grip = offset(
            (self.window.w.saturating_sub(GRIP_W)) as i32,
            (self.window.h.saturating_sub(GRIP_W)) as i32,
            resize_grip(Msg::ResizeWindow(RESIZE_RIGHT | RESIZE_BOTTOM), &ui).key(GRIP_KEY),
        );
        let body = stack(vec![body, grip]);

        // **The menu is not in this tree.** It used to be a `Stack` layer over the whole
        // window — a layer inside the 24-pixel bar would have been clipped to 24 pixels, so it
        // was hoisted to the first ancestor big enough to hold it. Since M6 C3 it is a *window*:
        // a `popup`, parented to this one and clipped only by the screen, which is what a menu
        // has always needed and what `libui`'s `offset` cannot give it. See [`menu_view`].
        //
        // [`menu_view`]: Self::menu_view
        body
    }

    /// The menu popup's own element tree — the root of a second window, not a layer in this one.
    ///
    /// Separate from [`view`](Self::view) because it is painted into a different surface with
    /// its own diff state. It carries no `Fill` sizing of its own beyond the backing, so
    /// `libui::layout::measure` under loose constraints gives the size the popup window should
    /// be created at.
    ///
    /// **The theme is the caller's**, like [`view`](Self::view)'s: the popup is painted by the
    /// binary, and a menu built from one theme beside a window painted with another is the same
    /// two-themes mistake one surface further out.
    pub fn menu_view(&self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        self.menu(ui, hovered)
    }

    /// The popup's contents: whichever menu is open, framed.
    ///
    /// **An empty frame when none is**, which cannot be drawn because the popup window only
    /// exists while one is open — but is a shape the type demands and the caller must not have
    /// to think about.
    fn menu(&self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        let menus = self.menu_table();
        match self.menus.open().and_then(|i| menus.get(i)) {
            Some(m) => libui::menu::popup(m, &self.menus, MENU_ROW_KEY, hovered, ui),
            None => libui::widget::popup_frame(padding(Insets::all(2), libui::element::text("")), ui),
        }
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
    use librsproto::surface::{KEY_UP, MOD_CTRL, MOD_SHIFT};
    use libui::layout::{FixedCell, layout, locate};

    const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSansMono.ttf");

    fn app() -> App {
        let f = Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses");
        App::new(20, 6, Metrics::new(&f, 16.0))
    }

    // --- selection and the clipboard (M12 Part E) ---------------------------

    /// A pointer event over the grid, in grid-local pixels.
    fn ptr(kind: u16, buttons: u16, flags: u16, x: i32, y: i32) -> PointerEvent {
        PointerEvent { kind, button: BTN_LEFT, buttons, flags, x, y, ..Default::default() }
    }

    /// A key press, as the compositor delivers one.
    fn press(a: &mut App, keycode: u16, modifiers: u16) {
        a.update(Msg::Key(KeyEvent::new(1, keycode, KEY_DOWN as u16, modifiers)));
    }

    #[test]
    fn a_drag_over_the_grid_selects_and_a_copy_asks_for_that_text() {
        let mut a = app();
        a.feed(b"hello world");
        let (w, h) = (a.metrics.cell_w as i32, a.metrics.cell_h as i32);
        a.update(Msg::GridPointer(ptr(POINTER_BUTTON, 1, POINTER_PRESSED, 0, h / 2)));
        a.update(Msg::GridPointer(ptr(POINTER_MOTION, 1, 0, 5 * w, h / 2)));
        press(&mut a, COPY_KEYCODE, MOD_CTRL | MOD_SHIFT);
        assert_eq!(
            a.take_clip_request(),
            Some(ClipRequest::Copy(alloc::string::String::from("hello")))
        );
        // …and the chord did not reach the shell as an interrupt.
        assert!(a.take_outbox().is_empty(), "the chord was consumed, not typed");
    }

    #[test]
    fn motion_after_a_release_does_not_go_on_selecting() {
        // The router delivers motion whether or not a button is down, so without
        // `grid_pointer`'s `buttons` check the pointer crossing the grid on its way to the
        // scrollbar would drag the head of a finished selection along with it.
        //
        // **It has to select first.** A version of this that only sent motion passed against the
        // guard *and* against its removal: with no anchor, `Grid::extend` returns early either
        // way, so what it pinned was `libterm`'s own no-anchor guard rather than this one
        // (PR #271 review, blocking 1). A release does not clear the anchor — that is what makes
        // the state after one the discriminating case.
        let mut a = app();
        a.feed(b"hello world");
        let (w, h) = (a.metrics.cell_w as i32, a.metrics.cell_h as i32);
        a.update(Msg::GridPointer(ptr(POINTER_BUTTON, 1, POINTER_PRESSED, 0, h / 2)));
        a.update(Msg::GridPointer(ptr(POINTER_MOTION, 1, 0, 5 * w, h / 2)));
        a.update(Msg::GridPointer(ptr(POINTER_BUTTON, 0, 0, 5 * w, h / 2)));
        assert_eq!(a.grid.selected_text().as_deref(), Some("hello"), "the drag selected");
        a.update(Msg::GridPointer(ptr(POINTER_MOTION, 0, 0, 9 * w, h / 2)));
        assert_eq!(
            a.grid.selected_text().as_deref(),
            Some("hello"),
            "and a pointer merely passing over it afterwards did not extend it"
        );
    }

    #[test]
    fn copying_nothing_is_not_a_request() {
        // A copy that pushed an empty entry would move the ring's serial under every client
        // that was mid-cycle, for a gesture that had nothing to copy.
        let mut a = app();
        a.feed(b"hello");
        press(&mut a, COPY_KEYCODE, MOD_CTRL | MOD_SHIFT);
        assert_eq!(a.take_clip_request(), None);
    }

    #[test]
    fn ctrl_c_without_shift_is_still_the_interrupt() {
        // **The one binding a terminal cannot give away** — M12 decision 6, and the reason the
        // copy chord carries Shift at all. Without the modifier check this chord would copy,
        // and nothing would be able to interrupt a running program.
        let mut a = app();
        a.feed(b"hello");
        a.grid.select_from(0, 0);
        a.grid.extend(0, 5);
        press(&mut a, COPY_KEYCODE, MOD_CTRL);
        assert_eq!(a.take_clip_request(), None, "a plain Ctrl+C is not a copy");
        assert_eq!(a.take_outbox(), alloc::vec![0x03], "it is the interrupt byte");
    }

    #[test]
    fn a_paste_is_typed_at_the_shell_rather_than_drawn() {
        // What is on screen is the *program's* output. A terminal that drew pasted text itself
        // would show something the shell never received.
        let mut a = app();
        a.pasted("ls\n");
        assert_eq!(a.take_outbox(), alloc::vec![b'l', b's', b'\n']);
    }

    #[test]
    fn typing_clears_the_selection() {
        // The same rule the kill ring's cycle follows: any other action ends the gesture.
        // Without it a highlight stays on screen over text that has scrolled away under it.
        let mut a = app();
        a.feed(b"hello");
        a.grid.select_from(0, 0);
        a.grid.extend(0, 5);
        press(&mut a, 30, 0); // `a`
        assert_eq!(a.grid.selection(), None);
    }

    #[test]
    fn a_selection_made_in_the_scrollback_repaints_what_is_being_looked_at() {
        // **The two damage spaces, from a new direction** (M12 Part E). `Grid::select_from`
        // damages every *screen* row; a view scrolled back into the history is showing none of
        // them, and `damage_rows` filters what does not map. So a drag in the scrollback
        // highlighted nothing, exactly as scrolling once painted nothing — the bug
        // `view_moved` was added for.
        let mut a = app();
        for i in 0..40 {
            a.feed(alloc::format!("line {i}\r\n").as_bytes());
        }
        a.scroll_to_line(a.grid.oldest_line());
        let _ = a.damage_rows();
        let h = a.metrics.cell_h as i32;
        a.update(Msg::GridPointer(ptr(POINTER_BUTTON, 1, POINTER_PRESSED, 0, h / 2)));
        let rows = a.damage_rows();
        assert!(
            rows.contains(&0),
            "the top viewport row is scrollback and has to repaint; got {rows:?}"
        );
    }

    #[test]
    fn the_copy_and_paste_keycodes_are_the_letters_they_claim() {
        // Pinned against `libinput`'s table rather than against the constants' own comments —
        // the same check `nxedit`'s chords get.
        assert_eq!(libinput::keymap::to_char(COPY_KEYCODE, 0), Some(b'c'));
        assert_eq!(libinput::keymap::to_char(PASTE_KEYCODE, 0), Some(b'v'));
    }

    #[test]
    fn the_grip_does_not_cover_any_part_of_the_scrollbar() {
        // **A control that cannot be pressed is worse than one that is absent.** The grip is the
        // topmost layer and hit-testing takes the topmost, so any part of the track under it is
        // a part of the track that does nothing — and `MIN_THUMB` is 16 while a following view
        // puts the thumb at the very bottom, so a 24-row terminal with a full scrollback had
        // its thumb *entirely* underneath (PR #253 review, finding 4).
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
        assert!(
            bar.intersect(&grip).is_none(),
            "the grip {grip:?} overlaps the scrollbar {bar:?}"
        );
        assert_eq!(grip.bottom(), bounds.bottom(), "and the grip is still in the corner");
        assert_eq!(grip.right(), bounds.right());
    }

    #[test]
    fn the_grip_asks_the_binary_for_a_resize_and_asks_once() {
        // The same shape as the title bar's drag: `update` has no syscalls, so what a press
        // produces is an outbox of one. Taking it clears it — a second `StartResize` for a
        // gesture the compositor is already running would mean this client believes in a drag
        // it is not in.
        let mut a = app();
        assert_eq!(a.take_resize_request(), None, "nothing owed before the corner is touched");
        a.update(Msg::ResizeWindow(RESIZE_RIGHT | RESIZE_BOTTOM));
        assert_eq!(a.take_resize_request(), Some(RESIZE_RIGHT | RESIZE_BOTTOM));
        assert_eq!(a.take_resize_request(), None, "and owed exactly once");
    }

    #[test]
    fn pressing_the_corner_produces_the_resize_and_not_a_move() {
        // **Through the router, at the grip's own laid-out rectangle.** The message is carried
        // by an `on_press_down` inside a `Stack` layer over the window's body; a press there
        // must reach the grip rather than the title bar's drag or the grid's keys.
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
    fn the_maximise_button_alternates_between_maximised_and_normal() {
        // **The only thing that ever sends `WINDOW_STATE_NORMAL`.** The shell has had a restore
        // path since M9 Part B and nothing could reach it: the button was one-way, which was
        // invisible while maximising did nothing and is a window you cannot get back the moment
        // it does (M9 Part D).
        let mut a = app();
        a.update(Msg::RequestState(WINDOW_STATE_MAXIMIZED));
        assert_eq!(a.take_state_request(), Some(WINDOW_STATE_MAXIMIZED));

        // The *button* now carries the other message — clicked where a person clicks it, so
        // this also says the toggle is on the bar rather than only in the state.
        click_maximise(&mut a);
        assert_eq!(a.take_state_request(), Some(WINDOW_STATE_NORMAL));
        click_maximise(&mut a);
        assert_eq!(a.take_state_request(), Some(WINDOW_STATE_MAXIMIZED), "and back again");
    }

    #[test]
    fn minimising_does_not_disturb_which_way_the_maximise_button_points() {
        // Three buttons, one message type: an arm that took every `RequestState` as an answer
        // about maximisation would flip on a minimise, and the maximise button would then
        // restore a window that had never been maximised.
        let mut a = app();
        a.update(Msg::RequestState(WINDOW_STATE_MAXIMIZED));
        a.update(Msg::RequestState(WINDOW_STATE_MINIMIZED));
        let _ = a.take_state_request();
        click_maximise(&mut a);
        assert_eq!(a.take_state_request(), Some(WINDOW_STATE_NORMAL));
    }

    #[test]
    fn a_configure_takes_the_window_exactly_and_fits_the_grid_inside_it() {
        // **The window is the size the manager asked for**, to the pixel — that is what makes
        // "the maximised terminal is the work area" a true statement rather than one off by a
        // rounding error in each axis. The grid is the whole cells that fit inside the chrome,
        // and what is left over is background.
        let mut a = app();
        let m = a.metrics;
        let want = Size::new(1280, 752);
        assert!(a.resize(want).is_some(), "a new size is a change");
        assert_eq!(a.window_size(), want, "committed at exactly what was asked for");
        assert_eq!(a.grid.cols(), ((1280 - SCROLL_W) / m.cell_w) as usize);
        assert_eq!(a.grid.rows(), ((752 - BAR_H - TITLE_BAR_H) / m.cell_h) as usize);
        // And the cells really do fit: chrome plus grid is no larger than the window.
        let g = m.pixel_size(a.grid.cols(), a.grid.rows());
        assert!(g.w + SCROLL_W <= want.w && g.h + BAR_H + TITLE_BAR_H <= want.h);
    }

    #[test]
    fn a_configure_repeating_the_current_size_is_not_a_resize() {
        // A `Configure` follows every *move* as well as every resize, carrying the origin. A
        // client that reallocated its buffers and rewrapped its history for each one would do
        // both per pointer motion of a drag.
        let mut a = app();
        let want = a.window_size();
        assert!(a.resize(want).is_none(), "same size, nothing to do");
    }

    #[test]
    fn a_degenerate_configure_still_leaves_a_usable_grid() {
        // Smaller than the chrome. A grid of zero columns has no valid cursor position, so the
        // floor is one of each rather than a refusal — the compositor composites whatever it is
        // given, and a client that panicked here would be one a manager could crash.
        let mut a = app();
        assert!(a.resize(Size::new(1, 1)).is_some());
        assert_eq!((a.grid.cols(), a.grid.rows()), (1, 1));
        assert_eq!(a.window_size(), Size::new(1, 1));
    }

    #[test]
    fn a_scrolled_back_view_shows_the_same_text_after_a_resize() {
        // The anchor is an absolute line number and a rewrap changes how many lines there are.
        // This is `Grid::resize`'s `Reflow` reaching the one place outside the grid that holds
        // such a number.
        let mut a = app();
        for i in 0..30 {
            a.feed(alloc::format!("line{i} and some more text here\r\n").as_bytes());
        }
        // **An even line, because each of those 29-character lines is two rows at 20 columns**
        // and nothing has been evicted: line 4 is where "line2" starts. Anchoring on a
        // *continuation* row would make the assertion below wrong rather than the code — the
        // row that then holds that text is the rejoined line, and the text is in its middle.
        a.scroll_to_line(a.grid.oldest_line() + 4);
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
        assert_eq!(a.menus.open(), None);
        a.update(Msg::MenuBar(1));
        assert_eq!(a.menus.open(), Some(1));
        a.update(Msg::MenuBar(1));
        assert_eq!(a.menus.open(), None, "the same word again closes it");
        // And a *different* word moves the menu rather than closing it, which is the half a
        // one-menu bar could not have.
        a.update(Msg::MenuBar(0));
        a.update(Msg::MenuBar(1));
        assert_eq!(a.menus.open(), Some(1));
    }

    #[test]
    fn choosing_an_item_closes_the_menu() {
        // A menu that stayed open after its item was chosen would cover the thing it just
        // acted on. **Every row, not the two that used to remember**: the rule is asked of the
        // table now, so this walks the table.
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
            // Close would end the process in a real run; here it only sets a flag.
            let mut a = app();
            a.update(Msg::MenuBar(1));
            a.update(msg);
            assert_eq!(a.menus.open(), None, "{msg:?} left the menu open");
        }
        // **The negative control**: the bar's own word is not a row, so it must *not* be caught
        // by the rule above — otherwise no menu could ever be opened.
        let mut a = app();
        a.update(Msg::MenuBar(0));
        assert_eq!(a.menus.open(), Some(0));
    }

    /// Arrowing sideways changes both where the popup hangs and how big it must be.
    ///
    /// **Why this is a test rather than an observation** (PR #280 review, blocking 2): the binary
    /// used to rebuild the popup window on `open().is_some()`, so moving from File to Edit left
    /// the window at File's word and at File's size. `Child::present` lays the new tree into a
    /// rectangle fixed when the window was created, so Edit's five rows were drawn into a
    /// one-row window and four of them were clipped away. Nothing in the binary is host-testable;
    /// what is, is that the two menus genuinely differ in both respects — which is what makes
    /// rebuilding on a *change* necessary rather than tidy.
    #[test]
    fn the_two_menus_hang_from_different_words_and_measure_differently() {
        let mut a = app();
        let cell = FixedCell { w: 8, h: 16 };
        let bounds = Rect::new(0, 0, a.window_size().w, a.window_size().h);
        let view = a.view(&UiTheme::default(), None);
        let l = layout(&view, bounds, &cell);
        a.menus.set_anchors(
            (0..MENU_COUNT).map(|i| locate(&view, &l, MENU_BAR_KEY + i as u64)).collect(),
        );

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
        assert!(
            edit_size.h > file_size.h,
            "Edit's five rows are taller than File's one — {edit_size:?} vs {file_size:?}"
        );
    }

    /// The chord a menu row advertises does what choosing that row does.
    ///
    /// **This drives `App::key`**, which is the point and is what the first version did not do
    /// (PR #280 review, worth fixing 4). It built an event from a row's `Accel` and asked
    /// `accel_match` about it — both sides of one table, with the terminal never consulted. That
    /// pins the table against two rows on one chord, which is asserted below; it does not pin
    /// that anything *routes* through it, and it passed with `App::key`'s `accel_match` deleted.
    ///
    /// **Stated as an equivalence rather than per row**, so it does not become a second list of
    /// what each action does — which is the drift decision 2 exists to prevent.
    #[test]
    fn every_advertised_chord_does_what_its_row_says() {
        /// Everything about a terminal that any of these rows can move.
        fn digest(a: &mut App) -> alloc::string::String {
            let clip = a.take_clip_request();
            alloc::format!(
                "{clip:?}|{}|{}|{}|{:?}",
                line(a, 0),
                a.closing(),
                a.grid.has_selection(),
                a.menus.open(),
            )
        }
        /// A terminal with text on screen and five characters of it swept out, so Copy is live.
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
            let Item::Action { accel: Some(acc), msg, label, enabled: true } = it else { continue };
            checked += 1;
            let ev = KeyEvent::new(1, acc.key(), KEY_DOWN as u16, acc.mods());
            assert_eq!(
                libui::menu::accel_match(&table, &ev).as_ref(),
                Some(msg),
                "{label} advertises {} and the table hands it to another row",
                acc.label()
            );
            let (mut by_row, mut by_chord) = (selected(), selected());
            by_row.update(*msg);
            by_chord.update(Msg::Key(ev));
            // Each digest exactly once: it drains the clipboard outbox, which is the only way to
            // see it, so a second call on the same terminal reports its own emptiness.
            let (by_row, by_chord, untouched) =
                (digest(&mut by_row), digest(&mut by_chord), digest(&mut selected()));
            assert_eq!(
                by_chord, by_row,
                "{label}: {} does not do what choosing the row does",
                acc.label()
            );
            // **The negative control**: the two agreeing is only evidence if the row moved
            // something. A terminal that ignored both would pass the assertion above.
            assert_ne!(
                by_row, untouched,
                "{label} changes nothing, so this row proves nothing about routing"
            );
        }
        assert_eq!(checked, 2, "Copy and Paste are the rows with chords");
    }
    /// The menu is **never** in the window's tree, open or closed.
    ///
    /// It was a `Stack` layer over the whole window until M6 C3 — hoisted there because a layer
    /// inside the 24-pixel bar would have been clipped to 24 pixels. It is a `popup` *window*
    /// now, parented to this one and clipped only by the screen, so the window's tree is the
    /// body and nothing else.
    #[test]
    fn the_menu_is_not_a_layer_in_the_windows_tree() {
        for (open, anchor) in [
            (false, None),
            (true, Some(Rect::new(4, 0, 60, BAR_H))),
            (true, None),
        ] {
            let mut a = app();
            if open {
                a.menus.toggle(0);
            }
            a.menus.set_anchors(vec![anchor, None]);
            // **The same shape whether the menu is open or not**, which is the property: the
            // menu is a *window*, so opening it adds nothing here. Since M9 Part E the root is
            // a stack of two — the body and the resize grip over its corner — rather than the
            // dock alone, and that is still fixed.
            let libui::element::Node::Stack(layers) = &a.view(&UiTheme::default(), None).node else {
                panic!("open={open}: the window's tree is the body under its grip");
            };
            assert_eq!(layers.len(), 2, "open={open}: the body and the grip, and nothing else");
            assert!(
                matches!(layers[0].node, libui::element::Node::Stack { .. }),
                "open={open}: the body is the framed window — a border, a face and the dock \
                 inside them, since M11 Part E batch 2b"
            );
        }
    }

    /// The anchor sits directly under the menu item, and is what the popup is placed at.
    ///
    /// `locate` finds the item; the anchor it yields becomes the popup window's offset from its
    /// parent's origin (`CreateWindowRequest::at`). The value is the same one the old in-window
    /// `offset` consumed — what changed is who reads it.
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
        // The anchor the popup is created at is the word's bottom-left, which is what
        // `MenuState` computes now — and the assertion below is about the *word*, so this
        // checks the two agree rather than restating one of them.
        assert_eq!(a.menus.anchor(), Some((item.origin.x, item.bottom() as i32)));

        // The popup hangs from the item's left edge, immediately below it. The menu bar sits
        // *under* the title bar since M9 Part A, so "inside the bar" is measured from there —
        // and inside the window's frame since M11 Part E batch 2b, which moves both bars down by
        // the top border. Written as the sum rather than a number, so the next thing added to
        // the chrome moves this with it.
        let bar_top = (WINDOW_CONTENT_Y + TITLE_BAR_H) as i32;
        assert!(item.origin.y >= bar_top, "the item is below the title bar");
        assert!(
            item.bottom() <= (bar_top as u32 + BAR_H) as i64,
            "the item is inside the menu bar"
        );

        // And the menu it will hold has a real size to be created at — measured, not guessed,
        // because a popup window needs its extent before it exists.
        let menu = a.menu_view(&UiTheme::default(), None);
        let size = libui::layout::measure(
            &menu,
            libui::layout::Constraints::loose(bounds.size),
            &cell,
        );
        assert!(size.w > 0 && size.h > 0, "the menu measures to something");
        assert!(
            item.bottom() + size.h as i64 > BAR_H as i64,
            "and it extends past the bar it hangs from — which only a window can do"
        );
    }

    #[test]
    fn the_window_is_the_grid_plus_its_chrome() {
        // **Including the title bar, which is the whole of what client-side decorations means
        // here**: the window grew by exactly its height, and the grid did not shrink. A client
        // that added a title bar by taking the space out of its own content would have chrome
        // that costs the user a row of text.
        let a = app();
        let g = a.metrics.pixel_size(20, 6);
        assert_eq!(a.window_size(), Size::new(g.w + CHROME_W, g.h + CHROME_H));
        // **And the grid starts inside the frame**, not at the window's edge: the origin is what
        // maps a pointer to a cell, so a frame the origin did not know about would offset every
        // click by four pixels — invisible until a click near a cell boundary lands one column
        // over (M11 Part E batch 2b).
        assert_eq!(
            a.grid_origin(),
            libdraw::geom::Point::new(
                WINDOW_CONTENT_X as i32,
                (WINDOW_CONTENT_Y + TITLE_BAR_H + BAR_H) as i32
            ),
            "the grid starts below the bars and inside the frame"
        );
        // The window is the grid plus chrome, and the chrome is *all* of it — a test that added
        // up only the parts it remembered would pass for a frame that took space from the grid.
        assert_eq!(
            a.window_size().h - a.grid_origin().y as u32 - g.h,
            libui::widget::WINDOW_BORDER + libui::widget::WINDOW_FRAME,
            "what is left below the grid is the frame and the border, and nothing else"
        );
    }

    #[test]
    fn dragging_the_title_bar_asks_the_binary_for_a_move_and_asks_once() {
        // `update` has no syscalls, so the request it produces is an outbox of one — the same
        // shape as the typed bytes. Taking it clears it: a second `StartMove` for a gesture the
        // compositor is already running would be answered, and would mean this client believes
        // a drag it is not in.
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
    ///
    /// The window id is arbitrary: by the time a record reaches the application, `libsurface`
    /// has already established that it belongs to this window.
    fn key_ev(code: u16, pressed: u16) -> KeyEvent {
        KeyEvent::new(1, code, pressed, 0)
    }

    /// The tree, layout and router of a live window, with the grid focused as `main` does it.
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

    /// Press and release the maximise button, at the coordinates a person would hit.
    ///
    /// **Through the router**, because the message the button carries is the thing under test:
    /// reading the state instead would pass for a toggle that never reached the bar. The
    /// buttons are laid out from the right edge — close, maximise, minimise — each
    /// `TITLE_BUTTON_W` wide, so the middle of the maximise button is a slot and a half in.
    fn click_maximise(a: &mut App) {
        use libui::widget::TITLE_BUTTON_W;
        let (t, l, mut r) = window(a);
        let e = a.view(&UiTheme::default(), None);
        let x = a.window_size().w as i32 - (TITLE_BUTTON_W as i32 + TITLE_BUTTON_W as i32 / 2);
        let y = TITLE_BAR_H as i32 / 2;
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

    #[test]
    fn a_key_repeat_reaches_the_grid_through_the_router_and_types() {
        // **B4.** Repeat is generated compositor-side and already had a consumer that *prints*
        // it; what is new is a repeat reaching a widget through `libui::route` and being
        // encoded exactly like the press it continues. Held-down keys are how anyone deletes a
        // line, so this is the difference between a demo and a terminal.
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
        let e = a.view(&UiTheme::default(), None);
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
        let e = a.view(&UiTheme::default(), None);
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
    /// The window's own tree survives being diffed frame to frame.
    ///
    /// **The class of bug this catches cost a QEMU round trip** (M14 Part A): `diff` rejects a
    /// parent whose children are *partly* keyed, and adding one unkeyed sibling to a keyed row
    /// makes the whole window undiffable — so nothing draws at all, in a way no layout or paint
    /// test can see. The states below are the ones a menu bar introduces: shut, open, and a word
    /// under the pointer, each of which changes a row's shape.
    #[test]
    fn the_window_tree_diffs_across_the_menu_states() {
        let mut a = app();
        let theme = UiTheme::default();
        let mut tree = libui::diff::Tree::new();
        let cell = libui::layout::FixedCell { w: 8, h: 16 };
        let mut check = |a: &mut App, hovered, tree: &mut libui::diff::Tree, what: &str| {
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

}
