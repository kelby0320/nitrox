//! Line editing judged on a screen: keys encoded, read by the line discipline, drawn in a grid.

use crate::encode::{MAX_ENCODED, encode};
use crate::grid::Grid;
use crate::parse::{MAX_PER_BYTE, Op, Parser};
use alloc::string::String;
use alloc::vec::Vec;
use libkern::abi::{KEY_BACKSPACE, KEY_DELETE, KEY_END, KEY_ENTER, KEY_HOME, KEY_LEFT, KEY_RIGHT};
use libline::{Discipline, Step};

/// What is typed: text as its bytes, or a key as `encode` sends it.
enum In<'a> {
    Text(&'a str),
    Key(u16),
}

struct Screen {
    grid: Grid,
    parser: Parser,
    disc: Discipline,
}

impl Screen {
    /// A grid `cols` wide showing the prompt `> `, as `nxsh`'s is.
    fn new(cols: usize) -> Screen {
        let mut s = Screen { grid: Grid::new(cols, 3), parser: Parser::new(), disc: Discipline::new() };
        s.draw(b"> ");
        s
    }

    /// A grid whose discipline knows the width, as `nxsh`'s does in a window.
    fn wide(cols: usize, rows: usize) -> Screen {
        Screen::under("", cols, rows)
    }

    /// [`wide`](Self::wide), the prompt after `above`, earlier output a misplaced edit would draw on.
    fn under(above: &str, cols: usize, rows: usize) -> Screen {
        let mut s = Screen { grid: Grid::new(cols, rows), parser: Parser::new(), disc: Discipline::new() };
        s.draw(above.as_bytes());
        s.disc.set_width(Some(cols));
        s.disc.set_prompt(2);
        s.draw(b"> ");
        s
    }

    /// The window refitted to `cols`, and the shell told at its next read.
    fn refit(&mut self, cols: usize) {
        let _ = self.grid.resize(cols, self.grid.rows());
        self.disc.set_width(Some(cols));
    }

    fn rows(&self) -> Vec<String> {
        (0..self.grid.rows())
            .map(|r| {
                let text: String = (0..self.grid.cols()).map(|c| self.grid.cell(r, c).map_or(' ', |x| x.ch)).collect();
                String::from(text.trim_end())
            })
            .collect()
    }

    fn draw(&mut self, bytes: &[u8]) {
        let mut ops = [Op::Print('\0'); MAX_PER_BYTE];
        for &b in bytes {
            let n = self.parser.feed(b, &mut ops);
            self.grid.apply_all(&ops[..n]);
        }
    }

    fn type_in(&mut self, inputs: &[In]) {
        for i in inputs {
            let bytes: Vec<u8> = match i {
                In::Text(t) => t.as_bytes().to_vec(),
                In::Key(k) => {
                    let mut out = [0u8; MAX_ENCODED];
                    let n = encode(*k, 0, 0, &mut out);
                    out[..n].to_vec()
                }
            };
            for b in bytes {
                match self.disc.feed(b) {
                    Step::Echo(e) | Step::Line { echo: e, .. } => self.draw(&e),
                    Step::Key(k) => {
                        let e = self.disc.edit(k);
                        self.draw(&e);
                    }
                    _ => {}
                }
            }
        }
    }

    /// The prompt's row, trailing blanks trimmed.
    fn row(&self) -> String {
        let text = self.grid.line_text(self.grid.top_line()).unwrap_or_default();
        String::from(text.trim_end())
    }

    fn column(&self) -> usize {
        self.grid.cursor().1
    }
}

use In::{Key, Text};

#[test]
fn a_character_typed_mid_line_goes_in_where_the_cursor_is() {
    let mut s = Screen::new(20);
    s.type_in(&[Text("helo"), Key(KEY_LEFT), Text("l")]);
    assert_eq!((s.row().as_str(), s.column()), ("> hello", 6));
}

#[test]
fn home_delete_and_end_redraw_the_row() {
    let mut s = Screen::new(20);
    s.type_in(&[Text("xhello"), Key(KEY_HOME)]);
    assert_eq!(s.column(), 2);
    s.type_in(&[Key(KEY_DELETE)]);
    assert_eq!((s.row().as_str(), s.column()), ("> hello", 2));
    s.type_in(&[Key(KEY_END)]);
    assert_eq!(s.column(), 7);
}

