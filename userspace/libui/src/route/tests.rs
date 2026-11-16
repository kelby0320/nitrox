use super::*;
use crate::element::{column, custom, row, sized, stack, text};
use crate::layout::{FixedCell, layout};
use alloc::vec;
use libdraw::geom::Size;
use librsproto::surface::{POINTER_ENTER, POINTER_LEAVE, POINTER_MOTION};

const CELL: FixedCell = FixedCell { w: 8, h: 16 };
const SCREEN: Rect = Rect::new(0, 0, 640, 480);

#[derive(Clone, PartialEq, Eq, Debug)]
enum Msg {
    Pressed(u8),
    Key(KeyEvent),
    Ptr(PointerEvent),
    Wheel(PointerEvent),
}

fn build(e: &Element<Msg>) -> (Tree, Layout) {
    let l = layout(e, SCREEN, &CELL);
    let mut t = Tree::new();
    t.update(e, &l).expect("a clean frame");
    (t, l)
}

#[test]
fn hovered_key_reports_the_applications_key_and_not_the_trees_id() {
    // **Two id spaces, and the bug is that mixing them compiles**: a key far outside the tree's
    // counter, on a widget deep enough that its id is not 1.
    const FAR: u64 = 9_000;
    let e: Element<Msg> = column(alloc::vec![
        sized(Size::new(640, 100), text("above")).key(FAR - 1),
        sized(Size::new(640, 100), stack(alloc::vec![text("target")])).key(FAR),
    ]);
    let (t, l) = build(&e);
    let mut r = Router::new();
    r.pointer(&t, &e, &l, motion(10, 150));

    let id = r.inside().expect("the cursor is inside something");
    assert_ne!(id, FAR, "the tree's id happened to equal the key — pick a different key");
    assert_eq!(r.hovered_key(&t), Some(FAR), "the key, not the id");

    assert!(id != FAR, "the hit was a descendant, which is the case worth covering");
}

#[test]
fn nothing_is_hovered_once_the_cursor_leaves() {
    let e: Element<Msg> = sized(Size::new(100, 100), text("x")).key(7);
    let (t, l) = build(&e);
    let mut r = Router::new();
    r.pointer(&t, &e, &l, motion(10, 10));
    assert_eq!(r.hovered_key(&t), Some(7));
    r.pointer(&t, &e, &l, motion(400, 400));
    assert_eq!(r.hovered_key(&t), None, "a highlight that never clears is a stuck highlight");
}

fn press(x: i32, y: i32) -> PointerEvent {
    PointerEvent { kind: POINTER_BUTTON, button: 0x110, buttons: 1, flags: POINTER_PRESSED, x, y, ..Default::default() }
}

fn release(x: i32, y: i32) -> PointerEvent {
    PointerEvent { kind: POINTER_BUTTON, button: 0x110, buttons: 0, flags: 0, x, y, ..Default::default() }
}

fn motion(x: i32, y: i32) -> PointerEvent {
    PointerEvent { kind: POINTER_MOTION, buttons: 1, x, y, ..Default::default() }
}

#[test]
fn one_element_may_carry_both_a_press_down_and_a_click() {
    let ui: Element<Msg> = sized(Size::new(640, 40), custom(1, Size::new(640, 40)))
        .on_press_down(Msg::Pressed(1))
        .on_press(Msg::Pressed(2));
    let (t, l) = build(&ui);
    let mut r = Router::new();

    let (down, _) = r.pointer(&t, &ui, &l, press(10, 10));
    assert_eq!(down, vec![Msg::Pressed(1)], "the press fires the gesture, not the click");

    let (up, _) = r.pointer(&t, &ui, &l, release(10, 10));
    assert_eq!(up, vec![Msg::Pressed(2)], "and the release inside fires the click");

    let (down, _) = r.pointer(&t, &ui, &l, press(10, 10));
    assert_eq!(down, vec![Msg::Pressed(1)]);
    let (up, _) = r.pointer(&t, &ui, &l, release(10, 400));
    assert!(up.is_empty(), "a release outside the widget is not a click");
}

#[test]
fn a_border_painted_over_a_window_does_not_take_its_presses() {
    let ui: Element<Msg> = crate::element::stack(vec![
        sized(Size::new(640, 40), custom(1, Size::new(640, 40))).on_press(Msg::Pressed(1)),
        crate::element::outline(libdraw::format::Rgb::BLACK, 8),
    ]);
    let (t, l) = build(&ui);
    let mut r = Router::new();
    let _ = r.pointer(&t, &ui, &l, press(10, 10));
    let (up, _) = r.pointer(&t, &ui, &l, release(10, 10));
    assert_eq!(up, vec![Msg::Pressed(1)], "the press reached the content under the border");
}

#[test]
fn a_drop_is_routed_by_position_like_a_press() {
    let ui: Element<Msg> = column(vec![
        sized(Size::new(640, 40), custom(1, Size::new(640, 40))),
        sized(Size::new(640, 200), custom(2, Size::new(640, 200))).on_drop(Msg::Pressed(9)),
    ]);
    let (t, l) = build(&ui);
    let r = Router::new();

    assert_eq!(r.drop_at(&t, &ui, &l, 20, 100), Some(Msg::Pressed(9)), "inside the region");
    assert_eq!(r.drop_at(&t, &ui, &l, 20, 10), None, "on the chrome above it");
    assert_eq!(r.drop_at(&t, &ui, &l, 20, 400), None, "past everything");
}

