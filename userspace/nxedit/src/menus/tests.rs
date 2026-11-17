use super::*;
use libkern::abi::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, KEY_COMPOSE, KEY_F10};
use librsproto::surface::{MOD_SHIFT, POINTER_BUTTON, POINTER_PRESSED};
use libui::diff::Tree;
use libui::layout::{FixedCell, Layout, layout, locate};
use libui::route::Router;
use libui::widget::TEXT_AREA_PAD;

const CELL: FixedCell = FixedCell { w: 8, h: 16 };

/// Eight pixels a character, as the fixed-advance face measures, and as `take_area_pointer` and
/// `place_menus` are handed here.
fn eight(s: &str) -> u32 {
    s.chars().count() as u32 * 8
}

/// The editor in its window, each event handled as the binary handles it: routed against a fresh
/// frame, the buttons told to the menus, then the document's pointer resolved.
struct Win {
    a: App,
    tree: Tree,
    r: Router,
}

impl Win {
    fn new(text: &str) -> Self {
        let mut a = App::new("/home/notes.txt", "/home");
        a.loaded(text, alloc::format!("{text}\n").as_bytes());
        Self { a, tree: Tree::new(), r: Router::new() }
    }

    fn frame(&mut self) -> (Element<Msg>, Layout) {
        let e = self.a.view(&UiTheme::default(), None);
        let s = self.a.window_size();
        let l = layout(&e, Rect::new(0, 0, s.w, s.h), &CELL);
        self.tree.update(&e, &l).expect("diffable");
        self.a.place_menus(&e, &l, eight);
        (e, l)
    }

    /// The document's top-left in the window.
    fn doc(&mut self) -> (i32, i32) {
        let (e, l) = self.frame();
        let r = locate(&e, &l, AREA_INNER_KEY).expect("the document");
        (r.origin.x, r.origin.y)
    }

    /// Where in the window the boundary before column `col` of `line` is, halfway down the row.
    fn at(&mut self, line: usize, col: usize) -> (i32, i32) {
        let (x, y) = self.doc();
        let t = &self.a.buf().text;
        let dx = TEXT_AREA_PAD.left + eight(&t.lines()[line][..col]);
        let dy = TEXT_AREA_PAD.top + (line - t.offset()) as u32 * ROW_H + ROW_H / 2;
        (x + dx as i32, y + dy as i32)
    }

    fn click(&mut self, button: u16, (x, y): (i32, i32)) {
        let held = match button {
            BTN_LEFT => 1,
            BTN_RIGHT => 2,
            _ => 4,
        };
        for pressed in [true, false] {
            let ev = PointerEvent {
                kind: POINTER_BUTTON,
                button,
                buttons: if pressed { held } else { 0 },
                flags: if pressed { POINTER_PRESSED } else { 0 },
                x,
                y,
                ..Default::default()
            };
            let (e, l) = self.frame();
            for m in self.r.pointer(&self.tree, &e, &l, ev).0 {
                self.a.update(m);
            }
            self.a.menus.buttons(ev.buttons);
            self.a.take_area_pointer(eight);
        }
    }

    fn select(&mut self, from: (usize, usize), to: (usize, usize)) {
        self.a.buf_mut().text.select_range(from, to);
    }

    fn key(&mut self, keycode: u16, modifiers: u16) {
        self.a.update(Msg::Key(KeyEvent::new(1, keycode, KEY_DOWN, modifiers)));
    }

    /// The context menu's rows, each with whether it is enabled; separators as `-`.
    fn rows(&self) -> Vec<(String, bool)> {
        let Some(m) = self.a.context_menu() else { return Vec::new() };
        m.items
            .iter()
            .map(|it| match it {
                Item::Action { label, enabled, .. } => (label.to_string(), *enabled),
                Item::Separator => (String::from("-"), false),
            })
            .collect()
    }

