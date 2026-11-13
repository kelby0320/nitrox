//! The syscall seam.
//!
//! The raw kernel calls libos makes, behind a thin module so everything above it is
//! host-testable with no kernel. [`real`] forwards to `libkern`'s `syscallN`; under test, or the
//! `mock` feature in an unwinding build, [`mock`], a small model of the kernel, takes its place. All returns
//! are the kernel's `i64`:
//! negative = a `KError` discriminant. Each takes typed arguments, so what the kernel is handed is
//! what the types say.

use libkern::{
    IoResult, RESOLVE_OP_CREATE, RESOLVE_OP_FLUSH, RESOLVE_OP_GROW, RESOLVE_OP_HELD, RESOLVE_OP_RENAME,
    RESOLVE_OP_TRUNCATE, ResolveOp,
};

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

/// **What a resolve does beyond resolving**, as the seam takes it: the kernel's `ResolveOp`, typed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resolve<'a> {
    Plain,
    /// The file's size changed first, as the verb says.
    Size(Resize, u64),
    /// The path renamed to `to`, with the server's flags.
    Rename {
        to: &'a [u8],
        flags: u16,
    },
    Flush,
    Held,
}

impl Resolve<'_> {
    /// The `ResolveOp` the kernel reads, `None` for a plain lookup; a rename's points at `to`.
    #[cfg_attr(any(test, feature = "mock"), allow(dead_code, reason = "the mock answers by the variant"))]
    pub(super) fn op(self) -> Option<ResolveOp> {
        let code = |code| ResolveOp { code, ..ResolveOp::default() };
        Some(match self {
            Resolve::Plain => return None,
            Resolve::Size(verb, size) => {
                let verb = match verb {
                    Resize::Create => RESOLVE_OP_CREATE,
                    Resize::Grow => RESOLVE_OP_GROW,
                    Resize::Truncate => RESOLVE_OP_TRUNCATE,
                };
                ResolveOp { size, ..code(verb) }
            }
            Resolve::Rename { to, flags } => ResolveOp {
                flags: flags as u32,
                dest_ptr: to.as_ptr() as u64,
                dest_len: to.len() as u64,
                ..code(RESOLVE_OP_RENAME)
            },
            Resolve::Flush => code(RESOLVE_OP_FLUSH),
            Resolve::Held => code(RESOLVE_OP_HELD),
        })
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