#[test]
fn a_drops_handler_may_be_on_an_ancestor_of_what_it_landed_on() {
    let ui: Element<Msg> = stack(vec![row(vec![text("a line"), text("another")])]).on_drop(Msg::Pressed(4));
    let (t, l) = build(&ui);
    let r = Router::new();
    assert_eq!(r.drop_at(&t, &ui, &l, 4, 4), Some(Msg::Pressed(4)));
}

#[test]
fn a_drop_needs_no_press_and_leaves_no_capture() {
    let ui: Element<Msg> = sized(Size::new(640, 200), custom(1, Size::new(640, 40))).on_drop(Msg::Pressed(1));
    let (t, l) = build(&ui);
    let r = Router::new();
    assert_eq!(r.drop_at(&t, &ui, &l, 5, 5), Some(Msg::Pressed(1)));
    assert_eq!(r.capture(), None, "a drop opens no capture");
    assert_eq!(r.inside(), None, "and synthesises no crossing");
}

fn key(code: u16) -> KeyEvent {
    KeyEvent::new(1, code, 1, 0)
}

#[test]
fn clicking_a_button_does_not_take_the_keyboard_from_the_focused_widget() {
    use crate::widget::{WidgetState, button};

    let p = crate::paint::Theme::default();
    let e: Element<Msg> = column(vec![
        button("Menu", Msg::Pressed(1), WidgetState::default(), &p),
        custom(7, Size::new(0, 0)).on_key(|k| Some(Msg::Key(k))).focusable(),
    ]);
    let (t, l) = build(&e);
    let mut r = Router::new();

    let grid = t.root().unwrap().children[1].id;
    r.focus(&t, &e, grid);
    assert_eq!(r.key(&t, &e, key(30)), Some(Msg::Key(key(30))), "the grid has the keyboard");

    let btn = l.children[0].rect;
    let (at_x, at_y) = (btn.origin.x + btn.size.w as i32 / 2, btn.origin.y + btn.size.h as i32 / 2);
    let (_, _) = r.pointer(&t, &e, &l, press(at_x, at_y));
    let (msgs, _) = r.pointer(&t, &e, &l, release(at_x, at_y));
    assert_eq!(msgs, vec![Msg::Pressed(1)], "the button still fires");
    assert_eq!(r.key(&t, &e, key(31)), Some(Msg::Key(key(31))), "and the grid still has the keyboard");
}

#[test]
fn a_press_on_a_button_in_a_padded_column_produces_its_message() {
    use crate::element::Insets;
    use crate::element::padding;
    use crate::widget::{WidgetState, button};

    let p = crate::paint::Theme::default();
    let e: Element<Msg> = stack(vec![
        crate::element::fill(p.face),
        padding(
            Insets::all(2),
            column(vec![
                button("Clear", Msg::Pressed(1), WidgetState::default(), &p),
                button("Reset", Msg::Pressed(2), WidgetState::default(), &p),
            ]),
        ),
    ]);
    let size = crate::layout::measure(&e, crate::layout::Constraints::loose(SCREEN.size), &CELL);
    let bounds = Rect::new(0, 0, size.w, size.h);
    let l = layout(&e, bounds, &CELL);
    let mut t = Tree::new();
    t.update(&e, &l).expect("a clean frame");

    let mut r = Router::new();
    let at = (bounds.size.w as i32 / 2, bounds.size.h as i32 / 4);
    let (_, hit) = r.pointer(&t, &e, &l, press(at.0, at.1));
    assert!(hit.is_some(), "something was hit at {at:?} in {bounds:?}");
    let (msgs, _) = r.pointer(&t, &e, &l, release(at.0, at.1));
    assert_eq!(msgs, vec![Msg::Pressed(1)], "the first item's message, at {at:?}");
}

#[test]
fn a_click_hits_the_topmost_widget_not_the_one_underneath() {
    let e: Element<Msg> = stack(vec![
        custom(1, Size::new(0, 0)).on_pointer(Msg::Ptr),
        sized(Size::new(40, 40), custom(2, Size::new(0, 0)).on_pointer(Msg::Ptr)),
    ]);
    let (t, l) = build(&e);
    let mut r = Router::new();

    let (_, hit) = r.pointer(&t, &e, &l, press(10, 10));
    let overlay = t.root().unwrap().children[1].children[0].id;
    assert_eq!(hit, Some(overlay), "the overlay, not the base layer");

    let mut r = Router::new();
    let (_, hit) = r.pointer(&t, &e, &l, press(100, 100));
    let base = t.root().unwrap().children[0].id;
    assert_eq!(hit, Some(base));
}

#[test]
fn a_press_captures_and_a_drag_that_leaves_still_reports_to_it() {
    let e: Element<Msg> = row(vec![
        sized(Size::new(50, 480), custom(1, Size::new(0, 0)).on_pointer(Msg::Ptr)),
        custom(2, Size::new(0, 0)).on_pointer(Msg::Ptr).flex(1),
    ]);
    let (t, l) = build(&e);
    let left = t.root().unwrap().children[0].children[0].id;
    let mut r = Router::new();

    let (_, hit) = r.pointer(&t, &e, &l, press(10, 10));
    assert_eq!(hit, Some(left));
    assert_eq!(r.capture(), Some(left));

    let (_, hit) = r.pointer(&t, &e, &l, motion(400, 10));
    assert_eq!(hit, Some(left), "still the widget that took the press");

    let (_, hit) = r.pointer(&t, &e, &l, release(400, 10));
    assert_eq!(hit, Some(left));
    assert_eq!(r.capture(), None, "and the capture ends");
}

