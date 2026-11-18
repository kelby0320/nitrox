use super::*;
use crate::cell::Ansi;
use crate::parse::{MAX_PER_BYTE, Parser};

/// Drive a grid from real bytes through the real parser.
fn feed(g: &mut Grid, s: &str) {
    let mut p = Parser::new();
    let mut out = [Op::Print('\0'); MAX_PER_BYTE];
    for &b in s.as_bytes() {
        let n = p.feed(b, &mut out);
        g.apply_all(&out[..n]);
    }
}

/// `select_all` takes every line the grid holds, scrollback included.
#[test]
fn select_all_takes_the_scrollback_too() {
    let mut g = Grid::new(20, 3);
    feed(&mut g, "one\r\ntwo\r\nthree\r\nfour\r\nfive");
    assert!(g.oldest_line() < g.top_line(), "there is scrollback to miss");

    assert!(g.select_all(), "something was selected");
    let text = g.selected_text().expect("a selection has text");
    for line in ["one", "two", "three", "four", "five"] {
        assert!(text.contains(line), "{line:?} is missing from {text:?}");
    }
}

/// `find_from` reports cell columns, not bytes (`é` is one cell and two bytes).
#[test]
fn find_from_reports_cells_not_bytes() {
    let mut g = Grid::new(30, 2);
    feed(&mut g, "ééxtarget");
    let line = g.top_line();
    assert_eq!(g.find_from("target", line, 0), Some((line, 3)), "two accents and an x are three cells, not five bytes");
    g.select_from(line, 3);
    g.extend(line, 3 + "target".chars().count());
    assert_eq!(g.selected_text().as_deref(), Some("target"));

    assert_eq!(g.find_from("x", line, 1), Some((line, 2)), "the rest of the line is searched");
}

#[test]
fn select_all_on_a_blank_grid_selects_nothing() {
    let mut g = Grid::new(20, 3);
    assert!(!g.select_all(), "an empty grid has nothing to select");
    assert!(!g.has_selection());
    assert_eq!(g.selected_text(), None, "and nothing to copy");
}

#[test]
fn select_all_stops_at_the_text() {
    let mut g = Grid::new(30, 3);
    feed(&mut g, "one\r\ntwo\r\nshort");
    assert!(g.select_all());
    let sel = g.selection().expect("a selection");
    assert_eq!(sel.head.1, 5, "`short` is five cells, and the grid is thirty wide");
    assert_eq!(g.selected_text().as_deref(), Some("one\ntwo\nshort"));
}

#[test]
fn find_from_walks_the_history() {
    let mut g = Grid::new(20, 3);
    feed(&mut g, "alpha\r\nbeta\r\ngamma\r\nbeta again");
    let first = g.find_from("beta", g.oldest_line(), 0).expect("the first beta");
    let next = g.find_from("beta", first.0, first.1 + 1).expect("the second beta");
    assert!(next.0 > first.0, "the second is on a later line: {first:?} then {next:?}");
    assert_eq!(g.find_from("zeta", g.oldest_line(), 0), None, "and what is absent is absent");
}

#[test]
fn clearing_the_scrollback_keeps_the_screen_and_the_numbering() {
    let mut g = Grid::new(20, 3);
    feed(&mut g, "one\r\ntwo\r\nthree\r\nfour\r\nfive");
    let top = g.top_line();
    assert!(g.oldest_line() < top, "there is scrollback to clear");

    assert!(g.clear_scrollback(), "something was dropped");
    assert_eq!(g.top_line(), top, "the screen's absolute number did not move");
    assert_eq!(g.oldest_line(), top, "and the history now starts at the screen");
    assert_eq!(g.line_text(top).as_deref(), Some("three"), "the screen is untouched");
    assert!(!g.clear_scrollback(), "and a second clear has nothing to do");
}

#[test]
fn a_word_is_a_run_of_non_blanks() {
    let mut g = Grid::new(30, 2);
    feed(&mut g, "ls --colour=auto /home");
    let line = g.top_line();

    assert!(g.select_word_at(line, 4), "inside the flag");
    assert_eq!(g.selected_text().as_deref(), Some("--colour=auto"));

    assert!(g.select_word_at(line, 0), "the first word");
    assert_eq!(g.selected_text().as_deref(), Some("ls"));

    assert!(!g.select_word_at(line, 2), "the space between them");
}

#[test]
fn a_line_selection_stops_at_the_text() {
    let mut g = Grid::new(30, 2);
    feed(&mut g, "short");
    let line = g.top_line();
    assert!(g.select_line_at(line));
    let sel = g.selection().expect("a selection");
    assert_eq!(sel.anchor, (line, 0));
    assert_eq!(sel.head, (line, 5), "to the end of `short`, not to column 30");
    assert_eq!(g.selected_text().as_deref(), Some("short"));
}

fn line(g: &Grid, row: usize) -> alloc::string::String {
    let s: alloc::string::String = (0..g.cols()).map(|c| g.cell(row, c).unwrap().ch).collect();
    s.trim_end().into()
}

fn view(g: &Grid, top: u64, row: usize) -> alloc::string::String {
    let s: alloc::string::String =
        (0..g.cols()).map(|c| g.view_cell(top, row, c).map(|x| x.ch).unwrap_or(' ')).collect();
    s.trim_end().into()
}

