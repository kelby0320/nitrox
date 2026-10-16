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
    for s in [ScrollState { offset: 0, visible: 100, total: 100 }, ScrollState { offset: 0, visible: 200, total: 100 }]
    {
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
fn a_grab_maps_back_to_the_offset_the_thumb_was_drawn_for() {
    // `offset_at` is `thumb`'s inverse, and the two are the only pair in the toolkit that
    // *must* agree: one decides where the thumb is painted, the other what grabbing it
    // there means. A drift between them is a thumb that jumps away from the cursor.
    //
    // Over the whole range rather than one offset, because the arithmetic is two
    // divisions and a clamp, and the ends are where each of them goes wrong.
    //
    // **Within a pixel's worth of lines, not exactly** — and that is a fact about
    // scrollbars, not a weak assertion. A thousand lines over a ~390-pixel span puts
    // roughly three lines on every pixel, so a thumb drawn for line 500 is at the same
    // pixel as one drawn for 501, and no inverse can tell them apart. Demanding equality
    // here would be demanding the impossible; **the ends are exact**, which is the part
    // that matters and the part off-by-one errors break.
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

    // And where the arithmetic *can* be exact — fewer lines than pixels of span — it is,
    // so the tolerance above is the quantisation and not a bug hiding inside it.
    let s = ScrollState { offset: 0, visible: 10, total: 40 };
    for offset in 0..=30 {
        let s = ScrollState { offset, ..s };
        let (pos, len) = s.thumb(400);
        assert_eq!(s.offset_at(400, pos as i32 + len as i32 / 2), offset);
    }
}

#[test]
fn dragging_past_either_end_of_the_track_stays_at_that_end() {
    // The router hands a captured widget coordinates outside itself rather than clamping
    // them — deliberately, so a drag is not indistinguishable from a drag that stopped at
    // the edge. Something has to clamp, and this is it.
    let s = ScrollState { offset: 300, visible: 24, total: 1024 };
    assert_eq!(s.offset_at(400, -900), 0, "dragged far above the bar");
    assert_eq!(s.offset_at(400, 9000), 1000, "and far below it");
    // The bottom of the track is the last line, not one short of it: the rounding.
    assert_eq!(s.offset_at(400, 400), 1000, "the very bottom is the end of the document");
}

#[test]
fn a_bar_with_nothing_to_scroll_reports_nothing_wherever_it_is_grabbed() {
    // Otherwise a short document scrolls when its full-height thumb is dragged, which is
    // the visible form of dividing by a zero span.
    let s = ScrollState { offset: 0, visible: 24, total: 24 };
    assert!(!s.scrollable());
    for y in [-5, 0, 100, 4000] {
        assert_eq!(s.offset_at(400, y), 0, "at y={y}");
    }
    // And a track so short that `MIN_THUMB` fills it — reachable, not hypothetical.
    let s = ScrollState { offset: 5, visible: 24, total: 1024 };
    assert_eq!(s.thumb(MIN_THUMB).1, MIN_THUMB, "the premise: the thumb fills the track");
    assert_eq!(s.offset_at(MIN_THUMB, 8), 0, "a position it cannot express");
}

#[test]
fn a_scrollbar_lays_its_thumb_out_where_the_arithmetic_says() {
    // The layout half: a `Column` of spacer, thumb and filler places the thumb without
    // any node needing to offset a child.
    let p = Theme::default();
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
    let p = Theme::default();
    let e = button("OK", (), WidgetState::default(), &p);
    assert!(e.on_press.is_some(), "a button that sends nothing is not a button");
    assert!(e.focusable, "a keyboard user must be able to reach it");
}

#[test]
fn a_buttons_face_follows_the_state_it_is_given() {
    // Widgets take their state as an argument rather than remembering it, so this is the
    // whole of a button's appearance logic and it is a pure function.
    let p = Theme::default();
    let face_of = |st: WidgetState| {
        let e: Element<Msg> = button("OK", (), st, &p);
        // The first layer is the face at rest and the ring when active; either way the
        // painted face is the last `Fill` before the label.
        let fills: vec::Vec<Rgb> = e
            .children()
            .filter_map(|c| match &c.node {
                // A button's layers are rounded fills since Part H; a square `Fill` is still
                // matched so this reads whichever shape the widget is built from.
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
    // Painted rather than inspected: the ring is the outermost layer, so a face drawn
    // over the whole area would hide it and the tree would still look right.
    let p = Theme::default();
    let t = Theme::default();
    let mut fb = MemFramebuffer::new(Geometry::packed(80, 40, PixelFormat::XRGB8888));
    let e: Element<Msg> = button("OK", (), WidgetState { active: true, ..Default::default() }, &p);
    let l = layout(&e, Rect::new(0, 0, 80, 40), &CELL);
    paint(&mut fb, &font(), &t, &e, &l, Rect::new(0, 0, 80, 40), &mut |_, _, _, _: &mut MemFramebuffer| {});
    // **Down the left edge rather than at the corner**: the button is rounded to
    // `CONTROL_RADIUS` since Part H, so `(0, 0)` is the ground the curve was cut out of.
    assert_eq!(fb.get_pixel(0, 20), Some(p.accent), "the ring is on the edge");
    // **Inside the ring but away from the label**, which is centred since M15: the middle
    // of the button is where the word is, so a sample taken there is a glyph.
    assert_eq!(fb.get_pixel(6, 20), Some(p.face), "and the face is inside it");
}

/// A resting button has an edge, and it is not the focus ring.
///
/// **What makes a shape read as a control** (M15). The face is eighteen units per channel
/// from a white window, which is a difference nobody can see — the report from running it
/// was that buttons "need different color background … so you know it's a button". An edge
/// is what every desktop draws and what this toolkit had only around a focused control.
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

/// A resting field has an edge, and on a white card that edge is the only thing that says
/// there is a field there at all.
///
/// **The greeter is what found this** (desktop refresh, Part D): a field was a flat fill of
/// `track`, `track` is the same white as a window's own ground in the light scheme, and so a
/// password box at rest was invisible. Painted rather than inspected, because the ground is
/// drawn over the edge if the layers are the wrong way round and the tree still looks right.
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
    // Down the left edge, clear of the rounded corners.
    let mid = l.rect.size.h / 2;
    assert_eq!(fb.get_pixel(0, mid), Some(p.border), "a resting field has an edge");
    assert_ne!(p.border, p.track, "…and it is not the colour of the well inside it");
    assert_ne!(p.border, p.background, "…nor of the surface it sits on");
    assert_eq!(fb.get_pixel(4, mid), Some(p.track), "the well is inside the edge");
    // Focused, the edge is the accent instead — a state, not a second kind of edge.
    let mut lit = MemFramebuffer::new(Geometry::packed(80, 40, PixelFormat::XRGB8888));
    lit.clear(p.background);
    let f: Element<Msg> = text_field(&state, false, WidgetState { active: true, ..Default::default() }, &p);
    let fl = layout(&f, Rect::new(0, 0, 80, 40), &CELL);
    paint(&mut lit, &font(), &p, &f, &fl, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
    assert_eq!(lit.get_pixel(0, mid), Some(p.accent), "a focused field rings in the accent");
}

/// The label is in the middle of the button, not against its corner.
///
/// **Measured as a distance from each edge**, because the exact pixels a glyph lands on are
/// the font's business: what this asserts is that the ink is not hard against the left,
/// which is where `padding` alone put it — a button 80 wide with a word 20 wide had 58
/// pixels of face to the right of its label (M15).
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
    // **The test this file was missing.** Every other one here lays *one* widget into a
    // rectangle of its own, where a widget that measures to "everything available" and one
    // that measures to its content are indistinguishable. Put two beside each other and
    // they are not: until 2026-08-11 `Node::Fill` measured to `c.max`, so the first
    // `button` — a `Stack` over a `Fill` — took the whole row and the second laid out at
    // zero width, off the right edge. The bar looked correct in every assertion above.
    let p = Theme::default();
    let items: vec::Vec<Element<Msg>> =
        vec![button("File", (), WidgetState::default(), &p), button("Edit", (), WidgetState::default(), &p)];
    let e = menu_bar(items, 24, &p);
    let l = layout(&e, Rect::new(0, 0, 200, 100), &CELL);
    // sized -> dock -> [the rule, stack] ; stack -> [face fill, row] ; row -> the buttons.
    // (One level deeper since the desktop refresh's Part B put a rule under the bar.)
    let row = &l.children[0].children[1].children[1];
    let (a, b) = (row.children[0].rect, row.children[1].rect);
    assert!(a.size.w > 0, "the first item measured to nothing");
    assert!(b.size.w > 0, "the second item got no width — the first one ate the row");
    assert_eq!(b.origin.x, a.origin.x + a.size.w as i32, "and they sit side by side");
    assert!(b.right() <= 200, "the second item runs off the bar: {:?}", b);
}

#[test]
fn a_button_measures_to_its_label_not_to_the_room_it_is_given() {
    // The same defect stated as a property rather than as a composition. A button in a
    // 400-pixel-tall column is not a 400-pixel-tall button.
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
    // **The layout property a person notices**, and the one that decides where a press
    // lands: the buttons are at the right, the title runs from the left, and everything
    // between them is draggable. A `flex(1)` label that measured to its text instead would
    // leave the buttons floating next to the title in the middle of the bar.
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
    // **Asked of the router, not read off the layout tree**, which this used to walk by child
    // index and which changed shape when the bar gained a rule and its buttons an ink
    // (desktop refresh, Part B). Where a press *lands* is the property; the tree's shape was
    // only ever a way of guessing it.
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
    // The buttons sit at the right edge, `TITLE_BUTTON_PAD` in, each answering across its
    // whole column and not a pixel beyond it.
    let right = (400 - TITLE_BUTTON_PAD) as i32;
    let step = (TITLE_BUTTON_W + TITLE_BUTTON_GAP) as i32;
    for (nth, want) in [(0, M::Close), (1, M::Max), (2, M::Min)] {
        let end = right - nth * step;
        let start = end - TITLE_BUTTON_W as i32;
        assert_eq!(click(start, 12), vec![want.clone()], "{want:?}'s first column");
        assert_eq!(click(end - 1, 12), vec![want.clone()], "{want:?}'s last column");
        assert_eq!(click(end, 12), vec![M::Drag], "just past {want:?} is the bar");
    }
    // And the face takes the rest: the title's end of the bar, the gaps, the padding.
    assert_eq!(click(1, 12), vec![M::Drag], "the title starts at the left and drags");
    assert_eq!(click(399, 12), vec![M::Drag], "the padding after close drags");
}

// ---- the text area (M10 Part C) ----

/// `abc` / `de` / `fghi`, cursor at the start.
fn area() -> TextAreaState {
    TextAreaState::with_text("abc\nde\nfghi")
}

/// Type `text` into `a`, one character at a time, the way a person does.
fn type_text(a: &mut TextAreaState, text: &str) {
    for c in text.chars() {
        if c == '\n' {
            a.newline();
        } else {
            a.insert(c);
        }
    }
}

/// **An area types under the key's locks** (Phase 6 Part B.5; PR #359 review), as a field does.
#[test]
fn an_area_types_under_the_keys_locks() {
    use librsproto::surface::{LOCK_CAPS, LOCK_NUM};
    let mut a = TextAreaState::with_text("");
    assert!(a.apply(30, 0, LOCK_CAPS), "a, under Caps Lock");
    assert!(a.apply(79, 0, LOCK_NUM), "keypad 1, under Num Lock");
    assert_eq!(a.text(), "A1");
}

// ---- paste and its range (M12 Part E) ----

#[test]
fn insert_text_returns_the_range_it_occupies() {
    // **The return value is what makes cycling possible at all.** M12 decision 3 says a
    // cycle *replaces what was just inserted*, so the caller has to be told where that is —
    // and deriving it at the call site from the cursor and the text's shape would be the
    // same arithmetic done somewhere with less to check it against.
    let mut a = area();
    a.apply(KEY_RIGHT, 0, 0); // between `a` and `bc`
    let (from, to) = a.insert_text("XY");
    assert_eq!(a.text(), "aXYbc\nde\nfghi");
    assert_eq!(from, (0, 1));
    assert_eq!(to, (0, 3), "one line, so the range is columns 1..3");

    // …and a multi-line paste ends on the line it made, not the one it started on.
    let mut a = area();
    let (from, to) = a.insert_text("one\ntwo");
    assert_eq!(from, (0, 0));
    assert_eq!(to, (1, 3));
    assert_eq!(a.text(), "one\ntwoabc\nde\nfghi", "and the tail followed it down");
}

#[test]
fn select_range_then_insert_text_is_the_cycle() {
    // The two halves together are what `nxedit::App::cycled` does. Doing it here as well
    // pins the *primitives*: `nxedit`'s own test would pass against a `select_range` that
    // selected nothing, because `insert_text` inserts at the cursor either way.
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
    // **The guard is documented and was not covered** (PR #271 review, optional 7). A range
    // is only valid until the next edit, and a cycle that arrived after one would otherwise
    // index out of the buffer — in an editor holding somebody's unsaved work. Clamping
    // makes a stale range select something harmless instead.
    let mut a = area();
    a.select_range((99, 99), (99, 99));
    assert_eq!(a.selected_text(), None, "collapsed onto the end of the last line");
    assert_eq!(a.cursor(), (2, 4), "and the cursor is inside the buffer");
    assert_eq!(a.text(), "abc\nde\nfghi", "nothing was changed by asking");
}

#[test]
fn select_range_lands_on_a_character_boundary() {
    // Columns are byte offsets, and a stale one can point into the middle of a multi-byte
    // character — where every `String` operation below panics. Clamping walks back to the
    // boundary rather than trusting the number.
    let mut a = TextAreaState::with_text("aéb");
    // `é` is two bytes, so 2 is inside it.
    a.select_range((0, 0), (0, 2));
    assert_eq!(a.selected_text().as_deref(), Some("a"));
}

// ---- undo and redo (M12 Part C) ----

#[test]
fn a_word_is_one_undo_step_and_so_is_a_line() {
    // **The grouping is the decision, not the stack.** Per keystroke is unusable — undoing a
    // sentence becomes forty presses — and per save is useless. A run of printable characters
    // is one group, a separator ends it, and `Enter` ends it.
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
    // **`Enter` joins the group it lands in and closes it**, so the line you just typed —
    // its text *and* its break — comes back in one step, and what you type on the next line
    // is a step of its own. Splitting the break out would make undoing a line take two
    // presses that look identical.
    let mut a = TextAreaState::new();
    type_text(&mut a, "one\ntwo");
    assert!(a.undo());
    assert_eq!(a.text(), "one\n", "the second line's word");
    assert!(a.undo());
    assert_eq!(a.text(), "", "then the first line and its break, together");
    assert!(!a.undo());

    // And a break typed after a movement is its own group, because the movement closed the
    // one before it.
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
    // **Wherever it lands.** Without this, typing at one end of a line, moving to the other
    // and typing again undoes both at once — two pieces of text a person put in two places.
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
    // A run of deletions coalesces, and typing after them starts a new group rather than
    // extending the one that was removing.
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
    // A `Backspace` at the start of the buffer and a `Delete` at its end change nothing, and
    // a snapshot for either is an undo press that visibly does nothing — which reads as a
    // broken undo rather than an empty history.
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
    // **The fifth boundary, and the one with no test until PR #269's review said so.** A
    // click is a movement and closes a group for the same reason an arrow key does — and it
    // is forward-looking here: nothing outside this crate's own tests wires a press to
    // `place` yet, so the editor cannot reach it. That is a reason for a test rather than
    // against one: the day `text_area` grows click-to-place, this rule has to already hold.
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
    // A save is the boundary this exists for: what a person wants back afterwards is what
    // they have typed *since*, and without an explicit end the whole session is one group.
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

    // **Typing after an undo is a branch**, so what was undone is not coming back.
    a.undo();
    assert_eq!(a.text(), "one ");
    a.insert('X');
    assert!(!a.redo(), "the way forward is gone");
    assert_eq!(a.text(), "one X");
}

#[test]
fn the_history_is_bounded_and_drops_the_oldest() {
    // Each step is a copy of the whole buffer, so the depth bound is the memory bound.
    let mut a = TextAreaState::new();
    for i in 0..MAX_UNDO + 10 {
        a.insert(char::from(b'a' + (i % 26) as u8));
        // A separator between each, so every character is its own group.
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
    // `revision` is what an editor derives "modified" from, and undo has to move it or a
    // buffer taken back to something else would read as matching the file.
    let mut a = TextAreaState::new();
    a.insert('x');
    let after_typing = a.revision();
    a.undo();
    assert_ne!(a.revision(), after_typing);
}

// ---- find (M12 Part C) ----

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
    // A cursor sitting silently at a hit leaves the person to spot it; a highlight says which
    // of several this one is.
    let mut a = TextAreaState::with_text("alpha beta");
    assert!(a.find("beta"));
    assert_eq!(a.selected_text().as_deref(), Some("beta"));
}

#[test]
fn find_answers_no_for_what_is_not_there_and_for_nothing() {
    let mut a = TextAreaState::with_text("alpha");
    assert!(!a.find("omega"), "absent");
    assert_eq!(a.cursor(), (0, 0), "and the cursor did not move");
    // An empty needle is what the field holds before anything is typed; matching everything
    // would move the buffer under the person on the way to their first character.
    assert!(!a.find(""));
    assert_eq!(a.cursor(), (0, 0));
}

#[test]
fn find_does_not_split_a_multi_byte_character() {
    // The search starts one byte past the cursor, and that byte can be inside a character.
    let mut a = TextAreaState::with_text("é and é");
    assert!(a.find("é"), "the second one");
    assert_eq!(a.cursor(), (0, 7));
    assert!(a.find("é"), "and back to the first");
    assert_eq!(a.cursor(), (0, 0));
}

/// How many `Fill`s of `colour` the tree holds — the caret is one, and a selection's
/// highlight is one per line it covers.
///
/// **What `text_area` draws had no host coverage in either direction** until PR #258's
/// review; `check-display` renders one arrangement of it and a picture cannot count.
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
    // **What was opened is what is saved.** A file that ended with a newline has a final
    // empty line; one that did not, does not — and an editor that "helpfully" added one
    // would rewrite every file it touched on the first save.
    for src in ["abc\nde", "abc\nde\n", "", "\n"] {
        assert_eq!(TextAreaState::with_text(src).text(), src, "round trip of {src:?}");
    }
    // A carriage return is dropped rather than kept: an invisible character at the end of
    // every line is one the cursor has to step over and nobody can see.
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
    // The case a single-line field never has. The cursor lands **where the join happened**,
    // which is where the text the person was deleting towards now is — not at the start of
    // the merged line.
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
    // **The goal column.** Down from column 3 of `abc` onto `de` (length 2) clamps to 2 —
    // and coming back up must return to 3, not stay at 2. Without it a person who pressed
    // only Down and Up has had their column moved for them.
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
    // Otherwise the goal outlives the intent that set it: press Down, Left, Down, and the
    // second Down would jump back out to a column the person just moved away from.
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
    // The anchor may be before or after the cursor; every consumer wants document order.
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
    // **The rule that makes a selection worth having.** An editor where typing appends
    // beside a highlighted run rather than replacing it is one nobody can use.
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
    // **`ensure_visible` is the widget's to call, not the application's.** A caller that had
    // to remember it would have a cursor that walks off the bottom of its own window — and
    // the widget takes `&mut` precisely so it cannot be forgotten (PR #257 review).
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
    // **What an editor's "modified" marker is made of.** The alternatives it replaces are
    // both wrong: comparing byte length misses replacing a one-character selection with one
    // character, and re-deriving which keycodes edit is a second copy of `apply`'s dispatch.
    let mut a = area();
    let start = a.revision();

    for k in [KEY_RIGHT, KEY_DOWN, KEY_END, KEY_HOME, KEY_UP, KEY_LEFT] {
        a.apply(k, 0, 0);
    }
    assert_eq!(a.revision(), start, "moving is not editing");

    // **Typed with nothing selected, and that is not incidental.** The first version made a
    // selection first and then typed, so the insert's own bump was covered by the deletion
    // of the selection — the assertion passed with `insert` not counting at all.
    a.apply(KEY_X, 0, 0);
    assert!(a.revision() > start, "typing is");

    let mut sel = area();
    let quiet = sel.revision();
    for _ in 0..2 {
        sel.apply(KEY_RIGHT, MOD_SHIFT, 0);
    }
    assert_eq!(sel.revision(), quiet, "nor is selecting");

    // The case a length comparison cannot see: one character selected, one typed.
    let mut b = TextAreaState::with_text("abc");
    b.apply(KEY_RIGHT, MOD_SHIFT, 0);
    let before = b.revision();
    let len = b.text().len();
    b.apply(KEY_X, 0, 0);
    assert_eq!(b.text().len(), len, "the fixture must keep its length, or it proves nothing");
    assert!(b.revision() > before, "replacing a selection is an edit");

    // And an edit that does nothing is not one.
    let mut c = TextAreaState::with_text("abc");
    let quiet = c.revision();
    assert!(!c.apply(KEY_BACKSPACE, 0, 0), "backspace at the start of the buffer does nothing");
    assert_eq!(c.revision(), quiet, "so it is not an edit");
}

#[test]
fn every_edit_path_counts_its_own_edit() {
    // **One case per bump, because the bumps are not one line.** `insert` has its own and so
    // does the `delete_selection` it calls first; `backspace` and `delete` each have two, a
    // character and a join. A test that only typed left five of the seven uncontrolled, and
    // the one that matters most is `delete_selection`'s: `backspace` and `delete` both
    // return the moment it reports it deleted something, so without its bump a person could
    // select a word, press Delete, and watch the buffer change while the title bar kept
    // saying the file was saved (PR #259 review, finding 2).
    let bumps = |setup: &dyn Fn(&mut TextAreaState)| -> u64 {
        let mut a = area();
        let before = a.revision();
        setup(&mut a);
        a.revision() - before
    };

    // A selection, deleted by each of the two keys that delete one.
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

    // Backspace's two paths: a character, and the join at the start of a line.
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

    // Delete's two, the same shape from the other side.
    assert!(bumps(&|a| assert!(a.apply(KEY_DELETE, 0, 0))) > 0, "delete over a character");
    assert!(
        bumps(&|a| {
            a.apply(KEY_END, 0, 0);
            assert!(a.apply(KEY_DELETE, 0, 0));
        }) > 0,
        "delete joining two lines"
    );

    // And the two that change nothing still count nothing, so "an edit" means an edit.
    let mut ends = TextAreaState::with_text("a");
    let quiet = ends.revision();
    assert!(!ends.apply(KEY_BACKSPACE, 0, 0), "backspace at the buffer's start");
    ends.apply(KEY_END, 0, 0);
    assert!(!ends.apply(KEY_DELETE, 0, 0), "delete at its end");
    assert_eq!(ends.revision(), quiet);
}

#[test]
fn the_caret_is_drawn_at_either_end_of_a_selection() {
    // **The half a picture cannot check.** `check-display`'s reference builds its selection
    // with `Shift+Right`, so the gate compares a *forward* one — and the caret was drawn
    // only after the highlight, which a forward selection satisfies and a backward one
    // never does. Both directions here, counted in the tree (PR #258 review, blocking 2).
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

/// Every `(text, ink)` pair in a tree, in order — an ink node's text, or `None` for plain.
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

/// A run splits its line and colours exactly its own bytes.
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

/// A pixel becomes a line and a column, and the column is the nearest boundary.
///
/// **Nearest, not the character under the cursor**: clicking the right half of a letter puts
/// the caret after it, which is what every editor does and what makes clicking past the end
/// of a word land after the word rather than inside it.
#[test]
fn a_point_becomes_the_nearest_line_and_column() {
    // Eight pixels a character, which is what the fixed metric these tests use gives.
    let w = |s: &str| (s.chars().count() * 8) as u32;
    let a = TextAreaState::with_text("abcdef\nghi");
    let px = |col: usize| FIELD_PAD.left as i32 + col as i32 * 8;
    let py = |row: usize| FIELD_PAD.top as i32 + row as i32 * 16 + 4;

    assert_eq!(a.at_point(px(0), py(0), 16, w), (0, 0));
    assert_eq!(a.at_point(px(3), py(0), 16, w), (0, 3), "a boundary is itself");
    assert_eq!(a.at_point(px(3) + 6, py(0), 16, w), (0, 4), "past the middle is the next");
    assert_eq!(a.at_point(px(3) + 2, py(0), 16, w), (0, 3), "before it is this one");
    assert_eq!(a.at_point(px(0), py(1), 16, w), (1, 0), "the second row is the second line");

    // **Off the ends is clamped, in both directions.** A drag runs off a widget routinely —
    // the router hands a captured widget negative coordinates rather than clamping them.
    assert_eq!(a.at_point(-40, py(0), 16, w), (0, 0));
    assert_eq!(a.at_point(px(99), py(0), 16, w), (0, 6), "past the end of the line is its end");
    assert_eq!(a.at_point(px(0), py(9), 16, w), (1, 0), "past the last line is the last line");
    assert_eq!(a.at_point(px(0), -80, 16, w), (0, 0));
}

/// A scrolled area maps a point to the line that is *on screen* there.
///
/// **The offset is the whole of it**, and leaving it out is the bug that makes clicking work
/// perfectly until the first time somebody scrolls — after which every click lands the same
/// number of lines too high.
#[test]
fn a_point_is_read_against_what_is_on_screen() {
    let w = |s: &str| (s.chars().count() * 8) as u32;
    let mut a = TextAreaState::with_text("0\n1\n2\n3\n4\n5\n6\n7\n8\n9");
    a.scroll_to(4, 3);
    assert_eq!(a.offset(), 4, "precondition: scrolled");
    let y = FIELD_PAD.top as i32 + 4;
    assert_eq!(a.at_point(FIELD_PAD.left as i32, y, 16, w).0, 4, "the top row is line 4");
}

/// The scrollbar's state, and a scroll that leaves the cursor where it was.
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
    // Clamped to what is left, so a drag past the end shows the last screen rather than
    // scrolling into blank space.
    a.scroll_to(99, 4);
    assert_eq!(a.offset(), 6, "ten lines, four visible");
}

/// The caret is drawn where the cursor is in the middle of a line.
///
/// **The commonest caret position there is, and the one the rewrite newly depends on `cuts`
/// for.** Every other test puts it at column 0, at the line's end, or at an end of a
/// selection — each of which is already a cut for another reason, so deleting the caret's
/// own cut left all 240 tests green while a person typing mid-line saw no caret at all
/// (PR #289 review, 2).
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

/// A coloured run under a selection keeps its colour.
///
/// **The reason ink is a wrapper rather than a property of the text node.** The selection is
/// a `fill` beneath the glyphs, so a selected keyword is a stack of a fill and an inked text
/// — and a version that dropped the ink when the piece was selected would make selecting a
/// line turn all of it black, which is the moment a person is most likely to be reading it.
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
/// **Reachable on every keystroke.** The runs are scanned from the buffer as it was a moment
/// ago, so deleting the end of a line leaves one naming bytes that are gone — and a run whose
/// bounds landed inside a multi-byte character would panic on the slice. A wrong colour for
/// one frame is the right failure; a crashed editor is not.
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

    // A boundary inside a character is dropped rather than split on.
    let mut a = TextAreaState::with_text("\u{4e2d}\u{6587}");
    let e: Element<()> =
        text_area(&mut a, 16, 16, false, &[InkRun { line: 0, start: 1, end: 2, colour: KEYWORD }], None, &p);
    assert_eq!(inked(&e).len(), 1, "the line was split inside a character: {:?}", inked(&e));

    // And a run naming a line that is not on screen colours nothing.
    let mut a = TextAreaState::with_text("only");
    let e: Element<()> =
        text_area(&mut a, 16, 16, false, &[InkRun { line: 40, start: 0, end: 2, colour: KEYWORD }], None, &p);
    assert!(inked(&e).iter().all(|(_, c)| c.is_none()));
}

#[test]
fn a_selection_is_highlighted_on_every_line_it_covers() {
    // The other half of what `text_area` draws, and the reason the count is per *line*: a
    // multi-line selection is one highlight per row, not one rectangle.
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
    // **Two keystrokes to arm and one to fire**, which is why it survived a review: walking
    // the cursor back onto its own anchor leaves no *selection* but does leave an anchor,
    // and the next edit shortens the text it names (PR #258 review, blocking 1).
    let mut a = area();
    a.apply(KEY_END, 0, 0);
    a.apply(KEY_LEFT, MOD_SHIFT, 0);
    a.apply(KEY_RIGHT, MOD_SHIFT, 0);
    assert_eq!(a.selection(), None, "the cursor is back on its anchor");
    assert!(a.apply(KEY_BACKSPACE, 0, 0));
    assert_eq!(a.text(), "ab\nde\nfghi");
    assert_eq!(a.selection(), None, "and the anchor went with the character");
    // This is where it used to panic: the anchor named byte 3 of a line now 2 long.
    let e: Element<()> = text_area(&mut a, 3 * 16, 16, true, &[], None, &Theme::default());
    assert_eq!(
        fills(&e, Theme::default().selection()),
        0,
        "nothing is selected, so nothing \
            is highlighted"
    );

    // The quieter symptom of the same defect: typing instead of deleting used to leave a
    // selection over the character just typed, which the next keystroke would replace.
    let mut a = area();
    a.apply(KEY_RIGHT, MOD_SHIFT, 0);
    a.apply(KEY_LEFT, MOD_SHIFT, 0);
    a.insert('x');
    assert_eq!(a.text(), "xabc\nde\nfghi");
    assert_eq!(a.selection(), None, "typing selects nothing");

    // And by pointer, which arms it the same way: a press and a release that never moved.
    let mut a = area();
    a.place(0, 3);
    a.extend_to(0, 3);
    assert!(a.apply(KEY_BACKSPACE, 0, 0));
    assert_eq!(a.text(), "ab\nde\nfghi");
    let _: Element<()> = text_area(&mut a, 3 * 16, 16, true, &[], None, &Theme::default());
}

#[test]
fn a_press_places_the_cursor_and_a_drag_selects_from_where_it_landed() {
    // The pointer half, which the state owns because the pixel-to-cell arithmetic is the
    // application's — it knows its own metrics — and what a press *means* is not.
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
    // The coordinates come from arithmetic on a pixel position, which knows nothing about
    // encoding or line lengths. Clamping is the widget's job, not the caller's.
    let mut a = area();
    a.place(1, 99);
    assert_eq!(a.cursor(), (1, 2));
    a.place(99, 0);
    assert_eq!(a.cursor(), (2, 0), "and past the last line lands on the last line");
}

#[test]
fn enter_is_the_text_areas_and_tab_is_not() {
    // **The one widget for which Enter is text rather than submission** — that is what
    // multi-line means. Tab stays traversal's: a buffer that swallowed it would trap the
    // keyboard inside itself with no way out.
    const KEY_TAB: u16 = 15;
    let mut a = TextAreaState::new();
    assert!(a.apply(KEY_ENTER, 0, 0));
    assert_eq!(a.lines().len(), 2);
    assert!(!a.apply(KEY_TAB, 0, 0), "Tab is not claimed");
    assert_eq!(a.text(), "\n", "and it inserted nothing");
}

#[test]
fn a_control_chord_is_not_text() {
    // `to_char` folds Ctrl-C to 0x03 because a terminal needs it to; an editor that
    // inserted that would put an unprintable byte in somebody's file.
    const KEY_C: u16 = 46;
    let mut a = TextAreaState::new();
    assert!(!a.apply(KEY_C, librsproto::surface::MOD_CTRL, 0));
    assert_eq!(a.text(), "");
}

#[test]
fn a_grip_is_a_square_that_reports_its_press_rather_than_its_click() {
    // **At the press, like the title bar's drag**, because a resize is a gesture that
    // *begins* there: a grip that waited for the click would hand the compositor a drag
    // whose button was already up. And it measures its own square, so a caller placing it
    // in a corner has a number to place it by.
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
    // The discrimination the whole widget exists for. The buttons sit *above* the face in
    // the stack, so a press on one must produce its own message and not also a drag —
    // otherwise every click on close would move the window a little first.
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
    // The press and the release, kept apart: a drag is decided by the first and a click by
    // the second, and this widget carries one of each.
    let down = |r: &mut Router, x: i32| r.pointer(&tree, &e, &l, at(x, true)).0;
    let up = |r: &mut Router, x: i32| r.pointer(&tree, &e, &l, at(x, false)).0;

    // The buttons' centres in a bare 400-wide bar, from the constants — the close button ends
    // `TITLE_BUTTON_PAD` in from the right, each is `TITLE_BUTTON_W` wide, `TITLE_BUTTON_GAP`
    // apart. These were 390 and 364 in contiguous 26-pixel slots until the desktop refresh's
    // Part B, and 364 now falls in a gap, which drags.
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

    // **And a second button pressed mid-drag is not a second drag.** While a capture is held
    // the router routes to the *captured* widget, so every later press was reaching the bar
    // — including one over a button, where the shadowing rule cannot help because it walks
    // the captured node's path rather than the pointer's. A window jumped by the drag's
    // accumulated distance on each extra click (PR #248 review, blocking 1).
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

/// Every `Text` node in a tree, joined — enough to say what a widget shows without pinning
/// where in its tree it sits.
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
    // **The published metrics, and the shadowing rule between them.** `check-login` presses
    // a tab at `TAB_SIDE + i * TAB_PITCH + something` and its close box at `TAB_CLOSE_CX`
    // from its own copies, because a gate that aimed from the toolkit could agree with a
    // toolkit that had stopped drawing where it says (M11 decision 2) — so the numbers are
    // asserted here against a tree that is actually built, the way the dialog's aim points
    // are, and `xtask`'s `the_gates_chrome_table_is_the_toolkits` compares the two sets.
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

    // **Tab `i` begins at `TAB_SIDE + i * TAB_PITCH`** since the refresh's Part H: the strip
    // has padding at its left end and a pixel between tabs, where it used to start hard
    // against the content's edge with none.
    let tab_x = |i: i32| TAB_SIDE as i32 + i * TAB_PITCH as i32;
    // The second tab's label area selects it, by **key** and not by position.
    assert_eq!(click(&mut router, tab_x(1) + 20), alloc::vec![M::Select(9)]);
    // Its close box closes it and does *not* also select it: a nearer `on_press` shadows the
    // one on the tab, which is the same rule that lets a title bar carry buttons.
    assert_eq!(click(&mut router, tab_x(1) + TAB_CLOSE_CX), alloc::vec![M::Close(9)]);
    // And the first tab is still where it was, which is what a fixed width buys.
    assert_eq!(click(&mut router, tab_x(0) + 20), alloc::vec![M::Select(7)]);
    // The `+` is after the last tab, and opens one rather than selecting anything.
    assert_eq!(click(&mut router, tab_x(2) + (NEW_TAB_W / 2) as i32), alloc::vec![M::New]);
}

/// How far off an exact ink a glyph's darkest pixel may land. A stem covers a pixel almost
/// but not quite completely, so even body ink comes out a few units light.
const ANTIALIAS_SLACK: u32 = 12;

/// A focused pill shows a ring, and its label reads in either scheme.
///
/// **Two failures in one control** (PR #319 review, optionals 4 and 9). The ring was the
/// ground drawn over the ground — nought pixels of the 2400 differed between an active pill
/// and a resting one — and the label was the window's paper, which in the dark scheme is
/// near-black on the accent. Painted, and compared *between states* rather than against a
/// colour, because "a ring is visible" is exactly the claim a same-colour ring passes.
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
    // **The label is the better-contrasting of the surface's two inks, in each scheme** —
    // asserted as the *choice* rather than as a distance, because a distance passes for the
    // wrong ink too: the dark scheme's near-black paper is a long way from the accent by any
    // arithmetic and still unreadable on it (review, optional 9). In the light scheme that
    // is the paper, in the dark scheme the text colour.
    let brightness = |c: Rgb| 299 * c.r as i32 + 587 * c.g as i32 + 114 * c.b as i32;
    for t in [Theme::light(), Theme::dark()] {
        let fb = draw(&t, WidgetState::default());
        let ground = brightness(t.accent);
        let (paper, text_ink) = (brightness(t.background), brightness(t.foreground));
        let wanted = if (paper - ground).abs() >= (text_ink - ground).abs() { paper } else { text_ink };
        // The label's extreme pixel on the middle row is the ink it is drawn in.
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
///
/// **The description's central claim, and nothing tested it** (PR #319 review, worth fixing
/// 5): every one of these could be broken with all 80 of this module's tests still passing,
/// which is the same "present and invisible" lesson as the close box, one level up.
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
    // Down the middle of each tab, clear of its label and its close box.
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
    // The inactive label is dim, and the current one is not.
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
    // Hovering one of the others must not open it at the bottom: the rule stays.
    let (hover, _) = paint_strip(TabExtras::none(), Some(2));
    assert_eq!(hover.get_pixel(second, TAB_STRIP_H - 1), Some(t.border), "a hovered tab still has the rule under it");
    assert_ne!(hover.get_pixel(second, TAB_TOP + TAB_H / 2), Some(t.face_hover), "…and it does light up");
    // A keyed right-hand slot is drawable: the diff refuses a half-keyed container.
    let keyed = TabExtras { new_tab: None, right: Some(ink(t.foreground, text("848 bytes")).key(99)) };
    let (_, diffable) = paint_strip(keyed, None);
    assert!(diffable, "a caller that keys its right-hand control makes the window undiffable");
}

/// A dialog's edge follows *its* window's focus, not the fact that it is up.
///
/// **Two windows edged in the accent is the failure** (PR #319 review, blocking 1): a dialog
/// that has lost the keyboard to another window is a click away — the compositor raises any
/// listed window, and a dialog is listed — and every caller already tracks that flag for the
/// dialog's own title bar. This passed `true` unconditionally, so the edge said "the
/// keyboard is here" while the title bar beside it said otherwise.
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
/// **The inverse of every other control here**, which is the whole point of it: a button is a
/// face with an edge and ink on it, and this is the accent with the window's own ground as
/// ink. Painted, because "filled with the accent" is a statement about pixels, and because a
/// label drawn in `foreground` on an accent ground is the failure worth catching — it reads
/// as a smudge rather than as a word (desktop refresh, Part H).
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
    // The ground, sampled down the left edge and clear of the label in the middle.
    assert_eq!(fb.get_pixel(2, h / 2), Some(t.accent), "a pill is filled with the accent");
    // **Measured as lightness along the middle row**, where the pill is full width so its
    // cut corners cannot be mistaken for the label. Counting pixels at exactly `background`
    // counts almost none: a glyph at this size is mostly antialiased, which is the same
    // thing the status bar's and the greeter's tests ran into.
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
/// **The design's own focus cue** (desktop refresh, Part H), and painted rather than read off
/// the tree: the edge is the last layer over everything else, so a frame that drew it first
/// would have the right colour in the wrong place and the tree would look the same.
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
        // Down the left edge, clear of the rounded corners.
        fb.get_pixel(0, h / 2)
    };
    assert_eq!(edge(true), Some(t.accent), "the focused window is edged in the accent");
    assert_eq!(edge(false), Some(t.border), "and an unfocused one in the line colour");
    assert_ne!(t.accent, t.border);
}

