//! logging-service's host-testable internals.
//!
//! This library holds the log-path classifier and the ring `Read` answers from (both
//! host-tested); `src/main.rs` is the bare-target resource server that uses them.
//! `#![no_std]` for the bare build; `std` under `cargo test`.
//!
//! See `docs/architecture/logging.md`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod path;
pub mod ring;
