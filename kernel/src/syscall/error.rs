//! Kernel error space and the syscall return-value encoding (`docs/spec/syscall-abi.md`).
//!
//! A syscall returns one `isize`: **negative** is a [`KError`] discriminant, **non-negative**
//! operation-specific. The values are the contract (`docs/reference/error-codes.md`), not those
//! of `docs/archive/os-design-v5.1.md`: `WouldBlock`/`TimedOut`/`PeerClosed` took `-11..-13`.

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
    /// A non-blocking operation could not complete at once (a `sys_wait` poll found nothing).
    WouldBlock = -11,
    /// A blocking operation's deadline elapsed before it completed.
    TimedOut = -12,
    /// The other end has gone: an IPC peer endpoint closed, or a device departed (reads, once
    /// drained, and lights writes are refused).
    PeerClosed = -13,
    /// The name to be created is already taken (`sys_ns_bind` on an occupied path; a server's
    /// create/rename onto an existing entry). Well-formed, unlike `InvalidArgument`, so a
    /// caller can treat it as success (`mkdir --parents`).
    AlreadyExists = -14,
    /// A container still has members and the operation needs it empty (`rmdir`): a recursive
    /// remove learns that something else added an entry.
    NotEmpty = -15,
    /// An argument was malformed or out of range.
    InvalidArgument = -30,
    /// A user buffer was inaccessible (bad address or page fault).
    FaultFromUser = -31,
    /// A length/size exceeded the permitted maximum.
    TooLarge = -32,
    /// A device or medium I/O error.
    IoError = -40,
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
        UserAccessError::BadAddress | UserAccessError::Fault => KError::FaultFromUser,
        UserAccessError::Misaligned | UserAccessError::NoTerminator => KError::InvalidArgument,
    }
}

/// A handler's result: `Ok(non-negative)` or `Err(`[`KError`]`)`, collapsed by [`encode`].
pub type SysResult = Result<isize, KError>;

/// Collapse a [`SysResult`] into the `isize` the ABI returns.
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
        // Not v5.1's `-11`: the blocking errors took `-11..-13` first.
        assert_eq!(KError::AlreadyExists.as_isize(), -14);
        assert_eq!(KError::NotEmpty.as_isize(), -15);
        assert_eq!(KError::FaultFromUser.as_isize(), -31);
        assert_eq!(KError::IoError.as_isize(), -40);
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
        assert_eq!(from_user_access(UserAccessError::NoTerminator), KError::InvalidArgument);
    }
}