#[test]
fn releasing_outside_the_widget_is_a_cancel_rather_than_a_click() {
    let e: Element<Msg> = row(vec![
        sized(Size::new(50, 480), custom(1, Size::new(0, 0)).on_press(Msg::Pressed(1))),
        custom(2, Size::new(0, 0)).flex(1),
    ]);
    let (t, l) = build(&e);
    let mut r = Router::new();

    let (msgs, _) = r.pointer(&t, &e, &l, press(10, 10));
    assert!(msgs.is_empty(), "a press is not yet a click");
    let (msgs, _) = r.pointer(&t, &e, &l, release(400, 10));
    assert!(msgs.is_empty(), "released outside: cancelled");

    let mut r = Router::new();
    r.pointer(&t, &e, &l, press(10, 10));
    let (msgs, _) = r.pointer(&t, &e, &l, release(20, 20));
    assert_eq!(msgs, [Msg::Pressed(1)]);
}

#[test]
fn a_click_lands_on_a_widget_away_from_the_window_origin() {
    let e: Element<Msg> = row(vec![
        sized(Size::new(500, 480), custom(1, Size::new(0, 0))),
        sized(Size::new(100, 480), custom(2, Size::new(0, 0)).on_press(Msg::Pressed(2))),
    ]);
    let (t, l) = build(&e);
    let target = t.root().unwrap().children[1].children[0].id;
    assert_eq!(
        layout_at(&l, &path_to_id(t.root(), target).unwrap()).unwrap().rect,
        Rect::new(500, 0, 100, 480),
        "the premise: this widget is nowhere near the origin",
    );

    let mut r = Router::new();
    let (_, hit) = r.pointer(&t, &e, &l, press(550, 10));
    assert_eq!(hit, Some(target), "the press landed on it");
    let (msgs, _) = r.pointer(&t, &e, &l, release(550, 10));
    assert_eq!(msgs, [Msg::Pressed(2)], "released inside: a click");

    let mut r = Router::new();
    r.pointer(&t, &e, &l, press(550, 10));
    let (msgs, _) = r.pointer(&t, &e, &l, release(50, 10));
    assert!(msgs.is_empty(), "released back over the spacer: cancelled");
}

#[test]
fn only_the_capturing_buttons_release_is_a_click() {
    let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).on_press(Msg::Pressed(1)).flex(1)]);
    let (t, l) = build(&e);
    let mut r = Router::new();

    let (msgs, _) = r.pointer(&t, &e, &l, press(10, 10));
    assert!(msgs.is_empty());

    let right_down = PointerEvent {
        kind: POINTER_BUTTON,
        button: 0x111,
        buttons: 3,
        flags: POINTER_PRESSED,
        x: 10,
        y: 10,
        ..Default::default()
    };
    let right_up =
        PointerEvent { kind: POINTER_BUTTON, button: 0x111, buttons: 1, flags: 0, x: 10, y: 10, ..Default::default() };
    let (msgs, _) = r.pointer(&t, &e, &l, right_down);
    assert!(msgs.is_empty(), "a second press is not a click");
    let (msgs, _) = r.pointer(&t, &e, &l, right_up);
    assert!(msgs.is_empty(), "and neither is its release: {msgs:?}");

    let (msgs, _) = r.pointer(&t, &e, &l, release(10, 10));
    assert_eq!(msgs, [Msg::Pressed(1)], "exactly one click");
}

#[test]
fn a_key_goes_to_the_focused_widget_not_the_one_under_the_cursor() {
    let e: Element<Msg> = column(vec![
        custom(1, Size::new(0, 0)).on_key(|k| Some(Msg::Key(k))).flex(1),
        custom(2, Size::new(0, 0)).on_key(|k| Some(Msg::Key(k))).flex(1),
    ]);
    let (t, l) = build(&e);
    let second = t.root().unwrap().children[1].id;
    let mut r = Router::new();
    assert!(r.focus(&t, &e, second));

    let (_, hit) = r.pointer(&t, &e, &l, motion(10, 10));
    assert_ne!(hit, Some(second));
    assert_eq!(r.key(&t, &e, key(30)), Some(Msg::Key(key(30))));
    assert_eq!(r.focused(), Some(second));
}

#[test]
fn an_unhandled_key_bubbles_to_an_ancestor() {
    let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).focusable()]).on_key(|k| Some(Msg::Key(k)));
    let (t, l) = build(&e);
    let _ = l;
    let leaf = t.root().unwrap().children[0].id;
    let mut r = Router::new();
    assert!(r.focus(&t, &e, leaf));
    assert_eq!(r.key(&t, &e, key(1)), Some(Msg::Key(key(1))), "the parent took it");
}

#[test]
fn a_focused_widget_can_decline_a_key_and_let_it_bubble() {
    let e: Element<Msg> =
        column(vec![custom(1, Size::new(0, 0)).on_key(|k| (k.keycode != 24).then_some(Msg::Key(k))).flex(1)])
            .on_key(|k| Some(Msg::Pressed(k.keycode as u8)));
    let (t, _) = build(&e);
    let leaf = t.root().unwrap().children[0].id;
    let mut r = Router::new();
    assert!(r.focus(&t, &e, leaf));

    assert_eq!(r.key(&t, &e, key(30)), Some(Msg::Key(key(30))), "an ordinary key stays");
    assert_eq!(r.key(&t, &e, key(24)), Some(Msg::Pressed(24)), "the declined one reached the ancestor");
}

#[test]
fn a_key_nothing_claims_returns_none() {
    let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).on_key(|_| None).flex(1)]);
    let (t, _) = build(&e);
    let leaf = t.root().unwrap().children[0].id;
    let mut r = Router::new();
    assert!(r.focus(&t, &e, leaf));
    assert_eq!(r.key(&t, &e, key(30)), None);
}

