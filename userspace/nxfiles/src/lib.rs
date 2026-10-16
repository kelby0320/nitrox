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

use alloc::vec;
use libdraw::geom::{Rect, Size};
use librsproto::file::{DIRENT_KIND_DIR, OwnedEntry};
use librsproto::surface::{
    KEY_DOWN, KEY_REPEAT, KeyEvent, MOD_CTRL, MOD_SHIFT, PointerEvent, RESIZE_BOTTOM, RESIZE_RIGHT,
    WINDOW_STATE_MAXIMIZED, WINDOW_STATE_MINIMIZED, WINDOW_STATE_NORMAL,
};
use libui::element::{
    Edge, Element, Insets, center_v, column, dock, docked, ink, padding, row, sized, text, with_spacing,
};
use libui::menu::{Accel, Item, Menu, MenuState};
use libui::widget::{
    ColumnAlign, DIALOG_GAP, ListColumn, ListRow, ListState, RowButton, STATUS_BAR_H, Swatch, TAB_STRIP_H, TITLE_BAR_H,
    TabExtras, TextFieldState, Theme as UiTheme, TitleButtons, WINDOW_FRAME_H, WidgetState, button, dialog_frame,
    list_view, popup_frame, resize_grip, status_bar, status_separator, status_text, tab_strip, text_field, title_bar,
    window_frame_with_grip,
};

mod keys;
mod model;
mod msg;
mod popups;
mod text;

pub use keys::*;
pub use model::*;
pub use msg::*;
pub use text::*;

/// What this window is called, in its own title bar and in the shell's window list.
pub const TITLE: &str = "Files";

/// The path strip's height in pixels — one row of chrome under the title bar.
pub const PATH_H: u32 = 24;

/// The listing's trailing columns: the design's `Size`, `Kind` and `Modified` at 70, 60 and 96,
/// with a size read right-aligned so the digits line up (desktop refresh, Part I).
pub const LIST_COLUMNS: &[ListColumn] = &[
    ListColumn { width: 70, align: ColumnAlign::Right },
    ListColumn { width: 60, align: ColumnAlign::Left },
    ListColumn { width: 96, align: ColumnAlign::Left },
];

/// What each of those columns is called, for the header above them.
pub const LIST_HEADINGS: [&str; 3] = ["Size", "Kind", "Modified"];

/// The column header's height in pixels — the toolkit's, which draws it.
pub const HEADER_H: u32 = libui::widget::LIST_HEADER_H;

/// How wide the search field is: the design's 120.
pub const SEARCH_W: u32 = 120;

/// A listing row's height in pixels: the design's 25, where it was 20.
pub const ROW_H: u32 = 25;


/// How far the pointer must travel with a button down before a press becomes a drag, in pixels.
///
/// **Because a click is a press that moved a little.** Nobody presses a mouse button without
/// nudging it, and a browser that started a drag on the first pixel would make opening a file by
/// clicking it a matter of luck. Four is the smallest number that survives a hand; a person
/// deliberately dragging crosses it immediately.
pub const DRAG_SLOP: i32 = 4;

/// The menu bar's height in pixels — one row of chrome above the path strip.
pub const MENU_BAR_H: u32 = 24;


/// How wide the sidebar of common locations is, in pixels.
///
/// **A fixed metric, like every other piece of chrome here** (M11's decision 2): the gates aim at
/// its rows, and a width that followed the theme's text size would move under a gate that had to
/// read a theme file to know where to click.
pub const SIDEBAR_W: u32 = 132;

/// How much space sits between the quick-access panel and everything around it: none.
///
/// **A margin until the desktop refresh's Part I, and an edge since.** M15 Part E put six pixels
/// around the panel so that the window showing through would make it read as a *panel* rather
/// than as part of the listing — reported from running it, "add some margin around the quick
/// access panel just to separate it from the rest". The design answers that differently: the
/// sidebar runs to the window's edge and its own ground is what separates it, which is what this
/// is now. **Kept as a named zero** rather than deleted, because the sums that place a sidebar
/// row read it and a gate writes it down (PR #320 review, worth fixing 8).
pub const SIDEBAR_PAD: u32 = 0;


/// The Drives heading's height: a row's, so a drive's row is a whole number of rows down.
pub const DRIVES_HEADING_H: u32 = ROW_H;
/// What an eject button shows: the eject symbol, which DejaVu Sans carries.
pub const EJECT_GLYPH: &str = "\u{23CF}";

/// The place a sidebar key names, or `None` if it is not a sidebar row's.
fn place_of(key: u64) -> Option<usize> {
    key.checked_sub(SIDEBAR_ROW_KEY).map(|i| i as usize)
}




/// How wide that dialog is — wider than a question, because it shows paths.
pub const PROPS_W: u32 = 420;
/// How tall: a title, five rows of text, and a button strip.
pub const PROPS_H: u32 = 236;

