//! The REPL's language-side logic — everything about an interactive session that is not
//! reading bytes from a device.
//!
//! Milestone 3 Part F. The console loop lives in the binary; what lives here is the part
//! worth testing: **when is a typed line finished?**
//!
//! ## §11b, and why this is exact rather than heuristic
//!
//! Multi-line continuation is genuinely REPL-specific. A whole script file parses
//! unambiguously — leading-`|` style included — because the parser sees all of it at once
//! (that is D2's lookahead, in `lex`). The ambiguity exists only in the line-by-line,
//! decide-after-each-Enter model.
//!
//! So continuation covers **only states the parser can prove incomplete**:
//!
//! - an unclosed `(`, `[` or `{`;
//! - a trailing `|` — real precedent, since bash's own `PS2` triggers on a trailing
//!   `|`/`&&`/`\` for exactly this reason: the parser is demonstrably mid-production.
//!
//! Everything else executes. In particular a line that is a complete, valid statement, and
//! whose *next* line would begin with `|`, is **not** continued — that case has a real
//! one-bit ambiguity no grammar refinement can resolve, and §11b's answer is Shift-Enter,
//! which needs a key-event channel and belongs to the deferred rich REPL. Leading-pipe
//! style stays fully valid in a file.
//!
//! Guessing there would be worse than not offering it: a shell that sometimes swallowed a
//! finished command waiting for more would be unusable in the way that matters.

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

/// Decide whether more input is needed (§11b).
///
/// Lexes rather than counts characters, so a brace inside a string literal or a comment
/// does not open anything — the naive character scan gets `"{"` wrong, and getting it
/// wrong means hanging the prompt on a finished command.
pub fn needs_continuation(src: &str) -> Continue {
    let mut lx = Lexer::new(src);
    let mut depth = 0i32;
    let mut last = Tok::Newline;
    loop {
        let t = match lx.bump(Mode::Expr) {
            Ok(t) => t.tok,
            // A lexical error is not an incomplete line — except an unterminated string,
            // which is the one case where more input genuinely helps. Treated as complete
            // so the error is reported rather than swallowed into a hanging prompt.
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
/// **The two modes differ on purpose.** A REPL appends `| display` to an unassigned
/// top-level pipeline, which is what Python, nushell and PowerShell all do in some form. A
/// *script* discards it, because a script full of `remove`/`move` calls run for their
/// effects would otherwise flood its output.
///
/// A statement that already ends in a terminal operator is left alone — `… | display`
/// should print once, not twice — and an assignment prints nothing, since `let x = 5` is a
/// binding rather than a question.
pub fn should_display(stmt: &Stmt) -> bool {
    // **The same predicate the language uses for a block's value.** `is_expression_shaped`
    // counts `if` and `try` as value-producing (§9a), so a block ending in one evaluates to
    // it — but this asked only for `Stmt::Expr`, and the REPL threw the rest away. Typing
    // `if n > 3 { "big" } else { "small" }` at a prompt computed "big" and printed nothing.
    if !stmt.is_expression_shaped() {
        return false;
    }
    match stmt {
        Stmt::Expr(e) => !ends_in_terminal_operator(e),
        // An `if`/`try` cannot itself *be* a terminal operator; its branches are blocks,
        // and a branch ending in `save` yields null, which the caller already suppresses.
        _ => true,
    }
}

/// The longest line the REPL will accept from the terminal, matching the tty server's own
/// limit. Sized here so the read buffer and the discipline cannot silently disagree about
/// where a line stops.
pub const LINE_MAX: usize = 1024;

/// Operators that consume a value and end the chain, so the REPL must not add another.
const TERMINAL_OPERATORS: &[&str] = &["display", "save"];

fn ends_in_terminal_operator(e: &Expr) -> bool {
    match e {
        Expr::Pipeline(stages) => stages.last().is_some_and(ends_in_terminal_operator),
        Expr::Call(c) => TERMINAL_OPERATORS.contains(&c.name.as_str()),
        _ => false,
    }
}

/// The prompt for a fresh statement, showing the session's namespace position (§11a).
pub fn prompt(position: &str) -> String {
    let mut s = String::from(position);
    s.push_str("> ");
    s
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
        // …and closing it on a later line finishes the statement.
        assert_eq!(needs_continuation("def f() {\n1\n}"), Continue::No);
    }

    #[test]
    fn a_trailing_pipe_continues() {
        assert_eq!(needs_continuation("ls |"), Continue::TrailingPipe);
        assert_eq!(needs_continuation("ls | sort size"), Continue::No);
    }

    /// The naive character-counting version gets this wrong, and getting it wrong means
    /// hanging the prompt on a finished command.
    #[test]
    fn a_delimiter_inside_a_string_or_comment_opens_nothing() {
        assert_eq!(needs_continuation("let x = \"{\""), Continue::No);
        assert_eq!(needs_continuation("let x = 1 # {"), Continue::No);
    }

    /// §11b's genuinely ambiguous case is deliberately **not** continued: a complete
    /// statement stands, even though the next line might have begun with `|`. Only the
    /// person typing knows, and guessing would sometimes swallow a finished command.
    #[test]
    fn a_complete_statement_is_not_held_waiting_for_a_leading_pipe() {
        assert_eq!(needs_continuation("ls --long"), Continue::No);
        assert_eq!(needs_continuation("ls"), Continue::No);
    }

    /// An unterminated string is reported, not turned into a hanging prompt.
    #[test]
    fn a_lexical_error_is_not_treated_as_incomplete() {
        assert_eq!(needs_continuation("let x = \"abc"), Continue::No);
    }

    // --- auto-display (§11e) ------------------------------------------------

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

    /// A chain that already ends in a terminal operator prints once, not twice.
    #[test]
    fn a_chain_ending_in_display_or_save_is_left_alone() {
        assert!(!should_display(&first("ls | display")));
        assert!(!should_display(&first("ls | save ./x.tsm")));
        assert!(should_display(&first("ls | count")));
    }

    #[test]
    fn the_prompt_shows_the_namespace_position() {
        assert_eq!(prompt("/home/alice"), "/home/alice> ");
    }
}
