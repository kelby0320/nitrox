use super::*;

fn labelled<M>(e: &Element<M>, key: u64) -> String {
    fn texts<M>(e: &Element<M>, out: &mut String) {
        if let libui::element::Node::Text(t) = &e.node {
            out.push_str(t);
        }
        for c in e.children() {
            texts(c, out);
        }
    }
    fn find<M>(e: &Element<M>, key: u64) -> Option<&Element<M>> {
        if e.key == Some(key) {
            return Some(e);
        }
        e.children().find_map(|c| find(c, key))
    }
    let mut out = String::new();
    if let Some(n) = find(e, key) {
        texts(n, &mut out);
    }
    out
}

fn row(i: u64) -> u64 {
    LIST_ROW_KEY + i
}

fn app() -> App {
    let mut a = App::new("/home");
    a.show(
        "/home",
        alloc::vec![Entry::file("notes.txt"), Entry::dir("work"), Entry::file("a.txt"), Entry::dir("archive"),],
    );
    a
}

/// Three consecutive frames must diff, and the frame that fails names the mistake: `MixedKeying`
/// fails at frame 0 (no window ever appears), `KeyingChanged` at frame 1 (a window that paints
/// once and goes dead).
#[test]
fn consecutive_frames_diff_and_the_failing_one_is_named() {
    use libui::diff::Tree;
    use libui::layout::{FixedCell, layout};
    let mut a = app();
    let cell = FixedCell { w: 8, h: 16 };
    let mut tree = Tree::new();
    for frame in 0..3 {
        let e = a.view(&UiTheme::default(), None);
        let l = layout(&e, bounds(a.window_size()), &cell);
        tree.update(&e, &l).unwrap_or_else(|err| panic!("frame {frame}: {err:?}"));
    }
}

fn big() -> App {
    let mut a = App::new("/big");
    let rows: Vec<Entry> = (0..40).map(|i| Entry::file(&alloc::format!("f{i:02}"))).collect();
    a.show("/big", rows);
    a
}

fn down(a: &mut App) {
    a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_DOWN, KEY_DOWN, 0)));
}

fn up(a: &mut App) {
    a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_UP, KEY_DOWN, 0)));
}

#[test]
fn the_scroll_offset_persists_so_up_moves_the_selection_and_not_the_list() {
    // `list_view` scrolls the state it is handed: a caller that keeps none re-derives the offset
    // from zero each frame, so Up would scroll the list under a stuck highlight.
    let mut a = big();
    for _ in 0..19 {
        down(&mut a);
    }
    let _ = a.view(&UiTheme::default(), None);
    let scrolled = a.list_offset();
    assert!(scrolled > 0, "precondition: 19 rows down has scrolled the list");
    assert_eq!(a.list_selected(), Some(19));

    up(&mut a);
    let _ = a.view(&UiTheme::default(), None);
    assert_eq!(
        a.list_offset(),
        scrolled,
        "the selection moved up inside the visible rows, so the list must not have moved"
    );
    assert_eq!(a.list_selected(), Some(18));
}

#[test]
fn a_listing_puts_directories_first_then_names_in_order() {
    let a = app();
    let names: Vec<&str> = a.entries().iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["archive", "work", "a.txt", "notes.txt"]);
}

#[test]
fn a_directory_is_marked_by_a_trailing_separator() {
    let a = app();
    assert_eq!(a.entries()[0].label(), "archive/");
    assert_eq!(a.entries()[2].label(), "a.txt");
}

#[test]
fn pressing_a_directory_navigates_and_pressing_a_file_opens_it() {
    let mut a = app();
    a.update(Msg::Activate(0)); // archive/
    assert_eq!(a.take_goto().as_deref(), Some("/home/archive"));
    assert_eq!(a.take_open(), None, "a directory is navigated, not opened");

    a.update(Msg::Activate(2)); // a.txt
    assert_eq!(a.take_goto(), None, "a file is not a place to go");
    assert_eq!(a.take_open().as_deref(), Some("/home/a.txt"), "it is a thing to open");
}

#[test]
fn the_strip_says_what_happened_to_the_last_row_opened() {
    let mut a = app();
    a.update(Msg::Activate(2));
    let path = a.take_open().unwrap();
    a.opened(&path, true);
    let ui: Element<Msg> = a.view(&UiTheme::default(), None);
    assert!(labelled(&ui, NOTICE_KEY).contains("opening a.txt"), "the strip says so");

    a.opened(&path, false);
    let ui: Element<Msg> = a.view(&UiTheme::default(), None);
    assert!(labelled(&ui, NOTICE_KEY).contains("could not open a.txt"));

    a.show("/home", alloc::vec![Entry::file("a.txt")]);
    let ui: Element<Msg> = a.view(&UiTheme::default(), None);
    assert_eq!(labelled(&ui, NOTICE_KEY), "", "a listing clears it");
}

const OUTSIDE: (i32, i32) = (START_SIZE.w as i32 + 10, 100);

#[test]
fn a_press_becomes_a_drag_only_after_it_has_travelled() {
    let mut a = app();
    a.update(Msg::Grab(row(2))); // a.txt
    assert_eq!(a.pointer_moved(100, 100, 1), Gesture::None, "the first motion fixes the origin");
    assert_eq!(a.pointer_moved(100 + DRAG_SLOP - 1, 100, 1), Gesture::None, "a nudge is not a drag");
    assert_eq!(a.pointer_moved(100 + DRAG_SLOP, 100, 1), Gesture::Moved, "and travelling is");

    assert_eq!(a.take_drag(), None, "an internal drag is not the compositor's");

    assert_eq!(a.pointer_moved(OUTSIDE.0, OUTSIDE.1, 1), Gesture::HandOff, "out of the window");
    let (entry, path) = a.take_drag().expect("the row that was pressed");
    assert_eq!((entry.name.as_str(), entry.is_dir), ("a.txt", false));
    assert_eq!(path, "/home/a.txt");
    assert_eq!(a.take_drag(), None, "and it is offered once");
}

#[test]
fn releasing_without_travelling_leaves_no_drag_behind() {
    let mut a = app();
    a.update(Msg::Grab(row(0)));
    assert_eq!(a.pointer_moved(10, 10, 1), Gesture::None);
    assert_eq!(a.pointer_moved(10, 10, 0), Gesture::None, "the button came up");
    assert_eq!(a.pointer_moved(400, 400, 1), Gesture::None, "a later motion carries nothing");
    assert_eq!(a.take_drag(), None);
}

#[test]
fn a_listing_that_changes_under_a_press_carries_nothing() {
    // Backspace between the press and the move changes the listing: a remembered index would name
    // another row, a name names none.
    let mut a = app();
    a.update(Msg::Grab(row(2))); // a.txt, in /home
    assert_eq!(a.pointer_moved(10, 10, 1), Gesture::None, "the press is recorded");

    a.show("/", alloc::vec![Entry::dir("bin"), Entry::dir("dev"), Entry::file("zzz")]);
    assert_eq!(a.pointer_moved(10 + DRAG_SLOP, 10, 1), Gesture::None, "the row it was on is gone");
    assert_eq!(a.pointer_moved(OUTSIDE.0, OUTSIDE.1, 1), Gesture::None, "so nothing leaves");
    assert_eq!(a.take_drag(), None, "and nothing is offered");

    let mut a = app();
    a.update(Msg::Grab(row(2))); // a.txt
    a.pointer_moved(10, 10, 1);
    a.show("/home", alloc::vec![Entry::file("a.txt"), Entry::dir("new"), Entry::dir("work")]);
    assert_eq!(a.pointer_moved(10 + DRAG_SLOP, 10, 1), Gesture::Moved);
    assert_eq!(a.pointer_moved(OUTSIDE.0, OUTSIDE.1, 1), Gesture::HandOff);
    let (entry, path) = a.take_drag().expect("the row that was pressed");
    assert_eq!(entry.name, "a.txt", "by name, not by position");
    assert_eq!(path, "/home/a.txt");
}

#[test]
fn a_directory_row_drags_as_a_directory() {
    let mut a = app();
    a.update(Msg::Grab(row(0))); // archive/
    a.pointer_moved(10, 10, 1);
    a.pointer_moved(10 + DRAG_SLOP, 10, 1);
    a.pointer_moved(OUTSIDE.0, OUTSIDE.1, 1);
    let (entry, path) = a.take_drag().expect("a drag");
    assert!(entry.is_dir, "the browser reports what the row is");
    assert_eq!(path, "/home/archive");
}

fn press_key(a: &mut App, code: u16) {
    a.update(Msg::Key(KeyEvent::new(1, code, KEY_DOWN, 0)));
}

/// `x`, the one letter these tests type, and `/`, the one they must not; both pinned below.
const KEY_X: u16 = 45;
const KEY_SLASH: u16 = 53;

#[test]
fn the_typed_keycodes_are_the_ones_the_keymap_calls_x_and_slash() {
    assert_eq!(libinput::keymap::to_char(KEY_X, 0), Some(b'x'));
    assert_eq!(libinput::keymap::to_char(KEY_SLASH, 0), Some(b'/'));
}

fn row_y(a: &App, i: u32) -> i32 {
    (a.list_top() + ROW_H * i + ROW_H / 2) as i32
}

fn select(a: &mut App, i: usize) {
    for _ in 0..a.entries().len() {
        press_key(a, libkern::abi::KEY_UP);
    }
    for _ in 0..i {
        press_key(a, libkern::abi::KEY_DOWN);
    }
}

#[test]
fn an_operation_with_nothing_selected_is_answered_rather_than_ignored() {
    // A menu row that silently does nothing looks live and is not: the three that act on a row
    // must say so with none.
    let mut a = App::new("/home");
    a.show("/home", alloc::vec![]);
    for action in [Action::Rename, Action::Duplicate, Action::Delete] {
        a.update(Msg::Choose(action));
        assert!(a.take_op().is_none(), "{action:?} did something with nothing selected");
        assert!(a.confirming().is_none(), "{action:?} asked about nothing");
        let ui: Element<Msg> = a.view(&UiTheme::default(), None);
        assert!(labelled(&ui, NOTICE_KEY).contains("nothing is selected"), "{action:?} said nothing");
    }
    a.update(Msg::Choose(Action::NewFolder));
    press_key(&mut a, KEY_X);
    press_key(&mut a, libkern::abi::KEY_ENTER);
    assert_eq!(a.take_op(), Some(FileOp::Create { path: "/home/x".into(), dir: true }));
}

#[test]
fn the_prompt_turns_what_was_typed_into_an_operation() {
    let mut a = app();
    select(&mut a, 2); // a.txt
    a.update(Msg::Choose(Action::Rename));
    let ui: Element<Msg> = a.view(&UiTheme::default(), None);
    assert_eq!(labelled(&ui, PATH_KEY), "rename to:", "the verb replaces the path");

    press_key(&mut a, KEY_X);
    press_key(&mut a, libkern::abi::KEY_ENTER);
    assert_eq!(a.take_op(), Some(FileOp::Rename { from: "/home/a.txt".into(), to: "/home/x".into() }));

    select(&mut a, 2);
    a.update(Msg::Choose(Action::Duplicate));
    press_key(&mut a, KEY_X);
    press_key(&mut a, libkern::abi::KEY_ENTER);
    assert_eq!(a.take_op(), Some(FileOp::Copy { from: "/home/a.txt".into(), to: "/home/x".into(), dir: false }));
}

#[test]
fn a_prompt_takes_a_name_and_not_a_path() {
    let mut a = app();
    a.update(Msg::Choose(Action::NewFile));
    for code in [KEY_X, KEY_SLASH, KEY_X] {
        press_key(&mut a, code);
    }
    press_key(&mut a, libkern::abi::KEY_ENTER);
    assert!(a.take_op().is_none(), "a path is not a name");
    let ui: Element<Msg> = a.view(&UiTheme::default(), None);
    assert!(labelled(&ui, NOTICE_KEY).contains("not a path"), "and it says which");
}

#[test]
fn an_empty_name_leaves_the_prompt_open_and_escape_closes_it() {
    let mut a = app();
    a.update(Msg::Choose(Action::NewFolder));
    press_key(&mut a, libkern::abi::KEY_ENTER);
    assert!(a.take_op().is_none(), "nothing typed is not a name");
    let ui: Element<Msg> = a.view(&UiTheme::default(), None);
    assert_eq!(labelled(&ui, PATH_KEY), "new folder:", "and it is still asking");

    press_key(&mut a, libkern::abi::KEY_ESC);
    let ui: Element<Msg> = a.view(&UiTheme::default(), None);
    assert_eq!(labelled(&ui, PATH_KEY), "/home", "the path is back");
    assert!(a.take_op().is_none());
}

