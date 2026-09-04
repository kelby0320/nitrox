//! `nxedit` — the text editor, minus the syscalls.
//!
//! The split `nxterm` and `nxfiles` use: what an editor *is* — a buffer, a name, whether what
//! is on screen matches what is on disk, and what happens when a save fails — is a function of
//! values and host-tests in milliseconds. Reading the file, writing it and pumping events are
//! the binary's.
//!
//! ## The two decisions here
//!
//! **A save that fails keeps the buffer.** That is the whole of the failure design: an editor
//! that loses your work quietly is the one thing an editor must never be, so [`App::saved`]
//! takes a failure and turns it into a *message*, never into a state change. The buffer stays
//! modified, the text stays exactly where it was, and the person can try again somewhere else.
//! The write-and-rename that makes the disk half safe is [`main`](../src/main.rs)'s, because it
//! is syscalls; what is here is the rule that a failure is reported rather than absorbed.
//!
//! **A buffer that could not be read refuses to be written.** Opening a path the editor cannot
//! read shows an empty window, and an empty window saved over a file is that file destroyed —
//! by an editor that never showed its contents. [`App::blocked`] is what a load failure sets,
//! and a blocked buffer declines to save with the reason on screen. A *missing* file is not a
//! failure: opening a path that is not there yet is how a file gets made.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use libdraw::geom::{Rect, Size};
use librsproto::surface::{
    KEY_DOWN, KEY_REPEAT, KeyEvent, MOD_CTRL, MOD_SHIFT, RESIZE_BOTTOM, RESIZE_RIGHT,
    WINDOW_STATE_MAXIMIZED, WINDOW_STATE_MINIMIZED, WINDOW_STATE_NORMAL,
};
use alloc::vec;
use libui::chooser::{self, ChooserState};
use libui::menu::{Accel, Item, Menu, MenuState};
use libui::element::{
    Edge, Element, Insets, column, dock, docked, offset, padding, row, sized, stack, text,
    with_spacing,
};
use libui::widget::{
    GRIP_W, TAB_STRIP_H, Theme as UiTheme, TITLE_BAR_H, TextAreaState, TextFieldState,
    TitleButtons, WINDOW_FRAME_H, WidgetState, button, dialog_frame, resize_grip, tab_strip,
    text_area, text_field, title_bar, window_frame,
};

/// The status strip's height in pixels — one row of chrome under the title bar.
pub const STATUS_H: u32 = 24;

/// A text line's height in pixels, matching the toolkit's default theme.
pub const ROW_H: u32 = 20;

/// The element key on the text area.
pub const AREA_KEY: u64 = 1;
/// The element key on the save control.
pub const SAVE_KEY: u64 = 2;
/// The element key on the title bar.
pub const TITLE_KEY: u64 = 3;
/// The element key on the resize grip.
pub const GRIP_KEY: u64 = 4;
/// The element key on the status strip.
///
/// **Every docked child needs one**: the diff requires a container's children to be all keyed
/// or all unkeyed, and one bare sibling makes the whole first frame `MixedKeying` — which in a
/// client like this one means no window ever appears. See `nxfiles`' `STRIP_KEY`, which carries
/// the full account of the two errors and their two symptoms.
pub const STRIP_KEY: u64 = 5;
/// The element key on the status text.
pub const STATUS_KEY: u64 = 6;
/// The element key on the tab strip.
pub const TAB_STRIP_KEY: u64 = 7;

/// Where buffer keys start, and therefore where the tabs' element keys do.
///
/// **Because a tab's key is an element key too.** `Router::hovered_key` walks up to the nearest
/// keyed ancestor and reports *that* number, across the whole window's tree — so a tab keyed `2`
/// and the save button keyed `2` are one number, and hovering the tab would draw the button
/// hovered.
///
/// **The top bit, not a large number.** This was `1000`, which is far from the chrome's keys and
/// not *disjoint* from a list's — a browser keys its rows by index and a directory can hold more
/// than a thousand entries, so row 1000 and the first tab would have been one number (PR #270
/// review, optional 6). Setting the high bit makes the two namespaces disjoint by construction,
/// which is what this doc already claimed.
pub const TAB_KEY_BASE: u64 = 1 << 63;

/// The element key on the confirmation dialog's title bar.
///
/// **A separate window's keys, in the same space.** Keys are per *tree*, and the dialog is a
/// tree of its own — nothing would break if these repeated the numbers above. They do not,
/// because a key that appears in two trees is a thing two `hovered ==` comparisons can both
/// match, and the hover the router reports is already one of the two id spaces that look alike
/// (M11 Part E batch 3).
pub const CONFIRM_TITLE_KEY: u64 = 10;
/// The element key on the dialog's question.
pub const CONFIRM_TEXT_KEY: u64 = 11;
/// The element key on the dialog's button strip.
///
/// **Unused since `dialog_frame` took the strip** (M12 Part B) — the helper builds it and does
/// not key it, wrapping both of the dock's children so the caller's keys sit one level down.
/// Kept because the numbers below are spent and reusing this one would put two meanings on it.
pub const CONFIRM_STRIP_KEY: u64 = 12;
/// The element key on the dialog's *discard* button.
pub const CONFIRM_DISCARD_KEY: u64 = 13;
/// The element key on the dialog's *keep editing* button.
pub const CONFIRM_KEEP_KEY: u64 = 14;

/// The confirmation dialog's geometry, **which is `libui`'s** since M12 Part B.
///
/// It was published from here for `check-login` to aim at, and `nxfiles` then grew a
/// confirmation of its own — so the five metrics, the four aim points and the measurable frame
/// moved down to [`libui::widget`] where both dialogs share one table. What stays here is what
/// this dialog *says*.
pub use libui::widget::{
    DIALOG_BUTTON_CY as CONFIRM_BUTTON_CY, DIALOG_H as CONFIRM_H, DIALOG_LEFT_CX,
    DIALOG_RIGHT_CX, DIALOG_W as CONFIRM_W,
};

/// The window's size in pixels at startup, before any manager places it.
pub const START_SIZE: Size = Size::new(560, 420);

/// What this window calls the acceptor it declares.
///
/// **A name rather than an index**, because it is a port in waiting (M10 decision 2): the same
/// string is what a command line will address when ports arrive, and an editor that called its
/// sink `0` would have to be re-specified to be addressed by anything but a pointer.
pub const ACCEPTOR: &str = "document";

/// The key that saves, with Ctrl held: `s`.
///
/// **A literal, because `libkern::abi` names only the keys with no character** — the arrows,
/// Enter, the modifiers. A letter is a keycode the keymap turns into text, and nothing has
/// needed to name one until a chord did. `31` is `KEY_S` in the Linux set the input stack
/// speaks (`libinput::keymap`, which maps it to `s`/`S`), and the test below pins it to that
/// table rather than to this comment.
pub const SAVE_KEYCODE: u16 = 31;

/// The key that undoes, with Ctrl held: `z`.
///
/// Literals for the reason [`SAVE_KEYCODE`] is one — `libkern::abi` names only the keys with no
/// character — and pinned against `libinput`'s table by a test rather than by this comment.
pub const UNDO_KEYCODE: u16 = 44;
/// The key that redoes: `y`. Not `Ctrl+Shift+Z`, which needs a modifier this editor does not
/// otherwise read, and which every application that offers both spells differently anyway.
pub const REDO_KEYCODE: u16 = 21;
/// The key that opens the find field: `f`.
pub const FIND_KEYCODE: u16 = 33;
/// How many menus the bar carries. `File` and `Edit`.
pub const MENU_COUNT: usize = 2;

/// The menu bar's height in pixels — one row of chrome, matching the browser's.
pub const MENU_BAR_H: u32 = 24;

/// Where the menu bar's words are keyed from: `MENU_BAR_KEY + i` for menu `i`.
pub const MENU_BAR_KEY: u64 = 40;

/// The element key on the menu bar itself.
///
/// **Not optional.** `diff` rejects a parent whose children are *partly* keyed, and the dock's
/// other two `Top` children have carried keys since they were written — so an unkeyed bar makes
/// the whole window undiffable and nothing draws at all. `nxfiles` keys its bar for the same
/// reason; this one was missed and `check-login` said `nxedit: the view is not diffable`.
pub const BAR_KEY: u64 = 15;

/// Where the open popup's rows are keyed from: `MENU_ROW_KEY + i` for item `i`.
pub const MENU_ROW_KEY: u64 = 100;

/// Where the chooser's element keys start — clear of this window's own.
pub const CHOOSER_KEY: u64 = 200;

/// The key that opens the file chooser: `o`, with Ctrl.
pub const OPEN_KEYCODE: u16 = 24;
/// The key that opens it to save under a new name: `s`, with Ctrl and **Shift** — the same letter
/// as Save, because it is the same verb with a question attached.
pub const SAVE_AS_KEYCODE: u16 = 31;

/// The key that opens a *window*: `n`, with Ctrl and **Shift**.
///
/// Shift because `Ctrl+N` is "new" in the singular everywhere and an editor's singular is a
/// buffer; the window is the larger thing, and every application that offers both spells it this
/// way.
pub const NEW_WINDOW_KEYCODE: u16 = 49;
/// The key that quits: `q`, with Ctrl. **Quit is not Close** — see [`Msg::Quit`].
pub const QUIT_KEYCODE: u16 = 16;

/// The key that opens a tab: `t`.
pub const NEW_TAB_KEYCODE: u16 = 20;
/// The key that closes one: `w`. Closing the last tab closes the window, which is what the
/// chord means everywhere else it exists.
pub const CLOSE_TAB_KEYCODE: u16 = 17;

/// The key that copies the selection: `c`.
///
/// **`Ctrl+C` here, not `Ctrl+Shift+C`.** M12 decision 6: what fingers already know, and this
/// is not a terminal — nothing in an editor claims `Ctrl+C` for an interrupt, so there is
/// nothing to work around. `nxterm` is the one that has to differ, and its own constants say
/// why.
pub const COPY_KEYCODE: u16 = 46;
/// The key that cuts: `x`.
pub const CUT_KEYCODE: u16 = 45;
/// The key that pastes: `v`. With **Shift** as well it cycles — see [`App::cycling`].
pub const PASTE_KEYCODE: u16 = 47;

/// Whether `k` is `Ctrl+Shift+V` — the one key that *continues* a paste rather than ending it.
///
/// A function rather than a condition spelled twice: [`App::key`] asks it to decide whether to
/// end a cycling sequence and the chord's own arm asks it to act, and two spellings of one rule
/// is how the sequence ends on the key that was meant to continue it.
fn is_cycle_chord(k: KeyEvent) -> bool {
    k.keycode == PASTE_KEYCODE && k.modifiers & MOD_CTRL != 0 && k.modifiers & MOD_SHIFT != 0
}

/// Confirms the name being typed for an untitled buffer.
const NAME_CONFIRM: u16 = libkern::abi::KEY_ENTER;
/// Abandons it, leaving the buffer untitled and unsaved.
const NAME_CANCEL: u16 = libkern::abi::KEY_ESC;

/// Join a directory and a file name — `libfs::join`'s rule, on `str`.
///
/// The same helper `nxfiles` keeps for the same reason: this half of the application never sees
/// a path as bytes, and converting to call `libfs` would be a round trip through a lossy
/// conversion for a rule that is one line.
fn join(dir: &str, name: &str) -> String {
    let mut s = String::from(dir);
    if !s.ends_with('/') {
        s.push('/');
    }
    s.push_str(name);
    s
}

/// One open file: everything that belongs to a buffer rather than to the editor.
///
/// **Split out in M12 Part D**, when the window grew tabs. What stays on [`App`] is what a
/// *window* has — its size, its focus, the outboxes, the strip's field, the question it may be
/// asking — and what moves here is what a person would expect to survive switching tabs. Getting
/// that line wrong is how a second tab inherits the first's undo history, which is the shape this
/// split exists to make impossible rather than careful.
pub struct Buffer {
    /// Identity across frames and across the tab strip.
    ///
    /// **Not the index**: closing a tab renumbers every one after it, and a message naming an
    /// index outlives the frame that produced it.
    key: u64,
    /// The absolute path being edited, or empty for a buffer that has never been named.
    path: String,
    /// The last component of [`path`](Self::path), for the tab and the title bar.
    name: String,
    /// The text, and the whole of the editing model — `libui` owns those rules, including the
    /// undo history, which is per buffer because it is part of the text.
    text: TextAreaState,
    /// The buffer's revision when it was last read or written.
    ///
    /// **A number rather than a flag**, because the flag has to be cleared by every path that
    /// makes the buffer match the disk and set by every path that does not — and a save's
    /// success arrives from the binary, one frame after the keystroke that asked for it.
    /// Comparing revisions makes "modified" a derived fact.
    saved_at: u64,
    /// Why this buffer may not be written, or `None` when it may.
    blocked: Option<String>,
    /// The buffer did not match the file the moment it was read.
    ///
    /// **Because reading is not always lossless.** `TextAreaState::with_text` drops a `\r` from
    /// the end of every line, so a CRLF file is *already* something else by the time it is on
    /// screen — and a `saved_at` taken after that says the buffer matches a file it does not.
    /// The consequence was an editor that could open a file, be told nothing had changed, and
    /// rewrite it two bytes shorter per line on the first `Ctrl+S` (PR #259 review, finding 3).
    ///
    /// So it is folded into [`App::modified`]: what the title bar marks is "this is not what is
    /// on disk", which is true from the first frame here.
    differs: bool,
}

