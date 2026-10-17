//! The ext4 parser and write path. See the crate docs for scope.
//!
//! Little-endian: the superblock at byte 1024, group descriptors after it, inodes via the
//! descriptor's inode-table block, data via the inode's **extent tree**, and directories as a
//! linear list of `ext4_dir_entry_2`.

use crate::extent::{self, Extent, Lookup};
use crate::{BlockReader, BlockWriter, FsError, rd_u16, rd_u32};

/// Cap on a served file's size: 64 KiB.
pub const MAX_FILE: usize = 64 * 1024;

/// Largest filesystem block the reader supports (its block scratch buffer).
pub(crate) const MAX_BLOCK: usize = 4096;

/// Zeroes for what a grow adds ([`grow_file`]); static, since a server thread's stack is small.
static ZERO_BLOCK: [u8; MAX_BLOCK] = [0; MAX_BLOCK];

const SUPER_MAGIC: u16 = 0xEF53;

/// Byte offset of `s_state`: the superblock at 1024, `s_state` at 58 within it.
const S_STATE_AT: u64 = 1024 + 58;
/// Where the superblock keeps `s_volume_name`: 16 bytes, NUL-padded.
const S_VOLUME_NAME_AT: u64 = 1024 + 120;
/// `s_state`'s "cleanly unmounted" bit (`EXT4_VALID_FS`).
const STATE_CLEAN: u16 = 0x0001;
const ROOT_INO: u32 = 2;
const INCOMPAT_64BIT: u32 = 0x80;
const EXTENTS_FL: u32 = 0x0008_0000;
const INLINE_DATA_FL: u32 = 0x1000_0000;
const S_IFMT: u16 = 0xF000;
const S_IFREG: u16 = 0x8000;
/// `ext4_dir_entry_2.file_type` for a regular file.
const EXT4_FT_REG_FILE: u8 = 1;
/// A regular file's default mode: `S_IFREG | 0o644`.
const REG_FILE_MODE: u16 = S_IFREG | 0o644;
const S_IFDIR: u16 = 0x4000;

// `ext4_inode` byte offsets. The `*_extra` words exist only in larger inodes and carry the
// post-2038 epoch bits. `i_ctime_extra` (132) and `i_mtime_extra` (136) are adjacent, so
// reading one for the other is invisible before 2038.

const I_ATIME: usize = 8;
const I_CTIME: usize = 12;
const I_MTIME: usize = 16;
const I_CTIME_EXTRA: usize = 132;
const I_MTIME_EXTRA: usize = 136;
const I_ATIME_EXTRA: usize = 140;
/// `i_crtime` — creation time (large inodes only).
const I_CRTIME: usize = 144;
const I_CRTIME_EXTRA: usize = 148;
/// An inode larger than this base size carries the `*_extra` words.
const INODE_BASE_SIZE: u32 = 128;

/// Decode an ext4 timestamp: `secs` plus the two epoch-extension bits in the low bits of its
/// `extra` word (`0` when the inode has no extra fields).
fn decode_time(secs: u32, extra: u32) -> i64 {
    secs as i64 | ((extra & 0x3) as i64) << 32
}

/// Write `now` (Unix epoch seconds) into an inode's timestamp fields, with the epoch-extension
/// bits. `which` selects which are stamped; `atime` is set at creation and never updated (the
/// kernel owns the data path, so there is no read here to hook).
fn stamp(inode: &mut [u8], now: i64, inode_size: u32, which: Stamp) {
    let secs = now as u32;
    let extra = ((now >> 32) & 0x3) as u32;
    let large = inode_size > INODE_BASE_SIZE && inode.len() > I_CRTIME_EXTRA;
    let mut put = |off: usize, off_extra: usize| {
        inode[off..off + 4].copy_from_slice(&secs.to_le_bytes());
        if large {
            inode[off_extra..off_extra + 4].copy_from_slice(&extra.to_le_bytes());
        }
    };
    put(I_CTIME, I_CTIME_EXTRA);
    if matches!(which, Stamp::Created | Stamp::Modified) {
        put(I_MTIME, I_MTIME_EXTRA);
    }
    if matches!(which, Stamp::Created) {
        put(I_ATIME, I_ATIME_EXTRA);
        if large {
            put(I_CRTIME, I_CRTIME_EXTRA);
        }
    }
}

/// Which timestamps [`stamp`] writes.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Stamp {
    /// A new inode: crtime + atime + mtime + ctime.
    Created,
    /// Contents changed: mtime + ctime.
    Modified,
    /// Only metadata changed (a link count, say): ctime.
    MetadataOnly,
}

/// The parsed superblock facts the reader (and the write path) need.
pub(crate) struct Superblock {
    pub(crate) block_size: u32,
    inodes_per_group: u32,
    inode_size: u32,
    desc_size: u32,
    first_gdt_block: u64,
    blocks_per_group: u32,
    /// The first data block (`1` for 1 KiB blocks, else `0`) — block numbering origin.
    pub(crate) first_data_block: u32,
    /// Blocks in the filesystem (`s_blocks_count_lo`; the `_hi` half needs `64bit`, which
    /// [`parse_superblock`] refuses). **The ceiling the allocator must not cross.**
    pub(crate) blocks_count: u64,
    inodes_count: u32,
}

impl Superblock {
    fn group_count(&self) -> u32 {
        let data = self.blocks_count.saturating_sub(self.first_data_block as u64);
        data.div_ceil(self.blocks_per_group as u64).max(1) as u32
    }

    /// How much of an inode this server reads and writes: its size, up to 256 bytes.
    pub(crate) fn inode_len(&self) -> usize {
        (self.inode_size as usize).min(256)
    }

    fn group_start(&self, group: u32) -> u64 {
        self.first_data_block as u64 + group as u64 * self.blocks_per_group as u64
    }

    /// How many blocks `group` has: `blocks_per_group` except for a final group cut short. **Not
    /// the bitmap's width**: a bit set past the end of the filesystem is a block number no reader
    /// can fetch, written into an extent that `e2fsck` calls corrupt.
    fn blocks_in_group(&self, group: u32) -> usize {
        let start = self.group_start(group);
        self.blocks_count.saturating_sub(start).min(self.blocks_per_group as u64) as usize
    }

    fn inodes_in_group(&self, group: u32) -> usize {
        let first = group as u64 * self.inodes_per_group as u64;
        (self.inodes_count as u64).saturating_sub(first).min(self.inodes_per_group as u64) as usize
    }

    /// Byte offset of `group`'s descriptor; the table is contiguous, so this spans blocks.
    fn group_desc_offset(&self, group: u32) -> u64 {
        self.first_gdt_block * self.block_size as u64 + group as u64 * self.desc_size as u64
    }
}

/// Why a device cannot be served, named specifically enough for a supervisor to print.
///
/// The same checks every request makes: [`read_superblock`] is [`parse_superblock`] with the
/// reason collapsed, so [`check_device`] cannot drift from the check each lookup runs.
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
}

impl Unservable {
    /// The [`FsError`] a request fails with for the same reason.
    pub fn fs_error(self) -> FsError {
        match self {
            Unservable::Unreadable | Unservable::RootUnreadable | Unservable::StateUnwritable => FsError::Io,
            Unservable::NoMagic { .. } | Unservable::ZeroField(_) | Unservable::RootNotDirectory { .. } => {
                FsError::Corrupt
            }
            Unservable::Wide64Bit | Unservable::BlockTooLarge { .. } => FsError::Unsupported,
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
        }
    }
}

/// Parse the 1024 bytes at device byte 1024 into the facts the reader needs.
fn parse_superblock(sb: &[u8; 1024]) -> Result<Superblock, Unservable> {
    let magic = rd_u16(sb, 56);
    if magic != SUPER_MAGIC {
        return Err(Unservable::NoMagic { found: magic });
    }
    if rd_u32(sb, 96) & INCOMPAT_64BIT != 0 {
        return Err(Unservable::Wide64Bit); // 64-bit changes the descriptor layout
    }
    let log = rd_u32(sb, 24);
    if log > 2 {
        return Err(Unservable::BlockTooLarge { log }); // > 4 KiB blocks exceed the scratch buffer
    }
    let block_size = 1024u32 << log;
    let inode_size = rd_u16(sb, 88) as u32;
    if inode_size == 0 {
        return Err(Unservable::ZeroField("s_inode_size"));
    }
    let inodes_per_group = rd_u32(sb, 40);
    if inodes_per_group == 0 {
        return Err(Unservable::ZeroField("s_inodes_per_group"));
    }
    let desc_size = match rd_u16(sb, 254) as u32 {
        0 => 32,
        d => d,
    };
    let blocks_per_group = rd_u32(sb, 32);
    let first_data_block = rd_u32(sb, 20);
    if blocks_per_group == 0 {
        return Err(Unservable::ZeroField("s_blocks_per_group"));
    }
    Ok(Superblock {
        block_size,
        inodes_per_group,
        inode_size,
        desc_size,
        first_gdt_block: if block_size == 1024 { 2 } else { 1 },
        blocks_per_group,
        first_data_block,
        blocks_count: rd_u32(sb, 4) as u64,
        inodes_count: rd_u32(sb, 0),
    })
}

/// The filesystem's block size, for a caller that places file data itself (`nxinstall`).
pub fn block_size<R: BlockReader>(r: &R) -> Result<u32, FsError> {
    Ok(read_superblock(r)?.block_size)
}

pub(crate) fn read_superblock<R: BlockReader>(r: &R) -> Result<Superblock, FsError> {
    let mut sb = [0u8; 1024];
    r.read_meta(1024, &mut sb)?;
    parse_superblock(&sb).map_err(Unservable::fs_error)
}

/// Check that `r` holds a filesystem this reader can serve, before the server says it can: the
/// superblock, then the root directory (inode 2, a directory). Nothing deeper is read; a
/// filesystem wrong there fails the request that reaches it.
pub fn check_device<R: BlockReader>(r: &R) -> Result<(), Unservable> {
    let mut raw = [0u8; 1024];
    r.read_at(1024, &mut raw).map_err(|_| Unservable::Unreadable)?;
    let sb = parse_superblock(&raw)?;
    let root = read_inode(r, &sb, ROOT_INO).map_err(|_| Unservable::RootUnreadable)?;
    let mode = rd_u16(&root, 0);
    if mode & S_IFMT != S_IFDIR {
        return Err(Unservable::RootNotDirectory { mode });
    }
    Ok(())
}

/// Read inode `ino` into a fixed 256-byte buffer (inodes are ≤ 256 bytes here).
pub(crate) fn read_inode<R: BlockReader>(r: &R, sb: &Superblock, ino: u32) -> Result<[u8; 256], FsError> {
    if ino == 0 {
        return Err(FsError::Corrupt);
    }
    let group = (ino - 1) / sb.inodes_per_group;
    let index = (ino - 1) % sb.inodes_per_group;
    let gd_off = sb.first_gdt_block * sb.block_size as u64 + group as u64 * sb.desc_size as u64;
    let mut gd = [0u8; 32];
    r.read_meta(gd_off, &mut gd)?;
    let inode_table = rd_u32(&gd, 8) as u64;
    let off = inode_table * sb.block_size as u64 + index as u64 * sb.inode_size as u64;
    let mut inode = [0u8; 256];
    let n = (sb.inode_size as usize).min(256);
    r.read_meta(off, &mut inode[..n])?;
    Ok(inode)
}

