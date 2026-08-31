//! `nxedit` — the text editor, minus the syscalls.
//!
//! The split `nxterm` and `nxfiles` use: what an editor *is* — a buffer, a name, whether what
//! is on screen matches what is on disk, and what happens when a save fails — is a function of
//! values and host-tests in milliseconds. Reading the file, writing it and pumping events are
//! the binary's.
//!
//! ## The two decisions here
//!
//! **A save that fails keeps the buffer.** That is the whole of the failure design: an editor
//! that loses your work quietly is the one thing an editor must never be, so [`App::saved`]
//! takes a failure and turns it into a *message*, never into a state change. The buffer stays
//! modified, the text stays exactly where it was, and the person can try again somewhere else.
//! The write-and-rename that makes the disk half safe is [`main`](../src/main.rs)'s, because it
//! is syscalls; what is here is the rule that a failure is reported rather than absorbed.
//!
//! **A buffer that could not be read refuses to be written.** Opening a path the editor cannot
//! read shows an empty window, and an empty window saved over a file is that file destroyed —
//! by an editor that never showed its contents. [`App::blocked`] is what a load failure sets,
//! and a blocked buffer declines to save with the reason on screen. A *missing* file is not a
//! failure: opening a path that is not there yet is how a file gets made.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use libdraw::geom::{Rect, Size};
use librsproto::surface::{
    KEY_DOWN, KEY_REPEAT, KeyEvent, MOD_CTRL, RESIZE_BOTTOM, RESIZE_RIGHT,
    WINDOW_STATE_MAXIMIZED, WINDOW_STATE_MINIMIZED, WINDOW_STATE_NORMAL,
};
use libui::element::{
    Edge, Element, Insets, dock, docked, offset, padding, row, sized, stack, text,
};
use libui::widget::{
    GRIP_W, Palette as UiPalette, TITLE_BAR_H, TextAreaState, TitleButtons, WidgetState, button,
    resize_grip, text_area, title_bar,
};

/// The status strip's height in pixels — one row of chrome under the title bar.
pub const STATUS_H: u32 = 24;

/// A text line's height in pixels, matching the toolkit's default theme.
pub const ROW_H: u32 = 20;

/// The element key on the text area.
pub const AREA_KEY: u64 = 1;
/// The element key on the save control.
pub const SAVE_KEY: u64 = 2;
/// The element key on the title bar.
pub const TITLE_KEY: u64 = 3;
/// The element key on the resize grip.
pub const GRIP_KEY: u64 = 4;
/// The element key on the status strip.
///
/// **Every docked child needs one**: the diff requires a container's children to be all keyed
/// or all unkeyed, and one bare sibling makes the whole first frame `MixedKeying` — which in a
/// client like this one means no window ever appears. See `nxfiles`' `STRIP_KEY`, which carries
/// the full account of the two errors and their two symptoms.
pub const STRIP_KEY: u64 = 5;
/// The element key on the status text.
pub const STATUS_KEY: u64 = 6;

/// The window's size in pixels at startup, before any manager places it.
pub const START_SIZE: Size = Size::new(560, 420);

/// The key that saves, with Ctrl held: `s`.
///
/// **A literal, because `libkern::abi` names only the keys with no character** — the arrows,
/// Enter, the modifiers. A letter is a keycode the keymap turns into text, and nothing has
/// needed to name one until a chord did. `31` is `KEY_S` in the Linux set the input stack
/// speaks (`libinput::keymap`, which maps it to `s`/`S`), and the test below pins it to that
/// table rather than to this comment.
pub const SAVE_KEYCODE: u16 = 31;