    fn choose(&mut self, label: &str) {
        let m = self.a.context_menu().expect("a context menu is shown");
        let msg = m
            .items
            .into_iter()
            .find_map(|it| match it {
                Item::Action { label: l, msg, enabled: true, .. } if l == label => Some(msg),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no enabled {label}"));
        self.a.update(msg);
    }
}

const ROWS: [&str; 8] = ["Undo", "Redo", "-", "Cut", "Copy", "Paste", "-", "Select All"];

#[test]
fn a_right_press_inside_a_selection_keeps_it_and_cut_takes_it() {
    let mut w = Win::new("hello world");
    w.select((0, 6), (0, 11));
    let at = w.at(0, 8);
    w.click(BTN_RIGHT, at);
    assert_eq!(w.a.buf().text.selected_text().as_deref(), Some("world"), "the selection stays");
    assert_eq!(w.a.menus.context(), Some(at), "the menu opens at the press");
    assert_eq!(w.rows().iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(), ROWS);
    w.choose("Cut");
    assert!(matches!(w.a.take_clip_request(), Some(ClipRequest::Copy(t)) if t == "world"), "Cut took the selection");
    assert_eq!(w.a.buf().text.text(), "hello ");
    assert_eq!(w.a.menus.shown(), None, "and the menu closed");
}

#[test]
fn a_right_press_outside_a_selection_moves_the_caret_there_first() {
    let mut w = Win::new("hello world");
    w.select((0, 6), (0, 11));
    let at = w.at(0, 2);
    w.click(BTN_RIGHT, at);
    assert_eq!((w.a.buf().text.selection(), w.a.buf().text.cursor()), (None, (0, 2)), "the caret moved");
    assert_eq!(w.a.menus.context(), Some(at));
    let cut = w.rows().into_iter().find(|(l, _)| l == "Cut");
    assert_eq!(cut, Some((String::from("Cut"), false)), "nothing to cut where the menu is");
    w.choose("Paste");
    assert!(matches!(w.a.take_clip_request(), Some(ClipRequest::Paste)));
    w.a.pasted("XY", 0, 1);
    assert_eq!(w.a.buf().text.text(), "heXYllo world", "pasted at the press, not where the selection was");
}

#[test]
fn a_left_press_places_the_caret_and_a_middle_press_moves_nothing() {
    let mut w = Win::new("hello world");
    w.select((0, 6), (0, 11));
    let at = w.at(0, 8);
    w.click(BTN_MIDDLE, at);
    assert_eq!(w.a.buf().text.selected_text().as_deref(), Some("world"), "a middle press");
    assert_eq!(w.a.menus.shown(), None, "and no menu");
    w.click(BTN_LEFT, at);
    assert_eq!((w.a.buf().text.selection(), w.a.buf().text.cursor()), (None, (0, 8)), "a left press");
    assert_eq!(w.a.menus.shown(), None);
}

#[test]
fn with_a_field_open_the_menus_rows_are_greyed_as_the_bars() {
    let mut w = Win::new("hello world");
    w.a.update(Msg::Find);
    assert!(w.a.field_kind().is_some(), "precondition: a field is open");
    let at = w.at(0, 3);
    w.click(BTN_RIGHT, at);
    let on: Vec<_> = w.rows().into_iter().filter(|(_, enabled)| *enabled).collect();
    assert_eq!(on, [], "every row greyed");
}

#[test]
fn the_menu_keys_open_the_menu_at_the_carets_cell() {
    for (code, mods) in [(KEY_COMPOSE, 0), (KEY_F10, MOD_SHIFT)] {
        let mut w = Win::new("hello world");
        w.a.buf_mut().text.place(0, 4);
        w.key(code, mods);
        assert_eq!(w.a.menus.shown(), None, "{code}: opened at the next frame, which measures the caret");
        let (x, y) = w.doc();
        let want = (x + (TEXT_AREA_PAD.left + 4 * 8) as i32, y + (TEXT_AREA_PAD.top + ROW_H) as i32);
        assert_eq!(w.a.menus.context(), Some(want), "{code}: the caret cell's bottom-left");
        assert_eq!(w.rows().len(), ROWS.len());
    }
    let mut w = Win::new("hello world");
    w.key(KEY_F10, 0);
    w.frame();
    assert_eq!(w.a.menus.shown(), None, "F10 alone is not the key");
}

#[test]
fn the_menu_key_brings_a_caret_scrolled_away_back_first() {
    let text: Vec<String> = (0..200).map(|i| alloc::format!("line {i}")).collect();
    let mut w = Win::new(&text.join("\n"));
    w.a.buf_mut().text.place(150, 2);
    w.frame();
    let visible = w.a.buf().text.offset()..;
    assert!(visible.contains(&150) && w.a.buf().text.offset() > 0, "precondition: the caret followed");
    w.a.update(Msg::AreaWheel(PointerEvent { wheel: -100, ..Default::default() }));
    w.frame();
    assert_eq!(w.a.buf().text.offset(), 0, "precondition: wheeled away from the caret");
    w.key(KEY_COMPOSE, 0);
    w.frame();
    let (x, y) = w.doc();
    let row = (150 - w.a.buf().text.offset()) as u32;
    let want = (x + (TEXT_AREA_PAD.left + 2 * 8) as i32, y + (TEXT_AREA_PAD.top + (row + 1) * ROW_H) as i32);
    assert_eq!(w.a.menus.context(), Some(want), "at the caret, in view again");
}

#[test]
fn an_older_context_menus_popup_draws_no_rows() {
    fn texts<M>(e: &Element<M>, out: &mut Vec<String>) {
        if let libui::element::Node::Text(t) = &e.node {
            out.push(t.to_string());
        }
        for c in e.children() {
            texts(c, out);
        }
    }
    let mut w = Win::new("hello world");
    let at = w.at(0, 2);
    w.click(BTN_RIGHT, at);
    let old = w.a.menus.shown().expect("a menu");
    let at = w.at(0, 8);
    w.click(BTN_RIGHT, at);
    assert_ne!(w.a.menus.shown(), Some(old), "precondition: a second menu replaced it");
    let mut drawn = Vec::new();
    texts(&w.a.menu_view(old, &UiTheme::default(), None), &mut drawn);
    assert!(!drawn.iter().any(|t| ROWS.contains(&t.as_str()) && t != "-"), "the old popup drew {drawn:?}");
}

/// `check-login` walks the menu to `Paste` by this key.
#[test]
fn the_gate_finds_paste_where_it_looks() {
    let mut w = Win::new("hello");
    let at = w.at(0, 1);
    w.click(BTN_RIGHT, at);
    let m = w.a.context_menu().expect("a menu");
    let i = m.items.iter().position(|it| matches!(it, Item::Action { label, .. } if label == "Paste"));
    assert_eq!(i.map(|i| MENU_ROW_KEY + i as u64), Some(105));
}