/// A chooser the editor has open: which job, where it is looking, and what is there.
///
/// **The entries are here because the application was given them**, not because this crate read
/// them — `libui::chooser` renders over what it is handed and `main` does the listing, which is
/// decision 3's rule seen from the other side.
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
    /// The open buffers, in the order their tabs are drawn. **Never empty**: the last one closing
    /// closes the window, so every method below can assume there is a current buffer.
    buffers: Vec<Buffer>,
    /// Which buffer's tab is current, by [`Buffer::key`].
    current: u64,
    /// The chooser this window has open, if any.
    chooser: Option<Chooser>,
    /// A directory the binary is being asked to list for the chooser.
    chooser_list: Option<String>,
    /// A path the binary is being asked to open into a tab.
    open_requested: Option<String>,
    /// Another window has been asked for, and the binary has not made it yet.
    new_window: bool,
    /// A quit has been asked for. **The binary owns what that means**, because it is the only
    /// thing that knows how many windows there are.
    quit: bool,
    /// Which menu is open, where each bar word sits, and where the keyboard is inside it.
    ///
    /// **New in M14 Part A**: this editor had no menu bar at all, only chords and a save button,
    /// so everything it could do was either advertised nowhere or advertised once in a status
    /// line. `nxterm` and `nxfiles` had one each and a hand-rolled popup apiece.
    pub menus: MenuState,
    /// The next key to hand out. Monotonic, so a key is never reused and a stale message can
    /// never name a buffer that has taken its place.
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
    /// **The key, not a flag** (PR #270 review, worth fixing 3). A `bool` was right when there
    /// was one buffer; with tabs, `take_save` and `path()` both read whatever is current when
    /// `main` gets round to the write — the top of the *next* iteration, after the whole batch
    /// has been applied. A `Ctrl+S` and a tab click in one drain therefore wrote the other tab's
    /// bytes to the other tab's path, marked *that* buffer saved, and left the one the person
    /// asked to save dirty with nothing to show anything had gone wrong. The batch is reachable
    /// whenever the client is behind, which `pool.acquire` blocking on the third commit makes
    /// ordinary. Same resolution rule as `confirming`: capture the subject when it is asked for.
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
    /// What is being closed over an unsaved buffer, and the person has not answered yet.
    ///
    /// **`true` is a second window**, not an overlay. `Surface::CloseRequested` says outright
    /// that "a client that wants to ask 'save first?' opens a dialog and closes when that
    /// resolves"; until M12 Part A no application had ever created one, so this editor answered
    /// every close by exiting and the buffer went with it. The binary reads this each frame and
    /// opens or destroys a `Role::Dialog` window to match — the same shape `nxterm` uses for its
    /// menu, and the reason [`libui::window::Child`] exists.
    confirming: Option<Closing>,
    /// Whether the *dialog* holds the keyboard, which its own title bar shows.
    ///
    /// **Not [`focused`](Self::focused)**, which is the main window's: a dialog taking focus
    /// from its parent sends both halves down one channel, and a title bar drawn from the wrong
    /// one would show two active windows or none.
    pub confirm_focused: bool,
    /// The dialog's title bar was dragged, and the binary owes the compositor a `StartMove` **on
    /// the dialog's window**.
    ///
    /// A second flag rather than a second use of [`move_requested`](Self::move_requested),
    /// because the two name different windows: an interactive move is a request on one window
    /// id, and one flag would have moved whichever window the binary happened to pass.
    confirm_move_requested: bool,
    /// Text being typed into the status strip's field, and what it is for.
    ///
    /// **`Some` is a mode**, and it is the first one this editor has: while a name is being typed
    /// the keys belong to it rather than to the buffer, and the status strip shows the field
    /// instead of what last happened. It exists because an editor launched from the applications
    /// menu has no `argv[1]` — it used to print "no file to edit" and exit, which is what "nxedit
    /// doesn't launch from the menu" turned out to be (M11 Part E batch 7).
    ///
    /// **In the editor rather than through the shell.** A `Desktop` op that asked the shell to
    /// collect a name would make the shell a dialog provider for arbitrary clients — an authority
    /// question — and would need a blocking exchange over an async protocol. A field in this
    /// window is no protocol at all, and this crate's own key path already noted that "the first
    /// widget that wants a key needs exactly this shape".
    field: Option<(Field, TextFieldState)>,
    /// Where an untitled buffer is saved, from the session's `HOME`.
    home: String,
    /// The last search's answer, which the binary owes the console.
    ///
    /// `Some(Some(line))` for a hit, `Some(None)` for a miss, and `None` once reported.
    ///
    /// **A line number, not the needle.** A gate driving a release image cannot read this window,
    /// so a search needs an outside receipt — and what somebody is looking for in their own file
    /// is theirs, the same rule that keeps the buffer's receipt a count and the compositor's
    /// chord log to the modifier alone.
    find_report: Option<Option<usize>>,
}

/// What a confirmation is about.
///
/// **Resolved when the question is asked**, which is M12 Part B's lesson applied one part later:
/// a tab's key is captured here rather than re-read when the answer arrives, so a question about
/// one tab cannot end up closing another that has since become current.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Closing {
    /// The whole window, and every buffer in it.
    Window,
    /// One tab, by its key.
    Tab(u64),
}

/// What the status strip's field is collecting.
///
/// **One mode with two purposes, because they are the same shape** — the plan's words for find
/// were "reuses the shape the save-as field established: a mode in which the keys are the field's
/// rather than the buffer's". That shape was called "the first widget that wants a key" when it
/// arrived in M11 Part E batch 7; this is the second, and a second copy of it would be two places
/// that can disagree about what `Esc` does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    /// A name for a buffer that has never had one, on the way to a save.
    Naming,
    /// Text to look for in the buffer.
    Finding,
}

/// What the editor owes the clipboard, which only `main` can do.
///
/// An outbox, like [`App::take_save`]: `update` is a function of values and `/dev/clipboard` is
/// IPC, so the application records what it wants and the binary that owns the namespace
/// performs it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ClipRequest {
    /// Push this text onto the ring.
    Copy(String),
    /// Fetch the newest entry and insert it.
    Paste,
    /// Fetch the entry **after** the one just pasted, replacing what that paste inserted.
    ///
    /// M12 decision 3's rule, and the reason this is a separate request rather than a paste
    /// with an index: cycling is only valid *immediately after a paste*, and what makes a stale
    /// position unreachable is that any other action ends the sequence. [`App::cycling`] is that
    /// state, and everything else clears it.
    Cycle,
}