#[test]
fn a_wrapped_line_rejoins_on_a_widen_and_two_short_lines_do_not() {
    // The assertion the flag exists for: a rewrap that joined adjacent rows would also
    // rejoin the long line, so the short pair staying two rows separates them.
    let mut g = Grid::new(6, 6);
    feed(&mut g, "abcdefghij\r\n"); // 10 characters at width 6: one soft wrap
    feed(&mut g, "xx\r\n");
    feed(&mut g, "yy\r\n");
    assert_eq!((line(&g, 0), line(&g, 1)), ("abcdef".into(), "ghij".into()));
    assert!(g.row_wrapped(0), "the long line continues below");
    assert!(!g.row_wrapped(2), "and a short one does not");

    g.resize(12, 6);
    assert_eq!(line(&g, 0), "abcdefghij", "the soft wrap was undone");
    assert_eq!(line(&g, 1), "xx", "and the pair that was never one line is still two");
    assert_eq!(line(&g, 2), "yy");
}

#[test]
fn a_rewrap_moves_the_breaks_and_never_the_count_of_lines() {
    let mut g = Grid::new(20, 4);
    for i in 0..10 {
        feed(&mut g, &alloc::format!("line{i} with a tail on it\r\n"));
    }
    let before = g.logical_lines();
    assert!(before >= 10, "ten lines and a partial one: {before}");

    g.resize(60, 4);
    assert_eq!(g.logical_lines(), before, "widening moved breaks, not lines");
    g.resize(9, 4);
    assert_eq!(g.logical_lines(), before, "and narrowing made more rows, not more lines");
    g.resize(20, 4);
    assert_eq!(g.logical_lines(), before, "and back where it started");
}

#[test]
fn what_the_ring_drops_on_a_narrow_is_counted_rather_than_lost() {
    let mut g = Grid::new(20, 3);
    for _ in 0..900 {
        feed(&mut g, "0123456789012345678\r\n"); // 19 characters: one row at width 20
    }
    let before = g.logical_lines();
    assert!(before > 890, "the history is deep enough to hit the cap when halved: {before}");

    let r = g.resize(10, 3); // every line is now two rows
    let after = g.logical_lines();
    assert!(after < before, "the ring really did evict: {before} -> {after}");
    assert_eq!(
        after + r.evicted_lines(),
        before,
        "every line that went is accounted for by the eviction, not by the rewrap"
    );
}

#[test]
fn a_line_feed_clears_a_stale_wrap() {
    // A row that once wrapped and was overwritten must stop being marked, or a widen joins it.
    let mut g = Grid::new(6, 4);
    feed(&mut g, "abcdefghij"); // row 0 "abcdef" wrapped -> row 1 "ghij"
    assert!(g.row_wrapped(0));
    feed(&mut g, "\x1b[1;1H"); // back into the wrapped row
    feed(&mut g, "xx\r\n"); // a short line, ended explicitly
    assert!(!g.row_wrapped(0), "the line feed ended it");

    g.resize(12, 4);
    assert_eq!(line(&g, 0), "xxcdef");
    assert_eq!(line(&g, 1), "ghij", "the row below was not joined onto it");
}

#[test]
fn erasing_to_the_margin_ends_a_wrapped_row() {
    let mut g = Grid::new(6, 4);
    feed(&mut g, "abcdefghij");
    feed(&mut g, "\x1b[1;3H\x1b[K"); // row 0, column 2, erase to the margin
    assert!(!g.row_wrapped(0));

    g.resize(12, 4);
    assert_eq!(line(&g, 0), "ab");
    assert_eq!(line(&g, 1), "ghij");
}

#[test]
fn erasing_the_display_ends_the_wrapped_rows_it_blanks() {
    // Each `ED` arm clears the flags of the rows it blanked; `ToStart` leaves this row's own.
    let mut g = Grid::new(6, 4);
    feed(&mut g, "abcdefghij\r\n"); // rows 0-1, row 0 wrapped
    feed(&mut g, "klmnopqrst"); // rows 2-3, row 2 wrapped
    assert!(g.row_wrapped(0) && g.row_wrapped(2));

    feed(&mut g, "\x1b[2;1H\x1b[J");
    assert!(g.row_wrapped(0), "the row above the erase is untouched");
    assert!(!g.row_wrapped(2), "the erased row continues nothing");

    let mut g = Grid::new(6, 4);
    feed(&mut g, "abcdefghij\r\n");
    feed(&mut g, "klmnopqrst");
    feed(&mut g, "\x1b[3;1H\x1b[1J");
    assert!(!g.row_wrapped(0), "an erased row continues nothing");
    assert!(g.row_wrapped(2), "this row's tail was not erased, so it still continues");

    let mut g = Grid::new(6, 4);
    feed(&mut g, "abcdefghij\r\n");
    feed(&mut g, "\x1b[2J");
    assert!(!g.row_wrapped(0), "a cleared screen continues nothing");
}

#[test]
fn narrowing_then_widening_puts_the_text_back() {
    let mut g = Grid::new(20, 6);
    feed(&mut g, "the quick brown fox\r\nshort\r\n");
    g.resize(7, 6);
    g.resize(20, 6);
    assert_eq!(line(&g, 0), "the quick brown fox");
    assert_eq!(line(&g, 1), "short");
    assert_eq!(g.cursor(), (2, 0), "and the cursor is still on the line it was on");
}

