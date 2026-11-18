//! `help`, a builtin: what an operator, a builtin or a keyword is and does, from the tables the
//! shell itself reads, so it cannot describe another shell than the one running.

use crate::ast::{Arg, Call, Expr};
use crate::eval::{EvalError, Interp};
use crate::operators::{self, Group, OPERATORS, Operator};
use crate::value::Val;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// A builtin's or a keyword's entry.
#[derive(Copy, Clone, Debug)]
pub struct Entry {
    /// The names it answers to: `if` and `else` share one.
    pub names: &'static [&'static str],
    /// How it is written.
    pub signature: &'static str,
    /// What it does, in a line.
    pub does: &'static str,
    /// Its use at a prompt, a line each, of which only the last prints; empty where none can run,
    /// as `exit`'s. The tests run it.
    pub example: &'static str,
    /// What the example prints.
    pub output: &'static str,
}

const fn entry(
    names: &'static [&'static str],
    signature: &'static str,
    does: &'static str,
    example: &'static str,
    output: &'static str,
) -> Entry {
    Entry { names, signature, does, example, output }
}

/// The shell-state builtins (§3): what acts on the shell itself, as no program can.
pub const BUILTINS: &[Entry] = &[
    entry(
        &["cd"],
        "cd [PATH]",
        "Go to PATH, or to HOME; the directory changes only if PATH is real.",
        "cd /\n$env.PWD",
        "/\n",
    ),
    entry(&["exit"], "exit [STATUS]", "Leave the shell; a script ends with STATUS, or 0.", "", ""),
    entry(&["help"], "help [NAME]", "What an operator, a builtin or a keyword does, or a list of all.", "", ""),
];

/// The keywords (§3), `time`'s prefix among them.
pub const KEYWORDS: &[Entry] = &[
    entry(&["let"], "let NAME = VALUE", "Bind a name, which cannot be assigned again.", "let n = 4\nn * 2", "8\n"),
    entry(
        &["mut"],
        "mut NAME = VALUE",
        "Bind a name that can be assigned again, and its fields.",
        "mut n = 1\nn = n + 1\nn",
        "2\n",
    ),
    entry(
        &["const"],
        "const NAME = VALUE",
        "A name fixed when the script is read.",
        "const LIMIT = 10\nLIMIT * 2",
        "20\n",
    ),
    entry(&["pub"], "pub def ... | pub const ...", "Offer a def or a const to the scripts that use this one.", "", ""),
    entry(
        &["if", "else"],
        "if CONDITION { ... } else { ... }",
        "Run one block or the other. The condition must be a Bool.",
        "if 2 > 1 { \"yes\" } else { \"no\" }",
        "yes\n",
    ),
    entry(
        &["for"],
        "for NAME in VALUE { ... }",
        "Run the block once for each row, value or character.",
        "mut total = 0\nfor n in 1..4 { total = total + n }\ntotal",
        "6\n",
    ),
    entry(
        &["in"],
        "VALUE in SEQUENCE",
        "Whether a list, a string or a range holds VALUE; and for's.",
        "3 in [1, 2, 3]",
        "true\n",
    ),
    entry(
        &["while"],
        "while CONDITION { ... }",
        "Run the block for as long as the condition holds.",
        "mut n = 1\nwhile n < 50 { n = n * 2 }\nn",
        "64\n",
    ),
    entry(
        &["break", "continue"],
        "break | continue",
        "Leave a loop, or go on to its next turn.",
        "mut last = 0\nfor n in 1..10 { if n == 4 { break } else { last = n } }\nlast",
        "3\n",
    ),
    entry(
        &["def"],
        "def NAME(PARAMETER, ...) { ... }",
        "A function, called as NAME(ARGUMENT, ...), or in a pipe with _.",
        "def twice(n) { n * 2 }\ntwice(21)",
        "42\n",
    ),
    entry(
        &["return"],
        "return [VALUE]",
        "Leave a function with VALUE, or with null.",
        "def sign(n) { if n < 0 { return \"-\" } else { \"+\" } }\nsign(-2)",
        "-\n",
    ),
    entry(
        &["fail"],
        "fail MESSAGE",
        "Fail with MESSAGE, as a failing command does, for try to catch.",
        "try { fail \"no\" } catch (e) { e.message }",
        "no\n",
    ),
    entry(
        &["try", "catch"],
        "try { ... } catch [(NAME)] { ... }",
        "The block's value, or if it fails the catch block's; NAME is why.",
        "try { \"x\" | parse Int } catch { 0 }",
        "0\n",
    ),
    entry(&["strict"], "strict { ... }", "In the block, a pipeline stops at its first failing stage.", "", ""),
    entry(
        &["match"],
        "match VALUE { PATTERN => VALUE ... }",
        "The first arm whose pattern fits: a type, a record, a range, _.",
        "match 7 { 0..5 => \"low\"  _ => \"high\" }",
        "high\n",
    ),
    entry(
        &["use", "as"],
        "use \"PATH\" { NAME, ... } | use \"PATH\" as NAME",
        "Take the names another script offers with pub.",
        "",
        "",
    ),
    entry(
        &["expect"],
        "... | expect TYPE",
        "Pass the value on if it is a TYPE, and fail if not.",
        "[1, 2] | expect List<Int> | count",
        "2\n",
    ),
    entry(
        &["assert"],
        "... | assert CONDITION",
        "Pass the value on if the condition, asked of it, holds.",
        "[1, 2] | assert (count > 1) | count",
        "2\n",
    ),
    entry(&["parse"], "... | parse TYPE", "Read text as a TYPE, or fail.", "\"42\" | parse Int", "42\n"),
    entry(&["true", "false"], "true | false", "The two Bools, which a condition must be.", "1 < 2", "true\n"),
    entry(&["null"], "null", "No value; ?? gives one in its place.", "null ?? \"none\"", "none\n"),
    entry(
        &["_"],
        "_",
        "Where a pipe's value goes in a call; in match, anything.",
        "def twice(n) { n * 2 }\n21 | twice(_)",
        "42\n",
    ),
    entry(&["time"], "time PIPELINE", "Run it as ever, then say how long it took and what the disks did.", "", ""),
];

