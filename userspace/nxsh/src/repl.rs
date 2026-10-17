//! The REPL's language-side logic: **when a typed line is finished** (§11b); the console loop lives
//! in the binary.
//!
//! Continuation covers only states the parser can prove incomplete: an unclosed `(`, `[` or `{`,
//! and a trailing `|`. A complete statement whose next line would begin with `|` is not continued:
//! that one-bit ambiguity cannot be resolved, and guessing would sometimes swallow a finished
//! command.

use alloc::string::String;

use crate::ast::{Expr, Stmt};
use crate::lex::{Lexer, Mode, Tok};

/// Why a typed line is not yet complete, or [`Continue::No`] if it is.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Continue {
    /// Execute it.
    No,
    /// A delimiter is open.
    Unclosed,
    /// The line ends on `|`, so a stage is expected.
    TrailingPipe,
}

/// Decide whether more input is needed (§11b). Lexes, so a brace inside a string or a comment opens
/// nothing.
pub fn needs_continuation(src: &str) -> Continue {
    let mut lx = Lexer::new(src);
    let mut depth = 0i32;
    let mut last = Tok::Newline;
    loop {
        let t = match lx.bump(Mode::Expr) {
            Ok(t) => t.tok,
            // A lexical error is reported rather than swallowed into a hanging prompt, even an
            // unterminated string.
            Err(_) => return Continue::No,
        };
        match t {
            Tok::Eof => break,
            Tok::LParen | Tok::LBracket | Tok::LBrace => depth += 1,
            Tok::RParen | Tok::RBracket | Tok::RBrace => depth -= 1,
            _ => {}
        }
        if t != Tok::Newline {
            last = t;
        }
    }
    if depth > 0 {
        return Continue::Unclosed;
    }
    if last == Tok::Pipe {
        return Continue::TrailingPipe;
    }
    Continue::No
}

/// Whether a statement's value should be printed in the REPL (§11e).
///
/// The two modes differ on purpose: a REPL appends `| display` to an unassigned top-level pipeline,
/// a script discards it. A chain already ending in a terminal operator is left alone, and an
/// assignment prints nothing.
pub fn should_display(stmt: &Stmt) -> bool {
    // Asks the language's own predicate for a block's value, so an `if` is echoed at the prompt;
    // the two must agree.
    if !stmt.is_expression_shaped() {
        return false;
    }
    match stmt {
        Stmt::Expr(e) => !ends_in_terminal_operator(e),
        // A branch ending in `save` yields null, which the caller already suppresses.
        _ => true,
    }
}

/// The longest line the REPL will accept, matching the tty server's own limit.
pub const LINE_MAX: usize = 1024;

/// Operators that consume a value and end the chain, so the REPL must not add another.
const TERMINAL_OPERATORS: &[&str] = &["display", "save"];

fn ends_in_terminal_operator(e: &Expr) -> bool {
    match e {
        Expr::Pipeline(stages) => stages.last().is_some_and(ends_in_terminal_operator),
        Expr::Time(timed) => ends_in_terminal_operator(timed),
        Expr::Call(c) => TERMINAL_OPERATORS.contains(&c.name.as_str()),
        _ => false,
    }
}

/// The prompt for a fresh statement, showing the session's namespace position (§11a).
///
/// It announces that position to the terminal too, as `OSC 7`, which `nxterm` puts in the window's
/// title bar; a terminal that ignores it swallows it whole.
pub fn prompt(position: &str, styled: bool) -> String {
    let mut s = String::from("\x1b]7;");
    s.push_str(position);
    s.push('\x07');
    // The prompt, trailing space included, in bright cyan; see
    // [`style::paint`](crate::style::paint).
    let mut text = String::from(position);
    text.push_str("> ");
    s.push_str(&crate::style::paint(styled, crate::style::PROMPT, &text));
    s
}

