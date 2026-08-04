//! `tty-server` — the line discipline, separated from the syscalls.
//!
//! Everything with behaviour lives here and host-tests in milliseconds: what a byte does to
//! a line buffer, when a line is complete, what gets echoed. The binary half owns the
//! console device, the IPC channels, and the wait loop.
//!
//! **The seam is the reason this crate is split.** Line editing existed three times before
//! this server — in `eshell`, in `session-mgr`'s login, and in `nxsh`'s REPL — and the three
//! copies disagreed: two of them differed over whether to echo the CR/LF that ends a line,
//! which is why a password prompt rendered as `alicepassword:` for weeks. One implementation
//! behind a testable seam is the whole point of the exercise, so the syscalls stay out.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::vec::Vec;

/// What the caller should do after feeding one byte.
///
/// The discipline never writes anything itself — it *says* what to write. That is what keeps
/// it free of syscalls, and it is also what lets a test assert on the echo.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Step {
    /// Nothing to do.
    None,
    /// Write these bytes to the terminal (an echo, or an erase sequence).
    Echo(Vec<u8>),
    /// A line is complete. Its bytes (no terminator) are the payload; `echo` is what to
    /// write to move the cursor on, which is empty in no-echo mode.
    Line { bytes: Vec<u8>, echo: Vec<u8> },
    /// A key this discipline deliberately does not act on, reported for the caller to
    /// decide. Arrow keys are the case that matters: recalling history means replacing the
    /// line, and *what* to replace it with is the shell's business, not the terminal's.
    Key(Key),
    /// End of input — Ctrl-D at an empty line. Distinct from an empty *line*, which is
    /// what Enter on its own produces: one means "nothing this time", the other means
    /// "nothing ever again", and a reader that conflates them either exits on a stray
    /// Enter or cannot be exited at all.
    Eof,
}

/// A key with no meaning to the line discipline itself.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Key {
    Up,
    Down,
}

/// Where the escape-sequence parser is.
///
/// Arrow keys arrive as three bytes (`ESC [ A`), so recognising them needs state. Kept
/// deliberately small: this parses *cursor keys and nothing else*, and unknown sequences are
/// discarded rather than accumulated. It is the first step toward terminal input parsing,
/// and the stopping point is chosen rather than discovered — anything richer belongs with a
/// real keyboard driver, which can report modifiers a serial byte stream cannot express.
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

/// The longest line accepted before further printable input is refused.
///
/// Refusing beats truncating: a silently shortened password or path is a wrong value that
/// looks like a right one.
pub const LINE_MAX: usize = 1024;

impl Default for Discipline {
    fn default() -> Self {
        Self::new()
    }
}

impl Discipline {
    pub fn new() -> Self {
        Discipline { line: Vec::new(), echo: true, max: LINE_MAX, esc: EscState::Ground }
    }

    /// Whether typed characters are echoed. Off is how a password is read — and because it
    /// is *the server's* state, a client cannot forget to ask for it the way every caller of
    /// the old `read_line(..., echo: false)` had to remember.
    pub fn set_echo(&mut self, on: bool) {
        self.echo = on;
    }

    pub fn echo_on(&self) -> bool {
        self.echo
    }

    /// Discard a partially typed line — what a tty being handed to a new reader needs, so no
    /// one inherits half of somebody else's input.
    pub fn reset(&mut self) {
        self.line.clear();
        self.esc = EscState::Ground;
    }

    /// The line as typed so far.
    pub fn line(&self) -> &[u8] {
        &self.line
    }

