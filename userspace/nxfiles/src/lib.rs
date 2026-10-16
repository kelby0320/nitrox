//! `nxfiles` — the file browser, minus the syscalls.
//!
//! The split `nxterm` uses: what a listing *is* — sorted, marked, navigable — is a function of
//! values and host-tests in milliseconds; the window, the buffers and the event pump are the
//! binary's.
//!
//! ## What this half does not do
//!
//! **It does not read the filesystem.** [`App::show`] is handed entries that somebody else
//! fetched, which is what makes every rule below testable without a disk: sorting, marking,
//! what a row press means, and where "up" goes are decisions about a `Vec`, not about I/O. The
//! binary calls [`libfs::list_dir`] and hands the result over.
//!
//! **And it does not decide what a listing contains.** That the entries under a path are the
//! filesystem's *plus* the namespace bindings mounted there, with bindings shadowing, is
//! `libfs`'s rule and is shared with `list` — a browser that re-derived it would show a mount
//! point twice, once as the directory it covers.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use alloc::vec;
use libdraw::geom::{Rect, Size};
use librsproto::file::{DIRENT_KIND_DIR, OwnedEntry};
use librsproto::surface::{
    KEY_DOWN, KEY_REPEAT, KeyEvent, MOD_CTRL, MOD_SHIFT, PointerEvent, RESIZE_BOTTOM, RESIZE_RIGHT,
    WINDOW_STATE_MAXIMIZED, WINDOW_STATE_MINIMIZED, WINDOW_STATE_NORMAL,
};
use libui::element::{
    Edge, Element, Insets, center_v, column, dock, docked, ink, padding, row, sized, text, with_spacing,
};
use libui::menu::{Accel, Item, Menu, MenuState};
use libui::widget::{
    ColumnAlign, DIALOG_GAP, ListColumn, ListRow, ListState, RowButton, STATUS_BAR_H, Swatch, TAB_STRIP_H, TITLE_BAR_H,
    TabExtras, TextFieldState, Theme as UiTheme, TitleButtons, WINDOW_FRAME_H, WidgetState, button, dialog_frame,
    list_view, popup_frame, resize_grip, status_bar, status_separator, status_text, tab_strip, text_field, title_bar,
    window_frame_with_grip,
};

mod app;
mod keys;
mod model;
mod msg;
mod popups;
mod text;

pub use app::*;
pub use keys::*;
pub use model::*;
pub use msg::*;
pub use text::*;

/// A listing row's height in pixels: the design's 25, where it was 20.
pub const ROW_H: u32 = 25;

/// How far the pointer must travel with a button down before a press becomes a drag, in pixels.
///
/// **Because a click is a press that moved a little.** Nobody presses a mouse button without
/// nudging it, and a browser that started a drag on the first pixel would make opening a file by
/// clicking it a matter of luck. Four is the smallest number that survives a hand; a person
/// deliberately dragging crosses it immediately.
pub const DRAG_SLOP: i32 = 4;

/// The menu bar's height in pixels — one row of chrome above the path strip.
pub const MENU_BAR_H: u32 = 24;

/// How wide the sidebar of common locations is, in pixels.
///
/// **A fixed metric, like every other piece of chrome here** (M11's decision 2): the gates aim at
/// its rows, and a width that followed the theme's text size would move under a gate that had to
/// read a theme file to know where to click.
pub const SIDEBAR_W: u32 = 132;

/// How much space sits between the quick-access panel and everything around it: none.
///
/// **A margin until the desktop refresh's Part I, and an edge since.** M15 Part E put six pixels
/// around the panel so that the window showing through would make it read as a *panel* rather
/// than as part of the listing — reported from running it, "add some margin around the quick
/// access panel just to separate it from the rest". The design answers that differently: the
/// sidebar runs to the window's edge and its own ground is what separates it, which is what this
/// is now. **Kept as a named zero** rather than deleted, because the sums that place a sidebar
/// row read it and a gate writes it down (PR #320 review, worth fixing 8).
pub const SIDEBAR_PAD: u32 = 0;

/// The place a sidebar key names, or `None` if it is not a sidebar row's.
fn place_of(key: u64) -> Option<usize> {
    key.checked_sub(SIDEBAR_ROW_KEY).map(|i| i as usize)
}

#[cfg(test)]
mod tests;
