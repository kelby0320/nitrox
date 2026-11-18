//! What a line leaves behind: `$last`, and the report of a pipeline whose program failed.

use crate::eval::{Interp, Mode};
use crate::host::MockHost;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use libstream::wire::{Schema, StreamFlags, Table, TypeModifiers, TypeTag, Value};

fn shell() -> Interp {
    let schema = Schema::new().field("user", TypeTag::String, TypeModifiers::NONE);
    let mut table = Vec::new();
    let rows = alloc::vec![alloc::vec![Value::Str("alice".into())]];
    Table { flags: StreamFlags::NONE, schema, rows }.encode(&mut table).expect("encodes");
    let host = MockHost::new().with_program("whoami", Some(table)).with_failing("bad", 1);
    Interp::with_host(Box::new(host), Mode::Repl)
}

fn said(i: &mut Interp, line: &str) -> Result<Option<String>, String> {
    i.run_line(line).map_err(|e| e.message)
}

/// `$last`'s value and status are one line's: a line that runs no pipeline has none, rather than
/// an earlier line's.
#[test]
fn last_pairs_a_lines_value_with_its_own_status() {
    let mut i = shell();
    said(&mut i, "whoami").expect("runs");
    assert_eq!(said(&mut i, "$last.status.all_ok"), Ok(Some("true\n".into())), "whoami's");
    said(&mut i, "1 + 1").expect("runs");
    assert_eq!(said(&mut i, "$last.status == null"), Ok(Some("true\n".into())), "not whoami's");
}

/// A program that fails and sends nothing fails the pipeline, naming it, before an operator after
/// it can blame its missing operand; the stages' report reaches a `catch`.
#[test]
fn a_failed_program_is_the_failure_not_the_operator_after_it() {
    let mut i = shell();
    assert_eq!(said(&mut i, "bad | count"), Err("pipeline failed: `bad` exited 1".into()));
    let report = "try { bad | count } catch (e) { e.stages | count }";
    assert_eq!(said(&mut i, report), Ok(Some("1\n".into())));
    assert_eq!(said(&mut i, "whoami | count"), Ok(Some("1\n".into())), "one that succeeds is counted");
}
