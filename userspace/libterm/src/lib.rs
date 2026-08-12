//! Terminal semantics: what a cell is, what an escape sequence means, and what a key sends.
//!
//! ```text
//! nxterm            ← the client binary (Part B)
//!   ↓
//! libui   libterm   ← toolkit; terminal semantics
//!   ↓         ↓
//! libdraw           ← pixels
//! ```
//!
//! **No syscalls in here**, the same split `fs-server-ext4` uses for its ext4 parser, `nxsh`
//! for its evaluator and `compositor` for its window model. A terminal's hard parts — deferred
//! wrapping, scroll into scrollback, cursor clamping, escape-parameter accumulation — are
//! functions of values, and they are asserted on the host in milliseconds rather than through
//! a boot.
//!
//! ## Build order
//!
//! Milestone 5 Part A of [`display-arm-plan.md`](../planning/display-arm-plan.md):
//! this crate and its vocabulary (A2), the output parser (A3), the grid (A4), the render and
//! the reference grid (A5), and the input encoder (A6).

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

pub mod cell;
pub mod grid;
pub mod parse;
pub mod render;
