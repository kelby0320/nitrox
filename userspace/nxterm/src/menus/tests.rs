use super::*;
use crate::tests::app;
use libkern::abi::{BTN_RIGHT, KEY_COMPOSE, KEY_F1, KEY_F10};
use librsproto::surface::{MOD_SHIFT, POINTER_MOTION};
use libui::diff::Tree;
use libui::layout::{FixedCell, layout};
use libui::route::Router;

const ROWS: [&str; 7] = ["Copy", "Paste", "-", "Select All", "-", "Find", "Clear Scrollback"];

fn ptr(button: u16, kind: u16, buttons: u16, flags: u16, x: i32, y: i32) -> PointerEvent {
    PointerEvent { kind, button, buttons, flags, x, y, ..Default::default() }
}

fn key(a: &mut App, keycode: u16, modifiers: u16) {
    a.update(Msg::Key(KeyEvent::new(1, keycode, KEY_DOWN, modifiers)));
}

fn rows(a: &App) -> Vec<String> {
    let Some(m) = a.context_menu() else { return Vec::new() };
    m.items
        .iter()
        .map(|it| match it {
            Item::Action { label, .. } => label.to_string(),
            Item::Separator => alloc::string::String::from("-"),
        })
        .collect()
}

/// "hello world" with "hello" selected by a drag, as `a_drag_over_the_grid_selects…` makes it.
fn selected_hello() -> App {
    let mut a = app();
    a.feed(b"hello world");
    let (w, h) = (a.metrics.cell_w as i32, a.metrics.cell_h as i32);
    a.update(Msg::GridPointer(ptr(BTN_LEFT, POINTER_BUTTON, 1, POINTER_PRESSED, 0, h / 2)));
    a.update(Msg::GridPointer(ptr(BTN_LEFT, POINTER_MOTION, 1, 0, 5 * w, h / 2)));
    a.update(Msg::GridPointer(ptr(BTN_LEFT, POINTER_BUTTON, 0, 0, 5 * w, h / 2)));
    assert_eq!(a.grid().selected_text().as_deref(), Some("hello"), "precondition");
    a
}

#[test]
fn a_right_press_keeps_the_selection_and_opens_the_menu_at_the_press() {
    let mut a = selected_hello();
    let size = a.window_size();
    let e = a.view(&UiTheme::default(), None);
    let l = layout(&e, Rect::new(0, 0, size.w, size.h), &FixedCell { w: 8, h: 16 });
    let mut tree = Tree::new();
    tree.update(&e, &l).expect("diffable");
    let o = a.grid_origin();
    let at = (o.x + 8 * a.metrics.cell_w as i32 + 3, o.y + a.metrics.cell_h as i32 / 2);
    let mut r = Router::new();
    for (buttons, flags) in [(2, POINTER_PRESSED), (0, 0)] {
        for m in r.pointer(&tree, &e, &l, ptr(BTN_RIGHT, POINTER_BUTTON, buttons, flags, at.0, at.1)).0 {
            a.update(m);
        }
    }
    assert_eq!(a.grid().selected_text().as_deref(), Some("hello"), "the selection stays");
    assert_eq!(a.menus.context(), Some(at), "the menu opens at the press");
    assert_eq!(rows(&a), ROWS);
    a.update(Msg::Copy);
    assert_eq!(a.take_clip_request(), Some(ClipRequest::Copy(alloc::string::String::from("hello"))));
    assert!(a.take_outbox().is_empty(), "nothing reached the shell");
}

#[test]
fn the_menu_keys_open_the_menu_at_the_cursors_cell_and_type_nothing() {
    for (code, mods) in [(KEY_COMPOSE, 0), (KEY_F10, MOD_SHIFT)] {
        let mut a = app();
        a.feed(b"abc\r\nde");
        key(&mut a, code, mods);
        let (o, w, h) = (a.grid_origin(), a.metrics.cell_w as i32, a.metrics.cell_h as i32);
        assert_eq!(a.menus.context(), Some((o.x + 2 * w, o.y + 2 * h)), "{code}: below the cursor's cell, row 1 col 2");
        assert_eq!(rows(&a), ROWS, "{code}");
        assert!(a.take_outbox().is_empty(), "{code} reached the shell");
    }
    // libterm encodes no function key, so these send nothing either: what is pinned is that none
    // of them opens the menu.
    for (code, mods) in [(KEY_F10, 0), (KEY_F1 + 8, MOD_SHIFT), (KEY_F10, MOD_SHIFT | librsproto::surface::MOD_ALT)] {
        let mut a = app();
        key(&mut a, code, mods);
        assert_eq!(a.menus.shown(), None, "{code} with {mods:#x} opened the menu");
    }
}