#[test]
fn while_a_name_is_being_typed_the_keys_are_the_fields() {
    let mut a = app();
    a.update(Msg::Choose(Action::NewFile));
    press_key(&mut a, libkern::abi::KEY_BACKSPACE);
    assert!(a.take_goto().is_none(), "Backspace edited the field, not the path");
    let before = a.list_selected();
    press_key(&mut a, libkern::abi::KEY_DOWN);
    assert_eq!(a.list_selected(), before);
}

#[test]
fn deleting_asks_first_and_only_the_answer_removes() {
    let mut a = app();
    select(&mut a, 2); // a.txt
    a.update(Msg::Choose(Action::Delete));
    assert_eq!(a.confirming(), Some("a.txt"), "it asks");
    assert!(a.take_op().is_none(), "and does nothing while it is asking");

    a.update(Msg::KeepIt);
    assert!(a.confirming().is_none());
    assert!(a.take_op().is_none(), "keeping must not delete");

    a.update(Msg::Choose(Action::Delete));
    a.update(Msg::ConfirmDelete);
    assert!(a.confirming().is_none(), "the question goes when it is answered");
    assert_eq!(a.take_op(), Some(FileOp::Delete { path: "/home/a.txt".into(), dir: false }));

    let mut a = app();
    a.update(Msg::Choose(Action::Delete)); // row 0 is `archive/`
    a.update(Msg::ConfirmDelete);
    assert_eq!(a.take_op(), Some(FileOp::Delete { path: "/home/archive".into(), dir: true }));
}

#[test]
fn the_dialog_answers_escape_and_nothing_else() {
    let a = app();
    let ev = |code: u16, pressed: u16| KeyEvent::new(1, code, pressed, 0);
    assert_eq!(a.confirm_key(ev(libkern::abi::KEY_ESC, KEY_DOWN)), Some(Msg::KeepIt));
    assert_eq!(a.confirm_key(ev(libkern::abi::KEY_ENTER, KEY_DOWN)), None);
    assert_eq!(a.confirm_key(ev(libkern::abi::KEY_ESC, 0)), None, "a release is not an answer");
}

#[test]
fn a_drag_that_stays_inside_moves_into_the_directory_it_is_released_over() {
    let mut a = app();
    a.update(Msg::Grab(row(2))); // a.txt
    a.pointer_moved(100, row_y(&a, 2), 1);
    assert_eq!(a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 2), 1), Gesture::Moved);
    assert_eq!(a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 1), Gesture::Moved);
    assert_eq!(a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 0), Gesture::Dropped);
    assert_eq!(a.take_op(), Some(FileOp::MoveInto { from: "/home/a.txt".into(), to: "/home/work/a.txt".into() }));
    assert_eq!(a.take_drag(), None, "and the compositor was never told");
}

#[test]
fn only_a_directory_is_a_drop_target() {
    let mut a = app();
    a.update(Msg::Grab(row(2))); // a.txt
    a.pointer_moved(100, row_y(&a, 2), 1);
    a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 2), 1);
    assert_eq!(a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 3), 1), Gesture::None);
    assert_eq!(a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 3), 0), Gesture::None);
    assert!(a.take_op().is_none(), "a file row is not a folder");

    let mut a = app();
    a.update(Msg::Grab(row(1))); // work/
    a.pointer_moved(100, row_y(&a, 1), 1);
    a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 1);
    assert_eq!(a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 0), Gesture::Dropped);
    assert!(a.take_op().is_none(), "onto itself is not a move");
}

#[test]
fn the_drop_target_follows_the_pointer_and_only_over_a_folder() {
    let mut a = app();
    a.update(Msg::Grab(row(2))); // a.txt
    a.pointer_moved(100, row_y(&a, 2), 1);
    a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 2), 1);
    assert_eq!(a.drop_target(), None, "over the file it came from");
    a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 1);
    assert_eq!(a.drop_target(), Some(1), "over `work/`");
    a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 3), 1);
    assert_eq!(a.drop_target(), None, "over a file again");
    a.pointer_moved(100 + DRAG_SLOP, 4, 1);
    assert_eq!(a.drop_target(), None, "over the title bar");
    a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 1);
    a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 0);
    assert_eq!(a.drop_target(), None);
}

#[test]
fn the_menus_hold_what_they_say() {
    let mut a = app();
    a.update(Msg::MenuBar(0));
    let file: Element<Msg> = a.menu_view(0, &UiTheme::default(), None);
    for (i, item) in [
        (0, "New TabCtrl+T"),
        (1, "Close TabCtrl+W"),
        (4, "New WindowCtrl+Shift+N"),
        (5, "QuitCtrl+Q"),
        (7, "New File"),
        (8, "New Folder"),
        (9, "Rename"),
        (10, "Delete"),
        (11, "PropertiesCtrl+I"),
    ] {
        assert_eq!(labelled(&file, MENU_ROW_KEY + i), item, "the File menu's row {i}");
    }
    for rule in [3, 6] {
        assert_eq!(labelled(&file, MENU_ROW_KEY + rule), "", "row {rule} is a rule, not an item");
    }

    a.update(Msg::MenuBar(1));
    let edit: Element<Msg> = a.menu_view(1, &UiTheme::default(), None);
    for (i, item) in [(0, "CutCtrl+X"), (1, "CopyCtrl+C"), (2, "PasteCtrl+V"), (4, "Duplicate\u{2026}")] {
        assert_eq!(labelled(&edit, MENU_ROW_KEY + i), item, "the Edit menu's row {i}");
    }
    assert_eq!(labelled(&edit, MENU_ROW_KEY + 3), "", "row 3 is a rule");
    assert_eq!(labelled(&edit, MENU_ROW_KEY + 5), "", "and there is no row after Duplicate");
}