/// Where a paste put its text, and which entry it was.
///
/// **The whole of the cycling rule, in one `Option`.** It exists only between a paste and the
/// next thing the person does; every other action clears it, so a position from five minutes ago
/// cannot be reached. The serial is what the *server* checks — a pipeline can push while
/// somebody is mid-cycle, and then the index means a different entry.
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
    /// Somebody wants this window gone — its own close button, or the shell asking.
    ///
    /// **Over an unsaved buffer this asks rather than closes**, which is the whole of M12 Part
    /// A: it raises [`App::confirming`] and the binary turns that into a real dialog window.
    /// Only [`Msg::Discard`] ends the run.
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
    /// Open the chooser to name this buffer — `Ctrl+Shift+S`, or File ▸ Save As.
    ///
    /// **Not Save with a prompt.** See [`App::adopt_path`] for what accepting it changes.
    SaveAs,
    /// A row of the chooser was activated: descend into it, or choose it.
    ChooserRow(u64),
    /// The chooser's accepting button, or `Enter` in its name field.
    ChooserAccept,
    /// The chooser's *Cancel*, its close button, or `Esc`.
    ChooserCancel,
    /// Open another window of this editor — `Ctrl+Shift+N`, or File ▸ New Window.
    ///
    /// **Recorded, not done.** A window is a compositor object and this crate makes no syscalls;
    /// the binary takes the request and creates one, the same seam `nxterm`'s tabs use for their
    /// ttys.
    NewWindow,
    /// Close every window of this application — `Ctrl+Q`, or File ▸ Quit.
    ///
    /// **Quit is not Close**, and the difference is only visible once there are several windows:
    /// Close is this window, Quit is all of them. Each is asked exactly as its own close button
    /// asks — there is no second question for quitting — and **cancelling one aborts the quit and
    /// leaves the rest open** (M14 decision 4). Windows already closed stay closed, because a
    /// close cannot be taken back.
    Quit,
    /// Undo the last edit — `Ctrl+Z`, or Edit ▸ Undo.
    ///
    /// **A message rather than a branch inside `key`**, since M14 Part A: the menu declares the
    /// chord and [`libui::menu::accel_match`] routes it, so the label and the binding are one
    /// statement rather than two that can drift (M14 decision 2). The five below are the same.
    Undo,
    /// Redo what was undone — `Ctrl+Y`. See [`Undo`](Msg::Undo).
    Redo,
    /// Open the find field — `Ctrl+F`. See [`Undo`](Msg::Undo).
    Find,
    /// Copy the selection — `Ctrl+C`. See [`Undo`](Msg::Undo).
    Copy,
    /// Cut the selection — `Ctrl+X`. See [`Undo`](Msg::Undo).
    Cut,
    /// Paste the newest clipboard entry — `Ctrl+V`. See [`Undo`](Msg::Undo).
    ///
    /// **Not the cycle**, which is `Ctrl+Shift+V` and is a continuation gesture rather than an
    /// action: it only means anything immediately after a paste, so it is not a menu row and is
    /// matched before the table is consulted.
    Paste,
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
            buffers: alloc::vec![Buffer::new(TAB_KEY_BASE, path)],
            current: TAB_KEY_BASE,
            chooser: None,
            chooser_list: None,
            open_requested: None,
            new_window: false,
            quit: false,
            menus: MenuState::new(MENU_COUNT),
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
            confirm_move_requested: false,
        }
    }

    /// Remove the tab keyed `k`, and close the window if it was the last one.
    ///
    /// **The last tab closing closes the window**, which is what keeps `buffers` non-empty — the
    /// invariant every accessor here relies on. The alternative, an editor showing no buffer at
    /// all, is a window with nothing in it and no way to get one back.
    fn drop_tab(&mut self, k: u64) {
        if self.buffers.len() <= 1 {
            self.closing = true;
            return;
        }
        let Some(i) = self.buffers.iter().position(|b| b.key == k) else { return };
        self.buffers.remove(i);
        if self.current == k {
            // The tab to its left, or the first — whichever survives. A person closing a tab is
            // looking at where it was, so the neighbour is the least surprising place to land.
            self.current = self.buffers[i.saturating_sub(1).min(self.buffers.len() - 1)].key;
        }
    }

    /// The buffer whose tab is current.
    ///
    /// **Never `None`**, and that is an invariant rather than a hope: `buffers` starts with one
    /// and the close path turns "the last tab" into "the window", so there is always a current
    /// buffer. Panicking here would be a `no_std` client dying in front of somebody, so it falls
    /// back to the first — a wrong buffer is recoverable and a dead editor is not.
    fn buf(&self) -> &Buffer {
        self.buffers
            .iter()
            .find(|b| b.key == self.current)
            .unwrap_or(&self.buffers[0])
    }

    /// The current buffer, mutably. See [`buf`](Self::buf) for why this cannot be `None`.
    fn buf_mut(&mut self) -> &mut Buffer {
        let key = self.current;
        let i = self.buffers.iter().position(|b| b.key == key).unwrap_or(0);
        &mut self.buffers[i]
    }

    /// Open `path` in a **new tab** and make it current, returning its key.
    ///
    /// The caller reads the file into it afterwards, the way `main` already does for the first.
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
                let name = if b.name.is_empty() {
                    String::from("untitled")
                } else {
                    b.name.clone()
                };
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

    /// The file held `raw`, which decoded to `text`.
    ///
    /// **Both, because the buffer may already differ from the file.** Line endings are
    /// normalised on the way in, so a CRLF file is not what is on screen — and an editor that
    /// called that "unmodified" would rewrite it, shorter, on a `Ctrl+S` the person pressed out
    /// of habit. Comparing what *would be written* against what was read is the only honest
    /// answer, and it is one comparison at open rather than a rule about encodings.
    pub fn loaded(&mut self, text: &str, raw: &[u8]) {
        let b = self.buf_mut();
        b.text = TextAreaState::with_text(text);
        b.saved_at = b.text.revision();
        b.blocked = None;
        b.differs = to_bytes(&b.text.text()) != raw;
        let differs = b.differs;
        self.status = describe(raw.len(), "opened");
        if differs {
            self.status.push_str(" · line endings normalised, so saving rewrites it");
        }
    }

    /// There is nothing at this path yet, which is not a failure.
    ///
    /// An editor opened on a path that does not exist is how a file gets made, so the buffer
    /// stays empty and writable and the strip says which of the two happened — a person who
    /// meant to open an existing file wants to know they did not.
    pub fn absent(&mut self) {
        let b = self.buf_mut();
        b.text = TextAreaState::new();
        b.saved_at = b.text.revision();
        b.blocked = None;
        b.differs = false;
        self.status = String::from("new file");
    }

    /// The file could not be read, so this buffer must not be written over it.
    ///
    /// **The empty window is the danger.** A failed read leaves nothing on screen, and saving
    /// nothing over a file is that file destroyed by an editor that never showed it. So a
    /// blocked buffer stays blocked for the run: what would clear it is a successful read, and
    /// this editor reads once.
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

    /// How many times the buffer has been edited — the receipt a keystroke reached it.
    ///
    /// **The one externally visible sign that typing arrived**, which is what the gate paces on:
    /// an editor's echo is its own window, and a gate driving a *release* image has no rendered
    /// grid to read. A count rather than the text, deliberately — what somebody is typing into
    /// an editor is theirs, and the same rule that keeps the compositor's chord log to the
    /// modifier alone applies here.
    pub fn revision(&self) -> u64 {
        self.buf().text.revision()
    }

    /// Whether what is on screen differs from what is on disk.
    ///
    /// Two ways for that to be true: something was typed, or the file did not survive being read
    /// unchanged — see [`differs`](Self::differs).
    pub fn modified(&self) -> bool {
        self.buf().modified()
    }

    /// What the status strip is saying.
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Whether this buffer refuses to be written, and why.
    pub fn refusal(&self) -> Option<&str> {
        self.buf().blocked.as_deref()
    }

    /// Copy the selection onto the ring, deleting it as well when `cut`.
    ///
    /// **Nothing selected is not a request.** A copy that pushed an empty entry would move the
    /// ring's serial under every client that was mid-cycle, for a gesture that had nothing to
    /// copy — and it would push whatever was last copied one place further back.
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
    ///
    /// The binary reads this to build a cycle's request: which index to ask for next, and which
    /// serial to carry.
    pub fn cycling(&self) -> Option<Cycling> {
        self.cycling
    }

    /// Insert `text` as a paste, and remember where it went so a cycle can replace it.
    ///
    /// `index` and `serial` are what the server answered with. A caller that pasted the newest
    /// entry passes `0` and the serial it came back with.
    pub fn pasted(&mut self, text: &str, index: u32, serial: u64) {
        let buffer = self.current;
        let (from, to) = self.buf_mut().text.insert_text(text);
        self.cycling = Some(Cycling { buffer, from, to, index, serial });
        self.status = String::from("pasted");
    }

    /// Replace what the last paste inserted with `text` — a cycle's answer.
    ///
    /// **Does nothing without a live [`cycling`](Self::cycling)**, and nothing if the current
    /// buffer is not the one that was pasted into: the range names positions in *that* buffer's
    /// text, and applying it to another would replace whatever happens to be at those
    /// coordinates.
    pub fn cycled(&mut self, text: &str, index: u32, serial: u64) {
        let Some(c) = self.cycling else { return };
        if c.buffer != self.current {
            return;
        }
        self.buf_mut().text.select_range(c.from, c.to);
        let (from, to) = self.buf_mut().text.insert_text(text);
        self.cycling = Some(Cycling { buffer: c.buffer, from, to, index, serial });
        self.status = String::from("pasted the one before");
    }

    /// End a cycling sequence — the ring moved, or there is nothing further back.
    pub fn cycle_ended(&mut self, why: &str) {
        self.cycling = None;
        self.status = String::from(why);
    }

    /// The save the binary owes, as the bytes to write. Clears the record.
    ///
    /// `None` when nothing asked for one — **or when this buffer is blocked**, which is where
    /// the refusal is enforced rather than merely displayed. The status strip says why, so a
    /// person pressing save on a file that could not be read is answered rather than ignored.
    pub fn take_save(&mut self) -> Option<(u64, String, String)> {
        let key = core::mem::take(&mut self.save_requested)?;
        let Some(b) = self.buffers.iter().find(|b| b.key == key) else { return None };
        if let Some(why) = b.blocked.clone() {
            self.status = alloc::format!("not saved — {why}");
            return None;
        }
        // **The path comes from the buffer that asked**, not from `path()`, which answers for
        // whatever is current now.
        Some((key, b.path.clone(), b.text.text()))
    }

    /// Take `path` as the file to edit, if this buffer can be given up.
    ///
    /// **A modified buffer refuses**, which is the same rule the save path follows from the
    /// other side: opening a dropped file would replace what is on screen, and an editor that
    /// discarded unsaved work because something was dragged onto it is the failure mode this
    /// application exists to not have. The status strip says so, because a drop that visibly
    /// does nothing is indistinguishable from one that was not delivered.
    ///
    /// **And dropping the file already open switches to it** rather than opening it twice: two
    /// tabs on one file are two buffers that can disagree about what is in it, and the last one
    /// saved wins silently.
    pub fn accept_drop(&mut self, path: &str) -> bool {
        if let Some(b) = self.buffers.iter().find(|b| b.path == path) {
            self.current = b.key;
            self.status = alloc::format!("{} is already open", libfs::basename_str(path));
            return false;
        }
        // **A new tab, since M12 Part D.** It used to replace the buffer, and had to refuse when
        // that buffer was modified — a drop that visibly did nothing, for a reason the person had
        // to read the strip to discover. With tabs there is nothing to lose by taking it.
        self.open_tab(path);
        true
    }

    /// A save finished: `Ok(bytes written)`, or `Err(what went wrong)`.
    ///
    /// **A failure changes nothing but the message.** The buffer stays as it is and stays
    /// modified, because the alternative — marking it saved and letting the person close the
    /// window — is the editor losing their work while telling them it did not.
    pub fn saved(&mut self, key: u64, result: Result<usize, &str>) {
        match result {
            Ok(n) => {
                let Some(i) = self.buffers.iter().position(|b| b.key == key) else { return };
                let b = &mut self.buffers[i];
                b.saved_at = b.text.revision();
                // The file is now what the buffer holds, whatever it held before.
                b.differs = false;
                // **A save closes the undo group.** What a person wants back after saving is
                // what they have typed since it — not everything since the file was opened,
                // which is what one long group would give them.
                b.text.end_group();
                self.status = describe(n, "saved");
            }
            Err(why) => self.status = alloc::format!("NOT saved — {why}"),
        }
    }

    /// Apply a message.
    pub fn update(&mut self, msg: Msg) {
        // **Every message that is not a key is an action, and an action ends a cycle** — see
        // `key`, which handles its own case because one key is the exception. Switching tabs,
        // saving, and closing a dialog are all "something else happened".
        if !matches!(msg, Msg::Key(_)) {
            self.cycling = None;
        }
        // **Choosing dismisses the menu, whichever row it was** — a menu that stayed open would
        // cover the thing it just acted on. Asked of the table rather than repeated in the arms,
        // so a row added later cannot forget. A message that is not a row leaves it alone, which
        // is what keeps `MenuBar` able to open one.
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
            Msg::ChooserRow(key) => self.chooser_row(key),
            Msg::ChooserAccept => self.chooser_accept(),
            Msg::ChooserCancel => self.chooser = None,
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
            // **Reached only when no field is open** by the chord, because a field takes the keys
            // before the chord match is looked at — so `Ctrl+F` while finding is swallowed by the
            // field (the keymap folds it to a control byte, which `apply` declines) and never
            // arrives. The first version guarded against re-opening, and that guard could not
            // fire: a guard that cannot fire reads as protecting an invariant it does not (PR
            // #269 review, worth fixing 1). **The menu row can reach it while a field is open**,
            // which is new — and re-opening find while finding is the same as opening it, so the
            // field starts empty either way and there is still nothing to guard.
            Msg::Find => {
                self.field = Some((Field::Finding, TextFieldState::new()));
                self.status = String::from("find, then Enter");
            }
            Msg::Copy => self.copy(false),
            Msg::Cut => self.copy(true),
            Msg::Paste => self.clip_request = Some(ClipRequest::Paste),
            // **Saving an untitled buffer asks for a name first.** The write itself is the
            // binary's, as always; what changes here is that there may be nowhere to write to
            // yet, and inventing a path would be a file somebody did not choose.
            Msg::Save => {
                if self.buf().path.is_empty() {
                    // **Any field but a naming one is replaced**, not treated as "already
                    // asking" (PR #269 review, worth fixing 2). `field.is_none()` was right
                    // when naming was the only mode; once find joined it, clicking *save* with
                    // the find field open did nothing at all — a control that looks live and is
                    // not, in an application whose own tests exist to catch exactly that.
                    if self.field_kind() != Some(Field::Naming) {
                        self.field = Some((Field::Naming, TextFieldState::new()));
                        self.status = String::from("name it, then Enter");
                    }
                } else {
                    self.save_requested = Some(self.current);
                }
            }
            // Nothing here: the payload is in the event the binary is holding, and *which*
            // widget took the drop is all the toolkit can say. The binary pairs them.
            Msg::Dropped => {}
            Msg::DragWindow => self.move_requested = true,
            Msg::DragConfirm => self.confirm_move_requested = true,
            Msg::ResizeWindow(edges) => self.resize_requested = Some(edges),
            // **A modified buffer never closes on this message, however many times it
            // arrives.** The obvious spelling — "ask if we are not already asking, otherwise
            // close" — turns a *second* `CloseRequested` into an exit, and a shell that asks
            // twice is exactly what a person clicking a taskbar entry twice produces. So the
            // only route out of a modified buffer is the answer the person gave.
            // **Any modified buffer stops it**, not just the current one: closing the window
            // takes every tab with it, so a question about only what is on screen would let the
            // others go silently.
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
            // **A new tab is an untitled buffer**, the same thing the applications menu launches
            // the editor into — so `Ctrl+T` and a launch with no argument reach one state.
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
            // **The status strip says so**, because a dialog that vanishes with nothing changed
            // is indistinguishable from one that took the other answer.
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
    /// **The chord is checked before the buffer sees the key**, and that ordering is load
    /// bearing: `libinput`'s keymap folds `Ctrl+S` to `0x13`, which `TextAreaState::apply`
    /// declines as unprintable — so the buffer would ignore it today and the editor would still
    /// work. It would stop working the moment the toolkit learned another control character,
    /// and the failure would be a keystroke that both saved and typed something.
    fn key(&mut self, k: KeyEvent) {
        if k.pressed != KEY_DOWN && k.pressed != KEY_REPEAT {
            return;
        }
        // **Every key but the cycle itself ends a cycling sequence** — M12 decision 3, and the
        // whole reason a stale ring position is unreachable rather than merely unlikely. The
        // position exists only inside one uninterrupted gesture, so this is where "uninterrupted"
        // is enforced: one place, before any arm can forget.
        if !is_cycle_chord(k) {
            self.cycling = None;
        }
        // **While a name is being typed the keys are the field's**, buffer and chords included.
        // A `Ctrl+S` here would ask to save the thing that has no name yet, which is what is
        // already being answered.
        // **The tab chords are the window's, and are checked first** — the rule `nxfiles`
        // follows, made to agree here (PR #270 review, optional 7). The two applications grew
        // the same widget in the same part and disagreed about this: a `Ctrl+T` during a find
        // opened no tab, because the field's branch returned before the chords were looked at.
        //
        // The line is *what the chord acts on*. `Ctrl+T` and `Ctrl+W` act on the **window** and
        // are checked before any field; `Ctrl+S`, `Ctrl+Z`, `Ctrl+Y` and `Ctrl+F` act on the
        // **buffer**, and while a field is open the buffer is not what is being addressed — a
        // `Ctrl+S` mid-name would ask to save the thing that has no name yet, which is exactly
        // what is already being answered.
        if k.modifiers & MOD_CTRL != 0
            && matches!(k.keycode, NEW_TAB_KEYCODE | CLOSE_TAB_KEYCODE)
        {
            if k.keycode == NEW_TAB_KEYCODE {
                self.update(Msg::NewTab);
            } else {
                let key = self.current;
                self.update(Msg::CloseTab(key));
            }
            return;
        }
        // **While a field is open the rest of the keys are the field's**, buffer and chords
        // included.
        //
        // **What was typed is read out before anything else is touched.** The field lives in
        // `self`, and both answers below reach for another part of `self` — the buffer, to name
        // it or to search it. Holding the field's borrow across that is the shape the split into
        // `Buffer` turned from "works" into "will not compile", which is the better failure.
        if let Some((which, _)) = self.field.as_ref() {
            let which = *which;
            match k.keycode {
                NAME_CANCEL => {
                    self.field = None;
                    // **Naming says what it did not do; find leaves the strip alone.** Blanking
                    // it was a control answering with nothing, and what it would have erased is
                    // the answer to the last search — which is still the most recent thing that
                    // happened (PR #269 review, optional 3).
                    if which == Field::Naming {
                        self.status = String::from("not saved");
                    }
                }
                NAME_CONFIRM => {
                    // **Owned**, so the field's borrow ends here rather than spanning the
                    // buffer work below. `TextFieldState::text` hands back a `&str` into the
                    // field, which is exactly the borrow that must not be alive.
                    let typed: String =
                        self.field.as_ref().map(|(_, f)| String::from(f.text())).unwrap_or_default();
                    match which {
                        Field::Naming => {
                            let name = typed.trim().to_string();
                            if name.is_empty() {
                                // Nothing typed is not a name, and an empty one would save to
                                // the directory itself. Left open rather than cancelled: the
                                // person is mid-answer.
                                self.status = String::from("a name, then Enter");
                                return;
                            }
                            let path = join(&self.home, &name);
                            self.field = None;
                            self.adopt_path(&path);
                        }
                        // **The field stays open**, which is the whole of what makes Enter walk
                        // through the matches: a find that closed on its first hit would need
                        // re-typing to see the second. `Esc` is what ends it.
                        Field::Finding => {
                            let hit = self.buf_mut().text.find(&typed);
                            let line = self.buf().text.cursor().0;
                            self.status = if hit {
                                alloc::format!("found {typed}")
                            } else {
                                alloc::format!("no {typed}")
                            };
                            self.find_report = Some(hit.then_some(line));
                        }
                    }
                }
                code => {
                    if let Some((_, f)) = self.field.as_mut() {
                        f.apply(code, k.modifiers);
                    }
                }
            }
            return;
        }
        if k.modifiers & MOD_CTRL != 0 {
            // **The cycle is taken before the table**, because it is not a menu row: `Ctrl+Shift+V`
            // continues a paste rather than starting an action, and it means nothing except
            // immediately after one. `Accel::matches` is exact on modifiers, so `Ctrl+V` and
            // `Ctrl+Shift+V` could not be confused — but a *continuation* has no business in a
            // list of things you can choose.
            if is_cycle_chord(k) {
                self.clip_request = Some(ClipRequest::Cycle);
                return;
            }
            // **Matched against the menu table rather than a `match` on keycodes** (M14 decision
            // 2). The constants above are still the source of truth — the table names them — but
            // there is now one statement of "Ctrl+Z undoes" rather than a label in a menu and a
            // branch here that could stop agreeing with it.
            if let Some(msg) = libui::menu::accel_match(&self.menu_table(), &k) {
                self.update(msg);
                return;
            }
            // **Every other chord is swallowed, not passed on.** `Ctrl+X` folding to a
            // printable character would otherwise type it, which is how an editor inserts junk
            // when a person reaches for a shortcut it does not have. This used to be a `match`
            // with a `_` arm; the table above answers the named ones now, so what is left is the
            // fall-through and it needs no arms.
            return;
        }
        self.buf_mut().text.apply(k.keycode, k.modifiers);
    }

    /// The bar's menus, in bar order.
    ///
    /// **Built rather than stored**, and it takes `&self` because half the rows depend on state:
    /// Cut and Copy need a selection, and Save is pointless on a buffer that matches the disk.
    /// The chords are the same constants the keyboard used to `match` on — the table is where
    /// they are declared now, and [`libui::menu::accel_match`] is what reads it back.
    pub fn menu_table(&self) -> Vec<Menu<Msg>> {
        let has_selection = self.buf().text.has_selection();
        // **Nothing is available while a field is open** (PR #280 review, worth fixing 3). `key`
        // has always given every keystroke to an open field — "while a name is being typed the
        // keys are the field's", chords included — and the menu rows this part added were a way
        // *around* that rule rather than a second statement of it: Find while naming replaced the
        // field and silently abandoned the save, and Undo edited the buffer behind an open
        // prompt. Greying the whole bar says the same thing in the vocabulary this part
        // introduced, and Esc is one key away from a menu that works again.
        let free = self.field.is_none();
        let act = |it: Item<Msg>| it.enabled(free);
        vec![
            Menu {
                title: "File",
                items: vec![
                    act(Item::new("New Tab", Accel::ctrl(NEW_TAB_KEYCODE, "T"), Msg::NewTab)),
                    act(Item::new(
                        "Close Tab",
                        Accel::ctrl(CLOSE_TAB_KEYCODE, "W"),
                        Msg::CloseTab(self.current),
                    )),
                    Item::Separator,
                    // **Enabled on a clean buffer too.** Saving one is a no-op on disk, but an
                    // untitled buffer is "dirty or not" independently of whether it has a path,
                    // and a Save that greyed out would hide the *name* prompt behind a state a
                    // person cannot see. `nxfiles`'s greying is about a selection, which is
                    // visible; this is not.
                    act(Item::new("Open…", Accel::ctrl(OPEN_KEYCODE, "O"), Msg::OpenFile)),
                    act(Item::new("Save", Accel::ctrl(SAVE_KEYCODE, "S"), Msg::Save)),
                    act(Item::new(
                        "Save As…",
                        Accel::ctrl_shift(SAVE_AS_KEYCODE, "S"),
                        Msg::SaveAs,
                    )),
                    Item::Separator,
                    act(Item::new(
                        "New Window",
                        Accel::ctrl_shift(NEW_WINDOW_KEYCODE, "N"),
                        Msg::NewWindow,
                    )),
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
                    Item::new("Cut", Accel::ctrl(CUT_KEYCODE, "X"), Msg::Cut)
                        .enabled(free && has_selection),
                    Item::new("Copy", Accel::ctrl(COPY_KEYCODE, "C"), Msg::Copy)
                        .enabled(free && has_selection),
                    act(Item::new("Paste", Accel::ctrl(PASTE_KEYCODE, "V"), Msg::Paste)),
                    Item::Separator,
                    act(Item::new("Find", Accel::ctrl(FIND_KEYCODE, "F"), Msg::Find)),
                ],
            },
        ]
    }

    /// The chooser's tree — the root of a window of its own, like the menu and the question.
    ///
    /// **Empty when nothing is being chosen**, which cannot be drawn because the window only
    /// exists while a chooser does — but the type demands an answer and a caller should not have
    /// to think about it.
    pub fn chooser_view(&mut self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        let Some(c) = self.chooser.as_mut() else {
            return libui::widget::popup_frame(padding(Insets::all(2), libui::element::text("")), ui);
        };
        // **The row keys are indices**, which is safe here and nowhere else: a chooser's listing
        // cannot change under it — nothing lists a directory while one is open — so unlike a tab
        // or a buffer there is no renumbering for a stale message to fall foul of.
        let rows: Vec<libui::widget::ListRow<'_>> = c
            .entries
            .iter()
            .enumerate()
            .map(|(i, (name, _))| libui::widget::ListRow { key: i as u64, label: name })
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
            Msg::ChooserAccept,
            Msg::ChooserCancel,
            ui,
        )
    }

    /// The open menu's popup, framed — the root of a second window, not a layer in this one.
    pub fn menu_view(&self, which: usize, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        let menus = self.menu_table();
        match menus.get(which) {
            Some(m) => libui::menu::popup(m, &self.menus, MENU_ROW_KEY, hovered, ui),
            None => {
                libui::widget::popup_frame(padding(Insets::all(2), libui::element::text("")), ui)
            }
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

    /// Drive the chooser from the keyboard.
    ///
    /// **The application types into the field, not the toolkit** — the same arrangement the
    /// naming field has had since M12: `text_field` draws a value and a caret, and what a
    /// keystroke *means* is the application's, because only it knows whether a key is text or an
    /// answer. `Esc` and `Enter` are the two answers; the arrows move the list; everything else
    /// is a character, and only when there is a field to put it in.
    pub fn chooser_key(&mut self, k: KeyEvent) -> bool {
        // **Fully qualified, as every other keycode in this file is.** `KEY_DOWN` names two
        // different things one import apart — `librsproto::surface::KEY_DOWN` is a key's *state*
        // and is what the rest of this file means by it, while `libkern::abi::KEY_DOWN` is the
        // arrow. Importing the second here would shadow the first inside this one function.
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
                    // **Nothing to type into when opening**, and swallowing the key is the point:
                    // a dialog holds the keyboard, so a character that fell through would reach
                    // nothing at all rather than the buffer behind it.
                    _ if c.mode == chooser::Mode::Save => {
                        c.state.name.apply(code, k.modifiers);
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
        // **The selection resets**, for `nxfiles::show`'s reason: a listing of a *different*
        // directory has no row the old selection refers to, and a clamped stale index silently
        // selects whatever happens to sit at that position.
        c.state.list = libui::widget::ListState { selected: None, offset: 0 };
    }

    /// Open the chooser, looking at the current buffer's directory.
    ///
    /// **Where the buffer already is**, not `home`: a Save As almost always means "beside this
    /// file, under another name", and an untitled buffer has no directory of its own to offer.
    fn open_chooser(&mut self, mode: chooser::Mode) {
        let path = String::from(self.buf().path.as_str());
        let dir = match path.rfind('/') {
            Some(0) | None => String::from("/"),
            Some(i) => String::from(&path[..i]),
        };
        let dir = if path.is_empty() { String::from(&self.home) } else { dir };
        let state = match mode {
            chooser::Mode::Open => ChooserState::new(),
            // Seeded with what the file is called, so Save As starts from the name rather than
            // from nothing.
            chooser::Mode::Save => ChooserState::saving(&self.buf().name),
        };
        self.chooser_list = Some(dir.clone());
        self.chooser = Some(Chooser { mode, dir, entries: Vec::new(), state });
    }

    /// A row was activated: descend into a directory, or take a file as the answer.
    fn chooser_row(&mut self, key: u64) {
        let Some(c) = self.chooser.as_mut() else { return };
        let Some((name, is_dir)) = c.entries.get(key as usize).cloned() else { return };
        if is_dir {
            let dir = join(&c.dir, &name);
            self.chooser_list = Some(dir);
            return;
        }
        c.state.list.selected = Some(key as usize);
        // **A file is the answer in either mode**, and in Save it becomes the *name* rather than
        // the choice — picking an existing file to overwrite is how a Save dialog is used, and
        // the accepting button is still what commits it.
        match c.mode {
            chooser::Mode::Open => self.chooser_accept(),
            chooser::Mode::Save => c.state.name = libui::widget::TextFieldState::with_text(&name),
        }
    }

    /// The chooser's answer.
    fn chooser_accept(&mut self) {
        let Some(c) = self.chooser.as_ref() else { return };
        let path = match c.mode {
            chooser::Mode::Open => {
                let Some(i) = c.state.list.selected else {
                    self.status = String::from("nothing chosen");
                    return;
                };
                let Some((name, _)) = c.entries.get(i) else { return };
                join(&c.dir, name)
            }
            chooser::Mode::Save => {
                let name = String::from(c.state.name.text());
                let name = name.trim();
                if name.is_empty() {
                    // Left open rather than cancelled: the person is mid-answer. The same
                    // decision the naming field makes, for the same reason.
                    self.status = String::from("a name, then Save");
                    return;
                }
                join(&c.dir, name)
            }
        };
        let mode = c.mode;
        self.chooser = None;
        match mode {
            chooser::Mode::Open => self.open_requested = Some(path),
            chooser::Mode::Save => self.adopt_path(&path),
        }
    }

    /// Make `path` what the current buffer **is**, and save it there.
    ///
    /// **This is the whole of "Save As is not Save with a prompt"** (M14 Part C). A prompt that
    /// only chose a destination would write the bytes and leave the buffer believing it is still
    /// the file it was opened as — so the tab would keep the old name, the unsaved marker would
    /// be about the old file, and the *next* `Ctrl+S` would write back to it. Changing what the
    /// buffer is makes all three follow, because each of them reads the buffer's path.
    ///
    /// Shared with the naming field an untitled buffer already had, which does exactly this with
    /// a name joined to `home` — one statement of what naming a buffer means rather than two.
    pub fn adopt_path(&mut self, path: &str) {
        let name = String::from(libfs::basename_str(path));
        let b = self.buf_mut();
        b.path = String::from(path);
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
    /// **Either field** — a name or a search — since M12 Part C, which is why the receipt that
    /// reports it says *which*: [`Field::label`], read through [`field_kind`](Self::field_kind).
    /// A caller that assumed this was only ever a name would build a line a gate waits on for
    /// ever while somebody types a search.
    ///
    /// **The receipt for the things typed here that are not the buffer.** `revision` covers
    /// edits; naming an untitled buffer changes nothing the revision counter can see, and a
    /// gate driving a release image has no rendered field to read — so seven injected
    /// keystrokes were seven chances to lose one and discover it as a file called `scrath`.
    /// A count rather than the text, for the reason `revision` is a count.
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

    /// The dialog could not be opened, so the question cannot be asked.
    ///
    /// **The window stays, and the buffer stays.** The two alternatives are both worse: exiting
    /// would discard unsaved work on the strength of a failed window creation, and leaving
    /// [`confirming`](Self::confirming) set would make an editor that can never be closed at
    /// all, because every later `Close` would try to open the same dialog again. So the close is
    /// abandoned and the strip says why — a person can save and close, which is the thing they
    /// were being asked about.
    pub fn confirm_failed(&mut self) {
        self.confirming = None;
        self.status = String::from("could not ask about the unsaved buffer — save, then close");
    }

    /// What the dialog does with a key.
    ///
    /// **`Esc` only, and it is the cautious answer.** A modal a person cannot dismiss from the
    /// keyboard is a modal that has taken the keyboard hostage, and `Esc` is what the naming
    /// field already uses for "never mind" — one rule for the two places in this editor where
    /// the keys stop being the buffer's. There is deliberately no key for *discard*: `Enter` is
    /// the obvious candidate and the obvious accident, since the question arrives while somebody
    /// is typing.
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
        // `WINDOW_FRAME_H` too, since M11 Part E batch 2b — see `nxfiles::App::list_h`. And
        // `MENU_BAR_H` since M14 Part A: a bar that took its height out of the text area would
        // be chrome the person paid for in rows of their document.
        self.window.h.saturating_sub(
            TITLE_BAR_H + MENU_BAR_H + TAB_STRIP_H + STATUS_H + GRIP_W + WINDOW_FRAME_H,
        )
    }

    /// What the title bar shows: the file's name, marked when the buffer differs from the disk.
    ///
    /// **A leading marker rather than a trailing one**, so it is in the same place whatever the
    /// name's length — a mark that moves is a mark that has to be looked for. The window's
    /// *title* (what the taskbar shows) is set once and stays the name alone: retitling on every
    /// keystroke is a message per keystroke to say something the window itself already shows.
    pub fn title(&self) -> String {
        if self.buf().name.is_empty() {
            // **Named for what it is, not left blank.** A window whose title bar says nothing
            // reads as a window that failed to load something.
            return if self.modified() {
                String::from("* untitled")
            } else {
                String::from("untitled")
            };
        }
        if self.modified() {
            alloc::format!("* {}", self.buf().name)
        } else {
            self.buf().name.clone()
        }
    }

    /// The element tree for the current state.
    ///
    /// `&mut self` because `text_area` scrolls the state it is handed — see `nxfiles::App::view`
    /// for the bug the value-returning shape shipped.
    ///
    /// **The theme is the caller's**, because the caller paints this tree — and a tree built from
    /// one theme and painted with another is two themes in one frame, which one type makes easy
    /// to write and the old `Theme`/`Palette` split made impossible (PR #262 review, optional 5).
    /// It is also the shape Part C needs: a theme read from a file arrives in `main` and is
    /// handed down, rather than being fetched from a default in the middle of a view.
    pub fn view(&mut self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {

        let title = title_bar(
            &self.title(),
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
            &ui,
        )
        .key(TITLE_KEY);

        // The status strip: the one control, and what the last thing that happened was.
        let strip = row(alloc::vec![
            button(
                "save",
                Msg::Save,
                WidgetState { hovered: hovered == Some(SAVE_KEY), ..Default::default() },
                &ui,
            )
            .key(SAVE_KEY),
            // **The field replaces the status, it does not sit beside it.** The strip is one row
            // of chrome and a name being typed *is* what last happened — showing both would make
            // a person read two things to find out which one is asking for an answer.
            match self.field.as_ref() {
                Some((_, f)) => padding(
                    Insets { top: 2, right: 6, bottom: 2, left: 6 },
                    text_field(f, false, WidgetState { active: true, ..Default::default() }, &ui),
                )
                .key(STATUS_KEY)
                .flex(1),
                None => padding(
                    Insets { top: 4, right: 4, bottom: 4, left: 6 },
                    text(self.status.clone()),
                )
                .key(STATUS_KEY),
            },
        ]);

        // **The tab strip, drawn whatever the count.** A strip that appeared with the second
        // tab would move everything below it the moment a file was dropped in — the window's
        // content jumping under the pointer that dropped it. One tab is a strip with one tab.
        let tabs: Vec<(u64, String, bool)> = self.tabs();
        let items: Vec<libui::widget::Tab<'_>> = tabs
            .iter()
            .map(|(k, name, marked)| libui::widget::Tab {
                key: *k,
                label: name.as_str(),
                marked: *marked,
            })
            .collect();
        let strip_tabs = tab_strip(
            &items,
            self.current,
            hovered,
            Msg::SelectTab,
            Msg::CloseTab,
            &ui,
        );

        let h = self.area_h();
        // **The text area takes drops; the chrome does not.** That distinction is the whole of
        // what decision 3 buys — the compositor knows only that this window declared an
        // acceptor, and *where* on the window a drop means something is decided here, by which
        // element is under the point. Dropping a file on the title bar does nothing, which is
        // the honest answer: the title bar is not where a document goes.
        let focused = self.focused;
        let area =
            text_area(&mut self.buf_mut().text, h, ROW_H, focused, &ui).on_drop(Msg::Dropped);

        let body = window_frame(
            title,
            dock(
                alloc::vec![
                    // **The menu bar is above the tab strip** (M14 Part A). A dock's `Top`
                    // children stack in order, so being first is what puts it there — and the
                    // order is the decision: `nxfiles` docks its bar above its strip, and a
                    // person who learned where the menus are in one window should not have to
                    // look somewhere else in the next. A bar *below* a tab strip also reads as
                    // belonging to the tab rather than to the window, which it does not.
                    docked(
                        Edge::Top,
                        libui::menu::bar(
                            &self.menu_table(),
                            &self.menus,
                            MENU_BAR_KEY,
                            hovered,
                            Msg::MenuBar,
                            &ui,
                            MENU_BAR_H,
                        )
                        .key(BAR_KEY),
                    ),
                    docked(
                        Edge::Top,
                        sized(Size::new(0, TAB_STRIP_H), strip_tabs).key(TAB_STRIP_KEY),
                    ),
                    docked(Edge::Top, sized(Size::new(0, STATUS_H), strip).key(STRIP_KEY)),
                ],
            // Sized to the height it was built for, like every scrolling widget in this tree:
            // the dock's flex child otherwise gets whatever is left, and the widget would build
            // rows for one height and be drawn at another.
                sized(Size::new(0, h), area).key(AREA_KEY),
            ),
            &ui,
        );

        let grip = offset(
            self.window.w.saturating_sub(GRIP_W) as i32,
            self.window.h.saturating_sub(GRIP_W) as i32,
            resize_grip(Msg::ResizeWindow(RESIZE_RIGHT | RESIZE_BOTTOM), &ui).key(GRIP_KEY),
        );
        stack(alloc::vec![body, grip])
    }

    /// The element tree for the confirmation dialog — a **second window's** whole face.
    ///
    /// **Sized rather than measured**, which is why the frame below is `window_frame` like every
    /// other window's rather than something that could report a natural size: `libui`'s `Dock`
    /// measures as everything it is offered, deliberately, so a tree containing one has no
    /// natural size at all and [`libui::window::Child::open`] refuses it. Wrapping the whole
    /// thing in a fixed `sized` is what makes the measurement exact — see [`CONFIRM_W`] for why
    /// a dialog wants a fixed size in the first place.
    ///
    /// **The name is on its own line.** Folded into the question it would push a long file name
    /// off the right edge and take the `?` with it, so what clips is the name and never the
    /// sentence.
    pub fn confirm_view(&self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        let title = title_bar(
            "Unsaved changes",
            self.confirm_focused,
            Msg::DragConfirm,
            // **One button, and it is the cautious answer.** Minimise and maximise are absent
            // rather than present-and-inert: a control that looks live and is not is the defect
            // M8's overview shipped three of, and `title_bar` draws only the buttons it is given
            // a message for. Closing the question is *keep editing* — the dialog's own frame
            // must not be a third way to discard a buffer.
            TitleButtons {
                minimise: None,
                maximise: None,
                close: Some(Msg::KeepEditing),
            },
            ui,
        )
        .key(CONFIRM_TITLE_KEY);

        // **The buffer the question is about, which is not always the current one**: closing a
        // *tab* asks about that tab even if the person switched away while the dialog was up.
        // The key was captured when the question was asked — M12 Part B's lesson, one part on.
        let subject = match self.confirming {
            Some(Closing::Tab(k)) => self.buffers.iter().find(|b| b.key == k),
            _ => Some(self.buf()),
        };
        let name = subject
            .map(|b| if b.name.is_empty() { "untitled" } else { b.name.as_str() })
            .unwrap_or("untitled");
        let question = padding(
            Insets::all(libui::widget::DIALOG_PAD),
            column(alloc::vec![
                text("Discard unsaved changes?"),
                text(String::from(name)),
            ]),
        )
        .key(CONFIRM_TEXT_KEY);

        // **Two buttons sharing the width equally**, which is what the published aim points
        // name: a quarter across and three quarters across, at a fixed height off the bottom.
        // `dialog_frame` supplies the strip and the fixed size; the labels, the messages and the
        // keys are this application's.
        //
        // **Two answers and not three** — `TODO(dialog-save-answer)`. *Save and close* is the
        // obvious third, and it is not a button: an untitled buffer has nowhere to save to, so
        // it is the naming field's flow and then a close. See `deferred-decisions.md`.
        let answer = |label: &str, msg: Msg, key: u64| {
            button(
                label,
                msg,
                WidgetState { hovered: hovered == Some(key), ..Default::default() },
                ui,
            )
            .key(key)
            .flex(1)
        };
        let buttons = with_spacing(
            row(alloc::vec![
                answer("discard", Msg::Discard, CONFIRM_DISCARD_KEY),
                answer("keep editing", Msg::KeepEditing, CONFIRM_KEEP_KEY),
            ]),
            libui::widget::DIALOG_GAP,
        );
        dialog_frame(title, question, buttons, ui)
    }
}

/// `"<n> bytes <what>"`, the status strip's one sentence.
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

/// The buffer as bytes, with a trailing newline if the last line has text.
///
/// **What a text file is**, and the editor is where the convention is applied rather than in
/// `libfs`: every line in a text file ends with a newline, so a file whose last line does not
/// is a file that `open` and every other reader treats as one line shorter than it looks. A
/// buffer whose last line is empty already ends with one — `TextAreaState::text` joins lines
/// with `\n`, so `"a\n"` is two lines, the second empty.
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

    /// An editor with `hello` open, as if the file had been read.
    fn app() -> App {
        let mut a = App::new("/home/notes.txt", "/home");
        // `hello\n` on disk, which is what `hello` writes back — so this buffer starts matching
        // its file, which every test below depends on.
        a.loaded("hello", b"hello\n");
        a
    }

    /// A key press, as the compositor delivers one.
    fn key(a: &mut App, keycode: u16, modifiers: u16) {
        a.update(Msg::Key(KeyEvent::new(1, keycode, KEY_DOWN as u16, modifiers)));
    }

    /// `x`, the one letter these tests type.
    const KEY_X: u16 = 45;
    /// `1` — the key that tells a swallowed chord from an unprintable one.
    const KEY_1: u16 = 2;

    #[test]
    fn a_failed_save_keeps_the_buffer_and_says_so() {
        // **The one rule this application exists to get right.** An editor that loses your work
        // quietly is the one thing an editor must never be, so a failure changes the message and
        // nothing else: the text stays, and it stays *modified*, because a buffer marked saved
        // is one a person can close without being asked.
        let mut a = app();
        key(&mut a, KEY_X, 0);
        let text = a.text();
        assert!(a.modified());

        a.saved(a.current_tab(), Err("the file could not be replaced"));
        assert_eq!(a.text(), text, "the buffer is what it was");
        assert!(a.modified(), "and it is still unsaved");
        assert!(a.status().contains("NOT saved"), "status was {:?}", a.status());

        // And a save that works clears exactly that.
        a.update(Msg::Save);
        let (saved_key, _, owed) = a.take_save().expect("a save was asked for");
        a.saved(saved_key, Ok(owed.len()));
        assert!(!a.modified(), "a successful save is what marks it saved");
    }

    #[test]
    fn a_buffer_that_could_not_be_read_refuses_to_be_written() {
        // The danger is the empty window: a failed read shows nothing, and saving nothing over a
        // file is that file destroyed by an editor that never displayed it.
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
        // **The one case where the editor would write something it never showed.** Reading
        // normalises line endings, so a CRLF file is already something else on screen — and an
        // editor that called that unmodified would rewrite it, two bytes shorter per line, on a
        // `Ctrl+S` pressed out of habit (PR #259 review, finding 3).
        let mut a = App::new("/home/dos.txt", "/home");
        a.loaded("alpha\nbeta\n", b"alpha\r\nbeta\r\n");
        assert!(a.modified(), "the buffer is not what the file holds");
        assert_eq!(a.title(), "* dos.txt", "and the title says so before anything is typed");
        assert!(a.status().contains("line endings"), "status was {:?}", a.status());
        assert_eq!(a.refusal(), None, "it is writable — it just is not the same bytes");

        // Saving makes the file match the buffer, which is what clears it.
        a.update(Msg::Save);
        let (saved_key, _, owed) = a.take_save().expect("a blocked buffer is a different thing");
        a.saved(saved_key, Ok(owed.len()));
        assert!(!a.modified());
        assert_eq!(a.title(), "dos.txt");

        // And a file that *does* survive is not marked: this must distinguish, or it is a
        // permanent asterisk rather than an answer.
        let mut b = App::new("/home/unix.txt", "/home");
        b.loaded("alpha\nbeta\n", b"alpha\nbeta\n");
        assert!(!b.modified());
    }

    #[test]
    fn a_drop_opens_a_tab_and_never_costs_the_one_that_is_open() {
        // **The rule this replaces, and why it went** (M12 Part D). A drop used to *replace* the
        // buffer, so it had to be refused while there was unsaved work — a drop that visibly did
        // nothing, for a reason the person had to read the strip to find. With tabs there is
        // nothing to lose by taking it, so the refusal is gone and the protection is stronger:
        // the modified buffer is not touched at all.
        let mut a = app();
        key(&mut a, KEY_X, 0);
        assert!(a.modified(), "precondition: there is something to lose");
        let before = a.text();

        assert!(a.accept_drop("/home/other.txt"), "taken, not refused");
        assert_eq!(a.path(), "/home/other.txt", "and it is what is on screen now");
        assert_eq!(a.tab_count(), 2);

        // The first buffer is still there, still modified, still exactly as it was.
        a.update(Msg::SelectTab(TAB_KEY_BASE));
        assert_eq!(a.path(), "/home/notes.txt");
        assert_eq!(a.text(), before);
        assert!(a.modified());
    }

    #[test]
    fn dropping_a_file_that_is_already_open_switches_to_its_tab() {
        // Two tabs on one file are two buffers that can disagree about what is in it, and the
        // last one saved wins silently.
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
        // The likeliest accident with a browser beside an editor — and re-reading would be a
        // way to lose an unsaved buffer that the rule above has just protected.
        let mut a = app();
        assert!(!a.accept_drop("/home/notes.txt"));
        assert!(a.status().contains("already open"), "status was {:?}", a.status());
        assert_eq!(a.path(), "/home/notes.txt");
    }

    #[test]
    fn an_untitled_buffer_is_named_on_save_and_saved_where_it_was_named() {
        // **This is what "nxedit doesn't launch from the menu" was**: the applications modal
        // passes no arguments, the editor required `argv[1]`, so it started and exited (M11
        // Part E batch 7).
        let mut a = App::new("", "/home/alice");
        assert_eq!(a.title(), "untitled", "an unnamed buffer has no title to show");
        assert!(a.take_save().is_none(), "an untitled buffer had somewhere to be written");

        // Save asks rather than writes. **The control is `take_save`**: a version that set the
        // flag anyway would pass an assertion about the status text alone.
        a.update(Msg::Save);
        assert!(a.take_save().is_none(), "an untitled save wrote to a path nobody chose");
        assert!(a.status().contains("name it"), "status was {:?}", a.status());

        // The keys are the field's now, buffer included.
        // "notes", by keycode — the codes the keymap maps to those letters, which is what an
        // injected keystroke carries.
        for c in [49u16, 24, 20, 18, 31] {
            key(&mut a, c, 0);
        }
        assert_eq!(a.text(), "", "typing a name went into the buffer");

        key(&mut a, libkern::abi::KEY_ENTER, 0);
        assert_eq!(a.path(), "/home/alice/notes", "named into the wrong directory");
        // Unmarked: naming a buffer is not editing it, and this one has had nothing typed into
        // it — the keys went to the field. The mark means "not what is on disk".
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

        // And an empty name is not a name: it would write to the directory itself.
        a.update(Msg::Save);
        key(&mut a, libkern::abi::KEY_ENTER, 0);
        assert_eq!(a.path(), "", "an empty name became a path");
        assert!(a.take_save().is_none());
        assert!(a.status().contains("a name"), "status was {:?}", a.status());
    }

    #[test]
    fn a_named_buffer_saves_without_asking() {
        // The control for the two above: the path that *has* a name must not grow a prompt.
        let mut a = app();
        a.update(Msg::Save);
        assert!(a.take_save().is_some(), "a named buffer asked for a name");
    }

    #[test]
    fn a_missing_file_is_not_a_failure() {
        // Opening a path that is not there is how a file gets made, so the buffer is writable —
        // and the strip says which of the two happened, because a person who meant to open an
        // existing file wants to know they did not.
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
        // Edited again after a save is modified again — the revision never goes backwards.
        key(&mut a, KEY_X, 0);
        assert_eq!(a.title(), "* notes.txt");
    }

    #[test]
    fn ctrl_s_saves_and_no_chord_types() {
        let mut a = app();
        key(&mut a, SAVE_KEYCODE, MOD_CTRL);
        assert_eq!(a.text(), "hello", "nothing was typed");
        assert_eq!(
            a.take_save().map(|(_, _, t)| t).as_deref(),
            Some("hello"),
            "and a save is owed"
        );

        // **A digit, because a letter proves nothing here.** `libinput`'s keymap folds
        // `Ctrl`+letter to a control byte, which `TextAreaState::apply` already declines — so a
        // test using `Ctrl+X` passes with this guard deleted, which is what the first version of
        // it did. The keymap folds *letters only*: `Ctrl+1` is still `1`, printable, and typed
        // straight into the file by an editor that passes chords through.
        key(&mut a, KEY_1, MOD_CTRL);
        assert_eq!(a.text(), "hello", "a chord this editor does not have types nothing");
        assert_eq!(a.take_save(), None, "and asks for nothing");

        // The same key without the chord is ordinary text, so what is being tested is the
        // modifier and not the key.
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
        // Both edges arrive; acting on both types every character twice.
        let mut a = app();
        a.update(Msg::Key(KeyEvent::new(1, KEY_X, 0, 0)));
        assert_eq!(a.text(), "hello");
    }

    #[test]
    fn what_is_written_ends_with_a_newline() {
        // **What a text file is.** A last line with no newline reads as one line shorter than it
        // looks to `open` and to every other reader, so the editor applies the convention at the
        // one point it writes bytes.
        assert_eq!(to_bytes("one\ntwo"), b"one\ntwo\n");
        assert_eq!(to_bytes("one\n"), b"one\n", "and does not double it");
        assert_eq!(to_bytes(""), b"", "an empty buffer is an empty file, not a blank line");
    }

    #[test]
    fn the_name_is_the_last_component() {
        // The title's name comes from `libfs`, so this pins what this application depends on
        // rather than re-testing that crate: a trailing separator must not give an empty title.
        let named = |p: &str| App::new(p, "/home").title();
        assert_eq!(named("/home/alice/notes.txt"), "notes.txt");
        assert_eq!(named("notes.txt"), "notes.txt");
        assert_eq!(named("/home/papers/"), "papers", "a trailing separator is not the name");
    }

    #[test]
    fn the_chord_keycodes_are_the_ones_the_keymap_names() {
        // Pinned against the table they have to agree with rather than against the comments
        // beside them, the way the save chord already is.
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
        // **Found by the gate rather than by a host test**, which is worth recording: the editor
        // opened an empty file, six characters were typed, it was saved, two more were typed —
        // and one undo emptied the buffer, because nothing had closed the group across the save.
        // Undoing past a save is fine; undoing *through* one in a single step is not.
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
        // A chord that silently does nothing is indistinguishable from one the editor does not
        // have — which is the discoverability problem, one keystroke at a time.
        let mut a = app();
        key(&mut a, UNDO_KEYCODE, MOD_CTRL);
        assert!(a.status().contains("nothing to undo"), "status was {:?}", a.status());
        key(&mut a, REDO_KEYCODE, MOD_CTRL);
        assert!(a.status().contains("nothing to redo"), "status was {:?}", a.status());
    }

    #[test]
    fn undoing_makes_the_buffer_modified_again() {
        // The whole point of undo in an editor: what is on screen no longer matches the file, so
        // closing has to ask. `modified` is derived from the revision, and `undo` moves it.
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

        // `t`, `w`, `o` — typed into the field rather than into the buffer.
        for code in [20u16, 17, 24] {
            key(&mut a, code, 0);
        }
        assert_eq!(a.text(), "one two\nthree two", "the buffer took none of it");
        assert_eq!(a.naming_len(), Some(3));

        key(&mut a, libkern::abi::KEY_ENTER, 0);
        assert!(a.status().contains("found"), "status was {:?}", a.status());
        // **The field stays open**, so Enter again is the next match rather than a re-type.
        assert_eq!(a.field_kind(), Some(Field::Finding));

        key(&mut a, libkern::abi::KEY_ESC, 0);
        assert_eq!(a.field_kind(), None, "and Escape ends it");
    }

    // --- the clipboard (M12 Part E) -----------------------------------------

    /// Select `needle` in the current buffer, the way `Ctrl+F` does.
    fn select(a: &mut App, needle: &str) {
        assert!(a.buf_mut().text.find(needle), "the fixture contains {needle}");
    }

    #[test]
    fn copying_nothing_is_not_a_request() {
        // **A copy that pushed an empty entry would move the ring's serial** under every client
        // that was mid-cycle, and push whatever was last copied one place further back — for a
        // gesture that had nothing to copy.
        //
        // **Two guards since M14 Part A, and they say different things.** The Edit row is greyed
        // with no selection, so `Ctrl+C` is declined by the table before it reaches anything —
        // the chord and the row agree, which is the whole of decision 2. Reaching `copy` some
        // other way still gets the explanation.
        let mut a = app();
        let before = String::from(a.status());
        key(&mut a, COPY_KEYCODE, MOD_CTRL);
        assert_eq!(a.take_clip_request(), None);
        assert_eq!(a.status(), before, "a greyed row's chord does nothing, silently");
        // The row is greyed, which is what makes the silence legible rather than a bug.
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
        // And the message is still there for a caller that arrives another way.
        a.update(Msg::Copy);
        assert_eq!(a.take_clip_request(), None);
        assert_eq!(a.status(), "nothing to copy");
        // **The negative control**: with a selection the same chord does reach the clipboard,
        // so the silence above is about the selection and not about the chord being unwired.
        select(&mut a, "ell");
        key(&mut a, COPY_KEYCODE, MOD_CTRL);
        assert_eq!(a.take_clip_request(), Some(ClipRequest::Copy(String::from("ell"))));
    }

    /// No menu row acts while a field is open, which is the rule `key` has always followed.
    ///
    /// **The demonstrated failure** (PR #280 review, worth fixing 3): with an untitled buffer's
    /// naming prompt open and a filename half typed, Edit ▸ Find replaced the field — losing the
    /// name and silently abandoning the save — and Edit ▸ Undo edited the buffer behind the
    /// prompt. The menu was a way *around* the "while a name is being typed the keys are the
    /// field's" invariant rather than a second statement of it.
    #[test]
    fn no_menu_row_acts_while_a_field_is_open() {
        // An untitled buffer, so Save asks for a name.
        let mut a = App::new("", "/home");
        a.update(Msg::Save);
        key(&mut a, 30, 0); // `a`, one character of the filename
        assert_eq!(a.field_kind(), Some(Field::Naming));
        assert_eq!(a.naming_len(), Some(1), "a name is half typed");

        // **Every row, not the two that were demonstrated.** A rule asked of the table has to be
        // checked against the table, or the next row added is the one that forgets.
        for it in a.menu_table().iter().flat_map(|m| m.items.iter()) {
            let Item::Action { label, enabled, .. } = it else { continue };
            assert!(!enabled, "{label} is offered while a name is being typed");
        }
        // Nothing reached the field or the buffer: the state is exactly what it was.
        assert_eq!(a.field_kind(), Some(Field::Naming));
        assert_eq!(a.naming_len(), Some(1));

        // **The negative control**, and it is the whole test: close the field and the same rows
        // come back. Without it this would pass for a version that disabled everything always.
        key(&mut a, libkern::abi::KEY_ESC, 0);
        assert_eq!(a.field_kind(), None, "Esc closed the prompt");
        let live = a
            .menu_table()
            .iter()
            .flat_map(|m| m.items.iter())
            .filter(|it| matches!(it, Item::Action { enabled: true, .. }))
            .count();
        assert_eq!(live, 12, "everything but Cut and Copy, which want a selection");
    }

    /// The chord a menu row advertises does what choosing that row does.
    ///
    /// **This drives `App::key`**, which is the whole point and is what the first version of this
    /// test did not do (PR #280 review, worth fixing 4). It built an event from a row's `Accel`
    /// and asked `accel_match` about it — both sides of one table, with the application never
    /// consulted. That pins the table against shadowing chords, which is worth having and is
    /// asserted below; it does not pin that anything *routes* through it, and the test passed
    /// with `App::key`'s `accel_match` call deleted.
    ///
    /// **The property is stated as an equivalence rather than per row**, which is what keeps it
    /// from being a second list of what each action does: two identical editors, one sent the
    /// row's message and one sent the row's chord, must end in the same state. A per-row
    /// assertion here would be exactly the drift decision 2 exists to prevent.
    #[test]
    fn every_advertised_chord_does_what_its_row_says() {
        /// Everything about an editor that any of these rows can move.
        fn digest(a: &mut App) -> String {
            let clip = a.take_clip_request();
            let save = a.take_save();
            // **Every outbox, and this is the third time the control below has said so** — Close
            // Tab, Save, and now New Window and Quit, each of which changes nothing a digest
            // watching only the buffer can see.
            let (nw, q) = (a.take_new_window(), a.take_quit());
            // **And the chooser's**, which is the fourth time this control has caught a digest
            // that could not see a new outbox: opening a chooser changes no buffer and no tab.
            let (cl, op) = (a.take_chooser_list(), a.take_open());
            let choosing = a.chooser().map(|c| c.mode);
            // **`closing` and `save` are in here because the negative control found them
            // missing.** Close Tab on a lone tab closes the *window* — the chord means that
            // everywhere it exists — and Save on a titled buffer records an outbox entry rather
            // than touching the text. A digest that watched only the buffer said both rows
            // changed nothing, which is exactly what that control is for.
            alloc::format!(
                "{:?}|{}|{:?}|{clip:?}|{save:?}|{}|{:?}|{}{}|{nw}{q}|{cl:?}{op:?}{choosing:?}",
                a.status(),
                a.text(),
                a.field_kind(),
                a.tabs().len(),
                a.naming_len(),
                a.closing(),
                a.confirming(),
            )
        }
        // **With a selection**, so Cut and Copy are live: the loop below skips disabled rows,
        // and a table built from an untouched editor would quietly check seven of the nine.
        let mut fixture = app();
        select(&mut fixture, "ell");
        let table = fixture.menu_table();
        let mut checked = 0;
        for it in table.iter().flat_map(|m| m.items.iter()) {
            let Item::Action { accel: Some(acc), msg, label, enabled: true } = it else { continue };
            checked += 1;
            // The table has no two rows on one chord, which is the property the old version of
            // this test pinned and the only one it pinned.
            let ev = KeyEvent::new(1, acc.key(), KEY_DOWN as u16, acc.mods());
            assert_eq!(
                libui::menu::accel_match(&table, &ev).as_ref(),
                Some(msg),
                "{label} advertises {} and the table hands it to another row",
                acc.label()
            );
            // …and the application routes through it. Two editors from the same fixture, with a
            // selection so the rows that need one are live.
            let (mut by_row, mut by_chord) = (app(), app());
            for a in [&mut by_row, &mut by_chord] {
                select(a, "ell");
            }
            by_row.update(msg.clone());
            by_chord.update(Msg::Key(ev));
            let mut untouched = app();
            select(&mut untouched, "ell");
            // **Each digest is taken exactly once**, because it is destructive: it drains the
            // clipboard and save outboxes, which is the only way to see them. Calling it twice
            // on one editor reports the second call's emptiness as a difference — which is what
            // the first version of this test did, and it read as "Save changes nothing".
            let (by_row, by_chord, untouched) =
                (digest(&mut by_row), digest(&mut by_chord), digest(&mut untouched));
            assert_eq!(
                by_chord, by_row,
                "{label}: {} does not do what choosing the row does",
                acc.label()
            );
            // **The negative control**: the two agreeing is only evidence if the row moved
            // something. An editor that ignored both would pass the assertion above.
            assert_ne!(
                by_row, untouched,
                "{label} changes nothing, so this row proves nothing about routing"
            );
        }
        assert_eq!(checked, 13, "every row but Close Window carries a chord");
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
        // **M12 decision 3's rule, and the whole reason a stale ring position is unreachable
        // rather than merely unlikely.** The position exists only inside one uninterrupted
        // gesture, so a typed character, a save, a tab switch — anything — has to end it.
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
        // …and the cycle chord itself does not.
        let mut a = app();
        a.pasted("WORLD", 0, 7);
        key(&mut a, PASTE_KEYCODE, MOD_CTRL | MOD_SHIFT);
        assert!(a.cycling().is_some(), "the one key that continues it");
    }

    #[test]
    fn a_cycle_into_a_different_tab_does_nothing() {
        // The range names positions in the buffer that was pasted into. Applying it to another
        // would replace whatever happens to be at those coordinates — a silent edit to a file
        // the person was not looking at.
        let mut a = app();
        a.pasted("WORLD", 0, 7);
        let c = a.cycling().expect("armed");
        a.update(Msg::NewTab);
        // **Re-armed by hand, with the *first* tab's key.** `NewTab` cleared the state, which is
        // the rule the test above covers; what is under test here is the other guard — a
        // `Cycling` naming a buffer that is no longer current.
        a.cycling = Some(c);
        let before = a.text();
        a.cycled("MARS", 1, 7);
        assert_eq!(a.text(), before, "a stale buffer key is not a cycle");
        assert_eq!(a.text(), "", "…and the new tab is still the empty one");
    }

    #[test]
    fn a_paste_is_one_undo_step() {
        // Grouped with typing, one undo would take back the paste *and* the word before it. A
        // person who pastes and then undoes means "not that".
        let mut a = app();
        key(&mut a, KEY_X, 0);
        a.pasted("WORLD", 0, 7);
        assert_eq!(a.text(), "xWORLDhello");
        key(&mut a, UNDO_KEYCODE, MOD_CTRL);
        assert_eq!(a.text(), "xhello", "the paste came back out and the typing stayed");
    }

    #[test]
    fn a_multi_line_paste_keeps_what_followed_the_cursor() {
        // The tail of the line moves to the end of what was inserted. Without that, pasting
        // into the middle of a line silently eats the rest of it.
        let mut a = app();
        a.pasted("one\ntwo", 0, 1);
        assert_eq!(a.text(), "one\ntwohello");
    }

    #[test]
    fn a_tab_chord_during_a_find_still_opens_a_tab() {
        // **The two applications agree about this now** (PR #270 review, optional 7). They grew
        // the same widget in the same part and disagreed: `nxfiles` checked the tab chords before
        // its prompt and this one let the field's branch return first, so `Ctrl+T` during a find
        // did nothing. The line is what the chord acts on — a tab is the *window's*, and the
        // buffer chords stay the field's while a field is open.
        let mut a = app();
        key(&mut a, FIND_KEYCODE, MOD_CTRL);
        for code in [20u16, 19, 24] {
            key(&mut a, code, 0);
        }
        assert_eq!(a.naming_len(), Some(3), "precondition: the field has the keys");

        key(&mut a, NEW_TAB_KEYCODE, MOD_CTRL);
        assert_eq!(a.tab_count(), 2, "the chord reached the window");

        // And a buffer chord in the same state is still the field's: `Ctrl+S` while naming asks
        // nothing new, because a name is what is already being asked for.
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
        // **The batch is what makes this reachable**: `Ctrl+S` only records that a save is owed,
        // and `main` performs it at the top of the *next* iteration — after every event in the
        // drain has been applied. A tab click in the same drain used to move both the bytes and
        // the path to the other buffer, so the wrong file was written, the wrong buffer was
        // marked saved, and the one the person asked for stayed dirty with nothing to show for
        // it (PR #270 review, worth fixing 3). Events queue whenever the client is behind, which
        // `pool.acquire` blocking on the third commit makes ordinary.
        let mut a = app(); // /home/notes.txt holding "hello"
        a.accept_drop("/home/other.txt");
        a.absent();
        let notes = a.tabs()[0].0;
        let other = a.tabs()[1].0;

        a.update(Msg::SelectTab(notes));
        key(&mut a, KEY_X, 0);
        a.update(Msg::Save);
        // …and the same drain carries a tab click.
        a.update(Msg::SelectTab(other));

        let (asked, path, text) = a.take_save().expect("a save was asked for");
        assert_eq!(asked, notes, "the buffer that asked, not the one on screen");
        assert_eq!(path, "/home/notes.txt");
        assert_eq!(text, "xhello");

        // And the result lands on that buffer too: marking the *current* one saved would leave
        // the written one dirty and the untouched one claiming to match a file it never wrote.
        a.saved(asked, Ok(text.len()));
        a.update(Msg::SelectTab(notes));
        assert!(!a.modified(), "the buffer that was written is the one marked saved");
        a.update(Msg::SelectTab(other));
        assert!(!a.modified(), "and the empty one was never dirty");
    }

    #[test]
    fn tabs_hold_their_own_buffer_cursor_and_history() {
        // **The whole point of the split into `Buffer`.** What a person expects to survive
        // switching tabs is everything about the file — its text, whether it is modified, and
        // what undo would take back. Getting the line wrong is how a second tab inherits the
        // first's history, and it is a mistake that reads as correct until somebody presses
        // `Ctrl+Z` in the wrong tab.
        let mut a = app(); // /home/notes.txt holding "hello"
        key(&mut a, KEY_X, 0);
        key(&mut a, KEY_X, 0);
        let first = a.text();

        a.update(Msg::NewTab);
        assert_eq!(a.tab_count(), 2);
        assert_eq!(a.text(), "", "a new tab is an untitled empty buffer");
        assert_eq!(a.path(), "");

        // Undo in the new tab has nothing to take: the history did not come with it.
        key(&mut a, UNDO_KEYCODE, MOD_CTRL);
        assert!(a.status().contains("nothing to undo"), "status was {:?}", a.status());

        // And the first tab still has both its text and its history.
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
        // **M12 Part B's lesson, one part on.** The key is captured when the question is asked,
        // so switching tabs while the dialog is up cannot make the answer close a different one
        // — and the dialog goes on naming the tab it asked about.
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
        // Which is what keeps `buffers` non-empty — the invariant every accessor relies on. An
        // editor showing no buffer at all is a window with nothing in it and no way back.
        let mut a = app();
        assert_eq!(a.tab_count(), 1);
        a.update(Msg::CloseTab(TAB_KEY_BASE));
        assert!(a.closing(), "the last tab takes the window with it");
    }

    #[test]
    fn closing_the_window_asks_about_any_modified_tab_not_just_the_current_one() {
        // Closing takes every tab, so a question about only what is on screen would let the
        // others go silently.
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
        // **The save button stays in the strip and stays clickable while a search is open**, and
        // "already asking" was written when naming was the only field there could be. With find
        // beside it, pressing save did nothing at all and the strip did not change — a control
        // that looks live and is not, one test over from the one that exists to catch that
        // (PR #269 review, worth fixing 2).
        let mut a = App::new("", "/home");
        a.absent();
        key(&mut a, FIND_KEYCODE, MOD_CTRL);
        assert_eq!(a.field_kind(), Some(Field::Finding));

        a.update(Msg::Save);
        assert_eq!(a.field_kind(), Some(Field::Naming), "the search gives way to the question");
        assert!(a.status().contains("name it"), "status was {:?}", a.status());

        // And a second save while *naming* is still "already asking", which is what that guard
        // was for: it must not throw away what has been typed so far.
        key(&mut a, KEY_X, 0);
        a.update(Msg::Save);
        assert_eq!(a.naming_len(), Some(1), "the name survives a second press");
    }

    #[test]
    fn escaping_a_search_leaves_the_strip_saying_what_last_happened() {
        // Blanking it was a control answering with nothing, and what it erased was the answer to
        // the search being escaped — still the most recent thing that happened.
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
        // **Find leaves its match selected, and typing replaces a selection.** Both halves are
        // right on their own, and together they are hand-made find-and-replace — which is worth
        // a test because it is also a surprise: `check-login` typed a character two steps after a
        // search and edited the middle of the file rather than appending to it, and the seven
        // bytes that reached disk were the first anybody knew (M12 Part D).
        let mut a = App::new("/home/notes.txt", "/home");
        a.loaded("nitroxab", b"nitroxab\n");
        key(&mut a, FIND_KEYCODE, MOD_CTRL);
        // `t`, `r`, `o`
        for code in [20u16, 19, 24] {
            key(&mut a, code, 0);
        }
        key(&mut a, libkern::abi::KEY_ENTER, 0);
        key(&mut a, libkern::abi::KEY_ESC, 0);

        key(&mut a, KEY_X, 0);
        assert_eq!(a.text(), "nixxab", "the match was replaced, not appended to");

        // And a movement first is what appends — which is the shape a person uses to get out of
        // a search, and what the gate does now.
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
        // The rule the save chord established, now that three more chords have joined it: every
        // other one is swallowed rather than folded into a printable character.
        let mut a = app();
        let before = a.text();
        key(&mut a, KEY_1, MOD_CTRL);
        assert_eq!(a.text(), before);
    }

    #[test]
    fn the_save_keycode_is_the_one_the_keymap_calls_s() {
        // The constant is a literal, so it is pinned against the table it has to agree with
        // rather than against the comment beside it.
        assert_eq!(libinput::keymap::to_char(SAVE_KEYCODE, 0), Some(b's'));
    }

    // ---- M12 Part A: the confirmation ----

    #[test]
    fn closing_a_modified_buffer_asks_rather_than_exiting() {
        // **The rule the whole part exists for.** Until M12 Part A this editor answered
        // `CloseRequested` by exiting, and its own comment said so: "an editor with somewhere to
        // put a question would ask it — and this one has no dialog to ask in".
        let mut a = app();
        key(&mut a, KEY_X, 0);
        a.update(Msg::Close);
        assert!(a.confirming(), "a modified buffer asks");
        assert!(!a.closing(), "and does not exit while it is asking");
    }

    #[test]
    fn closing_a_clean_buffer_asks_nothing() {
        // The other half, and the one that keeps the ordinary close a single click: a buffer
        // that matches its file has nothing to lose, so there is nothing to ask about.
        let mut a = app();
        a.update(Msg::Close);
        assert!(!a.confirming(), "nothing to ask about");
        assert!(a.closing());
    }

    #[test]
    fn a_second_close_request_does_not_discard_the_buffer() {
        // **The control for the obvious wrong spelling.** "Ask if we are not already asking,
        // otherwise close" reads fine and turns the *second* `CloseRequested` into an exit —
        // and a second one is exactly what a person clicking a taskbar entry twice produces,
        // which since this part is also how they force a wedged window shut. With that spelling
        // this test fails on the `closing` assertion.
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

        // Keeping ends the question and nothing else — the buffer is byte-for-byte what it was.
        let text = a.text();
        a.update(Msg::KeepEditing);
        assert!(!a.confirming(), "the question is answered");
        assert!(!a.closing(), "and the answer was no");
        assert_eq!(a.text(), text);
        assert!(a.modified(), "keeping does not mark it saved");
        assert!(a.status().contains("still editing"), "status was {:?}", a.status());

        // Asking again, and discarding this time.
        a.update(Msg::Close);
        assert!(a.confirming());
        a.update(Msg::Discard);
        assert!(!a.confirming(), "the dialog goes when it is answered");
        assert!(a.closing());
    }

    #[test]
    fn saving_removes_the_reason_to_ask() {
        // Derived rather than remembered: `confirming` is decided by `modified()` at the moment
        // of the close, so a save between the two makes the question go away by itself.
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
        // **`Esc` because a modal a keyboard cannot dismiss has taken the keyboard hostage**,
        // and nothing else because `Enter` is the obvious accident: the question arrives while
        // somebody is typing.
        let a = app();
        let ev = |code: u16, pressed: u16| KeyEvent::new(1, code, pressed, 0);
        assert_eq!(a.confirm_key(ev(NAME_CANCEL, KEY_DOWN)), Some(Msg::KeepEditing));
        assert_eq!(a.confirm_key(ev(NAME_CONFIRM, KEY_DOWN)), None, "Enter must not discard");
        assert_eq!(a.confirm_key(ev(KEY_X, KEY_DOWN)), None);
        // A release is not a press. Without this guard `Esc` would answer twice, and the second
        // answer would arrive after the window it belongs to had been destroyed.
        assert_eq!(a.confirm_key(ev(NAME_CANCEL, 0)), None, "a release is not an answer");
    }

    #[test]
    fn a_dialog_that_will_not_open_leaves_the_window_alone() {
        // **Neither of the two tempting failures.** Exiting would discard unsaved work because a
        // window could not be created; staying `confirming` would make an editor that can never
        // be closed, since every later `Close` would try the same failing creation again.
        let mut a = app();
        key(&mut a, KEY_X, 0);
        a.update(Msg::Close);
        a.confirm_failed();
        assert!(!a.confirming());
        assert!(!a.closing(), "a failed dialog must not discard the buffer");
        assert!(a.status().contains("save, then close"), "status was {:?}", a.status());
        // And the editor is still usable: a later save works.
        a.update(Msg::Save);
        assert!(a.take_save().is_some());
    }

    /// A fake face: every character an 8x16 box, which is all these tests need.
    const CELL: libui::layout::FixedCell = libui::layout::FixedCell { w: 8, h: 16 };

    #[test]
    fn the_left_answer_discards_and_the_right_one_keeps() {
        // **Which button means what, which is all that is left here.** The dialog's geometry —
        // that it measures to exactly its declared size, and that the two aim points are the
        // buttons' centres — moved into `libui` with `dialog_frame` when `nxfiles` grew the
        // second confirmation, and is tested there against a tree built the same way. What this
        // application still decides is which message each half carries, and getting *that* the
        // wrong way round is a click that discards a buffer when it was asked to keep it.
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

        assert_eq!(
            click(&mut router, DIALOG_LEFT_CX, CONFIRM_BUTTON_CY),
            alloc::vec![Msg::Discard],
        );
        assert_eq!(
            click(&mut router, DIALOG_RIGHT_CX, CONFIRM_BUTTON_CY),
            alloc::vec![Msg::KeepEditing],
        );
    }

    #[test]
    fn the_dialogs_close_button_keeps_the_buffer() {
        // **A dialog's own frame must not be a third way to discard.** `title_bar` draws only
        // the buttons it is given a message for, so the question here is which message the one
        // button carries — and the cautious answer is the only defensible default for a control
        // whose meaning is "make this go away".
        let a = app();
        let ui = a.confirm_view(&UiTheme::default(), None);
        let l = libui::layout::layout(&ui, Rect::new(0, 0, CONFIRM_W, CONFIRM_H), &CELL);
        let mut tree = libui::diff::Tree::new();
        tree.update(&ui, &l).expect("the dialog is diffable");
        let mut router = libui::route::Router::new();
        // The rightmost title-bar button, measured the way every gate in this tree measures one.
        let x = CONFIRM_W as i32 - libui::widget::WINDOW_CONTENT_X as i32
            - (libui::widget::TITLE_BUTTON_W / 2) as i32;
        let y = (libui::widget::WINDOW_CONTENT_Y + TITLE_BAR_H / 2) as i32;
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

    // --- the file chooser (M14 Part C) --------------------------------------

    /// **Save As is not Save with a prompt**, which is the whole of what this part owes.
    ///
    /// A prompt that only chose a destination would write the bytes and leave the buffer
    /// believing it is still the file it was opened as. Three things read the buffer's path, and
    /// all three would be wrong: the tab's label, the title bar's unsaved marker, and — the one
    /// that loses work — where the *next* `Ctrl+S` writes.
    #[test]
    fn save_as_changes_what_the_buffer_is() {
        let mut a = app();
        assert_eq!(a.path(), "/home/notes.txt");

        a.update(Msg::SaveAs);
        // The chooser opens on the buffer's own directory, seeded with its name.
        let c = a.chooser().expect("a chooser is open");
        assert_eq!(c.dir, "/home");
        assert_eq!(c.state.name.text(), "notes.txt");
        assert_eq!(a.take_chooser_list().as_deref(), Some("/home"), "and it asked for a listing");

        // Type a different name and accept.
        let c = a.chooser_mut().expect("a chooser is open");
        c.state.name = libui::widget::TextFieldState::with_text("other.txt");
        a.update(Msg::ChooserAccept);

        assert!(a.chooser().is_none(), "accepting closes it");
        assert_eq!(a.path(), "/home/other.txt", "the buffer *is* the new file");
        // The tab's label follows, because it reads the buffer's name.
        assert!(a.tabs().iter().any(|(_, label, _)| label.contains("other.txt")), "{:?}", a.tabs());
        // And the save that was requested is for the new path, not the old one.
        let (_, path, _) = a.take_save().expect("a save was requested");
        assert_eq!(path, "/home/other.txt", "the next write goes to the new file");
    }

    /// Opening walks into directories and answers with a file.
    #[test]
    fn the_chooser_descends_and_then_answers() {
        let mut a = app();
        a.update(Msg::OpenFile);
        let _ = a.take_chooser_list();
        a.show_chooser("/home", alloc::vec![
            (String::from("papers"), true),
            (String::from("notes.txt"), false),
        ]);

        // A directory row asks for its listing rather than answering.
        a.update(Msg::ChooserRow(0));
        assert_eq!(a.take_chooser_list().as_deref(), Some("/home/papers"));
        assert!(a.chooser().is_some(), "descending does not close the chooser");
        assert_eq!(a.take_open(), None, "and it is not an answer");

        a.show_chooser("/home/papers", alloc::vec![(String::from("draft.txt"), false)]);
        // A file row is the answer, in Open.
        a.update(Msg::ChooserRow(0));
        assert!(a.chooser().is_none(), "choosing closes it");
        assert_eq!(a.take_open().as_deref(), Some("/home/papers/draft.txt"));
    }

    /// A listing resets the selection, and an empty name is not an answer.
    #[test]
    fn the_chooser_refuses_the_answers_that_are_not_answers() {
        let mut a = app();
        a.update(Msg::OpenFile);
        a.show_chooser("/home", alloc::vec![(String::from("notes.txt"), false)]);
        // Nothing selected: accepting says so rather than choosing the first row.
        a.update(Msg::ChooserAccept);
        assert!(a.chooser().is_some(), "it stayed open");
        assert_eq!(a.take_open(), None);
        assert_eq!(a.status(), "nothing chosen");

        // Saving with an empty name is the same shape.
        let mut a = app();
        a.update(Msg::SaveAs);
        let c = a.chooser_mut().expect("a chooser is open");
        c.state.name = libui::widget::TextFieldState::with_text("   ");
        a.update(Msg::ChooserAccept);
        assert!(a.chooser().is_some(), "left open — the person is mid-answer");
        assert_eq!(a.take_save(), None, "and nothing was written");
        assert_eq!(a.status(), "a name, then Save");
    }

    /// The keyboard drives the chooser, and what a key *means* is decided here rather than by the
    /// toolkit.
    ///
    /// **A dialog holds the keyboard**, so a key this function declines does not fall through to
    /// the buffer behind it — it reaches nothing at all. That is why a character in `Open` is
    /// swallowed rather than ignored, and the assertion that says so is the buffer's text being
    /// unchanged.
    #[test]
    fn the_chooser_reads_the_keyboard_itself() {
        let ev = |code: u16, pressed: u16| KeyEvent::new(1, code, pressed, 0);
        let listing =
            || alloc::vec![(String::from("a.txt"), false), (String::from("b.txt"), false)];

        // Nothing to drive when nothing is being chosen — and *false*, so a caller can tell that
        // the key is still its own.
        let mut a = app();
        assert!(!a.chooser_key(ev(libkern::abi::KEY_ESC, 1)), "no chooser, no claim");

        // The arrows move the selection, and `up` from nothing does not wrap round the bottom.
        a.update(Msg::OpenFile);
        a.show_chooser("/home", listing());
        a.chooser_key(ev(libkern::abi::KEY_UP, 1));
        assert_eq!(a.chooser().unwrap().state.list.selected, None, "up from nothing selects none");
        a.chooser_key(ev(libkern::abi::KEY_DOWN, 1));
        a.chooser_key(ev(libkern::abi::KEY_DOWN, 1));
        assert_eq!(a.chooser().unwrap().state.list.selected, Some(1));
        // …and it stops at the end rather than running past the listing it was given.
        a.chooser_key(ev(libkern::abi::KEY_DOWN, 1));
        assert_eq!(a.chooser().unwrap().state.list.selected, Some(1), "clamped to the last row");

        // A release is not a keystroke. **The negative control for the whole test**: without it,
        // every assertion above would pass for a handler that acted on both transitions.
        assert!(!a.chooser_key(ev(libkern::abi::KEY_UP, 0)), "a release is declined");
        assert_eq!(a.chooser().unwrap().state.list.selected, Some(1), "and moved nothing");

        // A character in Open has nowhere to go, and is swallowed rather than reaching the buffer.
        let before = String::from(a.text());
        assert!(a.chooser_key(ev(KEY_X, 1)), "claimed even though it does nothing");
        assert_eq!(a.text(), before, "the buffer behind the dialog is untouched");

        // Enter answers with the selected row.
        a.chooser_key(ev(libkern::abi::KEY_ENTER, 1));
        assert!(a.chooser().is_none(), "Enter closed it");
        assert_eq!(a.take_open().as_deref(), Some("/home/b.txt"));

        // Esc closes without answering — and in Save, without writing.
        a.update(Msg::SaveAs);
        let _ = a.take_chooser_list();
        a.chooser_key(ev(libkern::abi::KEY_ESC, 1));
        assert!(a.chooser().is_none(), "Esc closed it");
        assert_eq!(a.take_save(), None, "and nothing was saved");

        // A character in Save reaches the field, which is the one mode that has one.
        a.update(Msg::SaveAs);
        let seeded = String::from(a.chooser().unwrap().state.name.text());
        a.chooser_key(ev(KEY_X, 1));
        let after = String::from(a.chooser().unwrap().state.name.text());
        assert_ne!(after, seeded, "the name field took the character: {after:?}");
    }
}
