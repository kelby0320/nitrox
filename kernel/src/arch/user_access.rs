//! Architecture-neutral user-memory copy contract.
//!
//! [`ArchUserAccess`] is the raw copy primitive the safe user-access layer
//! ([`crate::mm::user_access`]) calls: it copies bytes between kernel and user
//! memory inside an access window that the hardware otherwise forbids (SMAP on
//! x86_64, PAN on aarch64), converting a `#PF` during the copy into a fault
//! flag via the kernel-wide exception table. The window's open/close
//! instructions live *inside* the copy asm and never outlive it — see the
//! impl. The neutral [`UserPtr`](crate::mm::user_access::UserPtr) /
//! validation / public `copy_*_user` API is the arch-neutral half in
//! `crate::mm::user_access`.

pub use crate::arch::x86_64::user_access::CstrCopyOutcome;

/// Test-only fault-injection hook, re-exported on the neutral path so the
/// `mm::user_access` tests reach it without naming an arch-internal module.
#[cfg(test)]
pub(crate) use crate::arch::x86_64::user_access::FAIL_NEXT_CSTR_COPY;

/// Raw user/kernel copy primitives operating under the arch's access window.
pub trait ArchUserAccess {
    /// Fill `dst` from the user address `src` under the access window. Returns `true` if a `#PF`
    /// occurred inside the window: `dst` holds a partial copy, and the caller treats the
    /// operation as failed.
    ///
    /// # Safety
    /// `src` is a user address for `dst.len()` bytes, which may fault: the recovery path turns a
    /// fault into the `true` return. The arch's access protection is enabled.
    unsafe fn copy_in(dst: &mut [u8], src: *const u8) -> bool;

    /// Copy `src` to the user address `dst` under the access window; `true` on a `#PF`, after
    /// which a prefix of `src` may have landed.
    ///
    /// # Safety
    /// As [`copy_in`](ArchUserAccess::copy_in): `dst` is a user address for `src.len()` bytes.
    unsafe fn copy_out(dst: *mut u8, src: &[u8]) -> bool;

    /// Byte-by-byte copy of a NUL-terminated user string into `dst`, stopping at the first NUL or
    /// when `dst` is full. An empty `dst` is [`CstrCopyOutcome::NoTerminator`], touching nothing.
    ///
    /// # Safety
    /// As [`copy_in`](ArchUserAccess::copy_in): `src` is a user address for `dst.len()` bytes,
    /// and the arch's access protection is enabled.
    unsafe fn copy_cstr(dst: &mut [u8], src: *const u8) -> CstrCopyOutcome;
}
