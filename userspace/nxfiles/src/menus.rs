//! The menus: the bar's table, and where its words sit.

use super::*;

impl App {
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
}
