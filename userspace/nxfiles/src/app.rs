//! The browser: its state, `update` and `view`.

use super::*;

/// What this window is called, in its own title bar and in the shell's window list.
pub const TITLE: &str = "Files";

/// The path strip's height in pixels — one row of chrome under the title bar.
pub const PATH_H: u32 = 24;

/// The listing's trailing columns: `Size`, `Kind` and `Modified` at 70, 60 and 96, size
/// right-aligned so the digits line up.
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

/// The Drives heading's height: a row's, so a drive's row is a whole number of rows down.
pub const DRIVES_HEADING_H: u32 = ROW_H;

/// What an eject button shows: the eject symbol, which DejaVu Sans carries.
pub const EJECT_GLYPH: &str = "\u{23CF}";

/// How wide that dialog is — wider than a question, because it shows paths.
pub const PROPS_W: u32 = 420;

/// How tall: a title, five rows of text, and a button strip.
pub const PROPS_H: u32 = 236;

/// Everything the browser is.
pub struct App {
    /// The open panes, in tab order. Never empty: closing the last tab closes the window.
    panes: Vec<Pane>,
    current: u64,
    clip_push: Option<(librsproto::clipboard::PathVerb, Vec<String>)>,
    clip_read: bool,
    /// The entries picked besides the one the list state holds, by name: indices do not survive
    /// a re-sort.
    marked: Vec<String>,
    anchor: Option<usize>,
    /// This session's home, which the sidebar's first row goes to; `main` reads it from the
    /// environment.
    home: String,
    /// The sidebar's scroll and highlight; its selection is derived from where the tab is.
    pub(super) sidebar: ListState,
    /// The drives listed below the places, as the binary last read the storage table.
    drives: Vec<Drive>,
    /// The drives' scroll and highlight; its selection derived as the places' is.
    pub(super) drive_list: ListState,
    eject: Option<String>,
    /// The mounts an eject took, so their going is said as an eject, not as a stick pulled out.
    ejecting: Vec<String>,
    eject_said: Option<String>,
    /// What the strip says once the listing a drive's going asked for arrives.
    arrival_notice: Option<String>,
    /// The entry whose properties are being shown, and its directory: a snapshot, for
    /// [`Target`]'s reason.
    pub(super) properties: Option<(String, Entry)>,
    /// The location bar's field, while open: on the window, as only one can be open at a time.
    pub(super) location: Option<TextFieldState>,
    /// The search field, when open: it filters the listing as it is typed, by the Applications
    /// menu's rule.
    search: Option<TextFieldState>,
    /// Counts runs of pointer presses, fed by the binary (the only half that can read a clock).
    clicks: libui::click::Clicks,
    /// Where within the thumb the scrollbar was taken hold of; one per window, as a grab is.
    scroll_grab: libui::widget::ScrollGrab,
    click_run: u32,
    click_mods: u16,
    /// The next tab key — monotonic, so a stale message cannot name a pane that took its place;
    /// numbered from [`TAB_KEY_BASE`] to stay clear of element keys and list-row indices.
    next_key: u64,
    window: Size,
    /// Whether this window holds the keyboard, which the title bar shows. Starts `true`, as
    /// `libui::Router`'s `window_focused` does: `false` makes a client's first paint dim.
    pub focused: bool,
    maximized: bool,
    goto: Option<String>,
    /// What the strip says beside the path, or `None` — an answer to the last row activated.
    pub(super) notice: Option<String>,
    /// The row a press landed on, and where the pointer was, until the button comes up.
    ///
    /// A press becomes a drag only after travelling [`DRAG_SLOP`]. It holds the entry's *name*,
    /// not its index: a listing replaced between the press and the first motion (hold, Backspace,
    /// move) would make the drag carry the parent listing's row at that index.
    pressed: Option<(String, i32, i32)>,
    drag: Option<Entry>,
    open: Option<String>,
    state_requested: Option<u32>,
    move_requested: bool,
    resize_requested: Option<u32>,
    closing: bool,
    new_window: bool,
    /// A quit has been asked for; the binary owns what it means, knowing how many windows exist.
    quit: bool,
    /// Which menu is open, where each bar word sits, and where the keyboard is inside it.
    ///
    /// The window it drops into is the binary's, opened and destroyed with this.
    pub menus: MenuState,
    /// A name being typed, and what it is for. `Some` is a mode: the keys belong to the field and
    /// the strip shows it in place of the path.
    prompt: Option<(Action, Target, TextFieldState)>,
    /// What a delete is asking about. A question, not a prompt: removal cannot be undone and a
    /// typed name confirms nothing.
    pub(super) confirm: Option<Target>,
    /// Whether the *dialog* holds the keyboard, which its own title bar shows.
    pub dialog_focused: bool,
    /// What the wall clock said when this listing was read, in nanoseconds since the epoch.
    ///
    /// The binary reads it so `view` stays testable: the `Modified` column needs today's date.
    /// Zero means "not read", and then every row shows a date.
    pub now_nanos: u64,
    confirm_move_requested: bool,
    /// Filesystem work the binary owes, in order: a queue, as a paste of four files is four
    /// operations that can each fail.
    ops: Vec<FileOp>,
    over: Option<usize>,
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
            // Zero: `set_anchors` sizes this every frame before anything reads it.
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
    pub(super) fn pane(&self) -> &Pane {
        self.panes.iter().find(|p| p.key == self.current).unwrap_or(&self.panes[0])
    }

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
    /// The pane keeps an *entries* index while `ListState::selected` is a row position, and they
    /// differ while a search narrows the listing; this and the translation around `list_view`
    /// are the only places the two meet.
    fn entry_of_row(&self, row: usize) -> Option<usize> {
        self.visible_indices().get(row).copied()
    }

