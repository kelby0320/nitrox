//! Which run's last stage is told its output is shown (`crate::repl::shown`).

use crate::eval::{Interp, Mode};
use crate::host::MockHost;
use alloc::boxed::Box;
use alloc::vec::Vec;
use libstream::wire::{Schema, StreamFlags, Table, TypeModifiers, TypeTag, Value};

/// A one-row, one-column stream, as a program's output.
fn one_row() -> Vec<u8> {
    let schema = Schema::new().field("n", TypeTag::Int, TypeModifiers::NONE);
    let t = Table { flags: StreamFlags::NONE, schema, rows: alloc::vec![alloc::vec![Value::Int(1)]] };
    let mut buf = Vec::new();
    t.encode(&mut buf).expect("encodes");
    buf
}

/// What each run was told, for each line typed at a prompt in turn.
fn shown(lines: &[&str]) -> Vec<bool> {
    let host = MockHost::new().with_program("man", None).with_program("whoami", Some(one_row()));
    let log = host.log();
    let mut i = Interp::with_host(Box::new(host), Mode::Repl);
    for l in lines {
        let _ = i.run_line(l);
    }
    log.borrow().shown.clone()
}

#[test]
fn a_command_a_pipeline_and_a_bare_name_displayed_are_shown() {
    assert_eq!(shown(&["man ls"]), [true]);
    assert_eq!(shown(&["man ls | whoami"]), [true], "one run, the last stage told");
    assert_eq!(shown(&["man"]), [true], "a bare name run as a program");
}

#[test]
fn a_value_assigned_piped_on_or_computed_with_is_not() {
    assert_eq!(shown(&["let x = man ls"]), [false]);
    assert_eq!(shown(&["man ls | count"]), [false], "an operator takes it");
    assert_eq!(shown(&["man ls | display"]), [false], "displayed by the operator, as text");
    assert_eq!(shown(&["(man ls) + 1"]), [false], "an expression's operand");
}

/// The statement's own pipeline takes the flag before its arguments run.
#[test]
fn a_pipeline_in_an_argument_is_not_the_statements() {
    assert_eq!(shown(&["man x (whoami)"]), [false, true]);
}

/// Nor a script, though a name bound at the prompt last set the flag and ran nothing to take it.
#[test]
fn a_script_shows_nothing() {
    let host = MockHost::new().with_program("man", None);
    let log = host.log();
    let mut i = Interp::with_host(Box::new(host), Mode::Repl);
    for l in ["let m = 1", "m"] {
        i.run_line(l).expect("runs");
    }
    let _ = i.run(&crate::parse_script("man ls").expect("parses"));
    assert_eq!(log.borrow().shown.clone(), [false]);
}
