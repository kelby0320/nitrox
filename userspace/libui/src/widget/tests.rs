use super::*;
use crate::layout::{FixedCell, layout};
use crate::paint::{Theme, paint};
use alloc::vec;
use libdraw::format::PixelFormat;
use libdraw::format::Rgb;
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
    let s = ScrollState { offset: 75, visible: 25, total: 100 };
    let (pos, len) = s.thumb(200);
    assert_eq!(pos + len, 200, "flush with the end of the track");
}

#[test]
fn a_short_thumb_is_clamped_and_still_reaches_both_ends() {
    // Clamped before the position is computed, or the thumb runs past the track's end by
    // whatever the clamp added.
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
    for s in [ScrollState { offset: 0, visible: 100, total: 100 }, ScrollState { offset: 0, visible: 200, total: 100 }]
    {
        assert_eq!(s.thumb(200), (0, 200), "{s:?}");
        assert!(!s.scrollable());
    }
}

#[test]
fn degenerate_scroll_states_do_not_divide_by_zero() {
    assert_eq!(ScrollState { offset: 0, visible: 0, total: 0 }.thumb(200), (0, 200));
    assert_eq!(ScrollState { offset: 5, visible: 0, total: 10 }.thumb(200), (0, 200));
    assert_eq!(ScrollState { offset: 0, visible: 10, total: 10 }.thumb(0), (0, 0));
}

#[test]
fn an_offset_past_the_end_is_clamped_rather_than_overflowing() {
    let s = ScrollState { offset: 10_000, visible: 25, total: 100 };
    let (pos, len) = s.thumb(200);
    assert_eq!(pos + len, 200);
}

#[test]
fn a_grab_maps_back_to_the_offset_the_thumb_was_drawn_for() {
    // `offset_at` is `thumb`'s inverse and the two must agree: one decides where the thumb is
    // painted, the other what grabbing it there means. Over the whole range, since the ends are
    // where each division and clamp goes wrong. Within a pixel's worth of lines, not exactly:
    // a thousand lines over ~390 pixels puts three lines on a pixel, so no inverse can tell
    // them apart; the ends are exact.
    let s = ScrollState { offset: 0, visible: 24, total: 1024 };
    let track = 400;
    let (_, len) = s.thumb(track);
    let per_pixel = (1000u32).div_ceil(track - len);
    for offset in [0, 1, 7, 500, 999, 1000] {
        let s = ScrollState { offset, ..s };
        let (pos, len) = s.thumb(track);
        let got = s.offset_at(track, pos as i32 + len as i32 / 2);
        assert!(
            got.abs_diff(offset) <= per_pixel,
            "the thumb drawn for {offset} grabs as {got}, more than {per_pixel} lines out",
        );
    }
    assert_eq!(
        ScrollState { offset: 0, ..s }.offset_at(track, {
            let (p, l) = ScrollState { offset: 0, ..s }.thumb(track);
            p as i32 + l as i32 / 2
        }),
        0,
        "the top is exactly the top"
    );
    assert_eq!(
        ScrollState { offset: 1000, ..s }.offset_at(track, {
            let (p, l) = ScrollState { offset: 1000, ..s }.thumb(track);
            p as i32 + l as i32 / 2
        }),
        1000,
        "and the bottom exactly the bottom"
    );

    let s = ScrollState { offset: 0, visible: 10, total: 40 };
    for offset in 0..=30 {
        let s = ScrollState { offset, ..s };
        let (pos, len) = s.thumb(400);
        assert_eq!(s.offset_at(400, pos as i32 + len as i32 / 2), offset);
    }
}

#[test]
fn dragging_past_either_end_of_the_track_stays_at_that_end() {
    // The router hands a captured widget coordinates outside itself rather than clamping, so
    // something here has to.
    let s = ScrollState { offset: 300, visible: 24, total: 1024 };
    assert_eq!(s.offset_at(400, -900), 0, "dragged far above the bar");
    assert_eq!(s.offset_at(400, 9000), 1000, "and far below it");
    assert_eq!(s.offset_at(400, 400), 1000, "the very bottom is the end of the document");
}

#[test]
fn a_bar_with_nothing_to_scroll_reports_nothing_wherever_it_is_grabbed() {
    let s = ScrollState { offset: 0, visible: 24, total: 24 };
    assert!(!s.scrollable());
    for y in [-5, 0, 100, 4000] {
        assert_eq!(s.offset_at(400, y), 0, "at y={y}");
    }
    let s = ScrollState { offset: 5, visible: 24, total: 1024 };
    assert_eq!(s.thumb(MIN_THUMB).1, MIN_THUMB, "the premise: the thumb fills the track");
    assert_eq!(s.offset_at(MIN_THUMB, 8), 0, "a position it cannot express");
}

#[test]
fn a_scrollbar_lays_its_thumb_out_where_the_arithmetic_says() {
    let p = Theme::default();
    let s = ScrollState { offset: 50, visible: 25, total: 100 };
    let e: Element<Msg> = scrollbar(s, 12, 400, &p);
    let l = layout(&e, Rect::new(0, 0, 12, 400), &CELL);
    let (pos, len) = s.thumb(400);

    let col = &l.children[0].children[1];
    assert_eq!(col.children[0].rect.size.h, pos, "the spacer is the thumb's offset");
    assert_eq!(col.children[1].rect.size.h, len, "and then the thumb");
    assert_eq!(col.children[1].rect.origin.y, pos as i32);
    assert_eq!(l.rect.size.w, 12, "the bar is as wide as asked");
}

#[test]
fn a_button_carries_its_message_and_takes_focus() {
    let p = Theme::default();
    let e = button("OK", (), WidgetState::default(), &p);
    assert!(e.on_press.is_some(), "a button that sends nothing is not a button");
    assert!(e.focusable, "a keyboard user must be able to reach it");
}

