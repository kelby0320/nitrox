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
    /// A key this discipline does not act on, reported for the caller to decide: what to
    /// replace the line with on recall is the shell's business, not the terminal's.
    Key(Key),
    /// End of input — Ctrl-D at an empty line. Distinct from an empty *line* (a bare Enter):
    /// a reader that conflates them either exits on a stray Enter or cannot be exited.
    Eof,
}

/// A key with no meaning to the line discipline itself.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Key {
    /// Cursor up (`ESC [ A`).
    Up,
    /// Cursor down (`ESC [ B`).
    Down,
}

/// Where the escape-sequence parser is: cursor keys and nothing else, with unknown sequences
/// discarded, not accumulated.
#[derive(Copy, Clone, PartialEq, Eq)]
enum EscState {
    Ground,
    Esc,
    Csi,
}

/// One terminal's editing state.
pub struct Discipline {
    line: Vec<u8>,
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

impl Discipline {
    /// An empty line with echo on.
    pub fn new() -> Self {
        Discipline { line: Vec::new(), echo: true, max: LINE_MAX, esc: EscState::Ground }
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
        self.esc = EscState::Ground;
    }

    /// The line as typed so far.
    pub fn line(&self) -> &[u8] {
        &self.line
    }

    /// Replace the whole line, returning what to write so the display matches: only the
    /// discipline knows what is on screen. Erase-then-write (`\x08 \x08` per character), as a
    /// dumb terminal needs.
    pub fn replace_line(&mut self, new: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        if self.echo {
            for _ in 0..self.line.len() {
                out.extend_from_slice(b"\x08 \x08");
            }
            out.extend_from_slice(new);
        }
        self.line.clear();
        self.line.extend_from_slice(&new[..new.len().min(self.max)]);
        out
    }

    /// Feed one byte from the device.
    pub fn feed(&mut self, b: u8) -> Step {
        // Mid-escape bytes are never text: a `[` after ESC is not a bracket the user typed.
        match self.esc {
            EscState::Ground => {}
            // `ESC` is the meta prefix, so the byte after a bare one is consumed, as readline
            // does: `ESC d` is M-d, and an unbound pair is discarded rather than inserted.
            EscState::Esc => {
                self.esc = if b == b'[' { EscState::Csi } else { EscState::Ground };
                return Step::None;
            }
            EscState::Csi => {
                // A CSI ends at its *final* byte, not its first: ending `ESC [ 3 ~` (Delete) at
                // the `3` would let the `~` land on the line as printable text and be echoed.
                return match b {
                    // Parameter and intermediate bytes belong to the sequence.
                    0x20..=0x3F => Step::None,
                    // `ESC` restarts, so an interrupted sequence cannot swallow the next one.
                    0x1B => {
                        self.esc = EscState::Esc;
                        Step::None
                    }
                    // A final byte ends it.
                    0x40..=0x7E => {
                        self.esc = EscState::Ground;
                        match b {
                            b'A' => Step::Key(Key::Up),
                            b'B' => Step::Key(Key::Down),
                            // Ended but not acted on, so it cannot leak into the line.
                            _ => Step::None,
                        }
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
            // CR and LF both end a line; which one a serial line sends is an accident.
            b'\r' | b'\n' => {
                let bytes = core::mem::take(&mut self.line);
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
            // Backspace and DEL both erase: terminals disagree about which they send.
            0x08 | 0x7f => {
                if self.line.pop().is_some() && self.echo {
                    // Back up, overwrite with a space, back up again.
                    Step::Echo(b"\x08 \x08".to_vec())
                } else {
                    // Nothing to erase, or nothing to show.
                    Step::None
                }
            }
            // Ctrl-U kills the line, erasing exactly what is on screen: nothing in no-echo mode.
            0x15 => {
                let n = self.line.len();
                self.line.clear();
                if self.echo && n > 0 {
                    let mut out = Vec::with_capacity(n * 3);
                    for _ in 0..n {
                        out.extend_from_slice(b"\x08 \x08");
                    }
                    Step::Echo(out)
                } else {
                    Step::None
                }
            }
            // Ctrl-W erases the word before the cursor and the spaces before it, so repeated
            // presses walk back through a command line.
            0x17 => {
                let before = self.line.len();
                while self.line.last() == Some(&b' ') {
                    self.line.pop();
                }
                while let Some(&c) = self.line.last() {
                    if c == b' ' {
                        break;
                    }
                    self.line.pop();
                }
                let n = before - self.line.len();
                if self.echo && n > 0 {
                    let mut out = Vec::with_capacity(n * 3);
                    for _ in 0..n {
                        out.extend_from_slice(b"\x08 \x08");
                    }
                    Step::Echo(out)
                } else {
                    Step::None
                }
            }
            // Printable ASCII accumulates.
            0x20..=0x7e => {
                if self.line.len() >= self.max {
                    return Step::None; // refuse, do not truncate
                }
                self.line.push(b);
                if self.echo { Step::Echo(alloc::vec![b]) } else { Step::None }
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
}
