//! `nxedit` — the text editor, minus the syscalls: a function of values, host-tested in
//! milliseconds. Reading the file, writing it and pumping events are the binary's.
//!
//! **A save that fails keeps the buffer.** [`App::saved`] turns a failure into a *message*,
//! never a state change: the buffer stays modified and the text where it was. The
//! write-and-rename that makes the disk half safe is `main`'s.
//!
//! **A buffer that could not be read refuses to be written**: an empty window saved over an
//! unreadable file destroys it. [`App::blocked`] is what a load failure sets. A *missing* file
//! is not a failure; it is how a file gets made.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

pub mod syntax;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use alloc::vec;
use libdraw::geom::{Rect, Size};
use librsproto::surface::{
    KEY_DOWN, KEY_REPEAT, KeyEvent, MOD_CTRL, MOD_SHIFT, PointerEvent, RESIZE_BOTTOM, RESIZE_RIGHT,
    WINDOW_STATE_MAXIMIZED, WINDOW_STATE_MINIMIZED, WINDOW_STATE_NORMAL,
};
use libui::chooser::{self, ChooserState};
use libui::element::{
    Edge, Element, Insets, center_v, column, dock, docked, fill, mono, padding, row, sized, stack, text, with_spacing,
};
use libui::menu::{Accel, Item, Menu, MenuState};
use libui::widget::{
    InkRun, PILL_H, STATUS_GAP, TAB_STRIP_H, TITLE_BAR_H, TabExtras, TextAreaState, TextFieldState, Theme as UiTheme,
    TitleButtons, WINDOW_FRAME_H, WidgetState, button, dialog_frame, pill, resize_grip, scrollbar, status_bar,
    status_separator, status_text, tab_strip, text_area, text_field, title_bar, window_frame_with_grip,
};

/// The status bar's height in pixels: `libui`'s `status_bar`, along the window's foot.
pub const STATUS_H: u32 = libui::widget::STATUS_BAR_H;

/// What kind of window this is, beside the file's name: a title and a dim subtitle.
pub const EDITOR_KIND: &str = "Text Editor";

/// The element key on the strip's line-and-column readout.
pub const POSITION_KEY: u64 = 16;

/// A text line's height in pixels, matching the toolkit's default theme.
pub const ROW_H: u32 = 20;

/// The element key on the text area.
pub const AREA_KEY: u64 = 1;

/// The element key on the byte count in the tab strip, and on the gap after it.
pub const BYTES_KEY: u64 = 26;
/// See [`BYTES_KEY`].
pub const BYTES_GAP_KEY: u64 = 27;

/// The line-number gutter's width: the design's 36.
pub const GUTTER_W: u32 = 36;

/// How far the numbers sit clear of the gutter's rule: the design's 8.
const GUTTER_GAP: u32 = 8;

/// The element key on the gutter.
pub const GUTTER_KEY: u64 = 24;

/// The element key on the gutter and the document together.
pub const AREA_PANE_KEY: u64 = 25;

/// The text style the buffer is drawn in — and so the one anything measuring it must use.
///
/// The view draws in it and the binary measures a pointer's `x` with it; different faces put
/// the caret further off the further into the line it goes.
pub const BUFFER_STYLE: libui::element::TextStyle =
    libui::element::TextStyle { size: libui::element::TextSize::Body, bold: false, mono: true };

/// The document's own key inside the area's dock. Not 40 (`MENU_BAR_KEY`), which `locate` hits.
pub const AREA_INNER_KEY: u64 = 20;
/// The document scrollbar's key.
pub const AREA_BAR_KEY: u64 = 21;
/// How wide the document's scrollbar is, in pixels.
pub const SCROLL_W: u32 = 12;
/// The element key on the save control.
pub const SAVE_KEY: u64 = 2;
/// The element key on the title bar.
pub const TITLE_KEY: u64 = 3;
/// The element key on the resize grip.
pub const GRIP_KEY: u64 = 4;
/// The element key on the status strip.
///
/// Every docked child needs a key, or the first frame is `MixedKeying` and no window appears.
pub const STRIP_KEY: u64 = 5;
/// The element key on the status text.
pub const STATUS_KEY: u64 = 6;
/// The element key on the tab strip.
pub const TAB_STRIP_KEY: u64 = 7;

/// Where buffer keys start, and therefore where the tabs' element keys do.
///
/// `Router::hovered_key` reports the nearest keyed ancestor across the whole tree, so a tab
/// sharing a number with the save button would hover with it. The top bit keeps these disjoint
/// from a list's row keys by construction.
pub const TAB_KEY_BASE: u64 = 1 << 63;

/// The element key on the confirmation dialog's title bar.
pub const CONFIRM_TITLE_KEY: u64 = 10;
/// The element key on the dialog's question.
pub const CONFIRM_TEXT_KEY: u64 = 11;
/// The element key on the dialog's button strip. Unused since `dialog_frame` took the strip;
/// kept so the number is not reused.
pub const CONFIRM_STRIP_KEY: u64 = 12;
/// The element key on the dialog's *discard* button.
pub const CONFIRM_DISCARD_KEY: u64 = 13;
/// The element key on the dialog's *keep editing* button.
pub const CONFIRM_KEEP_KEY: u64 = 14;

/// The confirmation dialog's geometry, which is `libui`'s: both dialogs share one table.
pub use libui::widget::{
    DIALOG_BUTTON_CY as CONFIRM_BUTTON_CY, DIALOG_H as CONFIRM_H, DIALOG_LEFT_CX, DIALOG_RIGHT_CX,
    DIALOG_W as CONFIRM_W,
};

/// The window's size in pixels at startup, before any manager places it.
pub const START_SIZE: Size = Size::new(560, 420);

/// What this window calls the acceptor it declares: a name a command line can address.
pub const ACCEPTOR: &str = "document";

/// The key that saves, with Ctrl held: `s`. A literal, since `libkern::abi` names only keys
/// with no character; a test pins `31` to `libinput::keymap`.
pub const SAVE_KEYCODE: u16 = 31;

/// The key that undoes, with Ctrl held: `z`. A literal, pinned against `libinput` by a test.
pub const UNDO_KEYCODE: u16 = 44;
/// The key that redoes: `y`. Not `Ctrl+Shift+Z`, which needs a modifier read nowhere else.
pub const REDO_KEYCODE: u16 = 21;
/// The key that opens the find field: `f`.
pub const FIND_KEYCODE: u16 = 33;

/// The menu bar's height in pixels — one row of chrome, matching the browser's.
pub const MENU_BAR_H: u32 = 24;

/// Where the menu bar's words are keyed from: `MENU_BAR_KEY + i` for menu `i`.
pub const MENU_BAR_KEY: u64 = 40;

/// The element key on the menu bar itself. Not optional: a partly keyed parent makes the
/// window undiffable and nothing draws.
pub const BAR_KEY: u64 = 15;

/// Where the open popup's rows are keyed from: `MENU_ROW_KEY + i` for item `i`.
pub const MENU_ROW_KEY: u64 = 100;

/// Where the chooser's element keys start — clear of this window's own.
pub const CHOOSER_KEY: u64 = 200;

/// Where the chooser's *rows* are keyed from, a range of its own: a row keyed like a button
/// would light that button as `hovered` is matched against both.
pub const CHOOSER_ROW_KEY: u64 = 1000;

/// The key that opens the file chooser: `o`, with Ctrl.
pub const OPEN_KEYCODE: u16 = 24;
/// The key that opens the chooser to save under a new name: `s`, with Ctrl and **Shift**.
pub const SAVE_AS_KEYCODE: u16 = 31;

/// The key that opens a *window*: `n`, with Ctrl and **Shift** (`Ctrl+N` is a buffer's).
pub const NEW_WINDOW_KEYCODE: u16 = 49;
/// The key that quits: `q`, with Ctrl. **Quit is not Close** — see [`Msg::Quit`].
pub const QUIT_KEYCODE: u16 = 16;

/// The key that opens a tab: `t`.
pub const NEW_TAB_KEYCODE: u16 = 20;
/// The key that closes one: `w`. Closing the last tab closes the window.
pub const CLOSE_TAB_KEYCODE: u16 = 17;

/// The key that copies the selection: `c`. `Ctrl+C`, as nothing here claims it for an interrupt.
pub const COPY_KEYCODE: u16 = 46;
/// The key that cuts: `x`.
pub const CUT_KEYCODE: u16 = 45;
/// The key that selects the whole buffer: `a`, with Ctrl. No Shift, unlike `nxterm`'s.
pub const SELECT_ALL_KEYCODE: u16 = 30;
/// The key that opens Replace: `h`, with Ctrl — where every editor puts it.
pub const REPLACE_KEYCODE: u16 = 35;
/// The key that jumps to a line: `g`, with Ctrl.
pub const GO_TO_KEYCODE: u16 = 34;
/// The key that pastes: `v`. With **Shift** as well it cycles — see [`App::cycling`].
pub const PASTE_KEYCODE: u16 = 47;

/// Whether `k` is `Ctrl+Shift+V` — the one key that *continues* a paste rather than ending it.
/// One function: two spellings of the rule would end a cycle on the key meant to continue it.
fn is_cycle_chord(k: KeyEvent) -> bool {
    k.keycode == PASTE_KEYCODE && k.modifiers & MOD_CTRL != 0 && k.modifiers & MOD_SHIFT != 0
}

/// Confirms the name being typed for an untitled buffer.
const NAME_CONFIRM: u16 = libkern::abi::KEY_ENTER;
/// Abandons it, leaving the buffer untitled and unsaved.
const NAME_CANCEL: u16 = libkern::abi::KEY_ESC;

/// Join a directory and a file name — `libfs::join`'s rule, on `str`.
fn join(dir: &str, name: &str) -> String {
    let mut s = String::from(dir);
    if !s.ends_with('/') {
        s.push('/');
    }
    s.push_str(name);
    s
}

/// One open file: what survives switching tabs. [`App`] keeps what a *window* has, so a second
/// tab cannot inherit the first's undo.
pub struct Buffer {
    /// Identity across frames and tabs; not the index, which renumbers when a tab closes.
    key: u64,
    /// The absolute path being edited, or empty for a buffer that has never been named.
    path: String,
    /// The last component of [`path`](Self::path), for the tab and the title bar.
    name: String,
    /// The text and the whole editing model, including undo, which is per buffer.
    text: TextAreaState,
    /// The buffer's revision when it was last read or written. A number, not a flag, so
    /// "modified" is derived: a save's success arrives a frame after the keystroke.
    saved_at: u64,
    /// Why this buffer may not be written, or `None` when it may.
    blocked: Option<String>,
    /// The language this buffer's **name** implies, or `None`; settled at open and on Save As.
    lang: Option<syntax::Language>,
    /// What was open at the start of each line: the scanner's state, cached per line. The *runs*
    /// are recomputed for the visible lines each frame, so they are never stale.
    starts: alloc::vec::Vec<syntax::State>,
    /// The revision [`starts`](Self::starts) was computed for.
    scanned_at: u64,
    /// The lowest line an edit has touched since the last scan; `usize::MAX` if none.
    ///
    /// The rescan cannot start below it nor early-exit above it: an edit changes the state
    /// entering every later line.
    dirty_from: usize,
    /// The buffer did not match the file the moment it was read.
    ///
    /// `with_text` drops the `\r` from every line, so a CRLF file is already something else on
    /// screen and the first `Ctrl+S` would rewrite it shorter. Folded into [`App::modified`].
    differs: bool,
}

/// A chooser the editor has open; `main` does the listing, `libui::chooser` renders it.
pub struct Chooser {
    /// Open a file, or name one to save.
    pub mode: chooser::Mode,
    /// The directory being shown.
    pub dir: String,
    /// What is in it: a name and whether descending into it makes sense.
    pub entries: Vec<(String, bool)>,
    /// Selection, scroll, and the name being typed.
    pub state: ChooserState,
}

/// Everything the editor is.
pub struct App {
    /// A pointer event over the document, awaiting the metrics that resolve it. One, not a queue.
    area_pointer: Option<PointerEvent>,
    /// Where within the document scrollbar's thumb a drag took hold.
    scroll_grab: libui::widget::ScrollGrab,
    /// The open buffers, in tab order. **Never empty**: the last one closing closes the window.
    buffers: Vec<Buffer>,
    /// Which buffer's tab is current, by [`Buffer::key`].
    current: u64,
    /// The chooser this window has open, if any.
    chooser: Option<Chooser>,
    /// A directory the binary is being asked to list for the chooser.
    chooser_list: Option<String>,
    /// A path the binary is being asked to open into a tab.
    open_requested: Option<String>,
    /// What a replace is looking for, beside the field so `Field` stays a plain marker.
    replacing: Option<String>,
    /// How many replacements the running replace has made, for the strip to report.
    replaced: usize,
    /// Another window has been asked for, and the binary has not made it yet.
    new_window: bool,
    /// A quit has been asked for. The binary owns what that means: it knows the window count.
    quit: bool,
    /// Which menu is open, where each bar word sits, and where the keyboard is inside it.
    pub menus: MenuState,
    /// The next key to hand out. Monotonic, so a stale message never names a replacement.
    next_key: u64,
    /// What the status strip says.
    status: String,
    /// The window's size in pixels — what the client commits.
    window: Size,
    /// Whether this window holds the keyboard, which the title bar shows.
    pub focused: bool,
    /// This window last asked to be maximised, so its maximise button now asks for normal.
    maximized: bool,
    /// A save the binary owes the filesystem, and **which buffer asked for it**.
    ///
    /// The key, not a flag: `main` writes after the whole batch is applied, so a `Ctrl+S` and a
    /// tab click in one drain wrote the other tab's bytes to its path.
    save_requested: Option<u64>,
    /// What the editor owes the clipboard — see [`ClipRequest`].
    clip_request: Option<ClipRequest>,
    /// The paste a cycle would continue, or `None`. See [`Cycling`].
    cycling: Option<Cycling>,
    /// A title-bar button was pressed, and the binary owes the compositor a `RequestState`.
    state_requested: Option<u32>,
    /// The title bar was dragged, and the binary owes the compositor a `StartMove`.
    move_requested: bool,
    /// The grip was pressed, and the binary owes the compositor a `StartResize`.
    resize_requested: Option<u32>,
    /// The editor has been asked to close, and the binary owes an exit.
    closing: bool,
    /// What is being closed over an unsaved buffer, unanswered. The binary opens or destroys a
    /// `Role::Dialog` window to match each frame.
    confirming: Option<Closing>,
    /// Whether the *dialog* holds the keyboard, for its own title bar. Not `focused`: a focus
    /// change sends both halves down one channel.
    pub confirm_focused: bool,
    /// Whether the **chooser's** window holds the keyboard, read as `confirm_focused` is.
    pub chooser_focused: bool,
    /// The dialog's title bar was dragged; the binary owes a `StartMove` **on the dialog's
    /// window**. Not [`move_requested`](Self::move_requested): the two name different windows.
    confirm_move_requested: bool,
    /// Text being typed into the status strip's field, and what it is for.
    ///
    /// `Some` is a mode: the keys belong to the field, not the buffer. In the editor rather than
    /// collected through the shell, which would become a dialog provider for any client.
    field: Option<(Field, TextFieldState)>,
    /// Where an untitled buffer is saved, from the session's `HOME`.
    home: String,
    /// The last search's answer, which the binary owes the console: `Some(Some(line))` for a hit,
    /// `Some(None)` for a miss, `None` once reported. A line number, not the needle.
    find_report: Option<Option<usize>>,
}

/// What a confirmation is about; a tab's key is captured when asked, so the answer cannot retarget.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Closing {
    /// The whole window, and every buffer in it.
    Window,
    /// One tab, by its key.
    Tab(u64),
}

/// What the status strip's field is collecting: one mode, or `Esc` could mean two things.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    /// A name for a buffer that has never had one, on the way to a save.
    Naming,
    /// Text to look for in the buffer.
    Finding,
    /// Text to look for, on the way to replacing it. A stage of its own, or "find" would label a
    /// field whose Enter starts replacing.
    ReplaceFind,
    /// What to put in its place.
    ReplaceWith,
    /// A line number to jump to.
    GoingTo,
}

/// What the editor owes the clipboard, which only `main` can do — an outbox, like
/// [`App::take_save`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ClipRequest {
    /// Push this text onto the ring.
    Copy(String),
    /// Fetch the newest entry and insert it.
    Paste,
    /// Fetch the entry **after** the one just pasted, replacing what that paste inserted. Valid
    /// only immediately after a paste: any other action ends it ([`App::cycling`]).
    Cycle,
}

/// Where a paste put its text, and which entry it was. Exists only between a paste and the
/// next action; the serial is what the server checks, as a pipeline can push mid-cycle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cycling {
    /// The buffer the paste went into. A cycle after switching tabs is not a cycle.
    pub buffer: u64,
    /// The range the paste occupies now, which a cycle replaces.
    pub from: (usize, usize),
    /// See [`from`](Self::from).
    pub to: (usize, usize),
    /// Which ring entry was pasted. The next cycle asks for the one after it.
    pub index: u32,
    /// The ring's serial when that entry was read. The server refuses if it has moved.
    pub serial: u64,
}

impl Field {
    /// What the strip says the field is for, and what its receipt calls itself.
    pub fn label(self) -> &'static str {
        match self {
            Field::Naming => "name",
            Field::Finding => "find",
            Field::ReplaceFind => "replace what",
            Field::ReplaceWith => "replace with",
            Field::GoingTo => "go to line",
        }
    }
}

/// What can happen to the editor.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Msg {
    /// A key reached the window.
    Key(KeyEvent),
    /// Write the buffer to its path — the save control, or `Ctrl+S`.
    Save,
    /// A drag was released over the text area — the payload is the binary's to hand over.
    Dropped,
    /// The title bar was dragged.
    DragWindow,
    /// The resize grip was pressed, for these edges.
    ResizeWindow(u32),
    /// A title-bar button asking the manager for a window state.
    RequestState(u32),
    /// Somebody wants this window gone — its own close button, or the shell asking. Over an
    /// unsaved buffer this raises [`App::confirming`]; only [`Msg::Discard`] ends the run.
    Close,
    /// The dialog's *discard* answer: close, and the buffer goes with it.
    Discard,
    /// The dialog's *keep editing* answer, its close button, and `Esc`: the question goes away
    /// and nothing else happens.
    KeepEditing,
    /// The dialog's title bar was dragged.
    DragConfirm,
    /// A tab was pressed — make it current.
    SelectTab(u64),
    /// A tab's close box was pressed, or `Ctrl+W`.
    CloseTab(u64),
    /// `Ctrl+T`: an untitled buffer in a tab of its own.
    NewTab,
    /// A word on the menu bar was pressed: open that menu, or close it if it was already open.
    MenuBar(usize),
    /// Open the chooser to pick a file — `Ctrl+O`, or File ▸ Open.
    OpenFile,
    /// Open the chooser to name this buffer — `Ctrl+Shift+S`; see [`App::adopt_path`].
    SaveAs,
    /// A row of the chooser was activated: descend into it, or choose it.
    ChooserRow(u64),
    /// The chooser's accepting button, or `Enter` in its name field.
    ChooserAccept,
    /// The chooser's *Cancel*, its close button, or `Esc`.
    ChooserCancel,
    /// Leave the directory the chooser is showing — its *up* control, or `Backspace`.
    ChooserUp,
    /// The wheel turned over the chooser — see [`ListState::wheel`](libui::widget::ListState::wheel).
    ChooserWheel(PointerEvent),
    /// A pointer over the document. Recorded: a pixel needs metrics ([`App::take_area_pointer`]).
    AreaPointer(PointerEvent),
    /// A pointer event over the document's scrollbar.
    AreaScroll(PointerEvent),
    /// The wheel turned over the document.
    AreaWheel(PointerEvent),
    /// Open another window — `Ctrl+Shift+N`. Recorded; the binary makes it.
    NewWindow,
    /// Close every window of this application — `Ctrl+Q`. **Quit is not Close**: each window is
    /// asked as its own close button asks, **cancelling one aborts the quit**, and windows
    /// already closed stay closed.
    Quit,
    /// Undo the last edit — `Ctrl+Z`. The menu declares the chord and [`libui::menu::accel_match`]
    /// routes it, so label and binding cannot drift.
    Undo,
    /// Redo what was undone — `Ctrl+Y`. See [`Undo`](Msg::Undo).
    Redo,
    /// Open the find field — `Ctrl+F`. See [`Undo`](Msg::Undo).
    Find,
    /// Select the whole buffer — `Ctrl+A`, or Edit ▸ Select All.
    SelectAll,
    /// Start a replace — `Ctrl+H`, or Edit ▸ Replace.
    Replace,
    /// Jump to a line — `Ctrl+G`, or Edit ▸ Go to Line.
    GoTo,
    /// Copy the selection — `Ctrl+C`. See [`Undo`](Msg::Undo).
    Copy,
    /// Cut the selection — `Ctrl+X`. See [`Undo`](Msg::Undo).
    Cut,
    /// Paste the newest clipboard entry — `Ctrl+V`. Not the cycle (`Ctrl+Shift+V`), a continuation
    /// matched before the table.
    Paste,
}

