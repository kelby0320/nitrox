//! Lookahead over the raw source, past horizontal whitespace and comments.

use super::Lexer;

impl Lexer<'_> {
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
