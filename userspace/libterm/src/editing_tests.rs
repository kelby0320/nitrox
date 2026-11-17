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
        let mut s = Screen { grid: Grid::new(cols, rows), parser: Parser::new(), disc: Discipline::new() };
        s.disc.set_width(Some(cols));
        s.disc.set_prompt(2);
        s.draw(b"> ");
        s
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