#[test]
fn a_reflow_maps_a_scrolled_back_anchor_onto_the_same_text() {
    let mut g = Grid::new(5, 3);
    for i in 0..8 {
        feed(&mut g, &alloc::format!("aaaa{i}bbbb\r\n")); // 9 chars at width 5: two rows
    }
    let anchor = g.oldest_line() + 4;
    let want = view(&g, anchor, 0);
    assert_eq!(want, "aaaa2", "the first half of a line the old width broke in two");

    let r = g.resize(10, 3);
    let now = view(&g, r.map_line(anchor), 0);
    assert!(now.starts_with(&want), "anchor landed on {now:?}, wanted {want:?} first");
    assert_ne!(r.map_line(anchor), anchor, "the numbering really did move");
    assert!(
        !view(&g, anchor, 0).starts_with(&want),
        "and the unmapped number shows something else, so this test can tell them apart"
    );
}

#[test]
fn the_cursor_follows_the_character_it_was_on() {
    let mut g = Grid::new(10, 4);
    feed(&mut g, "hello world"); // wraps: "hello worl" then "d"
    assert_eq!(g.cursor(), (1, 1));
    g.resize(20, 4);
    assert_eq!(line(&g, 0), "hello world");
    assert_eq!(g.cursor(), (0, 11), "still just after the text it was after");
}

/// A space printed is text, so a narrower grid wraps it with the rest; one erased is gone.
#[test]
fn a_reflow_keeps_spaces_printed_and_drops_those_erased() {
    let mut g = Grid::new(8, 4);
    feed(&mut g, "ab   \r\nx");
    g.resize(3, 4);
    assert_eq!((line(&g, 2), g.cursor()), ("x".into(), (2, 1)), "the spaces took a row");
    let mut g = Grid::new(8, 4);
    feed(&mut g, "ab   \x1b[2D\x1b[K\r\nx");
    g.resize(2, 4);
    assert_eq!((line(&g, 2), g.cursor()), ("x".into(), (2, 1)), "of three spaces, the two erased took none");
    let mut g = Grid::new(8, 4);
    feed(&mut g, "ab  \x1b[1K\r\nx");
    g.resize(2, 4);
    assert_eq!(line(&g, 1), "x", "nor a row erased to the cursor from its start");
}

/// What was printed in a row goes with it when it scrolls.
#[test]
fn a_rows_printed_extent_scrolls_with_it() {
    let mut g = Grid::new(8, 2);
    feed(&mut g, "x\r\nab  \r\nc");
    g.resize(2, 4);
    assert_eq!((line(&g, 1), line(&g, 3), g.cursor()), ("ab".into(), "c".into(), (3, 1)), "its spaces took a row");
}

/// Its offset kept exactly, a row made for it where its line ends one exactly.
#[test]
fn the_cursor_past_its_lines_text_stays_where_the_next_character_goes() {
    let mut g = Grid::new(10, 4);
    feed(&mut g, "abcd");
    g.resize(4, 4);
    assert_eq!(g.cursor(), (1, 0));
    assert!(g.row_wrapped(0), "the cursor's row continues the line");
    let mut g = Grid::new(10, 4);
    feed(&mut g, "ab\x1b[3C");
    g.resize(4, 4);
    assert_eq!(g.cursor(), (1, 1), "three cells past its text, as it was");
}

#[test]
fn resizing_an_idle_screen_pushes_nothing_into_the_scrollback() {
    let mut g = Grid::new(20, 10);
    feed(&mut g, "/home> ");
    g.resize(40, 5);
    assert_eq!(g.scrollback().len(), 0, "nine blank rows became nine lines of history");
    assert_eq!(line(&g, 0), "/home>");
    assert_eq!(g.cursor(), (0, 7), "the prompt still has the cursor after it");
}

#[test]
fn a_resize_to_the_same_shape_changes_nothing() {
    let mut g = Grid::new(8, 4);
    feed(&mut g, "abc\r\ndef");
    let before = (g.cursor(), g.top_line(), line(&g, 0), line(&g, 1));
    let r = g.resize(8, 4);
    assert_eq!((g.cursor(), g.top_line(), line(&g, 0), line(&g, 1)), before);
    assert_eq!(r.map_line(7), 7, "the identity map, not an empty one that answers zero");
}

#[test]
fn a_coloured_run_to_the_right_margin_is_not_trimmed_away() {
    // `trim_trailing_blanks` drops only never-written cells: a bar painted to the margin
    // was written on purpose.
    let mut g = Grid::new(6, 3);
    feed(&mut g, "\x1b[44m\x1b[K"); // erase the line under blue: six blue spaces
    // A real resize: an unchanged shape takes `resize`'s early return and skips the trim.
    g.resize(8, 3);
    let blue = (0..6).filter(|c| g.cell(0, *c).unwrap().attrs.bg == Colour::Ansi(Ansi::Blue));
    assert_eq!(blue.count(), 6, "the painted run survived the rewrap");
}