#[test]
fn rows_that_need_a_selection_are_disabled_without_one() {
    let mut a = App::new("/empty");
    a.show("/empty", alloc::vec![]);
    let needs = |a: &App, title: &str| -> Vec<bool> {
        a.menu_table()
            .into_iter()
            .find(|m| m.title == title)
            .expect("the menu exists")
            .items
            .iter()
            .filter_map(|it| match it {
                Item::Action { msg: Msg::Choose(act), enabled, .. } => act.needs_selection().then_some(*enabled),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        needs(&a, "File"),
        alloc::vec![false, false, false],
        "Rename, Delete and Properties, with nothing selected"
    );
    assert_eq!(needs(&a, "Edit"), alloc::vec![false], "Copy, with nothing selected");
    // Negative control: with a listing the same three become available.
    a.show("/empty", alloc::vec![Entry::file("a"), Entry::file("b")]);
    assert_eq!(needs(&a, "File"), alloc::vec![true, true, true]);
    assert_eq!(needs(&a, "Edit"), alloc::vec![true]);
    let file = a.menu_table().into_iter().next().expect("File");
    assert!(matches!(file.items[7], Item::Action { enabled: true, .. }), "New File does not act on a selection");
}

#[test]
fn close_tab_is_unavailable_on_the_last_tab() {
    let mut a = app();
    let enabled = |a: &App| match &a.menu_table()[0].items[1] {
        Item::Action { label, enabled, .. } => {
            assert_eq!(*label, "Close Tab");
            *enabled
        }
        Item::Separator => panic!("row 1 is Close Tab"),
    };
    assert!(!enabled(&a), "the lone tab does not close");
    a.update(Msg::NewTab);
    assert!(enabled(&a), "with two, either can go");
}

#[test]
fn a_bar_item_opens_its_menu_and_the_same_press_closes_it() {
    let mut a = app();
    assert_eq!(a.menus.open(), None);
    a.update(Msg::MenuBar(0));
    assert_eq!(a.menus.open(), Some(0));
    a.update(Msg::MenuBar(0));
    assert_eq!(a.menus.open(), None, "the same item closes it");
    a.update(Msg::MenuBar(0));
    a.update(Msg::MenuBar(1));
    assert_eq!(a.menus.open(), Some(1));
    for msg in a
        .menu_table()
        .iter()
        .flat_map(|m| m.items.iter())
        .filter_map(|it| match it {
            Item::Action { msg, .. } => Some(msg.clone()),
            Item::Separator => None,
        })
        .collect::<Vec<_>>()
    {
        let mut a = app();
        a.update(Msg::MenuBar(0));
        a.update(msg.clone());
        assert_eq!(a.menus.open(), None, "{msg:?} left the menu open");
    }
}

#[test]
fn a_drop_below_the_last_drawn_row_lands_nowhere() {
    // A `y` in the band below the last drawn row is inside the window: bounding a drop by
    // `entries.len()` moved a file into a folder the person could not see.
    let mut a = App::new("/big");
    let mut rows: alloc::vec::Vec<Entry> = (1..=20).map(|i| Entry::dir(&alloc::format!("d{i:02}"))).collect();
    rows.extend((1..=21).map(|i| Entry::file(&alloc::format!("f{i:02}"))));
    a.show("/big", rows);
    assert!(a.entries().len() > a.visible_rows(), "more entries than the window draws");

    let last_drawn_bottom = (a.list_top() + a.visible_rows() as u32 * ROW_H) as i32;
    assert!(
        last_drawn_bottom < a.window_size().h as i32,
        "there is a band below the rows and inside the window, which is the whole hazard"
    );

    a.update(Msg::Grab(row(1))); // d02
    a.pointer_moved(100, row_y(&a, 1), 1);
    a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 1);
    assert_eq!(a.drop_target(), Some(1), "over a real row to start with");
    let below = last_drawn_bottom + 2;
    assert_eq!(a.pointer_moved(100 + DRAG_SLOP, below, 1), Gesture::Moved);
    assert_eq!(a.drop_target(), None, "nothing is drawn there");
    assert_eq!(a.pointer_moved(100 + DRAG_SLOP, below, 0), Gesture::None);
    assert!(a.take_op().is_none(), "and letting go there moves nothing");
}

#[test]
fn a_question_answered_after_navigating_still_names_what_it_asked_about() {
    // An operation is two gestures and its paths are resolved at the first: the parent keeps
    // taking clicks while a prompt or dialog is up (`TODO(dialog-modality)`).
    let mut a = app();
    select(&mut a, 2); // a.txt
    a.update(Msg::Choose(Action::Delete));
    assert_eq!(a.confirming(), Some("a.txt"));

    a.update(Msg::Activate(1));
    let to = a.take_goto().expect("a directory row navigates");
    assert_eq!(to, "/home/work");
    a.show(&to, alloc::vec![Entry::file("a.txt"), Entry::file("payroll.txt")]);

    assert_eq!(a.confirming(), None, "a question about a directory you have left is gone");
    a.update(Msg::ConfirmDelete);
    assert!(a.take_op().is_none());
}

#[test]
fn a_delete_answered_after_the_selection_moves_removes_what_was_asked_about() {
    // A click on a *file* row re-selects without navigating, so nothing clears the question. This
    // guards the question surviving a selection change; the rename below pins the captured path.
    let mut a = app();
    select(&mut a, 2); // a.txt
    a.update(Msg::Choose(Action::Delete));
    a.update(Msg::Activate(3)); // notes.txt — selects it, and asks the shell to open it
    let _ = a.take_open();
    assert_eq!(a.confirming(), Some("a.txt"), "the question still names what it asked about");
    a.update(Msg::ConfirmDelete);
    assert_eq!(
        a.take_op(),
        Some(FileOp::Delete { path: "/home/a.txt".into(), dir: false }),
        "and it removes that, not whatever is selected now"
    );
}

#[test]
fn a_rename_answered_after_the_selection_moves_renames_what_was_chosen() {
    let mut a = app();
    select(&mut a, 2); // a.txt
    a.update(Msg::Choose(Action::Rename));
    a.update(Msg::Activate(3)); // notes.txt
    let _ = a.take_open();
    press_key(&mut a, KEY_X);
    press_key(&mut a, libkern::abi::KEY_ENTER);
    assert_eq!(
        a.take_op(),
        Some(FileOp::Rename { from: "/home/a.txt".into(), to: "/home/x".into() }),
        "the entry the menu row was pressed on, not the one selected now"
    );

    let mut a = app();
    select(&mut a, 2);
    a.update(Msg::Choose(Action::Rename));
    a.show("/home/work", alloc::vec![Entry::file("payroll.txt")]);
    let ui: Element<Msg> = a.view(&UiTheme::default(), None);
    assert_eq!(labelled(&ui, PATH_KEY), "/home/work", "the path is back");
    press_key(&mut a, libkern::abi::KEY_ENTER);
    assert!(a.take_op().is_none(), "and Enter renames nothing");
}

#[test]
fn copying_a_folder_is_a_tree_copy() {
    let mut a = app();
    select(&mut a, 0); // archive/
    a.update(Msg::Choose(Action::Duplicate));
    press_key(&mut a, KEY_X);
    press_key(&mut a, libkern::abi::KEY_ENTER);
    assert_eq!(a.take_op(), Some(FileOp::Copy { from: "/home/archive".into(), to: "/home/x".into(), dir: true }));
}

fn washed_rows<M>(view: &Element<M>) -> Vec<u64> {
    fn washed<M>(e: &Element<M>) -> bool {
        if matches!(e.node, libui::element::Node::Wash { .. }) {
            return true;
        }
        e.children().any(washed)
    }
    fn walk<M>(e: &Element<M>, out: &mut Vec<u64>) {
        if let Some(k) = e.key
            && (LIST_ROW_KEY..LIST_ROW_KEY + 1000).contains(&k)
            && washed(e)
        {
            out.push(k);
        }
        for c in e.children() {
            walk(c, out);
        }
    }
    let mut out = Vec::new();
    walk(view, &mut out);
    out
}

/// Through the built view with a filter up: the row the screen highlights is the file an action
/// acts on, and `Enter` opens it. Keying the rows by position must fail this.
#[test]
fn a_filtered_row_is_the_row_that_is_acted_on() {
    const KEY_T: u16 = 20;
    const KEY_H: u16 = 35;
    let mut a = App::new("/home");
    a.show("/home", alloc::vec![Entry::dir("Documents"), Entry::file("Notes.txt"), Entry::file("other.txt"),]);
    a.update(Msg::OpenSearch);
    press_key(&mut a, KEY_T);
    press_key(&mut a, KEY_H);
    assert_eq!(a.visible_indices(), alloc::vec![2], "one row left: other.txt");

    let cell = libui::layout::FixedCell { w: 8, h: 16 };
    let size = a.window_size();
    let view = a.view(&UiTheme::default(), None);
    let l = libui::layout::layout(&view, Rect::new(0, 0, size.w, size.h), &cell);
    let mut tree = libui::diff::Tree::new();
    tree.update(&view, &l).expect("the view is diffable");
    let mut router = libui::route::Router::new();
    let at = |flags: u16, buttons: u16, y: i32| librsproto::surface::PointerEvent {
        kind: librsproto::surface::POINTER_BUTTON,
        button: 0x110,
        buttons,
        flags,
        x: (SIDEBAR_W + 60) as i32,
        y,
        ..Default::default()
    };
    let row_y = (a.list_top() + ROW_H / 2) as i32;
    for ev in [at(librsproto::surface::POINTER_PRESSED, 1, row_y), at(0, 0, row_y)] {
        for m in router.pointer(&tree, &view, &l, ev).0 {
            a.update(m);
        }
    }
    assert_eq!(a.selected_name(), Some("other.txt"), "the press picked the row it landed on");
    assert_eq!(
        a.selection().iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
        alloc::vec!["other.txt"],
        "and the selection the menus act on is that row"
    );
    a.update(Msg::Choose(Action::Delete));
    assert_eq!(a.confirming(), Some("other.txt"), "Delete asks about the row on screen");
    a.update(Msg::ConfirmDelete);
    assert_eq!(
        a.take_op(),
        Some(FileOp::Delete { path: "/home/other.txt".into(), dir: false }),
        "…and deletes that one"
    );

    let mut b = App::new("/home");
    b.show("/home", alloc::vec![Entry::dir("Documents"), Entry::dir("theatre")]);
    b.update(Msg::OpenSearch);
    press_key(&mut b, KEY_T);
    press_key(&mut b, KEY_H);
    assert_eq!(b.visible_indices(), alloc::vec![1], "theatre is the only match");
    press_key(&mut b, libkern::abi::KEY_ENTER);
    assert_eq!(b.take_goto().as_deref(), Some("/home/theatre"), "Enter descended into it");

    // A selection the filter hides keeps its place: handed straight to `list_view` it would be
    // read as a row position.
    let mut c = App::new("/home");
    c.show("/home", alloc::vec![Entry::dir("Documents"), Entry::dir("theatre")]);
    let theme = UiTheme::default();
    let size = c.window_size();
    c.update(Msg::Press(LIST_ROW_KEY));
    assert_eq!(c.selected_name(), Some("Documents"));
    c.update(Msg::OpenSearch);
    press_key(&mut c, KEY_T);
    press_key(&mut c, KEY_H);
    assert_eq!(c.visible_indices(), alloc::vec![1], "Documents is hidden");
    let framed = c.view(&theme, None);
    let _ = libui::layout::layout(&framed, Rect::new(0, 0, size.w, size.h), &libui::layout::FixedCell { w: 8, h: 16 });
    assert_eq!(washed_rows(&framed), Vec::<u64>::new(), "no row is lit while none is shown");
    press_key(&mut c, libkern::abi::KEY_ESC);
    assert_eq!(c.selected_name(), Some("Documents"), "the selection survived a search that hid it");
}

/// `list_top` is where the first row is actually drawn, asked of the laid-out tree: the drag
/// tests aim through it, so they cannot catch it drifting.
#[test]
fn list_top_is_where_the_first_row_is_drawn() {
    let cell = libui::layout::FixedCell { w: 8, h: 16 };
    let drawn_top = |a: &mut App| {
        let size = a.window_size();
        let view = a.view(&UiTheme::default(), None);
        let l = libui::layout::layout(&view, Rect::new(0, 0, size.w, size.h), &cell);
        libui::layout::locate(&view, &l, LIST_ROW_KEY).expect("row 0 is in the tree").origin.y
    };
    let mut one = app();
    assert_eq!(one.list_top() as i32, drawn_top(&mut one), "with one tab");
    let mut two = app();
    two.update(Msg::NewTab);
    two.show("/home", alloc::vec![Entry::dir("work"), Entry::file("a.txt")]);
    assert_eq!(two.list_top() as i32, drawn_top(&mut two), "and with a tab strip");
}

#[test]
fn search_filters_the_listing_and_keeps_each_rows_identity() {
    const KEY_N: u16 = 49;
    const KEY_O: u16 = 24;
    const KEY_T: u16 = 20;
    const KEY_H: u16 = 35;
    let mut a = App::new("/home");
    a.show("/home", alloc::vec![Entry::dir("Documents"), Entry::file("Notes.txt"), Entry::file("other.txt"),]);
    assert_eq!(a.visible_indices(), alloc::vec![0, 1, 2], "unfiltered, every row shows");
    a.update(Msg::OpenSearch);
    assert_eq!(a.search_text(), Some(""), "the field opens empty");
    press_key(&mut a, KEY_N);
    press_key(&mut a, KEY_O);
    assert_eq!(a.search_text(), Some("no"));
    assert_eq!(a.visible_indices(), alloc::vec![1]);
    a.update(Msg::SearchCancel);
    a.update(Msg::OpenSearch);
    press_key(&mut a, KEY_T);
    press_key(&mut a, KEY_H);
    assert_eq!(a.visible_indices(), alloc::vec![2], "matched in the middle of a name");
    press_key(&mut a, libkern::abi::KEY_ESC);
    assert_eq!(a.search_text(), None);
    assert_eq!(a.visible_indices(), alloc::vec![0, 1, 2]);
}

#[test]
fn the_columns_say_what_they_mean() {
    assert_eq!(column_size(0), "0");
    assert_eq!(column_size(848), "848");
    assert_eq!(column_size(4096), "4096");
    assert_eq!(column_size(10 * 1024 - 1), "10239", "the last exact one");
    assert_eq!(column_size(10 * 1024), "10K", "and the first rounded");
    assert_eq!(column_size(11 * 1024), "11K");
    assert_eq!(column_size(5 * 1024 * 1024 / 2), "2.5M", "under ten units keeps its tenth");
    assert_eq!(column_size(2936012), "2.8M");
    assert_eq!(column_size(100 * 1024 * 1024), "100M", "past ten units the decimal goes");
    assert_eq!(column_size(3 * 1024 * 1024 * 1024), "3.0G");

    assert_eq!(column_kind("work", true), "dir");
    assert_eq!(column_kind("theme.toml", false), "toml");
    assert_eq!(column_kind("notes", false), "file", "no dot is no extension");
    assert_eq!(column_kind(".quiet", false), "file", "a leading dot is not one either");
    assert_eq!(column_kind("archive.tar.gz", false), "gz", "the last one wins");

    const NOON: i64 = 1_790_685_296;
    let now = NOON as u64 * 1_000_000_000;
    let today = column_modified(NOON, now);
    assert_eq!(today.len(), 5, "today is a clock: {today:?}");
    let yesterday = NOON - 24 * 60 * 60;
    let then = column_modified(yesterday, now);
    assert_eq!(then.len(), 10, "another day is a date: {then:?}");
    assert!(then.starts_with("2026-"), "{then:?}");
    assert_eq!(column_modified(0, now), "", "unreported says nothing");
    assert_eq!(column_modified(-1, now), "", "and so does a negative");
    assert_eq!(column_modified(NOON, 0).len(), 10, "no today to compare against");
}

#[test]
fn a_breadcrumb_is_names_and_punctuation() {
    let ui = UiTheme::default();
    let parts = |p: &str| -> Vec<String> {
        breadcrumb::<Msg>(p, &ui)
            .iter()
            .map(|e| {
                let mut out = String::new();
                fn walk<M>(e: &Element<M>, out: &mut String) {
                    if let libui::element::Node::Text(t) = &e.node {
                        out.push_str(t);
                    }
                    for c in e.children() {
                        walk(c, out);
                    }
                }
                walk(e, &mut out);
                out
            })
            .collect()
    };
    assert_eq!(parts("/"), alloc::vec!["/"], "the root is a place, not a separator");
    assert_eq!(
        parts("/home/alice"),
        alloc::vec!["/", "home", "/", "alice"],
        "each name is its own element, with punctuation between"
    );
    let inks = |p: &str| -> Vec<Option<libdraw::format::Rgb>> {
        fn colour<M>(e: &Element<M>) -> Option<libdraw::format::Rgb> {
            if let libui::element::Node::Ink { colour, .. } = &e.node {
                return Some(*colour);
            }
            e.children().find_map(colour)
        }
        breadcrumb::<Msg>(p, &ui).iter().map(colour).collect()
    };
    assert_eq!(
        inks("/home/alice"),
        alloc::vec![Some(ui.border), Some(ui.foreground_dim), Some(ui.border), None,],
        "separators in the line colour, the parent dim, and where you are in the body ink"
    );
}

#[test]
fn a_single_tab_draws_no_strip_at_all() {
    use libdraw::format::PixelFormat;
    use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
    const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSans.ttf");
    let font = libdraw::text::Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font");
    let ui = UiTheme::default();
    let (w, h) = (500u32, 300u32);
    let ink_in_strip = |a: &mut App| {
        let e = a.view(&ui, None);
        let all = Rect::new(0, 0, w, h);
        let m = libui::paint::FontMetrics::new(&font, ui.font_px);
        let l = libui::layout::layout(&e, all, &m);
        let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
        fb.clear(ui.background);
        libui::paint::paint(&mut fb, &font, &ui, &e, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
        let top = TITLE_BAR_H + MENU_BAR_H;
        (top..top + TAB_STRIP_H)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .filter(|(x, y)| fb.get_pixel(*x, *y) == Some(ui.foreground_dim))
            .count()
    };
    let mut one = App::new("/home");
    one.show("/home", alloc::vec![Entry::dir("Documents")]);
    assert_eq!(one.tab_strip_h(), 0, "one tab is no strip");
    assert_eq!(ink_in_strip(&mut one), 0, "…and it draws nothing where a strip would be");
    let mut two = App::new("/home");
    two.show("/home", alloc::vec![Entry::dir("Documents")]);
    two.update(Msg::NewTab);
    assert_eq!(two.tab_strip_h(), TAB_STRIP_H, "two tabs are a strip");
    assert!(ink_in_strip(&mut two) > 0, "…and it draws one");
}

#[test]
fn tabs_hold_their_own_directory_selection_and_scroll() {
    let mut a = app();
    select(&mut a, 2); // a.txt
    assert_eq!(a.list_selected(), Some(2));

    a.update(Msg::NewTab);
    assert_eq!(a.tab_count(), 2);
    assert_eq!(a.take_goto().as_deref(), Some("/home"));
    a.show("/home/work", alloc::vec![Entry::file("payroll.txt")]);
    assert_eq!(a.path(), "/home/work");
    assert_eq!(a.list_selected(), Some(0));

    let first = a.tabs()[0].0;
    a.update(Msg::SelectTab(first));
    assert_eq!(a.path(), "/home");
    assert_eq!(a.list_selected(), Some(2), "its own selection");
    assert_eq!(a.entries().len(), 4, "and its own listing");
}

#[test]
fn a_tabs_key_can_never_be_a_row_index() {
    let mut a = app();
    for _ in 0..4 {
        a.update(Msg::NewTab);
        let _ = a.take_goto();
    }
    for (key, _) in a.tabs() {
        assert!(key >= TAB_KEY_BASE, "every tab is above the base");
    }
    assert_eq!(TAB_KEY_BASE, 1 << 63);

    for (key, _) in a.tabs() {
        assert_eq!(key & libui::widget::TAB_CLOSE_BIT, 0, "tab key {key:#x} sets the close bit");
    }
    assert_eq!((TAB_KEY_BASE + (1 << 32)) & libui::widget::TAB_CLOSE_BIT, 0);
}

#[test]
fn a_browser_tab_closes_without_asking_and_the_last_one_closes_the_window() {
    // No question: a listing is a view of the filesystem, not unsaved work.
    let mut a = app();
    a.update(Msg::NewTab);
    let _ = a.take_goto();
    assert_eq!(a.tab_count(), 2);
    let second = a.tabs()[1].0;

    a.update(Msg::CloseTab(second));
    assert_eq!(a.tab_count(), 1);
    assert!(a.confirming().is_none(), "a pane asks nothing");
    assert!(!a.closing());

    a.update(Msg::CloseTab(a.tabs()[0].0));
    assert!(a.closing(), "the last tab takes the window with it");
}

#[test]
fn a_tab_is_labelled_by_the_folder_it_shows() {
    let mut a = app();
    assert_eq!(a.tabs()[0].1, "home");
    a.show("/", alloc::vec![Entry::dir("home")]);
    assert_eq!(a.tabs()[0].1, "/", "the root has no last component");
}

#[test]
fn the_tab_chords_are_the_ones_the_keymap_names() {
    assert_eq!(libinput::keymap::to_char(NEW_TAB_KEYCODE, 0), Some(b't'));
    assert_eq!(libinput::keymap::to_char(CLOSE_TAB_KEYCODE, 0), Some(b'w'));
    assert_eq!(libinput::keymap::to_char(HIDDEN_KEYCODE, 0), Some(b'h'));
    assert_eq!(libinput::keymap::to_char(LOCATION_KEYCODE, 0), Some(b'l'));
    assert_eq!(libinput::keymap::to_char(PROPERTIES_KEYCODE, 0), Some(b'i'));
    assert_eq!(libinput::keymap::to_char(CUT_KEYCODE, 0), Some(b'x'));
    assert_eq!(libinput::keymap::to_char(COPY_KEYCODE, 0), Some(b'c'));
    assert_eq!(libinput::keymap::to_char(PASTE_KEYCODE, 0), Some(b'v'));
}

/// Hovering a listing row must not light a piece of chrome: `hovered` is one namespace, so a row
/// keyed by its bare index lights the button that shares the number.
#[test]
fn a_listing_row_does_not_light_the_chrome() {
    let mut a = app();
    a.show("/home", (0..8).map(|i| Entry::file(&alloc::format!("f{i}.txt"))).collect());
    let ui = UiTheme::default();
    let keys: Vec<u64> = a.list_rows().iter().map(|r| r.key).collect();
    for k in &keys {
        for (name, chrome) in [
            ("LIST_KEY", LIST_KEY),
            ("UP_KEY", UP_KEY),
            ("STRIP_KEY", STRIP_KEY),
            ("PATH_KEY", PATH_KEY),
            ("NOTICE_KEY", NOTICE_KEY),
            ("PROMPT_KEY", PROMPT_KEY),
            ("BAR_KEY", BAR_KEY),
            ("STRIP_INNER_KEY", STRIP_INNER_KEY),
            ("TAB_STRIP_KEY", TAB_STRIP_KEY),
            ("CONFIRM_TITLE_KEY", CONFIRM_TITLE_KEY),
            ("PROPS_KEY", PROPS_KEY),
        ] {
            assert_ne!(*k, chrome, "row key {k} is also {name}");
        }
    }
    let _ = a.view(&ui, None);
}

#[test]
fn the_anchor_follows_the_file_across_a_re_sort() {
    let mut a = app();
    a.show("/home", five());
    a.note_press(libdraw::geom::Point::new(40, 60), 1_000, 0);
    a.update(Msg::Press(row(0)));
    assert_eq!(picked(&a), alloc::vec!["f0.txt"]);

    a.update(Msg::SetOrder(libfs::Order::NameDesc));
    a.note_press(libdraw::geom::Point::new(40, 100), 2_000, MOD_SHIFT);
    a.update(Msg::Press(row(2)));
    // Rows 2..=4 under `NameDesc` are `f2`, `f1`, `f0` in listing order; a stale anchor would
    // give `f4, f3, f2`.
    assert_eq!(
        picked(&a),
        alloc::vec!["f2.txt", "f1.txt", "f0.txt"],
        "the range runs from the row the person can see highlighted"
    );
}

#[test]
fn a_navigation_abandons_the_run() {
    let at = libdraw::geom::Point::new(40, 60);
    let mut a = app();
    a.show("/home", alloc::vec![Entry::dir("work"), Entry::file("a.txt")]);
    a.note_press(at, 1_000, 0);
    a.update(Msg::Press(row(0)));
    a.note_press(at, 1_120, 0);
    a.update(Msg::Press(row(0)));
    assert_eq!(a.take_goto().as_deref(), Some("/home/work"), "the double click descended");

    a.show("/home/work", alloc::vec![Entry::file("inside.txt")]);
    a.note_press(at, 1_240, 0);
    a.update(Msg::Press(row(0)));
    assert_eq!(a.take_open(), None, "it selected rather than opening");
}

use librsproto::clipboard::PathVerb;

#[test]
fn copy_pushes_the_whole_selection_as_absolute_paths() {
    let mut a = app();
    a.show("/home/papers", five());
    a.note_press(libdraw::geom::Point::new(40, 60), 1_000, 0);
    a.update(Msg::Press(row(0)));
    a.note_press(libdraw::geom::Point::new(40, 100), 2_000, MOD_CTRL);
    a.update(Msg::Press(row(2)));

    a.update(Msg::CopyFiles);
    let (verb, paths) = a.take_clip_push().expect("a push was recorded");
    assert_eq!(verb, PathVerb::Copy);
    assert_eq!(paths, alloc::vec!["/home/papers/f0.txt", "/home/papers/f2.txt"]);
    assert_eq!(a.notice.as_deref(), Some("copied 2 entries"));
    assert_eq!(a.take_clip_push(), None, "and the record is cleared");
}

#[test]
fn cut_is_the_verb_on_the_wire() {
    let mut a = app();
    a.show("/home", five());
    a.note_press(libdraw::geom::Point::new(40, 60), 1_000, 0);
    a.update(Msg::Press(row(1)));
    a.update(Msg::CutFiles);
    let (verb, paths) = a.take_clip_push().expect("a push");
    assert_eq!(verb, PathVerb::Cut);
    assert_eq!(paths, alloc::vec!["/home/f1.txt"]);
    assert_eq!(a.notice.as_deref(), Some("cut 1 entry"));
}

#[test]
fn copying_an_empty_selection_pushes_nothing() {
    let mut a = app();
    a.show("/empty", alloc::vec![]);
    a.update(Msg::CopyFiles);
    assert_eq!(a.take_clip_push(), None);
    assert_eq!(a.notice.as_deref(), Some("nothing is selected"));
}

#[test]
fn a_paste_queues_one_operation_per_path() {
    let mut a = app();
    a.show("/home/papers", alloc::vec![Entry::file("kept.txt")]);
    a.update(Msg::PasteFiles);
    assert!(a.take_clip_read(), "the binary is asked to read the ring");
    assert!(!a.take_clip_read(), "and asked once");

    a.pasted(PathVerb::Copy, &[("/home/a.txt", false), ("/home/b.txt", false)]);
    let first = a.take_op().expect("one per path");
    let second = a.take_op().expect("and the second");
    assert_eq!(
        first,
        FileOp::Copy { from: String::from("/home/a.txt"), to: String::from("/home/papers/a.txt"), dir: false }
    );
    assert!(matches!(second, FileOp::Copy { .. }));
    assert_eq!(a.take_op(), None, "and no more than that");

    a.pasted(PathVerb::Cut, &[("/home/a.txt", false)]);
    assert!(matches!(a.take_op(), Some(FileOp::MoveInto { .. })));
}

/// A paste into the directory the file is already in is skipped: a copy onto itself truncates it.
#[test]
fn pasting_a_file_into_its_own_directory_does_nothing() {
    let mut a = app();
    a.show("/home", alloc::vec![Entry::file("a.txt")]);
    a.pasted(PathVerb::Copy, &[("/home/a.txt", false)]);
    assert_eq!(a.take_op(), None, "no operation was queued");
    assert_eq!(a.notice.as_deref(), Some("nothing to paste here"));
}

/// A pasted **folder** is queued as a tree copy, not a file copy; the binary asks the filesystem.
#[test]
fn a_pasted_folder_is_a_tree_copy() {
    let mut a = app();
    a.show("/home/papers", alloc::vec![]);
    a.pasted(PathVerb::Copy, &[("/home/work", true)]);
    assert_eq!(
        a.take_op(),
        Some(FileOp::Copy { from: String::from("/home/work"), to: String::from("/home/papers/work"), dir: true })
    );

    a.pasted(PathVerb::Copy, &[("/home/a.txt", false)]);
    assert!(matches!(a.take_op(), Some(FileOp::Copy { dir: false, .. })));
}

#[test]
fn pasting_text_says_what_it_found() {
    let mut a = app();
    a.paste_not_paths();
    assert_eq!(a.notice.as_deref(), Some("the clipboard holds text, not files"));
    assert_eq!(a.take_op(), None);
}

fn five() -> Vec<Entry> {
    (0..5).map(|i| Entry::file(&alloc::format!("f{i}.txt"))).collect()
}

fn picked(a: &App) -> Vec<String> {
    a.selection().iter().map(|e| e.name.clone()).collect()
}

#[test]
fn ctrl_click_adds_and_removes_one_row() {
    let mut a = app();
    a.show("/home", five());
    a.note_press(libdraw::geom::Point::new(40, 60), 1_000, 0);
    a.update(Msg::Press(row(1)));
    assert_eq!(picked(&a), alloc::vec!["f1.txt"]);

    a.note_press(libdraw::geom::Point::new(40, 100), 2_000, MOD_CTRL);
    a.update(Msg::Press(row(3)));
    assert_eq!(picked(&a), alloc::vec!["f1.txt", "f3.txt"], "in listing order");

    a.note_press(libdraw::geom::Point::new(40, 100), 3_000, MOD_CTRL);
    a.update(Msg::Press(row(3)));
    assert_eq!(picked(&a), alloc::vec!["f1.txt"]);

    a.note_press(libdraw::geom::Point::new(40, 60), 4_000, MOD_CTRL);
    a.update(Msg::Press(row(1)));
    assert!(picked(&a).is_empty(), "the anchor can be unpicked too: {:?}", picked(&a));
}

#[test]
fn shift_click_picks_a_range() {
    let mut a = app();
    a.show("/home", five());
    a.note_press(libdraw::geom::Point::new(40, 60), 1_000, 0);
    a.update(Msg::Press(row(1)));
    a.note_press(libdraw::geom::Point::new(40, 140), 2_000, MOD_SHIFT);
    a.update(Msg::Press(row(3)));
    assert_eq!(picked(&a), alloc::vec!["f1.txt", "f2.txt", "f3.txt"]);

    let mut a = app();
    a.show("/home", five());
    a.note_press(libdraw::geom::Point::new(40, 140), 1_000, 0);
    a.update(Msg::Press(row(3)));
    a.note_press(libdraw::geom::Point::new(40, 60), 2_000, MOD_SHIFT);
    a.update(Msg::Press(row(1)));
    assert_eq!(picked(&a), alloc::vec!["f1.txt", "f2.txt", "f3.txt"]);
}

#[test]
fn a_plain_press_replaces_the_picks() {
    let mut a = app();
    a.show("/home", five());
    a.note_press(libdraw::geom::Point::new(40, 60), 1_000, 0);
    a.update(Msg::Press(row(0)));
    a.note_press(libdraw::geom::Point::new(40, 100), 2_000, MOD_CTRL);
    a.update(Msg::Press(row(2)));
    assert_eq!(picked(&a).len(), 2);

    a.note_press(libdraw::geom::Point::new(40, 180), 3_000, 0);
    a.update(Msg::Press(row(4)));
    assert_eq!(picked(&a), alloc::vec!["f4.txt"], "one press, one row");
}

/// A Ctrl-click never opens, however fast it repeats.
#[test]
fn building_a_selection_does_not_open_anything() {
    let mut a = app();
    a.show("/home", five());
    let at = libdraw::geom::Point::new(40, 60);
    a.note_press(at, 1_000, MOD_CTRL);
    a.update(Msg::Press(row(1)));
    a.note_press(at, 1_100, MOD_CTRL);
    a.update(Msg::Press(row(1)));
    assert_eq!(a.take_open(), None, "two fast Ctrl-clicks are not a double click");
}

#[test]
fn picks_follow_the_files_across_a_re_sort() {
    let mut a = app();
    a.show("/home", five());
    a.note_press(libdraw::geom::Point::new(40, 60), 1_000, 0);
    a.update(Msg::Press(row(0)));
    a.note_press(libdraw::geom::Point::new(40, 100), 2_000, MOD_CTRL);
    a.update(Msg::Press(row(1)));
    assert_eq!(picked(&a), alloc::vec!["f0.txt", "f1.txt"]);

    a.update(Msg::SetOrder(libfs::Order::NameDesc));
    assert_eq!(picked(&a), alloc::vec!["f1.txt", "f0.txt"], "the same two files, reordered");

    a.show("/elsewhere", five());
    assert_eq!(
        picked(&a),
        alloc::vec!["f4.txt"],
        "a new listing keeps only its own row 0, under the order this tab is in"
    );
}

#[test]
fn the_places_are_built_from_this_sessions_home() {
    let a = App::new("/home/someone-else");
    assert_eq!(a.places(), libfs::places("/home/someone-else"));
    assert_eq!(a.places()[1].path, "/home/someone-else/Documents");
}

#[test]
fn one_press_on_a_place_navigates() {
    let mut a = app();
    let root = a.places().len() as u64 - 1;
    a.update(Msg::Place(SIDEBAR_ROW_KEY + root));
    assert_eq!(a.take_goto().as_deref(), Some("/"), "one press was enough");

    a.update(Msg::Place(SIDEBAR_ROW_KEY + 1));
    assert_eq!(a.take_goto().as_deref(), Some("/home/Documents"));
}

/// A key that is not a sidebar row's goes nowhere: a saturating conversion would make a stray
/// key mean Home.
#[test]
fn a_key_below_the_base_is_not_a_place() {
    let mut a = app();
    a.update(Msg::Place(0));
    assert_eq!(a.take_goto(), None, "key 0 is not Home");
    a.update(Msg::Place(SIDEBAR_ROW_KEY + 99));
    assert_eq!(a.take_goto(), None, "and neither is a row that does not exist");
}

#[test]
fn the_sidebar_follows_where_the_tab_is() {
    let mut a = app();
    let ui = UiTheme::default();
    let _ = a.view(&ui, None);
    assert_eq!(a.sidebar.selected, Some(0), "the browser starts at home");

    a.show("/home/Pictures", alloc::vec![]);
    let _ = a.view(&ui, None);
    let want = a.places().iter().position(|p| p.name == "Pictures");
    assert_eq!(a.sidebar.selected, want, "and follows a navigation it did not cause");

    a.show("/home/papers", alloc::vec![]);
    let _ = a.view(&ui, None);
    assert_eq!(a.sidebar.selected, None, "nowhere on the list is nothing highlighted");
}

/// The sidebar's rows share no key with the listing's or with the chrome's, asserted about the
/// ranges: a sample listing cannot reach the collision.
#[test]
fn a_place_does_not_light_a_listing_row() {
    const {
        assert!(
            SIDEBAR_ROW_KEY - LIST_ROW_KEY > (1u64 << 40),
            "a listing would have to hold over 2^40 entries to reach the sidebar"
        )
    };
    let mut a = app();
    a.show("/home", (0..8).map(|i| Entry::file(&alloc::format!("f{i}.txt"))).collect());
    let listing: Vec<u64> = a.list_rows().iter().map(|r| r.key).collect();
    for i in 0..a.places().len() as u64 {
        let p = SIDEBAR_ROW_KEY + i;
        assert!(!listing.contains(&p), "sidebar key {p} is also a listing row");
        for (name, chrome) in [
            ("SIDEBAR_KEY", SIDEBAR_KEY),
            ("LIST_KEY", LIST_KEY),
            ("UP_KEY", UP_KEY),
            ("TAB_STRIP_KEY", TAB_STRIP_KEY),
            ("PROPS_KEY", PROPS_KEY),
        ] {
            assert_ne!(p, chrome, "sidebar key {p} is also {name}");
        }
    }
    const { assert!(TAB_KEY_BASE > SIDEBAR_ROW_KEY, "tabs must stay clear of the sidebar too") };
}

/// A storage table as the service writes one: a disk with nothing on it; `init`'s root; the
/// internal root; two sticks labelled `DATA`, mounted `DATA` and `DATA-2`; a stick with no
/// label, mounted under its partition's name; and a stick refused, so mounted nowhere.
fn storage_table() -> libstream::wire::Table {
    use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};
    let mut schema = Schema::new();
    for (name, tag) in [
        ("name", TypeTag::String),
        ("kind", TypeTag::String),
        ("description", TypeTag::String),
        ("size", TypeTag::Int),
        ("filesystem", TypeTag::String),
        ("label", TypeTag::String),
        ("mounted", TypeTag::String),
        ("by", TypeTag::String),
        ("mode", TypeTag::String),
        ("clean", TypeTag::Bool),
        ("removable", TypeTag::Bool),
    ] {
        schema = schema.field(name, tag, TypeModifiers::NONE);
    }
    let s = |v: &str| Value::Str(String::from(v));
    let row = |n: &str, fs: Value, label: Value, at: Value, removable: bool| {
        alloc::vec![
            s(n),
            s("partition"),
            Value::Null,
            Value::Int(1 << 20),
            fs,
            label,
            at,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Bool(removable),
        ]
    };
    let rows = alloc::vec![
        row("blk-0", Value::Null, Value::Null, Value::Null, false),
        row("blk-2", s("ext4"), Value::Null, s("/"), false),
        row("blk-3", s("ext4"), s("nitrox-root"), s("/storage/nitrox-root"), false),
        row("blk-5", s("fat"), s("DATA"), s("/storage/DATA"), true),
        row("blk-7", s("fat"), s("DATA"), s("/storage/DATA-2"), true),
        row("blk-9", s("fat"), Value::Null, s("/storage/partition 1"), true),
        row("blk-11", s("fat"), s("SMALL"), Value::Null, true),
    ];
    libstream::wire::Table { flags: StreamFlags::NONE, schema, rows }
}

#[test]
fn the_drives_are_every_mount_under_storage_by_its_mount_name() {
    let d = |name: &str, block, removable| Drive { name: String::from(name), block, removable };
    assert_eq!(
        drives(&storage_table()),
        alloc::vec![d("nitrox-root", 3, false), d("DATA", 5, true), d("DATA-2", 7, true), d("partition 1", 9, true),]
    );
    assert_eq!(drives(&storage_table())[1].path(), "/storage/DATA");
    let mut old = storage_table();
    old.schema.fields.retain(|f| f.name != "mounted");
    assert_eq!(drives(&old), alloc::vec![], "a table without the column lists nothing");
}

fn with_drives() -> App {
    let mut a = app();
    a.set_drives(drives(&storage_table()));
    a
}

#[test]
fn a_removable_drive_has_an_eject_button_and_an_internal_one_none() {
    let mut a = with_drives();
    let cell = libui::layout::FixedCell { w: 8, h: 16 };
    let size = a.window_size();
    let e = a.view(&UiTheme::default(), None);
    let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &cell);
    for n in [3, 5, 7, 9] {
        assert!(libui::layout::locate(&e, &l, DRIVE_ROW_KEY + n).is_some(), "drive {n} has a row");
    }
    for n in [5, 7, 9] {
        assert!(libui::layout::locate(&e, &l, DRIVE_EJECT_KEY + n).is_some(), "drive {n} has a button");
    }
    assert!(libui::layout::locate(&e, &l, DRIVE_EJECT_KEY + 3).is_none(), "the internal disk has none");
    assert_eq!(libui::layout::locate(&e, &l, DRIVE_ROW_KEY + 11), None, "a stick mounted nowhere is no drive");
}

