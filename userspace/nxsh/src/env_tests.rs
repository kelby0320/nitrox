//! `cd` and `exit` held to one argument.

use crate::eval::{Interp, Mode};
use crate::host::MockHost;
use alloc::boxed::Box;
use alloc::string::String;
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

fn shell(host: MockHost) -> Interp {
    let host = host.with_dir("/x").with_dir("/y");
    let mut i = Interp::with_host(Box::new(host), Mode::Repl);
    i.set_env(session());
    i
}

fn line(i: &mut Interp, l: &str) -> Result<String, String> {
    i.run_line(l).map(|o| String::from(o.unwrap_or_default().trim_end())).map_err(|e| e.message)
}

/// **`cd` and `exit` take one argument, or none**: a second is refused, with nothing done, and a
/// flag is named as one.
#[test]
fn cd_and_exit_take_one_argument_or_none() {
    let mut i = shell(MockHost::new());
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
