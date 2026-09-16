//! GUID partition tables: read one, or write one (Phase 5 Part H.1).
//!
//! **What the installer needs and nothing more.** It writes a table with a couple of partitions on
//! a disk it is about to take over, and reads the one inside the image it copies from. There is no
//! editing, no growing, no repair: a table is built whole and written whole, which is also what
//! makes it testable — the bytes are a pure function of the request.
//!
//! **The kernel has a parser too** (`kernel/src/drivers/gpt.rs`), deliberately not shared: that one
//! runs at boot with no allocator against a disk nobody has checked, and checks neither CRC. This
//! one is held to `sgdisk` on the host. Two implementations of a published format, tested against
//! each other and against the tool everyone else uses, is the arrangement this project already
//! applies to its own render pipeline.

#![cfg_attr(not(test), no_std)]

pub mod crc32;
pub mod table;