#[test]
fn a_key_with_nothing_focused_is_dropped() {
    let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).on_key(|k| Some(Msg::Key(k)))]);
    let (t, _) = build(&e);
    let r = Router::new();
    assert_eq!(r.focused(), None);
    assert_eq!(r.key(&t, &e, key(30)), None);
}

#[test]
fn tab_traverses_focusable_widgets_in_tree_order_and_wraps() {
    let e: Element<Msg> = column(vec![
        custom(1, Size::new(0, 0)).focusable(),
        custom(2, Size::new(0, 0)), // skipped: not focusable
        custom(3, Size::new(0, 0)).focusable(),
    ]);
    let (t, _) = build(&e);
    let kids = &t.root().unwrap().children;
    let (a, c) = (kids[0].id, kids[2].id);
    let mut r = Router::new();

    assert_eq!(r.focus_next(&t, &e), Some(a));
    assert_eq!(r.focus_next(&t, &e), Some(c), "the middle one is skipped");
    assert_eq!(r.focus_next(&t, &e), Some(a), "and it wraps");
    assert_eq!(r.focus_prev(&t, &e), Some(c), "backwards wraps too");
}

#[test]
fn focus_cannot_be_given_to_a_widget_tab_could_never_reach() {
    let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0))]);
    let (t, _) = build(&e);
    let leaf = t.root().unwrap().children[0].id;
    let mut r = Router::new();
    assert!(!r.focus(&t, &e, leaf));
    assert_eq!(r.focused(), None);
}

#[test]
fn window_focus_and_widget_focus_are_two_fields_from_two_sources() {
    let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).focusable()]);
    let (t, _) = build(&e);
    let leaf = t.root().unwrap().children[0].id;
    let mut r = Router::new();
    r.focus(&t, &e, leaf);
    assert!(r.focus_of(leaf).is_active());

    r.set_window_focused(false);
    assert!(!r.focus_of(leaf).is_active(), "stops blinking");
    assert_eq!(r.focused(), Some(leaf), "but does not forget where it was");

    r.set_window_focused(true);
    assert!(r.focus_of(leaf).is_active(), "and comes back to the same widget");
}

#[test]
fn a_click_focuses_a_focusable_widget_and_leaves_focus_alone_otherwise() {
    let e: Element<Msg> = row(vec![
        sized(Size::new(50, 480), custom(1, Size::new(0, 0)).focusable()),
        custom(2, Size::new(0, 0)).flex(1), // not focusable
    ]);
    let (t, l) = build(&e);
    let focusable = t.root().unwrap().children[0].children[0].id;
    let mut r = Router::new();

    r.pointer(&t, &e, &l, press(10, 10));
    r.pointer(&t, &e, &l, release(10, 10));
    assert_eq!(r.focused(), Some(focusable));

    r.pointer(&t, &e, &l, press(400, 10));
    assert_eq!(r.focused(), Some(focusable));
}

/// A gesture survives the widget under it being **re-identified** between press and release.
///
/// The press establishes hover, so the next frame draws the control with more layers and
/// unkeyed children get fresh ids; a capture holding the deepest node would name one the tree
/// no longer has. **The control itself never moved**: capturing the node that carries the
/// handler survives.
#[test]
fn a_gesture_survives_the_widget_under_it_being_re_identified() {
    fn control(lit: bool) -> Element<Msg> {
        let mut layers = vec![];
        if lit {
            layers.push(text("ring"));
            layers.push(text("bevel"));
        }
        layers.push(sized(Size::new(200, 40), text("File")));
        stack(layers).on_press(Msg::Pressed(1)).key(7)
    }

    let rest: Element<Msg> = column(vec![control(false)]);
    let (mut t, l) = build(&rest);
    let mut r = Router::new();

    let before: Vec<u64> = ids(&t);
    assert!(r.pointer(&t, &rest, &l, press(10, 10)).0.is_empty(), "a press is not a click");

    let lit: Element<Msg> = column(vec![control(true)]);
    let l2 = layout(&lit, SCREEN, &CELL);
    t.update(&lit, &l2).expect("a clean frame");
    assert_ne!(ids(&t), before, "the highlight did not re-identify anything, so this test proves nothing");

    let (msgs, _) = r.pointer(&t, &lit, &l2, release(10, 10));
    assert_eq!(msgs, vec![Msg::Pressed(1)], "the click was dropped by the re-identification");
}

fn ids(t: &Tree) -> Vec<u64> {
    fn walk(w: &Widget, out: &mut Vec<u64>) {
        out.push(w.id);
        for c in &w.children {
            walk(c, out);
        }
    }
    let mut out = Vec::new();
    if let Some(root) = t.root() {
        walk(root, &mut out);
    }
    out
}

#[test]
fn focus_and_capture_are_dropped_when_their_widgets_go() {
    let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).focusable().on_pointer(Msg::Ptr).flex(1)]);
    let (mut t, l) = build(&e);
    let leaf = t.root().unwrap().children[0].id;
    let mut r = Router::new();
    r.focus(&t, &e, leaf);
    r.pointer(&t, &e, &l, press(10, 10));
    assert_eq!(r.capture(), Some(leaf));

    let e2: Element<Msg> = column(vec![text("nothing here")]);
    let l2 = layout(&e2, SCREEN, &CELL);
    t.update(&e2, &l2).expect("ok");
    r.pointer(&t, &e2, &l2, motion(10, 10));
    assert_eq!(r.focused(), None);
    assert_eq!(r.capture(), None);
}