/// Read inode `ino` and decode the metadata a directory listing reports. `i_size` adds `i_size_hi`
/// (offset 108) in a large inode, except for a directory, where that half is `i_dir_acl`.
fn stat_inode<R: BlockReader>(r: &R, sb: &Superblock, ino: u32) -> Result<InodeStat, FsError> {
    let inode = read_inode(r, sb, ino)?;
    let mode = rd_u16(&inode, 0);
    let large = sb.inode_size > 128;
    let size_hi = if large && mode & S_IFMT != S_IFDIR { rd_u32(&inode, 108) as u64 } else { 0 };
    let size = (rd_u32(&inode, 4) as u64) | (size_hi << 32);
    let extra = if large { rd_u32(&inode, I_MTIME_EXTRA) } else { 0 };
    let mtime = decode_time(rd_u32(&inode, I_MTIME), extra);
    Ok(InodeStat { ino, mode, size, mtime })
}

fn dir_lookup<R: BlockReader>(r: &R, sb: &Superblock, dir: &[u8; 256], name: &[u8]) -> Result<u32, FsError> {
    let size = rd_u32(dir, 4) as u64;
    let bs = sb.block_size as usize;
    let nblocks = size.div_ceil(sb.block_size as u64);
    let mut buf = [0u8; MAX_BLOCK];
    let mut map = Lookup::new(dir);
    for lb in 0..nblocks {
        let phys = map.block(r, sb, lb)?;
        if phys == 0 {
            continue;
        }
        r.read_meta(phys * sb.block_size as u64, &mut buf[..bs])?;
        let mut off = 0;
        while off + 8 <= bs {
            let e_ino = rd_u32(&buf, off);
            let rec_len = rd_u16(&buf, off + 4) as usize;
            let name_len = buf[off + 6] as usize;
            if rec_len < 8 || off + rec_len > bs {
                break; // malformed / end of block
            }
            if e_ino != 0 && name_len > 0 && off + 8 + name_len <= bs && &buf[off + 8..off + 8 + name_len] == name {
                return Ok(e_ino);
            }
            off += rec_len;
        }
    }
    Err(FsError::NotFound)
}

/// Resolve `path` (absolute) to a **regular extent file**: its inode number, inode bytes and exact
/// size. `NotFound` for a missing path or a non-regular file, `Unsupported` for a non-extent or
/// inline-data inode.
fn resolve_regular_file<R: BlockReader>(
    r: &R,
    sb: &Superblock,
    path: &[u8],
) -> Result<(u32, [u8; 256], usize), FsError> {
    let (ino, inode) = resolve_path_ino(r, sb, path)?;
    if rd_u16(&inode, 0) & S_IFMT != S_IFREG {
        return Err(FsError::NotFound);
    }
    let flags = rd_u32(&inode, 32);
    if flags & EXTENTS_FL == 0 || flags & INLINE_DATA_FL != 0 {
        return Err(FsError::Unsupported);
    }
    let size_hi = if sb.inode_size > 128 { rd_u32(&inode, 108) as u64 } else { 0 };
    let size = ((rd_u32(&inode, 4) as u64) | (size_hi << 32)) as usize;
    Ok((ino, inode, size))
}

/// Resolve `path` to a regular file and return its **size** without reading content (no
/// [`MAX_FILE`] cap), for the kernel's lazy resolve.
pub fn stat_file<R: BlockReader>(r: &R, path: &[u8]) -> Result<usize, FsError> {
    let sb = read_superblock(r)?;
    let (_, _, size) = resolve_regular_file(r, &sb, path)?;
    Ok(size)
}

/// `ext4_dir_entry_2.file_type` values (the ones we surface); others map to [`EXT4_FT_UNKNOWN`].
pub const EXT4_FT_UNKNOWN: u8 = 0;
/// A directory.
pub const EXT4_FT_DIR: u8 = 2;
/// A symbolic link.
pub const EXT4_FT_SYMLINK: u8 = 7;

/// Resolve `path` (absolute) to a **directory** inode number. `NotFound` if the path is missing or
/// not a directory; the caller enumerates it by inode via [`read_dir`] and addresses entries by
/// name, so the handle cannot reach outside it.
pub fn resolve_dir<R: BlockReader>(r: &R, path: &[u8]) -> Result<u32, FsError> {
    let sb = read_superblock(r)?;
    let (ino, inode) = resolve_path_ino(r, &sb, path)?;
    if rd_u16(&inode, 0) & S_IFMT != S_IFDIR {
        return Err(FsError::NotFound);
    }
    Ok(ino)
}

/// One directory entry's inode metadata, resolved by [`read_dir_stat`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct InodeStat {
    /// The inode number (also passed to `emit` as the entry's inode).
    pub ino: u32,
    /// Raw `i_mode` — the format bits (`S_IFMT`) plus permissions.
    pub mode: u16,
    /// `i_size` (both halves when the inode carries the high one).
    pub size: u64,
    /// Modification time, seconds since the Unix epoch.
    pub mtime: i64,
}

/// Enumerate directory inode `dir_ino` from the opaque `cursor` (a byte offset into its data; `0`
/// starts), calling `emit(inode, file_type, name)` for each live entry. `emit` returns `false` to
/// stop early (reply buffer full); enumeration resumes from the returned cursor, `0` once done.
/// `.` and `..` are included. A block's slack rides in the last entry's `rec_len`, so iteration
/// steps by `rec_len` and rounds up to the next block. [`read_dir_stat`] also reads each inode.
///
/// # Errors
/// `NotFound` if `dir_ino` is not a directory, plus `Io`/`Corrupt` from the device.
pub fn read_dir<R: BlockReader>(
    r: &R,
    dir_ino: u32,
    cursor: u64,
    mut emit: impl FnMut(u32, u8, &[u8]) -> bool,
) -> Result<u64, FsError> {
    read_dir_inner(r, dir_ino, cursor, |ino, ft, name, _| emit(ino, ft, name), false)
}

/// [`read_dir`], plus each entry's [`InodeStat`] — the listing form. Costs one inode read per
/// entry.
///
/// # Errors
/// As [`read_dir`]. An entry whose inode cannot be read is emitted with a **default** (zeroed)
/// `InodeStat`: one damaged inode must not make a directory unlistable.
pub fn read_dir_stat<R: BlockReader>(
    r: &R,
    dir_ino: u32,
    cursor: u64,
    mut emit: impl FnMut(u32, u8, &[u8], &InodeStat) -> bool,
) -> Result<u64, FsError> {
    read_dir_inner(r, dir_ino, cursor, |ino, ft, name, st| emit(ino, ft, name, st), true)
}

fn read_dir_inner<R: BlockReader>(
    r: &R,
    dir_ino: u32,
    cursor: u64,
    mut emit: impl FnMut(u32, u8, &[u8], &InodeStat) -> bool,
    want_stat: bool,
) -> Result<u64, FsError> {
    let sb = read_superblock(r)?;
    let inode = read_inode(r, &sb, dir_ino)?;
    if !live_dir(&inode) {
        return Err(FsError::NotFound);
    }
    let size = rd_u32(&inode, 4) as u64;
    let bs = sb.block_size as u64;
    let mut pos = cursor;
    let mut buf = [0u8; MAX_BLOCK];
    let mut loaded_block = u64::MAX; // which logical block `buf` currently holds
    let mut map = Lookup::new(&inode);

    while pos < size {
        let lb = pos / bs;
        let in_block = (pos % bs) as usize;
        if loaded_block != lb {
            let phys = map.block(r, &sb, lb)?;
            if phys == 0 {
                pos = (lb + 1) * bs;
                continue;
            }
            r.read_meta(phys * bs, &mut buf[..bs as usize])?;
            loaded_block = lb;
        }
        if in_block + 8 > bs as usize {
            pos = (lb + 1) * bs;
            continue;
        }
        let e_ino = rd_u32(&buf, in_block);
        let rec_len = rd_u16(&buf, in_block + 4) as usize;
        let name_len = buf[in_block + 6] as usize;
        let file_type = buf[in_block + 7];
        if rec_len < 8 || in_block + rec_len > bs as usize {
            pos = (lb + 1) * bs;
            continue;
        }
        // A live entry has a non-zero inode and a name that fits; `e_ino == 0` is a gap.
        if e_ino != 0 && name_len > 0 && in_block + 8 + name_len <= bs as usize {
            let name = &buf[in_block + 8..in_block + 8 + name_len];
            // A single bad inode degrades to a zeroed stat. This reads into its own buffers, so the
            // cached directory block in `buf` survives.
            let stat = if want_stat { stat_inode(r, &sb, e_ino).unwrap_or_default() } else { InodeStat::default() };
            if !emit(e_ino, file_type, name, &stat) {
                return Ok(pos);
            }
        }
        pos += rec_len as u64;
    }
    Ok(0)
}

/// Read the byte range `[offset, offset + len)` of the regular file at `path` into `out`,
/// returning the bytes read. The range is clamped to the file size and `out.len()`; past
/// end-of-file it returns `0`; holes read as zero. No [`MAX_FILE`] cap.
pub fn read_file_range<R: BlockReader>(
    r: &R,
    path: &[u8],
    offset: u64,
    len: usize,
    out: &mut [u8],
) -> Result<usize, FsError> {
    let sb = read_superblock(r)?;
    let (_, inode, size) = resolve_regular_file(r, &sb, path)?;
    if offset >= size as u64 {
        return Ok(0);
    }
    let avail = (size as u64 - offset) as usize;
    let want = len.min(avail).min(out.len());
    let bs = sb.block_size as usize;
    let mut buf = [0u8; MAX_BLOCK];
    let mut map = Lookup::new(&inode);
    let mut done = 0;
    while done < want {
        let pos = offset as usize + done; // absolute file byte position
        let lb = (pos / bs) as u64; // logical block
        let in_block = pos % bs; // byte offset within that block
        let n = (bs - in_block).min(want - done);
        let phys = map.block(r, &sb, lb)?;
        if phys == 0 {
            out[done..done + n].fill(0); // sparse hole
        } else {
            r.read_at(phys * sb.block_size as u64, &mut buf[..bs])?;
            out[done..done + n].copy_from_slice(&buf[in_block..in_block + n]);
        }
        done += n;
    }
    Ok(want)
}

/// Map the logical block range `[start_block, start_block + count)` of the regular file at `path`
/// to device block runs (the **Model A** data path) in `out`, returning the run count. Runs
/// coalesce contiguous mappings and holes (`device_lba = 0`), an unwritten extent among them. A
/// short return means re-request from the first uncovered block.
pub fn map_range<R: BlockReader>(
    r: &R,
    path: &[u8],
    start_block: u64,
    count: u64,
    out: &mut [crate::BlockRun],
) -> Result<usize, FsError> {
    let sb = read_superblock(r)?;
    let (_, inode, size) = resolve_regular_file(r, &sb, path)?;
    let bs = sb.block_size as u64;
    let file_blocks = size.div_ceil(bs as usize) as u64;
    let end = start_block.saturating_add(count).min(file_blocks);
    Ok(runs_of(r, &sb, &inode, start_block, end, out)?.0)
}

/// Map logical blocks `start..end` of the file `inode` describes into `out` as device runs, a
/// segment of the extent tree at a time: contiguous blocks coalesce, and so do holes. Returns the
/// runs written and the first block left uncovered.
fn runs_of<R: BlockReader>(
    r: &R,
    sb: &Superblock,
    inode: &[u8; 256],
    start: u64,
    end: u64,
    out: &mut [crate::BlockRun],
) -> Result<(usize, u64), FsError> {
    let mut map = Lookup::new(inode);
    let mut n = 0;
    let mut lb = start;
    while lb < end && n < out.len() {
        let seg = map.seg(r, sb, lb)?;
        let mut len = seg.len.min(end - lb);
        while lb + len < end && len < u32::MAX as u64 {
            let next = map.seg(r, sb, lb + len)?;
            let continues = if seg.phys == 0 { next.phys == 0 } else { next.phys == seg.phys + len };
            if !continues {
                break;
            }
            len += next.len.min(end - lb - len);
        }
        let len = len.min(u32::MAX as u64);
        out[n] = crate::BlockRun { file_block: lb, device_lba: seg.phys, length: len as u32, flags: 0 };
        n += 1;
        lb += len;
    }
    Ok((n, lb))
}

