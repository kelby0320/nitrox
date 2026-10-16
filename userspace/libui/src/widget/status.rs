//! The status bar and pills.

use super::*;

/// How tall a status bar is: the design's 25.
pub const STATUS_BAR_H: u32 = 25;

/// A status bar's inset at either end: the design's 11.
const STATUS_PAD_X: u32 = 11;

/// A window's status bar: what it is doing at one end, where it is at the other.
///
/// **The rule is on the edge that faces the content** — `Edge::Top` for a bar along the bottom of
/// a window, which is where the design puts one and where the editor's went in Part J, and
/// `Edge::Bottom` for one that sits under the chrome. A bar with its rule on the wrong side reads
/// as a lid rather than a floor.
///
/// **The readings are [`TextSize::Small`] and `foreground_dim`** — the design's hierarchy by size
/// and ink, which is what carries the "read this second" (desktop refresh, Part H).
///
/// **The face is the caller's**, and this paragraph used to say otherwise: it argued that a
/// status bar in the fixed-advance face would be the toolkit's first two-face surface "for the
/// sake of a byte count". Part J gave `libui` a `mono` wrapper for the editor's buffer, and the
/// editor wraps its own position readout in it — numbers that shuffle the line about as a caret
/// moves are what a fixed advance is for. A bar set wholly in it would still be wrong; a reading
/// inside one is the caller's judgement (PR #321 review, worth fixing 5).
pub fn status_bar<Msg>(left: Element<Msg>, right: Option<Element<Msg>>, rule: Edge, theme: &Theme) -> Element<Msg> {
    // **Both slots wrapped, and the zero-inset one is not decoration** — `frame_layers`'s rule,
    // for its reason: the diff wants a container's children all keyed or none, and a caller that
    // keys its readout would otherwise make this dock `MixedKeying` at the first frame. The
    // wrapper puts the caller's keys one level down, where they still do their job.
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
/// **The one button in a window that is the answer** — the design's `Save`, and the only place it
/// fills a control with the accent rather than drawing with it. Everything else on a surface is a
/// face with an edge, which is what makes this one read as the action rather than as an option
/// (desktop refresh, Part H).
///
/// Hover and press shade the ground rather than swapping it, because an accent that changed hue
/// under the pointer would read as a different control.
pub fn pill<Msg>(label: impl Into<String>, msg: Msg, state: WidgetState, theme: &Theme) -> Element<Msg> {
    let ground = if state.pressed {
        theme.accent.shade(-PILL_SHADE)
    } else if state.hovered {
        theme.accent.shade(PILL_SHADE)
    } else {
        theme.accent
    };
    // **The label is whichever of the surface's two inks stands furthest from the ground** — the
    // window's paper in the light scheme, which is the design's white on the accent, and its
    // text colour in the dark one, where the paper is near-black and would sit at 3.8:1 on the
    // same accent (PR #319 review, optional 9). One rule, so a scheme cannot be picked that
    // makes this control unreadable.
    let ink_colour = furthest_from(ground, theme.background, theme.foreground);
    let mut layers = alloc::vec::Vec::with_capacity(4);
    layers.push(rounded_fill(ground, CONTROL_RADIUS));
    if state.active {
        // **A band of the label's colour, inside the edge** — and it has to be a *band*: this
        // drew the ground inset by the ring over a ground of the same colour, which is the same
        // colour on the same colour and put down not one different pixel (review, optional 4).
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
/// **Weighted rather than a plain sum**, because green carries most of what an eye reads as
/// light: `0.299r + 0.587g + 0.114b`, the usual integer form, which needs no `powf` — `libdraw`
/// builds for a bare target and has no floating-point maths beyond arithmetic.
///
/// **It picks, and does not promise.** On the accent this system ships, it chooses white in the
/// light scheme (4.6:1) and the near-white ink in the dark one (3.9:1, where the window's own
/// paper would be 3.8:1). The dark figure is under WCAG's 4.5 and cannot be fixed here: `accent`
/// is one colour in both schemes, so a readable pill in the dark scheme is a question about the
/// palette, for whoever revisits it (PR #319 review, optional 9).
pub(super) fn furthest_from(ground: Rgb, a: Rgb, b: Rgb) -> Rgb {
    let brightness = |c: Rgb| 299 * c.r as i32 + 587 * c.g as i32 + 114 * c.b as i32;
    let (g, a_, b_) = (brightness(ground), brightness(a), brightness(b));
    if (a_ - g).abs() >= (b_ - g).abs() { a } else { b }
}

/// A pill's sides: the design's 12.
const PILL_PAD: Insets = Insets { top: 2, right: 12, bottom: 2, left: 12 };

/// How far a pill's ground moves under the pointer, and the other way when pressed.
const PILL_SHADE: i16 = 12;
