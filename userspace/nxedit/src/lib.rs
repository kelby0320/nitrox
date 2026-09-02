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
    Edge, Element, Insets, column, dock, docked, offset, padding, row, sized, stack, text,
    with_spacing,
};
use libui::widget::{
    GRIP_W, Theme as UiTheme, TITLE_BAR_H, TextAreaState, TextFieldState, TitleButtons,
    WINDOW_FRAME_H,
    WidgetState, button, resize_grip, text_area, text_field, title_bar, window_frame,
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

/// The element key on the confirmation dialog's title bar.
///
/// **A separate window's keys, in the same space.** Keys are per *tree*, and the dialog is a
/// tree of its own — nothing would break if these repeated the numbers above. They do not,
/// because a key that appears in two trees is a thing two `hovered ==` comparisons can both
/// match, and the hover the router reports is already one of the two id spaces that look alike
/// (M11 Part E batch 3).
pub const CONFIRM_TITLE_KEY: u64 = 10;
/// The element key on the dialog's question.
pub const CONFIRM_TEXT_KEY: u64 = 11;
/// The element key on the dialog's button strip.
pub const CONFIRM_STRIP_KEY: u64 = 12;
/// The element key on the dialog's *discard* button.
pub const CONFIRM_DISCARD_KEY: u64 = 13;
/// The element key on the dialog's *keep editing* button.
pub const CONFIRM_KEEP_KEY: u64 = 14;

/// The confirmation dialog's width in pixels.
///
/// **A size, not a measurement**, which is the opposite of what a menu does — and the reason is
/// the gate. `check-login` has to press two buttons in this window, and it aims with arithmetic
/// off the origin the shell logs; buttons that resized with the name of the file being edited
/// would move under it. `widget-toolkit.md` §11's "chrome metrics are not themeable" is the same
/// argument one level up (M11 decision 2), and the two buttons below share the width equally so
/// that "a quarter across and three quarters across" is all the gate has to know.
pub const CONFIRM_W: u32 = 340;
/// Its height in pixels.
pub const CONFIRM_H: u32 = 132;
/// The margin between the dialog's frame and the button strip inside it.
pub const CONFIRM_PAD: u32 = 12;
/// The gap between the dialog's two buttons.
pub const CONFIRM_GAP: u32 = 8;
/// How tall each of the dialog's buttons is.
pub const CONFIRM_BUTTON_H: u32 = 26;

/// How wide each of them is — half of what is left after the frame, the margins and the gap.
///
/// **Published, with the three below, because `check-login` presses these buttons and cannot
/// link this crate.** It is a host tool in another workspace, so it hardcodes coordinates the
/// way it already hardcodes `TITLE_BAR_H` and a list's row height — and the test beside these
/// constants is what pins the numbers it hardcodes to the tree that is actually built. Deriving
/// them here rather than writing four literals means a change to the padding moves the gate's
/// target and the test that guards it together.
pub const CONFIRM_BUTTON_W: u32 =
    (CONFIRM_W - libui::widget::WINDOW_FRAME_W - 2 * CONFIRM_PAD - CONFIRM_GAP) / 2;

/// The centre of the *discard* button, in the dialog window's own coordinates.
pub const CONFIRM_DISCARD_CX: i32 =
    (libui::widget::WINDOW_CONTENT_X + CONFIRM_PAD + CONFIRM_BUTTON_W / 2) as i32;

/// The centre of the *keep editing* button, likewise.
pub const CONFIRM_KEEP_CX: i32 = (libui::widget::WINDOW_CONTENT_X
    + CONFIRM_PAD
    + CONFIRM_BUTTON_W
    + CONFIRM_GAP
    + CONFIRM_BUTTON_W / 2) as i32;

/// The vertical centre of both, measured up from the dialog's bottom edge.
pub const CONFIRM_BUTTON_CY: i32 = (CONFIRM_H
    - libui::widget::WINDOW_BORDER
    - libui::widget::WINDOW_FRAME
    - CONFIRM_PAD
    - CONFIRM_BUTTON_H / 2) as i32;

/// The window's size in pixels at startup, before any manager places it.
pub const START_SIZE: Size = Size::new(560, 420);

/// What this window calls the acceptor it declares.
///
/// **A name rather than an index**, because it is a port in waiting (M10 decision 2): the same
/// string is what a command line will address when ports arrive, and an editor that called its
/// sink `0` would have to be re-specified to be addressed by anything but a pointer.
pub const ACCEPTOR: &str = "document";

/// The key that saves, with Ctrl held: `s`.
///
/// **A literal, because `libkern::abi` names only the keys with no character** — the arrows,
/// Enter, the modifiers. A letter is a keycode the keymap turns into text, and nothing has
/// needed to name one until a chord did. `31` is `KEY_S` in the Linux set the input stack
/// speaks (`libinput::keymap`, which maps it to `s`/`S`), and the test below pins it to that
/// table rather than to this comment.
pub const SAVE_KEYCODE: u16 = 31;

/// Confirms the name being typed for an untitled buffer.
const NAME_CONFIRM: u16 = libkern::abi::KEY_ENTER;
/// Abandons it, leaving the buffer untitled and unsaved.
const NAME_CANCEL: u16 = libkern::abi::KEY_ESC;

/// Join a directory and a file name — `libfs::join`'s rule, on `str`.
///
/// The same helper `nxfiles` keeps for the same reason: this half of the application never sees
/// a path as bytes, and converting to call `libfs` would be a round trip through a lossy
/// conversion for a rule that is one line.
fn join(dir: &str, name: &str) -> String {
    let mut s = String::from(dir);
    if !s.ends_with('/') {
        s.push('/');
    }
    s.push_str(name);
    s
}

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
    /// The buffer did not match the file the moment it was read.
    ///
    /// **Because reading is not always lossless.** `TextAreaState::with_text` drops a `\r` from
    /// the end of every line, so a CRLF file is *already* something else by the time it is on
    /// screen — and a `saved_at` taken after that says the buffer matches a file it does not.
    /// The consequence was an editor that could open a file, be told nothing had changed, and
    /// rewrite it two bytes shorter per line on the first `Ctrl+S` (PR #259 review, finding 3).
    ///
    /// So it is folded into [`modified`](Self::modified): what the title bar marks is "this is
    /// not what is on disk", which is true from the first frame here.
    differs: bool,
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
    /// Somebody asked this window to close over an unsaved buffer, and the person has not
    /// answered yet.
    ///
    /// **`true` is a second window**, not an overlay. `Surface::CloseRequested` says outright
    /// that "a client that wants to ask 'save first?' opens a dialog and closes when that
    /// resolves"; until M12 Part A no application had ever created one, so this editor answered
    /// every close by exiting and the buffer went with it. The binary reads this each frame and
    /// opens or destroys a `Role::Dialog` window to match — the same shape `nxterm` uses for its
    /// menu, and the reason [`libui::window::Child`] exists.
    confirming: bool,
    /// Whether the *dialog* holds the keyboard, which its own title bar shows.
    ///
    /// **Not [`focused`](Self::focused)**, which is the main window's: a dialog taking focus
    /// from its parent sends both halves down one channel, and a title bar drawn from the wrong
    /// one would show two active windows or none.
    pub confirm_focused: bool,
    /// The dialog's title bar was dragged, and the binary owes the compositor a `StartMove` **on
    /// the dialog's window**.
    ///
    /// A second flag rather than a second use of [`move_requested`](Self::move_requested),
    /// because the two name different windows: an interactive move is a request on one window
    /// id, and one flag would have moved whichever window the binary happened to pass.
    confirm_move_requested: bool,
    /// A name being typed for a buffer that has never had one.
    ///
    /// **`Some` is a mode**, and it is the first one this editor has: while a name is being typed
    /// the keys belong to it rather than to the buffer, and the status strip shows the field
    /// instead of what last happened. It exists because an editor launched from the applications
    /// menu has no `argv[1]` — it used to print "no file to edit" and exit, which is what "nxedit
    /// doesn't launch from the menu" turned out to be (M11 Part E batch 7).
    ///
    /// **In the editor rather than through the shell.** A `Desktop` op that asked the shell to
    /// collect a name would make the shell a dialog provider for arbitrary clients — an authority
    /// question — and would need a blocking exchange over an async protocol. A field in this
    /// window is no protocol at all, and this crate's own key path already noted that "the first
    /// widget that wants a key needs exactly this shape".
    naming: Option<TextFieldState>,
    /// Where an untitled buffer is saved, from the session's `HOME`.
    home: String,
}

