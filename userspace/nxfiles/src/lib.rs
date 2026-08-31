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
    /// What the strip says beside the path, or `None` — an answer to the last row activated.
    notice: Option<String>,
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
    /// Somebody wants this window gone — its own close button, or the shell asking.
    ///
    /// **One message for both**, as `nxterm` has: which of them it was is not something this
    /// application acts on differently.
    Close,
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
            notice: None,
            open: None,
            state_requested: None,
            move_requested: false,
            resize_requested: None,
            closing: false,
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
        // A listing supersedes whatever the last row press had to say about itself.
        self.notice = None;
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
        let e = self.entries.get(i)?;
        e.is_dir.then(|| join(&self.path, &e.name))
    }

    /// The path a row *names*, whether or not it is a directory.
    fn full(&self, i: usize) -> Option<String> {
        self.entries.get(i).map(|e| join(&self.path, &e.name))
    }

    /// Apply a message.
    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Activate(i) => {
                let i = i as usize;
                self.list.selected = Some(i);
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
            Msg::Close => self.closing = true,
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
        self.window.h.saturating_sub(TITLE_BAR_H + PATH_H + GRIP_W)
    }

    /// The element tree for the current state.
    ///
    /// **`&mut self`, because building the view scrolls the list.** `list_view` scrolls its
    /// state to follow the selection, and a view that did not keep the result would re-derive
    /// the offset from zero on every frame — parking the highlight on the last visible row and
    /// scrolling the whole list under it on every arrow press. That is what shipped in Part B,
    /// when the widget still *returned* the state and this caller dropped it; it takes `&mut`
    /// now, so the same mistake no longer compiles (PR #257 review, blocking 1).
    pub fn view(&mut self) -> Element<Msg> {
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
                // **A close button, because without one every close takes the wedged path.**
                // The taskbar's middle-click sends `CloseRequested`; a client that ignores it
                // survives the two-second grace period and is then destroyed by
                // `Manage::Close` — the route the shell documents as being for a client that
                // has *stopped answering*. An application with no way to close itself would
                // take it every single time (PR #257 review, finding 3).
                close: Some(Msg::Close),
            },
            &ui,
        )
        .key(TITLE_KEY);

        // The path strip: where you are, and the one control that leaves it.
        let strip = row(alloc::vec![
            button("^", Msg::Up, WidgetState::default(), &ui).key(UP_KEY),
            padding(Insets { top: 4, right: 4, bottom: 4, left: 6 }, text(self.path.clone()))
                .key(PATH_KEY),
            padding(
                Insets { top: 4, right: 4, bottom: 4, left: 6 },
                text(self.notice.clone().unwrap_or_default()),
            )
            .key(NOTICE_KEY),
        ]);

        let labels: Vec<String> = self.entries.iter().map(|e| e.label()).collect();
        let mut rows: Vec<ListRow<'_>> = Vec::with_capacity(labels.len());
        for (i, l) in labels.iter().enumerate() {
            rows.push(ListRow { key: i as u64, label: l });
        }
        let h = self.list_h();
        let list = list_view(&rows, &mut self.list, h, ROW_H, Msg::Activate, &ui);

        let body = dock(
            alloc::vec![
                docked(Edge::Top, title),
                docked(Edge::Top, sized(Size::new(0, PATH_H), strip).key(STRIP_KEY)),
            ],
            // **Sized to the height it was built for.** `list_view` does not size itself, and
            // the dock's flex child otherwise gets everything left over — so the widget would
            // build rows for one height and be drawn at another, leaving `visible` off by one
            // for the scroll arithmetic and a dead row at the bottom. Its own doc names this
            // wrapper as the reliable way to keep the two in step.
            sized(Size::new(0, h), list).key(LIST_KEY),
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
            let e = a.view();
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
        let _ = a.view();
        let scrolled = a.list.offset;
        assert!(scrolled > 0, "precondition: 19 rows down has scrolled the list");
        assert_eq!(a.list.selected, Some(19));

        up(&mut a);
        let _ = a.view();
        assert_eq!(
            a.list.offset, scrolled,
            "the selection moved up inside the visible rows, so the list must not have moved"
        );
        assert_eq!(a.list.selected, Some(18));
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
        let ui: Element<Msg> = a.view();
        assert!(labelled(&ui, NOTICE_KEY).contains("opening a.txt"), "the strip says so");

        a.opened(&path, false);
        let ui: Element<Msg> = a.view();
        assert!(labelled(&ui, NOTICE_KEY).contains("could not open a.txt"));

        // And a new listing supersedes it: the notice is about a press, not about the directory.
        a.show("/home", alloc::vec![Entry::file("a.txt")]);
        let ui: Element<Msg> = a.view();
        assert_eq!(labelled(&ui, NOTICE_KEY), "", "a listing clears it");
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
