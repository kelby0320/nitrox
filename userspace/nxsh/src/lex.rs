//! The `nxsh` lexer — tokens, and the three context decisions the grammar needs.
//!
//! Milestone 3 Part A. A shell's lexer is not the boring part: three of the plan's
//! design gaps (D1, D2, D3) are lexical, and each is a place where a naive tokenizer
//! silently produces the wrong thing rather than failing.
//!
//! ## D1 — word mode and expression mode are different languages
//!
//! The design specifies both of these, and no single tokenizer can serve them:
//!
//! ```text
//! filter size > 1000        # §8b: an expression. `size` is field shorthand on `it`
//! list --long /some/path    # §5b: barewords. `/some/path` is a string
//! ```
//!
//! Read as an expression, `/some/path` is a division operator with no left operand, and
//! `README.md` is field access on `README`. Read as words, `size > 1000` is three
//! meaningless strings.
//!
//! So the lexer has **two modes**, and the *parser* selects between them — it is the only
//! party that knows whether it is reading an argument list, and whose head token decides
//! the category (§3). [`Lexer::peek`] and [`Lexer::bump`] therefore take a [`Mode`], and
//! a token is re-lexed from its own start if it is asked for again in the other mode.
//! Modes differ *only* in how a token starting with a non-operator character is read:
//! delimiters, strings and numbers lex identically in both.
//!
//! ## D2 — newlines end statements, except when they demonstrably do not
//!
//! §9a settles that the grammar is newline-delimited with no semicolons, and §11b asserts
//! that a whole script parses leading-`|` style unambiguously — but nothing states the
//! rule that makes that true. It is implemented here, in [`Lexer::suppress_newline`],
//! because the lexer is what holds the bracket depth and the previous token:
//!
//! - Inside `(` or `[`, a newline is plain whitespace. **Not** inside `{`: a block's
//!   statements need their separators. A multi-line record literal still works, because
//!   it is covered by the trailing-comma rule below.
//! - After a token that cannot end a statement — `|`, `&&`, `,`, any binary operator, an
//!   opening delimiter, `=`, `->`, `=>`, `:`, `.` — the newline is suppressed. This is
//!   bash's `PS2` rule and Go's semicolon insertion, from the same reasoning: the parser
//!   is demonstrably mid-production, not guessing.
//! - **Before** a leading `|`, `else` or `catch`. This is the one that needs lookahead,
//!   and the one §11b depends on: `ls\n  | filter …` is one pipeline in a file because
//!   the lexer looks past the newline and finds a `|` that cannot begin a statement.
//!
//! Every rule is a case the parser could *prove* incomplete. Nothing here is a heuristic.
//!
//! ## D3 — `/` begins a path or a regex, and the difference is one token of context
//!
//! `list /system` and `name ~= /\.rs$/` both put `/` where an operand belongs. The rule:
//! **a regex literal is lexed only immediately after `~=`**, the one operator that takes
//! one (§10b); a leading `/` anywhere else begins a path word. This is the same shape as
//! JavaScript's regex-versus-divide problem, with a far narrower trigger — one operator
//! rather than a general "could an operand appear here" analysis.
//!
//! `./x` and `../x` need no rule at all: `.` is otherwise strictly infix, so a `.` with
//! no left operand is already unambiguous.

use alloc::string::String;
use alloc::vec::Vec;

/// Which language the next token is read in. See the module docs (D1).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Operators, identifiers and literals — the §8 expression grammar.
    Expr,
    /// Barewords: an unquoted run of non-whitespace is a [`Tok::Word`], and operator
    /// characters inside it are ordinary text. Flags (`--long`, `-f`) are recognised.
    Word,
}

/// A lexed token. Positions are carried by [`Spanned`], not here.
#[derive(Clone, PartialEq, Debug)]
pub enum Tok {
    // --- literals -----------------------------------------------------------
    Int(i64),
    Float(f64),
    /// A quoted string. No interpolation — §8d settled on `format()` instead.
    Str(String),
    /// A bareword read in [`Mode::Word`]: a path, a filename, a literal argument.
    Word(String),
    /// A `/pattern/` regex literal. Only ever produced immediately after `~=` (D3).
    Regex(String),
    Ident(String),
    /// `--name`, in word mode.
    Flag(String),
    /// `-f`, in word mode. A run like `-abc` lexes as one `ShortFlags("abc")`.
    ShortFlags(String),

    // --- keywords -----------------------------------------------------------
    Let,
    Mut,
    Const,
    Pub,
    If,
    Else,
    For,
    In,
    While,
    /// `break` / `continue` — loop-only, and the parser is what enforces that (§9c).
    Break,
    Continue,
    Def,
    Return,
    /// `fail expr` — raising an error, the half `try`/`catch` was missing (§2).
    Fail,
    Try,
    Catch,
    Strict,
    Match,
    Use,
    As,
    Expect,
    Assert,
    /// `parse T` — §6's converting sibling of `expect T`.
    Parse,
    True,
    False,
    Null,

    // --- operators and punctuation ------------------------------------------
    Pipe,
    OrOr,
    AndAnd,
    Plus,
    PlusPlus,
    Minus,
    Star,
    Slash,
    Percent,
    Lt,
    Le,
    Gt,
    Ge,
    EqEq,
    Ne,
    /// `~=` — regex match (§10b).
    Match_,
    Eq,
    Bang,
    Dot,
    DotDot,
    DotDotEq,
    /// `...` — the variadic-parameter marker (§5b).
    Ellipsis,
    /// `?` — Result propagation (§2).
    Question,
    /// `?.` — safe navigation (§9e).
    QuestionDot,
    /// `??` — null coalescing (§9e).
    QuestionQuestion,
    Arrow,
    FatArrow,
    Colon,
    Comma,
    At,
    /// `^` — force external resolution (§3).
    Caret,
    Underscore,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,