/// What can happen to the editor.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Msg {
    /// A key reached the window.
    Key(KeyEvent),
    /// Write the buffer to its path — the save control, or `Ctrl+S`.
    Save,
    /// A drag was released over the text area — the payload is the binary's to hand over.
    Dropped,
    /// The title bar was dragged.
    DragWindow,
    /// The resize grip was pressed, for these edges.
    ResizeWindow(u32),
    /// A title-bar button asking the manager for a window state.
    RequestState(u32),
    /// Somebody wants this window gone — its own close button, or the shell asking.
    ///
    /// **Over an unsaved buffer this asks rather than closes**, which is the whole of M12 Part
    /// A: it raises [`App::confirming`] and the binary turns that into a real dialog window.
    /// Only [`Msg::Discard`] ends the run.
    Close,
    /// The dialog's *discard* answer: close, and the buffer goes with it.
    Discard,
    /// The dialog's *keep editing* answer, its close button, and `Esc`: the question goes away
    /// and nothing else happens.
    KeepEditing,
    /// The dialog's title bar was dragged.
    DragConfirm,
}

impl App {
    /// An editor for `path`, with an empty buffer until something is loaded into it.
    pub fn new(path: &str, home: &str) -> App {
        App {
            path: String::from(path),
            name: libfs::basename_str(path).to_string(),
            naming: None,
            home: String::from(home),
            text: TextAreaState::new(),
            saved_at: 0,
            blocked: None,
            differs: false,
            status: if path.is_empty() {
                String::from("untitled — save to name it")
            } else {
                String::from("opening…")
            },
            window: START_SIZE,
            focused: true,
            maximized: false,
            save_requested: false,
            state_requested: None,
            move_requested: false,
            resize_requested: None,
            closing: false,
            confirming: false,
            confirm_focused: true,
            confirm_move_requested: false,
        }
    }

