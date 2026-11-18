//! The operators' table and `help`'s entries: each example prints what its entry says, the
//! dispatch knows each name and no other, and `shell-operators(7)` is the table.

use crate::ast::{BinOp, Expr, Stmt};
use crate::builtins::BUILTINS;
use crate::eval::{Interp, Mode};
use crate::help::{self, Entry, KEYWORDS};
use crate::host::MockHost;
use crate::operators::{Group, OPERATORS, Takes};
use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// What `example` prints at a prompt in `/home`, line by line: an error if a line fails, or if one
/// before the last prints, as the entries show only the last line's.
fn run(example: &str) -> Result<String, String> {
    let host = MockHost::new().with_dir("/").with_dir("/home");
    let log = host.log();
    let mut i = Interp::with_host(Box::new(host), Mode::Repl);
    i.run_line("cd /home").map_err(|e| e.message)?;
    let lines: Vec<&str> = example.lines().collect();
    let mut printed = String::new();
    for (n, line) in lines.iter().enumerate() {
        let before = log.borrow().output.len();
        let shown = i.run_line(line).map_err(|e| format!("`{line}` failed: {}", e.message))?;
        let mut out = log.borrow().output[before..].concat();
        out.push_str(shown.as_deref().unwrap_or(""));
        if n + 1 < lines.len() && !out.is_empty() {
            return Err(format!("`{line}` printed {out:?}, before the last line"));
        }
        printed.push_str(&out);
    }
    Ok(printed)
}

/// Every entry as (name, signature, does, example, output).
fn entries() -> Vec<(&'static str, &'static str, &'static str, &'static str, &'static str)> {
    let keywords = KEYWORDS.iter().map(|e: &Entry| (e.names[0], e.signature, e.does, e.example, e.output));
    OPERATORS
        .iter()
        .map(|o| (o.name, o.signature, o.does, o.example, o.output))
        .chain(BUILTINS.iter().map(|b| (b.name, b.signature, b.does, b.example, b.output)))
        .chain(keywords)
        .collect()
}

