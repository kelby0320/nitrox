//! The widget set, bounded by what Milestone 5's terminal needs.
//!
//! **Widgets are functions returning [`Element`]s**, not node kinds. A button is a fill with
//! a label on it that happens to carry a message; the runtime knows nothing about buttons.
//! That falls out of the declarative model rather than being a decision on top of it — and it
//! means a widget's correctness is the correctness of a pure function returning a tree, which
//! is the cheapest kind of thing to test in this repository.
//!
//! ## Widgets take their state as an argument
//!
//! There is no hover flag hidden inside a `Button`. `view(&state) -> Element` is the model, so
//! whether a button is hovered is *state*, and the application passes it in — reading it from
//! [`Router::inside`](crate::route::Router::inside) if it wants to. The alternative is
//! interaction state retained in the widget tree, which `widget-toolkit.md` §3 reserves for
//! things the application has no opinion about, and a button's appearance is not one of them.
//!
//! That keeps every widget here a pure function of its arguments. It also means a widget
//! cannot animate on its own, which is deferred along with the frame clock (§11).

use alloc::string::String;
use alloc::vec::Vec;
use libdraw::format::Rgb;
use libdraw::geom::Size;
// **Re-exported, because every widget here takes one.** A caller importing `button` and
// `list_view` from this module should not have to reach into `libdraw` for the third argument
// they all share; `libui::paint::Theme` names the same type for the painting half.
pub use libdraw::theme::Theme;

use librsproto::surface::{POINTER_BUTTON, POINTER_PRESSED, PointerEvent};

use crate::element::{
    Edge, Element, IconKind, Insets, TextSize, bevel, bold, center, center_v, column, dock, docked, fill, icon, ink,
    outline, padding, rounded_fill, row, scaled, sized, stack, text, wash, with_spacing,
};
// The editing keys. **Imported, not re-declared** — `libkern::abi` publishes these and
// `libterm::encode` already imports exactly this set from there, so a second copy is a second
// thing that can disagree about a key. The same argument the `libinput` dependency is
// justified by, applied to the codes as well as the mapping (PR #233 review, finding 3).
// `libinput::keymap` does not map them because it answers "what text does this produce", and
// these produce none.
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

/// How a widget should look, given what the application knows about it.
///
/// Passed in rather than remembered, so a widget stays a function of its arguments.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct WidgetState {
    /// The pointer is inside it.
    pub hovered: bool,
    /// A button is held down on it.
    pub pressed: bool,
    /// It holds widget focus **and** its window has the keyboard — see
    /// [`Focus::is_active`](crate::route::Focus::is_active). One bool here rather than two,
    /// because a widget paints the same whichever of the two is missing.
    pub active: bool,
}

/// Space between a button's label and its edge.
const BUTTON_PAD: Insets = Insets { top: 4, right: 8, bottom: 4, left: 8 };

/// A labelled button that sends `msg` when clicked.
///
/// Focusable, because a keyboard user must be able to reach it — and the focus ring is drawn
/// from `state.active` rather than from focus alone, so a button in an unfocused window does
/// not claim the keyboard it does not have.
pub fn button<Msg>(label: impl Into<String>, msg: Msg, state: WidgetState, theme: &Theme) -> Element<Msg> {
    let face = if state.pressed {
        theme.face_pressed
    } else if state.hovered {
        theme.face_hover
    } else {
        theme.face
    };
    // Bottom to top: the ring or the border, then the face, then the label. A `Stack` gives
    // every layer the whole area, so the outline is only visible because the face above it is
    // inset — and the label sits above both.
    //
    // **An edge, always** (M15). A face at `#EDECEB` on a window at `#FFFFFF` is a difference of
    // eighteen units per channel: technically not the ground, and in a real window indisputably
    // invisible — the report was that buttons need "a different color background … so you know
    // it's a button". What actually says *button* is the edge, which is what every desktop
    // draws and what this toolkit had only around a focused control.
    //
    // **Rounded since the desktop refresh's Part H**, to `CONTROL_RADIUS` — the curve a window,
    // a popup, a tab and a field all share. A square button beside a rounded field was the last
    // right angle on these surfaces.
    let (edge, thickness) = if state.active { (theme.accent, RING) } else { (theme.border, BORDER) };
    // **Centred, which is what a button's label is everywhere else in the world.** It was against
    // the top-left corner of the face: `padding` places a child at an inset from the origin, and
    // a face is usually much wider than the word on it, so every button in this toolkit read as
    // a label with a box drawn round it. `center` is the node that was missing.
    let layers = alloc::vec![
        rounded_fill(edge, CONTROL_RADIUS),
        padding(Insets::all(thickness), rounded_fill(face, CONTROL_RADIUS.saturating_sub(thickness))),
        center(padding(BUTTON_PAD, text(label))),
    ];
    stack(layers).on_press(msg).focusable()
}


/// The radius every small control on a surface is rounded to: the design's `--r`, 8 — the same
/// curve a window and a popup are cut to.
///
/// **Public, so the shell's bar shares it** rather than keeping a second 8 of its own. A field in
/// a window and a button on the bar being the same shape is the thing the design is consistent
/// about (desktop refresh, Part D).
pub const CONTROL_RADIUS: u32 = 8;

/// How wide the focus ring is, in pixels.
const RING: u32 = 2;

/// How wide a resting control's edge is, in pixels.
///
/// **One, because it is a line rather than a ring.** The focus ring is two so that it reads as
/// a state; an edge is what makes a shape a control at rest, and a second pixel of it would
/// compete with the ring instead of sitting under it.
const BORDER: u32 = 1;



#[cfg(test)]
mod list_view_tests;

#[cfg(test)]
mod text_field_tests;

#[cfg(test)]
mod tests;