/// A title bar's subtitle is beside the title, dimmer than it, and absent when there is none.
///
/// **Painted, and in that order**: the pair is a bold name and a second word that must read
/// as secondary, so what this asserts is that the darkest ink in the subtitle's half of the
/// bar is lighter than the title's — not merely that two strings are in the tree (desktop
/// refresh, Part H).
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
    // Where each word falls: the title starts at `TITLE_PAD`, and "Files" in the bold face
    // is comfortably inside 60 pixels at the default size.
    let (title_half, sub_half) = (60, 200);
    let with = draw(Some("/home"));
    let without = draw(None);
    assert!(
        darkest(&with, title_half, sub_half) < darkest(&without, title_half, sub_half),
        "the subtitle draws where there was nothing"
    );
    // **Against the dim ink itself, not merely against the title.** A first version compared
    // the two halves and passed with the subtitle in *body* ink: antialiasing left it three
    // units lighter than the title, and "darker than" was true of a difference nobody could
    // see. What matters is which ink it is, so the threshold is that ink.
    let (title_ink, sub_ink) = (darkest(&with, 0, title_half), darkest(&with, title_half, sub_half));
    assert!(
        sub_ink >= darkness(t.foreground_dim),
        "the subtitle is {sub_ink}, darker than `foreground_dim` — it is not a second read"
    );
    assert!(title_ink <= darkness(t.foreground) + ANTIALIAS_SLACK, "and the title is body ink at {title_ink}");
}