/// What [`map_file`] found: the file, and how many of its runs it wrote.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MappedFile {
    /// The file's size in bytes.
    pub size: usize,
    /// The filesystem's block size.
    pub block_size: u32,
    /// How many `BlockRun`s were written to the caller's buffer.
    pub runs: usize,
    /// **The file's inode number — its id to the kernel**, which keeps one page-cache object per
    /// id. Never `0`: ext4 numbers inodes from 1.
    pub ino: u32,
}

/// Resolve `path` to a regular file and map its **entire** block range to device runs (the
/// **Model A** resolve). `Err(TooLarge)` if the file needs more runs than `out` holds.
pub fn map_file<R: BlockReader>(r: &R, path: &[u8], out: &mut [crate::BlockRun]) -> Result<MappedFile, FsError> {
    let sb = read_superblock(r)?;
    let (ino, inode, size) = resolve_regular_file(r, &sb, path)?;
    let bs = sb.block_size;
    let file_blocks = size.div_ceil(bs as usize) as u64;
    let (n, reached) = runs_of(r, &sb, &inode, 0, file_blocks, out)?;
    if reached < file_blocks {
        return Err(FsError::TooLarge); // too fragmented to inline in the resolve reply
    }
    // TODO(ext4-subpage-runs): with blocks under a page, a run boundary inside a page is not
    // refused, and the kernel would fill that page from one device range regardless.
    Ok(MappedFile { size, block_size: bs, runs: n, ino })
}

fn bit_clear(map: &[u8], i: usize) -> bool {
    map[i / 8] & (1 << (i % 8)) == 0
}
fn bit_set(map: &mut [u8], i: usize) {
    map[i / 8] |= 1 << (i % 8);
}

/// Whether block `blk`'s bit is set in its group's bitmap: what a test asks of each mapped block.
#[cfg(test)]
pub(crate) fn block_in_use<R: BlockReader>(r: &R, sb: &Superblock, blk: u64) -> bool {
    let rel = blk - sb.first_data_block as u64;
    let (group, index) = ((rel / sb.blocks_per_group as u64) as u32, (rel % sb.blocks_per_group as u64) as usize);
    let mut gd = [0u8; 64];
    r.read_at(sb.group_desc_offset(group), &mut gd[..(sb.desc_size as usize).min(64)]).unwrap();
    let mut byte = [0u8; 1];
    r.read_at(rd_u32(&gd, 0) as u64 * sb.block_size as u64 + index as u64 / 8, &mut byte).unwrap();
    !bit_clear(&byte, index % 8)
}

/// Resolve an absolute path to `(inode_number, inode_bytes)`, walking from the root inode.
pub(crate) fn resolve_path_ino<R: BlockReader>(
    r: &R,
    sb: &Superblock,
    path: &[u8],
) -> Result<(u32, [u8; 256]), FsError> {
    let mut ino = ROOT_INO;
    let mut inode = read_inode(r, sb, ino)?;
    for comp in path.split(|&c| c == b'/').filter(|c| !c.is_empty()) {
        if rd_u16(&inode, 0) & S_IFMT != S_IFDIR {
            return Err(FsError::NotFound);
        }
        ino = dir_lookup(r, sb, &inode, comp)?;
        inode = read_inode(r, sb, ino)?;
    }
    Ok((ino, inode))
}

/// **Whether the filesystem was left clean**: `s_state`'s "cleanly unmounted" bit. A writable
/// mount clears it ([`mark_mounted`]) and an unmount sets it again as its last write
/// ([`mark_clean`]). `Err` if the device holds no ext4 filesystem.
pub fn was_left_clean<R: BlockReader>(r: &R) -> Result<bool, FsError> {
    read_superblock(r)?;
    let mut s = [0u8; 2];
    r.read_at(S_STATE_AT, &mut s)?;
    Ok(u16::from_le_bytes(s) & STATE_CLEAN != 0)
}

/// **The filesystem's own label**, `s_volume_name`, written into `out`: returns how many bytes are
/// meaningful (sixteen, NUL-padded; no terminator when all are used). `Err` if not ext4.
pub fn volume_label<R: BlockReader>(r: &R, out: &mut [u8; 16]) -> Result<usize, FsError> {
    read_superblock(r)?;
    r.read_at(S_VOLUME_NAME_AT, out)?;
    Ok(out.iter().position(|&b| b == 0).unwrap_or(out.len()))
}

/// Record that the filesystem is **mounted writable**: clear the "cleanly unmounted" bit, leaving
/// the rest of `s_state`. Done before `Meta::Ready`, so it never looks clean while it can change.
pub fn mark_mounted<RW: BlockReader + BlockWriter>(rw: &RW) -> Result<(), FsError> {
    set_state(rw, |s| s & !STATE_CLEAN)
}

/// Record that the filesystem was **cleanly unmounted**: set the bit, as the last write.
pub fn mark_clean<RW: BlockReader + BlockWriter>(rw: &RW) -> Result<(), FsError> {
    set_state(rw, |s| s | STATE_CLEAN)
}

fn set_state<RW: BlockReader + BlockWriter>(rw: &RW, f: impl Fn(u16) -> u16) -> Result<(), FsError> {
    read_superblock(rw)?;
    let mut s = [0u8; 2];
    rw.read_meta(S_STATE_AT, &mut s)?;
    rw.write_at(S_STATE_AT, &f(u16::from_le_bytes(s)).to_le_bytes())
}

/// Stamp the modification time of `name` inside `dir_ino`, the **session-scoped** counterpart of
/// [`touch_file`]: a handle addresses entries **by name** inside its directory, so confinement is
/// structural. `now` comes from the server, never the caller (forgeable metadata otherwise).
pub fn touch_at<RW: BlockReader + BlockWriter>(rw: &RW, dir_ino: u32, name: &[u8], now: i64) -> Result<(), FsError> {
    let sb = read_superblock(rw)?;
    let parent = read_inode(rw, &sb, dir_ino)?;
    if !live_dir(&parent) {
        return Err(FsError::NotFound);
    }
    let target_ino = dir_lookup(rw, &sb, &parent, name)?;
    touch_inode(rw, &sb, target_ino, now, Stamp::Modified)
}

/// Stamp inode `ino`'s modification time as `now` — the `File::Touch` entry point, naming the
/// file by the id [`map_file`] gave the kernel.
///
/// The one mutation that changes **no** content and no structure. Model A puts the kernel on the
/// data path, so a same-length overwrite reaches the device without a resolve and `mtime` would
/// not move; the kernel sends this after flushing such a write. By inode, not path: it flushes
/// long after the resolve that named the file, when a rename may have given its name to another.
/// **Only a live regular file is stamped** (`NotFound` otherwise), since the id comes off the
/// wire and the file may have been unlinked. `now` is the server's own clock reading.
pub fn touch_file<RW: BlockReader + BlockWriter>(rw: &RW, ino: u32, now: i64) -> Result<(), FsError> {
    let sb = read_superblock(rw)?;
    if ino == 0 || ino > sb.inodes_count {
        return Err(FsError::NotFound);
    }
    let inode = read_inode(rw, &sb, ino)?;
    if rd_u16(&inode, 0) & S_IFMT != S_IFREG || rd_u16(&inode, 26) == 0 {
        return Err(FsError::NotFound);
    }
    touch_inode(rw, &sb, ino, now, Stamp::Modified)
}

/// Re-stamp an existing inode in place, as for the *containing directory* after a link, unlink,
/// or rename: its contents changed, so its mtime and ctime move though no entry's inode did.
fn touch_inode<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    ino: u32,
    now: i64,
    which: Stamp,
) -> Result<(), FsError> {
    let mut inode = read_inode(rw, sb, ino)?;
    stamp(&mut inode, now, sb.inode_size, which);
    let off = inode_offset(rw, sb, ino)?;
    rw.write_at(off, &inode[..(sb.inode_size as usize).min(256)])
}

pub(crate) fn inode_offset<R: BlockReader>(r: &R, sb: &Superblock, ino: u32) -> Result<u64, FsError> {
    let group = (ino - 1) / sb.inodes_per_group;
    let index = (ino - 1) % sb.inodes_per_group;
    let gd_off = sb.first_gdt_block * sb.block_size as u64 + group as u64 * sb.desc_size as u64;
    let mut gd = [0u8; 32];
    r.read_meta(gd_off, &mut gd)?;
    let inode_table = rd_u32(&gd, 8) as u64;
    Ok(inode_table * sb.block_size as u64 + index as u64 * sb.inode_size as u64)
}

/// Allocate one free filesystem block, preferring `goal` for contiguity: [`alloc_run`] of one.
pub(crate) fn alloc_block<RW: BlockReader + BlockWriter>(rw: &RW, sb: &Superblock, goal: u64) -> Result<u64, FsError> {
    alloc_run(rw, sb, goal, 1).map(|(phys, _)| phys)
}

/// **Allocate a run of up to `want` free blocks**, contiguous, preferring `goal`: it starts where
/// [`alloc_block`] would put one block and goes on while the next block is free, in that group.
/// Returns `(first block, length)`, the length at least one.
///
/// Tries `goal`'s own group first (the goal bit, then any free bit), then **every other group in
/// turn**, wrapping round (confined to group 0, a 931 GiB root held about 112 MiB). Sets the
/// run's bitmap bits and decrements the group-descriptor and superblock free-block counts by its
/// length: read and written once a run, not once a block. `TooLarge` only when the whole
/// filesystem is full.
///
/// A group whose descriptor says zero free blocks is skipped without reading its bitmap: this
/// writer maintains that count and `mke2fs` sets it. `metadata_csum` is off, so there are no
/// bitmap or descriptor checksums to maintain, and no group is `BLOCK_UNINIT`.
pub(crate) fn alloc_run<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    goal: u64,
    want: u64,
) -> Result<(u64, u64), FsError> {
    let bs = sb.block_size as usize;
    let groups = sb.group_count();
    let first = goal
        .checked_sub(sb.first_data_block as u64)
        .map(|b| (b / sb.blocks_per_group as u64) as u32)
        .filter(|g| *g < groups)
        .unwrap_or(0);

    let mut gd = [0u8; 64];
    let dsz = (sb.desc_size as usize).min(64);
    let mut bitmap = [0u8; MAX_BLOCK];
    for n in 0..groups {
        let group = (first + n) % groups;
        let gd_off = sb.group_desc_offset(group);
        rw.read_meta(gd_off, &mut gd[..dsz])?;
        // bg_free_blocks_count_lo @12. Zero means skip — see the doc comment.
        let gfree = rd_u16(&gd, 12);
        if gfree == 0 {
            continue;
        }
        let capacity = sb.blocks_in_group(group);
        let bitmap_block = rd_u32(&gd, 0) as u64; // bg_block_bitmap_lo
        rw.read_meta(bitmap_block * sb.block_size as u64, &mut bitmap[..bs])?;

        let group_start = sb.group_start(group);
        let goal_idx = goal.checked_sub(group_start).unwrap_or(u64::MAX) as usize;
        let start = if n == 0 && goal_idx < capacity && bit_clear(&bitmap, goal_idx) {
            goal_idx
        } else {
            match (0..capacity).find(|&i| bit_clear(&bitmap, i)) {
                Some(i) => i,
                // The descriptor's count disagreed with its bitmap. Not fatal, and not this
                // function's to repair: move on rather than fail with space elsewhere.
                None => continue,
            }
        };
        // No more than the descriptor says is free, so its count cannot wrap below zero.
        let most = want.max(1).min(gfree as u64) as usize;
        let mut len = 0;
        while len < most && start + len < capacity && bit_clear(&bitmap, start + len) {
            bit_set(&mut bitmap, start + len);
            len += 1;
        }
        rw.write_at(bitmap_block * sb.block_size as u64, &bitmap[..bs])?;

        // Decrement free-block counts: group descriptor (@12, u16) and superblock (@12, u32).
        // TODO(ext4-failed-write-undo): a count write lost here leaves the run's bits set.
        gd[12..14].copy_from_slice(&(gfree - len as u16).to_le_bytes());
        rw.write_at(gd_off, &gd[..dsz])?;
        let mut sbbuf = [0u8; 1024];
        rw.read_meta(1024, &mut sbbuf)?;
        let sfree = rd_u32(&sbbuf, 12).wrapping_sub(len as u32);
        sbbuf[12..16].copy_from_slice(&sfree.to_le_bytes());
        rw.write_at(1024, &sbbuf)?;

        return Ok((group_start + start as u64, len as u64));
    }
    Err(FsError::TooLarge)
}

