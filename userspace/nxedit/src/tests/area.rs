//! The document's pointer: where a press puts the caret, a drag's selection, and the scrollbar.

use super::*;

/// A press lands on the column the **drawn** face puts there (real faces, not a `FixedCell`).
#[test]
fn a_press_in_the_buffer_lands_where_the_mono_face_draws() {
    const DEJAVU: &[u8] = include_bytes!("../../../../assets/fonts/DejaVuSans.ttf");
    const MONO: &[u8] = include_bytes!("../../../../assets/fonts/DejaVuSansMono.ttf");
    let face = libdraw::text::Font::from_bytes(DEJAVU.to_vec())
        .expect("the vendored face")
        .with_mono(libdraw::text::Font::from_bytes(MONO.to_vec()).expect("its mono"));
    let theme = UiTheme::default();
    use libui::layout::Metrics as _;
    let m = libui::paint::FontMetrics::new(&face, theme.font_px);
    let in_buffer = libui::element::TextStyle { mono: true, ..libui::element::TextStyle::default() };
    let mut a = App::new("/home/notes.txt", "/home");
    a.loaded("iiiimmmm", b"iiiimmmm\n");
    let size = a.window_size();
    let e = a.view(&theme, None);
    let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &m);
    let area = libui::layout::locate(&e, &l, AREA_INNER_KEY).expect("the document is keyed");
    let prefix = m.text_size_as("iiiimm", in_buffer).w as i32;
    let inset = libui::widget::TEXT_AREA_PAD;
    let x = area.origin.x + inset.left as i32 + prefix;
    let y = area.origin.y + inset.top as i32 + (ROW_H / 2) as i32;
    let mut tree = libui::diff::Tree::new();
    tree.update(&e, &l).expect("diffable");
    let mut r = libui::route::Router::new();
    let p = librsproto::surface::PointerEvent {
        kind: librsproto::surface::POINTER_BUTTON,
        button: 0x110,
        buttons: 1,
        flags: librsproto::surface::POINTER_PRESSED,
        x,
        y,
        ..Default::default()
    };
    for msg in r.pointer(&tree, &e, &l, p).0 {
        a.update(msg);
    }
    a.take_area_pointer(|s| m.text_size_as(s, in_buffer).w);
    assert_eq!(a.buf().text.cursor(), (0, 6), "the caret is in the column pressed");
    let mut b = App::new("/home/notes.txt", "/home");
    b.loaded("iiiimmmm", b"iiiimmmm\n");
    for msg in r.pointer(&tree, &e, &l, p).0 {
        b.update(msg);
    }
    b.take_area_pointer(|s| m.text_size(s).w);
    assert_ne!(b.buf().text.cursor(), (0, 6), "measuring the face the buffer is *not* drawn in would have to be wrong");
}

fn area_event(a: &mut App, kind: u16, flags: u16, buttons: u16, x: i32, y: i32) {
    let theme = UiTheme::default();
    let size = a.window_size();
    let e = a.view(&theme, None);
    let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &CELL);
    let mut tree = libui::diff::Tree::new();
    tree.update(&e, &l).expect("diffable");
    let mut r = libui::route::Router::new();
    let p = librsproto::surface::PointerEvent { kind, button: 0x110, buttons, flags, x, y, ..Default::default() };
    for m in r.pointer(&tree, &e, &l, p).0 {
        a.update(m);
    }
    a.take_area_pointer(width);
}

fn area_origin(a: &mut App) -> (i32, i32) {
    let theme = UiTheme::default();
    let size = a.window_size();
    let e = a.view(&theme, None);
    let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &CELL);
    let r = libui::layout::locate(&e, &l, AREA_INNER_KEY).expect("the document is keyed");
    (r.origin.x, r.origin.y)
}

#[test]
fn a_press_places_the_caret_where_it_landed() {
    let mut a = App::new("/home/notes.txt", "/home");
    a.loaded("hello world\nsecond line", b"hello world\nsecond line\n");
    assert_eq!(a.buf().text.cursor(), (0, 0), "precondition: at the start");

    let (ox, oy) = area_origin(&mut a);
    area_event(
        &mut a,
        librsproto::surface::POINTER_BUTTON,
        librsproto::surface::POINTER_PRESSED,
        1,
        ox + 6 + 4 * 8,
        oy + 4 + ROW_H as i32,
    );
    assert_eq!(a.buf().text.cursor(), (1, 4), "the press did not move the caret");
    assert!(a.buf().text.selection().is_none(), "a press selects nothing");
}

#[test]
fn a_drag_selects_from_the_press_to_the_pointer() {
    let mut a = App::new("/home/notes.txt", "/home");
    a.loaded("hello world", b"hello world\n");
    let (ox, oy) = area_origin(&mut a);
    let y = oy + 4;
    area_event(&mut a, librsproto::surface::POINTER_BUTTON, librsproto::surface::POINTER_PRESSED, 1, ox + 6, y);
    area_event(&mut a, librsproto::surface::POINTER_MOTION, 0, 1, ox + 6 + 5 * 8, y);
    assert_eq!(a.buf().text.selected_text().as_deref(), Some("hello"), "the drag did not select from the press");

    let before = a.buf().text.cursor();
    area_event(&mut a, librsproto::surface::POINTER_MOTION, 0, 0, ox + 6, y);
    assert_eq!(a.buf().text.cursor(), before, "a hover moved the caret");
}

