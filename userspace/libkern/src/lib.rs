//! `libkern` — the raw userspace syscall surface for Nitrox.
//!
//! The bottom layer of the userspace runtime (see `userspace/CLAUDE.md`): the
//! canonical userspace mirror of the kernel ABI — syscall numbers + the
//! `syscall`-instruction wrappers ([`syscall`]), the `#[repr(C)]` boundary types
//! ([`abi`]), [`KError`](error::KError), [`Rights`](handle::Rights),
//! [`KObjectType`](handle::KObjectType), thin debug helpers ([`debug`]), and [`scrub`], for
//! the passwords that pass through a process.
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
pub mod syscaps;

// Freestanding `mem*` intrinsics — only for the bare build; under `cargo test`
// libkern is a host `std` crate and must not redefine libc's `mem*`.
#[cfg(not(test))]
pub mod mem;

pub use abi::*;
pub use debug::{exit, kprint, kprint_hex, kprint_u64};
pub use error::{KError, from_raw};
pub use handle::*;
pub use syscall::*;
pub use syscaps::{SYSCAP_BIND_NAMESPACE, SYSCAP_REAL_TIME, SysCaps};

/// Zero `bytes` where they lie — a password, once it has been used.
///
/// **Volatile, a byte at a time**, because an ordinary fill of memory that nothing reads again is
/// a dead store the optimiser may delete, and a scrub that was compiled out looks exactly like one
/// that ran.
pub fn scrub(bytes: &mut [u8]) {
    for b in bytes {
        // SAFETY: `b` is a valid, exclusive reference to one byte.
        unsafe { core::ptr::write_volatile(b, 0) };
    }
}

#[cfg(test)]
mod scrub_tests {
    #[test]
    fn scrub_zeroes_every_byte_and_nothing_past_them() {
        let mut b = [0xA5u8; 9];
        super::scrub(&mut b[1..8]);
        assert_eq!(b, [0xA5, 0, 0, 0, 0, 0, 0, 0, 0xA5]);
    }
}
