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

    /// A name a binding takes, which is never `$env`: the shell's environment, wherever it is read.
    pub(super) fn binder(&mut self, message: &'static str) -> Result<String> {
        let name = self.ident(message)?;
        self.not_env(name)
    }

    pub(super) fn not_env(&mut self, name: String) -> Result<String> {
        if name == "$env" {
            return self.fail("`$env` is the shell's environment, and no binding takes its name");
        }
        Ok(name)
    }
}