#[test]
fn backspace_mid_line_closes_the_gap() {
    let mut s = Screen::new(20);
    s.type_in(&[Text("abxcd"), Key(KEY_LEFT), Key(KEY_LEFT), Key(KEY_BACKSPACE)]);
    assert_eq!((s.row().as_str(), s.column()), ("> abcd", 4));
    s.type_in(&[Key(KEY_RIGHT), Key(KEY_RIGHT), Key(KEY_RIGHT)]);
    assert_eq!(s.column(), 6, "Right stops at the end");
}

#[test]
fn a_recalled_line_holding_utf8_moves_a_cell_a_character() {
    let mut s = Screen::new(20);
    let shown = s.disc.replace_line("h\u{e9}llo".as_bytes());
    s.draw(&shown);
    s.type_in(&[Key(KEY_HOME), Key(KEY_RIGHT), Key(KEY_RIGHT)]);
    assert_eq!(s.column(), 4);
    s.type_in(&[Key(KEY_LEFT), Key(KEY_DELETE)]);
    assert_eq!((s.row().as_str(), s.column()), ("> hllo", 3));
}

/// Where the width is unknown, the longest line that draws right: a full row leaves a wrap pending
/// (`TODO(serial-size)`).
#[test]
fn a_line_one_cell_short_of_its_row_is_edited_right() {
    let mut s = Screen::new(10);
    s.type_in(&[Text("abcdefg")]);
    assert_eq!((s.row().as_str(), s.column()), ("> abcdefg", 9));
    s.type_in(&[Key(KEY_HOME), Key(KEY_DELETE), Key(KEY_END)]);
    assert_eq!((s.row().as_str(), s.column()), ("> bcdefg", 8));
    s.type_in(&[Key(KEY_LEFT), Key(KEY_LEFT), Text("X")]);
    assert_eq!((s.row().as_str(), s.column()), ("> bcdeXfg", 7));
}

/// Ten columns, so `> ` and eight characters fill a row.
const W: usize = 10;

#[test]
fn a_line_past_its_row_wraps_and_backspace_climbs_back() {
    let mut s = Screen::wide(W, 4);
    s.type_in(&[Text("abcdefghij")]);
    assert_eq!(
        (s.rows(), s.grid.cursor()),
        (alloc::vec!["> abcdefgh".into(), "ij".into(), "".into(), "".into()], (1, 2))
    );
    s.type_in(&[Key(KEY_BACKSPACE), Key(KEY_BACKSPACE), Key(KEY_BACKSPACE)]);
    assert_eq!((s.rows()[..2].to_vec(), s.grid.cursor()), (alloc::vec!["> abcdefg".into(), String::new()], (0, 9)));
}

#[test]
fn a_row_filled_exactly_puts_the_cursor_on_the_next() {
    let mut s = Screen::wide(W, 4);
    s.type_in(&[Text("abcdefgh")]);
    assert_eq!((s.rows()[0].as_str(), s.grid.cursor()), ("> abcdefgh", (1, 0)));
    s.type_in(&[Key(KEY_BACKSPACE)]);
    assert_eq!((s.rows()[..2].to_vec(), s.grid.cursor()), (alloc::vec!["> abcdefg".into(), String::new()], (0, 9)));
    s.type_in(&[Text("hi")]);
    assert_eq!((s.rows()[..2].to_vec(), s.grid.cursor()), (alloc::vec!["> abcdefgh".into(), "i".into()], (1, 1)));
}

#[test]
fn the_cursor_keys_cross_a_wrap_and_an_insert_rewraps_the_rest() {
    let mut s = Screen::wide(W, 4);
    s.type_in(&[Text("abcdefghijkl"), Key(KEY_HOME)]);
    assert_eq!(s.grid.cursor(), (0, 2));
    s.type_in(&[Key(KEY_END)]);
    assert_eq!(s.grid.cursor(), (1, 4));
    s.type_in(&[Key(KEY_LEFT), Key(KEY_LEFT), Key(KEY_LEFT), Key(KEY_LEFT), Key(KEY_LEFT), Text("X")]);
    assert_eq!((s.rows()[..2].to_vec(), s.grid.cursor()), (alloc::vec!["> abcdefgX".into(), "hijkl".into()], (1, 0)));
    s.type_in(&[Key(KEY_LEFT), Key(KEY_RIGHT), Key(KEY_RIGHT)]);
    assert_eq!(s.grid.cursor(), (1, 1));
}

