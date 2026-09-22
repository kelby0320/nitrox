//! What the shell colours, and what colour says what.
//!
//! **The shell emits SGR; the terminal decides what it looks like** (desktop refresh, Part F).
//! Nothing here is a pixel. Each constant is one of the sixteen ANSI colours, which
//! `libterm::cell` stores symbolically and `Palette::default` turns into pixels — so a scrollback
//! keeps its meaning when the palette is retuned, and re-theming recolours what is already on
//! screen rather than only what is printed next.
//!
//! **Why the shell and not each program.** A Unix terminal has the same mechanism and the
//! *program* emits it, which is why `ls --color` and `grep --color` each carry a flag and an
//! `isatty` check. Here a program's output is a typed stream that the shell renders, so colouring
//! the shell's renderer once colours every program's output — and the shell knows which cell is a
//! header because it built the table. There is no per-program flag to forget.
//!
//! **What is structural, and nothing else.** The design colours four things: the banner, the
//! prompt, a table's header row, and a diagnostic. Values are not coloured — a string that came
//! back from a program is data, and tinting data by guessing its meaning is the thing this
//! approach makes unnecessary.

use alloc::string::String;

/// Bright cyan — the prompt. The design's `#79C6D6`.
pub const PROMPT: &str = "\x1b[96m";

/// Cyan — the banner. The design's `#6FB7AE`.
pub const BANNER: &str = "\x1b[36m";

/// Bright black — a table's header row, and the shell's notes about itself. The design's
/// `#8FA5A3`, which it uses for both.
pub const HEADER: &str = "\x1b[90m";

/// Bright red — a diagnostic. The design's `#D68A83`.
pub const DIAG: &str = "\x1b[91m";

/// Back to whatever the terminal's default is — **not** to a colour of our choosing, which is
/// what `SGR 39` would be reaching for if the shell had opinions about the ground.
pub const RESET: &str = "\x1b[0m";

/// `text` set in `sgr`, or `text` unchanged when `on` is false.
///
/// **Trailing spaces go inside the paint; a trailing newline does not.** Two different reasons,
/// and both bite.
///
/// A space has no ink, so where the reset falls makes no visual difference — but it decides
/// whether what a reader sees as one string is one string in the byte stream. The prompt is the
/// case that matters: `\x1b[96m/home> \x1b[0m` keeps `/home> ` contiguous, where resetting
/// before the space would split it. Anything reading this stream as text — a gate, a log, a
/// person with `less` — sees the line it expects.
///
/// A **newline** is the opposite: `tty_write_crlf` splits on `\n` and emits a `\r\n` after
/// every chunk, so a reset after the newline is a chunk of its own — a blank line, with
/// `\x1b[0m` at the head of the next one. The caller keeps the newline outside.
///
/// **`on` is false for a shell with no terminal**: a script, or a Tier-0 stage. See
/// [`Host::styled`](crate::host::Host::styled).
pub fn paint(on: bool, sgr: &str, text: &str) -> String {
    if !on {
        return String::from(text);
    }
    let mut s = String::with_capacity(text.len() + sgr.len() + RESET.len());
    s.push_str(sgr);
    s.push_str(text);
    s.push_str(RESET);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_emitted_for_a_shell_with_no_terminal() {
        // **The whole of "only when it has a terminal".** A script's output is bytes somebody
        // will read as text or feed to something else; an escape in it is corruption, not colour.
        assert_eq!(paint(false, PROMPT, "/home> "), "/home> ");
        assert_eq!(paint(false, DIAG, "nxsh: no such thing\n"), "nxsh: no such thing\n");
    }

    #[test]
    fn a_painted_run_keeps_its_text_contiguous() {
        // The reason a trailing *space* goes inside: a reader looking for `/home> ` must find
        // it, and a reset before the space would put four bytes in the middle of it.
        let p = paint(true, PROMPT, "/home> ");
        assert!(p.contains("/home> "), "{p:?} split the prompt");
        assert!(p.starts_with(PROMPT) && p.ends_with(RESET), "{p:?}");
        // And every colour this module names is one of the sixteen — a code `libterm` parses
        // into a `Colour::Ansi`, not a 24-bit one it would have to store.
        //
        // **The newline rule is a caller's to keep, so it is not asserted here**: `paint` wraps
        // whatever it is given, and a test feeding it a string with no newline would only be
        // checking its own input. Where it is checked is `ops::display`'s
        // `a_terminal_gets_a_coloured_header_and_plain_values`, which strips the escapes back
        // out and compares the result against the uncoloured render — so a reset on the wrong
        // side of a `\n` shows up as a layout change.
        for sgr in [PROMPT, BANNER, HEADER, DIAG] {
            let code: u16 = sgr.trim_start_matches("\x1b[").trim_end_matches('m').parse().unwrap();
            assert!(
                (30..=37).contains(&code) || (90..=97).contains(&code),
                "{sgr:?} is not one of the sixteen foreground codes"
            );
        }
    }
}
