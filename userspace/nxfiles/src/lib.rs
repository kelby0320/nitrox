//! `nxfiles` — the file browser, minus the syscalls.
//!
//! The split `nxterm` uses: what a listing *is* — sorted, marked, navigable — is a function of
//! values and host-tests in milliseconds; the window, the buffers and the event pump are the
//! binary's.
//!
//! ## What this half does not do
//!
//! **It does not read the filesystem.** [`App::show`] is handed entries that somebody else
//! fetched, which is what makes every rule below testable without a disk: sorting, marking,
//! what a row press means, and where "up" goes are decisions about a `Vec`, not about I/O. The
//! binary calls [`libfs::list_dir`] and hands the result over.
//!
//! **And it does not decide what a listing contains.** That the entries under a path are the
//! filesystem's *plus* the namespace bindings mounted there, with bindings shadowing, is
//! `libfs`'s rule and is shared with `list` — a browser that re-derived it would show a mount
//! point twice, once as the directory it covers.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use libdraw::geom::{Rect, Size};
use librsproto::file::{DIRENT_KIND_DIR, OwnedEntry};
use librsproto::surface::{
    KEY_DOWN, KEY_REPEAT, KeyEvent, MOD_CTRL, PointerEvent, RESIZE_BOTTOM, RESIZE_RIGHT,
    WINDOW_STATE_MAXIMIZED, WINDOW_STATE_MINIMIZED, WINDOW_STATE_NORMAL,
};
use alloc::vec;
use libui::menu::{Accel, Item, Menu, MenuState};
use libui::element::{
    Edge, Element, Insets, column, dock, docked, offset, padding, row, sized, stack, text,
    with_spacing,
};
use libui::widget::{
    DIALOG_GAP, GRIP_W, ListRow, ListState, TAB_STRIP_H, Theme as UiTheme, TITLE_BAR_H,
    TextFieldState, TitleButtons, WINDOW_FRAME_H, WidgetState, button, dialog_frame, list_view,
    popup_frame, resize_grip, tab_strip, text_field, title_bar, window_frame,
};

/// What this window is called, in its own title bar and in the shell's window list.
pub const TITLE: &str = "Files";

/// The path strip's height in pixels — one row of chrome under the title bar.
pub const PATH_H: u32 = 24;

/// A listing row's height in pixels.
pub const ROW_H: u32 = 20;

/// The element key on the listing, so a test can find it without walking the tree.
pub const LIST_KEY: u64 = 1;
/// The element key on the "up" control.
pub const UP_KEY: u64 = 2;
/// The element key on the title bar.
pub const TITLE_KEY: u64 = 3;
/// The element key on the resize grip.
pub const GRIP_KEY: u64 = 4;
/// The element key on the path text inside the strip.
pub const PATH_KEY: u64 = 6;
/// The element key on the notice beside the path.
///
/// **Always present, empty when there is nothing to say.** A child that appears and disappears
/// changes the strip's child count between frames, and the diff pairs children by key — an
/// element that is sometimes there is one the reconciler has to rebuild the row around.
pub const NOTICE_KEY: u64 = 7;

/// The element key on the path strip.
///
/// **Every docked child needs one**, not just the ones a test looks for: the diff requires a
/// container's children to be all keyed or all unkeyed, and a single bare sibling makes the
/// whole frame `MixedKeying` — **on the first frame**, because `reconcile_children` runs the
/// check against an empty previous list for a brand-new node too. In this application that
/// reaches `fail(b"nxfiles: the view is not diffable")` before anything is committed, so the
/// symptom is *no window ever appears*.
///
/// **Not to be confused with `KeyingChanged`**, which is the second-frame error: it needs a
/// successful first frame to compare against, and its symptom is the one that sounds like an
/// event-loop bug — a window that paints once and then never updates. Two errors, two symptoms;
/// an earlier version of this note merged them under the wrong name (PR #257 review,
/// blocking 2).
pub const STRIP_KEY: u64 = 5;

/// How far the pointer must travel with a button down before a press becomes a drag, in pixels.
///
/// **Because a click is a press that moved a little.** Nobody presses a mouse button without
/// nudging it, and a browser that started a drag on the first pixel would make opening a file by
/// clicking it a matter of luck. Four is the smallest number that survives a hand; a person
/// deliberately dragging crosses it immediately.
pub const DRAG_SLOP: i32 = 4;

/// The menu bar's height in pixels — one row of chrome above the path strip.
pub const MENU_BAR_H: u32 = 24;

/// Where the menu bar's words are keyed from: `MENU_BAR_KEY + i` for menu `i`.
///
/// [`libui::layout::locate`] turns each into the rectangle its popup hangs under.
pub const MENU_BAR_KEY: u64 = 10;

/// Where the open popup's rows are keyed from: `MENU_ROW_KEY + i` for item `i`.
///
/// **Keys are what make hover possible**: `Router::inside` reports the id of the keyed widget
/// under the pointer, so an unkeyed row is one the router cannot name (M11 Part E batch 3). A
/// separate range from the bar's, because a row and a word are different widgets.
pub const MENU_ROW_KEY: u64 = 100;
/// The element key on the name field that replaces the path while a prompt is open.
pub const PROMPT_KEY: u64 = 17;
/// The element key on the menu bar.
pub const BAR_KEY: u64 = 18;
/// The element key on the path strip's inner row — the half that is either the path or a prompt.
pub const STRIP_INNER_KEY: u64 = 19;
/// The element key on the tab strip.
pub const TAB_STRIP_KEY: u64 = 24;
/// Where pane keys start — see `nxedit::TAB_KEY_BASE`, whose reasoning is the same.
///
/// **The top bit matters more here than there.** This browser keys its list rows by *index*, and
/// a directory can hold any number of entries — so a base of a few thousand would put row `n` and
/// a tab on the same number for a large enough folder, and hovering one would highlight the other
/// (PR #270 review, optional 6). A row index cannot reach the high bit.
pub const TAB_KEY_BASE: u64 = 1 << 63;
/// The key that opens a tab: `t`. Pinned against the keymap by a test, as `nxedit`'s are.
pub const NEW_TAB_KEYCODE: u16 = 20;
/// The key that closes one: `w`.
pub const CLOSE_TAB_KEYCODE: u16 = 17;
/// The element key on the confirmation dialog's title bar.
pub const CONFIRM_TITLE_KEY: u64 = 20;
/// The element key on its question.
pub const CONFIRM_TEXT_KEY: u64 = 21;
/// The element key on its *delete* answer — the left button.
pub const CONFIRM_DELETE_KEY: u64 = 22;
/// The element key on its *keep* answer — the right button.
pub const CONFIRM_KEEP_KEY: u64 = 23;

/// Which menu is open, if either.
///
/// **Two, because the operations divide in two**: *File* makes and unmakes things, *Edit* acts
/// on what is selected. It is the division every file browser draws, and it is the reason `copy`
/// does not sit beside `delete`.
///
/// **They are indices into [`App::menu_table`] since M14 Part A**, not an enum. The enum existed to
/// give each menu a key and a popup tree; both are the toolkit's now, and a `Menu` that only
/// said "File or Edit" was a second spelling of `0` and `1`.
pub const MENU_COUNT: usize = 2;

/// What a menu row asks for.
///
/// **Cut and paste are absent, and the reason is the clipboard.** They are a *pair*, and a pair
/// that holds something between two gestures is a clipboard however it is spelled — so building
/// a private one-slot path buffer here would be a second clipboard shipped weeks before the real
/// one. M12 decision 1 makes the clipboard a resource server precisely so that what you last
/// copied is not readable by everything running, and Part E's own words leave the door open for
/// this: "the type tag exists so a later image or a typed stream is a second kind rather than a
/// second clipboard". A file path is that second kind. **Trigger: Part E's ring exists** —
/// `TODO(file-clipboard)`.
///
/// Nothing is lost meanwhile: moving a file into a folder is a drag, which is the gesture people
/// reach for first anyway.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Make an empty file, named by a prompt.
    NewFile,
    /// Make a directory, named by a prompt.
    NewFolder,
    /// Give the selected entry a new name, from a prompt.
    Rename,
    /// Duplicate the selected entry under a name from a prompt.
    Copy,
    /// Remove the selected entry, after a confirmation.
    Delete,
}

impl Action {
    /// What the prompt says before the field, or `None` for the one that asks a question
    /// instead.
    pub fn prompt(self) -> Option<&'static str> {
        match self {
            Action::NewFile => Some("new file:"),
            Action::NewFolder => Some("new folder:"),
            Action::Rename => Some("rename to:"),
            Action::Copy => Some("copy to:"),
            Action::Delete => None,
        }
    }

    /// Whether this acts on the selected entry rather than on the directory.
    pub fn needs_selection(self) -> bool {
        matches!(self, Action::Rename | Action::Copy | Action::Delete)
    }
}

/// What an operation was chosen **for**, resolved at the moment it was chosen.
///
/// **The fix for a whole class of bug** (PR #268 review, blocking 2 and 3). Every operation here
/// is two gestures — choose it from a menu, then answer a prompt or a question — and the first
/// version composed the paths at the *second*, out of `self.path` and `self.list.selected`. Both
/// move while a prompt or a dialog is up: the compositor has no input-exclusive window
/// (`TODO(dialog-modality)`), the prompt is a *keyboard* mode only, and a click on a row still
/// navigates or re-selects. So a delete answered after walking into another directory removed a
/// file there with the same name — one the person was never asked about, while the dialog's own
/// text still named the one they chose.
///
/// Resolving here makes the operation mean what it meant when it was asked for, whatever the
/// window does afterwards.
///
/// **It overlaps with `show` dropping the prompt and the question**, and only partly. A listing
/// is the only thing that changes `self.path`, so for *delete* either fix alone would do; the
/// *selection* moves without one — a click on a file row re-selects and navigates nothing — and
/// there this is the only thing standing between a rename and the wrong file.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Target {
    /// The directory the operation happens in, as it was when the row was pressed.
    dir: String,
    /// The full path of the entry it acts on — `None` for the two that act on the directory.
    from: Option<String>,
    /// That entry's name, which is what the question shows.
    name: String,
    /// Whether it is a directory, which decides whether a removal descends.
    is_dir: bool,
}

/// A filesystem operation the binary owes, and the browser cannot perform.
///
/// **The outbox shape everything else here uses**: `update` is a function of values and a
/// filesystem mutation is a syscall, so the application says what it wants done and the `main`
/// that owns the namespace does it. `nxfiles` performs these *itself* rather than asking the
/// shell — it holds `/home`, which is the authority these need, and routing them through a
/// supervisor would be asking it to do what the application is already entitled to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FileOp {
    /// Create an empty file or a directory at `path`.
    Create {
        /// Where.
        path: String,
        /// A directory rather than a file.
        dir: bool,
    },
    /// Give `from` the name `to`, in the same directory.
    Rename {
        /// The entry as it is.
        from: String,
        /// What it should be called.
        to: String,
    },
    /// Duplicate `from` as `to`.
    Copy {
        /// The entry to read.
        from: String,
        /// The path to write.
        to: String,
        /// Whether it is a directory, which decides whether the copy descends.
        ///
        /// **`copy_file` on a folder just fails**, and reported "could not copy it" for a
        /// perfectly ordinary request (PR #268 review, optional 3). `libfs::copy_tree` was
        /// already there.
        dir: bool,
    },
    /// Remove `path`, and everything under it if it is a directory.
    Delete {
        /// What to remove.
        path: String,
        /// Whether it is a directory, which decides whether the removal descends.
        dir: bool,
    },
    /// Move `from` into a directory, arriving at `to`.
    ///
    /// **A rename, not a copy-and-delete**, because within one filesystem that is what a move
    /// is — and it is the one that cannot half-happen.
    MoveInto {
        /// The entry being dragged.
        from: String,
        /// Where it lands, directory and name.
        to: String,
    },
}

/// The window's size in pixels at startup, before any manager places it.
pub const START_SIZE: Size = Size::new(560, 420);

/// One thing in a directory, as the browser thinks of it.
///
/// **Not [`OwnedEntry`]**, which is the wire's shape: a row is a name plus the one bit this
/// application acts on — whether pressing it descends or opens. Keeping the browser's own type
/// is what lets `update` and `view` be tested with three lines of setup instead of a filesystem.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    /// The entry's name, with no path in it.
    pub name: String,
    /// Whether pressing this row descends into it.
    pub is_dir: bool,
}

impl Entry {
    /// A directory entry.
    pub fn dir(name: &str) -> Self {
        Self { name: String::from(name), is_dir: true }
    }

    /// A file entry.
    pub fn file(name: &str) -> Self {
        Self { name: String::from(name), is_dir: false }
    }

    /// What the row shows: directories carry a trailing separator, which is the whole of the
    /// marking.
    ///
    /// **A character rather than a colour or an icon**, because the toolkit has no icons and a
    /// colour would be the theme's to change (M11). A trailing `/` is what every terminal
    /// listing in this system already uses, so it is the marking a person has seen before.
    pub fn label(&self) -> String {
        let mut s = self.name.clone();
        if self.is_dir {
            s.push('/');
        }
        s
    }
}

