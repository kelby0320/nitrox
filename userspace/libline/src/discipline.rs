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

/// One terminal's editing state. Its redraw is Backspace and text, needing no terminal width; that
/// holds for a line shorter than its row, and a full or wrapped one draws wrong (`TODO(tty-size)`).
pub struct Discipline {
    line: Vec<u8>,
    /// Where typing lands: a character boundary, never before the last row, which Backspace cannot
    /// climb out of.
    cursor: usize,
    echo: bool,
    max: usize,
    esc: EscState,
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
        _ => None,
    }
}

impl Discipline {
    /// An empty line with echo on.
    pub fn new() -> Self {
        Discipline { line: Vec::new(), cursor: 0, echo: true, max: LINE_MAX, esc: EscState::Ground }
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
        let mut out = self.line.clone();
        back(&mut out, chars(&self.line[self.cursor..]));
        out
    }

    /// Act on an editing key and return what to write; Up and Down are not edits, and do nothing.
    pub fn edit(&mut self, key: Key) -> Vec<u8> {
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
        self.line.drain(from..self.cursor);
        self.cursor = from;
        self.erased_before(n)
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
                let bytes = core::mem::take(&mut self.line);
                self.cursor = 0;
                // Echoed because the user typed it: omitting it leaves the next prompt against
                // their input (`alicepassword:`). In no-echo mode nothing was shown, so the
                // caller supplies its own newline.
                let echo = if self.echo { b"\r\n".to_vec() } else { Vec::new() };
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
                self.line.insert(self.cursor, b);
                self.cursor += 1;
                if !self.echo {
                    return Step::None;
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
mod tests {
    use super::*;

    /// Feed a string, returning the completed lines and everything echoed.
    fn drive(d: &mut Discipline, input: &str) -> (Vec<alloc::string::String>, alloc::string::String) {
        let mut lines = Vec::new();
        let mut echoed = alloc::string::String::new();
        for b in input.bytes() {
            match d.feed(b) {
                Step::None => {}
                Step::Echo(e) => echoed.push_str(&alloc::string::String::from_utf8_lossy(&e)),
                Step::Line { bytes, echo } => {
                    lines.push(alloc::string::String::from_utf8_lossy(&bytes).into_owned());
                    echoed.push_str(&alloc::string::String::from_utf8_lossy(&echo));
                }
                // `drive` feeds ordinary input; a test that wants EOF asserts on `feed`.
                Step::Eof => panic!("unexpected end-of-input"),
                Step::Key(k) => panic!("unexpected key {k:?}"),
            }
        }
        (lines, echoed)
    }

    #[test]
    fn a_typed_line_is_returned_and_echoed() {
        let mut d = Discipline::new();
        let (lines, echoed) = drive(&mut d, "hello\n");
        assert_eq!(lines, ["hello"]);
        assert_eq!(echoed, "hello\r\n");
    }

    /// Without the echoed newline the next prompt lands against the input: `alicepassword:`.
    #[test]
    fn the_terminating_newline_is_echoed() {
        let mut d = Discipline::new();
        let (_, echoed) = drive(&mut d, "alice\n");
        assert!(echoed.ends_with("\r\n"), "echo was {echoed:?}");
    }

    #[test]
    fn cr_and_lf_both_end_a_line() {
        let mut d = Discipline::new();
        assert_eq!(drive(&mut d, "a\r").0, ["a"]);
        assert_eq!(drive(&mut d, "b\n").0, ["b"]);
    }

    #[test]
    fn no_echo_shows_nothing_at_all() {
        let mut d = Discipline::new();
        d.set_echo(false);
        let (lines, echoed) = drive(&mut d, "hunter2\n");
        assert_eq!(lines, ["hunter2"], "the line still reaches the caller");
        assert_eq!(echoed, "", "and nothing whatever was shown");
    }

    #[test]
    fn backspace_erases_one_character() {
        let mut d = Discipline::new();
        let (lines, echoed) = drive(&mut d, "abc\x08\n");
        assert_eq!(lines, ["ab"]);
        assert!(echoed.contains("\x08 \x08"));
    }

    #[test]
    fn del_erases_like_backspace() {
        let mut d = Discipline::new();
        assert_eq!(drive(&mut d, "abc\x7f\n").0, ["ab"]);
    }

    #[test]
    fn backspace_at_an_empty_line_writes_nothing() {
        let mut d = Discipline::new();
        let (_, echoed) = drive(&mut d, "\x08");
        assert_eq!(echoed, "");
    }

    #[test]
    fn ctrl_u_kills_the_line() {
        let mut d = Discipline::new();
        let (lines, _) = drive(&mut d, "throw away\x15kept\n");
        assert_eq!(lines, ["kept"]);
    }

    #[test]
    fn killing_a_hidden_line_erases_nothing_on_screen() {
        let mut d = Discipline::new();
        d.set_echo(false);
        let (_, echoed) = drive(&mut d, "secret\x15\n");
        assert_eq!(echoed, "");
    }

    #[test]
    fn ctrl_w_erases_a_word_and_the_spaces_before_it() {
        let mut d = Discipline::new();
        assert_eq!(drive(&mut d, "list /bin\x17\n").0, ["list "]);
        assert_eq!(drive(&mut d, "one two   \x17\n").0, ["one "]);
    }

    #[test]
    fn an_over_long_line_is_refused_not_truncated() {
        let mut d = Discipline::new();
        for _ in 0..LINE_MAX {
            d.feed(b'x');
        }
        assert_eq!(d.feed(b'y'), Step::None, "further input is refused");
        match d.feed(b'\n') {
            Step::Line { bytes, .. } => {
                assert_eq!(bytes.len(), LINE_MAX);
                assert!(bytes.iter().all(|&b| b == b'x'), "no `y` sneaked in");
            }
            other => panic!("expected a line, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_d_at_an_empty_line_is_end_of_input() {
        let mut d = Discipline::new();
        assert_eq!(d.feed(0x04), Step::Eof);
    }

    #[test]
    fn ctrl_d_mid_line_does_nothing() {
        let mut d = Discipline::new();
        d.feed(b'a');
        assert_eq!(d.feed(0x04), Step::None);
        match d.feed(b'\n') {
            Step::Line { bytes, .. } => assert_eq!(bytes, b"a"),
            other => panic!("expected the line intact, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_line_is_not_end_of_input() {
        let mut d = Discipline::new();
        match d.feed(b'\n') {
            Step::Line { bytes, .. } => assert!(bytes.is_empty()),
            other => panic!("expected an empty line, got {other:?}"),
        }
    }

    #[test]
    fn cursor_keys_are_reported_not_typed() {
        let mut d = Discipline::new();
        assert_eq!(d.feed(0x1b), Step::None);
        assert_eq!(d.feed(b'['), Step::None);
        assert_eq!(d.feed(b'A'), Step::Key(Key::Up));
        assert_eq!(d.feed(0x1b), Step::None);
        assert_eq!(d.feed(b'['), Step::None);
        assert_eq!(d.feed(b'B'), Step::Key(Key::Down));
        match d.feed(b'\n') {
            Step::Line { bytes, .. } => assert!(bytes.is_empty(), "got {bytes:?}"),
            other => panic!("expected an empty line, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_escape_sequence_leaks_nothing() {
        let mut d = Discipline::new();
        for b in b"ab\x1b[Zcd\n" {
            if let Step::Line { bytes, .. } = d.feed(*b) {
                assert_eq!(bytes, b"abcd");
                return;
            }
        }
        panic!("no line produced");
    }

    /// `ESC x` is how a terminal sends Alt-x; the discipline does not act on it.
    #[test]
    fn a_bare_escape_consumes_exactly_two_bytes() {
        let mut d = Discipline::new();
        d.feed(0x1b);
        match d.feed(b'x') {
            // ESC-then-`x` is a two-byte sequence we do not recognise; `x` ends it.
            Step::None => {}
            other => panic!("expected the sequence to end, got {other:?}"),
        }
        assert!(d.line().is_empty());
        // And the byte after those two is ordinary text again.
        d.feed(b'y');
        assert_eq!(d.line(), b"y");
    }

    #[test]
    fn a_parameterised_sequence_leaves_nothing_behind() {
        // Ending `ESC [ 3 ~` at the `3` would leave the `~` to be typed and echoed.
        for seq in [
            &b"\x1b[3~"[..],   // Delete
            &b"\x1b[2~"[..],   // Insert
            &b"\x1b[5~"[..],   // PageUp
            &b"\x1b[6~"[..],   // PageDown
            &b"\x1b[H"[..],    // Home
            &b"\x1b[F"[..],    // End
            &b"\x1b[1;5D"[..], // a modified cursor key: two parameters and a separator
        ] {
            let mut d = Discipline::new();
            for b in seq {
                d.feed(*b);
            }
            for b in b"hi" {
                d.feed(*b);
            }
            assert_eq!(d.line(), b"hi", "{seq:x?} leaked into the line");
        }
    }

    #[test]
    fn the_arrows_still_arrive_after_the_widening() {
        for (seq, key) in [(&b"\x1b[A"[..], Key::Up), (&b"\x1b[B"[..], Key::Down)] {
            let mut d = Discipline::new();
            let mut got = None;
            for b in seq {
                if let Step::Key(k) = d.feed(*b) {
                    got = Some(k);
                }
            }
            assert_eq!(got, Some(key), "{seq:x?}");
        }
    }

    #[test]
    fn an_escape_inside_a_sequence_starts_a_new_one() {
        // A program interrupted mid-sequence emits a fresh one; its `ESC` is not payload.
        let mut d = Discipline::new();
        let mut got = None;
        for b in b"\x1b[1\x1b[Ahi" {
            if let Step::Key(k) = d.feed(*b) {
                got = Some(k);
            }
        }
        assert_eq!(got, Some(Key::Up), "the second sequence was eaten by the first");
        assert_eq!(d.line(), b"hi");
    }

    #[test]
    fn a_malformed_csi_does_not_swallow_the_stream() {
        let mut d = Discipline::new();
        for b in b"\x1b[1\x7fZhi" {
            d.feed(*b);
        }
        // `0x7f` can appear in no CSI, so the sequence is abandoned there and `Z` is text,
        // as in `libterm::parse`.
        assert_eq!(d.line(), b"Zhi");
    }

    #[test]
    fn replacing_a_line_erases_what_was_shown() {
        let mut d = Discipline::new();
        drive(&mut d, "abc");
        let out = d.replace_line(b"xy");
        assert_eq!(out, b"\x08 \x08\x08 \x08\x08 \x08xy");
        assert_eq!(d.line(), b"xy");
    }

    #[test]
    fn replacing_a_hidden_line_writes_nothing() {
        let mut d = Discipline::new();
        d.set_echo(false);
        drive(&mut d, "secret");
        assert_eq!(d.replace_line(b"x"), b"");
        assert_eq!(d.line(), b"x", "the buffer still changed");
    }

    #[test]
    fn a_replaced_line_can_be_edited_and_submitted() {
        let mut d = Discipline::new();
        d.replace_line(b"list /");
        d.feed(0x08); // backspace
        match d.feed(b'\n') {
            Step::Line { bytes, .. } => assert_eq!(bytes, b"list "),
            other => panic!("expected a line, got {other:?}"),
        }
    }

    #[test]
    fn undefined_control_bytes_are_dropped() {
        let mut d = Discipline::new();
        assert_eq!(drive(&mut d, "a\x01\x02b\n").0, ["ab"]);
    }

    #[test]
    fn reset_discards_a_partial_line() {
        let mut d = Discipline::new();
        drive(&mut d, "partial");
        d.reset();
        assert_eq!(drive(&mut d, "fresh\n").0, ["fresh"]);
    }

    /// Feed `bytes` as `nxsh` does, acting on each reported key, and return what was written.
    fn edited(d: &mut Discipline, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for &b in bytes {
            match d.feed(b) {
                Step::Echo(e) => out.extend(e),
                Step::Key(k) => out.extend(d.edit(k)),
                _ => {}
            }
        }
        out
    }

    const LEFT: &[u8] = b"\x1b[D";

    #[test]
    fn left_and_right_move_by_a_character_and_stop_at_the_ends() {
        let mut d = Discipline::new();
        edited(&mut d, b"ab");
        assert_eq!(edited(&mut d, LEFT), b"\x08");
        assert_eq!(d.cursor(), 1);
        assert_eq!(edited(&mut d, b"\x1b[D\x1b[D"), b"\x08", "the second Left at the start does nothing");
        assert_eq!(edited(&mut d, b"\x1b[C"), b"a", "Right passes the character by writing it");
        assert_eq!(edited(&mut d, b"\x1b[C\x1b[C"), b"b");
        assert_eq!((d.line(), d.cursor()), (&b"ab"[..], 2));
    }

    #[test]
    fn home_and_end_walk_the_whole_line() {
        let mut d = Discipline::new();
        edited(&mut d, b"abc");
        assert_eq!(edited(&mut d, b"\x1b[H"), b"\x08\x08\x08");
        assert_eq!(d.cursor(), 0);
        assert_eq!(edited(&mut d, b"\x1b[F"), b"abc");
        assert_eq!(d.cursor(), 3);
    }

    #[test]
    fn typing_mid_line_inserts_and_redraws_the_rest() {
        let mut d = Discipline::new();
        edited(&mut d, b"ac");
        edited(&mut d, LEFT);
        assert_eq!(edited(&mut d, b"b"), b"bc\x08");
        assert_eq!((d.line(), d.cursor()), (&b"abc"[..], 2));
        edited(&mut d, b"\x1b[H");
        assert_eq!(edited(&mut d, b"_"), b"_abc\x08\x08\x08", "at the start");
        assert_eq!(d.line(), b"_abc");
    }

    #[test]
    fn backspace_mid_line_takes_the_character_before_the_cursor() {
        let mut d = Discipline::new();
        edited(&mut d, b"abcd");
        edited(&mut d, b"\x1b[D\x1b[D");
        assert_eq!(edited(&mut d, b"\x7f"), b"\x08cd \x08\x08\x08");
        assert_eq!((d.line(), d.cursor()), (&b"acd"[..], 1));
        edited(&mut d, b"\x1b[H");
        assert_eq!(edited(&mut d, b"\x7f"), b"", "nothing before the start");
    }

    #[test]
    fn delete_takes_the_character_under_the_cursor() {
        let mut d = Discipline::new();
        edited(&mut d, b"abc");
        edited(&mut d, b"\x1b[H");
        assert_eq!(edited(&mut d, b"\x1b[3~"), b"bc \x08\x08\x08");
        assert_eq!((d.line(), d.cursor()), (&b"bc"[..], 0));
        edited(&mut d, b"\x1b[F");
        assert_eq!(edited(&mut d, b"\x1b[3~"), b"", "nothing under the cursor at the end");
        assert_eq!(d.line(), b"bc");
    }

    #[test]
    fn ctrl_u_and_ctrl_w_take_only_what_is_before_the_cursor() {
        let mut d = Discipline::new();
        edited(&mut d, b"one two");
        edited(&mut d, b"\x1b[D\x1b[D");
        // The cursor is before `w`, so the word's part before it, `t`, goes.
        assert_eq!(edited(&mut d, b"\x17"), b"\x08wo \x08\x08\x08");
        assert_eq!((d.line(), d.cursor()), (&b"one wo"[..], 4));
        assert_eq!(edited(&mut d, b"\x15"), b"\x08\x08\x08\x08wo    \x08\x08\x08\x08\x08\x08");
        assert_eq!((d.line(), d.cursor()), (&b"wo"[..], 0));
    }

    #[test]
    fn enter_mid_line_submits_it_all_and_the_next_line_starts_empty() {
        let mut d = Discipline::new();
        edited(&mut d, b"abc");
        edited(&mut d, LEFT);
        match d.feed(b'\r') {
            Step::Line { bytes, .. } => assert_eq!(bytes, b"abc"),
            other => panic!("expected a line, got {other:?}"),
        }
        // A cursor left at 2 would insert past the end of the empty line and panic.
        edited(&mut d, b"x");
        assert_eq!((d.line(), d.cursor()), (&b"x"[..], 1));
    }

    #[test]
    fn a_reset_mid_line_leaves_the_cursor_at_the_start() {
        let mut d = Discipline::new();
        edited(&mut d, b"abc");
        edited(&mut d, LEFT);
        d.reset();
        edited(&mut d, b"x");
        assert_eq!((d.line(), d.cursor()), (&b"x"[..], 1));
    }

    #[test]
    fn a_replaced_line_leaves_the_cursor_at_its_end_and_erases_from_there() {
        let mut d = Discipline::new();
        edited(&mut d, b"abc");
        edited(&mut d, b"\x1b[H");
        // The rest is passed over first, so the erase starts from the end of what is shown.
        assert_eq!(d.replace_line(b"xy"), b"abc\x08 \x08\x08 \x08\x08 \x08xy");
        assert_eq!(d.cursor(), 2);
    }

    #[test]
    fn replace_around_puts_the_cursor_between_its_halves() {
        let mut d = Discipline::new();
        assert_eq!(d.replace_around(b"cd /ho", b" x"), b"cd /ho x\x08\x08");
        assert_eq!((d.line(), d.cursor()), (&b"cd /ho x"[..], 6));
        assert_eq!(d.redraw(), b"cd /ho x\x08\x08");
    }

    #[test]
    fn a_recalled_line_too_long_is_cut_at_a_whole_character() {
        let mut d = Discipline::new();
        let mut recalled = alloc::vec![b'a'; LINE_MAX - 1];
        recalled.extend_from_slice("\u{e9}".as_bytes());
        let shown = d.replace_line(&recalled);
        assert_eq!(d.line().len(), LINE_MAX - 1, "the half of `\u{e9}` that fit is dropped");
        assert!(core::str::from_utf8(d.line()).is_ok());
        assert_eq!(shown, d.line(), "only what is kept is written");
    }

    #[test]
    fn the_cursor_moves_over_a_character_not_a_byte() {
        let mut d = Discipline::new();
        d.replace_line("h\u{e9}llo".as_bytes());
        edited(&mut d, b"\x1b[H\x1b[C");
        assert_eq!(d.cursor(), 1);
        assert_eq!(edited(&mut d, b"\x1b[C"), "\u{e9}".as_bytes());
        edited(&mut d, LEFT);
        assert_eq!(edited(&mut d, b"\x1b[3~"), b"llo \x08\x08\x08\x08", "one character, two bytes, gone");
        assert_eq!(d.line(), b"hllo");
    }

    #[test]
    fn a_recalled_statement_of_several_lines_is_edited_on_its_last() {
        let mut d = Discipline::new();
        d.replace_line(b"if x {\n1 }");
        assert_eq!(edited(&mut d, b"\x1b[H"), b"\x08\x08\x08");
        assert_eq!(d.cursor(), 7);
        assert_eq!(edited(&mut d, b"\x1b[D\x7f"), b"", "no Left or Backspace past the row's start");
        assert_eq!(d.line(), b"if x {\n1 }");
    }

    #[test]
    fn every_form_of_each_key_is_reported() {
        for (seq, key) in [
            (&b"\x1b[D"[..], Key::Left),
            (b"\x1b[C", Key::Right),
            (b"\x1b[H", Key::Home),
            (b"\x1b[1~", Key::Home),
            (b"\x1b[7~", Key::Home),
            (b"\x1bOH", Key::Home),
            (b"\x1b[F", Key::End),
            (b"\x1b[4~", Key::End),
            (b"\x1b[8~", Key::End),
            (b"\x1bOF", Key::End),
            (b"\x1b[3~", Key::Delete),
            (b"\x1b[1;5D", Key::Left),
            (b"\x1bOA", Key::Up),
        ] {
            let mut d = Discipline::new();
            let got: Vec<Step> = seq.iter().map(|&b| d.feed(b)).filter(|s| *s != Step::None).collect();
            assert_eq!(got, [Step::Key(key)], "{seq:x?}");
        }
        // Insert names no key, and leaves nothing.
        let mut d = Discipline::new();
        assert_eq!(edited(&mut d, b"\x1b[2~"), b"");
    }

    #[test]
    fn an_ss3_sequence_ends_at_its_final_byte_rather_than_typing_it() {
        let mut d = Discipline::new();
        edited(&mut d, b"\x1bOxy");
        assert_eq!(d.line(), b"y");
    }

    #[test]
    fn with_echo_off_an_edit_still_moves_the_cursor_but_shows_nothing() {
        let mut d = Discipline::new();
        d.set_echo(false);
        edited(&mut d, b"ab");
        assert_eq!(edited(&mut d, b"\x1b[DX"), b"");
        assert_eq!(d.line(), b"aXb");
    }

    #[test]
    fn ctrl_d_ends_input_only_on_an_empty_line_wherever_the_cursor_is() {
        let mut d = Discipline::new();
        edited(&mut d, b"ab\x1b[H");
        assert_eq!(d.feed(0x04), Step::None, "the cursor at the start of a line is not an empty line");
        assert_eq!(d.line(), b"ab");
    }

    #[test]
    fn ctrl_w_takes_the_word_before_the_cursor_not_the_last() {
        let mut d = Discipline::new();
        edited(&mut d, b"one two three\x1b[D\x1b[D\x1b[D\x1b[D\x1b[D\x1b[D");
        edited(&mut d, b"\x17");
        assert_eq!((d.line(), d.cursor()), (&b"one  three"[..], 4));
    }

    #[test]
    fn ctrl_u_on_a_recalled_statement_takes_only_its_last_row() {
        let mut d = Discipline::new();
        d.replace_line(b"if x {\n1 }");
        edited(&mut d, b"\x15");
        assert_eq!(d.line(), b"if x {\n");
    }

    #[test]
    fn a_reset_abandons_a_half_read_sequence() {
        let mut d = Discipline::new();
        d.feed(0x1b);
        d.reset();
        edited(&mut d, b"x");
        assert_eq!(d.line(), b"x");
    }

    #[test]
    fn a_modifier_after_the_first_parameter_is_read_past() {
        let mut d = Discipline::new();
        let got: Vec<Step> = b"\x1b[3;5~".iter().map(|&b| d.feed(b)).filter(|s| *s != Step::None).collect();
        assert_eq!(got, [Step::Key(Key::Delete)], "Ctrl-Delete is Delete");
    }

    #[test]
    fn with_echo_off_a_redraw_shows_nothing() {
        let mut d = Discipline::new();
        d.set_echo(false);
        edited(&mut d, b"secret");
        assert_eq!(d.redraw(), b"");
    }

    #[test]
    fn replace_around_never_leaves_the_cursor_before_the_last_row() {
        let mut d = Discipline::new();
        d.replace_around(b"ab", b"c\nd");
        assert_eq!(d.cursor(), 4);
        assert_eq!(edited(&mut d, b"\x1b[H"), b"", "Home stays on the last row");
    }

    #[test]
    fn a_completion_that_would_pass_line_max_mid_line_is_refused_not_cut() {
        let mut d = Discipline::new();
        let line = alloc::vec![b'a'; LINE_MAX];
        d.replace_line(&line);
        assert_eq!(d.replace_around(&line[..4], &line[..LINE_MAX]), b"");
        assert_eq!((d.line().len(), d.cursor()), (LINE_MAX, LINE_MAX), "the line is as it was");
    }

    #[test]
    fn replacing_the_line_abandons_a_half_read_sequence() {
        let mut d = Discipline::new();
        d.feed(0x1b);
        d.replace_line(b"date");
        assert!(matches!(d.feed(b'\n'), Step::Line { .. }), "the Enter is not taken by the `ESC`");
    }

    #[test]
    fn a_byte_that_cannot_end_an_ss3_sequence_is_acted_on() {
        let mut d = Discipline::new();
        edited(&mut d, b"ab\x1bO");
        assert!(matches!(d.feed(b'\r'), Step::Line { bytes, .. } if bytes == b"ab"), "Enter still ends the line");
    }

    #[test]
    fn an_escape_after_ss3_starts_a_new_sequence() {
        let mut d = Discipline::new();
        let got: Vec<Step> = b"\x1bO\x1b[D".iter().map(|&b| d.feed(b)).filter(|s| *s != Step::None).collect();
        assert_eq!(got, [Step::Key(Key::Left)]);
    }
}
