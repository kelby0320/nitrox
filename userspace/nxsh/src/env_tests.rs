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
    let refusals: [(&str, &str); 10] = [
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