    /// Replace the whole line, returning what to write so the display matches.
    ///
    /// This is the redraw primitive history recall needs, and the reason the line buffer
    /// stays here rather than being reimplemented by every caller that wants a history: the
    /// discipline knows what is on screen, so only it can say how to erase it.
    ///
    /// Erase-then-write, not a cursor-addressing sequence: a dumb terminal is the baseline,
    /// and `\x08 \x08` per character is what works everywhere.
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
            EscState::Esc => {
                self.esc = if b == b'[' { EscState::Csi } else { EscState::Ground };
                return Step::None;
            }
            EscState::Csi => {
                self.esc = EscState::Ground;
                return match b {
                    b'A' => Step::Key(Key::Up),
                    b'B' => Step::Key(Key::Down),
                    // Left/right and everything else: recognised as *ended*, so the
                    // sequence cannot leak into the line, but not acted on.
                    _ => Step::None,
                };
            }
        }
        match b {
            // ESC begins a sequence. Alone it does nothing, which is the right answer for a
            // key with no meaning here.
            0x1b => {
                self.esc = EscState::Esc;
                Step::None
            }
            // CR and LF both end a line. A serial line may send either, and which one is
            // an accident of the terminal rather than a distinction worth carrying.
            b'\r' | b'\n' => {
                let bytes = core::mem::take(&mut self.line);
                // The newline is echoed *because the user typed it*. Omitting it leaves the
                // cursor on the line they typed on, so the next prompt lands against their
                // input — exactly the `alicepassword:` bug. In no-echo mode nothing was
                // shown, so the caller supplies its own newline when it is ready.
                let echo = if self.echo { b"\r\n".to_vec() } else { Vec::new() };
                Step::Line { bytes, echo }
            }
            // Ctrl-D. At an empty line it is end-of-input, the universal convention. With
            // a partial line it does nothing: bash uses it to list completions there, and
            // silently discarding what someone typed would be worse than ignoring a key.
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
                    // Back up, overwrite with a space, back up again — the only way to
                    // erase on a dumb terminal.
                    Step::Echo(b"\x08 \x08".to_vec())
                } else {
                    // Nothing to erase, or nothing to show. Not an error: backspace at an
                    // empty prompt is something people do constantly.
                    Step::None
                }
            }
            // Ctrl-U: kill the line. Erase exactly what is on screen, no more — in no-echo
            // mode that is nothing.
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
            // Ctrl-W: erase the word before the cursor, plus the run of spaces before it,
            // which is what makes repeated presses walk back through a command line.
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
            // Everything else — control codes this discipline does not define — is dropped
            // rather than accumulated, so a stray byte cannot corrupt a command.
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

    /// The bug this server exists to make impossible: the newline is echoed **because the
    /// user typed it**. Without it the cursor never leaves the line they typed on, so the
    /// next prompt lands against their input — `alicepassword:`.
    #[test]
    fn the_terminating_newline_is_echoed() {
        let mut d = Discipline::new();
        let (_, echoed) = drive(&mut d, "alice\n");
        assert!(echoed.ends_with("\r\n"), "echo was {echoed:?}");
    }

    /// CR and LF are the same key. Which one arrives is an accident of the terminal.
    #[test]
    fn cr_and_lf_both_end_a_line() {
        let mut d = Discipline::new();
        assert_eq!(drive(&mut d, "a\r").0, ["a"]);
        assert_eq!(drive(&mut d, "b\n").0, ["b"]);
    }

    /// No-echo is the server's state, so a password cannot leak through a caller that forgot
    /// to ask for it — which is what `read_line(..., echo: false)` depended on.
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

    /// DEL and backspace are the same key by another name.
    #[test]
    fn del_erases_like_backspace() {
        let mut d = Discipline::new();
        assert_eq!(drive(&mut d, "abc\x7f\n").0, ["ab"]);
    }

    /// Backspace at an empty prompt is something people do constantly; it must not erase
    /// the prompt itself.
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

    /// Erasing must not show more than was displayed: in no-echo mode, nothing.
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

    /// Refusing beats truncating: a silently shortened password is a wrong value that looks
    /// like a right one.
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

    /// A stray control byte must not become part of a command.
    /// Ctrl-D at an empty prompt is end-of-input — the convention the shell's own banner
    /// promises ("Ctrl-D or `exit` to leave").
    #[test]
    fn ctrl_d_at_an_empty_line_is_end_of_input() {
        let mut d = Discipline::new();
        assert_eq!(d.feed(0x04), Step::Eof);
    }

    /// ...but not mid-line, where discarding what someone typed would be worse than
    /// ignoring a key.
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

    /// End of input and an empty line are different answers: one means "nothing this
    /// time", the other "nothing ever again".
    #[test]
    fn an_empty_line_is_not_end_of_input() {
        let mut d = Discipline::new();
        match d.feed(b'\n') {
            Step::Line { bytes, .. } => assert!(bytes.is_empty()),
            other => panic!("expected an empty line, got {other:?}"),
        }
    }

    /// Arrow keys are three bytes and must not appear in the line.
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

    /// An unrecognised sequence is consumed to its end rather than leaking its bytes into
    /// the line — the failure that would make a stray arrow key corrupt a command.
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

    /// A bare ESC is a key with no meaning here, and must not swallow what follows.
    #[test]
    fn a_bare_escape_does_not_eat_the_next_character() {
        let mut d = Discipline::new();
        d.feed(0x1b);
        match d.feed(b'x') {
            // ESC-then-`x` is a two-byte sequence we do not recognise; `x` ends it.
            Step::None => {}
            other => panic!("expected the sequence to end, got {other:?}"),
        }
        assert!(d.line().is_empty());
    }

    /// The redraw primitive: erase exactly what is displayed, then draw the new line.
    #[test]
    fn replacing_a_line_erases_what_was_shown() {
        let mut d = Discipline::new();
        drive(&mut d, "abc");
        let out = d.replace_line(b"xy");
        assert_eq!(out, b"\x08 \x08\x08 \x08\x08 \x08xy");
        assert_eq!(d.line(), b"xy");
    }

    /// With nothing echoed there is nothing to erase, and nothing to draw.
    #[test]
    fn replacing_a_hidden_line_writes_nothing() {
        let mut d = Discipline::new();
        d.set_echo(false);
        drive(&mut d, "secret");
        assert_eq!(d.replace_line(b"x"), b"");
        assert_eq!(d.line(), b"x", "the buffer still changed");
    }

    /// A recalled line then behaves as if typed.
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

    /// A tty handed to a new reader must not carry half of somebody else's input.
    #[test]
    fn reset_discards_a_partial_line() {
        let mut d = Discipline::new();
        drive(&mut d, "partial");
        d.reset();
        assert_eq!(drive(&mut d, "fresh\n").0, ["fresh"]);
    }
}
