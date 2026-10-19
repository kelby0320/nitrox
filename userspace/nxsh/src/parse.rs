//! The `nxsh` parser — §8/§9 of the design, recursive descent.
//!
//! ## D1 — the argument grammar follows the head's spelling
//!
//! No hoisting pre-pass is needed to resolve a command head, because of §5b: a `def` is
//! called with the parens/named-argument convention, never with barewords. So a head
//! followed by `(` is a parenthesised call with expression arguments; a head in the
//! static operator or builtin set takes expression arguments in bareword form; anything
//! else is an external program with word-mode arguments. A bare `def` call in a pipeline
//! (`ls | summarize`) has no arguments, so the mode never comes up. **The head's spelling
//! and whether a `(` follows it** decide, in one token of lookahead, with no scope tracking.
//!
//! ## D9 — a bare identifier at the head of a statement is a command
//!
//! In expression position `x` is a variable; at the head of a statement or pipeline stage
//! `ls` has to be a command, and nothing distinguishes the two syntactically. Head position
//! is parsed as a call, always, and *resolution* is left to the evaluator's D4 order
//! (keyword → builtin → operator → `def` → external), which can also find a local binding.
//! A bare head with no arguments is mode-independent, so nothing was lexed wrongly.
//!
//! ## `??` has no tier in §8a
//!
//! §9e adds `??` without placing it. It sits between `&&` and `||`, both short-circuit
//! fallbacks, and binds tighter, so `a ?? b || c` is `(a ?? b) || c`. They rarely mix
//! (`??` takes a nullable, `||` a boolean), so the tier is a tie-break.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::ast::*;
use crate::exec::is_path_head;
use crate::lex::{LexError, Lexer, Mode, Spanned, Tok};

/// Generic value operators (§3, §5c, §10b). Closed at parse time because it selects the
/// argument grammar; a user-defined generic `def` is called with parens, so it is
/// syntactically distinct.
///
/// The bitwise family (`band`/`bor`/`bxor`/`shl`/`shr`) would join this table: `|`, `&` and
/// `^` are all spoken for.
/// TODO(shell-bitwise): `docs/rationale/deferred-decisions.md`.
pub(crate) const OPERATORS: &[&str] = &[
    "filter", "sort", "select", "save", "open", "each", "map", "display", "format", "last", "skip", "dedupe", "take",
    "count", "sum", "min", "max", "avg", "reduce", // §10b's Part E families: strings, records, numbers.
    "split", "join", "trim", "replace", "upper", "lower", "keys", "values", "merge", "round", "floor", "ceil", "trunc",
    "abs", "capture",
];

/// Shell-state builtins (§3): they mutate the shell's own process state, which an
/// external program structurally cannot do.
pub(crate) const BUILTINS: &[&str] = &["cd", "exit"];

/// Operators whose bareword argument is a *predicate*, and therefore desugars to an
/// implicit `{ |it| … }` closure (§8b).
const PREDICATE_OPERATORS: &[&str] = &["filter", "each", "map", "reduce"];

/// Operators whose argument may open with `{`.
///
/// A predicate operator takes a **closure**, `merge` a **record**; `brace_expr` tells them
/// apart by the `|` (§8c), so this only grants reading a `{` as an argument rather than as
/// an enclosing block.
const BRACE_ARG_OPERATORS: &[&str] = &["filter", "each", "map", "reduce", "merge"];

/// Operators that take **no** arguments at all.
///
/// `sum`/`min`/`max`/`avg` are not here: they take an optional column, so `sum size` must
/// keep reading `size`. Knowing this at parse time lets `(count > 0)` read as a comparison
/// rather than `count` applied to `> 0`, while a paren can still open a pipeline whose
/// first stage is a command.
const NULLARY_OPERATORS: &[&str] =
    &["count", "dedupe", "trim", "upper", "lower", "keys", "values", "round", "floor", "ceil", "trunc", "abs"];

/// Deepest nesting the parser will descend (D5): recursion on user input must end in an
/// error, not an overflow of the fixed userspace stack.
pub const MAX_DEPTH: u32 = 64;

/// A parse failure, with enough position to point at the source.
#[derive(Clone, PartialEq, Debug)]
pub struct ParseError {
    pub message: &'static str,
    pub start: usize,
    pub line: u32,
}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> ParseError {
        ParseError { message: e.message, start: e.start, line: e.line }
    }
}

type Result<T> = core::result::Result<T, ParseError>;

/// Parse a whole script.
pub fn parse_script(src: &str) -> Result<Script> {
    Parser::new(src).script()
}

pub struct Parser<'a> {
    lx: Lexer<'a>,
    depth: u32,
    /// Whether the next identifier parsed may be a *command head* rather than a variable.
    ///
    /// True only at the start of a statement or pipeline stage, and cleared once a head is read, so
    /// nothing inside an argument list is taken for a command: `sort size --reverse` would
    /// otherwise parse as `sort` applied to the command `size --reverse`.
    head_ok: bool,
    /// Nesting depth inside a generic operator's bareword argument list.
    ///
    /// Inside one, a `-` with a space before and none after is a **flag**, not a subtraction (`sort
    /// size --reverse`). Scoped here because elsewhere arithmetic is the commoner reading (`let y =
    /// x -1`).
    in_op_args: u32,
    /// How many `for`/`while` bodies enclose the statement being parsed, **within the current
    /// function**.
    ///
    /// `break`/`continue` are legal only where this is non-zero. Loop-only is the counter; "does
    /// not cross a function boundary" is the reset: a `def` or closure body parses with zero, so a
    /// `break` inside `filter { |it| … }` cannot see a loop outside it. Doing it here makes both
    /// diagnostics parse-time.
    loop_depth: u32,
    probe: u64,
    /// The directory stamped on every `use`; see [`crate::paths::parse_script_in`].
    pub(crate) base: Option<String>,
}