/// Everything the browser is.
/// One tab: a directory, what is in it, and where the person is in that list.
///
/// **Split out in M12 Part D**, the same line `nxedit` draws between a buffer and its window.
/// What stays on [`App`] is what a *window* has; what moves here is what a person expects to
/// survive switching tabs — including the scroll offset, because a tab that jumped back to the
/// top when you came back to it would lose your place every time you looked at another folder.
pub struct Pane {
    /// Identity across frames and across the strip. Not the index: closing a tab renumbers the
    /// rest.
    key: u64,
    /// Where the listing came from, absolute and with no trailing separator except at the root.
    path: String,
    /// What is in it, sorted.
    entries: Vec<Entry>,
    /// Which row is selected and how far the list is scrolled.
    list: ListState,
}

/// Everything the browser is.
pub struct App {
    /// The open panes, in the order their tabs are drawn. **Never empty**, for the reason
    /// `nxedit`'s buffers are not: closing the last tab closes the window.
    panes: Vec<Pane>,
    /// Which pane's tab is current, by [`Pane::key`].
    current: u64,
    /// The next key to hand out — monotonic, so a stale message can never name a pane that has
    /// taken its place. Numbered from [`TAB_KEY_BASE`] so a tab's key cannot collide with the
    /// chrome's element keys *or with a list row's index*, which `Router::hovered_key` reports in
    /// one namespace.
    next_key: u64,
    /// The window's size in pixels — what the client commits.
    window: Size,
    /// Whether this window holds the keyboard, which the title bar shows.
    ///
    /// Starts `true`, matching `libui::Router`'s own `window_focused`: starting `false` makes a
    /// client's first paint dim.
    pub focused: bool,
    /// This window last asked to be maximised, so its maximise button now asks for normal.
    maximized: bool,
    /// A path the binary owes a listing for — set by anything that navigates.
    ///
    /// **An outbox rather than a call**, the shape every application in this tree uses: `update`
    /// is a function of values and reading a directory is a syscall. The application says where
    /// it wants to be; the `main` that owns the namespace performs it.
    goto: Option<String>,
    /// What the strip says beside the path, or `None` — an answer to the last row activated.
    notice: Option<String>,
    /// The row a press landed on, and where the pointer was, until the button comes up.
    ///
    /// **A press is not yet a drag.** A person pressing a row to select or open it moves the
    /// pointer a pixel or two doing it, so the gesture only becomes a drag once it has travelled
    /// [`DRAG_SLOP`] — which is what keeps a click a click.
    ///
    /// **The entry's *name*, not its index**, and that is not fussiness: anything that replaces
    /// the listing between the press and the first motion past the slop makes those two
    /// different rows. It is reachable — hold the button on a row, press Backspace (the press
    /// just raised this window, so it has the keyboard), then move: the drag would carry the
    /// *parent* listing's row at that index. `list_view`'s rows are keyed precisely so an index
    /// does not have to survive a rebuild (PR #260 review, optional 4).
    pressed: Option<(String, i32, i32)>,
    /// A drag the binary owes the compositor a `StartDrag` for: the entry it carries.
    ///
    /// The same outbox shape as everything else here: `update` is a function of values, and
    /// telling the compositor is IPC.
    drag: Option<Entry>,
    /// A path the binary owes an `Open` for — set by activating a row that is not a directory.
    ///
    /// The same outbox shape as [`goto`](Self::goto), for the same reason: asking the shell to
    /// open something is IPC.
    open: Option<String>,
    /// A title-bar button was pressed, and the binary owes the compositor a `RequestState`.
    state_requested: Option<u32>,
    /// The title bar was dragged, and the binary owes the compositor a `StartMove`.
    move_requested: bool,
    /// The grip was pressed, and the binary owes the compositor a `StartResize`.
    resize_requested: Option<u32>,
    /// The browser has been asked to close, and the binary owes an exit.
    ///
    /// A flag rather than an `exit` here: `update` is a function of values and has no way to
    /// tear down a session.
    closing: bool,
    /// Which menu is open, where each bar word sits, and where the keyboard is inside it.
    ///
    /// The window it drops into is the binary's, opened and destroyed with this — the same
    /// arrangement `nxterm`'s menu has had since M6 Part C3, and a [`libui::window::Child`]
    /// since M12 Part A.
    ///
    /// **The toolkit's, since M14 Part A.** This was an `Option<Menu>` and a two-element array
    /// here, a `bool` and a `Rect` in `nxterm`, and the same open/close/anchor/dismiss logic in
    /// both — plus, in neither, the arrow keys a menu is supposed to have.
    pub menus: MenuState,
    /// A name being typed, and what it is for.
    ///
    /// **`Some` is a mode**, exactly as `nxedit`'s naming field is: while it is open the keys
    /// belong to the field rather than to the list, and the strip shows it in place of the path.
    /// Three of the five operations need a name and this is all three of them — one prompt, not
    /// three dialogs.
    prompt: Option<(Action, Target, TextFieldState)>,
    /// What a delete is asking about, and nothing while nothing is being asked.
    ///
    /// **A question rather than a prompt**, because removal is the one operation a person cannot
    /// undo and a typed name is not a confirmation of anything. The binary turns this into a
    /// `Role::Dialog` window, which is what M12 Part A built.
    confirm: Option<Target>,
    /// Whether the *dialog* holds the keyboard, which its own title bar shows.
    pub confirm_focused: bool,
    /// The dialog's title bar was dragged, and the binary owes a `StartMove` **on its window**.
    confirm_move_requested: bool,
    /// A filesystem operation the binary owes.
    op: Option<FileOp>,
    /// The row an internal drag is over, while one is running.
    ///
    /// **Internal**: a drag that has passed the slop but has not left this window. The payload
    /// never reaches the compositor while that is true, which is the whole of what "drag and
    /// drop within a window" means (M10 named it as M12's).
    over: Option<usize>,
    /// Whether an internal drag is running: past the slop, and not yet out of this window.
    dragging: bool,
}

/// What a pointer record did to a gesture in progress.
///
/// **Returned rather than logged or acted on**, because two of the four need a syscall the
/// application cannot make: handing a drag to the compositor is IPC, and a repaint is the
/// binary's loop. The same outbox discipline as everything else here, in the one place where
/// `update` is not what produced the change.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Gesture {
    /// Nothing that anything outside needs to do.
    None,
    /// A drag has left this window: the binary owes the compositor a `StartDrag`.
    HandOff,
    /// An internal drag ended over a directory row, and [`App::take_op`] may now have a move.
    Dropped,
    /// The row an internal drag is over has changed, so the frame is stale.
    Moved,
}

/// What can happen to the browser.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Msg {
    /// A listing row was activated — its index into [`App::entries`].
    Activate(u64),
    /// A press landed on a row: the gesture that *may* become a drag (M10 Part E).
    Grab(u64),
    /// The "up" control was pressed.
    Up,
    /// The scrollbar is being dragged — see [`ListState::drag_to`].
    Scroll(PointerEvent),
    /// A key reached the window.
    Key(KeyEvent),
    /// The title bar was dragged.
    DragWindow,
    /// The resize grip was pressed, for these edges.
    ResizeWindow(u32),
    /// A title-bar button asking the manager for a window state.
    RequestState(u32),
    /// Somebody wants this window gone — its own close button, or the shell asking.
    ///
    /// **One message for both**, as `nxterm` has: which of them it was is not something this
    /// application acts on differently.
    Close,
    /// A word on the menu bar was pressed: open that menu, or close it if it was already open.
    MenuBar(usize),
    /// A menu row was chosen.
    Choose(Action),
    /// The delete dialog's *delete* answer.
    ConfirmDelete,
    /// The delete dialog's *keep* answer, its close button, and `Esc`.
    KeepIt,
    /// The delete dialog's title bar was dragged.
    DragConfirm,
    /// A tab was pressed — make it current.
    SelectTab(u64),
    /// A tab's close box was pressed, or `Ctrl+W`.
    CloseTab(u64),
    /// `Ctrl+T`: a second view of where you are.
    NewTab,
}

impl App {
    /// A browser showing nothing, at `path`.
    pub fn new(path: &str) -> App {
        App {
            panes: alloc::vec![Pane {
                key: TAB_KEY_BASE,
                path: String::from(path),
                entries: Vec::new(),
                list: ListState::default(),
            }],
            current: TAB_KEY_BASE,
            next_key: TAB_KEY_BASE + 1,
            window: START_SIZE,
            focused: true,
            maximized: false,
            goto: None,
            notice: None,
            pressed: None,
            drag: None,
            open: None,
            state_requested: None,
            move_requested: false,
            resize_requested: None,
            closing: false,
            menus: MenuState::new(MENU_COUNT),
            prompt: None,
            confirm: None,
            confirm_focused: true,
            confirm_move_requested: false,
            op: None,
            over: None,
            dragging: false,
        }
    }

    /// The pane whose tab is current. Never `None` — the last tab closing closes the window.
    fn pane(&self) -> &Pane {
        self.panes.iter().find(|p| p.key == self.current).unwrap_or(&self.panes[0])
    }

    /// The current pane, mutably. See [`pane`](Self::pane).
    fn pane_mut(&mut self) -> &mut Pane {
        let key = self.current;
        let i = self.panes.iter().position(|p| p.key == key).unwrap_or(0);
        &mut self.panes[i]
    }

    /// The tabs, for the strip that draws them: key and label.
    pub fn tabs(&self) -> Vec<(u64, String)> {
        self.panes
            .iter()
            .map(|p| {
                let name = libfs::basename_str(&p.path);
                let label = if name.is_empty() { "/" } else { name };
                (p.key, String::from(label))
            })
            .collect()
    }

    /// Which tab is current.
    pub fn current_tab(&self) -> u64 {
        self.current
    }

    /// How many panes are open.
    pub fn tab_count(&self) -> usize {
        self.panes.len()
    }

    /// Remove the tab keyed `k`, closing the window if it was the last.
    fn drop_tab(&mut self, k: u64) {
        if self.panes.len() <= 1 {
            self.closing = true;
            return;
        }
        let Some(i) = self.panes.iter().position(|p| p.key == k) else { return };
        self.panes.remove(i);
        if self.current == k {
            self.current = self.panes[i.saturating_sub(1).min(self.panes.len() - 1)].key;
        }
    }

    /// Where the browser is.
    pub fn path(&self) -> &str {
        &self.pane().path
    }

    /// What it is showing.
    pub fn entries(&self) -> &[Entry] {
        &self.pane().entries
    }

    /// Replace the listing: `path` is now where we are, and `entries` is what is in it.
    ///
    /// **Sorted here rather than by the caller**, because the order is a fact about the browser
    /// and not about the filesystem: directories before files, then by name. A listing arrives
    /// in whatever order the directory server and the namespace enumeration produced, which is
    /// not an order anybody chose.
    ///
    /// The selection resets to the first row: a listing of a *different* directory has no row
    /// the old selection refers to, and `list_view` clamping a stale index would silently
    /// select whatever happens to be at that position.
    pub fn show(&mut self, path: &str, mut entries: Vec<Entry>) {
        entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
        let p = self.pane_mut();
        p.path = String::from(path);
        p.entries = entries;
        p.list = ListState { selected: (!p.entries.is_empty()).then_some(0), offset: 0 };
        // A listing supersedes whatever the last row press had to say about itself.
        self.notice = None;
        // **And a question about a directory you have left is not a question worth keeping**
        // (PR #268 review, blocking 2 and 3). Resolving the target at `choose` time is what
        // makes the *operation* correct; this is what stops the browser sitting in a mode whose
        // subject is no longer on screen — a prompt replaces the path in the strip, so after a
        // navigation there would be nothing at all saying where the name is about to land.
        //
        // Both, deliberately: neither closes the other's case. A click on a *file* row re-selects
        // without re-listing, which this never sees.
        self.prompt = None;
        self.confirm = None;
    }

    /// Turn a wire entry into a row.
    ///
    /// `.` and `..` are already filtered by [`libfs::list_dir`]; the only thing dropped here is
    /// an entry with an **empty name**, which is not a thing a person can act on.
    ///
    /// **An unknown kind is listed as a file, deliberately.** `DIRENT_KIND_UNKNOWN` and
    /// `DIRENT_KIND_SYMLINK` both land here, and a row that is not a directory is inert — it
    /// shows a name and does nothing when pressed. Hiding it would be worse: something is on
    /// disk and the browser would be the one place that does not say so. (An earlier version of
    /// this doc claimed the filter existed; the code never had it — PR #257 review, finding 5.)
    pub fn entry_of(e: &OwnedEntry) -> Option<Entry> {
        let name = String::from_utf8_lossy(e.name()).into_owned();
        if name.is_empty() {
            return None;
        }
        Some(Entry { name, is_dir: e.kind == DIRENT_KIND_DIR })
    }

    /// The path a row press leads to, or `None` for a row that is not a directory.
    fn child(&self, i: usize) -> Option<String> {
        let e = self.pane().entries.get(i)?;
        e.is_dir.then(|| join(&self.pane().path, &e.name))
    }

    /// The path a row *names*, whether or not it is a directory.
    fn full(&self, i: usize) -> Option<String> {
        self.pane().entries.get(i).map(|e| join(&self.pane().path, &e.name))
    }