fn click_key(a: &mut App, key: u64) {
    let cell = libui::layout::FixedCell { w: 8, h: 16 };
    let size = a.window_size();
    let e = a.view(&UiTheme::default(), None);
    let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &cell);
    let mut tree = libui::diff::Tree::new();
    tree.update(&e, &l).expect("diffable");
    let at = libui::layout::locate(&e, &l, key).expect("keyed");
    let mut r = libui::route::Router::new();
    for pressed in [true, false] {
        let ev = librsproto::surface::PointerEvent {
            kind: librsproto::surface::POINTER_BUTTON,
            button: 0x110,
            buttons: u16::from(pressed),
            flags: if pressed { librsproto::surface::POINTER_PRESSED } else { 0 },
            x: at.origin.x + at.size.w as i32 / 2,
            y: at.origin.y + at.size.h as i32 / 2,
            ..Default::default()
        };
        for m in r.pointer(&tree, &e, &l, ev).0 {
            a.update(m);
        }
    }
}

/// A press on a drive's button ejects that drive by its mount name; a press on its row opens
/// it. `DATA-2`'s button ejects `DATA-2`, not the other `DATA`.
#[test]
fn a_press_on_a_drives_button_ejects_it_and_on_its_row_opens_it() {
    let mut a = with_drives();
    click_key(&mut a, DRIVE_EJECT_KEY + 7);
    assert_eq!(a.take_eject().as_deref(), Some("DATA-2"));
    assert_eq!(a.take_goto(), None, "and goes nowhere");
    assert_eq!(a.notice.as_deref(), Some("ejecting DATA-2\u{2026}"));
    click_key(&mut a, DRIVE_EJECT_KEY + 9);
    assert_eq!(a.take_eject().as_deref(), Some("partition 1"), "an unlabelled stick, by its mount name");
    click_key(&mut a, DRIVE_ROW_KEY + 5);
    assert_eq!(a.take_goto().as_deref(), Some("/storage/DATA"));
    assert_eq!(a.take_eject(), None);
    a.update(Msg::Drive(DRIVE_EJECT_KEY + 3));
    assert_eq!(a.take_eject(), None);
    a.show("/storage/DATA", alloc::vec![]);
    let _ = a.view(&UiTheme::default(), None);
    assert_eq!((a.drive_list.selected, a.sidebar.selected), (Some(1), None));
}