impl<'a> Parser<'a> {
    pub fn new(src: &'a str) -> Parser<'a> {
        Parser { lx: Lexer::new(src), depth: 0, head_ok: true, in_op_args: 0, loop_depth: 0, probe: 0, base: None }
    }

    /// Parse a loop body: `break`/`continue` are legal inside it.
    fn loop_body(&mut self) -> Result<Vec<Stmt>> {
        self.loop_depth += 1;
        let body = self.block();
        self.loop_depth -= 1;
        body
    }

    /// Parse a function or closure body: enclosing loops are **not** visible from inside
    /// it, so `break` there is an error rather than a jump across the boundary.
    fn function_body(&mut self, f: impl FnOnce(&mut Self) -> Result<Vec<Stmt>>) -> Result<Vec<Stmt>> {
        let saved = core::mem::replace(&mut self.loop_depth, 0);
        let body = f(self);
        self.loop_depth = saved;
        body
    }

    fn peek(&mut self) -> Result<Tok> {
        self.probe += 1;
        assert!(self.probe < 200_000, "parser spin");
        Ok(self.lx.peek(Mode::Expr)?.tok)
    }

    fn peek_spanned(&mut self) -> Result<Spanned> {
        Ok(self.lx.peek(Mode::Expr)?)
    }

    fn bump(&mut self) -> Result<Tok> {
        Ok(self.lx.bump(Mode::Expr)?.tok)
    }

    fn eat(&mut self, t: &Tok) -> Result<bool> {
        if &self.peek()? == t {
            self.bump()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn expect(&mut self, t: &Tok, message: &'static str) -> Result<()> {
        if self.eat(t)? {
            return Ok(());
        }
        self.fail(message)
    }

    fn fail<T>(&mut self, message: &'static str) -> Result<T> {
        let s = self.peek_spanned()?;
        Err(ParseError { message, start: s.start, line: s.line })
    }

    /// Skip statement separators. Blank lines are never significant.
    fn skip_newlines(&mut self) -> Result<()> {
        while self.peek()? == Tok::Newline {
            self.bump()?;
        }
        Ok(())
    }

    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return self.fail("expression nests too deeply");
        }
        Ok(())
    }

    fn exit(&mut self) {
        self.depth -= 1;
    }

    fn ident(&mut self, message: &'static str) -> Result<String> {
        match self.bump()? {
            Tok::Ident(s) => Ok(s),
            // Keywords that are also ordinary field/parameter names in practice.
            Tok::Match => Ok(String::from("match")),
            _ => self.fail(message),
        }
    }

    pub fn script(&mut self) -> Result<Script> {
        let mut stmts = Vec::new();
        loop {
            self.skip_newlines()?;
            if self.peek()? == Tok::Eof {
                return Ok(Script { stmts });
            }
            stmts.push(self.statement()?);
            // A statement must be followed by a separator, a closer, or the end.
            match self.peek()? {
                Tok::Newline | Tok::Eof => {}
                Tok::RBrace => {}
                _ => return self.fail("expected a newline between statements"),
            }
        }
    }

    fn block(&mut self) -> Result<Vec<Stmt>> {
        self.enter()?;
        self.expect(&Tok::LBrace, "expected `{`")?;
        let mut stmts = Vec::new();
        loop {
            self.skip_newlines()?;
            if self.eat(&Tok::RBrace)? {
                self.exit();
                return Ok(stmts);
            }
            if self.peek()? == Tok::Eof {
                return self.fail("unclosed block");
            }
            stmts.push(self.statement()?);
            match self.peek()? {
                Tok::Newline | Tok::RBrace => {}
                _ => return self.fail("expected a newline between statements"),
            }
        }
    }

    fn statement(&mut self) -> Result<Stmt> {
        self.enter()?;
        self.head_ok = true;
        let s = self.statement_inner();
        self.exit();
        s
    }

    fn statement_inner(&mut self) -> Result<Stmt> {
        match self.peek()? {
            Tok::Let => self.binding(BindKind::Let, false),
            Tok::Mut => self.binding(BindKind::Mut, false),
            Tok::Const => self.binding(BindKind::Const, false),
            Tok::Pub => {
                self.bump()?;
                match self.peek()? {
                    Tok::Const => self.binding(BindKind::Const, true),
                    Tok::Def => self.def_stmt(true),
                    _ => self.fail("`pub` may only precede `const` or `def`"),
                }
            }
            Tok::Def => self.def_stmt(false),
            Tok::If => {
                let (cond, then, otherwise) = self.if_parts()?;
                Ok(Stmt::If { cond, then, otherwise })
            }
            Tok::For => {
                self.bump()?;
                let binding = self.ident("expected a loop variable")?;
                self.expect(&Tok::In, "expected `in`")?;
                // §9b restricts the iterable so a bare `|` cannot appear at its top level, else `{
                // … }` is ambiguous between the loop body and a trailing closure. It names
                // `or_expr`, which also excludes ranges, and `for i in 0..5` is the commonest loop;
                // so the iterable is parsed at `range_expr`. A piped iterable still needs its
                // parens.
                let iterable = self.range_expr()?;
                let body = self.loop_body()?;
                Ok(Stmt::For { binding, iterable, body })
            }
            Tok::While => {
                self.bump()?;
                let cond = self.or_expr()?;
                let body = self.loop_body()?;
                Ok(Stmt::While { cond, body })
            }
            Tok::Fail => {
                self.bump()?;
                Ok(Stmt::Fail(self.expr()?))
            }
            Tok::Break | Tok::Continue => {
                let kw = self.bump()?;
                if self.loop_depth == 0 {
                    let name = if kw == Tok::Break { "break" } else { "continue" };
                    return self.fail(if name == "break" {
                        "`break` is only meaningful inside a `for` or `while` body — and \
                         it does not reach a loop outside the enclosing `def` or closure"
                    } else {
                        "`continue` is only meaningful inside a `for` or `while` body — \
                         and it does not reach a loop outside the enclosing `def` or \
                         closure"
                    });
                }
                Ok(if kw == Tok::Break { Stmt::Break } else { Stmt::Continue })
            }
            // `try` is an expression (§9c), so statement position is just an expression statement:
            // one node and one evaluator path.
            Tok::Try => Ok(Stmt::Expr(self.try_expr()?)),
            Tok::Strict => {
                self.bump()?;
                Ok(Stmt::Strict(self.block()?))
            }
            Tok::Return => {
                self.bump()?;
                let v = match self.peek()? {
                    Tok::Newline | Tok::RBrace | Tok::Eof => None,
                    _ => Some(self.expr()?),
                };
                Ok(Stmt::Return(v))
            }
            Tok::Use => self.use_stmt(),
            _ => {
                let e = self.expr()?;
                if self.peek()? == Tok::Eq {
                    self.bump()?;
                    let target = self.lvalue(e)?;
                    let value = self.expr()?;
                    return Ok(Stmt::Assign { target, value });
                }
                Ok(Stmt::Expr(e))
            }
        }
    }

    /// Reinterpret an already-parsed expression as an assignment target (§9d).
    /// `lvalue := IDENT ("." IDENT | "[" expr "]")*`.
    fn lvalue(&mut self, e: Expr) -> Result<LValue> {
        let mut path = Vec::new();
        let mut cur = e;
        loop {
            match cur {
                Expr::Ident(root) => {
                    path.reverse();
                    return Ok(LValue { root, path });
                }
                Expr::Field(inner, name) => {
                    path.push(LValueStep::Field(name));
                    cur = *inner;
                }
                Expr::Index(inner, idx) => {
                    path.push(LValueStep::Index(*idx));
                    cur = *inner;
                }
                _ => return self.fail("this is not something that can be assigned to"),
            }
        }
    }

    fn binding(&mut self, kind: BindKind, public: bool) -> Result<Stmt> {
        self.bump()?; // let / mut / const
        let name = self.ident("expected a binding name")?;
        let ty = if self.eat(&Tok::Colon)? { Some(self.type_expr()?) } else { None };
        self.expect(&Tok::Eq, "expected `=` in a binding")?;
        // A path here is a value; only a name is a command.
        if matches!(self.peek()?, Tok::Word(_)) {
            self.head_ok = false;
        }
        let value = self.expr()?;
        Ok(Stmt::Bind { kind, name, ty, value, public })
    }

    /// `try { … } catch (e) { … }` (§9c). One production, reached from statement position and from
    /// `primary`, so `let x = try { … } catch { … }` needs no second implementation.
    fn try_expr(&mut self) -> Result<Expr> {
        self.bump()?; // try
        let body = self.block()?;
        self.expect(&Tok::Catch, "expected `catch` after a `try` block")?;
        let catch_binding = if self.eat(&Tok::LParen)? {
            let n = self.ident("expected an error binding")?;
            self.expect(&Tok::RParen, "expected `)`")?;
            Some(n)
        } else {
            None
        };
        let catch_body = self.block()?;
        Ok(Expr::Try { body, catch_binding, catch_body })
    }

    fn def_stmt(&mut self, public: bool) -> Result<Stmt> {
        self.bump()?; // def
        let name = self.ident("expected a function name")?;
        self.expect(&Tok::LParen, "expected `(` after a function name")?;
        let params = self.param_list()?;
        let ret = if self.eat(&Tok::Arrow)? { Some(self.type_expr()?) } else { None };
        let body = self.function_body(|p| p.block())?;
        Ok(Stmt::Def { name, params, ret, body, public })
    }

    fn param_list(&mut self) -> Result<Vec<Param>> {
        let mut params = Vec::new();
        if self.eat(&Tok::RParen)? {
            return Ok(params);
        }
        loop {
            let variadic = self.eat(&Tok::Ellipsis)?;
            let name = self.ident("expected a parameter name")?;
            let ty = if self.eat(&Tok::Colon)? { Some(self.type_expr()?) } else { None };
            let default = if self.eat(&Tok::Eq)? { Some(self.expr()?) } else { None };
            params.push(Param { name, ty, default, variadic });
            if self.eat(&Tok::Comma)? {
                if self.eat(&Tok::RParen)? {
                    return Ok(params); // trailing comma
                }
                continue;
            }
            self.expect(&Tok::RParen, "expected `,` or `)` in a parameter list")?;
            return Ok(params);
        }
    }

    /// `use "./lib/utils.nx" { helper, other }` / `… as utils` (§9h). A bare `use "path"`
    /// with no selector is deliberately illegal: name what is imported, or bind the whole
    /// module.
    fn use_stmt(&mut self) -> Result<Stmt> {
        self.bump()?; // use
        let path = match self.bump()? {
            Tok::Str(s) => s,
            _ => return self.fail("`use` takes a quoted path"),
        };
        if self.eat(&Tok::As)? {
            let alias = self.ident("expected a module alias")?;
            return Ok(Stmt::Use { path, names: None, alias: Some(alias), base: self.base.clone() });
        }
        if self.eat(&Tok::LBrace)? {
            let mut names = Vec::new();
            loop {
                self.skip_newlines()?;
                if self.eat(&Tok::RBrace)? {
                    break;
                }
                names.push(self.ident("expected an imported name")?);
                self.skip_newlines()?;
                if self.eat(&Tok::Comma)? {
                    continue;
                }
                self.skip_newlines()?;
                self.expect(&Tok::RBrace, "expected `,` or `}` in an import list")?;
                break;
            }
            if names.is_empty() {
                return self.fail("an import list may not be empty");
            }
            return Ok(Stmt::Use { path, names: Some(names), alias: None, base: self.base.clone() });
        }
        self.fail("`use` needs `{ names }` or `as alias` — there is no wildcard import")
    }

    fn if_parts(&mut self) -> Result<(Expr, Vec<Stmt>, Option<Vec<Stmt>>)> {
        self.bump()?; // if
        // §9b: the condition is `or_expr`, so `if (files | count) > 0` needs its parens
        // and `if ls | count > 0 { … }` cannot be misread as a trailing closure.
        let cond = self.or_expr()?;
        let then = self.block()?;
        let otherwise = if self.eat(&Tok::Else)? {
            if self.peek()? == Tok::If {
                let (c, t, o) = self.if_parts()?;
                Some(alloc::vec![Stmt::If { cond: c, then: t, otherwise: o }])
            } else {
                Some(self.block()?)
            }
        } else {
            None
        };
        Ok((cond, then, otherwise))
    }

    fn expr(&mut self) -> Result<Expr> {
        self.enter()?;
        let e = self.pipeline();
        self.exit();
        e
    }

    fn pipeline(&mut self) -> Result<Expr> {
        let first = self.range_expr()?;
        if self.peek()? != Tok::Pipe {
            return Ok(first);
        }
        let mut stages = alloc::vec![first];
        while self.eat(&Tok::Pipe)? {
            stages.push(self.stage()?);
        }
        Ok(Expr::Pipeline(stages))
    }

    /// One pipeline stage: a command call, or a closure literal (§8c).
    fn stage(&mut self) -> Result<Expr> {
        if self.peek()? == Tok::LBrace {
            return self.brace_expr();
        }
        self.head_ok = true;
        self.range_expr()
    }

    fn range_expr(&mut self) -> Result<Expr> {
        let lhs = self.or_expr()?;
        let inclusive = match self.peek()? {
            Tok::DotDot => false,
            Tok::DotDotEq => true,
            _ => return Ok(lhs),
        };
        self.bump()?;
        let rhs = self.or_expr()?;
        Ok(Expr::Range { start: Box::new(lhs), end: Box::new(rhs), inclusive })
    }

    fn or_expr(&mut self) -> Result<Expr> {
        self.enter()?;
        let r = self.or_expr_inner();
        self.exit();
        r
    }

    fn or_expr_inner(&mut self) -> Result<Expr> {
        let mut lhs = self.coalesce_expr()?;
        while self.eat(&Tok::OrOr)? {
            let rhs = self.coalesce_expr()?;
            lhs = Expr::Binary(BinOp::Or, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// `??`, between `&&` and `||`. See the module docs — §8a predates it.
    fn coalesce_expr(&mut self) -> Result<Expr> {
        let mut lhs = self.and_expr()?;
        while self.eat(&Tok::QuestionQuestion)? {
            let rhs = self.and_expr()?;
            lhs = Expr::Binary(BinOp::Coalesce, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn and_expr(&mut self) -> Result<Expr> {
        let mut lhs = self.eq_expr()?;
        while self.eat(&Tok::AndAnd)? {
            let rhs = self.eq_expr()?;
            lhs = Expr::Binary(BinOp::And, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn eq_expr(&mut self) -> Result<Expr> {
        let mut lhs = self.cmp_expr()?;
        loop {
            let op = match self.peek()? {
                Tok::EqEq => BinOp::Eq,
                Tok::Ne => BinOp::Ne,
                Tok::Match_ => BinOp::Match,
                _ => return Ok(lhs),
            };
            self.bump()?;
            let rhs = self.cmp_expr()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
    }

    fn cmp_expr(&mut self) -> Result<Expr> {
        let mut lhs = self.add_expr()?;
        loop {
            let op = match self.peek()? {
                Tok::Lt => BinOp::Lt,
                Tok::Le => BinOp::Le,
                Tok::Gt => BinOp::Gt,
                Tok::Ge => BinOp::Ge,
                // `for x in xs` consumes its own `in` inside the `for` rule, before any
                // expression is parsed — so the two never compete for the token (§8c).
                Tok::In => BinOp::In,
                _ => return Ok(lhs),
            };
            self.bump()?;
            // **`in` takes its right operand a tier lower than the others.** §8a puts
            // ranges *below* comparison, so `5 in 0..10` would otherwise parse as
            // `(5 in 0) .. 10` — the one spelling membership over a range obviously wants.
            // Nothing else is affected: a range is not a meaningful operand of `<`.
            let rhs = if op == BinOp::In { self.range_expr()? } else { self.add_expr()? };
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
    }

    fn add_expr(&mut self) -> Result<Expr> {
        let mut lhs = self.mul_expr()?;
        loop {
            if self.in_op_args > 0 && self.looks_like_flag()? {
                return Ok(lhs);
            }
            let op = match self.peek()? {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                Tok::PlusPlus => BinOp::Concat,
                _ => return Ok(lhs),
            };
            self.bump()?;
            let rhs = self.mul_expr()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
    }

    fn mul_expr(&mut self) -> Result<Expr> {
        let mut lhs = self.unary()?;
        loop {
            let op = match self.peek()? {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Rem,
                _ => return Ok(lhs),
            };
            self.bump()?;
            let rhs = self.unary()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
    }

    fn unary(&mut self) -> Result<Expr> {
        let op = match self.peek()? {
            Tok::Minus => UnOp::Neg,
            Tok::Bang => UnOp::Not,
            _ => return self.postfix(),
        };
        self.bump()?;
        self.enter()?;
        let e = self.unary();
        self.exit();
        Ok(Expr::Unary(op, Box::new(e?)))
    }

    fn postfix(&mut self) -> Result<Expr> {
        let mut e = self.primary()?;
        loop {
            e = match self.peek()? {
                Tok::Dot => {
                    self.bump()?;
                    Expr::Field(Box::new(e), self.ident("expected a field name")?)
                }
                Tok::QuestionDot => {
                    self.bump()?;
                    Expr::TryField(Box::new(e), self.ident("expected a field name")?)
                }
                Tok::LBracket => {
                    self.bump()?;
                    let idx = self.expr()?;
                    self.expect(&Tok::RBracket, "expected `]`")?;
                    Expr::Index(Box::new(e), Box::new(idx))
                }
                // `utils.helper(…)` — a call on a **qualified name** (§9h's `use … as`). The alias
                // binds `utils.helper` as one name, not a record: a module's exports are functions,
                // which are deliberately not TSM1-representable (§5c).
                Tok::LParen => {
                    let Expr::Field(base, field) = &e else { return Ok(e) };
                    let Expr::Ident(root) = base.as_ref() else { return Ok(e) };
                    let name = alloc::format!("{root}.{field}");
                    self.bump()?;
                    let args = self.paren_args()?;
                    Expr::Call(Box::new(Call { name, kind: CallKind::Def, args, forced_external: false }))
                }
                Tok::Question => {
                    self.bump()?;
                    return self.fail(
                        "`?` was retired in design v1.2 — an operation that fails already \
                         propagates to its caller, so there was nothing for it to mark. \
                         Use `try { … } catch { … }` to recover (§2)",
                    );
                }
                _ => return Ok(e),
            };
        }
    }

    fn primary(&mut self) -> Result<Expr> {
        let s = self.peek_spanned()?;
        match s.tok {
            Tok::Int(v) => {
                self.bump()?;
                Ok(Expr::Int(v))
            }
            Tok::Float(v) => {
                self.bump()?;
                Ok(Expr::Float(v))
            }
            Tok::Str(v) => {
                self.bump()?;
                Ok(Expr::Str(v))
            }
            Tok::Word(v) if self.head_ok && is_path_head(&v) => self.command_or_ident(),
            Tok::Word(v) => {
                self.bump()?;
                Ok(Expr::Word(v))
            }
            Tok::Regex(v) => {
                self.bump()?;
                Ok(Expr::Regex(v))
            }
            Tok::True => {
                self.bump()?;
                Ok(Expr::Bool(true))
            }
            Tok::False => {
                self.bump()?;
                Ok(Expr::Bool(false))
            }
            Tok::Null => {
                self.bump()?;
                Ok(Expr::Null)
            }
            Tok::Underscore => {
                self.bump()?;
                Ok(Expr::Underscore)
            }
            Tok::LParen => {
                self.bump()?;
                // §3's expression-position escape hatch. A `(` opens a fresh expression, possibly a
                // pipeline whose first stage is a command: `(list /system | filter …)`. `(count >
                // 0)` is still a comparison because `count` is nullary and swallows nothing.
                self.head_ok = true;
                let e = self.expr()?;
                self.expect(&Tok::RParen, "expected `)`")?;
                Ok(e)
            }
            Tok::LBracket => {
                self.bump()?;
                self.head_ok = false;
                let mut items = Vec::new();
                loop {
                    if self.eat(&Tok::RBracket)? {
                        return Ok(Expr::List(items));
                    }
                    items.push(self.expr()?);
                    if self.eat(&Tok::Comma)? {
                        continue;
                    }
                    self.expect(&Tok::RBracket, "expected `,` or `]` in a list")?;
                    return Ok(Expr::List(items));
                }
            }
            Tok::LBrace => self.brace_expr(),
            Tok::If => {
                let (cond, then, otherwise) = self.if_parts()?;
                Ok(Expr::If { cond: Box::new(cond), then, otherwise })
            }
            Tok::Match => self.match_expr(),
            Tok::Try => self.try_expr(),
            Tok::Expect => {
                self.bump()?;
                let t = self.type_expr()?;
                // `expect T` is ascription in pipeline position: it checks whatever
                // arrives, so it has no operand of its own here.
                Ok(Expr::Expect(Box::new(Expr::Underscore), t))
            }
            Tok::Parse => {
                self.bump()?;
                let t = self.type_expr()?;
                // Like `expect`, `parse` reads the value flowing past it, so in stage
                // position it has no operand of its own — `_` is bound to it (§8c).
                Ok(Expr::Parse(Box::new(Expr::Underscore), t))
            }
            Tok::Assert => {
                self.bump()?;
                self.expect(&Tok::LParen, "`assert` takes a parenthesised predicate")?;
                // **A command head is legal inside the predicate**, so §6's `ls | assert (count >
                // 0)` applies the operator `count` to the value flowing past. D4 still puts a local
                // binding first for a bare name, so `assert (n > 0)` over a `let n` is unaffected.
                self.head_ok = true;
                let e = self.expr()?;
                self.expect(&Tok::RParen, "expected `)`")?;
                Ok(Expr::Assert(Box::new(e)))
            }
            Tok::Caret | Tok::Ident(_) => self.command_or_ident(),
            _ => self.fail("expected an expression"),
        }
    }

    /// `{` — a closure if a `|` follows, a record literal otherwise (§8c).
    ///
    /// Making the pipes mandatory even when empty is what fixed the real ambiguity here:
    /// `{ name }` was otherwise both a record shorthand and a zero-argument closure body.
    fn brace_expr(&mut self) -> Result<Expr> {
        self.enter()?;
        let r = self.brace_expr_inner();
        self.exit();
        r
    }

    fn brace_expr_inner(&mut self) -> Result<Expr> {
        self.expect(&Tok::LBrace, "expected `{`")?;
        if self.peek()? == Tok::Pipe || self.peek()? == Tok::OrOr {
            // `||` is an empty parameter list, not logical-or, in this one position.
            let params = if self.eat(&Tok::OrOr)? {
                Vec::new()
            } else {
                self.bump()?; // the opening `|`
                let mut ps = Vec::new();
                loop {
                    if self.eat(&Tok::Pipe)? {
                        break;
                    }
                    let name = self.ident("expected a closure parameter")?;
                    let ty = if self.eat(&Tok::Colon)? { Some(self.type_expr()?) } else { None };
                    ps.push(Param { name, ty, default: None, variadic: false });
                    if self.eat(&Tok::Comma)? {
                        continue;
                    }
                    self.expect(&Tok::Pipe, "expected `,` or `|` in a parameter list")?;
                    break;
                }
                ps
            };
            let body = self.function_body(|p| {
                let mut body = Vec::new();
                loop {
                    p.skip_newlines()?;
                    if p.eat(&Tok::RBrace)? {
                        return Ok(body);
                    }
                    if p.peek()? == Tok::Eof {
                        return p.fail("unclosed closure");
                    }
                    body.push(p.statement()?);
                }
            })?;
            return Ok(Expr::Closure { params, body });
        }
        // A record literal. `{ name }` is shorthand for `{ name: name }` (§8e).
        let mut fields = Vec::new();
        loop {
            self.skip_newlines()?;
            if self.eat(&Tok::RBrace)? {
                return Ok(Expr::Record(fields));
            }
            let name = self.ident("expected a field name")?;
            let value = if self.eat(&Tok::Colon)? { self.expr()? } else { Expr::Ident(name.clone()) };
            fields.push((name, value));
            self.skip_newlines()?;
            if self.eat(&Tok::Comma)? {
                continue;
            }
            self.skip_newlines()?;
            self.expect(&Tok::RBrace, "expected `,` or `}` in a record")?;
            return Ok(Expr::Record(fields));
        }
    }

    fn match_expr(&mut self) -> Result<Expr> {
        self.bump()?; // match
        // §9f: the scrutinee is `or_expr`, for the same reason as `if`'s condition.
        let scrutinee = self.or_expr()?;
        self.expect(&Tok::LBrace, "expected `{` after a match scrutinee")?;
        let mut arms = Vec::new();
        loop {
            self.skip_newlines()?;
            if self.eat(&Tok::RBrace)? {
                if arms.is_empty() {
                    return self.fail("a match needs at least one arm");
                }
                return Ok(Expr::Match { scrutinee: Box::new(scrutinee), arms });
            }
            if self.peek()? == Tok::Eof {
                return self.fail("unclosed match");
            }
            let pattern = self.pattern()?;
            let guard = if self.eat(&Tok::If)? { Some(self.or_expr()?) } else { None };
            self.expect(&Tok::FatArrow, "expected `=>` in a match arm")?;
            let body = if self.peek()? == Tok::LBrace { self.block()? } else { alloc::vec![Stmt::Expr(self.expr()?)] };
            arms.push(MatchArm { pattern, guard, body });
        }
    }

    /// An identifier at the head of a statement or pipeline stage (D1, D9), or an
    /// ordinary variable reference elsewhere in an expression.
    fn command_or_ident(&mut self) -> Result<Expr> {
        let forced_external = self.eat(&Tok::Caret)?;
        let at_head = self.head_ok;
        self.head_ok = false;

        // A **hyphenated** name at head position is one command name, not a subtraction: programs
        // here are called `fs-server-ext4`, and reading the head in expression mode would leave `-
        // server - ext4` behind. Arithmetic at statement head is written with spaces or parens.
        if at_head || forced_external {
            let w = self.lx.peek(crate::lex::Mode::Word)?;
            if let Tok::Word(text) = &w.tok
                && (is_path_head(text)
                    || text.contains('-') && text.as_bytes().first().is_some_and(|c| c.is_ascii_alphabetic()))
            {
                let name = text.clone();
                self.lx.bump(crate::lex::Mode::Word)?;
                let args = self.word_args()?;
                return Ok(Expr::Call(Box::new(Call { name, kind: CallKind::External, args, forced_external })));
            }
        }
        let name = self.ident("expected a command or a name")?;

        // An `=` immediately after the head makes this an assignment target: operator names are not
        // reserved words, and `last = 2` must assign rather than call `last` (§10b).
        if self.peek()? == Tok::Eq {
            return Ok(Expr::Ident(name));
        }

        // **`time PIPELINE`** (§3): at a head and followed by an argument, the word times the whole
        // pipeline after it, every stage (`time list /home | count` times both). Alone, or before
        // `=`, it is a name like any other (D9).
        if name == "time" && at_head && !forced_external && self.peek()? != Tok::LParen && self.starts_an_argument()? {
            self.head_ok = true;
            let timed = self.expr()?;
            return Ok(Expr::Time(Box::new(timed)));
        }

        // A `(` immediately after the head is the parens/named-argument convention
        // (§5b) — a `def` call, or an operator written in call form.
        if self.peek()? == Tok::LParen {
            self.bump()?;
            let args = self.paren_args()?;
            let kind = if forced_external {
                CallKind::External
            } else if OPERATORS.contains(&name.as_str()) {
                CallKind::Operator
            } else {
                CallKind::Def
            };
            return Ok(Expr::Call(Box::new(Call { name, kind, args, forced_external })));
        }

        let is_operator = at_head && !forced_external && OPERATORS.contains(&name.as_str());
        let is_builtin = at_head && !forced_external && BUILTINS.contains(&name.as_str());
        if is_builtin {
            // A builtin takes a **path**, not an expression: `cd ..` and `cd /system` break
            // `operator_args` (a range, a division). Word mode is how an external gets its paths.
            let args = self.word_args()?;
            return Ok(Expr::Call(Box::new(Call { name, kind: CallKind::Builtin, args, forced_external })));
        }
        if is_operator {
            let args = self.operator_args(&name)?;
            return Ok(Expr::Call(Box::new(Call { name, kind: CallKind::Operator, args, forced_external })));
        }

        if !forced_external && (!at_head || !self.starts_an_argument()?) {
            // A variable reference or a bare command, indistinguishable here and left so: the
            // evaluator resolves it in D4's order, and a bare head has no arguments to mis-lex.
            return Ok(Expr::Ident(name));
        }
        let args = self.word_args()?;
        Ok(Expr::Call(Box::new(Call { name, kind: CallKind::External, args, forced_external })))
    }

    /// Whether the upcoming `-` introduces a flag rather than a subtraction. Only asked
    /// inside a generic operator's argument list — see [`Parser::in_op_args`].
    fn looks_like_flag(&mut self) -> Result<bool> {
        let s = self.lx.peek(Mode::Expr)?;
        if s.tok != Tok::Minus || !s.space_before {
            return Ok(false);
        }
        let w = self.lx.peek(Mode::Word)?;
        Ok(matches!(w.tok, Tok::Flag(_) | Tok::ShortFlags(_)))
    }

    /// After a command head, does the next token begin an *argument* rather than continue an
    /// expression? This is D9's decision procedure.
    ///
    /// `-` is both a binary operator and a flag prefix, read as a person reads it: `a - b` has
    /// space on both sides, `list -l` has the flag pressed against its dashes. So a `-` preceded by
    /// space and *not* followed by one introduces a flag. A postfix (`.`, `[`, `(`, `?`) pressed
    /// against the name continues an expression, as does any infix operator; `{` is a block or a
    /// record, never an argument, which lets `if loud { … }` parse under §9b.
    fn starts_an_argument(&mut self) -> Result<bool> {
        let s = self.lx.peek(Mode::Expr)?;
        Ok(match s.tok {
            Tok::Eof | Tok::Newline | Tok::Pipe | Tok::OrOr | Tok::AndAnd | Tok::Comma
            | Tok::RParen | Tok::RBrace | Tok::RBracket | Tok::LBrace | Tok::Colon
            | Tok::FatArrow | Tok::Arrow => false,
            Tok::Dot | Tok::LBracket | Tok::LParen | Tok::Question | Tok::QuestionDot
                if !s.space_before =>
            {
                false
            }
            // Every other infix operator continues an expression. The postfix set is
            // deliberately absent here: pressed against the name they were handled above,
            // and *separated* by a space they begin an argument — `list .` names the
            // current directory, and `list [x]` is a path, not an index.
            Tok::Plus | Tok::PlusPlus | Tok::Star | Tok::Percent | Tok::Lt
            | Tok::Le | Tok::Gt | Tok::Ge | Tok::EqEq | Tok::Ne | Tok::Match_ | Tok::Eq
            | Tok::QuestionQuestion | Tok::DotDot | Tok::DotDotEq
            // `in` is on this list because it is an infix comparison (§8a): `x in [1, 2]` must not
            // read as the *program* `x` with word-mode arguments.
            | Tok::In => false,
            // `/` is division except where division is impossible. `/system` lexes as one path
            // word, so a *lone* `Slash` means whitespace or a closer follows: `list /` and `list /
            // | count` have no right operand, and the only reading left is the root path. Spacing
            // matters as for `-`: `a/b` is division and nothing should turn it into an argument.
            Tok::Slash => s.space_before && self.lx.no_operand_follows(),
            Tok::Minus => {
                let flagged = self.lx.peek(Mode::Word)?;
                s.space_before && matches!(flagged.tok, Tok::Flag(_) | Tok::ShortFlags(_))
            }
            _ => true,
        })
    }

    /// `f(a, name: b, _)` — the §5b convention.
    fn paren_args(&mut self) -> Result<Vec<Arg>> {
        let mut args = Vec::new();
        self.head_ok = false;
        loop {
            self.skip_newlines()?;
            if self.eat(&Tok::RParen)? {
                return Ok(args);
            }
            if self.eat(&Tok::Underscore)? {
                args.push(Arg::PipeFill);
            } else if let Tok::Ident(n) = self.peek()? {
                // `name: value` — a named argument. Telling it from an expression needs the token
                // *after* the identifier, one more than the lexer caches: take a mark, look, and
                // rewind if there was no `:`.
                //
                // **The rewind is what makes an argument an expression like any other.** Resuming
                // in a hand-written copy of the expression tiers had no arm for a `(` (`add(2, 3)`
                // failed as an argument) and folded binary operators flat (`format("{}", a + b *
                // c)` was `(a + b) * c`).
                let mark = self.lx.clone();
                self.bump()?;
                if self.eat(&Tok::Colon)? {
                    args.push(Arg::Named(n, self.expr()?));
                } else {
                    self.lx = mark;
                    args.push(Arg::Positional(self.expr()?));
                }
            } else {
                args.push(Arg::Positional(self.expr()?));
            }
            self.skip_newlines()?;
            if self.eat(&Tok::Comma)? {
                continue;
            }
            self.skip_newlines()?;
            self.expect(&Tok::RParen, "expected `,` or `)` in an argument list")?;
            return Ok(args);
        }
    }

    /// Bareword-form arguments to a generic operator or builtin: expressions and flags, up to
    /// whatever ends the stage. §8b's sugar is applied here, not at evaluation.
    fn operator_args(&mut self, name: &str) -> Result<Vec<Arg>> {
        let mut args = Vec::new();
        if NULLARY_OPERATORS.contains(&name) {
            return Ok(args);
        }
        self.in_op_args += 1;
        loop {
            match self.peek()? {
                Tok::Pipe | Tok::Newline | Tok::Eof | Tok::RParen | Tok::RBrace | Tok::RBracket | Tok::Comma => break,
                // `=` never begins an argument, so an operator name followed by one is an
                // assignment to a binding that shares the name (`last` is both a §10b operator and
                // a reasonable variable).
                Tok::Eq => break,
                // A `{` after an operator is a closure argument only for operators that take one;
                // elsewhere it is the block of an enclosing `for` or `if`. `for line in open
                // ./log.txt { … }` would otherwise read the block as a record argument to `open`.
                Tok::LBrace if !BRACE_ARG_OPERATORS.contains(&name) => break,
                // A flag in expression-mode argument position. Re-lex in word mode: it is
                // `--reverse`, not minus-minus-reverse.
                Tok::Minus => {
                    let w = self.lx.peek(Mode::Word)?;
                    match w.tok {
                        Tok::Flag(f) => {
                            self.lx.bump(Mode::Word)?;
                            args.push(Arg::Flag(f, None));
                        }
                        Tok::ShortFlags(f) => {
                            self.lx.bump(Mode::Word)?;
                            args.push(Arg::ShortFlags(f));
                        }
                        _ => args.push(Arg::Positional(self.range_expr()?)),
                    }
                }
                _ => args.push(Arg::Positional(self.range_expr()?)),
            }
        }
        self.in_op_args -= 1;
        if PREDICATE_OPERATORS.contains(&name) {
            desugar_predicate(&mut args);
        }
        Ok(args)
    }

    /// Word-mode arguments to an external program (§5b, D1).
    fn word_args(&mut self) -> Result<Vec<Arg>> {
        let mut args = Vec::new();
        loop {
            let s = self.lx.peek(Mode::Word)?;
            match s.tok {
                Tok::Pipe
                | Tok::OrOr
                | Tok::AndAnd
                | Tok::Newline
                | Tok::Eof
                | Tok::RParen
                | Tok::RBrace
                | Tok::RBracket => return Ok(args),
                Tok::Flag(f) => {
                    self.lx.bump(Mode::Word)?;
                    args.push(Arg::Flag(f, None));
                }
                Tok::ShortFlags(f) => {
                    self.lx.bump(Mode::Word)?;
                    args.push(Arg::ShortFlags(f));
                }
                Tok::Word(w) => {
                    self.lx.bump(Mode::Word)?;
                    args.push(Arg::Positional(Expr::Word(w)));
                }
                Tok::Str(v) => {
                    self.lx.bump(Mode::Word)?;
                    args.push(Arg::Positional(Expr::Str(v)));
                }
                Tok::LParen => {
                    args.push(Arg::Positional(self.expr()?));
                }
                _ => return Ok(args),
            }
        }
    }

    fn pattern(&mut self) -> Result<Pattern> {
        self.enter()?;
        let first = self.pattern_single()?;
        // `a | b | c`. Never collides with pipeline `|`: this production is only reached
        // from a match arm, which never calls into `expr` (§9f).
        if self.peek()? != Tok::Pipe {
            self.exit();
            return Ok(first);
        }
        let mut alts = alloc::vec![first];
        while self.eat(&Tok::Pipe)? {
            alts.push(self.pattern_single()?);
        }
        self.exit();
        Ok(Pattern::Or(alts))
    }

    fn pattern_single(&mut self) -> Result<Pattern> {
        let s = self.peek_spanned()?;
        match s.tok {
            Tok::Underscore => {
                self.bump()?;
                Ok(Pattern::Wildcard)
            }
            Tok::LBrace => {
                self.bump()?;
                let mut fields = Vec::new();
                loop {
                    self.skip_newlines()?;
                    if self.eat(&Tok::RBrace)? {
                        return Ok(Pattern::Record(fields));
                    }
                    let name = self.ident("expected a field name in a record pattern")?;
                    let sub = if self.eat(&Tok::Colon)? { Some(self.pattern_single()?) } else { None };
                    fields.push((name, sub));
                    if self.eat(&Tok::Comma)? {
                        continue;
                    }
                    self.skip_newlines()?;
                    self.expect(&Tok::RBrace, "expected `,` or `}` in a record pattern")?;
                    return Ok(Pattern::Record(fields));
                }
            }
            Tok::Ident(name) => {
                self.bump()?;
                // §9f: capitalisation carries the meaning. `Int(n)` tests a variant; a
                // lowercase `x` is a catch-all binding.
                let capitalised = name.as_bytes().first().is_some_and(u8::is_ascii_uppercase);
                if capitalised {
                    let mut inner = Vec::new();
                    if self.eat(&Tok::LParen)? {
                        loop {
                            if self.eat(&Tok::RParen)? {
                                break;
                            }
                            inner.push(self.pattern()?);
                            if self.eat(&Tok::Comma)? {
                                continue;
                            }
                            self.expect(&Tok::RParen, "expected `,` or `)` in a pattern")?;
                            break;
                        }
                    }
                    return Ok(Pattern::Variant(name, inner));
                }
                if self.eat(&Tok::At)? {
                    let inner = self.pattern_single()?;
                    return Ok(Pattern::Capture(name, Box::new(inner)));
                }
                Ok(Pattern::Bind(name))
            }
            Tok::Int(_) | Tok::Float(_) | Tok::Str(_) | Tok::Minus | Tok::True | Tok::False | Tok::Null => {
                let lit = self.pattern_literal()?;
                let inclusive = match self.peek()? {
                    Tok::DotDot => false,
                    Tok::DotDotEq => true,
                    _ => return Ok(Pattern::Literal(lit)),
                };
                self.bump()?;
                let end = self.pattern_literal()?;
                Ok(Pattern::Range { start: lit, end, inclusive })
            }
            _ => self.fail("expected a pattern"),
        }
    }

    fn pattern_literal(&mut self) -> Result<Expr> {
        let negative = self.eat(&Tok::Minus)?;
        let e = match self.bump()? {
            Tok::Int(v) => Expr::Int(if negative { -v } else { v }),
            Tok::Float(v) => Expr::Float(if negative { -v } else { v }),
            Tok::Str(v) => Expr::Str(v),
            Tok::True => Expr::Bool(true),
            Tok::False => Expr::Bool(false),
            Tok::Null => Expr::Null,
            _ => return self.fail("expected a literal in a pattern"),
        };
        Ok(e)
    }

    /// `type_expr := base_type "?"?` (§9c). §6: this is a notation for a TSM1 `Schema`,
    /// not a separate concept.
    fn type_expr(&mut self) -> Result<TypeExpr> {
        self.enter()?;
        let r = self.type_expr_inner();
        self.exit();
        r
    }

    fn type_expr_inner(&mut self) -> Result<TypeExpr> {
        if self.peek()? == Tok::LBrace {
            self.bump()?;
            let mut fields = Vec::new();
            loop {
                self.skip_newlines()?;
                if self.eat(&Tok::RBrace)? {
                    break;
                }
                let name = self.ident("expected a field name in a record type")?;
                // `size?: Int` is sugar for `size: Int?` — both spellings kept, since
                // each reads better in a different place (§9e).
                let optional = self.eat(&Tok::Question)?;
                self.expect(&Tok::Colon, "expected `:` in a record type")?;
                let mut ty = self.type_expr()?;
                if optional {
                    ty = make_nullable(ty);
                }
                fields.push((name, ty));
                self.skip_newlines()?;
                if self.eat(&Tok::Comma)? {
                    continue;
                }
                self.skip_newlines()?;
                self.expect(&Tok::RBrace, "expected `,` or `}` in a record type")?;
                break;
            }
            let nullable = self.eat(&Tok::Question)?;
            return Ok(TypeExpr::Record { fields, nullable });
        }
        let name = self.ident("expected a type name")?;
        let mut params = Vec::new();
        if self.eat(&Tok::Lt)? {
            loop {
                params.push(self.type_expr()?);
                if self.eat(&Tok::Comma)? {
                    continue;
                }
                self.expect(&Tok::Gt, "expected `,` or `>` in type parameters")?;
                break;
            }
        }
        let nullable = self.eat(&Tok::Question)?;
        Ok(TypeExpr::Named { name, params, nullable })
    }
}

fn make_nullable(t: TypeExpr) -> TypeExpr {
    match t {
        TypeExpr::Named { name, params, .. } => TypeExpr::Named { name, params, nullable: true },
        TypeExpr::Record { fields, .. } => TypeExpr::Record { fields, nullable: true },
    }
}

/// §8b: `filter size > 1000` is sugar for `filter { |it| it.size > 1000 }`.
///
/// Every bare identifier becomes field access on `it`, so the sugared form cannot reach a *local*
/// variable: `filter size > threshold` reads as though it closes over `threshold` and does not. The
/// explicit closure form exists for that case.
fn desugar_predicate(args: &mut [Arg]) {
    if args.len() != 1 {
        return;
    }
    let Arg::Positional(e) = &args[0] else { return };
    // An explicit closure is already in the target form.
    if matches!(e, Expr::Closure { .. }) {
        return;
    }
    let body = field_shorthand(e.clone());
    args[0] = Arg::Positional(Expr::Closure {
        params: alloc::vec![Param { name: String::from("it"), ty: None, default: None, variadic: false }],
        body: alloc::vec![Stmt::Expr(body)],
    });
}

/// Rewrite bare identifiers as `it.<name>` throughout an expression (§8b).
fn field_shorthand(e: Expr) -> Expr {
    match e {
        Expr::Ident(name) => Expr::Field(Box::new(Expr::Ident(String::from("it"))), name),
        Expr::Unary(op, a) => Expr::Unary(op, Box::new(field_shorthand(*a))),
        Expr::Binary(op, a, b) => Expr::Binary(op, Box::new(field_shorthand(*a)), Box::new(field_shorthand(*b))),
        // A field access already names its base explicitly; only the base is rewritten.
        Expr::Field(a, f) => Expr::Field(Box::new(field_shorthand(*a)), f),
        Expr::TryField(a, f) => Expr::TryField(Box::new(field_shorthand(*a)), f),
        Expr::Index(a, i) => Expr::Index(Box::new(field_shorthand(*a)), Box::new(field_shorthand(*i))),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(src: &str) -> Script {
        match parse_script(src) {
            Ok(s) => s,
            Err(e) => panic!("parse failed at line {}: {}", e.line, e.message),
        }
    }

    fn one_expr(src: &str) -> Expr {
        let s = script(src);
        assert_eq!(s.stmts.len(), 1, "expected exactly one statement");
        match &s.stmts[0] {
            Stmt::Expr(e) => e.clone(),
            other => panic!("expected an expression statement, got {other:?}"),
        }
    }

    /// **The root directory is a path, not a division sign.** A lone `/` in argument position has
    /// no right operand, so division is impossible; `/system` lexes as one path word and was never
    /// affected, which is why `list /` went untested.
    #[test]
    fn a_lone_slash_in_argument_position_is_the_root_path() {
        for src in ["list /", "list / | count", "(list /)", "list /\nlist /home"] {
            assert!(parse_script(src).is_ok(), "{src} should parse");
        }
    }

    /// Division must keep working: a test of only the new case would not notice.
    #[test]
    fn division_is_still_division() {
        for src in ["let x = 6 / 2", "let x = a / b", "let x = a/b", "let x = (a / b) / c"] {
            assert!(parse_script(src).is_ok(), "{src} should parse");
        }
        // `a / b` is a division, not `a` called with two arguments; both readings parse.
        match one_expr("a / b") {
            Expr::Binary(..) => {}
            other => panic!("`a / b` should be a division, got {other:?}"),
        }
    }

    #[test]
    fn an_operator_takes_expressions_and_an_external_takes_words() {
        // `sort` is an operator: `size` is an expression, `--reverse` a flag.
        let e = one_expr("sort size --reverse");
        let Expr::Call(c) = e else { panic!("expected a call") };
        assert_eq!(c.kind, CallKind::Operator);
        assert_eq!(c.args[0], Arg::Positional(Expr::Ident("size".into())));
        assert_eq!(c.args[1], Arg::Flag("reverse".into(), None));

        // `list` is not: `README.md` is a filename, not field access on `README`.
        let e = one_expr("list --long README.md");
        let Expr::Call(c) = e else { panic!("expected a call") };
        assert_eq!(c.kind, CallKind::External);
        assert_eq!(c.args[0], Arg::Flag("long".into(), None));
        assert_eq!(c.args[1], Arg::Positional(Expr::Word("README.md".into())));
    }

    #[test]
    fn a_hyphenated_command_name_is_one_name() {
        let e = one_expr("fs-server-ext4 --check /dev/disk");
        let Expr::Call(c) = e else { panic!("expected a call") };
        assert_eq!(c.name, "fs-server-ext4");
        assert_eq!(c.kind, CallKind::External);
        assert_eq!(c.args[0], Arg::Flag("check".into(), None));
        // …and arithmetic with spaces is still arithmetic.
        assert!(matches!(one_expr("a - b"), Expr::Binary(BinOp::Sub, _, _)));
    }

    #[test]
    fn a_bare_path_reaches_an_external_program_intact() {
        let e = one_expr("list --long /some/path");
        let Expr::Call(c) = e else { panic!("expected a call") };
        assert_eq!(c.args[1], Arg::Positional(Expr::Word("/some/path".into())));
    }

    /// `^` forces external resolution (§3), which also forces word-mode arguments.
    #[test]
    fn caret_forces_an_external_call() {
        let e = one_expr("^sort a.b");
        let Expr::Call(c) = e else { panic!("expected a call") };
        assert_eq!(c.kind, CallKind::External);
        assert!(c.forced_external);
        assert_eq!(c.args[0], Arg::Positional(Expr::Word("a.b".into())));
    }

    #[test]
    fn a_parenthesised_call_is_a_def_and_takes_expressions() {
        let e = one_expr("summarize(_, label: \"src files\")");
        let Expr::Call(c) = e else { panic!("expected a call") };
        assert_eq!(c.kind, CallKind::Def);
        assert_eq!(c.args[0], Arg::PipeFill);
        assert_eq!(c.args[1], Arg::Named("label".into(), Expr::Str("src files".into())));
    }

    #[test]
    fn a_bare_identifier_is_left_for_the_evaluator_to_resolve() {
        assert_eq!(one_expr("x"), Expr::Ident("x".into()));
    }

    #[test]
    fn a_bareword_predicate_desugars_to_a_closure() {
        let e = one_expr("filter size > 1000");
        let Expr::Call(c) = e else { panic!("expected a call") };
        let Arg::Positional(Expr::Closure { params, body }) = &c.args[0] else {
            panic!("expected a desugared closure, got {:?}", c.args[0])
        };
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "it");
        assert_eq!(
            body[0],
            Stmt::Expr(Expr::Binary(
                BinOp::Gt,
                Box::new(Expr::Field(Box::new(Expr::Ident("it".into())), "size".into())),
                Box::new(Expr::Int(1000)),
            ))
        );
    }

    #[test]
    fn an_explicit_closure_is_left_alone() {
        let e = one_expr("filter { |row| row.size > 1000 }");
        let Expr::Call(c) = e else { panic!("expected a call") };
        let Arg::Positional(Expr::Closure { params, .. }) = &c.args[0] else { panic!("expected a closure") };
        assert_eq!(params[0].name, "row");
    }

    #[test]
    fn precedence_follows_8a() {
        assert_eq!(
            one_expr("1 + 2 * 3"),
            Expr::Binary(
                BinOp::Add,
                Box::new(Expr::Int(1)),
                Box::new(Expr::Binary(BinOp::Mul, Box::new(Expr::Int(2)), Box::new(Expr::Int(3)))),
            )
        );
        let e = one_expr("a && b || c");
        assert!(matches!(e, Expr::Binary(BinOp::Or, _, _)));
        let e = one_expr("a < b == c");
        assert!(matches!(e, Expr::Binary(BinOp::Eq, _, _)));
    }

    #[test]
    fn the_pipe_is_the_loosest_operator() {
        let e = one_expr("1 + 2 | display");
        let Expr::Pipeline(stages) = e else { panic!("expected a pipeline") };
        assert_eq!(stages.len(), 2);
        assert!(matches!(stages[0], Expr::Binary(BinOp::Add, _, _)));
    }

    /// A paren may open a pipeline or hold a comparison: nullary operators take no arguments.
    #[test]
    fn a_paren_can_open_a_pipeline_or_hold_a_comparison() {
        let e = one_expr("(list /system | count)");
        let Expr::Pipeline(stages) = e else { panic!("expected a pipeline") };
        assert_eq!(stages.len(), 2);
        let Expr::Call(c) = &stages[0] else { panic!("expected a call") };
        assert_eq!(c.name, "list");
        assert_eq!(c.args[0], Arg::Positional(Expr::Word("/system".into())));

        assert!(matches!(one_expr("(count > 0)"), Expr::Binary(BinOp::Gt, _, _)));
    }

    #[test]
    fn a_piped_condition_must_be_parenthesised() {
        let s = script("if (files | count) > 0 { display files }");
        assert!(matches!(s.stmts[0], Stmt::If { .. }));
        assert!(parse_script("if ls | count > 0 { display files }").is_err());
    }

    #[test]
    fn bindings_carry_their_mutability_and_type() {
        let s = script("let x: Int = 5");
        let Stmt::Bind { kind, name, ty, .. } = &s.stmts[0] else { panic!() };
        assert_eq!(*kind, BindKind::Let);
        assert_eq!(name, "x");
        assert_eq!(*ty, Some(TypeExpr::Named { name: "Int".into(), params: alloc::vec![], nullable: false }));

        let s = script("mut counts = { errors: 0 }");
        assert!(matches!(s.stmts[0], Stmt::Bind { kind: BindKind::Mut, .. }));

        let s = script("pub const THRESHOLD: Int = 1000");
        let Stmt::Bind { kind, public, .. } = &s.stmts[0] else { panic!() };
        assert_eq!(*kind, BindKind::Const);
        assert!(public);
    }

    #[test]
    fn assignment_targets_a_path_not_just_a_name() {
        let s = script("counts.errors = counts.errors + 1");
        let Stmt::Assign { target, .. } = &s.stmts[0] else { panic!() };
        assert_eq!(target.root, "counts");
        assert_eq!(target.path, alloc::vec![LValueStep::Field("errors".into())]);
        // A non-assignable left side is a parse error, not a silent discard.
        assert!(parse_script("1 + 2 = 3").is_err());
    }

    #[test]
    fn a_def_carries_defaults_variadics_and_a_return_type() {
        let s = script("def greet(name: String, loud: Bool = false, ...rest: List<String>) -> String { name }");
        let Stmt::Def { name, params, ret, .. } = &s.stmts[0] else { panic!() };
        assert_eq!(name, "greet");
        assert_eq!(params.len(), 3);
        assert_eq!(params[1].default, Some(Expr::Bool(false)));
        assert!(params[2].variadic);
        assert!(ret.is_some());
    }

    #[test]
    fn try_catch_is_an_expression_and_strict_is_a_statement() {
        let s = script("try { open ./x } catch (err) { print }");
        let Stmt::Expr(Expr::Try { catch_binding, .. }) = &s.stmts[0] else {
            panic!("expected a try expression, got {:?}", s.stmts[0])
        };
        assert_eq!(catch_binding.as_deref(), Some("err"));

        let s = script("let x = try { open ./x } catch { 0 }");
        let Stmt::Bind { value: Expr::Try { .. }, .. } = &s.stmts[0] else {
            panic!("expected a bound try expression, got {:?}", s.stmts[0])
        };

        let s = script("strict { open ./in | save ./out }");
        assert!(matches!(s.stmts[0], Stmt::Strict(_)));
    }

    /// §2: `?` retired, and the message says why rather than "expected an expression".
    #[test]
    fn the_propagation_operator_is_retired_with_a_message() {
        // In *word* mode a trailing `?` is part of the path (`./f?` is a filename), so the
        // retired operator only exists where an expression does.
        let e = parse_script("let x = a?").expect_err("`?` is gone");
        assert!(e.message.contains("retired"), "{}", e.message);
        assert!(e.message.contains("try"), "{}", e.message);
        script("let a: Int? = null");
        script("let r: { size?: Int } = { size: 1 }");
        script("let n = a?.size ?? 0");
    }

    #[test]
    fn imports_must_name_what_they_bring_in() {
        let s = script("use \"./lib/utils.nx\" { helper, other_fn }");
        let Stmt::Use { path, names, .. } = &s.stmts[0] else { panic!() };
        assert_eq!(path, "./lib/utils.nx");
        assert_eq!(names.as_ref().unwrap().len(), 2);

        let s = script("use \"./lib/utils.nx\" as utils");
        let Stmt::Use { alias, .. } = &s.stmts[0] else { panic!() };
        assert_eq!(alias.as_deref(), Some("utils"));

        assert!(parse_script("use \"./lib/utils.nx\"").is_err());
        assert!(parse_script("use \"./lib/utils.nx\" { }").is_err());
    }

    #[test]
    fn a_brace_is_a_closure_only_when_pipes_follow() {
        let s = script("let f = { |x| x + 1 }");
        let Stmt::Bind { value, .. } = &s.stmts[0] else { panic!() };
        assert!(matches!(value, Expr::Closure { .. }));

        let s = script("let r = { name }");
        let Stmt::Bind { value, .. } = &s.stmts[0] else { panic!() };
        assert_eq!(*value, Expr::Record(alloc::vec![("name".into(), Expr::Ident("name".into()))]));

        let s = script("let f = { || 1 }");
        let Stmt::Bind { value, .. } = &s.stmts[0] else { panic!() };
        let Expr::Closure { params, .. } = value else { panic!("expected a closure") };
        assert!(params.is_empty());
    }

    #[test]
    fn patterns_cover_the_9f_forms() {
        let e = one_expr(
            "match row {\n\
             { name, size } if size > 1000 => name\n\
             Int(n) => n\n\
             x @ Float => x\n\
             0..10 => 1\n\
             \"a\" | \"b\" => 2\n\
             _ => 3\n\
             }",
        );
        let Expr::Match { arms, .. } = e else { panic!("expected a match") };
        assert_eq!(arms.len(), 6);
        assert!(matches!(arms[0].pattern, Pattern::Record(_)));
        assert!(arms[0].guard.is_some());
        assert!(matches!(arms[1].pattern, Pattern::Variant(_, _)));
        assert!(matches!(arms[2].pattern, Pattern::Capture(_, _)));
        assert!(matches!(arms[3].pattern, Pattern::Range { .. }));
        assert!(matches!(arms[4].pattern, Pattern::Or(_)));
        assert!(matches!(arms[5].pattern, Pattern::Wildcard));
    }

    #[test]
    fn capitalisation_separates_a_type_test_from_a_binding() {
        let e = one_expr("match v { Int => 1\n other => 2 }");
        let Expr::Match { arms, .. } = e else { panic!() };
        assert_eq!(arms[0].pattern, Pattern::Variant("Int".into(), alloc::vec![]));
        assert_eq!(arms[1].pattern, Pattern::Bind("other".into()));
    }

    #[test]
    fn pattern_alternation_does_not_collide_with_the_pipe() {
        let e = one_expr("match n { 1 | 2 => \"low\"\n _ => \"high\" }");
        let Expr::Match { arms, .. } = e else { panic!() };
        let Pattern::Or(alts) = &arms[0].pattern else { panic!("expected an or-pattern") };
        assert_eq!(alts.len(), 2);
    }

    #[test]
    fn nullable_types_have_two_spellings_that_mean_one_thing() {
        let a = script("let x: Int? = 1");
        let Stmt::Bind { ty: Some(t), .. } = &a.stmts[0] else { panic!() };
        assert_eq!(*t, TypeExpr::Named { name: "Int".into(), params: alloc::vec![], nullable: true });

        let b = script("let x: { size?: Int } = y");
        let Stmt::Bind { ty: Some(TypeExpr::Record { fields, .. }), .. } = &b.stmts[0] else { panic!() };
        assert_eq!(fields[0].1, TypeExpr::Named { name: "Int".into(), params: alloc::vec![], nullable: true });
    }

    #[test]
    fn safe_navigation_and_coalescing_parse() {
        let e = one_expr("row?.name ?? \"unknown\"");
        let Expr::Binary(BinOp::Coalesce, lhs, _) = e else { panic!("expected `??`") };
        assert!(matches!(*lhs, Expr::TryField(_, _)));
    }

    #[test]
    fn nesting_past_the_limit_is_an_error() {
        let mut src = String::new();
        for _ in 0..(MAX_DEPTH + 20) {
            src.push('(');
        }
        src.push('1');
        for _ in 0..(MAX_DEPTH + 20) {
            src.push(')');
        }
        let e = parse_script(&src).expect_err("should refuse to nest this deeply");
        assert_eq!(e.message, "expression nests too deeply");
    }

    #[test]
    fn the_section_7_sketch_parses() {
        let src = r#"
const THRESHOLD_DEFAULT: Int = 1000
let threshold = THRESHOLD_DEFAULT
let files = open ./src | filter { |row| row.size > threshold }

if (files | count) > 0 {
    display files
} else {
    print "nothing matched"
}

def summarize(data: Table<{name: String, size: Int}>, label: String = "result")
    -> Record<{label: String, total: Int}> {
    return { label: label, total: data | count }
}

ls --long
  | expect Table<{name: String, size: Int}>
  | assert (count > 0)
  | sort size --reverse
  | summarize(_, label: "src files")

strict {
    open ./input.csv | validate_schema | save ./output.csv
}

mut counts = { errors: 0, warnings: 0 }
for line in open ./log.txt {
    if line.level == "error" {
        counts.errors = counts.errors + 1
    }
}
"#;
        let s = script(src);
        assert_eq!(s.stmts.len(), 9);
    }

    #[test]
    fn a_leading_pipe_pipeline_is_one_statement() {
        let s = script("ls --long\n  | sort size\n  | display");
        assert_eq!(s.stmts.len(), 1);
        let Stmt::Expr(Expr::Pipeline(stages)) = &s.stmts[0] else {
            panic!("expected one pipeline, got {:?}", s.stmts)
        };
        assert_eq!(stages.len(), 3);
    }

    #[test]
    fn two_statements_on_two_lines_stay_two_statements() {
        let s = script("let a = 1\nlet b = 2");
        assert_eq!(s.stmts.len(), 2);
        // …and running them together is an error rather than a silent join.
        assert!(parse_script("let a = 1 let b = 2").is_err());
    }

    /// Loop-only is the counter; "does not cross a `def` or closure" is the reset.
    #[test]
    fn break_and_continue_are_legal_only_inside_a_loop() {
        script("for x in 0..3 { break }");
        script("for x in 0..3 { continue }");
        script("while true { break }");
        script("for x in 0..3 { if x == 1 { break } }");

        for src in ["break", "continue", "if true { break }", "def f() { break }"] {
            let e = parse_script(src).expect_err(src);
            assert!(e.message.contains("`for` or `while`"), "{src}: {}", e.message);
        }
    }

    /// The closure is a function body; its iteration belongs to the operator.
    #[test]
    fn break_does_not_cross_a_closure_or_a_def() {
        let e = parse_script("for x in 0..3 { let f = { || break } }").expect_err("closure");
        assert!(e.message.contains("closure"), "{}", e.message);
        let e = parse_script("for x in 0..3 { def f() { break } }").expect_err("def");
        assert!(e.message.contains("`for` or `while`"), "{}", e.message);
        script("for x in 0..3 { let f = { || 1 }\n break }");
    }

    /// The counter must come back down, or a stray `break` after the first loop would pass.
    #[test]
    fn the_loop_counter_unwinds_after_the_body() {
        script("for x in 0..3 { break }");
        let e = parse_script("for x in 0..3 { }\nbreak").expect_err("after the loop");
        assert!(e.message.contains("`for` or `while`"), "{}", e.message);
    }

    #[test]
    fn a_name_before_in_is_an_operand_not_a_command() {
        let e = one_expr("x in [1, 2]");
        assert!(matches!(e, Expr::Binary(BinOp::In, _, _)), "got {e:?}");
        let s = script("for x in [1, 2] {\n x\n}");
        assert!(matches!(s.stmts[0], Stmt::For { .. }));
    }
}

#[cfg(test)]
mod argument_list_tests {
    use super::*;

    fn script(src: &str) -> Script {
        match parse_script(src) {
            Ok(s) => s,
            Err(e) => panic!("`{src}` failed at line {}: {}", e.line, e.message),
        }
    }

    /// The argument of a call is an **expression**, however it begins: `age_plus_n(my_age(), 3)`
    /// once failed with "expected `,` or `)`".
    #[test]
    fn a_call_can_be_an_argument() {
        for src in [
            "def my_age() { 41 }\ndef age_plus_n(a, n) { a + n }\nlet x = age_plus_n(my_age(), 3)",
            "def add(a, b) { a + b }\nformat(\"add={}\", add(2, 3))",
            "format(\"n={}\", count([1, 2, 3]))",
            // A qualified name — §9h's `use … as` — which failed for a *different* reason.
            "format(\"x={}\", utils.helper(1))",
            "def f(a) { a }\nf(f(f(1)))",
            "def g(a, b) { a }\ndef h() { 1 }\ng(h(), b: h())",
            // The workaround this replaces, which must keep working.
            "def add(a, b) { a + b }\nlet sum = add(2, 3)\nformat(\"add={}\", sum)",
            "format(\"len={}\", (\"hello\" | count))",
        ] {
            script(src);
        }
    }

    /// An argument starting with an identifier binds like any other: `format("{}", a + b * c)` once
    /// folded flat into `(a + b) * c` through a hand-rolled copy of the tiers.
    #[test]
    fn an_argument_starting_with_a_name_binds_like_any_other() {
        let by_name = one_arg("format(\"{}\", a + b * c)");
        let by_literal = one_arg("format(\"{}\", 1 + 2 * 3)");
        assert!(
            matches!(by_name, Expr::Binary(BinOp::Add, _, _)),
            "`a + b * c` as an argument parsed as {by_name:?}, not an addition"
        );
        assert!(
            matches!(by_literal, Expr::Binary(BinOp::Add, _, _)),
            "the literal control changed shape: {by_literal:?}"
        );
    }

    /// Every tier below the top one, exercised from argument position.
    #[test]
    fn an_argument_reaches_every_tier() {
        for (src, want) in [
            ("format(\"{}\", a ?? b)", BinOp::Coalesce),
            ("format(\"{}\", a || b && c)", BinOp::Or),
            ("format(\"{}\", a == b)", BinOp::Eq),
            ("format(\"{}\", a ++ b)", BinOp::Concat),
            ("format(\"{}\", a - b)", BinOp::Sub),
        ] {
            match one_arg(src) {
                Expr::Binary(op, _, _) => assert_eq!(op, want, "{src}"),
                other => panic!("{src} parsed as {other:?}"),
            }
        }
        // Postfix still binds tighter than any of them.
        assert!(matches!(one_arg("format(\"{}\", a.b + c)"), Expr::Binary(BinOp::Add, _, _)));
        assert!(matches!(one_arg("format(\"{}\", a[0] + c)"), Expr::Binary(BinOp::Add, _, _)));
    }

    /// The second positional argument of the one call in `src`.
    fn one_arg(src: &str) -> Expr {
        let s = script(src);
        let Stmt::Expr(Expr::Call(c)) = &s.stmts[0] else { panic!("{src}: not a call") };
        match &c.args[1] {
            Arg::Positional(e) => e.clone(),
            other => panic!("{src}: argument 1 is {other:?}"),
        }
    }
}
