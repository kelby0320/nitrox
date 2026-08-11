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
use libdraw::format::Rgb;
use libdraw::geom::Size;

use crate::element::{Element, Insets, column, fill, padding, row, sized, stack, text};

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

/// The colours a widget draws itself in.
///
/// Constants rather than a theming system, for the reason `widget-toolkit.md` §11 gives —
/// and shaped as a struct so that becoming one later is a change of provenance rather than a
/// change of every call site.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Palette {
    /// A button's face at rest.
    pub face: Rgb,
    /// Its face under the pointer.
    pub face_hover: Rgb,
    /// Its face while held.
    pub face_pressed: Rgb,
    /// The ring drawn around the focused widget.
    pub focus_ring: Rgb,
    /// A scrollbar's groove.
    pub track: Rgb,
    /// A scrollbar's thumb.
    pub thumb: Rgb,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            face: Rgb::new(0x24, 0x2C, 0x36),
            face_hover: Rgb::new(0x30, 0x3A, 0x46),
            face_pressed: Rgb::new(0x18, 0x1E, 0x26),
            focus_ring: Rgb::new(0x5A, 0x9F, 0xD4),
            track: Rgb::new(0x18, 0x1E, 0x26),
            thumb: Rgb::new(0x3A, 0x46, 0x54),
        }
    }
}

/// Space between a button's label and its edge.
const BUTTON_PAD: Insets = Insets { top: 4, right: 8, bottom: 4, left: 8 };

/// A labelled button that sends `msg` when clicked.
///
/// Focusable, because a keyboard user must be able to reach it — and the focus ring is drawn
/// from `state.active` rather than from focus alone, so a button in an unfocused window does
/// not claim the keyboard it does not have.
pub fn button<Msg>(
    label: impl Into<String>,
    msg: Msg,
    state: WidgetState,
    palette: &Palette,
) -> Element<Msg> {
    let face = if state.pressed {
        palette.face_pressed
    } else if state.hovered {
        palette.face_hover
    } else {
        palette.face
    };
    // Bottom to top: the ring, then the face, then the label. A `Stack` gives every layer
    // the whole area, so the ring is only visible because the face above it is inset by the
    // ring's width — and the label sits above both, inset further so it clears the edge.
    let mut layers = alloc::vec::Vec::with_capacity(3);
    if state.active {
        layers.push(fill(palette.focus_ring));
        layers.push(padding(Insets::all(RING), fill(face)));
    } else {
        layers.push(fill(face));
    }
    layers.push(padding(BUTTON_PAD, text(label)));
    stack(layers).on_press(msg).focusable()
}

/// How wide the focus ring is, in pixels.
const RING: u32 = 2;

/// Where a scrollbar is and how much of its content is visible.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScrollState {
    /// First visible unit — a line, for a terminal.
    pub offset: u32,
    /// How many units are visible at once.
    pub visible: u32,
    /// How many units there are in total.
    pub total: u32,
}

/// The smallest a thumb may get, in pixels.
///
/// A thumb proportional to a long scrollback shrinks to nothing and stops being grabbable;
/// every real scrollbar clamps it, and the clamp is what makes the position arithmetic
/// non-obvious enough to be worth testing.
pub const MIN_THUMB: u32 = 16;

impl ScrollState {
    /// Whether there is anything to scroll.
    pub fn scrollable(&self) -> bool {
        self.total > self.visible
    }

    /// The thumb's length and offset along a track of `track` pixels.
    ///
    /// Returns `(offset, length)`. With nothing to scroll the thumb fills the track, which is
    /// how a user sees "this is all of it" rather than seeing a full-height thumb that
    /// nevertheless moves.
    pub fn thumb(&self, track: u32) -> (u32, u32) {
        if !self.scrollable() || self.visible == 0 || track == 0 {
            return (0, track);
        }
        // Proportional, then clamped — and clamped *before* the position is computed, or the
        // thumb runs past the end of the track by however much the clamp added.
        let proportional = (track as u64 * self.visible as u64 / self.total as u64) as u32;
        let len = proportional.max(MIN_THUMB).min(track);
        let span = track - len;
        let max_offset = self.total - self.visible;
        let pos = (span as u64 * self.offset.min(max_offset) as u64 / max_offset as u64) as u32;
        (pos, len)
    }
}

