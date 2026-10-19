//! `~` at the start of an unquoted word: lexing, expansion at each place a word becomes a path,
//! and completion.

use crate::eval::{EvalError, Interp, Mode};
use crate::host::{MockHost, MockLog};
use crate::lex::{Lexer, Tok};
use crate::{Val, parse_script};
use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use core::cell::RefCell;
use libstream::wire::{Record, Schema, TypeModifiers, TypeTag, Value};

fn env(home: Option<&str>) -> Record {
    let mut schema = Schema::new().field("PWD", TypeTag::String, TypeModifiers::NONE);
    let mut values = vec![Value::Str(String::from("/p"))];
    if let Some(h) = home {
        schema = schema.field("HOME", TypeTag::String, TypeModifiers::NONE);
        values.push(Value::Str(String::from(h)));
    }
    Record { schema, values }
}

fn interp(host: MockHost, home: Option<&str>) -> Interp {
    let mut i = Interp::with_host(Box::new(host), Mode::Script);
    i.set_env(env(home));
    i
}

fn run(host: MockHost, home: Option<&str>, src: &str) -> Result<Val, EvalError> {
    interp(host, home).run(&parse_script(src).expect("parses"))
}

fn pwd_after(host: MockHost, home: Option<&str>, src: &str) -> String {
    let mut i = interp(host, home);
    i.run(&parse_script(src).expect("parses")).expect("runs");
    String::from(i.cwd().expect("PWD"))
}

fn words(src: &str) -> Vec<Tok> {
    Lexer::tokenize_expr(src).expect("lexes")
}

use alloc::vec::Vec;

fn w(s: &str) -> Tok {
    Tok::Word(String::from(s))
}

#[test]
fn a_tilde_that_starts_an_expression_word_lexes_as_a_word() {
    assert_eq!(words("~"), vec![w("~"), Tok::Eof]);
    assert_eq!(words("~/x"), vec![w("~/x"), Tok::Eof]);
    assert_eq!(words("~name"), vec![w("~name"), Tok::Eof]);
    assert_eq!(words("~\n")[0], w("~"));
    assert_eq!(words("~|")[0], w("~"));
    assert_eq!(words("(~)")[1], w("~"));
}

#[test]
fn the_match_operator_and_a_tilde_inside_a_word_are_unchanged() {
    let t = words("\"x\" ~= /re/");
    assert!(t.contains(&Tok::Match_), "{t:?}");
    assert!(!t.contains(&w("~")), "{t:?}");
    let r = run(MockHost::new().with_program("echo", None), Some("/h"), "echo a~b");
    assert!(r.is_ok());
}

#[test]
fn cd_expands_a_tilde_word() {
    let host = || MockHost::new().with_dir("/home/a").with_dir("/home/a/x");
    assert_eq!(pwd_after(host(), Some("/home/a"), "cd ~"), "/home/a");
    assert_eq!(pwd_after(host(), Some("/home/a"), "cd ~/x"), "/home/a/x");
}

#[test]
fn a_quoted_tilde_stays_literal() {
    let host = MockHost::new().with_dir("/p/~").with_dir("/home/a");
    assert_eq!(pwd_after(host, Some("/home/a"), "cd \"~\""), "/p/~");
}

#[test]
fn open_reads_the_home_directory_file() {
    let host = || MockHost::new().with_file("/home/a/n.txt", "hello\n");
    let r = run(host(), Some("/home/a"), "open ~/n.txt").expect("opens");
    let direct = run(host(), Some("/home/a"), "open /home/a/n.txt").expect("opens");
    assert_eq!(r.render(), direct.render());
}

#[test]
fn save_writes_under_the_home_directory() {
    let src = "[\"x\"] | save ~/s.txt\nopen /home/a/s.txt | count";
    let r = run(MockHost::new(), Some("/home/a"), src).expect("saved under HOME, then read back");
    assert_eq!(r.render(), "1");
}

#[test]
fn a_program_argument_arrives_expanded_and_a_quoted_one_does_not() {
    let host = MockHost::new().with_program("ls", None);
    let log: Rc<RefCell<MockLog>> = host.log();
    run(host, Some("/home/a"), "ls ~/x \"~/y\" a~b").expect("runs");
    let argv = log.borrow().runs[0][0].argv.clone();
    assert_eq!(argv, vec!["ls", "/home/a/x", "~/y", "a~b"]);
}

#[test]
fn home_that_is_the_root_does_not_double_the_slash() {
    let host = MockHost::new().with_program("ls", None);
    let log = host.log();
    run(host, Some("/"), "ls ~/x").expect("runs");
    assert_eq!(log.borrow().runs[0][0].argv, vec!["ls", "/x"]);
}

#[test]
fn a_tilde_name_is_an_error_naming_it() {
    let host = MockHost::new().with_program("ls", None).with_dir("/home/a");
    let e = run(host, Some("/home/a"), "ls ~bob/x").expect_err("refused");
    assert!(e.message.contains("~bob/x"), "{}", e.message);
    let host = MockHost::new().with_dir("/home/a");
    assert!(run(host, Some("/home/a"), "cd ~bob").is_err());
}

#[test]
fn an_unset_home_is_an_error_naming_it() {
    let host = MockHost::new().with_program("ls", None);
    let e = run(host, None, "ls ~/x").expect_err("refused");
    assert!(e.message.contains("HOME"), "{}", e.message);
    let e = run(MockHost::new(), None, "cd ~").expect_err("refused");
    assert!(e.message.contains("HOME"), "{}", e.message);
}

#[test]
fn a_column_name_is_not_a_path() {
    let host = MockHost::new();
    let r = run(host, None, "[{a: 1}] | select ~x");
    let m = r.err().map(|e| e.message).unwrap_or_default();
    assert!(!m.contains("HOME"), "{m}");
}

#[test]
fn completion_expands_the_directory_and_keeps_the_tilde() {
    let mut host = MockHost::new();
    host.entries =
        vec![(String::from("/home/a"), vec![(String::from("Pictures"), true), (String::from("Music"), true)])];
    let mut i = interp(host, Some("/home/a"));
    assert_eq!(i.complete("cd ~/Pi").candidates, vec![String::from("~/Pictures/")]);
}