    /// A statement separator that survived [`Lexer::suppress_newline`].
    Newline,
    Eof,
}

impl Tok {
    /// Whether a statement could legally end on this token. Drives the D2 rule that a
    /// newline after an incomplete construct is not a terminator.
    ///
    /// Deliberately a *whitelist of enders* rather than a list of continuers: a new
    /// operator added to the language should default to "continues", which produces a
    /// parse error on the next line, rather than to "ends", which silently splits one
    /// statement into two.
    fn can_end_statement(&self) -> bool {
        matches!(
            self,
            Tok::Int(_)
                | Tok::Float(_)
                | Tok::Str(_)
                | Tok::Word(_)
                | Tok::Regex(_)
                | Tok::Ident(_)
                | Tok::Flag(_)
                | Tok::ShortFlags(_)
                | Tok::True
                | Tok::False
                | Tok::Null
                | Tok::Return
                | Tok::Break
                | Tok::Continue
                | Tok::Underscore
                | Tok::Question
                | Tok::RParen
                | Tok::RBracket
                | Tok::RBrace
        )
    }
}

/// A token with its source position, for error reporting.
#[derive(Clone, PartialEq, Debug)]
pub struct Spanned {
    pub tok: Tok,
    /// Byte offset of the token's first character.
    pub start: usize,
    /// 1-based line, for messages.
    pub line: u32,
    /// Whether whitespace separated this token from the previous one.
    ///
    /// Load-bearing, not decoration: `a - b` and `list -l` differ only here, and so do
    /// `x.field` and a command taking `.field` as an argument. Shells have always read
    /// these by spacing; making it explicit is what lets D9 be decided without guessing.
    pub space_before: bool,
}

/// A lexical error. Carries a position so the parser can report in context.
#[derive(Clone, PartialEq, Debug)]
pub struct LexError {
    pub message: &'static str,
    pub start: usize,
    pub line: u32,
}

type Result<T> = core::result::Result<T, LexError>;