/// What a token kind is drawn in: the one place meaning maps onto a colour, as `libui` is handed
/// colours. [`syntax::Kind::Plain`] is never emitted as a run.
fn colour_of(kind: syntax::Kind, theme: &UiTheme) -> libdraw::format::Rgb {
    match kind {
        syntax::Kind::Keyword => theme.syntax_keyword,
        syntax::Kind::Str => theme.syntax_string,
        syntax::Kind::Comment => theme.syntax_comment,
        syntax::Kind::Number => theme.syntax_number,
        syntax::Kind::Heading => theme.syntax_heading,
        syntax::Kind::Variable => theme.syntax_variable,
        // A key takes the variable's colour: no language that has keys also has variables, and
        // the accent the design uses is the focus ring and selection here.
        syntax::Kind::Key => theme.syntax_variable,
        syntax::Kind::Plain => theme.foreground,
    }
}

impl Buffer {
    /// A buffer for `path`, empty until something is loaded into it.
    fn new(key: u64, path: &str) -> Buffer {
        Buffer {
            key,
            path: String::from(path),
            name: libfs::basename_str(path).to_string(),
            text: TextAreaState::new(),
            saved_at: 0,
            blocked: None,
            differs: false,
            lang: syntax::for_name(libfs::basename_str(path)),
            starts: alloc::vec::Vec::new(),
            scanned_at: u64::MAX,
            dirty_from: 0,
        }
    }

    /// Bring [`starts`](Self::starts) up to date with the text, if it is not already.
    ///
    /// Downward from the edit until the state matches the cache. Not when the line count changed:
    /// entries below an inserted line shifted, so a "match" compares another line's state.
    fn rescan(&mut self) {
        let rev = self.text.revision();
        if self.scanned_at == rev {
            return;
        }
        self.scanned_at = rev;
        let Some(lang) = self.lang else {
            self.starts.clear();
            self.dirty_from = usize::MAX;
            return;
        };
        let lines = self.text.lines();
        let n = lines.len();
        let count_changed = self.starts.len() != n;
        self.starts.resize(n, syntax::State::Normal);
        // An edit nobody located means the whole buffer, exit and all: clamping `usize::MAX` names
        // the last line, and starting at zero alone lets the early exit fire at once.
        let unknown = self.dirty_from == usize::MAX;
        let from = if unknown { 0 } else { self.dirty_from.min(n.saturating_sub(1)) };
        self.dirty_from = usize::MAX;
        let mut st = if from == 0 { syntax::State::Normal } else { self.starts[from] };
        #[allow(clippy::needless_range_loop, reason = "the index also feeds arithmetic or a second array")]
        for i in from..n {
            self.starts[i] = st;
            let (_, next) = syntax::scan(&lang, &lines[i], st);
            st = next;
            if !count_changed && !unknown && i + 1 < n && self.starts[i + 1] == st {
                return;
            }
        }
    }

    /// Whether what is on screen differs from what is on disk.
    fn modified(&self) -> bool {
        self.differs || self.text.revision() != self.saved_at
    }
}

impl App {
    /// An editor for `path`, with an empty buffer until something is loaded into it.
    pub fn new(path: &str, home: &str) -> App {
        App {
            area_pointer: None,
            scroll_grab: libui::widget::ScrollGrab::new(),
            buffers: alloc::vec![Buffer::new(TAB_KEY_BASE, path)],
            current: TAB_KEY_BASE,
            chooser: None,
            chooser_list: None,
            open_requested: None,
            replacing: None,
            replaced: 0,
            new_window: false,
            quit: false,
            // Zero: the length is set every frame by `set_anchors`.
            menus: MenuState::new(0),
            next_key: TAB_KEY_BASE + 1,
            field: None,
            home: String::from(home),
            find_report: None,
            status: if path.is_empty() {
                String::from("untitled — save to name it")
            } else {
                String::from("opening…")
            },
            window: START_SIZE,
            focused: true,
            maximized: false,
            save_requested: None,
            clip_request: None,
            cycling: None,
            state_requested: None,
            move_requested: false,
            resize_requested: None,
            closing: false,
            confirming: None,
            confirm_focused: true,
            chooser_focused: true,
            confirm_move_requested: false,
        }
    }

    /// Remove the tab keyed `k`, and close the window if it was the last one, which keeps
    /// `buffers` non-empty.
    fn drop_tab(&mut self, k: u64) {
        if self.buffers.len() <= 1 {
            self.closing = true;
            return;
        }
        let Some(i) = self.buffers.iter().position(|b| b.key == k) else { return };
        self.buffers.remove(i);
        if self.current == k {
            // The neighbour on the left, or the first: the least surprising place to land.
            self.current = self.buffers[i.saturating_sub(1).min(self.buffers.len() - 1)].key;
        }
    }

    /// The buffer whose tab is current. Never `None` (closing the last tab closes the window);
    /// falls back to the first rather than panicking.
    fn buf(&self) -> &Buffer {
        self.buffers.iter().find(|b| b.key == self.current).unwrap_or(&self.buffers[0])
    }

    /// The current buffer, mutably. See [`buf`](Self::buf) for why this cannot be `None`.
    fn buf_mut(&mut self) -> &mut Buffer {
        let key = self.current;
        let i = self.buffers.iter().position(|b| b.key == key).unwrap_or(0);
        &mut self.buffers[i]
    }

    /// Open `path` in a **new tab** and make it current, returning its key; the caller reads
    /// the file into it.
    pub fn open_tab(&mut self, path: &str) -> u64 {
        let key = self.next_key;
        self.next_key += 1;
        self.buffers.push(Buffer::new(key, path));
        self.current = key;
        key
    }

    /// The tabs, for the strip that draws them.
    pub fn tabs(&self) -> Vec<(u64, String, bool)> {
        self.buffers
            .iter()
            .map(|b| {
                let name = if b.name.is_empty() { String::from("untitled") } else { b.name.clone() };
                (b.key, name, b.modified())
            })
            .collect()
    }

    /// Which tab is current.
    pub fn current_tab(&self) -> u64 {
        self.current
    }

    /// How many buffers are open — what tells the last close from the others.
    pub fn tab_count(&self) -> usize {
        self.buffers.len()
    }

    /// The file held `raw`, which decoded to `text`. Both, because line endings are normalised on
    /// the way in: a CRLF file is not what is on screen, and calling it unmodified would rewrite
    /// it shorter on a habitual `Ctrl+S`.
    pub fn loaded(&mut self, text: &str, raw: &[u8]) {
        let b = self.buf_mut();
        b.text = TextAreaState::with_text(text);
        // `starts` is cleared as well as invalidated: stale states of the same length would let
        // the early exit compare against them and return on line 0.
        b.starts.clear();
        b.dirty_from = 0;
        b.scanned_at = u64::MAX;
        b.saved_at = b.text.revision();
        b.blocked = None;
        b.differs = to_bytes(&b.text.text()) != raw;
        let differs = b.differs;
        self.status = describe(raw.len(), "opened");
        if differs {
            self.status.push_str(" · line endings normalised, so saving rewrites it");
        }
    }

    /// There is nothing at this path yet, which is not a failure: the buffer stays empty and
    /// writable and the strip says "new file", so a person who meant an existing file notices.
    pub fn absent(&mut self) {
        let b = self.buf_mut();
        b.text = TextAreaState::new();
        b.saved_at = b.text.revision();
        b.blocked = None;
        b.differs = false;
        self.status = String::from("new file");
    }

    /// The file could not be read, so this buffer must not be written over it. It stays blocked
    /// for the run: only a successful read would clear it, and this editor reads once.
    pub fn blocked(&mut self, why: &str) {
        let b = self.buf_mut();
        b.text = TextAreaState::new();
        b.saved_at = b.text.revision();
        b.blocked = Some(String::from(why));
        b.differs = false;
        self.status = String::from(why);
    }

    /// The path being edited.
    pub fn path(&self) -> &str {
        &self.buf().path
    }

    /// The buffer, for a binary that is about to write it.
    pub fn text(&self) -> String {
        self.buf().text.text()
    }

    /// How many times the buffer has been edited — the receipt a keystroke reached it, which
    /// gates pace on. A count, not the text: what somebody types is theirs.
    pub fn revision(&self) -> u64 {
        self.buf().text.revision()
    }

    /// Whether what is on screen differs from what is on disk: something was typed, or the file
    /// did not survive being read unchanged (see `differs`).
    pub fn modified(&self) -> bool {
        self.buf().modified()
    }

    /// What the status strip is saying.
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Where the cursor is, as a person counts: `line 3, col 12`. One-based, and the column counts
    /// characters, not bytes.
    pub fn position_text(&self) -> String {
        let t = &self.buf().text;
        let (line, col) = t.cursor();
        let chars = t.lines().get(line).map_or(0, |l| l.get(..col).map_or(0, |head| head.chars().count()));
        alloc::format!("line {}, col {}", line + 1, chars + 1)
    }

    /// Whether this buffer refuses to be written, and why.
    pub fn refusal(&self) -> Option<&str> {
        self.buf().blocked.as_deref()
    }

    /// Copy the selection onto the ring, deleting it as well when `cut`. Nothing selected is not a
    /// request: an empty entry would move the ring's serial under a client mid-cycle.
    fn copy(&mut self, cut: bool) {
        let Some(text) = self.buf().text.selected_text() else {
            self.status = String::from(if cut { "nothing to cut" } else { "nothing to copy" });
            return;
        };
        if cut {
            self.buf_mut().text.delete_selection();
            // A cut is a complete edit, so the next keystroke is a separate undo step.
            self.buf_mut().text.end_group();
        }
        self.status = String::from(if cut { "cut" } else { "copied" });
        self.clip_request = Some(ClipRequest::Copy(text));
    }

    /// What the editor owes the clipboard, taken exactly once.
    pub fn take_clip_request(&mut self) -> Option<ClipRequest> {
        self.clip_request.take()
    }

    /// Where a cycle would continue from, or `None` — see [`Cycling`].
    pub fn cycling(&self) -> Option<Cycling> {
        self.cycling
    }

    /// Insert `text` as a paste and remember where it went; `index` and `serial` are the server's.
    pub fn pasted(&mut self, text: &str, index: u32, serial: u64) {
        let buffer = self.current;
        let (from, to) = self.buf_mut().text.insert_text(text);
        // A paste's answer does not pass through `update`, so it reports its own position.
        self.edited_at(from.0);
        self.cycling = Some(Cycling { buffer, from, to, index, serial });
        self.status = String::from("pasted");
    }

    /// Replace what the last paste inserted with `text` — a cycle's answer. Does nothing without a
    /// live [`cycling`](Self::cycling) or if the current buffer is not the one pasted into.
    pub fn cycled(&mut self, text: &str, index: u32, serial: u64) {
        let Some(c) = self.cycling else { return };
        if c.buffer != self.current {
            return;
        }
        self.buf_mut().text.select_range(c.from, c.to);
        let (from, to) = self.buf_mut().text.insert_text(text);
        self.edited_at(c.from.0.min(from.0));
        self.cycling = Some(Cycling { buffer: c.buffer, from, to, index, serial });
        self.status = String::from("pasted the one before");
    }

    /// End a cycling sequence — the ring moved, or there is nothing further back.
    pub fn cycle_ended(&mut self, why: &str) {
        self.cycling = None;
        self.status = String::from(why);
    }

    /// The save the binary owes, as `(key, path, bytes)`. Clears the record.
    ///
    /// `None` when nothing asked, or when this buffer is blocked: that is where the refusal is
    /// enforced, and the strip says why.
    pub fn take_save(&mut self) -> Option<(u64, String, String)> {
        let key = core::mem::take(&mut self.save_requested)?;
        let b = self.buffers.iter().find(|b| b.key == key)?;
        if let Some(why) = b.blocked.clone() {
            self.status = alloc::format!("not saved — {why}");
            return None;
        }
        // The path comes from the buffer that asked, not from the current one.
        Some((key, b.path.clone(), b.text.text()))
    }

    /// Take `path` as a file to edit, in a new tab; `false` if it is already open, which is
    /// switched to instead (two tabs on one file can disagree, and the last saved wins).
    pub fn accept_drop(&mut self, path: &str) -> bool {
        if let Some(b) = self.buffers.iter().find(|b| b.path == path) {
            self.current = b.key;
            self.status = alloc::format!("{} is already open", libfs::basename_str(path));
            return false;
        }
        self.open_tab(path);
        true
    }

    /// A save finished: `Ok(bytes written)`, or `Err(what went wrong)`.
    ///
    /// A failure changes nothing but the message: the buffer stays modified, as marking it saved
    /// would let the person close the window and lose their work.
    pub fn saved(&mut self, key: u64, result: Result<usize, &str>) {
        match result {
            Ok(n) => {
                let Some(i) = self.buffers.iter().position(|b| b.key == key) else { return };
                let b = &mut self.buffers[i];
                b.saved_at = b.text.revision();
                b.differs = false;
                // A save closes the undo group: undo should reach back to the save, not the open.
                b.text.end_group();
                self.status = describe(n, "saved");
            }
            Err(why) => self.status = alloc::format!("NOT saved — {why}"),
        }
    }

    /// Apply a message.
    pub fn update(&mut self, msg: Msg) {
        // Where an edit landed, for the highlighter: the cursor's line before and after bracket
        // every edit. Outside the match, so a message added later cannot forget it.
        let was = (self.current, self.buf().text.cursor().0, self.buf().text.revision());
        self.dispatch(msg);
        if self.current == was.0 && self.buf().text.revision() != was.2 {
            let now = self.buf().text.cursor().0;
            let from = was.1.min(now);
            let b = self.buf_mut();
            b.dirty_from = b.dirty_from.min(from);
        }
    }

    /// Record that an edit touched line `from`, so the next scan starts at or above it. For
    /// the paths that do not go through [`update`](Self::update), such as a clipboard answer.
    fn edited_at(&mut self, from: usize) {
        let b = self.buf_mut();
        b.dirty_from = b.dirty_from.min(from);
    }

    /// The body of [`update`](Self::update).
    fn dispatch(&mut self, msg: Msg) {
        // Any message but a key is an action and ends a cycle; `key` handles its own case.
        if !matches!(msg, Msg::Key(_)) {
            self.cycling = None;
        }
        // Choosing dismisses the menu, asked of the table so a row added later cannot forget.
        // A message that is not a row leaves it alone, so `MenuBar` can open one.
        if self
            .menu_table()
            .iter()
            .flat_map(|m| m.items.iter())
            .any(|it| matches!(it, Item::Action { msg: m, .. } if *m == msg))
        {
            self.menus.close();
        }
        match msg {
            Msg::Key(k) => self.key(k),
            Msg::MenuBar(i) => self.menus.toggle(i),
            Msg::OpenFile => self.open_chooser(chooser::Mode::Open),
            Msg::SaveAs => self.open_chooser(chooser::Mode::Save),
            // The message carries a key; a key below the base is not a row, where a saturating
            // subtraction would have made it row zero.
            Msg::ChooserRow(key) => {
                if let Some(row) = key.checked_sub(CHOOSER_ROW_KEY) {
                    self.chooser_activate(row);
                }
            }
            Msg::ChooserAccept => self.chooser_accept(),
            Msg::ChooserCancel => self.chooser = None,
            // The root is its own parent, so this is a no-op there, and the control is not greyed.
            Msg::ChooserUp => {
                if let Some(c) = self.chooser.as_ref() {
                    let up = core::str::from_utf8(libfs::parent(c.dir.as_bytes())).unwrap_or("/");
                    if up != c.dir {
                        self.chooser_list = Some(String::from(up));
                    }
                }
            }
            // Kept, not resolved: the binary hands it back through `take_area_pointer` with
            // metrics, since this half holds no font.
            Msg::AreaPointer(p) => self.area_pointer = Some(p),
            Msg::AreaScroll(p) => {
                let visible = (self.area_h() / ROW_H) as usize;
                let bar = self.buf().text.bar(visible);
                let h = self.area_h();
                if let Some(offset) = self.scroll_grab.apply(bar, h, p) {
                    self.buf_mut().text.scroll_to(offset as usize, visible);
                }
            }
            Msg::AreaWheel(p) => {
                let visible = (self.area_h() / ROW_H) as usize;
                let by = i64::from(p.wheel) * i64::from(libui::click::WHEEL_UNITS);
                let at = self.buf().text.offset() as i64;
                let want = (at + by).max(0) as usize;
                self.buf_mut().text.scroll_to(want, visible);
            }
            Msg::ChooserWheel(p) => {
                if let Some(c) = self.chooser.as_mut() {
                    c.state.list.wheel(p.wheel);
                }
            }
            Msg::NewWindow => self.new_window = true,
            Msg::Quit => self.quit = true,
            Msg::Undo => {
                let ok = self.buf_mut().text.undo();
                self.status = String::from(if ok { "undone" } else { "nothing to undo" });
            }
            Msg::Redo => {
                let ok = self.buf_mut().text.redo();
                self.status = String::from(if ok { "redone" } else { "nothing to redo" });
            }
            // Reached by the chord only when no field is open, since a field takes the keys first;
            // the menu row can reach it while one is, and re-opening is the same as opening.
            Msg::Find => {
                self.field = Some((Field::Finding, TextFieldState::new()));
                self.status = String::from("find, then Enter");
            }
            Msg::Replace => {
                self.replacing = None;
                self.replaced = 0;
                self.field = Some((Field::ReplaceFind, TextFieldState::new()));
                self.status = String::from("replace what, then Enter");
            }
            Msg::GoTo => {
                self.field = Some((Field::GoingTo, TextFieldState::new()));
                self.status = String::from("line number, then Enter");
            }
            // `select_range` clamps, so an empty buffer selects nothing rather than a zero-width
            // range the copy path would treat as a selection.
            Msg::SelectAll => {
                // A byte offset, because `select_range` takes one: a character count is short
                // for a line ending in a multi-byte character.
                let text = &self.buf().text;
                let last = text.lines().len().saturating_sub(1);
                let end = text.lines().get(last).map_or(0, |l| l.len());
                self.buf_mut().text.select_range((0, 0), (last, end));
            }
            Msg::Copy => self.copy(false),
            Msg::Cut => self.copy(true),
            Msg::Paste => self.clip_request = Some(ClipRequest::Paste),
            // Saving an untitled buffer asks for a name first: inventing a path would make a
            // file somebody did not choose.
            Msg::Save => {
                if self.buf().path.is_empty() {
                    // Any field but a naming one is replaced: clicking *save* with the find
                    // field open must not do nothing.
                    if self.field_kind() != Some(Field::Naming) {
                        self.field = Some((Field::Naming, TextFieldState::new()));
                        self.status = String::from("name it, then Enter");
                    }
                } else {
                    self.save_requested = Some(self.current);
                }
            }
            // The payload is in the event the binary holds, which pairs it with the widget.
            Msg::Dropped => {}
            Msg::DragWindow => self.move_requested = true,
            Msg::DragConfirm => self.confirm_move_requested = true,
            Msg::ResizeWindow(edges) => self.resize_requested = Some(edges),
            // A modified buffer never closes on this message, however often it arrives (a double
            // click on a taskbar entry would otherwise exit). Any modified tab stops it.
            Msg::Close => {
                if self.buffers.iter().any(Buffer::modified) {
                    self.confirming = Some(Closing::Window);
                } else {
                    self.closing = true;
                }
            }
            Msg::SelectTab(k) => {
                if self.buffers.iter().any(|b| b.key == k) {
                    self.current = k;
                }
            }
            // A new tab is an untitled buffer, as a launch with no argument gives.
            Msg::NewTab => {
                self.open_tab("");
                self.status = String::from("untitled — save to name it");
            }
            Msg::CloseTab(k) => {
                let modified = self.buffers.iter().any(|b| b.key == k && b.modified());
                if modified {
                    self.confirming = Some(Closing::Tab(k));
                } else {
                    self.drop_tab(k);
                }
            }
            Msg::Discard => match self.confirming.take() {
                Some(Closing::Window) | None => self.closing = true,
                Some(Closing::Tab(k)) => self.drop_tab(k),
            },
            // The strip says so: a dialog that vanishes with nothing changed looks like discard.
            Msg::KeepEditing => {
                self.confirming = None;
                self.status = String::from("still editing — nothing was discarded");
            }
            Msg::RequestState(s) => {
                if s == WINDOW_STATE_MAXIMIZED || s == WINDOW_STATE_NORMAL {
                    self.maximized = s == WINDOW_STATE_MAXIMIZED;
                }
                self.state_requested = Some(s);
            }
        }
    }