/// Grow the regular file at `path` to `new_size` bytes by allocating blocks and appending them to
/// its extent tree, a run at a time, updating the inode size and block count. Only **grows** (a
/// `new_size <= cur_size` is a no-op). Returns the new size. See
/// `docs/architecture/ext4-fs-server-rw.md`.
///
/// **A grow that fails for want of space or depth gives back what it took**: the tree is cut
/// where the grow began and the inode written to match. One that loses a write midway does not
/// yet (`TODO(ext4-failed-write-undo)`).
///
/// **Everything the grow adds reads as zero, on the device**: the old last block past the old
/// size, and every block allocated. The kernel fills a page it does not hold from the blocks the
/// map names, so otherwise a reader saw whatever those blocks last held (after a truncate and a
/// grow, the file's own old bytes: the allocator's goal is the block the truncate just freed).
pub fn grow_file<RW: BlockReader + BlockWriter>(
    rw: &RW,
    path: &[u8],
    new_size: usize,
    now: i64,
) -> Result<usize, FsError> {
    grow_file_from(rw, path, new_size, now).map(|(_, size)| size)
}

/// [`grow_file`], answering **the size the file had and the size it has**: everything from the
/// first reads as zero, which the server tells the kernel.
pub fn grow_file_from<RW: BlockReader + BlockWriter>(
    rw: &RW,
    path: &[u8],
    new_size: usize,
    now: i64,
) -> Result<(usize, usize), FsError> {
    let sb = read_superblock(rw)?;
    let bs = sb.block_size as usize;
    let (ino, mut inode) = resolve_path_ino(rw, &sb, path)?;
    if rd_u16(&inode, 0) & S_IFMT != S_IFREG {
        return Err(FsError::NotFound);
    }
    let flags = rd_u32(&inode, 32);
    if flags & EXTENTS_FL == 0 || flags & INLINE_DATA_FL != 0 {
        return Err(FsError::Unsupported);
    }
    let size_hi = if sb.inode_size > 128 { rd_u32(&inode, 108) as u64 } else { 0 };
    let cur_size = ((rd_u32(&inode, 4) as u64) | (size_hi << 32)) as usize;
    if new_size <= cur_size {
        return Ok((cur_size, cur_size));
    }
    let cur_blocks = cur_size.div_ceil(bs) as u64;
    let new_blocks = new_size.div_ceil(bs) as u64;
    let off = inode_offset(rw, &sb, ino)?;
    let isize = (sb.inode_size as usize).min(256);

    let grown = (|| {
        // Blocks past the end (an `fallocate` that kept the size leaves them) would sit under the
        // extents this appends, so they go first.
        extent::truncate(rw, &sb, &mut inode, cur_blocks, Some(off))?;
        // The old last block past the old end: a truncate leaves the bytes it cut there.
        let within = cur_size % bs;
        if within != 0 {
            let phys = Lookup::new(&inode).block(rw, &sb, cur_blocks - 1)?;
            if phys != 0 {
                rw.write_at(phys * bs as u64 + within as u64, &ZERO_BLOCK[..bs - within])?;
            }
        }
        grow_blocks(rw, &sb, &mut inode, cur_blocks, new_blocks)
    })();
    if let Err(e) = grown {
        // The runs already appended go back; the inode is written since a cut can fold its root.
        let _ = extent::truncate(rw, &sb, &mut inode, cur_blocks, Some(off));
        let _ = rw.write_at(off, &inode[..isize]);
        return Err(e);
    }

    // i_size_lo @4, hi @108; the tree counted its blocks in `i_blocks`.
    inode[4..8].copy_from_slice(&(new_size as u32).to_le_bytes());
    if sb.inode_size > 128 {
        inode[108..112].copy_from_slice(&((new_size as u64 >> 32) as u32).to_le_bytes());
    }
    stamp(&mut inode, now, sb.inode_size, Stamp::Modified);
    // TODO(ext4-failed-write-undo): this write lost leaves the tree mapping past the old size.
    rw.write_at(off, &inode[..isize])?;
    Ok((cur_size, new_size))
}

/// Allocate logical blocks `from..to` of a file, zero each on the device, and append them to its
/// tree **a run at a time**: contiguous blocks become one extent, up to the longest ext4 has. On
/// an error the run in hand is given back here; the runs already appended are the caller's, by
/// cutting the tree where they began.
fn grow_blocks<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    inode: &mut [u8; 256],
    from: u64,
    to: u64,
) -> Result<(), FsError> {
    let bs = sb.block_size as usize;
    let give_back = |runs: &[Option<Extent>]| {
        for r in runs.iter().flatten() {
            let _ = free_run(rw, sb, r.phys, r.len);
        }
    };
    let mut goal = extent::last_end(rw, sb, inode)?.unwrap_or(sb.first_data_block as u64);
    let mut run: Option<Extent> = None;
    let mut lb = from;
    while lb < to {
        let (phys, len) = match alloc_run(rw, sb, goal, (to - lb).min(extent::MAX_LEN)) {
            Ok(got) => got,
            Err(e) => {
                give_back(&[run]);
                return Err(e);
            }
        };
        let mut got = Extent::written(lb, phys, len);
        // The run's zeroes in one call, which the device may move in large commands. Not writing
        // them (unwritten extents) is `TODO(grow-zeroes)`.
        if let Err(e) = rw.write_zeroes(phys * bs as u64, len * bs as u64) {
            give_back(&[run, Some(got)]);
            return Err(e);
        }
        // What continues the run in hand joins it, up to an extent's longest; the rest starts the
        // next.
        if let Some(r) = run.as_mut().filter(|r| r.phys + r.len == phys && r.len < extent::MAX_LEN) {
            let joins = got.len.min(extent::MAX_LEN - r.len);
            r.len += joins;
            got = Extent::written(got.lblock + joins, got.phys + joins, got.len - joins);
        }
        if got.len > 0 {
            if let Some(r) = run
                && let Err(e) = extent::append(rw, sb, inode, r)
            {
                give_back(&[run, Some(got)]);
                return Err(e);
            }
            run = Some(got);
        }
        lb += len;
        goal = phys + len;
    }
    if let Some(r) = run
        && let Err(e) = extent::append(rw, sb, inode, r)
    {
        give_back(&[run]);
        return Err(e);
    }
    Ok(())
}

fn round4(n: usize) -> usize {
    (n + 3) & !3
}

/// Allocate one free inode, taking the **first group that has one**: sets the inode-bitmap bit
/// and decrements the group-descriptor and superblock free-inode counts. `TooLarge` only when
/// every group is exhausted. **Locality is not attempted**: real ext4 places a new inode near its
/// parent (the Orlov allocator); this takes the lowest free inode, worse for a spinning disk's
/// seeks, and nothing has measured it yet.
fn alloc_inode<RW: BlockReader + BlockWriter>(rw: &RW, sb: &Superblock) -> Result<u32, FsError> {
    let bs = sb.block_size as usize;
    let mut gd = [0u8; 64];
    let dsz = (sb.desc_size as usize).min(64);
    let mut bitmap = [0u8; MAX_BLOCK];
    for group in 0..sb.group_count() {
        let gd_off = sb.group_desc_offset(group);
        rw.read_meta(gd_off, &mut gd[..dsz])?;
        // bg_free_inodes_count_lo @14. Zero means skip without reading the bitmap.
        if rd_u16(&gd, 14) == 0 {
            continue;
        }
        let capacity = sb.inodes_in_group(group);
        let ibitmap_block = rd_u32(&gd, 4) as u64; // bg_inode_bitmap_lo
        rw.read_meta(ibitmap_block * sb.block_size as u64, &mut bitmap[..bs])?;
        let Some(idx) = (0..capacity).find(|&i| bit_clear(&bitmap, i)) else {
            continue; // the count disagreed with the bitmap; try the next group
        };
        bit_set(&mut bitmap, idx);
        rw.write_at(ibitmap_block * sb.block_size as u64, &bitmap[..bs])?;

        // Free-inode counts: group descriptor @14 (u16) + superblock @16 (u32).
        let gfree = rd_u16(&gd, 14).wrapping_sub(1);
        gd[14..16].copy_from_slice(&gfree.to_le_bytes());
        rw.write_at(gd_off, &gd[..dsz])?;
        let mut sbbuf = [0u8; 1024];
        rw.read_meta(1024, &mut sbbuf)?;
        let sfree = rd_u32(&sbbuf, 16).wrapping_sub(1);
        sbbuf[16..20].copy_from_slice(&sfree.to_le_bytes());
        rw.write_at(1024, &sbbuf)?;

        return Ok(group * sb.inodes_per_group + idx as u32 + 1);
    }
    Err(FsError::TooLarge)
}