    /// Apply a message.
    pub fn update(&mut self, msg: Msg) {
        // **Choosing dismisses the menu, whichever row it was** — a menu that stayed open would
        // cover the thing it just acted on. Asked of the table rather than repeated in the arms,
        // so a row added later cannot forget: `choose` used to clear it, and New Tab and Close
        // Tab arriving from the bar in M14 Part A do not go through `choose`. A message that is
        // not a row leaves it alone, which is what keeps `MenuBar` able to open one.
        if self
            .menu_table()
            .iter()
            .flat_map(|m| m.items.iter())
            .any(|it| matches!(it, Item::Action { msg: m, .. } if *m == msg))
        {
            self.menus.close();
        }
        match msg {
            Msg::Activate(i) => {
                let i = i as usize;
                self.pane_mut().list.selected = Some(i);
                // **A directory navigates; anything else is opened**, which since M10 Part D
                // means asking the shell — `Desktop::Open` — rather than launching anything
                // here. A browser holds no authority to spawn a program and should not: it has
                // no `/bin` and no way to build a namespace for one. It names a path; the shell
                // decides what opens it.
                //
                // **Including a row whose kind is unknown.** `entry_of` lists those as files,
                // and asking to open one is the same honest answer a directory listing gives:
                // something is there, and whatever opens it will say what it found.
                match self.child(i) {
                    Some(to) => self.goto = Some(to),
                    None => self.open = self.full(i),
                }
            }
            // **Remembered, not acted on.** What a press becomes is decided by what happens
            // next: a release makes it a click, and enough movement makes it a drag.
            Msg::Grab(i) => {
                // Resolved here, while the listing this index came from is still the listing.
                self.pressed =
                    self.pane().entries.get(i as usize).map(|e| (e.name.clone(), 0, 0));
            }
            // **The drag converts through the widget's own arithmetic** — `ListState::drag_to`,
            // the same `ScrollState::offset_at` `nxterm` uses for its grid — so a list and a
            // terminal cannot disagree about where a thumb points (M11 Part E batch 6).
            Msg::Scroll(p) => {
                if p.buttons != 0 {
                    let (h, total) = (self.list_h(), self.pane().entries.len());
                    self.pane_mut().list.drag_to(h, ROW_H, total, p.y);
                }
            }
            Msg::Up => {
                let up = parent(&self.pane().path);
                // The root is its own parent, so this is a no-op there rather than an error —
                // the same rule `libfs::parent` states, and the reason the control is never
                // disabled: a control that greys out at the root is one more state to draw.
                if up != self.pane().path {
                    self.goto = Some(up);
                }
            }
            Msg::Key(k) => self.key(k),
            Msg::DragWindow => self.move_requested = true,
            Msg::DragConfirm => self.confirm_move_requested = true,
            Msg::ResizeWindow(edges) => self.resize_requested = Some(edges),
            Msg::Close => self.closing = true,
            // **Toggling, so the same press that opened it closes it.** A bar item that only
            // opened would leave the menu up until something else dismissed it, and the
            // dismissal a popup gets is a press on *another* window.
            Msg::MenuBar(i) => self.menus.toggle(i),
            Msg::Choose(a) => self.choose(a),
            Msg::SelectTab(k) => {
                if self.panes.iter().any(|p| p.key == k) {
                    self.current = k;
                }
            }
            // **A new tab opens where you are**, not at `HOME`. Opening a second view of the
            // same folder is what a person reaches for when they are about to move something
            // out of it, and getting back to where they were is the work the tab exists to save.
            Msg::NewTab => {
                let key = self.next_key;
                self.next_key += 1;
                let here = self.pane().path.clone();
                self.panes.push(Pane {
                    key,
                    path: here.clone(),
                    entries: Vec::new(),
                    list: ListState::default(),
                });
                self.current = key;
                // The listing is a syscall, so the new pane starts empty and asks for one.
                self.goto = Some(here);
            }
            // **A browser's tab has nothing to lose**, so there is no question to ask: a listing
            // is a view of the filesystem rather than unsaved work.
            Msg::CloseTab(k) => self.drop_tab(k),
            // **The path was resolved when the question was asked**, not composed from
            // `self.path` now: the parent window is free to navigate while a dialog is up, and
            // this is the one operation a person cannot undo (PR #268 review, blocking 2).
            Msg::ConfirmDelete => {
                if let Some(t) = self.confirm.take()
                    && let Some(path) = t.from
                {
                    self.op = Some(FileOp::Delete { path, dir: t.is_dir });
                }
            }
            // **Says so**, because a dialog that vanishes with nothing changed is
            // indistinguishable from one that took the other answer.
            Msg::KeepIt => {
                if self.confirm.take().is_some() {
                    self.notice = Some(String::from("not deleted"));
                }
            }
            Msg::RequestState(s) => {
                if s == WINDOW_STATE_MAXIMIZED || s == WINDOW_STATE_NORMAL {
                    self.maximized = s == WINDOW_STATE_MAXIMIZED;
                }
                self.state_requested = Some(s);
            }
        }
    }

    /// Act on a menu row.
    ///
    /// **An operation with nothing selected is answered rather than ignored.** Three of the five
    /// act on a row, and a menu item that silently does nothing when none is chosen is a control
    /// that looks live and is not — the defect M8's overview shipped three of. The strip says so
    /// instead.
    ///
    /// **Since M14 Part A the row is also greyed**, so this branch is now the second guard
    /// rather than the only one — and it stays, because a caller reaching `choose` some other
    /// way must get the same answer as one reaching it from a menu.
    fn choose(&mut self, a: Action) {
        let selected =
            self.pane().list.selected.and_then(|i| self.pane().entries.get(i)).cloned();
        if a.needs_selection() && selected.is_none() {
            self.notice = Some(String::from("nothing is selected"));
            return;
        }
        // **Resolved now, not when the answer arrives.** See [`Target`]: `self.path` and the
        // selection both move while a prompt or a question is up, and an operation that read them
        // late acted on something nobody chose.
        let target = Target {
            dir: self.pane().path.clone(),
            from: selected.as_ref().map(|e| join(&self.pane().path, &e.name)),
            name: selected.as_ref().map(|e| e.name.clone()).unwrap_or_default(),
            is_dir: selected.as_ref().is_some_and(|e| e.is_dir),
        };
        match a {
            Action::Delete => self.confirm = Some(target),
            // The field starts empty rather than pre-filled with the current name. Pre-filling
            // would need a selection and a caret to be useful — a person would have to clear it
            // before typing — and the field has neither yet.
            _ => self.prompt = Some((a, target, TextFieldState::new())),
        }
    }

    /// Finish the open prompt, turning what was typed into an operation.
    ///
    /// **A name, not a path.** Everything these five do happens in the directory being shown, so
    /// a separator in the field would be a way to write outside it by typing — and this browser
    /// is showing one directory, not offering a command line.
    fn confirm_prompt(&mut self) {
        let Some((a, target, field)) = self.prompt.as_ref() else { return };
        let (a, name) = (*a, field.text().trim().to_string());
        if name.is_empty() {
            self.notice = Some(String::from("a name, then Enter"));
            return;
        }
        if name.contains('/') {
            self.notice = Some(String::from("a name, not a path"));
            return;
        }
        // **`target.dir`, not `self.path`**, and `target.from`, not the selection: both were
        // resolved when the menu row was pressed. Reading them here renamed whatever happened to
        // sit at row 0 after a navigation the prompt gave no sign of (PR #268 review, blocking 3).
        let to = join(&target.dir, &name);
        let from = target.from.clone();
        self.op = match a {
            Action::NewFile => Some(FileOp::Create { path: to, dir: false }),
            Action::NewFolder => Some(FileOp::Create { path: to, dir: true }),
            Action::Rename => from.map(|from| FileOp::Rename { from, to }),
            Action::Copy => from.map(|from| FileOp::Copy { from, to, dir: target.is_dir }),
            // `choose` sends a delete to the dialog and never to a prompt.
            Action::Delete => None,
        };
        self.prompt = None;
    }

    /// Move `carried` into the directory at row `i`.
    ///
    /// **Only onto a directory, and never onto itself.** A file row is not a destination, and a
    /// directory dropped into itself is a rename to a path beneath the thing being moved — which
    /// the filesystem would refuse and which nothing sensible could mean.
    fn drop_on(&mut self, i: usize, carried: &str) {
        // **`is_dir` is checked again here and nothing reaches it** — `pointer_moved` only ever
        // records a directory as the target, and this is its one caller. Kept as belt and
        // braces, and named as such rather than left looking load-bearing: breaking it alone
        // fails no test (PR #268 review, optional 1).
        let Some(target) = self.pane().entries.get(i).filter(|e| e.is_dir).cloned() else {
            return;
        };
        if target.name == carried {
            return;
        }
        let from = join(&self.pane().path, carried);
        let dir = join(&self.pane().path, &target.name);
        self.op = Some(FileOp::MoveInto { from, to: join(&dir, carried) });
    }

    /// Where the list's first row starts, in window coordinates.
    ///
    /// **One place, because two would disagree.** [`list_h`](Self::list_h) subtracts the chrome
    /// above the list and an internal drag has to turn a `y` back into a row; deriving that
    /// arithmetic twice is how a drop lands one row off the thing it was released over.
    pub fn list_top(&self) -> u32 {
        libui::widget::WINDOW_CONTENT_Y + TITLE_BAR_H + MENU_BAR_H + TAB_STRIP_H + PATH_H
    }

    /// How many rows the list actually draws — what `list_view` builds from the height it is
    /// given.
    ///
    /// **Not `entries.len()`**, which is the whole of PR #268's blocking 1: a window is taller
    /// than the rows it has room for by up to a row, plus the grip and the frame, and a `y` in
    /// that band is still inside the window and still under the pointer grab. Bounding a drop
    /// against the *entries* rather than the *rows* mapped it to a directory that was never
    /// drawn and never highlighted — so a file moved into a folder the person could not see.
    pub fn visible_rows(&self) -> usize {
        (self.list_h() / ROW_H) as usize
    }

    /// The row a window-local `y` is over, if it is over a **drawn** row at all.
    fn row_at(&self, y: i32) -> Option<usize> {
        let top = self.list_top() as i32;
        if y < top || y >= top + (self.visible_rows() as u32 * ROW_H) as i32 {
            return None;
        }
        let i = self.pane().list.offset + ((y - top) as u32 / ROW_H) as usize;
        (i < self.pane().entries.len()).then_some(i)
    }

    /// Whether a window-local point is inside this window at all.
    fn inside(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && (x as u32) < self.window.w && (y as u32) < self.window.h
    }

    /// Arrow keys move the selection; Enter activates it; Backspace goes up.
    ///
    /// **The keyboard reaches the same three messages the pointer does**, rather than a second
    /// set of rules: a browser where Enter and a row press disagree about what "open" means is
    /// two browsers.
    fn key(&mut self, k: KeyEvent) {
        if k.pressed != KEY_DOWN && k.pressed != KEY_REPEAT {
            return;
        }
        // The tab chords, which are the keyboard's half of the strip. **Before the prompt
        // check**, deliberately: without it the chord would not open a tab at all while a name
        // is being typed, because the field's branch returns first.
        //
        // It would *not* type a `t` into the name, which is what this comment used to claim —
        // `libinput`'s keymap folds Ctrl+letter into the C0 range and `TextFieldState::apply`
        // drops anything below `0x20`, with the comment "Control characters are not text". The
        // ordering is right for the other half (PR #270 review, worth fixing 5).
        //
        // **Matched against the menu table rather than a `match` on keycodes** (M14 decision 2).
        // The keycode constants are still the source of truth — the table names them — but there
        // is now one statement of "Ctrl+W closes a tab" rather than a label in the menu and a
        // branch here that could stop agreeing with it. The `Ctrl` guard stays around it: every
        // other chord is swallowed rather than folded into a printable character.
        if k.modifiers & MOD_CTRL != 0 {
            if let Some(msg) = libui::menu::accel_match(&self.menu_table(), &k) {
                self.update(msg);
            }
            return;
        }
        // **While a name is being typed the keys are the field's**, arrows and Backspace
        // included — the same rule `nxedit`'s naming field follows, and for the same reason: a
        // Backspace that went up a directory while somebody was correcting a typo would be one
        // key doing two things.
        if self.prompt.is_some() {
            match k.keycode {
                libkern::abi::KEY_ESC => {
                    self.prompt = None;
                    self.notice = Some(String::from("cancelled"));
                }
                libkern::abi::KEY_ENTER => self.confirm_prompt(),
                code => {
                    if let Some((_, _, f)) = self.prompt.as_mut() {
                        f.apply(code, k.modifiers);
                    }
                }
            }
            return;
        }
        let len = self.pane().entries.len();
        match k.keycode {
            libkern::abi::KEY_DOWN => {
                self.pane_mut().list.down(len);
            }
            libkern::abi::KEY_UP => {
                self.pane_mut().list.up();
            }
            libkern::abi::KEY_ENTER => {
                if let Some(i) = self.pane().list.selected {
                    self.update(Msg::Activate(i as u64));
                }
            }
            libkern::abi::KEY_BACKSPACE => self.update(Msg::Up),
            _ => {}
        }
    }

    /// The path the binary owes a listing for, if anything navigated. Clears the record.
    pub fn take_goto(&mut self) -> Option<String> {
        self.goto.take()
    }