/// The lexer. Pull-driven: the parser asks for one token at a time in a [`Mode`] it
/// chooses, because mode is a parser-level fact (D1).
///
/// One token of lookahead is cached. If it is asked for again in a *different* mode it is
/// re-lexed from its own start offset, which is why [`pending_start`](Self::pending_start)
/// is kept — re-lexing has to be exact, not approximate.
pub struct Lexer<'a> {
    src: &'a [u8],
    /// Offset just past the cached token, or the scan position when nothing is cached.
    pos: usize,
    line: u32,
    /// The cached token, the mode it was lexed in, and where it began.
    pending: Option<Spanned>,
    pending_mode: Mode,
    pending_start: usize,
    pending_line: u32,
    /// Nesting depth of `(` and `[` only. `{` is excluded on purpose: blocks need their
    /// newlines (D2).
    depth: u32,
    /// Whether the previous significant token was `~=`, which is what licenses a regex
    /// literal (D3).
    after_match_op: bool,
    /// The previous significant token, for the newline rule.
    prev: Option<Tok>,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Lexer<'a> {
        Lexer {
            src: src.as_bytes(),
            pos: 0,
            line: 1,
            pending: None,
            pending_mode: Mode::Expr,
            pending_start: 0,
            pending_line: 1,
            depth: 0,
            after_match_op: false,
            prev: None,
        }
    }

    /// The next token without consuming it, read in `mode`.
    pub fn peek(&mut self, mode: Mode) -> Result<Spanned> {
        if let Some(p) = &self.pending {
            if self.pending_mode == mode {
                return Ok(p.clone());
            }
            // Asked for in the other mode: rewind to where it started and read again.
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
            self.after_match_op = t.tok == Tok::Match_;
            self.prev = Some(t.tok.clone());
        }
        Ok(t)
    }

    /// After a peeked token, is there **nothing left that could be its right operand**?
    ///
    /// Pure lookahead over the raw source from just past the cached token: horizontal
    /// whitespace and comments are skipped, then the answer is yes at end of input, at a
    /// newline, and at each closer that ends an argument list (`|`, `)`, `]`, `}`, `,`).
    ///
    /// This exists for one decision — a bare `/` (see [`Parser::starts_an_argument`]) —
    /// and deliberately does **not** skip newlines. A binary operator at end of line is
    /// mid-production under D2, so "the line ended" is a real answer here, not a gap to
    /// look past.
    pub fn no_operand_follows(&self) -> bool {
        let mut i = self.pos;
        loop {
            match self.at(i) {
                b' ' | b'\t' | b'\r' => i += 1,
                b'#' => {
                    while i < self.src.len() && self.at(i) != b'\n' {
                        i += 1;
                    }
                }
                _ => break,
            }
        }
        matches!(self.at(i), 0 | b'\n' | b'|' | b')' | b']' | b'}' | b',')
    }

    /// Tokenize the whole input in [`Mode::Expr`]. Only useful for tests and for the
    /// lexer's own diagnostics — real parsing is pull-driven, since mode is contextual.
    pub fn tokenize_expr(src: &str) -> Result<Vec<Tok>> {
        let mut lx = Lexer::new(src);
        let mut out = Vec::new();
        loop {
            let t = lx.bump(Mode::Expr)?;
            let end = t.tok == Tok::Eof;
            out.push(t.tok);
            if end {
                return Ok(out);
            }
        }
    }

    // --- scanning -----------------------------------------------------------

    fn at(&self, i: usize) -> u8 {
        if i < self.src.len() { self.src[i] } else { 0 }
    }

    fn err<T>(&self, message: &'static str, start: usize) -> Result<T> {
        Err(LexError { message, start, line: self.line })
    }

    fn scan(&mut self, mode: Mode) -> Result<Spanned> {
        let before = self.pos;
        // Horizontal whitespace and comments never produce tokens. A newline may.
        loop {
            match self.at(self.pos) {
                b' ' | b'\t' | b'\r' => self.pos += 1,
                b'#' => {
                    while self.pos < self.src.len() && self.at(self.pos) != b'\n' {
                        self.pos += 1;
                    }
                }
                b'\n' => {
                    let start = self.pos;
                    let line = self.line;
                    self.pos += 1;
                    self.line += 1;
                    if self.suppress_newline() {
                        continue;
                    }
                    return Ok(Spanned { tok: Tok::Newline, start, line, space_before: true });
                }
                _ => break,
            }
        }
        let start = self.pos;
        let line = self.line;
        let space_before = start > before || start == 0;
        if self.pos >= self.src.len() {
            return Ok(Spanned { tok: Tok::Eof, start, line, space_before });
        }
        let tok = match mode {
            Mode::Expr => self.scan_expr(start)?,
            Mode::Word => self.scan_word_mode(start)?,
        };
        Ok(Spanned { tok, start, line, space_before })
    }

    /// The D2 decision, in one place: does the newline just consumed get discarded?
    ///
    /// Three independent reasons, all of them cases the parser could prove rather than
    /// guess. See the module docs.
    fn suppress_newline(&mut self) -> bool {
        // 1. Inside `(` or `[` a newline carries no meaning.
        if self.depth > 0 {
            return true;
        }
        // 2. The previous token cannot end a statement, so this one is mid-construct.
        match &self.prev {
            None => return true, // leading blank lines
            Some(t) if !t.can_end_statement() => return true,
            _ => {}
        }
        // 3. The *next* thing continues the previous line. This is the lookahead §11b
        //    needs: leading-pipe style, and `else`/`catch` on their own line.
        self.starts_continuation()
    }

    /// Scan forward over whitespace, comments and further newlines; report whether what
    /// follows can only be a continuation of the previous statement.
    ///
    /// Does not move `self.pos` — this is pure lookahead.
    fn starts_continuation(&self) -> bool {
        let mut i = self.pos;
        loop {
            match self.at(i) {
                b' ' | b'\t' | b'\r' | b'\n' => i += 1,
                b'#' => {
                    while i < self.src.len() && self.at(i) != b'\n' {
                        i += 1;
                    }
                }
                _ => break,
            }
        }
        if i >= self.src.len() {
            return false;
        }
        // A single `|` — a leading pipeline stage. `||` is logical-or, which cannot begin
        // a statement either, but treating it as a continuation would be guessing at a
        // construct nobody writes; keep the rule to the one §11b names.
        if self.at(i) == b'|' && self.at(i + 1) != b'|' {
            return true;
        }
        // `->` can only continue a `def` signature whose return type wrapped — §7's own
        // worked example is written that way.
        if self.at(i) == b'-' && self.at(i + 1) == b'>' {
            return true;
        }
        starts_keyword(&self.src[i..], b"else") || starts_keyword(&self.src[i..], b"catch")
    }

    // --- expression mode ----------------------------------------------------

    fn scan_expr(&mut self, start: usize) -> Result<Tok> {
        let c = self.at(start);

        // A regex literal, and only here (D3).
        if c == b'/' && self.after_match_op {
            return self.scan_regex(start);
        }
        // A path in expression position: `/etc/x`, `./x`, `../x`. `/` is otherwise
        // division, and `.` is otherwise field access — both strictly infix, so a
        // leading one is unambiguous.
        if c == b'/' && is_path_char(self.at(start + 1)) {
            return Ok(Tok::Word(self.scan_bareword(start)));
        }
        if c == b'.' && (self.at(start + 1) == b'/'
            || (self.at(start + 1) == b'.' && self.at(start + 2) == b'/'))
        {
            return Ok(Tok::Word(self.scan_bareword(start)));
        }

        if c.is_ascii_digit() {
            return self.scan_number(start);
        }
        if c == b'"' {
            return self.scan_string(start);
        }
        if is_ident_start(c) {
            let word = self.scan_ident(start);
            return Ok(keyword_or_ident(word));
        }
        self.scan_punct(start)
    }

    fn scan_punct(&mut self, start: usize) -> Result<Tok> {
        let c = self.at(start);
        let d = self.at(start + 1);
        let e = self.at(start + 2);
        // Longest match first, always — `..=` before `..` before `.`, and so on.
        let (tok, len) = match (c, d, e) {
            (b'.', b'.', b'.') => (Tok::Ellipsis, 3),
            (b'.', b'.', b'=') => (Tok::DotDotEq, 3),
            (b'.', b'.', _) => (Tok::DotDot, 2),
            (b'.', _, _) => (Tok::Dot, 1),
            (b'|', b'|', _) => (Tok::OrOr, 2),
            (b'|', _, _) => (Tok::Pipe, 1),
            (b'&', b'&', _) => (Tok::AndAnd, 2),
            (b'+', b'+', _) => (Tok::PlusPlus, 2),
            (b'+', _, _) => (Tok::Plus, 1),
            (b'-', b'>', _) => (Tok::Arrow, 2),
            (b'-', _, _) => (Tok::Minus, 1),
            (b'*', _, _) => (Tok::Star, 1),
            (b'/', _, _) => (Tok::Slash, 1),
            (b'%', _, _) => (Tok::Percent, 1),
            (b'<', b'=', _) => (Tok::Le, 2),
            (b'<', _, _) => (Tok::Lt, 1),
            (b'>', b'=', _) => (Tok::Ge, 2),
            (b'>', _, _) => (Tok::Gt, 1),
            (b'=', b'=', _) => (Tok::EqEq, 2),
            (b'=', b'>', _) => (Tok::FatArrow, 2),
            (b'=', _, _) => (Tok::Eq, 1),
            (b'!', b'=', _) => (Tok::Ne, 2),
            (b'!', _, _) => (Tok::Bang, 1),
            (b'~', b'=', _) => (Tok::Match_, 2),
            (b'?', b'.', _) => (Tok::QuestionDot, 2),
            (b'?', b'?', _) => (Tok::QuestionQuestion, 2),
            (b'?', _, _) => (Tok::Question, 1),
            (b':', _, _) => (Tok::Colon, 1),
            (b',', _, _) => (Tok::Comma, 1),
            (b'@', _, _) => (Tok::At, 1),
            (b'^', _, _) => (Tok::Caret, 1),
            (b'(', _, _) => (Tok::LParen, 1),
            (b')', _, _) => (Tok::RParen, 1),
            (b'[', _, _) => (Tok::LBracket, 1),
            (b']', _, _) => (Tok::RBracket, 1),
            (b'{', _, _) => (Tok::LBrace, 1),
            (b'}', _, _) => (Tok::RBrace, 1),
            _ => return self.err("unexpected character", start),
        };
        self.pos = start + len;
        match tok {
            Tok::LParen | Tok::LBracket => self.depth += 1,
            Tok::RParen | Tok::RBracket => self.depth = self.depth.saturating_sub(1),
            _ => {}
        }
        Ok(tok)
    }

    // --- word mode ----------------------------------------------------------

    /// In word mode almost everything is a bareword. Only quoting, flags and the
    /// delimiters that *end* an argument list are special — an argument list has to be
    /// able to end, and `ls foo | wc` has to find its pipe.
    fn scan_word_mode(&mut self, start: usize) -> Result<Tok> {
        let c = self.at(start);
        if c == b'"' {
            return self.scan_string(start);
        }
        // `--name`, but bare `--` ends option parsing and is itself a word (§10f).
        if c == b'-' && self.at(start + 1) == b'-' && is_ident_start(self.at(start + 2)) {
            self.pos = start + 2;
            let name = self.scan_bareword(self.pos);
            return Ok(Tok::Flag(name));
        }
        if c == b'-' && self.at(start + 1).is_ascii_alphabetic() {
            self.pos = start + 1;
            let flags = self.scan_bareword(self.pos);
            return Ok(Tok::ShortFlags(flags));
        }
        // Structure that must stay visible so an argument list can end.
        let structural = match c {
            b'|' if self.at(start + 1) != b'|' => Some((Tok::Pipe, 1)),
            b'|' => Some((Tok::OrOr, 2)),
            b')' => Some((Tok::RParen, 1)),
            b']' => Some((Tok::RBracket, 1)),
            b'}' => Some((Tok::RBrace, 1)),
            b'(' => Some((Tok::LParen, 1)),
            b'&' if self.at(start + 1) == b'&' => Some((Tok::AndAnd, 2)),
            _ => None,
        };
        if let Some((tok, len)) = structural {
            self.pos = start + len;
            match tok {
                Tok::LParen => self.depth += 1,
                Tok::RParen | Tok::RBracket => self.depth = self.depth.saturating_sub(1),
                _ => {}
            }
            return Ok(tok);
        }
        let word = self.scan_bareword(start);
        // **A zero-length token is an infinite loop, not a wrong answer.** `scan_bareword`
        // stops at anything that is not a word character, and `]`/`)`/`}` are handled
        // above as structure — but their *openers* are not, so `list [x]` scanned nothing,
        // made no progress, and the argument loop bumped the same empty `Word` forever.
        // It is the same failure `scan_ident` documents for `$`, one mode over.
        //
        // Refusing here makes the hang impossible for any character, rather than for the
        // ones somebody thought of: every path out of this function now advances `pos` or
        // returns an error.
        if word.is_empty() {
            return self.err(
                "this character cannot begin a bareword argument — quote it if it is part \
                 of a name",
                start,
            );
        }
        Ok(Tok::Word(word))
    }

    /// An unquoted run, ending at whitespace or at structure that could close the
    /// argument list. Operator characters inside it are ordinary text: `a+b` is one word.
    fn scan_bareword(&mut self, start: usize) -> String {
        let mut i = start;
        while i < self.src.len() && is_word_char(self.at(i)) {
            i += 1;
        }
        self.pos = i;
        String::from_utf8_lossy(&self.src[start..i]).into_owned()
    }

    // --- literals -----------------------------------------------------------

    fn scan_ident(&mut self, start: usize) -> &'a str {
        // Start past the first byte: the caller has already checked `is_ident_start`, and
        // a start character need not also be a *continue* character. `$` is exactly that
        // case — scanning from `start` consumed nothing and made no progress, so the outer
        // loop spun forever. An infinite loop, not a wrong token, which is why it showed
        // up as a hung test run rather than a failure.
        let mut i = start + 1;
        while i < self.src.len() && is_ident_char(self.at(i)) {
            i += 1;
        }
        self.pos = i;
        // SAFETY-free: `is_ident_char` admits ASCII only, so this slice is valid UTF-8.
        core::str::from_utf8(&self.src[start..i]).unwrap_or("")
    }

    /// `"…"` with backslash escapes. No interpolation — §8d rejected it, so a string
    /// literal never becomes a recursive parse.
    fn scan_string(&mut self, start: usize) -> Result<Tok> {
        let mut out = String::new();
        let mut i = start + 1;
        loop {
            if i >= self.src.len() {
                return self.err("unterminated string literal", start);
            }
            match self.at(i) {
                b'"' => {
                    self.pos = i + 1;
                    return Ok(Tok::Str(out));
                }
                b'\\' => {
                    i += 1;
                    let c = match self.at(i) {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'0' => '\0',
                        b'\\' => '\\',
                        b'"' => '"',
                        _ => return self.err("unknown escape in string literal", i - 1),
                    };
                    out.push(c);
                    i += 1;
                }
                b'\n' => return self.err("unterminated string literal", start),
                c => {
                    out.push(c as char);
                    i += 1;
                }
            }
        }
    }

    /// `/pattern/`, with `\/` escaping a slash. Only reachable right after `~=` (D3).
    fn scan_regex(&mut self, start: usize) -> Result<Tok> {
        let mut out = String::new();
        let mut i = start + 1;
        loop {
            if i >= self.src.len() || self.at(i) == b'\n' {
                return self.err("unterminated regex literal", start);
            }
            match self.at(i) {
                b'/' => {
                    self.pos = i + 1;
                    return Ok(Tok::Regex(out));
                }
                b'\\' if self.at(i + 1) == b'/' => {
                    out.push('/');
                    i += 2;
                }
                b'\\' => {
                    // Every other escape is the regex engine's to interpret, not ours.
                    out.push('\\');
                    out.push(self.at(i + 1) as char);
                    i += 2;
                }
                c => {
                    out.push(c as char);
                    i += 1;
                }
            }
        }
    }

    /// Integers (decimal, `0x`, `0b`) and floats, with `_` separators stripped (§8e).
    ///
    /// No leading-zero octal: `010` is ten, deliberately, because a silent `010 == 8` is
    /// a well-known C footgun and §8e rejects it outright.
    fn scan_number(&mut self, start: usize) -> Result<Tok> {
        let mut i = start;
        // Radix prefixes.
        if self.at(i) == b'0' && matches!(self.at(i + 1), b'x' | b'X' | b'b' | b'B') {
            let radix: u32 = if matches!(self.at(i + 1), b'x' | b'X') { 16 } else { 2 };
            i += 2;
            let digits_start = i;
            let mut v: i64 = 0;
            let mut any = false;
            while i < self.src.len() {
                let c = self.at(i);
                if c == b'_' {
                    i += 1;
                    continue;
                }
                let Some(d) = (c as char).to_digit(radix) else { break };
                v = match v.checked_mul(radix as i64).and_then(|v| v.checked_add(d as i64)) {
                    Some(v) => v,
                    None => return self.err("integer literal overflows i64", start),
                };
                any = true;
                i += 1;
            }
            if !any {
                return self.err("radix prefix with no digits", digits_start);
            }
            self.pos = i;
            return Ok(Tok::Int(v));
        }

        let mut text = String::new();
        while i < self.src.len() && (self.at(i).is_ascii_digit() || self.at(i) == b'_') {
            if self.at(i) != b'_' {
                text.push(self.at(i) as char);
            }
            i += 1;
        }
        // A float needs a digit after the `.` — `1..5` is a range over integers, and
        // `x.field` must never be read as a number.
        let mut is_float = false;
        if self.at(i) == b'.' && self.at(i + 1).is_ascii_digit() {
            is_float = true;
            text.push('.');
            i += 1;
            while i < self.src.len() && (self.at(i).is_ascii_digit() || self.at(i) == b'_') {
                if self.at(i) != b'_' {
                    text.push(self.at(i) as char);
                }
                i += 1;
            }
        }
        if matches!(self.at(i), b'e' | b'E') {
            let mut j = i + 1;
            if matches!(self.at(j), b'+' | b'-') {
                j += 1;
            }
            if self.at(j).is_ascii_digit() {
                is_float = true;
                text.push('e');
                if matches!(self.at(i + 1), b'+' | b'-') {
                    text.push(self.at(i + 1) as char);
                }
                i = j;
                while i < self.src.len() && self.at(i).is_ascii_digit() {
                    text.push(self.at(i) as char);
                    i += 1;
                }
            }
        }
        self.pos = i;
        if is_float {
            match parse_f64(&text) {
                Some(f) => Ok(Tok::Float(f)),
                None => self.err("malformed float literal", start),
            }
        } else {
            match parse_i64(&text) {
                Some(v) => Ok(Tok::Int(v)),
                None => self.err("integer literal overflows i64", start),
            }
        }
    }
}

