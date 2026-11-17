//! The menus: the bar's table and where its words sit, and the context menus.

use super::*;

/// What a context menu is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Context {
    /// A listing row, by its key. Its rows act on the selection, which the press made include it.
    Row(u64),
    /// The folder shown: the space below the listing's rows.
    Folder,
    /// A drive in the sidebar, by its row's key.
    Drive(u64),
}

/// A context menu's own state, beside the [`MenuState`] that shows it.
#[derive(Default)]
pub(crate) struct ContextMenus {
    /// What the menu shown is for.
    on: Option<Context>,
    /// Where the last right press landed, window-local: the menu it asks for opens there.
    press: Option<(i32, i32)>,
    /// The Menu key asked for a menu: the selection's, as the next frame finds it.
    keyed: bool,
}

/// The listing's menus: a row's, and the folder's below the rows.
pub(crate) fn listing_menus() -> ListMenus<Msg> {
    ListMenus { row: |k| Msg::ContextMenu(Context::Row(k)), empty: Some(Msg::ContextMenu(Context::Folder)) }
}

/// A drive's menu; a place in the sidebar has none, its one action being the press.
pub(crate) fn drive_menus() -> ListMenus<Msg> {
    ListMenus { row: |k| Msg::ContextMenu(Context::Drive(k)), empty: None }
}

impl App {
    /// Record where each bar word sits, so an open menu knows where to hang from, and open a menu
    /// the Menu key asked for from where its row now is.
    ///
    /// One method with one count (the bar's menus), testable on the host: a count kept in the
    /// binary drifted and *View* opened a popup with nowhere to go.
    pub fn place_menus(&mut self, view: &Element<Msg>, l: &libui::layout::Layout) {
        let n = self.menu_count();
        self.menus.set_anchors((0..n).map(|i| libui::layout::locate(view, l, MENU_BAR_KEY + i as u64)).collect());
        // A drive's menu outlives its drive by a frame at most, rather than staying up empty.
        if matches!(self.menus.shown(), Some(Shown::Context(_))) && self.context_menu().is_none() {
            self.menus.close();
        }
        // A row's bottom-left, or the listing's top-left for the folder's. **This frame's**: the
        // key had the list follow its selection, which this frame's build scrolled into view, and
        // the row is the selection's now, as a key typed after the Menu key may have moved it.
        if !core::mem::take(&mut self.context.keyed) {
            return;
        }
        let on = match self.pane().list.selected {
            Some(i) => Context::Row(LIST_ROW_KEY + i as u64),
            None => Context::Folder,
        };
        let at = match on {
            Context::Row(key) => libui::layout::locate(view, l, key).map(|r| (r.origin.x, r.bottom() as i32)),
            _ => libui::layout::locate(view, l, LIST_KEY).map(|r| (r.origin.x, r.origin.y)),
        };
        if let Some((x, y)) = at {
            self.context.on = Some(on);
            self.menus.open_context(x, y);
        }
    }

    /// Note a pointer event before it is routed: a left press counts toward the click run, and a
    /// right press is where the menu it asks for opens. Any other press ends the run, so a left
    /// click after it, say the one dismissing a menu, is a first click and not a double's second.
    pub fn note_pointer(&mut self, p: &PointerEvent) {
        if p.kind != POINTER_BUTTON || p.flags & POINTER_PRESSED == 0 {
            return;
        }
        match p.button {
            libkern::abi::BTN_LEFT => self.note_press(libdraw::geom::Point::new(p.x, p.y), p.time_ms, p.modifiers),
            button => {
                self.clicks.reset();
                self.click_run = 1;
                if button == libkern::abi::BTN_RIGHT {
                    self.context.press = Some((p.x, p.y));
                }
            }
        }
    }

    /// A right press asked for `on`'s menu: open it where the press landed.
    ///
    /// **Not a press**: a row not in the selection becomes it, alone, but the click run and its
    /// modifiers are not read, so nothing opens and no range moves. A row in it keeps the set,
    /// which Cut and Copy act on, and becomes its primary, the row Rename, Delete, Duplicate and
    /// Properties act on: a marked row is drawn as the primary is, so it must be the one they name.
    pub(super) fn open_context_menu(&mut self, on: Context) {
        let Some((x, y)) = self.context.press.take() else { return };
        match on {
            Context::Row(key) => {
                let Some(i) = list_row_of(key).filter(|&i| i < self.pane().entries.len()) else { return };
                let name = self.pane().entries[i].name.clone();
                if let Some(at) = self.marked.iter().position(|n| *n == name) {
                    self.marked.remove(at);
                    if let Some(primary) = self.selected_name().map(String::from) {
                        self.marked.push(primary);
                    }
                    self.pane_mut().list.selected = Some(i);
                } else if self.pane().list.selected != Some(i) {
                    self.marked.clear();
                    self.anchor = Some(i);
                    self.pane_mut().list.selected = Some(i);
                }
                self.notice = None;
            }
            Context::Folder => {}
            Context::Drive(key) => {
                if !self.drives().iter().any(|d| Some(d.block) == key.checked_sub(DRIVE_ROW_KEY)) {
                    return;
                }
            }
        }
        self.context.on = Some(on);
        self.menus.open_context(x, y);
    }