    /// Move the selection one row down (`1`) or up (`-1`), through the rows that are shown: a
    /// filtered row position must not land in a field that holds an entries index.
    fn step_selection(&mut self, by: i32) {
        let shown = self.visible_indices();
        if shown.is_empty() {
            return;
        }
        let here = self.pane().list.selected.and_then(|e| shown.iter().position(|&i| i == e));
        let next = match (here, by) {
            (Some(p), 1) => (p + 1).min(shown.len() - 1),
            (Some(p), _) => p.saturating_sub(1),
            (None, 1) => 0,
            (None, _) => shown.len() - 1,
        };
        self.pane_mut().list.selected = Some(shown[next]);
    }

    /// Which entries the listing shows, as indices into [`entries`](Self::entries).
    ///
    /// Indices, since a row's key is `LIST_ROW_KEY + its index` and opening, dragging, renaming
    /// and the marked set resolve through it. Matching is a case-insensitive substring, the
    /// Applications menu's rule.
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
    /// Sorted here, not by the caller, as the order is the browser's. The selection resets to
    /// the first row: `list_view` clamping a stale index would select whatever sits there.
    pub fn show(&mut self, path: &str, mut entries: Vec<Entry>) {
        // The comparison is `libfs`'s, shared with the chooser so both show one directory in one
        // order.
        let p = self.pane_mut();
        // Hidden entries are dropped here, not at draw time: a row's key and `selected` are both
        // indices into `entries`, and a view over a longer list would number a row twice.
        if !p.show_hidden {
            entries.retain(|e| !e.name.starts_with('.'));
        }
        libfs::sort(&mut entries, p.order);
        p.path = String::from(path);
        p.entries = entries;
        p.list = ListState::at((!p.entries.is_empty()).then_some(0), 0);
        self.marked.clear();
        self.anchor = None;
        // The click run ends here too: the second click of a double click navigates, and a third
        // moments later would be run 3 and open whatever is now row 0.
        self.clicks.reset();
        self.click_run = 1;
        // A listing supersedes the last row press's notice, unless a drive's going asked for it.
        self.notice = self.arrival_notice.take();
        // A question about a directory you have left is not worth keeping: a prompt replaces the
        // path in the strip, so nothing would say where the name is about to land. A click on a
        // *file* row re-selects without re-listing, which this never sees.
        self.prompt = None;
        self.confirm = None;
    }