    /// The file held `raw`, which decoded to `text`.
    ///
    /// **Both, because the buffer may already differ from the file.** Line endings are
    /// normalised on the way in, so a CRLF file is not what is on screen — and an editor that
    /// called that "unmodified" would rewrite it, shorter, on a `Ctrl+S` the person pressed out
    /// of habit. Comparing what *would be written* against what was read is the only honest
    /// answer, and it is one comparison at open rather than a rule about encodings.
    pub fn loaded(&mut self, text: &str, raw: &[u8]) {
        self.text = TextAreaState::with_text(text);
        self.saved_at = self.text.revision();
        self.blocked = None;
        self.differs = to_bytes(&self.text.text()) != raw;
        self.status = describe(raw.len(), "opened");
        if self.differs {
            self.status.push_str(" · line endings normalised, so saving rewrites it");
        }
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
        self.differs = false;
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
        self.differs = false;
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

    /// Whether what is on screen differs from what is on disk.
    ///
    /// Two ways for that to be true: something was typed, or the file did not survive being read
    /// unchanged — see [`differs`](Self::differs).
    pub fn modified(&self) -> bool {
        self.differs || self.text.revision() != self.saved_at
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

    /// Take `path` as the file to edit, if this buffer can be given up.
    ///
    /// **A modified buffer refuses**, which is the same rule the save path follows from the
    /// other side: opening a dropped file would replace what is on screen, and an editor that
    /// discarded unsaved work because something was dragged onto it is the failure mode this
    /// application exists to not have. The status strip says so, because a drop that visibly
    /// does nothing is indistinguishable from one that was not delivered.
    ///
    /// **And dropping the file already open is a no-op**, not a reload: it is the likeliest
    /// accident with a browser beside an editor, and re-reading would be a way to lose an
    /// unsaved buffer that this rule has just protected.
    pub fn accept_drop(&mut self, path: &str) -> bool {
        if path == self.path {
            self.status = alloc::format!("{} is already open", libfs::basename_str(path));
            return false;
        }
        if self.modified() {
            self.status =
                alloc::format!("not opening {} — save first", libfs::basename_str(path));
            return false;
        }
        self.path = String::from(path);
        self.name = libfs::basename_str(path).to_string();
        true
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
                // The file is now what the buffer holds, whatever it held before.
                self.differs = false;
                self.status = describe(n, "saved");
            }
            Err(why) => self.status = alloc::format!("NOT saved — {why}"),
        }
    }

