//! The status bar and pills.

use super::*;

/// How tall a status bar is: the design's 25.
pub const STATUS_BAR_H: u32 = 25;

/// A status bar's inset at either end: the design's 11.
const STATUS_PAD_X: u32 = 11;

/// A window's status bar: what it is doing at one end, where it is at the other.
///
/// The rule is on the edge that faces the content: `Edge::Top` for a bar along the bottom of a
/// window, `Edge::Bottom` for one under the chrome.
///
/// The readings are [`TextSize::Small`] and `foreground_dim`, which carries "read this second".
/// The face is the caller's: a position readout may sit in `mono`.
pub fn status_bar<Msg>(left: Element<Msg>, right: Option<Element<Msg>>, rule: Edge, theme: &Theme) -> Element<Msg> {
    // Both slots wrapped, and the zero-inset one is not decoration: a container's children are
    // all keyed or none, and a caller keying its readout would make this dock `MixedKeying`.
    let bare = |e: Element<Msg>| padding(Insets::all(0), e);
    let inner = match right {
        Some(right) => dock(alloc::vec![docked(Edge::Right, bare(right))], bare(left)),
        None => left,
    };
    sized(
        Size::new(0, STATUS_BAR_H),
        stack(alloc::vec![
            fill(theme.face_hover),
            dock(alloc::vec![docked(rule, sized(Size::new(0, 1), fill(theme.border)))], text(""),),
            padding(Insets { top: 0, right: STATUS_PAD_X, bottom: 0, left: STATUS_PAD_X }, center_v(inner),),
        ]),
    )
}

/// One reading on a status bar: a step below the body, in the ink a second read gets.
pub fn status_text<Msg>(s: impl Into<String>, theme: &Theme) -> Element<Msg> {
    ink(theme.foreground_dim, scaled(TextSize::Small, text(s)))
}

/// The upright between two readings on a status bar — the design's `|`, in the line colour.
pub fn status_separator<Msg>(theme: &Theme) -> Element<Msg> {
    padding(
        Insets { top: 0, right: STATUS_GAP, bottom: 0, left: STATUS_GAP },
        ink(theme.border, scaled(TextSize::Small, text("|"))),
    )
}

/// Between two readings on a status bar — either side of a separator, or between a control and
/// the reading next to it. The design's 16 from one to the next, less the bar it draws between.
pub const STATUS_GAP: u32 = 7;

/// A primary action: the accent as a ground, with its label in the paper colour.
///
/// The one button in a window that is the answer. Hover and press shade the ground rather than
/// swapping it, since a changing hue would read as a different control.
pub fn pill<Msg>(label: impl Into<String>, msg: Msg, state: WidgetState, theme: &Theme) -> Element<Msg> {
    let ground = if state.pressed {
        theme.accent.shade(-PILL_SHADE)
    } else if state.hovered {
        theme.accent.shade(PILL_SHADE)
    } else {
        theme.accent
    };
    // The label is whichever of the surface's two inks stands furthest from the ground: the dark
    // scheme's near-black paper would sit at 3.8:1 on the accent.
    let ink_colour = furthest_from(ground, theme.background, theme.foreground);
    let mut layers = alloc::vec::Vec::with_capacity(4);
    layers.push(rounded_fill(ground, CONTROL_RADIUS));
    if state.active {
        // A band of the label's colour inside the edge; it must be a band, since the ground
        // over a ground of the same colour puts down not one different pixel.
        layers.push(padding(Insets::all(PILL_RING_INSET), rounded_fill(ink_colour, CONTROL_RADIUS - PILL_RING_INSET)));
        layers.push(padding(
            Insets::all(PILL_RING_INSET + RING),
            rounded_fill(ground, CONTROL_RADIUS - PILL_RING_INSET - RING),
        ));
    }
    layers.push(center(padding(PILL_PAD, ink(ink_colour, text(label)))));
    stack(layers).on_press(msg).focusable()
}

/// How tall a pill is where a caller sizes one: the design's 21, which is a control sitting on a
/// 24-pixel row rather than filling it.
pub const PILL_H: u32 = 21;

/// How far inside a pill's edge its focus ring is drawn.
const PILL_RING_INSET: u32 = 1;

/// Whichever of `a` and `b` stands further from `ground`, by weighted brightness.
///
/// `0.299r + 0.587g + 0.114b` in integers: `libdraw` has no floating-point maths beyond
/// arithmetic.
///
/// It picks and does not promise: the dark scheme's 3.9:1 is under WCAG's 4.5 and cannot be
/// fixed here, since `accent` is one colour in both schemes.
pub(super) fn furthest_from(ground: Rgb, a: Rgb, b: Rgb) -> Rgb {
    let brightness = |c: Rgb| 299 * c.r as i32 + 587 * c.g as i32 + 114 * c.b as i32;
    let (g, a_, b_) = (brightness(ground), brightness(a), brightness(b));
    if (a_ - g).abs() >= (b_ - g).abs() { a } else { b }
}

/// A pill's sides: the design's 12.
const PILL_PAD: Insets = Insets { top: 2, right: 12, bottom: 2, left: 12 };

/// How far a pill's ground moves under the pointer, and the other way when pressed.
const PILL_SHADE: i16 = 12;
