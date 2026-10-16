//! The popups' faces: the menu, a confirmation, properties.

use super::*;

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
    ///
    /// **A report, not a question**, so it has one button and closing it means the same as
    /// pressing that button. Empty when nothing is being shown, for `dialog_view`'s reason.
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

        // **Every child keyed**, which the diff requires of a parent whose children are keyed at
        // all — an unkeyed one makes the whole dialog undiffable, and that shows up as a window
        // that opens, reports its size and never draws (M14 Part C).
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
        // **A folder's size is not reported rather than reported as zero.** The wire carries `0`
        // for a directory, and "0 bytes" would be a claim about what is inside it.
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