#[test]
fn printing_advances_the_cursor_and_stores_the_characters() {
    let mut g = Grid::new(10, 3);
    feed(&mut g, "hi");
    assert_eq!(line(&g, 0), "hi");
    assert_eq!(g.cursor(), (0, 2));
}

#[test]
fn a_full_line_then_a_newline_does_not_leave_a_blank_one() {
    // The reason wrapping is deferred: wrap-on-write double-spaces a program printing exactly
    // `cols` characters per line.
    let mut g = Grid::new(4, 4);
    feed(&mut g, "abcd\r\nxy");
    assert_eq!(line(&g, 0), "abcd");
    assert_eq!(line(&g, 1), "xy", "a blank line appeared between them");
    assert_eq!(line(&g, 2), "");
}

#[test]
fn a_line_feed_moves_down_and_leaves_the_column_alone() {
    let mut g = Grid::new(10, 3);
    feed(&mut g, "abc\ndef");
    assert_eq!(line(&g, 0), "abc");
    assert_eq!(line(&g, 1), "   def", "the line feed also returned to column 0");
}

#[test]
fn the_character_after_a_full_line_wraps() {
    let mut g = Grid::new(4, 4);
    feed(&mut g, "abcde");
    assert_eq!(line(&g, 0), "abcd");
    assert_eq!(line(&g, 1), "e");
    assert_eq!(g.cursor(), (1, 1));
}

#[test]
fn a_wrapped_line_keeps_filling_rather_than_wrapping_every_character() {
    let mut g = Grid::new(4, 4);
    feed(&mut g, "abcdefg");
    assert_eq!(line(&g, 0), "abcd");
    assert_eq!(line(&g, 1), "efg", "the wrapped line did not keep filling");
    assert_eq!(line(&g, 2), "");
    assert_eq!(g.cursor(), (1, 3));
}

#[test]
fn the_cursor_rests_on_the_last_column_rather_than_past_it() {
    let mut g = Grid::new(4, 4);
    feed(&mut g, "abcd");
    assert_eq!(g.cursor(), (0, 3), "the cursor left the screen");
}

#[test]
fn movement_cancels_a_pending_wrap() {
    let mut g = Grid::new(4, 4);
    feed(&mut g, "abcd\rZ");
    assert_eq!(line(&g, 0), "Zbcd");
    assert_eq!(line(&g, 1), "", "the write wrapped after a carriage return");

    let mut g = Grid::new(4, 4);
    feed(&mut g, "abcd\x1b[1;1HZ");
    assert_eq!(line(&g, 0), "Zbcd");
}

#[test]
fn a_newline_at_the_bottom_scrolls_and_the_line_goes_to_scrollback() {
    let mut g = Grid::new(4, 2);
    feed(&mut g, "one\r\ntwo\r\nsix");
    assert_eq!(line(&g, 0), "two");
    assert_eq!(line(&g, 1), "six", "the third line did not land on the bottom row");
    assert_eq!(g.scrollback().len(), 1);
    let first: alloc::string::String = g.scrollback().next().unwrap().cells.iter().map(|c| c.ch).collect();
    assert_eq!(first.trim_end(), "one");
}

#[test]
fn scrollback_is_bounded() {
    let mut g = Grid::new(4, 1);
    for i in 0..(SCROLLBACK + 50) {
        feed(&mut g, &alloc::format!("{}\r\n", i % 10));
    }
    assert_eq!(g.scrollback().len(), SCROLLBACK, "the ring grew past its bound");
}

#[test]
fn a_scrolled_back_view_reads_from_the_scrollback_and_the_screen_at_once() {
    // The seam: a viewport straddling the boundary serves top rows from history, bottom
    // rows from the screen, which are stored differently.
    let mut g = Grid::new(4, 2);
    for i in 0..6 {
        feed(&mut g, &alloc::format!("{i}\r\n"));
    }
    assert_eq!(g.top_line(), 5, "five lines scrolled off");
    assert_eq!(g.oldest_line(), 0, "and none evicted");

    let at = |top: u64, row: usize| g.view_cell(top, row, 0).map(|c| c.ch);
    assert_eq!((at(5, 0), at(5, 1)), (Some('5'), Some(' ')), "the bottom: the live screen");
    assert_eq!((at(3, 0), at(3, 1)), (Some('3'), Some('4')), "wholly in the scrollback");
    assert_eq!((at(4, 0), at(4, 1)), (Some('4'), Some('5')), "straddling: history above, screen below",);
}

#[test]
fn the_view_is_anchored_to_a_line_so_output_does_not_drag_it() {
    let mut g = Grid::new(4, 2);
    for i in 0..6 {
        feed(&mut g, &alloc::format!("{i}\r\n"));
    }
    let before: alloc::vec::Vec<Option<char>> = (0..2).map(|r| g.view_cell(2, r, 0).map(|c| c.ch)).collect();

    for i in 6..9 {
        feed(&mut g, &alloc::format!("{i}\r\n"));
    }
    let after: alloc::vec::Vec<Option<char>> = (0..2).map(|r| g.view_cell(2, r, 0).map(|c| c.ch)).collect();
    assert_eq!(before, after, "the anchored view moved when output arrived");
    assert_eq!(before, [Some('2'), Some('3')]);
}

