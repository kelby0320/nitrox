//! GUID partition tables, read or written, and a master boot record, written.
//!
//! The kernel's parser (`kernel/src/drivers/gpt.rs`) is separate and checks neither CRC; this one
//! is held to `sgdisk`.

#![cfg_attr(not(test), no_std)]

pub mod crc32;
pub mod mbr;
pub mod table;

/// The label of `install-root.img`'s one partition, which `nxinstall` copies its root from; the
/// build and the storage service (which skips mounting it) must use the same spelling.
pub const INSTALL_SOURCE_LABEL: &str = "nitrox-source";
