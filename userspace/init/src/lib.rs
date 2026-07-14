//! init's host-testable internals.
//!
//! `init` is a library + binary crate: this library holds the logic that can be
//! unit-tested on the host (the `manifest` + `toml_lite` parsers), while
//! `src/main.rs` is the bare-target PID-1 entry point that uses it. `#![no_std]` for
//! the bare build; `std` under `cargo test` so the host harness works (`cargo xtask
//! test` runs `cargo test -p init --lib`).
//!
//! init's `#[global_allocator]` is now `libheap` (the freeing userspace heap, slice
//! 4); the former fixed bump arena (`heap.rs`) is gone and its allocator tests moved
//! to `libheap`'s own suite.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod manifest;
pub mod toml_lite;
