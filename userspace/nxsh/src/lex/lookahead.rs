//! Lookahead: the one token held, and the raw source past horizontal whitespace and comments.

use super::{Lexer, Mode, Result, Spanned, Tok};

impl Lexer<'_> {
    /// The next token without consuming it, read in `mode`.
    pub fn peek(&mut self, mode: Mode) -> Result<Spanned> {
        if let Some(p) = &self.pending {
            if self.pending_mode == mode {
                return Ok(p.clone());
            }
            self.pos = self.pending_start;
            self.line = self.pending_line;
            self.pending = None;
        }
        let start_line = self.line;
        let tok = self.scan(mode)?;
        self.pending_mode = mode;
        self.pending_line = start_line;
        self.pending = Some(tok.clone());
        self.pending_start = tok.start;
        Ok(tok)
    }

    /// Consume and return the next token, read in `mode`.
    pub fn bump(&mut self, mode: Mode) -> Result<Spanned> {
        let t = self.peek(mode)?;
        self.pending = None;
        if t.tok != Tok::Eof {
            self.after_match_op = t.tok == Tok::Match_ || matches!(&t.tok, Tok::Ident(n) if n == "capture");
            self.prev = Some(t.tok.clone());
        }
        Ok(t)
    }

    /// After a peeked token, is there **nothing left that could be its right operand**? For a bare
    /// `/`; a newline counts, since a binary operator at end of line is mid-production under D2.
    pub fn no_operand_follows(&self) -> bool {
        matches!(self.at(self.past_space(self.pos)), 0 | b'\n' | b'|' | b')' | b']' | b'}' | b',')
    }

    /// Whether the next token is a `{`, which begins no word-mode argument.
    pub fn opens_a_block(&self) -> bool {
        let from = if self.pending.is_some() { self.pending_start } else { self.pos };
        self.at(self.past_space(from)) == b'{'
    }

    fn past_space(&self, mut i: usize) -> usize {
        loop {
            match self.at(i) {
                b' ' | b'\t' | b'\r' => i += 1,
                b'#' => {
                    while i < self.src.len() && self.at(i) != b'\n' {
                        i += 1;
                    }
                }
                _ => return i,
            }
        }
    }
}
