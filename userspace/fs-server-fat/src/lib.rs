//! `fs-server-fat` — FAT12, FAT16 and FAT32, with long names.
//!
//! A library behind `libfsserver`'s [`BlockReader`] and [`BlockWriter`], so it is host-tested
//! against images the host's FAT tools build, and served by this crate's binary over the device.
//!
//! **No `alloc`**: every buffer is the caller's or a bounded one on the stack, as in
//! `fs-server-ext4`'s library. **A FAT is anyone's bytes**: a stick from a shop, written by any
//! system. Every field read off one is checked before it is used, and a malformed one is an
//! [`FsError`], never a panic.

#![cfg_attr(not(test), no_std)]

pub mod bpb;
pub mod dir;
pub mod mkfs;
pub mod names;
pub mod table;
pub mod time;
pub mod volume;

pub use libfsserver::{BlockReader, BlockRun, BlockWriter, FsError, ReadOnly};
pub use volume::Fat;

#[cfg(test)]
pub(crate) mod test_support;
