//! logging-service's host-testable internals.
//!
//! `logging-service` is a library + binary crate (mirroring profile-server): this library
//! holds the log-path classifier and, since administration Part E.6, the ring `Read` answers
//! from (both host-tested); `src/main.rs` is the bare-target resource server that uses them.
//! `#![no_std]` for the bare build; `std` under `cargo test`.
//!
//! See `docs/architecture/logging.md`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod path;
pub mod ring;
