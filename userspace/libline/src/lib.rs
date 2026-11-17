//! `libline` — the line discipline: what a byte typed does to a line, and what to write so the
//! screen keeps up. `tty-server` drives it in canonical mode; `nxsh` reads raw and drives it to
//! edit its own prompt. `eshell` keeps a copy of its own, having no `alloc`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod discipline;
mod keys;

pub use discipline::{Discipline, LINE_MAX, Step};
pub use keys::{Key, Keys, Typed};