/// An eject refused says why; one done says the drive can be removed, even once the table no
/// longer lists it.
#[test]
fn an_eject_is_answered_in_the_strip() {
    let mut a = with_drives();
    a.update(Msg::Drive(DRIVE_EJECT_KEY + 5));
    let _ = a.take_eject();
    a.ejected("DATA", Err("a file on it is still open or mapped"));
    assert_eq!(a.notice.as_deref(), Some("DATA not ejected: a file on it is still open or mapped"));

    a.show("/storage/DATA", alloc::vec![]);
    a.update(Msg::Drive(DRIVE_EJECT_KEY + 5));
    let _ = a.take_eject();
    a.ejected("DATA", Ok(&[String::from("DATA")]));
    assert_eq!(a.notice.as_deref(), Some("DATA ejected: it can be removed"));
    a.set_drives(drives(&storage_table()).into_iter().filter(|d| d.name != "DATA").collect());
    assert_eq!(a.take_goto().as_deref(), Some("/home"), "the tab on it goes home");
    a.show("/home", alloc::vec![]);
    assert_eq!(a.notice.as_deref(), Some("DATA ejected: it can be removed"), "said past the listing");
}

/// An eject that took a stick's two mounts says both, and again as they go, so a tab on the
/// other goes home as ejected.
#[test]
fn an_eject_that_takes_two_mounts_says_both() {
    let mut a = with_drives();
    a.show("/storage/DATA-2", alloc::vec![]);
    a.update(Msg::Drive(DRIVE_EJECT_KEY + 5));
    assert_eq!(a.take_eject().as_deref(), Some("DATA"));
    let both = [String::from("DATA"), String::from("DATA-2")];
    a.ejected("DATA", Ok(&both));
    let said = "DATA, DATA-2 ejected: the drive can be removed";
    assert_eq!(a.notice.as_deref(), Some(said));
    a.set_drives(drives(&storage_table()).into_iter().filter(|d| !both.contains(&d.name)).collect());
    assert_eq!(a.take_goto().as_deref(), Some("/home"), "the tab on DATA-2 goes home");
    a.show("/home", alloc::vec![]);
    assert_eq!(a.notice.as_deref(), Some(said), "as ejected, not as pulled");
}