/// Insert `name` → `ino` into directory `dir_ino`, **growing the directory by a block when every
/// existing one is full**. `dir_inode` is the caller's copy, updated in place (extent tree, size,
/// block count) and written back **only if the directory grew**.
///
/// An insert splits a record with enough spare `rec_len` or claims a deleted slot; otherwise a new
/// block is appended as one free record spanning it, as a block looks after everything in it is
/// deleted. Each file's data and each `mkdir`'s block is allocated *between* a directory's growth
/// blocks, so every directory block is an extent of its own and the tree must reach any depth.
#[allow(
    clippy::too_many_arguments,
    reason = "the directory's inode, the new entry's name, inode and type are separate inputs of one insert"
)]
fn dir_insert<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    dir_ino: u32,
    dir_inode: &mut [u8; 256],
    name: &[u8],
    ino: u32,
    file_type: u8,
    now: i64,
) -> Result<(), FsError> {
    let bs = sb.block_size as usize;
    let size = rd_u32(dir_inode, 4) as u64;
    let nblocks = size.div_ceil(sb.block_size as u64);
    let need = round4(8 + name.len());
    let mut buf = [0u8; MAX_BLOCK];
    let mut map = Lookup::new(dir_inode);
    for lb in 0..nblocks {
        let phys = map.block(rw, sb, lb)?;
        if phys == 0 {
            continue;
        }
        rw.read_meta(phys * sb.block_size as u64, &mut buf[..bs])?;
        let mut off = 0;
        while off + 8 <= bs {
            let e_ino = rd_u32(&buf, off);
            let rec_len = rd_u16(&buf, off + 4) as usize;
            let e_name_len = buf[off + 6] as usize;
            if rec_len < 8 || off + rec_len > bs {
                break; // malformed / end of block
            }
            let used = if e_ino != 0 { round4(8 + e_name_len) } else { 0 };
            if rec_len - used >= need {
                let new_off = off + used;
                let new_rec = rec_len - used;
                if e_ino != 0 {
                    buf[off + 4..off + 6].copy_from_slice(&(used as u16).to_le_bytes());
                }
                buf[new_off..new_off + 4].copy_from_slice(&ino.to_le_bytes());
                buf[new_off + 4..new_off + 6].copy_from_slice(&(new_rec as u16).to_le_bytes());
                buf[new_off + 6] = name.len() as u8;
                buf[new_off + 7] = file_type;
                buf[new_off + 8..new_off + 8 + name.len()].copy_from_slice(name);
                rw.write_at(phys * sb.block_size as u64, &buf[..bs])?;
                return Ok(());
            }
            off += rec_len;
        }
    }

    let off = inode_offset(rw, sb, dir_ino)?;
    let phys = append_block(rw, sb, dir_inode, nblocks)?;
    // A fresh directory block is a single **unused** record spanning it: inode 0, `rec_len` =
    // block size. Zero the rest so no stale bytes are read as entries.
    buf[..bs].fill(0);
    buf[4..6].copy_from_slice(&(bs as u16).to_le_bytes()); // rec_len covers the block
    buf[..4].copy_from_slice(&ino.to_le_bytes());
    buf[6] = name.len() as u8;
    buf[7] = file_type;
    buf[8..8 + name.len()].copy_from_slice(name);
    if let Err(e) = rw.write_at(phys * sb.block_size as u64, &buf[..bs]) {
        // The block goes back, and the inode is written since a cut can fold its root.
        let _ = extent::truncate(rw, sb, dir_inode, nblocks, Some(off));
        let _ = rw.write_at(off, &dir_inode[..(sb.inode_size as usize).min(256)]);
        return Err(e);
    }

    // `i_size` of a directory is always a whole number of blocks. The tree counted the block in
    // `i_blocks`.
    let new_size = size + bs as u64;
    dir_inode[4..8].copy_from_slice(&(new_size as u32).to_le_bytes());
    stamp(dir_inode, now, sb.inode_size, Stamp::Modified);
    rw.write_at(off, &dir_inode[..(sb.inode_size as usize).min(256)])?;
    Ok(())
}

/// Create an empty regular file `name` in the directory at `parent_path`: allocate and init an
/// inode (regular, empty extent tree, size 0) and link it into the parent. Idempotent — if `name`
/// already exists, returns its inode. The caller grows the file afterwards. `NotFound` if the
/// parent is not a directory. See `docs/architecture/ext4-fs-server-rw.md`.
pub fn create_file<RW: BlockReader + BlockWriter>(
    rw: &RW,
    parent_path: &[u8],
    name: &[u8],
    now: i64,
) -> Result<u32, FsError> {
    if name.is_empty() || name.len() > 255 || name.contains(&b'/') {
        return Err(FsError::Unsupported);
    }
    let sb = read_superblock(rw)?;
    let (_, parent_inode) = resolve_path_ino(rw, &sb, parent_path)?;
    if rd_u16(&parent_inode, 0) & S_IFMT != S_IFDIR {
        return Err(FsError::NotFound);
    }
    if let Ok(existing) = dir_lookup(rw, &sb, &parent_inode, name) {
        return Ok(existing); // already exists — idempotent
    }

    let ino = alloc_inode(rw, &sb)?;
    // Initialise the new inode: regular file, one link, empty depth-0 extent tree, size 0.
    let mut inode = [0u8; 256];
    inode[0..2].copy_from_slice(&REG_FILE_MODE.to_le_bytes()); // i_mode
    stamp(&mut inode, now, sb.inode_size, Stamp::Created);
    inode[26..28].copy_from_slice(&1u16.to_le_bytes()); // i_links_count
    inode[32..36].copy_from_slice(&EXTENTS_FL.to_le_bytes()); // i_flags
    extent::empty_root(&mut inode); // i_block: a leaf of no extents
    let off = inode_offset(rw, &sb, ino)?;
    rw.write_at(off, &inode[..(sb.inode_size as usize).min(256)])?;

    // Link it into the parent directory, growing it by a block if it is full. **A failed link
    // gives the inode back**, rather than leaving it allocated and unattached.
    let linked = resolve_path_ino(rw, &sb, parent_path).and_then(|(parent_ino, mut parent_inode)| {
        dir_insert(rw, &sb, parent_ino, &mut parent_inode, name, ino, EXT4_FT_REG_FILE, now).map(|()| parent_ino)
    });
    let parent_ino = match linked {
        Ok(parent_ino) => parent_ino,
        Err(e) => {
            let _ = free_inode(rw, &sb, ino, false, now);
            return Err(e);
        }
    };
    // The parent's contents changed, so its mtime/ctime move.
    touch_inode(rw, &sb, parent_ino, now, Stamp::Modified)?;
    Ok(ino)
}

/// **Whether an inode is a directory a session may still use**: a directory with a link counted.
/// A removed directory keeps its mode and extent ([`free_inode`] zeroes `i_links_count` and
/// stamps `i_dtime`, nothing more), so a session held open on one would pass a check of the mode
/// alone and write entries into a block since given to another file.
fn live_dir(inode: &[u8; 256]) -> bool {
    rd_u16(inode, 0) & S_IFMT == S_IFDIR && rd_u16(inode, 26) != 0
}

const DIR_MODE: u16 = S_IFDIR | 0o755;

/// Fixed `i_dtime` stamp for a freed inode (2023-11-14T22:13:20Z), the **fallback** for a machine
/// whose wall clock could not be anchored; not a small value that `e2fsck` would read as an
/// orphan-list link.
const DELETION_TIME: u32 = 1_700_000_000;

fn bit_unset(map: &mut [u8], i: usize) {
    map[i / 8] &= !(1 << (i % 8));
}

/// Allocate one block and append it to `inode`'s tree as logical block `next_lb`, extending the
/// last extent when the allocation continues it. Returns the physical block. The caller owns
/// `inode`: only the in-memory copy changes. On an error the block goes back and the tree is as
/// it was.
fn append_block<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    inode: &mut [u8; 256],
    next_lb: u64,
) -> Result<u64, FsError> {
    let goal = extent::last_end(rw, sb, inode)?.unwrap_or(sb.first_data_block as u64);
    let phys = alloc_block(rw, sb, goal)?;
    if let Err(e) = extent::append(rw, sb, inode, Extent::written(next_lb, phys, 1)) {
        let _ = free_block(rw, sb, phys);
        return Err(e);
    }
    Ok(phys)
}

/// Free inode `ino`: clear its inode-bitmap bit and increment the group-descriptor and superblock
/// free-inode counts. If `is_dir`, also decrements the group's `bg_used_dirs_count`.
fn free_inode<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    ino: u32,
    is_dir: bool,
    now: i64,
) -> Result<(), FsError> {
    let bs = sb.block_size as usize;
    let group = (ino - 1) / sb.inodes_per_group;
    let index = ((ino - 1) % sb.inodes_per_group) as usize;
    let gd_off = sb.first_gdt_block * sb.block_size as u64 + group as u64 * sb.desc_size as u64;
    let dsz = (sb.desc_size as usize).min(64);
    let mut gd = [0u8; 64];
    rw.read_meta(gd_off, &mut gd[..dsz])?;
    let ibitmap_block = rd_u32(&gd, 4) as u64;

    // Mark the inode itself deleted: `e2fsck` decides an inode is in use from `i_links_count` +
    // `i_dtime`, *not* the bitmap. Zero `i_links_count` (@26) and stamp `i_dtime` (@20).
    let ioff = inode_offset(rw, sb, ino)?;
    let mut inode = [0u8; 256];
    let n = (sb.inode_size as usize).min(256);
    rw.read_meta(ioff, &mut inode[..n])?;
    // `i_dtime` doubles as the orphan-list "next" pointer while `i_links_count == 0`; a small value
    // reads as a corrupted orphan chain. So use the real deletion time, or `DELETION_TIME` when
    // the machine has no wall clock (`now == 0`).
    let dtime = if now > 0 { now as u32 } else { DELETION_TIME };
    inode[20..24].copy_from_slice(&dtime.to_le_bytes()); // i_dtime
    inode[26..28].copy_from_slice(&0u16.to_le_bytes()); // i_links_count = 0
    rw.write_at(ioff, &inode[..n])?;

    let mut bitmap = [0u8; MAX_BLOCK];
    rw.read_meta(ibitmap_block * sb.block_size as u64, &mut bitmap[..bs])?;
    bit_unset(&mut bitmap, index);
    rw.write_at(ibitmap_block * sb.block_size as u64, &bitmap[..bs])?;

    // Group free-inode count (@14, u16) + optionally used-dirs count (@16, u16).
    let gfree = rd_u16(&gd, 14).wrapping_add(1);
    gd[14..16].copy_from_slice(&gfree.to_le_bytes());
    if is_dir {
        let used = rd_u16(&gd, 16).wrapping_sub(1);
        gd[16..18].copy_from_slice(&used.to_le_bytes());
    }
    rw.write_at(gd_off, &gd[..dsz])?;
    let mut sbbuf = [0u8; 1024];
    rw.read_meta(1024, &mut sbbuf)?;
    let sfree = rd_u32(&sbbuf, 16).wrapping_add(1);
    sbbuf[16..20].copy_from_slice(&sfree.to_le_bytes());
    rw.write_at(1024, &sbbuf)?;
    Ok(())
}

/// Free filesystem block `block`: [`free_run`] of one.
pub(crate) fn free_block<RW: BlockReader + BlockWriter>(rw: &RW, sb: &Superblock, block: u64) -> Result<(), FsError> {
    free_run(rw, sb, block, 1)
}

/// **Free `len` blocks from `phys`**: clear their block-bitmap bits and increment the
/// group-descriptor and superblock free-block counts by as many as were set; the inverse of
/// [`alloc_run`], a group at a time where the run crosses one. `Corrupt` for a run not all on the
/// filesystem, before anything is written. A bit already clear is not counted, so a run freed
/// twice does not count its blocks free twice.
pub(crate) fn free_run<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    phys: u64,
    len: u64,
) -> Result<(), FsError> {
    let bs = sb.block_size as usize;
    let per_group = sb.blocks_per_group as u64;
    let end = phys.checked_add(len).ok_or(FsError::Corrupt)?;
    if phys < sb.first_data_block as u64 || end > sb.blocks_count {
        return Err(FsError::Corrupt);
    }
    // What is freed may next be a file's, which the kernel writes behind this server's back: a
    // metadata cache lets it go.
    rw.forget(phys * bs as u64, len * bs as u64);
    let dsz = (sb.desc_size as usize).min(64);
    let mut gd = [0u8; 64];
    let mut bitmap = [0u8; MAX_BLOCK];
    let mut blk = phys;
    while blk < end {
        let rel = blk - sb.first_data_block as u64;
        let group = (rel / per_group) as u32;
        let index = (rel % per_group) as usize;
        let n = (end - blk).min(per_group - index as u64) as usize;
        let gd_off = sb.group_desc_offset(group);
        rw.read_meta(gd_off, &mut gd[..dsz])?;
        let bitmap_block = rd_u32(&gd, 0) as u64;
        rw.read_meta(bitmap_block * sb.block_size as u64, &mut bitmap[..bs])?;
        let mut cleared = 0u16;
        for i in index..index + n {
            if !bit_clear(&bitmap, i) {
                bit_unset(&mut bitmap, i);
                cleared += 1;
            }
        }
        rw.write_at(bitmap_block * sb.block_size as u64, &bitmap[..bs])?;

        let gfree = rd_u16(&gd, 12).wrapping_add(cleared);
        gd[12..14].copy_from_slice(&gfree.to_le_bytes());
        rw.write_at(gd_off, &gd[..dsz])?;
        let mut sbbuf = [0u8; 1024];
        rw.read_meta(1024, &mut sbbuf)?;
        let sfree = rd_u32(&sbbuf, 12).wrapping_add(cleared as u32);
        sbbuf[12..16].copy_from_slice(&sfree.to_le_bytes());
        rw.write_at(1024, &sbbuf)?;
        blk += n as u64;
    }
    Ok(())
}