#[test]
fn an_anchor_the_scrollback_has_evicted_clamps_to_the_oldest_line_kept() {
    let mut g = Grid::new(4, 1);
    for i in 0..(SCROLLBACK + 50) {
        feed(&mut g, &alloc::format!("{}\r\n", i % 10));
    }
    assert_eq!(g.oldest_line(), (SCROLLBACK + 50 - SCROLLBACK) as u64, "50 evicted");
    assert_eq!(g.clamp_view(0), g.oldest_line(), "an evicted anchor");
    assert_eq!(g.clamp_view(u64::MAX), g.top_line(), "and one past the bottom");
    assert_eq!(g.clamp_view(60), 60, "one that is still there is left alone");
}

#[test]
fn the_cursor_is_not_in_a_view_scrolled_away_from_it() {
    // Scrolling back does not move the cursor; drawing it at its screen row would invert
    // a cell of history.
    let mut g = Grid::new(4, 2);
    for i in 0..6 {
        feed(&mut g, &alloc::format!("{i}\r\n"));
    }
    assert_eq!(g.cursor(), (1, 0));
    assert_eq!(g.view_cursor(g.top_line()), Some((1, 0)), "following: where it always was");
    assert_eq!(g.view_cursor(4), None, "one row past the bottom is still out of view");
    assert_eq!(g.view_cursor(5), Some((1, 0)), "and the row before it is the last in view");
    assert_eq!(g.view_cursor(0), None, "scrolled to the top: nowhere near it");
}

#[test]
fn cursor_addressing_clamps_and_never_scrolls() {
    let mut g = Grid::new(10, 3);
    feed(&mut g, "\x1b[99;99H");
    assert_eq!(g.cursor(), (2, 9));
    assert_eq!(g.scrollback().len(), 0, "addressing the cursor scrolled the screen");

    // One past the end, not far past: `99` clamps to 2 and `98 % 3` is also 2, so only row 4
    // separates clamping from a modulo.
    let mut g = Grid::new(10, 3);
    feed(&mut g, "\x1b[4;11H");
    assert_eq!(g.cursor(), (2, 9), "the cursor wrapped instead of clamping");
}

#[test]
fn relative_movement_clamps_at_every_edge() {
    let mut g = Grid::new(10, 3);
    feed(&mut g, "\x1b[10A\x1b[10D");
    assert_eq!(g.cursor(), (0, 0), "moving up and left past the origin");
    feed(&mut g, "\x1b[10B\x1b[10C");
    assert_eq!(g.cursor(), (2, 9), "moving down and right past the corner");
    assert_eq!(g.scrollback().len(), 0, "relative movement scrolled");
}

#[test]
fn backspace_stops_at_the_left_margin() {
    let mut g = Grid::new(4, 2);
    feed(&mut g, "ab\r\n\x08\x08\x08");
    assert_eq!(g.cursor(), (1, 0));
}

#[test]
fn tabs_advance_to_the_next_stop_and_stop_at_the_edge() {
    let mut g = Grid::new(20, 2);
    feed(&mut g, "a\t");
    assert_eq!(g.cursor(), (0, TAB_WIDTH));
    feed(&mut g, "\t");
    assert_eq!(g.cursor(), (0, TAB_WIDTH * 2));
    let mut g = Grid::new(10, 2);
    feed(&mut g, "\t\t\t\t");
    assert_eq!(g.cursor(), (0, 9));
}

#[test]
fn erasing_fills_with_the_current_background_not_the_default() {
    let mut g = Grid::new(4, 2);
    feed(&mut g, "\x1b[44m\x1b[2K");
    for c in 0..4 {
        let cell = g.cell(0, c).unwrap();
        assert_eq!(cell.ch, ' ');
        assert_eq!(cell.attrs.bg, Colour::Ansi(Ansi::Blue), "column {c} kept the default");
    }
}

#[test]
fn an_erased_cell_carries_no_ink_attributes() {
    let mut g = Grid::new(4, 2);
    feed(&mut g, "\x1b[1;4;44m\x1b[2K");
    let cell = g.cell(0, 0).unwrap();
    assert!(!cell.attrs.flags.contains(Flags::BOLD));
    assert!(!cell.attrs.flags.contains(Flags::UNDERLINE));
    assert_eq!(cell.attrs.fg, Colour::Default);
}

#[test]
fn erasing_while_reversed_fills_with_the_foreground() {
    // `blank()` keeps `REVERSE` and drops `BOLD`/`UNDERLINE`: that retention is what makes
    // "reverse, erase line" paint a bar, and removing it would otherwise leave tests green.
    let p = crate::cell::Palette::default();
    let mut g = Grid::new(4, 1);
    feed(&mut g, "\x1b[7m\x1b[2K");
    let (fg, bg) = g.cell(0, 0).unwrap().attrs.resolve(&p);
    assert_eq!(bg, p.foreground, "an erase under reverse did not paint a bar");
    assert_eq!(fg, p.background);

    let mut g = Grid::new(4, 1);
    feed(&mut g, "\x1b[2K");
    assert_eq!(g.cell(0, 0).unwrap().attrs.resolve(&p).1, p.background);
}

