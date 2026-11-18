//! The pager (`page(1)`): lines in rows at the terminal's width, a page of them over a status row,
//! and the keys that move it; pure, so host-tested. [`show`] drives it on a stage's terminal.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use libline::{Key, Typed};

mod terminal;

pub use terminal::{Shown, show};

/// The size taken where the terminal cannot say, as a serial line cannot.
pub const SIZE_UNKNOWN: (usize, usize) = (80, 24);

/// Columns between tab stops, as `libterm`'s.
const TAB: usize = 8;

/// `t`'s lines as the shell draws it, and how many of them lead as its header.
pub fn table_lines(t: &libstream::Table) -> (Vec<String>, usize) {
    let laid = libstream::display::table(t);
    let head = usize::from(laid.header.is_some());
    (laid.header.into_iter().chain(laid.rows).collect(), head)
}

/// What a key comes to.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Act {
    /// The page or its status changed: draw it again.
    Draw,
    /// Leave the pager.
    Quit,
    /// Nothing changed.
    Nothing,
}

/// The lines, the page shown of them, and a search.
pub struct Pager {
    lines: Vec<String>,
    /// Leading lines drawn as a table's header.
    head: usize,
    /// Each row: its line, and the bytes of the line it shows.
    rows: Vec<(usize, usize, usize)>,
    width: usize,
    /// Rows of text: the terminal's, less the status row.
    height: usize,
    top: usize,
    typing: Option<String>,
    /// The last word searched for, and the line it was last found on.
    word: Option<String>,
    found: Option<usize>,
    /// The last search found nothing, said until the next key.
    missed: bool,
}

/// `line` as it is drawn: a tab to the next stop, and no other control, so a page cannot drive the
/// terminal.
fn clean(line: &str) -> String {
    let mut out = String::new();
    let mut col = 0;
    for c in line.chars() {
        if c == '\t' {
            let next = (col / TAB + 1) * TAB;
            out.extend(core::iter::repeat_n(' ', next - col));
            col = next;
        } else if !c.is_control() {
            out.push(c);
            col += 1;
        }
    }
    out
}

/// `lines` broken into rows of at most `width` characters; an empty line is a row.
fn rows_of(lines: &[String], width: usize) -> Vec<(usize, usize, usize)> {
    let mut rows = Vec::new();
    for (l, line) in lines.iter().enumerate() {
        let (mut start, mut n) = (0, 0);
        for (i, _) in line.char_indices() {
            if n == width {
                rows.push((l, start, i));
                (start, n) = (i, 0);
            }
            n += 1;
        }
        rows.push((l, start, line.len()));
    }
    rows
}

impl Pager {
    /// `lines` on a terminal `size` (columns, rows), the first `head` of them a header.
    pub fn new(lines: &[String], head: usize, size: (usize, usize)) -> Pager {
        let lines = lines.iter().map(|l| clean(l)).collect();
        let mut p = Pager {
            lines,
            head,
            rows: Vec::new(),
            width: 1,
            height: 1,
            top: 0,
            typing: None,
            word: None,
            found: None,
            missed: false,
        };
        p.resize(size);
        p
    }

    /// The terminal is now `size`: the rows are broken again, the line at the top kept there.
    pub fn resize(&mut self, (cols, rows): (usize, usize)) {
        let line = self.rows.get(self.top).map_or(0, |r| r.0);
        (self.width, self.height) = (cols.max(1), rows.saturating_sub(1).max(1));
        self.rows = rows_of(&self.lines, self.width);
        self.top = self.rows.iter().position(|r| r.0 == line).unwrap_or(0).min(self.last_top());
    }

