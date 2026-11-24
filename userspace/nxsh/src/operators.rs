//! The shell's generic value operators (§5c, §10b), an entry each: how the parser reads its
//! arguments, and what `help` and `shell-operators(7)` say of it.

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
    /// Paths, read as a builtin's are, in word mode: `open hello.txt` names a file.
    Paths,
}

/// An operator's family, as `help` lists them and the page heads them.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Group {
    /// Narrowing, ordering and walking what a pipe carries.
    Sequences,
    /// Many values made one.
    Reducing,
    /// Text taken apart and put together.
    Strings,
    /// A record's fields.
    Records,
    /// Floats made Ints, and signs dropped.
    Numbers,
    /// Files read and written, and a value shown.
    InAndOut,
}

impl Group {
    /// Every group, in the order `help` lists them.
    pub const ALL: [Group; 6] =
        [Group::Sequences, Group::Reducing, Group::Strings, Group::Records, Group::Numbers, Group::InAndOut];

    /// Its name, as `help` lists it.
    pub fn name(self) -> &'static str {
        match self {
            Group::Sequences => "sequences",
            Group::Reducing => "reducing",
            Group::Strings => "strings",
            Group::Records => "records",
            Group::Numbers => "numbers",
            Group::InAndOut => "in and out",
        }
    }

    /// What its operators have in common, which the page says under its heading.
    pub fn about(self) -> &'static str {
        match self {
            Group::Sequences => {
                "A table's rows, a list's values, a string's characters and a range's values are each a \
                 sequence. A table, a list or a string comes back as it went in, a string filtered a \
                 string; a range comes back a list; and `map` makes records a table."
            }
            Group::Reducing => {
                "Many values made one. Of nothing, `count` and `sum` are 0, `reduce --from` its seed, \
                 and the rest are errors."
            }
            Group::Strings => "Text taken apart and put together. `upper` and `lower` change ASCII letters only.",
            Group::Records => "A record's fields, in the order its schema has them.",
            Group::Numbers => "A Float made an Int, each named by which way it loses information.",
            Group::InAndOut => "Files read and written, and a value shown. `save` and `display` end a chain.",
        }
    }
}

/// One operator: how it reads, and what `help` says of it.
#[derive(Copy, Clone, Debug)]
pub struct Operator {
    /// What it is called.
    pub name: &'static str,
    /// Its family.
    pub group: Group,
    /// How its arguments read.
    pub takes: Takes,
    /// It consumes its value and ends the chain, so the REPL adds no `display` after it.
    pub terminal: bool,
    /// How it is written.
    pub signature: &'static str,
    /// What it does, in a line.
    pub does: &'static str,
    /// Its use at a prompt, a line each, of which only the last prints. The tests run it.
    pub example: &'static str,
    /// What the example prints.
    pub output: &'static str,
}

const fn op(
    name: &'static str,
    (group, takes, terminal): (Group, Takes, bool),
    signature: &'static str,
    does: &'static str,
    example: &'static str,
    output: &'static str,
) -> Operator {
    Operator { name, group, takes, terminal, signature, does, example, output }
}

const SEQUENCE: (Group, Takes, bool) = (Group::Sequences, Takes::Words, false);
const REDUCING: (Group, Takes, bool) = (Group::Reducing, Takes::Words, false);
const STRING: (Group, Takes, bool) = (Group::Strings, Takes::Words, false);
const NUMBER: (Group, Takes, bool) = (Group::Numbers, Takes::Nothing, false);