    /// Note where the pointer is while a button is held, and say whether a drag begins now.
    ///
    /// **Called with window-local coordinates from the pointer record**, because the toolkit
    /// routes *messages* and this needs a distance. The first motion after the press fixes the
    /// origin — the press itself carries no position through `on_press_down`, and asking the
    /// router for one would be asking it to remember a pixel it has no reason to keep.
    pub fn pointer_moved(&mut self, x: i32, y: i32, buttons: u16) -> Gesture {
        if buttons == 0 {
            // **The button came up, which is the only thing that completes an internal drag.**
            // A release over a *different* row produces no `Msg` at all — the toolkit fires a
            // click only when the release lands on the widget the press captured — so there is
            // nothing to route and the gesture is read from the record instead.
            let landed = self.dragging.then_some(self.over).flatten();
            let carried = self.pressed.as_ref().map(|(n, _, _)| n.clone());
            self.pressed = None;
            self.dragging = false;
            self.over = None;
            if let (Some(i), Some(name)) = (landed, carried) {
                self.drop_on(i, &name);
                return Gesture::Dropped;
            }
            return Gesture::None;
        }
        // **An internal drag stays this window's until the pointer leaves it.** That is the
        // whole of "drag and drop within a window": the payload never reaches the compositor,
        // which could not deliver it back here anyway — it skips the source window when it looks
        // for a drop target, deliberately, because dropping a thing where it came from is a
        // no-op (`compositor::input::highlight_target`).
        if self.dragging {
            if !self.inside(x, y) {
                // **Handed over at the edge, not at the slop.** The compositor runs the drag
                // from here — it draws the outline and delivers to whatever accepts the payload
                // — and the client goes blind for the rest of the gesture, which is why the
                // handoff has to be the *last* thing this window decides.
                let name = self.pressed.take().map(|(n, _, _)| n);
                self.dragging = false;
                self.over = None;
                self.drag =
                    name.and_then(|n| self.pane().entries.iter().find(|e| e.name == n).cloned());
                return if self.drag.is_some() { Gesture::HandOff } else { Gesture::None };
            }
            let was = self.over;
            // **Only a directory is a target**, so the highlight never offers a landing place
            // that would do nothing — the same rule the compositor follows for windows that do
            // not take the payload.
            self.over =
                self.row_at(y).filter(|&i| self.pane().entries.get(i).is_some_and(|e| e.is_dir));
            return if was == self.over { Gesture::None } else { Gesture::Moved };
        }
        let Some((name, ox, oy)) = self.pressed.clone() else { return Gesture::None };
        if (ox, oy) == (0, 0) {
            self.pressed = Some((name, x, y));
            return Gesture::None;
        }
        if (x - ox).abs() < DRAG_SLOP && (y - oy).abs() < DRAG_SLOP {
            return Gesture::None;
        }
        // The gesture is a drag now — an *internal* one, which it stays until it leaves.
        // **Checked against the current listing here rather than at the drop**, so a row that
        // has gone carries nothing rather than carrying whatever now sits at that position.
        if !self.pane().entries.iter().any(|e| e.name == name) {
            self.pressed = None;
            return Gesture::None;
        }
        self.dragging = true;
        self.over =
            self.row_at(y).filter(|&i| self.pane().entries.get(i).is_some_and(|e| e.is_dir));
        Gesture::Moved
    }

    /// The entry the binary owes a `StartDrag` for. Clears the record.
    pub fn take_drag(&mut self) -> Option<(Entry, String)> {
        let e = self.drag.take()?;
        let path = join(&self.pane().path, &e.name);
        Some((e, path))
    }

    /// The path the binary owes an `Open` for, if a file was activated. Clears the record.
    pub fn take_open(&mut self) -> Option<String> {
        self.open.take()
    }

    /// Say what happened to the last [`take_open`](Self::take_open) — shown in the path strip.
    ///
    /// **Because a row press that does nothing visible is indistinguishable from a broken
    /// one.** The shell answers before the program it launched has drawn anything, so the only
    /// thing the browser can report is whether the *request* was taken — which is worth
    /// reporting, since the failure it names (a shell that will not launch) is otherwise silent
    /// on this side.
    pub fn opened(&mut self, path: &str, ok: bool) {
        self.notice = Some(if ok {
            alloc::format!("opening {}", libfs::basename_str(path))
        } else {
            alloc::format!("could not open {}", libfs::basename_str(path))
        });
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

    /// The bar's menus, in bar order.
    ///
    /// **Built rather than stored**, and it takes `&self` because half the rows depend on state:
    /// the ones that act on a selection are disabled without one, and Close Tab is disabled on
    /// the last tab. Before M14 Part A every row looked available and the ones that were not
    /// were refused *after* being chosen — a menu that lets you pick something and then declines
    /// is the shape that reads as broken.
    pub fn menu_table(&self) -> Vec<Menu<Msg>> {
        let selected = self.pane().list.selected.is_some();
        let act = |label: &'static str, a: Action| {
            Item::plain(label, Msg::Choose(a)).enabled(!a.needs_selection() || selected)
        };
        vec![
            Menu {
                title: "File",
                items: vec![
                    Item::new("New Tab", Accel::ctrl(NEW_TAB_KEYCODE, "T"), Msg::NewTab),
                    Item::new(
                        "Close Tab",
                        Accel::ctrl(CLOSE_TAB_KEYCODE, "W"),
                        Msg::CloseTab(self.current),
                    )
                    // **The last tab does not close**, which is the strip's existing rule said
                    // out loud: `CloseTab` on a lone tab is already a no-op, and a row that
                    // silently did nothing was the half of the affordance that was missing.
                    .enabled(self.panes.len() > 1),
                    Item::Separator,
                    act("New File", Action::NewFile),
                    act("New Folder", Action::NewFolder),
                    act("Rename", Action::Rename),
                    act("Delete", Action::Delete),
                ],
            },
            Menu { title: "Edit", items: vec![act("Copy", Action::Copy)] },
        ]
    }

    /// The name a delete is asking about, for the binary that owns the dialog's window.
    pub fn confirming(&self) -> Option<&str> {
        self.confirm.as_ref().map(|t| t.name.as_str())
    }

    /// The dialog could not be opened, so the question cannot be asked.
    ///
    /// **Nothing is removed**, which is the only safe answer: the alternative to asking is not
    /// deleting anyway. The same stance `nxedit::App::confirm_failed` takes, for the same
    /// reason — a window that failed to appear must not decide anything.
    pub fn confirm_failed(&mut self) {
        self.confirm = None;
        self.notice = Some(String::from("could not ask — nothing was deleted"));
    }

    /// What the dialog does with a key: `Esc` keeps the entry, and nothing else answers.
    ///
    /// **No key deletes.** `Enter` is the obvious candidate and the obvious accident, and this
    /// is the one operation in the browser that cannot be undone.
    pub fn confirm_key(&self, k: KeyEvent) -> Option<Msg> {
        if k.pressed != KEY_DOWN && k.pressed != KEY_REPEAT {
            return None;
        }
        (k.keycode == libkern::abi::KEY_ESC).then_some(Msg::KeepIt)
    }

    /// Whether a `StartMove` is owed **on the dialog's window**. Clears the record.
    pub fn take_confirm_move(&mut self) -> bool {
        core::mem::take(&mut self.confirm_move_requested)
    }

    /// How many characters have been typed into the name prompt, or `None` when none is open.
    ///
    /// **The receipt for the one thing typed here that is not a navigation key.** A gate driving
    /// a release image has no rendered field to read, so an injected burst of keystrokes is
    /// otherwise unacknowledged — and a dropped one arrives as a file with the wrong name, three
    /// steps later, looking like a logic bug. The same shape `nxedit::App::naming_len` has, and
    /// added for the same reason it was: a count, not the text.
    pub fn prompt_len(&self) -> Option<usize> {
        self.prompt.as_ref().map(|(_, _, f)| f.text().chars().count())
    }

    /// The row an internal drag is over, which the view draws a highlight on.
    pub fn drop_target(&self) -> Option<usize> {
        self.over
    }

    /// Which row is selected, for a caller that needs to know nothing moved it.
    pub fn list_selected(&self) -> Option<usize> {
        self.pane().list.selected
    }

    /// How far the current pane's list is scrolled, in rows.
    pub fn list_offset(&self) -> usize {
        self.pane().list.offset
    }

    /// The filesystem operation the binary owes, if any. Clears the record.
    pub fn take_op(&mut self) -> Option<FileOp> {
        self.op.take()
    }

    /// Report what an operation did, and say where to look next.
    ///
    /// **The notice is the whole of the feedback**, and it is set whichever way the operation
    /// went: an operation that silently succeeds and one that silently fails look identical, and
    /// the second is the one that matters. The caller re-lists afterwards, which is what makes
    /// the result visible.
    pub fn operated(&mut self, said: &str) {
        self.notice = Some(String::from(said));
    }

    /// Whether the browser has been asked to close.
    pub fn closing(&self) -> bool {
        self.closing
    }

    /// The size of this window in pixels.
    pub fn window_size(&self) -> Size {
        self.window
    }

    /// Take the window to `size`. `true` if anything changed.
    ///
    /// Accepts every `Configure`, which costs a browser nothing: unlike a terminal it has no
    /// cell grid to refit and no history to rewrap — the listing simply gets more or fewer
    /// visible rows, which `list_view` derives from the height it is given.
    pub fn resize(&mut self, size: Size) -> bool {
        if size == self.window {
            return false;
        }
        self.window = size;
        true
    }

    /// The height the listing is laid out at — the window less its chrome and the grip's corner.
    ///
    /// The grip's square is subtracted for the reason `nxterm`'s scrollbar subtracts it: the grip
    /// is the topmost layer and takes any press under it, so a row there would be a row that
    /// cannot be clicked.
    pub fn list_h(&self) -> u32 {
        // **`WINDOW_FRAME_H` too, since M11 Part E batch 2b.** `window_frame` insets the content
        // below the title bar, so a list built for the old height would be laid out three pixels
        // shorter — one row of arithmetic off, which this method's own reason for existing is to
        // prevent.
        self.window.h.saturating_sub(
            TITLE_BAR_H + MENU_BAR_H + TAB_STRIP_H + PATH_H + GRIP_W + WINDOW_FRAME_H,
        )
    }

    /// The element tree for the current state.
    ///
    /// **`&mut self`, because building the view scrolls the list.** `list_view` scrolls its
    /// state to follow the selection, and a view that did not keep the result would re-derive
    /// the offset from zero on every frame — parking the highlight on the last visible row and
    /// scrolling the whole list under it on every arrow press. That is what shipped in Part B,
    /// when the widget still *returned* the state and this caller dropped it; it takes `&mut`
    /// now, so the same mistake no longer compiles (PR #257 review, blocking 1).
    ///
    /// **The theme is the caller's**, because the caller paints this tree — and a tree built from
    /// one theme and painted with another is two themes in one frame, which one type makes easy
    /// to write and the old `Theme`/`Palette` split made impossible (PR #262 review, optional 5).
    /// It is also the shape Part C needs: a theme read from a file arrives in `main` and is
    /// handed down, rather than being fetched from a default in the middle of a view.
    pub fn view(&mut self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {

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
                // **A close button, because without one every close takes the wedged path.**
                // The taskbar's middle-click sends `CloseRequested`; a client that ignores it
                // keeps its window until a *second* middle-click destroys it with
                // `Manage::Close` — the route the shell documents as being for a client that
                // has *stopped answering*. An application with no way to close itself would
                // take it every single time (PR #257 review, finding 3). (That second click was
                // a two-second timer until M12 Part A; what changed is who decides, not that
                // this is the path an unanswered request ends on.)
                close: Some(Msg::Close),
            },
            &ui,
        )
        .key(TITLE_KEY);

        // The menu bar: two names, each of which drops a list of things to do. **The toolkit's
        // since M14 Part A** — the words were `button`s here, which is not what a menu bar's
        // word is, and the popup under them was a second copy of `nxterm`'s.
        let bar = libui::menu::bar(
            &self.menu_table(),
            &self.menus,
            MENU_BAR_KEY,
            hovered,
            Msg::MenuBar,
            &ui,
            MENU_BAR_H,
        );

