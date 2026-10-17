//! Architecture-neutral syscall dispatch: the dispatch table ([`table`]) and the kernel error
//! space ([`error`]). The arch layer owns the privilege transition and the register frame; it
//! decodes the syscall number and arguments and calls [`table::dispatch`]. See
//! `docs/spec/syscall-abi.md`.
//! ([`error`]). See `docs/spec/syscall-abi.md`.

pub mod error;
pub mod table;

pub use error::{KError, SysResult};