    /// Editing keys go to the buffer; `Ctrl+S` saves.
    ///
    /// The chord is checked before the buffer sees the key: `libinput` folds `Ctrl+S` to `0x13`,
    /// which `apply` declines today, but a new control character could make one key do both.
    fn key(&mut self, k: KeyEvent) {
        if k.pressed != KEY_DOWN && k.pressed != KEY_REPEAT {
            return;
        }
        // Every key but the cycle itself ends a cycling sequence, here, before any arm can
        // forget: this is what makes a stale ring position unreachable.
        if !is_cycle_chord(k) {
            self.cycling = None;
        }
        // The tab chords act on the window, so they come before any field; chords that act on
        // the buffer do not, as a `Ctrl+S` mid-name would ask to save what has no name yet.
        if k.modifiers & MOD_CTRL != 0 && matches!(k.keycode, NEW_TAB_KEYCODE | CLOSE_TAB_KEYCODE) {
            if k.keycode == NEW_TAB_KEYCODE {
                self.update(Msg::NewTab);
            } else {
                let key = self.current;
                self.update(Msg::CloseTab(key));
            }
            return;
        }
        // While a field is open the rest of the keys are the field's, chords included. What was
        // typed is read out first so the field's borrow ends before the buffer work.
        if let Some((which, _)) = self.field.as_ref() {
            let which = *which;
            match k.keycode {
                NAME_CANCEL => {
                    self.field = None;
                    // Naming says what it did not do; find leaves the strip alone, so the last
                    // search's answer stays.
                    if which == Field::Naming {
                        self.status = String::from("not saved");
                    }
                }
                NAME_CONFIRM => {
                    // Owned, so the field's borrow ends before the buffer work below.
                    let typed: String = self.field.as_ref().map(|(_, f)| String::from(f.text())).unwrap_or_default();
                    match which {
                        Field::Naming => {
                            let name = typed.trim().to_string();
                            if name.is_empty() {
                                // An empty name would save to the directory itself. Left open:
                                // the person is mid-answer.
                                self.status = String::from("a name, then Enter");
                                return;
                            }
                            let path = join(&self.home, &name);
                            self.field = None;
                            self.adopt_path(&path);
                        }
                        // The field stays open so Enter walks the matches; `Esc` ends it.
                        Field::Finding => {
                            let hit = self.buf_mut().text.find(&typed);
                            let line = self.buf().text.cursor().0;
                            self.status =
                                if hit { alloc::format!("found {typed}") } else { alloc::format!("no {typed}") };
                            self.find_report = Some(hit.then_some(line));
                        }
                        // The first stage finds the needle, so the person sees what is about to
                        // be replaced. An empty needle ends the replace.
                        Field::ReplaceFind => {
                            if typed.is_empty() {
                                self.field = None;
                                self.status = String::from("nothing to replace");
                                return;
                            }
                            // A needle that is not there does not advance: `find` leaves the anchor
                            // alone on a miss, so an earlier selection would be replaced.
                            if !self.buf_mut().text.find(&typed) {
                                self.status = alloc::format!("no {typed}");
                                return;
                            }
                            self.replacing = Some(typed.clone());
                            self.field = Some((Field::ReplaceWith, TextFieldState::new()));
                            self.status = alloc::format!("replace {typed} with, then Enter");
                        }
                        // Clamped to the buffer, as every go-to-line is: line 900 of 40 means the
                        // end. A number that is not one is refused as a typo.
                        Field::GoingTo => {
                            let Ok(want) = typed.parse::<usize>() else {
                                self.status = alloc::format!("{typed} is not a line number");
                                return;
                            };
                            let lines = self.buf().text.lines().len();
                            let line = want.max(1).min(lines.max(1)) - 1;
                            self.buf_mut().text.place(line, 0);
                            self.field = None;
                            self.status = alloc::format!("line {}", line + 1);
                            self.find_report = Some(Some(line));
                        }
                        Field::ReplaceWith => {
                            let Some(needle) = self.replacing.clone() else {
                                self.field = None;
                                return;
                            };
                            // The selection must be the match: anything else is somebody's own.
                            // Each Enter replaces and finds the next, not all at once.
                            if self.buf().text.selected_text().as_deref() == Some(needle.as_str()) {
                                self.buf_mut().text.insert_text(&typed);
                                self.replaced += 1;
                            }
                            let more = self.buf_mut().text.find(&needle);
                            let line = self.buf().text.cursor().0;
                            self.find_report = Some(more.then_some(line));
                            self.status = if more {
                                alloc::format!("replaced {}, Enter for the next", self.replaced)
                            } else {
                                alloc::format!("replaced {}", self.replaced)
                            };
                        }
                    }
                }
                code => {
                    if let Some((_, f)) = self.field.as_mut() {
                        f.apply(code, k.modifiers, k.locks);
                    }
                }
            }
            return;
        }
        if k.modifiers & MOD_CTRL != 0 {
            // The cycle is taken before the table: it continues a paste and means nothing
            // otherwise, so it has no business in a list of things to choose.
            if is_cycle_chord(k) {
                self.clip_request = Some(ClipRequest::Cycle);
                return;
            }
            // Matched against the menu table, so "Ctrl+Z undoes" is stated once.
            if let Some(msg) = libui::menu::accel_match(&self.menu_table(), &k) {
                self.update(msg);
                return;
            }
            // Every other chord is swallowed, not passed on: `Ctrl+X` folding to a printable
            // character would otherwise type it.
            return;
        }
        self.buf_mut().text.apply(k.keycode, k.modifiers, k.locks);
    }

    /// Record where each bar word sits, so an open menu knows where to hang from.
    pub fn place_menus(&mut self, view: &Element<Msg>, l: &libui::layout::Layout) {
        let n = self.menu_count();
        self.menus.set_anchors((0..n).map(|i| libui::layout::locate(view, l, MENU_BAR_KEY + i as u64)).collect());
    }

    /// How many menus the bar has — derived from the table, never declared, as a constant drifted.
    pub fn menu_count(&self) -> usize {
        self.menu_table().len()
    }

    /// The bar's menus, in bar order. Built rather than stored: half the rows depend on state.
    /// The chords are declared here and read back by [`libui::menu::accel_match`].
    pub fn menu_table(&self) -> Vec<Menu<Msg>> {
        let has_selection = self.buf().text.has_selection();
        // Nothing is available while a field is open: `key` gives an open field every keystroke,
        // and a menu row would be a way around that.
        let free = self.field.is_none();
        let act = |it: Item<Msg>| it.enabled(free);
        vec![
            Menu {
                title: "File",
                items: vec![
                    act(Item::new("New Tab", Accel::ctrl(NEW_TAB_KEYCODE, "T"), Msg::NewTab)),
                    act(Item::new("Close Tab", Accel::ctrl(CLOSE_TAB_KEYCODE, "W"), Msg::CloseTab(self.current))),
                    Item::Separator,
                    // Save is enabled on a clean buffer too: greying it would hide the name
                    // prompt of an untitled buffer behind a state a person cannot see.
                    act(Item::new("Open…", Accel::ctrl(OPEN_KEYCODE, "O"), Msg::OpenFile)),
                    act(Item::new("Save", Accel::ctrl(SAVE_KEYCODE, "S"), Msg::Save)),
                    act(Item::new("Save As…", Accel::ctrl_shift(SAVE_AS_KEYCODE, "S"), Msg::SaveAs)),
                    Item::Separator,
                    act(Item::new("New Window", Accel::ctrl_shift(NEW_WINDOW_KEYCODE, "N"), Msg::NewWindow)),
                    Item::Separator,
                    act(Item::plain("Close Window", Msg::Close)),
                    act(Item::new("Quit", Accel::ctrl(QUIT_KEYCODE, "Q"), Msg::Quit)),
                ],
            },
            Menu {
                title: "Edit",
                items: vec![
                    act(Item::new("Undo", Accel::ctrl(UNDO_KEYCODE, "Z"), Msg::Undo)),
                    act(Item::new("Redo", Accel::ctrl(REDO_KEYCODE, "Y"), Msg::Redo)),
                    Item::Separator,
                    Item::new("Cut", Accel::ctrl(CUT_KEYCODE, "X"), Msg::Cut).enabled(free && has_selection),
                    Item::new("Copy", Accel::ctrl(COPY_KEYCODE, "C"), Msg::Copy).enabled(free && has_selection),
                    act(Item::new("Paste", Accel::ctrl(PASTE_KEYCODE, "V"), Msg::Paste)),
                    // Offered whenever the buffer holds anything: making a selection is its job.
                    Item::new("Select All", Accel::ctrl(SELECT_ALL_KEYCODE, "A"), Msg::SelectAll)
                        // Never `text()`: a copy of the whole file, and this table is rebuilt on
                        // every keystroke for `accel_match`.
                        .enabled(free && self.buf().text.lines().iter().any(|l| !l.is_empty())),
                    Item::Separator,
                    act(Item::new("Find", Accel::ctrl(FIND_KEYCODE, "F"), Msg::Find)),
                    act(Item::new("Replace\u{2026}", Accel::ctrl(REPLACE_KEYCODE, "H"), Msg::Replace)),
                    act(Item::new("Go to Line\u{2026}", Accel::ctrl(GO_TO_KEYCODE, "G"), Msg::GoTo)),
                ],
            },
        ]
    }

    /// The chooser's tree, the root of a window of its own. Empty when nothing is being chosen.
    pub fn chooser_view(&mut self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        let Some(c) = self.chooser.as_mut() else {
            return libui::widget::popup_frame(padding(Insets::all(2), libui::element::text("")), ui);
        };
        // Row keys are indices, safe here because no listing happens while a chooser is open.
        let rows: Vec<libui::widget::ListRow<'_>> = c
            .entries
            .iter()
            .enumerate()
            .map(|(i, (name, _))| libui::widget::ListRow {
                key: CHOOSER_ROW_KEY + i as u64,
                label: name,
                marked: false,
                ..Default::default()
            })
            .collect();
        let dir = c.dir.clone();
        chooser::view(
            c.mode,
            &dir,
            &rows,
            &mut c.state,
            CHOOSER_KEY,
            hovered,
            Msg::ChooserRow,
            Msg::ChooserUp,
            Msg::ChooserAccept,
            Msg::ChooserCancel,
            self.chooser_focused,
            ui,
        )
        // The whole dialog takes the wheel, from outside the widget: a wheel bubbles to the
        // nearest handler, so one turned over the rows, the path strip or the buttons scrolls.
        .on_wheel(Msg::ChooserWheel)
    }

    /// The coloured runs of the lines that are on screen, recomputed every frame; only the start
    /// states are cached ([`Buffer::rescan`]).
    fn ink(&mut self, theme: &UiTheme, visible: usize) -> Vec<InkRun> {
        self.buf_mut().text.ensure_visible(visible);
        self.buf_mut().rescan();
        let b = self.buf();
        let Some(lang) = b.lang else {
            return Vec::new();
        };
        let lines = b.text.lines();
        let mut out = Vec::new();
        #[allow(clippy::needless_range_loop, reason = "the index also feeds arithmetic or a second array")]
        for i in b.text.offset()..(b.text.offset() + visible).min(lines.len()) {
            let start = b.starts.get(i).copied().unwrap_or_default();
            let (runs, _) = syntax::scan(&lang, &lines[i], start);
            out.extend(runs.iter().map(|r| InkRun {
                line: i,
                start: r.start,
                end: r.end,
                colour: colour_of(r.kind, theme),
            }));
        }
        out
    }

    /// Resolve a pointer event over the document, given a way to measure text as drawn.
    ///
    /// A press places the caret and drops the selection; a motion with a button held extends
    /// it; anything else is ignored. Answers whether anything changed.
    pub fn take_area_pointer(&mut self, width: impl Fn(&str) -> u32) -> bool {
        let Some(p) = self.area_pointer.take() else { return false };
        let pressed =
            p.kind == librsproto::surface::POINTER_BUTTON && p.flags & librsproto::surface::POINTER_PRESSED != 0;
        let dragging = p.kind == librsproto::surface::POINTER_MOTION && p.buttons & 1 != 0;
        if !pressed && !dragging {
            return false;
        }
        let (line, col) = self.buf().text.at_point(p.x, p.y, ROW_H, width);
        if pressed {
            self.buf_mut().text.place(line, col);
        } else {
            self.buf_mut().text.extend_to(line, col);
        }
        true
    }

    /// How big the buffer is, as the tab strip says it: `1173 bytes`. What would be written,
    /// not what was read.
    pub fn size_text(&self) -> String {
        let n = to_bytes(&self.buf().text.text()).len();
        if n == 1 { String::from("1 byte") } else { alloc::format!("{n} bytes") }
    }