        // The path strip: where you are, and the one control that leaves it — **or the name
        // being typed**, which replaces the path rather than sitting beside it. The strip is one
        // row of chrome and a prompt *is* what is happening; showing both would make a person
        // read two things to find out which one is asking for an answer. `nxedit`'s status strip
        // does exactly this with its own field, for the same reason.
        let middle = match self.prompt.as_ref() {
            // **The notice is beside the field, not behind it.** A rejected name — empty, or
            // one with a separator in it — leaves the prompt *open* so the person can fix it,
            // and the first version put the explanation in a slot the prompt had replaced. So
            // the answer was written and never drawn: the field kept the caret and nothing said
            // why Enter had done nothing. Found by the test below, which asserted the message
            // rather than the absence of an operation.
            Some((a, _, f)) => row(alloc::vec![
                padding(
                    Insets { top: 4, right: 4, bottom: 4, left: 6 },
                    text(String::from(a.prompt().unwrap_or(""))),
                )
                .key(PATH_KEY),
                padding(
                    Insets { top: 2, right: 6, bottom: 2, left: 0 },
                    text_field(f, false, WidgetState { active: true, ..Default::default() }, &ui),
                )
                .key(PROMPT_KEY)
                .flex(1),
                padding(
                    Insets { top: 4, right: 6, bottom: 4, left: 0 },
                    text(self.notice.clone().unwrap_or_default()),
                )
                .key(NOTICE_KEY),
            ]),
            None => row(alloc::vec![
                padding(Insets { top: 4, right: 4, bottom: 4, left: 6 }, text(self.pane().path.clone()))
                    .key(PATH_KEY),
                padding(
                    Insets { top: 4, right: 4, bottom: 4, left: 6 },
                    text(self.notice.clone().unwrap_or_default()),
                )
                .key(NOTICE_KEY),
            ]),
        };
        let strip = row(alloc::vec![
            button(
                "^",
                Msg::Up,
                WidgetState { hovered: hovered == Some(UP_KEY), ..Default::default() },
                &ui,
            )
            .key(UP_KEY),
            middle.key(STRIP_INNER_KEY).flex(1),
        ]);

        // **The tab strip, between the menus and the path.** Above the path because a tab *is*
        // a path — the strip says which of several you are looking at, and the strip below says
        // where that one is.
        let tab_list = self.tabs();
        let items: Vec<libui::widget::Tab<'_>> = tab_list
            .iter()
            .map(|(k, label)| libui::widget::Tab { key: *k, label: label.as_str(), marked: false })
            .collect();
        let tabs = tab_strip(&items, self.current, hovered, Msg::SelectTab, Msg::CloseTab, &ui);

        let labels: Vec<String> = self.pane().entries.iter().map(|e| e.label()).collect();
        let mut rows: Vec<ListRow<'_>> = Vec::with_capacity(labels.len());
        for (i, l) in labels.iter().enumerate() {
            rows.push(ListRow { key: i as u64, label: l });
        }
        let h = self.list_h();
        // **`Grab` on the press, `Activate` on the click.** A drag is decided when the button
        // lands on a row; by the time it comes up the gesture is over. The two do not fight —
        // a press that never moves produces a click and opens what was pressed, and one that
        // moves has already told the compositor it is carrying something.
        // **The drop target borrows the hover face.** A row an internal drag is over is the row
        // that would be acted on if you let go now, which is what hover already means — so it
        // needs no new state in the widget and cannot disagree with what a pointer highlight
        // looks like. While a drag runs it *replaces* the pointer's own hover, because the
        // pointer is holding something and the only thing under it that matters is where it
        // lands.
        let highlight = match self.over {
            Some(i) => Some(i as u64),
            None => hovered,
        };
        let list = list_view(
            &rows,
            &mut self.pane_mut().list,
            h,
            ROW_H,
            Msg::Activate,
            Some(Msg::Grab),
            Some(Msg::Scroll),
            highlight,
            &ui,
        );

        let body = window_frame(
            title,
            dock(
                alloc::vec![
                    docked(Edge::Top, sized(Size::new(0, MENU_BAR_H), bar).key(BAR_KEY)),
                    docked(
                        Edge::Top,
                        sized(Size::new(0, TAB_STRIP_H), tabs).key(TAB_STRIP_KEY),
                    ),
                    docked(Edge::Top, sized(Size::new(0, PATH_H), strip).key(STRIP_KEY)),
                ],
            // **Sized to the height it was built for.** `list_view` does not size itself, and
            // the dock's flex child otherwise gets everything left over — so the widget would
            // build rows for one height and be drawn at another, leaving `visible` off by one
            // for the scroll arithmetic and a dead row at the bottom. Its own doc names this
            // wrapper as the reliable way to keep the two in step.
                sized(Size::new(0, h), list).key(LIST_KEY),
            ),
            &ui,
        );

        // The grip over the bottom-right corner, as `nxterm` places its own.
        let grip = offset(
            self.window.w.saturating_sub(GRIP_W) as i32,
            self.window.h.saturating_sub(GRIP_W) as i32,
            resize_grip(Msg::ResizeWindow(RESIZE_RIGHT | RESIZE_BOTTOM), &ui).key(GRIP_KEY),
        );
        stack(alloc::vec![body, grip])
    }
}

impl App {
    /// The open menu's contents — the popup's whole face.
    ///
    /// **`menu_item`, not `button`**: a menu row highlights the way a selected list row does,
    /// because they are the same thing seen twice — the item that would happen if you acted now.
    /// `hovered` comes from the popup's own router, which is a different tree from this window's.
    /// **Which menu is a parameter, not read from state** (PR #268 review, optional 2). It used
    /// to fall through to *File* whenever `self.menu` was `None` — and `Msg::Choose` clears that
    /// before the binary closes the window, so any further event from an *Edit* popup in the same
    /// batch would have routed against the File tree. Nothing could be made of it, and a shape
    /// that depends on nobody making anything of it is one to remove rather than to argue about.
    pub fn menu_view(&self, which: usize, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        let menus = self.menu_table();
        match menus.get(which) {
            Some(m) => libui::menu::popup(m, &self.menus, MENU_ROW_KEY, hovered, ui),
            // Not reachable while a popup exists — the window is opened and destroyed with the
            // menu — but the type demands an answer and a caller should not have to think about
            // which index is live.
            None => popup_frame(padding(Insets::all(2), libui::element::text("")), ui),
        }
    }

    /// The confirmation dialog's tree — a second window, and the browser's only question.
    ///
    /// **The frame and the two aim points are `libui`'s** since M12 Part B, shared with the
    /// editor's confirmation: one table of metrics, one host test pinning it, and one set of
    /// numbers for `check-login` to type. What is this application's is what the question says
    /// and which half carries which answer.
    ///
    /// **The destructive answer is on the left**, matching the editor's *discard*, because a
    /// person who has learned where the safe half is should not have to relearn it per window.
    pub fn confirm_view(&self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        let name = self.confirm.as_ref().map(|t| t.name.as_str()).unwrap_or_default();
        let what = if self.confirm.as_ref().is_some_and(|t| t.is_dir) {
            "Delete this folder and everything in it?"
        } else {
            "Delete this file?"
        };
        let title = title_bar(
            "Delete",
            self.confirm_focused,
            Msg::DragConfirm,
            // One button, and it is the cautious answer: closing a question must not perform it.
            TitleButtons { minimise: None, maximise: None, close: Some(Msg::KeepIt) },
            ui,
        )
        .key(CONFIRM_TITLE_KEY);
        let question = padding(
            Insets::all(libui::widget::DIALOG_PAD),
            column(alloc::vec![text(String::from(what)), text(String::from(name))]),
        )
        .key(CONFIRM_TEXT_KEY);
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
                answer("delete", Msg::ConfirmDelete, CONFIRM_DELETE_KEY),
                answer("keep it", Msg::KeepIt, CONFIRM_KEEP_KEY),
            ]),
            DIALOG_GAP,
        );
        dialog_frame(title, question, buttons, ui)
    }
}

/// Join a directory path and an entry name.
///
/// `libfs::join`'s rule, on `str` rather than bytes — this half of the application never sees a
/// path as bytes, and converting to call it would be a round trip through a lossy conversion
/// for a rule that is one line.
fn join(dir: &str, name: &str) -> String {
    let mut s = String::from(dir);
    if !s.ends_with('/') {
        s.push('/');
    }
    s.push_str(name);
    s
}

/// Everything before the final component, or `"/"` at the root.
///
/// `libfs::parent`'s rule on `str`, for the reason [`join`] is here.
fn parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => String::from("/"),
        Some(i) => String::from(&trimmed[..i]),
    }
}

