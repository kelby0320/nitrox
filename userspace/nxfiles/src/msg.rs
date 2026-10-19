//! Gestures, dialogs and the messages `App::update` takes.

use super::*;

/// What a pointer record did to a gesture in progress.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Gesture {
    /// Nothing that anything outside needs to do.
    None,
    /// A drag has left this window: the binary owes the compositor a `StartDrag`.
    HandOff,
    /// An internal drag ended over a directory row, and [`App::take_op`] may now have a move.
    Dropped,
    /// The row an internal drag is over has changed, so the frame is stale.
    Moved,
}

/// Which dialog a browser window has open.
///
/// One slot, not one per dialog: hosting one (open, close, present, route) is plumbing the
/// binary would otherwise copy for each. A variant carries the console lines its dialog needs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dialog {
    /// "Delete this?" — the one question this browser asks, and the only operation it cannot undo.
    Confirm,
    /// What is known about one entry; not a question: one button, no answer.
    Properties,
}

impl Dialog {
    /// The line printed **before** the window is asked for.
    ///
    /// After `Child::open` returned it would race the shell to the console, the dialog's first
    /// `Configure` being held for the manager.
    pub fn opening(self) -> &'static [u8] {
        match self {
            Dialog::Confirm => b"nxfiles: asking before deleting\n",
            Dialog::Properties => b"nxfiles: showing properties\n",
        }
    }

    /// The line printed when the window is first handed the keyboard.
    ///
    /// The one a gate types after, not [`Dialog::opening`]: a key sent then can reach the browser
    /// while the manager is still placing the dialog.
    pub fn has_keyboard(self) -> &'static [u8] {
        match self {
            Dialog::Confirm => b"nxfiles: the question has the keyboard\n",
            Dialog::Properties => b"nxfiles: properties has the keyboard\n",
        }
    }

    /// The line printed when the window goes away; a gate needs both ends, as a dialog that opened
    /// and never drew looks like one that opened and closed.
    pub fn closed(self) -> &'static [u8] {
        match self {
            Dialog::Confirm => b"nxfiles: the question closed\n",
            Dialog::Properties => b"nxfiles: properties closed\n",
        }
    }

    /// The line printed when the window could not be created.
    pub fn open_failed(self) -> &'static [u8] {
        match self {
            Dialog::Confirm => b"nxfiles: could not open the confirmation dialog\n",
            Dialog::Properties => b"nxfiles: could not open the properties dialog\n",
        }
    }

    /// The line printed when a frame could not be drawn.
    pub fn draw_failed(self) -> &'static [u8] {
        match self {
            Dialog::Confirm => b"nxfiles: the confirmation dialog could not be drawn\n",
            Dialog::Properties => b"nxfiles: the properties dialog could not be drawn\n",
        }
    }
}

/// What can happen to the browser.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Msg {
    /// A listing row was activated — its index into [`App::entries`].
    Activate(u64),
    /// A row was **pressed** by the pointer — its index into [`App::entries`].
    ///
    /// Not activating it: one click selects, a second within the run opens. `Enter` sends
    /// `Activate` directly.
    Press(u64),
    /// Put the selection on the clipboard to be **moved**.
    CutFiles,
    /// Put it there to be **copied**.
    CopyFiles,
    /// Act on whatever paths are on the clipboard, here.
    PasteFiles,
    /// A drive's row or its eject button was pressed: go there, or eject it.
    Drive(u64),
    /// A sidebar row was pressed — go to that place; one press, as it has nothing to select.
    Place(u64),
    /// Close the properties dialog, which asks nothing.
    CloseProperties,
    /// Open the location bar, seeded with where this tab is — `Ctrl+L`, or the File menu.
    OpenLocation,
    /// Open the toolbar's search field, and filter the listing as it is typed.
    OpenSearch,
    /// Close it, and show the whole listing again.
    SearchCancel,
    /// Go to what was typed there.
    LocationGo,
    /// Close it without going anywhere.
    LocationCancel,
    /// Order this tab's listing that way — a View menu row, applied to the entries already held.
    SetOrder(libfs::Order),
    /// Show or stop showing the entries a leading dot marks — `Ctrl+H`, or the View menu.
    ///
    /// Unlike [`Msg::SetOrder`] this asks for a listing: hiding drops entries, so showing them
    /// needs a re-read.
    ToggleHidden,
    /// A press landed on a row: the gesture that *may* become a drag.
    Grab(u64),
    /// The "up" control was pressed.
    Up,
    /// A pointer event over the scrollbar — see [`ScrollGrab`](libui::widget::ScrollGrab).
    Scroll(PointerEvent),
    /// The wheel turned over the listing — see [`ListState::wheel`].
    Wheel(PointerEvent),
    /// A key reached the window.
    Key(KeyEvent),
    /// The title bar was dragged.
    DragWindow,
    /// The resize grip was pressed, for these edges.
    ResizeWindow(u32),
    /// A title-bar button asking the manager for a window state.
    RequestState(u32),
    /// The manager put this window in this state: the maximise button follows it, not its click.
    WindowState(u32),
    /// Somebody wants this window gone — its own close button, or the shell asking.
    Close,
    /// A word on the menu bar was pressed: open that menu, or close it if it was already open.
    MenuBar(usize),
    /// Open another window of this browser — `Ctrl+Shift+N`, or File ▸ New Window.
    ///
    /// **Recorded, not done**: a window is a compositor object and this crate makes no syscalls.
    NewWindow,
    /// Close every window of this browser — `Ctrl+Q`, or File ▸ Quit; none can refuse, nothing here
    /// has unsaved work.
    Quit,
    /// A menu row was chosen.
    Choose(Action),
    /// The delete dialog's *delete* answer.
    ConfirmDelete,
    /// The delete dialog's *keep* answer, its close button, and `Esc`.
    KeepIt,
    /// The delete dialog's title bar was dragged.
    DragConfirm,
    /// A tab was pressed — make it current.
    SelectTab(u64),
    /// A tab's close box was pressed, or `Ctrl+W`.
    CloseTab(u64),
    /// `Ctrl+T`: a second view of where you are.
    NewTab,
}
