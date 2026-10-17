//! The widget set.
//!
//! **Widgets are functions returning [`Element`]s**, not node kinds; the runtime knows nothing
//! about buttons.
//!
//! **Widgets take their state as an argument**: whether a button is hovered is *state*, and the
//! application passes it in (e.g. from [`Router::inside`](crate::route::Router::inside)).

use alloc::string::String;
use alloc::vec::Vec;
use libdraw::format::Rgb;
use libdraw::geom::Size;
pub use libdraw::theme::Theme;

use librsproto::surface::{POINTER_BUTTON, POINTER_PRESSED, PointerEvent};

use crate::element::{
    Edge, Element, IconKind, Insets, TextSize, bevel, bold, center, center_v, column, dock, docked, fill, icon, ink,
    outline, padding, rounded_fill, row, scaled, sized, stack, text, wash, with_spacing,
};
use libkern::abi::{KEY_BACKSPACE, KEY_DELETE, KEY_DOWN, KEY_END, KEY_ENTER, KEY_HOME, KEY_LEFT, KEY_RIGHT, KEY_UP};
use librsproto::surface::MOD_SHIFT;

mod bars;
mod frames;
mod list;
mod scroll;
mod status;
mod tabs;
mod text_area;
mod text_field;

pub use bars::*;
pub use frames::*;
pub use list::*;
pub use scroll::*;
pub use status::*;
pub use tabs::*;
pub use text_area::*;
pub use text_field::*;

/// How a widget should look, given what the application knows about it; passed in rather than
/// remembered.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct WidgetState {
    /// The pointer is inside it.
    pub hovered: bool,
    /// A button is held down on it.
    pub pressed: bool,
    /// It holds widget focus **and** its window has the keyboard (see
    /// [`Focus::is_active`](crate::route::Focus::is_active)); one bool, as a widget paints the same
    /// whichever is missing.
    pub active: bool,
}

const BUTTON_PAD: Insets = Insets { top: 4, right: 8, bottom: 4, left: 8 };

/// A labelled button that sends `msg` when clicked; the focus ring follows `state.active`, so a
/// button in an unfocused window does not claim the keyboard.
pub fn button<Msg>(label: impl Into<String>, msg: Msg, state: WidgetState, theme: &Theme) -> Element<Msg> {
    let face = if state.pressed {
        theme.face_pressed
    } else if state.hovered {
        theme.face_hover
    } else {
        theme.face
    };
    // Bottom to top: the ring or border, the face, the label. **An edge, always**: a face barely
    // off the window colour is invisible; the edge says *button*.
    let (edge, thickness) = if state.active { (theme.accent, RING) } else { (theme.border, BORDER) };
    let layers = alloc::vec![
        rounded_fill(edge, CONTROL_RADIUS),
        padding(Insets::all(thickness), rounded_fill(face, CONTROL_RADIUS.saturating_sub(thickness))),
        center(padding(BUTTON_PAD, text(label))),
    ];
    stack(layers).on_press(msg).focusable()
}

/// The radius every small control on a surface is rounded to (the design's `--r`), **public so the
/// shell's bar shares it**.
pub const CONTROL_RADIUS: u32 = 8;

/// How wide the focus ring is, in pixels.
const RING: u32 = 2;

/// How wide a resting control's edge is: **one**, as a second pixel would compete with the focus
/// ring.
const BORDER: u32 = 1;

#[cfg(test)]
mod list_view_tests;

#[cfg(test)]
mod text_field_tests;

#[cfg(test)]
mod tests;
