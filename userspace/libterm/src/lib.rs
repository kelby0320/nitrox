//! Terminal semantics: what a cell is, what an escape sequence means, and what a key sends.
//!
//! No syscalls in here, so a terminal's hard parts (deferred wrapping, scrollback, cursor
//! clamping, escape-parameter accumulation) are functions of values, asserted on the host.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

pub mod cell;
pub mod encode;
pub mod grid;
pub mod parse;
pub mod render;

#[cfg(test)]
mod editing_tests;