/// A status bar is a ground, a rule on the edge that faces the content, and dim readings.
///
/// **Painted**, because every part of this is a colour in a place: a bar whose rule is on the
/// wrong edge reads as a lid rather than a floor, and a reading in body ink is not a second
/// read (desktop refresh, Part H).
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
    // **Both readings are there, and neither is body ink — measured by darkness, not by the
    // exact colour.** At the small step a glyph is mostly antialiased, so counting pixels
    // equal to `foreground_dim` counts almost nothing; what separates the two inks is how
    // dark the darkest pixel gets. Dim is 91/103/102 and body is 22/32/31, so nothing drawn
    // in dim reaches the threshold and anything drawn in body sails past it.
    let darkness = |c: Rgb| c.r as u32 + c.g as u32 + c.b as u32;
    let darkest = (0..w)
        .flat_map(|x| (0..h).map(move |y| (x, y)))
        .filter_map(|(x, y)| top.get_pixel(x, y))
        .map(darkness)
        .min()
        .expect("the bar has pixels");
    // **Each end counted separately** (PR #319 review, blocking 2). One count over the whole
    // bar passes on the left reading alone, so dropping the right slot — which is where the
    // editor's line and column live — failed nothing. Each half must have ink of its own.
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
/// **`Node::Icon` measures as nothing** and paints into the rect it is handed, so any wrapper
/// that sizes to its child's measurement — `center`, and it was a `center` — gives the glyph
/// a zero rect. The tab then still *clicks* like a tab with a close box, which is why the
/// routing test above passed while the × was missing from a screendump (desktop refresh,
/// Part H). Painted, because that is the only place the difference exists.
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
    // The close box is the last `TAB_CLOSE_W` of the first tab.
    let (from, to) = (TAB_SIDE + TAB_W - TAB_CLOSE_W, TAB_SIDE + TAB_W);
    let inked = |x0: u32, x1: u32| {
        (x0..x1)
            .flat_map(|x| (0..TAB_STRIP_H).map(move |y| (x, y)))
            .filter(|(x, y)| fb.get_pixel(*x, *y) == Some(t.foreground_dim))
            .count()
    };
    assert!(inked(from, to) > 8, "the close box drew nothing");
    // And the ink is the box's, not the label's spilling into it.
    assert!(inked(to, to + TAB_GAP + 4) == 0, "and it stays inside the tab");
}

