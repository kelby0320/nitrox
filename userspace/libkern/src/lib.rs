//! `libkern` — the raw userspace syscall surface for Nitrox.
//!
//! The bottom layer of the userspace runtime (see `userspace/CLAUDE.md`): the
//! canonical userspace mirror of the kernel ABI — syscall numbers + the
//! `syscall`-instruction wrappers ([`syscall`]), the `#[repr(C)]` boundary types
//! ([`abi`]), [`KError`](error::KError), [`Rights`](handle::Rights),
//! [`KObjectType`](handle::KObjectType), and thin debug helpers ([`debug`]).
//!
//! `#![no_std]`, no `alloc`, `core` only — init and the demos link it before any
//! heap exists. The one exception is `cargo test`, where the host harness needs
//! `std`; under `test` the crate is compiled with `std` so its pure logic
//! (formatting, error decoding, layout asserts) can be unit-tested host-side.
//! The `syscall`-instruction wrappers compile on the host but are never invoked
//! by a test.
//!
//! This crate is the **single source** for the userspace ABI; other userspace
//! crates use what's here rather than re-declaring syscall numbers or layouts.
//! When it changes, the kernel side (`kernel/src/syscall/` + `kernel/src/libkern/`)
//! and `docs/spec/syscall-abi.md` must change identically. A
//! `cargo xtask abi-sync-check` to enforce that is deferred
//! (`docs/rationale/deferred-decisions.md`); for now the compile-time
//! `offset_of!`/`size_of` asserts in [`abi`] self-pin each layout.

#![cfg_attr(not(test), no_std)]

pub mod abi;
pub mod debug;
pub mod error;
pub mod handle;
pub mod syscall;

// Freestanding `mem*` intrinsics — only for the bare build; under `cargo test`
// libkern is a host `std` crate and must not redefine libc's `mem*`.
#[cfg(not(test))]
pub mod mem;

pub use abi::*;
pub use debug::{exit, kprint, kprint_hex, kprint_u64};
pub use error::{KError, from_raw};
pub use handle::*;
pub use syscall::*;
