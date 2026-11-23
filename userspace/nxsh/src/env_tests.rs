//! `$env` written through: what a program, `cd` and `~` see after an assignment, each form refused
//! whole, and `cd` and `exit` held to one argument.

use crate::eval::{Interp, Mode};
use crate::host::{MockHost, MockLog};
use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use core::cell::RefCell;
use libstream::wire::{Record, Schema, TypeModifiers, TypeTag, Value};

/// A login's environment, as `libsession::session_env` makes it: `PATH` is a List.
fn session() -> Record {
    let schema = Schema::new()
        .field("HOME", TypeTag::String, TypeModifiers::NONE)
        .field("PWD", TypeTag::String, TypeModifiers::NONE)
        .field("PATH", TypeTag::List, TypeModifiers::NONE);
    let path = Value::List(alloc::sync::Arc::from(alloc::vec![Value::Str("/bin".into())]));
    Record { schema, values: alloc::vec![Value::Str("/home/a".into()), Value::Str("/".into()), path] }
}

fn shell(host: MockHost) -> (Interp, Rc<RefCell<MockLog>>) {
    let host = host.with_program("probe", None).with_dir("/x").with_dir("/y").with_dir("/tmp").with_dir("/home/a");
    let log = host.log();
    let mut i = Interp::with_host(Box::new(host), Mode::Repl);
    i.set_env(session());
    (i, log)
}

fn line(i: &mut Interp, l: &str) -> Result<String, String> {
    i.run_line(l).map(|o| String::from(o.unwrap_or_default().trim_end())).map_err(|e| e.message)
}

/// What variable `name` held for the last program run.
fn seen(log: &Rc<RefCell<MockLog>>, name: &str) -> Option<Value> {
    let log = log.borrow();
    let env = log.envs.last()?;
    let i = env.schema.fields.iter().position(|f| f.name == name)?;
    env.values.get(i).cloned()
}

fn s(v: &str) -> Option<Value> {
    Some(Value::Str(v.into()))
}

#[test]
fn a_field_assigned_is_what_a_program_gets() {
    let (mut i, log) = shell(MockHost::new());
    line(&mut i, "$env.HOME = \"/x\"").unwrap();
    line(&mut i, "probe").unwrap();
    assert_eq!(seen(&log, "HOME"), s("/x"));
    line(&mut i, "$env[\"HOME\"] = \"/y\"").unwrap();
    line(&mut i, "probe").unwrap();
    assert_eq!(seen(&log, "HOME"), s("/y"), "by a name index");
    assert_eq!(line(&mut i, "$env.HOME").unwrap(), "/y");
}

#[test]
fn a_new_field_is_a_variable() {
    let (mut i, log) = shell(MockHost::new());
    line(&mut i, "$env.EDITOR = \"nxedit\"").unwrap();
    line(&mut i, "$env[\"PAGER\"] = \"less\"").unwrap();
    line(&mut i, "probe").unwrap();
    assert_eq!((seen(&log, "EDITOR"), seen(&log, "PAGER")), (s("nxedit"), s("less")));
}

/// **`cd` keeps what was assigned**, and a bare `cd` and `~` go to the `HOME` assigned.
#[test]
fn cd_and_tilde_read_what_was_assigned() {
    let (mut i, log) = shell(MockHost::new());
    line(&mut i, "$env.HOME = \"/x\"").unwrap();
    line(&mut i, "cd /tmp").unwrap();
    assert_eq!(line(&mut i, "$env.HOME").unwrap(), "/x");
    line(&mut i, "probe ~/notes").unwrap();
    assert_eq!(log.borrow().runs.last().unwrap()[0].argv, ["probe", "/x/notes"]);
    assert_eq!((seen(&log, "HOME"), seen(&log, "PWD")), (s("/x"), s("/tmp")));
    line(&mut i, "cd").unwrap();
    assert_eq!(line(&mut i, "$env.PWD").unwrap(), "/x");
}

/// A whole record replaces the environment when it keeps `PWD`.
#[test]
fn a_whole_record_that_keeps_pwd_is_the_environment() {
    let (mut i, log) = shell(MockHost::new());
    line(&mut i, "$env = ($env | merge {HOME: \"/y\", LANG: \"C\"})").unwrap();
    line(&mut i, "probe").unwrap();
    assert_eq!((seen(&log, "HOME"), seen(&log, "LANG"), seen(&log, "PWD")), (s("/y"), s("C"), s("/")));
}

/// **Every form that would move `PWD`, leave `$env` no record, or a value no program could be
/// handed, is refused whole**: the directory and every variable are as they were.
#[test]
fn each_form_that_would_break_the_environment_is_refused_whole() {
    let big = alloc::format!("$env.BIG = \"{}\"", "x".repeat(5000));
    let refusals: [(&str, &str); 17] = [
        ("$env.PWD = \"/tmp\"", "`cd`'s"),
        ("$env[\"PWD\"] = \"/tmp\"", "`cd`'s"),
        ("$env = ($env | merge {PWD: \"/tmp\"})", "`cd`'s"),
        ("$env = {}", "`cd`'s"),
        ("$env = 5", "a Int cannot replace it"),
        ("$env.PATH = \"/bin\"", "`$env.PATH` is a List"),
        ("$env.HOME = 5", "`$env.HOME` is a String"),
        ("$env = ($env | merge {HOME: 5})", "`$env.HOME` is a String"),
        ("$env.FOO = null", "would hold nothing"),
        ("$env.HOME.x = 1", "cannot set field"),
        ("$env = ($env | merge {PATH: \"/bin\"})", "`$env.PATH` is a List"),
        ("$env = {HOME: \"/home/a\", PWD: \"/\", PATH: \"/bin\"}", "`$env.PATH` is a List"),
        ("$env.TT = ([1, 2] | map { |x| {a: x} })", "would hold a Table"),
        ("$env = {HOME: \"/home/a\", PWD: \"/\", PATH: [\"/bin\"], PWD: \"/tmp\"}", "would be there twice"),
        ("$env[\"\"] = \"v\"", "has a name"),
        (&big, "larger than the 4008 bytes"),
        ("$env.L = [(1..3 | map { |n| {n: n} })]", "a Table inside it"),
    ];
    for (form, says) in refusals {
        let (mut i, log) = shell(MockHost::new());
        let e = line(&mut i, form).expect_err(form);
        assert!(e.contains(says), "{form}: {e}");
        line(&mut i, "probe").unwrap();
        let log = log.borrow();
        assert_eq!(log.envs.last(), Some(&session()), "{form} changed the environment");
        drop(log);
        assert_eq!(line(&mut i, "$env.PWD").unwrap(), "/", "{form}");
    }
}

