//! `help`, a builtin: what an operator, a builtin or a keyword is and does, from the tables the
//! shell itself reads, so it cannot describe another shell than the one running.

use crate::ast::{Arg, Call, Expr};
use crate::builtins::{self, BUILTINS};
use crate::eval::{EvalError, Interp};
use crate::operators::{self, Group, OPERATORS, Operator};
use crate::value::Val;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// A keyword's entry.
#[derive(Copy, Clone, Debug)]
pub struct Entry {
    /// The names it answers to: `if` and `else` share one.
    pub names: &'static [&'static str],
    /// How it is written.
    pub signature: &'static str,
    /// What it does, in a line.
    pub does: &'static str,
    /// Its use at a prompt, as `operators::Operator::example`; empty where none can run.
    pub example: &'static str,
    /// What the example prints.
    pub output: &'static str,
}

const fn keyword(
    names: &'static [&'static str],
    signature: &'static str,
    does: &'static str,
    example: &'static str,
    output: &'static str,
) -> Entry {
    Entry { names, signature, does, example, output }
}

/// The keywords (§3), `time`'s prefix among them.
pub const KEYWORDS: &[Entry] = &[
    keyword(&["let"], "let NAME = VALUE", "Bind a name, which cannot be assigned again.", "let n = 4\nn * 2", "8\n"),
    keyword(
        &["mut"],
        "mut NAME = VALUE",
        "Bind a name that can be assigned again, and its fields.",
        "mut n = 1\nn = n + 1\nn",
        "2\n",
    ),
    keyword(
        &["const"],
        "const NAME = VALUE",
        "A name not to be bound again where it is; pub offers it.",
        "const LIMIT = 10\nLIMIT * 2",
        "20\n",
    ),
    keyword(
        &["pub"],
        "pub def ... | pub const ...",
        "Offer a def or a const to the scripts that use this one.",
        "",
        "",
    ),
    keyword(
        &["if", "else"],
        "if CONDITION { ... } else { ... }",
        "Run one block or the other. The condition must be a Bool.",
        "if 2 > 1 { \"yes\" } else { \"no\" }",
        "yes\n",
    ),
    keyword(
        &["for"],
        "for NAME in VALUE { ... }",
        "Run the block once for each row or value.",
        "mut total = 0\nfor n in 1..4 { total = total + n }\ntotal",
        "6\n",
    ),
    keyword(
        &["in"],
        "VALUE in SEQUENCE",
        "Whether a list, a string or a range holds VALUE; and for's.",
        "3 in [1, 2, 3]",
        "true\n",
    ),
    keyword(
        &["while"],
        "while CONDITION { ... }",
        "Run the block for as long as the condition holds.",
        "mut n = 1\nwhile n < 50 { n = n * 2 }\nn",
        "64\n",
    ),
    keyword(
        &["break", "continue"],
        "break | continue",
        "Leave a loop, or go on to its next turn.",
        "mut last = 0\nfor n in 1..10 { if n == 4 { break } else { last = n } }\nlast",
        "3\n",
    ),
    keyword(
        &["def"],
        "def NAME(PARAMETER, ...) { ... }",
        "A function, called as NAME(ARGUMENT, ...), or in a pipe with _.",
        "def twice(n) { n * 2 }\ntwice(21)",
        "42\n",
    ),
    keyword(
        &["return"],
        "return [VALUE]",
        "Leave a function with VALUE, or with null.",
        "def sign(n) { if n < 0 { return \"-\" } else { \"+\" } }\nsign(-2)",
        "-\n",
    ),
    keyword(
        &["fail"],
        "fail MESSAGE",
        "Fail with MESSAGE, as a failing command does, for try to catch.",
        "try { fail \"no\" } catch (e) { e.message }",
        "no\n",
    ),
    keyword(
        &["try", "catch"],
        "try { ... } catch [(NAME)] { ... }",
        "The block's value, or if it fails the catch block's; NAME is why.",
        "try { \"x\" | parse Int } catch { 0 }",
        "0\n",
    ),
    keyword(&["strict"], "strict { ... }", "In the block, a pipeline stops at its first failing stage.", "", ""),
    keyword(
        &["match"],
        "match VALUE { PATTERN => VALUE ... }",
        "The first arm whose pattern fits: a type, a record, a range, _.",
        "match 7 { 0..5 => \"low\"  _ => \"high\" }",
        "high\n",
    ),
    keyword(
        &["use", "as"],
        "use \"PATH\" { NAME, ... } | use \"PATH\" as NAME",
        "Take the names another script offers with pub.",
        "",
        "",
    ),
    keyword(
        &["expect"],
        "... | expect TYPE",
        "Pass the value on if it is a TYPE, and fail if not.",
        "[1, 2] | expect List<Int> | count",
        "2\n",
    ),
    keyword(
        &["assert"],
        "... | assert CONDITION",
        "Pass the value on if the condition, asked of it, holds.",
        "[1, 2] | assert (count > 1) | count",
        "2\n",
    ),
    keyword(&["parse"], "... | parse TYPE", "Read text as a TYPE, or fail.", "\"42\" | parse Int", "42\n"),
    keyword(&["true", "false"], "true | false", "The two Bools, which a condition must be.", "1 < 2", "true\n"),
    keyword(&["null"], "null", "No value; ?? gives one in its place.", "null ?? \"none\"", "none\n"),
    keyword(
        &["_"],
        "_",
        "Where a pipe's value goes in a call; in match, anything.",
        "def twice(n) { n * 2 }\n21 | twice(_)",
        "42\n",
    ),
    keyword(&["time"], "time PIPELINE", "Run it as ever, then say how long it took and what the disks did.", "", ""),
];

