//! profile-server's host-testable internals.
//!
//! `profile-server` is a library + binary crate (mirroring init/service-mgr): this
//! library holds the profile-manifest parser (host-tested), while `src/main.rs` is the
//! bare-target resource server that uses it. `#![no_std]` for the bare build; `std`
//! under `cargo test` so the host harness works (`cargo xtask test` runs
//! `cargo test -p profile-server --lib`).
//!
//! The bare-target binary provides the `#[global_allocator]` (`libheap`); this library
//! only needs `alloc`. See `docs/architecture/profiles-and-namespace-projection.md`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod manifest;
