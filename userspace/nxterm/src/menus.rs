//! The menus: the bar's table, where its words sit, the context menu, and the popup.

use super::*;

impl App {
    /// Record where each bar word sits, so an open menu knows where to hang from.
    pub fn place_menus(&mut self, view: &Element<Msg>, l: &libui::layout::Layout) {
        let n = self.menu_count();
        self.menus.set_anchors((0..n).map(|i| libui::layout::locate(view, l, MENU_BAR_KEY + i as u64)).collect());
    }

    /// How many menus the bar has, derived from the table.
    pub fn menu_count(&self) -> usize {
        self.menu_table().len()
    }

    /// The bar's menus, in bar order.
    ///
    /// Built rather than stored; Clear and Reset sit under Edit.
    pub fn menu_table(&self) -> Vec<Menu<Msg>> {
        vec![
            Menu {
                title: "File",
                items: vec![
                    Item::new("New Tab", Accel::ctrl_shift(NEW_TAB_KEYCODE, "T"), Msg::NewTab),
                    Item::new("Close Tab", Accel::ctrl_shift(CLOSE_TAB_KEYCODE, "W"), Msg::CloseTab(self.current)),
                    Item::Separator,
                    Item::new("New Window", Accel::ctrl_shift(NEW_WINDOW_KEYCODE, "N"), Msg::NewWindow),
                    Item::Separator,
                    Item::plain("Close Window", Msg::Close),
                    Item::new("Quit", Accel::ctrl_shift(QUIT_KEYCODE, "Q"), Msg::Quit),
                ],
            },
            Menu {
                title: "Edit",
                items: vec![
                    Item::new("Copy", Accel::ctrl_shift(COPY_KEYCODE, "C"), Msg::Copy)
                        .enabled(self.tab().grid.has_selection()),
                    Item::new("Paste", Accel::ctrl_shift(PASTE_KEYCODE, "V"), Msg::Paste),
                    Item::new("Select All", Accel::ctrl_shift(SELECT_ALL_KEYCODE, "A"), Msg::SelectAll)
                        .enabled(self.tab().grid.line_text(self.tab().grid.top_line()).is_some()),
                    Item::new("Find", Accel::ctrl_shift(FIND_KEYCODE, "F"), Msg::Find),
                    Item::new("Clear Scrollback", Accel::ctrl_shift(CLEAR_KEYCODE, "K"), Msg::ClearScrollback)
                        .enabled(self.tab().grid.oldest_line() < self.tab().grid.top_line()),
                    Item::Separator,
                    Item::plain("Clear", Msg::Clear),
                    Item::plain("Reset", Msg::Reset),
                ],
            },
        ]
    }

    /// The popup of the menu `shown` — the root of a second window, not a layer in this one.
    pub fn menu_view(&self, shown: Shown, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        // Only the context menu shown: a popup closing after a newer one opened routes its last
        // events with its own `shown`, and must not find the newer menu's rows under them.
        let context = self.context_menu().filter(|_| self.menus.shown() == Some(shown));
        let menus = self.menu_table();
        match if let Shown::Bar(i) = shown { menus.get(i) } else { context.as_ref() } {
            Some(m) => libui::menu::popup(m, &self.menus, MENU_ROW_KEY, hovered, ui),
            None => libui::widget::popup_frame(padding(Insets::all(2), libui::element::text("")), ui),
        }
    }

    /// The context menu shown, if one is: the Edit menu's rows but Clear and Reset, which stay on
    /// the bar, taken from it.
    pub fn context_menu(&self) -> Option<Menu<Msg>> {
        if !matches!(self.menus.shown(), Some(Shown::Context(_))) {
            return None;
        }
        let rows = [
            Some(Msg::Copy),
            Some(Msg::Paste),
            None,
            Some(Msg::SelectAll),
            None,
            Some(Msg::Find),
            Some(Msg::ClearScrollback),
        ];
        Some(Menu { title: "", items: libui::menu::bar_rows(self.menu_table(), &rows)? })
    }

    /// The Menu key or `Shift+F10`: the menu at the bottom-left of the cursor's cell, the view
    /// following the output back to the cursor first.
    pub(crate) fn menu_key(&mut self) {
        self.snap_to_bottom();
        let (row, col) = self.tab().grid.cursor();
        let (o, w, h) = (self.grid_origin(), self.metrics.cell_w as i32, self.metrics.cell_h as i32);
        self.menus.open_context(o.x + col as i32 * w, o.y + (row as i32 + 1) * h);
    }
}

#[cfg(test)]
mod tests;