/// `help` alone: every operator by group, the builtins and the keywords.
pub fn listing() -> String {
    let mut out = String::from("Operators work on the value a pipe carries:\n");
    for g in Group::ALL {
        let names: Vec<&str> = OPERATORS.iter().filter(|o| o.group == g).map(|o| o.name).collect();
        out.push_str(&format!("  {:<12}{}\n", g.name(), names.join(" ")));
    }
    let builtins: Vec<&str> = BUILTINS.iter().map(|b| b.name).collect();
    out.push_str(&format!("Builtins act on the shell: {}\nKeywords:\n", builtins.join(" ")));
    let mut line = String::from(" ");
    for name in KEYWORDS.iter().flat_map(|k| k.names.iter()) {
        if line.len() + 1 + name.len() > 78 {
            out.push_str(&line);
            out.push('\n');
            line = String::from(" ");
        }
        line.push(' ');
        line.push_str(name);
    }
    out.push_str(&line);
    out.push_str(
        "\n`help NAME` says what one does, and `man shell` is a tour of the shell;\n\
         `man shell-operators` shows every operator, and `man shell-language` the rules.",
    );
    out
}

/// One entry as `help NAME` prints it: how it is written, what it does, its example, and where
/// more is said.
pub fn render(signature: &str, does: &str, example: &str, output: &str, more: &str) -> String {
    let mut out = format!("{signature}\n  {does}\n");
    if !example.is_empty() {
        out.push('\n');
        for line in example.lines() {
            out.push_str(&format!("  > {line}\n"));
        }
        for line in output.lines() {
            out.push_str(&format!("  {line}\n"));
        }
    }
    out.push_str(&format!("\n  {more}"));
    out
}

fn operator(o: &Operator) -> String {
    let more = format!("An operator, among the {}: `man shell-operators`.", o.group.name());
    render(o.signature, o.does, o.example, o.output, &more)
}

/// An operator's, a builtin's or a keyword's entry, as `help NAME` prints it, if `name` has one.
pub fn entry(name: &str) -> Option<String> {
    let more = "the rules are in `man shell-language`.";
    if let Some(o) = operators::find(name) {
        Some(operator(o))
    } else if let Some(b) = builtins::find(name) {
        Some(render(b.signature, b.does, b.example, b.output, &format!("A builtin; {more}")))
    } else {
        let k = KEYWORDS.iter().find(|k| k.names.contains(&name))?;
        Some(render(k.signature, k.does, k.example, k.output, &format!("A keyword; {more}")))
    }
}

/// What `help NAME` says of a name bound here, to `v`.
pub fn bound(name: &str, v: &Val) -> String {
    match v {
        Val::Func(_) => format!("`{name}` is a function defined here, not one of the shell's."),
        _ => format!("`{name}` is a name bound here; its value is of type {}.", v.type_name()),
    }
}

/// What `help NAME` says of a program in `/bin`: its page, if the manual has one, and if not, its
/// `--help`, which every program answers.
pub fn program(name: &str, page: bool) -> String {
    match page {
        true => format!("`{name}` is a program, not the shell's: `man {name}` is its page."),
        false => format!("`{name}` is a program, not the shell's: `{name} --help` says what it takes."),
    }
}

/// Where a program's page is, if it has one: section 1's, flat, as the manual is projected.
pub fn page_path(name: &str) -> String {
    format!("/system/manual/{name}.1.md")
}

impl Interp {
    /// The `help` builtin: the shell's own tables first, then the names bound here, and only then
    /// `/bin`, which is a listing to ask for.
    pub(crate) fn help(&mut self, c: &Call) -> Result<Val, EvalError> {
        let name = match c.args.as_slice() {
            [] => return Ok(Val::str(listing())),
            [Arg::Positional(Expr::Word(n) | Expr::Str(n))] => n.as_str(),
            _ => return Err(EvalError::new("`help` takes one name, or none")),
        };
        if let Some(text) = entry(name) {
            return Ok(Val::str(text));
        }
        if let Some(v) = self.lookup_any(name) {
            return Ok(Val::str(bound(name, &v)));
        }
        if self.host.commands().iter().any(|p| p == name) {
            let page = self.host.exists(&page_path(name));
            return Ok(Val::str(program(name, page)));
        }
        Err(EvalError::new(format!(
            "nothing here is called `{name}`: `help` lists the shell's own, and `man --keyword {name}` \
             searches the manual"
        )))
    }
}
