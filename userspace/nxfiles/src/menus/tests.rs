use super::*;
use libkern::abi::{BTN_LEFT, BTN_RIGHT, KEY_COMPOSE, KEY_F10};
use libui::diff::Tree;
use libui::layout::{FixedCell, Layout, layout, locate};
use libui::route::Router;

const CELL: FixedCell = FixedCell { w: 8, h: 16 };

/// The browser in its window, each event routed as the binary routes it: noted, routed against
/// a fresh frame, then the buttons told to the menus.
struct Window {
    a: App,
    tree: Tree,
    r: Router,
    ms: u64,
}

impl Window {
    fn new(a: App) -> Self {
        Self { a, tree: Tree::new(), r: Router::new(), ms: 1_000 }
    }

    /// One frame, as the binary's: built, laid out, and the menus placed.
    fn frame(&mut self) -> (Element<Msg>, Layout) {
        let e = self.a.view(&UiTheme::default(), None);
        let l = layout(&e, bounds(self.a.window_size()), &CELL);
        self.tree.update(&e, &l).expect("diffable");
        self.a.place_menus(&e, &l);
        (e, l)
    }

    /// Where `key` is laid out now.
    fn locate(&mut self, key: u64) -> Rect {
        let (e, l) = self.frame();
        locate(&e, &l, key).unwrap_or_else(|| panic!("{key} is not laid out"))
    }

    /// A press and its release of `button` at `(x, y)`, with `mods` held, a second after the last.
    fn click_at(&mut self, button: u16, x: i32, y: i32, mods: u16) {
        self.ms += 1_000;
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
                modifiers: mods,
                time_ms: self.ms,
                x,
                y,
                ..Default::default()
            };
            let (e, l) = self.frame();
            self.a.note_pointer(&ev);
            for m in self.r.pointer(&self.tree, &e, &l, ev).0 {
                self.a.update(m);
            }
            self.a.menus.buttons(ev.buttons);
        }
    }

    /// The same, at the middle of what `key` names.
    fn click(&mut self, button: u16, key: u64, mods: u16) {
        let r = self.locate(key);
        self.click_at(button, r.origin.x + r.size.w as i32 / 2, r.origin.y + r.size.h as i32 / 2, mods);
    }

    /// A double click on `key`'s row: two presses 100 ms apart.
    fn double_click(&mut self, key: u64) {
        self.click(BTN_LEFT, key, 0);
        self.ms -= 900;
        self.click(BTN_LEFT, key, 0);
    }

    fn key(&mut self, code: u16, mods: u16) {
        self.a.update(Msg::Key(KeyEvent::new(1, code, KEY_DOWN, mods)));
    }

    /// The labels of the context menu shown, separators as `-`.
    fn shown(&self) -> Vec<String> {
        let Some(m) = self.a.context_menu() else { return Vec::new() };
        m.items
            .iter()
            .map(|it| match it {
                Item::Action { label, .. } => label.to_string(),
                Item::Separator => String::from("-"),
            })
            .collect()
    }

    /// Choose the context menu's row labelled `label`.
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

    fn picked(&self) -> Vec<String> {
        self.a.selection().iter().map(|e| e.name.clone()).collect()
    }
}

fn row(i: u64) -> u64 {
    LIST_ROW_KEY + i
}

fn five() -> Window {
    let mut a = App::new("/home");
    a.show("/home", (0..5).map(|i| Entry::file(&alloc::format!("f{i}.txt"))).collect());
    Window::new(a)
}

const ROW: [&str; 10] = ["Open", "-", "Cut", "Copy", "Duplicate\u{2026}", "-", "Rename", "Delete", "-", "Properties"];
const FOLDER: [&str; 6] = ["New File", "New Folder", "-", "Paste", "-", "Show Hidden Files"];

#[test]
fn a_right_press_after_a_double_click_opens_nothing_and_selects_its_row_alone() {
    let mut w = five();
    w.double_click(row(0));
    assert_eq!(w.a.take_open().as_deref(), Some("/home/f0.txt"), "precondition: the double click opened f0");

    w.click(BTN_RIGHT, row(1), 0);
    assert_eq!(w.a.take_open(), None, "the right press opened what it landed on");
    assert_eq!(w.picked(), ["f1.txt"], "it selects its row alone");
    assert_eq!(w.shown(), ROW, "and shows the row's menu");
}

