//! A handle narrowed to rights no libos type names.

use libkern::{Rights, SYS_HANDLE_RESTRICT, syscall2};
use libos::Handle;

/// Narrow `h` to `rights`, in place: whether the kernel did. Raw because libos attenuates only to
/// drop `TRANSFER` or `DUPLICATE`, and these handles lack `WAIT`, `INSPECT` or the rest.
pub(crate) fn narrow<T, M>(h: &Handle<T, M>, rights: Rights) -> bool {
    // SAFETY: register-only syscall on a handle this process holds.
    unsafe { syscall2(SYS_HANDLE_RESTRICT, h.raw().0, rights.bits()) == 0 }
}