/// Whether `name` is a builtin's.
pub fn is_builtin(name: &str) -> bool {
    BUILTINS.iter().any(|b| b.names.contains(&name))
}

fn find(table: &'static [Entry], name: &str) -> Option<&'static Entry> {
    table.iter().find(|e| e.names.contains(&name))
}

/// `help` alone: every operator by group, the builtins and the keywords.
pub fn listing() -> String {
    let mut out = String::from("Operators work on the value a pipe carries:\n");
    for g in Group::ALL {
        let names: Vec<&str> = OPERATORS.iter().filter(|o| o.group == g).map(|o| o.name).collect();
        out.push_str(&format!("  {:<12}{}\n", g.name(), names.join(" ")));
    }
    let builtins: Vec<&str> = BUILTINS.iter().flat_map(|b| b.names.iter().copied()).collect();
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
        "\n`help NAME` says what one does. `man shell-operators` shows every operator,\n\
         and `man shell-language` is the whole language.",
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

/// What `help NAME` says: an operator's, a builtin's or a keyword's entry; for a program, its page;
/// and for anything else, an error.
pub fn about(name: &str, is_program: bool) -> Result<String, String> {
    if let Some(o) = operators::find(name) {
        return Ok(operator(o));
    }
    for (table, what) in [(BUILTINS, "A builtin"), (KEYWORDS, "A keyword")] {
        if let Some(e) = find(table, name) {
            let more = format!("{what}; the rules are in `man shell-language`.");
            return Ok(render(e.signature, e.does, e.example, e.output, &more));
        }
    }
    if is_program {
        return Ok(format!("`{name}` is a program, not the shell's: `man {name}` is its page."));
    }
    Err(format!(
        "nothing here is called `{name}`: `help` lists the shell's own, and `man --keyword {name}` \
         searches the manual"
    ))
}

impl Interp {
    /// The `help` builtin.
    pub(crate) fn help(&mut self, c: &Call) -> Result<Val, EvalError> {
        let name = match c.args.as_slice() {
            [] => return Ok(Val::str(listing())),
            [Arg::Positional(Expr::Word(n) | Expr::Ident(n) | Expr::Str(n))] => n.as_str(),
            [Arg::Positional(Expr::Underscore)] => "_",
            _ => return Err(EvalError::new("`help` takes one name, or none")),
        };
        let is_program = self.host.commands().iter().any(|p| p == name);
        about(name, is_program).map(Val::str).map_err(EvalError::new)
    }
}
