//! The `nxsh` syntax tree — §8/§9 of the design, as data.
//!
//! Milestone 3 Part A. Two things here are worth reading before the types themselves,
//! because both are places where the tree deliberately keeps a distinction the grammar
//! makes rather than flattening it.
//!
//! ## Command calls carry their category
//!
//! §3 splits commands four ways — keyword, shell-state builtin, generic value operator,
//! external program — and that split is not documentation: it is what decides whether
//! arguments were read as expressions or as barewords (D1, see [`crate::lex`]). The
//! parser resolves the category and records it in [`Call::kind`], so the evaluator never
//! has to re-derive from the argument shape what the parser already knew.
//!
//! ## Sugar is desugared here, not at evaluation
//!
//! §8b's bareword predicate (`filter size > 1000`) is defined as sugar for an implicit
//! closure (`filter { |it| it.size > 1000 }`). The parser performs that expansion, so
//! there is exactly one closure representation downstream and the evaluator has no idea
//! two spellings ever existed. Same treatment as `expect`/ascription: one primitive,
//! several surfaces.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// A parsed script: a sequence of statements.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Script {
    pub stmts: Vec<Stmt>,
}

/// Which of §3's four categories a command head resolved to.
///
/// Resolved at parse time because it selects the argument grammar (D1). `Def` and
/// `External` differ in call *syntax*, not just in what they run: a `def` uses the parens
/// convention (§5b) and an external program takes barewords.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CallKind {
    /// A generic value operator — `filter`, `sort`, `save`, … (§5c). Runs in-process on
    /// the `Value` tree; takes expression-mode arguments and may take a closure.
    Operator,
    /// A shell-state builtin — `cd`, `exit` (§3). Must mutate the shell's own process
    /// state, which an external program structurally cannot do.
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
    /// `_` — the explicit pipeline-fill placeholder, required whenever a `def` call has
    /// an argument list at all (§5b).
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
    Range { start: Box<Expr>, end: Box<Expr>, inclusive: bool },
    /// `a | b | c`. Stages in order; always at least two.
    Pipeline(Vec<Expr>),

    Call(Box<Call>),
    /// `x.field`.
    Field(Box<Expr>, String),
    /// `x?.field` — safe navigation (§9e).
    TryField(Box<Expr>, String),
    /// `x[i]`.
    Index(Box<Expr>, Box<Expr>),
    /// `x?` — Result propagation, exits the current function (§2).
    Try(Box<Expr>),

    List(Vec<Expr>),
    Record(Vec<(String, Expr)>),
    /// `{ |a, b| … }`. Pipes are mandatory even when empty, which is what disambiguates a
    /// closure from a record literal (§8c).
    Closure { params: Vec<Param>, body: Vec<Stmt> },

    /// A block in expression position; its value is the last expression-shaped statement
    /// (§9a).
    Block(Vec<Stmt>),
    If { cond: Box<Expr>, then: Vec<Stmt>, otherwise: Option<Vec<Stmt>> },
    Match { scrutinee: Box<Expr>, arms: Vec<MatchArm> },
    /// `expect T` — ascription in expression position (§6).
    Expect(Box<Expr>, TypeExpr),
    /// `assert (predicate)` — a content check, deliberately not folded into `expect` (§6).
    Assert(Box<Expr>),
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
    /// Evaluated fresh per call, in the function's own parameter scope — which is what
    /// avoids Python's shared-mutable-default trap and lets a later default reference an
    /// earlier parameter (§5b).
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

/// A pattern (§9f). Capitalisation carries meaning: a capitalised head is a type test, a
/// bare lowercase identifier is a catch-all binding.
#[derive(Clone, PartialEq, Debug)]
pub enum Pattern {
    /// `_`.
    Wildcard,
    /// A lowercase identifier: matches anything, binds the whole value.
    Bind(String),
    /// `Int`, `Table`, `Record(…)` — a variant/type test, optionally destructuring.
    Variant(String, Vec<Pattern>),
    /// `{ name, size: s }` — subset match, extra fields ignored (§6).
    Record(Vec<(String, Option<Pattern>)>),
    /// A literal value.
    Literal(Expr),
    /// `0..10` / `0..=10`.
    Range { start: Expr, end: Expr, inclusive: bool },
    /// `x @ pattern`.
    Capture(String, Box<Pattern>),
    /// `a | b | c`. Never collides with pipeline `|`: patterns are a separate production
    /// and `match_arm` never calls into `expr`'s pipeline rule (§9f).
    Or(Vec<Pattern>),
}

/// A type annotation. §6: this is a textual notation for a TSM1 `Schema`, not a separate
/// concept — checking one is comparing it against the schema already in a stream header.
#[derive(Clone, PartialEq, Debug)]
pub enum TypeExpr {
    /// `Int`, `Table<{…}>`, `List<T>` — a named type with optional parameters.
    Named { name: String, params: Vec<TypeExpr>, nullable: bool },
    /// `{ name: String, size?: Int }` — a record shape. `size?: T` is sugar for
    /// `size: T?`, and both spellings are kept because each reads better somewhere (§9e).
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
    /// `try { … } catch (e) { … }` — sugar over branching on a Result-shaped value, not
    /// stack unwinding (§2).
    Try {
        body: Vec<Stmt>,
        catch_binding: Option<String>,
        catch_body: Vec<Stmt>,
    },
    /// `strict { … }` — first stage failure terminates the rest (§1). Deliberately a
    /// local block rather than a global `pipefail`-style mode switch.
    Strict(Vec<Stmt>),
    Return(Option<Expr>),
    /// `use "./lib/utils.nx" { helper }` or `… as utils` (§9h).
    Use {
        path: String,
        /// Named imports, or `None` when the whole module is bound via `as`.
        names: Option<Vec<String>>,
        alias: Option<String>,
    },
    /// A bare expression. Auto-displayed in the REPL, discarded in a script (§11e) — the
    /// two modes differ, which is why the statement keeps no opinion of its own.
    Expr(Expr),
}

impl Expr {
    /// Whether this expression is a call to a `def` or an external program — the two
    /// forms that can be a pipeline *stage* running as a process or a function, as
    /// opposed to a value computed in place.
    pub fn is_call(&self) -> bool {
        matches!(self, Expr::Call(_))
    }
}

impl Stmt {
    /// Whether a block ending in this statement has that statement's value (§9a).
    ///
    /// `let`, `return`, `for`, `while` and `def` produce no value, so a block ending in
    /// one evaluates to `Null`. This is what makes the implicit-last-expression return
    /// rule work when a function ends in an `if`/`else`.
    pub fn is_expression_shaped(&self) -> bool {
        matches!(self, Stmt::Expr(_))
    }
}
