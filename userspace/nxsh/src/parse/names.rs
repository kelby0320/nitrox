//! The names the parser reads.

use super::*;

impl Parser<'_> {
    pub(super) fn ident(&mut self, message: &'static str) -> Result<String> {
        match self.bump()? {
            Tok::Ident(s) => Ok(s),
            // Keywords that are also ordinary field/parameter names in practice.
            Tok::Match => Ok(String::from("match")),
            _ => self.fail(message),
        }
    }
}