/// A drive that goes takes every tab inside it home: the current one by a listing now, with a
/// notice that outlives it; another when chosen.
#[test]
fn a_drive_that_goes_takes_its_tabs_home() {
    let mut a = with_drives();
    let tab_at = |a: &mut App, path: &str| {
        a.update(Msg::NewTab);
        let _ = a.take_goto();
        a.show(path, alloc::vec![Entry::file("x")]);
        a.current_tab()
    };
    let at_home = a.current_tab();
    let inside = tab_at(&mut a, "/storage/DATA");
    let other = tab_at(&mut a, "/storage/DATA-2");
    let current = tab_at(&mut a, "/storage/DATA/deeper");
    a.set_drives(drives(&storage_table()).into_iter().filter(|d| d.name != "DATA").collect());
    assert_eq!(a.take_goto().as_deref(), Some("/home"), "the current tab is read at home");
    assert_eq!(a.path(), "/home");
    a.show("/home", alloc::vec![Entry::file("notes.txt")]);
    assert_eq!(a.notice.as_deref(), Some("DATA is gone, so this tab went home"));
    assert_eq!(a.current_tab(), current);
    a.update(Msg::SelectTab(inside));
    assert_eq!(a.path(), "/home");
    assert_eq!(a.take_goto().as_deref(), Some("/home"));
    a.update(Msg::SelectTab(current));
    a.update(Msg::SelectTab(inside));
    assert_eq!(a.take_goto(), None, "read once");
    a.update(Msg::SelectTab(other));
    assert_eq!(a.take_goto(), None);
    assert_eq!(a.path(), "/storage/DATA-2");
    a.update(Msg::SelectTab(at_home));
    assert_eq!(a.take_goto(), None);
    a.notice = None;
    a.set_drives(drives(&storage_table()).into_iter().filter(|d| d.name == "nitrox-root").collect());
    assert_eq!(a.notice.as_deref(), Some("DATA-2 is gone, so a tab on it went home"));
    a.notice = None;
    a.set_drives(alloc::vec![]);
    assert_eq!(a.notice, None);
}

/// A short window still shows a drive: the places give way, so the Drives heading never stands
/// over an empty list. At the start size the places are whole, where `check-media` aims.
#[test]
fn a_short_window_still_shows_a_drive() {
    let cell = libui::layout::FixedCell { w: 8, h: 16 };
    let first = |h: u32| {
        let mut a = with_drives();
        a.resize(Size::new(START_SIZE.w, h));
        let size = a.window_size();
        let e = a.view(&UiTheme::default(), None);
        let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &cell);
        let row = libui::layout::locate(&e, &l, DRIVE_ROW_KEY + 3);
        let last_place = libui::layout::locate(&e, &l, SIDEBAR_ROW_KEY + a.places().len() as u64 - 1);
        (row, last_place)
    };
    for h in [240, 260, 280, 300] {
        assert!(first(h).0.is_some(), "a {h} px window lays out no drive row");
    }
    assert!(first(START_SIZE.h).1.is_some(), "at the start size every place is laid out");
}

/// The drives' keys share nothing with the places', the listing's, the tabs' or the chrome's —
#[test]
fn a_drive_lights_nothing_else() {
    let top = (1u64 << 60) - 1;
    for base in [DRIVE_ROW_KEY, DRIVE_EJECT_KEY] {
        assert!(base > SIDEBAR_ROW_KEY + (1 << 40), "clear of every place");
        assert!(base + top < TAB_KEY_BASE, "and below the tabs");
        assert!(place_of(base).is_none_or(|i| i >= 1 << 40), "a drive is no place");
    }
    assert!(DRIVE_ROW_KEY + top < DRIVE_EJECT_KEY, "rows and buttons never meet");
    for k in [PLACES_KEY, DRIVES_HEADING_KEY, DRIVES_KEY] {
        for chrome in [
            LIST_KEY,
            UP_KEY,
            TITLE_KEY,
            GRIP_KEY,
            STRIP_KEY,
            PATH_KEY,
            NOTICE_KEY,
            SIDEBAR_KEY,
            PROPS_KEY,
            HEADER_KEY,
            STATUS_KEY,
            LIST_PANE_KEY,
            SEARCH_KEY,
            TAB_STRIP_KEY,
            BAR_KEY,
            PROMPT_KEY,
            STRIP_INNER_KEY,
        ] {
            assert_ne!(k, chrome);
        }
    }
    let mut a = with_drives();
    let cell = libui::layout::FixedCell { w: 8, h: 16 };
    let size = a.window_size();
    let mut tree = libui::diff::Tree::new();
    for drives in [alloc::vec![], drives(&storage_table()), alloc::vec![]] {
        a.set_drives(drives);
        let e = a.view(&UiTheme::default(), None);
        let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &cell);
        tree.update(&e, &l).expect("the sidebar diffs as drives come and go");
    }
}

#[test]
fn a_size_reads_exactly_and_roundly() {
    assert_eq!(size_text(0), "0 bytes");
    assert_eq!(size_text(1023), "1023 bytes", "just under a KiB is still exact");
    assert_eq!(size_text(1024), "1.0 KiB (1024 bytes)");
    assert_eq!(size_text(1536), "1.5 KiB (1536 bytes)", "and it rounds rather than truncates");
    assert_eq!(size_text(1024 * 1024), "1.0 MiB (1048576 bytes)");
    assert_eq!(size_text(1024 * 1024 * 1024), "1.0 GiB (1073741824 bytes)");
    assert!(size_text(1_234_567).ends_with("(1234567 bytes)"));
}

#[test]
fn an_unreported_time_is_unknown_rather_than_1970() {
    assert_eq!(modified_text(0), "unknown");
    assert_eq!(modified_text(-1), "unknown", "and so is anything before the epoch");
    assert_eq!(modified_text(i64::MAX), "unknown", "rather than overflowing into a date");
    assert_eq!(modified_text(1_000_000_000), "2001-09-09 01:46:40 UTC");
}

#[test]
fn properties_reports_the_entry_it_was_opened_on() {
    let mut a = app();
    a.show(
        "/home",
        alloc::vec![Entry { mtime: 1_000_000_000, size: 1536, ..Entry::file("notes.txt") }, Entry::dir("work"),],
    );
    a.update(Msg::Press(row(1)));
    a.update(Msg::Choose(Action::Properties));
    assert_eq!(a.dialog(), Some(Dialog::Properties));

    let labels = {
        let mut out = Vec::new();
        fn walk<M>(e: &Element<M>, out: &mut Vec<String>) {
            if let libui::element::Node::Text(t) = &e.node {
                out.push(t.clone());
            }
            for c in e.children() {
                walk(c, out);
            }
        }
        walk(&a.properties_view(&UiTheme::default(), None), &mut out);
        out
    };
    assert!(labels.iter().any(|l| l == "notes.txt"), "{labels:?}");
    assert!(labels.iter().any(|l| l == "/home"), "where it is: {labels:?}");
    assert!(labels.iter().any(|l| l == "File"), "{labels:?}");
    assert!(labels.iter().any(|l| l.starts_with("1.5 KiB")), "{labels:?}");
    assert!(labels.iter().any(|l| l.starts_with("2001-09-09")), "{labels:?}");

    a.show("/elsewhere", alloc::vec![Entry::file("other.txt")]);
    let after = {
        let mut out = Vec::new();
        fn walk<M>(e: &Element<M>, out: &mut Vec<String>) {
            if let libui::element::Node::Text(t) = &e.node {
                out.push(t.clone());
            }
            for c in e.children() {
                walk(c, out);
            }
        }
        walk(&a.properties_view(&UiTheme::default(), None), &mut out);
        out
    };
    assert!(after.iter().any(|l| l == "notes.txt"), "still the entry it opened on: {after:?}");

    a.update(Msg::CloseProperties);
    assert_eq!(a.dialog(), None, "its one button closes it");
}

#[test]
fn a_folder_does_not_claim_to_be_empty() {
    let mut a = app();
    a.show("/home", alloc::vec![Entry::dir("work")]);
    a.update(Msg::Press(row(0)));
    a.update(Msg::Choose(Action::Properties));
    let mut out: Vec<String> = Vec::new();
    fn walk<M>(e: &Element<M>, out: &mut Vec<String>) {
        if let libui::element::Node::Text(t) = &e.node {
            out.push(t.clone());
        }
        for c in e.children() {
            walk(c, out);
        }
    }
    walk(&a.properties_view(&UiTheme::default(), None), &mut out);
    assert!(out.iter().any(|l| l == "Folder"), "{out:?}");
    assert!(!out.iter().any(|l| l.contains("0 bytes")), "a folder's size is not zero: {out:?}");
}

#[test]
fn the_dialog_slot_tracks_what_is_open() {
    let mut a = app();
    a.show("/home", alloc::vec![Entry::file("notes.txt")]);
    assert_eq!(a.dialog(), None, "nothing is open to begin with");
    assert_eq!(a.dialog_dismissed(), None, "and nothing to dismiss");

    a.update(Msg::Press(row(0)));
    a.update(Msg::Choose(Action::Delete));
    assert_eq!(a.dialog(), Some(Dialog::Confirm), "the question is up");

    assert_eq!(a.dialog_dismissed(), Some(Msg::KeepIt));
    assert_eq!(a.dialog_key(KeyEvent::new(1, libkern::abi::KEY_ESC, KEY_DOWN, 0)), Some(Msg::KeepIt));
    assert_eq!(
        a.dialog_key(KeyEvent::new(1, libkern::abi::KEY_ENTER, KEY_DOWN, 0)),
        None,
        "no key deletes — Enter is the obvious candidate and the obvious accident"
    );

    a.update(Msg::KeepIt);
    assert_eq!(a.dialog(), None, "answering closes the slot");
    assert_eq!(a.take_op(), None, "and nothing was removed");
}

#[test]
fn the_location_bar_opens_seeded_and_navigates() {
    let mut a = app();
    assert!(a.location_text().is_none(), "closed to begin with");

    a.update(Msg::Key(KeyEvent::new(1, LOCATION_KEYCODE, KEY_DOWN, MOD_CTRL)));
    assert_eq!(a.location_text().as_deref(), Some("/home"), "seeded with where the tab is, not left blank");

    a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_ESC, KEY_DOWN, 0)));
    a.update(Msg::OpenLocation);
    *a.location.as_mut().expect("open") = TextFieldState::with_text("/system/fonts");
    press_key(&mut a, libkern::abi::KEY_ENTER);
    assert_eq!(a.take_goto().as_deref(), Some("/system/fonts"));
    assert!(a.location_text().is_none(), "and it closed");
}

#[test]
fn a_relative_path_is_joined_to_the_current_directory() {
    let mut a = app();
    a.update(Msg::OpenLocation);
    *a.location.as_mut().expect("open") = TextFieldState::with_text("papers");
    press_key(&mut a, libkern::abi::KEY_ENTER);
    assert_eq!(a.take_goto().as_deref(), Some("/home/papers"));
}

#[test]
fn the_location_bar_refuses_the_answers_that_are_not_answers() {
    let mut a = app();
    a.update(Msg::OpenLocation);
    *a.location.as_mut().expect("open") = TextFieldState::with_text("   ");
    press_key(&mut a, libkern::abi::KEY_ENTER);
    assert!(a.location_text().is_some(), "left open — the person is mid-answer");
    assert_eq!(a.take_goto(), None, "and went nowhere");
    assert_eq!(a.notice.as_deref(), Some("a path, then Enter"));

    press_key(&mut a, libkern::abi::KEY_ESC);
    assert!(a.location_text().is_none(), "Esc closed it");
    assert_eq!(a.take_goto(), None);
    assert_eq!(a.notice.as_deref(), Some("cancelled"));
}

