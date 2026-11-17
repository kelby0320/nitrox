//! The line discipline: what a byte does to a line being typed, and what to write so the screen
//! keeps up. Canonical mode drives it from the server; `nxsh` links it to edit its own prompt.

use alloc::vec::Vec;

/// What the caller should do after feeding one byte; the discipline *says* what to write and
/// never writes, which keeps it free of syscalls.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Step {
    /// Nothing to do.
    None,
    /// Write these bytes to the terminal (an echo, or an erase sequence).
    Echo(Vec<u8>),
    /// A line is complete. Its bytes (no terminator) are the payload; `echo` is what to
    /// write to move the cursor on, which is empty in no-echo mode.
    Line { bytes: Vec<u8>, echo: Vec<u8> },
    /// A key that is not text, reported for the caller to decide: an editing key goes to
    /// [`Discipline::edit`] or is dropped, and what to replace the line with on recall is the
    /// shell's business, not the terminal's.
    Key(Key),
    /// End of input — Ctrl-D at an empty line. Distinct from an empty *line* (a bare Enter):
    /// a reader that conflates them either exits on a stray Enter or cannot be exited.
    Eof,
}

/// A key the discipline reports rather than acts on: canonical mode drops every one, as Linux's
/// does, and `nxsh` recalls history with Up and Down and edits through [`Discipline::edit`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Key {
    /// Cursor up (`ESC [ A`).
    Up,
    /// Cursor down (`ESC [ B`).
    Down,
    /// Cursor left (`ESC [ D`).
    Left,
    /// Cursor right (`ESC [ C`).
    Right,
    /// To the line's start (`ESC [ H`, or `ESC [ 1 ~` and `ESC [ 7 ~` as other terminals send it).
    Home,
    /// To the line's end (`ESC [ F`, `ESC [ 4 ~`, `ESC [ 8 ~`).
    End,
    /// Delete the character under the cursor (`ESC [ 3 ~`).
    Delete,
    /// A page back (`ESC [ 5 ~`): the line takes no page, so it is for whoever reads keys.
    PageUp,
    /// A page on (`ESC [ 6 ~`).
    PageDown,
}

/// Where the escape-sequence parser is; a sequence naming no key is discarded, not accumulated.
#[derive(Copy, Clone, PartialEq, Eq)]
enum EscState {
    Ground,
    Esc,
    /// In a CSI (`ESC [`): its first numeric parameter so far, and whether a `;` has ended it.
    Csi {
        param: u32,
        done: bool,
    },
    /// After `ESC O` (SS3): one final byte follows.
    Ss3,
}

mod screen;

use screen::{Geometry, move_cursor};

/// One terminal's editing state. With a width it redraws by row and column; without, as on a serial
/// line, by Backspace and text, which draws a wrapped line wrong (`TODO(serial-size)`).
pub struct Discipline {
    line: Vec<u8>,
    /// Where typing lands: a character boundary, never before a recalled statement's last row.
    cursor: usize,
    echo: bool,
    max: usize,
    esc: EscState,
    width: Option<usize>,
    prompt: usize,
}

/// The longest line accepted before further printable input is refused, not truncated: a
/// shortened password or path is a wrong value that looks like a right one.
pub const LINE_MAX: usize = 1024;

impl Default for Discipline {
    fn default() -> Self {
        Self::new()
    }
}

/// How many characters `bytes` holds: a cell each, as `libterm` and `fbcon` draw them.
fn chars(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&b| b & 0xC0 != 0x80).count()
}

/// Append `n` backspaces.
fn back(out: &mut Vec<u8>, n: usize) {
    out.resize(out.len() + n, 0x08);
}

/// The longest prefix of `bytes` no longer than `max` that ends on a character boundary.
fn whole_chars(bytes: &[u8], max: usize) -> usize {
    if bytes.len() <= max {
        return bytes.len();
    }
    let mut i = max;
    while i > 0 && bytes[i] & 0xC0 == 0x80 {
        i -= 1;
    }
    i
}

/// The key a CSI's final byte and first parameter name, if it is one this discipline reports.
fn csi_key(fin: u8, param: u32) -> Option<Key> {
    match (fin, param) {
        (b'A', _) => Some(Key::Up),
        (b'B', _) => Some(Key::Down),
        (b'C', _) => Some(Key::Right),
        (b'D', _) => Some(Key::Left),
        (b'H', _) | (b'~', 1 | 7) => Some(Key::Home),
        (b'F', _) | (b'~', 4 | 8) => Some(Key::End),
        (b'~', 3) => Some(Key::Delete),
        (b'~', 5) => Some(Key::PageUp),
        (b'~', 6) => Some(Key::PageDown),
        _ => None,
    }
}