#[test]
fn a_delete_on_the_first_row_pulls_the_rest_back_and_clears_what_is_left() {
    let mut s = Screen::wide(W, 4);
    s.type_in(&[Text("abcdefghijkl"), Key(KEY_HOME)]);
    s.type_in(&[Key(KEY_DELETE), Key(KEY_DELETE), Key(KEY_DELETE), Key(KEY_DELETE), Key(KEY_DELETE)]);
    assert_eq!((s.rows()[..2].to_vec(), s.grid.cursor()), (alloc::vec!["> fghijkl".into(), String::new()], (0, 2)));
}

#[test]
fn a_long_recalled_line_replaced_by_a_short_one_leaves_nothing_behind() {
    let mut s = Screen::wide(W, 5);
    let shown = s.disc.replace_line(b"0123456789012345678901234");
    s.draw(&shown);
    assert_eq!(s.rows()[..3].to_vec(), alloc::vec![String::from("> 01234567"), "8901234567".into(), "8901234".into()]);
    let shown = s.disc.replace_line(b"ok");
    s.draw(&shown);
    assert_eq!(
        (s.rows(), s.grid.cursor()),
        (alloc::vec!["> ok".into(), "".into(), "".into(), "".into(), "".into()], (0, 4))
    );
}

#[test]
fn a_recalled_statement_of_several_lines_draws_each_on_its_own_row() {
    let mut s = Screen::wide(W, 4);
    let shown = s.disc.replace_line(b"if x {\n1 }");
    s.draw(&shown);
    assert_eq!(s.rows()[..2].to_vec(), alloc::vec![String::from("> if x {"), "1 }".into()]);
    s.type_in(&[Key(KEY_HOME), Key(KEY_LEFT)]);
    assert_eq!(s.grid.cursor(), (1, 0), "editing stays on the last row");
}

#[test]
fn enter_mid_line_goes_below_the_line() {
    let mut s = Screen::wide(W, 4);
    s.type_in(&[Text("abcdefghijkl"), Key(KEY_HOME), Key(KEY_ENTER)]);
    assert_eq!((s.rows()[1].as_str(), s.grid.cursor()), ("ijkl", (2, 0)));
    let mut s = Screen::wide(W, 4);
    s.type_in(&[Text("abcdefgh"), Key(KEY_HOME), Key(KEY_ENTER)]);
    assert_eq!(s.grid.cursor(), (1, 0), "a row filled exactly is followed by no empty one");
}

#[test]
fn after_a_refit_an_edit_counts_the_new_rows() {
    let mut s = Screen::wide(W, 4);
    s.type_in(&[Text("abcdefghijkl")]);
    let _ = s.grid.resize(8, 4);
    s.disc.set_width(Some(8));
    assert_eq!((s.rows()[..2].to_vec(), s.grid.cursor()), (alloc::vec!["> abcdef".into(), "ghijkl".into()], (1, 6)));
    s.type_in(&[Key(KEY_HOME), Key(KEY_DELETE)]);
    assert_eq!((s.rows()[..2].to_vec(), s.grid.cursor()), (alloc::vec!["> bcdefg".into(), "hijkl".into()], (0, 2)));
}

/// Each row a recalled statement ends early is cleared to its end.
#[test]
fn a_recalled_statement_over_a_long_line_leaves_no_debris() {
    let mut s = Screen::wide(W, 4);
    let shown = s.disc.replace_line(b"0123456789012345678");
    s.draw(&shown);
    let shown = s.disc.replace_line(b"if {\n1 }");
    s.draw(&shown);
    assert_eq!(s.rows()[..3].to_vec(), alloc::vec![String::from("> if {"), "1 }".into(), "".into()]);
}