#[test]
fn tab_from_a_stale_focus_starts_from_the_beginning_rather_than_refusing() {
    let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).focusable()]);
    let (t, _) = build(&e);
    let mut r = Router::new();
    assert_eq!(r.focus_next(&t, &e), Some(t.root().unwrap().children[0].id));
}

#[test]
fn a_window_with_nothing_focusable_clears_focus_rather_than_pointing_at_a_ghost() {
    let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).focusable()]);
    let (t, _) = build(&e);
    let mut r = Router::new();
    r.focus_next(&t, &e);
    assert!(r.focused().is_some());

    let e2: Element<Msg> = column(vec![custom(1, Size::new(0, 0))]);
    let (t2, _) = build(&e2);
    assert_eq!(r.focus_next(&t2, &e2), None);
    assert_eq!(r.focused(), None);
}

#[test]
fn the_compositors_window_crossing_becomes_one_widget_crossing_not_two() {
    let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).on_pointer(Msg::Ptr).flex(1)]);
    let (t, l) = build(&e);
    let leaf = t.root().unwrap().children[0].id;
    let mut r = Router::new();

    let enter = PointerEvent { kind: POINTER_ENTER, x: 5, y: 5, ..Default::default() };
    let (msgs, _) = r.pointer(&t, &e, &l, enter);
    assert_eq!(msgs.len(), 1, "one crossing, not two: {msgs:?}");
    let Msg::Ptr(got) = &msgs[0] else { panic!("a pointer message") };
    assert_eq!(got.kind, POINTER_ENTER);
    assert_eq!(r.inside(), Some(leaf));
    assert_eq!(r.capture(), None, "a crossing is not a press");

    let leave = PointerEvent { kind: POINTER_LEAVE, x: 5, y: 5, ..Default::default() };
    let (msgs, _) = r.pointer(&t, &e, &l, leave);
    assert_eq!(msgs.len(), 1);
    let Msg::Ptr(got) = &msgs[0] else { panic!("a pointer message") };
    assert_eq!(got.kind, POINTER_LEAVE);
    assert_eq!(r.inside(), None);
}