/// The current tab is outlined in `border` on three sides, and open on the fourth.
///
/// **Painted and read back** (desktop refresh, after Part K). The outline is two faces, and
/// the tree has them whether or not they come out as a line: an inner face a pixel too wide,
/// or inset on the bottom as well, leaves the tree identical and the tab unoutlined or closed.
/// So this looks at the pixels where each edge must be — and at the **resting** state of the
/// tab beside it, which must stay unoutlined, since an outline on every tab marks none.
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

    // The current tab spans `TAB_SIDE..TAB_SIDE + TAB_W`, from `TAB_TOP` to the strip's foot,
    // whose last row is the strip's rule.
    let (left, right) = (TAB_SIDE, TAB_SIDE + TAB_W - 1);
    let (mid, middle_y, rule_y) = (TAB_SIDE + TAB_W / 2, TAB_TOP + TAB_H / 2, TAB_STRIP_H - 1);
    assert_eq!(at(mid, TAB_TOP), t.border, "no line along the current tab's top");
    assert_eq!(at(left, middle_y), t.border, "no line down its left side");
    assert_eq!(at(right, middle_y), t.border, "no line down its right side");
    // **A line, not a border-coloured tab**: a pixel in, it is the window's ground again.
    assert_eq!(at(left + 1, middle_y), t.background, "the outline is more than a pixel wide");
    assert_eq!(at(mid, TAB_TOP + 1), t.background, "the outline is more than a pixel deep");
    // **And open at the bottom**: the strip's rule stops under the current tab, which is what
    // makes the tab a continuation of the content rather than a box sitting on it.
    assert_eq!(at(mid, rule_y), t.background, "the current tab is closed at the bottom");

    // The tab beside it has no outline, and the rule runs under it.
    let next = TAB_SIDE + TAB_W + TAB_GAP;
    assert_ne!(at(next, middle_y), t.border, "an inactive tab was outlined too");
    assert_eq!(at(next + TAB_W / 2, rule_y), t.border, "the rule stopped under an inactive tab");
}

