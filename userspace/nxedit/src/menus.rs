//! The menus: the bar's table and where its words sit, the popup, a pointer over the document, and
//! the context menu it or the keyboard asks for.

use super::*;

/// What a context menu hangs from.
#[derive(Default)]
pub(crate) struct ContextMenu {
    /// The document's top-left in the window, as last laid out: a pointer event over it is
    /// measured from there.
    area: Option<(i32, i32)>,
    /// The Menu key asked for the menu at the caret.
    keyed: bool,
}

impl App {
    /// Record where each bar word sits, so an open menu knows where to hang from, and where the
    /// document is; open a menu the Menu key asked for at the caret, measured with `width` as drawn.
    pub fn place_menus(&mut self, view: &Element<Msg>, l: &libui::layout::Layout, width: impl Fn(&str) -> u32) {
        let n = self.menu_count();
        self.menus.set_anchors((0..n).map(|i| libui::layout::locate(view, l, MENU_BAR_KEY + i as u64)).collect());
        // The document, not `AREA_KEY`'s box, which holds the gutter too.
        let area = libui::layout::locate(view, l, AREA_INNER_KEY);
        self.context.area = area.map(|r| (r.origin.x, r.origin.y));
        // **This frame's caret**: the key had the document follow it, which this frame's build did.
        if !core::mem::take(&mut self.context.keyed) {
            return;
        }
        let Some(r) = area else { return };
        let t = &self.buf().text;
        if let Some((x, y)) = t.point_below(t.cursor(), ROW_H, (r.size.h / ROW_H) as usize, width) {
            self.menus.open_context(r.origin.x + x, r.origin.y + y);
        }
    }

    /// The Menu key or `Shift+F10`: the menu at the bottom-left of the caret's cell, opened by the
    /// next [`place_menus`](Self::place_menus), as only it can measure the caret.
    pub(crate) fn menu_key(&mut self) {
        self.buf_mut().text.follow();
        self.context.keyed = true;
    }

    /// The context menu shown, if one is: the Edit menu's rows that act where the text is, taken
    /// from the bar, so they are greyed while a field is open as the bar's are.
    pub fn context_menu(&self) -> Option<Menu<Msg>> {
        if !matches!(self.menus.shown(), Some(Shown::Context(_))) {
            return None;
        }
        let rows = [
            Some(Msg::Undo),
            Some(Msg::Redo),
            None,
            Some(Msg::Cut),
            Some(Msg::Copy),
            Some(Msg::Paste),
            None,
            Some(Msg::SelectAll),
        ];
        Some(Menu { title: "", items: libui::menu::bar_rows(self.menu_table(), &rows)? })
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

    /// Resolve a pointer event over the document, given a way to measure text as drawn.
    ///
    /// A left press places the caret and drops the selection; a motion with the left button held
    /// extends it. A right press keeps a selection it lands in, or places the caret, then asks for
    /// the menu at the press. Anything else, a middle press too, is ignored. Answers whether it
    /// changed.
    pub fn take_area_pointer(&mut self, width: impl Fn(&str) -> u32) -> bool {
        use librsproto::surface::{POINTER_BUTTON, POINTER_MOTION, POINTER_PRESSED};
        let Some(p) = self.area_pointer.take() else { return false };
        let press = p.kind == POINTER_BUTTON && p.flags & POINTER_PRESSED != 0;
        let pressed = press && p.button == libkern::abi::BTN_LEFT;
        let context = press && p.button == libkern::abi::BTN_RIGHT;
        let dragging = p.kind == POINTER_MOTION && p.buttons & 1 != 0;
        if !pressed && !dragging && !context {
            return false;
        }
        let (line, col) = self.buf().text.at_point(p.x, p.y, ROW_H, &width);
        // The menu's rows act on the selection as it stands after the press, so Cut never takes
        // text away from where the click was: the press is inside only on a selected character.
        if context {
            let on = self.buf().text.char_at(p.x, p.y, ROW_H, &width);
            if !self.buf().text.selection().is_some_and(|(from, to)| on.is_some_and(|c| from <= c && c < to)) {
                self.buf_mut().text.place(line, col);
            }
            if let Some((x, y)) = self.context.area {
                self.menus.open_context(x + p.x, y + p.y);
            }
        } else if pressed {
            self.buf_mut().text.place(line, col);
        } else {
            self.buf_mut().text.extend_to(line, col);
        }
        true
    }

    /// The open menu's popup, framed — the root of a second window, not a layer in this one.
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
}

#[cfg(test)]
mod tests;
