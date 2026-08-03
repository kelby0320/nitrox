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
    /// End of input — Ctrl-D at an empty line. Distinct from an empty *line*, which is
    /// what Enter on its own produces: one means "nothing this time", the other means
    /// "nothing ever again", and a reader that conflates them either exits on a stray
    /// Enter or cannot be exited at all.
    Eof,
}

/// One terminal's editing state.
pub struct Discipline {
    line: Vec<u8>,
    echo: bool,
    max: usize,
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
        Discipline { line: Vec::new(), echo: true, max: LINE_MAX }
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
    }

    /// Feed one byte from the device.
    pub fn feed(&mut self, b: u8) -> Step {
        match b {
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
