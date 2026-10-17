//! `libdraw` — geometry, pixel formats, framebuffers, compositing, and the theme.
//!
//! The pixel layer shared by the compositor and its clients. The theme lives here because the
//! compositor paints chrome (cursor, drag outline, ground) but links no widget toolkit; see
//! [`theme`].
//!
//! [`framebuffer::Framebuffer`] is the seam: a real implementation over a mapped aperture
//! ([`framebuffer::RawFramebuffer`]) and an in-memory one for tests
//! ([`framebuffer::MemFramebuffer`]). Behind it [`compose::compose`] is a pure function of
//! (surfaces, geometry, damage, stacking), so it is asserted pixel-exactly on the host.
//!
//! Determinism is a design constraint (`docs/design/display-substrate.md` §7): the same inputs
//! must give the same bytes on any machine, because [`hash::hash_visible`] and
//! [`scene::REFERENCE_HASH`] are the gate.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

#[cfg(feature = "io")]
pub mod acquire;
pub mod compose;
pub mod corner;
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
