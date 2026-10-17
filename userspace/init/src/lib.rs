//! init's host-testable internals: the first message a resource server sends ([`ready`]), the
//! shutdown request ([`unmount`]) and, re-exported from `libinittoml`, the manifest parsers.
//! `src/main.rs` is the bare-target entry point; `std` under `cargo test`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod ready;
pub mod unmount;
/// The manifest parser, shared with the storage service and kept at its old path here.
pub use libinittoml::{manifest, toml_lite};
