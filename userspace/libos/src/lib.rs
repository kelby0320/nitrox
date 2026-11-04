//! `libos` — the typed, async userspace runtime for Nitrox, and every crate's way to the kernel.
//!
//! The typed/async face of the raw `libkern` syscall surface. Design contract:
//! `docs/architecture/libos.md`.
//!
//! It is **`#![no_std]` with no `alloc`**, so the heap-free binaries (eshell, fs-server)
//! can use it too.
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
//! [`block_on`] is the single-task driver: poll, `sys_wait`, re-poll.

#![cfg_attr(not(any(test, all(feature = "mock", panic = "unwind"))), no_std)]

pub mod boot;
mod channel;
pub mod control;
pub mod entropy;
mod error;
mod exec;
pub mod fs;
mod handle;
mod io;
pub mod log;
mod memory;
mod ns;
mod process;
mod sys;
pub mod thread;
pub mod time;
mod wait;

pub use channel::{Carried, Msg};
pub use error::{Error, ErrorKind, Result};
pub use exec::{Op, block_on};
pub use handle::{
    CanBind, CanLookup, CanMapRead, CanMapWrite, CanRead, CanWrite, Channel, File, Handle, Handles, MapExec, MapRead,
    MapReadWrite, Mappable, Memory, Namespace, Notify, NsMutable, NsReadOnly, Only, Process, ReadOnly, ReadWrite, Ref,
    Resource, Thread, WriteOnly,
};
pub use memory::Mapping;
pub use ns::Bindings;
pub use process::{Refused, Spawn, exit, spawn};
pub use thread::thread_create;
pub use time::Deadline;
pub use wait::{Signalled, WaitSet};

/// The model of the kernel libos's calls reach in an unwinding build under the `mock` feature,
/// and its test controls: a crate's host tests take it as a dev-dependency, so a path through
/// libos runs there.
#[cfg(all(feature = "mock", panic = "unwind"))]
pub use sys::mock;