#[test]
fn a_right_press_inside_a_shift_range_keeps_it_and_copy_copies_it_all() {
    let mut w = five();
    w.click(BTN_LEFT, row(1), 0);
    w.click(BTN_LEFT, row(3), MOD_SHIFT);
    assert_eq!(w.picked(), ["f1.txt", "f2.txt", "f3.txt"], "precondition: a range");

    w.click(BTN_RIGHT, row(2), 0);
    assert_eq!(w.picked(), ["f1.txt", "f2.txt", "f3.txt"], "the range stays");
    w.choose("Copy");
    let (_, paths) = w.a.take_clip_push().expect("Copy pushed");
    assert_eq!(paths, ["/home/f1.txt", "/home/f2.txt", "/home/f3.txt"]);
    assert_eq!(w.a.context_menu().map(|m| m.items.len()), None, "choosing closed the menu");

    // A right press made after a Shift press counts nothing: the next Shift-click still ranges from
    // the row the range began at.
    w.click(BTN_LEFT, row(4), MOD_SHIFT);
    assert_eq!(w.picked(), ["f1.txt", "f2.txt", "f3.txt", "f4.txt"], "the anchor did not move");
}

#[test]
fn a_right_press_on_a_marked_row_keeps_the_marks_and_on_another_clears_them() {
    let mut w = five();
    w.click(BTN_LEFT, row(0), 0);
    w.click(BTN_LEFT, row(2), MOD_CTRL);
    assert_eq!(w.picked(), ["f0.txt", "f2.txt"], "precondition: two picks");

    w.click(BTN_RIGHT, row(2), 0);
    assert_eq!(w.picked(), ["f0.txt", "f2.txt"], "a marked row keeps them");
    w.click(BTN_RIGHT, row(0), 0);
    assert_eq!(w.picked(), ["f0.txt", "f2.txt"], "and so does the selected one");
    w.click(BTN_RIGHT, row(4), 0);
    assert_eq!(w.picked(), ["f4.txt"], "another row clears them");
    assert_eq!(w.shown(), ROW);
}

#[test]
fn a_right_press_opens_the_rows_menu_on_a_row_and_the_folders_below_them() {
    let mut w = five();
    let list = w.locate(LIST_KEY);
    let below = list.origin.y + list.size.h as i32 - 4;
    w.click_at(BTN_RIGHT, list.origin.x + 40, below, 0);
    assert_eq!(w.shown(), FOLDER, "below the rows");
    assert_eq!(w.a.menus.context(), Some((list.origin.x + 40, below)), "at the press");
    assert_eq!(w.picked(), ["f0.txt"], "the selection is left alone");

    let r = w.locate(row(3));
    w.click(BTN_RIGHT, row(3), 0);
    assert_eq!(w.shown(), ROW, "a row's");
    assert_eq!(w.a.menus.context(), Some((r.origin.x + r.size.w as i32 / 2, r.origin.y + r.size.h as i32 / 2)));
    w.choose("Open");
    assert_eq!(w.a.take_open().as_deref(), Some("/home/f3.txt"), "Open opens the row");
    assert_eq!(w.shown(), Vec::<String>::new(), "and closes the menu");

    let mut empty = App::new("/home");
    empty.show("/home/void", Vec::new());
    let mut w = Window::new(empty);
    let list = w.locate(LIST_KEY);
    w.click_at(BTN_RIGHT, list.origin.x + 40, list.origin.y + 10, 0);
    assert_eq!(w.shown(), FOLDER, "an empty folder's");
}

#[test]
fn a_right_press_on_the_scrollbar_opens_nothing() {
    let mut a = App::new("/big");
    a.show("/big", (0..40).map(|i| Entry::file(&alloc::format!("f{i:02}"))).collect());
    let mut w = Window::new(a);
    let list = w.locate(LIST_KEY);
    let (x, y) = (list.origin.x + list.size.w as i32 - 3, list.origin.y + list.size.h as i32 / 2);
    w.click_at(BTN_LEFT, x, y, 0);
    let paged = w.a.pane().list.offset;
    assert!(paged > 0, "precondition: the bar is under the pointer");

    w.click_at(BTN_RIGHT, x, y, 0);
    assert_eq!(w.a.menus.shown(), None, "a menu opened on the bar");
    assert_eq!(w.a.pane().list.offset, paged, "and the right button did not page it");
}

