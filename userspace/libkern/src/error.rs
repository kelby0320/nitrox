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
            -30 => KError::InvalidArgument,
            -31 => KError::FaultFromUser,
            -32 => KError::TooLarge,
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
            KError::InvalidArgument,
            KError::FaultFromUser,
            KError::TooLarge,
            KError::Unsupported,
            KError::KernelError,
        ] {
            assert_eq!(KError::from_i32(e.as_i32()), e);
        }
    }
}