#[test]
fn every_example_prints_what_its_entry_says() {
    let mut wrong = Vec::new();
    for (name, _, _, example, output) in entries().into_iter().filter(|e| !e.3.is_empty()) {
        match run(example) {
            Ok(got) if got == output => {}
            Ok(got) => wrong.push(format!("{name}: printed {got:?}, its entry says {output:?}")),
            Err(e) => wrong.push(format!("{name}: {e}")),
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// Read over a serial line too, which is 80 columns and may drop what is not ASCII.
#[test]
fn every_operator_has_an_example_and_every_entry_fits_a_serial_line() {
    assert!(OPERATORS.iter().all(|o| !o.example.is_empty() && !o.output.is_empty()));
    for (name, signature, does, example, output) in entries() {
        let text = [signature, does, example, output].concat();
        assert!(text.is_ascii(), "{name}");
        assert!(signature.len() <= 78 && does.len() <= 76, "{name}");
        assert!(example.lines().chain(output.lines()).all(|l| l.len() <= 74), "{name}");
    }
}

#[test]
fn no_name_has_two_entries() {
    let names: Vec<&str> = OPERATORS
        .iter()
        .map(|o| o.name)
        .chain(BUILTINS.iter().map(|b| b.name))
        .chain(KEYWORDS.iter().flat_map(|e| e.names.iter().copied()))
        .collect();
    let once: BTreeSet<&str> = names.iter().copied().collect();
    assert_eq!(once.len(), names.len(), "{names:?}");
}

/// Every keyword the lexer knows, and `time`'s prefix, has an entry; and each entry's name is one.
#[test]
fn every_keyword_has_an_entry_and_every_entry_is_a_keyword() {
    let lexed: BTreeSet<&str> = crate::lex::KEYWORDS.iter().map(|(k, _)| *k).chain(["time"]).collect();
    let entered: BTreeSet<&str> = KEYWORDS.iter().flat_map(|e| e.names.iter().copied()).collect();
    assert_eq!(lexed, entered);
}

/// The names `fn_name`'s match arms name in `eval.rs`: each arm's patterns, `"a" | "b" =>`.
fn arms(fn_name: &str) -> BTreeSet<String> {
    let src = include_str!("eval.rs");
    let body = &src[src.find(&format!("fn {fn_name}(")).expect("the function")..];
    let body = &body[..body[1..].find("\n    fn ").expect("a function after it")];
    let mut named = BTreeSet::new();
    for line in body.lines().map(str::trim_start).filter(|l| l.starts_with('"') && l.contains("=>")) {
        let pattern = line.split("=>").next().unwrap_or_default();
        for p in pattern.split('|').map(str::trim) {
            if let Some(name) = p.strip_prefix('"').and_then(|p| p.strip_suffix('"')) {
                named.insert(name.to_string());
            }
        }
    }
    named
}

/// The parser makes an operator or a builtin only of a name its table has, so an arm for a name the
/// table lacks could never run, and a name with no arm would fail every use.
#[test]
fn the_dispatch_knows_each_name_in_the_tables_and_no_other() {
    let operators: BTreeSet<String> = OPERATORS.iter().map(|o| o.name.to_string()).collect();
    assert_eq!(arms("apply_operator"), operators);
    let builtins: BTreeSet<String> = BUILTINS.iter().map(|b| b.name.to_string()).collect();
    assert_eq!(arms("apply_builtin"), builtins);
}

fn repl(host: MockHost, line: &str) -> Result<Option<String>, String> {
    Interp::with_host(Box::new(host), Mode::Repl).run_line(line).map_err(|e| e.message)
}

/// The operators that take nothing (§10b) leave what follows them to the expression, so
/// `(count > 0)` is a comparison and not `count` given `> 0`. Named here, not read from the table
/// the test holds.
#[test]
fn an_operator_that_takes_nothing_ends_at_its_name() {
    let none =
        ["count", "dedupe", "trim", "upper", "lower", "keys", "values", "round", "floor", "ceil", "trunc", "abs"];
    for name in none {
        let script = crate::parse::parse_script(&format!("({name} > 0)")).map_err(|e| e.message);
        assert!(
            matches!(script.as_ref().map(|s| &s.stmts[..]), Ok([Stmt::Expr(Expr::Binary(BinOp::Gt, ..))])),
            "{name}"
        );
    }
    let table: BTreeSet<&str> = OPERATORS.iter().filter(|o| o.takes == Takes::Nothing).map(|o| o.name).collect();
    assert_eq!(table, none.into_iter().collect());
}

#[test]
fn help_alone_lists_the_operators_builtins_and_keywords() {
    let listed = repl(MockHost::new(), "help").expect("runs").expect("prints");
    let words: BTreeSet<&str> = listed.split_whitespace().collect();
    let missing: Vec<&str> = entries().into_iter().map(|e| e.0).filter(|n| !words.contains(n)).collect();
    assert!(missing.is_empty(), "{missing:?} missing from {listed}");
    assert!(listed.lines().all(|l| l.len() <= 80), "{listed}");
    assert!(listed.contains("  in and out  open save display\n"), "by group: {listed}");
    assert_eq!(listed, format!("{}\n", help::listing()));
}

#[test]
fn help_name_prints_its_entry_and_for_a_program_its_page() {
    let filter = repl(MockHost::new(), "help filter").expect("runs").expect("prints");
    assert_eq!(
        filter,
        "filter PREDICATE\n  Keep what a predicate holds for: a comparison, or a closure.\n\n  \
         > [{n: 1}, {n: 5}, {n: 9}] | filter n > 2\n  [{ n: 5 }, { n: 9 }]\n\n  \
         An operator, among the sequences: `man shell-operators`.\n"
    );
    for (line, starts) in [
        ("help else", "if CONDITION"),
        ("help \"if\"", "if CONDITION"),
        ("help _", "_\n"),
        ("help cd", "cd [PATH]"),
        ("help time", "time"),
    ] {
        let said = repl(MockHost::new(), line).expect("runs").expect("prints");
        assert!(said.starts_with(starts) && said.contains("`man shell-language`"), "{line}: {said}");
    }
}

/// A program is sent to its page where the manual has one, and to its `--help`, which every
/// program answers, where it has none; `/bin` is listed only once no table has the name.
#[test]
fn help_for_a_program_names_its_page_or_its_help() {
    let host = || {
        let mut h = MockHost::new().with_file(&help::page_path("man"), "# man");
        h.commands.extend(["list".into(), "man".into()]);
        h
    };
    let said = |line: &str| repl(host(), line).expect("runs").expect("prints");
    assert_eq!(said("help man"), "`man` is a program, not the shell's: `man man` is its page.\n");
    assert_eq!(said("help list"), "`list` is a program, not the shell's: `list --help` says what it takes.\n");
    let h = host();
    let log = h.log();
    let _ = repl(h, "help filter");
    assert_eq!(log.borrow().listed, 0, "the tables answered, and /bin was listed anyway");
}

/// A function or a name bound here is said to be one, not "nothing here".
#[test]
fn help_for_a_name_bound_here_says_so() {
    let mut i = Interp::with_host(Box::new(MockHost::new()), Mode::Repl);
    for line in ["def twice(n) { n * 2 }", "let x = 2"] {
        i.run_line(line).expect("binds");
    }
    let said = |i: &mut Interp, line| i.run_line(line).map_err(|e| e.message);
    assert_eq!(
        said(&mut i, "help twice"),
        Ok(Some("`twice` is a function defined here, not one of the shell's.\n".into()))
    );
    assert_eq!(said(&mut i, "help x"), Ok(Some("`x` is a name bound here; its value is of type Int.\n".into())));
}

#[test]
fn help_for_nothing_known_or_two_names_is_an_error() {
    let unknown = repl(MockHost::new(), "help frobnicate").expect_err("nothing is called that");
    assert!(unknown.contains("`man --keyword frobnicate`"), "{unknown}");
    assert_eq!(repl(MockHost::new(), "help a b"), Err("`help` takes one name, or none".into()));
}

/// `help` can head a pipeline, its value going on; no builtin takes one.
#[test]
fn help_heads_a_pipe_but_no_builtin_is_fed_one() {
    let n = help::entry("trim").expect("an operator").chars().count();
    assert_eq!(repl(MockHost::new(), "help trim | count"), Ok(Some(format!("{n}\n"))));
    assert_eq!(repl(MockHost::new(), "3 | help"), Err("`help` is a builtin: nothing is piped into it".into()));
    let refused = "`cd` is a builtin, which acts on the shell: it takes no pipe, and heads none";
    assert_eq!(repl(MockHost::new().with_dir("/"), "[1] | cd /"), Err(refused.into()));
}

/// `cd` and `exit` act on the shell, so at a pipeline's head they are refused before they act: a
/// stage failing after them would come too late to undo a move or an exit.
#[test]
fn cd_or_exit_heading_a_pipe_is_refused_before_it_acts() {
    let mut i = Interp::with_host(Box::new(MockHost::new().with_dir("/").with_dir("/home")), Mode::Repl);
    i.run_line("cd /home").expect("goes");
    let refused = "`cd` is a builtin, which acts on the shell: it takes no pipe, and heads none";
    assert_eq!(i.run_line("cd / | count").map_err(|e| e.message), Err(refused.into()));
    assert_eq!(i.run_line("$env.PWD").map_err(|e| e.message), Ok(Some("/home\n".into())), "it moved");
    let exit = i.run_line("exit 3 | count").expect_err("refused");
    assert_eq!((exit.exit, exit.message.contains("`exit` is a builtin")), (None, true), "{}", exit.message);
}

/// A builtin's name at a statement's head is the builtin, given what follows as its words, as an
/// operator's is (§3); a name bound to the same word is read there alone, or in parentheses.
#[test]
fn a_binding_named_as_a_builtin_is_read_alone_or_in_parentheses() {
    let mut i = Interp::with_host(Box::new(MockHost::new()), Mode::Repl);
    i.run_line("let help = 2").expect("binds");
    let said = |i: &mut Interp, line| i.run_line(line).map_err(|e| e.message);
    assert_eq!(said(&mut i, "help"), Ok(Some("2\n".into())));
    assert_eq!(said(&mut i, "(help) + 1"), Ok(Some("3\n".into())));
    assert_eq!(said(&mut i, "help + 1"), Err("`help` takes one name, or none".into()));
}

/// What each group's heading says of its operators holds of them.
#[test]
fn the_groups_say_what_their_operators_do() {
    for (line, said) in [
        ("\"nitrox\" | filter { |c| c != \"t\" }", "nirox\n"),
        ("1..4 | filter { |n| n > 1 }", "[2, 3]\n"),
        ("[] | count", "0\n"),
        ("[] | sum", "0\n"),
        ("[] | reduce --from 7 { |a, it| a + it }", "7\n"),
    ] {
        assert_eq!(repl(MockHost::new(), line), Ok(Some(said.into())), "{line}");
    }
    for line in ["[] | min", "[] | max", "[] | avg", "[] | reduce { |a, it| a + it }"] {
        assert!(repl(MockHost::new(), line).is_err(), "{line}");
    }
}

const PAGE: &str = include_str!("../../../docs/manual/7/shell-operators.md");

const ABOUT: &str = "\
## Description

An operator works on the value a pipe carries, in the shell itself, where a program runs as a
process of its own: in `list /bin | filter size > 100000 | count`, `list` is a program, and
`filter` and `count` are operators. What passes between them is a value, typed: a table, a list,
a record, a string or a number.

An operator's arguments are its own. A bareword names a column, as in `sort size`; a predicate is
a comparison or a closure, `filter size > 100` or `filter { |it| it.size > 100 }`; and `--reverse`
is a flag. Each example below is run by the shell's tests, and prints what is shown under it.
`help NAME` says the same of one operator at the prompt.
";

const SEE_ALSO: &str = "
## See also

`shell-language(7)`, the language whole.
";

/// `text` broken into lines of at most 100 columns, as the docs are.
fn wrap(text: &str) -> String {
    let mut out = String::new();
    let mut width = 0;
    for word in text.split(' ') {
        if width > 0 && width + 1 + word.len() > 100 {
            out.push('\n');
            width = 0;
        } else if width > 0 {
            out.push(' ');
            width += 1;
        }
        out.push_str(word);
        width += word.len();
    }
    out
}

/// The page the table makes.
fn page() -> String {
    let mut out = String::from("# shell-operators — the shell's operators, each with an example\n\n");
    out.push_str(ABOUT);
    for g in Group::ALL {
        let name = g.name();
        out.push_str(&format!("\n## {}{}\n\n{}\n", name[..1].to_uppercase(), &name[1..], wrap(g.about())));
        for o in OPERATORS.iter().filter(|o| o.group == g) {
            out.push_str(&format!("\n### {}\n\n`{}`\n\n{}\n\n```\n", o.name, o.signature, o.does));
            for line in o.example.lines() {
                out.push_str(&format!("> {line}\n"));
            }
            out.push_str(o.output);
            out.push_str("```\n");
        }
    }
    out.push_str(SEE_ALSO);
    out
}

/// `NXSH_WRITE_PAGE=1` writes the page from the table, for a change to the table to be read in
/// the page's diff.
#[test]
fn the_page_is_the_table() {
    let want = page();
    if std::env::var_os("NXSH_WRITE_PAGE").is_some() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/manual/7/shell-operators.md");
        // Written, not compared: `PAGE` is the page as it was when this test was built.
        return std::fs::write(path, &want).expect("writes the page");
    }
    assert!(
        PAGE == want,
        "docs/manual/7/shell-operators.md is not the table: run this test with NXSH_WRITE_PAGE=1 and read the diff"
    );
}
