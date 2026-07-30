//! [`KError`] — the kernel error space, mirrored from `kernel/src/syscall/error.rs`.
//!
//! Every syscall returns a single `isize`: a **negative** value is a `KError`
//! discriminant; a non-negative value is operation-specific (a byte count, a
//! handle, or `0`). The numeric values are the contract and must match the kernel.

/// A kernel error, as returned (negated) across the syscall boundary.
/// `#[repr(i32)]` so the discriminant is exactly the wire value.
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
    /// A non-blocking operation could not complete immediately.
    WouldBlock = -11,
    /// A blocking operation's deadline elapsed before it completed.
    TimedOut = -12,
    /// An IPC channel's peer endpoint has closed.
    PeerClosed = -13,
    /// The name the operation would create is already taken. Well-formed
    /// request, occupied name — which is why `mkdir --parents` can treat it as
    /// success instead of asking a second time.
    AlreadyExists = -14,
    /// A container still has members and the operation requires it to be empty.
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
    /// Decode a raw negative syscall return into a `KError`. An unrecognised
    /// value maps to [`KError::KernelError`] (forward-compat: a kernel newer than
    /// this `libkern` may return an error this build doesn't name).
    ///
    /// **Every variant above must have an arm here.** The forward-compat fallback
    /// means an omission is silent — `IoError` was missing from 2026-06 until
    /// 2026-07-30, so every device error decoded as `KernelError` while the enum
    /// itself matched the kernel perfectly. `cargo xtask abi-sync-check` now
    /// checks this table against the kernel's variants for exactly that reason.
    pub const fn from_i32(v: i32) -> KError {
        match v {
            -1 => KError::InvalidHandle,
            -2 => KError::NoAccess,
            -3 => KError::OutOfHandles,
            -4 => KError::OutOfMemory,
            -10 => KError::NotFound,
            -11 => KError::WouldBlock,
            -12 => KError::TimedOut,
            -13 => KError::PeerClosed,
            -14 => KError::AlreadyExists,
            -15 => KError::NotEmpty,
            -30 => KError::InvalidArgument,
            -31 => KError::FaultFromUser,
            -32 => KError::TooLarge,
            -40 => KError::IoError,
            -52 => KError::Unsupported,
            _ => KError::KernelError,
        }
    }

    /// The `i32` wire value (the negative discriminant).
    pub const fn as_i32(self) -> i32 {
        self as i32
    }
}

/// Split a raw syscall return (`i64`) into `Ok(non-negative)` or `Err(KError)`.
/// The thin convenience the safe wrappers are built from: a negative return is a
/// `KError` discriminant, a non-negative is the operation's value (count/handle/0).
pub fn from_raw(ret: i64) -> Result<i64, KError> {
    if ret < 0 {
        Err(KError::from_i32(ret as i32))
    } else {
        Ok(ret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_raw_splits_sign() {
        assert_eq!(from_raw(0), Ok(0));
        assert_eq!(from_raw(42), Ok(42));
        assert_eq!(from_raw(-10), Err(KError::NotFound));
        assert_eq!(from_raw(-2), Err(KError::NoAccess));
    }

    #[test]
    fn unknown_negative_is_kernel_error() {
        assert_eq!(KError::from_i32(-9999), KError::KernelError);
    }

    /// Every variant must survive `as_i32` → `from_i32`.
    ///
    /// This list is only as good as its completeness, which is how `IoError` sat
    /// undecodable for a month: it was in the enum, absent from `from_i32`, and
    /// absent from here too, so nothing failed. The enumeration is duplicated in
    /// `xtask abi-sync-check`, which derives it from the *kernel's* enum rather
    /// than from this file — a list that cannot be kept in step by the same
    /// oversight that made it wrong.
    #[test]
    fn discriminants_round_trip() {
        for e in [
            KError::InvalidHandle,
            KError::NoAccess,
            KError::OutOfHandles,
            KError::OutOfMemory,
            KError::NotFound,
            KError::WouldBlock,
            KError::TimedOut,
            KError::PeerClosed,
            KError::AlreadyExists,
            KError::NotEmpty,
            KError::InvalidArgument,
            KError::FaultFromUser,
            KError::TooLarge,
            KError::IoError,
            KError::Unsupported,
            KError::KernelError,
        ] {
            assert_eq!(KError::from_i32(e.as_i32()), e);
        }
    }

    /// The regression proper: a device error must not read as an internal one.
    #[test]
    fn io_error_decodes_as_itself() {
        assert_eq!(KError::from_i32(-40), KError::IoError);
        assert_ne!(KError::from_i32(-40), KError::KernelError);
    }

    /// The two errors this pass exists to make visible are distinguishable from
    /// each other and from the `InvalidArgument` they used to share.
    #[test]
    fn create_and_empty_errors_are_distinct() {
        assert_eq!(from_raw(-14), Err(KError::AlreadyExists));
        assert_eq!(from_raw(-15), Err(KError::NotEmpty));
        assert_ne!(KError::AlreadyExists, KError::InvalidArgument);
        assert_ne!(KError::NotEmpty, KError::InvalidArgument);
        assert_ne!(KError::AlreadyExists, KError::NotEmpty);
    }
}
