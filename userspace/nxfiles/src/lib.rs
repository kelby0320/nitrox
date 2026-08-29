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

use alloc::string::String;
use alloc::vec::Vec;

use libdraw::geom::{Rect, Size};
use librsproto::file::{DIRENT_KIND_DIR, OwnedEntry};
use librsproto::surface::{
    KEY_DOWN, KEY_REPEAT, KeyEvent, RESIZE_BOTTOM, RESIZE_RIGHT,
    WINDOW_STATE_MAXIMIZED, WINDOW_STATE_MINIMIZED, WINDOW_STATE_NORMAL,
};
use libui::element::{
    Edge, Element, Insets, dock, docked, offset, padding, row, sized, stack, text,
};
use libui::widget::{
    GRIP_W, ListRow, ListState, Palette as UiPalette, TITLE_BAR_H, TitleButtons, WidgetState,
    button, list_view, resize_grip, title_bar,
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
/// The element key on the path strip.
///
/// **Every docked child needs one**, not just the ones a test looks for: the diff requires a
/// container's children to be all keyed or all unkeyed, and a single bare sibling makes the
/// whole frame `MixedKeying`. It fails on the *second* frame, not the first — the first builds
/// the tree and the second compares against it — which is a first paint that works followed by
/// a window that never updates again.
pub const STRIP_KEY: u64 = 5;

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
pub struct App {
    /// Where the listing came from, absolute and with no trailing separator except at the root.
    path: String,
    /// What is in it, sorted.
    entries: Vec<Entry>,
    /// Which row is selected and how far the list is scrolled.
    list: ListState,
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
    /// A title-bar button was pressed, and the binary owes the compositor a `RequestState`.
    state_requested: Option<u32>,
    /// The title bar was dragged, and the binary owes the compositor a `StartMove`.
    move_requested: bool,
    /// The grip was pressed, and the binary owes the compositor a `StartResize`.
    resize_requested: Option<u32>,
}

/// What can happen to the browser.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Msg {
    /// A listing row was activated — its index into [`App::entries`].
    Activate(u64),
    /// The "up" control was pressed.
    Up,
    /// A key reached the window.
    Key(KeyEvent),
    /// The title bar was dragged.
    DragWindow,
    /// The resize grip was pressed, for these edges.
    ResizeWindow(u32),
    /// A title-bar button asking the manager for a window state.
    RequestState(u32),
}

impl App {
    /// A browser showing nothing, at `path`.
    pub fn new(path: &str) -> App {
        App {
            path: String::from(path),
            entries: Vec::new(),
            list: ListState::default(),
            window: START_SIZE,
            focused: true,
            maximized: false,
            goto: None,
            state_requested: None,
            move_requested: false,
            resize_requested: None,
        }
    }