/// Generic value operators (§3, §5c, §10b). Closed at parse time because it selects the
/// argument grammar; a user-defined `def` is called with parens, so it is distinct.
/// The bitwise family would join: `|`, `&` and `^` are spoken for.
/// TODO(shell-bitwise): `docs/rationale/deferred-decisions.md`.
pub const OPERATORS: &[Operator] = &[
    op(
        "filter",
        (Group::Sequences, Takes::Predicate, false),
        "filter PREDICATE",
        "Keep what a predicate holds for: a comparison, or a closure.",
        "[{n: 1}, {n: 5}, {n: 9}] | filter n > 2",
        "[{ n: 5 }, { n: 9 }]\n",
    ),
    op(
        "select",
        SEQUENCE,
        "select COLUMN...",
        "Keep only the columns named, in the order named.",
        "[{a: 1, b: 2, c: 3}] | select c a",
        "c  a\n3  1\n",
    ),
    op(
        "sort",
        SEQUENCE,
        "sort [COLUMN...] [--reverse]",
        "Order by each column named in turn, or by the values themselves.",
        "[\"fig\", \"pear\", \"apple\"] | sort --reverse",
        "[\"pear\", \"fig\", \"apple\"]\n",
    ),
    op("take", SEQUENCE, "take N", "The first N.", "\"nitrox\" | take 3", "nit\n"),
    op("skip", SEQUENCE, "skip N", "All but the first N.", "\"nitrox\" | skip 3", "rox\n"),
    op("last", SEQUENCE, "last N", "The last N.", "1..10 | last 3", "[7, 8, 9]\n"),
    op(
        "dedupe",
        (Group::Sequences, Takes::Nothing, false),
        "dedupe",
        "Drop each one equal to one before it, keeping the order.",
        "[1, 2, 1, 3, 2] | dedupe",
        "[1, 2, 3]\n",
    ),
    op(
        "each",
        (Group::Sequences, Takes::Predicate, false),
        "each { |it| ... }",
        "Run a closure on each, for what it does, and pass them on unchanged.",
        "[1, 2] | each { |n| display n * 10 }",
        "10\n20\n[1, 2]\n",
    ),
    op(
        "map",
        (Group::Sequences, Takes::Predicate, false),
        "map { |it| ... }",
        "Each made what the closure returns; records make a table.",
        "1..4 | map { |n| {n: n, square: n * n} }",
        "n  square\n1  1\n2  4\n3  9\n",
    ),
    op("count", (Group::Reducing, Takes::Nothing, false), "count", "How many there are.", "\"nitrox\" | count", "6\n"),
    op(
        "sum",
        REDUCING,
        "sum [COLUMN]",
        "The total of a column, or of the values.",
        "[{size: 3}, {size: 4}] | sum size",
        "7\n",
    ),
    op(
        "min",
        REDUCING,
        "min [COLUMN]",
        "The least: strings in order, and mixed types refused.",
        "[\"pear\", \"fig\"] | min",
        "fig\n",
    ),
    op("max", REDUCING, "max [COLUMN]", "The greatest, as `min` orders.", "[2, 9, 4] | max", "9\n"),
    op("avg", REDUCING, "avg [COLUMN]", "The mean, a Float even of Ints.", "[1, 2] | avg", "1.5\n"),
    op(
        "reduce",
        (Group::Reducing, Takes::Predicate, false),
        "reduce [--from SEED] { |acc, it| ... }",
        "Fold them into one value, starting from the first or from SEED.",
        "[\"a\", \"b\", \"c\"] | reduce --from \">\" { |acc, it| acc ++ it }",
        ">abc\n",
    ),
    op(
        "split",
        STRING,
        "split SEPARATOR",
        "A string cut at each separator, as a list.",
        "\"a,b,c\" | split \",\"",
        "[\"a\", \"b\", \"c\"]\n",
    ),
    op(
        "join",
        STRING,
        "join SEPARATOR",
        "A list's values as one string, the separator between.",
        "[\"a\", \"b\", \"c\"] | join \"-\"",
        "a-b-c\n",
    ),
    op(
        "trim",
        (Group::Strings, Takes::Nothing, false),
        "trim",
        "Without leading and trailing whitespace.",
        "\"  x  \" | trim",
        "x\n",
    ),
    op(
        "replace",
        STRING,
        "replace FROM TO",
        "Every FROM made TO, as written: not a pattern.",
        "\"a-b-c\" | replace \"-\" \"+\"",
        "a+b+c\n",
    ),
    op("upper", (Group::Strings, Takes::Nothing, false), "upper", "In capitals.", "\"Nitrox\" | upper", "NITROX\n"),
    op(
        "lower",
        (Group::Strings, Takes::Nothing, false),
        "lower",
        "In small letters.",
        "\"Nitrox\" | lower",
        "nitrox\n",
    ),
    op(
        "capture",
        STRING,
        "capture /PATTERN/",
        "The match and each group's, as a list, or null if it does not match.",
        r#""10-20" | capture /(\d+)-(\d+)/"#,
        "[\"10-20\", \"10\", \"20\"]\n",
    ),
    op(
        "format",
        STRING,
        "format(TEMPLATE, VALUE...)",
        "Each {} filled with the next value; in a pipe, its value first.",
        "format(\"{} of {}\", 3, 4)",
        "3 of 4\n",
    ),
    op(
        "keys",
        (Group::Records, Takes::Nothing, false),
        "keys",
        "A record's field names.",
        "{name: \"a\", size: 3} | keys",
        "[\"name\", \"size\"]\n",
    ),
    op(
        "values",
        (Group::Records, Takes::Nothing, false),
        "values",
        "A record's values.",
        "{name: \"a\", size: 3} | values",
        "[\"a\", 3]\n",
    ),
    op(
        "merge",
        (Group::Records, Takes::Record, false),
        "merge RECORD",
        "Two records as one, RECORD's value kept where both have a field.",
        "{a: 1, b: 2} | merge {b: 3, c: 4}",
        "{ a: 1, b: 3, c: 4 }\n",
    ),
    op("round", NUMBER, "round", "To the nearest Int, a half away from zero.", "2.5 | round", "3\n"),
    op("floor", NUMBER, "floor", "To the Int at or below.", "-1.5 | floor", "-2\n"),
    op("ceil", NUMBER, "ceil", "To the Int at or above.", "1.2 | ceil", "2\n"),
    op("trunc", NUMBER, "trunc", "To the Int toward zero.", "-1.7 | trunc", "-1\n"),
    op("abs", NUMBER, "abs", "Without its sign.", "-3 | abs", "3\n"),
    op(
        "open",
        (Group::InAndOut, Takes::Paths, false),
        "open PATH...",
        "Files read as one stream: a .tsm as itself, a .txt a line a row.",
        "1..4 | map { |n| {n: n} } | save ./n.tsm\nopen ./n.tsm | sum n",
        "6\n",
    ),
    op(
        "save",
        (Group::InAndOut, Takes::Paths, true),
        "save PATH",
        "The value written to PATH: a table as .tsm, or a line a row as .txt.",
        "[\"one\", \"two\"] | save ./words.txt\nopen ./words.txt",
        "line\none\ntwo\n",
    ),
    op(
        "display",
        (Group::InAndOut, Takes::Words, true),
        "display [VALUE]",
        "Show a value, a table in columns, as the prompt shows what is left.",
        "[1, 2] | display",
        "[1, 2]\n",
    ),
];

/// The operator called `name`, if one is.
pub fn find(name: &str) -> Option<&'static Operator> {
    OPERATORS.iter().find(|o| o.name == name)
}

/// Whether `name` is an operator's.
pub fn is_operator(name: &str) -> bool {
    find(name).is_some()
}