    /// The terminal's size, as last given.
    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height + 1)
    }

    /// Whether every row fits on one page, so there is nothing to page.
    pub fn fits(&self) -> bool {
        self.rows.len() <= self.height
    }

    fn last_top(&self) -> usize {
        self.rows.len().saturating_sub(self.height)
    }

    fn line_at_top(&self) -> usize {
        self.rows.get(self.top).map_or(0, |r| r.0)
    }

    /// Act on what a byte typed came to.
    pub fn feed(&mut self, typed: Typed) -> Act {
        if let Some(word) = &mut self.typing {
            match typed {
                Typed::Byte(b'\r' | b'\n') => {
                    let word = self.typing.take().unwrap_or_default();
                    if !word.is_empty() {
                        (self.word, self.found) = (Some(word), None);
                    }
                    self.search();
                }
                Typed::Byte(0x08 | 0x7f) if word.pop().is_none() => self.typing = None,
                Typed::Byte(0x08 | 0x7f) => {}
                Typed::Byte(b @ 0x20..=0x7e) => word.push(char::from(b)),
                _ => return Act::Nothing,
            }
            return Act::Draw;
        }
        let (top, missed) = (self.top, core::mem::take(&mut self.missed));
        match typed {
            Typed::Byte(b'q') => return Act::Quit,
            Typed::Byte(b' ' | b'f') | Typed::Key(Key::PageDown) => self.top = (top + self.height).min(self.last_top()),
            Typed::Byte(b'b') | Typed::Key(Key::PageUp) => self.top = top.saturating_sub(self.height),
            Typed::Byte(b'j' | b'\r' | b'\n') | Typed::Key(Key::Down) => self.top = (top + 1).min(self.last_top()),
            Typed::Byte(b'k') | Typed::Key(Key::Up) => self.top = top.saturating_sub(1),
            Typed::Byte(b'g') | Typed::Key(Key::Home) => self.top = 0,
            Typed::Byte(b'G') | Typed::Key(Key::End) => self.top = self.last_top(),
            Typed::Byte(b'/') => self.typing = Some(String::new()),
            Typed::Byte(b'n') => self.search(),
            _ => {}
        }
        if self.top != top || missed || self.typing.is_some() || self.missed { Act::Draw } else { Act::Nothing }
    }

    /// To the next line holding the word, past the top line and the line it was last found on.
    fn search(&mut self) {
        let Some(word) = &self.word else { return };
        let from = self.found.filter(|&f| f >= self.line_at_top()).unwrap_or(self.line_at_top()) + 1;
        match self.lines.iter().enumerate().skip(from).find(|(_, l)| l.contains(word.as_str())) {
            Some((l, _)) => {
                self.found = Some(l);
                self.top = self.rows.iter().position(|r| r.0 == l).unwrap_or(0).min(self.last_top());
            }
            None => self.missed = true,
        }
    }

    /// What the status row says: where the page is, a search being typed, or one that failed.
    pub fn status(&self) -> String {
        if let Some(word) = &self.typing {
            return format!("/{word}");
        }
        if let (true, Some(word)) = (self.missed, &self.word) {
            return format!("not found: {word}");
        }
        let last = self.rows.get((self.top + self.height).min(self.rows.len()).saturating_sub(1)).map_or(0, |r| r.0);
        let end = if self.top >= self.last_top() { " (END)" } else { "" };
        format!("lines {}-{} of {}{end}", self.line_at_top() + 1, last + 1, self.lines.len())
    }

    /// The text of each row shown, top to bottom.
    pub fn page(&self) -> Vec<&str> {
        self.rows[self.top..].iter().take(self.height).map(|&(l, a, b)| &self.lines[l][a..b]).collect()
    }

    /// What to write to draw the page and the status row, each row placed by its number, so no
    /// row's wrap or a terminal's way with one can move the next.
    pub fn draw(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for r in 0..self.height {
            out.extend_from_slice(format!("\x1b[{};1H\x1b[K", r + 1).as_bytes());
            if let Some(&(l, a, b)) = self.rows.get(self.top + r) {
                let text = &self.lines[l][a..b];
                if l < self.head {
                    out.extend_from_slice(format!("{}{text}\x1b[0m", libstream::display::HEADER).as_bytes());
                } else {
                    out.extend_from_slice(text.as_bytes());
                }
            }
        }
        // Short of the last column: a full bottom row would leave a wrap that scrolls the screen.
        let status: String = self.status().chars().take(self.width - 1).collect();
        out.extend_from_slice(format!("\x1b[{};1H\x1b[K\x1b[7m{status}\x1b[0m", self.height + 1).as_bytes());
        out
    }
}

#[cfg(test)]
mod tests;
