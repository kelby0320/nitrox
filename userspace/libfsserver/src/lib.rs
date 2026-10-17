//! `libfsserver` — the half of a filesystem server that is the protocol.
//!
//! **One protocol, two servers.** The setup message, `Ready` or a refusal, the forwarding
//! endpoint, directory sessions, a rename resolved ahead of a session, `File::Forget` before a
//! file's blocks are freed, `File::Touch` by id, and `Meta::Unmount`: a fix to one fixes both.
//!
//! `no_std`, no `alloc`: the server loop's buffers are statics, one set per process, which is one
//! server. See `CLAUDE.md` beside this file.

#![cfg_attr(not(test), no_std)]

pub mod block;
pub mod cache;
pub mod disk;
pub mod serve;
pub mod server;
pub mod volume;

pub use block::{BlockReader, BlockRun, BlockWriter, FsError, RUN_UNWRITTEN, ReadOnly};
pub use volume::{DirEntry, Mapped, Refusal, Volume};
