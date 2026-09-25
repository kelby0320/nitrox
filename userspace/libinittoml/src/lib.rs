//! **`init.toml`, parsed** — the bootstrap manifest's schema (`docs/spec/init-toml-schema.md`)
//! and the minimal TOML reader it needs.
//!
//! **Two consumers, so it lives below both.** `init` reads the manifest to mount the machine's
//! filesystems. The storage service (administration Part C.5) reads it to know which devices
//! `init` mounted, so it reports them and never mounts them twice. Both must read the file the
//! same way, or the service would disagree with `init` about what `init` did. It was `init`'s
//! own module until then.
//!
//! `#![no_std]` with `alloc` for the bare build, and `std` under `cargo test`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod manifest;
pub mod toml_lite;
