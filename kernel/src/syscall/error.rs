//! Kernel error space and the syscall return-value encoding.
//!
//! Per `docs/spec/syscall-abi.md`, every syscall returns a single `isize`:
//! a **negative** value is a [`KError`] discriminant; a **non-negative**
//! value is operation-specific (a byte count, a handle, or `0`). [`KError`]
//! is `#[repr(i32)]` so the discriminant is exactly what crosses the
//! boundary (sign-extended into the `isize` return register).
//!
//! Only the variants the current slices use are listed; the rest of the
//! v5.1 error space (`docs/history/os-design-v5.1.md`) is filled in as
//! syscalls that need them land. The numeric values are the contract and
//! must not change once userspace mirrors them.

use crate::mm::user_access::UserAccessError;

/// A kernel error, as returned (negated) across the syscall boundary.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KError {
    /// The supplied handle is not live in the caller's table.
    InvalidHandle = -1,
    /// The handle lacks a required right.
    NoAccess = -2,
    /// The handle table is full.
    OutOfHandles = -3,
    /// The kernel heap is exhausted.
    OutOfMemory = -4,
    /// A named resource does not exist.
    NotFound = -10,
    /// A non-blocking operation could not complete immediately (e.g. a poll —
    /// `sys_wait` with `deadline == 0` — found nothing signaled).
    WouldBlock = -11,
    /// A blocking operation's deadline elapsed before it completed (e.g.
    /// `sys_wait` timed out with no handle signaled).
    TimedOut = -12,
    /// An IPC channel's peer endpoint has closed: no further messages can be
    /// sent or received on this endpoint.
    PeerClosed = -13,
    /// An argument was malformed or out of range.
    InvalidArgument = -30,
    /// A user buffer was inaccessible (bad address or page fault).
    FaultFromUser = -31,
    /// A length/size exceeded the permitted maximum.
    TooLarge = -32,
    /// The operation is not implemented.
    Unsupported = -52,
    /// Catch-all for an unexpected internal condition.
    KernelError = -255,
}

impl KError {
    /// The `isize` wire value — the negative discriminant, sign-extended.
    pub const fn as_isize(self) -> isize {
        self as i32 as isize
    }
}

/// Convert a user-memory-access failure into the syscall error space.
pub fn from_user_access(e: UserAccessError) -> KError {
    match e {
        // An unmapped/inaccessible buffer or a fault mid-copy both read as
        // "the user pointer was not usable".
        UserAccessError::BadAddress | UserAccessError::Fault => KError::FaultFromUser,
        // Misalignment / missing terminator are malformed arguments.
        UserAccessError::Misaligned | UserAccessError::NoTerminator => {
            KError::InvalidArgument
        }
    }
}

/// A syscall handler's result: `Ok(non-negative)` on success, `Err` for a
/// [`KError`]. Collapsed to the single `isize` the ABI returns by
/// [`encode`].
pub type SysResult = Result<isize, KError>;

/// Collapse a [`SysResult`] into the `isize` the ABI returns: the success
/// value unchanged, or the error's negative discriminant.
pub fn encode(r: SysResult) -> isize {
    match r {
        Ok(v) => {
            debug_assert!(v >= 0, "syscall success value must be non-negative");
            v
        }
        Err(e) => e.as_isize(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminants_match_spec() {
        assert_eq!(KError::InvalidHandle.as_isize(), -1);
        assert_eq!(KError::WouldBlock.as_isize(), -11);
        assert_eq!(KError::TimedOut.as_isize(), -12);
        assert_eq!(KError::PeerClosed.as_isize(), -13);
        assert_eq!(KError::FaultFromUser.as_isize(), -31);
        assert_eq!(KError::TooLarge.as_isize(), -32);
        assert_eq!(KError::Unsupported.as_isize(), -52);
        assert_eq!(KError::KernelError.as_isize(), -255);
    }

    #[test]
    fn encode_passes_success_and_negates_errors() {
        assert_eq!(encode(Ok(0)), 0);
        assert_eq!(encode(Ok(13)), 13);
        assert_eq!(encode(Err(KError::TooLarge)), -32);
        assert_eq!(encode(Err(KError::FaultFromUser)), -31);
    }

    #[test]
    fn user_access_errors_map_into_kerror() {
        assert_eq!(from_user_access(UserAccessError::BadAddress), KError::FaultFromUser);
        assert_eq!(from_user_access(UserAccessError::Fault), KError::FaultFromUser);
        assert_eq!(from_user_access(UserAccessError::Misaligned), KError::InvalidArgument);
        assert_eq!(
            from_user_access(UserAccessError::NoTerminator),
            KError::InvalidArgument
        );
    }
}