#[test]
fn the_location_bar_holds_the_keyboard_while_it_is_open() {
    let mut a = app();
    a.update(Msg::OpenLocation);
    assert_eq!(a.location_text().as_deref(), Some("/home"));
    press_key(&mut a, libkern::abi::KEY_BACKSPACE);
    assert_eq!(a.location_text().as_deref(), Some("/hom"), "Backspace edited the field");
    assert_eq!(a.take_goto(), None, "and did not go up a directory");
}

/// The two fields in the path strip cannot both be open: `key` feeds the bar while `view` draws
/// the prompt, so both set would eat keystrokes in a field nobody can see.
#[test]
fn opening_one_field_closes_the_other() {
    let mut a = app();
    a.update(Msg::OpenLocation);
    a.update(Msg::Choose(Action::NewFile));
    assert_eq!(a.prompt_len(), Some(0), "the prompt is open");
    assert_eq!(a.location_text(), None, "and the bar is not");
    press_key(&mut a, KEY_X);
    press_key(&mut a, libkern::abi::KEY_ENTER);
    assert_eq!(a.take_goto(), None, "nothing navigated");
    assert!(matches!(a.take_op(), Some(FileOp::Create { .. })), "the file was created");

    let mut a = app();
    a.update(Msg::Choose(Action::NewFolder));
    a.update(Msg::OpenLocation);
    assert_eq!(a.prompt_len(), None, "the prompt is closed");
    assert_eq!(a.location_text().as_deref(), Some("/home"));
}

#[test]
fn a_directory_that_is_not_there_is_reported() {
    let mut a = app();
    a.update(Msg::OpenLocation);
    *a.location.as_mut().expect("open") = TextFieldState::with_text("/nowhere");
    press_key(&mut a, libkern::abi::KEY_ENTER);
    let to = a.take_goto().expect("it tried");
    a.list_failed(&to);
    assert_eq!(a.notice.as_deref(), Some("no such directory: /nowhere"));
}

#[test]
fn a_single_click_selects_and_a_second_one_opens() {
    let at = libdraw::geom::Point::new(40, 60);
    let mut a = app();
    a.show("/home", alloc::vec![Entry::file("notes.txt"), Entry::dir("work")]);
    assert_eq!(names(&a), alloc::vec!["work", "notes.txt"]);

    a.note_press(at, 1_000, 0);
    a.update(Msg::Press(row(1)));
    assert_eq!(a.pane().list.selected, Some(1), "it selected the row");
    assert_eq!(a.take_open(), None, "and did not open it");
    assert_eq!(a.take_goto(), None);

    a.note_press(at, 1_150, 0);
    a.update(Msg::Press(row(1)));
    assert_eq!(a.take_open().as_deref(), Some("/home/notes.txt"), "the second click opened it");

    a.note_press(at, 5_000, 0);
    a.update(Msg::Press(row(0)));
    assert_eq!(a.take_goto(), None, "still just a selection");
    a.note_press(at, 5_150, 0);
    a.update(Msg::Press(row(0)));
    assert_eq!(a.take_goto().as_deref(), Some("/home/work"));
}

#[test]
fn two_slow_clicks_do_not_open() {
    let at = libdraw::geom::Point::new(40, 60);
    let mut a = app();
    a.show("/home", alloc::vec![Entry::file("notes.txt")]);
    a.note_press(at, 1_000, 0);
    a.update(Msg::Press(row(0)));
    a.note_press(at, 1_000 + libui::click::RUN_MS + 1, 0);
    a.update(Msg::Press(row(0)));
    assert_eq!(a.take_open(), None, "two deliberate clicks are not a double click");
    assert_eq!(a.pane().list.selected, Some(0), "and the row is still selected");
}

#[test]
fn enter_opens_without_a_second_press() {
    let mut a = app();
    a.show("/home", alloc::vec![Entry::file("notes.txt")]);
    a.note_press(libdraw::geom::Point::new(40, 60), 1_000, 0);
    a.update(Msg::Press(row(0)));
    assert_eq!(a.take_open(), None);
    press_key(&mut a, libkern::abi::KEY_ENTER);
    assert_eq!(a.take_open().as_deref(), Some("/home/notes.txt"));
}

/// A drag abandons the run, so the click after it is a first click, not number two.
#[test]
fn a_drag_abandons_the_run() {
    let mut a = app();
    a.show("/home", alloc::vec![Entry::file("notes.txt")]);

    // A real drag, not a call to `note_drag`, which a version that never called it would pass.
    a.note_press(libdraw::geom::Point::new(40, 60), 1_000, 0);
    a.update(Msg::Grab(row(0)));
    a.pointer_moved(40, 60, 1);
    assert_eq!(a.pointer_moved(40 + DRAG_SLOP, 60, 1), Gesture::Moved, "travelling past the slop is a drag");
    a.pointer_moved(40 + DRAG_SLOP, 60, 0);
    a.note_press(libdraw::geom::Point::new(40, 60), 1_150, 0);
    a.update(Msg::Press(row(0)));
    assert_eq!(a.take_open(), None, "the click after a drag is a first click");
}

fn dated() -> Vec<Entry> {
    let at = |name: &str, mtime: i64| Entry { mtime, ..Entry::file(name) };
    alloc::vec![at("beta.txt", 300), at(".hidden", 100), at("alpha.txt", 200)]
}

fn names(a: &App) -> Vec<&str> {
    a.entries().iter().map(|e| e.name.as_str()).collect()
}

#[test]
fn a_dot_hides_an_entry_until_the_toggle_asks_for_it() {
    let mut a = app();
    a.show("/home", dated());
    assert_eq!(names(&a), alloc::vec!["alpha.txt", "beta.txt"], "the dot is not listed");

    a.update(Msg::Key(KeyEvent::new(1, HIDDEN_KEYCODE, KEY_DOWN, MOD_CTRL)));
    assert_eq!(a.take_goto().as_deref(), Some("/home"), "the toggle asks for a listing");
    a.show("/home", dated());
    assert_eq!(names(&a), alloc::vec![".hidden", "alpha.txt", "beta.txt"]);

    a.update(Msg::ToggleHidden);
    let _ = a.take_goto();
    a.show("/home", dated());
    assert_eq!(names(&a), alloc::vec!["alpha.txt", "beta.txt"]);
}

#[test]
fn an_order_rearranges_without_a_listing_and_keeps_the_file_selected() {
    let mut a = app();
    a.show("/home", dated());
    assert_eq!(names(&a), alloc::vec!["alpha.txt", "beta.txt"]);
    a.update(Msg::Activate(0));
    let _ = a.take_goto();
    let picked = |a: &App| -> String {
        let i = a.pane().list.selected.expect("a row is selected");
        a.entries()[i].name.clone()
    };
    assert_eq!(picked(&a), "alpha.txt");

    a.update(Msg::SetOrder(libfs::Order::NewestFirst));
    assert_eq!(a.take_goto(), None, "a re-order is not a navigation");
    assert_eq!(names(&a), alloc::vec!["beta.txt", "alpha.txt"], "newest first");
    assert_eq!(picked(&a), "alpha.txt", "the selection followed the file rather than staying on row 0");

    a.update(Msg::SetOrder(libfs::Order::NewestFirst));
    assert_eq!(names(&a), alloc::vec!["beta.txt", "alpha.txt"]);
}

#[test]
fn the_view_menu_says_what_it_is_set_to() {
    let marked = |a: &App| -> Vec<String> {
        a.menu_table()
            .into_iter()
            .find(|m| m.title == "View")
            .expect("a View menu")
            .items
            .iter()
            .filter_map(|it| match it {
                Item::Action { label, marked: true, .. } => Some(String::from(label.as_ref())),
                _ => None,
            })
            .collect()
    };

    let mut a = app();
    assert_eq!(marked(&a), alloc::vec![String::from("Name (A\u{2013}Z)")], "the default");

    a.update(Msg::SetOrder(libfs::Order::OldestFirst));
    assert_eq!(
        marked(&a),
        alloc::vec![String::from("Oldest First")],
        "one order at a time — the old mark moved rather than a second appearing"
    );

    a.update(Msg::ToggleHidden);
    assert_eq!(marked(&a), alloc::vec![String::from("Oldest First"), String::from("Show Hidden Files")]);
    a.update(Msg::ToggleHidden);
    assert_eq!(marked(&a), alloc::vec![String::from("Oldest First")], "and back off again");
}

#[test]
fn a_tab_chord_while_naming_still_opens_a_tab() {
    // The chords are checked before the prompt, or the field's branch returns first. The name is
    // asserted unchanged too, which is the field's doing: `apply` drops control characters.
    let mut a = app();
    a.update(Msg::Choose(Action::NewFolder));
    press_key(&mut a, KEY_X);
    assert_eq!(a.prompt_len(), Some(1));

    a.update(Msg::Key(KeyEvent::new(1, NEW_TAB_KEYCODE, KEY_DOWN, MOD_CTRL)));
    assert_eq!(a.tab_count(), 2, "the chord opened a tab");
    assert_eq!(a.prompt_len(), Some(1), "and typed nothing into the name");
}

#[test]
fn up_leaves_a_directory_and_stops_at_the_root() {
    let mut a = App::new("/home/work");
    a.update(Msg::Up);
    assert_eq!(a.take_goto().as_deref(), Some("/home"));

    let mut a = App::new("/home");
    a.update(Msg::Up);
    assert_eq!(a.take_goto().as_deref(), Some("/"));

    let mut a = App::new("/");
    a.update(Msg::Up);
    assert_eq!(a.take_goto(), None);
}

#[test]
fn the_keyboard_and_the_pointer_reach_the_same_three_messages() {
    let mut a = app();
    for _ in 0..2 {
        a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_DOWN, KEY_DOWN, 0)));
    }
    a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_ENTER, KEY_DOWN, 0)));
    assert_eq!(a.take_goto(), None, "the third row is a file");

    let mut a = app();
    a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_DOWN, KEY_DOWN, 0)));
    a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_ENTER, KEY_DOWN, 0)));
    assert_eq!(a.take_goto().as_deref(), Some("/home/work"), "the second row is a directory");
}

#[test]
fn backspace_goes_up() {
    let mut a = App::new("/home/work");
    a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_BACKSPACE, KEY_DOWN, 0)));
    assert_eq!(a.take_goto().as_deref(), Some("/home"));
}

#[test]
fn a_new_listing_resets_the_selection_rather_than_keeping_an_index() {
    // A different directory has no row the old selection refers to. Two presses, not one: after
    // one the stale `Some(0)` equals what correct code produces.
    let mut a = app();
    for _ in 0..2 {
        a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_DOWN, KEY_DOWN, 0)));
    }
    assert_eq!(a.list_selected(), Some(2), "precondition: the selection has moved");
    a.show("/home/work", alloc::vec![Entry::file("only.txt")]);
    assert_eq!(a.list_selected(), Some(0));
}

#[test]
fn an_empty_directory_selects_nothing() {
    let mut a = app();
    a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_DOWN, KEY_DOWN, 0)));
    assert!(a.list_selected().is_some(), "precondition: something is selected");
    a.show("/home/empty", Vec::new());
    assert_eq!(a.list_selected(), None, "nothing to select, and no phantom row 0");
    assert!(a.entries().is_empty());
}

#[test]
fn a_configure_is_accepted_and_the_listing_gets_the_room() {
    let mut a = app();
    let before = a.list_h();
    assert!(a.resize(Size::new(800, 600)));
    assert!(a.list_h() > before);
    assert!(!a.resize(Size::new(800, 600)), "the same size is not a change");
}
/// The window's own tree survives being diffed frame to frame across the menu states: a parent
/// whose children are partly keyed makes the window undiffable, which no layout or paint test sees.
#[test]
fn the_window_tree_diffs_across_the_menu_states() {
    let mut a = app();
    let theme = UiTheme::default();
    let mut tree = libui::diff::Tree::new();
    let cell = libui::layout::FixedCell { w: 8, h: 16 };
    let check = |a: &mut App, hovered, tree: &mut libui::diff::Tree, what: &str| {
        let size = a.window_size();
        let e = a.view(&theme, hovered);
        let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &cell);
        tree.update(&e, &l).unwrap_or_else(|err| panic!("{what}: {err:?}"));
    };
    check(&mut a, None, &mut tree, "with the menu shut");
    check(&mut a, Some(MENU_BAR_KEY), &mut tree, "with a bar word hovered");
    a.update(Msg::MenuBar(0));
    check(&mut a, Some(MENU_BAR_KEY), &mut tree, "with File open and hovered");
    a.update(Msg::MenuBar(1));
    check(&mut a, None, &mut tree, "with Edit open and nothing hovered");
    a.update(Msg::MenuBar(1));
    check(&mut a, None, &mut tree, "back to shut");
}

