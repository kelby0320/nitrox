//! service-mgr's host-testable internals.
//!
//! `service-mgr` is a library + binary crate (mirroring init): this library holds the
//! logic that can be unit-tested on the host (the service-declaration parser), while
//! `src/main.rs` is the bare-target supervisor entry point that uses it.
//! `#![no_std]` for the bare build; `std` under `cargo test` so the host harness works
//! (`cargo xtask test` runs `cargo test -p service-mgr --lib`).
//!
//! The bare-target binary provides the `#[global_allocator]` (`libheap`); this library
//! only needs `alloc`. See `docs/architecture/service-manager.md`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod service_toml;