    /// The Menu key or `Shift+F10`: the selected row's menu, or with none the folder's.
    ///
    /// Opened by the next [`place_menus`](Self::place_menus), for the selection then and from where
    /// its row is then: the list builds only the rows that fit, so a selection scrolled away has
    /// nothing to hang from until the list follows it back.
    pub(super) fn menu_key(&mut self) {
        self.pane_mut().list.follow();
        self.context.keyed = true;
    }

    /// The listing was replaced: a context menu shown was for what it held, or for where it was.
    pub(super) fn close_context_menu(&mut self) {
        if matches!(self.menus.shown(), Some(Shown::Context(_))) {
            self.menus.close();
        }
        self.context.keyed = false;
    }

    /// Whether `msg` is a row of the bar's menus or of the context menu shown: choosing one closes
    /// the menu, whichever row it was.
    ///
    /// Asked of the tables, not repeated in the arms, so a row added later cannot forget. A message
    /// that is not a row leaves the menu alone, which keeps `MenuBar` able to open one.
    pub(super) fn is_menu_row(&self, msg: &Msg) -> bool {
        libui::menu::sends(&self.menu_table(), msg)
            || self.context_menu().is_some_and(|m| libui::menu::sends(&[m], msg))
    }

    /// A key the menu's popup received: arrows, `Esc` and `Enter` drive the menu it shows.
    pub fn menu_key_event(&mut self, k: KeyEvent) {
        let (bar, context) = (self.menu_table(), self.context_menu());
        if let Some(msg) = self.menus.choose(&k, &bar, context.as_ref()) {
            self.update(msg);
        }
    }

    /// The context menu shown, if one is.
    pub fn context_menu(&self) -> Option<Menu<Msg>> {
        if !matches!(self.menus.shown(), Some(Shown::Context(_))) {
            return None;
        }
        self.context_menu_for(self.context.on?)
    }

    /// Where the context menu shown was asked for, window-local, and the name of the row or drive it
    /// is for, or `None` for the folder's: the binary's receipt.
    pub fn context_shown(&self) -> Option<((i32, i32), Option<&str>)> {
        if !matches!(self.menus.shown(), Some(Shown::Context(_))) {
            return None;
        }
        let name = match self.context.on? {
            Context::Row(key) => Some(self.pane().entries.get(list_row_of(key)?)?.name.as_str()),
            Context::Folder => None,
            Context::Drive(key) => {
                let block = key.checked_sub(DRIVE_ROW_KEY)?;
                Some(self.drives().iter().find(|d| d.block == block)?.name.as_str())
            }
        };
        Some((self.menus.context()?, name))
    }

    /// `on`'s menu. **A row the bar has is the bar's row**, taken from its table, so its label, its
    /// chord and whether it is enabled cannot differ from it.
    pub fn context_menu_for(&self, on: Context) -> Option<Menu<Msg>> {
        let bar = self.menu_table();
        let items = match on {
            Context::Row(key) => {
                let open = Item::plain("Open", Msg::Activate(list_row_of(key)? as u64));
                let rows = [
                    Some(Msg::CutFiles),
                    Some(Msg::CopyFiles),
                    Some(Msg::Choose(Action::Duplicate)),
                    None,
                    Some(Msg::Choose(Action::Rename)),
                    Some(Msg::Choose(Action::Delete)),
                    None,
                    Some(Msg::Choose(Action::Properties)),
                ];
                [open, Item::Separator].into_iter().chain(libui::menu::bar_rows(bar, &rows)?).collect()
            }
            Context::Folder => {
                let rows = [
                    Some(Msg::Choose(Action::NewFile)),
                    Some(Msg::Choose(Action::NewFolder)),
                    None,
                    Some(Msg::PasteFiles),
                    None,
                    Some(Msg::ToggleHidden),
                ];
                libui::menu::bar_rows(bar, &rows)?
            }
            // What a press on the row and on its eject button send.
            Context::Drive(key) => {
                let block = key.checked_sub(DRIVE_ROW_KEY)?;
                let d = self.drives().iter().find(|d| d.block == block)?;
                vec![
                    Item::plain("Open", Msg::Drive(key)),
                    Item::plain("Eject", Msg::Drive(DRIVE_EJECT_KEY + block)).enabled(d.removable),
                ]
            }
        };
        Some(Menu { title: "", items })
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
}

#[cfg(test)]
mod tests;
