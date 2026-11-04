//! The syscall seam.
//!
//! The raw kernel calls libos makes, behind a thin module so everything above it is
//! host-testable with no kernel. [`real`] forwards to `libkern`'s `syscallN`; under test, or the
//! `mock` feature in an unwinding build, [`mock`], a small model of the kernel, takes its place. All returns
//! are the kernel's `i64`:
//! negative = a `KError` discriminant. Each takes typed arguments, so what the kernel is handed is
//! what the types say.

use libkern::{IoResult, SYS_FILE_CREATE, SYS_FILE_GROW, SYS_FILE_TRUNCATE};

/// The three file verbs that resolve a path to a file of a size: what each does to the size.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resize {
    /// Make the file, then grow it to the size.
    Create,
    /// Grow an existing file to at least the size.
    Grow,
    /// Shrink an existing file to the size.
    Truncate,
}

impl Resize {
    #[cfg_attr(any(test, feature = "mock"), allow(dead_code, reason = "the mock answers by the variant"))]
    pub(super) fn number(self) -> u64 {
        match self {
            Resize::Create => SYS_FILE_CREATE,
            Resize::Grow => SYS_FILE_GROW,
            Resize::Truncate => SYS_FILE_TRUNCATE,
        }
    }
}

/// `sys_wait` on as many of `handles` as `results` has room for — returns the count of signaled
/// handles (one `IoResult` each), or an error. `deadline`: `0` = poll, `u64::MAX` = forever; on
/// no handles, a sleep until a deadline between, which the kernel never refuses.
pub fn wait(handles: &[u64], results: &mut [IoResult], deadline: u64) -> i64 {
    let count = handles.len().min(results.len());
    wait_exact(&handles[..count], &mut results[..count], deadline)
}

#[cfg(not(any(test, all(feature = "mock", panic = "unwind"))))]
use real::wait_exact;
#[cfg(not(any(test, all(feature = "mock", panic = "unwind"))))]
pub use real::*;

#[cfg(not(any(test, all(feature = "mock", panic = "unwind"))))]
mod real;

#[cfg(any(test, all(feature = "mock", panic = "unwind")))]
pub use mock::*;

/// The model of the kernel the host tests run on: libos's, and, through the `mock` feature, a
/// crate's above it.
#[cfg(any(test, all(feature = "mock", panic = "unwind")))]
pub mod mock;
