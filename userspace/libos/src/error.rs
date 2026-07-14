//! `libos::Error` — an `std::io::Error`-shaped wrapper over [`KError`].
//!
//! Shaped like `std::io::Error` (a [`kind`](Error::kind) mapping to an
//! `io::ErrorKind`-analog, `From<KError>`, `Display`) so a future `std` port's
//! `std::io::Error` can re-export rather than adapt — the "std-shaped where free"
//! rule (`docs/architecture/overview.md`).

use core::fmt;
use libkern::KError;

/// libos's result type.
pub type Result<T> = core::result::Result<T, Error>;

/// An error from a libos operation: a kernel [`KError`], plus a coarse
/// [`ErrorKind`] classification.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Error {
    code: KError,
}

impl Error {
    /// Build an `Error` from a raw negative syscall status (an `IoResult.status`
    /// or a syscall return). A non-negative `status` is not an error and should not
    /// reach here.
    pub fn from_status(status: i32) -> Error {
        Error {
            code: KError::from_i32(status),
        }
    }

    /// The underlying kernel error.
    pub fn code(&self) -> KError {
        self.code
    }

    /// A coarse, `std::io::ErrorKind`-shaped classification.
    pub fn kind(&self) -> ErrorKind {
        ErrorKind::from_kerror(self.code)
    }
}

impl From<KError> for Error {
    fn from(code: KError) -> Error {
        Error { code }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "libos error: {:?}", self.code)
    }
}

/// A coarse error classification mirroring `std::io::ErrorKind`, so a future `std`
/// facade maps onto it directly. `#[non_exhaustive]` — variants may be added.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum ErrorKind {
    /// A named resource does not exist ([`KError::NotFound`]).
    NotFound,
    /// The handle lacked a required right ([`KError::NoAccess`]).
    PermissionDenied,
    /// A non-blocking operation would block ([`KError::WouldBlock`]).
    WouldBlock,
    /// A deadline elapsed ([`KError::TimedOut`]).
    TimedOut,
    /// An IPC peer closed ([`KError::PeerClosed`]).
    BrokenPipe,
    /// A malformed handle / argument / buffer.
    InvalidInput,
    /// The kernel heap or handle table was exhausted.
    OutOfMemory,
    /// The operation is not implemented.
    Unsupported,
    /// A device/medium error, or anything not otherwise classified.
    Other,
}

impl ErrorKind {
    fn from_kerror(e: KError) -> ErrorKind {
        match e {
            KError::NotFound => ErrorKind::NotFound,
            KError::NoAccess => ErrorKind::PermissionDenied,
            KError::WouldBlock => ErrorKind::WouldBlock,
            KError::TimedOut => ErrorKind::TimedOut,
            KError::PeerClosed => ErrorKind::BrokenPipe,
            KError::InvalidHandle
            | KError::InvalidArgument
            | KError::FaultFromUser
            | KError::TooLarge => ErrorKind::InvalidInput,
            KError::OutOfMemory | KError::OutOfHandles => ErrorKind::OutOfMemory,
            KError::Unsupported => ErrorKind::Unsupported,
            KError::IoError | KError::KernelError => ErrorKind::Other,
        }
    }
}
