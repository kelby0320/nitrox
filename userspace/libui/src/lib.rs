//! The Nitrox widget toolkit.
//!
//! A **retained tree with a declarative face**: an application holds state and writes
//! `view(&state) -> Element`, and the runtime diffs that description against the tree it
//! keeps. See `docs/architecture/widget-toolkit.md` for why — the short version is that `view` is
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
//! **One module reaches down, and the rest is a function of values.** [`element`], [`layout`],
//! [`diff`], [`paint`], [`route`] and [`widget`] cannot make a syscall: they take values and
//! return values, which is why they host-test in milliseconds. [`window`] is the exception, and
//! it arrived in M12 Part A rather than in M4 — a second window is a `Session`, a `BufferPool`
//! and a scratch framebuffer as much as it is a tree and a router, and two applications wanting
//! the same six fields is when a helper goes down a layer.
//!
//! Until then this paragraph said the crate "does **not** depend on `libsurface` yet", with the
//! *yet* doing the work: the layering above has always had this crate above that one.
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
pub mod chooser;
pub mod menu;
pub mod paint;
pub mod reference;
pub mod route;
pub mod widget;
pub mod window;