impl Discipline {
    /// An empty line with echo on.
    pub fn new() -> Self {
        Discipline {
            line: Vec::new(),
            cursor: 0,
            echo: true,
            max: LINE_MAX,
            esc: EscState::Ground,
            width: None,
            prompt: 0,
        }
    }

    /// The terminal is `width` columns wide, or cannot say (`None`, or 0); set again whenever it
    /// may have changed, as a refitted terminal has rewrapped what it shows.
    pub fn set_width(&mut self, width: Option<usize>) {
        self.width = width.filter(|&w| w > 0);
    }

    /// The line starts after a prompt `columns` wide.
    pub fn set_prompt(&mut self, columns: usize) {
        self.prompt = columns;
    }

    fn geometry(&self) -> Option<Geometry> {
        self.width.map(|width| Geometry { width, prompt: self.prompt })
    }

    /// Whether typed characters are echoed. Off is how a password is read; it is the server's
    /// state, so a client cannot forget to ask for it.
    pub fn set_echo(&mut self, on: bool) {
        self.echo = on;
    }

    /// Whether typed characters are echoed.
    pub fn echo_on(&self) -> bool {
        self.echo
    }

    /// Discard a partially typed line, so a new reader inherits none of someone else's input.
    pub fn reset(&mut self) {
        self.line.clear();
        self.cursor = 0;
        self.esc = EscState::Ground;
    }

    /// The line as typed so far.
    pub fn line(&self) -> &[u8] {
        &self.line
    }

    /// Where typing lands, as a byte offset into [`line`](Self::line).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether an escape sequence is half-read, so the next byte belongs to it.
    pub fn in_escape(&self) -> bool {
        self.esc != EscState::Ground
    }

    /// Replace the line, the cursor at its end, returning what to write: the old line passed over
    /// and erased, then the new one, cut at a whole character past [`LINE_MAX`], written.
    pub fn replace_line(&mut self, new: &[u8]) -> Vec<u8> {
        self.replace_around(new, b"")
    }

    /// [`replace_line`](Self::replace_line) with `before` then `after`, the cursor between them: a
    /// completion of what is before the cursor that keeps what follows it. One that would not fit
    /// in [`LINE_MAX`] with what follows it is refused, writing nothing, rather than cut.
    pub fn replace_around(&mut self, before: &[u8], after: &[u8]) -> Vec<u8> {
        if !after.is_empty() && before.len() + after.len() > self.max {
            return Vec::new();
        }
        // A sequence half-read when the line is replaced is abandoned with it.
        self.esc = EscState::Ground;
        if let Some(g) = self.geometry() {
            let at = self.cell(g, self.cursor);
            let mut new = [before, after].concat();
            new.truncate(whole_chars(&new, self.max));
            let last_row = new.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
            self.cursor = before.len().min(new.len()).max(last_row);
            self.line = new;
            return if self.echo { self.repaint(g, at, 0, true) } else { Vec::new() };
        }
        let mut out = Vec::new();
        out.extend_from_slice(&self.line[self.cursor..]);
        for _ in 0..chars(&self.line) {
            out.extend_from_slice(b"\x08 \x08");
        }
        let mut new = [before, after].concat();
        new.truncate(whole_chars(&new, self.max));
        let last_row = new.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        self.cursor = before.len().min(new.len()).max(last_row);
        out.extend_from_slice(&new);
        back(&mut out, chars(&new[self.cursor..]));
        self.line = new;
        if self.echo { out } else { Vec::new() }
    }

    /// The line written and the cursor walked back to its place: after a completion's listing,
    /// for one.
    pub fn redraw(&self) -> Vec<u8> {
        if !self.echo {
            return Vec::new();
        }
        if let Some(g) = self.geometry() {
            return self.repaint(g, self.cell(g, 0), 0, false);
        }
        let mut out = self.line.clone();
        back(&mut out, chars(&self.line[self.cursor..]));
        out
    }

