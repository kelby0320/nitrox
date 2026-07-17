//! logging-service's host-testable internals.
//!
//! `logging-service` is a library + binary crate (mirroring profile-server): this library
//! holds the log-path classifier (host-tested); `src/main.rs` is the bare-target resource
//! server that uses it. `#![no_std]` for the bare build; `std` under `cargo test`.
//!
//! See `docs/architecture/logging.md`.

#![cfg_attr(not(test), no_std)]

pub mod path;
