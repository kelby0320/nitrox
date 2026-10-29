//! Why a device cannot be served: a superblock or root this server cannot read, or write.

use super::{MAX_BLOCK, SUPER_MAGIC, features};
use crate::FsError;

/// Why a device cannot be served, named specifically enough for a supervisor to print.
///
/// The same checks every request makes: [`read_superblock`](super::read_superblock) is
/// `parse_superblock` with the reason collapsed, so [`check_device`](super::check_device) cannot
/// drift from the check each lookup runs.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Unservable {
    /// The superblock's bytes could not be read off the device.
    Unreadable,
    /// No ext4 magic where the superblock belongs — the device holds no ext4 filesystem.
    NoMagic {
        /// What was there instead.
        found: u16,
    },
    /// 64-bit block numbers, which change the group-descriptor layout this reader walks.
    Wide64Bit,
    /// Blocks larger than the reader's 4 KiB scratch.
    BlockTooLarge {
        /// `s_log_block_size`: the block size is `1024 << log`.
        log: u32,
    },
    /// A superblock field the layout divides or indexes by is zero.
    ZeroField(&'static str),
    /// A superblock count its block groups cannot hold: a group of more blocks or inodes than its
    /// bitmap block has bits, or more inodes than the groups have.
    Geometry(&'static str),
    /// The root's inode could not be read: its table is past the device's end, or the device
    /// failed.
    RootUnreadable,
    /// Inode 2 is not a directory: the layout the superblock describes is not there.
    RootNotDirectory {
        /// The inode's `i_mode`.
        mode: u16,
    },
    /// A writable mount could not record that it is mounted: a filesystem that changed while
    /// looking clean would be worse than one not mounted.
    StateUnwritable,
    /// The journal holds changes not yet replayed: what is on disk is not the filesystem yet.
    NeedsRecovery,
    /// Features this server does not read: their `s_feature_incompat` bits.
    Incompatible {
        /// The bits.
        features: u32,
    },
    /// A writable mount of a filesystem a write here would leave wrong. It can be served read-only.
    NotWritable {
        /// The `ro_compat` bits.
        ro_compat: u32,
        /// The `incompat` bits missing.
        lacking: u32,
    },
}

impl Unservable {
    /// The [`FsError`] a request fails with for the same reason.
    pub fn fs_error(self) -> FsError {
        match self {
            Unservable::Unreadable | Unservable::RootUnreadable | Unservable::StateUnwritable => FsError::Io,
            Unservable::NoMagic { .. }
            | Unservable::ZeroField(_)
            | Unservable::Geometry(_)
            | Unservable::RootNotDirectory { .. } => FsError::Corrupt,
            Unservable::Wide64Bit
            | Unservable::BlockTooLarge { .. }
            | Unservable::NeedsRecovery
            | Unservable::Incompatible { .. } => FsError::Unsupported,
            Unservable::NotWritable { .. } => FsError::ReadOnly,
        }
    }
}

impl core::fmt::Display for Unservable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Unservable::Unreadable => write!(f, "the superblock could not be read off the device"),
            Unservable::NoMagic { found } => {
                write!(f, "no ext4 filesystem: superblock magic {found:#06x}, not {SUPER_MAGIC:#06x}")
            }
            Unservable::Wide64Bit => write!(f, "a 64-bit ext4 filesystem, which this server does not read"),
            // The shift is taken on a u64 and saturated, since `log` is whatever the device held.
            Unservable::BlockTooLarge { log } => {
                write!(f, "blocks of 2^{} bytes, over the {MAX_BLOCK} this server reads", 10u32.saturating_add(log))
            }
            Unservable::ZeroField(field) => write!(f, "a malformed superblock: {field} is zero"),
            Unservable::Geometry(field) => write!(f, "a malformed superblock: {field} does not fit its groups"),
            Unservable::RootUnreadable => write!(
                f,
                "the root directory's inode could not be read: the inode table is past the end of \
                 the device, or the device failed"
            ),
            Unservable::RootNotDirectory { mode } => write!(
                f,
                "inode 2 is not a directory (mode {mode:#06o}): the group descriptors or the inode \
                 table are not what the superblock describes"
            ),
            Unservable::StateUnwritable => {
                write!(f, "the superblock could not be written to record the filesystem mounted")
            }
            Unservable::NeedsRecovery => {
                write!(f, "its journal holds changes not yet replayed: mount it on Linux, or run e2fsck, first")
            }
            Unservable::Incompatible { features } => {
                write!(f, "features this server does not read: ")?;
                features::names(f, features, false)
            }
            Unservable::NotWritable { ro_compat, lacking } => {
                write!(f, "it can only be read here: ")?;
                if ro_compat != 0 {
                    write!(f, "this server does not keep its ")?;
                    features::names(f, ro_compat, true)?;
                }
                if lacking != 0 {
                    write!(f, "{}it lacks ", if ro_compat != 0 { "; " } else { "" })?;
                    features::names(f, lacking, false)?;
                    write!(f, ", which this server's writes make")?;
                }
                Ok(())
            }
        }
    }
}