#[test]
fn the_document_scrollbar_scrolls_and_leaves_the_caret_alone() {
    let mut a = App::new("/home/notes.txt", "/home");
    let text: String = (0..200).map(|i| alloc::format!("line {i}\n")).collect();
    a.loaded(&text, text.as_bytes());
    let theme = UiTheme::default();
    let size = a.window_size();
    let e = a.view(&theme, None);
    let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &CELL);
    let bar = libui::layout::locate(&e, &l, AREA_BAR_KEY).expect("the bar is keyed");
    let mut tree = libui::diff::Tree::new();
    tree.update(&e, &l).expect("diffable");
    let mut r = libui::route::Router::new();

    let before = a.buf().text.cursor();
    let at = |y: i32, kind: u16, flags: u16| librsproto::surface::PointerEvent {
        kind,
        button: 0x110,
        buttons: 1,
        flags,
        x: bar.origin.x + 6,
        y,
        ..Default::default()
    };
    let mid = bar.origin.y + bar.size.h as i32 / 2;
    for m in
        r.pointer(&tree, &e, &l, at(mid, librsproto::surface::POINTER_BUTTON, librsproto::surface::POINTER_PRESSED)).0
    {
        a.update(m);
    }
    assert!(a.buf().text.offset() > 0, "a press on the bar's middle scrolled nowhere");
    let after_press = a.buf().text.offset();
    for m in r.pointer(&tree, &e, &l, at(mid + 40, librsproto::surface::POINTER_MOTION, 0)).0 {
        a.update(m);
    }
    assert!(a.buf().text.offset() > after_press, "the drag did not follow the pointer");
    assert_eq!(a.buf().text.cursor(), before, "a scrollbar drag moved the caret");
}

/// Typing brings the view back to the caret, however far it was scrolled away.
#[test]
fn typing_brings_the_view_back_to_the_caret() {
    let mut a = App::new("/home/notes.txt", "/home");
    let text: String = (0..200).map(|i| alloc::format!("line {i}\n")).collect();
    a.loaded(&text, text.as_bytes());
    let visible = (a.area_h() / ROW_H) as usize;

    a.buf_mut().text.ensure_visible(visible);
    a.buf_mut().text.scroll_to(142, visible);
    a.buf_mut().text.ensure_visible(visible);
    assert_eq!(a.buf().text.offset(), 142, "precondition: scrolled away and staying there");

    type_into(&mut a, "a");
    a.buf_mut().text.ensure_visible(visible);
    assert_eq!(a.buf().text.offset(), 0, "the character went into a line the person cannot see");

    a.buf_mut().text.scroll_to(142, visible);
    a.buf_mut().text.ensure_visible(visible);
    assert_eq!(a.buf().text.offset(), 142, "precondition: scrolled away again");
    press_key(&mut a, libkern::abi::KEY_RIGHT);
    a.buf_mut().text.ensure_visible(visible);
    assert_eq!(a.buf().text.offset(), 0, "a caret that moved was left off screen");
}

/// A document scroll survives the repaint that follows it.
#[test]
fn a_document_scroll_survives_the_next_repaint() {
    let mut a = App::new("/home/notes.txt", "/home");
    let text: String = (0..200).map(|i| alloc::format!("line {i}\n")).collect();
    a.loaded(&text, text.as_bytes());
    let theme = UiTheme::default();
    let size = a.window_size();
    let mut tree = libui::diff::Tree::new();
    let frame = |a: &mut App, tree: &mut libui::diff::Tree| {
        let e = a.view(&theme, None);
        let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &CELL);
        tree.update(&e, &l).expect("diffable");
        (e, l)
    };
    let (e, l) = frame(&mut a, &mut tree);
    let bar = libui::layout::locate(&e, &l, AREA_BAR_KEY).expect("the bar is keyed");
    let mut r = libui::route::Router::new();
    let press = librsproto::surface::PointerEvent {
        kind: librsproto::surface::POINTER_BUTTON,
        button: 0x110,
        buttons: 1,
        flags: librsproto::surface::POINTER_PRESSED,
        x: bar.origin.x + 6,
        y: bar.origin.y + bar.size.h as i32 / 2,
        ..Default::default()
    };
    for m in r.pointer(&tree, &e, &l, press).0 {
        a.update(m);
    }
    let scrolled = a.buf().text.offset();
    assert!(scrolled > 0, "the press on the bar scrolled nowhere");

    frame(&mut a, &mut tree);
    assert_eq!(a.buf().text.offset(), scrolled, "the repaint put the document back where its caret is");
}
