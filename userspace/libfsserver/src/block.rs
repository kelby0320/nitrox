//! **The device a filesystem reads**, by byte offset, through [`BlockReader`] and [`BlockWriter`],
//! so a library is host-tested over an image and served over [`crate::disk::Disk`].

/// Random-access read of the underlying block device, by byte offset. The implementor maps
/// `read_at` to device reads (the server: `sys_io_submit` over the sectors that cover the range;
/// host tests: a slice of an image).
pub trait BlockReader {
    /// Fill `buf` with the bytes at device byte `offset`. `Err` on any short or failed read.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError>;

    /// **Read metadata**: bytes only this server writes (directories, inode tables, bitmaps,
    /// descriptors, superblocks, extent nodes, a FAT), which a cache
    /// ([`crate::cache::MetaCache`]) may answer. **Never a file's data**, which the kernel writes
    /// behind the server's back: use [`read_at`](Self::read_at). A read left as `read_at` is only
    /// slower; one wrongly made here could be stale. By default, a read.
    fn read_meta(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        self.read_at(offset, buf)
    }

    /// Whether this is a **read-only mount**: `true` only for [`ReadOnly`]. The block-file reply's
    /// mark comes from here, so it and the refusal of every write are one fact.
    fn read_only(&self) -> bool {
        false
    }
}

/// A filesystem operation's failure.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FsError {
    /// A device read failed or returned short.
    Io,
    /// Not a filesystem the library reads, or a structure was malformed.
    Corrupt,
    /// A feature the library does not support (an unknown `incompat` flag, say).
    Unsupported,
    /// A path component was not found, or the path named a non-regular file.
    NotFound,
    /// The file is larger than the caller's buffer, or too fragmented to map in one reply.
    TooLarge,
    /// A create/rename target already exists (POSIX `EEXIST`).
    Exists,
    /// An `rmdir` target directory is not empty (POSIX `ENOTEMPTY`).
    NotEmpty,
    /// A write to a **read-only mount** ([`ReadOnly`]). What the server answers is `NoAccess`.
    ReadOnly,
    /// **A name the filesystem cannot hold** (FAT's forbidden characters): answered
    /// `InvalidArgument`. ext4 answers its own bad names `Unsupported`.
    InvalidName,
}

/// A block-device **writer**, the read-write counterpart of [`BlockReader`].
pub trait BlockWriter {
    /// Write `buf` at absolute byte `offset`.
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError>;

    /// **Write `len` zeroes from `offset`**, over the blocks a grow allocates. A device that can
    /// moves them in large commands ([`crate::disk::Disk`]: 64 KiB); by default, 4 KiB at a time.
    fn write_zeroes(&self, offset: u64, len: u64) -> Result<(), FsError> {
        static ZERO: [u8; 4096] = [0; 4096];
        let mut done = 0;
        while done < len {
            let n = (len - done).min(ZERO.len() as u64) as usize;
            self.write_at(offset + done, &ZERO[..n])?;
            done += n as u64;
        }
        Ok(())
    }

    /// **`len` bytes from `offset` are no longer metadata** — freed, so they may become a file's
    /// data, which the kernel writes behind the server's back: a cache lets them go. By default,
    /// nothing.
    fn forget(&self, _offset: u64, _len: u64) {}
}

/// **A read-only mount**: reads pass through, and every write is refused with
/// [`FsError::ReadOnly`] before it reaches the device. Any mutation fails at its first write
/// having changed nothing, since a mutation only reads before it writes.
pub struct ReadOnly<'a, R: BlockReader>(pub &'a R);

impl<R: BlockReader> BlockReader for ReadOnly<'_, R> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        self.0.read_at(offset, buf)
    }

    fn read_meta(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        self.0.read_meta(offset, buf)
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

/// One contiguous mapping from a file's blocks to the device, for the **Model A** data path
/// (`docs/architecture/filesystem-data-path.md`). `device_lba` counts **filesystem blocks**; the
/// kernel scales it by the block size. Mirrors the wire `BlockRun`
/// (`docs/spec/rsproto-block-ops.md`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct BlockRun {
    /// First block of the run, within the file.
    pub file_block: u64,
    /// First block of the run, on the device; `0` is a hole.
    pub device_lba: u64,
    /// Blocks in the run.
    pub length: u32,
    /// Bit flags; see [`RUN_UNWRITTEN`].
    pub flags: u32,
}

/// [`BlockRun::flags`] bit 0, **`UNWRITTEN`**: the run's blocks read as zero, so the kernel fills a
/// page of them without reading the device, and writes it back to them as to any other. Set on
/// what a grow just added, in that grow's reply only (`docs/spec/rsproto-block-ops.md`).
pub const RUN_UNWRITTEN: u32 = 1 << 0;
