//! `libkern` — the raw userspace syscall surface for Nitrox.
//!
//! The bottom layer of the userspace runtime (see `userspace/CLAUDE.md`): syscall numbers and
//! wrappers ([`syscall`]), the `#[repr(C)]` boundary types ([`abi`]),
//! [`KError`](error::KError), [`Rights`](handle::Rights),
//! [`KObjectType`](handle::KObjectType), debug helpers ([`debug`]), and [`scrub`].
//!
//! `#![no_std]`, no `alloc`, `core` only — init links it before any heap exists. Under `test`
//! the crate is compiled with `std` so its pure logic can be unit-tested host-side; the
//! `syscall`-instruction wrappers compile there but are never invoked.
//!
//! This crate is the **single source** for the userspace ABI. When it changes, the kernel side
//! (`kernel/src/syscall/` + `kernel/src/libkern/`) and `docs/spec/syscall-abi.md` must change
//! identically; the compile-time `offset_of!`/`size_of` asserts in [`abi`] self-pin each layout.

#![cfg_attr(not(test), no_std)]

pub mod abi;
pub mod channel;
pub mod control;
pub mod debug;
pub mod device;
pub mod error;
pub mod handle;
pub mod syscall;
pub mod syscaps;

// Freestanding `mem*` intrinsics — only for the bare build; under `cargo test`
// libkern is a host `std` crate and must not redefine libc's `mem*`.
#[cfg(not(test))]
pub mod mem;

pub use abi::*;
pub use channel::{Received, channel_recv, channel_recv_raw};
pub use debug::{exit, kprint, kprint_hex, kprint_u64};
pub use error::{KError, from_raw};
pub use handle::*;
pub use syscall::*;
pub use syscaps::{SYSCAP_BIND_NAMESPACE, SYSCAP_REAL_TIME, SYSCAP_SYSTEM_CLOCK, SysCaps};

/// Zero `bytes` where they lie — a password, once it has been used.
///
/// **Volatile, a byte at a time**, because an ordinary fill of memory nothing reads again is a
/// dead store the optimiser may delete.
pub fn scrub(bytes: &mut [u8]) {
    for b in bytes {
        // SAFETY: `b` is a valid, exclusive reference to one byte.
        unsafe { core::ptr::write_volatile(b, 0) };
    }
}

/// **Bytes [`scrub`]bed when dropped**, so a password, or a buffer one passed through, is zeroed on
/// every way out of the scope that holds it. Neither `Clone` nor `Debug`.
pub struct Secret<B: AsMut<[u8]>>(pub B);

impl<B: AsMut<[u8]>> Drop for Secret<B> {
    fn drop(&mut self) {
        scrub(self.0.as_mut());
    }
}

impl<B: AsMut<[u8]>> core::ops::Deref for Secret<B> {
    type Target = B;
    fn deref(&self) -> &B {
        &self.0
    }
}

impl<B: AsMut<[u8]>> core::ops::DerefMut for Secret<B> {
    fn deref_mut(&mut self) -> &mut B {
        &mut self.0
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

    #[test]
    fn a_dropped_secret_is_zeroed() {
        let mut s = core::mem::MaybeUninit::new(super::Secret([0xA5u8; 9]));
        // SAFETY: dropped once, in place; the bytes stay readable as the plain data they are.
        let bytes = unsafe {
            core::ptr::drop_in_place(s.as_mut_ptr());
            *(s.as_ptr() as *const [u8; 9])
        };
        assert_eq!(bytes, [0; 9]);
    }
}