    /// Act on an editing key and return what to write; Up and Down are not edits, and do nothing.
    pub fn edit(&mut self, key: Key) -> Vec<u8> {
        if let Some(g) = self.geometry() {
            return self.edit_on(g, key);
        }
        let mut out = Vec::new();
        match key {
            Key::Left if self.cursor > self.row_start() => {
                self.cursor = self.prev(self.cursor);
                out.push(0x08);
            }
            Key::Right if self.cursor < self.line.len() => {
                let next = self.next(self.cursor);
                out.extend_from_slice(&self.line[self.cursor..next]);
                self.cursor = next;
            }
            Key::Home => {
                let start = self.row_start();
                back(&mut out, chars(&self.line[start..self.cursor]));
                self.cursor = start;
            }
            Key::End => {
                out.extend_from_slice(&self.line[self.cursor..]);
                self.cursor = self.line.len();
            }
            Key::Delete if self.cursor < self.line.len() => {
                let next = self.next(self.cursor);
                self.line.drain(self.cursor..next);
                out = self.rewrite_tail(1);
            }
            _ => {}
        }
        if self.echo { out } else { Vec::new() }
    }

    fn edit_on(&mut self, g: Geometry, key: Key) -> Vec<u8> {
        let at = self.cell(g, self.cursor);
        match key {
            Key::Left if self.cursor > self.row_start() => self.cursor = self.prev(self.cursor),
            Key::Right if self.cursor < self.line.len() => self.cursor = self.next(self.cursor),
            Key::Home => self.cursor = self.row_start(),
            Key::End => self.cursor = self.line.len(),
            Key::Delete if self.cursor < self.line.len() => {
                let next = self.next(self.cursor);
                self.line.drain(self.cursor..next);
                return if self.echo { self.repaint(g, at, self.cursor, true) } else { Vec::new() };
            }
            _ => return Vec::new(),
        }
        let mut out = Vec::new();
        if self.echo {
            move_cursor(&mut out, at, self.cell(g, self.cursor));
        }
        out
    }

