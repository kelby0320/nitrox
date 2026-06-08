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
//!
//! The active architecture's implementation is re-exported from
//! `crate::arch` as `UserAccess` (see `kernel/src/arch/mod.rs`).

pub use crate::arch::x86_64::user_access::CstrCopyOutcome;

/// Test-only fault-injection hook, re-exported on the neutral path so the
/// `mm::user_access` tests reach it without naming an arch-internal module.
#[cfg(test)]
pub(crate) use crate::arch::x86_64::user_access::FAIL_NEXT_CSTR_COPY;

/// Raw user/kernel copy primitives operating under the arch's access window.
pub trait ArchUserAccess {
    /// Copy `len` bytes from `src` to `dst` under the access window. Returns
    /// `true` if a `#PF` occurred inside the window (the kernel-side bytes are
    /// partially copied; the caller treats the operation as failed).
    ///
    /// # Safety
    /// Exactly one of `src`/`dst` points into user memory; the other is a
    /// valid kernel pointer for the direction. `len` bytes must be reachable
    /// for that direction — the user side may fault, which the recovery path
    /// converts into the `true` return. The two regions must not overlap, and
    /// the arch's access protection must be enabled.
    unsafe fn copy_bytes(dst: *mut u8, src: *const u8, len: usize) -> bool;

    /// Byte-by-byte copy of a NUL-terminated user string, stopping at the
    /// first NUL or when `max_len` bytes have been read.
    ///
    /// # Safety
    /// Same contract as [`copy_bytes`](ArchUserAccess::copy_bytes): `src`
    /// points into user memory, `dst` provides `max_len` writable kernel
    /// bytes, the arch's access protection is enabled.
    unsafe fn copy_cstr(dst: *mut u8, src: *const u8, max_len: usize) -> CstrCopyOutcome;
}