#[test]
fn the_menu_keys_open_the_selected_rows_menu_at_its_bottom_left() {
    for (code, mods) in [(KEY_COMPOSE, 0), (KEY_F10, MOD_SHIFT)] {
        let mut w = five();
        w.click(BTN_LEFT, row(2), 0);
        w.key(code, mods);
        assert_eq!(w.a.menus.shown(), None, "it waits for the row to be laid out");
        let r = w.locate(row(2));
        assert_eq!(w.shown(), ROW, "{code}");
        assert_eq!(w.a.menus.context(), Some((r.origin.x, r.bottom() as i32)), "{code}: at its bottom-left");
    }
    let mut w = five();
    w.click(BTN_LEFT, row(2), 0);
    w.key(KEY_F10, 0);
    w.frame();
    assert_eq!(w.a.menus.shown(), None, "F10 alone is not the key");
}

#[test]
fn the_menu_key_brings_a_selection_scrolled_away_back_first() {
    let mut a = App::new("/big");
    a.show("/big", (0..40).map(|i| Entry::file(&alloc::format!("f{i:02}"))).collect());
    let mut w = Window::new(a);
    w.click(BTN_LEFT, row(1), 0);
    // A frame follows the new selection once; the wheel after it is the person's own scrolling.
    w.frame();
    for _ in 0..5 {
        w.a.update(Msg::Wheel(PointerEvent { wheel: 5, ..Default::default() }));
    }
    let (e, l) = w.frame();
    assert_eq!(locate(&e, &l, row(1)), None, "precondition: the selection is scrolled away");

    w.key(KEY_COMPOSE, 0);
    let r = w.locate(row(1));
    assert_eq!(w.shown(), ROW);
    assert_eq!(w.a.menus.context(), Some((r.origin.x, r.bottom() as i32)), "from the row, in view again");
}

#[test]
fn with_nothing_selected_the_menu_key_opens_the_folders_menu_at_the_listings_top_left() {
    let mut empty = App::new("/home");
    empty.show("/home/void", Vec::new());
    let mut w = Window::new(empty);
    w.key(KEY_COMPOSE, 0);
    let list = w.locate(LIST_KEY);
    assert_eq!(w.shown(), FOLDER);
    assert_eq!(w.a.menus.context(), Some((list.origin.x, list.origin.y)));
}

/// Every row the context menus share with the bar reads, binds and is enabled as the bar's.
#[test]
fn a_shared_row_is_the_bar_rows_twin() {
    /// A row's message, label, chord, and whether it is enabled, marked and destructive.
    type Row = (Msg, String, Option<String>, bool, bool, bool);
    fn rows(m: Menu<Msg>) -> Vec<Row> {
        m.items
            .into_iter()
            .filter_map(|it| match it {
                Item::Action { label, accel, msg, enabled, marked, destructive, .. } => {
                    Some((msg, label.to_string(), accel.map(|a| a.label()), enabled, marked, destructive))
                }
                Item::Separator => None,
            })
            .collect()
    }
    // Each state a row's availability or mark turns on: nothing selected, a row selected, hidden
    // files shown.
    let mut w = five();
    let mut states = Vec::new();
    let now = |a: &App, on| -> (Option<Vec<Row>>, Vec<Row>) {
        (a.context_menu_for(on).map(rows), a.menu_table().into_iter().flat_map(rows).collect())
    };
    w.a.pane_mut().list.selected = None;
    states.push(now(&w.a, Context::Folder));
    states.push(now(&w.a, Context::Row(row(0))));
    w.click(BTN_LEFT, row(1), 0);
    states.push(now(&w.a, Context::Row(row(1))));
    w.a.update(Msg::ToggleHidden);
    states.push(now(&w.a, Context::Folder));
    let mut shared = 0;
    for (i, (menu, bar)) in states.into_iter().enumerate() {
        for r in menu.expect("a menu") {
            if let Some(b) = bar.iter().find(|b| b.0 == r.0) {
                assert_eq!(&r, b, "state {i}");
                shared += 1;
            }
        }
    }
    assert_eq!(shared, 4 + 6 + 6 + 4, "every shared row was compared: Open is a row's own");
}