#[test]
fn args_stays_unassignable() {
    let (mut i, _) = shell(MockHost::new());
    assert!(line(&mut i, "$args = []").unwrap_err().contains("`$args`"));
}

/// **`cd` and `exit` take one argument, or none**: a second is refused, with nothing done, and a
/// flag is named as one.
#[test]
fn cd_and_exit_take_one_argument_or_none() {
    let (mut i, _) = shell(MockHost::new());
    let e = line(&mut i, "cd /x /y").unwrap_err();
    assert!(e.contains("`cd` takes one path, or none, and was given 2"), "{e}");
    assert_eq!(line(&mut i, "$env.PWD").unwrap(), "/");
    assert!(line(&mut i, "cd --all").unwrap_err().contains("`--all` is one"));
    line(&mut i, "cd /x").unwrap();
    assert_eq!(line(&mut i, "$env.PWD").unwrap(), "/x");

    let e = i.run_line("exit 1 2").expect_err("refused");
    assert!(!e.is_exit() && e.message.contains("`exit` takes one status, or none"), "{}", e.message);
    let e = i.run_line("exit -n").expect_err("refused");
    assert!(!e.is_exit() && e.message.contains("`-n` is one"), "{}", e.message);
    assert_eq!(i.run_line("exit 3").expect_err("exits").exit, Some(3));
}

/// **A value inside a variable keeps its type too**: the environment must still encode.
#[test]
fn a_value_retyped_inside_a_variable_is_refused() {
    let (mut i, log) = shell(MockHost::new());
    line(&mut i, "$env.R = {a: 1}").unwrap();
    let e = line(&mut i, "$env.R.a = \"s\"").unwrap_err();
    assert!(e.contains("does not fit its field's type"), "{e}");
    line(&mut i, "probe").unwrap();
    assert_eq!(line(&mut i, "$env.R.a").unwrap(), "1");
    assert!(seen(&log, "R").is_some());
}

/// A name with a space or an `=` is a variable's like any: the environment is a record, not
/// `key=value` strings. A List variable takes a List of anything; nothing reads `PATH`'s items.
#[test]
fn any_name_but_an_empty_one_and_any_list_are_taken() {
    let (mut i, log) = shell(MockHost::new());
    for form in ["$env[\"a b\"] = \"v\"", "$env[\"X=Y\"] = \"v\"", "$env.PATH = [5]"] {
        line(&mut i, form).unwrap_or_else(|e| panic!("{form}: {e}"));
    }
    line(&mut i, "probe").unwrap();
    assert_eq!((seen(&log, "a b"), seen(&log, "X=Y")), (s("v"), s("v")));
}

/// **One environment, in a `def` and a closure as at the prompt**: what either writes, the prompt
/// reads, and a `cd` in a `def` moves the prompt's `PWD`.
#[test]
fn a_def_and_a_closure_write_the_one_environment() {
    let (mut i, log) = shell(MockHost::new());
    line(&mut i, "def setx() { $env.EDITOR = \"vi\" }").unwrap();
    line(&mut i, "setx()").unwrap();
    assert_eq!(line(&mut i, "$env.EDITOR").unwrap(), "vi");
    line(&mut i, "[1] | each { |x| $env.PAGER = \"less\" }").unwrap();
    line(&mut i, "$env = ($env | merge {LANG: \"C\"})").unwrap();
    line(&mut i, "probe").unwrap();
    assert_eq!((seen(&log, "EDITOR"), seen(&log, "PAGER")), (s("vi"), s("less")));
    line(&mut i, "def go() { cd /tmp }").unwrap();
    line(&mut i, "go()").unwrap();
    assert_eq!(line(&mut i, "$env.PWD").unwrap(), "/tmp");
    line(&mut i, "def home() { $env.HOME }").unwrap();
    assert_eq!(line(&mut i, "home()").unwrap(), "/home/a");
}

/// **No binding takes `$env`'s name**, which would hide the environment where it is read.
#[test]
fn no_binding_takes_the_environments_name() {
    for form in [
        "let $env = 5",
        "mut $env = 5",
        "const $env = 5",
        "for $env in [1] { 1 }",
        "def f($env) { 1 }",
        "def $env() { 1 }",
        "[1] | each { |$env| 1 }",
        "try { 1 } catch ($env) { 2 }",
        "match 1 { $env => 1 }",
        "match {a: 1} { {$env} => 1 }",
    ] {
        let (mut i, _) = shell(MockHost::new());
        let e = line(&mut i, form).expect_err(form);
        assert!(e.contains("no binding takes its name"), "{form}: {e}");
    }
}