/// Free every block of `inode` — its data and its extent tree's own blocks, at any depth — on a
/// copy; the inode itself is untouched. **Called once the inode is freed**, so an error here leaks
/// what is left, which `e2fsck` gives back; called first, a lost write after it would leave a live
/// inode mapping blocks this had freed.
fn free_inode_blocks<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    inode: &[u8; 256],
) -> Result<(), FsError> {
    let mut copy = *inode;
    extent::truncate(rw, sb, &mut copy, 0, None)
}

/// Remove the directory entry `name` from directory-inode bytes `dir_inode`, returning the removed
/// entry's `(inode, file_type)`. Merges its `rec_len` into the previous entry in its block; if it
/// is the first in a block, tombstones it (`e_ino = 0`). Both are the standard ext2/3/4 removal
/// and stay `e2fsck`-clean. `NotFound` if absent.
fn dir_remove<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    dir_inode: &[u8; 256],
    name: &[u8],
) -> Result<(u32, u8), FsError> {
    let bs = sb.block_size as usize;
    let size = rd_u32(dir_inode, 4) as u64;
    let nblocks = size.div_ceil(sb.block_size as u64);
    let mut buf = [0u8; MAX_BLOCK];
    let mut map = Lookup::new(dir_inode);
    for lb in 0..nblocks {
        let phys = map.block(rw, sb, lb)?;
        if phys == 0 {
            continue;
        }
        rw.read_meta(phys * sb.block_size as u64, &mut buf[..bs])?;
        let mut off = 0;
        let mut prev: Option<usize> = None;
        while off + 8 <= bs {
            let e_ino = rd_u32(&buf, off);
            let rec_len = rd_u16(&buf, off + 4) as usize;
            let name_len = buf[off + 6] as usize;
            let file_type = buf[off + 7];
            if rec_len < 8 || off + rec_len > bs {
                break;
            }
            if e_ino != 0
                && name_len == name.len()
                && off + 8 + name_len <= bs
                && &buf[off + 8..off + 8 + name_len] == name
            {
                match prev {
                    Some(p) => {
                        let prev_rec = rd_u16(&buf, p + 4) as usize + rec_len;
                        buf[p + 4..p + 6].copy_from_slice(&(prev_rec as u16).to_le_bytes());
                    }
                    None => {
                        buf[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
                    }
                }
                rw.write_at(phys * sb.block_size as u64, &buf[..bs])?;
                return Ok((e_ino, file_type));
            }
            prev = Some(off);
            off += rec_len;
        }
    }
    Err(FsError::NotFound)
}

/// Adjust directory inode `ino`'s `i_links_count` by `delta`, for a subdirectory's `..`.
fn adjust_links<RW: BlockReader + BlockWriter>(rw: &RW, sb: &Superblock, ino: u32, delta: i32) -> Result<(), FsError> {
    let off = inode_offset(rw, sb, ino)?;
    let mut inode = [0u8; 256];
    let n = (sb.inode_size as usize).min(256);
    rw.read_meta(off, &mut inode[..n])?;
    let links = (rd_u16(&inode, 26) as i32 + delta) as u16;
    inode[26..28].copy_from_slice(&links.to_le_bytes());
    rw.write_at(off, &inode[..n])?;
    Ok(())
}

/// Create a subdirectory `name` inside directory inode `dir_ino`: allocate a directory inode and
/// one data block holding `.`/`..`, link it into the parent, and bump the parent's link count
/// (the new `..`). `Exists` if `name` is taken; `TooLarge` if the filesystem is full. **A failed
/// `mkdir` gives back what it took**: the inode, its block, and its own group's directory count.
pub fn mkdir_at<RW: BlockReader + BlockWriter>(rw: &RW, dir_ino: u32, name: &[u8], now: i64) -> Result<(), FsError> {
    if name.is_empty() || name.len() > 255 || name.contains(&b'/') || name == b"." || name == b".." {
        return Err(FsError::Unsupported);
    }
    let sb = read_superblock(rw)?;
    let parent = read_inode(rw, &sb, dir_ino)?;
    if !live_dir(&parent) {
        return Err(FsError::NotFound);
    }
    if dir_lookup(rw, &sb, &parent, name).is_ok() {
        return Err(FsError::Exists);
    }
    let bs = sb.block_size as usize;

    let new_ino = alloc_inode(rw, &sb)?;
    let mut taken_block = None;
    let mut counted = false;
    let made = (|| {
        let dblock = alloc_block(rw, &sb, sb.first_data_block as u64)?;
        taken_block = Some(dblock);

        // The new directory's block: `.` (self) at 0 with `rec_len` 12, then `..` (the parent) at
        // 12 with the rest of the block.
        let mut db = [0u8; MAX_BLOCK];
        db[0..4].copy_from_slice(&new_ino.to_le_bytes());
        db[4..6].copy_from_slice(&12u16.to_le_bytes());
        db[6] = 1;
        db[7] = EXT4_FT_DIR;
        db[8] = b'.';
        db[12..16].copy_from_slice(&dir_ino.to_le_bytes());
        db[16..18].copy_from_slice(&((bs - 12) as u16).to_le_bytes());
        db[18] = 2;
        db[19] = EXT4_FT_DIR;
        db[20] = b'.';
        db[21] = b'.';
        rw.write_at(dblock * sb.block_size as u64, &db[..bs])?;

        // Initialise the new inode: directory, 2 links (`.` + the parent's entry), one extent.
        let mut inode = [0u8; 256];
        inode[0..2].copy_from_slice(&DIR_MODE.to_le_bytes());
        stamp(&mut inode, now, sb.inode_size, Stamp::Created);
        inode[4..8].copy_from_slice(&(bs as u32).to_le_bytes()); // i_size = one block
        inode[26..28].copy_from_slice(&2u16.to_le_bytes()); // i_links_count
        inode[28..32].copy_from_slice(&((bs / 512) as u32).to_le_bytes()); // i_blocks (512-B units)
        inode[32..36].copy_from_slice(&EXTENTS_FL.to_le_bytes());
        // Extent header (magic, 1 entry, max 4, depth 0) + one leaf extent.
        inode[40..42].copy_from_slice(&extent::MAGIC.to_le_bytes());
        inode[42..44].copy_from_slice(&1u16.to_le_bytes()); // eh_entries
        inode[44..46].copy_from_slice(&4u16.to_le_bytes()); // eh_max
        inode[52..56].copy_from_slice(&0u32.to_le_bytes()); // ee_block = 0
        inode[56..58].copy_from_slice(&1u16.to_le_bytes()); // ee_len = 1
        inode[58..60].copy_from_slice(&((dblock >> 32) as u16).to_le_bytes()); // start_hi
        inode[60..64].copy_from_slice(&(dblock as u32).to_le_bytes()); // start_lo
        let ioff = inode_offset(rw, &sb, new_ino)?;
        rw.write_at(ioff, &inode[..(sb.inode_size as usize).min(256)])?;

        // `bg_used_dirs_count` (@16, u16) for the new inode's group.
        let gd_off = sb.group_desc_offset((new_ino - 1) / sb.inodes_per_group);
        let dsz = (sb.desc_size as usize).min(64);
        let mut gd = [0u8; 64];
        rw.read_meta(gd_off, &mut gd[..dsz])?;
        let used = rd_u16(&gd, 16).wrapping_add(1);
        gd[16..18].copy_from_slice(&used.to_le_bytes());
        rw.write_at(gd_off, &gd[..dsz])?;
        counted = true;

        let mut parent = parent;
        dir_insert(rw, &sb, dir_ino, &mut parent, name, new_ino, EXT4_FT_DIR, now)
    })();
    if let Err(e) = made {
        if let Some(dblock) = taken_block {
            let _ = free_block(rw, &sb, dblock);
        }
        let _ = free_inode(rw, &sb, new_ino, counted, now);
        return Err(e);
    }
    adjust_links(rw, &sb, dir_ino, 1)?;
    touch_inode(rw, &sb, dir_ino, now, Stamp::Modified)?;
    Ok(())
}

/// Remove the **regular file** `name` from directory inode `dir_ino`: unlink the entry and
/// decrement the target's link count. `NotFound` if absent; `Unsupported` if `name` is a
/// directory (use [`rmdir_at`]).
///
/// **The last name's removal frees nothing yet**: it returns the inode, still counting that link
/// and holding its blocks, and [`release_inode`] frees it only after `File::Forget` has been
/// answered — the kernel may still be writing those blocks, and a write it had already issued
/// could land in a block handed to another file. `None` when another name still reaches the inode.
pub fn unlink_at<RW: BlockReader + BlockWriter>(
    rw: &RW,
    dir_ino: u32,
    name: &[u8],
    now: i64,
) -> Result<Option<u32>, FsError> {
    let sb = read_superblock(rw)?;
    let parent = read_inode(rw, &sb, dir_ino)?;
    if !live_dir(&parent) {
        return Err(FsError::NotFound);
    }
    let target_ino = dir_lookup(rw, &sb, &parent, name)?;
    let target = read_inode(rw, &sb, target_ino)?;
    if rd_u16(&target, 0) & S_IFMT == S_IFDIR {
        return Err(FsError::Unsupported); // rmdir handles directories
    }

    dir_remove(rw, &sb, &parent, name)?;
    let orphan = drop_link(rw, &sb, target_ino, &target, now)?;
    touch_inode(rw, &sb, dir_ino, now, Stamp::Modified)?;
    Ok(orphan)
}

/// Take away one of `ino`'s names. With others left, its link count drops now; for its last,
/// nothing is written, and `Some(ino)` says it waits for [`release_inode`].
fn drop_link<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    ino: u32,
    inode: &[u8; 256],
    now: i64,
) -> Result<Option<u32>, FsError> {
    let links = rd_u16(inode, 26);
    if links <= 1 {
        return Ok(Some(ino));
    }
    let off = inode_offset(rw, sb, ino)?;
    let mut t = *inode;
    t[26..28].copy_from_slice(&(links - 1).to_le_bytes());
    // A surviving hard link: only the link count changed, not the contents — ctime moves, mtime
    // does not.
    stamp(&mut t, now, sb.inode_size, Stamp::MetadataOnly);
    rw.write_at(off, &t[..(sb.inode_size as usize).min(256)])?;
    Ok(None)
}

/// **Free an inode no name reaches any more** — the inode itself, then its blocks: the second half
/// of [`unlink_at`], or of a [`rename_path`] that replaced a file. A server calls it only once the
/// kernel has answered `File::Forget`, so nothing it issued can still land in these blocks.
/// Between the halves the inode is unattached with its link still counted; a crash there leaves
/// what `rename_path` already could (`e2fsck` moves the file to `lost+found`) and never a block
/// claimed twice. `NotFound` unless `ino` is a regular file with a link counted.
pub fn release_inode<RW: BlockReader + BlockWriter>(rw: &RW, ino: u32, now: i64) -> Result<(), FsError> {
    let sb = read_superblock(rw)?;
    if ino == 0 || ino > sb.inodes_count {
        return Err(FsError::NotFound);
    }
    let inode = read_inode(rw, &sb, ino)?;
    if rd_u16(&inode, 0) & S_IFMT != S_IFREG || rd_u16(&inode, 26) == 0 {
        return Err(FsError::NotFound);
    }
    // The inode, then its blocks: a failure between leaks them rather than mapping them freed.
    free_inode(rw, &sb, ino, false, now)?;
    free_inode_blocks(rw, &sb, &inode)
}