    /// What the current buffer is being highlighted as, for a receipt: a name, since the
    /// failure to catch is "highlighted as the wrong thing", not "off".
    pub fn language_name(&self) -> &'static str {
        syntax::name_of(self.buf().lang)
    }

    /// The open menu's popup, framed — the root of a second window, not a layer in this one.
    pub fn menu_view(&self, which: usize, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        let menus = self.menu_table();
        match menus.get(which) {
            Some(m) => libui::menu::popup(m, &self.menus, MENU_ROW_KEY, hovered, ui),
            None => libui::widget::popup_frame(padding(Insets::all(2), libui::element::text("")), ui),
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

    /// Drive the chooser from the keyboard: `Esc` and `Enter` answer, the arrows move the list,
    /// and everything else is a character when there is a field to put it in. Only the
    /// application knows whether a key is text or an answer.
    pub fn chooser_key(&mut self, k: KeyEvent) -> bool {
        // `librsproto::surface::KEY_DOWN` is a key's state; `libkern::abi::KEY_DOWN` is the
        // arrow. Fully qualified, as every keycode here is, so the two cannot be confused.
        if k.pressed == 0 {
            return false;
        }
        if self.chooser.is_none() {
            return false;
        }
        match k.keycode {
            libkern::abi::KEY_ESC => self.update(Msg::ChooserCancel),
            libkern::abi::KEY_ENTER => self.update(Msg::ChooserAccept),
            code => {
                let Some(c) = self.chooser.as_mut() else { return true };
                match code {
                    libkern::abi::KEY_DOWN => {
                        let n = c.entries.len();
                        c.state.list.down(n);
                    }
                    libkern::abi::KEY_UP => {
                        c.state.list.up();
                    }
                    // Backspace goes up while opening, as in the browser. In `Save` it edits the
                    // name: navigating out from under a half-typed filename would surprise.
                    libkern::abi::KEY_BACKSPACE if c.mode == chooser::Mode::Open => {
                        self.update(Msg::ChooserUp);
                    }
                    // Nothing to type into when opening: the dialog holds the keyboard, so the
                    // key is swallowed rather than reaching nothing.
                    _ if c.mode == chooser::Mode::Save => {
                        c.state.name.apply(code, k.modifiers, k.locks);
                    }
                    _ => {}
                }
            }
        }
        true
    }

    /// The chooser this window has open, for the binary that owns its window.
    pub fn chooser(&self) -> Option<&Chooser> {
        self.chooser.as_ref()
    }

    /// The chooser, mutably — its list state moves under the pointer.
    pub fn chooser_mut(&mut self) -> Option<&mut Chooser> {
        self.chooser.as_mut()
    }

    /// A directory the binary is asked to list. Clears the record.
    pub fn take_chooser_list(&mut self) -> Option<String> {
        self.chooser_list.take()
    }

    /// A path the binary is asked to open into a tab. Clears the record.
    pub fn take_open(&mut self) -> Option<String> {
        self.open_requested.take()
    }

    /// Hand the chooser what a directory holds.
    pub fn show_chooser(&mut self, dir: &str, entries: Vec<(String, bool)>) {
        let Some(c) = self.chooser.as_mut() else { return };
        c.dir = String::from(dir);
        c.entries = entries;
        // The selection resets: a different directory has no row the old index refers to.
        c.state.list = libui::widget::ListState::at(None, 0);
    }

    /// Open the chooser, looking at the current buffer's directory, where a Save As almost
    /// always means to stay. An untitled buffer offers `home`.
    fn open_chooser(&mut self, mode: chooser::Mode) {
        let path = String::from(self.buf().path.as_str());
        let dir = match path.rfind('/') {
            Some(0) | None => String::from("/"),
            Some(i) => String::from(&path[..i]),
        };
        let dir = if path.is_empty() { String::from(&self.home) } else { dir };
        let state = match mode {
            chooser::Mode::Open => ChooserState::new(),
            // Seeded with the file's name, so Save As starts from it.
            chooser::Mode::Save => ChooserState::saving(&self.buf().name),
        };
        self.chooser_list = Some(dir.clone());
        self.chooser = Some(Chooser { mode, dir, entries: Vec::new(), state });
    }

    /// A row was activated: descend into a directory, or take a file as the answer. One
    /// statement for the pointer and `Enter`, which once disagreed about directories.
    fn chooser_activate(&mut self, row: u64) {
        let Some(c) = self.chooser.as_ref() else { return };
        let Some((name, is_dir)) = c.entries.get(row as usize).cloned() else { return };
        let path = join(&c.dir, &name);
        if is_dir {
            self.chooser_list = Some(path);
            return;
        }
        // A file is the answer in either mode; in Save it becomes the name, and the accepting
        // button still commits it (overwriting an existing file is how a Save dialog is used).
        match c.mode {
            chooser::Mode::Open => {
                self.chooser = None;
                self.open_requested = Some(path);
            }
            chooser::Mode::Save => {
                let Some(c) = self.chooser.as_mut() else { return };
                c.state.list.selected = Some(row as usize);
                c.state.name = libui::widget::TextFieldState::with_text(&name);
            }
        }
    }

    /// The chooser's answer.
    fn chooser_accept(&mut self) {
        let Some(c) = self.chooser.as_ref() else { return };
        match c.mode {
            // Through the row, so the two paths cannot disagree about directories. No
            // recursion: `chooser_activate` answers or descends, and never accepts.
            chooser::Mode::Open => {
                let Some(i) = c.state.list.selected else {
                    self.status = String::from("nothing chosen");
                    return;
                };
                self.chooser_activate(i as u64);
            }
            chooser::Mode::Save => {
                let name = String::from(c.state.name.text());
                let name = name.trim();
                if name.is_empty() {
                    // Left open: the person is mid-answer, as with the naming field.
                    self.status = String::from("a name, then Save");
                    return;
                }
                let path = join(&c.dir, name);
                self.chooser = None;
                self.adopt_path(&path);
            }
        }
    }

    /// Make `path` what the current buffer **is**, and save it there.
    ///
    /// Save As is not Save with a prompt: the tab's name, the unsaved marker and the next
    /// `Ctrl+S` all read the buffer's path. The naming field shares this.
    pub fn adopt_path(&mut self, path: &str) {
        let name = String::from(libfs::basename_str(path));
        let b = self.buf_mut();
        b.path = String::from(path);
        // The language is settled by the name, so a rename re-settles it.
        b.lang = syntax::for_name(&name);
        b.starts.clear();
        b.dirty_from = 0;
        b.scanned_at = u64::MAX;
        b.name = name;
        let key = b.key;
        self.save_requested = Some(key);
    }

    /// The state a `RequestState` is owed for. Clears the record.
    pub fn take_state_request(&mut self) -> Option<u32> {
        self.state_requested.take()
    }

    /// Whether a `StartMove` is owed. Clears the record.
    pub fn take_move_request(&mut self) -> bool {
        core::mem::take(&mut self.move_requested)
    }

    /// The edges a `StartResize` is owed for. Clears the record.
    pub fn take_resize_request(&mut self) -> Option<u32> {
        self.resize_requested.take()
    }

    /// Whether the editor has been asked to close.
    pub fn closing(&self) -> bool {
        self.closing
    }

    /// Whether a confirmation is being asked — the binary opens a dialog window to match.
    pub fn confirming(&self) -> bool {
        self.confirming.is_some()
    }

    /// How many characters have been typed into the strip's field, or `None` when none is open.
    ///
    /// The receipt for what is typed that is not the buffer (`revision` cannot see it), which
    /// says *which* field ([`Field::label`]). A count, not the text.
    pub fn naming_len(&self) -> Option<usize> {
        self.field.as_ref().map(|(_, f)| f.text().chars().count())
    }

    /// The last search's answer, for the binary to report. Clears the record.
    pub fn take_find_report(&mut self) -> Option<Option<usize>> {
        self.find_report.take()
    }

    /// What the open field is for, so the receipt can say which.
    pub fn field_kind(&self) -> Option<Field> {
        self.field.as_ref().map(|(k, _)| *k)
    }

    /// Whether a `StartMove` is owed **on the dialog's window**. Clears the record.
    pub fn take_confirm_move(&mut self) -> bool {
        core::mem::take(&mut self.confirm_move_requested)
    }

    /// The dialog could not be opened, so the question cannot be asked. The window and buffer
    /// stay: exiting would discard work, and staying `confirming` would retry on every `Close`.
    pub fn confirm_failed(&mut self) {
        self.confirming = None;
        self.status = String::from("could not ask about the unsaved buffer — save, then close");
    }

    /// What the dialog does with a key: `Esc` only, the cautious answer. There is no key for
    /// *discard*, as `Enter` is the obvious accident.
    pub fn confirm_key(&self, k: KeyEvent) -> Option<Msg> {
        if k.pressed != KEY_DOWN && k.pressed != KEY_REPEAT {
            return None;
        }
        (k.keycode == NAME_CANCEL).then_some(Msg::KeepEditing)
    }

    /// The size of this window in pixels.
    pub fn window_size(&self) -> Size {
        self.window
    }

    /// Take the window to `size`. `true` if anything changed.
    pub fn resize(&mut self, size: Size) -> bool {
        if size == self.window {
            return false;
        }
        self.window = size;
        true
    }

    /// The height the text area is laid out at — the window less its chrome and the grip.
    pub fn area_h(&self) -> u32 {
        // No `GRIP_W`: the grip overlays the status bar now, and subtracting it left a band of
        // bare window face above the bar.
        self.window.h.saturating_sub(TITLE_BAR_H + MENU_BAR_H + TAB_STRIP_H + STATUS_H + WINDOW_FRAME_H)
    }

    /// What the title bar shows: the file's name, with a leading mark when the buffer differs
    /// from the disk. The window's *title* (the taskbar's) stays the name alone.
    pub fn title(&self) -> String {
        if self.buf().name.is_empty() {
            // Named for what it is: a blank title reads as a failed load.
            return if self.modified() { String::from("* untitled") } else { String::from("untitled") };
        }
        if self.modified() { alloc::format!("* {}", self.buf().name) } else { self.buf().name.clone() }
    }

    /// The element tree for the current state; `&mut self` because `text_area` scrolls its state.
    pub fn view(&mut self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        let title = title_bar(
            self.title(),
            Some(EDITOR_KIND),
            self.focused,
            Msg::DragWindow,
            TitleButtons {
                minimise: Some(Msg::RequestState(WINDOW_STATE_MINIMIZED)),
                maximise: Some(Msg::RequestState(if self.maximized {
                    WINDOW_STATE_NORMAL
                } else {
                    WINDOW_STATE_MAXIMIZED
                })),
                close: Some(Msg::Close),
            },
            hovered,
            ui,
        )
        .key(TITLE_KEY);

        // The foot of the window: the last message on the left, the buffer and caret on the right.
        let reading = match self.field.as_ref() {
            // The field replaces the status: a name being typed is what last happened.
            Some((_, f)) => padding(
                Insets { top: 2, right: 6, bottom: 2, left: 0 },
                text_field(f, false, WidgetState { active: true, ..Default::default() }, ui),
            ),
            None => status_text(self.status.clone(), ui),
        };
        // `language · ln N, col M`, in the fixed-advance face so the numbers do not shuffle.
        let position = libui::element::row(alloc::vec![
            status_text(self.language_name(), ui),
            status_separator(ui),
            mono(status_text(self.position_text(), ui)),
        ]);
        let strip = status_bar(reading.key(STATUS_KEY), Some(position.key(POSITION_KEY)), Edge::Top, ui);

        // The tab strip is drawn whatever the count, or dropping in a file would move the
        // content under the pointer that dropped it.
        let tabs: Vec<(u64, String, bool)> = self.tabs();
        let items: Vec<libui::widget::Tab<'_>> = tabs
            .iter()
            .map(|(k, name, marked)| libui::widget::Tab { key: *k, label: name.as_str(), marked: *marked })
            .collect();
        // The file's own controls ride on the tab strip: how big the buffer is, and Save.
        let controls = libui::element::row(alloc::vec![
            center_v(mono(status_text(self.size_text(), ui))).key(BYTES_KEY),
            sized(Size::new(STATUS_GAP, 0), text("")).key(BYTES_GAP_KEY),
            center_v(sized(
                Size::new(0, PILL_H),
                pill("Save", Msg::Save, WidgetState { hovered: hovered == Some(SAVE_KEY), ..Default::default() }, ui,),
            ))
            .key(SAVE_KEY),
        ]);
        let strip_tabs = tab_strip(
            &items,
            self.current,
            hovered,
            Msg::SelectTab,
            Msg::CloseTab,
            TabExtras { new_tab: Some(Msg::NewTab), right: Some(controls) },
            ui,
        );

        let h = self.area_h();
        // Before the widget is built: `text_area` scrolls to follow the cursor first, so runs
        // computed from the old offset would be for different lines.
        let ink = self.ink(ui, (h / ROW_H) as usize);
        // The text area takes drops; the chrome does not. The compositor knows only that this
        // window declared an acceptor; which element is under the point decides the rest.
        let focused = self.focused;
        let visible = (h / ROW_H) as usize;
        let bar = self.buf().text.bar(visible);
        // The buffer is set in the fixed-advance face so columns line up; `mono` wraps only what
        // is inside it, so the chrome stays proportional.
        let area = mono(
            text_area(&mut self.buf_mut().text, h, ROW_H, focused, &ink, Some(Msg::AreaPointer), ui)
                .on_drop(Msg::Dropped)
                .on_wheel(Msg::AreaWheel),
        );
        // The line-number gutter is a sibling of the document, not a wrapper: a gutter inside
        // the area would shift every column.
        let first = self.buf().text.offset();
        let last = self.buf().text.lines().len();
        let mut numbers = alloc::vec::Vec::with_capacity(visible);
        // `row` and `ink` are taken here, so the element builders are named in full.
        for n in 0..visible {
            let line = first + n;
            let label = if line < last { alloc::format!("{}", line + 1) } else { String::new() };
            // At the body size and the top of the row, where the document sets its line: a
            // smaller number centred in the box sat three pixels low, more as `font_px` grows.
            let number = libui::element::ink(ui.foreground_dim, mono(text(label)));
            // A row centres its children, so the flexible space underneath pins the number up.
            numbers.push(sized(
                Size::new(0, ROW_H),
                column(alloc::vec![
                    libui::element::row(alloc::vec![
                        text("").flex(1),
                        number,
                        sized(Size::new(GUTTER_GAP, 0), text("")),
                    ]),
                    text("").flex(1),
                ]),
            ));
        }
        let gutter = sized(
            Size::new(GUTTER_W, 0),
            stack(alloc::vec![
                fill(ui.face_hover),
                dock(alloc::vec![docked(Edge::Right, sized(Size::new(1, 0), fill(ui.border)))], text(""),),
                padding(
                    Insets { top: libui::widget::TEXT_AREA_PAD.top, right: 0, bottom: 0, left: 0 },
                    column(numbers),
                ),
            ]),
        );

        // A scrollbar beside the document (`text_area` draws none). `AREA_INNER_KEY` stays on
        // the document itself, which tests and gates aim from.
        let pane = libui::element::row(alloc::vec![
            gutter.key(GUTTER_KEY),
            padding(Insets::all(0), area).key(AREA_INNER_KEY).flex(1),
        ]);
        let area = dock(
            alloc::vec![docked(
                Edge::Right,
                sized(Size::new(SCROLL_W, h), scrollbar(bar, SCROLL_W, h, ui).on_pointer(Msg::AreaScroll))
                    .key(AREA_BAR_KEY),
            )],
            pane.key(AREA_PANE_KEY),
        );

        window_frame_with_grip(
            title,
            dock(
                alloc::vec![
                    // The menu bar is above the tab strip, as in `nxfiles`: a dock's `Top`
                    // children stack in order, and below the strip it would read as the tab's.
                    docked(
                        Edge::Top,
                        libui::menu::bar(
                            &self.menu_table(),
                            &self.menus,
                            MENU_BAR_KEY,
                            hovered,
                            Msg::MenuBar,
                            ui,
                            MENU_BAR_H,
                        )
                        .key(BAR_KEY),
                    ),
                    docked(Edge::Top, sized(Size::new(0, TAB_STRIP_H), strip_tabs).key(TAB_STRIP_KEY),),
                    // At the foot: a status bar is a floor.
                    docked(Edge::Bottom, sized(Size::new(0, STATUS_H), strip).key(STRIP_KEY)),
                ],
                // Sized to the height it was built for, like every scrolling widget: otherwise the
                // dock's flex child gets what is left and rows are built for another height.
                sized(Size::new(0, h), area).key(AREA_KEY),
            ),
            resize_grip(Msg::ResizeWindow(RESIZE_RIGHT | RESIZE_BOTTOM), ui).key(GRIP_KEY),
            self.window,
            self.focused,
            ui,
        )
    }

    /// The element tree for the confirmation dialog — a **second window's** whole face.
    ///
    /// Sized rather than measured: a `Dock` has no natural size, so `Child::open` refuses one
    /// unsized. The file's name is on its own line, so a long name clips, never the sentence.
    pub fn confirm_view(&self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        let title = title_bar(
            "Unsaved changes",
            None,
            self.confirm_focused,
            Msg::DragConfirm,
            // One button, the cautious answer. Minimise and maximise are absent, not inert; the
            // dialog's frame must not be a third way to discard a buffer.
            TitleButtons { minimise: None, maximise: None, close: Some(Msg::KeepEditing) },
            hovered,
            ui,
        )
        .key(CONFIRM_TITLE_KEY);

        // The buffer the question is about, not always the current one: closing a tab asks about
        // that tab even if the person switched away while the dialog was up.
        let subject = match self.confirming {
            Some(Closing::Tab(k)) => self.buffers.iter().find(|b| b.key == k),
            _ => Some(self.buf()),
        };
        let name = subject.map(|b| if b.name.is_empty() { "untitled" } else { b.name.as_str() }).unwrap_or("untitled");
        let question = padding(
            Insets::all(libui::widget::DIALOG_PAD),
            column(alloc::vec![text("Discard unsaved changes?"), text(String::from(name)),]),
        )
        .key(CONFIRM_TEXT_KEY);

        // Two buttons sharing the width equally, at the published aim points.
        //
        // **Two answers and not three** — `TODO(dialog-save-answer)`. *Save and close* is the
        // obvious third, and it is not a button: an untitled buffer has nowhere to save to, so
        // it is the naming field's flow and then a close. See `deferred-decisions.md`.
        let answer = |label: &str, msg: Msg, key: u64| {
            button(label, msg, WidgetState { hovered: hovered == Some(key), ..Default::default() }, ui).key(key).flex(1)
        };
        let buttons = with_spacing(
            row(alloc::vec![
                answer("discard", Msg::Discard, CONFIRM_DISCARD_KEY),
                answer("keep editing", Msg::KeepEditing, CONFIRM_KEEP_KEY),
            ]),
            libui::widget::DIALOG_GAP,
        );
        dialog_frame(title, question, buttons, self.confirm_focused, ui)
    }
}

/// `"<what> — <n> bytes"`, the status strip's one sentence.
fn describe(bytes: usize, what: &str) -> String {
    let mut s = String::new();
    s.push_str(what);
    s.push_str(" — ");
    s.push_str(&bytes.to_string());
    s.push_str(" bytes");
    s
}

/// The rectangle the window occupies, for the binary's layout call.
pub fn bounds(size: Size) -> Rect {
    Rect::new(0, 0, size.w, size.h)
}

/// The buffer as bytes, with a trailing newline if the last line has text: every line in a
/// text file ends with one, and the editor applies that convention, not `libfs`.
pub fn to_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::from(text.as_bytes());
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut a = App::new("/home/notes.txt", "/home");
        a.loaded("hello", b"hello\n");
        a
    }

    fn key(a: &mut App, keycode: u16, modifiers: u16) {
        a.update(Msg::Key(KeyEvent::new(1, keycode, KEY_DOWN, modifiers)));
    }

    fn type_into(a: &mut App, text: &str) {
        for want in text.bytes() {
            let code = (1u16..=90)
                .find(|c| libinput::keymap::to_char(*c, 0) == Some(want))
                .unwrap_or_else(|| panic!("no keycode types {:?}", want as char));
            key(a, code, 0);
        }
    }

    fn press_key(a: &mut App, keycode: u16) {
        key(a, keycode, 0);
    }

    const KEY_X: u16 = 45;
    const KEY_1: u16 = 2;

    /// Every key this window uses names a different element.
    ///
    /// `AREA_INNER_KEY` was once 40, `MENU_BAR_KEY`, and a press on the document hit the chrome.
    #[test]
    fn every_element_key_in_this_window_is_its_own() {
        let menus = App::new("/home/x", "/home").menu_table().len() as u64;
        let mut keys: Vec<u64> = alloc::vec![
            AREA_KEY,
            AREA_INNER_KEY,
            AREA_BAR_KEY,
            SAVE_KEY,
            TITLE_KEY,
            GRIP_KEY,
            STRIP_KEY,
            STATUS_KEY,
            TAB_STRIP_KEY,
            POSITION_KEY,
            BAR_KEY,
            GUTTER_KEY,
            AREA_PANE_KEY,
            BYTES_KEY,
            BYTES_GAP_KEY,
            libui::widget::TITLE_MINIMISE_KEY,
            libui::widget::TITLE_MAXIMISE_KEY,
            libui::widget::TITLE_CLOSE_KEY,
            TAB_KEY_BASE,
            libui::widget::tab_close_key(TAB_KEY_BASE),
        ];
        keys.extend(MENU_BAR_KEY..MENU_BAR_KEY + menus);
        for k in [TAB_KEY_BASE, TAB_KEY_BASE + (1 << 32)] {
            assert_eq!(k & libui::widget::TAB_CLOSE_BIT, 0, "tab key {k:#x} sets the close bit");
        }
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), keys.len(), "two of {keys:?} are the same number");
    }

    fn width(s: &str) -> u32 {
        (s.chars().count() * 8) as u32
    }

    #[test]
    fn the_byte_count_follows_the_buffer() {
        let mut a = App::new("/home/notes.txt", "/home");
        a.loaded("", b"");
        assert_eq!(a.size_text(), "0 bytes", "an empty buffer is plural, like every other count");
        a.loaded("\n", b"\n");
        assert_eq!(a.size_text(), "1 byte", "one is singular");
        a.loaded("x", b"x\n");
        assert_eq!(a.size_text(), "2 bytes", "a character and the newline after it");
        a.loaded("hello", b"hello\n");
        assert_eq!(a.size_text(), "6 bytes");
        a.buf_mut().text.place(0, 5);
        a.buf_mut().text.insert('!');
        assert_eq!(a.size_text(), "7 bytes", "the count followed the keystroke");
    }

    #[test]
    fn a_line_number_sits_on_its_line() {
        const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSans.ttf");
        const MONO: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSansMono.ttf");
        let face = libdraw::text::Font::from_bytes(DEJAVU.to_vec())
            .expect("the vendored face")
            .with_mono(libdraw::text::Font::from_bytes(MONO.to_vec()).expect("its mono"));
        for font_px in [12.0, 16.0] {
            let ui = UiTheme { font_px, ..UiTheme::default() };
            let m = libui::paint::FontMetrics::new(&face, ui.font_px);
            let mut a = App::new("/home/notes.txt", "/home");
            // No ascenders or descenders: `1` and `l` differ in height, so comparing tops would
            // compare glyph shapes. `nn` and a digit share a baseline, so ink bottoms line up.
            a.loaded("nn\nnn\n", b"nn\nnn\n");
            a.resize(Size::new(600, 300));
            let e = a.view(&ui, None);
            let l = libui::layout::layout(&e, Rect::new(0, 0, 600, 300), &m);
            let doc = libui::layout::locate(&e, &l, AREA_INNER_KEY).expect("the document");
            let gutter = libui::layout::locate(&e, &l, GUTTER_KEY).expect("the gutter");
            assert_eq!(
                doc.origin.y, gutter.origin.y,
                "the gutter and the document start at the same height at font_px {font_px}"
            );
            // Where the ink lands, not what a constant says: comparing two `text_size_as` calls
            // with the test's own style once passed against a number a step too small.
            use libdraw::format::PixelFormat;
            use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
            let (w, h) = (600u32, 300u32);
            let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
            fb.clear(ui.background);
            let mut custom = |_: u32, _: Rect, _: Rect, _: &mut MemFramebuffer| {};
            libui::paint::paint(&mut fb, &face, &ui, &e, &l, Rect::new(0, 0, w, h), &mut custom);
            let foot_ink = |x0: u32, x1: u32, y0: u32, y1: u32| {
                (y0..y1).rev().find(|y| {
                    (x0..x1).any(|x| {
                        let c = fb.get_pixel(x, *y);
                        c.is_some()
                            && c != Some(ui.background)
                            && c != Some(ui.face_hover)
                            && c != Some(ui.border)
                            && c != Some(ui.accent)
                    })
                })
            };
            let band = (doc.origin.y as u32, doc.origin.y as u32 + ROW_H);
            let number =
                foot_ink(gutter.origin.x as u32, (gutter.origin.x + gutter.size.w as i32) as u32 - 2, band.0, band.1);
            let line = foot_ink(doc.origin.x as u32 + 4, doc.origin.x as u32 + 60, band.0, band.1);
            let number = number.expect("a number is drawn");
            let line = line.expect("a line is drawn");
            assert!(
                number.abs_diff(line) <= 1,
                "the number's ink ends at {number} and the line's at {line}, at font_px \
                 {font_px}"
            );
            // And no size step, which the ink cannot say: the small step's feet differ by a pixel.
            fn scaled_inside<M>(e: &Element<M>) -> bool {
                matches!(e.node, libui::element::Node::Scale { .. }) || e.children().any(scaled_inside)
            }
            fn keyed<M>(e: &Element<M>, key: u64) -> Option<&Element<M>> {
                if e.key == Some(key) {
                    return Some(e);
                }
                e.children().find_map(|c| keyed(c, key))
            }
            let g = keyed(&e, GUTTER_KEY).expect("the gutter is keyed");
            assert!(!scaled_inside(g), "a number set at another step than its line, at font_px {font_px}");
        }
    }

    /// The document reaches the status bar: the grip overlays the bar, not the document.
    #[test]
    fn the_document_reaches_the_status_bar() {
        let mut a = App::new("/home/notes.txt", "/home");
        a.loaded("one\ntwo\n", b"one\ntwo\n");
        a.resize(Size::new(600, 400));
        let ui = UiTheme::default();
        let e = a.view(&ui, None);
        let l = libui::layout::layout(&e, Rect::new(0, 0, 600, 400), &CELL);
        let at = |key: u64| libui::layout::locate(&e, &l, key).expect("keyed");
        let (area, strip, gutter) = (at(AREA_KEY), at(STRIP_KEY), at(GUTTER_KEY));
        assert_eq!(area.origin.y + area.size.h as i32, strip.origin.y, "the document stops short of the status bar");
        assert_eq!(gutter.origin.y + gutter.size.h as i32, strip.origin.y, "and so does the gutter's own column");
        let grip = at(GRIP_KEY);
        assert!(
            grip.origin.y >= strip.origin.y,
            "the grip is over the document rather than over the bar: {grip:?} against {strip:?}"
        );
    }

    #[test]
    fn the_gutter_numbers_the_lines_that_are_showing() {
        let mut a = App::new("/home/long.txt", "/home");
        let body: String = (1..=40).map(|n| alloc::format!("line {n}\n")).collect::<Vec<_>>().concat();
        a.loaded(&body, body.as_bytes());
        a.resize(Size::new(600, 400));
        let theme = UiTheme::default();
        let numbers = |a: &mut App| -> Vec<String> {
            let e = a.view(&theme, None);
            let size = a.window_size();
            let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &CELL);
            let g = libui::layout::locate(&e, &l, GUTTER_KEY).expect("the gutter is keyed");
            assert_eq!(g.size.w, GUTTER_W, "the gutter is the design's width");
            fn walk<M>(e: &Element<M>, out: &mut Vec<String>) {
                if let libui::element::Node::Text(t) = &e.node
                    && !t.is_empty()
                {
                    out.push(t.clone());
                }
                for c in e.children() {
                    walk(c, out);
                }
            }
            fn keyed<M>(e: &Element<M>, key: u64) -> Option<&Element<M>> {
                if e.key == Some(key) {
                    return Some(e);
                }
                e.children().find_map(|c| keyed(c, key))
            }
            let mut out = Vec::new();
            walk(keyed(&e, GUTTER_KEY).expect("keyed"), &mut out);
            out
        };
        let top = numbers(&mut a);
        assert_eq!(top.first().map(String::as_str), Some("1"), "the first line is 1");
        assert!(top.len() > 1, "and the gutter numbers more than one line");
        let visible = top.len();
        a.buf_mut().text.scroll_to(10, visible);
        let scrolled = numbers(&mut a);
        assert_eq!(scrolled.first().map(String::as_str), Some("11"), "ten lines down, the gutter starts at eleven");
        let lines = a.buf().text.lines().len();
        a.buf_mut().text.scroll_to(lines.saturating_sub(2), visible);
        let last = numbers(&mut a);
        assert_eq!(
            last.last().map(String::as_str),
            Some(alloc::format!("{lines}").as_str()),
            "the last number is the last line"
        );
        assert!(last.len() <= visible, "and the gutter does not number past the end: {last:?}");
        let mut short = App::new("/home/short.txt", "/home");
        short.loaded("one\ntwo\nthree\n", b"one\ntwo\nthree\n");
        short.resize(Size::new(600, 400));
        let shown = numbers(&mut short);
        assert_eq!(
            shown,
            alloc::vec!["1", "2", "3", "4"],
            "a three-line file with a trailing newline numbers four rows and stops"
        );
    }

    #[test]
    fn the_buffer_is_drawn_in_the_style_the_caret_is_measured_in() {
        const { assert!(BUFFER_STYLE.mono, "the buffer is fixed-advance") };
        let mut a = app();
        let e = a.view(&UiTheme::default(), None);
        fn keyed<M>(e: &Element<M>, key: u64) -> Option<&Element<M>> {
            if e.key == Some(key) {
                return Some(e);
            }
            e.children().find_map(|c| keyed(c, key))
        }
        fn has_mono<M>(e: &Element<M>) -> bool {
            matches!(e.node, libui::element::Node::Mono { .. }) || e.children().any(has_mono)
        }
        // `AREA_INNER_KEY`, not `AREA_KEY`: the outer box holds the gutter, whose numbers are
        // `mono` too, and would satisfy the assertion with the document's wrapper deleted.
        let area = keyed(&e, AREA_INNER_KEY).expect("the document is keyed");
        assert!(has_mono(area), "the document is not drawn in the face the caret is measured in");
        assert!(
            keyed(area, GUTTER_KEY).is_none(),
            "the gutter is inside the document's subtree, so `has_mono` proves nothing"
        );
    }

    /// The buffer's glyphs are *painted* at fixed advances; the other face is the control.
    #[test]
    fn the_buffers_glyphs_are_painted_at_fixed_advances() {
        use libdraw::format::PixelFormat;
        use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
        use libui::layout::Metrics as _;
        const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSans.ttf");
        const MONO: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSansMono.ttf");
        let face = libdraw::text::Font::from_bytes(DEJAVU.to_vec())
            .expect("the vendored face")
            .with_mono(libdraw::text::Font::from_bytes(MONO.to_vec()).expect("its mono"));
        let theme = UiTheme::default();
        let m = libui::paint::FontMetrics::new(&face, theme.font_px);
        let mut a = App::new("/home/notes.txt", "/home");
        a.loaded("iiiiiiii\n", b"iiiiiiii\n");
        a.resize(Size::new(600, 300));
        let (w, h) = (600u32, 300u32);
        let e = a.view(&theme, None);
        let all = Rect::new(0, 0, w, h);
        let l = libui::layout::layout(&e, all, &m);
        let doc = libui::layout::locate(&e, &l, AREA_INNER_KEY).expect("the document is keyed");
        let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
        fb.clear(theme.background);
        let mut custom = |_: u32, _: Rect, _: Rect, _: &mut MemFramebuffer| {};
        libui::paint::paint(&mut fb, &face, &theme, &e, &l, all, &mut custom);
        let row = (doc.origin.y + libui::widget::TEXT_AREA_PAD.top as i32) as u32;
        let band = row..(row + ROW_H);
        let left = doc.origin.x as u32;
        let right = (doc.origin.x + doc.size.w as i32) as u32;
        let reach = (left..right)
            .rev()
            .find(|x| {
                band.clone().any(|y| {
                    let c = fb.get_pixel(*x, y);
                    c.is_some() && c != Some(theme.background) && c != Some(theme.face_hover)
                })
            })
            .expect("the line is drawn");
        let drawn = reach - left - libui::widget::TEXT_AREA_PAD.left;
        let in_mono = m.text_size_as("iiiiiiii", BUFFER_STYLE).w;
        let proportional = m.text_size("iiiiiiii").w;
        assert!(
            proportional.abs_diff(in_mono) > 8,
            "the two faces must differ for this to discriminate: {proportional} against {in_mono}"
        );
        assert!(
            drawn.abs_diff(in_mono) <= 4,
            "the line reaches {drawn}, the fixed-advance face would put it at {in_mono} \
             (the proportional one at {proportional})"
        );
    }

    /// A press lands on the column the **drawn** face puts there (real faces, not a `FixedCell`).
    #[test]
    fn a_press_in_the_buffer_lands_where_the_mono_face_draws() {
        const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSans.ttf");
        const MONO: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSansMono.ttf");
        let face = libdraw::text::Font::from_bytes(DEJAVU.to_vec())
            .expect("the vendored face")
            .with_mono(libdraw::text::Font::from_bytes(MONO.to_vec()).expect("its mono"));
        let theme = UiTheme::default();
        use libui::layout::Metrics as _;
        let m = libui::paint::FontMetrics::new(&face, theme.font_px);
        let in_buffer = libui::element::TextStyle { mono: true, ..libui::element::TextStyle::default() };
        let mut a = App::new("/home/notes.txt", "/home");
        a.loaded("iiiimmmm", b"iiiimmmm\n");
        let size = a.window_size();
        let e = a.view(&theme, None);
        let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &m);
        let area = libui::layout::locate(&e, &l, AREA_INNER_KEY).expect("the document is keyed");
        let prefix = m.text_size_as("iiiimm", in_buffer).w as i32;
        let inset = libui::widget::TEXT_AREA_PAD;
        let x = area.origin.x + inset.left as i32 + prefix;
        let y = area.origin.y + inset.top as i32 + (ROW_H / 2) as i32;
        let mut tree = libui::diff::Tree::new();
        tree.update(&e, &l).expect("diffable");
        let mut r = libui::route::Router::new();
        let p = librsproto::surface::PointerEvent {
            kind: librsproto::surface::POINTER_BUTTON,
            button: 0x110,
            buttons: 1,
            flags: librsproto::surface::POINTER_PRESSED,
            x,
            y,
            ..Default::default()
        };
        for msg in r.pointer(&tree, &e, &l, p).0 {
            a.update(msg);
        }
        a.take_area_pointer(|s| m.text_size_as(s, in_buffer).w);
        assert_eq!(a.buf().text.cursor(), (0, 6), "the caret is in the column pressed");
        let mut b = App::new("/home/notes.txt", "/home");
        b.loaded("iiiimmmm", b"iiiimmmm\n");
        for msg in r.pointer(&tree, &e, &l, p).0 {
            b.update(msg);
        }
        b.take_area_pointer(|s| m.text_size(s).w);
        assert_ne!(
            b.buf().text.cursor(),
            (0, 6),
            "measuring the face the buffer is *not* drawn in would have to be wrong"
        );
    }

    fn area_event(a: &mut App, kind: u16, flags: u16, buttons: u16, x: i32, y: i32) {
        let theme = UiTheme::default();
        let size = a.window_size();
        let e = a.view(&theme, None);
        let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &CELL);
        let mut tree = libui::diff::Tree::new();
        tree.update(&e, &l).expect("diffable");
        let mut r = libui::route::Router::new();
        let p = librsproto::surface::PointerEvent { kind, button: 0x110, buttons, flags, x, y, ..Default::default() };
        for m in r.pointer(&tree, &e, &l, p).0 {
            a.update(m);
        }
        a.take_area_pointer(width);
    }

    fn area_origin(a: &mut App) -> (i32, i32) {
        let theme = UiTheme::default();
        let size = a.window_size();
        let e = a.view(&theme, None);
        let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &CELL);
        let r = libui::layout::locate(&e, &l, AREA_INNER_KEY).expect("the document is keyed");
        (r.origin.x, r.origin.y)
    }

    #[test]
    fn a_press_places_the_caret_where_it_landed() {
        let mut a = App::new("/home/notes.txt", "/home");
        a.loaded("hello world\nsecond line", b"hello world\nsecond line\n");
        assert_eq!(a.buf().text.cursor(), (0, 0), "precondition: at the start");

        let (ox, oy) = area_origin(&mut a);
        area_event(
            &mut a,
            librsproto::surface::POINTER_BUTTON,
            librsproto::surface::POINTER_PRESSED,
            1,
            ox + 6 + 4 * 8,
            oy + 4 + ROW_H as i32,
        );
        assert_eq!(a.buf().text.cursor(), (1, 4), "the press did not move the caret");
        assert!(a.buf().text.selection().is_none(), "a press selects nothing");
    }

    #[test]
    fn a_drag_selects_from_the_press_to_the_pointer() {
        let mut a = App::new("/home/notes.txt", "/home");
        a.loaded("hello world", b"hello world\n");
        let (ox, oy) = area_origin(&mut a);
        let y = oy + 4;
        area_event(&mut a, librsproto::surface::POINTER_BUTTON, librsproto::surface::POINTER_PRESSED, 1, ox + 6, y);
        area_event(&mut a, librsproto::surface::POINTER_MOTION, 0, 1, ox + 6 + 5 * 8, y);
        assert_eq!(a.buf().text.selected_text().as_deref(), Some("hello"), "the drag did not select from the press");

        let before = a.buf().text.cursor();
        area_event(&mut a, librsproto::surface::POINTER_MOTION, 0, 0, ox + 6, y);
        assert_eq!(a.buf().text.cursor(), before, "a hover moved the caret");
    }

    #[test]
    fn the_document_scrollbar_scrolls_and_leaves_the_caret_alone() {
        let mut a = App::new("/home/notes.txt", "/home");
        let text: String = (0..200).map(|i| alloc::format!("line {i}\n")).collect();
        a.loaded(&text, text.as_bytes());
        let theme = UiTheme::default();
        let size = a.window_size();
        let e = a.view(&theme, None);
        let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &CELL);
        let bar = libui::layout::locate(&e, &l, AREA_BAR_KEY).expect("the bar is keyed");
        let mut tree = libui::diff::Tree::new();
        tree.update(&e, &l).expect("diffable");
        let mut r = libui::route::Router::new();

        let before = a.buf().text.cursor();
        let at = |y: i32, kind: u16, flags: u16| librsproto::surface::PointerEvent {
            kind,
            button: 0x110,
            buttons: 1,
            flags,
            x: bar.origin.x + 6,
            y,
            ..Default::default()
        };
        let mid = bar.origin.y + bar.size.h as i32 / 2;
        for m in r
            .pointer(&tree, &e, &l, at(mid, librsproto::surface::POINTER_BUTTON, librsproto::surface::POINTER_PRESSED))
            .0
        {
            a.update(m);
        }
        assert!(a.buf().text.offset() > 0, "a press on the bar's middle scrolled nowhere");
        let after_press = a.buf().text.offset();
        for m in r.pointer(&tree, &e, &l, at(mid + 40, librsproto::surface::POINTER_MOTION, 0)).0 {
            a.update(m);
        }
        assert!(a.buf().text.offset() > after_press, "the drag did not follow the pointer");
        assert_eq!(a.buf().text.cursor(), before, "a scrollbar drag moved the caret");
    }

    /// Typing brings the view back to the caret, however far it was scrolled away.
    #[test]
    fn typing_brings_the_view_back_to_the_caret() {
        let mut a = App::new("/home/notes.txt", "/home");
        let text: String = (0..200).map(|i| alloc::format!("line {i}\n")).collect();
        a.loaded(&text, text.as_bytes());
        let visible = (a.area_h() / ROW_H) as usize;

        a.buf_mut().text.ensure_visible(visible);
        a.buf_mut().text.scroll_to(142, visible);
        a.buf_mut().text.ensure_visible(visible);
        assert_eq!(a.buf().text.offset(), 142, "precondition: scrolled away and staying there");

        type_into(&mut a, "a");
        a.buf_mut().text.ensure_visible(visible);
        assert_eq!(a.buf().text.offset(), 0, "the character went into a line the person cannot see");

        a.buf_mut().text.scroll_to(142, visible);
        a.buf_mut().text.ensure_visible(visible);
        assert_eq!(a.buf().text.offset(), 142, "precondition: scrolled away again");
        press_key(&mut a, libkern::abi::KEY_RIGHT);
        a.buf_mut().text.ensure_visible(visible);
        assert_eq!(a.buf().text.offset(), 0, "a caret that moved was left off screen");
    }

    /// A document scroll survives the repaint that follows it.
    #[test]
    fn a_document_scroll_survives_the_next_repaint() {
        let mut a = App::new("/home/notes.txt", "/home");
        let text: String = (0..200).map(|i| alloc::format!("line {i}\n")).collect();
        a.loaded(&text, text.as_bytes());
        let theme = UiTheme::default();
        let size = a.window_size();
        let mut tree = libui::diff::Tree::new();
        let frame = |a: &mut App, tree: &mut libui::diff::Tree| {
            let e = a.view(&theme, None);
            let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &CELL);
            tree.update(&e, &l).expect("diffable");
            (e, l)
        };
        let (e, l) = frame(&mut a, &mut tree);
        let bar = libui::layout::locate(&e, &l, AREA_BAR_KEY).expect("the bar is keyed");
        let mut r = libui::route::Router::new();
        let press = librsproto::surface::PointerEvent {
            kind: librsproto::surface::POINTER_BUTTON,
            button: 0x110,
            buttons: 1,
            flags: librsproto::surface::POINTER_PRESSED,
            x: bar.origin.x + 6,
            y: bar.origin.y + bar.size.h as i32 / 2,
            ..Default::default()
        };
        for m in r.pointer(&tree, &e, &l, press).0 {
            a.update(m);
        }
        let scrolled = a.buf().text.offset();
        assert!(scrolled > 0, "the press on the bar scrolled nowhere");

        frame(&mut a, &mut tree);
        assert_eq!(a.buf().text.offset(), scrolled, "the repaint put the document back where its caret is");
    }

    fn coloured(name: &str, text: &str) -> App {
        let mut a = App::new(&alloc::format!("/home/{name}"), "/home");
        a.loaded(text, text.as_bytes());
        a
    }

    fn type_shifted(a: &mut App, text: &str) {
        for want in text.bytes() {
            let (code, mods) = (1u16..=90)
                .find_map(|c| match libinput::keymap::to_char(c, 0) {
                    Some(got) if got == want => Some((c, 0)),
                    _ => match libinput::keymap::to_char(c, MOD_SHIFT) {
                        Some(got) if got == want => Some((c, MOD_SHIFT)),
                        _ => None,
                    },
                })
                .unwrap_or_else(|| panic!("no keycode types {:?}", want as char));
            key(a, code, mods);
        }
    }

    fn ink_of(a: &mut App) -> Vec<(usize, String, libdraw::format::Rgb)> {
        let ui = UiTheme::default();
        let lines: Vec<String> = a.buf().text.lines().to_vec();
        a.ink(&ui, 40).iter().map(|r| (r.line, String::from(&lines[r.line][r.start..r.end]), r.colour)).collect()
    }

    #[test]
    fn a_files_extension_decides_what_it_is_coloured_as() {
        let ui = UiTheme::default();
        let mut a = coloured("init.toml", "[server]\nport = 80 # here\n");
        assert_eq!(a.language_name(), "toml");
        assert_eq!(
            ink_of(&mut a),
            alloc::vec![
                (0, String::from("[server]"), ui.syntax_heading),
                (1, String::from("port"), ui.syntax_variable),
                (1, String::from("80"), ui.syntax_number),
                (1, String::from("# here"), ui.syntax_comment),
            ]
        );

        let mut a = coloured("notes.txt", "[server]\nport = 80\n");
        assert_eq!(a.language_name(), "plain");
        assert!(ink_of(&mut a).is_empty());
    }

    #[test]
    fn saving_under_a_new_name_recolours_the_buffer() {
        let mut a = coloured("untitled", "# a heading\n");
        assert!(ink_of(&mut a).is_empty(), "an unknown name colours nothing");

        a.adopt_path("/home/notes.md");
        assert_eq!(a.language_name(), "markdown");
        assert_eq!(
            ink_of(&mut a),
            alloc::vec![(0, String::from("# a heading"), UiTheme::default().syntax_heading)],
            "the rename did not re-settle the language"
        );
    }

    /// An edit above a block comment recolours the lines **below** it.
    #[test]
    fn opening_a_block_comment_recolours_the_lines_below_the_edit() {
        let ui = UiTheme::default();
        let mut a = coloured("lib.rs", "\nlet a = 1;\nlet b = 2;\n");
        let keywords = |a: &mut App| ink_of(a).iter().filter(|(_, _, c)| *c == ui.syntax_keyword).count();
        assert_eq!(keywords(&mut a), 2, "precondition: two `let`s are coloured");

        type_shifted(&mut a, "/*");
        assert_eq!(keywords(&mut a), 0, "the lines below the edit are still code");
        let runs = ink_of(&mut a);
        assert!(
            runs.iter().all(|(_, _, c)| *c == ui.syntax_comment),
            "everything below an open block comment is a comment: {runs:?}"
        );
        assert_eq!(runs.len(), 3, "one run per line, each the whole line");

        type_shifted(&mut a, "*/");
        assert_eq!(keywords(&mut a), 2, "closing the comment left the lines below stale");
    }

    /// The rescan starts at or **above** the edit: Enter leaves the cursor below it.
    #[test]
    fn a_newline_rescans_from_the_line_that_was_split_not_from_the_cursor() {
        let ui = UiTheme::default();
        let mut a = coloured("lib.rs", "let a = \"x\ny\";\n");
        assert_eq!(
            ink_of(&mut a).iter().filter(|(_, _, c)| *c == ui.syntax_keyword).count(),
            1,
            "precondition: `let` is coloured and the string runs to the next line"
        );

        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(a.buf().text.cursor().0, 1, "precondition: the cursor is below the split");
        let runs = ink_of(&mut a);
        assert!(
            runs.iter().any(|(l, t, c)| *l == 1 && t == "let" && *c == ui.syntax_keyword),
            "the line that moved down was scanned from a stale state: {runs:?}"
        );
    }

    /// A paste recolours the lines below it, as typing the same bytes does; it bypasses `update`.
    #[test]
    fn a_pasted_block_comment_recolours_the_lines_below_it() {
        let ui = UiTheme::default();
        let keywords = |a: &mut App| ink_of(a).iter().filter(|(_, _, c)| *c == ui.syntax_keyword).count();
        let mut a = coloured("lib.rs", "let a = 1;\nlet b = 2;\n");
        assert_eq!(keywords(&mut a), 2, "precondition: a frame has been drawn");

        a.pasted("/*\n", 0, 1);
        assert_eq!(keywords(&mut a), 0, "the lines below the paste kept the colours they had before it");

        // A paste that adds no line, below the top: the scan from line 0 would exit at once.
        let mut a = coloured("lib.rs", "fn f() {\nx\n    let a = 1;\n}\n");
        assert_eq!(keywords(&mut a), 2, "precondition: `fn` and `let`");
        press_key(&mut a, libkern::abi::KEY_DOWN);
        a.pasted("/*", 0, 1);
        assert_eq!(keywords(&mut a), 1, "only `fn`, above the comment, is still a keyword");

        let mut a = coloured("lib.rs", "let a = 1;\nlet b = 2;\n");
        assert_eq!(keywords(&mut a), 2);
        a.pasted("//\n", 0, 1);
        assert_eq!(keywords(&mut a), 2, "a line comment leaves the lines below it alone");
        a.cycled("/*\n", 1, 2);
        assert_eq!(keywords(&mut a), 0, "the cycled paste did not recolour below itself");
    }

    /// An edit that reports nothing still recolours correctly — slowly, rather than wrongly.
    #[test]
    fn an_edit_that_says_nothing_about_where_it_landed_still_recolours_below_itself() {
        let ui = UiTheme::default();
        let keywords = |a: &mut App| ink_of(a).iter().filter(|(_, _, c)| *c == ui.syntax_keyword).count();
        let mut a = coloured("lib.rs", "fn f() {\nx\n    let a = 1;\n}\n");
        assert_eq!(keywords(&mut a), 2, "precondition: `fn` and `let`");

        a.buf_mut().text.select_range((1, 0), (1, 0));
        a.buf_mut().text.insert_text("/*");
        assert_eq!(keywords(&mut a), 1, "the lines below an unreported edit went stale");
    }

    /// Inserting a line above a block comment does not leave the cache one line out of step.
    #[test]
    fn inserting_a_line_above_a_block_comment_does_not_shift_the_cache_under_it() {
        let ui = UiTheme::default();
        let mut a = coloured("lib.rs", "let a = 1;\n/*\nstill\n*/\n");
        let closer = |a: &mut App| ink_of(a).iter().filter(|(_, t, c)| t == "*/" && *c == ui.syntax_comment).count();
        assert_eq!(closer(&mut a), 1, "precondition: the closer is part of the comment");

        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(
            closer(&mut a),
            1,
            "the cache was read one line out of step, so the comment's closer fell outside it"
        );
    }

    #[test]
    fn a_failed_save_keeps_the_buffer_and_says_so() {
        // A failure changes the message and nothing else: the text stays and stays *modified*,
        // since a buffer marked saved can be closed without being asked.
        let mut a = app();
        key(&mut a, KEY_X, 0);
        let text = a.text();
        assert!(a.modified());

        a.saved(a.current_tab(), Err("the file could not be replaced"));
        assert_eq!(a.text(), text, "the buffer is what it was");
        assert!(a.modified(), "and it is still unsaved");
        assert!(a.status().contains("NOT saved"), "status was {:?}", a.status());

        a.update(Msg::Save);
        let (saved_key, _, owed) = a.take_save().expect("a save was asked for");
        a.saved(saved_key, Ok(owed.len()));
        assert!(!a.modified(), "a successful save is what marks it saved");
    }

    #[test]
    fn a_buffer_that_could_not_be_read_refuses_to_be_written() {
        let mut a = App::new("/home/notes.txt", "/home");
        a.blocked("could not be read");
        key(&mut a, KEY_X, 0);
        a.update(Msg::Save);
        assert_eq!(a.take_save(), None, "a blocked buffer owes no write");
        assert!(a.status().contains("not saved"), "status was {:?}", a.status());
        assert_eq!(a.refusal(), Some("could not be read"));
    }

    #[test]
    fn a_file_that_did_not_survive_being_read_opens_modified() {
        // The one case where the editor would write something it never showed: reading
        // normalises line endings, and a habitual `Ctrl+S` would rewrite a CRLF file shorter.
        let mut a = App::new("/home/dos.txt", "/home");
        a.loaded("alpha\nbeta\n", b"alpha\r\nbeta\r\n");
        assert!(a.modified(), "the buffer is not what the file holds");
        assert_eq!(a.title(), "* dos.txt", "and the title says so before anything is typed");
        assert!(a.status().contains("line endings"), "status was {:?}", a.status());
        assert_eq!(a.refusal(), None, "it is writable — it just is not the same bytes");

        a.update(Msg::Save);
        let (saved_key, _, owed) = a.take_save().expect("a blocked buffer is a different thing");
        a.saved(saved_key, Ok(owed.len()));
        assert!(!a.modified());
        assert_eq!(a.title(), "dos.txt");

        let mut b = App::new("/home/unix.txt", "/home");
        b.loaded("alpha\nbeta\n", b"alpha\nbeta\n");
        assert!(!b.modified());
    }

    #[test]
    fn a_drop_opens_a_tab_and_never_costs_the_one_that_is_open() {
        let mut a = app();
        key(&mut a, KEY_X, 0);
        assert!(a.modified(), "precondition: there is something to lose");
        let before = a.text();

        assert!(a.accept_drop("/home/other.txt"), "taken, not refused");
        assert_eq!(a.path(), "/home/other.txt", "and it is what is on screen now");
        assert_eq!(a.tab_count(), 2);

        a.update(Msg::SelectTab(TAB_KEY_BASE));
        assert_eq!(a.path(), "/home/notes.txt");
        assert_eq!(a.text(), before);
        assert!(a.modified());
    }

    #[test]
    fn dropping_a_file_that_is_already_open_switches_to_its_tab() {
        let mut a = app();
        a.accept_drop("/home/other.txt");
        assert_eq!(a.tab_count(), 2);
        a.update(Msg::SelectTab(TAB_KEY_BASE));

        assert!(!a.accept_drop("/home/other.txt"), "not a second tab");
        assert_eq!(a.tab_count(), 2);
        assert_eq!(a.path(), "/home/other.txt", "but it is what is on screen");
        assert!(a.status().contains("already open"), "status was {:?}", a.status());
    }

    #[test]
    fn dropping_the_file_already_open_does_nothing() {
        let mut a = app();
        assert!(!a.accept_drop("/home/notes.txt"));
        assert!(a.status().contains("already open"), "status was {:?}", a.status());
        assert_eq!(a.path(), "/home/notes.txt");
    }

    #[test]
    fn an_untitled_buffer_is_named_on_save_and_saved_where_it_was_named() {
        let mut a = App::new("", "/home/alice");
        assert_eq!(a.title(), "untitled", "an unnamed buffer has no title to show");
        assert!(a.take_save().is_none(), "an untitled buffer had somewhere to be written");

        a.update(Msg::Save);
        assert!(a.take_save().is_none(), "an untitled save wrote to a path nobody chose");
        assert!(a.status().contains("name it"), "status was {:?}", a.status());

        for c in [49u16, 24, 20, 18, 31] {
            key(&mut a, c, 0);
        }
        assert_eq!(a.text(), "", "typing a name went into the buffer");

        key(&mut a, libkern::abi::KEY_ENTER, 0);
        assert_eq!(a.path(), "/home/alice/notes", "named into the wrong directory");
        assert_eq!(a.title(), "notes", "the title bar still says untitled");
        assert!(a.take_save().is_some(), "confirming a name did not ask for the save");
    }

    #[test]
    fn abandoning_the_name_leaves_the_buffer_untitled_and_unwritten() {
        let mut a = App::new("", "/home/alice");
        a.update(Msg::Save);
        key(&mut a, 49, 0);
        key(&mut a, libkern::abi::KEY_ESC, 0);
        assert_eq!(a.path(), "", "escaping the prompt named the buffer anyway");
        assert!(a.take_save().is_none(), "escaping the prompt saved it anyway");

        a.update(Msg::Save);
        key(&mut a, libkern::abi::KEY_ENTER, 0);
        assert_eq!(a.path(), "", "an empty name became a path");
        assert!(a.take_save().is_none());
        assert!(a.status().contains("a name"), "status was {:?}", a.status());
    }

    #[test]
    fn a_named_buffer_saves_without_asking() {
        let mut a = app();
        a.update(Msg::Save);
        assert!(a.take_save().is_some(), "a named buffer asked for a name");
    }

    #[test]
    fn a_missing_file_is_not_a_failure() {
        let mut a = App::new("/home/new.txt", "/home");
        a.absent();
        assert_eq!(a.refusal(), None);
        assert_eq!(a.status(), "new file");
        key(&mut a, KEY_X, 0);
        a.update(Msg::Save);
        assert_eq!(
            a.take_save().map(|(_, p, t)| (p, t)),
            Some((String::from("/home/new.txt"), String::from("x"))),
            "and it can be written"
        );
    }

    #[test]
    fn the_title_marks_a_modified_buffer_and_stops_marking_it_when_saved() {
        let mut a = app();
        assert_eq!(a.title(), "notes.txt");
        key(&mut a, KEY_X, 0);
        assert_eq!(a.title(), "* notes.txt");
        a.update(Msg::Save);
        let (saved_key, _, owed) = a.take_save().unwrap();
        a.saved(saved_key, Ok(owed.len()));
        assert_eq!(a.title(), "notes.txt");
        key(&mut a, KEY_X, 0);
        assert_eq!(a.title(), "* notes.txt");
    }

    #[test]
    fn ctrl_s_saves_and_no_chord_types() {
        let mut a = app();
        key(&mut a, SAVE_KEYCODE, MOD_CTRL);
        assert_eq!(a.text(), "hello", "nothing was typed");
        assert_eq!(a.take_save().map(|(_, _, t)| t).as_deref(), Some("hello"), "and a save is owed");

        // A digit, because a letter proves nothing: `libinput` folds `Ctrl`+letter to a control
        // byte that `apply` declines, so `Ctrl+X` passes with this guard deleted. `Ctrl+1` is
        // still printable `1`.
        key(&mut a, KEY_1, MOD_CTRL);
        assert_eq!(a.text(), "hello", "a chord this editor does not have types nothing");
        assert_eq!(a.take_save(), None, "and asks for nothing");

        key(&mut a, KEY_1, 0);
        assert_eq!(a.text(), "1hello");
    }

    #[test]
    fn typing_reaches_the_buffer_and_enter_makes_a_line() {
        let mut a = app();
        key(&mut a, libkern::abi::KEY_END, 0);
        key(&mut a, KEY_X, 0);
        key(&mut a, libkern::abi::KEY_ENTER, 0);
        key(&mut a, KEY_X, 0);
        assert_eq!(a.text(), "hellox\nx");
    }

    #[test]
    fn a_key_release_does_nothing() {
        let mut a = app();
        a.update(Msg::Key(KeyEvent::new(1, KEY_X, 0, 0)));
        assert_eq!(a.text(), "hello");
    }

    #[test]
    fn what_is_written_ends_with_a_newline() {
        assert_eq!(to_bytes("one\ntwo"), b"one\ntwo\n");
        assert_eq!(to_bytes("one\n"), b"one\n", "and does not double it");
        assert_eq!(to_bytes(""), b"", "an empty buffer is an empty file, not a blank line");
    }

    #[test]
    fn the_name_is_the_last_component() {
        let named = |p: &str| App::new(p, "/home").title();
        assert_eq!(named("/home/alice/notes.txt"), "notes.txt");
        assert_eq!(named("notes.txt"), "notes.txt");
        assert_eq!(named("/home/papers/"), "papers", "a trailing separator is not the name");
    }

    #[test]
    fn the_chord_keycodes_are_the_ones_the_keymap_names() {
        assert_eq!(libinput::keymap::to_char(SELECT_ALL_KEYCODE, 0), Some(b'a'));
        assert_eq!(libinput::keymap::to_char(REPLACE_KEYCODE, 0), Some(b'h'));
        assert_eq!(libinput::keymap::to_char(GO_TO_KEYCODE, 0), Some(b'g'));
        assert_eq!(libinput::keymap::to_char(UNDO_KEYCODE, 0), Some(b'z'));
        assert_eq!(libinput::keymap::to_char(REDO_KEYCODE, 0), Some(b'y'));
        assert_eq!(libinput::keymap::to_char(FIND_KEYCODE, 0), Some(b'f'));
    }

    #[test]
    fn ctrl_z_undoes_and_ctrl_y_comes_back() {
        let mut a = app();
        key(&mut a, KEY_X, 0);
        key(&mut a, KEY_X, 0);
        assert_eq!(a.text(), "xxhello", "two characters, one group — the cursor opens at the start");

        key(&mut a, UNDO_KEYCODE, MOD_CTRL);
        assert_eq!(a.text(), "hello", "the group, not the character");
        assert!(a.status().contains("undone"), "status was {:?}", a.status());

        key(&mut a, REDO_KEYCODE, MOD_CTRL);
        assert_eq!(a.text(), "xxhello");
    }

    #[test]
    fn a_save_is_an_undo_boundary() {
        // Found by a gate: six typed, saved, two more typed, and one undo emptied the buffer
        // because nothing closed the group across the save.
        let mut a = App::new("/home/notes.txt", "/home");
        a.loaded("", b"");
        for _ in 0..2 {
            key(&mut a, KEY_X, 0);
        }
        a.update(Msg::Save);
        let (saved_key, _, owed) = a.take_save().expect("a save was asked for");
        a.saved(saved_key, Ok(owed.len()));
        key(&mut a, KEY_X, 0);
        assert_eq!(a.text(), "xxx");

        key(&mut a, UNDO_KEYCODE, MOD_CTRL);
        assert_eq!(a.text(), "xx", "back to what was saved, not to what was opened");
        key(&mut a, UNDO_KEYCODE, MOD_CTRL);
        assert_eq!(a.text(), "", "and past it, one group at a time");
    }

    #[test]
    fn an_undo_with_nothing_behind_it_says_so_rather_than_nothing() {
        let mut a = app();
        key(&mut a, UNDO_KEYCODE, MOD_CTRL);
        assert!(a.status().contains("nothing to undo"), "status was {:?}", a.status());
        key(&mut a, REDO_KEYCODE, MOD_CTRL);
        assert!(a.status().contains("nothing to redo"), "status was {:?}", a.status());
    }

    #[test]
    fn undoing_makes_the_buffer_modified_again() {
        let mut a = app();
        key(&mut a, KEY_X, 0);
        a.update(Msg::Save);
        let (saved_key, _, owed) = a.take_save().expect("a save was asked for");
        a.saved(saved_key, Ok(owed.len()));
        assert!(!a.modified());

        key(&mut a, UNDO_KEYCODE, MOD_CTRL);
        assert!(a.modified(), "the buffer is not what was written any more");
    }

    #[test]
    fn ctrl_f_opens_a_find_field_and_enter_walks_the_matches() {
        let mut a = App::new("/home/notes.txt", "/home");
        a.loaded("one two\nthree two", b"one two\nthree two\n");
        key(&mut a, FIND_KEYCODE, MOD_CTRL);
        assert_eq!(a.field_kind(), Some(Field::Finding), "the keys are the field's now");

        for code in [20u16, 17, 24] {
            key(&mut a, code, 0);
        }
        assert_eq!(a.text(), "one two\nthree two", "the buffer took none of it");
        assert_eq!(a.naming_len(), Some(3));

        key(&mut a, libkern::abi::KEY_ENTER, 0);
        assert!(a.status().contains("found"), "status was {:?}", a.status());
        assert_eq!(a.field_kind(), Some(Field::Finding));

        key(&mut a, libkern::abi::KEY_ESC, 0);
        assert_eq!(a.field_kind(), None, "and Escape ends it");
    }

    fn select(a: &mut App, needle: &str) {
        assert!(a.buf_mut().text.find(needle), "the fixture contains {needle}");
    }

    #[test]
    fn copying_nothing_is_not_a_request() {
        // An empty entry would move the ring's serial under a client mid-cycle. The Edit row is
        // greyed, so the table declines the chord; reaching `copy` another way gets the message.
        let mut a = app();
        let before = String::from(a.status());
        key(&mut a, COPY_KEYCODE, MOD_CTRL);
        assert_eq!(a.take_clip_request(), None);
        assert_eq!(a.status(), before, "a greyed row's chord does nothing, silently");
        let edit = a.menu_table().into_iter().find(|m| m.title == "Edit").expect("an Edit menu");
        for row in ["Cut", "Copy"] {
            assert!(
                edit.items.iter().any(|it| matches!(
                    it,
                    Item::Action { label, enabled: false, .. } if *label == row
                )),
                "{row} is offered with nothing selected"
            );
        }
        a.update(Msg::Copy);
        assert_eq!(a.take_clip_request(), None);
        assert_eq!(a.status(), "nothing to copy");
        select(&mut a, "ell");
        key(&mut a, COPY_KEYCODE, MOD_CTRL);
        assert_eq!(a.take_clip_request(), Some(ClipRequest::Copy(String::from("ell"))));
    }

    /// No menu row acts while a field is open: Find once replaced a naming field.
    #[test]
    fn no_menu_row_acts_while_a_field_is_open() {
        let mut a = App::new("", "/home");
        a.update(Msg::Save);
        key(&mut a, 30, 0); // `a`, one character of the filename
        assert_eq!(a.field_kind(), Some(Field::Naming));
        assert_eq!(a.naming_len(), Some(1), "a name is half typed");

        for it in a.menu_table().iter().flat_map(|m| m.items.iter()) {
            let Item::Action { label, enabled, .. } = it else { continue };
            assert!(!enabled, "{label} is offered while a name is being typed");
        }
        assert_eq!(a.field_kind(), Some(Field::Naming));
        assert_eq!(a.naming_len(), Some(1));

        key(&mut a, libkern::abi::KEY_ESC, 0);
        assert_eq!(a.field_kind(), None, "Esc closed the prompt");
        let live = a
            .menu_table()
            .iter()
            .flat_map(|m| m.items.iter())
            .filter(|it| matches!(it, Item::Action { enabled: true, .. }))
            .count();
        assert_eq!(live, 14, "everything but Cut and Copy, which want a selection");
    }

    #[test]
    fn select_all_takes_the_buffer_and_an_empty_one_stays_empty() {
        let mut a = app();
        a.loaded("one\ntwo\nthree", b"one\ntwo\nthree");
        a.update(Msg::SelectAll);
        assert_eq!(a.buf().text.selected_text().as_deref(), Some("one\ntwo\nthree"), "every line, including the last");

        let mut b = app();
        b.loaded("", b"");
        b.update(Msg::SelectAll);
        assert!(b.buf().text.selection().is_none(), "nothing to select");
        assert!(!b.buf().text.has_selection(), "and nothing for Copy to find");

        let mut c = app();
        c.loaded("one\néxy", "one\néxy".as_bytes());
        c.update(Msg::SelectAll);
        assert_eq!(c.buf().text.selected_text().as_deref(), Some("one\néxy"), "the last character is not dropped");
    }

    #[test]
    fn replace_walks_the_matches_one_enter_at_a_time() {
        let mut a = app();
        a.loaded("a cat and a cat", b"a cat and a cat");
        a.update(Msg::Replace);
        assert_eq!(a.field_kind(), Some(Field::ReplaceFind));

        type_into(&mut a, "cat");
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(a.field_kind(), Some(Field::ReplaceWith), "the second stage");
        assert_eq!(a.status(), "replace cat with, then Enter");

        type_into(&mut a, "dog");
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(a.text(), "a dog and a cat", "the first match only");
        assert_eq!(a.status(), "replaced 1, Enter for the next");

        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(a.text(), "a dog and a dog", "and the second");
        assert_eq!(a.status(), "replaced 2");
        assert_eq!(a.field_kind(), Some(Field::ReplaceWith), "still open");
    }

    /// A needle that is not in the buffer does not consume somebody's selection.
    #[test]
    fn replacing_an_absent_needle_leaves_the_selection_alone() {
        let mut a = app();
        a.loaded("hello world", b"hello world");
        a.buf_mut().text.select_range((0, 6), (0, 11));
        assert_eq!(a.buf().text.selected_text().as_deref(), Some("world"));

        a.update(Msg::Replace);
        type_into(&mut a, "zzz");
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(a.status(), "no zzz");
        assert_eq!(
            a.field_kind(),
            Some(Field::ReplaceFind),
            "it stays on the first stage rather than asking what to replace it with"
        );

        type_into(&mut a, "qq");
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(a.text(), "hello world", "the buffer is untouched");
    }

    /// A selection made *between* the two stages is not what gets replaced.
    #[test]
    fn a_selection_made_between_the_stages_is_not_replaced() {
        let mut a = app();
        a.loaded("cat and dog", b"cat and dog");
        a.update(Msg::Replace);
        type_into(&mut a, "cat");
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(a.field_kind(), Some(Field::ReplaceWith), "the match was found");

        a.buf_mut().text.select_range((0, 8), (0, 11));
        assert_eq!(a.buf().text.selected_text().as_deref(), Some("dog"));

        type_into(&mut a, "zzz");
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(a.text(), "cat and dog", "`dog` was not replaced");
    }

    #[test]
    fn replacing_nothing_is_refused() {
        let mut a = app();
        a.loaded("hello", b"hello");
        a.update(Msg::Replace);
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(a.field_kind(), None, "the field closed");
        assert_eq!(a.status(), "nothing to replace");
        assert_eq!(a.text(), "hello", "and the buffer is untouched");
    }

    #[test]
    fn the_strip_reports_the_position_a_person_counts() {
        let mut a = app();
        a.loaded("one\ntwo", b"one\ntwo");
        assert_eq!(a.position_text(), "line 1, col 1", "counted from one, not zero");

        a.buf_mut().text.place(1, 3);
        assert_eq!(a.position_text(), "line 2, col 4");

        let mut b = app();
        b.loaded("é!", "é!".as_bytes());
        b.buf_mut().text.place(0, 2);
        assert_eq!(b.position_text(), "line 1, col 2", "characters, not bytes");
    }

    #[test]
    fn go_to_line_clamps_and_refuses() {
        let mut a = app();
        a.loaded("one\ntwo\nthree", b"one\ntwo\nthree");

        a.update(Msg::GoTo);
        assert_eq!(a.field_kind(), Some(Field::GoingTo));
        type_into(&mut a, "2");
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(a.buf().text.cursor().0, 1, "line 2 is index 1");
        assert_eq!(a.field_kind(), None, "and the field closed");

        a.update(Msg::GoTo);
        type_into(&mut a, "900");
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(a.buf().text.cursor().0, 2, "clamped to the last line");
        // `place` clamps too: the strip's message and the scroll line are what differ.
        assert_eq!(a.status(), "line 3", "and says where it actually went");
        assert_eq!(a.take_find_report(), Some(Some(2)), "the scroll target is the real line");

        a.update(Msg::GoTo);
        type_into(&mut a, "x");
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(a.status(), "x is not a line number");
        assert_eq!(a.field_kind(), Some(Field::GoingTo), "and the field stays open");
    }

    /// The chord a menu row advertises does what choosing that row does, via `App::key`.
    ///
    /// Stated as an equivalence: two editors, one sent the message and one the chord, end alike.
    #[test]
    fn every_advertised_chord_does_what_its_row_says() {
        fn digest(a: &mut App) -> String {
            let clip = a.take_clip_request();
            let save = a.take_save();
            // Every outbox: several rows change nothing a digest of the buffer alone can see.
            let (nw, q) = (a.take_new_window(), a.take_quit());
            let (cl, op) = (a.take_chooser_list(), a.take_open());
            let choosing = a.chooser().map(|c| c.mode);
            // `closing` and `save` are here because the negative control found them missing; so is
            // the selection's extent (Select All widens it).
            let sel = a.buf().text.selection();
            alloc::format!(
                "{:?}|{}|{:?}|{clip:?}|{save:?}|{}|{:?}|{}{}|{nw}{q}|{cl:?}{op:?}{choosing:?}|{sel:?}",
                a.status(),
                a.text(),
                a.field_kind(),
                a.tabs().len(),
                a.naming_len(),
                a.closing(),
                a.confirming(),
            )
        }
        let mut fixture = app();
        select(&mut fixture, "ell");
        let table = fixture.menu_table();
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
            let (mut by_row, mut by_chord) = (app(), app());
            for a in [&mut by_row, &mut by_chord] {
                select(a, "ell");
            }
            by_row.update(msg.clone());
            by_chord.update(Msg::Key(ev));
            let mut untouched = app();
            select(&mut untouched, "ell");
            // Each digest is taken once: it drains the outboxes.
            let (by_row, by_chord, untouched) = (digest(&mut by_row), digest(&mut by_chord), digest(&mut untouched));
            assert_eq!(by_chord, by_row, "{label}: {} does not do what choosing the row does", acc.label());
            assert_ne!(by_row, untouched, "{label} changes nothing, so this row proves nothing about routing");
        }
        assert_eq!(checked, 16, "every row but Close Window carries a chord");
    }

    #[test]
    fn copy_takes_the_selection_and_cut_also_removes_it() {
        let mut a = app();
        select(&mut a, "ell");
        key(&mut a, COPY_KEYCODE, MOD_CTRL);
        assert_eq!(a.take_clip_request(), Some(ClipRequest::Copy(String::from("ell"))));
        assert_eq!(a.text(), "hello", "a copy does not change the buffer");

        select(&mut a, "ell");
        key(&mut a, CUT_KEYCODE, MOD_CTRL);
        assert_eq!(a.take_clip_request(), Some(ClipRequest::Copy(String::from("ell"))));
        assert_eq!(a.text(), "ho", "a cut does");
    }

    #[test]
    fn the_shift_is_what_makes_a_paste_a_cycle() {
        let mut a = app();
        key(&mut a, PASTE_KEYCODE, MOD_CTRL);
        assert_eq!(a.take_clip_request(), Some(ClipRequest::Paste));
        key(&mut a, PASTE_KEYCODE, MOD_CTRL | MOD_SHIFT);
        assert_eq!(a.take_clip_request(), Some(ClipRequest::Cycle));
    }

    #[test]
    fn a_paste_arms_a_cycle_and_the_cycle_replaces_what_it_inserted() {
        let mut a = app();
        a.pasted("WORLD", 0, 7);
        assert_eq!(a.text(), "WORLDhello");
        let c = a.cycling().expect("a paste arms a cycle");
        assert_eq!((c.index, c.serial), (0, 7));

        a.cycled("MARS", 1, 7);
        assert_eq!(a.text(), "MARShello", "the cycle replaced the paste, not the buffer");
        assert_eq!(a.cycling().map(|c| c.index), Some(1), "and armed the next one");
    }

    #[test]
    fn anything_but_the_cycle_chord_ends_the_sequence() {
        // The position exists only inside one uninterrupted gesture; anything else ends it.
        for (name, act) in [
            ("a typed character", (|a: &mut App| key(a, KEY_X, 0)) as fn(&mut App)),
            ("a copy chord", |a| key(a, COPY_KEYCODE, MOD_CTRL)),
            ("a plain paste", |a| key(a, PASTE_KEYCODE, MOD_CTRL)),
            ("a save", |a| a.update(Msg::Save)),
            ("a new tab", |a| a.update(Msg::NewTab)),
        ] {
            let mut a = app();
            a.pasted("WORLD", 0, 7);
            assert!(a.cycling().is_some(), "{name}: precondition");
            act(&mut a);
            assert_eq!(a.cycling(), None, "{name} should have ended the sequence");
        }
        let mut a = app();
        a.pasted("WORLD", 0, 7);
        key(&mut a, PASTE_KEYCODE, MOD_CTRL | MOD_SHIFT);
        assert!(a.cycling().is_some(), "the one key that continues it");
    }

    #[test]
    fn a_cycle_into_a_different_tab_does_nothing() {
        let mut a = app();
        a.pasted("WORLD", 0, 7);
        let c = a.cycling().expect("armed");
        a.update(Msg::NewTab);
        a.cycling = Some(c);
        let before = a.text();
        a.cycled("MARS", 1, 7);
        assert_eq!(a.text(), before, "a stale buffer key is not a cycle");
        assert_eq!(a.text(), "", "…and the new tab is still the empty one");
    }

    #[test]
    fn a_paste_is_one_undo_step() {
        let mut a = app();
        key(&mut a, KEY_X, 0);
        a.pasted("WORLD", 0, 7);
        assert_eq!(a.text(), "xWORLDhello");
        key(&mut a, UNDO_KEYCODE, MOD_CTRL);
        assert_eq!(a.text(), "xhello", "the paste came back out and the typing stayed");
    }

    #[test]
    fn a_multi_line_paste_keeps_what_followed_the_cursor() {
        let mut a = app();
        a.pasted("one\ntwo", 0, 1);
        assert_eq!(a.text(), "one\ntwohello");
    }

    #[test]
    fn a_tab_chord_during_a_find_still_opens_a_tab() {
        // Tab chords act on the window, before the prompt; buffer chords stay the field's.
        let mut a = app();
        key(&mut a, FIND_KEYCODE, MOD_CTRL);
        for code in [20u16, 19, 24] {
            key(&mut a, code, 0);
        }
        assert_eq!(a.naming_len(), Some(3), "precondition: the field has the keys");

        key(&mut a, NEW_TAB_KEYCODE, MOD_CTRL);
        assert_eq!(a.tab_count(), 2, "the chord reached the window");

        let mut a = App::new("", "/home");
        a.absent();
        a.update(Msg::Save);
        assert_eq!(a.field_kind(), Some(Field::Naming));
        key(&mut a, KEY_X, 0);
        key(&mut a, SAVE_KEYCODE, MOD_CTRL);
        assert_eq!(a.naming_len(), Some(1), "the name survives, and nothing was saved");
        assert!(a.take_save().is_none());
    }

    #[test]
    fn a_save_writes_the_buffer_that_asked_even_if_the_tab_changes_first() {
        // `Ctrl+S` only records that a save is owed; a tab click in the same drain once moved the
        // bytes and the path to the other buffer.
        let mut a = app(); // /home/notes.txt holding "hello"
        a.accept_drop("/home/other.txt");
        a.absent();
        let notes = a.tabs()[0].0;
        let other = a.tabs()[1].0;

        a.update(Msg::SelectTab(notes));
        key(&mut a, KEY_X, 0);
        a.update(Msg::Save);
        a.update(Msg::SelectTab(other));

        let (asked, path, text) = a.take_save().expect("a save was asked for");
        assert_eq!(asked, notes, "the buffer that asked, not the one on screen");
        assert_eq!(path, "/home/notes.txt");
        assert_eq!(text, "xhello");

        a.saved(asked, Ok(text.len()));
        a.update(Msg::SelectTab(notes));
        assert!(!a.modified(), "the buffer that was written is the one marked saved");
        a.update(Msg::SelectTab(other));
        assert!(!a.modified(), "and the empty one was never dirty");
    }

    #[test]
    fn tabs_hold_their_own_buffer_cursor_and_history() {
        let mut a = app(); // /home/notes.txt holding "hello"
        key(&mut a, KEY_X, 0);
        key(&mut a, KEY_X, 0);
        let first = a.text();

        a.update(Msg::NewTab);
        assert_eq!(a.tab_count(), 2);
        assert_eq!(a.text(), "", "a new tab is an untitled empty buffer");
        assert_eq!(a.path(), "");

        key(&mut a, UNDO_KEYCODE, MOD_CTRL);
        assert!(a.status().contains("nothing to undo"), "status was {:?}", a.status());

        a.update(Msg::SelectTab(TAB_KEY_BASE));
        assert_eq!(a.text(), first);
        key(&mut a, UNDO_KEYCODE, MOD_CTRL);
        assert_eq!(a.text(), "hello", "its own group, in its own tab");
    }

    #[test]
    fn the_strip_marks_the_tabs_that_are_unsaved() {
        let mut a = app();
        a.update(Msg::NewTab);
        a.update(Msg::SelectTab(TAB_KEY_BASE));
        key(&mut a, KEY_X, 0);
        let tabs = a.tabs();
        assert_eq!(tabs.len(), 2);
        assert_eq!((tabs[0].1.as_str(), tabs[0].2), ("notes.txt", true));
        assert_eq!((tabs[1].1.as_str(), tabs[1].2), ("untitled", false));
    }

    #[test]
    fn closing_a_modified_tab_asks_and_only_the_answer_removes_it() {
        let mut a = app();
        a.update(Msg::NewTab);
        a.update(Msg::SelectTab(TAB_KEY_BASE));
        key(&mut a, KEY_X, 0);

        a.update(Msg::CloseTab(TAB_KEY_BASE));
        assert!(a.confirming(), "a modified tab asks");
        assert_eq!(a.tab_count(), 2, "and nothing has gone");
        assert!(!a.closing(), "and the window is not closing");

        a.update(Msg::KeepEditing);
        assert_eq!(a.tab_count(), 2);

        a.update(Msg::CloseTab(TAB_KEY_BASE));
        a.update(Msg::Discard);
        assert_eq!(a.tab_count(), 1, "the tab, not the window");
        assert!(!a.closing());
        assert_eq!(a.path(), "", "and the untitled one is what is left");
    }

    #[test]
    fn a_question_about_one_tab_survives_switching_to_another() {
        // The key is captured when asked, so switching tabs cannot retarget the answer.
        let mut a = app();
        key(&mut a, KEY_X, 0); // notes.txt is modified
        a.update(Msg::NewTab);
        a.update(Msg::SelectTab(TAB_KEY_BASE));
        a.update(Msg::CloseTab(TAB_KEY_BASE));
        assert!(a.confirming());

        a.update(Msg::SelectTab(TAB_KEY_BASE + 1)); // the person switches while it is asking
        assert_eq!(a.path(), "");
        a.update(Msg::Discard);
        assert_eq!(a.tab_count(), 1);
        assert_eq!(a.path(), "", "the untitled tab is what survived — tab 0 was the subject");
    }

    #[test]
    fn closing_the_last_tab_closes_the_window() {
        let mut a = app();
        assert_eq!(a.tab_count(), 1);
        a.update(Msg::CloseTab(TAB_KEY_BASE));
        assert!(a.closing(), "the last tab takes the window with it");
    }

    #[test]
    fn closing_the_window_asks_about_any_modified_tab_not_just_the_current_one() {
        let mut a = app();
        key(&mut a, KEY_X, 0); // tab 0 modified
        a.update(Msg::NewTab); // tab 1 clean, and current
        assert!(!a.modified(), "the current buffer has nothing to lose");

        a.update(Msg::Close);
        assert!(a.confirming(), "but another tab does");
        assert!(!a.closing());
    }

    #[test]
    fn save_while_finding_asks_for_a_name_rather_than_doing_nothing() {
        // The save button stays clickable during a search, and once did nothing there.
        let mut a = App::new("", "/home");
        a.absent();
        key(&mut a, FIND_KEYCODE, MOD_CTRL);
        assert_eq!(a.field_kind(), Some(Field::Finding));

        a.update(Msg::Save);
        assert_eq!(a.field_kind(), Some(Field::Naming), "the search gives way to the question");
        assert!(a.status().contains("name it"), "status was {:?}", a.status());

        key(&mut a, KEY_X, 0);
        a.update(Msg::Save);
        assert_eq!(a.naming_len(), Some(1), "the name survives a second press");
    }

    #[test]
    fn escaping_a_search_leaves_the_strip_saying_what_last_happened() {
        let mut a = App::new("/home/notes.txt", "/home");
        a.loaded("one two", b"one two\n");
        key(&mut a, FIND_KEYCODE, MOD_CTRL);
        for code in [20u16, 17, 24] {
            key(&mut a, code, 0);
        }
        key(&mut a, libkern::abi::KEY_ENTER, 0);
        let after_find = String::from(a.status());
        assert!(after_find.contains("found"));
        key(&mut a, libkern::abi::KEY_ESC, 0);
        assert_eq!(a.status(), after_find, "escaping a search erases nothing");
    }

    #[test]
    fn typing_after_a_find_replaces_what_was_found() {
        // Find leaves its match selected and typing replaces it (a gate once hit this).
        let mut a = App::new("/home/notes.txt", "/home");
        a.loaded("nitroxab", b"nitroxab\n");
        key(&mut a, FIND_KEYCODE, MOD_CTRL);
        for code in [20u16, 19, 24] {
            key(&mut a, code, 0);
        }
        key(&mut a, libkern::abi::KEY_ENTER, 0);
        key(&mut a, libkern::abi::KEY_ESC, 0);

        key(&mut a, KEY_X, 0);
        assert_eq!(a.text(), "nixxab", "the match was replaced, not appended to");

        let mut a = App::new("/home/notes.txt", "/home");
        a.loaded("nitroxab", b"nitroxab\n");
        key(&mut a, FIND_KEYCODE, MOD_CTRL);
        for code in [20u16, 19, 24] {
            key(&mut a, code, 0);
        }
        key(&mut a, libkern::abi::KEY_ENTER, 0);
        key(&mut a, libkern::abi::KEY_ESC, 0);
        key(&mut a, libkern::abi::KEY_END, 0);
        key(&mut a, KEY_X, 0);
        assert_eq!(a.text(), "nitroxabx");
    }

    #[test]
    fn a_search_that_finds_nothing_says_so() {
        let mut a = app();
        key(&mut a, FIND_KEYCODE, MOD_CTRL);
        key(&mut a, KEY_X, 0);
        key(&mut a, libkern::abi::KEY_ENTER, 0);
        assert!(a.status().starts_with("no "), "status was {:?}", a.status());
        assert_eq!(a.text(), "hello", "and a failed search edits nothing");
    }

    #[test]
    fn a_chord_the_editor_does_not_have_still_types_nothing() {
        let mut a = app();
        let before = a.text();
        key(&mut a, KEY_1, MOD_CTRL);
        assert_eq!(a.text(), before);
    }

    #[test]
    fn the_save_keycode_is_the_one_the_keymap_calls_s() {
        assert_eq!(libinput::keymap::to_char(SAVE_KEYCODE, 0), Some(b's'));
    }

    #[test]
    fn closing_a_modified_buffer_asks_rather_than_exiting() {
        let mut a = app();
        key(&mut a, KEY_X, 0);
        a.update(Msg::Close);
        assert!(a.confirming(), "a modified buffer asks");
        assert!(!a.closing(), "and does not exit while it is asking");
    }

    #[test]
    fn closing_a_clean_buffer_asks_nothing() {
        let mut a = app();
        a.update(Msg::Close);
        assert!(!a.confirming(), "nothing to ask about");
        assert!(a.closing());
    }

    #[test]
    fn a_second_close_request_does_not_discard_the_buffer() {
        // "Ask unless already asking" would turn a second `CloseRequested` into an exit.
        let mut a = app();
        key(&mut a, KEY_X, 0);
        a.update(Msg::Close);
        a.update(Msg::Close);
        assert!(a.confirming(), "still asking");
        assert!(!a.closing(), "a repeated ask is not an answer");
    }

    #[test]
    fn only_discard_ends_the_run() {
        let mut a = app();
        key(&mut a, KEY_X, 0);
        a.update(Msg::Close);

        let text = a.text();
        a.update(Msg::KeepEditing);
        assert!(!a.confirming(), "the question is answered");
        assert!(!a.closing(), "and the answer was no");
        assert_eq!(a.text(), text);
        assert!(a.modified(), "keeping does not mark it saved");
        assert!(a.status().contains("still editing"), "status was {:?}", a.status());

        a.update(Msg::Close);
        assert!(a.confirming());
        a.update(Msg::Discard);
        assert!(!a.confirming(), "the dialog goes when it is answered");
        assert!(a.closing());
    }

    #[test]
    fn saving_removes_the_reason_to_ask() {
        let mut a = app();
        key(&mut a, KEY_X, 0);
        a.update(Msg::Save);
        let (saved_key, _, owed) = a.take_save().expect("a save was asked for");
        a.saved(saved_key, Ok(owed.len()));
        assert!(!a.modified());

        a.update(Msg::Close);
        assert!(!a.confirming(), "nothing unsaved to ask about");
        assert!(a.closing());
    }

    #[test]
    fn the_dialog_answers_escape_and_nothing_else() {
        let a = app();
        let ev = |code: u16, pressed: u16| KeyEvent::new(1, code, pressed, 0);
        assert_eq!(a.confirm_key(ev(NAME_CANCEL, KEY_DOWN)), Some(Msg::KeepEditing));
        assert_eq!(a.confirm_key(ev(NAME_CONFIRM, KEY_DOWN)), None, "Enter must not discard");
        assert_eq!(a.confirm_key(ev(KEY_X, KEY_DOWN)), None);
        // A release is not a press: otherwise `Esc` answers twice, once after the window is gone.
        assert_eq!(a.confirm_key(ev(NAME_CANCEL, 0)), None, "a release is not an answer");
    }

    #[test]
    fn a_dialog_that_will_not_open_leaves_the_window_alone() {
        // Exiting would discard work; staying `confirming` would retry the dialog on every `Close`.
        let mut a = app();
        key(&mut a, KEY_X, 0);
        a.update(Msg::Close);
        a.confirm_failed();
        assert!(!a.confirming());
        assert!(!a.closing(), "a failed dialog must not discard the buffer");
        assert!(a.status().contains("save, then close"), "status was {:?}", a.status());
        a.update(Msg::Save);
        assert!(a.take_save().is_some());
    }

    const CELL: libui::layout::FixedCell = libui::layout::FixedCell { w: 8, h: 16 };

    #[test]
    fn the_left_answer_discards_and_the_right_one_keeps() {
        // Swapped, a click discards a buffer it was asked to keep.
        let a = app();
        let ui = a.confirm_view(&UiTheme::default(), None);
        let l = libui::layout::layout(&ui, Rect::new(0, 0, CONFIRM_W, CONFIRM_H), &CELL);
        let mut tree = libui::diff::Tree::new();
        tree.update(&ui, &l).expect("the dialog is diffable");
        let mut router = libui::route::Router::new();

        let click = |r: &mut libui::route::Router, x: i32, y: i32| {
            let at = |flags: u16, buttons: u16| librsproto::surface::PointerEvent {
                kind: librsproto::surface::POINTER_BUTTON,
                button: 0x110,
                buttons,
                flags,
                x,
                y,
                ..Default::default()
            };
            r.pointer(&tree, &ui, &l, at(librsproto::surface::POINTER_PRESSED, 1));
            r.pointer(&tree, &ui, &l, at(0, 0)).0
        };

        assert_eq!(click(&mut router, DIALOG_LEFT_CX, CONFIRM_BUTTON_CY), alloc::vec![Msg::Discard],);
        assert_eq!(click(&mut router, DIALOG_RIGHT_CX, CONFIRM_BUTTON_CY), alloc::vec![Msg::KeepEditing],);
    }

    #[test]
    fn the_dialogs_close_button_keeps_the_buffer() {
        let a = app();
        let ui = a.confirm_view(&UiTheme::default(), None);
        let l = libui::layout::layout(&ui, Rect::new(0, 0, CONFIRM_W, CONFIRM_H), &CELL);
        let mut tree = libui::diff::Tree::new();
        tree.update(&ui, &l).expect("the dialog is diffable");
        let mut router = libui::route::Router::new();
        let (x, y) = libui::widget::title_button_centre(CONFIRM_W, 0);
        let at = |flags: u16, buttons: u16| librsproto::surface::PointerEvent {
            kind: librsproto::surface::POINTER_BUTTON,
            button: 0x110,
            buttons,
            flags,
            x,
            y,
            ..Default::default()
        };
        router.pointer(&tree, &ui, &l, at(librsproto::surface::POINTER_PRESSED, 1));
        let (msgs, _) = router.pointer(&tree, &ui, &l, at(0, 0));
        assert_eq!(msgs, alloc::vec![Msg::KeepEditing]);
    }
    /// The window's own tree survives being diffed frame to frame: one unkeyed sibling makes it
    /// undiffable and nothing draws, which no layout or paint test sees.
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

    /// Save As changes what the buffer is: tab, unsaved marker and next `Ctrl+S` follow.
    #[test]
    fn save_as_changes_what_the_buffer_is() {
        let mut a = app();
        assert_eq!(a.path(), "/home/notes.txt");

        a.update(Msg::SaveAs);
        let c = a.chooser().expect("a chooser is open");
        assert_eq!(c.dir, "/home");
        assert_eq!(c.state.name.text(), "notes.txt");
        assert_eq!(a.take_chooser_list().as_deref(), Some("/home"), "and it asked for a listing");

        let c = a.chooser_mut().expect("a chooser is open");
        c.state.name = libui::widget::TextFieldState::with_text("other.txt");
        a.update(Msg::ChooserAccept);

        assert!(a.chooser().is_none(), "accepting closes it");
        assert_eq!(a.path(), "/home/other.txt", "the buffer *is* the new file");
        assert!(a.tabs().iter().any(|(_, label, _)| label.contains("other.txt")), "{:?}", a.tabs());
        let (_, path, _) = a.take_save().expect("a save was requested");
        assert_eq!(path, "/home/other.txt", "the next write goes to the new file");
    }

    /// The wheel scrolls the chooser's list, through the real dialog tree and router.
    #[test]
    fn the_wheel_scrolls_the_choosers_list() {
        let mut a = app();
        a.update(Msg::OpenFile);
        let _ = a.take_chooser_list();
        a.show_chooser("/home", (0..60).map(|i| (alloc::format!("f{i}.txt"), false)).collect());

        let ui = a.chooser_view(&UiTheme::default(), None);
        let l =
            libui::layout::layout(&ui, Rect::new(0, 0, libui::chooser::CHOOSER_W, libui::chooser::CHOOSER_H), &CELL);
        let mut tree = libui::diff::Tree::new();
        tree.update(&ui, &l).expect("the chooser is diffable");
        let mut router = libui::route::Router::new();
        let ev = librsproto::surface::PointerEvent {
            kind: librsproto::surface::POINTER_WHEEL,
            wheel: 1,
            x: (libui::chooser::CHOOSER_W / 2) as i32,
            y: (libui::chooser::CHOOSER_H / 2) as i32,
            ..Default::default()
        };
        let msgs = router.pointer(&tree, &ui, &l, ev).0;
        assert!(!msgs.is_empty(), "nothing over the chooser took the wheel");
        for m in msgs {
            a.update(m);
        }
        assert_eq!(a.chooser().expect("still open").state.list.offset, libui::click::WHEEL_UNITS as usize,);
    }

    #[test]
    fn the_chooser_goes_up_by_control_and_by_backspace() {
        let mut a = app();
        a.update(Msg::OpenFile);
        let _ = a.take_chooser_list();
        a.show_chooser("/home/papers/drafts", alloc::vec![(String::from("a.txt"), false)]);

        a.update(Msg::ChooserUp);
        assert_eq!(a.take_chooser_list().as_deref(), Some("/home/papers"));
        a.show_chooser("/home/papers", alloc::vec![(String::from("b.txt"), false)]);

        a.chooser_key(KeyEvent::new(1, libkern::abi::KEY_BACKSPACE, 1, 0));
        assert_eq!(a.take_chooser_list().as_deref(), Some("/home"));
        a.show_chooser("/", alloc::vec![(String::from("home"), true)]);

        a.update(Msg::ChooserUp);
        assert_eq!(a.take_chooser_list(), None, "the root tried to go somewhere");
        assert!(a.chooser().is_some(), "and the dialog is still open");
    }

    /// In `Save`, Backspace edits the name instead of navigating.
    #[test]
    fn backspace_edits_the_name_while_saving() {
        let mut a = app();
        a.update(Msg::SaveAs);
        let _ = a.take_chooser_list();
        a.show_chooser("/home/papers", alloc::vec![]);
        let before = a.chooser().expect("open").state.name.text().to_string();
        assert!(!before.is_empty(), "precondition: the field is seeded with the buffer's name");

        a.chooser_key(KeyEvent::new(1, libkern::abi::KEY_BACKSPACE, 1, 0));
        assert_eq!(a.take_chooser_list(), None, "saving navigated instead of editing");
        assert_eq!(a.chooser().expect("open").state.name.text().len(), before.len() - 1, "the name was not edited");
    }

    #[test]
    fn the_chooser_descends_and_then_answers() {
        let mut a = app();
        a.update(Msg::OpenFile);
        let _ = a.take_chooser_list();
        a.show_chooser("/home", alloc::vec![(String::from("papers"), true), (String::from("notes.txt"), false),]);

        a.update(Msg::ChooserRow(CHOOSER_ROW_KEY));
        assert_eq!(a.take_chooser_list().as_deref(), Some("/home/papers"));
        assert!(a.chooser().is_some(), "descending does not close the chooser");
        assert_eq!(a.take_open(), None, "and it is not an answer");

        a.show_chooser("/home/papers", alloc::vec![(String::from("draft.txt"), false)]);
        a.update(Msg::ChooserRow(CHOOSER_ROW_KEY));
        assert!(a.chooser().is_none(), "choosing closes it");
        assert_eq!(a.take_open().as_deref(), Some("/home/papers/draft.txt"));
    }

    #[test]
    fn the_chooser_refuses_the_answers_that_are_not_answers() {
        let mut a = app();
        a.update(Msg::OpenFile);
        a.show_chooser("/home", alloc::vec![(String::from("notes.txt"), false)]);
        a.update(Msg::ChooserAccept);
        assert!(a.chooser().is_some(), "it stayed open");
        assert_eq!(a.take_open(), None);
        assert_eq!(a.status(), "nothing chosen");

        let mut a = app();
        a.update(Msg::SaveAs);
        let c = a.chooser_mut().expect("a chooser is open");
        c.state.name = libui::widget::TextFieldState::with_text("   ");
        a.update(Msg::ChooserAccept);
        assert!(a.chooser().is_some(), "left open — the person is mid-answer");
        assert_eq!(a.take_save(), None, "and nothing was written");
        assert_eq!(a.status(), "a name, then Save");
    }

    /// `Enter` on a selected directory descends, exactly as clicking it does.
    #[test]
    fn enter_on_a_directory_descends_rather_than_answering_with_it() {
        let mut a = app();
        a.update(Msg::OpenFile);
        let _ = a.take_chooser_list();
        a.show_chooser("/home", alloc::vec![(String::from("papers"), true), (String::from("notes.txt"), false),]);

        a.chooser_key(KeyEvent::new(1, libkern::abi::KEY_DOWN, 1, 0));
        assert_eq!(a.chooser().unwrap().state.list.selected, Some(0), "the directory is selected");
        a.chooser_key(KeyEvent::new(1, libkern::abi::KEY_ENTER, 1, 0));

        assert!(a.chooser().is_some(), "descending does not close the chooser");
        assert_eq!(a.take_chooser_list().as_deref(), Some("/home/papers"), "it asked to list it");
        // The failure: the directory went out to `open_into`, which refuses it as a blocked tab.
        assert_eq!(a.take_open(), None, "a directory is not an answer");

        a.show_chooser("/home/papers", alloc::vec![(String::from("draft.txt"), false)]);
        a.chooser_key(KeyEvent::new(1, libkern::abi::KEY_DOWN, 1, 0));
        a.chooser_key(KeyEvent::new(1, libkern::abi::KEY_ENTER, 1, 0));
        assert!(a.chooser().is_none(), "a file closes it");
        assert_eq!(a.take_open().as_deref(), Some("/home/papers/draft.txt"));
    }

    /// The keyboard drives the chooser; a declined key reaches nothing, as the buffer's text shows.
    #[test]
    fn the_chooser_reads_the_keyboard_itself() {
        let ev = |code: u16, pressed: u16| KeyEvent::new(1, code, pressed, 0);
        let listing = || alloc::vec![(String::from("a.txt"), false), (String::from("b.txt"), false)];

        let mut a = app();
        assert!(!a.chooser_key(ev(libkern::abi::KEY_ESC, 1)), "no chooser, no claim");

        a.update(Msg::OpenFile);
        a.show_chooser("/home", listing());
        a.chooser_key(ev(libkern::abi::KEY_UP, 1));
        assert_eq!(a.chooser().unwrap().state.list.selected, None, "up from nothing selects none");
        a.chooser_key(ev(libkern::abi::KEY_DOWN, 1));
        a.chooser_key(ev(libkern::abi::KEY_DOWN, 1));
        assert_eq!(a.chooser().unwrap().state.list.selected, Some(1));
        a.chooser_key(ev(libkern::abi::KEY_DOWN, 1));
        assert_eq!(a.chooser().unwrap().state.list.selected, Some(1), "clamped to the last row");

        assert!(!a.chooser_key(ev(libkern::abi::KEY_UP, 0)), "a release is declined");
        assert_eq!(a.chooser().unwrap().state.list.selected, Some(1), "and moved nothing");

        let before = a.text();
        assert!(a.chooser_key(ev(KEY_X, 1)), "claimed even though it does nothing");
        assert_eq!(a.text(), before, "the buffer behind the dialog is untouched");

        a.chooser_key(ev(libkern::abi::KEY_ENTER, 1));
        assert!(a.chooser().is_none(), "Enter closed it");
        assert_eq!(a.take_open().as_deref(), Some("/home/b.txt"));

        a.update(Msg::SaveAs);
        let _ = a.take_chooser_list();
        a.chooser_key(ev(libkern::abi::KEY_ESC, 1));
        assert!(a.chooser().is_none(), "Esc closed it");
        assert_eq!(a.take_save(), None, "and nothing was saved");

        a.update(Msg::SaveAs);
        let seeded = String::from(a.chooser().unwrap().state.name.text());
        a.chooser_key(ev(KEY_X, 1));
        let after = String::from(a.chooser().unwrap().state.name.text());
        assert_ne!(after, seeded, "the name field took the character: {after:?}");
    }

    /// **Every** menu in the bar can be opened, not just the ones a test happened to name.
    #[test]
    fn every_menu_in_the_bar_has_somewhere_to_hang_from() {
        let mut a = app();
        let cell = libui::layout::FixedCell { w: 8, h: 16 };
        let size = a.window_size();
        let view = a.view(&UiTheme::default(), None);
        let l = libui::layout::layout(&view, Rect::new(0, 0, size.w, size.h), &cell);
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