#[test]
fn a_statement_of_several_rows_replaced_starts_again_on_the_prompts_row() {
    let mut s = Screen::wide(W, 4);
    let shown = s.disc.replace_line(b"if {\n1 }");
    s.draw(&shown);
    let shown = s.disc.replace_line(b"ok");
    s.draw(&shown);
    assert_eq!((s.rows()[..2].to_vec(), s.grid.cursor()), (alloc::vec![String::from("> ok"), String::new()], (0, 4)));
}

#[test]
fn a_refit_to_where_the_line_fills_its_row_leaves_the_cursor_after_it() {
    let mut s = Screen::under("prev\r\n", W, 4);
    s.type_in(&[Text("abcde")]);
    s.refit(7);
    assert_eq!(s.grid.cursor(), (2, 0), "after the line, as a space typed there would be");
    s.type_in(&[Text("f")]);
    assert_eq!(
        (s.rows()[..3].to_vec(), s.grid.cursor()),
        (alloc::vec!["prev".into(), "> abcde".into(), "f".into()], (2, 1))
    );
    s.type_in(&[Key(KEY_BACKSPACE), Key(KEY_BACKSPACE)]);
    assert_eq!(
        (s.rows()[..3].to_vec(), s.grid.cursor()),
        (alloc::vec!["prev".into(), "> abcd".into(), "".into()], (1, 6))
    );
}

#[test]
fn a_space_typed_before_a_refit_keeps_its_cell() {
    let mut s = Screen::wide(W, 4);
    s.type_in(&[Text("cat ")]);
    s.refit(5);
    s.type_in(&[Text("x")]);
    assert_eq!((s.rows()[..2].to_vec(), s.grid.cursor()), (alloc::vec!["> cat".into(), " x".into()], (1, 2)));
}

#[test]
fn a_recalled_statement_refitted_is_replaced_from_its_prompt() {
    let mut s = Screen::under("prev\r\n", W, 4);
    let shown = s.disc.replace_line(b"if x {\n1 }");
    s.draw(&shown);
    s.refit(8);
    assert_eq!(s.rows()[..3].to_vec(), alloc::vec![String::from("prev"), "> if x {".into(), "1 }".into()]);
    let shown = s.disc.replace_line(b"ok");
    s.draw(&shown);
    assert_eq!(
        (s.rows()[..3].to_vec(), s.grid.cursor()),
        (alloc::vec!["prev".into(), "> ok".into(), "".into()], (1, 4))
    );
}

/// Moved to, the row a refit filled would be held apart from the next by the terminal, and at the
/// screen's foot there would be no next.
#[test]
fn the_end_of_a_row_a_refit_filled_is_reached_as_typing_reaches_it() {
    let mut s = Screen::wide(W, 4);
    s.type_in(&[Text("abcde"), Key(KEY_HOME)]);
    s.refit(7);
    s.type_in(&[Key(KEY_END)]);
    assert_eq!(s.grid.cursor(), (1, 0));
    s.refit(W);
    assert_eq!((s.rows()[..2].to_vec(), s.grid.cursor()), (alloc::vec!["> abcde".into(), "".into()], (0, 7)));

    let mut s = Screen::under("1\r\n2\r\n3\r\n", W, 4);
    s.type_in(&[Text("abcde"), Key(KEY_HOME)]);
    s.refit(7);
    s.type_in(&[Key(KEY_END), Text("f")]);
    assert_eq!((s.rows()[2..].to_vec(), s.grid.cursor()), (alloc::vec!["> abcde".into(), "f".into()], (3, 1)));
}

#[test]
fn output_after_a_line_that_fills_its_row_is_a_line_of_its_own() {
    let mut s = Screen::wide(W, 4);
    s.type_in(&[Text("abcdefgh"), Key(KEY_ENTER)]);
    s.draw(b"out\r\n");
    s.refit(12);
    assert_eq!(s.rows()[..2].to_vec(), alloc::vec![String::from("> abcdefgh"), "out".into()]);
}