/// A vertical scrollbar `width` pixels across.
///
/// Built out of layout rather than arithmetic on a canvas: a `Column` of a spacer, the thumb
/// and a filler places the thumb without any node needing to offset its child. That is why
/// the toolkit has no `Offset` primitive — this was the thing that would have wanted one.
pub fn scrollbar<Msg>(state: ScrollState, width: u32, height: u32, palette: &Palette) -> Element<Msg> {
    let (pos, len) = state.thumb(height);
    sized(
        Size::new(width, 0),
        stack(alloc::vec![
            fill(palette.track),
            column(alloc::vec![
                sized(Size::new(0, pos), fill(palette.track)),
                sized(Size::new(0, len), fill(palette.thumb)),
                // The remainder, so the thumb does not stretch to the bottom.
                fill(palette.track).flex(1),
            ]),
        ]),
    )
}

/// A horizontal bar of buttons — a menu bar, or a toolbar.
///
/// The popup half of a menu is not here: an open menu is a `stack` layer the application adds
/// over its content, which needs the popup positioned under its item and is Milestone 5's
/// first real requirement for it. This is the part the terminal's chrome needs.
pub fn menu_bar<Msg>(items: alloc::vec::Vec<Element<Msg>>, height: u32, palette: &Palette) -> Element<Msg> {
    sized(
        Size::new(0, height),
        stack(alloc::vec![fill(palette.face), row(items)]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{FixedCell, layout};
    use crate::paint::{Theme, paint};
    use alloc::vec;
    use libdraw::format::PixelFormat;
    use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
    use libdraw::geom::Rect;

    type Msg = ();

    const CELL: FixedCell = FixedCell { w: 8, h: 16 };

    #[test]
    fn a_thumb_is_proportional_to_what_is_visible() {
        let s = ScrollState { offset: 0, visible: 25, total: 100 };
        let (pos, len) = s.thumb(200);
        assert_eq!(len, 50, "a quarter visible is a quarter of the track");
        assert_eq!(pos, 0, "at the top");
    }

    #[test]
    fn a_thumb_reaches_the_bottom_exactly_at_the_last_line() {
        // Off by one here leaves a gap at the bottom that says "there is more" when there is
        // not, or runs the thumb past the end.
        let s = ScrollState { offset: 75, visible: 25, total: 100 };
        let (pos, len) = s.thumb(200);
        assert_eq!(pos + len, 200, "flush with the end of the track");
    }

    #[test]
    fn a_short_thumb_is_clamped_and_still_reaches_both_ends() {
        // A thumb proportional to a long scrollback shrinks below grabbing size. Clamping it
        // *after* computing the position runs it past the track's end by whatever the clamp
        // added — which is why the clamp comes first.
        let s = ScrollState { offset: 0, visible: 1, total: 10_000 };
        let (pos, len) = s.thumb(200);
        assert_eq!(len, MIN_THUMB, "clamped to something grabbable");
        assert_eq!(pos, 0);

        let s = ScrollState { offset: 9_999, visible: 1, total: 10_000 };
        let (pos, len) = s.thumb(200);
        assert_eq!(len, MIN_THUMB);
        assert_eq!(pos + len, 200, "and the clamped thumb still reaches the bottom");
    }

    #[test]
    fn nothing_to_scroll_fills_the_track() {
        // How a user sees "this is all of it". A full-height thumb that nevertheless moves is
        // the alternative, and it is nonsense.
        for s in [
            ScrollState { offset: 0, visible: 100, total: 100 },
            ScrollState { offset: 0, visible: 200, total: 100 },
        ] {
            assert_eq!(s.thumb(200), (0, 200), "{s:?}");
            assert!(!s.scrollable());
        }
    }

    #[test]
    fn degenerate_scroll_states_do_not_divide_by_zero() {
        // `total == visible` makes `max_offset` zero and `visible == 0` makes the proportion
        // zero; a scrollbar for an empty document is an ordinary thing to ask for.
        assert_eq!(ScrollState { offset: 0, visible: 0, total: 0 }.thumb(200), (0, 200));
        assert_eq!(ScrollState { offset: 5, visible: 0, total: 10 }.thumb(200), (0, 200));
        assert_eq!(ScrollState { offset: 0, visible: 10, total: 10 }.thumb(0), (0, 0));
    }

    #[test]
    fn an_offset_past_the_end_is_clamped_rather_than_overflowing() {
        // An application that scrolled past the end must not produce a thumb outside the
        // track — and the arithmetic is unsigned, so getting it wrong wraps rather than
        // going negative.
        let s = ScrollState { offset: 10_000, visible: 25, total: 100 };
        let (pos, len) = s.thumb(200);
        assert_eq!(pos + len, 200);
    }

    #[test]
    fn a_scrollbar_lays_its_thumb_out_where_the_arithmetic_says() {
        // The layout half: a `Column` of spacer, thumb and filler places the thumb without
        // any node needing to offset a child.
        let p = Palette::default();
        let s = ScrollState { offset: 50, visible: 25, total: 100 };
        let e: Element<Msg> = scrollbar(s, 12, 400, &p);
        let l = layout(&e, Rect::new(0, 0, 12, 400), &CELL);
        let (pos, len) = s.thumb(400);

        // stack -> [track fill, column] ; column -> [spacer, thumb, filler]
        let col = &l.children[0].children[1];
        assert_eq!(col.children[0].rect.size.h, pos, "the spacer is the thumb's offset");
        assert_eq!(col.children[1].rect.size.h, len, "and then the thumb");
        assert_eq!(col.children[1].rect.origin.y, pos as i32);
        assert_eq!(l.rect.size.w, 12, "the bar is as wide as asked");
    }

    #[test]
    fn a_button_carries_its_message_and_takes_focus() {
        let p = Palette::default();
        let e = button("OK", (), WidgetState::default(), &p);
        assert!(e.on_press.is_some(), "a button that sends nothing is not a button");
        assert!(e.focusable, "a keyboard user must be able to reach it");
    }

    #[test]
    fn a_buttons_face_follows_the_state_it_is_given() {
        // Widgets take their state as an argument rather than remembering it, so this is the
        // whole of a button's appearance logic and it is a pure function.
        let p = Palette::default();
        let face_of = |st: WidgetState| {
            let e: Element<Msg> = button("OK", (), st, &p);
            // The first layer is the face at rest and the ring when active; either way the
            // painted face is the last `Fill` before the label.
            let fills: vec::Vec<Rgb> = e
                .children()
                .filter_map(|c| match &c.node {
                    crate::element::Node::Fill(c) => Some(*c),
                    crate::element::Node::Padding { child, .. } => match &child.node {
                        crate::element::Node::Fill(c) => Some(*c),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            *fills.last().expect("a button has a face")
        };
        assert_eq!(face_of(WidgetState::default()), p.face);
        assert_eq!(face_of(WidgetState { hovered: true, ..Default::default() }), p.face_hover);
        assert_eq!(
            face_of(WidgetState { hovered: true, pressed: true, ..Default::default() }),
            p.face_pressed,
            "pressed wins over hovered — a held button is held wherever the pointer is"
        );
    }

    #[test]
    fn a_focused_button_draws_a_ring_that_the_face_does_not_cover() {
        // Painted rather than inspected: the ring is the outermost layer, so a face drawn
        // over the whole area would hide it and the tree would still look right.
        let p = Palette::default();
        let t = Theme::default();
        let mut fb =
            MemFramebuffer::new(Geometry::packed(80, 40, PixelFormat::XRGB8888));
        let e: Element<Msg> =
            button("OK", (), WidgetState { active: true, ..Default::default() }, &p);
        let l = layout(&e, Rect::new(0, 0, 80, 40), &CELL);
        paint(&mut fb, &font(), &t, &e, &l, Rect::new(0, 0, 80, 40), &mut |_, _, _, _: &mut MemFramebuffer| {});
        assert_eq!(fb.get_pixel(0, 0), Some(p.focus_ring), "the ring is on the edge");
        assert_eq!(fb.get_pixel(40, 20), Some(p.face), "and the face is inside it");
    }

    #[test]
    fn an_unfocused_button_draws_no_ring() {
        let p = Palette::default();
        let t = Theme::default();
        let mut fb = MemFramebuffer::new(Geometry::packed(80, 40, PixelFormat::XRGB8888));
        let e: Element<Msg> = button("OK", (), WidgetState::default(), &p);
        let l = layout(&e, Rect::new(0, 0, 80, 40), &CELL);
        paint(&mut fb, &font(), &t, &e, &l, Rect::new(0, 0, 80, 40), &mut |_, _, _, _: &mut MemFramebuffer| {});
        assert_eq!(fb.get_pixel(0, 0), Some(p.face), "face all the way to the edge");
    }

    #[test]
    fn a_menu_bar_is_as_tall_as_asked_and_no_taller() {
        let p = Palette::default();
        let e: Element<Msg> = menu_bar(vec![text("File"), text("Edit")], 24, &p);
        let l = layout(&e, Rect::new(0, 0, 200, 100), &CELL);
        assert_eq!(l.rect.size.h, 24);
        assert_eq!(l.rect.size.w, 200, "and spans the width it is given");
    }

    const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSansMono.ttf");

    fn font() -> libdraw::text::Font {
        libdraw::text::Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses")
    }
}
