//! **The device a filesystem reads**, as a filesystem library sees it: by byte offset, through
//! [`BlockReader`] and [`BlockWriter`], so a library is host-tested over an image in memory or in a
//! file, and served over [`crate::disk::Disk`].

/// Random-access read of the underlying block device, by byte offset. The reader
/// translates filesystem structures (ext4's superblock at byte 1024, its blocks at
/// `block_no * block_size`, …) into `read_at` calls; the implementor maps them to
/// device reads (the fs-server: `sys_io_submit` over the sectors that cover the range; host
/// tests: a slice of an image).
pub trait BlockReader {
    /// Fill `buf` with the bytes at device byte `offset`. `Err` on any short or
    /// failed read.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError>;

    /// Whether this is a **read-only mount** — `true` only for [`ReadOnly`]. What the block-file
    /// reply marks a file with comes from here, so the mark and the refusal of every write are
    /// one fact, the type the server serves through.
    fn read_only(&self) -> bool {
        false
    }
}

/// A filesystem operation's failure.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FsError {
    /// A device read failed or returned short.
    Io,
    /// Not a filesystem the library reads (a bad magic number), or a structure was
    /// malformed (a bad extent magic, a truncated directory, …).
    Corrupt,
    /// A feature the library does not support (an unknown `incompat` flag, a non-extent
    /// inode, a 64-bit ext4, …).
    Unsupported,
    /// A path component was not found, or the path named a non-regular file.
    NotFound,
    /// The file is larger than the caller's buffer, or too fragmented to map in one reply.
    TooLarge,
    /// A create/rename target already exists (POSIX `EEXIST`).
    Exists,
    /// An `rmdir` target directory is not empty (POSIX `ENOTEMPTY`).
    NotEmpty,
    /// A write to a **read-only mount** ([`ReadOnly`], administration Part C.3). What the
    /// server answers is `NoAccess`.
    ReadOnly,
}

/// A block-device **writer** — the read-write counterpart of [`BlockReader`], for the
/// metadata mutation a write path needs (ext4's bitmaps, extent tree, inodes and superblock).
/// `write_at` writes `buf` at absolute byte `offset`. Read-only builds never require this; the
/// read-write server implements it over `sys_io_submit` writes.
pub trait BlockWriter {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError>;
}

/// **A read-only mount** (administration Part C.3): reads pass through to the device, and every
/// write is refused with [`FsError::ReadOnly`] before it reaches it.
///
/// The server serves a read-only mount through this type, so read-only is not a check each
/// mutating operation has to remember. Any mutation, reached any way, fails at its first write
/// having changed nothing: a mutation only reads before it writes, and none of its writes
/// happen. The host tests hold every mutating operation to that against this same type.
pub struct ReadOnly<'a, R: BlockReader>(pub &'a R);

impl<R: BlockReader> BlockReader for ReadOnly<'_, R> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        self.0.read_at(offset, buf)
    }

    fn read_only(&self) -> bool {
        true
    }
}

impl<R: BlockReader> BlockWriter for ReadOnly<'_, R> {
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }
}

/// One contiguous mapping from a file's blocks to the device, for the **Model A** data
/// path (`docs/architecture/filesystem-data-path.md`). `device_lba` is a **filesystem
/// block** number (`0` = a hole → reads as zero); the kernel scales it to a byte offset by
/// the filesystem block size. Mirrors the wire `BlockRun` (`docs/spec/rsproto-block-ops.md`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct BlockRun {
    pub file_block: u64,
    pub device_lba: u64,
    pub length: u32,
    pub flags: u32,
}
