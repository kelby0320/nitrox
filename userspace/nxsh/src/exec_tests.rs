//! A program run by its path, and `$args`: parsing a path at a head, the program a stage asks for,
//! and the arguments a script is given.

use crate::ast::{Arg, CallKind, Expr, Stmt};
use crate::eval::{EvalError, Interp, Mode};
use crate::host::{MockHost, MockLog};
use crate::{Val, parse_script};
use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use libstream::wire::{Record, Schema, TypeModifiers, TypeTag, Value};

fn head(src: &str) -> Expr {
    let s = parse_script(src).unwrap_or_else(|e| panic!("`{src}` did not parse: {}", e.message));
    match &s.stmts[..] {
        [Stmt::Expr(e)] => e.clone(),
        other => panic!("expected one expression, got {other:?}"),
    }
}

fn call(e: &Expr) -> (&str, Vec<String>, bool) {
    let Expr::Call(c) = e else { panic!("not a call: {e:?}") };
    assert_eq!(c.kind, CallKind::External);
    let args = c
        .args
        .iter()
        .map(|a| match a {
            Arg::Positional(Expr::Word(w) | Expr::Ident(w) | Expr::Str(w)) | Arg::ShortFlags(w) => w.clone(),
            other => panic!("unexpected argument {other:?}"),
        })
        .collect();
    (&c.name, args, c.forced_external)
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| String::from(*s)).collect()
}

#[test]
fn a_path_at_a_head_is_an_external_call() {
    assert_eq!(call(&head("./tool x y")), ("./tool", strs(&["x", "y"]), false));
    assert_eq!(call(&head("../t")), ("../t", vec![], false));
    assert_eq!(call(&head("/bin/date -u")), ("/bin/date", strs(&["u"]), false));
    assert_eq!(call(&head("~/bin/x a")), ("~/bin/x", strs(&["a"]), false));
    assert_eq!(call(&head("./x")), ("./x", vec![], false));
    assert_eq!(call(&head("^./tool x")), ("./tool", strs(&["x"]), true));
}

#[test]
fn a_path_is_a_stage_of_a_pipeline() {
    let Expr::Pipeline(stages) = head("list | ./tool a") else { panic!("not a pipeline") };
    assert_eq!(call(&stages[1]), ("./tool", strs(&["a"]), false));
}

#[test]
fn a_path_anywhere_else_stays_a_word() {
    let s = parse_script("let p = ./x\nlet q = ~/y").expect("parses");
    for (st, want) in s.stmts.iter().zip(["./x", "~/y"]) {
        let Stmt::Bind { value, .. } = st else { panic!("not a bind") };
        assert_eq!(value, &Expr::Word(String::from(want)));
    }
    let Expr::Call(c) = head("open ./x") else { panic!("not a call") };
    assert_eq!(c.args, vec![Arg::Positional(Expr::Word(String::from("./x")))]);
    assert_eq!(head("~"), Expr::Word(String::from("~")));
}

fn env(pwd: Option<&str>, home: Option<&str>) -> Record {
    let mut schema = Schema::new();
    let mut values = Vec::new();
    for (k, v) in [("PWD", pwd), ("HOME", home)] {
        if let Some(v) = v {
            schema = schema.field(k, TypeTag::String, TypeModifiers::NONE);
            values.push(Value::Str(String::from(v)));
        }
    }
    Record { schema, values }
}

fn run(host: MockHost, pwd: Option<&str>, home: Option<&str>, src: &str) -> Result<Val, EvalError> {
    let mut i = Interp::with_host(Box::new(host), Mode::Script);
    i.set_env(env(pwd, home));
    i.run(&parse_script(src).expect("parses"))
}

fn spec_of(host: MockHost, pwd: Option<&str>, home: Option<&str>, src: &str) -> (String, Vec<String>) {
    let log: Rc<RefCell<MockLog>> = host.log();
    run(host, pwd, home, src).expect("runs");
    let s = log.borrow().runs[0][0].clone();
    (s.program, s.argv)
}

#[test]
fn a_path_head_runs_the_program_at_its_absolute_path() {
    let host = MockHost::new().with_program("/p/tool", None);
    assert_eq!(spec_of(host, Some("/p"), None, "./tool x"), (String::from("/p/tool"), strs(&["./tool", "x"])));

    let host = MockHost::new().with_program("/home/a/bin/x", None);
    let (program, argv) = spec_of(host, Some("/p"), Some("/home/a"), "~/bin/x");
    assert_eq!((program.as_str(), argv), ("/home/a/bin/x", strs(&["/home/a/bin/x"])));

    let host = MockHost::new().with_program("/p/bin/tool", None);
    assert_eq!(spec_of(host, Some("/p/sub"), None, "../bin/tool").0, "/p/bin/tool");

    let host = MockHost::new().with_program("/bin/date", None);
    assert_eq!(spec_of(host, None, None, "/bin/date").0, "/bin/date");
}

