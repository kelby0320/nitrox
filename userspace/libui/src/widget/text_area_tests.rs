use super::*;

/// Eight pixels a character, as a fixed-advance face measures.
fn eight(s: &str) -> u32 {
    s.chars().count() as u32 * 8
}

fn fifty_lines() -> TextAreaState {
    let text: Vec<String> = (0..50).map(|i| alloc::format!("line {i}")).collect();
    TextAreaState::with_text(&text.join("\n"))
}

#[test]
fn the_point_below_a_cell_is_the_cell_at_point_turned_back() {
    let mut s = fifty_lines();
    s.scroll_to(10, 5);
    for (line, col) in [(10, 0), (10, 3), (12, 7), (14, 6)] {
        let (x, y) = s.point_below((line, col), 20, 5, eight).expect("on screen");
        assert_eq!(s.at_point(x, y - 1, 20, eight), (line, col), "a pixel above the bottom-left of ({line}, {col})");
        assert_eq!(s.at_point(x, y, 20, eight).0, line + 1, "and on it, the line below");
    }
    assert_eq!(s.point_below((10, 0), 20, 5, eight), Some((FIELD_PAD.left as i32, (FIELD_PAD.top + 20) as i32)));
}

#[test]
fn a_cell_scrolled_out_of_view_has_no_point() {
    let mut s = fifty_lines();
    s.scroll_to(10, 5);
    assert_eq!(s.point_below((9, 0), 20, 5, eight), None, "above");
    assert!(s.point_below((14, 0), 20, 5, eight).is_some(), "the last row shown");
    assert_eq!(s.point_below((15, 0), 20, 5, eight), None, "below");
    assert_eq!(s.point_below((12, 99), 20, 5, eight), None, "past the line's end");
    assert_eq!(s.point_below((60, 0), 20, 50, eight), None, "past the buffer");
}

#[test]
fn follow_brings_a_caret_scrolled_away_back_at_the_next_build() {
    let mut s = fifty_lines();
    s.place(30, 0);
    s.ensure_visible(10);
    assert_eq!(s.offset(), 21, "precondition: the caret followed");
    s.scroll_to(0, 10);
    s.ensure_visible(10);
    assert_eq!(s.offset(), 0, "precondition: scrolled away, nothing moved, it stays away");
    s.follow();
    s.ensure_visible(10);
    assert_eq!(s.offset(), 21, "followed again");
}

#[test]
fn a_cut_is_one_undo_step_of_its_own() {
    let mut s = TextAreaState::with_text("hello world");
    s.place(0, 11);
    s.insert('!');
    s.select_range((0, 6), (0, 11));
    assert!(s.cut_selection());
    assert_eq!(s.text(), "hello !");
    s.insert('?');
    assert!(s.undo());
    assert_eq!(s.text(), "hello !", "the typing after the cut is a step of its own");
    assert!(s.undo());
    assert_eq!(s.text(), "hello world!", "the cut is undone, and the typing before it kept");
    assert!(s.redo());
    assert_eq!(s.text(), "hello !");
    assert!(!TextAreaState::with_text("abc").cut_selection(), "nothing selected, nothing cut");

    // After a deletion too: its own step, and the deletion's kept.
    let mut s = TextAreaState::with_text("hello world");
    s.place(0, 11);
    assert!(s.backspace());
    s.select_range((0, 6), (0, 10));
    assert!(s.cut_selection());
    assert!(s.undo());
    assert_eq!(s.text(), "hello worl", "the cut is undone, and the deletion before it kept");
}

#[test]
fn a_line_past_the_buffer_has_no_point_though_its_row_is_shown() {
    let s = fifty_lines();
    assert_eq!(s.point_below((50, 0), 20, 60, eight), None, "row 50 of 60 shown, past the 50 lines");
}

#[test]
fn a_press_is_on_the_character_under_it_not_the_nearest_boundary() {
    let s = TextAreaState::with_text("abc\nde");
    let at =
        |x: u32, line: u32| s.char_at((FIELD_PAD.left + x) as i32, (FIELD_PAD.top + line * 20 + 10) as i32, 20, eight);
    assert_eq!(at(0, 0), Some((0, 0)), "the left edge of `a`");
    assert_eq!(at(7, 0), Some((0, 0)), "the right half of `a`, which at_point puts after it");
    assert_eq!(at(8, 0), Some((0, 1)), "the left edge of `b`");
    assert_eq!(at(23, 0), Some((0, 2)), "the last pixel of `c`");
    assert_eq!(at(24, 0), Some((0, 3)), "past the line, its end");
    assert_eq!(at(300, 1), Some((1, 2)), "far past the second line, its end");
    assert_eq!(at(4, 2), None, "below the last line");
}