#[test]
fn a_drives_menu_sends_what_its_row_and_its_button_send() {
    let mut a = App::new("/home");
    a.show("/home", Vec::new());
    let d = |name: &str, block, removable| Drive { name: String::from(name), block, removable };
    a.set_drives(alloc::vec![d("nitrox-root", 3, false), d("DATA", 5, true)]);
    let mut w = Window::new(a);

    w.click(BTN_RIGHT, DRIVE_EJECT_KEY + 5, 0);
    assert_eq!(w.shown(), ["Open", "Eject"], "the button's row's");
    w.choose("Eject");
    assert_eq!(w.a.notice.as_deref(), Some("ejecting DATA\u{2026}"), "as the button does");

    w.click(BTN_RIGHT, DRIVE_ROW_KEY + 3, 0);
    let m = w.a.context_menu().expect("the internal disk's");
    assert!(matches!(&m.items[1], Item::Action { enabled: false, .. }), "an internal disk cannot be ejected");
    w.choose("Open");
    assert_eq!(w.a.take_goto().as_deref(), Some("/storage/nitrox-root"), "as the row does");

    w.click(BTN_RIGHT, SIDEBAR_ROW_KEY, 0);
    assert_eq!(w.a.menus.shown(), None, "a place has none");
}

#[test]
fn a_drives_menu_closes_when_the_drive_goes() {
    let mut a = App::new("/home");
    a.show("/home", Vec::new());
    let d = |name: &str, block| Drive { name: String::from(name), block, removable: true };
    a.set_drives(alloc::vec![d("DATA", 5), d("STICK", 7)]);
    let mut w = Window::new(a);
    w.click(BTN_RIGHT, DRIVE_ROW_KEY + 5, 0);
    assert_eq!(w.shown(), ["Open", "Eject"], "precondition");

    w.a.set_drives(alloc::vec![d("DATA", 5)]);
    w.frame();
    assert_eq!(w.shown(), ["Open", "Eject"], "another drive's going leaves it up");
    w.a.set_drives(Vec::new());
    w.frame();
    assert_eq!(w.a.menus.shown(), None, "its own closes it");
}

#[test]
fn a_context_menu_is_driven_by_its_keys() {
    let mut w = five();
    w.click(BTN_RIGHT, row(1), 0);
    let key = |code| KeyEvent::new(1, code, KEY_DOWN, 0);
    for _ in 0..3 {
        w.a.menu_key_event(key(libkern::abi::KEY_DOWN));
    }
    w.a.menu_key_event(key(libkern::abi::KEY_ENTER));
    let (_, paths) = w.a.take_clip_push().expect("the third row down, Copy, was chosen");
    assert_eq!(paths, ["/home/f1.txt"]);
}

/// `check-login` walks a row's menu to `Properties` by this key, and reads the receipt this answers.
#[test]
fn the_gate_finds_properties_where_it_looks() {
    let mut w = five();
    w.click(BTN_RIGHT, row(4), 0);
    let m = w.a.context_menu().expect("a row's menu");
    let at = m.items.iter().position(|it| matches!(it, Item::Action { label, .. } if label == "Properties"));
    assert_eq!(at.map(|i| MENU_ROW_KEY + i as u64), Some(109));
    let r = w.locate(row(4));
    let press = (r.origin.x + r.size.w as i32 / 2, r.origin.y + r.size.h as i32 / 2);
    assert_eq!(w.a.context_shown(), Some((press, Some("f4.txt"))));
    let list = w.locate(LIST_KEY);
    w.click_at(BTN_RIGHT, list.origin.x + 9, list.bottom() as i32 - 9, 0);
    assert_eq!(w.a.context_shown(), Some(((list.origin.x + 9, list.bottom() as i32 - 9), None)));
}

#[test]
fn a_new_listing_closes_a_context_menu() {
    let mut w = five();
    w.click(BTN_RIGHT, row(4), 0);
    assert_eq!(w.shown(), ROW, "precondition");
    w.a.show("/home", (0..5).map(|i| Entry::file(&alloc::format!("g{i}.txt"))).collect());
    assert_eq!(w.a.menus.shown(), None, "its rows would act on a row it was not opened on");

    // A listing with a row 2 of its own, which a request left standing would hang a menu from.
    w.click(BTN_LEFT, row(2), 0);
    w.key(KEY_COMPOSE, 0);
    w.a.show("/elsewhere", (0..5).map(|i| Entry::file(&alloc::format!("h{i}.txt"))).collect());
    w.frame();
    assert_eq!(w.a.menus.shown(), None, "nor does a menu the key asked for open after one");
}