/// Wrapped, empty and filled exactly, each as a terminal lays it out, so the cursor's row is right.
#[test]
fn a_recalled_statements_earlier_rows_take_the_rows_their_text_fills() {
    let mut s = Screen::under("prev\r\n", W, 7);
    let shown = s.disc.replace_line(b"abcdefghijkl\n\nabcdefghij\nxy");
    s.draw(&shown);
    let rows = ["prev", "> abcdefgh", "ijkl", "", "abcdefghij", "xy"].map(String::from);
    assert_eq!((s.rows()[..6].to_vec(), s.grid.cursor()), (rows.to_vec(), (5, 2)));
    let shown = s.disc.replace_line(b"ok");
    s.draw(&shown);
    assert_eq!(
        (s.rows()[..3].to_vec(), s.grid.cursor()),
        (alloc::vec!["prev".into(), "> ok".into(), "".into()], (1, 4))
    );
}

/// Only the first row follows the prompt; one filled exactly stays apart from the next on a refit.
#[test]
fn a_recalled_statements_later_rows_start_at_the_margin() {
    let mut s = Screen::wide(W, 5);
    let shown = s.disc.replace_line(b"if {\n12345678\n1234567890\nx");
    s.draw(&shown);
    let rows = ["> if {", "12345678", "1234567890", "x"].map(String::from);
    assert_eq!((s.rows()[..4].to_vec(), s.grid.cursor()), (rows.to_vec(), (3, 1)));
    s.refit(12);
    assert_eq!((s.rows()[..4].to_vec(), s.grid.cursor()), (rows.to_vec(), (3, 1)));
}

/// A completion's listing goes below the line's end, wherever the cursor is in it.
#[test]
fn a_listing_leaves_the_line_whole_above_it() {
    for (typed, line) in [("abcdefghijkl", alloc::vec!["> abcdefgh", "ijkl"]), ("abcdefgh", alloc::vec!["> abcdefgh"])]
    {
        let mut s = Screen::wide(W, 8);
        s.type_in(&[Text(typed), Key(KEY_HOME), Key(KEY_RIGHT)]);
        let left = s.disc.leave();
        s.draw(&left);
        s.draw(b"x  y\r\n> ");
        let shown = s.disc.redraw();
        s.draw(&shown);
        let want: Vec<String> = line.iter().chain(&["x  y"]).chain(&line).map(|r| String::from(*r)).collect();
        assert_eq!((s.rows()[..want.len()].to_vec(), s.grid.cursor()), (want, (line.len() + 1, 3)), "{typed}");
    }
}

/// Nothing typed under a prompt as wide as the terminal: the wrap the prompt left pending is its own,
/// and what follows the line starts on the next row, not over the prompt.
#[test]
fn enter_under_a_prompt_that_fills_its_row_leaves_the_prompts_wrap() {
    let mut s = Screen { grid: Grid::new(W, 4), parser: Parser::new(), disc: Discipline::new() };
    s.draw(b"prev\r\n0123456789");
    s.disc.set_width(Some(W));
    s.disc.set_prompt(W);
    s.type_in(&[Key(KEY_ENTER)]);
    s.draw(b"out");
    assert_eq!(s.rows()[..3].to_vec(), alloc::vec![String::from("prev"), "0123456789".into(), "out".into()]);
}

/// Every row there is, history first, to the last that holds text or the cursor; and the cursor.
fn every_row(s: &Screen) -> (Vec<String>, (usize, usize)) {
    let text = |cells: &[crate::cell::Cell]| String::from(cells.iter().map(|c| c.ch).collect::<String>().trim_end());
    let mut rows: Vec<String> = s.grid.scrollback().map(|l| text(&l.cells)).collect();
    let (history, (row, col)) = (rows.len(), s.grid.cursor());
    let screen = s.rows();
    let last = screen.iter().rposition(|r| !r.is_empty()).unwrap_or(0).max(row);
    rows.extend_from_slice(&screen[..=last]);
    (rows, (history + row, col))
}