/// The rectangle the window occupies, for the binary's layout call.
pub fn bounds(size: Size) -> Rect {
    Rect::new(0, 0, size.w, size.h)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text under the element keyed `key`, joined — for asserting on chrome without
    /// hard-coding where in the tree it sits.
    fn labelled<M>(e: &Element<M>, key: u64) -> String {
        fn texts<M>(e: &Element<M>, out: &mut String) {
            if let libui::element::Node::Text(t) = &e.node {
                out.push_str(t);
            }
            for c in e.children() {
                texts(c, out);
            }
        }
        fn find<'a, M>(e: &'a Element<M>, key: u64) -> Option<&'a Element<M>> {
            if e.key == Some(key) {
                return Some(e);
            }
            e.children().find_map(|c| find(c, key))
        }
        let mut out = String::new();
        if let Some(n) = find(e, key) {
            texts(n, &mut out);
        }
        out
    }

    fn app() -> App {
        let mut a = App::new("/home");
        a.show(
            "/home",
            alloc::vec![
                Entry::file("notes.txt"),
                Entry::dir("work"),
                Entry::file("a.txt"),
                Entry::dir("archive"),
            ],
        );
        a
    }

    /// Three consecutive frames must diff, and **the frame that fails tells you which mistake
    /// it is.**
    ///
    /// `MixedKeying` — a container with some keyed children and some not — fails at frame **0**,
    /// because the check runs against an empty previous list for a new node as well. In this
    /// application that is `fail()` before the first commit, so no window ever appears.
    /// `KeyingChanged` needs a successful first frame to compare against and fails at frame 1,
    /// which is the window that paints once and then goes dead. Reporting the frame number is
    /// what separates them.
    ///
    /// Found the expensive way, in a four-minute boot, when this would have found it in a
    /// millisecond — and it did, for the second instance, at frame 0 (M10 Part B).
    #[test]
    fn consecutive_frames_diff_and_the_failing_one_is_named() {
        use libui::diff::Tree;
        use libui::layout::{FixedCell, layout};
        let mut a = app();
        let cell = FixedCell { w: 8, h: 16 };
        let mut tree = Tree::new();
        for frame in 0..3 {
            let e = a.view(&UiTheme::default(), None);
            let l = layout(&e, bounds(a.window_size()), &cell);
            tree.update(&e, &l).unwrap_or_else(|err| panic!("frame {frame}: {err:?}"));
        }
    }

    /// A listing long enough to scroll.
    fn big() -> App {
        let mut a = App::new("/big");
        let rows: Vec<Entry> = (0..40)
            .map(|i| Entry::file(&alloc::format!("f{i:02}")))
            .collect();
        a.show("/big", rows);
        a
    }

    fn down(a: &mut App) {
        a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_DOWN, KEY_DOWN, 0)));
    }

    fn up(a: &mut App) {
        a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_UP, KEY_DOWN, 0)));
    }

    #[test]
    fn the_scroll_offset_persists_so_up_moves_the_selection_and_not_the_list() {
        // **`list_view` scrolls the state it is handed, and a caller that keeps none re-derives
        // the offset from zero every frame.** The selection then sits on the *last visible row*
        // for ever: press Up and the highlight does not move, the entire listing scrolls down
        // by one underneath it — and the whole list area repaints instead of two rows
        // (PR #257 review, blocking 1). The signature took its state by value then and returned
        // the scrolled copy; since Part C it takes `&mut`, so the drop this catches now needs a
        // deliberate `&mut x.clone()` rather than a `_` in a pattern.
        let mut a = big();
        for _ in 0..19 {
            down(&mut a);
        }
        let _ = a.view(&UiTheme::default(), None);
        let scrolled = a.list_offset();
        assert!(scrolled > 0, "precondition: 19 rows down has scrolled the list");
        assert_eq!(a.list_selected(), Some(19));

        up(&mut a);
        let _ = a.view(&UiTheme::default(), None);
        assert_eq!(
            a.list_offset(), scrolled,
            "the selection moved up inside the visible rows, so the list must not have moved"
        );
        assert_eq!(a.list_selected(), Some(18));
    }

    #[test]
    fn a_listing_puts_directories_first_then_names_in_order() {
        // **The order is the browser's, not the filesystem's.** Entries arrive in whatever order
        // the directory server and the namespace enumeration produced, which is not an order
        // anybody chose — and a listing that changed order between two visits to the same
        // directory would be unusable for the one thing a browser is for.
        let a = app();
        let names: Vec<&str> = a.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["archive", "work", "a.txt", "notes.txt"]);
    }

    #[test]
    fn a_directory_is_marked_by_a_trailing_separator() {
        let a = app();
        assert_eq!(a.entries()[0].label(), "archive/");
        assert_eq!(a.entries()[2].label(), "a.txt");
    }

    #[test]
    fn pressing_a_directory_navigates_and_pressing_a_file_opens_it() {
        // **The two halves of what a row press means**, and the reason they are one message:
        // Enter and a press must agree about "open", or there are two browsers. Which of the
        // two outboxes fills is the entry's kind, and nothing else.
        let mut a = app();
        a.update(Msg::Activate(0)); // archive/
        assert_eq!(a.take_goto().as_deref(), Some("/home/archive"));
        assert_eq!(a.take_open(), None, "a directory is navigated, not opened");

        a.update(Msg::Activate(2)); // a.txt
        assert_eq!(a.take_goto(), None, "a file is not a place to go");
        assert_eq!(a.take_open().as_deref(), Some("/home/a.txt"), "it is a thing to open");
    }

    #[test]
    fn the_strip_says_what_happened_to_the_last_row_opened() {
        // **A row press with no visible effect is indistinguishable from a broken one.** The
        // window the shell launches belongs to another process and may take a moment; what this
        // browser can say is whether the request was taken.
        let mut a = app();
        a.update(Msg::Activate(2));
        let path = a.take_open().unwrap();
        a.opened(&path, true);
        let ui: Element<Msg> = a.view(&UiTheme::default(), None);
        assert!(labelled(&ui, NOTICE_KEY).contains("opening a.txt"), "the strip says so");

        a.opened(&path, false);
        let ui: Element<Msg> = a.view(&UiTheme::default(), None);
        assert!(labelled(&ui, NOTICE_KEY).contains("could not open a.txt"));

        // And a new listing supersedes it: the notice is about a press, not about the directory.
        a.show("/home", alloc::vec![Entry::file("a.txt")]);
        let ui: Element<Msg> = a.view(&UiTheme::default(), None);
        assert_eq!(labelled(&ui, NOTICE_KEY), "", "a listing clears it");
    }

    /// A point outside a `START_SIZE` window, which is what hands a drag to the compositor.
    const OUTSIDE: (i32, i32) = (START_SIZE.w as i32 + 10, 100);

    #[test]
    fn a_press_becomes_a_drag_only_after_it_has_travelled() {
        // **A click is a press that moved a little.** Nobody presses a button without nudging
        // it, so a browser that started a drag on the first pixel would make opening a file by
        // clicking it a matter of luck.
        let mut a = app();
        a.update(Msg::Grab(2)); // a.txt
        assert_eq!(a.pointer_moved(100, 100, 1), Gesture::None, "the first motion fixes the origin");
        assert_eq!(
            a.pointer_moved(100 + DRAG_SLOP - 1, 100, 1),
            Gesture::None,
            "a nudge is not a drag"
        );
        assert_eq!(a.pointer_moved(100 + DRAG_SLOP, 100, 1), Gesture::Moved, "and travelling is");

        // **And travelling does not reach the compositor** (M12 Part B): the drag is this
        // window's until it leaves. Asking for it here is asking for a payload that has not been
        // handed over.
        assert_eq!(a.take_drag(), None, "an internal drag is not the compositor's");

        assert_eq!(a.pointer_moved(OUTSIDE.0, OUTSIDE.1, 1), Gesture::HandOff, "out of the window");
        let (entry, path) = a.take_drag().expect("the row that was pressed");
        assert_eq!((entry.name.as_str(), entry.is_dir), ("a.txt", false));
        assert_eq!(path, "/home/a.txt");
        assert_eq!(a.take_drag(), None, "and it is offered once");
    }

    #[test]
    fn releasing_without_travelling_leaves_no_drag_behind() {
        // The record has to be cleared by the button coming up, or the *next* press-free motion
        // would start a drag for a row nobody is holding.
        let mut a = app();
        a.update(Msg::Grab(0));
        assert_eq!(a.pointer_moved(10, 10, 1), Gesture::None);
        assert_eq!(a.pointer_moved(10, 10, 0), Gesture::None, "the button came up");
        assert_eq!(a.pointer_moved(400, 400, 1), Gesture::None, "a later motion carries nothing");
        assert_eq!(a.take_drag(), None);
    }

    #[test]
    fn a_listing_that_changes_under_a_press_carries_nothing() {
        // **Reachable, not theoretical**: the press that starts this also raised the window, so
        // the keyboard is here — Backspace between the press and the move is one keystroke away.
        // An index remembered across that names a different row; a name names none.
        let mut a = app();
        a.update(Msg::Grab(2)); // a.txt, in /home
        assert_eq!(a.pointer_moved(10, 10, 1), Gesture::None, "the press is recorded");

        a.show("/", alloc::vec![Entry::dir("bin"), Entry::dir("dev"), Entry::file("zzz")]);
        assert_eq!(
            a.pointer_moved(10 + DRAG_SLOP, 10, 1),
            Gesture::None,
            "the row it was on is gone"
        );
        assert_eq!(a.pointer_moved(OUTSIDE.0, OUTSIDE.1, 1), Gesture::None, "so nothing leaves");
        assert_eq!(a.take_drag(), None, "and nothing is offered");

        // And a listing that still holds the row drags *that* row, whatever moved around it.
        let mut a = app();
        a.update(Msg::Grab(2)); // a.txt
        a.pointer_moved(10, 10, 1);
        a.show("/home", alloc::vec![Entry::file("a.txt"), Entry::dir("new"), Entry::dir("work")]);
        assert_eq!(a.pointer_moved(10 + DRAG_SLOP, 10, 1), Gesture::Moved);
        assert_eq!(a.pointer_moved(OUTSIDE.0, OUTSIDE.1, 1), Gesture::HandOff);
        let (entry, path) = a.take_drag().expect("the row that was pressed");
        assert_eq!(entry.name, "a.txt", "by name, not by position");
        assert_eq!(path, "/home/a.txt");
    }

    #[test]
    fn a_directory_row_drags_as_a_directory() {
        // The kind is the entry's, not the gesture's: an editor that takes files only must not
        // be highlighted for a folder, and the compositor decides that from what this says.
        let mut a = app();
        a.update(Msg::Grab(0)); // archive/
        a.pointer_moved(10, 10, 1);
        a.pointer_moved(10 + DRAG_SLOP, 10, 1);
        a.pointer_moved(OUTSIDE.0, OUTSIDE.1, 1);
        let (entry, path) = a.take_drag().expect("a drag");
        assert!(entry.is_dir, "the browser reports what the row is");
        assert_eq!(path, "/home/archive");
    }

    /// One key press, as the compositor delivers one.
    fn press_key(a: &mut App, code: u16) {
        a.update(Msg::Key(KeyEvent::new(1, code, KEY_DOWN, 0)));
    }

    /// `x`, the one letter these tests type, and `/`, the one they must not be allowed to.
    ///
    /// Literals because `libkern::abi` names only the keys with no character; the test below
    /// pins both against `libinput`'s table rather than against this comment.
    const KEY_X: u16 = 45;
    /// See [`KEY_X`].
    const KEY_SLASH: u16 = 53;

    #[test]
    fn the_typed_keycodes_are_the_ones_the_keymap_calls_x_and_slash() {
        assert_eq!(libinput::keymap::to_char(KEY_X, 0), Some(b'x'));
        assert_eq!(libinput::keymap::to_char(KEY_SLASH, 0), Some(b'/'));
    }

    /// The middle of row `i`, in window coordinates.
    fn row_y(a: &App, i: u32) -> i32 {
        (a.list_top() + ROW_H * i + ROW_H / 2) as i32
    }

    /// Select row `i` with the arrow keys, which is what a person has to do first.
    ///
    /// **From the top every time**, rather than `i` steps from wherever the selection happens to
    /// be: a helper that moved *relatively* makes every test after the first one in a function
    /// depend on where the last one left it.
    fn select(a: &mut App, i: usize) {
        for _ in 0..a.entries().len() {
            press_key(a, libkern::abi::KEY_UP);
        }
        for _ in 0..i {
            press_key(a, libkern::abi::KEY_DOWN);
        }
    }

    #[test]
    fn an_operation_with_nothing_selected_is_answered_rather_than_ignored() {
        // **A menu row that silently does nothing is a control that looks live and is not** —
        // the defect M8's overview shipped three of. Two of the five act on the directory and
        // three on a row, and the three have to say so when there is no row.
        let mut a = App::new("/home");
        a.show("/home", alloc::vec![]);
        for action in [Action::Rename, Action::Copy, Action::Delete] {
            a.update(Msg::Choose(action));
            assert!(a.take_op().is_none(), "{action:?} did something with nothing selected");
            assert!(a.confirming().is_none(), "{action:?} asked about nothing");
            let ui: Element<Msg> = a.view(&UiTheme::default(), None);
            assert!(
                labelled(&ui, NOTICE_KEY).contains("nothing is selected"),
                "{action:?} said nothing"
            );
        }
        // And the two that make something do not need one.
        a.update(Msg::Choose(Action::NewFolder));
        press_key(&mut a, KEY_X);
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(a.take_op(), Some(FileOp::Create { path: "/home/x".into(), dir: true }));
    }

    #[test]
    fn the_prompt_turns_what_was_typed_into_an_operation() {
        let mut a = app();
        select(&mut a, 2); // a.txt
        a.update(Msg::Choose(Action::Rename));
        // **The strip shows the prompt in place of the path**, not beside it.
        let ui: Element<Msg> = a.view(&UiTheme::default(), None);
        assert_eq!(labelled(&ui, PATH_KEY), "rename to:", "the verb replaces the path");

        press_key(&mut a, KEY_X);
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(
            a.take_op(),
            Some(FileOp::Rename { from: "/home/a.txt".into(), to: "/home/x".into() })
        );

        // Copy is the same shape and a different operation — the one that reaches `copy_file`.
        select(&mut a, 2);
        a.update(Msg::Choose(Action::Copy));
        press_key(&mut a, KEY_X);
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(
            a.take_op(),
            Some(FileOp::Copy {
                from: "/home/a.txt".into(),
                to: "/home/x".into(),
                dir: false
            })
        );
    }

    #[test]
    fn a_prompt_takes_a_name_and_not_a_path() {
        // **A separator would be a way to write outside the directory being shown by typing
        // into it.** This browser shows one directory; it is not a command line.
        let mut a = app();
        a.update(Msg::Choose(Action::NewFile));
        for code in [KEY_X, KEY_SLASH, KEY_X] {
            press_key(&mut a, code);
        }
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert!(a.take_op().is_none(), "a path is not a name");
        let ui: Element<Msg> = a.view(&UiTheme::default(), None);
        assert!(labelled(&ui, NOTICE_KEY).contains("not a path"), "and it says which");
    }

    #[test]
    fn an_empty_name_leaves_the_prompt_open_and_escape_closes_it() {
        let mut a = app();
        a.update(Msg::Choose(Action::NewFolder));
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert!(a.take_op().is_none(), "nothing typed is not a name");
        let ui: Element<Msg> = a.view(&UiTheme::default(), None);
        assert_eq!(labelled(&ui, PATH_KEY), "new folder:", "and it is still asking");

        press_key(&mut a, libkern::abi::KEY_ESC);
        let ui: Element<Msg> = a.view(&UiTheme::default(), None);
        assert_eq!(labelled(&ui, PATH_KEY), "/home", "the path is back");
        assert!(a.take_op().is_none());
    }

    #[test]
    fn while_a_name_is_being_typed_the_keys_are_the_fields() {
        // **Backspace especially**: it goes *up a directory* the rest of the time, and a person
        // correcting a typo would otherwise leave the directory they are naming something in.
        let mut a = app();
        a.update(Msg::Choose(Action::NewFile));
        press_key(&mut a, libkern::abi::KEY_BACKSPACE);
        assert!(a.take_goto().is_none(), "Backspace edited the field, not the path");
        // And the arrows do not move the selection out from under a rename in progress.
        let before = a.list_selected();
        press_key(&mut a, libkern::abi::KEY_DOWN);
        assert_eq!(a.list_selected(), before);
    }

    #[test]
    fn deleting_asks_first_and_only_the_answer_removes() {
        let mut a = app();
        select(&mut a, 2); // a.txt
        a.update(Msg::Choose(Action::Delete));
        assert_eq!(a.confirming(), Some("a.txt"), "it asks");
        assert!(a.take_op().is_none(), "and does nothing while it is asking");

        // Keeping it removes the question and nothing else.
        a.update(Msg::KeepIt);
        assert!(a.confirming().is_none());
        assert!(a.take_op().is_none(), "keeping must not delete");

        // Asked again, and answered the other way.
        a.update(Msg::Choose(Action::Delete));
        a.update(Msg::ConfirmDelete);
        assert!(a.confirming().is_none(), "the question goes when it is answered");
        assert_eq!(
            a.take_op(),
            Some(FileOp::Delete { path: "/home/a.txt".into(), dir: false })
        );

        // A directory carries `dir`, which is what decides whether the removal descends.
        let mut a = app();
        a.update(Msg::Choose(Action::Delete)); // row 0 is `archive/`
        a.update(Msg::ConfirmDelete);
        assert_eq!(
            a.take_op(),
            Some(FileOp::Delete { path: "/home/archive".into(), dir: true })
        );
    }

    #[test]
    fn the_dialog_answers_escape_and_nothing_else() {
        // No key deletes. `Enter` is the obvious candidate and the obvious accident, and this is
        // the one operation in the browser that cannot be undone.
        let a = app();
        let ev = |code: u16, pressed: u16| KeyEvent::new(1, code, pressed, 0);
        assert_eq!(a.confirm_key(ev(libkern::abi::KEY_ESC, KEY_DOWN)), Some(Msg::KeepIt));
        assert_eq!(a.confirm_key(ev(libkern::abi::KEY_ENTER, KEY_DOWN)), None);
        assert_eq!(a.confirm_key(ev(libkern::abi::KEY_ESC, 0)), None, "a release is not an answer");
    }

    #[test]
    fn a_drag_that_stays_inside_moves_into_the_directory_it_is_released_over() {
        // **The half M10 Part E did not build.** The compositor is not involved and could not
        // be: it skips the source window when it looks for a drop target, so a drag that came
        // out of this list can never be delivered back to it.
        let mut a = app();
        a.update(Msg::Grab(2)); // a.txt
        a.pointer_moved(100, row_y(&a, 2), 1);
        assert_eq!(a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 2), 1), Gesture::Moved);
        // Over `work/`, which is a directory and therefore a target.
        assert_eq!(a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 1), Gesture::Moved);
        assert_eq!(a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 0), Gesture::Dropped);
        assert_eq!(
            a.take_op(),
            Some(FileOp::MoveInto { from: "/home/a.txt".into(), to: "/home/work/a.txt".into() })
        );
        assert_eq!(a.take_drag(), None, "and the compositor was never told");
    }

    #[test]
    fn only_a_directory_is_a_drop_target() {
        // A file row is not a destination, and a directory dropped on itself is a rename to a
        // path underneath the thing being moved.
        let mut a = app();
        a.update(Msg::Grab(2)); // a.txt
        a.pointer_moved(100, row_y(&a, 2), 1);
        a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 2), 1);
        // Row 3 is `notes.txt` — a file.
        assert_eq!(a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 3), 1), Gesture::None);
        assert_eq!(a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 3), 0), Gesture::None);
        assert!(a.take_op().is_none(), "a file row is not a folder");

        let mut a = app();
        a.update(Msg::Grab(1)); // work/
        a.pointer_moved(100, row_y(&a, 1), 1);
        a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 1);
        assert_eq!(a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 0), Gesture::Dropped);
        assert!(a.take_op().is_none(), "onto itself is not a move");
    }

    #[test]
    fn the_drop_target_follows_the_pointer_and_only_over_a_folder() {
        // **What the view draws the highlight from.** It borrows the list's own hover face, so a
        // drop target and a pointer highlight cannot come to look different — and the widget
        // needs no state for it.
        let mut a = app();
        a.update(Msg::Grab(2)); // a.txt
        a.pointer_moved(100, row_y(&a, 2), 1);
        a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 2), 1);
        assert_eq!(a.drop_target(), None, "over the file it came from");
        a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 1);
        assert_eq!(a.drop_target(), Some(1), "over `work/`");
        a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 3), 1);
        assert_eq!(a.drop_target(), None, "over a file again");
        // Off the list entirely — the chrome above it is not a row.
        a.pointer_moved(100 + DRAG_SLOP, 4, 1);
        assert_eq!(a.drop_target(), None, "over the title bar");
        // And it is cleared when the gesture ends, or the next frame would highlight a row
        // nobody is dragging onto.
        a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 1);
        a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 0);
        assert_eq!(a.drop_target(), None);
    }

    #[test]
    fn the_menus_hold_what_they_say() {
        // **Cut and paste are absent on purpose**, and this is where that stays true: a pair
        // that holds something between two gestures is a clipboard, and M12 Part E builds the
        // real one. An `Edit` menu that grew them here would be a second.
        let mut a = app();
        a.update(Msg::MenuBar(0));
        let file: Element<Msg> = a.menu_view(0, &UiTheme::default(), None);
        // **Row keys are positional now**, so the labels are read in order — which also pins the
        // separator's place, since a rule occupies a row index without being one.
        // The label *and its chord*, because `labelled` concatenates the row's text and the
        // accelerator column is text in the same row — which makes this the place the advertised
        // chords are pinned as strings rather than as `Accel` values.
        for (i, item) in [
            (0, "New TabCtrl+T"),
            (1, "Close TabCtrl+W"),
            (3, "New File"),
            (4, "New Folder"),
            (5, "Rename"),
            (6, "Delete"),
        ] {
            assert_eq!(labelled(&file, MENU_ROW_KEY + i), item, "the File menu's row {i}");
        }
        // Row 2 is the separator: it is keyed by nothing, so nothing is found at its index.
        assert_eq!(labelled(&file, MENU_ROW_KEY + 2), "", "row 2 is a rule, not an item");

        a.update(Msg::MenuBar(1));
        let edit: Element<Msg> = a.menu_view(1, &UiTheme::default(), None);
        assert_eq!(labelled(&edit, MENU_ROW_KEY), "Copy");
        // **And nothing else is here**: cut and paste are a pair that holds something between
        // two gestures, which is a clipboard however it is spelled, and M12 Part E builds the
        // real one. An `Edit` menu that grew them now would be a second clipboard.
        assert_eq!(labelled(&edit, MENU_ROW_KEY + 1), "", "the Edit menu holds one thing");
    }

    /// A row that would be refused is drawn unavailable rather than offered.
    ///
    /// **The half of the affordance that was missing** (M14 Part A). `choose` has always answered
    /// "nothing is selected" in the strip; what it could not do is stop the row looking live.
    #[test]
    fn rows_that_need_a_selection_are_disabled_without_one() {
        // **An empty directory is how there is no selection**: `show` selects the first row of
        // any listing that has one, so this is the real state a person reaches rather than a
        // constructed one.
        let mut a = App::new("/empty");
        a.show("/empty", alloc::vec![]);
        let needs = |a: &App, title: &str| -> Vec<bool> {
            a.menu_table()
                .into_iter()
                .find(|m| m.title == title)
                .expect("the menu exists")
                .items
                .iter()
                .filter_map(|it| match it {
                    Item::Action { msg: Msg::Choose(act), enabled, .. } => {
                        act.needs_selection().then_some(*enabled)
                    }
                    _ => None,
                })
                .collect()
        };
        assert_eq!(needs(&a, "File"), alloc::vec![false, false], "Rename and Delete, with nothing selected");
        assert_eq!(needs(&a, "Edit"), alloc::vec![false], "Copy, with nothing selected");
        // **The negative control.** Give it a listing and the same three become available —
        // without it this test would pass for a version that disabled everything unconditionally.
        a.show("/empty", alloc::vec![Entry::file("a"), Entry::file("b")]);
        assert_eq!(needs(&a, "File"), alloc::vec![true, true]);
        assert_eq!(needs(&a, "Edit"), alloc::vec![true]);
        // …and the ones that do not need a selection were never affected either way.
        let file = a.menu_table().into_iter().next().expect("File");
        assert!(
            matches!(file.items[3], Item::Action { enabled: true, .. }),
            "New File does not act on a selection"
        );
    }

    /// Close Tab is offered only when closing one leaves a browser behind.
    #[test]
    fn close_tab_is_unavailable_on_the_last_tab() {
        let mut a = app();
        let enabled = |a: &App| match &a.menu_table()[0].items[1] {
            Item::Action { label, enabled, .. } => {
                assert_eq!(*label, "Close Tab");
                *enabled
            }
            Item::Separator => panic!("row 1 is Close Tab"),
        };
        assert!(!enabled(&a), "the lone tab does not close");
        a.update(Msg::NewTab);
        assert!(enabled(&a), "with two, either can go");
    }

    #[test]
    fn a_bar_item_opens_its_menu_and_the_same_press_closes_it() {
        // A bar item that only opened would leave the menu up until something else dismissed
        // it, and what dismisses a popup is a press on another window.
        let mut a = app();
        assert_eq!(a.menus.open(), None);
        a.update(Msg::MenuBar(0));
        assert_eq!(a.menus.open(), Some(0));
        a.update(Msg::MenuBar(0));
        assert_eq!(a.menus.open(), None, "the same item closes it");
        // And the other item swaps rather than stacking.
        a.update(Msg::MenuBar(0));
        a.update(Msg::MenuBar(1));
        assert_eq!(a.menus.open(), Some(1));
        // Choosing anything puts the menu away, or it would sit over the answer. **Every row,
        // not the one this used to check** — the rule is asked of the table now, so this walks it.
        for msg in a
            .menu_table()
            .iter()
            .flat_map(|m| m.items.iter())
            .filter_map(|it| match it {
                Item::Action { msg, .. } => Some(msg.clone()),
                Item::Separator => None,
            })
            .collect::<Vec<_>>()
        {
            let mut a = app();
            a.update(Msg::MenuBar(0));
            a.update(msg.clone());
            assert_eq!(a.menus.open(), None, "{msg:?} left the menu open");
        }
    }

    #[test]
    fn a_drop_below_the_last_drawn_row_lands_nowhere() {
        // **A window is taller than the rows it has room for** — up to a row's worth, plus the
        // grip and the frame — and a `y` in that band is still inside the window and still under
        // the pointer grab. Bounding a drop against `entries.len()` rather than against the rows
        // `list_view` actually drew mapped it to a directory that was never on screen and never
        // highlighted, so a file moved into a folder the person could not see (PR #268 review,
        // blocking 1).
        let mut a = App::new("/big");
        let mut rows: alloc::vec::Vec<Entry> = (1..=20)
            .map(|i| Entry::dir(&alloc::format!("d{i:02}")))
            .collect();
        rows.extend((1..=21).map(|i| Entry::file(&alloc::format!("f{i:02}"))));
        a.show("/big", rows);
        assert!(a.entries().len() > a.visible_rows(), "more entries than the window draws");

        let last_drawn_bottom = (a.list_top() + a.visible_rows() as u32 * ROW_H) as i32;
        assert!(
            last_drawn_bottom < a.window_size().h as i32,
            "there is a band below the rows and inside the window, which is the whole hazard"
        );

        a.update(Msg::Grab(1)); // d02
        a.pointer_moved(100, row_y(&a, 1), 1);
        a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 1);
        assert_eq!(a.drop_target(), Some(1), "over a real row to start with");
        // Two pixels below the last drawn row, and well inside the window. The gesture reports
        // `Moved` because the *highlight* changed — it had to be cleared — and what matters is
        // what it changed to.
        let below = last_drawn_bottom + 2;
        assert_eq!(a.pointer_moved(100 + DRAG_SLOP, below, 1), Gesture::Moved);
        assert_eq!(a.drop_target(), None, "nothing is drawn there");
        assert_eq!(a.pointer_moved(100 + DRAG_SLOP, below, 0), Gesture::None);
        assert!(a.take_op().is_none(), "and letting go there moves nothing");
    }

    #[test]
    fn a_question_answered_after_navigating_still_names_what_it_asked_about() {
        // **The class this and the two below share**: an operation is two gestures, and the first
        // version composed its paths at the *second* — out of `self.path` and the selection, both
        // of which move while a prompt or a dialog is up. There is no input-exclusive window
        // (`TODO(dialog-modality)`) and the prompt is a keyboard mode only, so the parent goes on
        // taking clicks (PR #268 review, blocking 2).
        let mut a = app();
        select(&mut a, 2); // a.txt
        a.update(Msg::Choose(Action::Delete));
        assert_eq!(a.confirming(), Some("a.txt"));

        // The person clicks `work/` in the parent, which navigates. The binary re-lists.
        a.update(Msg::Activate(1));
        let to = a.take_goto().expect("a directory row navigates");
        assert_eq!(to, "/home/work");
        a.show(&to, alloc::vec![Entry::file("a.txt"), Entry::file("payroll.txt")]);

        // **A listing supersedes the question**, so there is nothing left to answer wrongly.
        assert_eq!(a.confirming(), None, "a question about a directory you have left is gone");
        a.update(Msg::ConfirmDelete);
        assert!(a.take_op().is_none());
    }

    #[test]
    fn a_delete_answered_after_the_selection_moves_removes_what_was_asked_about() {
        // The half a re-listing does *not* cover: clicking a **file** row re-selects without
        // navigating, so nothing clears the question.
        //
        // **This does not pin the captured path, and saying so is the point.** `self.path` only
        // changes in `show`, and `show` now drops the question — so for *delete* the two fixes
        // overlap and composing the path late would give the same answer here. Run alone against
        // that version this test passes, which makes it a guard for the question surviving a
        // selection change and nothing more. What pins the capture is the rename below, where the
        // *source* moves without a listing.
        let mut a = app();
        select(&mut a, 2); // a.txt
        a.update(Msg::Choose(Action::Delete));
        a.update(Msg::Activate(3)); // notes.txt — selects it, and asks the shell to open it
        let _ = a.take_open();
        assert_eq!(a.confirming(), Some("a.txt"), "the question still names what it asked about");
        a.update(Msg::ConfirmDelete);
        assert_eq!(
            a.take_op(),
            Some(FileOp::Delete { path: "/home/a.txt".into(), dir: false }),
            "and it removes that, not whatever is selected now"
        );
    }

    #[test]
    fn a_rename_answered_after_the_selection_moves_renames_what_was_chosen() {
        // Same shape in the prompt, where it is worse: the prompt *replaces* the path in the
        // strip, so after a navigation nothing on screen says where the name is about to land
        // (PR #268 review, blocking 3).
        let mut a = app();
        select(&mut a, 2); // a.txt
        a.update(Msg::Choose(Action::Rename));
        a.update(Msg::Activate(3)); // notes.txt
        let _ = a.take_open();
        press_key(&mut a, KEY_X);
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(
            a.take_op(),
            Some(FileOp::Rename { from: "/home/a.txt".into(), to: "/home/x".into() }),
            "the entry the menu row was pressed on, not the one selected now"
        );

        // And a navigation drops the prompt outright, so the mode cannot outlive its directory.
        let mut a = app();
        select(&mut a, 2);
        a.update(Msg::Choose(Action::Rename));
        a.show("/home/work", alloc::vec![Entry::file("payroll.txt")]);
        let ui: Element<Msg> = a.view(&UiTheme::default(), None);
        assert_eq!(labelled(&ui, PATH_KEY), "/home/work", "the path is back");
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert!(a.take_op().is_none(), "and Enter renames nothing");
    }

    #[test]
    fn copying_a_folder_is_a_tree_copy() {
        // `copy_file` on a directory merely fails, and `Action::Copy` does not exclude one —
        // so the operation has to carry which it is (PR #268 review, optional 3).
        let mut a = app();
        select(&mut a, 0); // archive/
        a.update(Msg::Choose(Action::Copy));
        press_key(&mut a, KEY_X);
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(
            a.take_op(),
            Some(FileOp::Copy {
                from: "/home/archive".into(),
                to: "/home/x".into(),
                dir: true
            })
        );
    }

    #[test]
    fn tabs_hold_their_own_directory_selection_and_scroll() {
        // **What a person expects to survive switching**, and the reason `Pane` exists: a tab
        // that came back at the top of a different folder would lose their place every time they
        // glanced at another one.
        let mut a = app();
        select(&mut a, 2); // a.txt
        assert_eq!(a.list_selected(), Some(2));

        a.update(Msg::NewTab);
        assert_eq!(a.tab_count(), 2);
        // A new tab opens where you are, and asks for its own listing.
        assert_eq!(a.take_goto().as_deref(), Some("/home"));
        a.show("/home/work", alloc::vec![Entry::file("payroll.txt")]);
        assert_eq!(a.path(), "/home/work");
        assert_eq!(a.list_selected(), Some(0));

        // And the first tab is exactly as it was left.
        let first = a.tabs()[0].0;
        a.update(Msg::SelectTab(first));
        assert_eq!(a.path(), "/home");
        assert_eq!(a.list_selected(), Some(2), "its own selection");
        assert_eq!(a.entries().len(), 4, "and its own listing");
    }

    #[test]
    fn a_tabs_key_can_never_be_a_row_index() {
        // **One namespace, two things numbering into it.** `Router::hovered_key` reports the
        // nearest keyed ancestor across the whole window, and this browser keys its list rows by
        // index — so a base merely *far* from the chrome's keys still collides in a big enough
        // directory, and hovering row `n` would draw a tab hovered (PR #270 review, optional 6).
        let mut a = app();
        for _ in 0..4 {
            a.update(Msg::NewTab);
            let _ = a.take_goto();
        }
        for (key, _) in a.tabs() {
            assert!(key >= TAB_KEY_BASE, "every tab is above the base");
        }
        // A row index is a `usize` that counts entries; it cannot reach the high bit, so the two
        // ranges are disjoint by construction rather than by being far apart.
        assert_eq!(TAB_KEY_BASE, 1 << 63);
    }

    #[test]
    fn a_browser_tab_closes_without_asking_and_the_last_one_closes_the_window() {
        // **Nothing to lose**: a listing is a view of the filesystem rather than unsaved work,
        // so there is no question to ask — which is the difference between this and the
        // editor's tabs, and worth a test because the two look alike.
        let mut a = app();
        a.update(Msg::NewTab);
        let _ = a.take_goto();
        assert_eq!(a.tab_count(), 2);
        let second = a.tabs()[1].0;

        a.update(Msg::CloseTab(second));
        assert_eq!(a.tab_count(), 1);
        assert!(a.confirming().is_none(), "a pane asks nothing");
        assert!(!a.closing());

        a.update(Msg::CloseTab(a.tabs()[0].0));
        assert!(a.closing(), "the last tab takes the window with it");
    }

    #[test]
    fn a_tab_is_labelled_by_the_folder_it_shows() {
        let mut a = app();
        assert_eq!(a.tabs()[0].1, "home");
        a.show("/", alloc::vec![Entry::dir("home")]);
        assert_eq!(a.tabs()[0].1, "/", "the root has no last component");
    }

    #[test]
    fn the_tab_chords_are_the_ones_the_keymap_names() {
        assert_eq!(libinput::keymap::to_char(NEW_TAB_KEYCODE, 0), Some(b't'));
        assert_eq!(libinput::keymap::to_char(CLOSE_TAB_KEYCODE, 0), Some(b'w'));
    }

    #[test]
    fn a_tab_chord_while_naming_still_opens_a_tab() {
        // **The chords are checked before the prompt**, or the field's branch returns first and
        // the chord does nothing at all while a name is being typed.
        //
        // The name is asserted unchanged as well, and that half is the *field's* doing rather
        // than the ordering's: `apply` drops control characters. Kept because it says what the
        // whole gesture leaves behind, and labelled so nobody reads it as what the ordering
        // buys (PR #270 review, worth fixing 5).
        let mut a = app();
        a.update(Msg::Choose(Action::NewFolder));
        press_key(&mut a, KEY_X);
        assert_eq!(a.prompt_len(), Some(1));

        a.update(Msg::Key(KeyEvent::new(1, NEW_TAB_KEYCODE, KEY_DOWN, MOD_CTRL)));
        assert_eq!(a.tab_count(), 2, "the chord opened a tab");
        assert_eq!(a.prompt_len(), Some(1), "and typed nothing into the name");
    }

    #[test]
    fn up_leaves_a_directory_and_stops_at_the_root() {
        let mut a = App::new("/home/work");
        a.update(Msg::Up);
        assert_eq!(a.take_goto().as_deref(), Some("/home"));

        let mut a = App::new("/home");
        a.update(Msg::Up);
        assert_eq!(a.take_goto().as_deref(), Some("/"));

        // **The root is its own parent**, so this asks for nothing rather than erroring — which
        // is why the control is never disabled: a greyed-out state is one more thing to draw.
        let mut a = App::new("/");
        a.update(Msg::Up);
        assert_eq!(a.take_goto(), None);
    }

    #[test]
    fn the_keyboard_and_the_pointer_reach_the_same_three_messages() {
        // A browser where Enter and a row press disagree about what "open" means is two
        // browsers. Down-Down-Enter must land where pressing the third row lands.
        let mut a = app();
        for _ in 0..2 {
            a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_DOWN, KEY_DOWN, 0)));
        }
        a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_ENTER, KEY_DOWN, 0)));
        // Rows are [archive/, work/, a.txt, notes.txt]; two downs from row 0 is `a.txt`.
        assert_eq!(a.take_goto(), None, "the third row is a file");

        let mut a = app();
        a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_DOWN, KEY_DOWN, 0)));
        a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_ENTER, KEY_DOWN, 0)));
        assert_eq!(a.take_goto().as_deref(), Some("/home/work"), "the second row is a directory");
    }

    #[test]
    fn backspace_goes_up() {
        let mut a = App::new("/home/work");
        a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_BACKSPACE, KEY_DOWN, 0)));
        assert_eq!(a.take_goto().as_deref(), Some("/home"));
    }

    #[test]
    fn a_new_listing_resets_the_selection_rather_than_keeping_an_index() {
        // A listing of a *different* directory has no row the old selection refers to.
        // `list_view` clamps a stale index rather than clearing it, so a browser that kept one
        // would silently select whatever happened to be at that position.
        //
        // **Two presses, not one**, and the reason is the whole point of a control: with the
        // reset deleted the selection is whatever it was, and after *one* press that is `Some(0)`
        // — the same value the correct code produces. The test passed for both implementations
        // until the control was run.
        let mut a = app();
        for _ in 0..2 {
            a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_DOWN, KEY_DOWN, 0)));
        }
        assert_eq!(a.list_selected(), Some(2), "precondition: the selection has moved");
        a.show("/home/work", alloc::vec![Entry::file("only.txt")]);
        assert_eq!(a.list_selected(), Some(0));
    }

    #[test]
    fn an_empty_directory_selects_nothing() {
        // Something must be selected first, or this passes for a browser that never selects
        // anything at all — the same trap the test above fell into.
        let mut a = app();
        a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_DOWN, KEY_DOWN, 0)));
        assert!(a.list_selected().is_some(), "precondition: something is selected");
        a.show("/home/empty", Vec::new());
        assert_eq!(a.list_selected(), None, "nothing to select, and no phantom row 0");
        assert!(a.entries().is_empty());
    }

    #[test]
    fn a_configure_is_accepted_and_the_listing_gets_the_room() {
        // A browser has no grid to refit: more height is more visible rows, which `list_view`
        // derives from what it is given.
        let mut a = app();
        let before = a.list_h();
        assert!(a.resize(Size::new(800, 600)));
        assert!(a.list_h() > before);
        assert!(!a.resize(Size::new(800, 600)), "the same size is not a change");
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

    /// The bar words are where `check-login` aims, which is a hardcoded number in a gate.
    ///
    /// **A gate cannot link this crate**, so it spells the browser's chrome metrics as constants
    /// and aims at `(fx + 20, fy + 1 + TITLE_BAR_H + MENU_BAR_H / 2)` for the File menu. Nothing
    /// connected that number to the layout until now — and M14 Part A changed what a bar word
    /// *is*, from a `button` to a `menu_item` with different padding. If the word ever moves out
    /// from under that point the gate fails in QEMU with no explanation; here it fails in a
    /// second and says which number is wrong.
    #[test]
    fn the_gate_aims_inside_the_file_word() {
        let mut a = app();
        let cell = libui::layout::FixedCell { w: 8, h: 16 };
        let size = a.window_size();
        let view = a.view(&UiTheme::default(), None);
        let l = libui::layout::layout(&view, Rect::new(0, 0, size.w, size.h), &cell);
        let file =
            libui::layout::locate(&view, &l, MENU_BAR_KEY).expect("the File word is keyed");
        let edit =
            libui::layout::locate(&view, &l, MENU_BAR_KEY + 1).expect("the Edit word is keyed");

        // The gate's own arithmetic, in window-local coordinates.
        let (ax, ay) = (20, 1 + TITLE_BAR_H as i32 + MENU_BAR_H as i32 / 2);
        assert!(
            file.contains(ax, ay),
            "check-login aims at ({ax}, {ay}) for the File menu and the word is at {file:?}"
        );
        // …and it is not so close to Edit that a pixel of drift chooses the other menu.
        assert!(
            !edit.contains(ax, ay),
            "the aim point is inside Edit as well: File {file:?}, Edit {edit:?}"
        );
    }

    /// A click on the menu bar opens the menu even when its two halves fall in different frames.
    ///
    /// **This is `click-not-acted-on` as the browser experiences it** — the intermittent
    /// fault that failed PR #280's CI, where the compositor delivered a press *and* a release to
    /// this window at the File word's coordinates and no menu opened.
    ///
    /// The two halves land in different frames whenever the click is the one that *raises* the
    /// window, because the raise costs a recompose in between; that is why it looked random and
    /// why it hit KVM more often than TCG. The frame in between is drawn with the word
    /// **hovered** — the press is what establishes hover — and a hovered `menu_item` has three
    /// layers where a resting one has one, so every node inside it is re-identified. The fix is
    /// in `libui`: `Router` captures the node carrying the handler, which is the word itself and
    /// survives. This is the application-level proof that the path a person clicks is repaired.
    #[test]
    fn a_click_split_across_a_repaint_still_opens_the_menu() {
        let mut a = app();
        let cell = libui::layout::FixedCell { w: 8, h: 16 };
        let size = a.window_size();
        let theme = UiTheme::default();
        let mut tree = libui::diff::Tree::new();
        let mut router = libui::route::Router::new();

        // Frame 1: nothing hovered, because hover is what the press is about to establish.
        let rest = a.view(&theme, None);
        let l = libui::layout::layout(&rest, Rect::new(0, 0, size.w, size.h), &cell);
        tree.update(&rest, &l).expect("the view is diffable");
        let word = libui::layout::locate(&rest, &l, MENU_BAR_KEY).expect("the File word is keyed");
        let at = (word.origin.x + word.size.w as i32 / 2, word.origin.y + word.size.h as i32 / 2);

        let ev = |flags: u16, buttons: u16| librsproto::surface::PointerEvent {
            kind: librsproto::surface::POINTER_BUTTON,
            button: 0x110,
            buttons,
            flags,
            x: at.0,
            y: at.1,
            ..Default::default()
        };
        let (msgs, _) =
            router.pointer(&tree, &rest, &l, ev(librsproto::surface::POINTER_PRESSED, 1));
        assert!(msgs.is_empty(), "a press is not a click");

        // Frame 2: a repaint between the two halves, with the word lit. **Lit explicitly, not
        // via `hovered_key`** — crossings are suppressed while a capture is in force, so the
        // press does not itself set `inside`, and a frame built from `hovered_key` here would be
        // identical to frame 1 and the test would pass against the bug. It did, until the
        // negative control caught it. What matters is only that *a* repaint between the halves
        // re-identifies the nodes inside the word, which is what a highlight does.
        let lit = a.view(&theme, Some(MENU_BAR_KEY));
        let l2 = libui::layout::layout(&lit, Rect::new(0, 0, size.w, size.h), &cell);
        tree.update(&lit, &l2).expect("the view is diffable");

        // The release, in that new frame.
        let (msgs, _) = router.pointer(&tree, &lit, &l2, ev(0, 0));
        for m in msgs {
            a.update(m);
        }
        assert_eq!(
            a.menus.open(),
            Some(0),
            "the click was dropped because the repaint between its halves re-identified the word"
        );
    }

}
