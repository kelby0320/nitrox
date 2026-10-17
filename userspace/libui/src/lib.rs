//! The Nitrox widget toolkit.
//!
//! A **retained tree with a declarative face**: an application holds state and writes
//! `view(&state) -> Element`, and the runtime diffs that description against the tree it
//! keeps. See `docs/architecture/widget-toolkit.md` for why.
//!
//! ```text
//! libui        ← this crate: the tree, layout, the diff, damage
//!   ↓
//! libsurface   ← window lifecycle, shared buffers, the event queue
//!   ↓
//! libdraw      ← pixels
//! ```
//!
//! **One module reaches down, and the rest is a function of values**: [`window`] owns a `Session`,
//! a `BufferPool` and a scratch framebuffer; [`element`], [`layout`], [`diff`], [`paint`],
//! [`route`] and [`widget`] cannot make a syscall.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

pub mod chooser;
pub mod click;
pub mod damage;
pub mod diff;
pub mod element;
pub mod layout;
pub mod menu;
pub mod paint;
pub mod reference;
pub mod route;
pub mod widget;
pub mod window;
