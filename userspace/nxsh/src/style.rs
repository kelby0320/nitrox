//! What the shell colours, and what colour says what.
//!
//! The shell emits SGR; the terminal decides what it looks like. Each constant is one of the
//! sixteen ANSI colours, which `libterm::cell` stores symbolically, so re-theming recolours what is
//! already on screen. Only structure is coloured (banner, prompt, a table's header, a diagnostic),
//! never a value.

use alloc::string::String;

/// Bright cyan — the prompt. The design's `#79C6D6`.
pub const PROMPT: &str = "\x1b[96m";

/// Cyan — the banner. The design's `#6FB7AE`.
pub const BANNER: &str = "\x1b[36m";

/// Bright black — a table's header row, and the shell's notes about itself (`#8FA5A3`).
pub const HEADER: &str = "\x1b[90m";

/// Bright red — a diagnostic. The design's `#D68A83`.
pub const DIAG: &str = "\x1b[91m";
/// Bright yellow, a warning: the design has no warning colour, so this is the ANSI one left.
pub const WARN: &str = "\x1b[93m";

/// The colour a diagnostic of `level` is drawn in; a notice is a program's progress or answer, so
/// it gets none.
pub fn for_level(level: libstream::diag::Level) -> Option<&'static str> {
    use libstream::diag::Level;
    match level {
        Level::Error => Some(DIAG),
        Level::Warning => Some(WARN),
        Level::Notice => None,
    }
}

/// Back to the terminal's default, not to a colour of our choosing.
pub const RESET: &str = "\x1b[0m";

/// `text` set in `sgr`, or `text` unchanged when `on` is false.
///
/// A trailing space goes inside the paint so the prompt stays contiguous in the byte stream
/// (`\x1b[96m/home> \x1b[0m`), where a gate or log reads it. A trailing newline stays outside:
/// `tty_write_crlf` splits on `\n`, so a reset after it would start the next line.
///
/// `on` is false for a shell with no terminal: a script, or a Tier-0 stage. See
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
mod level_tests {
    use super::*;
    use libstream::diag::Level;

    #[test]
    fn each_level_has_its_colour() {
        assert_eq!(for_level(Level::Error), Some(DIAG));
        assert_eq!(for_level(Level::Warning), Some(WARN));
        assert_eq!(for_level(Level::Notice), None);
        assert_ne!(WARN, DIAG, "a warning is not drawn as an error");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_emitted_for_a_shell_with_no_terminal() {
        // An escape in a script's output is corruption, not colour.
        assert_eq!(paint(false, PROMPT, "/home> "), "/home> ");
        assert_eq!(paint(false, DIAG, "nxsh: no such thing\n"), "nxsh: no such thing\n");
    }

    #[test]
    fn a_painted_run_keeps_its_text_contiguous() {
        let p = paint(true, PROMPT, "/home> ");
        assert!(p.contains("/home> "), "{p:?} split the prompt");
        assert!(p.starts_with(PROMPT) && p.ends_with(RESET), "{p:?}");
        // The newline rule is the callers' to keep: asserted on the bytes in `repl::diagnostic`'s
        // test and `ops::display`'s, not by a strip-and-compare, which cannot see which side of
        // the `\n` the reset fell on.
        for sgr in [PROMPT, BANNER, HEADER, DIAG] {
            let code: u16 = sgr.trim_start_matches("\x1b[").trim_end_matches('m').parse().unwrap();
            assert!(
                (30..=37).contains(&code) || (90..=97).contains(&code),
                "{sgr:?} is not one of the sixteen foreground codes"
            );
        }
    }
}