/// Everything the browser is.
pub struct App {
    /// The open panes, in the order their tabs are drawn. **Never empty**, for the reason
    /// `nxedit`'s buffers are not: closing the last tab closes the window.
    panes: Vec<Pane>,
    /// Which pane's tab is current, by [`Pane::key`].
    current: u64,
    /// Paths the binary is asked to push onto the clipboard, and what should happen to them.
    ///
    /// **Recorded rather than done**, like every other syscall this crate cannot make.
    clip_push: Option<(librsproto::clipboard::PathVerb, Vec<String>)>,
    /// The binary is asked to read the clipboard and hand back what is on it.
    clip_read: bool,
    /// The entries picked *besides* the one the list state holds, by name.
    ///
    /// **Names rather than indices**, because indices do not survive a re-sort: the View menu can
    /// reorder the listing under a selection, and a set of positions would then name different
    /// files. Names are what the selection means to a person, and `show` clears them anyway when
    /// the listing changes directory.
    marked: Vec<String>,
    /// Where a range selection counts from — the last row picked without Shift.
    anchor: Option<usize>,
    /// This session's home, which the sidebar's first row goes to.
    ///
    /// **What the browser was started at**, which `main` reads from the environment — the same
    /// value `desktop-shell` hands every application. A browser that guessed `/home` would be
    /// wrong for anybody whose home is not there.
    home: String,
    /// The sidebar's scroll and highlight. Its *selection* is derived from where the tab is
    /// rather than remembered, so the two cannot disagree.
    sidebar: ListState,
    /// **The drives**, listed below the places (Phase 6 Part F): as the binary last read the
    /// storage service's table, which it reads again whenever its watch says the mounts changed.
    drives: Vec<Drive>,
    /// The drives' scroll and highlight; its selection derived as the places' is.
    drive_list: ListState,
    /// A drive the binary owes an `Eject` for, by the name it is mounted under.
    eject: Option<String>,
    /// The mounts an eject took — the one asked for until the service answers, then every one it
    /// unmounted on that stick — so that their going is said as an eject and not as a stick
    /// pulled out.
    ejecting: Vec<String>,
    /// What an eject that took several mounts said, to be said again as they leave the table.
    eject_said: Option<String>,
    /// What the strip says once the listing that a drive's going asked for arrives — which
    /// clears the notice, as every listing does.
    arrival_notice: Option<String>,
    /// The entry whose properties are being shown, and the directory it was in.
    ///
    /// **A snapshot taken when the row was chosen**, for [`Target`]'s reason: the listing and the
    /// selection both move while a dialog is up, so a dialog that read them late would describe
    /// whatever now sits at that position.
    properties: Option<(String, Entry)>,
    /// The location bar's field, while it is open.
    ///
    /// **On the window rather than the pane**, because only one can be open at a time and it acts
    /// on whichever tab is current — the same reason the name prompt is not per pane.
    location: Option<TextFieldState>,
    /// The search field, when it is open — the toolbar's, which **filters the listing as it is
    /// typed** (desktop refresh, Part I).
    ///
    /// **A filter, not a placeholder.** The design draws a Search field and does nothing with it;
    /// a field that looked like search and did nothing is the defect every part of this refresh
    /// has named, so this narrows the rows by the same rule the Applications menu uses.
    search: Option<TextFieldState>,
    /// Counts runs of pointer presses, so a second click on a row can mean something else.
    ///
    /// **Fed by the binary**, which is the only half that can read a clock — see
    /// [`App::note_press`] and `libui::click`.
    clicks: libui::click::Clicks,
    /// Where within the thumb the scrollbar was taken hold of — see
    /// [`ScrollGrab`](libui::widget::ScrollGrab).
    ///
    /// **One per window rather than one per pane**, because a grab is: one button, one drag.
    scroll_grab: libui::widget::ScrollGrab,
    /// What number the press being routed now is in its run; `1` unless a run is under way.
    click_run: u32,
    /// The modifiers held at that press — what makes Ctrl-click and Shift-click expressible.
    click_mods: u16,
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
    /// Another window has been asked for, and the binary has not made it yet.
    new_window: bool,
    /// A quit has been asked for. **The binary owns what that means**, because it is the only
    /// thing that knows how many windows there are.
    quit: bool,
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
    pub dialog_focused: bool,
    /// What the wall clock said when this listing was read, in nanoseconds since the epoch.
    ///
    /// **The binary reads it and the view is a function of it** (desktop refresh, Part I): the
    /// `Modified` column says `HH:MM` for today and a date before that, which needs to know what
    /// today is — and a `view` that called the clock would be a view that could not be tested on
    /// the host. Zero means "not read", and then every row shows a date.
    pub now_nanos: u64,
    /// The dialog's title bar was dragged, and the binary owes a `StartMove` **on its window**.
    confirm_move_requested: bool,
    /// Filesystem work the binary owes, in the order it was asked for.
    ///
    /// **A queue since M14 Part D**, because a paste of four files is four operations that can
    /// each fail on their own — a single slot would have made the last one silently replace the
    /// three before it.
    ops: Vec<FileOp>,
    /// The row an internal drag is over, while one is running.
    ///
    /// **Internal**: a drag that has passed the slop but has not left this window. The payload
    /// never reaches the compositor while that is true, which is the whole of what "drag and
    /// drop within a window" means (M10 named it as M12's).
    over: Option<usize>,
    /// Whether an internal drag is running: past the slop, and not yet out of this window.
    dragging: bool,
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
                order: libfs::Order::default(),
                show_hidden: false,
                relist: false,
            }],
            current: TAB_KEY_BASE,
            clip_push: None,
            clip_read: false,
            marked: Vec::new(),
            anchor: None,
            home: String::from(path),
            sidebar: ListState::default(),
            drives: Vec::new(),
            drive_list: ListState::default(),
            eject: None,
            ejecting: Vec::new(),
            eject_said: None,
            arrival_notice: None,
            properties: None,
            location: None,
            search: None,
            clicks: libui::click::Clicks::new(),
            scroll_grab: libui::widget::ScrollGrab::new(),
            click_run: 1,
            click_mods: 0,
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
            new_window: false,
            quit: false,
            // **Zero, because the length is set every frame.** `set_anchors` replaces this
            // vector before anything reads it, and sizing it here from a constant is
            // what drifted (M15 Part E).
            menus: MenuState::new(0),
            prompt: None,
            confirm: None,
            dialog_focused: true,
            now_nanos: 0,
            confirm_move_requested: false,
            ops: Vec::new(),
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

    /// The entry a row position names, if that row is drawn.
    ///
    /// **`ListState::selected` is a row position and this browser's index is an entries index**,
    /// and until a filter existed they were the same number. They are not while a search narrows
    /// the listing, and the widget clamps what it is given against the rows it drew — so a
    /// stored index of 2 became row 0, and the file the screen highlighted stopped being the
    /// file `Delete` acted on (PR #320 review, blocking 1).
    ///
    /// The pane keeps the **entries index**, because every reader in this file means that. This
    /// and the translation around `list_view` in `view` are the only places the two spaces meet.
    fn entry_of_row(&self, row: usize) -> Option<usize> {
        self.visible_indices().get(row).copied()
    }

    /// Move the selection one row down (`1`) or up (`-1`), **through the rows that are shown**.
    ///
    /// **One implementation for both key paths**: the listing's arrows and the search field's
    /// were separate branches, and the filtered one moved a row position into a field that holds
    /// an entries index (PR #320 review, blocking 1).
    fn step_selection(&mut self, by: i32) {
        let shown = self.visible_indices();
        if shown.is_empty() {
            return;
        }
        let here = self.pane().list.selected.and_then(|e| shown.iter().position(|&i| i == e));
        let next = match (here, by) {
            (Some(p), 1) => (p + 1).min(shown.len() - 1),
            (Some(p), _) => p.saturating_sub(1),
            // Nothing picked yet: down takes the first row, up takes the last.
            (None, 1) => 0,
            (None, _) => shown.len() - 1,
        };
        self.pane_mut().list.selected = Some(shown[next]);
    }

    /// Which entries the listing shows, as indices into [`entries`](Self::entries).
    ///
    /// **Indices rather than entries**, because a row's key is `LIST_ROW_KEY + its index` and
    /// everything downstream — opening, dragging, renaming, the marked set — resolves through
    /// that. A filter that renumbered the rows would quietly make every one of those act on the
    /// wrong file (desktop refresh, Part I).
    ///
    /// **Case-insensitive, and a substring rather than a prefix**: the Applications menu's rule,
    /// because a person searching a directory for `notes` should not have to know whether the
    /// file begins with it.
    pub fn visible_indices(&self) -> Vec<usize> {
        let query = self.search.as_ref().map(|f| f.text().to_lowercase()).unwrap_or_default();
        self.pane()
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| query.is_empty() || e.name.to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect()
    }

    /// What the search field holds, if it is open — for the binary's receipts and for tests.
    pub fn search_text(&self) -> Option<&str> {
        self.search.as_ref().map(|f| f.text())
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
        // **Directories first, then by name — and the comparison is `libfs`'s, not a copy of it.**
        // It was a copy until PR #284's review: the same rule spelled a second time over this
        // crate's own `Entry`, with a comment saying so, which is precisely the divergence
        // `libfs::sort` exists to prevent. Make `NameAsc` case-insensitive and a browser and a
        // chooser would have shown one directory in two orders.
        let p = self.pane_mut();
        // **Hidden entries are dropped here, not at draw time.** A row's key is its index into
        // `entries`, and so is `ListState::selected` — so filtering a view built from a longer
        // list would mean two numberings for one row, which is how a selection comes to name the
        // wrong file. What the tab holds is what the tab shows.
        if !p.show_hidden {
            entries.retain(|e| !e.name.starts_with('.'));
        }
        libfs::sort(&mut entries, p.order);
        p.path = String::from(path);
        p.entries = entries;
        p.list = ListState::at((!p.entries.is_empty()).then_some(0), 0);
        // **A listing of another directory has nothing the old picks refer to.** Same argument as
        // the selection reset above it.
        self.marked.clear();
        self.anchor = None;
        // **And the click run ends here for the reason a drag ends it** (PR #285 review, optional
        // 8): the second click of a double click navigates, so a third press moments later would
        // be run 3 and open whatever is now row 0 — a file in a directory that appeared a tenth
        // of a second ago. Three fast clicks is a thing people do.
        self.clicks.reset();
        self.click_run = 1;
        // A listing supersedes whatever the last row press had to say about itself — unless what
        // asked for it was a drive going, which is said once the tab it moved has arrived.
        self.notice = self.arrival_notice.take();
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
        Some(Entry { name, is_dir: e.kind == DIRENT_KIND_DIR, mtime: e.mtime, size: e.size })
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
            // **A single click selects; the second one opens** (M14 decision 5). Selecting on
            // every press and opening only on the second is what makes the first click useful —
            // before this, clicking a file *opened* it, so there was no way to point at one
            // without acting on it, which is the half the maintainer reported as "clicking a file
            // opens the file rather than selecting it".
            // **The message carries the row's *key*** and this is the one place that knows the
            // numbering. A key below the base is not row zero — `checked_sub`, not saturating,
            // for the reason `nxedit`'s chooser learned: the saturating version makes a stray key
            // act on the first row and makes a test pass by accident.
            Msg::Press(key) => {
                let Some(i) = list_row_of(key) else { return };
                if i >= self.pane().entries.len() {
                    return;
                }
                self.notice = None;
                let mods = self.click_mods;
                // **Ctrl adds one, Shift adds a run, and a plain press replaces everything** —
                // the three every file manager has, and the reason the modifiers ride on the
                // press record rather than being tracked from the keyboard.
                if mods & MOD_CTRL != 0 {
                    self.toggle_mark(i);
                } else if mods & MOD_SHIFT != 0 {
                    self.mark_range_to(i);
                } else {
                    self.marked.clear();
                    self.anchor = Some(i);
                    self.pane_mut().list.selected = Some(i);
                    // **Only a plain press opens.** A second Ctrl-click is somebody building a
                    // selection, not asking for anything to happen — and opening a file
                    // mid-selection is exactly the accident decision 5 exists to prevent.
                    if self.click_run >= 2 {
                        self.update(Msg::Activate(i as u64));
                    }
                }
            }
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
            Msg::Grab(key) => {
                // Resolved here, while the listing this key came from is still the listing.
                self.pressed =
                    list_row_of(key).and_then(|i| self.pane().entries.get(i)).map(|e| (e.name.clone(), 0, 0));
            }
            // **The drag converts through the widget's own arithmetic** — `ListState::bar` and
            // `ScrollGrab`, the same pair `nxterm` uses for its grid — so a list and a terminal
            // cannot disagree about where a thumb points (M11 Part E batch 6), nor about what
            // taking hold of one means (M14 Part I).
            Msg::Scroll(p) => {
                let (h, total) = (self.list_h(), self.visible_indices().len());
                let bar = self.pane().list.bar(h, ROW_H, total);
                if let Some(offset) = self.scroll_grab.apply(bar, h, p) {
                    self.pane_mut().list.offset = offset as usize;
                }
            }
            // **The same conversion the terminal does, from the widget that owns it**
            // (M14 Part I). `list_view` clamps the offset against the rows it is given on the
            // next build, so this does not need the count — and a second clamp here is how a
            // list ends up unable to reach its last row.
            Msg::Wheel(p) => self.pane_mut().list.wheel(p.wheel),
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
            Msg::NewWindow => self.new_window = true,
            Msg::Quit => self.quit = true,
            Msg::Choose(a) => self.choose(a),
            Msg::SelectTab(k) => {
                if let Some(p) = self.panes.iter_mut().find(|p| p.key == k) {
                    self.current = k;
                    if core::mem::take(&mut p.relist) {
                        self.goto = Some(p.path.clone());
                    }
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
                    order: libfs::Order::default(),
                    show_hidden: false,
                    relist: false,
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
                    self.ops.push(FileOp::Delete { path, dir: t.is_dir });
                }
            }
            // **Says so**, because a dialog that vanishes with nothing changed is
            // indistinguishable from one that took the other answer.
            Msg::KeepIt => {
                if self.confirm.take().is_some() {
                    self.notice = Some(String::from("not deleted"));
                }
            }
            // **Seeded with where the tab is**, so the common edit is to the tail of a path
            // rather than to an empty field — and a person who opened it by accident sees where
            // they are rather than a blank.
            Msg::OpenLocation => {
                self.prompt = None;
                self.notice = None;
                self.search = None;
                self.location = Some(TextFieldState::with_text(&self.pane().path));
            }
            // **Opened empty**, where the location bar is seeded: a search that began with the
            // directory's name in it would start by matching nothing.
            Msg::OpenSearch => {
                self.prompt = None;
                self.notice = None;
                self.location = None;
                self.search = Some(TextFieldState::new());
            }
            Msg::SearchCancel => self.search = None,
            Msg::Place(key) => {
                if let Some(p) = place_of(key).and_then(|i| self.places().into_iter().nth(i)) {
                    self.goto = Some(p.path);
                }
            }
            // **The button before the row**, since its range is above the rows'. An eject is the
            // binary's to send; a removable drive's alone, which is the only kind drawn with a
            // button, and asked again here because a key is a number anything could send.
            Msg::Drive(key) => {
                if let Some(n) = key.checked_sub(DRIVE_EJECT_KEY) {
                    if let Some(d) = self.drives.iter().find(|d| d.block == n && d.removable) {
                        self.notice = Some(alloc::format!("ejecting {}\u{2026}", d.name));
                        self.eject = Some(d.name.clone());
                        self.ejecting = alloc::vec![d.name.clone()];
                    }
                } else if let Some(n) = key.checked_sub(DRIVE_ROW_KEY)
                    && let Some(d) = self.drives.iter().find(|d| d.block == n)
                {
                    self.goto = Some(d.path());
                }
            }
            Msg::CutFiles | Msg::CopyFiles => {
                let verb = if msg == Msg::CutFiles {
                    librsproto::clipboard::PathVerb::Cut
                } else {
                    librsproto::clipboard::PathVerb::Copy
                };
                // **Resolved to absolute paths now**, while this listing is still this listing —
                // the same rule `Target` follows. A path pasted later is read by whoever reads
                // it, and a relative one would name a file in *their* directory.
                let dir = self.pane().path.clone();
                let paths: Vec<String> = self.selection().iter().map(|e| join(&dir, &e.name)).collect();
                if paths.is_empty() {
                    self.notice = Some(String::from("nothing is selected"));
                    return;
                }
                let n = paths.len();
                self.clip_push = Some((verb, paths));
                self.notice = Some(alloc::format!(
                    "{} {n} {}",
                    if verb == librsproto::clipboard::PathVerb::Cut { "cut" } else { "copied" },
                    if n == 1 { "entry" } else { "entries" }
                ));
            }
            Msg::PasteFiles => self.clip_read = true,
            Msg::CloseProperties => self.properties = None,
            Msg::LocationCancel => {
                if self.location.take().is_some() {
                    // **Says so**, for the reason the delete question does: a field that vanishes
                    // with nothing changed is indistinguishable from one that acted.
                    self.notice = Some(String::from("cancelled"));
                }
            }
            Msg::LocationGo => {
                let Some(f) = self.location.as_ref() else { return };
                let typed = f.text().trim().to_string();
                if typed.is_empty() {
                    self.notice = Some(String::from("a path, then Enter"));
                    return;
                }
                // **A relative path is joined to where the tab is**, which is what a location bar
                // is for — typing `papers` from `/home` should go where typing it into a shell
                // would. An absolute one replaces it outright.
                let to = if typed.starts_with('/') { typed } else { join(&self.pane().path, &typed) };
                self.location = None;
                self.goto = Some(to);
            }
            // **Re-sorted in place**, and the selection follows the *file* rather than the row:
            // a person watching a name they picked jump to another line as the order changes
            // would reasonably think the browser had selected something else.
            Msg::SetOrder(order) => {
                let p = self.pane_mut();
                if p.order != order {
                    p.order = order;
                    let picked = p.list.selected.and_then(|i| p.entries.get(i)).map(|e| e.name.clone());
                    libfs::sort(&mut p.entries, order);
                    p.list.selected = match picked {
                        Some(name) => p.entries.iter().position(|e| e.name == name),
                        None => None,
                    };
                    p.list.offset = 0;
                    // **The anchor follows too** (PR #285 review, optional 7). It is an index
                    // like the selection, so leaving it behind means a later Shift-click extends
                    // from wherever that row now sits — a range measured from a file the person
                    // is not looking at.
                    self.anchor = self.pane().list.selected;
                }
            }
            // **A listing, because the hidden ones are not held.** See [`Msg::ToggleHidden`].
            Msg::ToggleHidden => {
                let p = self.pane_mut();
                p.show_hidden = !p.show_hidden;
                self.goto = Some(self.pane().path.clone());
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
        let selected = self.pane().list.selected.and_then(|i| self.pane().entries.get(i)).cloned();
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
            // **The whole entry, not the `Target`.** Size and modification time are what this
            // dialog is for and `Target` carries neither; it is the path-resolution record.
            Action::Properties => {
                self.properties = selected.clone().map(|e| (self.pane().path.clone(), e));
            }
            // The field starts empty rather than pre-filled with the current name. Pre-filling
            // would need a selection and a caret to be useful — a person would have to clear it
            // before typing — and the field has neither yet.
            //
            // **And the location bar closes, which is the other half of a rule that was only
            // half true** (PR #285 review, blocking 1). `key` checks the bar first and `view`
            // draws the prompt first, so with both open the screen showed one field and the
            // keyboard fed the other: every character went into a bar nobody could see, and
            // Enter navigated somewhere instead of creating the file.
            _ => {
                self.location = None;
                self.prompt = Some((a, target, TextFieldState::new()));
            }
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
        let queued = match a {
            Action::NewFile => Some(FileOp::Create { path: to, dir: false }),
            Action::NewFolder => Some(FileOp::Create { path: to, dir: true }),
            Action::Rename => from.map(|from| FileOp::Rename { from, to }),
            Action::Duplicate => from.map(|from| FileOp::Copy { from, to, dir: target.is_dir }),
            // `choose` sends these two to a dialog and never to a prompt, so this arm is
            // unreachable rather than a decision — and saying `None` keeps it that way.
            Action::Delete | Action::Properties => None,
        };
        self.ops.extend(queued);
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
        self.ops.push(FileOp::MoveInto { from, to: join(&dir, carried) });
    }

    /// Where the list's first row starts, in window coordinates.
    ///
    /// **One place, because two would disagree.** [`list_h`](Self::list_h) subtracts the chrome
    /// above the list and an internal drag has to turn a `y` back into a row; deriving that
    /// arithmetic twice is how a drop lands one row off the thing it was released over.
    pub fn list_top(&self) -> u32 {
        // **The same sum `list_h` subtracts, and it stopped being that** (PR #320 review,
        // blocking 3): Part I made the tab strip conditional and added a column header, and this
        // — the function whose whole reason is that "two would disagree" — kept the old chrome.
        // A drop then resolved to the row above the one it was released over, and with two tabs
        // to the row below; the drag tests aim *through* this function, so they agreed with it.
        libui::widget::WINDOW_CONTENT_Y + TITLE_BAR_H + MENU_BAR_H + self.tab_strip_h() + PATH_H + HEADER_H
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
        let row = self.pane().list.offset + ((y - top) as u32 / ROW_H) as usize;
        // **Through the rows that are drawn**, which is not the entries list while a search is
        // narrowing it: a `y` names a row, and the caller wants the entry under it.
        self.entry_of_row(row)
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
        // **And while a search is being typed they are the field's**, for the location bar's
        // reason: `Backspace` correcting a query must not also go up a directory. `Enter` opens
        // what is left, which is what a filter that has narrowed to one thing is for.
        if self.search.is_some() {
            match k.keycode {
                libkern::abi::KEY_ESC => self.update(Msg::SearchCancel),
                // **`Activate` takes an entries index**, which is what `Press` and the plain
                // `Enter` below send it; this sent a *key* and so opened nothing at all
                // (PR #320 review, blocking 2). And it opens **what is selected**, falling back
                // to the first match — arrows move the selection while a search is up, so
                // opening the first row would ignore where the person had arrowed to.
                libkern::abi::KEY_ENTER => {
                    let pick = self
                        .pane()
                        .list
                        .selected
                        .filter(|e| self.visible_indices().contains(e))
                        .or_else(|| self.visible_indices().first().copied());
                    if let Some(i) = pick {
                        self.search = None;
                        self.update(Msg::Activate(i as u64));
                    }
                }
                // Arrows still move the selection: a filter narrows the list, it does not take
                // the list's keys away — and they move it **through what is shown**, which is
                // what keeps the highlight and the file an action acts on the same file.
                libkern::abi::KEY_DOWN => self.step_selection(1),
                libkern::abi::KEY_UP => self.step_selection(-1),
                _ => {
                    if let Some(f) = self.search.as_mut() {
                        f.apply(k.keycode, k.modifiers, k.locks);
                    }
                }
            }
            return;
        }
        // **And while a path is being typed they are the location bar's**, for exactly the
        // reason above: `Backspace` correcting a typo must not also go up a directory. Before the
        // prompt check because the two are mutually exclusive — opening either closes the other —
        // so the order between them only decides which branch answers when neither is open.
        if self.location.is_some() {
            match k.keycode {
                libkern::abi::KEY_ESC => self.update(Msg::LocationCancel),
                libkern::abi::KEY_ENTER => self.update(Msg::LocationGo),
                code => {
                    if let Some(f) = self.location.as_mut() {
                        f.apply(code, k.modifiers, k.locks);
                    }
                }
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
                        f.apply(code, k.modifiers, k.locks);
                    }
                }
            }
            return;
        }
        match k.keycode {
            libkern::abi::KEY_DOWN => self.step_selection(1),
            libkern::abi::KEY_UP => self.step_selection(-1),
            libkern::abi::KEY_ENTER => {
                if let Some(i) = self.pane().list.selected {
                    self.update(Msg::Activate(i as u64));
                }
            }
            libkern::abi::KEY_BACKSPACE => self.update(Msg::Up),
            _ => {}
        }
    }

    /// Record a pointer press at `at`, `at_ms` milliseconds into the monotonic clock.
    ///
    /// **Called before the event is routed**, so the message the press produces can ask what
    /// number the click was. The binary owns the clock because this crate makes no syscalls, and
    /// `libui::click` owns the counting because every list wants it — see decision 5.
    pub fn note_press(&mut self, at: libdraw::geom::Point, at_ms: u64, modifiers: u16) {
        self.click_run = self.clicks.press(at, at_ms);
        // **Held at the press, and carried rather than re-derived.** `PointerEvent` reports them
        // on every record precisely so a client does not track them from `KeyEvent`s — which it
        // could only get right while it also held keyboard focus, so a Ctrl-click on an unfocused
        // window would behave as a plain one (the note on `PointerEvent::modifiers`).
        self.click_mods = modifiers;
    }

    /// Abandon the click run — the gesture turned out to be a drag.
    ///
    /// **Or the next click after a drag opens something.** A drag begins with a press the tracker
    /// has already counted; without this the click that follows would be number two of a run the
    /// person never meant to start.
    pub fn note_drag(&mut self) {
        self.clicks.reset();
        self.click_run = 1;
    }

    /// The listing the binary was asked for could not be read.
    ///
    /// **Said out loud, because a typed path that does not exist is the ordinary mistake.** The
    /// browser used to leave the pane exactly as it was, so a location bar entry with a typo did
    /// nothing at all and looked like a key that had not registered.
    pub fn list_failed(&mut self, path: &str) {
        self.notice = Some(alloc::format!("no such directory: {path}"));
    }

    /// The selected row's name, if a row is selected.
    ///
    /// **For the binary's receipt**, which reports it on change: a single click now selects rather
    /// than opens, so what a click did has no outward sign unless something says so.
    pub fn picked_name(&self) -> Option<String> {
        let p = self.pane();
        p.list.selected.and_then(|i| p.entries.get(i)).map(|e| e.name.clone())
    }

    /// **The drives, as the storage service's table now lists them** (Phase 6 Part F). A drive
    /// that has gone takes every tab inside it home: the current one by a listing the binary
    /// owes, the rest when chosen. If it went because this window ejected it, that is what the
    /// strip says; otherwise that it is gone, when a tab was on it.
    pub fn set_drives(&mut self, drives: Vec<Drive>) {
        let gone: Vec<Drive> =
            self.drives.iter().filter(|d| !drives.iter().any(|n| n.name == d.name)).cloned().collect();
        self.drives = drives;
        for d in gone {
            let at = d.path();
            let inside = |p: &str| p == at || p.strip_prefix(at.as_str()).is_some_and(|rest| rest.starts_with('/'));
            let (mut current, mut others) = (false, 0);
            for p in self.panes.iter_mut().filter(|p| inside(&p.path)) {
                p.path = self.home.clone();
                p.entries.clear();
                p.list = ListState::default();
                if p.key == self.current {
                    current = true;
                } else {
                    p.relist = true;
                    others += 1;
                }
            }
            let ejected = self.ejecting.iter().position(|n| *n == d.name).map(|i| self.ejecting.remove(i)).is_some();
            let said = match (ejected, current, others) {
                (true, ..) => {
                    self.eject_said.clone().unwrap_or_else(|| alloc::format!("{} ejected: it can be removed", d.name))
                }
                (false, true, _) => alloc::format!("{} is gone, so this tab went home", d.name),
                (false, false, 1) => alloc::format!("{} is gone, so a tab on it went home", d.name),
                (false, false, n) if n > 1 => alloc::format!("{} is gone, so the tabs on it went home", d.name),
                _ => continue,
            };
            if current {
                self.goto = Some(self.home.clone());
                self.arrival_notice = Some(said.clone());
            }
            self.notice = Some(said);
        }
    }

    /// The drives the sidebar lists.
    pub fn drives(&self) -> &[Drive] {
        &self.drives
    }

    /// The drive the binary owes an `Eject` for, by its mount name. Clears the record.
    pub fn take_eject(&mut self) -> Option<String> {
        self.eject.take()
    }

    /// What the storage service answered an `Eject` of `name`: the names of every mount it
    /// unmounted on that stick — the drive goes whole — or its reason. A drive ejected is said so
    /// here, and again when the table no longer lists it.
    pub fn ejected(&mut self, name: &str, answer: Result<&[String], &str>) {
        match answer {
            Ok(names) => {
                let said = match names {
                    [] => alloc::format!("{name} ejected: it can be removed"),
                    [one] => alloc::format!("{one} ejected: it can be removed"),
                    many => alloc::format!("{} ejected: the drive can be removed", many.join(", ")),
                };
                if !names.is_empty() {
                    self.ejecting = names.to_vec();
                }
                self.eject_said = Some(said.clone());
                self.notice = Some(said);
            }
            Err(why) => {
                self.ejecting.clear();
                self.eject_said = None;
                self.notice = Some(alloc::format!("{name} not ejected: {why}"));
            }
        }
    }

    /// The common locations the sidebar offers — `libfs::places` for this window's home.
    ///
    /// **The shell's Places menu asks the same function** (desktop refresh, Part C), which is
    /// why the list is not here any more: two copies of it are two answers to what a person's
    /// places are.
    pub fn places(&self) -> Vec<libfs::Place> {
        libfs::places(&self.home)
    }

    /// Paths the binary owes a clipboard push for. Clears the record.
    pub fn take_clip_push(&mut self) -> Option<(librsproto::clipboard::PathVerb, Vec<String>)> {
        self.clip_push.take()
    }

    /// Whether the binary owes a clipboard *read*. Clears the record.
    pub fn take_clip_read(&mut self) -> bool {
        core::mem::take(&mut self.clip_read)
    }

    /// What was on the clipboard, handed back by the binary.
    ///
    /// **One operation per path, queued** — a paste of four files is four things that can each
    /// fail on their own, and reporting them as one would be a single sentence about four
    /// answers. Nothing here reads the filesystem: `main` refuses a destination that is taken,
    /// which is where that promise already lives.
    pub fn pasted(&mut self, verb: librsproto::clipboard::PathVerb, paths: &[(&str, bool)]) {
        let dir = self.pane().path.clone();
        let mut queued = 0usize;
        for (from, is_dir) in paths {
            let name = libfs::basename_str(from);
            if name.is_empty() {
                continue;
            }
            let to = join(&dir, name);
            // **A paste into the directory a file is already in is refused**, rather than
            // silently doing nothing or overwriting it: `from == to` is a rename onto itself for
            // a cut and a copy onto itself for a copy, and the second would truncate the file it
            // was reading.
            if *from == to {
                continue;
            }
            queued += 1;
            self.ops.push(match verb {
                librsproto::clipboard::PathVerb::Cut => FileOp::MoveInto { from: String::from(*from), to },
                // **Whether it is a directory comes from the binary**, which asked the
                // filesystem — the clipboard carries paths and nothing else, and this crate makes
                // no syscalls. A copy queued as a file copy for a *folder* reaches
                // `libfs::copy_file`, whose first act is asking for a size a directory has not
                // got, so it reported "could not copy it" for an ordinary request. `Duplicate`
                // has carried `dir` for exactly this reason since M12; the paste path bypassed it
                // (PR #285 review, worth fixing 3).
                librsproto::clipboard::PathVerb::Copy => FileOp::Copy { from: String::from(*from), to, dir: *is_dir },
            });
        }
        if queued == 0 {
            self.notice = Some(String::from("nothing to paste here"));
        }
    }

    /// The clipboard held something that is not a list of paths.
    pub fn paste_not_paths(&mut self) {
        self.notice = Some(String::from("the clipboard holds text, not files"));
    }

    /// Add or remove one row from the picks.
    ///
    /// **The row the list state holds is part of the selection**, so toggling it off has to move
    /// it somewhere: the state keeps a single `selected` and the picks are everything *besides*
    /// it, which would otherwise make the anchor impossible to unpick.
    fn toggle_mark(&mut self, i: usize) {
        let Some(name) = self.pane().entries.get(i).map(|e| e.name.clone()) else { return };
        if self.pane().list.selected == Some(i) {
            // **Unpicking the state's own row empties it rather than the picks**, which is the
            // half a set-only model would miss: `selected` is part of the selection, so
            // Ctrl-clicking it has to be able to take it out.
            self.pane_mut().list.selected = None;
        } else if let Some(at) = self.marked.iter().position(|n| *n == name) {
            self.marked.remove(at);
        } else {
            self.marked.push(name);
        }
        self.anchor = Some(i);
    }

    /// Pick every row between the anchor and `i`, inclusive.
    fn mark_range_to(&mut self, i: usize) {
        let from = self.anchor.or(self.pane().list.selected).unwrap_or(i);
        // **Between the two rows, not between the two entries** (PR #320 review): with a filter
        // up, the entries between them include files the listing is not showing, and a sweep
        // that picked those would act on what nobody can see.
        let shown = self.visible_indices();
        let (Some(a), Some(b)) = (shown.iter().position(|&x| x == from), shown.iter().position(|&x| x == i)) else {
            return;
        };
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let names: Vec<String> =
            shown[lo..=hi].iter().filter_map(|&e| self.pane().entries.get(e)).map(|e| e.name.clone()).collect();
        self.pane_mut().list.selected = Some(from);
        self.marked = names.into_iter().filter(|n| Some(n.as_str()) != self.selected_name()).collect();
    }

    /// The name of the row the list state holds, if any.
    fn selected_name(&self) -> Option<&str> {
        let p = self.pane();
        p.list.selected.and_then(|i| p.entries.get(i)).map(|e| e.name.as_str())
    }

    /// Every entry picked right now, in listing order.
    ///
    /// **One answer for every caller**, because "the selection" is what the Edit menu acts on and
    /// a second derivation of it is how a cut and a paste come to disagree about what was picked.
    pub fn selection(&self) -> Vec<&Entry> {
        self.pane()
            .entries
            .iter()
            .enumerate()
            .filter(|(i, e)| self.pane().list.selected == Some(*i) || self.marked.contains(&e.name))
            .map(|(_, e)| e)
            .collect()
    }

    /// The listing's rows as the tree keys them.
    ///
    /// **For the test that pins the keys apart from the chrome's.** Building the whole view to
    /// read them back would work and would also depend on every other thing the view does.
    pub fn list_rows(&self) -> Vec<libui::widget::ListRow<'_>> {
        self.pane()
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| libui::widget::ListRow {
                key: LIST_ROW_KEY + i as u64,
                label: e.name.as_str(),
                marked: self.marked.contains(&e.name),
                ..Default::default()
            })
            .collect()
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
            // **No sentinel name.** `unwrap_or_default` here would hand `""` to a lookup rather
            // than saying there is nothing to carry (PR #282 review, optional 7).
            let Some((name, _, _)) = self.pressed.clone() else { return Gesture::None };
            return self.drag_moved(x, y, &name);
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
        // **The click run ends the moment this becomes a drag** (M14 decision 5), and here rather
        // than in the binary: a drag begins with a press the tracker has already counted, so a
        // run left open makes the next click number two and opens something nobody asked for.
        // The binary cannot see this moment — `Gesture::Moved` says the row under an *existing*
        // drag changed, so a drag that never leaves its own row would report nothing at all.
        self.note_drag();
        // **The motion that crosses the slop can also be the one that leaves**, and until
        // 2026-09-04 nothing checked that: the hand-off lived in the already-dragging branch,
        // which only a *later* motion reaches — so a gesture that left the window in one step
        // armed a drag and then sat there with nothing to move it on. A person dragging quickly
        // out of the window got nothing, and `check-login` saw it whenever its six injected
        // motions coalesced into two and the second landed outside, which is a function of how
        // busy the guest is and looked exactly like a flake.
        //
        // Delegating rather than repeating the test, so the two paths cannot come to disagree
        // about what "left the window" means.
        if !self.inside(x, y) {
            return self.drag_moved(x, y, &name);
        }
        self.over = self.row_at(y).filter(|&i| self.pane().entries.get(i).is_some_and(|e| e.is_dir));
        Gesture::Moved
    }

    /// What a motion means to a drag already under way: still inside, or handed over.
    ///
    /// **Split out so the slop-crossing motion can run it too** — it was inline in
    /// `pointer_moved`'s already-dragging branch, which the *second* motion is the first to
    /// reach. See the call site above for what that cost.
    fn drag_moved(&mut self, x: i32, y: i32, name: &str) -> Gesture {
        if !self.inside(x, y) {
            // **Handed over at the edge, not at the slop.** The compositor runs the drag from
            // here — it draws the outline and delivers to whatever accepts the payload — and the
            // client goes blind for the rest of the gesture, which is why the hand-off has to be
            // the *last* thing this window decides.
            self.pressed = None;
            self.dragging = false;
            self.over = None;
            self.drag = self.pane().entries.iter().find(|e| e.name == name).cloned();
            return if self.drag.is_some() { Gesture::HandOff } else { Gesture::None };
        }
        let was = self.over;
        // **Only a directory is a target**, so the highlight never offers a landing place that
        // would do nothing — the same rule the compositor follows for windows that do not take
        // the payload.
        self.over = self.row_at(y).filter(|&i| self.pane().entries.get(i).is_some_and(|e| e.is_dir));
        if was == self.over { Gesture::None } else { Gesture::Moved }
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

    /// Whether another window has been asked for. Clears the record.
    pub fn take_new_window(&mut self) -> bool {
        core::mem::take(&mut self.new_window)
    }

    /// Whether a quit has been asked for. Clears the record.
    pub fn take_quit(&mut self) -> bool {
        core::mem::take(&mut self.quit)
    }

    /// The edges a `StartResize` is owed for. Clears the record.
    pub fn take_resize_request(&mut self) -> Option<u32> {
        self.resize_requested.take()
    }

    /// Record where each bar word sits, so an open menu knows where to hang from.
    ///
    /// **Here rather than in the binary, since M15 Part E.** Three applications each copied this
    /// loop, and the count in it was a constant beside a `menu_table` that grew: `nxfiles` asked
    /// for two anchors while its bar had three menus, so *View* opened a popup with nowhere to
    /// go and drew nothing at all. In the binary it was untestable — no host test builds a
    /// `main.rs` — which is why it could be wrong for two milestones. Here it is one method with
    /// one count, and a test walks the whole bar through it.
    pub fn place_menus(&mut self, view: &Element<Msg>, l: &libui::layout::Layout) {
        let n = self.menu_count();
        self.menus.set_anchors((0..n).map(|i| libui::layout::locate(view, l, MENU_BAR_KEY + i as u64)).collect());
    }

    /// How many menus the bar has — **derived from the table, never declared**.
    ///
    /// **A constant here drifted and cost the View menu** (M15 Part E). `MENU_COUNT` was 2 while
    /// `menu_table` returned three menus, so the binary asked for two anchors,
    /// `MenuState::anchor` answered `None` for the third, and clicking *View* opened a menu that
    /// had nowhere to hang from — a bar word that did nothing, with no error anywhere. Two
    /// numbers that must be equal are one number.
    pub fn menu_count(&self) -> usize {
        self.menu_table().len()
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
        let here = self.pane().order;
        let order_row = |label: &'static str, o: libfs::Order| Item::plain(label, Msg::SetOrder(o)).marked(o == here);
        vec![
            Menu {
                title: "File",
                items: vec![
                    Item::new("New Tab", Accel::ctrl(NEW_TAB_KEYCODE, "T"), Msg::NewTab),
                    Item::new("Close Tab", Accel::ctrl(CLOSE_TAB_KEYCODE, "W"), Msg::CloseTab(self.current))
                        // **The last tab does not close**, which is the strip's existing rule said
                        // out loud: `CloseTab` on a lone tab is already a no-op, and a row that
                        // silently did nothing was the half of the affordance that was missing.
                        .enabled(self.panes.len() > 1),
                    // **After the tab pair, not between it.** New Tab and Close Tab are one
                    // thought, and a row wedged between them reads as part of neither.
                    Item::new("Go to Location\u{2026}", Accel::ctrl(LOCATION_KEYCODE, "L"), Msg::OpenLocation),
                    Item::Separator,
                    Item::new("New Window", Accel::ctrl_shift(NEW_WINDOW_KEYCODE, "N"), Msg::NewWindow),
                    Item::new("Quit", Accel::ctrl(QUIT_KEYCODE, "Q"), Msg::Quit),
                    Item::Separator,
                    act("New File", Action::NewFile),
                    act("New Folder", Action::NewFolder),
                    act("Rename", Action::Rename),
                    // **In `deny`** (desktop refresh, Part B): the one row here that destroys
                    // something, drawn as the design draws such a row. It still asks first.
                    act("Delete", Action::Delete).destructive(true),
                    // **Last, and after the destructive row rather than before it.** Properties
                    // is the one row here that changes nothing, so it sits where a mis-aimed
                    // press lands on it instead of on Delete.
                    Item::new("Properties", Accel::ctrl(PROPERTIES_KEYCODE, "I"), Msg::Choose(Action::Properties))
                        .enabled(selected),
                ],
            },
            // **Cut, Copy and Paste are the clipboard's**, and Duplicate is the older row that
            // used to be called Copy — a different thing, which is why it moved rather than being
            // renamed in place: this one asks for a name and writes a file, those three move a
            // path through `/dev/clipboard` and act on a Paste.
            Menu {
                title: "Edit",
                items: vec![
                    Item::new("Cut", Accel::ctrl(CUT_KEYCODE, "X"), Msg::CutFiles).enabled(selected),
                    Item::new("Copy", Accel::ctrl(COPY_KEYCODE, "C"), Msg::CopyFiles).enabled(selected),
                    // **Paste is offered whatever is on the ring**, because a browser cannot know
                    // what is there without reading it, and reading it is a syscall this crate
                    // does not make. A paste of text says so instead.
                    Item::new("Paste", Accel::ctrl(PASTE_KEYCODE, "V"), Msg::PasteFiles),
                    Item::Separator,
                    act("Duplicate\u{2026}", Action::Duplicate),
                ],
            },
            // **A menu that sets something says what it is set to** — the four orders are a radio
            // group and the last row is a toggle, and both read their mark from the tab whose
            // menu this is. Without the marks the menu would be write-only: you could choose an
            // order and never see which one is in force.
            Menu {
                title: "View",
                items: vec![
                    order_row("Name (A\u{2013}Z)", libfs::Order::NameAsc),
                    order_row("Name (Z\u{2013}A)", libfs::Order::NameDesc),
                    order_row("Oldest First", libfs::Order::OldestFirst),
                    order_row("Newest First", libfs::Order::NewestFirst),
                    Item::Separator,
                    Item::new("Show Hidden Files", Accel::ctrl(HIDDEN_KEYCODE, "H"), Msg::ToggleHidden)
                        .marked(self.pane().show_hidden),
                    // **In View rather than in File**, where it was first written: this row
                    // changes *which rows are shown*, which is what the rest of this menu does —
                    // and the File menu's rows are indexed by `check-login`, so a row wedged
                    // into the middle of it moves a gate's aim for a reason that has nothing to
                    // do with the gate (desktop refresh, Part I).
                    Item::new("Search\u{2026}", Accel::ctrl(SEARCH_KEYCODE, "F"), Msg::OpenSearch),
                ],
            },
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

    /// Which dialog this window has open, if any.
    ///
    /// **The binary reconciles its one window against this**, so a dialog that opens while
    /// another is up replaces it rather than being lost — which is what makes the single slot
    /// safe rather than merely shorter.
    pub fn dialog(&self) -> Option<Dialog> {
        // **The question wins if both are somehow set**, which nothing can currently do: it is
        // the one that cannot be dismissed by ignoring it.
        if self.confirm.is_some() {
            Some(Dialog::Confirm)
        } else {
            self.properties.as_ref().map(|_| Dialog::Properties)
        }
    }

    /// The open dialog's tree.
    ///
    /// **Empty when nothing is open**, which cannot be drawn because the window only exists while
    /// a dialog does — but the type demands an answer and a caller should not have to think about
    /// it. The same shape `nxedit::App::chooser_view` settled on.
    pub fn dialog_view(&self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        match self.dialog() {
            Some(Dialog::Confirm) => self.confirm_view(ui, hovered),
            Some(Dialog::Properties) => self.properties_view(ui, hovered),
            None => libui::widget::popup_frame(text(String::new()), ui),
        }
    }

    /// What the open dialog does with a key.
    pub fn dialog_key(&self, k: KeyEvent) -> Option<Msg> {
        match self.dialog() {
            Some(Dialog::Confirm) => self.confirm_key(k),
            // **`Esc` closes it and so does `Enter`**, because there is nothing to refuse: a
            // dialog that only reports should close on whichever key a person reaches for.
            Some(Dialog::Properties) => {
                if k.pressed != KEY_DOWN && k.pressed != KEY_REPEAT {
                    return None;
                }
                matches!(k.keycode, libkern::abi::KEY_ESC | libkern::abi::KEY_ENTER).then_some(Msg::CloseProperties)
            }
            None => None,
        }
    }

    /// What a manager asking the *dialog's window* to close means.
    ///
    /// **The cautious answer, per dialog.** For a question that is the one that changes nothing:
    /// closing a question must not perform it.
    pub fn dialog_dismissed(&self) -> Option<Msg> {
        match self.dialog() {
            Some(Dialog::Confirm) => Some(Msg::KeepIt),
            Some(Dialog::Properties) => Some(Msg::CloseProperties),
            None => None,
        }
    }

    /// The dialog's window could not be created, so it cannot be shown.
    pub fn dialog_failed(&mut self) {
        match self.dialog() {
            Some(Dialog::Confirm) => self.confirm_failed(),
            // Nothing was going to change, so there is nothing to undo — but the window that
            // failed must not be left recorded as open, or the slot never reconciles again.
            Some(Dialog::Properties) => {
                self.properties = None;
                self.notice = Some(String::from("could not show the properties"));
            }
            None => {}
        }
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

    /// What the location bar holds, if it is open.
    ///
    /// **For the binary's receipt**, which reports it on change the way the name prompt's length
    /// is reported: a bar that is open and a bar that is not look different on screen and
    /// identical in a transcript.
    pub fn location_text(&self) -> Option<String> {
        self.location.as_ref().map(|f| String::from(f.text()))
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
        if self.ops.is_empty() { None } else { Some(self.ops.remove(0)) }
    }

    /// Whether any filesystem work is queued.
    pub fn has_ops(&self) -> bool {
        !self.ops.is_empty()
    }

    /// Report what a *batch* of operations did, when one sentence cannot cover them.
    ///
    /// **Both numbers when both happened**, because "copied" after three refusals is the browser
    /// telling somebody their files arrived when they did not — and the "nothing overwrites"
    /// promise is only worth having if it is visible when it fires.
    pub fn operated_many(&mut self, done: usize, refused: usize, why: &str) {
        self.notice = Some(if refused == 0 {
            alloc::format!("{done} done")
        } else if done == 0 {
            alloc::format!("{refused} refused: {why}")
        } else {
            alloc::format!("{done} done, {refused} refused: {why}")
        });
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
            TITLE_BAR_H + MENU_BAR_H + self.tab_strip_h() + PATH_H + HEADER_H + STATUS_BAR_H + WINDOW_FRAME_H,
        )
    }

    /// How tall the tab strip is in this window — the widget's height, or nothing at all.
    ///
    /// **One tab is no strip** (desktop refresh, Part I): the design draws none, tabs are ours,
    /// and a strip that can only say where you already are is chrome for its own sake. Every sum
    /// over the window's chrome goes through here, so the listing's height and the gate's row
    /// arithmetic cannot disagree about whether it is there.
    pub fn tab_strip_h(&self) -> u32 {
        if self.tabs().len() > 1 { TAB_STRIP_H } else { 0 }
    }

    /// The window's foot: how many entries are in this directory, and what is picked.
    fn status_bar(&self, ui: &UiTheme) -> Element<Msg> {
        // **What is shown, not what is there** (PR #320 review, optional 11): a filter narrowing
        // eighty entries to one used to read `82 items` beside a single row. When a search is
        // narrowing, both numbers are said, because "3 items" alone hides that there are more.
        let shown = self.visible_indices().len();
        let total = self.pane().entries.len();
        let count = match (self.search.is_some(), shown) {
            (true, n) => alloc::format!("{n} of {total} items"),
            (false, 1) => String::from("1 item"),
            (false, n) => alloc::format!("{n} items"),
        };
        let mut left = alloc::vec![status_text(count, ui)];
        // **What is selected, after a separator** — the design's `6 items | Documents/ selected`.
        if let Some(i) = self.pane().list.selected
            && let Some(e) = self.pane().entries.get(i)
        {
            left.push(status_separator(ui));
            left.push(status_text(alloc::format!("{} selected", e.label()), ui));
        }
        status_bar(row(left), None, Edge::Top, ui)
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
            // **What this window is showing**, which for a browser is where it is looking.
            Some(self.pane().path.as_str()),
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
            hovered,
            ui,
        )
        .key(TITLE_KEY);

        // The menu bar: two names, each of which drops a list of things to do. **The toolkit's
        // since M14 Part A** — the words were `button`s here, which is not what a menu bar's
        // word is, and the popup under them was a second copy of `nxterm`'s.
        let bar =
            libui::menu::bar(&self.menu_table(), &self.menus, MENU_BAR_KEY, hovered, Msg::MenuBar, ui, MENU_BAR_H);

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
                    text_field(f, false, WidgetState { active: true, ..Default::default() }, ui),
                )
                .key(PROMPT_KEY)
                .flex(1),
                padding(
                    Insets { top: 4, right: 6, bottom: 4, left: 0 },
                    text(self.notice.clone().unwrap_or_default()),
                )
                .key(NOTICE_KEY),
            ]),
            // **The location bar replaces the path with a field showing the same text**, which
            // is what makes it read as editing where you are rather than as a second prompt about
            // something else.
            None if self.location.is_some() => {
                let f = self.location.as_ref().expect("checked by the guard");
                row(alloc::vec![
                    padding(
                        Insets { top: 2, right: 6, bottom: 2, left: 6 },
                        text_field(f, false, WidgetState { active: true, ..Default::default() }, ui),
                    )
                    .key(PATH_KEY)
                    .flex(1),
                    padding(
                        Insets { top: 4, right: 6, bottom: 4, left: 0 },
                        text(self.notice.clone().unwrap_or_default()),
                    )
                    .key(NOTICE_KEY),
                ])
            }
            // **The path as segments, with the separators drawn quieter than the names** — the
            // design's breadcrumb (desktop refresh, Part I). One `Text` per segment rather than
            // one string, because the `/` between them is the only part that is not a name and
            // the whole point is that it reads as punctuation.
            None => row(alloc::vec![
                padding(
                    Insets { top: 4, right: 4, bottom: 4, left: 6 },
                    center_v(row(breadcrumb(self.pane().path.as_str(), ui))),
                )
                .key(PATH_KEY),
                padding(
                    Insets { top: 4, right: 4, bottom: 4, left: 6 },
                    center_v(text(self.notice.clone().unwrap_or_default())),
                )
                .key(NOTICE_KEY),
            ]),
        };
        let mut strip_items = alloc::vec![
            button("^", Msg::Up, WidgetState { hovered: hovered == Some(UP_KEY), ..Default::default() }, ui,)
                .key(UP_KEY),
            middle.key(STRIP_INNER_KEY).flex(1),
        ];
        // **The search field only while it is open**, which is the honest version of the
        // design's Search box: it filters the listing as it is typed, and a field that sat there
        // doing nothing is the defect every part of this refresh has named. `Ctrl+F` opens it,
        // `Esc` closes it, `Enter` opens what is left.
        if let Some(f) = self.search.as_ref() {
            strip_items.push(
                sized(
                    Size::new(SEARCH_W, 0),
                    padding(Insets { top: 2, right: 6, bottom: 2, left: 0 }, {
                        let state = WidgetState { active: true, ..Default::default() };
                        text_field(f, false, state, ui)
                    }),
                )
                .key(SEARCH_KEY),
            );
        }
        let strip = row(strip_items);

        // **The tab strip, between the menus and the path.** Above the path because a tab *is*
        // a path — the strip says which of several you are looking at, and the strip below says
        // where that one is.
        let tab_list = self.tabs();
        let items: Vec<libui::widget::Tab<'_>> = tab_list
            .iter()
            .map(|(k, label)| libui::widget::Tab { key: *k, label: label.as_str(), marked: false })
            .collect();
        let tabs = tab_strip(
            &items,
            self.current,
            hovered,
            Msg::SelectTab,
            Msg::CloseTab,
            TabExtras::new_tab(Msg::NewTab),
            ui,
        );

        // **The rows the filter leaves**, by index into the whole listing: a row's key carries
        // its original index, so opening, dragging and renaming resolve to the same file whether
        // or not a search is narrowing what is on screen.
        let shown = self.visible_indices();
        let labels: Vec<String> = shown.iter().map(|i| self.pane().entries[*i].label()).collect();
        // **The three facts beside each name** (desktop refresh, Part I), built as owned strings
        // first because a `ListRow` borrows its cells: one `Vec` of them, then a `Vec` of
        // borrows into it, which is the same two-step the labels above already take.
        let facts: Vec<[String; 3]> = shown
            .iter()
            .map(|i| &self.pane().entries[*i])
            .map(|e| {
                [column_size(e.size), column_kind(e.name.as_str(), e.is_dir), column_modified(e.mtime, self.now_nanos)]
            })
            .collect();
        let cells: Vec<[&str; 3]> = facts.iter().map(|f| [f[0].as_str(), f[1].as_str(), f[2].as_str()]).collect();
        let mut rows: Vec<ListRow<'_>> = Vec::with_capacity(labels.len());
        for (n, l) in labels.iter().enumerate() {
            let i = shown[n];
            let entry = &self.pane().entries[i];
            rows.push(ListRow {
                key: LIST_ROW_KEY + i as u64,
                label: l,
                marked: self.marked.contains(&entry.name),
                cells: &cells[n][..],
                // **The design's mark**: a folder in the accent, a file in the line colour.
                swatch: Some(Swatch::block(if entry.is_dir { ui.accent } else { ui.border })),
                button: None,
            });
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
            Some(i) => Some(LIST_ROW_KEY + i as u64),
            None => hovered,
        };
        // **The sidebar's highlight is derived, not remembered** — whichever place matches where
        // this tab is, recomputed each frame. A stored selection would be a second answer to
        // "where am I" and would disagree the moment anything else navigated.
        let places = self.places();
        let here = places.iter().position(|p| p.path == self.pane().path);
        let side_rows: Vec<ListRow<'_>> = places
            .iter()
            .enumerate()
            .map(|(i, p)| ListRow {
                key: SIDEBAR_ROW_KEY + i as u64,
                label: p.name,
                marked: false,
                cells: &[],
                // **A dot per place, and `Root`'s is the deny colour** (desktop refresh,
                // Part I): the design marks every place with one and makes that one different,
                // which is the only warning a person gets before walking out of their own home
                // directory into the system's.
                swatch: Some(Swatch::dot(if p.path == "/" { ui.deny } else { ui.ok })),
                button: None,
            })
            .collect();
        self.sidebar.selected = here;
        // **The panel is built for the height it will be drawn at**, which is the box minus the
        // margin on each side — the obligation `list_view` states, and the one thing a margin
        // around a scrolling widget is easy to get wrong.
        let side_h = (h + HEADER_H).saturating_sub(2 * SIDEBAR_PAD);
        // **The places take the whole column until there is a drive** (Phase 6 Part F), and then
        // as many rows as they have, with the drives under a heading below them.
        // **And never all of it**: in a short window the places give way, scrolling, so a drive row
        // stays under its heading rather than the heading standing over nothing (PR #367 review).
        let places_h = if self.drives.is_empty() {
            side_h
        } else {
            (side_rows.len() as u32 * ROW_H).min(side_h.saturating_sub(DRIVES_HEADING_H + ROW_H))
        };
        let places = list_view(
            &side_rows,
            // The places have a name and a dot and nothing else to line up.
            &[],
            &mut self.sidebar,
            places_h,
            ROW_H,
            place_row,
            None,
            None,
            hovered,
            // **A ground of its own** (M15): a sidebar drawn in the list's ground is a list with
            // a gap in it, and the report from running it was that the quick-access folders
            // "need a different background color so it's clear it's actually a sidebar".
            Some(ui.sidebar),
            ui,
        );
        let mut side = alloc::vec![sized(Size::new(0, places_h), places).key(PLACES_KEY)];
        if !self.drives.is_empty() {
            // **Every mount under `/storage`, by the name it is mounted under**, a removable one
            // with an eject button — as a tab carries its `×`. Highlighted where the tab is, as a
            // place is.
            let here_drive = self.drives.iter().position(|d| d.path() == self.pane().path);
            let drive_rows: Vec<ListRow<'_>> = self
                .drives
                .iter()
                .map(|d| ListRow {
                    key: DRIVE_ROW_KEY + d.block,
                    label: d.name.as_str(),
                    swatch: Some(Swatch::dot(ui.ok)),
                    button: d.removable.then_some(RowButton { key: DRIVE_EJECT_KEY + d.block, glyph: EJECT_GLYPH }),
                    ..Default::default()
                })
                .collect();
            self.drive_list.selected = here_drive;
            let drives_h = side_h.saturating_sub(places_h + DRIVES_HEADING_H);
            let drives = list_view(
                &drive_rows,
                &[],
                &mut self.drive_list,
                drives_h,
                ROW_H,
                drive_row,
                None,
                None,
                hovered,
                Some(ui.sidebar),
                ui,
            );
            let heading = padding(
                Insets { left: libui::widget::ROW_PAD.left, ..Insets::all(0) },
                center_v(ink(
                    ui.foreground_dim,
                    libui::element::scaled(libui::element::TextSize::Small, text("Drives")),
                )),
            );
            side.push(sized(Size::new(0, DRIVES_HEADING_H), heading).key(DRIVES_HEADING_KEY));
            side.push(sized(Size::new(0, drives_h), drives).key(DRIVES_KEY));
        }
        let sidebar = libui::element::stack(alloc::vec![libui::element::fill(ui.sidebar), column(side)]);

        // **Into the widget's space, and back out again** (PR #320 review, blocking 1). The pane
        // holds an entries index; `list_view` means a row position by `selected`, highlights by
        // position and clamps against the rows it was given. Handing it the entries index made
        // the highlight and the acted-on file different files as soon as a filter was up.
        let stored = self.pane().list.selected;
        let position = stored.and_then(|e| shown.iter().position(|&i| i == e));
        self.pane_mut().list.selected = position;
        let list = list_view(
            &rows,
            LIST_COLUMNS,
            &mut self.pane_mut().list,
            h,
            ROW_H,
            press_row,
            Some(grab_row),
            Some(Msg::Scroll),
            highlight,
            None,
            ui,
        )
        // **On the element the widget returns, rather than through a tenth parameter.** A
        // wheel bubbles to the nearest `on_wheel`, and `list_view`'s outermost node is exactly
        // the region a person means by "over the list" — rows and scrollbar both. The widget
        // needs to know nothing about it (M14 Part I).
        .on_wheel(Msg::Wheel);
        // **Back to an entries index.** `list_view` may have clamped the position or scrolled to
        // follow it. A selection the filter is *hiding* has no position at all, and keeps its
        // stored index rather than being forgotten — so closing the search puts the highlight
        // back on the file it was on.
        let after = self.pane().list.selected;
        self.pane_mut().list.selected = match (position, after) {
            (None, _) => stored,
            (Some(_), Some(p)) => shown.get(p).copied(),
            (Some(_), None) => None,
        };

        // **The strip is left out, not sized to nothing** (desktop refresh, Part I). The design
        // draws no tab strip at all; tabs are ours, and a strip holding one tab can only say
        // where you already are. A zero-height box still paints its children — the tab and the
        // `+` drew over the chrome below it — so the edge has to be absent from the dock.
        let mut edges = alloc::vec![docked(Edge::Top, sized(Size::new(0, MENU_BAR_H), bar).key(BAR_KEY)),];
        if self.tab_strip_h() > 0 {
            edges.push(docked(Edge::Top, sized(Size::new(0, TAB_STRIP_H), tabs).key(TAB_STRIP_KEY)));
        }
        edges.extend(alloc::vec![
            docked(Edge::Top, sized(Size::new(0, PATH_H), strip).key(STRIP_KEY)),
            // **The foot: what is in this directory, and what is picked** (Part I). Docked to
            // the bottom before the sidebar takes its column, so it spans the whole window as
            // the design draws it rather than stopping at the listing.
            docked(Edge::Bottom, self.status_bar(ui).key(STATUS_KEY)),
            // **Docked after the strips, so it starts below them** — a dock takes its edges in
            // order, and each one divides what the last left. The path strip spans the full
            // width above both panes, which is what makes it read as the window's location
            // rather than the listing's.
            docked(
                Edge::Left,
                sized(
                    // **The whole column, header included** (PR #320 review, worth fixing 7):
                    // the sidebar was sized to the *listing's* height while its column is that
                    // plus the header, so its ground stopped short of the status bar and the
                    // window showed through underneath it.
                    Size::new(SIDEBAR_W + 2 * SIDEBAR_PAD, h + HEADER_H),
                    padding(Insets::all(SIDEBAR_PAD), sidebar),
                )
                .key(SIDEBAR_KEY),
            ),
        ]);
        window_frame_with_grip(
            title,
            dock(
                edges,
                // **Sized to the height it was built for.** `list_view` does not size itself, and
                // the dock's flex child otherwise gets everything left over — so the widget would
                // build rows for one height and be drawn at another, leaving `visible` off by one
                // for the scroll arithmetic and a dead row at the bottom. Its own doc names this
                // wrapper as the reliable way to keep the two in step.
                dock(
                    alloc::vec![docked(
                        Edge::Top,
                        libui::widget::list_header(
                            "Name",
                            &LIST_HEADINGS,
                            LIST_COLUMNS,
                            true,
                            libui::widget::list_scrolls(shown.len(), h, ROW_H),
                            ui,
                        )
                        .key(HEADER_KEY),
                    )],
                    sized(Size::new(0, h), list).key(LIST_KEY),
                )
                .key(LIST_PANE_KEY),
            ),
            resize_grip(Msg::ResizeWindow(RESIZE_RIGHT | RESIZE_BOTTOM), ui).key(GRIP_KEY),
            self.window,
            self.focused,
            ui,
        )
    }
}

#[cfg(test)]
mod tests;