#[test]
fn the_popup_draws_the_context_menu_shown() {
    fn texts<M>(e: &Element<M>, out: &mut Vec<String>) {
        if let libui::element::Node::Text(t) = &e.node {
            out.push(t.to_string());
        }
        for c in e.children() {
            texts(c, out);
        }
    }
    let mut w = five();
    w.click(BTN_RIGHT, row(1), 0);
    let shown = w.a.menus.shown().expect("a menu");
    let mut drawn = Vec::new();
    texts(&w.a.menu_view(shown, &UiTheme::default(), None), &mut drawn);
    for label in ROW.iter().filter(|l| **l != "-") {
        assert!(drawn.iter().any(|t| t == label), "{label} is not drawn: {drawn:?}");
    }
    assert!(!drawn.iter().any(|t| t == "New Tab"), "the File menu was drawn instead");
}

/// `x`, the one letter these tests type.
const KEY_X: u16 = 45;

/// The rename a prompt chosen from the menu asks for, typed `x` and entered.
fn renamed(w: &mut Window) -> Option<FileOp> {
    w.choose("Rename");
    w.key(KEY_X, 0);
    w.key(libkern::abi::KEY_ENTER, 0);
    w.a.take_op()
}

#[test]
fn a_right_press_on_a_marked_row_is_the_row_its_menu_acts_on() {
    let mut w = five();
    w.click(BTN_LEFT, row(0), 0);
    w.click(BTN_LEFT, row(2), MOD_CTRL);
    w.click(BTN_RIGHT, row(2), 0);
    assert_eq!(w.picked(), ["f0.txt", "f2.txt"], "the set stays");
    w.choose("Delete");
    assert_eq!(w.a.confirming(), Some("f2.txt"), "Delete asks about the row pressed");
    w.a.update(Msg::KeepIt);
    w.click(BTN_RIGHT, row(0), 0);
    w.choose("Properties");
    assert_eq!(w.a.properties.as_ref().map(|(_, e)| e.name.as_str()), Some("f0.txt"), "and back");
    w.a.update(Msg::CloseProperties);
    w.click(BTN_RIGHT, row(2), 0);
    w.choose("Copy");
    let (_, paths) = w.a.take_clip_push().expect("Copy pushed");
    assert_eq!(paths, ["/home/f0.txt", "/home/f2.txt"], "Copy still takes the set");

    let mut w = five();
    w.click(BTN_LEFT, row(1), 0);
    w.click(BTN_LEFT, row(3), MOD_SHIFT);
    w.click(BTN_RIGHT, row(3), 0);
    assert_eq!(w.picked(), ["f1.txt", "f2.txt", "f3.txt"]);
    let to = renamed(&mut w);
    assert_eq!(to, Some(FileOp::Rename { from: "/home/f3.txt".into(), to: "/home/x".into() }), "a range's end");
}

#[test]
fn a_right_press_on_the_only_mark_makes_its_rows_available() {
    let mut w = five();
    w.click(BTN_LEFT, row(0), 0);
    w.click(BTN_LEFT, row(2), MOD_CTRL);
    w.click(BTN_LEFT, row(0), MOD_CTRL);
    assert_eq!((w.a.pane().list.selected, w.picked()), (None, alloc::vec![String::from("f2.txt")]), "precondition");
    w.click(BTN_RIGHT, row(2), 0);
    let m = w.a.context_menu().expect("its menu");
    let off = m.items.iter().filter(|it| matches!(it, Item::Action { enabled: false, .. })).count();
    assert_eq!(off, 0, "every row is available");
    w.choose("Delete");
    assert_eq!(w.a.confirming(), Some("f2.txt"));
}

#[test]
fn the_menu_key_opens_on_the_selection_the_frame_finds() {
    let mut w = five();
    w.click(BTN_LEFT, row(2), 0);
    w.key(KEY_COMPOSE, 0);
    w.key(libkern::abi::KEY_DOWN, 0);
    let r = w.locate(row(3));
    assert_eq!(w.a.context_shown(), Some(((r.origin.x, r.bottom() as i32), Some("f3.txt"))), "hung from f3, for f3");
    w.choose("Delete");
    assert_eq!(w.a.confirming(), Some("f3.txt"), "and acting on it");
}