/// Everything the editor is.
pub struct App {
    /// The absolute path being edited. Never changes: this editor opens one file.
    path: String,
    /// The last component of [`path`](Self::path), for the title bar.
    name: String,
    /// The buffer, and the whole of the editing model — `libui` owns those rules.
    text: TextAreaState,
    /// The buffer's revision when it was last read or written.
    ///
    /// **A number rather than a flag**, because the flag has to be cleared by every path that
    /// makes the buffer match the disk and set by every path that does not — and a save's
    /// success arrives from the binary, one frame after the keystroke that asked for it.
    /// Comparing revisions makes "modified" a derived fact.
    saved_at: u64,
    /// Why this buffer may not be written, or `None` when it may.
    blocked: Option<String>,
    /// What the status strip says.
    status: String,
    /// The window's size in pixels — what the client commits.
    window: Size,
    /// Whether this window holds the keyboard, which the title bar shows.
    pub focused: bool,
    /// This window last asked to be maximised, so its maximise button now asks for normal.
    maximized: bool,
    /// A save the binary owes the filesystem.
    save_requested: bool,
    /// A title-bar button was pressed, and the binary owes the compositor a `RequestState`.
    state_requested: Option<u32>,
    /// The title bar was dragged, and the binary owes the compositor a `StartMove`.
    move_requested: bool,
    /// The grip was pressed, and the binary owes the compositor a `StartResize`.
    resize_requested: Option<u32>,
    /// The editor has been asked to close, and the binary owes an exit.
    closing: bool,
}

/// What can happen to the editor.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Msg {
    /// A key reached the window.
    Key(KeyEvent),
    /// Write the buffer to its path — the save control, or `Ctrl+S`.
    Save,
    /// The title bar was dragged.
    DragWindow,
    /// The resize grip was pressed, for these edges.
    ResizeWindow(u32),
    /// A title-bar button asking the manager for a window state.
    RequestState(u32),
    /// Somebody wants this window gone — its own close button, or the shell asking.
    Close,
}

impl App {
    /// An editor for `path`, with an empty buffer until something is loaded into it.
    pub fn new(path: &str) -> App {
        App {
            path: String::from(path),
            name: basename(path).to_string(),
            text: TextAreaState::new(),
            saved_at: 0,
            blocked: None,
            status: String::from("opening…"),
            window: START_SIZE,
            focused: true,
            maximized: false,
            save_requested: false,
            state_requested: None,
            move_requested: false,
            resize_requested: None,
            closing: false,
        }
    }

    /// The file's contents are `text`: this is now what is on disk.
    pub fn loaded(&mut self, text: &str) {
        self.text = TextAreaState::with_text(text);
        self.saved_at = self.text.revision();
        self.blocked = None;
        self.status = describe(text.len(), "opened");
    }

    /// There is nothing at this path yet, which is not a failure.
    ///
    /// An editor opened on a path that does not exist is how a file gets made, so the buffer
    /// stays empty and writable and the strip says which of the two happened — a person who
    /// meant to open an existing file wants to know they did not.
    pub fn absent(&mut self) {
        self.text = TextAreaState::new();
        self.saved_at = self.text.revision();
        self.blocked = None;
        self.status = String::from("new file");
    }

    /// The file could not be read, so this buffer must not be written over it.
    ///
    /// **The empty window is the danger.** A failed read leaves nothing on screen, and saving
    /// nothing over a file is that file destroyed by an editor that never showed it. So a
    /// blocked buffer stays blocked for the run: what would clear it is a successful read, and
    /// this editor reads once.
    pub fn blocked(&mut self, why: &str) {
        self.text = TextAreaState::new();
        self.saved_at = self.text.revision();
        self.blocked = Some(String::from(why));
        self.status = String::from(why);
    }

    /// The path being edited.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The buffer, for a binary that is about to write it.
    pub fn text(&self) -> String {
        self.text.text()
    }

    /// How many times the buffer has been edited — the receipt a keystroke reached it.
    ///
    /// **The one externally visible sign that typing arrived**, which is what the gate paces on:
    /// an editor's echo is its own window, and a gate driving a *release* image has no rendered
    /// grid to read. A count rather than the text, deliberately — what somebody is typing into
    /// an editor is theirs, and the same rule that keeps the compositor's chord log to the
    /// modifier alone applies here.
    pub fn revision(&self) -> u64 {
        self.text.revision()
    }

    /// Whether what is on screen differs from what was last read or written.
    pub fn modified(&self) -> bool {
        self.text.revision() != self.saved_at
    }

    /// What the status strip is saying.
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Whether this buffer refuses to be written, and why.
    pub fn refusal(&self) -> Option<&str> {
        self.blocked.as_deref()
    }

