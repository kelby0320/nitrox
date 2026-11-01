use super::*;
use crate::element::Node;

/// Tab, Enter and Escape belong to whatever is above the field.
#[test]
fn declines_the_keys_that_belong_above_it() {
    const KEY_TAB: u16 = 15;
    const KEY_ENTER: u16 = 28;
    const KEY_ESC: u16 = 1;
    for key in [KEY_TAB, KEY_ENTER, KEY_ESC] {
        let mut f = TextFieldState::with_text("abc");
        assert!(!f.apply(key, 0, 0), "keycode {key} was claimed by the field");
        assert_eq!(f.text(), "abc", "keycode {key} changed the text");
    }
}

#[test]
fn claims_the_keys_it_handles() {
    const KEY_A: u16 = 30;
    let mut f = TextFieldState::new();
    assert!(f.apply(KEY_A, 0, 0), "a letter was not claimed");
    assert_eq!(f.text(), "a");
}

/// Ctrl-C folds to `0x03` in the keymap; inserting it would put an unprintable byte in a password.
#[test]
fn control_characters_are_not_text() {
    const KEY_C: u16 = 46;
    let mut f = TextFieldState::new();
    assert!(!f.apply(KEY_C, librsproto::surface::MOD_CTRL, 0), "Ctrl-C was treated as text");
    assert_eq!(f.text(), "");
    // Negative control: the same key without Ctrl *is* text.
    assert!(f.apply(KEY_C, 0, 0));
    assert_eq!(f.text(), "c");
}

/// Caps Lock upper-cases a letter and Num Lock makes keypad 1 a digit, once the lock arrives.
#[test]
fn a_field_types_under_the_keys_locks() {
    use librsproto::surface::{LOCK_CAPS, LOCK_NUM};
    const KEY_A: u16 = 30;
    const KEY_KP1: u16 = 79;
    let mut f = TextFieldState::new();
    assert!(f.apply(KEY_A, 0, LOCK_CAPS));
    assert!(f.apply(KEY_KP1, 0, LOCK_NUM));
    assert_eq!(f.text(), "A1");
}

/// The caret must never land inside a character; slicing at it would panic.
#[test]
fn the_caret_stays_on_character_boundaries() {
    let mut f = TextFieldState::new();
    f.insert('é'); // two bytes
    f.insert('x');
    assert_eq!(f.cursor(), 3);
    assert!(f.left());
    assert_eq!(f.cursor(), 2, "left stopped inside the two-byte character");
    assert!(f.left());
    assert_eq!(f.cursor(), 0);
    assert!(!f.left(), "left at the start reported a move");
    // Slicing at the cursor is what a caret-splitting render does; it panics off-boundary.
    let _ = f.text().split_at(f.cursor());
    assert!(f.right());
    assert_eq!(f.cursor(), 2, "right stopped inside the two-byte character");
}

/// Backspace deletes a *character*, not a byte.
#[test]
fn backspace_removes_a_whole_character() {
    let mut f = TextFieldState::with_text("aé");
    assert!(f.backspace());
    assert_eq!(f.text(), "a");
    assert!(f.backspace());
    assert_eq!(f.text(), "");
    assert!(!f.backspace(), "backspace on an empty field reported a change");
}

/// `delete` is the other direction, and is the one an implementation is most likely to
/// omit or alias to backspace.
#[test]
fn delete_removes_forward_and_backspace_removes_back() {
    let mut f = TextFieldState::with_text("abc");
    f.home();
    assert!(f.delete());
    assert_eq!(f.text(), "bc", "delete removed the wrong side");
    assert_eq!(f.cursor(), 0, "delete moved the caret");
    assert!(!f.backspace(), "backspace at the start reported a change");
    f.end();
    assert!(!f.delete(), "delete at the end reported a change");
}

/// Home and End answer whether they moved, so a caller can skip a repaint. Pressing Home
/// twice must not report a change the second time.
#[test]
fn home_and_end_report_only_real_movement() {
    let mut f = TextFieldState::with_text("abc");
    assert!(f.home());
    assert!(!f.home(), "Home at the start reported a move");
    assert!(f.end());
    assert!(!f.end(), "End at the end reported a move");
}

/// A mask counts characters, not bytes.
#[test]
fn masking_counts_characters_not_bytes() {
    let f = TextFieldState::with_text("aé");
    assert_eq!(f.text().len(), 3, "the fixture is not multi-byte");
    let e: Element<()> = text_field(&f, true, WidgetState { active: true, ..Default::default() }, &Theme::default());
    assert_eq!(rendered(&e), "**", "the mask leaked the byte length");
    // Negative control: unmasked shows the real text.
    let e: Element<()> = text_field(&f, false, WidgetState::default(), &Theme::default());
    assert_eq!(rendered(&e), "aé");
}

/// The caret is drawn from `active`, so a field in an unfocused window does not blink one.
#[test]
fn the_caret_appears_only_when_active() {
    let f = TextFieldState::with_text("ab");
    let active: Element<()> =
        text_field(&f, false, WidgetState { active: true, ..Default::default() }, &Theme::default());
    let idle: Element<()> = text_field(&f, false, WidgetState::default(), &Theme::default());
    assert_eq!(row_children(&active), 3, "no caret between the two text runs");
    assert_eq!(row_children(&idle), 2, "an inactive field drew a caret");
}

/// The split is at the cursor, not the end.
#[test]
fn the_caret_splits_the_text_at_the_cursor() {
    let mut f = TextFieldState::with_text("abcd");
    f.home();
    f.right();
    let e: Element<()> = text_field(&f, false, WidgetState { active: true, ..Default::default() }, &Theme::default());
    assert_eq!(runs(&e), alloc::vec!["a", "bcd"], "the caret was not placed at the cursor");
}

/// Every `Text` run in the tree, in order.
fn runs<Msg>(e: &Element<Msg>) -> alloc::vec::Vec<alloc::string::String> {
    let mut out = alloc::vec::Vec::new();
    walk(e, &mut out);
    out
}

fn walk<Msg>(e: &Element<Msg>, out: &mut alloc::vec::Vec<alloc::string::String>) {
    if let Node::Text(s) = &e.node {
        out.push(s.clone());
    }
    for c in e.children() {
        walk(c, out);
    }
}

/// The concatenated text, which is what a reader of the field would see.
fn rendered<Msg>(e: &Element<Msg>) -> alloc::string::String {
    runs(e).concat()
}

/// How many children the content `Row` has — two text runs, plus the caret when active.
fn row_children<Msg>(e: &Element<Msg>) -> usize {
    fn find<Msg>(e: &Element<Msg>) -> Option<usize> {
        if let Node::Row { children, .. } = &e.node {
            return Some(children.len());
        }
        e.children().find_map(find)
    }
    find(e).expect("the field has a content row")
}

#[test]
fn a_scrub_leaves_nothing_in_the_allocation() {
    let mut f = TextFieldState::with_text("secret");
    let at = f.text().as_ptr();
    assert!(f.backspace());
    f.scrub();
    assert_eq!((f.text(), f.cursor()), ("", 0));
    // SAFETY: a scrub keeps the allocation, and those six bytes were written.
    let left = unsafe { core::slice::from_raw_parts(at, 6) };
    assert_eq!(left, [0; 6]);
}
