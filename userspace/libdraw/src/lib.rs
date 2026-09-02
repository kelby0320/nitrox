//! `libdraw` — geometry, pixel formats, framebuffers, compositing, and the theme.
//!
//! **The theme is the fifth item and it arrived last**, in M11 Part B, against the grain of every
//! sentence below that calls this "the pixel layer". It is here because the *compositor* paints
//! chrome — a cursor, a drag outline, the ground between windows — and does not link a widget
//! toolkit, deliberately; this crate is what it and `libui` share. See [`theme`] for the argument
//! in full. Nothing else about the layering changed: a theme is a table of colours, which is
//! pixel vocabulary, and not a claim that this crate knows what a button is.
//!
//! The pixel layer of the display arm, and **the gate that arrives with it**. The
//! plan is explicit about the ordering ([`docs/planning/display-arm-plan.md`],
//! governing decision 1): "No compositing code merges without the `Framebuffer` trait
//! and host tests behind it. Everything else in this system has a gate; pixels are
//! the one place where 'we'll add tests once it works' would actually happen."
//!
//! ## The seam
//!
//! [`framebuffer::Framebuffer`] is base, width, height, pitch and format, with a real
//! implementation over a mapped aperture ([`framebuffer::RawFramebuffer`]) and an
//! in-memory one for tests ([`framebuffer::MemFramebuffer`]). Behind it,
//! [`compose::compose`] is a pure function of (surfaces, geometry, damage, stacking)
//! — so the part that *looks* like it needs a screen is asserted pixel-exactly in
//! milliseconds. It is the same move `BlockReader` made for the ext4 parser and
//! `Host` made for the shell's evaluator, and it is why those subsystems are tested
//! at all.
//!
//! ## Shared, not compositor-only
//!
//! The compositor composites surfaces and a client draws into one; both do the same
//! rect and blit work. Building this inside the compositor would mean writing it
//! twice, and the second copy would live in an application.
//!
//! ## Determinism is a design constraint
//!
//! `docs/design/display-substrate.md` §7: the same surfaces, geometry, damage and
//! stacking must produce the same bytes, every time, on any machine. Not a testing
//! preference — [`hash::hash_visible`] and [`scene::REFERENCE_HASH`] are the gate,
//! and a compositor that varied by clock, scheduling order or uninitialised padding
//! could not be gated at all.
//!
//! [`docs/planning/display-arm-plan.md`]: ../../../docs/planning/display-arm-plan.md

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

#[cfg(feature = "io")]
pub mod acquire;
pub mod compose;
pub mod format;
pub mod framebuffer;
pub mod geom;
pub mod hash;
pub mod png;
pub mod ppm;
pub mod scale;
pub mod scene;
pub mod text;
pub mod theme;

pub use compose::{SurfaceRef, compose, compose_full};
pub use format::{Channel, PixelFormat, Rgb};
pub use framebuffer::{Framebuffer, Geometry, MemFramebuffer, RawFramebuffer};
pub use geom::{Point, Rect, Size};
pub use hash::hash_visible;