#[test]
fn a_rows_menu_asked_for_after_its_row_is_gone_opens_nothing() {
    let mut w = five();
    let at = w.locate(row(4));
    let ev = PointerEvent {
        kind: POINTER_BUTTON,
        button: BTN_RIGHT,
        buttons: 2,
        flags: POINTER_PRESSED,
        x: at.origin.x + 10,
        y: at.origin.y + 10,
        ..Default::default()
    };
    w.a.note_pointer(&ev);
    w.a.show("/home", (0..2).map(|i| Entry::file(&alloc::format!("f{i}.txt"))).collect());
    w.a.update(Msg::ContextMenu(Context::Row(row(4))));
    assert_eq!(w.a.menus.shown(), None);
}

#[test]
fn a_right_press_outside_the_selection_moves_the_anchor_and_clears_the_strip() {
    let mut w = five();
    w.click(BTN_LEFT, row(0), 0);
    w.a.update(Msg::CopyFiles);
    assert!(w.a.notice.is_some(), "precondition: the strip says something");
    w.click(BTN_RIGHT, row(3), 0);
    assert_eq!(w.a.notice, None, "a right press clears it, as a click does");
    w.a.menus.close();
    w.click(BTN_LEFT, row(4), MOD_SHIFT);
    assert_eq!(w.picked(), ["f3.txt", "f4.txt"], "the range starts where the right press was");
}

#[test]
fn the_menu_keys_request_opens_one_menu_once() {
    let mut w = five();
    w.click(BTN_LEFT, row(1), 0);
    w.key(KEY_COMPOSE, 0);
    w.frame();
    assert!(w.a.context_menu().is_some(), "precondition");
    w.a.menu_key_event(KeyEvent::new(1, libkern::abi::KEY_ESC, KEY_DOWN, 0));
    w.frame();
    assert_eq!(w.a.menus.shown(), None, "Esc closed it, and nothing reopened it");
}

#[test]
fn an_older_context_menus_popup_draws_none_of_the_newer_ones_rows() {
    fn texts<M>(e: &Element<M>, out: &mut Vec<String>) {
        if let libui::element::Node::Text(t) = &e.node {
            out.push(t.to_string());
        }
        for c in e.children() {
            texts(c, out);
        }
    }
    let mut w = five();
    w.click(BTN_RIGHT, row(1), 0);
    let old = w.a.menus.shown().expect("a row's menu");
    let list = w.locate(LIST_KEY);
    w.click_at(BTN_RIGHT, list.origin.x + 9, list.bottom() as i32 - 9, 0);
    assert_ne!(w.a.menus.shown(), Some(old), "precondition: the folder's menu replaced it");
    let mut drawn = Vec::new();
    texts(&w.a.menu_view(old, &UiTheme::default(), None), &mut drawn);
    assert!(!drawn.iter().any(|t| FOLDER.contains(&t.as_str()) && t != "-"), "the old popup drew {drawn:?}");
}

#[test]
fn a_prompt_chosen_while_searching_takes_the_keys() {
    let mut w = five();
    w.a.update(Msg::OpenSearch);
    // `f`, which every row's name holds.
    w.key(33, 0);
    assert_eq!(w.a.search_text(), Some("f"), "precondition: a search is up");
    w.click(BTN_RIGHT, row(3), 0);
    let to = renamed(&mut w);
    assert_eq!(to, Some(FileOp::Rename { from: "/home/f3.txt".into(), to: "/home/x".into() }));
}

#[test]
fn a_right_or_middle_press_ends_the_click_run() {
    for button in [BTN_RIGHT, libkern::abi::BTN_MIDDLE] {
        let mut w = five();
        w.click(BTN_LEFT, row(1), 0);
        w.ms -= 900;
        w.click(button, row(1), 0);
        w.ms -= 900;
        w.click(BTN_LEFT, row(1), 0);
        assert_eq!(w.a.take_open(), None, "{button:#x}: the left click after it opened the file");
    }
    let mut w = five();
    w.double_click(row(1));
    assert_eq!(w.a.take_open().as_deref(), Some("/home/f1.txt"), "the same two clicks alone are a double");
}

#[test]
fn a_bar_menu_outlives_the_frame_that_closes_a_drives() {
    let mut w = five();
    w.a.update(Msg::MenuBar(0));
    w.frame();
    assert_eq!(w.a.menus.shown(), Some(Shown::Bar(0)));
}

#[test]
fn a_right_presses_point_opens_one_menu() {
    let mut w = five();
    w.click(BTN_RIGHT, row(1), 0);
    w.a.menus.close();
    w.a.update(Msg::ContextMenu(Context::Row(row(2))));
    assert_eq!(w.a.menus.shown(), None, "a message with no press of its own opened a menu at the last one's");
}
