//! Line editing judged on a screen: keys encoded, read by the line discipline, drawn in a grid.

use crate::encode::{MAX_ENCODED, encode};
use crate::grid::Grid;
use crate::parse::{MAX_PER_BYTE, Op, Parser};
use alloc::string::String;
use alloc::vec::Vec;
use libkern::abi::{KEY_BACKSPACE, KEY_DELETE, KEY_END, KEY_HOME, KEY_LEFT, KEY_RIGHT};
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
                    Step::Echo(e) => self.draw(&e),
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

/// The longest line that draws right: a full row leaves a wrap pending (`TODO(tty-size)`).
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