    /// Where the browser is.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// What it is showing.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
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
        self.path = String::from(path);
        self.entries = entries;
        self.list = ListState { selected: (!self.entries.is_empty()).then_some(0), offset: 0 };
    }

    /// Turn a wire entry into a row, dropping the ones a browser has no use for.
    ///
    /// `.` and `..` are already filtered by [`libfs::list_dir`]; what this drops is anything
    /// whose kind says nothing — a browser that showed an `UNKNOWN` as a file would offer to
    /// open something it cannot describe.
    pub fn entry_of(e: &OwnedEntry) -> Option<Entry> {
        let name = String::from_utf8_lossy(e.name()).into_owned();
        if name.is_empty() {
            return None;
        }
        Some(Entry { name, is_dir: e.kind == DIRENT_KIND_DIR })
    }

    /// The path a row press leads to, or `None` for a row that is not a directory.
    fn child(&self, i: usize) -> Option<String> {
        let e = self.entries.get(i)?;
        e.is_dir.then(|| join(&self.path, &e.name))
    }

    /// Apply a message.
    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Activate(i) => {
                let i = i as usize;
                self.list.selected = Some(i);
                // **A directory navigates; a file does nothing yet.** Opening a file means
                // launching something that can show it, and there is nothing to launch until
                // the editor exists (M10 Part D). A row that does nothing is honest; a row that
                // launched a program that is not there would be a control that looks live.
                if let Some(to) = self.child(i) {
                    self.goto = Some(to);
                }
            }
            Msg::Up => {
                let up = parent(&self.path);
                // The root is its own parent, so this is a no-op there rather than an error —
                // the same rule `libfs::parent` states, and the reason the control is never
                // disabled: a control that greys out at the root is one more state to draw.
                if up != self.path {
                    self.goto = Some(up);
                }
            }
            Msg::Key(k) => self.key(k),
            Msg::DragWindow => self.move_requested = true,
            Msg::ResizeWindow(edges) => self.resize_requested = Some(edges),
            Msg::RequestState(s) => {
                if s == WINDOW_STATE_MAXIMIZED || s == WINDOW_STATE_NORMAL {
                    self.maximized = s == WINDOW_STATE_MAXIMIZED;
                }
                self.state_requested = Some(s);
            }
        }
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
        let len = self.entries.len();
        match k.keycode {
            libkern::abi::KEY_DOWN => {
                self.list.down(len);
            }
            libkern::abi::KEY_UP => {
                self.list.up();
            }
            libkern::abi::KEY_ENTER => {
                if let Some(i) = self.list.selected {
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

    /// The height the listing is laid out at — the window less its chrome.
    pub fn list_h(&self) -> u32 {
        self.window.h.saturating_sub(TITLE_BAR_H + PATH_H + GRIP_W)
    }

    /// The element tree for the current state.
    pub fn view(&self) -> Element<Msg> {
        let ui = UiPalette::default();
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
                close: None,
            },
            &ui,
        )
        .key(TITLE_KEY);

        // The path strip: where you are, and the one control that leaves it.
        let strip = row(alloc::vec![
            button("^", Msg::Up, WidgetState::default(), &ui).key(UP_KEY),
            padding(Insets { top: 4, right: 4, bottom: 4, left: 6 }, text(self.path.clone()))
                .key(PATH_KEY),
        ]);

        let rows: Vec<ListRow<'_>> = Vec::new();
        let labels: Vec<String> = self.entries.iter().map(|e| e.label()).collect();
        let mut rows = rows;
        for (i, l) in labels.iter().enumerate() {
            rows.push(ListRow { key: i as u64, label: l });
        }
        let (list, _) = list_view(&rows, self.list, self.list_h(), ROW_H, Msg::Activate, &ui);

        let body = dock(
            alloc::vec![
                docked(Edge::Top, title),
                docked(Edge::Top, sized(Size::new(0, PATH_H), strip).key(STRIP_KEY)),
            ],
            list.key(LIST_KEY),
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

    /// Two consecutive frames must diff — **the second is where a keying mistake shows.**
    ///
    /// The first `update` builds the tree and the second compares against it, so a container
    /// whose children are partly keyed produces a window that paints once and then never
    /// updates again. Found the expensive way, in a four-minute boot, when this would have
    /// found it in a millisecond (M10 Part B).
    #[test]
    fn consecutive_frames_diff_rather_than_erroring_on_the_second() {
        use libui::diff::Tree;
        use libui::layout::{FixedCell, layout};
        let a = app();
        let cell = FixedCell { w: 8, h: 16 };
        let mut tree = Tree::new();
        for frame in 0..3 {
            let e = a.view();
            let l = layout(&e, bounds(a.window_size()), &cell);
            tree.update(&e, &l).unwrap_or_else(|err| panic!("frame {frame}: {err:?}"));
        }
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
    fn pressing_a_directory_navigates_and_pressing_a_file_does_not() {
        // **A file row does nothing until there is something to open it with** (M10 Part D). A
        // row that launched a program which is not there would be a control that looks live —
        // the defect M8's overview shipped three of.
        let mut a = app();
        a.update(Msg::Activate(0)); // archive/
        assert_eq!(a.take_goto().as_deref(), Some("/home/archive"));

        a.update(Msg::Activate(2)); // a.txt
        assert_eq!(a.take_goto(), None, "a file is not a place to go");
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
        assert_eq!(a.list.selected, Some(2), "precondition: the selection has moved");
        a.show("/home/work", alloc::vec![Entry::file("only.txt")]);
        assert_eq!(a.list.selected, Some(0));
    }

    #[test]
    fn an_empty_directory_selects_nothing() {
        // Something must be selected first, or this passes for a browser that never selects
        // anything at all — the same trap the test above fell into.
        let mut a = app();
        a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_DOWN, KEY_DOWN, 0)));
        assert!(a.list.selected.is_some(), "precondition: something is selected");
        a.show("/home/empty", Vec::new());
        assert_eq!(a.list.selected, None, "nothing to select, and no phantom row 0");
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
}