    /// Where the last row starts: after the last `\n`, which only a recalled statement holds.
    fn row_start(&self) -> usize {
        self.line.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1)
    }

    /// The character boundary before `i`.
    fn prev(&self, i: usize) -> usize {
        let mut j = i - 1;
        while j > 0 && self.line[j] & 0xC0 == 0x80 {
            j -= 1;
        }
        j
    }

    /// The character boundary after `i`.
    fn next(&self, i: usize) -> usize {
        let mut j = i + 1;
        while j < self.line.len() && self.line[j] & 0xC0 == 0x80 {
            j += 1;
        }
        j
    }

    /// The rest of the line rewritten, `gap` spaces over what is gone, and the cursor walked back.
    fn rewrite_tail(&self, gap: usize) -> Vec<u8> {
        let tail = &self.line[self.cursor..];
        let mut out = tail.to_vec();
        out.resize(out.len() + gap, b' ');
        back(&mut out, chars(tail) + gap);
        out
    }

    /// What to write once `n` characters before the cursor are gone: at the end `\x08 \x08` each.
    fn erased_before(&self, n: usize) -> Step {
        if !self.echo || n == 0 {
            return Step::None;
        }
        if self.cursor == self.line.len() {
            return Step::Echo(b"\x08 \x08".repeat(n));
        }
        let mut out = Vec::new();
        back(&mut out, n);
        out.extend(self.rewrite_tail(n));
        Step::Echo(out)
    }

    /// Remove the bytes from `from` to the cursor, which is left at `from`, and say what to write.
    fn remove_before(&mut self, from: usize) -> Step {
        let n = chars(&self.line[from..self.cursor]);
        let at = self.geometry().map(|g| (g, self.cell(g, self.cursor)));
        self.line.drain(from..self.cursor);
        self.cursor = from;
        match at {
            Some((g, at)) if self.echo && n > 0 => Step::Echo(self.repaint(g, at, from, true)),
            Some(_) => Step::None,
            None => self.erased_before(n),
        }
    }

    /// Feed one byte from the device.
    pub fn feed(&mut self, b: u8) -> Step {
        // Mid-escape bytes are never text: a `[` after ESC is not a bracket the user typed.
        match self.esc {
            EscState::Ground => {}
            // `ESC` is the meta prefix, so the byte after a bare one is consumed, as readline
            // does: `ESC d` is M-d, and an unbound pair is discarded rather than inserted.
            EscState::Esc => {
                self.esc = match b {
                    b'[' => EscState::Csi { param: 0, done: false },
                    b'O' => EscState::Ss3,
                    _ => EscState::Ground,
                };
                return Step::None;
            }
            // `ESC O` ends at its final byte, which is never typed. A byte that cannot end it,
            // Enter say, ends the sequence and is itself acted on.
            EscState::Ss3 => {
                self.esc = EscState::Ground;
                match b {
                    0x40..=0x7E => {
                        return csi_key(if matches!(b, b'A'..=b'D' | b'H' | b'F') { b } else { 0 }, 0)
                            .map_or(Step::None, Step::Key);
                    }
                    0x1B => {
                        self.esc = EscState::Esc;
                        return Step::None;
                    }
                    _ => {}
                }
            }
            EscState::Csi { param, done } => {
                // A CSI ends at its *final* byte, not its first: ending `ESC [ 3 ~` (Delete) at
                // the `3` would let the `~` land on the line as printable text and be echoed.
                return match b {
                    // The first parameter names the key (`ESC [ 3 ~`); a `;` ends it.
                    b'0'..=b'9' if !done => {
                        let param = param.saturating_mul(10).saturating_add(u32::from(b - b'0'));
                        self.esc = EscState::Csi { param, done };
                        Step::None
                    }
                    b';' => {
                        self.esc = EscState::Csi { param, done: true };
                        Step::None
                    }
                    // Other parameter and intermediate bytes belong to the sequence.
                    0x20..=0x3F => Step::None,
                    // `ESC` restarts, so an interrupted sequence cannot swallow the next one.
                    0x1B => {
                        self.esc = EscState::Esc;
                        Step::None
                    }
                    // A final byte ends it; one that names no key leaks nothing into the line.
                    0x40..=0x7E => {
                        self.esc = EscState::Ground;
                        csi_key(b, param).map_or(Step::None, Step::Key)
                    }
                    // Malformed: end it rather than swallow the rest of the stream.
                    _ => {
                        self.esc = EscState::Ground;
                        Step::None
                    }
                };
            }
        }
        match b {
            0x1b => {
                self.esc = EscState::Esc;
                Step::None
            }
            // CR and LF both end a line, wherever the cursor is; which one a serial line sends is
            // an accident.
            b'\r' | b'\n' => {
                // Echoed because the user typed it: omitting it leaves the next prompt against
                // their input (`alicepassword:`). In no-echo mode nothing was shown, so the
                // caller supplies its own newline.
                let echo = match self.geometry() {
                    _ if !self.echo => Vec::new(),
                    Some(g) => self.finish(g),
                    None => b"\r\n".to_vec(),
                };
                let bytes = core::mem::take(&mut self.line);
                self.cursor = 0;
                Step::Line { bytes, echo }
            }
            // Ctrl-D is end-of-input at an empty line; mid-line it does nothing, since discarding
            // what someone typed would be worse than ignoring a key.
            0x04 => {
                if self.line.is_empty() {
                    Step::Eof
                } else {
                    Step::None
                }
            }
            // Backspace and DEL both erase the character before the cursor: terminals disagree
            // about which they send.
            0x08 | 0x7f => {
                if self.cursor == self.row_start() {
                    return Step::None;
                }
                let from = self.prev(self.cursor);
                self.remove_before(from)
            }
            // Ctrl-U kills what is before the cursor, erasing exactly what is on screen: nothing in
            // no-echo mode.
            0x15 => self.remove_before(self.row_start()),
            // Ctrl-W erases the word before the cursor and the spaces before it, so repeated
            // presses walk back through a command line.
            0x17 => {
                let start = self.row_start();
                let mut i = self.cursor;
                while i > start && self.line[i - 1] == b' ' {
                    i -= 1;
                }
                while i > start && self.line[i - 1] != b' ' {
                    i -= 1;
                }
                self.remove_before(i)
            }
            // Printable ASCII goes in at the cursor; mid-line, the rest is redrawn after it.
            0x20..=0x7e => {
                if self.line.len() >= self.max {
                    return Step::None; // refuse, do not truncate
                }
                let at = self.geometry().map(|g| (g, self.cell(g, self.cursor)));
                self.line.insert(self.cursor, b);
                self.cursor += 1;
                if !self.echo {
                    return Step::None;
                }
                if let Some((g, at)) = at {
                    return Step::Echo(self.repaint(g, at, self.cursor - 1, false));
                }
                let mut out = alloc::vec![b];
                out.extend(self.rewrite_tail(0));
                Step::Echo(out)
            }
            // Undefined control codes are dropped so a stray byte cannot corrupt a command.
            _ => Step::None,
        }
    }
}

#[cfg(test)]
mod tests;