    /// Apply a message.
    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Key(k) => self.key(k),
            // **Saving an untitled buffer asks for a name first.** The write itself is the
            // binary's, as always; what changes here is that there may be nowhere to write to
            // yet, and inventing a path would be a file somebody did not choose.
            Msg::Save => {
                if self.path.is_empty() {
                    if self.naming.is_none() {
                        self.naming = Some(TextFieldState::new());
                        self.status = String::from("name it, then Enter");
                    }
                } else {
                    self.save_requested = true;
                }
            }
            // Nothing here: the payload is in the event the binary is holding, and *which*
            // widget took the drop is all the toolkit can say. The binary pairs them.
            Msg::Dropped => {}
            Msg::DragWindow => self.move_requested = true,
            Msg::DragConfirm => self.confirm_move_requested = true,
            Msg::ResizeWindow(edges) => self.resize_requested = Some(edges),
            // **A modified buffer never closes on this message, however many times it
            // arrives.** The obvious spelling — "ask if we are not already asking, otherwise
            // close" — turns a *second* `CloseRequested` into an exit, and a shell that asks
            // twice is exactly what a person clicking a taskbar entry twice produces. So the
            // only route out of a modified buffer is the answer the person gave.
            Msg::Close => {
                if self.modified() {
                    self.confirming = true;
                } else {
                    self.closing = true;
                }
            }
            Msg::Discard => {
                self.confirming = false;
                self.closing = true;
            }
            // **The status strip says so**, because a dialog that vanishes with nothing changed
            // is indistinguishable from one that took the other answer.
            Msg::KeepEditing => {
                self.confirming = false;
                self.status = String::from("still editing — nothing was discarded");
            }
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
        // **While a name is being typed the keys are the field's**, buffer and chords included.
        // A `Ctrl+S` here would ask to save the thing that has no name yet, which is what is
        // already being answered.
        if let Some(field) = self.naming.as_mut() {
            match k.keycode {
                NAME_CANCEL => {
                    self.naming = None;
                    self.status = String::from("not saved");
                }
                NAME_CONFIRM => {
                    let name = field.text().trim().to_string();
                    if name.is_empty() {
                        // Nothing typed is not a name, and an empty one would save to the
                        // directory itself. Left open rather than cancelled: the person is
                        // mid-answer.
                        self.status = String::from("a name, then Enter");
                        return;
                    }
                    self.path = join(&self.home, &name);
                    self.name = name;
                    self.naming = None;
                    self.save_requested = true;
                }
                code => {
                    field.apply(code, k.modifiers);
                }
            }
            return;
        }
        if k.modifiers & MOD_CTRL != 0 {
            if k.keycode == SAVE_KEYCODE {
                // **Through `update`, not straight to the flag**, so the untitled case asks for a
                // name here too — a chord and a button that did different things would be the
                // same control answering twice.
                self.update(Msg::Save);
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

    /// Whether a confirmation is being asked — the binary opens a dialog window to match.
    pub fn confirming(&self) -> bool {
        self.confirming
    }

    /// Whether a `StartMove` is owed **on the dialog's window**. Clears the record.
    pub fn take_confirm_move(&mut self) -> bool {
        core::mem::take(&mut self.confirm_move_requested)
    }

    /// The dialog could not be opened, so the question cannot be asked.
    ///
    /// **The window stays, and the buffer stays.** The two alternatives are both worse: exiting
    /// would discard unsaved work on the strength of a failed window creation, and leaving
    /// [`confirming`](Self::confirming) set would make an editor that can never be closed at
    /// all, because every later `Close` would try to open the same dialog again. So the close is
    /// abandoned and the strip says why — a person can save and close, which is the thing they
    /// were being asked about.
    pub fn confirm_failed(&mut self) {
        self.confirming = false;
        self.status = String::from("could not ask about the unsaved buffer — save, then close");
    }

    /// What the dialog does with a key.
    ///
    /// **`Esc` only, and it is the cautious answer.** A modal a person cannot dismiss from the
    /// keyboard is a modal that has taken the keyboard hostage, and `Esc` is what the naming
    /// field already uses for "never mind" — one rule for the two places in this editor where
    /// the keys stop being the buffer's. There is deliberately no key for *discard*: `Enter` is
    /// the obvious candidate and the obvious accident, since the question arrives while somebody
    /// is typing.
    pub fn confirm_key(&self, k: KeyEvent) -> Option<Msg> {
        if k.pressed != KEY_DOWN && k.pressed != KEY_REPEAT {
            return None;
        }
        (k.keycode == NAME_CANCEL).then_some(Msg::KeepEditing)
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
        // `WINDOW_FRAME_H` too, since M11 Part E batch 2b — see `nxfiles::App::list_h`.
        self.window.h.saturating_sub(TITLE_BAR_H + STATUS_H + GRIP_W + WINDOW_FRAME_H)
    }

    /// What the title bar shows: the file's name, marked when the buffer differs from the disk.
    ///
    /// **A leading marker rather than a trailing one**, so it is in the same place whatever the
    /// name's length — a mark that moves is a mark that has to be looked for. The window's
    /// *title* (what the taskbar shows) is set once and stays the name alone: retitling on every
    /// keystroke is a message per keystroke to say something the window itself already shows.
    pub fn title(&self) -> String {
        if self.name.is_empty() {
            // **Named for what it is, not left blank.** A window whose title bar says nothing
            // reads as a window that failed to load something.
            return if self.modified() {
                String::from("* untitled")
            } else {
                String::from("untitled")
            };
        }
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
    ///
    /// **The theme is the caller's**, because the caller paints this tree — and a tree built from
    /// one theme and painted with another is two themes in one frame, which one type makes easy
    /// to write and the old `Theme`/`Palette` split made impossible (PR #262 review, optional 5).
    /// It is also the shape Part C needs: a theme read from a file arrives in `main` and is
    /// handed down, rather than being fetched from a default in the middle of a view.
    pub fn view(&mut self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {

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
            button(
                "save",
                Msg::Save,
                WidgetState { hovered: hovered == Some(SAVE_KEY), ..Default::default() },
                &ui,
            )
            .key(SAVE_KEY),
            // **The field replaces the status, it does not sit beside it.** The strip is one row
            // of chrome and a name being typed *is* what last happened — showing both would make
            // a person read two things to find out which one is asking for an answer.
            match self.naming.as_ref() {
                Some(f) => padding(
                    Insets { top: 2, right: 6, bottom: 2, left: 6 },
                    text_field(f, false, WidgetState { active: true, ..Default::default() }, &ui),
                )
                .key(STATUS_KEY)
                .flex(1),
                None => padding(
                    Insets { top: 4, right: 4, bottom: 4, left: 6 },
                    text(self.status.clone()),
                )
                .key(STATUS_KEY),
            },
        ]);

        let h = self.area_h();
        // **The text area takes drops; the chrome does not.** That distinction is the whole of
        // what decision 3 buys — the compositor knows only that this window declared an
        // acceptor, and *where* on the window a drop means something is decided here, by which
        // element is under the point. Dropping a file on the title bar does nothing, which is
        // the honest answer: the title bar is not where a document goes.
        let area = text_area(&mut self.text, h, ROW_H, self.focused, &ui).on_drop(Msg::Dropped);

        let body = window_frame(
            title,
            dock(
                alloc::vec![docked(Edge::Top, sized(Size::new(0, STATUS_H), strip).key(STRIP_KEY))],
            // Sized to the height it was built for, like every scrolling widget in this tree:
            // the dock's flex child otherwise gets whatever is left, and the widget would build
            // rows for one height and be drawn at another.
                sized(Size::new(0, h), area).key(AREA_KEY),
            ),
            &ui,
        );

        let grip = offset(
            self.window.w.saturating_sub(GRIP_W) as i32,
            self.window.h.saturating_sub(GRIP_W) as i32,
            resize_grip(Msg::ResizeWindow(RESIZE_RIGHT | RESIZE_BOTTOM), &ui).key(GRIP_KEY),
        );
        stack(alloc::vec![body, grip])
    }

    /// The element tree for the confirmation dialog — a **second window's** whole face.
    ///
    /// **Sized rather than measured**, which is why the frame below is `window_frame` like every
    /// other window's rather than something that could report a natural size: `libui`'s `Dock`
    /// measures as everything it is offered, deliberately, so a tree containing one has no
    /// natural size at all and [`libui::window::Child::open`] refuses it. Wrapping the whole
    /// thing in a fixed `sized` is what makes the measurement exact — see [`CONFIRM_W`] for why
    /// a dialog wants a fixed size in the first place.
    ///
    /// **The name is on its own line.** Folded into the question it would push a long file name
    /// off the right edge and take the `?` with it, so what clips is the name and never the
    /// sentence.
    pub fn confirm_view(&self, ui: &UiTheme, hovered: Option<u64>) -> Element<Msg> {
        let title = title_bar(
            "Unsaved changes",
            self.confirm_focused,
            Msg::DragConfirm,
            // **One button, and it is the cautious answer.** Minimise and maximise are absent
            // rather than present-and-inert: a control that looks live and is not is the defect
            // M8's overview shipped three of, and `title_bar` draws only the buttons it is given
            // a message for. Closing the question is *keep editing* — the dialog's own frame
            // must not be a third way to discard a buffer.
            TitleButtons {
                minimise: None,
                maximise: None,
                close: Some(Msg::KeepEditing),
            },
            ui,
        )
        .key(CONFIRM_TITLE_KEY);

        let name = if self.name.is_empty() { "untitled" } else { self.name.as_str() };
        let question = padding(
            Insets::all(CONFIRM_PAD),
            column(alloc::vec![
                text("Discard unsaved changes?"),
                text(String::from(name)),
            ]),
        )
        .key(CONFIRM_TEXT_KEY);

        // **Two buttons sharing the width equally**, so that where they are is arithmetic
        // anybody can do: a quarter across and three quarters across, at a fixed height off the
        // bottom. `check-login` presses both, and it aims from the origin the shell logs.
        //
        // **Two answers and not three** — `TODO(dialog-save-answer)`. *Save and close* is the
        // obvious third, and it is not a button: an untitled buffer has nowhere to save to, so
        // it is the naming field's flow and then a close. See `deferred-decisions.md`.
        let answer = |label: &str, msg: Msg, key: u64| {
            button(
                label,
                msg,
                WidgetState { hovered: hovered == Some(key), ..Default::default() },
                ui,
            )
            .key(key)
            .flex(1)
        };
        let strip = sized(
            Size::new(0, CONFIRM_BUTTON_H + CONFIRM_PAD),
            padding(
                Insets { top: 0, right: CONFIRM_PAD, bottom: CONFIRM_PAD, left: CONFIRM_PAD },
                with_spacing(
                    row(alloc::vec![
                        answer("discard", Msg::Discard, CONFIRM_DISCARD_KEY),
                        answer("keep editing", Msg::KeepEditing, CONFIRM_KEEP_KEY),
                    ]),
                    CONFIRM_GAP,
                ),
            ),
        )
        .key(CONFIRM_STRIP_KEY);

        sized(
            Size::new(CONFIRM_W, CONFIRM_H),
            window_frame(title, dock(alloc::vec![docked(Edge::Bottom, strip)], question), ui),
        )
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
        let mut a = App::new("/home/notes.txt", "/home");
        // `hello\n` on disk, which is what `hello` writes back — so this buffer starts matching
        // its file, which every test below depends on.
        a.loaded("hello", b"hello\n");
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
        let mut a = App::new("/home/notes.txt", "/home");
        a.blocked("could not be read");
        key(&mut a, KEY_X, 0);
        a.update(Msg::Save);
        assert_eq!(a.take_save(), None, "a blocked buffer owes no write");
        assert!(a.status().contains("not saved"), "status was {:?}", a.status());
        assert_eq!(a.refusal(), Some("could not be read"));
    }

    #[test]
    fn a_file_that_did_not_survive_being_read_opens_modified() {
        // **The one case where the editor would write something it never showed.** Reading
        // normalises line endings, so a CRLF file is already something else on screen — and an
        // editor that called that unmodified would rewrite it, two bytes shorter per line, on a
        // `Ctrl+S` pressed out of habit (PR #259 review, finding 3).
        let mut a = App::new("/home/dos.txt", "/home");
        a.loaded("alpha\nbeta\n", b"alpha\r\nbeta\r\n");
        assert!(a.modified(), "the buffer is not what the file holds");
        assert_eq!(a.title(), "* dos.txt", "and the title says so before anything is typed");
        assert!(a.status().contains("line endings"), "status was {:?}", a.status());
        assert_eq!(a.refusal(), None, "it is writable — it just is not the same bytes");

        // Saving makes the file match the buffer, which is what clears it.
        a.update(Msg::Save);
        let owed = a.take_save().expect("a blocked buffer is a different thing");
        a.saved(Ok(owed.len()));
        assert!(!a.modified());
        assert_eq!(a.title(), "dos.txt");

        // And a file that *does* survive is not marked: this must distinguish, or it is a
        // permanent asterisk rather than an answer.
        let mut b = App::new("/home/unix.txt", "/home");
        b.loaded("alpha\nbeta\n", b"alpha\nbeta\n");
        assert!(!b.modified());
    }

    #[test]
    fn a_drop_is_refused_while_there_is_work_to_lose() {
        // **The same rule the save path follows, from the other side.** Opening a dropped file
        // replaces what is on screen, and an editor that discarded unsaved work because
        // something was dragged onto it is the failure this application exists not to have.
        let mut a = app();
        key(&mut a, KEY_X, 0);
        assert!(a.modified(), "precondition: there is something to lose");

        assert!(!a.accept_drop("/home/other.txt"));
        assert_eq!(a.path(), "/home/notes.txt", "still editing what it was");
        assert!(a.status().contains("save first"), "status was {:?}", a.status());

        // Saved, and the same drop is taken.
        a.update(Msg::Save);
        let owed = a.take_save().unwrap();
        a.saved(Ok(owed.len()));
        assert!(a.accept_drop("/home/other.txt"));
        assert_eq!(a.path(), "/home/other.txt");
        assert_eq!(a.title(), "other.txt", "and the title follows the file");
    }

    #[test]
    fn dropping_the_file_already_open_does_nothing() {
        // The likeliest accident with a browser beside an editor — and re-reading would be a
        // way to lose an unsaved buffer that the rule above has just protected.
        let mut a = app();
        assert!(!a.accept_drop("/home/notes.txt"));
        assert!(a.status().contains("already open"), "status was {:?}", a.status());
        assert_eq!(a.path(), "/home/notes.txt");
    }

    #[test]
    fn an_untitled_buffer_is_named_on_save_and_saved_where_it_was_named() {
        // **This is what "nxedit doesn't launch from the menu" was**: the applications modal
        // passes no arguments, the editor required `argv[1]`, so it started and exited (M11
        // Part E batch 7).
        let mut a = App::new("", "/home/alice");
        assert_eq!(a.title(), "untitled", "an unnamed buffer has no title to show");
        assert!(a.take_save().is_none(), "an untitled buffer had somewhere to be written");

        // Save asks rather than writes. **The control is `take_save`**: a version that set the
        // flag anyway would pass an assertion about the status text alone.
        a.update(Msg::Save);
        assert!(a.take_save().is_none(), "an untitled save wrote to a path nobody chose");
        assert!(a.status().contains("name it"), "status was {:?}", a.status());

        // The keys are the field's now, buffer included.
        // "notes", by keycode — the codes the keymap maps to those letters, which is what an
        // injected keystroke carries.
        for c in [49u16, 24, 20, 18, 31] {
            key(&mut a, c, 0);
        }
        assert_eq!(a.text.text(), "", "typing a name went into the buffer");

        key(&mut a, libkern::abi::KEY_ENTER, 0);
        assert_eq!(a.path(), "/home/alice/notes", "named into the wrong directory");
        // Unmarked: naming a buffer is not editing it, and this one has had nothing typed into
        // it — the keys went to the field. The mark means "not what is on disk".
        assert_eq!(a.title(), "notes", "the title bar still says untitled");
        assert!(a.take_save().is_some(), "confirming a name did not ask for the save");
    }

    #[test]
    fn abandoning_the_name_leaves_the_buffer_untitled_and_unwritten() {
        let mut a = App::new("", "/home/alice");
        a.update(Msg::Save);
        key(&mut a, 49, 0);
        key(&mut a, libkern::abi::KEY_ESC, 0);
        assert_eq!(a.path(), "", "escaping the prompt named the buffer anyway");
        assert!(a.take_save().is_none(), "escaping the prompt saved it anyway");

        // And an empty name is not a name: it would write to the directory itself.
        a.update(Msg::Save);
        key(&mut a, libkern::abi::KEY_ENTER, 0);
        assert_eq!(a.path(), "", "an empty name became a path");
        assert!(a.take_save().is_none());
        assert!(a.status().contains("a name"), "status was {:?}", a.status());
    }

    #[test]
    fn a_named_buffer_saves_without_asking() {
        // The control for the two above: the path that *has* a name must not grow a prompt.
        let mut a = app();
        a.update(Msg::Save);
        assert!(a.take_save().is_some(), "a named buffer asked for a name");
    }

    #[test]
    fn a_missing_file_is_not_a_failure() {
        // Opening a path that is not there is how a file gets made, so the buffer is writable —
        // and the strip says which of the two happened, because a person who meant to open an
        // existing file wants to know they did not.
        let mut a = App::new("/home/new.txt", "/home");
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
        // The title's name comes from `libfs`, so this pins what this application depends on
        // rather than re-testing that crate: a trailing separator must not give an empty title.
        let named = |p: &str| App::new(p, "/home").title();
        assert_eq!(named("/home/alice/notes.txt"), "notes.txt");
        assert_eq!(named("notes.txt"), "notes.txt");
        assert_eq!(named("/home/papers/"), "papers", "a trailing separator is not the name");
    }

    #[test]
    fn the_save_keycode_is_the_one_the_keymap_calls_s() {
        // The constant is a literal, so it is pinned against the table it has to agree with
        // rather than against the comment beside it.
        assert_eq!(libinput::keymap::to_char(SAVE_KEYCODE, 0), Some(b's'));
    }

    // ---- M12 Part A: the confirmation ----

    #[test]
    fn closing_a_modified_buffer_asks_rather_than_exiting() {
        // **The rule the whole part exists for.** Until M12 Part A this editor answered
        // `CloseRequested` by exiting, and its own comment said so: "an editor with somewhere to
        // put a question would ask it — and this one has no dialog to ask in".
        let mut a = app();
        key(&mut a, KEY_X, 0);
        a.update(Msg::Close);
        assert!(a.confirming(), "a modified buffer asks");
        assert!(!a.closing(), "and does not exit while it is asking");
    }

    #[test]
    fn closing_a_clean_buffer_asks_nothing() {
        // The other half, and the one that keeps the ordinary close a single click: a buffer
        // that matches its file has nothing to lose, so there is nothing to ask about.
        let mut a = app();
        a.update(Msg::Close);
        assert!(!a.confirming(), "nothing to ask about");
        assert!(a.closing());
    }

    #[test]
    fn a_second_close_request_does_not_discard_the_buffer() {
        // **The control for the obvious wrong spelling.** "Ask if we are not already asking,
        // otherwise close" reads fine and turns the *second* `CloseRequested` into an exit —
        // and a second one is exactly what a person clicking a taskbar entry twice produces,
        // which since this part is also how they force a wedged window shut. With that spelling
        // this test fails on the `closing` assertion.
        let mut a = app();
        key(&mut a, KEY_X, 0);
        a.update(Msg::Close);
        a.update(Msg::Close);
        assert!(a.confirming(), "still asking");
        assert!(!a.closing(), "a repeated ask is not an answer");
    }

    #[test]
    fn only_discard_ends_the_run() {
        let mut a = app();
        key(&mut a, KEY_X, 0);
        a.update(Msg::Close);

        // Keeping ends the question and nothing else — the buffer is byte-for-byte what it was.
        let text = a.text();
        a.update(Msg::KeepEditing);
        assert!(!a.confirming(), "the question is answered");
        assert!(!a.closing(), "and the answer was no");
        assert_eq!(a.text(), text);
        assert!(a.modified(), "keeping does not mark it saved");
        assert!(a.status().contains("still editing"), "status was {:?}", a.status());

        // Asking again, and discarding this time.
        a.update(Msg::Close);
        assert!(a.confirming());
        a.update(Msg::Discard);
        assert!(!a.confirming(), "the dialog goes when it is answered");
        assert!(a.closing());
    }

    #[test]
    fn saving_removes_the_reason_to_ask() {
        // Derived rather than remembered: `confirming` is decided by `modified()` at the moment
        // of the close, so a save between the two makes the question go away by itself.
        let mut a = app();
        key(&mut a, KEY_X, 0);
        a.update(Msg::Save);
        let owed = a.take_save().expect("a save was asked for");
        a.saved(Ok(owed.len()));
        assert!(!a.modified());

        a.update(Msg::Close);
        assert!(!a.confirming(), "nothing unsaved to ask about");
        assert!(a.closing());
    }

    #[test]
    fn the_dialog_answers_escape_and_nothing_else() {
        // **`Esc` because a modal a keyboard cannot dismiss has taken the keyboard hostage**,
        // and nothing else because `Enter` is the obvious accident: the question arrives while
        // somebody is typing.
        let a = app();
        let ev = |code: u16, pressed: u16| KeyEvent::new(1, code, pressed, 0);
        assert_eq!(a.confirm_key(ev(NAME_CANCEL, KEY_DOWN)), Some(Msg::KeepEditing));
        assert_eq!(a.confirm_key(ev(NAME_CONFIRM, KEY_DOWN)), None, "Enter must not discard");
        assert_eq!(a.confirm_key(ev(KEY_X, KEY_DOWN)), None);
        // A release is not a press. Without this guard `Esc` would answer twice, and the second
        // answer would arrive after the window it belongs to had been destroyed.
        assert_eq!(a.confirm_key(ev(NAME_CANCEL, 0)), None, "a release is not an answer");
    }

    #[test]
    fn a_dialog_that_will_not_open_leaves_the_window_alone() {
        // **Neither of the two tempting failures.** Exiting would discard unsaved work because a
        // window could not be created; staying `confirming` would make an editor that can never
        // be closed, since every later `Close` would try the same failing creation again.
        let mut a = app();
        key(&mut a, KEY_X, 0);
        a.update(Msg::Close);
        a.confirm_failed();
        assert!(!a.confirming());
        assert!(!a.closing(), "a failed dialog must not discard the buffer");
        assert!(a.status().contains("save, then close"), "status was {:?}", a.status());
        // And the editor is still usable: a later save works.
        a.update(Msg::Save);
        assert!(a.take_save().is_some());
    }

    /// A fake face: every character an 8x16 box, which is all these tests need.
    const CELL: libui::layout::FixedCell = libui::layout::FixedCell { w: 8, h: 16 };

    #[test]
    fn the_dialog_measures_to_exactly_the_size_it_declares() {
        // **What lets it be a window at all.** `libui::window::Child::open` sizes a child window
        // from what its tree measures, and `Node::Dock` measures as *everything it is offered* —
        // deliberately, since a dock's job is to divide a given area. `window_frame` contains
        // one, so without the fixed `sized` wrapper this tree measures to the constraint's
        // maximum and `Child::open` refuses it. Delete the wrapper and this fails naming a
        // number in the hundreds of millions.
        let a = app();
        let ui = a.confirm_view(&UiTheme::default(), None);
        let got = libui::layout::measure(
            &ui,
            libui::layout::Constraints::loose(Size::new(u32::MAX / 4, u32::MAX / 4)),
            &CELL,
        );
        assert_eq!(got, Size::new(CONFIRM_W, CONFIRM_H));
    }

    #[test]
    fn the_published_button_centres_are_where_the_buttons_are() {
        // **This is what `check-login` aims at.** The gate cannot link this crate, so it
        // hardcodes these four numbers the way it already hardcodes a title bar's height — and
        // this test is what stops them being four numbers nothing checks. A press *and* a
        // release, because a click is the release: pressing alone proves only that something is
        // under the point.
        // **The literals `check-login` actually types, asserted here.** Deriving the constants
        // from `CONFIRM_PAD` and then comparing them against a tree built from the same
        // `CONFIRM_PAD` pins nothing: both sides move together, and the gate's own table —
        // which cannot import this crate — is linked to them by nothing at all. With
        // `CONFIRM_PAD` at 40 every test in this file passed while the gate went on clicking
        // `y = 103` at buttons spanning 62..88, failing after a three-minute boot, which is
        // exactly the outcome the doc comment on `CONFIRM_BUTTON_W` says is prevented (PR #267
        // review, finding 2). These four numbers are that table.
        assert_eq!(
            (CONFIRM_DISCARD_CX, CONFIRM_KEEP_CX, CONFIRM_BUTTON_CY),
            (91, 249, 103),
            "check-login hardcodes these three; change the dialog's metrics and change them there"
        );
        assert_eq!((CONFIRM_W, CONFIRM_H), (340, 132), "and these two, as the size it checks");

        let a = app();
        let ui = a.confirm_view(&UiTheme::default(), None);
        let l = libui::layout::layout(&ui, Rect::new(0, 0, CONFIRM_W, CONFIRM_H), &CELL);
        let mut tree = libui::diff::Tree::new();
        tree.update(&ui, &l).expect("the dialog is diffable");
        let mut router = libui::route::Router::new();

        let click = |r: &mut libui::route::Router, x: i32, y: i32| {
            let at = |flags: u16, buttons: u16| librsproto::surface::PointerEvent {
                kind: librsproto::surface::POINTER_BUTTON,
                button: 0x110,
                buttons,
                flags,
                x,
                y,
                ..Default::default()
            };
            r.pointer(&tree, &ui, &l, at(librsproto::surface::POINTER_PRESSED, 1));
            r.pointer(&tree, &ui, &l, at(0, 0)).0
        };

        assert_eq!(
            click(&mut router, CONFIRM_DISCARD_CX, CONFIRM_BUTTON_CY),
            alloc::vec![Msg::Discard],
        );
        assert_eq!(
            click(&mut router, CONFIRM_KEEP_CX, CONFIRM_BUTTON_CY),
            alloc::vec![Msg::KeepEditing],
        );
        // And the two are not the same button: a layout that collapsed one of them would make
        // both presses land on whichever survived, and both assertions above would still pass.
        assert!(CONFIRM_KEEP_CX - CONFIRM_DISCARD_CX >= CONFIRM_BUTTON_W as i32);

        // **The aim point is the button's *centre*, not merely a point inside it** (PR #267
        // review, optional 6). Padding the strip on all four sides instead of three halves the
        // buttons — 26 pixels to 14 — and every assertion above still passes, because a centre
        // stays inside a box that shrank around it. So the row's own height is bracketed: one
        // pixel inside each edge hits, and a point a full button above it does not.
        let half = CONFIRM_BUTTON_H as i32 / 2;
        for edge in [CONFIRM_BUTTON_CY - half + 1, CONFIRM_BUTTON_CY + half - 1] {
            assert_eq!(
                click(&mut router, CONFIRM_DISCARD_CX, edge),
                alloc::vec![Msg::Discard],
                "the button does not reach {edge}, so {CONFIRM_BUTTON_CY} is not its centre"
            );
        }
        assert!(
            click(&mut router, CONFIRM_DISCARD_CX, CONFIRM_BUTTON_CY - CONFIRM_BUTTON_H as i32)
                .is_empty(),
            "a whole button above the aim point is not the button"
        );
    }

    #[test]
    fn the_dialogs_close_button_keeps_the_buffer() {
        // **A dialog's own frame must not be a third way to discard.** `title_bar` draws only
        // the buttons it is given a message for, so the question here is which message the one
        // button carries — and the cautious answer is the only defensible default for a control
        // whose meaning is "make this go away".
        let a = app();
        let ui = a.confirm_view(&UiTheme::default(), None);
        let l = libui::layout::layout(&ui, Rect::new(0, 0, CONFIRM_W, CONFIRM_H), &CELL);
        let mut tree = libui::diff::Tree::new();
        tree.update(&ui, &l).expect("the dialog is diffable");
        let mut router = libui::route::Router::new();
        // The rightmost title-bar button, measured the way every gate in this tree measures one.
        let x = CONFIRM_W as i32 - libui::widget::WINDOW_CONTENT_X as i32
            - (libui::widget::TITLE_BUTTON_W / 2) as i32;
        let y = (libui::widget::WINDOW_CONTENT_Y + TITLE_BAR_H / 2) as i32;
        let at = |flags: u16, buttons: u16| librsproto::surface::PointerEvent {
            kind: librsproto::surface::POINTER_BUTTON,
            button: 0x110,
            buttons,
            flags,
            x,
            y,
            ..Default::default()
        };
        router.pointer(&tree, &ui, &l, at(librsproto::surface::POINTER_PRESSED, 1));
        let (msgs, _) = router.pointer(&tree, &ui, &l, at(0, 0));
        assert_eq!(msgs, alloc::vec![Msg::KeepEditing]);
    }
}
