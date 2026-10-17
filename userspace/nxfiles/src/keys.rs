//! The element keys and keycodes the browser's events are routed by.

use super::*;

/// The element key on the column header.
pub const HEADER_KEY: u64 = 21;

/// The element key on the pane holding the header and the listing, which keeps [`LIST_KEY`].
pub const LIST_PANE_KEY: u64 = 23;

/// The element key on the search field, when it is open.
pub const SEARCH_KEY: u64 = 24;

/// The element key on the status bar at the window's foot.
pub const STATUS_KEY: u64 = 22;

/// The element key on the listing, so a test can find it without walking the tree.
pub const LIST_KEY: u64 = 1;

/// The element key on the "up" control.
pub const UP_KEY: u64 = 2;

/// The element key on the title bar.
pub const TITLE_KEY: u64 = 3;

/// The element key on the resize grip.
pub const GRIP_KEY: u64 = 4;

/// The element key on the path text inside the strip.
pub const PATH_KEY: u64 = 6;

/// The element key on the notice beside the path.
///
/// Always present, empty when there is nothing to say: the diff pairs children by key.
pub const NOTICE_KEY: u64 = 7;

/// The element key on the path strip.
///
/// Every docked child needs a key: the diff requires a container's children to be all keyed or
/// all unkeyed, and a bare sibling makes even the first frame `MixedKeying`, so no window ever
/// appears. `KeyingChanged` is the second-frame error: a window that paints once, then never
/// updates.
pub const STRIP_KEY: u64 = 5;

/// Where the menu bar's words are keyed from: `MENU_BAR_KEY + i` for menu `i`.
pub const MENU_BAR_KEY: u64 = 10;

/// Where the open popup's rows are keyed from: `MENU_ROW_KEY + i` for item `i`.
///
/// Keyed so `Router::inside` can name the row under the pointer for hover; a range apart from
/// the bar's, a row and a word being different widgets.
pub const MENU_ROW_KEY: u64 = 100;

/// The element key on the name field that replaces the path while a prompt is open.
pub const PROMPT_KEY: u64 = 17;

/// The element key on the menu bar.
pub const BAR_KEY: u64 = 18;

/// The element key on the path strip's inner row — the half that is either the path or a prompt.
pub const STRIP_INNER_KEY: u64 = 19;

/// The element key on the tab strip.
pub const TAB_STRIP_KEY: u64 = 24;

/// Where pane keys start — see `nxedit::TAB_KEY_BASE`.
///
/// The top bit keeps a list row's index (any number of entries) from sharing a tab's key.
pub const TAB_KEY_BASE: u64 = 1 << 63;

/// The element key on the sidebar.
pub const SIDEBAR_KEY: u64 = 26;

/// Where the sidebar's rows are keyed from, clear of the listing's by the top bits no row index
/// reaches: a nearer base let listing row 1000 collide with the first sidebar row.
pub const SIDEBAR_ROW_KEY: u64 = 1 << 62;

/// Where the sidebar's **drive** rows are keyed from: `DRIVE_ROW_KEY + n` for block device `n`.
///
/// Keyed by the device, not the row's position: a stick pulled between a frame and a click would
/// otherwise make the click land on whatever moved up — on an eject button, ejecting another.
pub const DRIVE_ROW_KEY: u64 = SIDEBAR_ROW_KEY | (1 << 61);

/// Where their eject buttons are keyed from, by the same `n` ([`drives`] drops a device index
/// that would reach bit 60).
pub const DRIVE_EJECT_KEY: u64 = DRIVE_ROW_KEY | (1 << 60);

/// The element key on the places, inside the sidebar.
pub const PLACES_KEY: u64 = 27;

/// The element key on the Drives heading.
pub const DRIVES_HEADING_KEY: u64 = 28;

/// The element key on the drives' list.
pub const DRIVES_KEY: u64 = 29;

/// `Msg::Drive` for the row or button a key names.
pub(super) fn drive_row(key: u64) -> Msg {
    Msg::Drive(key)
}

/// `Msg::Place` for the row a key names.
pub(super) fn place_row(key: u64) -> Msg {
    Msg::Place(key)
}

/// Where the listing's rows are keyed from: `LIST_ROW_KEY + i` for row `i`.
///
/// A range of its own because `hovered` is one namespace: bare indices lit whichever chrome
/// button shared the number.
pub const LIST_ROW_KEY: u64 = 1000;

/// The row a listing key names, or `None` if it is not a listing row's.
pub(super) fn list_row_of(key: u64) -> Option<usize> {
    key.checked_sub(LIST_ROW_KEY).map(|i| i as usize)
}

/// `Msg::Press` for the row a key names.
pub(super) fn press_row(key: u64) -> Msg {
    Msg::Press(key)
}

/// `Msg::Grab` likewise.
pub(super) fn grab_row(key: u64) -> Msg {
    Msg::Grab(key)
}

/// The key that opens a *window*: `n`, with Ctrl and **Shift** (`Ctrl+N` alone is a tab).
pub const NEW_WINDOW_KEYCODE: u16 = 49;

/// The key that quits: `q`, with Ctrl. Quit is every window of this browser; Close is one.
pub const QUIT_KEYCODE: u16 = 16;

/// The key that opens a tab: `t`.
pub const NEW_TAB_KEYCODE: u16 = 20;

/// The key that closes one: `w`.
pub const CLOSE_TAB_KEYCODE: u16 = 17;

/// The key that shows and hides the entries a leading dot marks: `h`, with Ctrl.
///
/// Not a `Super` chord: the compositor owns those.
pub const HIDDEN_KEYCODE: u16 = 35;

/// The key that opens the location bar: `l`, with Ctrl.
pub const LOCATION_KEYCODE: u16 = 38;

/// `Ctrl+F` opens the toolbar's search field.
pub const SEARCH_KEYCODE: u16 = 33;

/// The key that shows what is known about the selected entry: `i`, with Ctrl.
pub const PROPERTIES_KEYCODE: u16 = 23;

/// The key that cuts the selection: `x`, with Ctrl.
pub const CUT_KEYCODE: u16 = 45;

/// The key that copies it: `c`, with Ctrl.
pub const COPY_KEYCODE: u16 = 46;

/// The key that pastes: `v`, with Ctrl.
pub const PASTE_KEYCODE: u16 = 47;

/// The element key on the confirmation dialog's title bar.
pub const CONFIRM_TITLE_KEY: u64 = 20;

/// Where the properties dialog's element keys start.
pub const PROPS_KEY: u64 = 30;

/// The element key on its question.
pub const CONFIRM_TEXT_KEY: u64 = 21;

/// The element key on its *delete* answer — the left button.
pub const CONFIRM_DELETE_KEY: u64 = 22;

/// The element key on its *keep* answer — the right button.
pub const CONFIRM_KEEP_KEY: u64 = 23;