/// What `every_row` should be, laid out afresh: `above`, the prompt and the line at width `w`, each
/// row of a statement as many as its text fills, the cursor at its offset into the last.
fn laid_out(above: &[&str], prompt: &str, line: &[u8], cursor: usize, w: usize) -> (Vec<String>, (usize, usize)) {
    let mut rows = Vec::new();
    let put = |text: &[char], least: usize, rows: &mut Vec<String>| {
        let n = text.len().div_ceil(w).max(least).max(1);
        rows.extend((0..n).map(|r| {
            text[(r * w).min(text.len())..((r + 1) * w).min(text.len())].iter().collect::<String>().trim_end().into()
        }));
    };
    for a in above {
        put(&a.chars().collect::<Vec<_>>(), 1, &mut rows);
    }
    let (mut start, mut at) = (0, (0, 0));
    let parts: Vec<&[u8]> = line.split(|&b| b == b'\n').collect();
    for (k, part) in parts.iter().enumerate() {
        let lead = if k == 0 { prompt } else { "" };
        let text: Vec<char> = lead.chars().chain(String::from_utf8_lossy(part).chars()).collect();
        let mut least = 1;
        if k + 1 == parts.len() {
            let off = lead.chars().count() + String::from_utf8_lossy(&line[start..cursor]).chars().count();
            (least, at) = (off / w + 1, (rows.len() + off / w, off % w));
        }
        put(&text, least, &mut rows);
        start += part.len() + 1;
    }
    while rows.len() > at.0 + 1 && rows.last().is_some_and(|r| r.is_empty()) {
        rows.pop();
    }
    (rows, at)
}

/// The next of a fixed sequence (xorshift), below `n`.
fn below(seed: &mut u64, n: u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed % n
}

fn word(seed: &mut u64) -> String {
    (0..below(seed, 8)).map(|_| ['a', ' ', 'b', '\u{e9}'][below(seed, 4) as usize]).collect()
}

/// Typing, editing, recalling and refitting at random, the screen always shows the line where the
/// discipline puts it: at the screen's top and its foot, where a move down cannot go.
#[test]
fn random_edits_and_refits_keep_the_screen_and_the_line_in_step() {
    use libkern::abi::KEY_DELETE;
    let seed = &mut 0x9E37_79B9_7F4A_7C15_u64;
    for case in 0..240 {
        let p = 1 + below(seed, 3) as usize;
        let prompt = "$".repeat(p - 1) + " ";
        let (mut w, mut height) = (p + 1 + below(seed, 10) as usize, 16 + below(seed, 4) as usize);
        let above: Vec<&str> = if case % 2 == 0 { alloc::vec!["output"] } else { alloc::vec!["output"; 20] };
        let mut s = Screen { grid: Grid::new(w, height), parser: Parser::new(), disc: Discipline::new() };
        for a in &above {
            s.draw(a.as_bytes());
            s.draw(b"\r\n");
        }
        s.disc.set_width(Some(w));
        s.disc.set_prompt(p);
        s.draw(prompt.as_bytes());
        let mut done = Vec::new();
        for _ in 0..30 {
            match below(seed, 14) {
                0..=3 if s.disc.line().len() < 20 => {
                    let t = word(seed);
                    s.type_in(&[Text(&t)]);
                    done.push(alloc::format!("type {t:?}"));
                }
                4 => s.type_in(&[Key(KEY_BACKSPACE)]),
                5 => s.type_in(&[Key(KEY_DELETE)]),
                6 => s.type_in(&[Key(KEY_LEFT)]),
                7 => s.type_in(&[Key(KEY_RIGHT)]),
                8 => s.type_in(&[Key(KEY_HOME)]),
                9 => s.type_in(&[Key(KEY_END)]),
                10 => s.type_in(&[Text(["\x15", "\x17"][below(seed, 2) as usize])]),
                11 => {
                    let t = alloc::format!("{}\n{}", word(seed), word(seed));
                    let shown = s.disc.replace_line(t.as_bytes());
                    s.draw(&shown);
                    done.push(alloc::format!("recall {t:?}"));
                }
                12 => {
                    height = 16 + below(seed, 4) as usize;
                    let _ = s.grid.resize(w, height);
                }
                _ => {
                    w = p + 1 + below(seed, 10) as usize;
                    s.refit(w);
                }
            }
            done.push(alloc::format!("at {w}x{height}"));
            let want = laid_out(&above, &prompt, s.disc.line(), s.disc.cursor(), w);
            assert_eq!(every_row(&s), want, "case {case}: {done:?}");
        }
    }
}
