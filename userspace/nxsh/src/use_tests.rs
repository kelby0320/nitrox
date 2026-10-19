//! `use` against a whole interpreter: where it reads from, and the chain of files being loaded.

use crate::eval::{EvalError, Interp, Mode};
use crate::host::MockHost;
use crate::{Val, parse_script_in};
use alloc::boxed::Box;
use alloc::string::String;
use libstream::wire::{Record, Schema, TypeModifiers, TypeTag, Value};

fn env(pwd: Option<&str>) -> Record {
    let mut schema = Schema::new();
    let mut values = alloc::vec::Vec::new();
    if let Some(p) = pwd {
        schema = schema.field("PWD", TypeTag::String, TypeModifiers::NONE);
        values.push(Value::Str(String::from(p)));
    }
    Record { schema, values }
}

/// Run `src` as the script at `file` (so its `use`s resolve against `file`'s directory).
fn script_at(host: MockHost, pwd: &str, file: &str, src: &str) -> Result<Val, EvalError> {
    let mut i = Interp::with_host(Box::new(host), Mode::Script);
    i.set_env(env(Some(pwd)));
    i.mark_loading(file);
    let s = parse_script_in(src, Some(crate::paths::dir_of(file))).expect("parses");
    i.run(&s)
}

/// Run `src` as a script with no file behind it, so its `use`s resolve against `pwd`.
fn run_in(host: MockHost, pwd: &str, src: &str) -> Result<Val, EvalError> {
    let mut i = Interp::with_host(Box::new(host), Mode::Script);
    i.set_env(env(Some(pwd)));
    i.run(&crate::parse_script(src).expect("parses"))
}

const LIB: &str = "pub def f() -> Int { 7 }\n";

#[test]
fn use_imports_only_what_is_exported() {
    let host =
        MockHost::new().with_file("/w/lib.nx", "pub def helper(x: Int) -> Int { x + 1 }\ndef hidden() -> Int { 0 }\n");
    let r = run_in(host, "/w", "use \"./lib.nx\" { helper }\nhelper(1)");
    assert_eq!(r.expect("imports").render(), "2");

    let host = MockHost::new().with_file("/w/lib.nx", "def hidden() -> Int { 0 }\n");
    let r = run_in(host, "/w", "use \"./lib.nx\" { hidden }\nhidden()");
    assert!(r.expect_err("not exported").message.contains("pub"));
}

#[test]
fn use_as_binds_a_qualified_name() {
    let host = MockHost::new().with_file("/w/lib.nx", "pub def helper(x: Int) -> Int { x * 2 }\n");
    let r = run_in(host, "/w", "use \"./lib.nx\" as utils\nutils.helper(4)");
    assert_eq!(r.expect("imports").render(), "8");
}

#[test]
fn a_script_resolves_use_against_its_own_directory_not_pwd() {
    let host = MockHost::new().with_file("/home/a/lib/m.nx", LIB);
    let r = script_at(host, "/elsewhere", "/home/a/main.nx", "use \"./lib/m.nx\" { f }\nf()");
    assert_eq!(r.expect("read /home/a/lib/m.nx").render(), "7");
}

#[test]
fn a_module_resolves_use_against_its_own_directory() {
    let host = MockHost::new()
        .with_file("/home/a/lib/m.nx", "use \"./util.nx\" { g }\ng()\npub def f() -> Int { 5 }\n")
        .with_file("/home/a/lib/util.nx", "pub def g() -> Int { 5 }\n");
    let r = script_at(host, "/elsewhere", "/home/a/main.nx", "use \"./lib/m.nx\" { f }\nf()");
    assert_eq!(r.expect("read /home/a/lib/util.nx").render(), "5");
}

#[test]
fn a_use_in_an_exported_def_resolves_against_the_defs_file() {
    let host = MockHost::new()
        .with_file("/home/a/lib/m.nx", "pub def f() -> Int {\n use \"./util.nx\" { g }\n g()\n}\n")
        .with_file("/home/a/lib/util.nx", "pub def g() -> Int { 9 }\n");
    let r = script_at(host, "/home/b", "/home/b/main.nx", "use \"/home/a/lib/m.nx\" { f }\nf()");
    assert_eq!(r.expect("lexical").render(), "9");
}

