//! The shell's generic value operators (§5c, §10b), and how the parser reads their arguments.

/// How an operator's arguments read, which the parser must know before it reads them.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Takes {
    /// A predicate: a bareword comparison desugars to a `{ |it| … }` closure (§8b); `{` opens one.
    Predicate,
    /// A record, which may open with `{`; `brace_expr` tells it from a closure by the `|` (§8c).
    Record,
    /// None, so `(count > 0)` is a comparison, not `count` applied to `> 0`.
    Nothing,
    /// As it needs. `sum`, `min`, `max`, `avg` are here: `sum size` must keep reading `size`.
    Words,
}

/// One operator.
#[derive(Copy, Clone, Debug)]
pub struct Operator {
    /// What it is called.
    pub name: &'static str,
    /// How its arguments read.
    pub takes: Takes,
    /// It consumes its value and ends the chain, so the REPL adds no `display` after it.
    pub terminal: bool,
}

/// Generic value operators (§3, §5c, §10b). Closed at parse time because it selects the
/// argument grammar; a user-defined `def` is called with parens, so it is distinct.
/// The bitwise family would join: `|`, `&` and `^` are spoken for.
/// TODO(shell-bitwise): `docs/rationale/deferred-decisions.md`.
pub const OPERATORS: &[Operator] = &[
    Operator { name: "filter", takes: Takes::Predicate, terminal: false },
    Operator { name: "sort", takes: Takes::Words, terminal: false },
    Operator { name: "select", takes: Takes::Words, terminal: false },
    Operator { name: "save", takes: Takes::Words, terminal: true },
    Operator { name: "open", takes: Takes::Words, terminal: false },
    Operator { name: "each", takes: Takes::Predicate, terminal: false },
    Operator { name: "map", takes: Takes::Predicate, terminal: false },
    Operator { name: "display", takes: Takes::Words, terminal: true },
    Operator { name: "format", takes: Takes::Words, terminal: false },
    Operator { name: "last", takes: Takes::Words, terminal: false },
    Operator { name: "skip", takes: Takes::Words, terminal: false },
    Operator { name: "dedupe", takes: Takes::Nothing, terminal: false },
    Operator { name: "take", takes: Takes::Words, terminal: false },
    Operator { name: "count", takes: Takes::Nothing, terminal: false },
    Operator { name: "sum", takes: Takes::Words, terminal: false },
    Operator { name: "min", takes: Takes::Words, terminal: false },
    Operator { name: "max", takes: Takes::Words, terminal: false },
    Operator { name: "avg", takes: Takes::Words, terminal: false },
    Operator { name: "reduce", takes: Takes::Predicate, terminal: false },
    Operator { name: "split", takes: Takes::Words, terminal: false },
    Operator { name: "join", takes: Takes::Words, terminal: false },
    Operator { name: "trim", takes: Takes::Nothing, terminal: false },
    Operator { name: "replace", takes: Takes::Words, terminal: false },
    Operator { name: "upper", takes: Takes::Nothing, terminal: false },
    Operator { name: "lower", takes: Takes::Nothing, terminal: false },
    Operator { name: "keys", takes: Takes::Nothing, terminal: false },
    Operator { name: "values", takes: Takes::Nothing, terminal: false },
    Operator { name: "merge", takes: Takes::Record, terminal: false },
    Operator { name: "round", takes: Takes::Nothing, terminal: false },
    Operator { name: "floor", takes: Takes::Nothing, terminal: false },
    Operator { name: "ceil", takes: Takes::Nothing, terminal: false },
    Operator { name: "trunc", takes: Takes::Nothing, terminal: false },
    Operator { name: "abs", takes: Takes::Nothing, terminal: false },
    Operator { name: "capture", takes: Takes::Words, terminal: false },
];

/// The operator called `name`, if one is.
pub fn find(name: &str) -> Option<&'static Operator> {
    OPERATORS.iter().find(|o| o.name == name)
}

/// Whether `name` is an operator's.
pub fn is_operator(name: &str) -> bool {
    find(name).is_some()
}
