//! The `nxsh` syntax tree — §8/§9 of the design, as data.
//!
//! Two distinctions the grammar makes are kept rather than flattened:
//!
//! - **A call carries its category** (§3: keyword, builtin, operator, external) in
//!   [`Call::kind`]. It decides whether arguments are expressions or barewords (see
//!   [`crate::lex`]), so the evaluator never re-derives it.
//! - **Sugar is desugared by the parser.** §8b's bareword predicate (`filter size > 1000`)
//!   becomes an implicit closure, so there is one closure representation downstream.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// A parsed script: a sequence of statements.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Script {
    pub stmts: Vec<Stmt>,
}

/// Which of §3's four categories a head resolved to; it selects the argument grammar.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CallKind {
    /// A generic value operator — `filter`, `sort`, `save` (§5c); may take a closure.
    Operator,
    /// A shell-state builtin — `cd`, `exit` (§3); it must mutate the shell's own state.
    Builtin,
    /// A script function called with the parens/named-argument convention (§5b).
    Def,
    /// An ordinary userspace program, spawned as a pipeline stage. Bareword arguments.
    External,
}

/// A command invocation: a head name, its category, and its arguments.
#[derive(Clone, PartialEq, Debug)]
pub struct Call {
    pub name: String,
    pub kind: CallKind,
    pub args: Vec<Arg>,
    /// `^name` — §3's keyword-collision override, which forces [`CallKind::External`].
    pub forced_external: bool,
}

/// One argument at a call site.
#[derive(Clone, PartialEq, Debug)]
pub enum Arg {
    /// A positional expression, or a bareword for an external program.
    Positional(Expr),
    /// `name: expr` — the named-argument form for `def` calls (§5b).
    Named(String, Expr),
    /// `--flag` with no value, or `--flag value`.
    Flag(String, Option<Expr>),
    /// `-abc`, a run of short flags (§10f).
    ShortFlags(String),
    /// `_` — the pipeline-fill placeholder, required once a `def` call has arguments (§5b).
    PipeFill,
}

/// An expression.
#[derive(Clone, PartialEq, Debug)]
pub enum Expr {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Null,
    /// A bareword argument to an external program: a path, a filename, a literal.
    Word(String),
    /// A `/pattern/` literal. Only legal as the right operand of `~=` (§10b).
    Regex(String),
    /// A variable reference.
    Ident(String),
    /// `_` in expression position.
    Underscore,

    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// `a .. b` / `a ..= b`. Language-only, never a `Value` (§8c).
    Range {
        start: Box<Expr>,
        end: Box<Expr>,
        inclusive: bool,
    },
    /// `a | b | c`. Stages in order; always at least two.
    Pipeline(Vec<Expr>),

    Call(Box<Call>),
    /// `x.field`.
    Field(Box<Expr>, String),
    /// `x?.field` — safe navigation (§9e).
    TryField(Box<Expr>, String),
    /// `x[i]`.
    Index(Box<Expr>, Box<Expr>),
    /// `try { … } catch (e) { … }` — an **expression** (§9c, v1.2), so a failure can
    /// produce a fallback *value*. Sugar over branching on a failure, not unwinding (§2).
    Try {
        body: Vec<Stmt>,
        catch_binding: Option<String>,
        catch_body: Vec<Stmt>,
    },

    List(Vec<Expr>),
    Record(Vec<(String, Expr)>),
    /// `{ |a, b| … }`. Pipes are mandatory even when empty, which is what disambiguates a
    /// closure from a record literal (§8c).
    Closure {
        params: Vec<Param>,
        body: Vec<Stmt>,
    },

    /// A block in expression position; its value is its last expression-shaped statement.
    Block(Vec<Stmt>),
    If {
        cond: Box<Expr>,
        then: Vec<Stmt>,
        otherwise: Option<Vec<Stmt>>,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    /// `expect T` — ascription in expression position (§6).
    Expect(Box<Expr>, TypeExpr),
    /// `parse T` — conversion, `expect`'s converting sibling (§6); same fail-loud contract.
    Parse(Box<Expr>, TypeExpr),
    /// `assert (predicate)` — a content check, deliberately not folded into `expect` (§6).
    Assert(Box<Expr>),
    /// `time PIPELINE`: the pipeline's value, and a line on `stderr` when it ends with the
    /// elapsed time and the disk and page-cache activity (§3).
    Time(Box<Expr>),
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    /// `++` — string concatenation (§8a).
    Concat,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    /// `~=` — regex match (§10b).
    Match,
    /// `in` — membership (§8a, §10b); infix because that reads correctly inside an `if`.
    In,
    And,
    Or,
    /// `??` — null coalescing (§9e).
    Coalesce,
}

/// A function or closure parameter (§5b).
#[derive(Clone, PartialEq, Debug)]
pub struct Param {
    pub name: String,
    pub ty: Option<TypeExpr>,
    /// Evaluated fresh per call in the function's parameter scope, so a later default can
    /// reference an earlier parameter and no mutable default is shared (§5b).
    pub default: Option<Expr>,
    /// `...paths: List<String>` — must be the last positional parameter.
    pub variadic: bool,
}

/// One arm of a `match` (§9f).
#[derive(Clone, PartialEq, Debug)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Vec<Stmt>,
}