/// The bar words are where `check-login` aims, a number spelled as constants in the gate
/// (`(fx + 20, fy + 1 + TITLE_BAR_H + MENU_BAR_H / 2)` for File); if the word moves, this names it.
#[test]
fn the_gate_aims_inside_the_file_word() {
    let mut a = app();
    let cell = libui::layout::FixedCell { w: 8, h: 16 };
    let size = a.window_size();
    let view = a.view(&UiTheme::default(), None);
    let l = libui::layout::layout(&view, Rect::new(0, 0, size.w, size.h), &cell);
    let file = libui::layout::locate(&view, &l, MENU_BAR_KEY).expect("the File word is keyed");
    let edit = libui::layout::locate(&view, &l, MENU_BAR_KEY + 1).expect("the Edit word is keyed");

    let (ax, ay) = (20, 1 + TITLE_BAR_H as i32 + MENU_BAR_H as i32 / 2);
    assert!(file.contains(ax, ay), "check-login aims at ({ax}, {ay}) for the File menu and the word is at {file:?}");
    assert!(!edit.contains(ax, ay), "the aim point is inside Edit as well: File {file:?}, Edit {edit:?}");
}

fn ids(t: &libui::diff::Tree) -> Vec<u64> {
    fn walk(w: &libui::diff::Widget, out: &mut Vec<u64>) {
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

/// Every menu in the bar can be opened, not just the ones a test named: a constant count once
/// left a bar word with nowhere to hang from.
#[test]
fn every_menu_in_the_bar_has_somewhere_to_hang_from() {
    let mut a = app();
    let cell = libui::layout::FixedCell { w: 8, h: 16 };
    let size = a.window_size();
    let view = a.view(&UiTheme::default(), None);
    let l = libui::layout::layout(&view, Rect::new(0, 0, size.w, size.h), &cell);
    let n = a.menu_count();
    assert!(n >= 2, "a bar with fewer than two menus is not this window's");
    a.place_menus(&view, &l);
    for i in 0..n {
        a.menus.toggle(i);
        assert_eq!(a.menus.open(), Some(i));
        assert!(
            a.menus.anchor().is_some(),
            "menu {i} of {n} opened with nowhere to hang from, so nothing would be drawn"
        );
        a.menus.close();
    }
}

/// The quick-access panel sits where [`SIDEBAR_PAD`] says, and fills its column down to the
/// status bar: sized to the listing's height alone, the window showed through beneath it.
#[test]
fn the_quick_access_panel_sits_where_the_metric_says_and_fills_its_column() {
    let mut a = app();
    let cell = libui::layout::FixedCell { w: 8, h: 16 };
    let size = a.window_size();
    let e = a.view(&UiTheme::default(), None);
    let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &cell);
    let box_ = libui::layout::locate(&e, &l, SIDEBAR_KEY).expect("the sidebar is keyed");
    let row = libui::layout::locate(&e, &l, SIDEBAR_ROW_KEY).expect("its first row is keyed");
    assert_eq!(row.origin.x - box_.origin.x, SIDEBAR_PAD as i32, "the panel is flush with the window's edge");
    assert_eq!(
        (box_.origin.x + box_.size.w as i32) - (row.origin.x + row.size.w as i32),
        SIDEBAR_PAD as i32,
        "the panel is flush with the listing"
    );
    let status = libui::layout::locate(&e, &l, STATUS_KEY).expect("the status bar is keyed");
    assert_eq!(
        box_.origin.y + box_.size.h as i32,
        status.origin.y,
        "the panel stops short of the status bar, leaving the window showing through"
    );
    let pane = libui::layout::locate(&e, &l, LIST_PANE_KEY).expect("the pane is keyed");
    assert_eq!(pane.origin.y + pane.size.h as i32, status.origin.y, "the listing stops short of the status bar");
}

/// A scrollbar drag survives the repaint that follows it: `list_view` follows the selection on
/// every build, so a drag computed the right offset and the next frame put it back on row 0.
#[test]
fn a_scrollbar_drag_survives_the_next_repaint() {
    let mut a = app();
    a.show("/home", (0..80).map(|i| Entry::file(&alloc::format!("f{i}"))).collect());
    let cell = libui::layout::FixedCell { w: 8, h: 16 };
    let size = a.window_size();
    let theme = UiTheme::default();
    let mut tree = libui::diff::Tree::new();
    let mut r = libui::route::Router::new();
    let frame = |a: &mut App, tree: &mut libui::diff::Tree| {
        let e = a.view(&theme, None);
        let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &cell);
        tree.update(&e, &l).expect("diffable");
        (e, l)
    };

    let (e, l) = frame(&mut a, &mut tree);
    let x = size.w as i32 - 8;
    let press = librsproto::surface::PointerEvent {
        kind: librsproto::surface::POINTER_BUTTON,
        button: 0x110,
        buttons: 1,
        flags: librsproto::surface::POINTER_PRESSED,
        x,
        y: (size.h / 2) as i32,
        ..Default::default()
    };
    for m in r.pointer(&tree, &e, &l, press).0 {
        a.update(m);
    }
    let scrolled = a.pane().list.offset;
    assert!(scrolled > 0, "the press on the bar scrolled nowhere");

    frame(&mut a, &mut tree);
    assert_eq!(a.pane().list.offset, scrolled, "the repaint put the listing back where its selection is");
}

/// The wheel scrolls the listing through the real tree and router: only this sees that the
/// element `list_view` returns carries an `on_wheel` at all.
#[test]
fn the_wheel_scrolls_the_listing() {
    let mut a = app();
    a.show("/home", (0..80).map(|i| Entry::file(&alloc::format!("f{i}"))).collect());
    let cell = libui::layout::FixedCell { w: 8, h: 16 };
    let size = a.window_size();
    let theme = UiTheme::default();
    let e = a.view(&theme, None);
    let l = libui::layout::layout(&e, Rect::new(0, 0, size.w, size.h), &cell);
    let mut tree = libui::diff::Tree::new();
    tree.update(&e, &l).expect("the view is diffable");
    let mut router = libui::route::Router::new();

    let ev = librsproto::surface::PointerEvent {
        kind: librsproto::surface::POINTER_WHEEL,
        wheel: 2,
        x: (size.w / 2) as i32,
        y: (size.h / 2) as i32,
        ..Default::default()
    };
    let (msgs, _) = router.pointer(&tree, &e, &l, ev);
    assert!(!msgs.is_empty(), "nothing over the listing took the wheel");
    for m in msgs {
        a.update(m);
    }
    assert_eq!(
        a.pane().list.offset,
        2 * libui::click::WHEEL_UNITS as usize,
        "two detents down is two detents' worth of rows"
    );
}

/// A click on the menu bar opens the menu even when its two halves fall in different frames.
/// The halves split whenever the click raises the window, and the frame between draws the word
/// hovered, re-identifying every node inside it; `Router` must capture the word itself.
#[test]
fn a_click_split_across_a_repaint_still_opens_the_menu() {
    let mut a = app();
    let cell = libui::layout::FixedCell { w: 8, h: 16 };
    let size = a.window_size();
    let theme = UiTheme::default();
    let mut tree = libui::diff::Tree::new();
    let mut router = libui::route::Router::new();

    let rest = a.view(&theme, None);
    let l = libui::layout::layout(&rest, Rect::new(0, 0, size.w, size.h), &cell);
    tree.update(&rest, &l).expect("the view is diffable");
    let word = libui::layout::locate(&rest, &l, MENU_BAR_KEY).expect("the File word is keyed");
    let at = (word.origin.x + word.size.w as i32 / 2, word.origin.y + word.size.h as i32 / 2);

    let ev = |flags: u16, buttons: u16| librsproto::surface::PointerEvent {
        kind: librsproto::surface::POINTER_BUTTON,
        button: 0x110,
        buttons,
        flags,
        x: at.0,
        y: at.1,
        ..Default::default()
    };
    let (msgs, _) = router.pointer(&tree, &rest, &l, ev(librsproto::surface::POINTER_PRESSED, 1));
    assert!(msgs.is_empty(), "a press is not a click");

    // Frame 2: a repaint between the two halves, with the word lit explicitly: crossings are
    // suppressed during a capture, so a frame built from `hovered_key` would equal frame 1 and
    // pass against the bug.
    let lit = a.view(&theme, Some(MENU_BAR_KEY));
    let l2 = libui::layout::layout(&lit, Rect::new(0, 0, size.w, size.h), &cell);
    let before = ids(&tree);
    tree.update(&lit, &l2).expect("the view is diffable");
    // The precondition, asserted: if `menu_item` stops changing shape on hover there is
    // nothing to survive and the test would prove nothing.
    assert_ne!(ids(&tree), before, "the repaint re-identified nothing, so this proves nothing");

    let (msgs, _) = router.pointer(&tree, &lit, &l2, ev(0, 0));
    for m in msgs {
        a.update(m);
    }
    assert_eq!(
        a.menus.open(),
        Some(0),
        "the click was dropped because the repaint between its halves re-identified the word"
    );
}

/// A press on a row produces `Grab`, the message every other drag test injects by hand.
#[test]
fn a_press_on_a_row_grabs_it() {
    let mut a = app();
    let cell = libui::layout::FixedCell { w: 8, h: 16 };
    let size = a.window_size();
    let view = a.view(&UiTheme::default(), None);
    let l = libui::layout::layout(&view, Rect::new(0, 0, size.w, size.h), &cell);
    let mut tree = libui::diff::Tree::new();
    tree.update(&view, &l).expect("the view is diffable");
    let mut router = libui::route::Router::new();

    let (msgs, _) = router.pointer(
        &tree,
        &view,
        &l,
        librsproto::surface::PointerEvent {
            kind: librsproto::surface::POINTER_BUTTON,
            button: 0x110,
            buttons: 1,
            flags: librsproto::surface::POINTER_PRESSED,
            x: SIDEBAR_W as i32 + 120,
            y: row_y(&a, 1),
            ..Default::default()
        },
    );
    assert!(
        msgs.iter().any(|m| matches!(m, Msg::Grab(_))),
        "a press on a row produced {msgs:?}, so no drag can start from one"
    );
}

/// A drag that leaves the window in **one** motion still hands off: the hand-off once lived
/// only in the already-dragging branch, so a motion that crossed the slop and left armed a drag
/// and sat there.
#[test]
fn a_drag_that_leaves_in_one_motion_still_hands_off() {
    let mut a = app();
    a.update(Msg::Grab(row(2))); // a.txt, a file
    assert_eq!(a.pointer_moved(120, row_y(&a, 2), 1), Gesture::None);
    let far = a.window_size().w as i32 + 200;
    assert_eq!(
        a.pointer_moved(far, row_y(&a, 2) + 240, 1),
        Gesture::HandOff,
        "a drag that left the window in one step never asked the compositor to carry it"
    );
    assert!(a.take_drag().is_some(), "and it carries the entry that was pressed");
}

/// The negative control: a drag that stays inside is still the browser's own.
#[test]
fn a_drag_that_stays_inside_does_not_hand_off() {
    let mut a = app();
    a.update(Msg::Grab(row(2)));
    assert_eq!(a.pointer_moved(120, row_y(&a, 2), 1), Gesture::None);
    let inside = a.pointer_moved(120, row_y(&a, 1), 1);
    assert!(
        matches!(inside, Gesture::Moved | Gesture::None),
        "a drag inside the window is the browser's own, not the compositor's: {inside:?}"
    );
    assert!(a.take_drag().is_none(), "nothing was handed over");
    let far = a.window_size().w as i32 + 200;
    assert_eq!(a.pointer_moved(far, row_y(&a, 1), 1), Gesture::HandOff);
}

/// `check-login` clicks a menu row by index; the gate cannot link this crate, so it spells the
/// row as a number, and this makes that number wrong here instead of in a three-minute boot.
#[test]
fn the_gate_clicks_the_row_it_means() {
    // The constant in `tools/xtask/src/gates/login/clipboard.rs`, as `MENU_ROW_KEY + n`.
    const RENAME_ROW: usize = 9;
    let file = &app().menu_table()[0];
    assert!(file.title == "File", "the first menu is File");
    match &file.items[RENAME_ROW] {
        Item::Action { label, msg, .. } => {
            assert_eq!(*label, "Rename", "check-login's RENAME_ROW is not Rename any more");
            assert_eq!(*msg, Msg::Choose(Action::Rename));
        }
        Item::Separator => panic!("check-login's RENAME_ROW is a rule, not a row"),
    }
}
