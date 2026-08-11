//! The Nitrox widget toolkit.
//!
//! A **retained tree with a declarative face**: an application holds state and writes
//! `view(&state) -> Element`, and the runtime diffs that description against the tree it
//! keeps. See `docs/design/widget-toolkit.md` for why — the short version is that `view` is
//! a pure function, so it host-tests like everything else in this tree, and damage falls out
//! of the diff rather than being remembered by each widget.
//!
//! ## Layering
//!
//! ```text
//! libui        ← this crate: the tree, layout, the diff, damage
//!   ↓
//! libsurface   ← window lifecycle, shared buffers, the event queue
//!   ↓
//! libdraw      ← pixels
//! ```
//!
//! This crate does **not** depend on `libsurface` yet: Part A is pure — every function here
//! is a function of values, with no syscall behind any of them. The runtime that drives a
//! real window arrives with event routing in Part B, and that is what will reach downward.
//!
//! ## Build order
//!
//! Milestone 4 Part A is the tree ([`element`]), layout ([`layout`]) and — landing next —
//! the keyed diff and per-buffer damage. Event routing is Part B; the widget set the
//! terminal needs is Part C.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

pub mod damage;
pub mod diff;
pub mod element;
pub mod layout;
pub mod paint;
pub mod route;
pub mod widget;