/// A pattern (§9f). A capitalised head is a type test, a bare lowercase identifier a catch-all.
#[derive(Clone, PartialEq, Debug)]
pub enum Pattern {
    /// `_`.
    Wildcard,
    /// A lowercase identifier: matches anything, binds the whole value.
    Bind(String),
    /// `Int`, `Table`, `Record(…)` — a type test, optionally destructuring.
    Variant(String, Vec<Pattern>),
    /// `{ name, size: s }` — subset match, extra fields ignored (§6).
    Record(Vec<(String, Option<Pattern>)>),
    /// A literal value.
    Literal(Expr),
    /// `0..10` / `0..=10`.
    Range { start: Expr, end: Expr, inclusive: bool },
    /// `x @ pattern`.
    Capture(String, Box<Pattern>),
    /// `a | b | c`; never collides with pipeline `|`, which `match_arm` never reaches (§9f).
    Or(Vec<Pattern>),
}

/// A type annotation: a textual notation for a TSM1 `Schema` (§6).
#[derive(Clone, PartialEq, Debug)]
pub enum TypeExpr {
    /// `Int`, `Table<{…}>`, `List<T>` — a named type with optional parameters.
    Named { name: String, params: Vec<TypeExpr>, nullable: bool },
    /// `{ name: String, size?: Int }`; `size?: T` is sugar for `size: T?` (§9e).
    Record { fields: Vec<(String, TypeExpr)>, nullable: bool },
}

/// The target of an assignment (§9d). Legal only when the root binding is `mut`.
#[derive(Clone, PartialEq, Debug)]
pub struct LValue {
    pub root: String,
    pub path: Vec<LValueStep>,
}

#[derive(Clone, PartialEq, Debug)]
pub enum LValueStep {
    Field(String),
    Index(Expr),
}

/// How a binding was introduced (§9d).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum BindKind {
    /// `let` — immutable, file-private.
    Let,
    /// `mut` — mutable, file-private.
    Mut,
    /// `const` — immutable, exportable with `pub`.
    Const,
}

/// A statement (§9c).
#[derive(Clone, PartialEq, Debug)]
pub enum Stmt {
    Bind {
        kind: BindKind,
        name: String,
        ty: Option<TypeExpr>,
        value: Expr,
        /// `pub const` only.
        public: bool,
    },
    Assign {
        target: LValue,
        value: Expr,
    },
    /// A `def`. Hoisted within its declaring scope so mutual recursion works (§5a).
    Def {
        name: String,
        params: Vec<Param>,
        ret: Option<TypeExpr>,
        body: Vec<Stmt>,
        public: bool,
    },
    If {
        cond: Expr,
        then: Vec<Stmt>,
        otherwise: Option<Vec<Stmt>>,
    },
    For {
        binding: String,
        iterable: Expr,
        body: Vec<Stmt>,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    /// `break` / `continue` (§9c). Legal only inside a loop body in the same function; the
    /// parser enforces it (`Parser::loop_depth`).
    ///
    /// Unlabelled: both break the innermost loop only.
    /// TODO(shell-labelled-break): `docs/rationale/deferred-decisions.md`.
    Break,
    Continue,
    /// `strict { … }` — first stage failure terminates the rest (§1); a local block, not a mode.
    Strict(Vec<Stmt>),
    Return(Option<Expr>),
    /// `fail expr` (§2). A `String` is the message; a `Record` is raised as the error value.
    Fail(Expr),
    /// `use "./lib/utils.nx" { helper }` or `… as utils` (§9h).
    Use {
        path: String,
        /// Named imports, or `None` when the whole module is bound via `as`.
        names: Option<Vec<String>>,
        alias: Option<String>,
    },
    /// A bare expression. Auto-displayed in the REPL, discarded in a script (§11e).
    Expr(Expr),
}

impl Expr {
    /// Whether this expression is a call to a `def` or an external program.
    pub fn is_call(&self) -> bool {
        matches!(self, Expr::Call(_))
    }
}

impl Stmt {
    /// Whether a block ending in this statement has that statement's value (§9a).
    ///
    /// `let`, `return`, `for`, `while`, `def`, `break` and `continue` produce no value. `if`
    /// does: the implicit-last-expression return rule needs a function ending in `if`/`else`
    /// to yield it. `try` is an expression and arrives as `Stmt::Expr`.
    pub fn is_expression_shaped(&self) -> bool {
        matches!(self, Stmt::Expr(_) | Stmt::If { .. })
    }
}