/// Each title-bar button lights on hover, in its own colour, and only that one.
///
/// **Painted, and the resting state asserted first** — the state no test passed in Part H, when
/// a widget that drew nothing passed every tree test. Close lights in `deny` and the other two
/// in `face_pressed`; a hover that lit the wrong button, or all three, or lit close in the
/// neutral face, fails here.
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

    // At rest, no button has a face of either colour.
    let (fb, rects) = draw(None);
    for r in &rects {
        assert_eq!(count(&fb, *r, t.face_pressed) + count(&fb, *r, t.deny), 0, "a button lit at rest");
    }

    // Hovered, each lights alone, and in its own colour.
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
    // **Close is red, not neutral** — the one button of the three that ends something.
    let (fb, rects) = draw(Some(TITLE_CLOSE_KEY));
    assert_eq!(count(&fb, rects[2], t.face_pressed), 0, "close lit in the neutral face");
}

/// Hover reaches the keys the buttons carry **through the real router**, not only in a tree.
///
/// **The half the painted tests cannot see.** They hand `hovered` in directly, so they pass
/// for a key that no pointer could ever produce — a button keyed but laid out under a sibling
/// that the hit-test finds first, say. This moves a pointer over each target and asks the
/// router what it names, which is what an application's `view` is actually given.
///
/// **And the tab's label keeps its tab's key** — the property the close box's own key rests
/// on: a label keyed anything else would stop "the pointer is over this tab" from meaning what
/// it did, and `locate` must still find the *whole* tab under that key rather than the label.
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

