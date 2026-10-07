//! `libfsserver` — the half of a filesystem server that is the protocol (Phase 6 Part E.1).
//!
//! **One protocol, two servers.** Everything `fs-server-ext4`'s binary did that was not ext4
//! moved here when `fs-server-fat` arrived: the setup message, `Ready` or a refusal, the
//! forwarding endpoint, directory sessions and their wait slots, a rename resolved ahead of a
//! session, `File::Forget` before a file's blocks are freed, `File::Touch` by id, and the control
//! channel's `Meta::Unmount`. A fix to any of them is a fix to both servers.
//!
//! - [`block`]: the device a filesystem reads, as a filesystem library sees it, and
//!   [`ReadOnly`], through which a read-only mount is served.
//! - [`volume`]: the filesystem, as the protocol sees it — the [`Volume`] trait a server
//!   implements over its library.
//! - [`serve`]: the pure request→reply core for a forwarded resolve or range read, generic over
//!   [`Volume`] and so host-tested through each server's volume.
//! - [`disk`]: the [`BlockReader`] and [`BlockWriter`] over `sys_io_submit`.
//! - [`server`]: the server loop and its bootstrap.
//!
//! `no_std`, no `alloc`, as the server it came out of: the server loop's buffers are statics, one
//! set per process, which is one server. See `CLAUDE.md` beside this file.

#![cfg_attr(not(test), no_std)]

pub mod block;
pub mod disk;
pub mod serve;
pub mod server;
pub mod volume;

pub use block::{BlockReader, BlockRun, BlockWriter, FsError, ReadOnly};
pub use volume::{DirEntry, Mapped, Refusal, Volume};
