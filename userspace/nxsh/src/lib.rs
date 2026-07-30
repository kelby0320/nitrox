//! `nxsh` — the Nitrox shell language.
//!
//! This library is the whole language and none of the operating system: lexer, parser,
//! and (from Part B) the evaluator, generic operators and regex engine. It makes no
//! syscalls, so `cargo test -p nxsh --lib` runs it on the host in a second rather than in
//! a 90-second boot. The `[[bin]]` beside it is the *host* — `_start`, spawning stages,
//! wiring pipes, the console reader.
//!
//! That split is the same one that made the ext4 parser testable behind `BlockReader`,
//! and it is worth more here: an interpreter is mostly pure logic, and even the parts
//! that are not — pipeline ordering, backpressure, error propagation — become testable
//! once the OS sits behind a trait.
//!
//! Design: `docs/history/nitrox-shell-design-v1.1.md` (§8 lexical/expression grammar, §9
//! statements). Plan and the decisions it resolves: `docs/planning/shell-coreutils-plan.md`
//! § Milestone 3.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod ast;
pub mod lex;
pub mod parse;

pub use lex::{LexError, Lexer, Mode, Tok};
pub use parse::{ParseError, Parser, parse_script};