/// Remove the **empty subdirectory** `name` from directory inode `dir_ino`: verify it holds only
/// `.`/`..`, unlink it, free its block and inode, and decrement the parent's link count. `NotFound`
/// if absent; `Unsupported` if not a directory (use [`unlink_at`]); `NotEmpty` if it has entries.
pub fn rmdir_at<RW: BlockReader + BlockWriter>(rw: &RW, dir_ino: u32, name: &[u8], now: i64) -> Result<(), FsError> {
    if name == b"." || name == b".." {
        return Err(FsError::Unsupported);
    }
    let sb = read_superblock(rw)?;
    let parent = read_inode(rw, &sb, dir_ino)?;
    if !live_dir(&parent) {
        return Err(FsError::NotFound);
    }
    let sub_ino = dir_lookup(rw, &sb, &parent, name)?;
    let sub = read_inode(rw, &sb, sub_ino)?;
    if rd_u16(&sub, 0) & S_IFMT != S_IFDIR {
        return Err(FsError::Unsupported);
    }
    let mut extra = false;
    read_dir(rw, sub_ino, 0, |_ino, _ft, ename| {
        if ename != b"." && ename != b".." {
            extra = true;
        }
        !extra // stop as soon as a third entry is seen
    })?;
    if extra {
        return Err(FsError::NotEmpty);
    }

    dir_remove(rw, &sb, &parent, name)?;
    free_inode(rw, &sb, sub_ino, true, now)?;
    adjust_links(rw, &sb, dir_ino, -1)?; // the subdir's `..` no longer links the parent
    touch_inode(rw, &sb, dir_ino, now, Stamp::Modified)?;
    free_inode_blocks(rw, &sb, &sub)
}

/// Point the existing entry `name` in `dir_inode` at a different inode, in place.
///
/// One block write, no record shuffling, which makes an overwriting rename's crash window benign:
/// both names then refer to the source inode and the replaced inode is merely unreferenced (its
/// link count still ≥ 1), so a crash leaves work for `e2fsck` rather than a lost file. Removing
/// the destination first and re-inserting would open a window with *no* name for either.
fn dir_repoint<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    dir_inode: &[u8; 256],
    name: &[u8],
    new_ino: u32,
    new_ft: u8,
) -> Result<u32, FsError> {
    let bs = sb.block_size as usize;
    let size = rd_u32(dir_inode, 4) as u64;
    let nblocks = size.div_ceil(sb.block_size as u64);
    let mut buf = [0u8; MAX_BLOCK];
    let mut map = Lookup::new(dir_inode);
    for lb in 0..nblocks {
        let phys = map.block(rw, sb, lb)?;
        if phys == 0 {
            continue;
        }
        rw.read_meta(phys * sb.block_size as u64, &mut buf[..bs])?;
        let mut off = 0;
        while off + 8 <= bs {
            let e_ino = rd_u32(&buf, off);
            let rec_len = rd_u16(&buf, off + 4) as usize;
            let name_len = buf[off + 6] as usize;
            if rec_len < 8 || off + rec_len > bs {
                break;
            }
            if e_ino != 0 && name_len == name.len() && &buf[off + 8..off + 8 + name_len] == name {
                let old = e_ino;
                buf[off..off + 4].copy_from_slice(&new_ino.to_le_bytes());
                buf[off + 7] = new_ft;
                rw.write_at(phys * sb.block_size as u64, &buf[..bs])?;
                return Ok(old);
            }
            off += rec_len;
        }
    }
    Err(FsError::NotFound)
}

fn split_parent(path: &[u8]) -> Option<(&[u8], &[u8])> {
    let slash = path.iter().rposition(|&c| c == b'/')?;
    let leaf = &path[slash + 1..];
    if leaf.is_empty() || leaf == b"." || leaf == b".." {
        return None;
    }
    Some((if slash == 0 { b"/" } else { &path[..slash] }, leaf))
}

/// Rename `old_path` to `new_path` **anywhere within this filesystem**, optionally replacing an
/// existing destination: the path-addressed counterpart to [`rename_at`], since a session bound to
/// one inode cannot name two directories.
///
/// Ordering is chosen so that a crash cannot lose the **source**, since no journal exists:
///
/// 1. Point the destination name at the source inode: repoint an existing entry when replacing,
///    otherwise insert a new one.
/// 2. Remove the source's old entry.
/// 3. Drop the replaced inode's link. If it was the last, the inode is returned rather than freed,
///    for [`release_inode`] once `File::Forget` has been answered, as [`unlink_at`] does.
///
/// A crash between 1 and 2 leaves the file reachable under *both* names; after 2 it can leave the
/// replaced inode unreferenced with a positive link count. `e2fsck` repairs both (the latter into
/// `lost+found`), and neither loses the file being moved. Moving a **directory** also repoints
/// its `..` and shifts one link between parents; replacing one is refused (`Unsupported`).
pub fn rename_path<RW: BlockReader + BlockWriter>(
    rw: &RW,
    old_path: &[u8],
    new_path: &[u8],
    replace: bool,
    now: i64,
) -> Result<Option<u32>, FsError> {
    let sb = read_superblock(rw)?;
    let (old_parent_path, old_name) = split_parent(old_path).ok_or(FsError::Unsupported)?;
    let (new_parent_path, new_name) = split_parent(new_path).ok_or(FsError::Unsupported)?;
    let (old_dir_ino, old_dir) = resolve_path_ino(rw, &sb, old_parent_path)?;
    let (new_dir_ino, mut new_dir) = resolve_path_ino(rw, &sb, new_parent_path)?;
    if rd_u16(&old_dir, 0) & S_IFMT != S_IFDIR || rd_u16(&new_dir, 0) & S_IFMT != S_IFDIR {
        return Err(FsError::NotFound);
    }
    if old_dir_ino == new_dir_ino && old_name == new_name {
        return Ok(None);
    }

    let src_ino = dir_lookup(rw, &sb, &old_dir, old_name)?;
    let src = read_inode(rw, &sb, src_ino)?;
    let src_is_dir = rd_u16(&src, 0) & S_IFMT == S_IFDIR;
    let src_ft = if src_is_dir { EXT4_FT_DIR } else { EXT4_FT_REG_FILE };

    // Moving a directory into its own subtree would detach it (its `..` would point into the
    // cycle). Cheap guard: refuse the parent-into-child case, the reachable one; a full ancestor
    // walk is not done.
    if src_is_dir && new_parent_path.starts_with(old_path) {
        return Err(FsError::Unsupported);
    }

    let replaced = match dir_lookup(rw, &sb, &new_dir, new_name) {
        Ok(dest_ino) => {
            if !replace {
                return Err(FsError::Exists);
            }
            let dest = read_inode(rw, &sb, dest_ino)?;
            if rd_u16(&dest, 0) & S_IFMT == S_IFDIR {
                return Err(FsError::Unsupported); // replacing a directory is deferred
            }
            dir_repoint(rw, &sb, &new_dir, new_name, src_ino, src_ft)?;
            Some(dest_ino)
        }
        Err(FsError::NotFound) => {
            dir_insert(rw, &sb, new_dir_ino, &mut new_dir, new_name, src_ino, src_ft, now)?;
            None
        }
        Err(e) => return Err(e),
    };

    // Step 2: drop the source's old name. Re-read the old parent: `dir_insert` may have grown it
    // (when both paths share a parent), which rewrote its inode.
    let (_, old_dir) = resolve_path_ino(rw, &sb, old_parent_path)?;
    dir_remove(rw, &sb, &old_dir, old_name)?;

    // Step 3: drop the link the destination name held.
    let orphan = match replaced {
        Some(dest_ino) => {
            let dest = read_inode(rw, &sb, dest_ino)?;
            drop_link(rw, &sb, dest_ino, &dest, now)?
        }
        None => None,
    };

    // A directory carries a link to its parent through `..`, so a move between parents shifts
    // one link and rewrites that entry.
    if src_is_dir && old_dir_ino != new_dir_ino {
        let src_dir = read_inode(rw, &sb, src_ino)?;
        dir_repoint(rw, &sb, &src_dir, b"..", new_dir_ino, EXT4_FT_DIR)?;
        adjust_links(rw, &sb, old_dir_ino, -1)?;
        adjust_links(rw, &sb, new_dir_ino, 1)?;
    }

    touch_inode(rw, &sb, old_dir_ino, now, Stamp::Modified)?;
    if new_dir_ino != old_dir_ino {
        touch_inode(rw, &sb, new_dir_ino, now, Stamp::Modified)?;
    }
    Ok(orphan)
}

/// Rename `old` to `new` **within** directory inode `dir_ino`: move the entry, preserving its
/// target inode and type. `NotFound` if `old` is absent; `Exists` if `new` is taken.
pub fn rename_at<RW: BlockReader + BlockWriter>(
    rw: &RW,
    dir_ino: u32,
    old: &[u8],
    new: &[u8],
    now: i64,
) -> Result<(), FsError> {
    if new.is_empty() || new.len() > 255 || new.contains(&b'/') || new == b"." || new == b".." {
        return Err(FsError::Unsupported);
    }
    let sb = read_superblock(rw)?;
    let parent = read_inode(rw, &sb, dir_ino)?;
    if !live_dir(&parent) {
        return Err(FsError::NotFound);
    }
    if dir_lookup(rw, &sb, &parent, new).is_ok() {
        return Err(FsError::Exists);
    }
    // Remove `old` (yielding its inode and type), then insert it under `new`. `dir_remove` did
    // not change the parent *inode*, so the cached bytes still locate the blocks.
    let (ino, ft) = dir_remove(rw, &sb, &parent, old)?;
    let mut parent = parent;
    if let Err(e) = dir_insert(rw, &sb, dir_ino, &mut parent, new, ino, ft, now) {
        // **The old name goes back**: a longer name in a full directory on a full filesystem needs
        // a block it cannot have, and the file would be left with no name. The old record's room is
        // where the removal left it, so putting it back needs no block.
        let _ = dir_insert(rw, &sb, dir_ino, &mut parent, old, ino, ft, now);
        return Err(e);
    }
    touch_inode(rw, &sb, dir_ino, now, Stamp::Modified)?;
    Ok(())
}

/// Shrink the regular file at `path` to `new_size` bytes, freeing the blocks past the new end,
/// and return the resulting size. Shrink **only**: a `new_size` at or above the current size is a
/// no-op reporting the current size, not an error. The cut handles an extent entirely past the
/// new end (freed and dropped), straddling it (shortened, its tail freed), or within it
/// (untouched), at any depth, freeing the tree blocks it empties (`extent::truncate`).
pub fn truncate_file<RW: BlockReader + BlockWriter>(
    rw: &RW,
    path: &[u8],
    new_size: usize,
    now: i64,
) -> Result<usize, FsError> {
    let sb = read_superblock(rw)?;
    let (ino, mut inode) = resolve_path_ino(rw, &sb, path)?;
    if rd_u16(&inode, 0) & S_IFMT != S_IFREG {
        return Err(FsError::NotFound);
    }
    let flags = rd_u32(&inode, 32);
    if flags & EXTENTS_FL == 0 || flags & INLINE_DATA_FL != 0 {
        return Err(FsError::Unsupported);
    }

    let size_hi = if sb.inode_size > 128 { rd_u32(&inode, 108) as u64 } else { 0 };
    let cur_size = ((rd_u32(&inode, 4) as u64) | (size_hi << 32)) as usize;
    if new_size >= cur_size {
        return Ok(cur_size); // nothing to shrink; growing is `grow_file`'s job
    }

    let bs = sb.block_size as usize;
    // Blocks the file keeps: everything holding a byte below `new_size`; a partial final block
    // stays.
    let keep_blocks = new_size.div_ceil(bs) as u64;

    // The tree is cut where the kept blocks end; it counts what it frees in `i_blocks`, and writes
    // the inode before freeing what its root drops.
    let off = inode_offset(rw, &sb, ino)?;
    extent::truncate(rw, &sb, &mut inode, keep_blocks, Some(off))?;

    inode[4..8].copy_from_slice(&(new_size as u32).to_le_bytes());
    if sb.inode_size > 128 {
        inode[108..112].copy_from_slice(&((new_size as u64 >> 32) as u32).to_le_bytes());
    }
    stamp(&mut inode, now, sb.inode_size, Stamp::Modified);
    rw.write_at(off, &inode[..sb.inode_len()])?;
    Ok(new_size)
}