/// A tab's `×` lights when the pointer is over it, and the tab stays lit around it.
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
    // The second tab — inactive — and its close box.
    let tab2 = TAB_SIDE + TAB_W + TAB_GAP;
    let (box0, box1) = (tab2 + TAB_W - TAB_CLOSE_W, tab2 + TAB_W);

    let rest = draw(None);
    assert_eq!(count(&rest, box0, box1, t.face_pressed), 0, "the `×` lit at rest");

    let hot = draw(Some(tab_close_key(2)));
    assert!(count(&hot, box0, box1, t.face_pressed) > 50, "the `×` did not light on hover");
    // **Its tab stays lit.** The pointer over the `×` names the close box rather than the tab,
    // and a tab that went dark there would flicker at exactly the moment a person is aiming.
    assert!(
        count(&hot, tab2, box0, t.face) > 100,
        "the inactive tab lost its hover face as the pointer reached its `×`"
    );
    // Nothing lights on the *other* tab's `×`.
    let other = (TAB_SIDE + TAB_W - TAB_CLOSE_W, TAB_SIDE + TAB_W);
    assert_eq!(count(&hot, other.0, other.1, t.face_pressed), 0, "the wrong `×` lit");

    // **And the `×` stays at rest while only its tab is hovered** (PR #325 review, finding 1).
    // Without this the two states above pass for a `×` that lights whenever its tab does —
    // a one-word "simplification" to `if over_tab` — which would light every tab's `×` the
    // moment the pointer crossed into it, the current tab's included.
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
/// **The faces were counted and the inks were not** (PR #325 review, finding 2): swapping the
/// close glyph to the scheme's `foreground` passed every test while shipping `#16201F` on
/// `#A4453C`, about 2.8:1. So this reads the glyph's own pixels.
///
/// **Close is pinned by its reason rather than by its colour.** The design writes `#fff`; in
/// the dark scheme that is 3.36:1 on its `#D46F63`, below what text needs, and the near-black
/// `furthest_from` picks instead is 5.25:1. So the assertion is that the glyph is whichever of
/// the scheme's two inks has the higher **WCAG** contrast on `deny`, and that it clears 4.5:1
/// — a different formula from `furthest_from`'s weighted brightness, so this is not the code
/// checking itself, and it stays right if either scheme's red is retuned.
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
        // At rest every glyph is dim, and no button holds a pixel of full ink.
        let (fb, rects) = draw(&t, None);
        for (i, r) in rects.iter().enumerate() {
            assert!(count(&fb, *r, t.foreground_dim) > 0, "{scheme}: button {i}'s glyph is not dim at rest");
            assert_eq!(count(&fb, *r, t.foreground), 0, "{scheme}: button {i} in full ink at rest");
        }

        // Minimise and maximise lit: the glyph goes to full ink, the dim one is gone.
        for (i, key) in [(0, TITLE_MINIMISE_KEY), (1, TITLE_MAXIMISE_KEY)] {
            let (fb, rects) = draw(&t, Some(key));
            assert!(count(&fb, rects[i], t.foreground) > 0, "{scheme}: button {i} lit, glyph not in full ink");
            assert_eq!(count(&fb, rects[i], t.foreground_dim), 0, "{scheme}: button {i} lit, glyph still dim");
        }

        // Close lit: the glyph is the more legible of the scheme's two inks on red.
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