// --- character classes ------------------------------------------------------

/// What may begin an identifier.
///
/// `$` is included so the REPL's own bindings — `$last` (§11d) and `$env` — are nameable
/// from source at all. Part F bound `$last` and nothing could refer to it, because the
/// lexer had no way to produce that token: a silent dead end rather than an error.
///
/// It is deliberately *only* a start character, so `a$b` is not one identifier and `$` has
/// no meaning mid-word. Nothing else in the grammar uses it.
fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'$'
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// What may follow a leading `/` for it to be a path rather than division.
fn is_path_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b'/')
}

/// What continues a bareword. Ends at whitespace and at the structure that can close an
/// argument list — everything else, operators included, is ordinary text in a word.
fn is_word_char(c: u8) -> bool {
    !matches!(
        c,
        0 | b' ' | b'\t' | b'\r' | b'\n' | b'|' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'"'
    )
}

/// Whether `s` begins with `kw` as a whole word.
fn starts_keyword(s: &[u8], kw: &[u8]) -> bool {
    s.len() >= kw.len()
        && &s[..kw.len()] == kw
        && (s.len() == kw.len() || !is_ident_char(s[kw.len()]))
}

fn keyword_or_ident(w: &str) -> Tok {
    match w {
        "let" => Tok::Let,
        "mut" => Tok::Mut,
        "const" => Tok::Const,
        "pub" => Tok::Pub,
        "if" => Tok::If,
        "else" => Tok::Else,
        "for" => Tok::For,
        "in" => Tok::In,
        "while" => Tok::While,
        "break" => Tok::Break,
        "continue" => Tok::Continue,
        "def" => Tok::Def,
        "return" => Tok::Return,
        "fail" => Tok::Fail,
        "try" => Tok::Try,
        "catch" => Tok::Catch,
        "strict" => Tok::Strict,
        "match" => Tok::Match,
        "use" => Tok::Use,
        "as" => Tok::As,
        "expect" => Tok::Expect,
        "assert" => Tok::Assert,
        "parse" => Tok::Parse,
        "true" => Tok::True,
        "false" => Tok::False,
        "null" => Tok::Null,
        "_" => Tok::Underscore,
        _ => Tok::Ident(String::from(w)),
    }
}

