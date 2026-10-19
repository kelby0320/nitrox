//! `nxsh` — the Nitrox shell language.
//!
//! This library is the whole language and none of the operating system: it makes no syscalls, so
//! `cargo test -p nxsh --lib` runs it on the host in a second. The `[[bin]]` beside it is the
//! host: `_start`, spawning stages, wiring pipes, the console reader.
//!
//! Design: `docs/spec/shell-language.md` (§8 lexical/expression grammar, §9 statements).

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod ast;
pub mod complete;
pub mod eval;
pub mod exec;
pub mod history;
pub mod host;
pub mod lex;
pub mod ops;
pub mod parse;
pub mod paths;
pub mod regex;
pub mod repl;
pub mod search;
pub mod style;
pub mod timing;
pub mod value;

#[cfg(test)]
mod exec_tests;
#[cfg(test)]
mod tilde_tests;
#[cfg(test)]
mod use_tests;

pub use complete::{Completion, Where, word_at_end};
pub use eval::{EvalError, Interp, Mode as RunMode};
pub use host::{Host, MockHost, MockLog, NullHost, PipelineRun, StageSpec, StageStatus};
pub use lex::{LexError, Lexer, Mode as LexMode, Tok};
pub use parse::{ParseError, Parser, parse_script};
pub use paths::parse_script_in;
pub use repl::{Continue, needs_continuation};
pub use value::Val;