#[test]
fn erase_in_line_covers_exactly_its_extent() {
    let mut g = Grid::new(6, 2);
    feed(&mut g, "abcdef\x1b[1;3H\x1b[K"); // cursor at column 2, erase to end
    assert_eq!(line(&g, 0), "ab");

    let mut g = Grid::new(6, 2);
    feed(&mut g, "abcdef\x1b[1;3H\x1b[1K"); // erase to start, inclusive of the cursor
    assert_eq!(line(&g, 0), "   def", "`EL 1` left the cell under the cursor");

    let mut g = Grid::new(6, 2);
    feed(&mut g, "abcdef\x1b[1;3H\x1b[2K");
    assert_eq!(line(&g, 0), "");
}

#[test]
fn erase_in_display_covers_exactly_its_extent() {
    let mut g = Grid::new(4, 3);
    feed(&mut g, "aaaa\r\nbbbb\r\ncccc\x1b[2;3H\x1b[J"); // row 1 col 2, erase to end
    assert_eq!(line(&g, 0), "aaaa");
    assert_eq!(line(&g, 1), "bb");
    assert_eq!(line(&g, 2), "");

    let mut g = Grid::new(4, 3);
    feed(&mut g, "aaaa\r\nbbbb\r\ncccc\x1b[2;3H\x1b[1J");
    assert_eq!(line(&g, 0), "");
    assert_eq!(line(&g, 1), "   b");
    assert_eq!(line(&g, 2), "cccc");
}

#[test]
fn clearing_the_screen_does_not_push_it_into_scrollback() {
    let mut g = Grid::new(4, 3);
    feed(&mut g, "aaaa\r\nbbbb\x1b[2J");
    assert_eq!(g.scrollback().len(), 0);
    assert_eq!(line(&g, 0), "");
}

#[test]
fn attributes_accumulate_and_reset_clears_all_of_them() {
    let mut g = Grid::new(4, 2);
    feed(&mut g, "\x1b[1m\x1b[4m\x1b[31m\x1b[44m");
    let a = g.attributes();
    assert!(a.flags.contains(Flags::BOLD.with(Flags::UNDERLINE)));
    assert_eq!(a.fg, Colour::Ansi(Ansi::Red));
    assert_eq!(a.bg, Colour::Ansi(Ansi::Blue));
    feed(&mut g, "\x1b[m");
    assert_eq!(g.attributes(), Attributes::default(), "reset left something behind");
}

#[test]
fn a_character_carries_the_attributes_in_force_when_it_was_written() {
    let mut g = Grid::new(4, 2);
    feed(&mut g, "\x1b[31ma\x1b[32mb");
    assert_eq!(g.cell(0, 0).unwrap().attrs.fg, Colour::Ansi(Ansi::Red));
    assert_eq!(g.cell(0, 1).unwrap().attrs.fg, Colour::Ansi(Ansi::Green));
}

#[test]
fn damage_is_the_rows_that_changed_and_clears_when_taken() {
    let mut g = Grid::new(10, 4);
    feed(&mut g, "\x1b[3;1H");
    g.take_damage(); // drop the initial full-screen damage *and* settle the cursor on row 2
    feed(&mut g, "x");
    assert_eq!(g.take_damage(), alloc::vec![2], "only the written row should be dirty");
    assert_eq!(g.take_damage(), alloc::vec![], "damage was not cleared");

    feed(&mut g, "y");
    assert_eq!(g.take_damage(), alloc::vec![2]);
}

#[test]
fn a_scroll_damages_every_row() {
    let mut g = Grid::new(4, 3);
    feed(&mut g, "a\r\nb\r\nc");
    g.take_damage();
    feed(&mut g, "\r\nd");
    assert_eq!(g.take_damage(), alloc::vec![0, 1, 2]);
}

#[test]
fn erasing_the_display_damages_every_row_it_touched() {
    let mut g = Grid::new(4, 3);
    feed(&mut g, "aaaa\r\nbbbb\r\ncccc");
    g.take_damage();
    feed(&mut g, "\x1b[2J");
    assert_eq!(g.take_damage(), alloc::vec![0, 1, 2]);

    let mut g = Grid::new(4, 3);
    feed(&mut g, "aaaa\r\nbbbb\r\ncccc\x1b[2;3H");
    g.take_damage();
    feed(&mut g, "\x1b[J");
    let d = g.take_damage();
    assert!(d.contains(&1) && d.contains(&2), "rows below the cursor were not damaged: {d:?}");
}

#[test]
fn moving_the_cursor_damages_the_row_it_left_and_the_one_it_reached() {
    // The render paints the cursor into its cell, so the row it left keeps an inverted block
    // until repainted.
    let mut g = Grid::new(10, 4);
    g.take_damage();
    feed(&mut g, "\x1b[2;5H");
    assert_eq!(g.take_damage(), alloc::vec![0, 1], "left row 0, arrived in row 1");

    feed(&mut g, "\x1b[C");
    assert_eq!(g.take_damage(), alloc::vec![1]);

    assert_eq!(g.take_damage(), alloc::vec![]);
}

#[test]
fn a_wrap_damages_the_row_the_cursor_left() {
    // `abcd` rests the cursor on the last column and `e` wraps it; reporting only row 1 would
    // leave a phantom block at the end of row 0.
    let mut g = Grid::new(4, 3);
    feed(&mut g, "abcd");
    g.take_damage();
    feed(&mut g, "e");
    assert_eq!(g.take_damage(), alloc::vec![0, 1], "the row the cursor left was not repainted");
}