// --- numeric conversion (no `std`, so these are hand-rolled) ----------------

fn parse_i64(s: &str) -> Option<i64> {
    let mut v: i64 = 0;
    for c in s.bytes() {
        let d = (c as char).to_digit(10)?;
        v = v.checked_mul(10)?.checked_add(d as i64)?;
    }
    Some(v)
}

/// Decimal float parsing, sufficient for source literals.
///
/// Scales an integer mantissa by a power of ten rather than accumulating digit by digit
/// in floating point: `0.1` and `1.5` come out as the nearest `f64` to the written value,
/// which digit-wise accumulation does not guarantee. The same integer-scaling reasoning
/// as `coreutils::time::parse_duration`.
fn parse_f64(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut mantissa: u64 = 0;
    let mut scale: i32 = 0;
    let mut seen_digit = false;
    while i < b.len() && b[i].is_ascii_digit() {
        mantissa = mantissa.checked_mul(10)?.checked_add((b[i] - b'0') as u64)?;
        seen_digit = true;
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            mantissa = mantissa.checked_mul(10)?.checked_add((b[i] - b'0') as u64)?;
            scale -= 1;
            seen_digit = true;
            i += 1;
        }
    }
    if !seen_digit {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        let neg = if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            let n = b[i] == b'-';
            i += 1;
            n
        } else {
            false
        };
        let mut exp: i32 = 0;
        if i >= b.len() {
            return None;
        }
        while i < b.len() && b[i].is_ascii_digit() {
            exp = exp.checked_mul(10)?.checked_add((b[i] - b'0') as i32)?;
            i += 1;
        }
        scale += if neg { -exp } else { exp };
    }
    if i != b.len() {
        return None;
    }
    // Divide by an exact power of ten rather than multiplying by its reciprocal: `1/1000`
    // is not representable, so `2675 * 0.001` is not `2.675`, while `2675.0 / 1000.0` is
    // correctly rounded. Same reasoning as scaling `parse_duration`'s fractions as
    // integers — do the exact operation, not the convenient one.
    Some(if scale < 0 {
        mantissa as f64 / pow10(-scale)
    } else {
        mantissa as f64 * pow10(scale)
    })
}

