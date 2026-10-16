    use super::*;

    /// The text under the element keyed `key`, joined — for asserting on chrome without
    /// hard-coding where in the tree it sits.
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

    /// A listing row's element key, which is what `Msg::Press` and `Msg::Grab` carry.
    ///
    /// **Rows are keyed from `LIST_ROW_KEY`, not from zero** — see its doc for the aliasing that
    /// forced it. A test naming a bare index is naming a key the tree never produces.
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

    /// Three consecutive frames must diff, and **the frame that fails tells you which mistake
    /// it is.**
    ///
    /// `MixedKeying` — a container with some keyed children and some not — fails at frame **0**,
    /// because the check runs against an empty previous list for a new node as well. In this
    /// application that is `fail()` before the first commit, so no window ever appears.
    /// `KeyingChanged` needs a successful first frame to compare against and fails at frame 1,
    /// which is the window that paints once and then goes dead. Reporting the frame number is
    /// what separates them.
    ///
    /// Found the expensive way, in a four-minute boot, when this would have found it in a
    /// millisecond — and it did, for the second instance, at frame 0 (M10 Part B).
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

    /// A listing long enough to scroll.
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
        // **`list_view` scrolls the state it is handed, and a caller that keeps none re-derives
        // the offset from zero every frame.** The selection then sits on the *last visible row*
        // for ever: press Up and the highlight does not move, the entire listing scrolls down
        // by one underneath it — and the whole list area repaints instead of two rows
        // (PR #257 review, blocking 1). The signature took its state by value then and returned
        // the scrolled copy; since Part C it takes `&mut`, so the drop this catches now needs a
        // deliberate `&mut x.clone()` rather than a `_` in a pattern.
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
        // **The order is the browser's, not the filesystem's.** Entries arrive in whatever order
        // the directory server and the namespace enumeration produced, which is not an order
        // anybody chose — and a listing that changed order between two visits to the same
        // directory would be unusable for the one thing a browser is for.
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
        // **The two halves of what a row press means**, and the reason they are one message:
        // Enter and a press must agree about "open", or there are two browsers. Which of the
        // two outboxes fills is the entry's kind, and nothing else.
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
        // **A row press with no visible effect is indistinguishable from a broken one.** The
        // window the shell launches belongs to another process and may take a moment; what this
        // browser can say is whether the request was taken.
        let mut a = app();
        a.update(Msg::Activate(2));
        let path = a.take_open().unwrap();
        a.opened(&path, true);
        let ui: Element<Msg> = a.view(&UiTheme::default(), None);
        assert!(labelled(&ui, NOTICE_KEY).contains("opening a.txt"), "the strip says so");

        a.opened(&path, false);
        let ui: Element<Msg> = a.view(&UiTheme::default(), None);
        assert!(labelled(&ui, NOTICE_KEY).contains("could not open a.txt"));

        // And a new listing supersedes it: the notice is about a press, not about the directory.
        a.show("/home", alloc::vec![Entry::file("a.txt")]);
        let ui: Element<Msg> = a.view(&UiTheme::default(), None);
        assert_eq!(labelled(&ui, NOTICE_KEY), "", "a listing clears it");
    }

    /// A point outside a `START_SIZE` window, which is what hands a drag to the compositor.
    const OUTSIDE: (i32, i32) = (START_SIZE.w as i32 + 10, 100);

    #[test]
    fn a_press_becomes_a_drag_only_after_it_has_travelled() {
        // **A click is a press that moved a little.** Nobody presses a button without nudging
        // it, so a browser that started a drag on the first pixel would make opening a file by
        // clicking it a matter of luck.
        let mut a = app();
        a.update(Msg::Grab(row(2))); // a.txt
        assert_eq!(a.pointer_moved(100, 100, 1), Gesture::None, "the first motion fixes the origin");
        assert_eq!(a.pointer_moved(100 + DRAG_SLOP - 1, 100, 1), Gesture::None, "a nudge is not a drag");
        assert_eq!(a.pointer_moved(100 + DRAG_SLOP, 100, 1), Gesture::Moved, "and travelling is");

        // **And travelling does not reach the compositor** (M12 Part B): the drag is this
        // window's until it leaves. Asking for it here is asking for a payload that has not been
        // handed over.
        assert_eq!(a.take_drag(), None, "an internal drag is not the compositor's");

        assert_eq!(a.pointer_moved(OUTSIDE.0, OUTSIDE.1, 1), Gesture::HandOff, "out of the window");
        let (entry, path) = a.take_drag().expect("the row that was pressed");
        assert_eq!((entry.name.as_str(), entry.is_dir), ("a.txt", false));
        assert_eq!(path, "/home/a.txt");
        assert_eq!(a.take_drag(), None, "and it is offered once");
    }

    #[test]
    fn releasing_without_travelling_leaves_no_drag_behind() {
        // The record has to be cleared by the button coming up, or the *next* press-free motion
        // would start a drag for a row nobody is holding.
        let mut a = app();
        a.update(Msg::Grab(row(0)));
        assert_eq!(a.pointer_moved(10, 10, 1), Gesture::None);
        assert_eq!(a.pointer_moved(10, 10, 0), Gesture::None, "the button came up");
        assert_eq!(a.pointer_moved(400, 400, 1), Gesture::None, "a later motion carries nothing");
        assert_eq!(a.take_drag(), None);
    }

    #[test]
    fn a_listing_that_changes_under_a_press_carries_nothing() {
        // **Reachable, not theoretical**: the press that starts this also raised the window, so
        // the keyboard is here — Backspace between the press and the move is one keystroke away.
        // An index remembered across that names a different row; a name names none.
        let mut a = app();
        a.update(Msg::Grab(row(2))); // a.txt, in /home
        assert_eq!(a.pointer_moved(10, 10, 1), Gesture::None, "the press is recorded");

        a.show("/", alloc::vec![Entry::dir("bin"), Entry::dir("dev"), Entry::file("zzz")]);
        assert_eq!(a.pointer_moved(10 + DRAG_SLOP, 10, 1), Gesture::None, "the row it was on is gone");
        assert_eq!(a.pointer_moved(OUTSIDE.0, OUTSIDE.1, 1), Gesture::None, "so nothing leaves");
        assert_eq!(a.take_drag(), None, "and nothing is offered");

        // And a listing that still holds the row drags *that* row, whatever moved around it.
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
        // The kind is the entry's, not the gesture's: an editor that takes files only must not
        // be highlighted for a folder, and the compositor decides that from what this says.
        let mut a = app();
        a.update(Msg::Grab(row(0))); // archive/
        a.pointer_moved(10, 10, 1);
        a.pointer_moved(10 + DRAG_SLOP, 10, 1);
        a.pointer_moved(OUTSIDE.0, OUTSIDE.1, 1);
        let (entry, path) = a.take_drag().expect("a drag");
        assert!(entry.is_dir, "the browser reports what the row is");
        assert_eq!(path, "/home/archive");
    }

    /// One key press, as the compositor delivers one.
    fn press_key(a: &mut App, code: u16) {
        a.update(Msg::Key(KeyEvent::new(1, code, KEY_DOWN, 0)));
    }

    /// `x`, the one letter these tests type, and `/`, the one they must not be allowed to.
    ///
    /// Literals because `libkern::abi` names only the keys with no character; the test below
    /// pins both against `libinput`'s table rather than against this comment.
    const KEY_X: u16 = 45;
    /// See [`KEY_X`].
    const KEY_SLASH: u16 = 53;

    #[test]
    fn the_typed_keycodes_are_the_ones_the_keymap_calls_x_and_slash() {
        assert_eq!(libinput::keymap::to_char(KEY_X, 0), Some(b'x'));
        assert_eq!(libinput::keymap::to_char(KEY_SLASH, 0), Some(b'/'));
    }

    /// The middle of row `i`, in window coordinates.
    fn row_y(a: &App, i: u32) -> i32 {
        (a.list_top() + ROW_H * i + ROW_H / 2) as i32
    }

    /// Select row `i` with the arrow keys, which is what a person has to do first.
    ///
    /// **From the top every time**, rather than `i` steps from wherever the selection happens to
    /// be: a helper that moved *relatively* makes every test after the first one in a function
    /// depend on where the last one left it.
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
        // **A menu row that silently does nothing is a control that looks live and is not** —
        // the defect M8's overview shipped three of. Two of the five act on the directory and
        // three on a row, and the three have to say so when there is no row.
        let mut a = App::new("/home");
        a.show("/home", alloc::vec![]);
        for action in [Action::Rename, Action::Duplicate, Action::Delete] {
            a.update(Msg::Choose(action));
            assert!(a.take_op().is_none(), "{action:?} did something with nothing selected");
            assert!(a.confirming().is_none(), "{action:?} asked about nothing");
            let ui: Element<Msg> = a.view(&UiTheme::default(), None);
            assert!(labelled(&ui, NOTICE_KEY).contains("nothing is selected"), "{action:?} said nothing");
        }
        // And the two that make something do not need one.
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
        // **The strip shows the prompt in place of the path**, not beside it.
        let ui: Element<Msg> = a.view(&UiTheme::default(), None);
        assert_eq!(labelled(&ui, PATH_KEY), "rename to:", "the verb replaces the path");

        press_key(&mut a, KEY_X);
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(a.take_op(), Some(FileOp::Rename { from: "/home/a.txt".into(), to: "/home/x".into() }));

        // Copy is the same shape and a different operation — the one that reaches `copy_file`.
        select(&mut a, 2);
        a.update(Msg::Choose(Action::Duplicate));
        press_key(&mut a, KEY_X);
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(a.take_op(), Some(FileOp::Copy { from: "/home/a.txt".into(), to: "/home/x".into(), dir: false }));
    }

    #[test]
    fn a_prompt_takes_a_name_and_not_a_path() {
        // **A separator would be a way to write outside the directory being shown by typing
        // into it.** This browser shows one directory; it is not a command line.
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
        // **Backspace especially**: it goes *up a directory* the rest of the time, and a person
        // correcting a typo would otherwise leave the directory they are naming something in.
        let mut a = app();
        a.update(Msg::Choose(Action::NewFile));
        press_key(&mut a, libkern::abi::KEY_BACKSPACE);
        assert!(a.take_goto().is_none(), "Backspace edited the field, not the path");
        // And the arrows do not move the selection out from under a rename in progress.
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

        // Keeping it removes the question and nothing else.
        a.update(Msg::KeepIt);
        assert!(a.confirming().is_none());
        assert!(a.take_op().is_none(), "keeping must not delete");

        // Asked again, and answered the other way.
        a.update(Msg::Choose(Action::Delete));
        a.update(Msg::ConfirmDelete);
        assert!(a.confirming().is_none(), "the question goes when it is answered");
        assert_eq!(a.take_op(), Some(FileOp::Delete { path: "/home/a.txt".into(), dir: false }));

        // A directory carries `dir`, which is what decides whether the removal descends.
        let mut a = app();
        a.update(Msg::Choose(Action::Delete)); // row 0 is `archive/`
        a.update(Msg::ConfirmDelete);
        assert_eq!(a.take_op(), Some(FileOp::Delete { path: "/home/archive".into(), dir: true }));
    }

    #[test]
    fn the_dialog_answers_escape_and_nothing_else() {
        // No key deletes. `Enter` is the obvious candidate and the obvious accident, and this is
        // the one operation in the browser that cannot be undone.
        let a = app();
        let ev = |code: u16, pressed: u16| KeyEvent::new(1, code, pressed, 0);
        assert_eq!(a.confirm_key(ev(libkern::abi::KEY_ESC, KEY_DOWN)), Some(Msg::KeepIt));
        assert_eq!(a.confirm_key(ev(libkern::abi::KEY_ENTER, KEY_DOWN)), None);
        assert_eq!(a.confirm_key(ev(libkern::abi::KEY_ESC, 0)), None, "a release is not an answer");
    }

    #[test]
    fn a_drag_that_stays_inside_moves_into_the_directory_it_is_released_over() {
        // **The half M10 Part E did not build.** The compositor is not involved and could not
        // be: it skips the source window when it looks for a drop target, so a drag that came
        // out of this list can never be delivered back to it.
        let mut a = app();
        a.update(Msg::Grab(row(2))); // a.txt
        a.pointer_moved(100, row_y(&a, 2), 1);
        assert_eq!(a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 2), 1), Gesture::Moved);
        // Over `work/`, which is a directory and therefore a target.
        assert_eq!(a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 1), Gesture::Moved);
        assert_eq!(a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 0), Gesture::Dropped);
        assert_eq!(a.take_op(), Some(FileOp::MoveInto { from: "/home/a.txt".into(), to: "/home/work/a.txt".into() }));
        assert_eq!(a.take_drag(), None, "and the compositor was never told");
    }

    #[test]
    fn only_a_directory_is_a_drop_target() {
        // A file row is not a destination, and a directory dropped on itself is a rename to a
        // path underneath the thing being moved.
        let mut a = app();
        a.update(Msg::Grab(row(2))); // a.txt
        a.pointer_moved(100, row_y(&a, 2), 1);
        a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 2), 1);
        // Row 3 is `notes.txt` — a file.
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
        // **What the view draws the highlight from.** It borrows the list's own hover face, so a
        // drop target and a pointer highlight cannot come to look different — and the widget
        // needs no state for it.
        let mut a = app();
        a.update(Msg::Grab(row(2))); // a.txt
        a.pointer_moved(100, row_y(&a, 2), 1);
        a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 2), 1);
        assert_eq!(a.drop_target(), None, "over the file it came from");
        a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 1);
        assert_eq!(a.drop_target(), Some(1), "over `work/`");
        a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 3), 1);
        assert_eq!(a.drop_target(), None, "over a file again");
        // Off the list entirely — the chrome above it is not a row.
        a.pointer_moved(100 + DRAG_SLOP, 4, 1);
        assert_eq!(a.drop_target(), None, "over the title bar");
        // And it is cleared when the gesture ends, or the next frame would highlight a row
        // nobody is dragging onto.
        a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 1);
        a.pointer_moved(100 + DRAG_SLOP, row_y(&a, 1), 0);
        assert_eq!(a.drop_target(), None);
    }

    #[test]
    fn the_menus_hold_what_they_say() {
        // **Cut and paste are absent on purpose**, and this is where that stays true: a pair
        // that holds something between two gestures is a clipboard, and M12 Part E builds the
        // real one. An `Edit` menu that grew them here would be a second.
        let mut a = app();
        a.update(Msg::MenuBar(0));
        let file: Element<Msg> = a.menu_view(0, &UiTheme::default(), None);
        // **Row keys are positional now**, so the labels are read in order — which also pins the
        // separator's place, since a rule occupies a row index without being one.
        // The label *and its chord*, because `labelled` concatenates the row's text and the
        // accelerator column is text in the same row — which makes this the place the advertised
        // chords are pinned as strings rather than as `Accel` values.
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
        // Row 3 is the separator: it is keyed by nothing, so nothing is found at its index.
        for rule in [3, 6] {
            assert_eq!(labelled(&file, MENU_ROW_KEY + rule), "", "row {rule} is a rule, not an item");
        }

        a.update(Msg::MenuBar(1));
        let edit: Element<Msg> = a.menu_view(1, &UiTheme::default(), None);
        // **Cut and paste live here now**, and the note this replaces said why they could not:
        // they are a pair that holds something between two gestures, which is a clipboard however
        // it is spelled, so an Edit menu that grew them before M12 Part E built the real one would
        // have shipped a second clipboard. Part E built it and M14 Part D gave it a path kind.
        for (i, item) in [(0, "CutCtrl+X"), (1, "CopyCtrl+C"), (2, "PasteCtrl+V"), (4, "Duplicate\u{2026}")] {
            assert_eq!(labelled(&edit, MENU_ROW_KEY + i), item, "the Edit menu's row {i}");
        }
        assert_eq!(labelled(&edit, MENU_ROW_KEY + 3), "", "row 3 is a rule");
        assert_eq!(labelled(&edit, MENU_ROW_KEY + 5), "", "and there is no row after Duplicate");
    }

    /// A row that would be refused is drawn unavailable rather than offered.
    ///
    /// **The half of the affordance that was missing** (M14 Part A). `choose` has always answered
    /// "nothing is selected" in the strip; what it could not do is stop the row looking live.
    #[test]
    fn rows_that_need_a_selection_are_disabled_without_one() {
        // **An empty directory is how there is no selection**: `show` selects the first row of
        // any listing that has one, so this is the real state a person reaches rather than a
        // constructed one.
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
        // **The negative control.** Give it a listing and the same three become available —
        // without it this test would pass for a version that disabled everything unconditionally.
        a.show("/empty", alloc::vec![Entry::file("a"), Entry::file("b")]);
        assert_eq!(needs(&a, "File"), alloc::vec![true, true, true]);
        assert_eq!(needs(&a, "Edit"), alloc::vec![true]);
        // …and the ones that do not need a selection were never affected either way.
        let file = a.menu_table().into_iter().next().expect("File");
        assert!(matches!(file.items[7], Item::Action { enabled: true, .. }), "New File does not act on a selection");
    }

    /// Close Tab is offered only when closing one leaves a browser behind.
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
        // A bar item that only opened would leave the menu up until something else dismissed
        // it, and what dismisses a popup is a press on another window.
        let mut a = app();
        assert_eq!(a.menus.open(), None);
        a.update(Msg::MenuBar(0));
        assert_eq!(a.menus.open(), Some(0));
        a.update(Msg::MenuBar(0));
        assert_eq!(a.menus.open(), None, "the same item closes it");
        // And the other item swaps rather than stacking.
        a.update(Msg::MenuBar(0));
        a.update(Msg::MenuBar(1));
        assert_eq!(a.menus.open(), Some(1));
        // Choosing anything puts the menu away, or it would sit over the answer. **Every row,
        // not the one this used to check** — the rule is asked of the table now, so this walks it.
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
        // **A window is taller than the rows it has room for** — up to a row's worth, plus the
        // grip and the frame — and a `y` in that band is still inside the window and still under
        // the pointer grab. Bounding a drop against `entries.len()` rather than against the rows
        // `list_view` actually drew mapped it to a directory that was never on screen and never
        // highlighted, so a file moved into a folder the person could not see (PR #268 review,
        // blocking 1).
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
        // Two pixels below the last drawn row, and well inside the window. The gesture reports
        // `Moved` because the *highlight* changed — it had to be cleared — and what matters is
        // what it changed to.
        let below = last_drawn_bottom + 2;
        assert_eq!(a.pointer_moved(100 + DRAG_SLOP, below, 1), Gesture::Moved);
        assert_eq!(a.drop_target(), None, "nothing is drawn there");
        assert_eq!(a.pointer_moved(100 + DRAG_SLOP, below, 0), Gesture::None);
        assert!(a.take_op().is_none(), "and letting go there moves nothing");
    }

    #[test]
    fn a_question_answered_after_navigating_still_names_what_it_asked_about() {
        // **The class this and the two below share**: an operation is two gestures, and the first
        // version composed its paths at the *second* — out of `self.path` and the selection, both
        // of which move while a prompt or a dialog is up. There is no input-exclusive window
        // (`TODO(dialog-modality)`) and the prompt is a keyboard mode only, so the parent goes on
        // taking clicks (PR #268 review, blocking 2).
        let mut a = app();
        select(&mut a, 2); // a.txt
        a.update(Msg::Choose(Action::Delete));
        assert_eq!(a.confirming(), Some("a.txt"));

        // The person clicks `work/` in the parent, which navigates. The binary re-lists.
        a.update(Msg::Activate(1));
        let to = a.take_goto().expect("a directory row navigates");
        assert_eq!(to, "/home/work");
        a.show(&to, alloc::vec![Entry::file("a.txt"), Entry::file("payroll.txt")]);

        // **A listing supersedes the question**, so there is nothing left to answer wrongly.
        assert_eq!(a.confirming(), None, "a question about a directory you have left is gone");
        a.update(Msg::ConfirmDelete);
        assert!(a.take_op().is_none());
    }

    #[test]
    fn a_delete_answered_after_the_selection_moves_removes_what_was_asked_about() {
        // The half a re-listing does *not* cover: clicking a **file** row re-selects without
        // navigating, so nothing clears the question.
        //
        // **This does not pin the captured path, and saying so is the point.** `self.path` only
        // changes in `show`, and `show` now drops the question — so for *delete* the two fixes
        // overlap and composing the path late would give the same answer here. Run alone against
        // that version this test passes, which makes it a guard for the question surviving a
        // selection change and nothing more. What pins the capture is the rename below, where the
        // *source* moves without a listing.
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
        // Same shape in the prompt, where it is worse: the prompt *replaces* the path in the
        // strip, so after a navigation nothing on screen says where the name is about to land
        // (PR #268 review, blocking 3).
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

        // And a navigation drops the prompt outright, so the mode cannot outlive its directory.
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
        // `copy_file` on a directory merely fails, and `Action::Duplicate` does not exclude one —
        // so the operation has to carry which it is (PR #268 review, optional 3).
        let mut a = app();
        select(&mut a, 0); // archive/
        a.update(Msg::Choose(Action::Duplicate));
        press_key(&mut a, KEY_X);
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(a.take_op(), Some(FileOp::Copy { from: "/home/archive".into(), to: "/home/x".into(), dir: true }));
    }

    /// The keys of the listing rows drawn with a selection wash over them.
    ///
    /// **What the screen highlights**, read off the tree the window builds: `list_view` washes a
    /// selected row, so a row whose subtree carries one is a row a person sees as picked.
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

    /// **Through the built view, with a filter up**: the row the screen highlights is the file
    /// an action acts on, and `Enter` opens it.
    ///
    /// **This is the test the last one should have been** (PR #320 review, blocking 4). That one
    /// pinned `visible_indices` against itself: its control renumbered inside that helper, which
    /// is not the path anything uses. Renumbering the *rows* — keying them by position, which is
    /// exactly the mistake the entry says would "leave every one of those acting on the wrong
    /// file" — passed it and all 94 tests in this crate.
    ///
    /// So this presses a row through the real tree, asks the Edit menu what it would act on, and
    /// presses `Enter`. Blocking 1 and blocking 2 are both here.
    #[test]
    fn a_filtered_row_is_the_row_that_is_acted_on() {
        // `libinput`'s codes for t and h, as the filter test above uses.
        const KEY_T: u16 = 20;
        const KEY_H: u16 = 35;
        let mut a = App::new("/home");
        a.show("/home", alloc::vec![Entry::dir("Documents"), Entry::file("Notes.txt"), Entry::file("other.txt"),]);
        a.update(Msg::OpenSearch);
        press_key(&mut a, KEY_T);
        press_key(&mut a, KEY_H);
        assert_eq!(a.visible_indices(), alloc::vec![2], "one row left: other.txt");

        // Press that row where it is drawn, through the tree the window actually builds.
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
        // **What the screen says and what the Edit menu would do are the same file.**
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

        // **`Enter` opens what is left**, which the plan and the log both claim and which sent a
        // key where an entries index belongs, so it opened nothing.
        let mut b = App::new("/home");
        b.show("/home", alloc::vec![Entry::dir("Documents"), Entry::dir("theatre")]);
        b.update(Msg::OpenSearch);
        press_key(&mut b, KEY_T);
        press_key(&mut b, KEY_H);
        assert_eq!(b.visible_indices(), alloc::vec![1], "theatre is the only match");
        press_key(&mut b, libkern::abi::KEY_ENTER);
        assert_eq!(b.take_goto().as_deref(), Some("/home/theatre"), "Enter descended into it");

        // **A selection the filter hides keeps its place.** This is the half the translation
        // *into* the widget is for: with the entries index handed straight to `list_view`, a
        // hidden selection is read as a row position, highlights whatever row that is, and comes
        // back as a different file — the selection moving on its own while a search is typed.
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
        // **Nothing is highlighted while the selection is hidden**, which is what the
        // translation *into* the widget is for: handed an entries index, `list_view` reads it as
        // a row position and lights up whichever row that is — here the one remaining row, a
        // file the person did not pick. The selection survives either way, thanks to the
        // translation back; the *highlight* is the half that needs both.
        let framed = c.view(&theme, None);
        let _ =
            libui::layout::layout(&framed, Rect::new(0, 0, size.w, size.h), &libui::layout::FixedCell { w: 8, h: 16 });
        assert_eq!(washed_rows(&framed), Vec::<u64>::new(), "no row is lit while none is shown");
        press_key(&mut c, libkern::abi::KEY_ESC);
        assert_eq!(c.selected_name(), Some("Documents"), "the selection survived a search that hid it");
    }

    /// `list_top` is where the first row is actually drawn — asked of the laid-out tree.
    ///
    /// **The function's whole reason is that "two would disagree"**, and they did: Part I made
    /// the tab strip conditional and added a column header, `list_h` followed and this did not,
    /// so a drop landed on the row above the one it was released over (PR #320 review, blocking
    /// 3). The drag tests aim *through* `list_top`, so they agreed with it and passed.
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

    /// Search narrows the listing, and a narrowed row still opens the file it names.
    ///
    /// **The second half is the one worth pinning** (desktop refresh, Part I): a filter that
    /// renumbered its rows would leave every press, drag and rename acting on the wrong entry,
    /// because a row's key is its index into the *whole* listing and everything downstream
    /// resolves through that.
    #[test]
    fn search_filters_the_listing_and_keeps_each_rows_identity() {
        // `libinput`'s table, as `KEY_X` above is: n, o, t, h.
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
        // **One row left, and it is row 1 of the listing rather than row 0 of the filter** —
        // and `Notes.txt` matched a lower-case query, which is the case-insensitivity.
        assert_eq!(a.visible_indices(), alloc::vec![1]);
        // A substring rather than a prefix: `th` is in the middle of `other.txt`.
        a.update(Msg::SearchCancel);
        a.update(Msg::OpenSearch);
        press_key(&mut a, KEY_T);
        press_key(&mut a, KEY_H);
        assert_eq!(a.visible_indices(), alloc::vec![2], "matched in the middle of a name");
        // Escape puts the whole listing back.
        press_key(&mut a, libkern::abi::KEY_ESC);
        assert_eq!(a.search_text(), None);
        assert_eq!(a.visible_indices(), alloc::vec![0, 1, 2]);
    }

    /// The three column formatters, at the boundaries each of them has.
    ///
    /// **They had no tests at all** (PR #320 review, worth fixing 10), and `column_modified`'s
    /// own doc claimed it was pure "so the host can test the boundary between the two forms".
    #[test]
    fn the_columns_say_what_they_mean() {
        // Sizes: exact while a number is short, then one decimal, then none.
        assert_eq!(column_size(0), "0");
        assert_eq!(column_size(848), "848");
        assert_eq!(column_size(4096), "4096");
        assert_eq!(column_size(10 * 1024 - 1), "10239", "the last exact one");
        // **Ten units is where the decimal stops meaning anything**, so `10K` rather than
        // `10.0K` — which is also what the design draws beside an 11 KiB file.
        assert_eq!(column_size(10 * 1024), "10K", "and the first rounded");
        assert_eq!(column_size(11 * 1024), "11K");
        assert_eq!(column_size(5 * 1024 * 1024 / 2), "2.5M", "under ten units keeps its tenth");
        assert_eq!(column_size(2936012), "2.8M");
        assert_eq!(column_size(100 * 1024 * 1024), "100M", "past ten units the decimal goes");
        assert_eq!(column_size(3 * 1024 * 1024 * 1024), "3.0G");

        // Kinds: a directory, an extension, or neither.
        assert_eq!(column_kind("work", true), "dir");
        assert_eq!(column_kind("theme.toml", false), "toml");
        assert_eq!(column_kind("notes", false), "file", "no dot is no extension");
        assert_eq!(column_kind(".quiet", false), "file", "a leading dot is not one either");
        assert_eq!(column_kind("archive.tar.gz", false), "gz", "the last one wins");

        // Modified: `HH:MM` today, a date before that, nothing when unreported.
        // 2026-09-22 12:34:56 UTC, and the same clock at 00:00:01 the next day.
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
        // **With no clock, every row is a date** rather than everything looking like today.
        assert_eq!(column_modified(NOON, 0).len(), 10, "no today to compare against");
    }

    /// A path reads as segments with the separators quieter than the names.
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
        // **And the separators are drawn quieter than the names**, which this claimed in its
        // title and did not check: it passed with every `ink` removed (PR #320 review, 10).
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

    /// One tab is no strip, and two are: the strip's height and what it *draws* agree.
    ///
    /// **Painted, because a zero-height box still has children** (desktop refresh, Part I): the
    /// strip was sized to nothing and went on drawing a tab and a `+` over the chrome below it,
    /// which a height assertion alone would have called a pass. The screendump caught it; this
    /// is what would have.
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
            // The band the strip would occupy: below the menu bar, one strip tall.
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
        // **What a person expects to survive switching**, and the reason `Pane` exists: a tab
        // that came back at the top of a different folder would lose their place every time they
        // glanced at another one.
        let mut a = app();
        select(&mut a, 2); // a.txt
        assert_eq!(a.list_selected(), Some(2));

        a.update(Msg::NewTab);
        assert_eq!(a.tab_count(), 2);
        // A new tab opens where you are, and asks for its own listing.
        assert_eq!(a.take_goto().as_deref(), Some("/home"));
        a.show("/home/work", alloc::vec![Entry::file("payroll.txt")]);
        assert_eq!(a.path(), "/home/work");
        assert_eq!(a.list_selected(), Some(0));

        // And the first tab is exactly as it was left.
        let first = a.tabs()[0].0;
        a.update(Msg::SelectTab(first));
        assert_eq!(a.path(), "/home");
        assert_eq!(a.list_selected(), Some(2), "its own selection");
        assert_eq!(a.entries().len(), 4, "and its own listing");
    }

    #[test]
    fn a_tabs_key_can_never_be_a_row_index() {
        // **One namespace, two things numbering into it.** `Router::hovered_key` reports the
        // nearest keyed ancestor across the whole window, and this browser keys its list rows by
        // index — so a base merely *far* from the chrome's keys still collides in a big enough
        // directory, and hovering row `n` would draw a tab hovered (PR #270 review, optional 6).
        let mut a = app();
        for _ in 0..4 {
            a.update(Msg::NewTab);
            let _ = a.take_goto();
        }
        for (key, _) in a.tabs() {
            assert!(key >= TAB_KEY_BASE, "every tab is above the base");
        }
        // A row index is a `usize` that counts entries; it cannot reach the high bit, so the two
        // ranges are disjoint by construction rather than by being far apart.
        assert_eq!(TAB_KEY_BASE, 1 << 63);

        // **And a tab's `×` has a key of its own** (desktop refresh, after Part K): the toolkit
        // keys it as the tab's key with `TAB_CLOSE_BIT` set, which relies on every tab key
        // leaving that bit clear. They are numbered upward from the base, and four billion tabs
        // later they still do — nor can a close key reach a row index, which is a `usize`
        // counting entries.
        for (key, _) in a.tabs() {
            assert_eq!(key & libui::widget::TAB_CLOSE_BIT, 0, "tab key {key:#x} sets the close bit");
        }
        assert_eq!((TAB_KEY_BASE + (1 << 32)) & libui::widget::TAB_CLOSE_BIT, 0);
    }

    #[test]
    fn a_browser_tab_closes_without_asking_and_the_last_one_closes_the_window() {
        // **Nothing to lose**: a listing is a view of the filesystem rather than unsaved work,
        // so there is no question to ask — which is the difference between this and the
        // editor's tabs, and worth a test because the two look alike.
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
        // **All six of this part's chords, not three of them** (PR #285 review, optional 10).
        assert_eq!(libinput::keymap::to_char(CUT_KEYCODE, 0), Some(b'x'));
        assert_eq!(libinput::keymap::to_char(COPY_KEYCODE, 0), Some(b'c'));
        assert_eq!(libinput::keymap::to_char(PASTE_KEYCODE, 0), Some(b'v'));
    }

    /// Hovering a listing row must not light a piece of chrome.
    ///
    /// **`hovered` is one namespace.** `Router::hovered_key` reports the key of whatever is under
    /// the pointer, and the chrome compares it against its own constants — so a listing row keyed
    /// by its bare index lights the button that happens to share that number. `TAB_KEY_BASE`'s
    /// doc already worked this out for tabs; the chrome is the same hazard and was not covered.
    #[test]
    fn a_listing_row_does_not_light_the_chrome() {
        let mut a = app();
        a.show("/home", (0..8).map(|i| Entry::file(&alloc::format!("f{i}.txt"))).collect());
        let ui = UiTheme::default();
        // Every row's key, as the tree carries it.
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

    /// A re-sort moves the anchor with the file, not just the selection.
    #[test]
    fn the_anchor_follows_the_file_across_a_re_sort() {
        let mut a = app();
        a.show("/home", five());
        a.note_press(libdraw::geom::Point::new(40, 60), 1_000, 0);
        a.update(Msg::Press(row(0)));
        assert_eq!(picked(&a), alloc::vec!["f0.txt"]);

        a.update(Msg::SetOrder(libfs::Order::NameDesc));
        // `f0.txt` is row 4 now. A Shift-click on row 2 must extend from *there*, not from 0.
        a.note_press(libdraw::geom::Point::new(40, 100), 2_000, MOD_SHIFT);
        a.update(Msg::Press(row(2)));
        // Rows 2..=4 under `NameDesc` are `f2`, `f1`, `f0` — and `selection()` reports them in
        // *listing* order, which is the order they are on screen. **A stale anchor would have
        // given `f4, f3, f2`**, the range from row 0, which is the distinction this pins.
        assert_eq!(
            picked(&a),
            alloc::vec!["f2.txt", "f1.txt", "f0.txt"],
            "the range runs from the row the person can see highlighted"
        );
    }

    /// A navigation ends the click run, so the click after it is a first click.
    ///
    /// **The second click of a double click navigates**, which is what makes this reachable: a
    /// third press moments later would otherwise open whatever is now row 0.
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

        // The listing arrives, and a third press lands on it.
        a.show("/home/work", alloc::vec![Entry::file("inside.txt")]);
        a.note_press(at, 1_240, 0);
        a.update(Msg::Press(row(0)));
        assert_eq!(a.take_open(), None, "it selected rather than opening");
    }

    // --- cut, copy and paste of files (M14 Part D) ---------------------------

    use librsproto::clipboard::PathVerb;

    /// Copy puts every picked entry on the clipboard, as absolute paths.
    ///
    /// **Resolved before pushing**, because a path is read by whoever reads it — a relative one
    /// would name a file in *their* directory.
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

    /// Cut says so on the wire rather than being remembered here.
    ///
    /// **A browser that remembered its own pending cut** would move files for itself and copy
    /// them for any other window, which is a difference nobody can see until it has happened.
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

    /// Copying nothing says so and pushes nothing.
    #[test]
    fn copying_an_empty_selection_pushes_nothing() {
        let mut a = app();
        a.show("/empty", alloc::vec![]);
        a.update(Msg::CopyFiles);
        assert_eq!(a.take_clip_push(), None);
        assert_eq!(a.notice.as_deref(), Some("nothing is selected"));
    }

    /// A paste queues one operation per path, and the verb decides which.
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

        // A cut pastes as a move.
        a.pasted(PathVerb::Cut, &[("/home/a.txt", false)]);
        assert!(matches!(a.take_op(), Some(FileOp::MoveInto { .. })));
    }

    /// A paste into the directory the file is already in is refused.
    ///
    /// **The copy is the dangerous half**: `from == to` would open the file for reading and
    /// truncate it for writing at the same path.
    #[test]
    fn pasting_a_file_into_its_own_directory_does_nothing() {
        let mut a = app();
        a.show("/home", alloc::vec![Entry::file("a.txt")]);
        a.pasted(PathVerb::Copy, &[("/home/a.txt", false)]);
        assert_eq!(a.take_op(), None, "no operation was queued");
        assert_eq!(a.notice.as_deref(), Some("nothing to paste here"));
    }

    /// A pasted **folder** is queued as a tree copy, not a file copy.
    ///
    /// **`copy_file` on a directory merely fails**, and reported "could not copy it" for an
    /// ordinary request — the same hazard `Duplicate` has carried `dir` for since M12, which the
    /// paste path bypassed (PR #285 review, worth fixing 3). The browser cannot tell a folder
    /// from a file by its path, so the binary asks the filesystem and hands the answer down.
    #[test]
    fn a_pasted_folder_is_a_tree_copy() {
        let mut a = app();
        a.show("/home/papers", alloc::vec![]);
        a.pasted(PathVerb::Copy, &[("/home/work", true)]);
        assert_eq!(
            a.take_op(),
            Some(FileOp::Copy { from: String::from("/home/work"), to: String::from("/home/papers/work"), dir: true })
        );

        // …and a file is still a file copy, which is the control: a version that always said
        // `true` would pass the assertion above.
        a.pasted(PathVerb::Copy, &[("/home/a.txt", false)]);
        assert!(matches!(a.take_op(), Some(FileOp::Copy { dir: false, .. })));
    }

    /// Text on the clipboard is reported rather than pasted as a filename.
    #[test]
    fn pasting_text_says_what_it_found() {
        let mut a = app();
        a.paste_not_paths();
        assert_eq!(a.notice.as_deref(), Some("the clipboard holds text, not files"));
        assert_eq!(a.take_op(), None);
    }

    // --- multi-select (M14 Part D) -------------------------------------------

    /// A listing with enough rows to pick a range out of.
    fn five() -> Vec<Entry> {
        (0..5).map(|i| Entry::file(&alloc::format!("f{i}.txt"))).collect()
    }

    fn picked(a: &App) -> Vec<String> {
        a.selection().iter().map(|e| e.name.clone()).collect()
    }

    /// Ctrl-click adds a row and takes it away again, including the anchor.
    ///
    /// **Unpicking the anchor is the case a set-only model misses.** The list state holds one
    /// `selected` and the picks are everything besides it, so Ctrl-clicking the selected row has
    /// to empty the state rather than the set.
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

        // Again on the same row takes it out.
        a.note_press(libdraw::geom::Point::new(40, 100), 3_000, MOD_CTRL);
        a.update(Msg::Press(row(3)));
        assert_eq!(picked(&a), alloc::vec!["f1.txt"]);

        // And on the anchor, which lives in the list state rather than in the picks.
        a.note_press(libdraw::geom::Point::new(40, 60), 4_000, MOD_CTRL);
        a.update(Msg::Press(row(1)));
        assert!(picked(&a).is_empty(), "the anchor can be unpicked too: {:?}", picked(&a));
    }

    /// Shift-click picks everything between the anchor and the row, both ways round.
    #[test]
    fn shift_click_picks_a_range() {
        let mut a = app();
        a.show("/home", five());
        a.note_press(libdraw::geom::Point::new(40, 60), 1_000, 0);
        a.update(Msg::Press(row(1)));
        a.note_press(libdraw::geom::Point::new(40, 140), 2_000, MOD_SHIFT);
        a.update(Msg::Press(row(3)));
        assert_eq!(picked(&a), alloc::vec!["f1.txt", "f2.txt", "f3.txt"]);

        // **Upwards too**, which a range written as `from..=to` gets wrong.
        let mut a = app();
        a.show("/home", five());
        a.note_press(libdraw::geom::Point::new(40, 140), 1_000, 0);
        a.update(Msg::Press(row(3)));
        a.note_press(libdraw::geom::Point::new(40, 60), 2_000, MOD_SHIFT);
        a.update(Msg::Press(row(1)));
        assert_eq!(picked(&a), alloc::vec!["f1.txt", "f2.txt", "f3.txt"]);
    }

    /// A plain press replaces the whole selection.
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

    /// **A Ctrl-click never opens**, however fast it repeats.
    ///
    /// Somebody building a selection is not asking for anything to happen, and opening a file
    /// mid-selection is the accident decision 5 exists to prevent.
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

    /// The picks survive a re-sort and do not survive a navigation.
    ///
    /// **Names, not indices.** The View menu reorders the listing under a selection, and a set of
    /// positions would name different files afterwards.
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

        // **And the order is still the tab's**, which is why row 0 here is `f4.txt` rather than
        // `f0.txt`: an order chosen once follows you down a tree, by design.
        a.show("/elsewhere", five());
        assert_eq!(
            picked(&a),
            alloc::vec!["f4.txt"],
            "a new listing keeps only its own row 0, under the order this tab is in"
        );
    }

    // --- the sidebar (M14 Part D) --------------------------------------------

    /// The sidebar's places are this window's home's — `libfs::places` is pinned by its own
    /// test, and this is the half that says the browser asks it about the right home.
    #[test]
    fn the_places_are_built_from_this_sessions_home() {
        let a = App::new("/home/someone-else");
        assert_eq!(a.places(), libfs::places("/home/someone-else"));
        assert_eq!(a.places()[1].path, "/home/someone-else/Documents");
    }

    /// One press on a sidebar row goes there — no second click.
    ///
    /// **Not the listing's rule.** A listing row needs a double click because a single one has to
    /// select; a sidebar row has nothing to select and no other verb.
    #[test]
    fn one_press_on_a_place_navigates() {
        let mut a = app();
        let root = a.places().len() as u64 - 1;
        a.update(Msg::Place(SIDEBAR_ROW_KEY + root));
        assert_eq!(a.take_goto().as_deref(), Some("/"), "one press was enough");

        a.update(Msg::Place(SIDEBAR_ROW_KEY + 1));
        assert_eq!(a.take_goto().as_deref(), Some("/home/Documents"));
    }

    /// A key that is not a sidebar row's goes nowhere.
    ///
    /// **The control on `checked_sub`.** A saturating conversion would make every stray key mean
    /// row zero — Home — so a press on anything at all would navigate.
    #[test]
    fn a_key_below_the_base_is_not_a_place() {
        let mut a = app();
        a.update(Msg::Place(0));
        assert_eq!(a.take_goto(), None, "key 0 is not Home");
        a.update(Msg::Place(SIDEBAR_ROW_KEY + 99));
        assert_eq!(a.take_goto(), None, "and neither is a row that does not exist");
    }

    /// The sidebar highlights the place the tab is in, and nothing when it is elsewhere.
    ///
    /// **Derived rather than remembered**, so it cannot disagree with the listing: this navigates
    /// by a route the sidebar knows nothing about and the highlight still follows.
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

    /// The sidebar's rows share no key with the listing's or with the chrome's.
    ///
    /// **Asserted about the ranges, not about a sample listing.** The first version of this
    /// listed eight entries and passed while row 1000 *was* the sidebar's first row — a directory
    /// with a thousand files is ordinary, and a test that cannot reach the collision says nothing
    /// about it (PR #285 review, worth fixing 4).
    #[test]
    fn a_place_does_not_light_a_listing_row() {
        // The bases cannot meet however long a directory is: a listing row's key is
        // `LIST_ROW_KEY + i`, and `i` is bounded by what a filesystem can hold.
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
        // And a tab's base is above the sidebar's, which is the third range in this namespace.
        const { assert!(TAB_KEY_BASE > SIDEBAR_ROW_KEY, "tabs must stay clear of the sidebar too") };
    }

    // --- drives (Phase 6 Part F) ----------------------------------------------

    /// A storage table as the service writes one, column for column: a disk with nothing on it;
    /// `init`'s root; the internal root on a live boot; **two sticks labelled `DATA`, mounted
    /// `DATA` and `DATA-2`**; a stick with no label, mounted under its partition's name; and a
    /// stick refused, so mounted nowhere (PR #366 review).
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

    /// **The drives are every mount under `/storage`, by the name it is mounted under** — not
    /// `init`'s root, not a device with nothing mounted, and never by the filesystem's label, which
    /// two sticks share here and one lacks.
    #[test]
    fn the_drives_are_every_mount_under_storage_by_its_mount_name() {
        let d = |name: &str, block, removable| Drive { name: String::from(name), block, removable };
        assert_eq!(
            drives(&storage_table()),
            alloc::vec![
                d("nitrox-root", 3, false),
                d("DATA", 5, true),
                d("DATA-2", 7, true),
                d("partition 1", 9, true),
            ]
        );
        assert_eq!(drives(&storage_table())[1].path(), "/storage/DATA");
        let mut old = storage_table();
        old.schema.fields.retain(|f| f.name != "mounted");
        assert_eq!(drives(&old), alloc::vec![], "a table without the column lists nothing");
    }

    /// A browser with the fixture's drives, laid out as the binary lays one out.
    fn with_drives() -> App {
        let mut a = app();
        a.set_drives(drives(&storage_table()));
        a
    }

    /// **An eject button on a removable drive's row, and on no other** — not on the internal
    /// disk's, and never on a place's.
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

    /// Route a click at the centre of whatever is keyed `key`, through the real tree and router,
    /// and hand what it produced to the browser.
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

    /// **A press on a drive's button ejects that drive, by its mount name; a press on its row
    /// opens it** — through the real tree and router, so the wiring is what is tested. `DATA-2`'s
    /// button ejects `DATA-2`, not the other `DATA`.
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
        // **The internal disk's key, sent anyway**, is refused here as well as by the service.
        a.update(Msg::Drive(DRIVE_EJECT_KEY + 3));
        assert_eq!(a.take_eject(), None);
        // The tab is highlighted in Drives when it is on one, as a place is.
        a.show("/storage/DATA", alloc::vec![]);
        let _ = a.view(&UiTheme::default(), None);
        assert_eq!((a.drive_list.selected, a.sidebar.selected), (Some(1), None));
    }

    /// **An eject refused says why; one done says the drive can be removed**, and says it still
    /// when the table no longer lists the drive and the tab that was on it has gone home.
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

    /// **An eject that took a stick's two mounts says both, and says it again as they go**: the
    /// service unmounts every filesystem on the stick (PR #367 review), so a tab on the other one
    /// goes home as ejected, not as a stick pulled out.
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

    /// **A drive that goes takes every tab inside it home**: the current one by a listing now, with
    /// a notice that outlives that listing; another when it is chosen. A tab on `DATA-2` is not
    /// inside `DATA`, nor is one at home.
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
        // The other tab inside it, home when chosen — and read then, once.
        a.update(Msg::SelectTab(inside));
        assert_eq!(a.path(), "/home");
        assert_eq!(a.take_goto().as_deref(), Some("/home"));
        a.update(Msg::SelectTab(current));
        a.update(Msg::SelectTab(inside));
        assert_eq!(a.take_goto(), None, "read once");
        // Not a tab on DATA-2, nor one at home.
        a.update(Msg::SelectTab(other));
        assert_eq!(a.take_goto(), None);
        assert_eq!(a.path(), "/storage/DATA-2");
        a.update(Msg::SelectTab(at_home));
        assert_eq!(a.take_goto(), None);
        // **A stick pulled with no tab on it** says nothing.
        a.notice = None;
        a.set_drives(drives(&storage_table()).into_iter().filter(|d| d.name == "nitrox-root").collect());
        assert_eq!(a.notice.as_deref(), Some("DATA-2 is gone, so a tab on it went home"));
        a.notice = None;
        a.set_drives(alloc::vec![]);
        assert_eq!(a.notice, None);
    }

    /// **A short window still shows a drive**: the places give way and scroll, so the Drives heading
    /// never stands over an empty list (PR #367 review: below about 305 px it did). At the window's
    /// start size the places are whole, which is where `check-media` aims.
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
    /// as ranges, for a device index up to the bound [`drives`] keeps.
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

    // --- properties (M14 Part D) ---------------------------------------------

    /// A size reads exactly below a kibibyte and both ways above it.
    ///
    /// **The boundary is the case worth pinning.** 1023 must not round to "1.0 KiB" and 1024 must
    /// not read as a bare four-digit number; a comparison written with the wrong `>=` gets one of
    /// them and looks right on the other.
    #[test]
    fn a_size_reads_exactly_and_roundly() {
        assert_eq!(size_text(0), "0 bytes");
        assert_eq!(size_text(1023), "1023 bytes", "just under a KiB is still exact");
        assert_eq!(size_text(1024), "1.0 KiB (1024 bytes)");
        assert_eq!(size_text(1536), "1.5 KiB (1536 bytes)", "and it rounds rather than truncates");
        assert_eq!(size_text(1024 * 1024), "1.0 MiB (1048576 bytes)");
        assert_eq!(size_text(1024 * 1024 * 1024), "1.0 GiB (1073741824 bytes)");
        // The exact count is always there, which is the half a rounded number cannot give back.
        assert!(size_text(1_234_567).ends_with("(1234567 bytes)"));
    }

    /// An unreported time says so rather than claiming 1970.
    ///
    /// **This is the whole reason the function exists.** `0` means "the server does not keep
    /// one", which is every namespace listing — and formatting it would state 1970-01-01 as a
    /// fact about a file made this morning.
    #[test]
    fn an_unreported_time_is_unknown_rather_than_1970() {
        assert_eq!(modified_text(0), "unknown");
        assert_eq!(modified_text(-1), "unknown", "and so is anything before the epoch");
        assert_eq!(modified_text(i64::MAX), "unknown", "rather than overflowing into a date");
        // A real one is the UTC calendar date, which is `libtime`'s to compute.
        assert_eq!(modified_text(1_000_000_000), "2001-09-09 01:46:40 UTC");
    }

    /// Properties shows what the entry was when it was chosen, and closes on its one button.
    #[test]
    fn properties_reports_the_entry_it_was_opened_on() {
        let mut a = app();
        a.show(
            "/home",
            alloc::vec![Entry { mtime: 1_000_000_000, size: 1536, ..Entry::file("notes.txt") }, Entry::dir("work"),],
        );
        // Row 0 is the directory; row 1 is the file.
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

        // **The listing may move under it and the dialog must not follow**, which is the reason
        // the entry is snapshotted rather than read back through the selection.
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

    /// A folder reports no size rather than a size of zero.
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

    // --- the dialog slot (M14 Part D) ----------------------------------------

    /// The slot reports which dialog is open, and its dismissal is the cautious answer.
    ///
    /// **The seam a second dialog will be added through.** The binary hosts one window against
    /// `dialog()` and reconciles on the *kind*, so what this pins is that the kind tracks the
    /// state — a `dialog()` stuck on `Some` would leave a window on screen with nothing behind
    /// it, and one stuck on `None` would close a question mid-answer.
    #[test]
    fn the_dialog_slot_tracks_what_is_open() {
        let mut a = app();
        a.show("/home", alloc::vec![Entry::file("notes.txt")]);
        assert_eq!(a.dialog(), None, "nothing is open to begin with");
        assert_eq!(a.dialog_dismissed(), None, "and nothing to dismiss");

        a.update(Msg::Press(row(0)));
        a.update(Msg::Choose(Action::Delete));
        assert_eq!(a.dialog(), Some(Dialog::Confirm), "the question is up");

        // **Closing the window must not perform it**, which is what a dialog's own close button
        // already means and what the manager's request has to mean too.
        assert_eq!(a.dialog_dismissed(), Some(Msg::KeepIt));
        // `Esc` is the only key that answers, and it answers the same way.
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

    // --- the location bar (M14 Part D) ---------------------------------------

    /// `Ctrl+L` opens the bar on where the tab is, and Enter goes where it says.
    #[test]
    fn the_location_bar_opens_seeded_and_navigates() {
        let mut a = app();
        assert!(a.location_text().is_none(), "closed to begin with");

        a.update(Msg::Key(KeyEvent::new(1, LOCATION_KEYCODE, KEY_DOWN, MOD_CTRL)));
        assert_eq!(a.location_text().as_deref(), Some("/home"), "seeded with where the tab is, not left blank");

        // Replace it with somewhere else and go.
        a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_ESC, KEY_DOWN, 0)));
        a.update(Msg::OpenLocation);
        *a.location.as_mut().expect("open") = TextFieldState::with_text("/system/fonts");
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(a.take_goto().as_deref(), Some("/system/fonts"));
        assert!(a.location_text().is_none(), "and it closed");
    }

    /// A relative path is joined to where the tab is, as it would be in a shell.
    #[test]
    fn a_relative_path_is_joined_to_the_current_directory() {
        let mut a = app();
        a.update(Msg::OpenLocation);
        *a.location.as_mut().expect("open") = TextFieldState::with_text("papers");
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(a.take_goto().as_deref(), Some("/home/papers"));
    }

    /// Esc closes it and says so; an empty path is refused and it stays open.
    ///
    /// **"Cancelled" is not decoration**: a field that vanishes with nothing changed is
    /// indistinguishable from one that acted, which is the same argument the delete question
    /// already makes.
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

    /// While the bar is open the keys are its own — Backspace edits, it does not go up.
    ///
    /// **One key doing two things is the failure**, and it is the rule the name prompt already
    /// follows: correcting a typo must not also navigate.
    #[test]
    fn the_location_bar_holds_the_keyboard_while_it_is_open() {
        let mut a = app();
        a.update(Msg::OpenLocation);
        assert_eq!(a.location_text().as_deref(), Some("/home"));
        press_key(&mut a, libkern::abi::KEY_BACKSPACE);
        assert_eq!(a.location_text().as_deref(), Some("/hom"), "Backspace edited the field");
        assert_eq!(a.take_goto(), None, "and did not go up a directory");
    }

    /// The two fields in the path strip cannot both be open.
    ///
    /// **One draws and the other takes the keys**, which is what made this worth a test rather
    /// than a comment: `key` checks the location bar first and `view` draws the prompt first, so
    /// a state with both set is not a cosmetic muddle — it is a field you cannot see eating every
    /// keystroke you type (PR #285 review, blocking 1).
    #[test]
    fn opening_one_field_closes_the_other() {
        // The prompt closes the bar.
        let mut a = app();
        a.update(Msg::OpenLocation);
        a.update(Msg::Choose(Action::NewFile));
        assert_eq!(a.prompt_len(), Some(0), "the prompt is open");
        assert_eq!(a.location_text(), None, "and the bar is not");
        // …so typing reaches the prompt and Enter makes a file rather than navigating.
        press_key(&mut a, KEY_X);
        press_key(&mut a, libkern::abi::KEY_ENTER);
        assert_eq!(a.take_goto(), None, "nothing navigated");
        assert!(matches!(a.take_op(), Some(FileOp::Create { .. })), "the file was created");

        // And the bar closes the prompt, which already held.
        let mut a = app();
        a.update(Msg::Choose(Action::NewFolder));
        a.update(Msg::OpenLocation);
        assert_eq!(a.prompt_len(), None, "the prompt is closed");
        assert_eq!(a.location_text().as_deref(), Some("/home"));
    }

    /// A path that cannot be listed says so rather than doing nothing.
    #[test]
    fn a_directory_that_is_not_there_is_reported() {
        let mut a = app();
        a.update(Msg::OpenLocation);
        *a.location.as_mut().expect("open") = TextFieldState::with_text("/nowhere");
        press_key(&mut a, libkern::abi::KEY_ENTER);
        let to = a.take_goto().expect("it tried");
        // The binary comes back with the bad news, as it does for any listing.
        a.list_failed(&to);
        assert_eq!(a.notice.as_deref(), Some("no such directory: /nowhere"));
    }

    // --- single click selects, double click opens (M14 Part D, decision 5) ----

    /// One click points at a file; two open it.
    ///
    /// **The first click doing nothing but select is the whole feature.** Before this, clicking a
    /// file opened it, so there was no way to point at one without acting on it — the maintainer's
    /// report was "clicking a file opens the file rather than selecting it".
    #[test]
    fn a_single_click_selects_and_a_second_one_opens() {
        let at = libdraw::geom::Point::new(40, 60);
        let mut a = app();
        a.show("/home", alloc::vec![Entry::file("notes.txt"), Entry::dir("work")]);
        // **Directories lead, so row 0 is `work` and row 1 is `notes.txt`.** Naming the rows by
        // the order they were passed in is how the first version of this test asserted against
        // the wrong file and read as a broken feature.
        assert_eq!(names(&a), alloc::vec!["work", "notes.txt"]);

        a.note_press(at, 1_000, 0);
        a.update(Msg::Press(row(1)));
        assert_eq!(a.pane().list.selected, Some(1), "it selected the row");
        assert_eq!(a.take_open(), None, "and did not open it");
        assert_eq!(a.take_goto(), None);

        a.note_press(at, 1_150, 0);
        a.update(Msg::Press(row(1)));
        assert_eq!(a.take_open().as_deref(), Some("/home/notes.txt"), "the second click opened it");

        // A directory's second click navigates rather than opening.
        a.note_press(at, 5_000, 0);
        a.update(Msg::Press(row(0)));
        assert_eq!(a.take_goto(), None, "still just a selection");
        a.note_press(at, 5_150, 0);
        a.update(Msg::Press(row(0)));
        assert_eq!(a.take_goto().as_deref(), Some("/home/work"));
    }

    /// Two clicks too far apart in time are two single clicks.
    ///
    /// **The negative control for the run**, and it is the case a person actually produces:
    /// clicking a file to select it and clicking it again a second later to be sure.
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

    /// `Enter` opens the selected row, click run or no click run.
    ///
    /// **The keyboard is not a click** — it has no position and no run to be part of, and a
    /// keyboard that had to be pressed twice would be an interaction nobody has ever wanted.
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

    /// A drag abandons the run, so the click after it is a first click.
    ///
    /// **Without this the browser opens something after every drag**: a drag starts with a press
    /// the tracker counted, so the next click lands as number two of a run nobody meant to start.
    #[test]
    fn a_drag_abandons_the_run() {
        let mut a = app();
        a.show("/home", alloc::vec![Entry::file("notes.txt")]);

        // **A real drag, not a call to `note_drag`.** The browser ends the run itself when a
        // press becomes a drag, and a test that called the helper directly would pass for a
        // version that never called it — which is what the first one did.
        a.note_press(libdraw::geom::Point::new(40, 60), 1_000, 0);
        a.update(Msg::Grab(row(0)));
        a.pointer_moved(40, 60, 1);
        // Past the slop in one step, which is the motion that arms the drag.
        assert_eq!(a.pointer_moved(40 + DRAG_SLOP, 60, 1), Gesture::Moved, "travelling past the slop is a drag");
        a.pointer_moved(40 + DRAG_SLOP, 60, 0);
        a.note_press(libdraw::geom::Point::new(40, 60), 1_150, 0);
        a.update(Msg::Press(row(0)));
        assert_eq!(a.take_open(), None, "the click after a drag is a first click");
    }

    // --- the View menu (M14 Part D) ------------------------------------------

    /// A listing with something to hide in it, and something to order by.
    fn dated() -> Vec<Entry> {
        let at = |name: &str, mtime: i64| Entry { mtime, ..Entry::file(name) };
        alloc::vec![at("beta.txt", 300), at(".hidden", 100), at("alpha.txt", 200)]
    }

    fn names(a: &App) -> Vec<&str> {
        a.entries().iter().map(|e| e.name.as_str()).collect()
    }

    /// A dot hides an entry, `Ctrl+H` shows it, and showing it costs a listing.
    ///
    /// **The listing is the half worth asserting.** Hiding *drops* entries rather than skipping
    /// them at draw time, so the ones hidden are not held anywhere — a toggle that only flipped
    /// the flag would show the same rows it was already showing, which looks exactly like a
    /// binding that did not fire.
    #[test]
    fn a_dot_hides_an_entry_until_the_toggle_asks_for_it() {
        let mut a = app();
        a.show("/home", dated());
        assert_eq!(names(&a), alloc::vec!["alpha.txt", "beta.txt"], "the dot is not listed");

        a.update(Msg::Key(KeyEvent::new(1, HIDDEN_KEYCODE, KEY_DOWN, MOD_CTRL)));
        assert_eq!(a.take_goto().as_deref(), Some("/home"), "the toggle asks for a listing");
        // …and the browser is the thing that filters, so the same entries come back different.
        a.show("/home", dated());
        assert_eq!(names(&a), alloc::vec![".hidden", "alpha.txt", "beta.txt"]);

        // Off again, by the menu row this time rather than the chord.
        a.update(Msg::ToggleHidden);
        let _ = a.take_goto();
        a.show("/home", dated());
        assert_eq!(names(&a), alloc::vec!["alpha.txt", "beta.txt"]);
    }

    /// Ordering rearranges what the tab holds — no listing — and the selection follows the file.
    ///
    /// **The selection is the part that is easy to get wrong.** `selected` is an index, so
    /// leaving it alone across a re-sort silently selects whatever lands on that line; a person
    /// watching the name they picked jump elsewhere would reasonably think the browser had
    /// selected something else.
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

        // And the same order chosen twice is not a change.
        a.update(Msg::SetOrder(libfs::Order::NewestFirst));
        assert_eq!(names(&a), alloc::vec!["beta.txt", "alpha.txt"]);
    }

    /// The View menu shows which order is in force and whether hidden files are shown.
    ///
    /// **Exactly one order is marked at a time**, which is what makes it a radio group rather
    /// than four independent rows, and the toggle's mark tracks the flag both ways.
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
        // **The chords are checked before the prompt**, or the field's branch returns first and
        // the chord does nothing at all while a name is being typed.
        //
        // The name is asserted unchanged as well, and that half is the *field's* doing rather
        // than the ordering's: `apply` drops control characters. Kept because it says what the
        // whole gesture leaves behind, and labelled so nobody reads it as what the ordering
        // buys (PR #270 review, worth fixing 5).
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

        // **The root is its own parent**, so this asks for nothing rather than erroring — which
        // is why the control is never disabled: a greyed-out state is one more thing to draw.
        let mut a = App::new("/");
        a.update(Msg::Up);
        assert_eq!(a.take_goto(), None);
    }

    #[test]
    fn the_keyboard_and_the_pointer_reach_the_same_three_messages() {
        // A browser where Enter and a row press disagree about what "open" means is two
        // browsers. Down-Down-Enter must land where pressing the third row lands.
        let mut a = app();
        for _ in 0..2 {
            a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_DOWN, KEY_DOWN, 0)));
        }
        a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_ENTER, KEY_DOWN, 0)));
        // Rows are [archive/, work/, a.txt, notes.txt]; two downs from row 0 is `a.txt`.
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
        // A listing of a *different* directory has no row the old selection refers to.
        // `list_view` clamps a stale index rather than clearing it, so a browser that kept one
        // would silently select whatever happened to be at that position.
        //
        // **Two presses, not one**, and the reason is the whole point of a control: with the
        // reset deleted the selection is whatever it was, and after *one* press that is `Some(0)`
        // — the same value the correct code produces. The test passed for both implementations
        // until the control was run.
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
        // Something must be selected first, or this passes for a browser that never selects
        // anything at all — the same trap the test above fell into.
        let mut a = app();
        a.update(Msg::Key(KeyEvent::new(1, libkern::abi::KEY_DOWN, KEY_DOWN, 0)));
        assert!(a.list_selected().is_some(), "precondition: something is selected");
        a.show("/home/empty", Vec::new());
        assert_eq!(a.list_selected(), None, "nothing to select, and no phantom row 0");
        assert!(a.entries().is_empty());
    }

    #[test]
    fn a_configure_is_accepted_and_the_listing_gets_the_room() {
        // A browser has no grid to refit: more height is more visible rows, which `list_view`
        // derives from what it is given.
        let mut a = app();
        let before = a.list_h();
        assert!(a.resize(Size::new(800, 600)));
        assert!(a.list_h() > before);
        assert!(!a.resize(Size::new(800, 600)), "the same size is not a change");
    }
    /// The window's own tree survives being diffed frame to frame.
    ///
    /// **The class of bug this catches cost a QEMU round trip** (M14 Part A): `diff` rejects a
    /// parent whose children are *partly* keyed, and adding one unkeyed sibling to a keyed row
    /// makes the whole window undiffable — so nothing draws at all, in a way no layout or paint
    /// test can see. The states below are the ones a menu bar introduces: shut, open, and a word
    /// under the pointer, each of which changes a row's shape.
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

    /// The bar words are where `check-login` aims, which is a hardcoded number in a gate.
    ///
    /// **A gate cannot link this crate**, so it spells the browser's chrome metrics as constants
    /// and aims at `(fx + 20, fy + 1 + TITLE_BAR_H + MENU_BAR_H / 2)` for the File menu. Nothing
    /// connected that number to the layout until now — and M14 Part A changed what a bar word
    /// *is*, from a `button` to a `menu_item` with different padding. If the word ever moves out
    /// from under that point the gate fails in QEMU with no explanation; here it fails in a
    /// second and says which number is wrong.
    #[test]
    fn the_gate_aims_inside_the_file_word() {
        let mut a = app();
        let cell = libui::layout::FixedCell { w: 8, h: 16 };
        let size = a.window_size();
        let view = a.view(&UiTheme::default(), None);
        let l = libui::layout::layout(&view, Rect::new(0, 0, size.w, size.h), &cell);
        let file = libui::layout::locate(&view, &l, MENU_BAR_KEY).expect("the File word is keyed");
        let edit = libui::layout::locate(&view, &l, MENU_BAR_KEY + 1).expect("the Edit word is keyed");

        // The gate's own arithmetic, in window-local coordinates.
        let (ax, ay) = (20, 1 + TITLE_BAR_H as i32 + MENU_BAR_H as i32 / 2);
        assert!(
            file.contains(ax, ay),
            "check-login aims at ({ax}, {ay}) for the File menu and the word is at {file:?}"
        );
        // …and it is not so close to Edit that a pixel of drift chooses the other menu.
        assert!(!edit.contains(ax, ay), "the aim point is inside Edit as well: File {file:?}, Edit {edit:?}");
    }

    /// Every widget id in the tree, in walk order.
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

    /// **Every** menu in the bar can be opened, not just the ones a test happened to name.
    ///
    /// **The View menu did nothing for two milestones** (M15 Part E). `MENU_COUNT` was a
    /// constant beside a `menu_table` that grew: the binary asked for two anchors, the third
    /// menu's `anchor()` answered `None`, and the popup had nowhere to hang from — a bar word
    /// that opened nothing, silently. The count is derived now, and this walks the *whole* bar
    /// rather than a menu chosen when the test was written.
    #[test]
    fn every_menu_in_the_bar_has_somewhere_to_hang_from() {
        let mut a = app();
        let cell = libui::layout::FixedCell { w: 8, h: 16 };
        let size = a.window_size();
        let view = a.view(&UiTheme::default(), None);
        let l = libui::layout::layout(&view, Rect::new(0, 0, size.w, size.h), &cell);
        let n = a.menu_count();
        assert!(n >= 2, "a bar with fewer than two menus is not this window's");
        // **Through the method the binary calls**, which is the whole point of it being a
        // method: the loop that was wrong lived in a `main.rs` no test builds.
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

    /// The quick-access panel sits where [`SIDEBAR_PAD`] says, and reaches the status bar.
    ///
    /// **It was a margin and is an edge** (desktop refresh, Part I): M15 Part E put six pixels
    /// around the panel so the window would show through and make it read as a panel; the design
    /// runs it to the window's edge instead and lets its own ground do that work. The assertion
    /// is the same either way — the rows sit `SIDEBAR_PAD` inside the panel's box — which is why
    /// it kept passing at nought and needed its name changed rather than its arithmetic
    /// (PR #320 review, worth fixing 8).
    ///
    /// **And it fills its column**, which it did not: sized to the listing's height while the
    /// column is that plus the header, it stopped short of the status bar and the window showed
    /// through below it.
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
        // **Down to the status bar**, with no window showing through beneath it.
        let status = libui::layout::locate(&e, &l, STATUS_KEY).expect("the status bar is keyed");
        assert_eq!(
            box_.origin.y + box_.size.h as i32,
            status.origin.y,
            "the panel stops short of the status bar, leaving the window showing through"
        );
        // And the listing's own pane reaches it too.
        let pane = libui::layout::locate(&e, &l, LIST_PANE_KEY).expect("the pane is keyed");
        assert_eq!(pane.origin.y + pane.size.h as i32, status.origin.y, "the listing stops short of the status bar");
    }

    /// A scrollbar drag survives the repaint that follows it.
    ///
    /// **This is the test the earlier ones were missing, and the bug they let through.** Every
    /// other pointer test here routes an event and reads the state; the *application* rebuilds
    /// its tree after every event, and `list_view` followed the selection on every build — so a
    /// drag computed the right offset and the next frame put it back on row 0. The bar moved and
    /// the listing did not, which is exactly what was reported (M15 Part D).
    #[test]
    fn a_scrollbar_drag_survives_the_next_repaint() {
        let mut a = app();
        a.show("/home", (0..80).map(|i| Entry::file(&alloc::format!("f{i}"))).collect());
        let cell = libui::layout::FixedCell { w: 8, h: 16 };
        let size = a.window_size();
        let theme = UiTheme::default();
        let mut tree = libui::diff::Tree::new();
        let mut r = libui::route::Router::new();
        // A frame, exactly as the binary draws one.
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

    /// The wheel scrolls the listing, **through the real tree and router**.
    ///
    /// **The wiring is the thing under test.** `ListState::wheel` is `libui`'s and tested there;
    /// what only this can say is that the element `list_view` returns carries an `on_wheel` at
    /// all — delete the one line and every other test in this file stays green while the
    /// browser silently stops scrolling.
    #[test]
    fn the_wheel_scrolls_the_listing() {
        let mut a = app();
        // More rows than fit, or there is nothing to scroll and the offset clamps back to zero.
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
            // Over the middle of the window, which is the listing.
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
    ///
    /// **This is `click-not-acted-on` as the browser experiences it** — the intermittent
    /// fault that failed PR #280's CI, where the compositor delivered a press *and* a release to
    /// this window at the File word's coordinates and no menu opened.
    ///
    /// The two halves land in different frames whenever the click is the one that *raises* the
    /// window, because the raise costs a recompose in between; that is why it looked random and
    /// why it hit KVM more often than TCG. The frame in between is drawn with the word
    /// **hovered** — the press is what establishes hover — and a hovered `menu_item` has three
    /// layers where a resting one has one, so every node inside it is re-identified. The fix is
    /// in `libui`: `Router` captures the node carrying the handler, which is the word itself and
    /// survives. This is the application-level proof that the path a person clicks is repaired.
    #[test]
    fn a_click_split_across_a_repaint_still_opens_the_menu() {
        let mut a = app();
        let cell = libui::layout::FixedCell { w: 8, h: 16 };
        let size = a.window_size();
        let theme = UiTheme::default();
        let mut tree = libui::diff::Tree::new();
        let mut router = libui::route::Router::new();

        // Frame 1: nothing hovered, because hover is what the press is about to establish.
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

        // Frame 2: a repaint between the two halves, with the word lit. **Lit explicitly, not
        // via `hovered_key`** — crossings are suppressed while a capture is in force, so the
        // press does not itself set `inside`, and a frame built from `hovered_key` here would be
        // identical to frame 1 and the test would pass against the bug. It did, until the
        // negative control caught it. What matters is only that *a* repaint between the halves
        // re-identifies the nodes inside the word, which is what a highlight does.
        let lit = a.view(&theme, Some(MENU_BAR_KEY));
        let l2 = libui::layout::layout(&lit, Rect::new(0, 0, size.w, size.h), &cell);
        let before = ids(&tree);
        tree.update(&lit, &l2).expect("the view is diffable");
        // **The precondition, asserted rather than assumed** (PR #281 review, optional 4). If
        // `menu_item` ever stops changing shape on hover there is nothing to survive, and this
        // test would go quietly green while proving nothing — which is the exact trap its first
        // version fell into for a different reason.
        assert_ne!(ids(&tree), before, "the repaint re-identified nothing, so this proves nothing");

        // The release, in that new frame.
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

    /// A press on a row produces `Grab` — the message every drag test injects by hand.
    ///
    /// **Every other drag test here starts at `a.update(Msg::Grab(i))`**, which tests what the
    /// browser does *once* a gesture has started and says nothing about whether a press starts
    /// one. That is the shape of "test the component, not the path", and it is why the whole
    /// drag could be broken with every drag test green.
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
                // **Past the sidebar**, which took the left of the window in M14 Part D: `120`
                // used to be over the listing and is now over *Documents*, so the press produced
                // a `Place` and no `Grab`. The listing starts at `SIDEBAR_W`.
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

    /// A drag that leaves the window in **one** motion still hands off.
    ///
    /// **Reported from a real session** — "click and drag doesn't work" — and it was not a flake,
    /// though it looked like one for weeks. The hand-off lived only in the already-dragging
    /// branch, which the *second* motion is the first to reach, so a motion that both crossed the
    /// slop and left the window armed a drag and then sat there. Dragging slowly worked, which is
    /// what made it look intermittent; `check-login` hit it whenever its injected motions
    /// coalesced.
    #[test]
    fn a_drag_that_leaves_in_one_motion_still_hands_off() {
        let mut a = app();
        a.update(Msg::Grab(row(2))); // a.txt, a file
        // The first motion only records the origin — that much was never in doubt.
        assert_eq!(a.pointer_moved(120, row_y(&a, 2), 1), Gesture::None);
        // **One motion, past the slop and past the window's right edge.**
        let far = a.window_size().w as i32 + 200;
        assert_eq!(
            a.pointer_moved(far, row_y(&a, 2) + 240, 1),
            Gesture::HandOff,
            "a drag that left the window in one step never asked the compositor to carry it"
        );
        assert!(a.take_drag().is_some(), "and it carries the entry that was pressed");
    }

    /// **The negative control**: a drag that stays inside is still the browser's own.
    ///
    /// Without it, a version that handed off on every slop crossing would pass the test above and
    /// break dragging a file onto a folder in the same window.
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
        // …and it still hands off when it does leave.
        let far = a.window_size().w as i32 + 200;
        assert_eq!(a.pointer_moved(far, row_y(&a, 1), 1), Gesture::HandOff);
    }

    /// `check-login` clicks a menu row by index, and this is where that index is checked.
    ///
    /// **It has moved twice and broken the gate both times** — once when the File menu gained
    /// separators, again when M14 Part B added New Window and Quit — and both times the failure
    /// appeared as a *rename prompt that never opened*, several steps away from the menu it was
    /// really about. A gate cannot link this crate, so it spells the row as a number; this makes
    /// that number wrong in one second instead of in a three-minute boot.
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