#[test]
fn a_degenerate_size_is_clamped_rather_than_panicking() {
    let mut g = Grid::new(0, 0);
    feed(&mut g, "abc\r\n\x1b[9;9H\x1b[2J");
    assert_eq!(g.cols(), 1);
    assert_eq!(g.rows(), 1);
    assert_eq!(g.cursor(), (0, 0));
}

#[test]
fn a_press_selects_nothing_until_it_is_dragged() {
    let mut g = Grid::new(20, 4);
    feed(&mut g, "hello world");
    g.select_from(0, 0);
    assert!(g.selection().is_some(), "the gesture is in progress");
    assert_eq!(g.selected_text(), None);
    g.extend(0, 5);
    assert_eq!(g.selected_text().as_deref(), Some("hello"));
}

#[test]
fn dragging_backwards_selects_the_same_text() {
    let mut g = Grid::new(20, 4);
    feed(&mut g, "hello world");
    g.select_from(0, 5);
    g.extend(0, 0);
    assert_eq!(g.selected_text().as_deref(), Some("hello"));
}

#[test]
fn a_selection_across_a_hard_break_keeps_the_newline() {
    let mut g = Grid::new(20, 4);
    feed(&mut g, "one\r\ntwo");
    g.select_from(0, 0);
    g.extend(1, 3);
    assert_eq!(g.selected_text().as_deref(), Some("one\ntwo"));
}

#[test]
fn a_soft_wrap_does_not_become_a_newline() {
    // A soft wrap is one line of text, and pasting it with a newline inserts a break the
    // program never wrote.
    let mut g = Grid::new(4, 4);
    feed(&mut g, "abcdefgh");
    assert!(g.row_wrapped(0), "precondition: the first row is a soft wrap");
    g.select_from(0, 0);
    g.extend(1, 4);
    assert_eq!(g.selected_text().as_deref(), Some("abcdefgh"));
}

#[test]
fn trailing_blanks_are_not_selected() {
    // Cells past the end of a line were never written; keeping them pastes a screen's width
    // of spaces.
    let mut g = Grid::new(20, 4);
    feed(&mut g, "hi\r\nthere");
    g.select_from(0, 0);
    g.extend(1, 5);
    assert_eq!(g.selected_text().as_deref(), Some("hi\nthere"));
}

#[test]
fn a_selection_in_the_history_names_the_same_text_as_output_arrives() {
    // Absolute positions, so a selection in the history does not drift while output arrives.
    let mut g = Grid::new(20, 2);
    feed(&mut g, "first\r\nsecond\r\nthird\r\n");
    let top = g.top_line();
    g.select_from(top - 2, 0);
    g.extend(top - 2, 5);
    assert_eq!(g.selected_text().as_deref(), Some("first"));
    feed(&mut g, "fourth\r\nfifth\r\n");
    assert_eq!(
        g.selected_text().as_deref(),
        Some("first"),
        "two more lines scrolled past and the selection still names the same text"
    );
}

#[test]
fn a_selection_survives_a_rewrap() {
    // A logical line's text is invariant across a rewrap, so an offset into it survives. At
    // width 3 "defg" sits in row 1 at a different column than at width 4, which
    // `map_position` follows and `map_line` alone cannot.
    let mut g = Grid::new(4, 4);
    feed(&mut g, "abcdefgh");
    g.select_from(0, 3);
    g.extend(1, 3);
    assert_eq!(g.selected_text().as_deref(), Some("defg"));
    g.resize(3, 4);
    assert_eq!(
        g.selected_text().as_deref(),
        Some("defg"),
        "the same characters, at a width where they sit in different cells"
    );
}

#[test]
fn clearing_a_selection_that_is_not_there_damages_nothing() {
    let mut g = Grid::new(20, 4);
    feed(&mut g, "hello");
    let _ = g.take_damage();
    assert!(!g.clear_selection());
    assert!(g.take_damage().is_empty(), "nothing was selected, so nothing changed");
    g.select_from(0, 0);
    g.extend(0, 3);
    let _ = g.take_damage();
    assert!(g.clear_selection());
    assert!(!g.take_damage().is_empty(), "the highlight has to be painted out");
}

#[test]
fn extending_without_an_anchor_selects_nothing() {
    let mut g = Grid::new(20, 4);
    feed(&mut g, "hello");
    g.extend(0, 3);
    assert_eq!(g.selection(), None);
    assert_eq!(g.selected_text(), None);
}

#[test]
fn a_selection_reaching_past_the_history_stops_at_what_exists() {
    let mut g = Grid::new(10, 2);
    for i in 0..(SCROLLBACK + 10) {
        feed(&mut g, "x\r\n");
        let _ = i;
    }
    g.select_from(0, 0);
    g.extend(2, 1);
    let _ = g.selected_text();
}
/// `has_selection` agrees with `selected_text().is_some()` in every state. An empty range
/// is the case they could disagree in.
#[test]
fn has_selection_is_the_cheap_half_of_selected_text() {
    let mut g = Grid::new(20, 4);
    feed(&mut g, "hello world");
    assert!(!g.has_selection(), "nothing swept out");
    assert_eq!(g.selected_text(), None);
    g.select_from(0, 3);
    assert_eq!(g.selected_text(), None, "a press selects no text");
    assert!(!g.has_selection(), "…and this must agree, which selection().is_some() would not");
    assert!(g.selection().is_some(), "the range does exist — that is the trap");
    g.extend(0, 8);
    assert!(g.selected_text().is_some());
    assert!(g.has_selection());
    g.clear_selection();
    assert_eq!(g.selected_text(), None);
    assert!(!g.has_selection());
}

