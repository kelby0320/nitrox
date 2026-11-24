//! **`open` and `save` take a path as `cd` does**, not a field access or a division.

use crate::eval::{EvalError, Interp, Mode};
use crate::host::MockHost;
use crate::{Val, parse_script};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use libstream::wire::{Record, Schema, TypeModifiers, TypeTag, Value};

fn run(host: MockHost, src: &str) -> Result<Val, EvalError> {
    let schema = Schema::new().field("PWD", TypeTag::String, TypeModifiers::NONE);
    let mut i = Interp::with_host(Box::new(host), Mode::Script);
    i.set_env(Record { schema, values: vec![Value::Str(String::from("/p"))] });
    i.run(&parse_script(src).expect("parses"))
}

fn count(host: MockHost, src: &str) -> String {
    run(host, src).unwrap_or_else(|e| panic!("{src}: {}", e.message)).render()
}

#[test]
fn a_plain_file_name_is_opened_and_saved() {
    assert_eq!(count(MockHost::new().with_file("/p/hello.txt", "a\nb\n"), "open hello.txt | count"), "2");
    assert_eq!(count(MockHost::new(), "[\"x\"] | save out.txt\nopen /p/out.txt | count"), "1");
}

#[test]
fn a_relative_path_through_a_directory_is_one_path() {
    let host = MockHost::new().with_file("/p/Documents/hello.txt", "a\n");
    assert_eq!(count(host, "open Documents/hello.txt | count"), "1");
    let src = "[\"x\", \"y\"] | save Documents/out.txt\nopen /p/Documents/out.txt | count";
    assert_eq!(count(MockHost::new(), src), "2");
}

#[test]
fn open_concatenates_plain_names_as_its_section_says() {
    let host = MockHost::new().with_file("/p/a.txt", "1\n").with_file("/p/b.txt", "2\n3\n");
    assert_eq!(count(host, "open a.txt b.txt | count"), "3");
}

/// What already worked still does, and a block's `{` ends the paths.
#[test]
fn a_dotted_path_a_quoted_one_and_a_following_block_are_read_as_before() {
    let host = || MockHost::new().with_file("/p/a b.txt", "1\n2\n").with_file("/p/log.txt", "x\ny\nz\n");
    assert_eq!(count(host(), "open ./log.txt | count"), "3");
    assert_eq!(count(host(), "open \"a b.txt\" | count"), "2");
    let src = "mut n = 0\nfor line in open log.txt { n = n + 1 }\nn";
    assert_eq!(count(host(), src), "3");
    let e = run(host(), "for line in open { 1 }").expect_err("no path");
    assert_eq!(e.message, "`open` needs at least one path", "the brace is the block's");
}