    /// The save the binary owes, as the bytes to write. Clears the record.
    ///
    /// `None` when nothing asked for one — **or when this buffer is blocked**, which is where
    /// the refusal is enforced rather than merely displayed. The status strip says why, so a
    /// person pressing save on a file that could not be read is answered rather than ignored.
    pub fn take_save(&mut self) -> Option<String> {
        if !core::mem::take(&mut self.save_requested) {
            return None;
        }
        if let Some(why) = &self.blocked {
            self.status = alloc::format!("not saved — {why}");
            return None;
        }
        Some(self.text.text())
    }

    /// A save finished: `Ok(bytes written)`, or `Err(what went wrong)`.
    ///
    /// **A failure changes nothing but the message.** The buffer stays as it is and stays
    /// modified, because the alternative — marking it saved and letting the person close the
    /// window — is the editor losing their work while telling them it did not.
    pub fn saved(&mut self, result: Result<usize, &str>) {
        match result {
            Ok(n) => {
                self.saved_at = self.text.revision();
                self.status = describe(n, "saved");
            }
            Err(why) => self.status = alloc::format!("NOT saved — {why}"),
        }
    }

    /// Apply a message.
    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Key(k) => self.key(k),
            Msg::Save => self.save_requested = true,
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

    /// Editing keys go to the buffer; `Ctrl+S` saves.
    ///
    /// **The chord is checked before the buffer sees the key**, and that ordering is load
    /// bearing: `libinput`'s keymap folds `Ctrl+S` to `0x13`, which `TextAreaState::apply`
    /// declines as unprintable — so the buffer would ignore it today and the editor would still
    /// work. It would stop working the moment the toolkit learned another control character,
    /// and the failure would be a keystroke that both saved and typed something.
    fn key(&mut self, k: KeyEvent) {
        if k.pressed != KEY_DOWN && k.pressed != KEY_REPEAT {
            return;
        }
        if k.modifiers & MOD_CTRL != 0 {
            if k.keycode == SAVE_KEYCODE {
                self.save_requested = true;
            }
            // **Every other chord is swallowed, not passed on.** `Ctrl+X` folding to a printable
            // character would otherwise type it, which is how an editor inserts junk when a
            // person reaches for a shortcut it does not have.
            return;
        }
        self.text.apply(k.keycode, k.modifiers);
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

    /// Whether the editor has been asked to close.
    pub fn closing(&self) -> bool {
        self.closing
    }

    /// The size of this window in pixels.
    pub fn window_size(&self) -> Size {
        self.window
    }

    /// Take the window to `size`. `true` if anything changed.
    pub fn resize(&mut self, size: Size) -> bool {
        if size == self.window {
            return false;
        }
        self.window = size;
        true
    }

    /// The height the text area is laid out at — the window less its chrome and the grip.
    pub fn area_h(&self) -> u32 {
        self.window.h.saturating_sub(TITLE_BAR_H + STATUS_H + GRIP_W)
    }

    /// What the title bar shows: the file's name, marked when the buffer differs from the disk.
    ///
    /// **A leading marker rather than a trailing one**, so it is in the same place whatever the
    /// name's length — a mark that moves is a mark that has to be looked for. The window's
    /// *title* (what the taskbar shows) is set once and stays the name alone: retitling on every
    /// keystroke is a message per keystroke to say something the window itself already shows.
    pub fn title(&self) -> String {
        if self.modified() {
            alloc::format!("* {}", self.name)
        } else {
            self.name.clone()
        }
    }

    /// The element tree for the current state.
    ///
    /// `&mut self` because `text_area` scrolls the state it is handed — see `nxfiles::App::view`
    /// for the bug the value-returning shape shipped.
    pub fn view(&mut self) -> Element<Msg> {
        let ui = UiPalette::default();
        let title = title_bar(
            &self.title(),
            self.focused,
            Msg::DragWindow,
            TitleButtons {
                minimise: Some(Msg::RequestState(WINDOW_STATE_MINIMIZED)),
                maximise: Some(Msg::RequestState(if self.maximized {
                    WINDOW_STATE_NORMAL
                } else {
                    WINDOW_STATE_MAXIMIZED
                })),
                close: Some(Msg::Close),
            },
            &ui,
        )
        .key(TITLE_KEY);

        // The status strip: the one control, and what the last thing that happened was.
        let strip = row(alloc::vec![
            button("save", Msg::Save, WidgetState::default(), &ui).key(SAVE_KEY),
            padding(Insets { top: 4, right: 4, bottom: 4, left: 6 }, text(self.status.clone()))
                .key(STATUS_KEY),
        ]);

        let h = self.area_h();
        let area = text_area(&mut self.text, h, ROW_H, self.focused, &ui);

        let body = dock(
            alloc::vec![
                docked(Edge::Top, title),
                docked(Edge::Top, sized(Size::new(0, STATUS_H), strip).key(STRIP_KEY)),
            ],
            // Sized to the height it was built for, like every scrolling widget in this tree:
            // the dock's flex child otherwise gets whatever is left, and the widget would build
            // rows for one height and be drawn at another.
            sized(Size::new(0, h), area).key(AREA_KEY),
        );

        let grip = offset(
            self.window.w.saturating_sub(GRIP_W) as i32,
            self.window.h.saturating_sub(GRIP_W) as i32,
            resize_grip(Msg::ResizeWindow(RESIZE_RIGHT | RESIZE_BOTTOM), &ui).key(GRIP_KEY),
        );
        stack(alloc::vec![body, grip])
    }
}

/// `"<n> bytes <what>"`, the status strip's one sentence.
fn describe(bytes: usize, what: &str) -> String {
    let mut s = String::new();
    s.push_str(what);
    s.push_str(" — ");
    s.push_str(&bytes.to_string());
    s.push_str(" bytes");
    s
}

/// The last component of a path, or the whole of it if there is no separator.
///
/// `libfs::basename`'s rule on `str` rather than bytes, for the reason `nxfiles` keeps its own
/// `join` and `parent`: this half of the application never sees a path as bytes.
pub fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    }
}