/// The alternate screen comes up clear with the cursor where it was, and leaving it puts back the
/// screen it covered, its cursor and its attributes.
#[test]
fn the_alternate_screen_covers_the_main_one_and_gives_it_back() {
    let mut g = Grid::new(10, 3);
    feed(&mut g, "one\r\ntwo\x1b[31m");
    let attrs = g.attributes();
    feed(&mut g, "\x1b[?1049h");
    assert!(g.alternate());
    assert_eq!(((0..3).map(|r| line(&g, r)).collect::<Vec<_>>(), g.cursor()), (alloc::vec![String::new(); 3], (1, 3)));
    feed(&mut g, "\x1b[0m\x1b[Hpage\x1b[?1049h");
    assert_eq!(line(&g, 0), "page", "asked again, the shown screen stays");
    feed(&mut g, "\x1b[?1049l");
    assert!(!g.alternate());
    assert_eq!((line(&g, 0), line(&g, 1), g.cursor(), g.attributes()), ("one".into(), "two".into(), (1, 3), attrs));
}

/// What scrolls off the alternate screen is gone: it is no history, and the main screen's lines
/// keep their numbers.
#[test]
fn the_alternate_screen_keeps_no_history() {
    let mut g = Grid::new(10, 2);
    feed(&mut g, "a\r\nb\r\nc");
    let (history, top) = (g.scrollback().len(), g.top_line());
    feed(&mut g, "\x1b[?1049h1\r\n2\r\n3\r\n4");
    assert_eq!((g.scrollback().len(), g.top_line(), line(&g, 1)), (history, top, "4".into()));
    feed(&mut g, "\x1b[?1049l");
    assert_eq!((line(&g, 0), line(&g, 1)), ("b".into(), "c".into()));
}

/// A refit on the alternate screen rewraps the main one beneath, as it is shown again, and cuts
/// the alternate one, which its program draws again.
#[test]
fn a_refit_on_the_alternate_screen_rewraps_the_main_one_beneath() {
    let mut g = Grid::new(10, 3);
    feed(&mut g, "abcdefghij");
    feed(&mut g, "\x1b[?1049h\x1b[H0123456789\r\nxy");
    g.resize(5, 3);
    assert!(g.alternate());
    assert_eq!((line(&g, 0), line(&g, 1), g.cursor()), ("01234".into(), "xy".into(), (1, 2)));
    feed(&mut g, "\x1b[?1049l");
    assert_eq!((line(&g, 0), line(&g, 1), g.cursor()), ("abcde".into(), "fghij".into(), (1, 4)));
}

/// What a grid holds whatever it was fed: rows sized to it, the cursor on it, nothing printed past
/// its width.
fn assert_sound(g: &Grid) {
    assert_eq!(g.cells.len(), g.cols * g.rows);
    assert_eq!((g.wrapped.len(), g.printed.len(), g.dirty.len()), (g.rows, g.rows, g.rows));
    assert!(g.row < g.rows && g.col < g.cols, "the cursor {:?} is off a {}x{} grid", g.cursor(), g.cols, g.rows);
    assert!(g.printed.iter().all(|&p| p <= g.cols), "printed past the width: {:?}", g.printed);
}

/// A selection names lines of the screen it was made on, so a switch, either way, drops it; and
/// draws every row again, though the cursor has not moved.
#[test]
fn a_switch_drops_the_selection_and_draws_every_row() {
    let mut g = Grid::new(10, 3);
    feed(&mut g, "one");
    for switch in ["\x1b[?1049h", "\x1b[?1049l"] {
        g.select_from(0, 0);
        g.extend(0, 2);
        g.take_damage();
        feed(&mut g, switch);
        assert!(g.selection().is_none(), "{switch:?}");
        assert_eq!(g.take_damage(), alloc::vec![0, 1, 2], "{switch:?}");
    }
}

/// A refit cuts the alternate screen to its new bounds, the cursor and each row's printed extent
/// too, drops a selection, and draws every row again.
#[test]
fn a_refit_cuts_the_alternate_screen_to_its_bounds() {
    let mut g = Grid::new(10, 3);
    feed(&mut g, "\x1b[?1049h\x1b[H0123456789\x1b[3;10H");
    g.select_from(0, 0);
    g.extend(0, 4);
    g.take_damage();
    g.resize(5, 2);
    assert_sound(&g);
    assert_eq!((line(&g, 0), g.cursor()), ("01234".into(), (1, 4)));
    assert!(g.selection().is_none());
    assert_eq!(g.take_damage(), alloc::vec![0, 1]);
    feed(&mut g, "\x1b[?1049l");
    assert_sound(&g);
}
