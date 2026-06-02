//! Architecture-neutral syscall dispatch.
//!
//! The arch layer owns the privilege transition — the entry stub, the
//! register save/restore, the per-CPU kernel stack — and the register
//! frame it builds. It decodes the syscall number and arguments and calls
//! [`table::dispatch`]. This module holds only the architecture-independent
//! surface: the dispatch table ([`table`]) and the kernel error space
//! ([`error`]). See `docs/spec/syscall-abi.md`.

pub mod error;
pub mod table;

pub use error::{KError, SysResult};