#[test]
fn a_name_is_handed_to_the_host_as_written() {
    let host = MockHost::new().with_program("ls", None);
    assert_eq!(spec_of(host, Some("/p"), None, "ls").0, "ls");
}

#[test]
fn a_relative_path_head_with_no_pwd_is_the_resolvers_error() {
    let e = run(MockHost::new(), None, None, "./tool").expect_err("no PWD");
    assert!(e.message.contains("`./tool`: this is a relative path and no working directory is set"), "{}", e.message);
}

fn args_of(args: Option<&[&str]>, src: &str) -> Result<Val, EvalError> {
    let mut i = Interp::with_host(Box::new(MockHost::new()), Mode::Script);
    i.set_env(env(Some("/p"), None));
    if let Some(a) = args {
        i.set_args(strs(a));
    }
    i.run(&parse_script(src).expect("parses"))
}

#[test]
fn args_are_the_list_a_script_was_given() {
    assert_eq!(args_of(Some(&["a", "b"]), "$args").unwrap().render(), r#"["a", "b"]"#);
    assert_eq!(args_of(Some(&["a", "b"]), "$args | count").unwrap().render(), "2");
    assert_eq!(args_of(Some(&["a", "b"]), "$args[1]").unwrap().render(), "b");
    assert_eq!(args_of(Some(&["a"]), "$args | count").unwrap().render(), "1");
}

#[test]
fn args_are_empty_when_none_were_given() {
    assert_eq!(args_of(None, "$args | count").unwrap().render(), "0");
}

#[test]
fn args_cannot_be_assigned() {
    let e = args_of(Some(&["a"]), "$args = [\"z\"]").expect_err("immutable");
    assert!(e.message.contains("$args"), "{}", e.message);
}

#[test]
fn set_args_replaces_the_list() {
    let mut i = Interp::with_host(Box::new(MockHost::new()), Mode::Script);
    i.set_args(strs(&["a"]));
    i.set_args(strs(&["b", "c"]));
    assert_eq!(i.run(&parse_script("$args | count").expect("parses")).unwrap().render(), "2");
}

/// Only a statement's, a stage's or a parenthesised expression's first token is a head: a path in
/// an operand, a record or a keyword's argument is a string, and must not swallow a `{` as a word.
#[test]
fn a_path_past_the_first_token_is_never_run() {
    for src in [
        "let r = {src: ./deploy.nx}",
        "let s = \"see \" ++ ./deploy.nx",
        "\"a\" == ./b",
        "1 + ./x",
        "let x = true && ./b",
        "let y = \"s\" ++ ~/x",
        "return ./x",
        "fail ./x",
        "match 1 { _ => ./y }",
        "assert (\"./a\" == ./a)",
        "if \"a\" == ./b { 1 }",
        "if ./x == \"a\" { 1 }",
        "let m = match ./x { _ => 1 }",
        "for f in ./x { }",
        "while ./x { }",
    ] {
        let s = parse_script(src).unwrap_or_else(|e| panic!("`{src}` did not parse: {}", e.message));
        assert!(!format!("{s:?}").contains("External"), "`{src}` runs a path");
    }
    for src in ["./x y", "list | ./x", "(./x | count)", "time ./x"] {
        let s = parse_script(src).unwrap_or_else(|e| panic!("`{src}` did not parse: {}", e.message));
        assert!(format!("{s:?}").contains("name: \"./x\", kind: External"), "`{src}` does not run `./x`");
    }
}

#[test]
fn a_path_in_a_record_is_a_value_and_nothing_runs() {
    let host = MockHost::new().with_program("/p/deploy.nx", None);
    let log = host.log();
    let r = run(host, Some("/p"), None, "let r = {src: ./deploy.nx}\nr").expect("runs");
    assert_eq!(r.render(), "{ src: \"./deploy.nx\" }");
    assert!(log.borrow().runs.is_empty(), "a program ran");
}

#[test]
fn a_path_after_an_assignment_stays_a_word() {
    let s = parse_script("mut p = 1\np = ./x").expect("parses");
    assert!(!format!("{s:?}").contains("External"));
}

#[test]
fn a_shebang_line_is_a_comment_to_the_script_it_starts() {
    let s = parse_script("#!/bin/nxsh\n1 + 1").unwrap_or_else(|e| panic!("did not parse: {}", e.message));
    assert_eq!(s.stmts.len(), 1, "the `#!` line is a comment, not a statement");
}