#[test]
fn moving_between_widgets_leaves_one_before_entering_the_next() {
    let e: Element<Msg> = row(vec![
        sized(Size::new(50, 480), custom(1, Size::new(0, 0)).on_pointer(Msg::Ptr)),
        custom(2, Size::new(0, 0)).on_pointer(Msg::Ptr).flex(1),
    ]);
    let (t, l) = build(&e);
    let mut r = Router::new();

    r.pointer(&t, &e, &l, motion(10, 10));
    let (msgs, _) = r.pointer(&t, &e, &l, motion(400, 10));
    let kinds: alloc::vec::Vec<u16> = msgs
        .iter()
        .map(|m| match m {
            Msg::Ptr(p) => p.kind,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        kinds,
        [POINTER_LEAVE, POINTER_ENTER, POINTER_MOTION],
        "leave, then enter, then the motion that caused both"
    );
}

#[test]
fn crossings_are_suppressed_during_a_capture_and_re_derived_on_release() {
    let e: Element<Msg> = row(vec![
        sized(Size::new(50, 480), custom(1, Size::new(0, 0)).on_pointer(Msg::Ptr)),
        custom(2, Size::new(0, 0)).on_pointer(Msg::Ptr).flex(1),
    ]);
    let (t, l) = build(&e);
    let left = t.root().unwrap().children[0].children[0].id;
    let right = t.root().unwrap().children[1].id;
    let mut r = Router::new();

    r.pointer(&t, &e, &l, motion(10, 10));
    r.pointer(&t, &e, &l, press(10, 10));
    let (msgs, _) = r.pointer(&t, &e, &l, motion(400, 10));
    assert_eq!(r.inside(), Some(left), "still inside, though over the other widget");
    assert!(
        msgs.iter().all(|m| !matches!(m, Msg::Ptr(p) if p.kind == POINTER_LEAVE)),
        "no crossing while captured: {msgs:?}"
    );

    let (msgs, _) = r.pointer(&t, &e, &l, release(400, 10));
    assert_eq!(r.inside(), Some(right), "re-derived once the capture ends");
    let kinds: alloc::vec::Vec<u16> = msgs
        .iter()
        .filter_map(|m| match m {
            Msg::Ptr(p) if p.kind == POINTER_LEAVE || p.kind == POINTER_ENTER => Some(p.kind),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, [POINTER_LEAVE, POINTER_ENTER]);
}

#[test]
fn a_handler_gets_widget_local_coordinates() {
    let e: Element<Msg> = row(vec![
        sized(Size::new(500, 480), custom(1, Size::new(0, 0))),
        sized(Size::new(100, 480), custom(2, Size::new(0, 0)).on_pointer(Msg::Ptr)),
    ]);
    let (t, l) = build(&e);
    let mut r = Router::new();
    let (msgs, _) = r.pointer(&t, &e, &l, motion(550, 30));
    let motion_msg = msgs
        .iter()
        .find_map(|m| match m {
            Msg::Ptr(p) if p.kind == POINTER_MOTION => Some(*p),
            _ => None,
        })
        .expect("a motion");
    assert_eq!((motion_msg.x, motion_msg.y), (50, 30), "window 550 minus the widget's 500");
}

#[test]
fn widget_local_coordinates_go_negative_during_a_capture() {
    let e: Element<Msg> = row(vec![
        sized(Size::new(500, 480), custom(1, Size::new(0, 0))),
        sized(Size::new(100, 480), custom(2, Size::new(0, 0)).on_pointer(Msg::Ptr)),
    ]);
    let (t, l) = build(&e);
    let mut r = Router::new();
    r.pointer(&t, &e, &l, press(550, 10));
    let (msgs, _) = r.pointer(&t, &e, &l, motion(400, 10));
    let m = msgs
        .iter()
        .find_map(|m| match m {
            Msg::Ptr(p) if p.kind == POINTER_MOTION => Some(*p),
            _ => None,
        })
        .expect("a motion");
    assert_eq!(m.x, -100, "100px left of the captured widget's left edge");
}

fn turn(x: i32, y: i32, dz: i16) -> PointerEvent {
    PointerEvent { kind: librsproto::surface::POINTER_WHEEL, wheel: dz, x, y, ..Default::default() }
}

/// A wheel walks up to the nearest `on_wheel`, past widgets that only track the pointer.
///
/// **The whole reason the wheel is not an `on_pointer` kind**: the thing that scrolls is the
/// pane around the row, which must still see it.
#[test]
fn a_wheel_bubbles_past_a_pointer_handler_to_the_nearest_wheel_handler() {
    let e: Element<Msg> = sized(
        Size::new(400, 400),
        stack(vec![sized(Size::new(200, 200), custom(1, Size::new(0, 0))).on_pointer(Msg::Ptr)]),
    )
    .on_wheel(Msg::Wheel);
    let (t, l) = build(&e);
    let mut r = Router::new();

    let (msgs, _) = r.pointer(&t, &e, &l, turn(50, 60, 2));
    assert_eq!(msgs.len(), 1, "exactly one handler took it");
    let Msg::Wheel(p) = &msgs[0] else { panic!("the wheel handler, not the pointer one") };
    assert_eq!(p.wheel, 2);
    assert_eq!((p.x, p.y), (50, 60), "widget-local, like every other record");
}

#[test]
fn a_pointer_handler_never_receives_a_wheel() {
    let e: Element<Msg> = sized(Size::new(400, 400), custom(1, Size::new(0, 0))).on_pointer(Msg::Ptr);
    let (t, l) = build(&e);
    let mut r = Router::new();
    let (msgs, _) = r.pointer(&t, &e, &l, turn(10, 10, 1));
    assert!(msgs.is_empty(), "a tree with no wheel handler scrolls nothing: {msgs:?}");
}

#[test]
fn a_wheel_opens_no_gesture_and_moves_no_focus() {
    let e: Element<Msg> = row(vec![
        sized(Size::new(200, 480), custom(1, Size::new(0, 0)).on_key(|k| Some(Msg::Key(k)))),
        sized(Size::new(200, 480), custom(2, Size::new(0, 0))).on_press(Msg::Pressed(9)).on_wheel(Msg::Wheel),
    ]);
    let (t, l) = build(&e);
    let mut r = Router::new();
    r.pointer(&t, &e, &l, press(10, 10));
    r.pointer(&t, &e, &l, release(10, 10));
    let focused = r.focused();
    assert!(focused.is_some(), "precondition: the keyboard is somewhere");

    let (msgs, _) = r.pointer(&t, &e, &l, turn(300, 10, 1));
    assert!(matches!(msgs.as_slice(), [Msg::Wheel(_)]), "only the wheel: {msgs:?}");
    assert_eq!(r.focused(), focused, "scrolling over a widget must not take the keyboard");

    let (msgs, _) = r.pointer(&t, &e, &l, release(300, 10));
    assert!(!msgs.contains(&Msg::Pressed(9)), "a wheel became half a click: {msgs:?}");
}

fn button_event(button: u16, pressed: bool, x: i32, y: i32) -> PointerEvent {
    let held = match button {
        BTN_LEFT => 1,
        BTN_RIGHT => 2,
        _ => 4,
    };
    let (buttons, flags) = if pressed { (held, POINTER_PRESSED) } else { (0, 0) };
    PointerEvent { kind: POINTER_BUTTON, button, buttons, flags, x, y, ..Default::default() }
}

/// A press and its release by `button` at the middle of `at`, on a fresh router.
fn click_with<M: Clone>(ui: &Element<M>, at: Rect, button: u16) -> Vec<M> {
    let l = layout(ui, SCREEN, &CELL);
    let mut t = Tree::new();
    t.update(ui, &l).expect("a clean frame");
    let (x, y) = (at.origin.x + at.size.w as i32 / 2, at.origin.y + at.size.h as i32 / 2);
    let mut r = Router::new();
    let mut out = r.pointer(&t, ui, &l, button_event(button, true, x, y)).0;
    out.extend(r.pointer(&t, ui, &l, button_event(button, false, x, y)).0);
    out
}

#[test]
fn only_the_left_button_clicks_a_button_a_close_box_a_tab_or_a_row() {
    use crate::widget::{
        ListRow, ListState, TITLE_CLOSE_KEY, Tab, TabExtras, Theme, TitleButtons, WidgetState, button, list_view,
        tab_strip, title_bar,
    };
    #[derive(Clone, PartialEq, Debug)]
    enum M {
        Drag,
        Close,
        Button,
        Tab(u64),
        TabClose(u64),
        Row(u64),
    }
    let theme = Theme::default();
    let tabs = [Tab { key: 1, label: "one", marked: false }, Tab { key: 2, label: "two", marked: false }];
    let rows =
        [ListRow { key: 10, label: "a", ..Default::default() }, ListRow { key: 11, label: "b", ..Default::default() }];
    let mut state = ListState::default();
    let buttons = TitleButtons { minimise: None, maximise: None, close: Some(M::Close) };
    let ui: Element<M> = column(vec![
        sized(Size::new(640, 30), title_bar("a window", None, true, M::Drag, buttons, None, &theme)).key(100),
        sized(Size::new(120, 30), button("Go", M::Button, WidgetState::default(), &theme)).key(200),
        sized(Size::new(640, 30), tab_strip(&tabs, 1, None, M::Tab, M::TabClose, TabExtras::none(), &theme)).key(300),
        list_view(&rows, &[], &mut state, 100, 20, M::Row, None, None, None, None, &theme).key(400),
    ]);
    let l = layout(&ui, SCREEN, &CELL);
    let place = |key| crate::layout::locate(&ui, &l, key).expect("laid out");
    let bar = place(100);
    let targets = [
        (Rect::new(bar.origin.x + 10, bar.origin.y, 20, bar.size.h), M::Drag),
        (place(TITLE_CLOSE_KEY), M::Close),
        (place(200), M::Button),
        (place(2), M::Tab(2)),
        (place(11), M::Row(11)),
    ];
    for (at, want) in targets {
        // The left button reaching it is what makes the other two's silence mean something.
        assert_eq!(click_with(&ui, at, BTN_LEFT), core::slice::from_ref(&want), "the left button, at {at:?}");
        assert_eq!(click_with(&ui, at, BTN_RIGHT), [], "a right click on what sends {want:?}");
        assert_eq!(click_with(&ui, at, BTN_MIDDLE), [], "a middle click on what sends {want:?}");
    }
}

#[test]
fn a_right_press_asks_the_nearest_for_its_menu_once_at_the_press() {
    let ui: Element<Msg> = column(vec![
        sized(Size::new(640, 100), stack(vec![text("bare")])).on_context(Msg::Pressed(1)).key(1),
        sized(Size::new(640, 100), stack(vec![sized(Size::new(640, 100), text("inner")).on_context(Msg::Pressed(3))]))
            .on_context(Msg::Pressed(2))
            .key(2),
    ]);
    let (t, l) = build(&ui);
    let mut r = Router::new();
    assert_eq!(r.pointer(&t, &ui, &l, button_event(BTN_RIGHT, true, 10, 50)).0, [Msg::Pressed(1)], "walked up");
    assert_eq!(r.pointer(&t, &ui, &l, button_event(BTN_RIGHT, false, 10, 50)).0, [], "not again on the release");
    assert_eq!(r.pointer(&t, &ui, &l, button_event(BTN_RIGHT, true, 10, 150)).0, [Msg::Pressed(3)], "the nearest");
    r.pointer(&t, &ui, &l, button_event(BTN_RIGHT, false, 10, 150));
    let left = [button_event(BTN_LEFT, true, 10, 50), button_event(BTN_LEFT, false, 10, 50)];
    assert!(left.into_iter().all(|e| r.pointer(&t, &ui, &l, e).0.is_empty()), "the left button asks for no menu");
}

#[test]
fn a_right_press_inside_a_left_gesture_asks_for_no_menu() {
    let ui: Element<Msg> =
        sized(Size::new(640, 100), text("x")).on_press(Msg::Pressed(1)).on_context(Msg::Pressed(2)).key(1);
    let (t, l) = build(&ui);
    let mut r = Router::new();
    assert_eq!(r.pointer(&t, &ui, &l, button_event(BTN_LEFT, true, 10, 50)).0, []);
    let right_down = PointerEvent { buttons: 3, ..button_event(BTN_RIGHT, true, 10, 50) };
    assert_eq!(r.pointer(&t, &ui, &l, right_down).0, [], "the compositor would never dismiss this menu");
    let right_up = PointerEvent { buttons: 1, ..button_event(BTN_RIGHT, false, 10, 50) };
    assert_eq!(r.pointer(&t, &ui, &l, right_up).0, []);
    assert_eq!(r.pointer(&t, &ui, &l, button_event(BTN_LEFT, false, 10, 50)).0, [Msg::Pressed(1)], "the click");
}

#[test]
fn a_middle_click_reaches_on_middle_and_a_left_click_on_press() {
    let ui: Element<Msg> =
        sized(Size::new(640, 100), text("task")).on_press(Msg::Pressed(1)).on_middle(Msg::Pressed(2)).key(1);
    let at = Rect::new(0, 0, 640, 100);
    assert_eq!(click_with(&ui, at, BTN_MIDDLE), [Msg::Pressed(2)]);
    assert_eq!(click_with(&ui, at, BTN_LEFT), [Msg::Pressed(1)]);
    let (t, l) = build(&ui);
    let mut r = Router::new();
    r.pointer(&t, &ui, &l, button_event(BTN_MIDDLE, true, 10, 50));
    assert_eq!(r.pointer(&t, &ui, &l, button_event(BTN_MIDDLE, false, 10, 300)).0, [], "released outside: cancelled");
}

#[test]
fn every_button_still_focuses_and_still_reaches_on_pointer() {
    // A leaf, as a terminal's grid is: focus goes to the node hit, not to an ancestor.
    let ui: Element<Msg> = custom(1, Size::new(640, 100)).on_key(|k| Some(Msg::Key(k))).on_pointer(Msg::Ptr).key(1);
    for button in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE] {
        let (t, l) = build(&ui);
        let mut r = Router::new();
        let press = button_event(button, true, 10, 50);
        assert_eq!(r.pointer(&t, &ui, &l, press).0, [Msg::Ptr(press)], "button {button:#x}");
        assert!(r.focused().is_some(), "button {button:#x} took the keyboard for what it pressed");
    }
}

#[test]
fn a_press_joining_a_held_button_starts_nothing_even_with_no_capture() {
    let ui: Element<Msg> = column(vec![
        sized(Size::new(640, 100), text("x"))
            .on_press_down(Msg::Pressed(1))
            .on_press(Msg::Pressed(2))
            .on_context(Msg::Pressed(3))
            .key(1),
    ]);
    let (t, l) = build(&ui);
    let outside = |button, buttons| PointerEvent { buttons, ..button_event(button, true, 10, 600) };
    // The left button went down outside the root, so nothing captured it, and it is still held.
    let mut r = Router::new();
    assert_eq!(r.pointer(&t, &ui, &l, outside(BTN_LEFT, 1)).0, []);
    let right = PointerEvent { buttons: 3, ..button_event(BTN_RIGHT, true, 10, 50) };
    assert_eq!(r.pointer(&t, &ui, &l, right).0, [], "a menu asked for mid-gesture");
    // And the other way round: the right held, the left pressed and released over the element.
    let mut r = Router::new();
    assert_eq!(r.pointer(&t, &ui, &l, outside(BTN_RIGHT, 2)).0, []);
    let left_down = PointerEvent { buttons: 3, ..button_event(BTN_LEFT, true, 10, 50) };
    let left_up = PointerEvent { buttons: 2, ..button_event(BTN_LEFT, false, 10, 50) };
    let mut msgs = r.pointer(&t, &ui, &l, left_down).0;
    msgs.extend(r.pointer(&t, &ui, &l, left_up).0);
    assert_eq!(msgs, [], "a press-down and a click inside another button's gesture");
}

#[test]
fn a_press_joining_a_gesture_whose_widget_went_starts_nothing() {
    let first: Element<Msg> = stack(vec![text("row")]).on_press(Msg::Pressed(1)).key(1);
    let (mut t, l) = build(&first);
    let mut r = Router::new();
    r.pointer(&t, &first, &l, button_event(BTN_LEFT, true, 10, 50));
    assert!(r.grabbed(), "precondition: the left press captured the row");
    // The tree changes under the held button: another kind of node, so the row's widget is gone.
    let second: Element<Msg> =
        sized(Size::new(640, 100), custom(9, Size::new(640, 100))).on_context(Msg::Pressed(3)).key(2);
    let l2 = layout(&second, SCREEN, &CELL);
    t.update(&second, &l2).expect("a clean frame");
    let right = PointerEvent { buttons: 3, ..button_event(BTN_RIGHT, true, 10, 50) };
    assert_eq!(r.pointer(&t, &second, &l2, right).0, [], "the left button is still down");
}

#[test]
fn a_press_with_nothing_else_held_ends_a_gesture_whose_release_never_came() {
    let ui: Element<Msg> = column(vec![
        sized(Size::new(640, 100), text("a")).on_press(Msg::Pressed(1)).key(1),
        sized(Size::new(640, 100), text("b")).on_context(Msg::Pressed(3)).key(2),
    ]);
    let (t, l) = build(&ui);
    let mut r = Router::new();
    r.pointer(&t, &ui, &l, button_event(BTN_LEFT, true, 10, 50));
    // A release that still reports the button held, as a grab lost with buttons down is closed.
    r.pointer(&t, &ui, &l, PointerEvent { buttons: 1, ..button_event(BTN_LEFT, false, 10, 50) });
    assert!(r.grabbed(), "precondition: the capture outlived its gesture");
    assert_eq!(r.pointer(&t, &ui, &l, button_event(BTN_RIGHT, true, 10, 150)).0, [Msg::Pressed(3)]);
}

/// The capture is the nearest node with **any** pointer handler: counting only clicks would hand a
/// right press inside a clickable element to the outer one, which asks for no menu.
#[test]
fn a_menu_handler_inside_a_clickable_element_takes_its_own_right_press() {
    let ui: Element<Msg> = stack(vec![sized(Size::new(640, 100), text("inner")).on_context(Msg::Pressed(2))])
        .on_press(Msg::Pressed(1))
        .key(1);
    let (t, l) = build(&ui);
    let mut r = Router::new();
    assert_eq!(r.pointer(&t, &ui, &l, button_event(BTN_RIGHT, true, 10, 50)).0, [Msg::Pressed(2)]);
}

/// A middle click survives a repaint that re-identifies the part pressed, as a left click does
/// (`a_gesture_survives_the_widget_under_it_being_re_identified`).
#[test]
fn a_middle_click_survives_the_widget_under_it_being_re_identified() {
    fn task(lit: bool) -> Element<Msg> {
        let mut layers = vec![];
        if lit {
            layers.push(text("wash"));
        }
        layers.push(sized(Size::new(200, 40), text("task")));
        column(vec![stack(layers).on_middle(Msg::Pressed(1)).key(7)])
    }
    let rest = task(false);
    let (mut t, l) = build(&rest);
    let before = ids(&t);
    let mut r = Router::new();
    assert_eq!(r.pointer(&t, &rest, &l, button_event(BTN_MIDDLE, true, 10, 10)).0, []);
    let lit = task(true);
    let l2 = layout(&lit, SCREEN, &CELL);
    t.update(&lit, &l2).expect("a clean frame");
    assert_ne!(ids(&t), before, "nothing was re-identified, so this proves nothing");
    assert_eq!(r.pointer(&t, &lit, &l2, button_event(BTN_MIDDLE, false, 10, 10)).0, [Msg::Pressed(1)]);
}
