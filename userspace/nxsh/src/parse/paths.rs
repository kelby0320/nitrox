//! Word-mode arguments: an external's and a builtin's, and a path operator's.

use super::*;

impl Parser<'_> {
    /// `open`'s and `save`'s paths, read as `cd` reads its own: `hello.txt` is a path in word
    /// mode, where expression mode reads a field access. A block's `{` ends them.
    pub(super) fn path_args(&mut self) -> Result<Vec<Arg>> {
        let mut args = Vec::new();
        while !self.lx.opens_a_block() {
            let Some(arg) = self.word_arg()? else { break };
            args.push(arg);
        }
        Ok(args)
    }

    /// One word-mode argument (§5b, D1), or `None` at whatever ends the list.
    pub(super) fn word_arg(&mut self) -> Result<Option<Arg>> {
        let arg = match self.lx.peek(Mode::Word)?.tok {
            Tok::Flag(f) => Arg::Flag(f, None),
            Tok::ShortFlags(f) => Arg::ShortFlags(f),
            Tok::Word(w) => Arg::Positional(Expr::Word(w)),
            Tok::Str(v) => Arg::Positional(Expr::Str(v)),
            Tok::LParen => return Ok(Some(Arg::Positional(self.expr()?))),
            _ => return Ok(None),
        };
        self.lx.bump(Mode::Word)?;
        Ok(Some(arg))
    }
}