#[test]
fn a_buttons_face_follows_the_state_it_is_given() {
    let p = Theme::default();
    let face_of = |st: WidgetState| {
        let e: Element<Msg> = button("OK", (), st, &p);
        let fills: vec::Vec<Rgb> = e
            .children()
            .filter_map(|c| match &c.node {
                crate::element::Node::Fill(c) | crate::element::Node::RoundedFill { colour: c, .. } => Some(*c),
                crate::element::Node::Padding { child, .. } => match &child.node {
                    crate::element::Node::Fill(c) | crate::element::Node::RoundedFill { colour: c, .. } => Some(*c),
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
    // Painted rather than inspected: the ring is the outermost layer, and a face drawn over the
    // whole area would hide it while the tree still looked right.
    let p = Theme::default();
    let t = Theme::default();
    let mut fb = MemFramebuffer::new(Geometry::packed(80, 40, PixelFormat::XRGB8888));
    let e: Element<Msg> = button("OK", (), WidgetState { active: true, ..Default::default() }, &p);
    let l = layout(&e, Rect::new(0, 0, 80, 40), &CELL);
    paint(&mut fb, &font(), &t, &e, &l, Rect::new(0, 0, 80, 40), &mut |_, _, _, _: &mut MemFramebuffer| {});
    // Down the left edge rather than at the corner, which the rounding cuts away.
    assert_eq!(fb.get_pixel(0, 20), Some(p.accent), "the ring is on the edge");
    // Inside the ring but away from the centred label.
    assert_eq!(fb.get_pixel(6, 20), Some(p.face), "and the face is inside it");
}

/// A resting button has an edge, and it is not the focus ring: the face is eighteen units per
/// channel from a white window, which nobody can see.
#[test]
fn a_resting_button_has_a_border_and_a_focused_one_keeps_the_ring() {
    let p = Theme::default();
    let t = Theme::default();
    let mut fb = MemFramebuffer::new(Geometry::packed(80, 40, PixelFormat::XRGB8888));
    let e: Element<Msg> = button("OK", (), WidgetState::default(), &p);
    let l = layout(&e, Rect::new(0, 0, 80, 40), &CELL);
    paint(&mut fb, &font(), &t, &e, &l, Rect::new(0, 0, 80, 40), &mut |_, _, _, _: &mut MemFramebuffer| {});
    assert_eq!(fb.get_pixel(0, 20), Some(p.border), "a resting button has no edge at all");
    assert_ne!(p.border, p.face, "…and the edge is not the face");
    assert_eq!(fb.get_pixel(6, 20), Some(p.face), "the face is inside the edge");
}

/// A resting field has an edge, and on a white card that edge is all that says there is a field.
/// Painted, because the ground is drawn over the edge if the layers are the wrong way round.
#[test]
fn a_resting_field_has_an_edge_against_the_ground_it_sits_on() {
    let p = Theme::default();
    let mut fb = MemFramebuffer::new(Geometry::packed(80, 40, PixelFormat::XRGB8888));
    fb.clear(p.background);
    let state = TextFieldState::new();
    let e: Element<Msg> = text_field(&state, false, WidgetState::default(), &p);
    let l = layout(&e, Rect::new(0, 0, 80, 40), &CELL);
    let all = Rect::new(0, 0, 80, 40);
    paint(&mut fb, &font(), &p, &e, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
    let mid = l.rect.size.h / 2;
    assert_eq!(fb.get_pixel(0, mid), Some(p.border), "a resting field has an edge");
    assert_ne!(p.border, p.track, "…and it is not the colour of the well inside it");
    assert_ne!(p.border, p.background, "…nor of the surface it sits on");
    assert_eq!(fb.get_pixel(4, mid), Some(p.track), "the well is inside the edge");
    let mut lit = MemFramebuffer::new(Geometry::packed(80, 40, PixelFormat::XRGB8888));
    lit.clear(p.background);
    let f: Element<Msg> = text_field(&state, false, WidgetState { active: true, ..Default::default() }, &p);
    let fl = layout(&f, Rect::new(0, 0, 80, 40), &CELL);
    paint(&mut lit, &font(), &p, &f, &fl, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
    assert_eq!(lit.get_pixel(0, mid), Some(p.accent), "a focused field rings in the accent");
}

/// The label is in the middle of the button, not against its corner: measured as a distance
/// from each edge, since `padding` alone left 58 pixels of face to the label's right.
#[test]
fn a_buttons_label_is_centred_in_its_face() {
    let p = Theme::default();
    let mut fb = MemFramebuffer::new(Geometry::packed(80, 40, PixelFormat::XRGB8888));
    let e: Element<Msg> = button("OK", (), WidgetState::default(), &p);
    let l = layout(&e, Rect::new(0, 0, 80, 40), &CELL);
    paint(&mut fb, &font(), &p, &e, &l, Rect::new(0, 0, 80, 40), &mut |_, _, _, _: &mut MemFramebuffer| {});
    let inked: Vec<u32> = (0..80)
        .filter(|x| {
            (0..40).any(|y| {
                let c = fb.get_pixel(*x, y);
                c != Some(p.face) && c != Some(p.border)
            })
        })
        .collect();
    let (first, last) = (inked[0], inked[inked.len() - 1]);
    let (left, right) = (first, 79 - last);
    assert!(left.abs_diff(right) <= 2, "the label sits {left} from the left and {right} from the right");
}

#[test]
fn a_menu_bar_is_as_tall_as_asked_and_no_taller() {
    let p = Theme::default();
    let e: Element<Msg> = menu_bar(vec![text("File"), text("Edit")], 24, &p);
    let l = layout(&e, Rect::new(0, 0, 200, 100), &CELL);
    assert_eq!(l.rect.size.h, 24);
    assert_eq!(l.rect.size.w, 200, "and spans the width it is given");
}

#[test]
fn a_menu_bars_items_each_get_their_own_width() {
    // Two widgets beside each other: alone, one that measures to "everything available" and one
    // that measures to its content are indistinguishable. `Node::Fill` once measured to `c.max`,
    // so the first `button` took the whole row and the second laid out at zero width.
    let p = Theme::default();
    let items: vec::Vec<Element<Msg>> =
        vec![button("File", (), WidgetState::default(), &p), button("Edit", (), WidgetState::default(), &p)];
    let e = menu_bar(items, 24, &p);
    let l = layout(&e, Rect::new(0, 0, 200, 100), &CELL);
    // sized -> dock -> [the rule, stack] ; stack -> [face fill, row] ; row -> the buttons.
    let row = &l.children[0].children[1].children[1];
    let (a, b) = (row.children[0].rect, row.children[1].rect);
    assert!(a.size.w > 0, "the first item measured to nothing");
    assert!(b.size.w > 0, "the second item got no width — the first one ate the row");
    assert_eq!(b.origin.x, a.origin.x + a.size.w as i32, "and they sit side by side");
    assert!(b.right() <= 200, "the second item runs off the bar: {:?}", b);
}

#[test]
fn a_button_measures_to_its_label_not_to_the_room_it_is_given() {
    let p = Theme::default();
    let e: Element<Msg> = button("OK", (), WidgetState::default(), &p);
    let big = crate::layout::measure(&e, crate::layout::Constraints::loose(libdraw::geom::Size::new(400, 400)), &CELL);
    let small =
        crate::layout::measure(&e, crate::layout::Constraints::loose(libdraw::geom::Size::new(100, 100)), &CELL);
    assert_eq!(big, small, "a button's size must not depend on the room around it");
    assert!(big.h < 100, "a two-line-tall button: {big:?}");
}

#[test]
fn a_title_bars_buttons_sit_at_its_right_edge_and_the_drag_face_takes_the_rest() {
    // The layout property that decides where a press lands: buttons at the right, the title
    // from the left, and everything between draggable.
    #[derive(Clone, PartialEq, Debug)]
    enum M {
        Drag,
        Min,
        Max,
        Close,
    }
    let p = Theme::default();
    let e = title_bar(
        "a terminal",
        None,
        true,
        M::Drag,
        TitleButtons { minimise: Some(M::Min), maximise: Some(M::Max), close: Some(M::Close) },
        None,
        &p,
    );
    let l = layout(&e, Rect::new(0, 0, 400, 100), &CELL);
    let mut tree = crate::diff::Tree::new();
    tree.update(&e, &l).expect("a clean frame");
    // Asked of the router, not read off the layout tree: where a press lands is the property.
    let click = |x: i32, y: i32| {
        let at = |pressed: bool| librsproto::surface::PointerEvent {
            kind: librsproto::surface::POINTER_BUTTON,
            button: 0x110,
            buttons: u16::from(pressed),
            flags: if pressed { librsproto::surface::POINTER_PRESSED } else { 0 },
            x,
            y,
            ..Default::default()
        };
        let mut r = crate::route::Router::new();
        let mut got = r.pointer(&tree, &e, &l, at(true)).0;
        got.extend(r.pointer(&tree, &e, &l, at(false)).0);
        got
    };
    assert_eq!(l.rect.size.h, TITLE_BAR_H, "the bar is its height");
    let right = (400 - TITLE_BUTTON_PAD) as i32;
    let step = (TITLE_BUTTON_W + TITLE_BUTTON_GAP) as i32;
    for (nth, want) in [(0, M::Close), (1, M::Max), (2, M::Min)] {
        let end = right - nth * step;
        let start = end - TITLE_BUTTON_W as i32;
        assert_eq!(click(start, 12), vec![want.clone()], "{want:?}'s first column");
        assert_eq!(click(end - 1, 12), vec![want.clone()], "{want:?}'s last column");
        assert_eq!(click(end, 12), vec![M::Drag], "just past {want:?} is the bar");
    }
    assert_eq!(click(1, 12), vec![M::Drag], "the title starts at the left and drags");
    assert_eq!(click(399, 12), vec![M::Drag], "the padding after close drags");
}

// ---- the text area ----

fn area() -> TextAreaState {
    TextAreaState::with_text("abc\nde\nfghi")
}

fn type_text(a: &mut TextAreaState, text: &str) {
    for c in text.chars() {
        if c == '\n' {
            a.newline();
        } else {
            a.insert(c);
        }
    }
}

/// An area types under the key's locks, as a field does.
#[test]
fn an_area_types_under_the_keys_locks() {
    use librsproto::surface::{LOCK_CAPS, LOCK_NUM};
    let mut a = TextAreaState::with_text("");
    assert!(a.apply(30, 0, LOCK_CAPS), "a, under Caps Lock");
    assert!(a.apply(79, 0, LOCK_NUM), "keypad 1, under Num Lock");
    assert_eq!(a.text(), "A1");
}

// ---- paste and its range ----

#[test]
fn insert_text_returns_the_range_it_occupies() {
    // The return value lets a cycle, which replaces what was just inserted, know where that is.
    let mut a = area();
    a.apply(KEY_RIGHT, 0, 0); // between `a` and `bc`
    let (from, to) = a.insert_text("XY");
    assert_eq!(a.text(), "aXYbc\nde\nfghi");
    assert_eq!(from, (0, 1));
    assert_eq!(to, (0, 3), "one line, so the range is columns 1..3");

    let mut a = area();
    let (from, to) = a.insert_text("one\ntwo");
    assert_eq!(from, (0, 0));
    assert_eq!(to, (1, 3));
    assert_eq!(a.text(), "one\ntwoabc\nde\nfghi", "and the tail followed it down");
}

#[test]
fn select_range_then_insert_text_is_the_cycle() {
    // The two halves together are what `nxedit::App::cycled` does; this pins the primitives.
    let mut a = area();
    let (from, to) = a.insert_text("FIRST");
    assert_eq!(a.text(), "FIRSTabc\nde\nfghi");
    a.select_range(from, to);
    assert_eq!(a.selected_text().as_deref(), Some("FIRST"), "the range names what went in");
    a.insert_text("SECOND");
    assert_eq!(a.text(), "SECONDabc\nde\nfghi", "which the next paste replaced");
}

#[test]
fn select_range_clamps_a_stale_range_rather_than_panicking() {
    // A range is only valid until the next edit; clamping makes a stale one select something
    // harmless instead of indexing out of the buffer.
    let mut a = area();
    a.select_range((99, 99), (99, 99));
    assert_eq!(a.selected_text(), None, "collapsed onto the end of the last line");
    assert_eq!(a.cursor(), (2, 4), "and the cursor is inside the buffer");
    assert_eq!(a.text(), "abc\nde\nfghi", "nothing was changed by asking");
}

#[test]
fn select_range_lands_on_a_character_boundary() {
    // A stale column can point into the middle of a multi-byte character; clamping walks back
    // to the boundary.
    let mut a = TextAreaState::with_text("aéb");
    a.select_range((0, 0), (0, 2));
    assert_eq!(a.selected_text().as_deref(), Some("a"));
}

// ---- undo and redo ----

#[test]
fn a_word_is_one_undo_step_and_so_is_a_line() {
    // A run of printable characters is one group, a separator ends it, and `Enter` ends it.
    let mut a = TextAreaState::new();
    type_text(&mut a, "hello world");
    assert!(a.undo(), "there is something to undo");
    assert_eq!(a.text(), "hello ", "the last word, and not the last letter");
    assert!(a.undo());
    assert_eq!(a.text(), "", "and the word before it, with its separator");
    assert!(!a.undo(), "and then there is nothing");
}

#[test]
fn a_line_ends_a_group() {
    // `Enter` joins the group it lands in and closes it: the line's text and its break come
    // back in one step.
    let mut a = TextAreaState::new();
    type_text(&mut a, "one\ntwo");
    assert!(a.undo());
    assert_eq!(a.text(), "one\n", "the second line's word");
    assert!(a.undo());
    assert_eq!(a.text(), "", "then the first line and its break, together");
    assert!(!a.undo());

    let mut a = TextAreaState::with_text("ab");
    a.end(false);
    a.newline();
    a.insert('c');
    assert!(a.undo());
    assert_eq!(a.text(), "ab\n");
    assert!(a.undo());
    assert_eq!(a.text(), "ab");
}

#[test]
fn moving_ends_a_group_so_two_edits_undo_separately() {
    // Wherever it lands: typing at one end of a line, moving to the other and typing again
    // must not undo both at once.
    let mut a = TextAreaState::with_text("ab");
    a.end(false);
    a.insert('X');
    a.home(false);
    a.insert('Y');
    assert_eq!(a.text(), "YabX");
    assert!(a.undo());
    assert_eq!(a.text(), "abX", "only the edit after the move");
    assert!(a.undo());
    assert_eq!(a.text(), "ab");
}

#[test]
fn deleting_is_its_own_kind_of_group() {
    let mut a = TextAreaState::with_text("abcdef");
    a.end(false);
    a.backspace();
    a.backspace();
    assert_eq!(a.text(), "abcd");
    a.insert('Z');
    assert_eq!(a.text(), "abcdZ");
    assert!(a.undo());
    assert_eq!(a.text(), "abcd", "the typing, on its own");
    assert!(a.undo());
    assert_eq!(a.text(), "abcdef", "then both deletions together");
}

#[test]
fn an_edit_that_does_nothing_is_not_a_step() {
    // A `Backspace` at the start and a `Delete` at the end change nothing, and a snapshot for
    // either would be an undo press that visibly does nothing.
    let mut a = TextAreaState::with_text("ab");
    assert!(!a.backspace(), "nothing before the cursor");
    assert!(!a.undo(), "and nothing to undo");
    a.end(false);
    a.down(false);
    a.end(false);
    assert!(!a.delete(), "nothing after it");
    assert!(!a.undo());
}

#[test]
fn placing_the_cursor_ends_a_group() {
    // A click is a movement and closes a group as an arrow key does. Forward-looking:
    // nothing outside this crate's tests wires a press to `place` yet.
    let mut a = TextAreaState::with_text("ab");
    a.end(false);
    a.insert('X');
    a.place(0, 0);
    a.insert('Y');
    assert_eq!(a.text(), "YabX");
    assert!(a.undo());
    assert_eq!(a.text(), "abX", "only the edit after the click");
    assert!(a.undo());
    assert_eq!(a.text(), "ab");
}

#[test]
fn a_boundary_the_buffer_cannot_see_can_still_be_drawn() {
    // A save is the boundary this exists for: without an explicit end the session is one group.
    let mut a = TextAreaState::new();
    type_text(&mut a, "first");
    a.end_group();
    type_text(&mut a, "second");
    assert!(a.undo());
    assert_eq!(a.text(), "first", "only what came after the boundary");
    assert!(a.undo());
    assert_eq!(a.text(), "");
}

#[test]
fn redo_comes_forward_and_a_new_edit_abandons_it() {
    let mut a = TextAreaState::new();
    type_text(&mut a, "one two");
    a.undo();
    assert_eq!(a.text(), "one ");
    assert!(a.redo(), "and forward again");
    assert_eq!(a.text(), "one two");

    a.undo();
    assert_eq!(a.text(), "one ");
    a.insert('X');
    assert!(!a.redo(), "the way forward is gone");
    assert_eq!(a.text(), "one X");
}

#[test]
fn the_history_is_bounded_and_drops_the_oldest() {
    let mut a = TextAreaState::new();
    for i in 0..MAX_UNDO + 10 {
        a.insert(char::from(b'a' + (i % 26) as u8));
        a.insert(' ');
    }
    let mut steps = 0;
    while a.undo() {
        steps += 1;
    }
    assert_eq!(steps, MAX_UNDO, "the oldest go, and the bound holds");
    assert_ne!(a.text(), "", "so the very beginning is not reachable, which the cap means");
}

#[test]
fn undoing_marks_the_buffer_changed() {
    // `revision` is what an editor derives "modified" from, and undo has to move it.
    let mut a = TextAreaState::new();
    a.insert('x');
    let after_typing = a.revision();
    a.undo();
    assert_ne!(a.revision(), after_typing);
}

// ---- find ----

#[test]
fn find_walks_forward_through_every_match_and_wraps() {
    let mut a = TextAreaState::with_text(
        "one two
three two
two",
    );
    assert!(a.find("two"));
    assert_eq!(a.cursor(), (0, 4), "the first, after the cursor");
    assert!(a.find("two"));
    assert_eq!(a.cursor(), (1, 6), "the next");
    assert!(a.find("two"));
    assert_eq!(a.cursor(), (2, 0));
    assert!(a.find("two"), "and round again");
    assert_eq!(a.cursor(), (0, 4));
}

#[test]
fn find_selects_what_it_found() {
    let mut a = TextAreaState::with_text("alpha beta");
    assert!(a.find("beta"));
    assert_eq!(a.selected_text().as_deref(), Some("beta"));
}

#[test]
fn find_answers_no_for_what_is_not_there_and_for_nothing() {
    let mut a = TextAreaState::with_text("alpha");
    assert!(!a.find("omega"), "absent");
    assert_eq!(a.cursor(), (0, 0), "and the cursor did not move");
    assert!(!a.find(""));
    assert_eq!(a.cursor(), (0, 0));
}

#[test]
fn find_does_not_split_a_multi_byte_character() {
    let mut a = TextAreaState::with_text("é and é");
    assert!(a.find("é"), "the second one");
    assert_eq!(a.cursor(), (0, 7));
    assert!(a.find("é"), "and back to the first");
    assert_eq!(a.cursor(), (0, 0));
}

/// How many `Fill`s of `colour` the tree holds: the caret is one, and a selection's highlight is
/// one per line it covers. `check-display` renders one arrangement and a picture cannot count.
fn fills<M>(e: &Element<M>, colour: Rgb) -> usize {
    fn walk<M>(e: &Element<M>, colour: Rgb, n: &mut usize) {
        if matches!(&e.node, crate::element::Node::Fill(rgb) if *rgb == colour) {
            *n += 1;
        }
        for c in e.children() {
            walk(c, colour, n);
        }
    }
    let mut n = 0;
    walk(e, colour, &mut n);
    n
}

const KEY_A: u16 = 30;
const KEY_X: u16 = 45;

#[test]
fn a_buffer_round_trips_its_text_and_keeps_a_trailing_empty_line() {
    // What was opened is what is saved: no trailing newline added or lost.
    for src in ["abc\nde", "abc\nde\n", "", "\n"] {
        assert_eq!(TextAreaState::with_text(src).text(), src, "round trip of {src:?}");
    }
    assert_eq!(TextAreaState::with_text("a\r\nb").text(), "a\nb");
}

#[test]
fn an_empty_buffer_is_one_empty_line_so_the_cursor_always_has_somewhere_to_be() {
    let a = TextAreaState::new();
    assert_eq!(a.lines().len(), 1);
    assert_eq!(a.cursor(), (0, 0));
}

#[test]
fn typing_inserts_and_enter_splits_the_line() {
    let mut a = TextAreaState::new();
    for k in [KEY_A, KEY_A] {
        a.apply(k, 0, 0);
    }
    a.apply(KEY_ENTER, 0, 0);
    a.apply(KEY_X, 0, 0);
    assert_eq!(a.text(), "aa\nx");
    assert_eq!(a.cursor(), (1, 1));
}

#[test]
fn backspace_at_the_start_of_a_line_joins_it_to_the_one_above() {
    // The cursor lands where the join happened, not at the start of the merged line.
    let mut a = area();
    a.apply(KEY_DOWN, 0, 0);
    assert_eq!(a.cursor(), (1, 0));
    assert!(a.apply(KEY_BACKSPACE, 0, 0));
    assert_eq!(a.text(), "abcde\nfghi");
    assert_eq!(a.cursor(), (0, 3), "at the join, not at the start of the line");
}

#[test]
fn delete_at_the_end_of_a_line_pulls_the_next_one_up() {
    let mut a = area();
    a.apply(KEY_END, 0, 0);
    assert!(a.apply(KEY_DELETE, 0, 0));
    assert_eq!(a.text(), "abcde\nfghi");
    assert_eq!(a.cursor(), (0, 3));
}

#[test]
fn backspace_at_the_very_start_and_delete_at_the_very_end_do_nothing() {
    let mut a = area();
    assert!(!a.apply(KEY_BACKSPACE, 0, 0));
    let mut a = area();
    for _ in 0..2 {
        a.apply(KEY_DOWN, 0, 0);
    }
    a.apply(KEY_END, 0, 0);
    assert!(!a.apply(KEY_DELETE, 0, 0));
    assert_eq!(a.text(), "abc\nde\nfghi", "and neither changed the buffer");
}

#[test]
fn vertical_movement_keeps_the_column_it_was_aiming_for() {
    // The goal column: Down from column 3 onto a two-character line clamps to 2, and coming
    // back up must return to 3.
    let mut a = area();
    a.apply(KEY_END, 0, 0);
    assert_eq!(a.cursor(), (0, 3));
    a.apply(KEY_DOWN, 0, 0);
    assert_eq!(a.cursor(), (1, 2), "clamped to the short line's end");
    a.apply(KEY_DOWN, 0, 0);
    assert_eq!(a.cursor(), (2, 3), "and back out to the goal on a line long enough");
    a.apply(KEY_UP, 0, 0);
    a.apply(KEY_UP, 0, 0);
    assert_eq!(a.cursor(), (0, 3), "all the way back to where it started");
}

#[test]
fn a_horizontal_move_gives_up_the_goal_column() {
    // Otherwise the goal outlives the intent that set it.
    let mut a = area();
    a.apply(KEY_END, 0, 0);
    a.apply(KEY_DOWN, 0, 0);
    a.apply(KEY_LEFT, 0, 0);
    assert_eq!(a.cursor(), (1, 1));
    a.apply(KEY_DOWN, 0, 0);
    assert_eq!(a.cursor(), (2, 1), "the new column, not the old goal");
}

#[test]
fn shift_extends_a_selection_and_an_unshifted_move_drops_it() {
    let mut a = area();
    for _ in 0..2 {
        a.apply(KEY_RIGHT, MOD_SHIFT, 0);
    }
    assert_eq!(a.selection(), Some(((0, 0), (0, 2))));
    assert_eq!(a.selected_text().as_deref(), Some("ab"));

    a.apply(KEY_RIGHT, 0, 0);
    assert_eq!(a.selection(), None, "an unshifted move drops it");
}

#[test]
fn a_selection_reads_the_same_whichever_way_it_was_made() {
    let mut a = area();
    a.apply(KEY_END, 0, 0);
    for _ in 0..2 {
        a.apply(KEY_LEFT, MOD_SHIFT, 0);
    }
    assert_eq!(a.selection(), Some(((0, 1), (0, 3))));
    assert_eq!(a.selected_text().as_deref(), Some("bc"));
}

#[test]
fn a_selection_spanning_lines_reads_the_newlines_back() {
    let mut a = area();
    a.apply(KEY_RIGHT, 0, 0);
    a.apply(KEY_DOWN, MOD_SHIFT, 0);
    a.apply(KEY_DOWN, MOD_SHIFT, 0);
    assert_eq!(a.selected_text().as_deref(), Some("bc\nde\nf"));
}

#[test]
fn typing_over_a_selection_replaces_it() {
    let mut a = area();
    a.apply(KEY_DOWN, MOD_SHIFT, 0);
    a.apply(KEY_END, MOD_SHIFT, 0);
    assert_eq!(a.selected_text().as_deref(), Some("abc\nde"));
    a.apply(KEY_X, 0, 0);
    assert_eq!(a.text(), "x\nfghi");
    assert_eq!(a.selection(), None);
    assert_eq!(a.cursor(), (0, 1));
}

#[test]
fn backspace_over_a_selection_deletes_the_selection_and_not_a_character() {
    let mut a = area();
    for _ in 0..2 {
        a.apply(KEY_RIGHT, MOD_SHIFT, 0);
    }
    assert!(a.apply(KEY_BACKSPACE, 0, 0));
    assert_eq!(a.text(), "c\nde\nfghi", "the two selected characters, not three");
}

#[test]
fn the_view_scrolls_to_follow_the_cursor_and_the_widget_does_it() {
    // `ensure_visible` is the widget's to call, not the application's.
    let mut a = TextAreaState::with_text("0\n1\n2\n3\n4\n5\n6\n7");
    let p = Theme::default();
    let _: Element<()> = text_area(&mut a, 3 * 16, 16, true, &[], None, &p);
    assert_eq!(a.offset(), 0);

    for _ in 0..5 {
        a.apply(KEY_DOWN, 0, 0);
    }
    let _: Element<()> = text_area(&mut a, 3 * 16, 16, true, &[], None, &p);
    assert_eq!(a.offset(), 3, "line 5 is visible in a three-line window");

    for _ in 0..5 {
        a.apply(KEY_UP, 0, 0);
    }
    let _: Element<()> = text_area(&mut a, 3 * 16, 16, true, &[], None, &p);
    assert_eq!(a.offset(), 0, "and it scrolls back the other way");
}

#[test]
fn the_revision_counts_edits_and_not_movement() {
    // What an editor's "modified" marker is made of: byte length misses replacing a
    // one-character selection with one character.
    let mut a = area();
    let start = a.revision();

    for k in [KEY_RIGHT, KEY_DOWN, KEY_END, KEY_HOME, KEY_UP, KEY_LEFT] {
        a.apply(k, 0, 0);
    }
    assert_eq!(a.revision(), start, "moving is not editing");

    // Typed with nothing selected: the insert's own bump must not be covered by a deletion.
    a.apply(KEY_X, 0, 0);
    assert!(a.revision() > start, "typing is");

    let mut sel = area();
    let quiet = sel.revision();
    for _ in 0..2 {
        sel.apply(KEY_RIGHT, MOD_SHIFT, 0);
    }
    assert_eq!(sel.revision(), quiet, "nor is selecting");

    let mut b = TextAreaState::with_text("abc");
    b.apply(KEY_RIGHT, MOD_SHIFT, 0);
    let before = b.revision();
    let len = b.text().len();
    b.apply(KEY_X, 0, 0);
    assert_eq!(b.text().len(), len, "the fixture must keep its length, or it proves nothing");
    assert!(b.revision() > before, "replacing a selection is an edit");

    let mut c = TextAreaState::with_text("abc");
    let quiet = c.revision();
    assert!(!c.apply(KEY_BACKSPACE, 0, 0), "backspace at the start of the buffer does nothing");
    assert_eq!(c.revision(), quiet, "so it is not an edit");
}

#[test]
fn every_edit_path_counts_its_own_edit() {
    // One case per bump, because the bumps are not one line: `delete_selection` has its own, and
    // without it a person could select a word, press Delete, and watch the buffer change while
    // the title bar kept saying the file was saved.
    let bumps = |setup: &dyn Fn(&mut TextAreaState)| -> u64 {
        let mut a = area();
        let before = a.revision();
        setup(&mut a);
        a.revision() - before
    };

    let select_two = |a: &mut TextAreaState| {
        for _ in 0..2 {
            a.apply(KEY_RIGHT, MOD_SHIFT, 0);
        }
    };
    assert!(
        bumps(&|a| {
            select_two(a);
            assert!(a.apply(KEY_BACKSPACE, 0, 0));
        }) > 0,
        "backspace over a selection"
    );
    assert!(
        bumps(&|a| {
            select_two(a);
            assert!(a.apply(KEY_DELETE, 0, 0));
        }) > 0,
        "delete over a selection"
    );

    assert!(bumps(&|a| a.newline()) > 0, "enter splits a line");

    assert!(
        bumps(&|a| {
            a.apply(KEY_RIGHT, 0, 0);
            assert!(a.apply(KEY_BACKSPACE, 0, 0));
        }) > 0,
        "backspace over a character"
    );
    assert!(
        bumps(&|a| {
            a.apply(KEY_DOWN, 0, 0);
            a.apply(KEY_HOME, 0, 0);
            assert!(a.apply(KEY_BACKSPACE, 0, 0));
        }) > 0,
        "backspace joining two lines"
    );

    assert!(bumps(&|a| assert!(a.apply(KEY_DELETE, 0, 0))) > 0, "delete over a character");
    assert!(
        bumps(&|a| {
            a.apply(KEY_END, 0, 0);
            assert!(a.apply(KEY_DELETE, 0, 0));
        }) > 0,
        "delete joining two lines"
    );

    let mut ends = TextAreaState::with_text("a");
    let quiet = ends.revision();
    assert!(!ends.apply(KEY_BACKSPACE, 0, 0), "backspace at the buffer's start");
    ends.apply(KEY_END, 0, 0);
    assert!(!ends.apply(KEY_DELETE, 0, 0), "delete at its end");
    assert_eq!(ends.revision(), quiet);
}

#[test]
fn the_caret_is_drawn_at_either_end_of_a_selection() {
    // The half a picture cannot check: `check-display` builds its selection with `Shift+Right`,
    // so the gate compares a forward one. Both directions here, counted in the tree.
    let p = Theme::default();
    let draw = |a: &mut TextAreaState| -> usize {
        let e: Element<()> = text_area(a, 3 * 16, 16, true, &[], None, &p);
        fills(&e, p.accent)
    };

    let mut a = area();
    a.apply(KEY_END, 0, 0);
    assert_eq!(draw(&mut a), 1, "no selection at all");

    let mut a = area();
    for _ in 0..2 {
        a.apply(KEY_RIGHT, MOD_SHIFT, 0);
    }
    assert_eq!(draw(&mut a), 1, "forward: the cursor is at the highlight's end");

    let mut a = area();
    a.apply(KEY_END, 0, 0);
    for _ in 0..2 {
        a.apply(KEY_LEFT, MOD_SHIFT, 0);
    }
    assert_eq!(draw(&mut a), 1, "backward: the cursor is at the highlight's start");

    let mut a = area();
    a.apply(KEY_DOWN, 0, 0);
    a.apply(KEY_UP, MOD_SHIFT, 0);
    assert_eq!(draw(&mut a), 1, "backward across a line break");

    let mut a = area();
    a.apply(KEY_END, 0, 0);
    let e: Element<()> = text_area(&mut a, 3 * 16, 16, false, &[], None, &p);
    assert_eq!(fills(&e, p.accent), 0, "and none at all when the widget is not active");
}

/// Every `(text, ink)` pair in a tree, in order.
fn inked<M>(e: &Element<M>) -> Vec<(String, Option<Rgb>)> {
    fn walk<M>(e: &Element<M>, under: Option<Rgb>, out: &mut Vec<(String, Option<Rgb>)>) {
        match &e.node {
            crate::element::Node::Text(t) if !t.is_empty() => {
                out.push((t.clone(), under));
            }
            crate::element::Node::Ink { colour, child } => walk(child, Some(*colour), out),
            _ => {
                for c in e.children() {
                    walk(c, under, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(e, None, &mut out);
    out
}

const KEYWORD: Rgb = Rgb::new(0x11, 0x22, 0x33);

#[test]
fn an_ink_run_splits_a_line_and_colours_only_itself() {
    let p = Theme::default();
    let mut a = TextAreaState::with_text("let x = 1");
    let e: Element<()> =
        text_area(&mut a, 16, 16, false, &[InkRun { line: 0, start: 0, end: 3, colour: KEYWORD }], None, &p);
    assert_eq!(
        inked(&e),
        alloc::vec![(String::from("let"), Some(KEYWORD)), (String::from(" x = 1"), None),],
        "the line was not split at the run's edge"
    );
}

/// A pixel becomes a line and a column, and the column is the nearest boundary: clicking the
/// right half of a letter puts the caret after it.
#[test]
fn a_point_becomes_the_nearest_line_and_column() {
    let w = |s: &str| (s.chars().count() * 8) as u32;
    let a = TextAreaState::with_text("abcdef\nghi");
    let px = |col: usize| FIELD_PAD.left as i32 + col as i32 * 8;
    let py = |row: usize| FIELD_PAD.top as i32 + row as i32 * 16 + 4;

    assert_eq!(a.at_point(px(0), py(0), 16, w), (0, 0));
    assert_eq!(a.at_point(px(3), py(0), 16, w), (0, 3), "a boundary is itself");
    assert_eq!(a.at_point(px(3) + 6, py(0), 16, w), (0, 4), "past the middle is the next");
    assert_eq!(a.at_point(px(3) + 2, py(0), 16, w), (0, 3), "before it is this one");
    assert_eq!(a.at_point(px(0), py(1), 16, w), (1, 0), "the second row is the second line");

    // Off the ends is clamped in both directions: a drag runs off a widget routinely.
    assert_eq!(a.at_point(-40, py(0), 16, w), (0, 0));
    assert_eq!(a.at_point(px(99), py(0), 16, w), (0, 6), "past the end of the line is its end");
    assert_eq!(a.at_point(px(0), py(9), 16, w), (1, 0), "past the last line is the last line");
    assert_eq!(a.at_point(px(0), -80, 16, w), (0, 0));
}

/// A scrolled area maps a point to the line that is *on screen* there: leaving the offset out
/// makes clicking work until the first scroll.
#[test]
fn a_point_is_read_against_what_is_on_screen() {
    let w = |s: &str| (s.chars().count() * 8) as u32;
    let mut a = TextAreaState::with_text("0\n1\n2\n3\n4\n5\n6\n7\n8\n9");
    a.scroll_to(4, 3);
    assert_eq!(a.offset(), 4, "precondition: scrolled");
    let y = FIELD_PAD.top as i32 + 4;
    assert_eq!(a.at_point(FIELD_PAD.left as i32, y, 16, w).0, 4, "the top row is line 4");
}

#[test]
fn a_text_area_reports_a_bar_and_scrolls_without_moving_the_cursor() {
    let mut a = TextAreaState::with_text("0\n1\n2\n3\n4\n5\n6\n7\n8\n9");
    let bar = a.bar(4);
    assert_eq!((bar.offset, bar.visible, bar.total), (0, 4, 10));
    assert!(bar.scrollable());

    let before = a.cursor();
    a.scroll_to(3, 4);
    assert_eq!(a.offset(), 3);
    assert_eq!(a.cursor(), before, "a scrollbar drag moved the caret");
    a.scroll_to(99, 4);
    assert_eq!(a.offset(), 6, "ten lines, four visible");
}

/// The caret is drawn where the cursor is in the middle of a line.
///
/// Every other test puts it at column 0, a line's end, or an end of a selection, each already a
/// cut for another reason, so deleting the caret's own cut left the suite green.
#[test]
fn the_caret_is_drawn_in_the_middle_of_a_line() {
    let p = Theme::default();
    let mut a = TextAreaState::with_text("abcdef");
    for _ in 0..2 {
        a.apply(KEY_RIGHT, 0, 0);
    }
    assert_eq!(a.cursor(), (0, 2), "precondition: mid-line, with no selection");
    let e: Element<()> = text_area(&mut a, 16, 16, true, &[], None, &p);
    assert_eq!(fills(&e, p.accent), 1, "no caret while typing in the middle of a line");
}

/// A coloured run under a selection keeps its colour: ink is a wrapper, so a selected keyword is
/// a fill and an inked text, and dropping the ink would turn a selected line black.
#[test]
fn a_selected_run_keeps_its_ink() {
    let p = Theme::default();
    let mut a = TextAreaState::with_text("let x = 1");
    for _ in 0..3 {
        a.apply(KEY_RIGHT, MOD_SHIFT, 0);
    }
    let e: Element<()> =
        text_area(&mut a, 16, 16, true, &[InkRun { line: 0, start: 0, end: 3, colour: KEYWORD }], None, &p);
    assert_eq!(fills(&e, p.selection()), 1, "precondition: the keyword is selected");
    assert!(
        inked(&e).contains(&(String::from("let"), Some(KEYWORD))),
        "the selection took the colour with it: {:?}",
        inked(&e)
    );
}

/// A stale run past the end of a line is clamped, not indexed with.
///
/// Reachable on every keystroke: runs are scanned from the buffer as it was, and a bound inside
/// a multi-byte character would panic on the slice.
#[test]
fn a_stale_or_misaligned_run_neither_panics_nor_colours_past_the_end() {
    let p = Theme::default();
    let mut a = TextAreaState::with_text("ab");
    let e: Element<()> =
        text_area(&mut a, 16, 16, false, &[InkRun { line: 0, start: 1, end: 99, colour: KEYWORD }], None, &p);
    assert_eq!(
        inked(&e),
        alloc::vec![(String::from("a"), None), (String::from("b"), Some(KEYWORD))],
        "a run past the end was not clamped to it"
    );

    let mut a = TextAreaState::with_text("\u{4e2d}\u{6587}");
    let e: Element<()> =
        text_area(&mut a, 16, 16, false, &[InkRun { line: 0, start: 1, end: 2, colour: KEYWORD }], None, &p);
    assert_eq!(inked(&e).len(), 1, "the line was split inside a character: {:?}", inked(&e));

    let mut a = TextAreaState::with_text("only");
    let e: Element<()> =
        text_area(&mut a, 16, 16, false, &[InkRun { line: 40, start: 0, end: 2, colour: KEYWORD }], None, &p);
    assert!(inked(&e).iter().all(|(_, c)| c.is_none()));
}

#[test]
fn a_selection_is_highlighted_on_every_line_it_covers() {
    let p = Theme::default();
    let mut a = area();
    a.apply(KEY_RIGHT, 0, 0);
    a.apply(KEY_DOWN, MOD_SHIFT, 0);
    let e: Element<()> = text_area(&mut a, 3 * 16, 16, true, &[], None, &p);
    assert_eq!(fills(&e, p.selection()), 2, "the tail of line 0 and the head of line 1");

    let mut a = area();
    let e: Element<()> = text_area(&mut a, 3 * 16, 16, true, &[], None, &p);
    assert_eq!(fills(&e, p.selection()), 0, "and nothing when nothing is selected");
}

#[test]
fn a_collapsed_selection_leaves_no_anchor_behind_an_edit() {
    // Two keystrokes arm it and one fires: walking the cursor back onto its anchor leaves no
    // selection but does leave an anchor, and the next edit shortens the text it names.
    let mut a = area();
    a.apply(KEY_END, 0, 0);
    a.apply(KEY_LEFT, MOD_SHIFT, 0);
    a.apply(KEY_RIGHT, MOD_SHIFT, 0);
    assert_eq!(a.selection(), None, "the cursor is back on its anchor");
    assert!(a.apply(KEY_BACKSPACE, 0, 0));
    assert_eq!(a.text(), "ab\nde\nfghi");
    assert_eq!(a.selection(), None, "and the anchor went with the character");
    let e: Element<()> = text_area(&mut a, 3 * 16, 16, true, &[], None, &Theme::default());
    assert_eq!(
        fills(&e, Theme::default().selection()),
        0,
        "nothing is selected, so nothing \
            is highlighted"
    );

    let mut a = area();
    a.apply(KEY_RIGHT, MOD_SHIFT, 0);
    a.apply(KEY_LEFT, MOD_SHIFT, 0);
    a.insert('x');
    assert_eq!(a.text(), "xabc\nde\nfghi");
    assert_eq!(a.selection(), None, "typing selects nothing");

    let mut a = area();
    a.place(0, 3);
    a.extend_to(0, 3);
    assert!(a.apply(KEY_BACKSPACE, 0, 0));
    assert_eq!(a.text(), "ab\nde\nfghi");
    let _: Element<()> = text_area(&mut a, 3 * 16, 16, true, &[], None, &Theme::default());
}

#[test]
fn a_press_places_the_cursor_and_a_drag_selects_from_where_it_landed() {
    let mut a = area();
    a.place(2, 2);
    assert_eq!(a.cursor(), (2, 2));
    assert_eq!(a.selection(), None, "a press starts no selection");

    a.extend_to(0, 1);
    assert_eq!(a.selection(), Some(((0, 1), (2, 2))), "the drag selects from the press");
    assert_eq!(a.selected_text().as_deref(), Some("bc\nde\nfg"));
}

#[test]
fn a_press_past_the_end_of_a_line_lands_on_its_last_character_boundary() {
    // Clamping is the widget's job: the coordinates come from pixel arithmetic.
    let mut a = area();
    a.place(1, 99);
    assert_eq!(a.cursor(), (1, 2));
    a.place(99, 0);
    assert_eq!(a.cursor(), (2, 0), "and past the last line lands on the last line");
}

#[test]
fn enter_is_the_text_areas_and_tab_is_not() {
    // Enter is text in a text area, and Tab stays traversal's: a buffer that swallowed it
    // would trap the keyboard inside itself.
    const KEY_TAB: u16 = 15;
    let mut a = TextAreaState::new();
    assert!(a.apply(KEY_ENTER, 0, 0));
    assert_eq!(a.lines().len(), 2);
    assert!(!a.apply(KEY_TAB, 0, 0), "Tab is not claimed");
    assert_eq!(a.text(), "\n", "and it inserted nothing");
}

#[test]
fn a_control_chord_is_not_text() {
    const KEY_C: u16 = 46;
    let mut a = TextAreaState::new();
    assert!(!a.apply(KEY_C, librsproto::surface::MOD_CTRL, 0));
    assert_eq!(a.text(), "");
}

#[test]
fn a_grip_is_a_square_that_reports_its_press_rather_than_its_click() {
    // At the press, like the title bar's drag: a grip that waited for the click would hand the
    // compositor a drag whose button was already up.
    use crate::diff::Tree;
    use crate::layout::{Constraints, measure};
    use crate::route::Router;
    use librsproto::surface::{POINTER_BUTTON, POINTER_PRESSED, PointerEvent};

    #[derive(Clone, PartialEq, Debug)]
    struct Resize;
    let p = Theme::default();
    let e = resize_grip(Resize, &p);
    assert_eq!(measure(&e, Constraints::loose(Size::new(400, 400)), &CELL), Size::new(GRIP_W, GRIP_W));

    let l = layout(&e, Rect::new(0, 0, 400, 400), &CELL);
    let mut tree = Tree::new();
    tree.update(&e, &l).expect("a clean frame");
    let mut r = Router::new();
    let at = |pressed: bool| PointerEvent {
        kind: POINTER_BUTTON,
        button: 0x110,
        buttons: u16::from(pressed),
        flags: if pressed { POINTER_PRESSED } else { 0 },
        x: GRIP_W as i32 / 2,
        y: GRIP_W as i32 / 2,
        ..Default::default()
    };
    assert_eq!(r.pointer(&tree, &e, &l, at(true)).0, alloc::vec![Resize], "at the press");
    assert!(r.pointer(&tree, &e, &l, at(false)).0.is_empty(), "and not again at the click");
}

#[test]
fn a_press_on_the_bars_face_is_a_drag_and_a_press_on_a_button_is_not() {
    // The buttons sit above the face, so a press on one must produce its own message and not
    // also a drag.
    use crate::diff::Tree;
    use crate::route::Router;
    use librsproto::surface::{POINTER_BUTTON, POINTER_PRESSED, PointerEvent};

    #[derive(Clone, PartialEq, Debug)]
    enum M {
        Drag,
        Min,
        Max,
        Close,
    }
    let p = Theme::default();
    let e = title_bar(
        "a terminal",
        None,
        true,
        M::Drag,
        TitleButtons { minimise: Some(M::Min), maximise: Some(M::Max), close: Some(M::Close) },
        None,
        &p,
    );
    let l = layout(&e, Rect::new(0, 0, 400, 100), &CELL);
    let mut tree = Tree::new();
    tree.update(&e, &l).expect("a clean frame");

    let at = |x: i32, pressed: bool| PointerEvent {
        kind: POINTER_BUTTON,
        button: 0x110,
        buttons: u16::from(pressed),
        flags: if pressed { POINTER_PRESSED } else { 0 },
        x,
        y: 8,
        ..Default::default()
    };
    let down = |r: &mut Router, x: i32| r.pointer(&tree, &e, &l, at(x, true)).0;
    let up = |r: &mut Router, x: i32| r.pointer(&tree, &e, &l, at(x, false)).0;

    // The buttons' centres in a bare 400-wide bar, from the constants.
    let step = (TITLE_BUTTON_W + TITLE_BUTTON_GAP) as i32;
    let close_x = (400 - TITLE_BUTTON_PAD - TITLE_BUTTON_W / 2 - 1) as i32;
    let max_x = close_x - step;

    let mut r = Router::new();
    assert_eq!(down(&mut r, 200), vec![M::Drag], "the bar moves the window on the press…");
    assert_eq!(up(&mut r, 200), vec![], "…and the release adds nothing");

    let mut r = Router::new();
    assert_eq!(
        down(&mut r, close_x),
        vec![],
        "a press on close must not also drag: the window would move under the pointer while \
             the user is aiming at a button"
    );
    assert_eq!(up(&mut r, close_x), vec![M::Close], "and the click is the close");

    let mut r = Router::new();
    assert_eq!(down(&mut r, max_x), vec![], "the same for maximise");
    assert_eq!(up(&mut r, max_x), vec![M::Max]);

    // A second button pressed mid-drag is not a second drag: while a capture is held the router
    // routes to the captured widget, and the shadowing rule walks the captured node's path.
    let mut r = Router::new();
    assert_eq!(down(&mut r, 200), vec![M::Drag], "the left press starts the drag");
    let other = librsproto::surface::PointerEvent {
        kind: librsproto::surface::POINTER_BUTTON,
        button: 0x111,
        buttons: 3,
        flags: librsproto::surface::POINTER_PRESSED,
        x: 200,
        y: 8,
        ..Default::default()
    };
    assert_eq!(
        r.pointer(&tree, &e, &l, other).0,
        vec![],
        "a second button while the first is held must not start a second drag"
    );
    let over_close = librsproto::surface::PointerEvent { x: 390, ..other };
    assert_eq!(
        r.pointer(&tree, &e, &l, over_close).0,
        vec![],
        "nor one pressed over a button, where the capture is still the bar"
    );
}

const DEJAVU: &[u8] = include_bytes!("../../../../assets/fonts/DejaVuSans.ttf");

fn font() -> libdraw::text::Font {
    libdraw::text::Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses")
}

/// Every `Text` node in a tree, joined.
fn all_text<M>(e: &Element<M>) -> String {
    fn walk<M>(e: &Element<M>, out: &mut String) {
        if let crate::element::Node::Text(t) = &e.node {
            out.push_str(t);
        }
        for c in e.children() {
            walk(c, out);
        }
    }
    let mut out = String::new();
    walk(e, &mut out);
    out
}

#[test]
fn a_tab_selects_where_it_is_and_its_close_box_does_not_select_it() {
    // The published metrics: `check-login` aims from its own copies, so the numbers are
    // asserted against a tree actually built, and `xtask`'s
    // `the_gates_chrome_table_is_the_toolkits` compares the two sets.
    assert_eq!((TAB_W, TAB_STRIP_H, TAB_H, TAB_CLOSE_W), (120, 30, 24, 20));
    assert_eq!((TAB_SIDE, TAB_TOP, TAB_GAP, TAB_PITCH), (6, 5, 1, 121));
    assert_eq!(TAB_CLOSE_CX, 110);

    #[derive(Clone, PartialEq, Eq, Debug)]
    enum M {
        Select(u64),
        Close(u64),
        New,
    }
    let theme = Theme::default();
    let tabs = [Tab { key: 7, label: "one", marked: false }, Tab { key: 9, label: "two", marked: true }];
    let ui: Element<M> = tab_strip(&tabs, 7, None, M::Select, M::Close, TabExtras::new_tab(M::New), &theme);

    let cell = crate::layout::FixedCell { w: 8, h: 16 };
    let l = crate::layout::layout(&ui, Rect::new(0, 0, 400, TAB_STRIP_H), &cell);
    let mut tree = crate::diff::Tree::new();
    tree.update(&ui, &l).expect("a tab strip is diffable");
    let mut router = crate::route::Router::new();
    let click = |r: &mut crate::route::Router, x: i32| {
        let at = |flags: u16, buttons: u16| PointerEvent {
            kind: librsproto::surface::POINTER_BUTTON,
            button: 0x110,
            buttons,
            flags,
            x,
            y: (TAB_STRIP_H / 2) as i32,
            ..Default::default()
        };
        r.pointer(&tree, &ui, &l, at(librsproto::surface::POINTER_PRESSED, 1));
        r.pointer(&tree, &ui, &l, at(0, 0)).0
    };

    // Tab `i` begins at `TAB_SIDE + i * TAB_PITCH`.
    let tab_x = |i: i32| TAB_SIDE as i32 + i * TAB_PITCH as i32;
    assert_eq!(click(&mut router, tab_x(1) + 20), alloc::vec![M::Select(9)]);
    // Its close box closes it and does not also select it: a nearer `on_press` shadows.
    assert_eq!(click(&mut router, tab_x(1) + TAB_CLOSE_CX), alloc::vec![M::Close(9)]);
    assert_eq!(click(&mut router, tab_x(0) + 20), alloc::vec![M::Select(7)]);
    assert_eq!(click(&mut router, tab_x(2) + (NEW_TAB_W / 2) as i32), alloc::vec![M::New]);
}

/// How far off an exact ink a glyph's darkest pixel may land: stems antialias a few units light.
const ANTIALIAS_SLACK: u32 = 12;

/// A focused pill shows a ring, and its label reads in either scheme.
///
/// Painted and compared between states rather than against a colour, since "a ring is visible"
/// is what a same-colour ring passes.
#[test]
fn a_pill_rings_when_focused_and_its_label_reads_in_either_scheme() {
    let (w, h) = (80, 30);
    let draw = |t: &Theme, state: WidgetState| {
        let e: Element<Msg> = pill("Save", (), state, t);
        let all = Rect::new(0, 0, w, h);
        let l = layout(&e, all, &CELL);
        let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
        fb.clear(t.face);
        paint(&mut fb, &font(), t, &e, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
        fb
    };
    let t = Theme::default();
    let resting = draw(&t, WidgetState::default());
    let active = draw(&t, WidgetState { active: true, ..Default::default() });
    let differing = (0..w)
        .flat_map(|x| (0..h).map(move |y| (x, y)))
        .filter(|(x, y)| resting.get_pixel(*x, *y) != active.get_pixel(*x, *y))
        .count();
    assert!(differing > 40, "a focused pill looks no different: {differing} pixels");
    // The label is the better-contrasting of the surface's two inks, asserted as the choice
    // rather than a distance: the dark scheme's near-black paper is far from the accent by any
    // arithmetic and still unreadable on it.
    let brightness = |c: Rgb| 299 * c.r as i32 + 587 * c.g as i32 + 114 * c.b as i32;
    for t in [Theme::light(), Theme::dark()] {
        let fb = draw(&t, WidgetState::default());
        let ground = brightness(t.accent);
        let (paper, text_ink) = (brightness(t.background), brightness(t.foreground));
        let wanted = if (paper - ground).abs() >= (text_ink - ground).abs() { paper } else { text_ink };
        let row: alloc::vec::Vec<i32> = (0..w).filter_map(|x| fb.get_pixel(x, h / 2)).map(brightness).collect();
        let reached =
            if wanted > ground { *row.iter().max().expect("pixels") } else { *row.iter().min().expect("pixels") };
        let got = (reached - wanted).abs();
        let other = (reached - if wanted == paper { text_ink } else { paper }).abs();
        assert!(
            got < other,
            "the label is not the better-contrasting ink: reached {reached}, wanted \
                 {wanted}, the other is {}",
            if wanted == paper { text_ink } else { paper }
        );
    }
}

/// What a tab strip *looks* like: the current tab is the window's ground and covers the rule,
/// the others have no face and their labels are dim, and a keyed right slot is drawable.
#[test]
fn a_tab_strip_draws_the_current_tab_as_the_windows_own_ground() {
    #[derive(Clone, PartialEq, Eq, Debug)]
    enum M {
        Select(u64),
        Close(u64),
    }
    let t = Theme::default();
    let (w, h) = (300, TAB_STRIP_H);
    let tabs = [Tab { key: 1, label: "one", marked: false }, Tab { key: 2, label: "two", marked: false }];
    let paint_strip = |extras: TabExtras<M>, hovered: Option<u64>| {
        let ui: Element<M> = tab_strip(&tabs, 1, hovered, M::Select, M::Close, extras, &t);
        let all = Rect::new(0, 0, w, h);
        let l = layout(&ui, all, &CELL);
        let mut tree = crate::diff::Tree::new();
        let diffable = tree.update(&ui, &l).is_ok();
        let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
        fb.clear(t.background);
        paint(&mut fb, &font(), &t, &ui, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
        (fb, diffable)
    };
    let (fb, _) = paint_strip(TabExtras::none(), None);
    let (first, second) = (TAB_SIDE + 4, TAB_SIDE + TAB_PITCH + 4);
    assert_eq!(
        fb.get_pixel(first, TAB_TOP + TAB_H / 2),
        Some(t.background),
        "the current tab is the window's own ground"
    );
    assert_eq!(
        fb.get_pixel(first, TAB_STRIP_H - 1),
        Some(t.background),
        "…and it covers the rule, so it runs into the content below"
    );
    assert_eq!(
        fb.get_pixel(second, TAB_TOP + TAB_H / 2),
        Some(t.face_hover),
        "another tab has no face of its own: the strip's shows through"
    );
    assert_eq!(fb.get_pixel(second, TAB_STRIP_H - 1), Some(t.border), "…and the rule runs under it");
    let darkness = |c: Rgb| c.r as u32 + c.g as u32 + c.b as u32;
    let darkest = |x0: u32, x1: u32| {
        (x0..x1)
            .flat_map(|x| (0..h).map(move |y| (x, y)))
            .filter_map(|(x, y)| fb.get_pixel(x, y))
            .map(darkness)
            .min()
            .expect("pixels")
    };
    assert!(
        darkest(TAB_SIDE, TAB_SIDE + TAB_W - TAB_CLOSE_W) <= darkness(t.foreground) + 12,
        "the current tab's label is body ink"
    );
    assert!(
        darkest(second, second + TAB_W - TAB_CLOSE_W - 4) >= darkness(t.foreground_dim),
        "another tab's label is dim"
    );
    let (hover, _) = paint_strip(TabExtras::none(), Some(2));
    assert_eq!(hover.get_pixel(second, TAB_STRIP_H - 1), Some(t.border), "a hovered tab still has the rule under it");
    assert_ne!(hover.get_pixel(second, TAB_TOP + TAB_H / 2), Some(t.face_hover), "…and it does light up");
    let keyed = TabExtras { new_tab: None, right: Some(ink(t.foreground, text("848 bytes")).key(99)) };
    let (_, diffable) = paint_strip(keyed, None);
    assert!(diffable, "a caller that keys its right-hand control makes the window undiffable");
}

/// A dialog's edge follows *its* window's focus, not the fact that it is up: two windows edged
/// in the accent is the failure.
#[test]
fn a_dialogs_edge_follows_its_own_focus() {
    let t = Theme::default();
    let edge = |focused: bool| {
        let ui: Element<Msg> = dialog_frame(
            sized(Size::new(0, TITLE_BAR_H), fill(t.face)).key(1),
            padding(Insets::all(DIALOG_PAD), text("Really?")).key(2),
            sized(Size::new(0, 20), text("")).key(5),
            focused,
            &t,
        );
        let all = Rect::new(0, 0, DIALOG_W, DIALOG_H);
        let l = layout(&ui, all, &CELL);
        let mut fb = MemFramebuffer::new(Geometry::packed(DIALOG_W, DIALOG_H, PixelFormat::XRGB8888));
        fb.clear(t.background);
        paint(&mut fb, &font(), &t, &ui, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
        fb.get_pixel(0, DIALOG_H / 2)
    };
    assert_eq!(edge(true), Some(t.accent), "a dialog holding the keyboard is edged in it");
    assert_eq!(edge(false), Some(t.border), "and one that has lost it is not");
}

/// A pill is the accent filled in, with its label in the paper colour.
///
/// Painted, since "filled with the accent" is a statement about pixels.
#[test]
fn a_pill_is_the_accent_with_the_paper_as_its_ink() {
    let t = Theme::default();
    let (w, h) = (80, 30);
    let e: Element<Msg> = pill("Save", (), WidgetState::default(), &t);
    let all = Rect::new(0, 0, w, h);
    let l = layout(&e, all, &CELL);
    let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
    fb.clear(t.face);
    paint(&mut fb, &font(), &t, &e, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
    assert_eq!(fb.get_pixel(2, h / 2), Some(t.accent), "a pill is filled with the accent");
    // Measured as lightness along the middle row: a glyph at this size is mostly antialiased,
    // so counting pixels at exactly `background` counts almost none.
    let light = |c: Rgb| c.r as u32 + c.g as u32 + c.b as u32;
    let row: alloc::vec::Vec<u32> = (0..w).filter_map(|x| fb.get_pixel(x, h / 2)).map(light).collect();
    let (lightest, darkest) = (*row.iter().max().expect("pixels"), *row.iter().min().expect("pixels"));
    let (ground, paper) = (light(t.accent), light(t.background));
    assert!(
        lightest > ground + (paper - ground) / 2,
        "the label is drawn toward the paper colour: {lightest} against a ground of {ground}"
    );
    assert!(
        darkest >= ground - ANTIALIAS_SLACK,
        "something on the pill is darker than its ground ({darkest} against {ground}) — a \
             label in body ink on the accent reads as a smudge"
    );
    assert!(light(t.foreground) < ground - ANTIALIAS_SLACK, "and body ink would fail that");
}

/// A focused window's edge is the accent; an unfocused one's is the line colour.
///
/// Painted: the edge is the last layer, so a frame that drew it first has the right colour in
/// the wrong place and the same tree.
#[test]
fn a_focused_windows_edge_is_the_accent() {
    let t = Theme::default();
    let (w, h) = (120, 60);
    let edge = |focused: bool| {
        let e: Element<Msg> = window_frame(
            sized(Size::new(0, TITLE_BAR_H), fill(t.face)).key(1),
            sized(Size::new(0, 0), text("")).key(2),
            focused,
            &t,
        );
        let all = Rect::new(0, 0, w, h);
        let l = layout(&e, all, &CELL);
        let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
        fb.clear(t.background);
        paint(&mut fb, &font(), &t, &e, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
        fb.get_pixel(0, h / 2)
    };
    assert_eq!(edge(true), Some(t.accent), "the focused window is edged in the accent");
    assert_eq!(edge(false), Some(t.border), "and an unfocused one in the line colour");
    assert_ne!(t.accent, t.border);
}

/// A title bar's subtitle is beside the title, dimmer than it, and absent when there is none.
///
/// Painted: the darkest ink in the subtitle's half must be lighter than the title's.
#[test]
fn a_titles_subtitle_is_beside_it_and_dimmer_than_it() {
    let t = Theme::default();
    let (w, h) = (300, TITLE_BAR_H);
    let draw = |subtitle: Option<&str>| {
        let e: Element<Msg> = title_bar("Files", subtitle, true, (), TitleButtons::default(), None, &t);
        let all = Rect::new(0, 0, w, h);
        let l = layout(&e, all, &CELL);
        let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
        fb.clear(t.background);
        paint(&mut fb, &font(), &t, &e, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
        fb
    };
    let darkness = |c: Rgb| c.r as u32 + c.g as u32 + c.b as u32;
    let darkest = |fb: &MemFramebuffer, x0: u32, x1: u32| {
        (x0..x1)
            .flat_map(|x| (0..h).map(move |y| (x, y)))
            .filter_map(|(x, y)| fb.get_pixel(x, y))
            .map(darkness)
            .min()
            .expect("pixels")
    };
    let (title_half, sub_half) = (60, 200);
    let with = draw(Some("/home"));
    let without = draw(None);
    assert!(
        darkest(&with, title_half, sub_half) < darkest(&without, title_half, sub_half),
        "the subtitle draws where there was nothing"
    );
    // Against the dim ink itself, not merely the title: antialiasing once left a subtitle in
    // body ink three units lighter than the title.
    let (title_ink, sub_ink) = (darkest(&with, 0, title_half), darkest(&with, title_half, sub_half));
    assert!(
        sub_ink >= darkness(t.foreground_dim),
        "the subtitle is {sub_ink}, darker than `foreground_dim` — it is not a second read"
    );
    assert!(title_ink <= darkness(t.foreground) + ANTIALIAS_SLACK, "and the title is body ink at {title_ink}");
}

/// A status bar is a ground, a rule on the edge that faces the content, and dim readings.
///
/// Painted: a rule on the wrong edge reads as a lid, and a body-ink reading is not a second read.
#[test]
fn a_status_bar_grounds_itself_and_rules_the_edge_it_is_given() {
    let t = Theme::default();
    let (w, h) = (200, STATUS_BAR_H);
    let draw = |rule: Edge| {
        let ui: Element<Msg> = status_bar(status_text("opened", &t), Some(status_text("ln 6", &t)), rule, &t);
        let all = Rect::new(0, 0, w, h);
        let l = layout(&ui, all, &CELL);
        let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
        fb.clear(t.background);
        paint(&mut fb, &font(), &t, &ui, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
        fb
    };
    let top = draw(Edge::Top);
    assert_eq!(top.get_pixel(w / 2, 0), Some(t.border), "the rule is along the top edge");
    assert_eq!(top.get_pixel(w / 2, h - 1), Some(t.face_hover), "and the other edge is ground");
    let bottom = draw(Edge::Bottom);
    assert_eq!(bottom.get_pixel(w / 2, h - 1), Some(t.border), "or along the bottom");
    assert_eq!(bottom.get_pixel(w / 2, 0), Some(t.face_hover), "…and then the top is ground");
    // Both readings are there and neither is body ink, measured by darkness: at the small step a
    // glyph is mostly antialiased. Dim is 91/103/102 and body is 22/32/31.
    let darkness = |c: Rgb| c.r as u32 + c.g as u32 + c.b as u32;
    let darkest = (0..w)
        .flat_map(|x| (0..h).map(move |y| (x, y)))
        .filter_map(|(x, y)| top.get_pixel(x, y))
        .map(darkness)
        .min()
        .expect("the bar has pixels");
    // Each end counted separately: one count over the bar passes on the left reading alone.
    let ink_between = |x0: u32, x1: u32| {
        (x0..x1)
            .flat_map(|x| (0..h).map(move |y| (x, y)))
            .filter(|(x, y)| {
                let c = top.get_pixel(*x, *y);
                c != Some(t.face_hover) && c != Some(t.border)
            })
            .count()
    };
    let (left_ink, right_ink) = (ink_between(0, w / 2), ink_between(w / 2, w));
    assert!(left_ink > 20, "the left reading is drawn: {left_ink} pixels");
    assert!(right_ink > 20, "the right reading is drawn: {right_ink} pixels");
    let floor = darkness(t.foreground_dim);
    assert!(
        darkest >= floor,
        "something is darker than `foreground_dim` ({darkest} against {floor}) — a reading \
             on a status bar is a second read, not body ink"
    );
    assert!(darkness(t.foreground) < floor, "and body ink would fail that");
}

/// A tab's close box is drawn, not merely clickable.
///
/// `Node::Icon` measures as nothing, so a `center` around it gave the glyph a zero rect while the
/// tab still clicked. Painted, because that is the only place the difference exists.
#[test]
fn a_tabs_close_box_puts_ink_on_the_screen() {
    #[derive(Clone, PartialEq, Eq, Debug)]
    enum M {
        Select(u64),
        Close(u64),
    }
    let t = Theme::default();
    let tabs = [Tab { key: 1, label: "one", marked: false }];
    let ui: Element<M> = tab_strip(&tabs, 1, None, M::Select, M::Close, TabExtras::none(), &t);
    let all = Rect::new(0, 0, 200, TAB_STRIP_H);
    let l = layout(&ui, all, &CELL);
    let mut fb = MemFramebuffer::new(Geometry::packed(200, TAB_STRIP_H, PixelFormat::XRGB8888));
    fb.clear(t.background);
    paint(&mut fb, &font(), &t, &ui, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
    let (from, to) = (TAB_SIDE + TAB_W - TAB_CLOSE_W, TAB_SIDE + TAB_W);
    let inked = |x0: u32, x1: u32| {
        (x0..x1)
            .flat_map(|x| (0..TAB_STRIP_H).map(move |y| (x, y)))
            .filter(|(x, y)| fb.get_pixel(*x, *y) == Some(t.foreground_dim))
            .count()
    };
    assert!(inked(from, to) > 8, "the close box drew nothing");
    assert!(inked(to, to + TAB_GAP + 4) == 0, "and it stays inside the tab");
}

/// The current tab is outlined in `border` on three sides, and open on the fourth.
///
/// Painted and read back: an inner face a pixel too wide leaves the tree identical and the tab
/// unoutlined. The resting tab beside it must stay unoutlined.
#[test]
fn the_current_tab_is_outlined_and_open_into_the_content() {
    #[derive(Clone, PartialEq, Eq, Debug)]
    enum M {
        Select(u64),
        Close(u64),
    }
    let t = Theme::default();
    let tabs = [Tab { key: 1, label: "one", marked: false }, Tab { key: 2, label: "two", marked: false }];
    let ui: Element<M> = tab_strip(&tabs, 1, None, M::Select, M::Close, TabExtras::none(), &t);
    let w = TAB_SIDE * 2 + TAB_W * 2 + TAB_GAP;
    let all = Rect::new(0, 0, w, TAB_STRIP_H);
    let l = layout(&ui, all, &CELL);
    let mut fb = MemFramebuffer::new(Geometry::packed(w, TAB_STRIP_H, PixelFormat::XRGB8888));
    fb.clear(t.background);
    paint(&mut fb, &font(), &t, &ui, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
    let at = |x: u32, y: u32| fb.get_pixel(x, y).expect("inside the strip");

    let (left, right) = (TAB_SIDE, TAB_SIDE + TAB_W - 1);
    let (mid, middle_y, rule_y) = (TAB_SIDE + TAB_W / 2, TAB_TOP + TAB_H / 2, TAB_STRIP_H - 1);
    assert_eq!(at(mid, TAB_TOP), t.border, "no line along the current tab's top");
    assert_eq!(at(left, middle_y), t.border, "no line down its left side");
    assert_eq!(at(right, middle_y), t.border, "no line down its right side");
    assert_eq!(at(left + 1, middle_y), t.background, "the outline is more than a pixel wide");
    assert_eq!(at(mid, TAB_TOP + 1), t.background, "the outline is more than a pixel deep");
    // Open at the bottom: the strip's rule stops under the current tab.
    assert_eq!(at(mid, rule_y), t.background, "the current tab is closed at the bottom");

    let next = TAB_SIDE + TAB_W + TAB_GAP;
    assert_ne!(at(next, middle_y), t.border, "an inactive tab was outlined too");
    assert_eq!(at(next + TAB_W / 2, rule_y), t.border, "the rule stopped under an inactive tab");
}

/// Each title-bar button lights on hover, in its own colour, and only that one.
///
/// Painted, with the resting state asserted first. Close lights in `deny`, the other two in
/// `face_pressed`.
#[test]
fn a_title_bar_button_lights_only_when_the_pointer_is_over_it() {
    #[derive(Clone, PartialEq, Eq, Debug)]
    enum M {
        Drag,
        Min,
        Max,
        Close,
    }
    let t = Theme::default();
    let (w, h) = (400u32, TITLE_BAR_H);
    let draw = |hovered: Option<u64>| {
        let e: Element<M> = title_bar(
            "x",
            None,
            true,
            M::Drag,
            TitleButtons { minimise: Some(M::Min), maximise: Some(M::Max), close: Some(M::Close) },
            hovered,
            &t,
        );
        let all = Rect::new(0, 0, w, h);
        let l = layout(&e, all, &CELL);
        let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
        fb.clear(t.background);
        paint(&mut fb, &font(), &t, &e, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
        let rects: alloc::vec::Vec<Rect> = [TITLE_MINIMISE_KEY, TITLE_MAXIMISE_KEY, TITLE_CLOSE_KEY]
            .iter()
            .map(|k| crate::layout::locate(&e, &l, *k).expect("each button is keyed"))
            .collect();
        (fb, rects)
    };
    let count = |fb: &MemFramebuffer, r: Rect, c: Rgb| {
        (r.origin.x as u32..r.right() as u32)
            .flat_map(|x| (r.origin.y as u32..r.bottom() as u32).map(move |y| (x, y)))
            .filter(|(x, y)| fb.get_pixel(*x, *y) == Some(c))
            .count()
    };

    let (fb, rects) = draw(None);
    for r in &rects {
        assert_eq!(count(&fb, *r, t.face_pressed) + count(&fb, *r, t.deny), 0, "a button lit at rest");
    }

    for (i, (key, face)) in
        [(TITLE_MINIMISE_KEY, t.face_pressed), (TITLE_MAXIMISE_KEY, t.face_pressed), (TITLE_CLOSE_KEY, t.deny)]
            .iter()
            .enumerate()
    {
        let (fb, rects) = draw(Some(*key));
        assert!(count(&fb, rects[i], *face) > 100, "button {i} did not light in its colour");
        for (j, r) in rects.iter().enumerate().filter(|(j, _)| *j != i) {
            assert_eq!(
                count(&fb, *r, t.face_pressed) + count(&fb, *r, t.deny),
                0,
                "hovering button {i} lit button {j}"
            );
        }
    }
    let (fb, rects) = draw(Some(TITLE_CLOSE_KEY));
    assert_eq!(count(&fb, rects[2], t.face_pressed), 0, "close lit in the neutral face");
}

/// Hover reaches the keys the buttons carry **through the real router**, not only in a tree.
///
/// The painted tests hand `hovered` in directly, so they pass for a key no pointer could produce.
/// The tab's label keeps its tab's key, and `locate` must still find the whole tab under it.
#[test]
fn hover_names_the_button_and_the_close_box_through_the_router() {
    use crate::diff::Tree;
    use crate::route::Router;
    #[derive(Clone, PartialEq, Eq, Debug)]
    enum M {
        Drag,
        Close,
        Select(u64),
        Shut(u64),
    }
    let t = Theme::default();
    let over = |e: &Element<M>, x: i32, y: i32| {
        let l = layout(e, Rect::new(0, 0, 400, 100), &CELL);
        let mut tree = Tree::new();
        tree.update(e, &l).expect("a clean frame");
        let mut r = Router::new();
        let motion = PointerEvent { kind: librsproto::surface::POINTER_MOTION, x, y, ..Default::default() };
        r.pointer(&tree, e, &l, motion);
        r.hovered_key(&tree)
    };

    let bar: Element<M> = title_bar(
        "x",
        None,
        true,
        M::Drag,
        TitleButtons { minimise: None, maximise: None, close: Some(M::Close) },
        None,
        &t,
    );
    let l = layout(&bar, Rect::new(0, 0, 400, 100), &CELL);
    let c = crate::layout::locate(&bar, &l, TITLE_CLOSE_KEY).expect("close is keyed");
    let (cx, cy) = (c.origin.x + c.size.w as i32 / 2, c.origin.y + c.size.h as i32 / 2);
    assert_eq!(over(&bar, cx, cy), Some(TITLE_CLOSE_KEY), "the pointer over close does not name it");

    let tabs = [Tab { key: 1 << 62, label: "one", marked: false }];
    let strip: Element<M> = tab_strip(&tabs, 1 << 62, None, M::Select, M::Shut, TabExtras::none(), &t);
    let l = layout(&strip, Rect::new(0, 0, 400, 100), &CELL);
    let tab = crate::layout::locate(&strip, &l, 1 << 62).expect("the tab is keyed");
    assert_eq!(tab.size.w, TAB_W, "`locate` found the label rather than the whole tab");
    let y = tab.origin.y + tab.size.h as i32 / 2;
    let close_x = tab.origin.x + (TAB_W - TAB_CLOSE_W / 2) as i32;
    let label_x = tab.origin.x + (TAB_LABEL_PAD + 4) as i32;
    assert_eq!(over(&strip, close_x, y), Some(tab_close_key(1 << 62)), "the `×` has no key of its own");
    assert_eq!(over(&strip, label_x, y), Some(1 << 62), "the label stopped naming its tab");
}

#[test]
fn a_tabs_close_box_lights_on_hover_and_its_tab_stays_lit() {
    #[derive(Clone, PartialEq, Eq, Debug)]
    enum M {
        Select(u64),
        Close(u64),
    }
    let t = Theme::default();
    let tabs = [Tab { key: 1, label: "one", marked: false }, Tab { key: 2, label: "two", marked: false }];
    let w = TAB_SIDE * 2 + TAB_W * 2 + TAB_GAP;
    let draw = |hovered: Option<u64>| {
        let ui: Element<M> = tab_strip(&tabs, 1, hovered, M::Select, M::Close, TabExtras::none(), &t);
        let all = Rect::new(0, 0, w, TAB_STRIP_H);
        let l = layout(&ui, all, &CELL);
        let mut fb = MemFramebuffer::new(Geometry::packed(w, TAB_STRIP_H, PixelFormat::XRGB8888));
        fb.clear(t.background);
        paint(&mut fb, &font(), &t, &ui, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
        fb
    };
    let count = |fb: &MemFramebuffer, x0: u32, x1: u32, c: Rgb| {
        (x0..x1)
            .flat_map(|x| (0..TAB_STRIP_H).map(move |y| (x, y)))
            .filter(|(x, y)| fb.get_pixel(*x, *y) == Some(c))
            .count()
    };
    let tab2 = TAB_SIDE + TAB_W + TAB_GAP;
    let (box0, box1) = (tab2 + TAB_W - TAB_CLOSE_W, tab2 + TAB_W);

    let rest = draw(None);
    assert_eq!(count(&rest, box0, box1, t.face_pressed), 0, "the `×` lit at rest");

    let hot = draw(Some(tab_close_key(2)));
    assert!(count(&hot, box0, box1, t.face_pressed) > 50, "the `×` did not light on hover");
    // Its tab stays lit: the pointer over the `×` names the close box rather than the tab.
    assert!(
        count(&hot, tab2, box0, t.face) > 100,
        "the inactive tab lost its hover face as the pointer reached its `×`"
    );
    let other = (TAB_SIDE + TAB_W - TAB_CLOSE_W, TAB_SIDE + TAB_W);
    assert_eq!(count(&hot, other.0, other.1, t.face_pressed), 0, "the wrong `×` lit");

    // The `×` stays at rest while only its tab is hovered, or it would light with every tab.
    let tab_only = draw(Some(2));
    assert!(count(&tab_only, tab2, box0, t.face) > 100, "precondition: the tab is lit");
    assert_eq!(
        count(&tab_only, box0, box1, t.face_pressed),
        0,
        "the `×` lit with its tab rather than with the pointer on it"
    );
}

/// A lit button's glyph is set in an ink that reads on its face — in both schemes.
///
/// Close is pinned by its reason rather than its colour: the glyph is whichever of the scheme's
/// two inks has the higher WCAG contrast on `deny`, and it clears 4.5:1. That is a different
/// formula from `furthest_from`'s, so this is not the code checking itself.
#[test]
fn a_lit_buttons_glyph_reads_on_its_face_in_both_schemes() {
    #[derive(Clone, PartialEq, Eq, Debug)]
    enum M {
        Drag,
        Min,
        Max,
        Close,
    }
    fn lum(c: Rgb) -> f32 {
        fn ch(v: u8) -> f32 {
            let v = v as f32 / 255.0;
            if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        }
        0.2126 * ch(c.r) + 0.7152 * ch(c.g) + 0.0722 * ch(c.b)
    }
    fn contrast(a: Rgb, b: Rgb) -> f32 {
        let (hi, lo) = if lum(a) >= lum(b) { (lum(a), lum(b)) } else { (lum(b), lum(a)) };
        (hi + 0.05) / (lo + 0.05)
    }
    let draw = |t: &Theme, hovered: Option<u64>| {
        let e: Element<M> = title_bar(
            "x",
            None,
            true,
            M::Drag,
            TitleButtons { minimise: Some(M::Min), maximise: Some(M::Max), close: Some(M::Close) },
            hovered,
            t,
        );
        let (w, h) = (400u32, TITLE_BAR_H);
        let all = Rect::new(0, 0, w, h);
        let l = layout(&e, all, &CELL);
        let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
        fb.clear(t.background);
        paint(&mut fb, &font(), t, &e, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
        let rects: alloc::vec::Vec<Rect> = [TITLE_MINIMISE_KEY, TITLE_MAXIMISE_KEY, TITLE_CLOSE_KEY]
            .iter()
            .map(|k| crate::layout::locate(&e, &l, *k).expect("each button is keyed"))
            .collect();
        (fb, rects)
    };
    let count = |fb: &MemFramebuffer, r: Rect, c: Rgb| {
        (r.origin.x as u32..r.right() as u32)
            .flat_map(|x| (r.origin.y as u32..r.bottom() as u32).map(move |y| (x, y)))
            .filter(|(x, y)| fb.get_pixel(*x, *y) == Some(c))
            .count()
    };

    for (scheme, t) in [("light", Theme::default()), ("dark", Theme::dark())] {
        let (fb, rects) = draw(&t, None);
        for (i, r) in rects.iter().enumerate() {
            assert!(count(&fb, *r, t.foreground_dim) > 0, "{scheme}: button {i}'s glyph is not dim at rest");
            assert_eq!(count(&fb, *r, t.foreground), 0, "{scheme}: button {i} in full ink at rest");
        }

        for (i, key) in [(0, TITLE_MINIMISE_KEY), (1, TITLE_MAXIMISE_KEY)] {
            let (fb, rects) = draw(&t, Some(key));
            assert!(count(&fb, rects[i], t.foreground) > 0, "{scheme}: button {i} lit, glyph not in full ink");
            assert_eq!(count(&fb, rects[i], t.foreground_dim), 0, "{scheme}: button {i} lit, glyph still dim");
        }

        let (better, worse) = if contrast(t.background, t.deny) >= contrast(t.foreground, t.deny) {
            (t.background, t.foreground)
        } else {
            (t.foreground, t.background)
        };
        assert!(
            contrast(better, t.deny) >= 4.5,
            "{scheme}: neither ink reads on `deny` — {:.2}:1 at best",
            contrast(better, t.deny)
        );
        let (fb, rects) = draw(&t, Some(TITLE_CLOSE_KEY));
        assert!(count(&fb, rects[2], better) > 20, "{scheme}: close's glyph is not the legible ink");
        assert_eq!(count(&fb, rects[2], worse), 0, "{scheme}: close's glyph is the illegible ink");
    }
}

#[test]
fn the_new_tab_plus_lights_on_hover() {
    #[derive(Clone, PartialEq, Eq, Debug)]
    enum M {
        Select(u64),
        Close(u64),
        New,
    }
    let t = Theme::default();
    let tabs = [Tab { key: 1, label: "one", marked: false }];
    let w = TAB_SIDE * 2 + TAB_W + TAB_GAP + NEW_TAB_W;
    let draw = |hovered: Option<u64>| {
        let ui: Element<M> = tab_strip(&tabs, 1, hovered, M::Select, M::Close, TabExtras::new_tab(M::New), &t);
        let all = Rect::new(0, 0, w, TAB_STRIP_H);
        let l = layout(&ui, all, &CELL);
        let plus = crate::layout::locate(&ui, &l, NEW_TAB_KEY).expect("the `+` is keyed");
        let mut fb = MemFramebuffer::new(Geometry::packed(w, TAB_STRIP_H, PixelFormat::XRGB8888));
        fb.clear(t.background);
        paint(&mut fb, &font(), &t, &ui, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
        // The darkest pixel, not a count of exact ink: the `+` is antialiased, and which ink the
        // most-covered pixel sits nearer says which ink the glyph was drawn in.
        (plus.origin.x as u32..plus.right() as u32)
            .flat_map(|x| (plus.origin.y as u32..plus.bottom() as u32).map(move |y| (x, y)))
            .filter_map(|(x, y)| fb.get_pixel(x, y))
            .min_by_key(|c| u32::from(c.r) + u32::from(c.g) + u32::from(c.b))
            .expect("the `+` has pixels")
    };
    let nearer_full = |c: Rgb| {
        let d = |a: Rgb, b: Rgb| a.r.abs_diff(b.r) as u32 + a.g.abs_diff(b.g) as u32 + a.b.abs_diff(b.b) as u32;
        d(c, t.foreground) < d(c, t.foreground_dim)
    };
    assert!(!nearer_full(draw(None)), "the `+` is in full ink at rest");
    assert!(nearer_full(draw(Some(NEW_TAB_KEY))), "the `+` did not light on hover");
    assert!(!nearer_full(draw(Some(1))), "the `+` lit when its neighbouring tab was hovered");
}

#[test]
fn a_marked_tab_says_so_in_its_label() {
    // The unsaved mark is the only thing distinguishing two tabs on the same file name.
    #[derive(Clone, PartialEq, Eq, Debug)]
    enum M {
        Select(u64),
        Close(u64),
    }
    let theme = Theme::default();
    let quiet = [Tab { key: 1, label: "notes", marked: false }];
    let dirty = [Tab { key: 1, label: "notes", marked: true }];
    let extras = || TabExtras::none();
    let a: Element<M> = tab_strip(&quiet, 1, None, M::Select, M::Close, extras(), &theme);
    let b: Element<M> = tab_strip(&dirty, 1, None, M::Select, M::Close, extras(), &theme);
    assert_eq!(all_text(&a), "notes");
    assert_eq!(all_text(&b), "* notes");
}

#[test]
fn the_title_buttons_land_where_the_gates_aim() {
    // The literals the gates' `chrome` table types, asserted against a framed window actually
    // built: `title_button_centre` moves with the constants. In a window 400 wide: close 18 in
    // from the right, maximise 50, minimise 82, all 16 down.
    assert_eq!(title_button_centre(400, 0), (382, 16));
    assert_eq!(title_button_centre(400, 1), (350, 16));
    assert_eq!(title_button_centre(400, 2), (318, 16));

    #[derive(Clone, PartialEq, Eq, Debug)]
    enum M {
        Drag,
        Min,
        Max,
        Close,
    }
    let theme = Theme::default();
    let bar = title_bar(
        "a window",
        None,
        true,
        M::Drag,
        TitleButtons { minimise: Some(M::Min), maximise: Some(M::Max), close: Some(M::Close) },
        None,
        &theme,
    )
    .key(1);
    let e: Element<M> = window_frame(bar, sized(Size::new(0, 0), text("")).key(2), true, &theme);
    let cell = crate::layout::FixedCell { w: 8, h: 16 };
    let l = crate::layout::layout(&e, Rect::new(0, 0, 400, 200), &cell);
    let mut tree = crate::diff::Tree::new();
    tree.update(&e, &l).expect("a clean frame");
    let click = |(x, y): (i32, i32)| {
        let at = |pressed: bool| PointerEvent {
            kind: POINTER_BUTTON,
            button: 0x110,
            buttons: u16::from(pressed),
            flags: if pressed { POINTER_PRESSED } else { 0 },
            x,
            y,
            ..Default::default()
        };
        let mut r = crate::route::Router::new();
        let _ = r.pointer(&tree, &e, &l, at(true));
        r.pointer(&tree, &e, &l, at(false)).0
    };
    assert_eq!(click((382, 16)), vec![M::Close]);
    assert_eq!(click((350, 16)), vec![M::Max]);
    assert_eq!(click((318, 16)), vec![M::Min]);
}

#[test]
fn a_framed_windows_grip_is_inside_its_border_and_under_it() {
    // The grip is placed inside the border, as a layer before it, and takes no presses
    // anywhere but the grip, which a full-size overlay would.
    #[derive(Clone, PartialEq, Eq, Debug)]
    enum M {
        Content,
        Grip,
    }
    let theme = Theme::default();
    let window = Size::new(200, 120);
    let content: Element<M> =
        sized(Size::new(0, 0), crate::element::custom(1, Size::new(0, 0))).on_press(M::Content).key(2);
    let e = window_frame_with_grip(
        sized(Size::new(0, TITLE_BAR_H), fill(theme.face)).key(1),
        content,
        resize_grip(M::Grip, &theme).key(3),
        window,
        true,
        &theme,
    );
    let crate::element::Node::Stack(layers) = &e.node else { panic!("the frame is a stack") };
    assert!(
        matches!(layers.last().map(|l| &l.node), Some(crate::element::Node::Outline { .. })),
        "the border is not the top layer, so the grip is painted over it"
    );
    let cell = crate::layout::FixedCell { w: 8, h: 16 };
    let l = crate::layout::layout(&e, Rect::new(0, 0, window.w, window.h), &cell);
    let grip = crate::layout::locate(&e, &l, 3).expect("the grip is keyed");
    assert_eq!(
        (grip.right(), grip.bottom()),
        ((window.w - WINDOW_BORDER) as i64, (window.h - WINDOW_BORDER) as i64),
        "the grip is not in the corner inside the border: {grip:?}"
    );
    let mut tree = crate::diff::Tree::new();
    tree.update(&e, &l).expect("a clean frame");
    let click = |x: i32, y: i32| {
        let at = |pressed: bool| PointerEvent {
            kind: POINTER_BUTTON,
            button: 0x110,
            buttons: u16::from(pressed),
            flags: if pressed { POINTER_PRESSED } else { 0 },
            x,
            y,
            ..Default::default()
        };
        let mut r = crate::route::Router::new();
        let mut got = r.pointer(&tree, &e, &l, at(true)).0;
        got.extend(r.pointer(&tree, &e, &l, at(false)).0);
        got
    };
    assert_eq!(click(window.w as i32 - 8, window.h as i32 - 8), vec![M::Grip], "the grip's middle");
    assert_eq!(click(window.w as i32 - 30, window.h as i32 - 8), vec![M::Content], "beside it is content");
}

#[test]
fn dialog_buttons_land_where_the_constants_say() {
    // The four numbers `check-login` types, asserted against a tree actually built. The
    // literals matter as much as the derivation: derived constants compared against a tree
    // built from the same constants pin nothing, and the gate's table is linked to neither.
    assert_eq!((DIALOG_W, DIALOG_H), (340, 132));
    assert_eq!((DIALOG_LEFT_CX, DIALOG_RIGHT_CX, DIALOG_BUTTON_CY), (89, 250, 106));

    #[derive(Clone, PartialEq, Eq, Debug)]
    enum M {
        Left,
        Right,
        Drag,
    }
    let theme = Theme::default();
    let answer = |label: &str, msg: M, key: u64| button(label, msg, WidgetState::default(), &theme).key(key).flex(1);
    let ui: Element<M> = dialog_frame(
        title_bar(
            "Question",
            None,
            true,
            M::Drag,
            TitleButtons { minimise: None, maximise: None, close: None },
            None,
            &theme,
        )
        .key(1),
        padding(Insets::all(DIALOG_PAD), text("Really?")).key(2),
        crate::element::with_spacing(
            row(alloc::vec![answer("yes", M::Left, 3), answer("no", M::Right, 4)]),
            DIALOG_GAP,
        )
        .key(5),
        true,
        &theme,
    );

    // It measures to exactly what it declares, which lets it be a `Child`.
    let cell = crate::layout::FixedCell { w: 8, h: 16 };
    assert_eq!(
        crate::layout::measure(&ui, crate::layout::Constraints::loose(Size::new(u32::MAX / 4, u32::MAX / 4)), &cell,),
        Size::new(DIALOG_W, DIALOG_H)
    );

    let l = crate::layout::layout(&ui, Rect::new(0, 0, DIALOG_W, DIALOG_H), &cell);
    let mut tree = crate::diff::Tree::new();
    tree.update(&ui, &l).expect("a dialog is diffable");
    let mut router = crate::route::Router::new();
    let click = |r: &mut crate::route::Router, x: i32, y: i32| {
        let at = |flags: u16, buttons: u16| PointerEvent {
            kind: librsproto::surface::POINTER_BUTTON,
            button: 0x110,
            buttons,
            flags,
            x,
            y,
            ..Default::default()
        };
        r.pointer(&tree, &ui, &l, at(librsproto::surface::POINTER_PRESSED, 1));
        r.pointer(&tree, &ui, &l, at(0, 0)).0
    };
    assert_eq!(click(&mut router, DIALOG_LEFT_CX, DIALOG_BUTTON_CY), alloc::vec![M::Left]);
    assert_eq!(click(&mut router, DIALOG_RIGHT_CX, DIALOG_BUTTON_CY), alloc::vec![M::Right]);

    // The aim point is the button's centre: padding the strip on four sides instead of three
    // halves the buttons, so the row's own height is bracketed here.
    let half = DIALOG_BUTTON_H as i32 / 2;
    for edge in [DIALOG_BUTTON_CY - half + 1, DIALOG_BUTTON_CY + half - 1] {
        assert_eq!(
            click(&mut router, DIALOG_LEFT_CX, edge),
            alloc::vec![M::Left],
            "the button does not reach {edge}, so {DIALOG_BUTTON_CY} is not its centre"
        );
    }
    assert!(
        click(&mut router, DIALOG_LEFT_CX, DIALOG_BUTTON_CY - DIALOG_BUTTON_H as i32).is_empty(),
        "a whole button above the aim point is not the button"
    );
}
