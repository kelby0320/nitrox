//! init's host-testable internals.
//!
//! `init` is a library + binary crate: this library holds the logic that can be
//! unit-tested on the host (the bump-allocator math now; the `toml_lite` parser
//! in Part 4), while `src/main.rs` is the bare-target PID-1 entry point that uses
//! it. `#![no_std]` for the bare build; `std` under `cargo test` so the host
//! harness works (`cargo xtask test` runs `cargo test -p init --lib`).

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod heap;
pub mod manifest;
pub mod toml_lite;
