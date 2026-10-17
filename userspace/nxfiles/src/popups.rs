//! The popups' faces: the menu, a confirmation, properties.

use super::*;

impl App {
    /// The open menu's contents — the popup's whole face.
    ///
    /// `hovered` comes from the popup's own router, a different tree from this window's. Which menu
    /// is a parameter, not read from state: `Msg::Choose` clears `self.menu` before the window
    /// closes, so a later event from an *Edit* popup would route against the File tree.
    pub fn menu_view(&self, which: usize, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        let menus = self.menu_table();
        match menus.get(which) {
            Some(m) => libui::menu::popup(m, &self.menus, MENU_ROW_KEY, hovered, ui),
            // Not reachable while a popup exists, but the type demands an answer.
            None => popup_frame(padding(Insets::all(2), libui::element::text("")), ui),
        }
    }

    /// The confirmation dialog's tree — a second window, and the browser's only question.
    ///
    /// The frame and aim points are `libui`'s, shared with the editor's confirmation. The
    /// destructive answer is on the left, matching the editor's *discard*, so the safe half stays
    /// in one place across windows.
    pub fn confirm_view(&self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        let name = self.confirm.as_ref().map(|t| t.name.as_str()).unwrap_or_default();
        let what = if self.confirm.as_ref().is_some_and(|t| t.is_dir) {
            "Delete this folder and everything in it?"
        } else {
            "Delete this file?"
        };
        let title = title_bar(
            "Delete",
            None,
            self.dialog_focused,
            Msg::DragConfirm,
            // One button, and it is the cautious answer: closing a question must not perform it.
            TitleButtons { minimise: None, maximise: None, close: Some(Msg::KeepIt) },
            hovered,
            ui,
        )
        .key(CONFIRM_TITLE_KEY);
        let question = padding(
            Insets::all(libui::widget::DIALOG_PAD),
            column(alloc::vec![text(String::from(what)), text(String::from(name))]),
        )
        .key(CONFIRM_TEXT_KEY);
        let answer = |label: &str, msg: Msg, key: u64| {
            button(label, msg, WidgetState { hovered: hovered == Some(key), ..Default::default() }, ui).key(key).flex(1)
        };
        let buttons = with_spacing(
            row(alloc::vec![
                answer("delete", Msg::ConfirmDelete, CONFIRM_DELETE_KEY),
                answer("keep it", Msg::KeepIt, CONFIRM_KEEP_KEY),
            ]),
            DIALOG_GAP,
        );
        dialog_frame(title, question, buttons, self.dialog_focused, ui)
    }

    /// What is known about one entry: where it is, what it is, how big, when it changed.
    pub fn properties_view(&self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        let Some((dir, e)) = self.properties.as_ref() else {
            return libui::widget::popup_frame(text(String::new()), ui);
        };
        let title = title_bar(
            "Properties",
            None,
            self.dialog_focused,
            Msg::DragConfirm,
            TitleButtons { minimise: None, maximise: None, close: Some(Msg::CloseProperties) },
            hovered,
            ui,
        )
        .key(PROPS_KEY);

        // Every child keyed: an unkeyed one makes the dialog undiffable, and the window opens
        // and never draws.
        let mut rows: Vec<Element<Msg>> = Vec::new();
        let mut line = |i: u64, label: &str, value: String| {
            rows.push(
                row(alloc::vec![
                    sized(Size::new(96, 0), text(String::from(label))).key(PROPS_KEY + 10 + i * 2),
                    text(value).flex(1).key(PROPS_KEY + 11 + i * 2),
                ])
                .key(PROPS_KEY + 2 + i),
            );
        };
        line(0, "Name", e.name.clone());
        line(1, "Where", dir.clone());
        line(2, "Kind", String::from(if e.is_dir { "Folder" } else { "File" }));
        // A folder shows no size: the wire carries `0` for a directory, which would claim it is
        // empty.
        line(3, "Size", if e.is_dir { String::from("—") } else { size_text(e.size) });
        line(4, "Modified", modified_text(e.mtime));

        let body = padding(Insets::all(libui::widget::DIALOG_PAD), column(rows)).key(PROPS_KEY + 1);
        let buttons = row(alloc::vec![
            text("").flex(1).key(PROPS_KEY + 30),
            button(
                "close",
                Msg::CloseProperties,
                WidgetState { hovered: hovered == Some(PROPS_KEY + 31), ..Default::default() },
                ui,
            )
            .key(PROPS_KEY + 31),
        ]);
        libui::widget::dialog_frame_sized(Size::new(PROPS_W, PROPS_H), title, body, buttons, self.dialog_focused, ui)
    }
}