/// The rectangle the window occupies, for the binary's layout call.
pub fn bounds(size: Size) -> Rect {
    Rect::new(0, 0, size.w, size.h)
}

/// The buffer as bytes, with a trailing newline if the last line has text.
///
/// **What a text file is**, and the editor is where the convention is applied rather than in
/// `libfs`: every line in a text file ends with a newline, so a file whose last line does not
/// is a file that `open` and every other reader treats as one line shorter than it looks. A
/// buffer whose last line is empty already ends with one — `TextAreaState::text` joins lines
/// with `\n`, so `"a\n"` is two lines, the second empty.
pub fn to_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::from(text.as_bytes());
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An editor with `hello` open, as if the file had been read.
    fn app() -> App {
        let mut a = App::new("/home/notes.txt");
        a.loaded("hello");
        a
    }

    /// A key press, as the compositor delivers one.
    fn key(a: &mut App, keycode: u16, modifiers: u16) {
        a.update(Msg::Key(KeyEvent::new(1, keycode, KEY_DOWN as u16, modifiers)));
    }

    /// `x`, the one letter these tests type.
    const KEY_X: u16 = 45;
    /// `1` — the key that tells a swallowed chord from an unprintable one.
    const KEY_1: u16 = 2;

    #[test]
    fn a_failed_save_keeps_the_buffer_and_says_so() {
        // **The one rule this application exists to get right.** An editor that loses your work
        // quietly is the one thing an editor must never be, so a failure changes the message and
        // nothing else: the text stays, and it stays *modified*, because a buffer marked saved
        // is one a person can close without being asked.
        let mut a = app();
        key(&mut a, KEY_X, 0);
        let text = a.text();
        assert!(a.modified());

        a.saved(Err("the file could not be replaced"));
        assert_eq!(a.text(), text, "the buffer is what it was");
        assert!(a.modified(), "and it is still unsaved");
        assert!(a.status().contains("NOT saved"), "status was {:?}", a.status());

        // And a save that works clears exactly that.
        a.update(Msg::Save);
        let owed = a.take_save().expect("a save was asked for");
        a.saved(Ok(owed.len()));
        assert!(!a.modified(), "a successful save is what marks it saved");
    }

    #[test]
    fn a_buffer_that_could_not_be_read_refuses_to_be_written() {
        // The danger is the empty window: a failed read shows nothing, and saving nothing over a
        // file is that file destroyed by an editor that never displayed it.
        let mut a = App::new("/home/notes.txt");
        a.blocked("could not be read");
        key(&mut a, KEY_X, 0);
        a.update(Msg::Save);
        assert_eq!(a.take_save(), None, "a blocked buffer owes no write");
        assert!(a.status().contains("not saved"), "status was {:?}", a.status());
        assert_eq!(a.refusal(), Some("could not be read"));
    }

    #[test]
    fn a_missing_file_is_not_a_failure() {
        // Opening a path that is not there is how a file gets made, so the buffer is writable —
        // and the strip says which of the two happened, because a person who meant to open an
        // existing file wants to know they did not.
        let mut a = App::new("/home/new.txt");
        a.absent();
        assert_eq!(a.refusal(), None);
        assert_eq!(a.status(), "new file");
        key(&mut a, KEY_X, 0);
        a.update(Msg::Save);
        assert_eq!(a.take_save().as_deref(), Some("x"), "and it can be written");
    }

    #[test]
    fn the_title_marks_a_modified_buffer_and_stops_marking_it_when_saved() {
        let mut a = app();
        assert_eq!(a.title(), "notes.txt");
        key(&mut a, KEY_X, 0);
        assert_eq!(a.title(), "* notes.txt");
        a.update(Msg::Save);
        let owed = a.take_save().unwrap();
        a.saved(Ok(owed.len()));
        assert_eq!(a.title(), "notes.txt");
        // Edited again after a save is modified again — the revision never goes backwards.
        key(&mut a, KEY_X, 0);
        assert_eq!(a.title(), "* notes.txt");
    }

    #[test]
    fn ctrl_s_saves_and_no_chord_types() {
        let mut a = app();
        key(&mut a, SAVE_KEYCODE, MOD_CTRL);
        assert_eq!(a.text(), "hello", "nothing was typed");
        assert_eq!(a.take_save().as_deref(), Some("hello"), "and a save is owed");

        // **A digit, because a letter proves nothing here.** `libinput`'s keymap folds
        // `Ctrl`+letter to a control byte, which `TextAreaState::apply` already declines — so a
        // test using `Ctrl+X` passes with this guard deleted, which is what the first version of
        // it did. The keymap folds *letters only*: `Ctrl+1` is still `1`, printable, and typed
        // straight into the file by an editor that passes chords through.
        key(&mut a, KEY_1, MOD_CTRL);
        assert_eq!(a.text(), "hello", "a chord this editor does not have types nothing");
        assert_eq!(a.take_save(), None, "and asks for nothing");

        // The same key without the chord is ordinary text, so what is being tested is the
        // modifier and not the key.
        key(&mut a, KEY_1, 0);
        assert_eq!(a.text(), "1hello");
    }

    #[test]
    fn typing_reaches_the_buffer_and_enter_makes_a_line() {
        let mut a = app();
        key(&mut a, libkern::abi::KEY_END, 0);
        key(&mut a, KEY_X, 0);
        key(&mut a, libkern::abi::KEY_ENTER, 0);
        key(&mut a, KEY_X, 0);
        assert_eq!(a.text(), "hellox\nx");
    }

    #[test]
    fn a_key_release_does_nothing() {
        // Both edges arrive; acting on both types every character twice.
        let mut a = app();
        a.update(Msg::Key(KeyEvent::new(1, KEY_X, 0, 0)));
        assert_eq!(a.text(), "hello");
    }

    #[test]
    fn what_is_written_ends_with_a_newline() {
        // **What a text file is.** A last line with no newline reads as one line shorter than it
        // looks to `open` and to every other reader, so the editor applies the convention at the
        // one point it writes bytes.
        assert_eq!(to_bytes("one\ntwo"), b"one\ntwo\n");
        assert_eq!(to_bytes("one\n"), b"one\n", "and does not double it");
        assert_eq!(to_bytes(""), b"", "an empty buffer is an empty file, not a blank line");
    }

    #[test]
    fn the_name_is_the_last_component() {
        assert_eq!(basename("/home/alice/notes.txt"), "notes.txt");
        assert_eq!(basename("/notes.txt"), "notes.txt");
        assert_eq!(basename("notes.txt"), "notes.txt");
        assert_eq!(basename("/home/papers/"), "papers", "a trailing separator is not the name");
    }

    #[test]
    fn the_save_keycode_is_the_one_the_keymap_calls_s() {
        // The constant is a literal, so it is pinned against the table it has to agree with
        // rather than against the comment beside it.
        assert_eq!(libinput::keymap::to_char(SAVE_KEYCODE, 0), Some(b's'));
    }
}
