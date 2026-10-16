//! Gestures, dialogs and the messages `App::update` takes.

use super::*;

/// What a pointer record did to a gesture in progress.
///
/// **Returned rather than logged or acted on**, because two of the four need a syscall the
/// application cannot make: handing a drag to the compositor is IPC, and a repaint is the
/// binary's loop. The same outbox discipline as everything else here, in the one place where
/// `update` is not what produced the change.
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
/// **One slot, not one per dialog** (M14 Part D). A window shows at most one of these at a time,
/// and the binary's plumbing for hosting one — open, close, present, route, and deciding which
/// window an event belongs to — is about sixty lines that were written for the delete question
/// and would otherwise be copied for the next one. `nxedit` has three such copies; this is the
/// seam that keeps `nxfiles` at zero.
///
/// A variant carries the console lines its dialog needs, so adding one cannot forget them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dialog {
    /// "Delete this?" — the one question this browser asks, and the only operation it cannot undo.
    Confirm,
    /// What is known about one entry. **Not a question**: it has one button and no answer.
    Properties,
}

impl Dialog {
    /// The line printed **before** the window is asked for.
    ///
    /// Before, because a dialog's first `Configure` is held for the manager: a line printed after
    /// `Child::open` returned would be downstream of the shell and racing it to the console
    /// (M12 Part A, PR #267).
    pub fn opening(self) -> &'static [u8] {
        match self {
            Dialog::Confirm => b"nxfiles: asking before deleting\n",
            Dialog::Properties => b"nxfiles: showing properties\n",
        }
    }

    /// The line printed when the window is first handed the keyboard.
    ///
    /// **The one a gate types after**, not [`Dialog::opening`]: that is said before the window
    /// exists, and a key pressed on it can reach the browser while the manager is still placing
    /// the dialog (`libui::window::Child::took_keyboard`).
    pub fn has_keyboard(self) -> &'static [u8] {
        match self {
            Dialog::Confirm => b"nxfiles: the question has the keyboard\n",
            Dialog::Properties => b"nxfiles: properties has the keyboard\n",
        }
    }

    /// The line printed when the window goes away.
    ///
    /// **Symmetric with [`Dialog::opening`]**, and not decoration: a dialog that opened and never
    /// drew looks, from outside, exactly like one that opened and closed. A gate needs both ends.
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
    /// **Not the same as activating it** (M14 decision 5). A single click selects; a second one
    /// within the run opens. `Activate` is what "open" means, and the keyboard still sends it
    /// directly — `Enter` on a selected row is not a click and has no run to be part of.
    Press(u64),
    /// Put the selection on the clipboard to be **moved**.
    CutFiles,
    /// Put it there to be **copied**.
    CopyFiles,
    /// Act on whatever paths are on the clipboard, here.
    PasteFiles,
    /// A drive's row or its eject button was pressed (Phase 6 Part F): go there, or eject it. One
    /// press, as for a place.
    Drive(u64),
    /// A sidebar row was pressed — go to that place.
    ///
    /// **One press, not two.** The listing needs a double click because a single one has to be
    /// able to *select* a file; a sidebar row has nothing to select and no other verb, so making
    /// it wait for a second click would be a rule copied past its reason.
    Place(u64),
    /// Close the properties dialog. **The only answer it has**, because it asks nothing.
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
    /// Order this tab's listing that way — a View menu row.
    ///
    /// **Applied to what is already held**, with no listing asked for: the entries are the same
    /// entries, and a syscall to put them in a different order would be a round trip to sort a
    /// `Vec` this process owns.
    SetOrder(libfs::Order),
    /// Show or stop showing the entries a leading dot marks — `Ctrl+H`, or the View menu.
    ///
    /// **This one does ask for a listing**, and the asymmetry with [`Msg::SetOrder`] is the
    /// point: an order rearranges what the tab holds, while hiding *drops* entries — so the ones
    /// that were dropped are not there to bring back, and the directory has to be read again.
    ToggleHidden,
    /// A press landed on a row: the gesture that *may* become a drag (M10 Part E).
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
    /// Somebody wants this window gone — its own close button, or the shell asking.
    ///
    /// **One message for both**, as `nxterm` has: which of them it was is not something this
    /// application acts on differently.
    Close,
    /// A word on the menu bar was pressed: open that menu, or close it if it was already open.
    MenuBar(usize),
    /// Open another window of this browser — `Ctrl+Shift+N`, or File ▸ New Window.
    ///
    /// **Recorded, not done**: a window is a compositor object and this crate makes no syscalls.
    NewWindow,
    /// Close every window of this browser — `Ctrl+Q`, or File ▸ Quit. Nothing here has unsaved
    /// work to ask about, so no window can refuse; the editor is where decision 4's interesting
    /// half lives.
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