/// The `+` goes to full ink when the pointer is on it — the design's own hover for it.
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
        // **The darkest pixel, not a count of exact ink.** The `+` is text, antialiased, and
        // at this size no pixel of its strokes is fully covered — so a count of pixels exactly
        // `foreground` is zero lit or not, which is how the first version of this test
        // failed for a `+` that did light. The darkest pixel is the glyph's most-covered one,
        // and which ink it sits nearer says which ink the glyph was drawn in.
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
    // The editor's unsaved mark, which is the only thing distinguishing two tabs on the same
    // file name — and the reason `Tab` carries a flag rather than the caller pre-marking the
    // string: two applications would otherwise spell the mark two ways.
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
    // **The literals the gates' `chrome` table types**, asserted against a framed window that
    // is actually built — the same guard `dialog_buttons_land_where_the_constants_say` gives
    // the dialog, and for its reason: `title_button_centre` moves with the constants, so a
    // change to the padding moved it and this crate's own tests together and left the gates
    // aiming four pixels off with nothing saying so (PR #313 review, optional 6). In a window
    // 400 wide: close 18 in from the right, maximise 50, minimise 82, all 16 down.
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
    // **The one corner a person reaches for was the one whose border was missing**: all three
    // applications stacked the grip over the finished frame (PR #313 review, optional 4). The
    // frame places it now — inside the border, as a layer before the border — and takes no
    // presses anywhere but the grip, which a full-size overlay would.
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
    // **The four numbers `check-login` types**, asserted against a tree that is actually
    // built. They were `nxedit`'s until `nxfiles` grew a second confirmation; deriving them
    // beside the frame and checking them *here* is what stops two applications and one gate
    // drifting apart in three separate places.
    //
    // The literals matter as much as the derivation. Comparing derived constants against a
    // tree built from the same constants pins nothing — both sides move together, and the
    // gate's own table is linked to neither (PR #267 review, finding 2).
    //
    // **Moved by the desktop refresh's Part B**, from (91, 249, 103): the window's content now
    // runs flush to its border (`WINDOW_FRAME` 3 → 0), so the strip is three pixels wider on
    // each side and three pixels lower. `check-login`'s `CONFIRM_*` moved in the same change.
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

    // **It measures to exactly what it declares**, which is what lets it be a `Child`:
    // `Node::Dock` measures as everything it is offered, so without the fixed wrapper this
    // is a window a thousand screens wide and `Child::open` refuses it.
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

    // **And the aim point is the button's *centre*, not merely a point inside it.** Padding
    // the strip on four sides instead of three halves the buttons, and a centre stays inside
    // a box that shrank around it — so the row's own height is bracketed here.
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
