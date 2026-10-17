//! `nxfiles` — the file browser, minus the syscalls.
//!
//! What a listing *is* — sorted, marked, navigable — is a function of values and host-tests
//! without a disk: [`App::show`] is handed entries the binary fetched with [`libfs::list_dir`].
//! Which entries a path holds (the filesystem's plus the namespace bindings mounted there,
//! bindings shadowing) is `libfs`'s rule, shared with `list`; re-deriving it would show a mount
//! point twice.

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

/// A listing row's height in pixels.
pub const ROW_H: u32 = 25;

/// How far the pointer must travel with a button down before a press becomes a drag, in pixels.
///
/// A click is a press that moved a little; four survives a hand.
pub const DRAG_SLOP: i32 = 4;

/// The menu bar's height in pixels.
pub const MENU_BAR_H: u32 = 24;

/// How wide the sidebar of common locations is, in pixels.
///
/// A fixed metric: the gates aim at its rows, and a theme-dependent width would move under them.
pub const SIDEBAR_W: u32 = 132;

/// How much space sits between the quick-access panel and everything around it: none.
///
/// Kept as a named zero: the sums that place a sidebar row read it and a gate writes it down.
pub const SIDEBAR_PAD: u32 = 0;

/// The place a sidebar key names, or `None` if it is not a sidebar row's.
fn place_of(key: u64) -> Option<usize> {
    key.checked_sub(SIDEBAR_ROW_KEY).map(|i| i as usize)
}

#[cfg(test)]
mod tests;