fn pow10(mut e: i32) -> f64 {
    let neg = e < 0;
    if neg {
        e = -e;
    }
    let mut r = 1.0f64;
    let mut base = 10.0f64;
    while e > 0 {
        if e & 1 == 1 {
            r *= base;
        }
        base *= base;
        e >>= 1;
    }
    if neg { 1.0 / r } else { r }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn toks(src: &str) -> Vec<Tok> {
        Lexer::tokenize_expr(src).expect("lexes")
    }

    #[test]
    fn numeric_literals_follow_8e() {
        assert_eq!(toks("1_000_000"), vec![Tok::Int(1_000_000), Tok::Eof]);
        assert_eq!(toks("0xff"), vec![Tok::Int(255), Tok::Eof]);
        assert_eq!(toks("0b1010"), vec![Tok::Int(10), Tok::Eof]);
        // No leading-zero octal: §8e rejects the C footgun explicitly.
        assert_eq!(toks("010"), vec![Tok::Int(10), Tok::Eof]);
        assert_eq!(toks("1.5"), vec![Tok::Float(1.5), Tok::Eof]);
        assert_eq!(toks("1e3"), vec![Tok::Float(1000.0), Tok::Eof]);
        assert_eq!(toks("1.5e-2"), vec![Tok::Float(0.015), Tok::Eof]);
    }

    /// `1..5` must be a range, not `1.` followed by `.5`. A float needs a digit after
    /// the dot, which is what keeps `x.field` safe too.
    #[test]
    fn a_dot_only_starts_a_float_before_a_digit() {
        assert_eq!(toks("1..5"), vec![Tok::Int(1), Tok::DotDot, Tok::Int(5), Tok::Eof]);
        assert_eq!(
            toks("x.size"),
            vec![Tok::Ident("x".into()), Tok::Dot, Tok::Ident("size".into()), Tok::Eof]
        );
    }

    #[test]
    fn longest_match_wins_on_operators() {
        assert_eq!(toks("..="), vec![Tok::DotDotEq, Tok::Eof]);
        assert_eq!(toks(".."), vec![Tok::DotDot, Tok::Eof]);
        assert_eq!(toks("??"), vec![Tok::QuestionQuestion, Tok::Eof]);
        assert_eq!(toks("?."), vec![Tok::QuestionDot, Tok::Eof]);
        assert_eq!(toks("?"), vec![Tok::Question, Tok::Eof]);
        assert_eq!(toks("++"), vec![Tok::PlusPlus, Tok::Eof]);
        assert_eq!(toks("->"), vec![Tok::Arrow, Tok::Eof]);
        assert_eq!(toks("=>"), vec![Tok::FatArrow, Tok::Eof]);
        assert_eq!(toks("||"), vec![Tok::OrOr, Tok::Eof]);
    }

    // --- D3: paths and regexes both begin with `/` --------------------------

    #[test]
    fn a_leading_slash_is_a_path_not_division() {
        assert_eq!(toks("/system"), vec![Tok::Word("/system".into()), Tok::Eof]);
        assert_eq!(toks("./data.csv"), vec![Tok::Word("./data.csv".into()), Tok::Eof]);
        assert_eq!(toks("../x"), vec![Tok::Word("../x".into()), Tok::Eof]);
        // …but a `/` with a left operand is still division.
        assert_eq!(
            toks("a / b"),
            vec![Tok::Ident("a".into()), Tok::Slash, Tok::Ident("b".into()), Tok::Eof]
        );
    }

    /// The rule that makes D3 decidable: a regex is lexed only after `~=`.
    #[test]
    fn a_regex_literal_needs_the_match_operator_before_it() {
        assert_eq!(
            toks("name ~= /\\.rs$/"),
            vec![
                Tok::Ident("name".into()),
                Tok::Match_,
                Tok::Regex("\\.rs$".into()),
                Tok::Eof
            ]
        );
        // Without `~=` the same text is a path — which is the whole point of the rule.
        assert!(matches!(toks("/x/")[0], Tok::Word(_)));
    }

    // --- D1: the two modes --------------------------------------------------

    #[test]
    fn word_mode_reads_arguments_that_expression_mode_would_mangle() {
        let mut lx = Lexer::new("README.md --long -rf");
        assert_eq!(lx.bump(Mode::Word).unwrap().tok, Tok::Word("README.md".into()));
        assert_eq!(lx.bump(Mode::Word).unwrap().tok, Tok::Flag("long".into()));
        assert_eq!(lx.bump(Mode::Word).unwrap().tok, Tok::ShortFlags("rf".into()));
        assert_eq!(lx.bump(Mode::Word).unwrap().tok, Tok::Eof);
    }

    /// The same source, read in the other mode, is a different token stream. This is the
    /// concrete statement of D1: `README.md` is a filename or a field access, and only
    /// the parser knows which.
    #[test]
    fn the_same_text_lexes_differently_per_mode() {
        assert_eq!(
            toks("README.md"),
            vec![
                Tok::Ident("README".into()),
                Tok::Dot,
                Tok::Ident("md".into()),
                Tok::Eof
            ]
        );
        let mut lx = Lexer::new("README.md");
        assert_eq!(lx.bump(Mode::Word).unwrap().tok, Tok::Word("README.md".into()));
    }

    /// Peeking in one mode must not commit the token: the parser decides the mode only
    /// after it has seen the head, so a re-read in the other mode has to be exact.
    #[test]
    fn a_peeked_token_is_relexed_when_the_mode_changes() {
        let mut lx = Lexer::new("size > 1000");
        assert_eq!(lx.peek(Mode::Word).unwrap().tok, Tok::Word("size".into()));
        assert_eq!(lx.peek(Mode::Expr).unwrap().tok, Tok::Ident("size".into()));
        // …and consuming it now consumes exactly one token, not two.
        assert_eq!(lx.bump(Mode::Expr).unwrap().tok, Tok::Ident("size".into()));
        assert_eq!(lx.bump(Mode::Expr).unwrap().tok, Tok::Gt);
        assert_eq!(lx.bump(Mode::Expr).unwrap().tok, Tok::Int(1000));
    }

    /// A bareword swallows operator characters. `a+b` is one argument, not three tokens.
    #[test]
    fn word_mode_does_not_split_on_operators() {
        let mut lx = Lexer::new("a+b*c");
        assert_eq!(lx.bump(Mode::Word).unwrap().tok, Tok::Word("a+b*c".into()));
    }

    /// …but structure that could *end* the argument list stays visible, or `list x | wc`
    /// would swallow its own pipe.
    #[test]
    fn word_mode_still_sees_the_pipe() {
        let mut lx = Lexer::new("x | wc");
        assert_eq!(lx.bump(Mode::Word).unwrap().tok, Tok::Word("x".into()));
        assert_eq!(lx.bump(Mode::Word).unwrap().tok, Tok::Pipe);
    }

    // --- D2: newlines -------------------------------------------------------

    #[test]
    fn a_newline_ends_a_statement() {
        assert_eq!(
            toks("a\nb"),
            vec![Tok::Ident("a".into()), Tok::Newline, Tok::Ident("b".into()), Tok::Eof]
        );
    }

    #[test]
    fn a_trailing_operator_continues_the_line() {
        assert_eq!(
            toks("a |\nb"),
            vec![Tok::Ident("a".into()), Tok::Pipe, Tok::Ident("b".into()), Tok::Eof]
        );
        assert_eq!(
            toks("a +\nb"),
            vec![Tok::Ident("a".into()), Tok::Plus, Tok::Ident("b".into()), Tok::Eof]
        );
    }

    /// §11b asserts that a whole script parses leading-`|` style unambiguously without
    /// saying why. This is why: the lexer looks past the newline for a `|`.
    #[test]
    fn a_leading_pipe_on_the_next_line_continues_the_previous_one() {
        assert_eq!(
            toks("ls\n  | filter"),
            vec![
                Tok::Ident("ls".into()),
                Tok::Pipe,
                Tok::Ident("filter".into()),
                Tok::Eof
            ]
        );
        // Even across a comment and a blank line.
        assert_eq!(
            toks("ls\n  # why\n\n  | filter"),
            vec![
                Tok::Ident("ls".into()),
                Tok::Pipe,
                Tok::Ident("filter".into()),
                Tok::Eof
            ]
        );
    }

    #[test]
    fn else_and_catch_may_start_a_line() {
        let t = toks("}\nelse");
        assert_eq!(t, vec![Tok::RBrace, Tok::Else, Tok::Eof]);
        let t = toks("}\ncatch");
        assert_eq!(t, vec![Tok::RBrace, Tok::Catch, Tok::Eof]);
        // But an identifier that merely *starts* with `else` is not the keyword.
        assert_eq!(
            toks("}\nelsewhere"),
            vec![Tok::RBrace, Tok::Newline, Tok::Ident("elsewhere".into()), Tok::Eof]
        );
    }

    #[test]
    fn newlines_are_whitespace_inside_parens_and_brackets_but_not_braces() {
        assert_eq!(
            toks("(a\nb)"),
            vec![
                Tok::LParen,
                Tok::Ident("a".into()),
                Tok::Ident("b".into()),
                Tok::RParen,
                Tok::Eof
            ]
        );
        // A block's statements need their separators.
        assert_eq!(
            toks("{a\nb}"),
            vec![
                Tok::LBrace,
                Tok::Ident("a".into()),
                Tok::Newline,
                Tok::Ident("b".into()),
                Tok::RBrace,
                Tok::Eof
            ]
        );
    }

    /// A multi-line record literal works without a brace rule, because the newline
    /// follows a comma.
    #[test]
    fn a_trailing_comma_carries_a_record_across_lines() {
        assert_eq!(
            toks("{ a: 1,\n  b: 2 }"),
            vec![
                Tok::LBrace,
                Tok::Ident("a".into()),
                Tok::Colon,
                Tok::Int(1),
                Tok::Comma,
                Tok::Ident("b".into()),
                Tok::Colon,
                Tok::Int(2),
                Tok::RBrace,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn comments_and_leading_blank_lines_produce_no_tokens() {
        assert_eq!(toks("# just a comment"), vec![Tok::Eof]);
        assert_eq!(toks("\n\n\na"), vec![Tok::Ident("a".into()), Tok::Eof]);
    }

    // --- strings ------------------------------------------------------------

    #[test]
    fn strings_escape_but_never_interpolate() {
        assert_eq!(toks(r#""a\nb""#), vec![Tok::Str("a\nb".into()), Tok::Eof]);
        // §8d: no interpolation, so braces inside a string are just characters.
        assert_eq!(
            toks(r#""hello {}""#),
            vec![Tok::Str("hello {}".into()), Tok::Eof]
        );
    }

    #[test]
    fn unterminated_literals_are_errors_not_silent_truncation() {
        assert!(Lexer::tokenize_expr("\"abc").is_err());
        assert!(Lexer::tokenize_expr("x ~= /abc").is_err());
        assert!(Lexer::tokenize_expr(r#""a\qb""#).is_err());
    }

    #[test]
    fn an_integer_that_does_not_fit_is_an_error() {
        assert!(Lexer::tokenize_expr("99999999999999999999").is_err());
        assert!(Lexer::tokenize_expr("0x").is_err());
    }

    /// §11d writes `$last`, so it has to lex. Part F bound it and nothing could name it.
    #[test]
    fn a_dollar_may_begin_an_identifier() {
        assert_eq!(toks("$last"), vec![Tok::Ident("$last".into()), Tok::Eof]);
        assert_eq!(
            toks("$env.PWD"),
            vec![Tok::Ident("$env".into()), Tok::Dot, Tok::Ident("PWD".into()), Tok::Eof]
        );
        // …but only at the start: `a$b` is not one name.
        assert_eq!(
            toks("a$b"),
            vec![Tok::Ident("a".into()), Tok::Ident("$b".into()), Tok::Eof]
        );
    }

    #[test]
    fn keywords_are_distinguished_from_identifiers() {
        assert_eq!(toks("let"), vec![Tok::Let, Tok::Eof]);
        assert_eq!(toks("letter"), vec![Tok::Ident("letter".into()), Tok::Eof]);
        assert_eq!(toks("_"), vec![Tok::Underscore, Tok::Eof]);
    }

    /// Floats are scaled from an integer mantissa, so a literal is the nearest `f64` to
    /// what was written rather than an accumulation of rounding steps.
    #[test]
    fn float_literals_are_exact_to_the_written_value() {
        assert_eq!(toks("0.1"), vec![Tok::Float(0.1), Tok::Eof]);
        assert_eq!(toks("2.675"), vec![Tok::Float(2.675), Tok::Eof]);
        assert_eq!(toks("1_000.5"), vec![Tok::Float(1000.5), Tok::Eof]);
    }

    /// **A token that consumes nothing is a hang, not a wrong answer.**
    ///
    /// `]`, `)` and `}` are structural in word mode; their openers were not, so
    /// `scan_bareword` at `[` scanned zero characters and left `pos` where it was — and
    /// the caller bumped that empty `Word` forever. `list [x]` locked the shell up, on a
    /// path that had nothing to do with the feature that finally surfaced it.
    #[test]
    fn a_bareword_that_scans_nothing_is_an_error_not_an_empty_token() {
        let mut lx = Lexer::new("[x]");
        assert!(lx.bump(Mode::Word).is_err(), "an empty word means no progress");
        // The characters that *do* start a word still do.
        let mut lx = Lexer::new("file[1].txt");
        assert_eq!(lx.bump(Mode::Word).unwrap().tok, Tok::Word("file".into()));
    }
}