/// Resolve `path` (absolute) to a **regular file** and read its content into `out[..size]`,
/// returning the size. Errors as [`resolve_regular_file`], plus `TooLarge` (the file exceeds
/// [`MAX_FILE`] or `out`).
pub fn read_file<R: BlockReader>(r: &R, path: &[u8], out: &mut [u8]) -> Result<usize, FsError> {
    let sb = read_superblock(r)?;
    let (_, inode, size) = resolve_regular_file(r, &sb, path)?;
    if size > MAX_FILE || size > out.len() {
        return Err(FsError::TooLarge);
    }

    let bs = sb.block_size as usize;
    let mut buf = [0u8; MAX_BLOCK];
    let mut map = Lookup::new(&inode);
    let mut copied = 0;
    let mut lb = 0u64;
    while copied < size {
        let n = bs.min(size - copied);
        let phys = map.block(r, &sb, lb)?;
        if phys == 0 {
            out[copied..copied + n].fill(0); // sparse hole
        } else {
            r.read_at(phys * sb.block_size as u64, &mut buf[..bs])?;
            out[copied..copied + n].copy_from_slice(&buf[..n]);
        }
        copied += n;
        lb += 1;
    }
    Ok(size)
}

#[cfg(test)]
mod tests {
    /// The group geometry the allocators work from, and the **last group's short tail**: a bitmap's
    /// spare bits address blocks past the end of the device.
    #[test]
    fn a_short_final_group_reports_only_the_blocks_it_has() {
        use super::Superblock;
        let sb = Superblock {
            block_size: 1024,
            inodes_per_group: 2048,
            inode_size: 256,
            desc_size: 32,
            first_gdt_block: 2,
            blocks_per_group: 8192,
            first_data_block: 1,
            blocks_count: 24576,
            inodes_count: 6144,
        };
        assert_eq!(sb.group_count(), 3);
        assert_eq!(sb.blocks_in_group(0), 8192);
        assert_eq!(sb.blocks_in_group(1), 8192);
        assert_eq!(sb.blocks_in_group(2), 8191, "the tail group is short by one");
        let total: usize = (0..3).map(|g| sb.blocks_in_group(g)).sum();
        assert_eq!(total as u64, sb.blocks_count - sb.first_data_block as u64);
        assert_eq!(sb.group_start(0), 1);
        assert_eq!(sb.group_start(2), 16385);
        assert_eq!(sb.inodes_in_group(2), 2048);
        let odd = Superblock { inodes_count: 5000, ..sb };
        assert_eq!(odd.inodes_in_group(2), 5000 - 2 * 2048);
        assert_eq!(odd.inodes_in_group(9), 0, "a group past the end has nothing");
    }

    /// Descriptor offsets are a flat array, so a table spanning blocks needs no special case.
    #[test]
    fn group_descriptors_are_a_flat_array_across_block_boundaries() {
        use super::Superblock;
        let sb = Superblock {
            block_size: 1024,
            inodes_per_group: 2048,
            inode_size: 256,
            desc_size: 32,
            first_gdt_block: 2,
            blocks_per_group: 8192,
            first_data_block: 1,
            blocks_count: 8 * 1024 * 1024,
            inodes_count: 262_144,
        };
        assert_eq!(sb.group_desc_offset(0), 2048);
        assert_eq!(sb.group_desc_offset(31), 2048 + 31 * 32);
        assert_eq!(sb.group_desc_offset(32), 2048 + 1024);
    }

    /// `decode_time` takes the epoch-extension bits from the *matching* `extra` word.
    #[test]
    fn decode_time_applies_the_epoch_extension() {
        assert_eq!(super::decode_time(1_784_900_730, 0), 1_784_900_730);
        assert_eq!(super::decode_time(0, 1), 1i64 << 32);
        assert_eq!(super::decode_time(7, 2), (2i64 << 32) | 7);
        assert_eq!(super::decode_time(5, 0xFFFF_FFFC), 5);
    }

    /// A stamped inode places each timestamp at its own offset.
    #[test]
    fn stamp_writes_each_field_at_its_own_offset() {
        use super::{I_ATIME, I_CRTIME, I_CTIME, I_MTIME, I_MTIME_EXTRA, Stamp, stamp};
        let now: i64 = (1i64 << 32) | 1_784_900_730; // past 2038: epoch bit set
        let mut inode = [0u8; 256];
        stamp(&mut inode, now, 256, Stamp::Created);

        let at = |off: usize| u32::from_le_bytes(inode[off..off + 4].try_into().unwrap());
        assert_eq!(at(I_MTIME), 1_784_900_730);
        assert_eq!(at(I_CTIME), 1_784_900_730);
        assert_eq!(at(I_ATIME), 1_784_900_730);
        assert_eq!(at(I_CRTIME), 1_784_900_730);
        // The epoch bit lands in mtime's own extra word, and `decode_time` recovers the full value.
        assert_eq!(at(I_MTIME_EXTRA) & 0x3, 1);
        assert_eq!(super::decode_time(at(I_MTIME), at(I_MTIME_EXTRA)), now);
    }

    /// A metadata-only change moves ctime and leaves mtime alone.
    #[test]
    fn metadata_only_stamp_leaves_mtime_untouched() {
        use super::{I_CTIME, I_MTIME, Stamp, stamp};
        let mut inode = [0u8; 256];
        stamp(&mut inode, 1000, 256, Stamp::Created);
        stamp(&mut inode, 2000, 256, Stamp::MetadataOnly);
        let at = |off: usize| u32::from_le_bytes(inode[off..off + 4].try_into().unwrap());
        assert_eq!(at(I_MTIME), 1000, "contents did not change");
        assert_eq!(at(I_CTIME), 2000, "the inode did");
    }

    /// A base-size (128-byte) inode has no `*_extra` words.
    #[test]
    fn a_small_inode_gets_no_extra_words() {
        use super::{I_CTIME_EXTRA, Stamp, stamp};
        let mut inode = [0u8; 256];
        stamp(&mut inode, 1_784_900_730, 128, Stamp::Created);
        assert_eq!(&inode[I_CTIME_EXTRA..I_CTIME_EXTRA + 4], &[0, 0, 0, 0]);
    }

    use super::{Unservable, check_device, read_file, stat_file};
    use crate::FsError;
    use crate::test_support::{ImageReader, fixture};

    /// Set a little-endian field in a fixture image's superblock (byte 1024 onwards).
    fn poke_sb(img: &mut [u8], off: usize, bytes: &[u8]) {
        img[1024 + off..1024 + off + bytes.len()].copy_from_slice(bytes);
    }

    #[test]
    fn a_real_filesystem_passes_the_check_at_both_block_sizes() {
        for bs in [1024, 4096] {
            assert_eq!(check_device(&ImageReader(fixture(bs, b"g\n"))), Ok(()), "{bs}-byte blocks");
        }
    }

    #[test]
    fn a_zeroed_device_is_refused_for_its_magic_and_a_short_one_as_unreadable() {
        let len = fixture(4096, b"g\n").len();
        assert_eq!(check_device(&ImageReader(vec![0; len])), Err(Unservable::NoMagic { found: 0 }));
        assert_eq!(check_device(&ImageReader(vec![0; 1500])), Err(Unservable::Unreadable));
    }

    #[test]
    fn each_superblock_check_names_what_it_found() {
        let good = fixture(4096, b"g\n");
        let refused = |off: usize, bytes: &[u8]| {
            let mut img = good.clone();
            poke_sb(&mut img, off, bytes);
            check_device(&ImageReader(img)).unwrap_err()
        };
        assert_eq!(refused(56, &0x1234u16.to_le_bytes()), Unservable::NoMagic { found: 0x1234 });
        assert_eq!(refused(96, &0x80u32.to_le_bytes()), Unservable::Wide64Bit);
        assert_eq!(refused(24, &3u32.to_le_bytes()), Unservable::BlockTooLarge { log: 3 });
        assert_eq!(refused(88, &0u16.to_le_bytes()), Unservable::ZeroField("s_inode_size"));
        assert_eq!(refused(40, &0u32.to_le_bytes()), Unservable::ZeroField("s_inodes_per_group"));
        assert_eq!(refused(32, &0u32.to_le_bytes()), Unservable::ZeroField("s_blocks_per_group"));
    }

    /// **A right superblock over the wrong blocks**: the superblock alone passes it.
    #[test]
    fn a_superblock_whose_layout_is_not_there_is_refused_at_the_root() {
        for (bs, gdt) in [(4096usize, 4096usize), (1024, 2048)] {
            let good = fixture(bs as u32, b"g\n");

            let mut img = good.clone();
            img[gdt..gdt + bs].fill(0);
            assert_eq!(
                check_device(&ImageReader(img)),
                Err(Unservable::RootNotDirectory { mode: 0 }),
                "{bs}-byte blocks, descriptors zeroed"
            );

            let mut img = good.clone();
            img[gdt + 8..gdt + 12].copy_from_slice(&0xFFFF_FF00u32.to_le_bytes());
            assert_eq!(
                check_device(&ImageReader(img)),
                Err(Unservable::RootUnreadable),
                "{bs}-byte blocks, inode table past the end"
            );
        }
    }

    /// The startup check and a request share one parser, so a refused device fails a request with
    /// the error the check's reason maps to.
    #[test]
    fn a_request_over_a_refused_device_fails_with_the_error_its_reason_names() {
        let good = fixture(4096, b"g\n");
        let mut out = [0u8; 64];

        let zeroed = ImageReader(vec![0; good.len()]);
        let why = check_device(&zeroed).unwrap_err();
        assert_eq!(read_file(&zeroed, b"/system/current-generation", &mut out), Err(why.fs_error()));
        assert_eq!(why.fs_error(), FsError::Corrupt);

        let mut wide = good.clone();
        poke_sb(&mut wide, 96, &0x80u32.to_le_bytes());
        let wide = ImageReader(wide);
        let why = check_device(&wide).unwrap_err();
        assert_eq!(stat_file(&wide, b"/system/current-generation"), Err(why.fs_error()));
        assert_eq!(why.fs_error(), FsError::Unsupported);
    }

    #[test]
    fn each_reason_reads_as_a_sentence_naming_what_was_found() {
        assert_eq!(
            format!("{}", Unservable::NoMagic { found: 0 }),
            "no ext4 filesystem: superblock magic 0x0000, not 0xef53"
        );
        assert_eq!(
            format!("{}", Unservable::RootNotDirectory { mode: 0o100644 }),
            "inode 2 is not a directory (mode 0o100644): the group descriptors or the inode table \
             are not what the superblock describes"
        );
        assert_eq!(
            format!("{}", Unservable::BlockTooLarge { log: u32::MAX }),
            "blocks of 2^4294967295 bytes, over the 4096 this server reads"
        );
    }
}