    /// Turn a wire entry into a row; only an entry with an empty name is dropped.
    ///
    /// An unknown kind or a symlink is listed as a file: it shows a name and is inert when
    /// pressed, but something is on disk and the browser should say so.
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
        // Choosing dismisses the menu, whichever row it was. Asked of the table, not repeated in
        // the arms, so a row added later cannot forget. A message that is not a row leaves it
        // alone, which keeps `MenuBar` able to open one.
        if self
            .menu_table()
            .iter()
            .flat_map(|m| m.items.iter())
            .any(|it| matches!(it, Item::Action { msg: m, .. } if *m == msg))
        {
            self.menus.close();
        }
        match msg {
            // A single click selects; the second opens, so a file can be pointed at without
            // acting on it. The message carries the row's *key*: `checked_sub`, not saturating,
            // so a stray key does not act on the first row.
            Msg::Press(key) => {
                let Some(i) = list_row_of(key) else { return };
                if i >= self.pane().entries.len() {
                    return;
                }
                self.notice = None;
                let mods = self.click_mods;
                if mods & MOD_CTRL != 0 {
                    self.toggle_mark(i);
                } else if mods & MOD_SHIFT != 0 {
                    self.mark_range_to(i);
                } else {
                    self.marked.clear();
                    self.anchor = Some(i);
                    self.pane_mut().list.selected = Some(i);
                    if self.click_run >= 2 {
                        self.update(Msg::Activate(i as u64));
                    }
                }
            }
            Msg::Activate(i) => {
                let i = i as usize;
                self.pane_mut().list.selected = Some(i);
                // A directory navigates; anything else, including a row of unknown kind, is
                // opened by asking the shell (`Desktop::Open`): a browser holds no authority to
                // spawn a program.
                match self.child(i) {
                    Some(to) => self.goto = Some(to),
                    None => self.open = self.full(i),
                }
            }
            Msg::Grab(key) => {
                self.pressed =
                    list_row_of(key).and_then(|i| self.pane().entries.get(i)).map(|e| (e.name.clone(), 0, 0));
            }
            Msg::Scroll(p) => {
                let (h, total) = (self.list_h(), self.visible_indices().len());
                let bar = self.pane().list.bar(h, ROW_H, total);
                if let Some(offset) = self.scroll_grab.apply(bar, h, p) {
                    self.pane_mut().list.offset = offset as usize;
                }
            }
            Msg::Wheel(p) => self.pane_mut().list.wheel(p.wheel),
            Msg::Up => {
                let up = parent(&self.pane().path);
                // The root is its own parent, so this is a no-op there; the control is never
                // disabled, which would be one more state to draw.
                if up != self.pane().path {
                    self.goto = Some(up);
                }
            }
            Msg::Key(k) => self.key(k),
            Msg::DragWindow => self.move_requested = true,
            Msg::DragConfirm => self.confirm_move_requested = true,
            Msg::ResizeWindow(edges) => self.resize_requested = Some(edges),
            Msg::Close => self.closing = true,
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
            // A new tab opens where you are, not at `HOME`.
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
                self.goto = Some(here);
            }
            Msg::CloseTab(k) => self.drop_tab(k),
            // The path was resolved when the question was asked, not composed from `self.path`
            // now: the parent window may have navigated while the dialog was up.
            Msg::ConfirmDelete => {
                if let Some(t) = self.confirm.take()
                    && let Some(path) = t.from
                {
                    self.ops.push(FileOp::Delete { path, dir: t.is_dir });
                }
            }
            // Says so: a dialog that vanishes unchanged looks like one that took the other answer.
            Msg::KeepIt => {
                if self.confirm.take().is_some() {
                    self.notice = Some(String::from("not deleted"));
                }
            }
            // Seeded with where the tab is, so the common edit is to a path's tail.
            Msg::OpenLocation => {
                self.prompt = None;
                self.notice = None;
                self.search = None;
                self.location = Some(TextFieldState::with_text(&self.pane().path));
            }
            // Opened empty: a search seeded with the directory's name would match nothing.
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
            // The button before the row, its range being above the rows'. Only a removable drive
            // is ejected, asked again here because a key is a number anything could send.
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
                // Resolved to absolute paths now: whoever pastes them reads them in their own
                // directory.
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
                let to = if typed.starts_with('/') { typed } else { join(&self.pane().path, &typed) };
                self.location = None;
                self.goto = Some(to);
            }
            // Re-sorted in place; the selection follows the *file*, not the row.
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
                    // The anchor follows too, or a later Shift-click extends from wherever that
                    // row now sits.
                    self.anchor = self.pane().list.selected;
                }
            }
            Msg::ToggleHidden => {
                let p = self.pane_mut();
                p.show_hidden = !p.show_hidden;
                self.goto = Some(self.pane().path.clone());
            }
            Msg::RequestState(s) => self.state_requested = Some(s),
            Msg::WindowState(s) => {
                if s == WINDOW_STATE_MAXIMIZED || s == WINDOW_STATE_NORMAL {
                    self.maximized = s == WINDOW_STATE_MAXIMIZED;
                }
            }
        }
    }

    /// Act on a menu row.
    ///
    /// An operation with nothing selected is answered ("nothing is selected"), not ignored, even
    /// though the row is greyed: a caller reaching `choose` another way gets the same answer.
    fn choose(&mut self, a: Action) {
        let selected = self.pane().list.selected.and_then(|i| self.pane().entries.get(i)).cloned();
        if a.needs_selection() && selected.is_none() {
            self.notice = Some(String::from("nothing is selected"));
            return;
        }
        let target = Target {
            dir: self.pane().path.clone(),
            from: selected.as_ref().map(|e| join(&self.pane().path, &e.name)),
            name: selected.as_ref().map(|e| e.name.clone()).unwrap_or_default(),
            is_dir: selected.as_ref().is_some_and(|e| e.is_dir),
        };
        match a {
            Action::Delete => self.confirm = Some(target),
            Action::Properties => {
                self.properties = selected.clone().map(|e| (self.pane().path.clone(), e));
            }
            // The field starts empty: pre-filling would need a selection and a caret to be useful.
            // The location bar closes too: `key` checks it first and `view` draws the prompt
            // first, so with both open the keyboard would feed the field nobody can see.
            _ => {
                self.location = None;
                self.prompt = Some((a, target, TextFieldState::new()));
            }
        }
    }

    /// Finish the open prompt, turning what was typed into an operation.
    ///
    /// A name, not a path: a separator would write outside the directory being shown.
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
        let to = join(&target.dir, &name);
        let from = target.from.clone();
        let queued = match a {
            Action::NewFile => Some(FileOp::Create { path: to, dir: false }),
            Action::NewFolder => Some(FileOp::Create { path: to, dir: true }),
            Action::Rename => from.map(|from| FileOp::Rename { from, to }),
            Action::Duplicate => from.map(|from| FileOp::Copy { from, to, dir: target.is_dir }),
            Action::Delete | Action::Properties => None,
        };
        self.ops.extend(queued);
        self.prompt = None;
    }

    /// Move `carried` into the directory at row `i`.
    ///
    /// Only onto a directory, and never onto itself: a directory moved beneath itself is refused.
    fn drop_on(&mut self, i: usize, carried: &str) {
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
    /// One place, because two derivations disagree and a drop lands a row off.
    pub fn list_top(&self) -> u32 {
        libui::widget::WINDOW_CONTENT_Y + TITLE_BAR_H + MENU_BAR_H + self.tab_strip_h() + PATH_H + HEADER_H
    }

    /// How many rows the list actually draws, from the height `list_view` is given. Not
    /// `entries.len()`: bounding a drop by entries moved a file into a folder nobody could see.
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
        self.entry_of_row(row)
    }

    /// Whether a window-local point is inside this window at all.
    fn inside(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && (x as u32) < self.window.w && (y as u32) < self.window.h
    }

    /// Arrow keys move the selection; Enter activates it; Backspace goes up.
    ///
    /// The keyboard reaches the same messages the pointer does, so Enter and a row press agree.
    fn key(&mut self, k: KeyEvent) {
        if k.pressed != KEY_DOWN && k.pressed != KEY_REPEAT {
            return;
        }
        // The tab chords, matched against the menu table so a label and its binding cannot
        // disagree. Before the prompt check: the field's branch returns first and would swallow
        // them. Every other Ctrl chord is swallowed rather than folded into a printable character.
        if k.modifiers & MOD_CTRL != 0 {
            if let Some(msg) = libui::menu::accel_match(&self.menu_table(), &k) {
                self.update(msg);
            }
            return;
        }
        // While a search is typed the keys are the field's: `Backspace` correcting a query must
        // not go up a directory.
        if self.search.is_some() {
            match k.keycode {
                libkern::abi::KEY_ESC => self.update(Msg::SearchCancel),
                // `Activate` takes an entries index. It opens what is selected, falling back to
                // the first match, since arrows move the selection while a search is up.
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
                // Arrows move the selection through what is shown, so the highlight and the file
                // an action acts on stay the same file.
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
        // While a path is typed the keys are the location bar's, for the same reason.
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
        // While a name is typed the keys are the field's, arrows and Backspace included.
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
    /// Called before the event is routed, so the message the press produces can ask what number
    /// the click was; the binary owns the clock.
    pub fn note_press(&mut self, at: libdraw::geom::Point, at_ms: u64, modifiers: u16) {
        self.click_run = self.clicks.press(at, at_ms);
        self.click_mods = modifiers;
    }

    /// Abandon the click run — the gesture turned out to be a drag.
    ///
    /// Or the next click after a drag opens something.
    pub fn note_drag(&mut self) {
        self.clicks.reset();
        self.click_run = 1;
    }

    /// The listing the binary was asked for could not be read.
    ///
    /// Said out loud ("no such directory"): a typed path that does not exist is the ordinary
    /// mistake, and silence looks like a key that did not register.
    pub fn list_failed(&mut self, path: &str) {
        self.notice = Some(alloc::format!("no such directory: {path}"));
    }

    /// The selected row's name, if a row is selected, for the binary's receipt: a click selects
    /// rather than opens, so it has no outward sign unless something says so.
    pub fn picked_name(&self) -> Option<String> {
        let p = self.pane();
        p.list.selected.and_then(|i| p.entries.get(i)).map(|e| e.name.clone())
    }

    /// The drives, as the storage service's table now lists them. A drive that has gone takes
    /// every tab inside it home: the current one by a listing the binary owes, the rest when
    /// chosen. The strip says it was ejected if this window ejected it, else that it is gone.
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
    /// unmounted on that stick, or its reason. It is said here, and again when the table drops it.
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

    /// The common locations the sidebar offers — `libfs::places` for this window's home, the
    /// function the shell's Places menu asks too.
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
    /// One operation per path, queued: four files are four things that can each fail. `main`
    /// refuses a destination that is taken.
    pub fn pasted(&mut self, verb: librsproto::clipboard::PathVerb, paths: &[(&str, bool)]) {
        let dir = self.pane().path.clone();
        let mut queued = 0usize;
        for (from, is_dir) in paths {
            let name = libfs::basename_str(from);
            if name.is_empty() {
                continue;
            }
            let to = join(&dir, name);
            // A paste into the directory a file is already in is skipped: for a copy it would
            // truncate the file it was reading.
            if *from == to {
                continue;
            }
            queued += 1;
            self.ops.push(match verb {
                librsproto::clipboard::PathVerb::Cut => FileOp::MoveInto { from: String::from(*from), to },
                // Whether it is a directory comes from the binary, which asked the filesystem:
                // `copy_file` on a folder fails.
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
    /// The row the list state holds is part of the selection, so toggling it off empties it.
    fn toggle_mark(&mut self, i: usize) {
        let Some(name) = self.pane().entries.get(i).map(|e| e.name.clone()) else { return };
        if self.pane().list.selected == Some(i) {
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
        // Between the two rows, not the two entries: with a filter up, a sweep over entries would
        // pick files nobody can see.
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
    pub(super) fn selected_name(&self) -> Option<&str> {
        let p = self.pane();
        p.list.selected.and_then(|i| p.entries.get(i)).map(|e| e.name.as_str())
    }

    /// Every entry picked right now, in listing order: the one answer the Edit menu acts on.
    pub fn selection(&self) -> Vec<&Entry> {
        self.pane()
            .entries
            .iter()
            .enumerate()
            .filter(|(i, e)| self.pane().list.selected == Some(*i) || self.marked.contains(&e.name))
            .map(|(_, e)| e)
            .collect()
    }

    /// The listing's rows as the tree keys them, for the test that pins them apart from the
    /// chrome's.
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
    /// Called with window-local coordinates; the first motion after the press fixes the origin.
    pub fn pointer_moved(&mut self, x: i32, y: i32, buttons: u16) -> Gesture {
        if buttons == 0 {
            // The button came up, which alone completes an internal drag: a release over a
            // different row produces no `Msg`, so the gesture is read from the record.
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
        // An internal drag stays this window's until the pointer leaves it: the compositor skips
        // the source window as a drop target (`compositor::input::highlight_target`).
        if self.dragging {
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
        if !self.pane().entries.iter().any(|e| e.name == name) {
            self.pressed = None;
            return Gesture::None;
        }
        self.dragging = true;
        // The click run ends the moment this becomes a drag, here and not in the binary, which
        // cannot see it: a run left open makes the next click number two.
        self.note_drag();
        // The motion that crosses the slop can also be the one that leaves; a drag armed and
        // never handed over got nothing, and `check-login` saw that as a flake. Delegating keeps
        // one meaning of "left the window".
        if !self.inside(x, y) {
            return self.drag_moved(x, y, &name);
        }
        self.over = self.row_at(y).filter(|&i| self.pane().entries.get(i).is_some_and(|e| e.is_dir));
        Gesture::Moved
    }

    /// What a motion means to a drag already under way: still inside, or handed over.
    fn drag_moved(&mut self, x: i32, y: i32, name: &str) -> Gesture {
        if !self.inside(x, y) {
            // Handed over at the edge, not at the slop: the compositor runs the rest of the drag
            // and the client goes blind, so this is the last thing the window decides.
            self.pressed = None;
            self.dragging = false;
            self.over = None;
            self.drag = self.pane().entries.iter().find(|e| e.name == name).cloned();
            return if self.drag.is_some() { Gesture::HandOff } else { Gesture::None };
        }
        let was = self.over;
        // Only a directory is a target, so the highlight never offers a landing place that does
        // nothing.
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
    /// The shell answers before the program has drawn, so the only thing to report is whether
    /// the *request* was taken; otherwise a shell that will not launch is silent here.
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
    /// One method with one count (the bar's menus), testable on the host: a count kept in the
    /// binary drifted and *View* opened a popup with nowhere to go.
    pub fn place_menus(&mut self, view: &Element<Msg>, l: &libui::layout::Layout) {
        let n = self.menu_count();
        self.menus.set_anchors((0..n).map(|i| libui::layout::locate(view, l, MENU_BAR_KEY + i as u64)).collect());
    }

    /// How many menus the bar has — derived from the table, never declared: a constant here
    /// drifted from `menu_table` and a bar word did nothing, with no error anywhere.
    pub fn menu_count(&self) -> usize {
        self.menu_table().len()
    }

    /// The bar's menus, in bar order.
    ///
    /// Built from `&self` because half the rows depend on state: the ones that act on a
    /// selection are disabled without one, and Close Tab on the last tab.
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
                        .enabled(self.panes.len() > 1),
                    Item::new("Go to Location\u{2026}", Accel::ctrl(LOCATION_KEYCODE, "L"), Msg::OpenLocation),
                    Item::Separator,
                    Item::new("New Window", Accel::ctrl_shift(NEW_WINDOW_KEYCODE, "N"), Msg::NewWindow),
                    Item::new("Quit", Accel::ctrl(QUIT_KEYCODE, "Q"), Msg::Quit),
                    Item::Separator,
                    act("New File", Action::NewFile),
                    act("New Folder", Action::NewFolder),
                    act("Rename", Action::Rename),
                    // In `deny`: the one row here that destroys something. It still asks first.
                    act("Delete", Action::Delete).destructive(true),
                    // Last, after the destructive row: it changes nothing, so a mis-aimed press
                    // lands on it instead of Delete.
                    Item::new("Properties", Accel::ctrl(PROPERTIES_KEYCODE, "I"), Msg::Choose(Action::Properties))
                        .enabled(selected),
                ],
            },
            // Cut, Copy and Paste move a path through `/dev/clipboard`; Duplicate asks for a name
            // and writes a file.
            Menu {
                title: "Edit",
                items: vec![
                    Item::new("Cut", Accel::ctrl(CUT_KEYCODE, "X"), Msg::CutFiles).enabled(selected),
                    Item::new("Copy", Accel::ctrl(COPY_KEYCODE, "C"), Msg::CopyFiles).enabled(selected),
                    // Paste is offered whatever is on the ring: reading it is a syscall this
                    // crate does not make. A paste of text says so instead.
                    Item::new("Paste", Accel::ctrl(PASTE_KEYCODE, "V"), Msg::PasteFiles),
                    Item::Separator,
                    act("Duplicate\u{2026}", Action::Duplicate),
                ],
            },
            // The four orders are a radio group and the last row a toggle, marked from the tab
            // whose menu this is.
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
                    // In View: this row changes which rows are shown, and `check-login` indexes
                    // the File menu's rows, so one wedged into it would move a gate's aim.
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
    /// Nothing is removed: a window that failed to appear must not decide anything.
    pub fn confirm_failed(&mut self) {
        self.confirm = None;
        self.notice = Some(String::from("could not ask — nothing was deleted"));
    }

    /// What the dialog does with a key: `Esc` keeps the entry, and nothing else answers. No key
    /// deletes: `Enter` would be the obvious accident.
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

    /// Which dialog this window has open, if any; the binary reconciles its one window against
    /// this, so a dialog opened over another replaces it.
    pub fn dialog(&self) -> Option<Dialog> {
        // The question wins if both are somehow set: it cannot be dismissed by ignoring it.
        if self.confirm.is_some() {
            Some(Dialog::Confirm)
        } else {
            self.properties.as_ref().map(|_| Dialog::Properties)
        }
    }

    /// The open dialog's tree; empty when nothing is open, which cannot be drawn.
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
            Some(Dialog::Properties) => {
                if k.pressed != KEY_DOWN && k.pressed != KEY_REPEAT {
                    return None;
                }
                matches!(k.keycode, libkern::abi::KEY_ESC | libkern::abi::KEY_ENTER).then_some(Msg::CloseProperties)
            }
            None => None,
        }
    }

    /// What a manager asking the *dialog's window* to close means: the cautious answer, which
    /// for a question is the one that changes nothing.
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
            // Nothing was going to change, but a failed window must not stay recorded as open.
            Some(Dialog::Properties) => {
                self.properties = None;
                self.notice = Some(String::from("could not show the properties"));
            }
            None => {}
        }
    }

    /// How many characters have been typed into the name prompt, or `None` when none is open.
    ///
    /// The receipt for typing: a gate has no rendered field to read, and a dropped keystroke
    /// otherwise arrives as a file with the wrong name, three steps later.
    pub fn prompt_len(&self) -> Option<usize> {
        self.prompt.as_ref().map(|(_, _, f)| f.text().chars().count())
    }

    /// What the location bar holds, if it is open, for the binary's receipt.
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
    /// Both numbers when both happened: "copied" after three refusals would say files arrived
    /// that did not.
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
    /// The notice is set whichever way the operation went: a silent success and a silent failure
    /// look identical. The caller re-lists afterwards.
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
    /// Every `Configure` is accepted: the listing just gets more or fewer visible rows.
    pub fn resize(&mut self, size: Size) -> bool {
        if size == self.window {
            return false;
        }
        self.window = size;
        true
    }

    /// The height the listing is laid out at — the window less its chrome and the grip's corner,
    /// since the grip is the topmost layer and takes any press under it.
    pub fn list_h(&self) -> u32 {
        // `window_frame` insets the content below the title bar by `WINDOW_FRAME_H`.
        self.window.h.saturating_sub(
            TITLE_BAR_H + MENU_BAR_H + self.tab_strip_h() + PATH_H + HEADER_H + STATUS_BAR_H + WINDOW_FRAME_H,
        )
    }

    /// How tall the tab strip is in this window — the widget's height, or nothing at all.
    ///
    /// One tab is no strip. Every sum over the window's chrome goes through here, so the
    /// listing's height and a gate's row arithmetic agree about whether it is there.
    pub fn tab_strip_h(&self) -> u32 {
        if self.tabs().len() > 1 { TAB_STRIP_H } else { 0 }
    }

    /// The window's foot: how many entries are in this directory, and what is picked.
    fn status_bar(&self, ui: &UiTheme) -> Element<Msg> {
        // What is shown, not what is there; while a search narrows, both numbers are said.
        let shown = self.visible_indices().len();
        let total = self.pane().entries.len();
        let count = match (self.search.is_some(), shown) {
            (true, n) => alloc::format!("{n} of {total} items"),
            (false, 1) => String::from("1 item"),
            (false, n) => alloc::format!("{n} items"),
        };
        let mut left = alloc::vec![status_text(count, ui)];
        // What is selected, after a separator.
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
    /// `&mut self` because building the view scrolls the list: `list_view` scrolls its state to
    /// follow the selection, and a view that dropped the result would re-derive the offset from
    /// zero each frame. The theme is the caller's, as the caller paints this tree.
    pub fn view(&mut self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        let title = title_bar(
            TITLE,
            // Where this window is looking.
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
                // A close button: without one every close takes the wedged path (a second
                // middle-click on the taskbar destroys the window with `Manage::Close`).
                close: Some(Msg::Close),
            },
            hovered,
            ui,
        )
        .key(TITLE_KEY);

        // The menu bar: each word drops a list of things to do.
        let bar =
            libui::menu::bar(&self.menu_table(), &self.menus, MENU_BAR_KEY, hovered, Msg::MenuBar, ui, MENU_BAR_H);

        // The path strip: where you are and the one control that leaves it, or the name being
        // typed, which replaces the path: a prompt is what is happening.
        let middle = match self.prompt.as_ref() {
            // The notice is beside the field, not behind it: a rejected name leaves the prompt
            // open, and an explanation in a slot the prompt replaced was written and never drawn.
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
            // The location bar replaces the path with a field showing the same text, so it reads
            // as editing where you are.
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
            // The path as segments, the separators quieter than the names.
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
        // The search field only while it is open: `Ctrl+F` opens it, `Esc` closes it, `Enter`
        // opens what is left.
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

        // The tab strip, above the path because a tab *is* a path.
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

        // The rows the filter leaves, by index into the whole listing: a row's key carries its
        // original index, so actions resolve to the same file whether or not a search narrows.
        let shown = self.visible_indices();
        let labels: Vec<String> = shown.iter().map(|i| self.pane().entries[*i].label()).collect();
        // The three facts beside each name, as owned strings first since a `ListRow` borrows.
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
                // The design's mark: a folder in the accent, a file in the line colour.
                swatch: Some(Swatch::block(if entry.is_dir { ui.accent } else { ui.border })),
                button: None,
            });
        }
        let h = self.list_h();
        // `Grab` on the press, `Activate` on the click: a drag is decided when the button lands.
        // A row an internal drag is over borrows the hover face, replacing the pointer's own
        // hover: the pointer holds something and only where it lands matters.
        let highlight = match self.over {
            Some(i) => Some(LIST_ROW_KEY + i as u64),
            None => hovered,
        };
        // The sidebar's highlight is derived each frame from where this tab is, never stored.
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
                // A dot per place, and `Root`'s is the deny colour: the only warning before
                // walking out of one's home into the system's.
                swatch: Some(Swatch::dot(if p.path == "/" { ui.deny } else { ui.ok })),
                button: None,
            })
            .collect();
        self.sidebar.selected = here;
        // The panel is built for the height it will be drawn at, the box minus the margins.
        let side_h = (h + HEADER_H).saturating_sub(2 * SIDEBAR_PAD);
        // The places take the whole column until there is a drive, then as many rows as they
        // have; never all of it, so a drive row stays under its heading in a short window.
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
            // A ground of its own: a sidebar in the list's ground reads as a list with a gap.
            Some(ui.sidebar),
            ui,
        );
        let mut side = alloc::vec![sized(Size::new(0, places_h), places).key(PLACES_KEY)];
        if !self.drives.is_empty() {
            // Every mount under `/storage`, by its mount name; a removable one has an eject
            // button, and the row is highlighted where the tab is.
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

        // Into the widget's space and back: the pane holds an entries index, `list_view` means a
        // row position, so with a filter up the highlight and the acted-on file would differ.
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
        // On the returned element: a wheel bubbles to the nearest `on_wheel`, and the outermost
        // node is the region meant by "over the list".
        .on_wheel(Msg::Wheel);
        // Back to an entries index. A selection the filter hides has no position and keeps its
        // stored index, so closing the search restores the highlight.
        let after = self.pane().list.selected;
        self.pane_mut().list.selected = match (position, after) {
            (None, _) => stored,
            (Some(_), Some(p)) => shown.get(p).copied(),
            (Some(_), None) => None,
        };

        // The strip is left out, not sized to nothing: a zero-height box still paints its
        // children over the chrome below.
        let mut edges = alloc::vec![docked(Edge::Top, sized(Size::new(0, MENU_BAR_H), bar).key(BAR_KEY)),];
        if self.tab_strip_h() > 0 {
            edges.push(docked(Edge::Top, sized(Size::new(0, TAB_STRIP_H), tabs).key(TAB_STRIP_KEY)));
        }
        edges.extend(alloc::vec![
            docked(Edge::Top, sized(Size::new(0, PATH_H), strip).key(STRIP_KEY)),
            // The foot, docked before the sidebar so it spans the whole window.
            docked(Edge::Bottom, self.status_bar(ui).key(STATUS_KEY)),
            // Docked after the strips, so it starts below them: a dock takes its edges in order.
            docked(
                Edge::Left,
                sized(
                    // The whole column, header included, or its ground stops short of the status
                    // bar.
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
                // Sized to the height it was built for: `list_view` does not size itself, and the
                // dock's flex child would otherwise draw it at another height than it built rows
                // for.
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