/// The shell's own diagnostic, as it goes to a terminal: `nxsh: <message>`, in the diagnostic
/// colour when there is one.
///
/// No trailing newline, by contract: `tty_write_crlf` ends every chunk it splits on `\n`, so a
/// reset after a newline would be a blank line with `\x1b[0m` at the head of the next.
pub fn diagnostic(message: &str, styled: bool) -> String {
    let mut body = String::from("nxsh: ");
    body.push_str(message);
    crate::style::paint(styled, crate::style::DIAG, &body)
}

/// The continuation prompt, bash's `PS2` by another name.
pub fn continuation_prompt() -> &'static str {
    "... "
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_script;

    #[test]
    fn a_complete_line_executes() {
        assert_eq!(needs_continuation("let x = 1"), Continue::No);
        assert_eq!(needs_continuation("ls --long"), Continue::No);
        assert_eq!(needs_continuation(""), Continue::No);
    }

    #[test]
    fn an_unclosed_delimiter_continues() {
        assert_eq!(needs_continuation("if (a > 1"), Continue::Unclosed);
        assert_eq!(needs_continuation("let x = ["), Continue::Unclosed);
        assert_eq!(needs_continuation("def f() {"), Continue::Unclosed);
        assert_eq!(needs_continuation("def f() {\n1\n}"), Continue::No);
    }

    #[test]
    fn a_trailing_pipe_continues() {
        assert_eq!(needs_continuation("ls |"), Continue::TrailingPipe);
        assert_eq!(needs_continuation("ls | sort size"), Continue::No);
    }

    #[test]
    fn a_delimiter_inside_a_string_or_comment_opens_nothing() {
        assert_eq!(needs_continuation("let x = \"{\""), Continue::No);
        assert_eq!(needs_continuation("let x = 1 # {"), Continue::No);
    }

    #[test]
    fn a_complete_statement_is_not_held_waiting_for_a_leading_pipe() {
        assert_eq!(needs_continuation("ls --long"), Continue::No);
        assert_eq!(needs_continuation("ls"), Continue::No);
    }

    #[test]
    fn a_lexical_error_is_not_treated_as_incomplete() {
        assert_eq!(needs_continuation("let x = \"abc"), Continue::No);
    }

    fn first(src: &str) -> Stmt {
        parse_script(src).expect("parses").stmts.into_iter().next().expect("one statement")
    }

    #[test]
    fn a_bare_expression_is_displayed_but_a_binding_is_not() {
        assert!(should_display(&first("1 + 1")));
        assert!(should_display(&first("ls | sort size")));
        assert!(!should_display(&first("let x = 1")));
        assert!(!should_display(&first("mut x = 1")));
    }

    #[test]
    fn a_chain_ending_in_display_or_save_is_left_alone() {
        assert!(!should_display(&first("ls | display")));
        assert!(!should_display(&first("ls | save ./x.tsm")));
        assert!(should_display(&first("ls | count")));
    }

    #[test]
    fn a_diagnostic_is_painted_and_carries_no_line_of_its_own() {
        assert_eq!(diagnostic("boom", false), "nxsh: boom");

        let d = diagnostic("boom", true);
        assert!(d.contains("nxsh: boom"), "{d:?}");
        // Asserted on the bytes: stripping the escapes gives the same text either way.
        assert!(!d.contains('\n'), "{d:?} carries a newline the caller must add");
        assert!(d.ends_with(crate::style::RESET), "{d:?}");
    }

    #[test]
    fn the_prompt_shows_the_namespace_position() {
        let p = prompt("/home/alice", false);
        assert!(p.ends_with("/home/alice> "), "{p:?}");
        // The terminal is told the same position, byte for byte. Spelled out rather than built from
        // a constant, because the other end of the agreement is `libterm::parse`;
        // `cargo xtask check-terminal` is what boots both.
        assert_eq!(p, "\x1b]7;/home/alice\x07/home/alice> ");

        let c = prompt("/home/alice", true);
        assert!(c.starts_with("\x1b]7;/home/alice\x07"), "{c:?}");
        assert!(c.contains("/home/alice> "), "{c:?} split the prompt");
        assert!(c.ends_with(crate::style::RESET), "{c:?}");
    }
}
