//! Architecture-neutral syscall dispatch: the dispatch table ([`table`]) and the kernel error
//! space ([`error`]). The arch layer owns the privilege transition and the register frame; it
//! decodes the syscall number and arguments and calls [`dispatch`]. See
//! `docs/spec/syscall-abi.md`.

mod block_map;
#[cfg(any(test, feature = "syscall-counts"))]
#[cfg_attr(not(feature = "syscall-counts"), allow(dead_code, reason = "host-tested for its format alone"))]
pub(crate) mod counts;
pub mod error;
pub mod table;

pub use error::{KError, SysResult};

/// Serve syscall `nr` with its six arguments: [`table::dispatch`], counted first in a kernel built
/// to count.
pub fn dispatch(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> isize {
    #[cfg(feature = "syscall-counts")]
    counts::before(nr, a0, a1);
    table::dispatch(nr, a0, a1, a2, a3, a4, a5)
}