#[test]
fn a_cd_in_a_script_moves_nothing_use_sees() {
    let host = MockHost::new().with_file("/home/a/m.nx", LIB).with_dir("/tmp");
    let r = script_at(host, "/home/a", "/home/a/main.nx", "cd /tmp\nuse \"./m.nx\" { f }\nf()");
    assert_eq!(r.expect("still /home/a").render(), "7");
}

#[test]
fn at_the_prompt_use_resolves_against_pwd() {
    let host = MockHost::new().with_file("/p/x.nx", LIB);
    let mut i = Interp::with_host(Box::new(host), Mode::Repl);
    i.set_env(env(Some("/p")));
    i.run_line("use \"./x.nx\" { f }").expect("read /p/x.nx");
    assert_eq!(i.run_line("f()").unwrap().as_deref(), Some("7\n"));
}

#[test]
fn at_the_prompt_with_no_pwd_a_relative_use_is_refused() {
    let mut i = Interp::with_host(Box::new(MockHost::new().with_file("/x.nx", LIB)), Mode::Repl);
    let e = i.run_line("use \"./x.nx\" { f }").expect_err("no PWD");
    assert!(e.message.starts_with("`./x.nx`: "), "{}", e.message);
    assert!(!e.message.contains("no such file"), "{}", e.message);
}

#[test]
fn a_cycle_is_refused_naming_the_chain() {
    let host = MockHost::new()
        .with_file("/s/a.nx", "use \"./b.nx\" { y }\npub def x() -> Int { 1 }\n")
        .with_file("/s/b.nx", "use \"./a.nx\" { x }\npub def y() -> Int { 1 }\n");
    let e = script_at(host, "/s", "/s/main.nx", "use \"./a.nx\" { x }").expect_err("cycle");
    assert_eq!(e.message, "`./a.nx` is already being loaded: /s/main.nx → /s/a.nx → /s/b.nx → /s/a.nx");
}

#[test]
fn a_script_that_uses_itself_is_refused() {
    let e = script_at(MockHost::new(), "/s", "/s/main.nx", "use \"./main.nx\" { f }").expect_err("self");
    assert!(e.message.contains("is already being loaded"), "{}", e.message);
}

#[test]
fn a_failed_use_can_be_tried_again_and_leaves_no_base_behind() {
    let host = MockHost::new()
        .with_file("/d/bad.nx", "1 / 0\n")
        .with_file("/d/parse.nx", "use use\n")
        .with_file("/p/ok.nx", LIB);
    let mut i = Interp::with_host(Box::new(host), Mode::Repl);
    i.set_env(env(Some("/p")));
    for m in ["/d/bad.nx", "/d/parse.nx", "/d/missing.nx"] {
        let src = alloc::format!("use \"{m}\" {{ f }}");
        let e = i.run_line(&src).expect_err("fails");
        assert!(!e.message.contains("already being loaded"), "{}", e.message);
        // Twice: the first failure must not leave the path in the chain.
        let e = i.run_line(&src).expect_err("fails again");
        assert!(!e.message.contains("already being loaded"), "{}", e.message);
    }
    i.run_line("use \"./ok.nx\" { f }").expect("PWD, not the failed module's directory");
}

#[test]
fn a_module_that_fails_inside_a_cycle_check_unwinds_every_level() {
    let host = MockHost::new()
        .with_file("/s/a.nx", "use \"./b.nx\" { y }\n")
        .with_file("/s/b.nx", "use \"./nope.nx\" { y }\n");
    let mut i = Interp::with_host(Box::new(host), Mode::Repl);
    i.set_env(env(Some("/s")));
    assert!(i.run_line("use \"./a.nx\" { y }").is_err());
    let e = i.run_line("use \"./a.nx\" { y }").expect_err("fails");
    // Not only `nope.nx`: a chain left behind names it too, inside "already being loaded".
    assert!(e.message.contains("nope.nx") && !e.message.contains("already being loaded"), "{}", e.message);
}

#[test]
fn the_mock_hosts_missing_file_quotes_its_path() {
    let mut i = Interp::with_host(Box::new(MockHost::new()), Mode::Repl);
    i.set_env(env(Some("/s")));
    let e = i.run_line("use \"./m.nx \" { f }").expect_err("missing");
    assert_eq!(e.message, "no such file: `/s/m.nx `");
}
