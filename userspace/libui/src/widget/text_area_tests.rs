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