#[test]
fn the_menu_key_follows_the_output_back_to_the_cursor_first() {
    let mut a = app();
    for i in 0..40 {
        a.feed(alloc::format!("line {i}\r\n").as_bytes());
    }
    a.update(Msg::Wheel(PointerEvent { wheel: -5, ..Default::default() }));
    assert!(a.view_line() < a.grid().top_line(), "precondition: scrolled back");
    key(&mut a, KEY_COMPOSE, 0);
    assert_eq!(a.view_line(), a.grid().top_line(), "back at the output");
    let (row, col) = a.grid().cursor();
    let (o, w, h) = (a.grid_origin(), a.metrics.cell_w as i32, a.metrics.cell_h as i32);
    assert_eq!(a.menus.context(), Some((o.x + col as i32 * w, o.y + (row as i32 + 1) * h)));
}

#[test]
fn a_right_press_ends_the_click_run() {
    let mut a = app();
    a.feed(b"hello world");
    let (w, h) = (a.metrics.cell_w as i32, a.metrics.cell_h as i32);
    let press = |a: &mut App, button: u16, held: u16, t: u64| {
        let down = PointerEvent { time_ms: t, ..ptr(button, POINTER_BUTTON, held, POINTER_PRESSED, 2 * w, h / 2) };
        a.note_pointer(&down);
        a.update(Msg::GridPointer(down));
        a.update(Msg::GridPointer(ptr(button, POINTER_BUTTON, 0, 0, 2 * w, h / 2)));
    };
    press(&mut a, BTN_LEFT, 1, 1_000);
    press(&mut a, BTN_RIGHT, 2, 1_100);
    press(&mut a, BTN_LEFT, 1, 1_200);
    assert_eq!(a.grid().selected_text(), None, "the left click after the right one selected a word");

    let mut a = app();
    a.feed(b"hello world");
    press(&mut a, BTN_LEFT, 1, 1_000);
    press(&mut a, BTN_LEFT, 1, 1_200);
    assert_eq!(a.grid().selected_text().as_deref(), Some("hello"), "the same two left clicks alone are a double");
}

#[test]
fn an_older_context_menus_popup_draws_no_rows() {
    fn texts<M>(e: &Element<M>, out: &mut Vec<alloc::string::String>) {
        if let libui::element::Node::Text(t) = &e.node {
            out.push(t.to_string());
        }
        for c in e.children() {
            texts(c, out);
        }
    }
    let mut a = app();
    key(&mut a, KEY_COMPOSE, 0);
    let old = a.menus.shown().expect("a menu");
    key(&mut a, KEY_COMPOSE, 0);
    assert_ne!(a.menus.shown(), Some(old), "precondition: a second menu replaced it");
    let mut drawn = Vec::new();
    texts(&a.menu_view(old, &UiTheme::default(), None), &mut drawn);
    assert!(!drawn.iter().any(|t| ROWS.contains(&t.as_str()) && t != "-"), "the old popup drew {drawn:?}");
}

/// `check-terminal` walks the menu to `Copy` and `Paste` by these keys.
#[test]
fn the_gate_finds_copy_and_paste_where_it_looks() {
    let mut a = app();
    key(&mut a, KEY_COMPOSE, 0);
    let m = a.context_menu().expect("a menu");
    let at = |want: &str| m.items.iter().position(|it| matches!(it, Item::Action { label, .. } if label == want));
    assert_eq!((at("Copy"), at("Paste")), (Some(0), Some(1)), "MENU_ROW_KEY + 0 and + 1");
}
