//! The element keys and keycodes the browser's events are routed by.

use super::*;

/// The element key on the column header.
pub const HEADER_KEY: u64 = 21;

/// The element key on the pane holding the header and the listing under it.
///
/// **The listing keeps [`LIST_KEY`]**, which the gates and this crate's tests find it by; this
/// names the pair, because the window's dock wants every child keyed and the pane is now a child.
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
/// **Always present, empty when there is nothing to say.** A child that appears and disappears
/// changes the strip's child count between frames, and the diff pairs children by key — an
/// element that is sometimes there is one the reconciler has to rebuild the row around.
pub const NOTICE_KEY: u64 = 7;

/// The element key on the path strip.
///
/// **Every docked child needs one**, not just the ones a test looks for: the diff requires a
/// container's children to be all keyed or all unkeyed, and a single bare sibling makes the
/// whole frame `MixedKeying` — **on the first frame**, because `reconcile_children` runs the
/// check against an empty previous list for a brand-new node too. In this application that
/// reaches `fail(b"nxfiles: the view is not diffable")` before anything is committed, so the
/// symptom is *no window ever appears*.
///
/// **Not to be confused with `KeyingChanged`**, which is the second-frame error: it needs a
/// successful first frame to compare against, and its symptom is the one that sounds like an
/// event-loop bug — a window that paints once and then never updates. Two errors, two symptoms;
/// an earlier version of this note merged them under the wrong name (PR #257 review,
/// blocking 2).
pub const STRIP_KEY: u64 = 5;

/// Where the menu bar's words are keyed from: `MENU_BAR_KEY + i` for menu `i`.
///
/// [`libui::layout::locate`] turns each into the rectangle its popup hangs under.
pub const MENU_BAR_KEY: u64 = 10;

/// Where the open popup's rows are keyed from: `MENU_ROW_KEY + i` for item `i`.
///
/// **Keys are what make hover possible**: `Router::inside` reports the id of the keyed widget
/// under the pointer, so an unkeyed row is one the router cannot name (M11 Part E batch 3). A
/// separate range from the bar's, because a row and a word are different widgets.
pub const MENU_ROW_KEY: u64 = 100;

/// The element key on the name field that replaces the path while a prompt is open.
pub const PROMPT_KEY: u64 = 17;

/// The element key on the menu bar.
pub const BAR_KEY: u64 = 18;

/// The element key on the path strip's inner row — the half that is either the path or a prompt.
pub const STRIP_INNER_KEY: u64 = 19;

/// The element key on the tab strip.
pub const TAB_STRIP_KEY: u64 = 24;

/// Where pane keys start — see `nxedit::TAB_KEY_BASE`, whose reasoning is the same.
///
/// **The top bit matters more here than there.** This browser keys its list rows by *index*, and
/// a directory can hold any number of entries — so a base of a few thousand would put row `n` and
/// a tab on the same number for a large enough folder, and hovering one would highlight the other
/// (PR #270 review, optional 6). A row index cannot reach the high bit.
pub const TAB_KEY_BASE: u64 = 1 << 63;

/// The element key on the sidebar.
pub const SIDEBAR_KEY: u64 = 26;

/// Where the sidebar's rows are keyed from — clear of the listing's and of the chrome's, for the
/// reason [`LIST_ROW_KEY`] gives.
///
/// **Unreachable rather than merely distant**, which is the difference `TAB_KEY_BASE` already
/// knew and this did not: at `2000` it sat exactly `1000` above `LIST_ROW_KEY`, so listing row
/// 1000 *was* the sidebar's first row and hovering either lit both. A directory with a thousand
/// entries is ordinary; one with `1 << 62` is not (PR #285 review, worth fixing 4).
pub const SIDEBAR_ROW_KEY: u64 = 1 << 62;

/// Where the sidebar's **drive** rows are keyed from (Phase 6 Part F): `DRIVE_ROW_KEY + n` for the
/// drive on block device `n`.
///
/// **Keyed by the device, not by the row's position**, because the drives change under a person's
/// pointer: a stick pulled between a frame and the click on the row below it would otherwise make
/// that click land on whatever moved up into its place — and on an eject button, eject another
/// stick. A device's index is never reused while its record is in the table. Inside the sidebar's
/// range, clear of the places below it by bit 61, which no place index reaches.
pub const DRIVE_ROW_KEY: u64 = SIDEBAR_ROW_KEY | (1 << 61);

/// Where their eject buttons are keyed from, by the same `n`: clear of the rows by bit 60, which
/// no device index reaches ([`drives`] drops one that would).
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
/// **A range of their own, because `hovered` is one namespace.** `Router::hovered_key` reports
/// whatever is under the pointer and the chrome compares that against its own constants — so rows
/// keyed by their bare index lit whichever button shared the number, and rows 1, 2, 5, 6 and 7 all
/// did (`LIST_KEY`, `UP_KEY`, `STRIP_KEY`, `PATH_KEY`, `NOTICE_KEY`). `TAB_KEY_BASE` worked this
/// out for tabs and the chrome was left with the same hazard; `a_listing_row_does_not_light_the_
/// chrome` now holds the two apart. The same defect the chooser had, found in PR #284's review.
pub const LIST_ROW_KEY: u64 = 1000;

/// The row a listing key names, or `None` if it is not a listing row's.
pub(super) fn list_row_of(key: u64) -> Option<usize> {
    key.checked_sub(LIST_ROW_KEY).map(|i| i as usize)
}

/// `Msg::Press` for the row a key names — `list_view` hands back the key it was given.
pub(super) fn press_row(key: u64) -> Msg {
    Msg::Press(key)
}

/// `Msg::Grab` likewise.
pub(super) fn grab_row(key: u64) -> Msg {
    Msg::Grab(key)
}

/// The key that opens a *window*: `n`, with Ctrl and **Shift**.
///
/// Shift because `Ctrl+N` is "new" in the singular everywhere, and a browser's singular is a tab.
pub const NEW_WINDOW_KEYCODE: u16 = 49;

/// The key that quits: `q`, with Ctrl. **Quit is not Close** — it is every window of this
/// browser, where Close is this one.
pub const QUIT_KEYCODE: u16 = 16;

/// The key that opens a tab: `t`. Pinned against the keymap by a test, as `nxedit`'s are.
pub const NEW_TAB_KEYCODE: u16 = 20;

/// The key that closes one: `w`.
pub const CLOSE_TAB_KEYCODE: u16 = 17;

/// The key that shows and hides the entries a leading dot marks: `h`, with Ctrl.
///
/// **`Ctrl+H` rather than a `Super` chord**, which the compositor owns: every one of those is the
/// shell's, and a browser binding one would be a keystroke the guest never sees.
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
