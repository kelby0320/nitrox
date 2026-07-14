//! `libos` — the typed, async userspace runtime for Nitrox.
//!
//! The typed/async face of the raw `libkern` syscall surface. Design contract:
//! `docs/architecture/libos.md`.
//!
//! This slice (Phase 3 slice 5) ships the **async core** — the [`Op`] future over
//! `sys_wait` and [`block_on`], plus an `io::Error`-shaped [`Error`] — and (in later
//! parts) the `Handle<T, M>` typestate wrappers. It is **`#![no_std]` with no
//! `alloc`**, so the heap-free binaries (eshell/parent/fs-server) can use it too.
//!
//! ## The async core
//!
//! A potentially-blocking syscall is issued via [`Op::submit`], which returns a
//! future resolving to the operation's [`IoResult`](libkern::IoResult). Drive one to
//! completion with [`block_on`]:
//!
//! ```ignore
//! let op = Op::submit(console, &read_op)?;   // sys_io_submit, non-blocking
//! let done = block_on(op)?;                   // polls + sys_wait until complete
//! let n = done.result as usize;               // bytes read
//! ```
//!
//! [`block_on`] is the single-task driver — the same poll→`sys_wait`→re-poll loop a
//! future multi-task executor would run, minus the ready queue (deferred; see the
//! design doc). It collapses the `po_wait` idiom copy-pasted into every binary today.

#![cfg_attr(not(test), no_std)]

mod error;
mod exec;
mod handle;
mod objects;
mod sys;

pub use error::{Error, ErrorKind, Result};
pub use exec::{Op, block_on};
pub use handle::{
    CanBind, CanLookup, CanMapRead, CanMapWrite, CanRead, CanWrite, Handle, MapExec, MapRead,
    MapReadWrite, Memory, Namespace, Notify, NsMutable, NsReadOnly, Only, Process, ReadOnly,
    ReadWrite, Resource, Thread, WriteOnly,
};
pub use objects::{spawn, thread_create};
