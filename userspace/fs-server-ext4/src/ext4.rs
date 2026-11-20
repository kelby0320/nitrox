//! The ext4 parser and write path. See the crate docs for scope.
//!
//! Little-endian: the superblock at byte 1024, group descriptors after it, inodes via the
//! descriptor's inode-table block, data via the inode's **extent tree**, and directories as a
//! linear list of `ext4_dir_entry_2`.

mod alloc;
mod bitmap;
mod features;
mod grow;
mod insert;
mod listing;
mod state;
mod superblock;
mod unservable;
pub(crate) mod zone;

#[cfg(test)]
pub(crate) use alloc::block_in_use;
pub(crate) use alloc::{alloc_block, alloc_run, free_block, free_run};
use alloc::{alloc_inode, free_inode};
use bitmap::Bitmap;
pub use grow::{grow_file, grow_file_from};
use insert::dir_insert;
pub use listing::{read_dir, read_dir_stat};
pub use state::{mark_clean, mark_mounted, volume_label, was_left_clean};
pub(crate) use superblock::{Superblock, read_superblock};
pub use superblock::{block_size, check_device, check_writable};
pub use unservable::Unservable;

use crate::extent::{self, Extent, Lookup};
use crate::{BlockReader, BlockWriter, FsError, rd_u16, rd_u32};

/// Cap on a served file's size: 64 KiB.
pub const MAX_FILE: usize = 64 * 1024;

/// Largest filesystem block the reader supports (its block scratch buffer).
pub(crate) const MAX_BLOCK: usize = 4096;

const SUPER_MAGIC: u16 = 0xEF53;

const ROOT_INO: u32 = 2;
const INCOMPAT_64BIT: u32 = 0x80;
const EXTENTS_FL: u32 = 0x0008_0000;
const INLINE_DATA_FL: u32 = 0x1000_0000;
/// `EXT4_INDEX_FL`: the directory carries a hash tree, which this writer does not keep.
const INDEX_FL: u32 = 0x0000_1000;
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

/// Read inode `ino` into a fixed 256-byte buffer (inodes are ≤ 256 bytes here).
pub(crate) fn read_inode<R: BlockReader>(r: &R, sb: &Superblock, ino: u32) -> Result<[u8; 256], FsError> {
    let off = inode_offset(r, sb, ino)?;
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
    dir_find(r, sb, dir, name).map(|(ino, _)| ino)
}

/// [`dir_lookup`], and the type its record gives.
fn dir_find<R: BlockReader>(r: &R, sb: &Superblock, dir: &[u8; 256], name: &[u8]) -> Result<(u32, u8), FsError> {
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
        while let Some(e) = record(&buf[..bs], off) {
            if e.ino != 0 && e.name_len > 0 && &buf[off + 8..off + 8 + e.name_len] == name {
                return Ok((e.ino, e.file_type));
            }
            off += e.len;
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
    // avoided, and the kernel refuses such a map, so the file cannot be opened.
    Ok(MappedFile { size, block_size: bs, runs: n, ino })
}

/// Whether `name` may be written as a new entry: 1 to 255 bytes, its length being one byte, with no
/// `/` and no control character, as FAT refuses them; `e2fsck` calls a NUL illegal.
fn writable_name(name: &[u8]) -> bool {
    !name.is_empty() && name.len() <= 255 && !name.iter().any(|&c| c == b'/' || c < 0x20 || c == 0x7F)
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
    let (group, index) = sb.inode_place(ino)?;
    let gd_off = sb.group_desc_offset(group);
    let mut gd = [0u8; 32];
    r.read_meta(gd_off, &mut gd)?;
    let inode_table = rd_u32(&gd, 8) as u64;
    Ok(inode_table * sb.block_size as u64 + index as u64 * sb.inode_size as u64)
}

fn round4(n: usize) -> usize {
    (n + 3) & !3
}

/// A directory record's header: its inode (`0` for a gap), its length, and its name's.
struct Record {
    ino: u32,
    len: usize,
    name_len: usize,
    file_type: u8,
}

/// **The record at `off` in the directory block `buf`**, or `None` where the block is malformed
/// from there on: a record shorter than its header and name, not a multiple of 4, or past the
/// block, as Linux checks one. Its name is then `buf[off + 8..off + 8 + name_len]`, in bounds.
fn record(buf: &[u8], off: usize) -> Option<Record> {
    if off + 8 > buf.len() {
        return None;
    }
    let len = rd_u16(buf, off + 4) as usize;
    let name_len = buf[off + 6] as usize;
    if len < round4(8 + name_len) || !len.is_multiple_of(4) || off + len > buf.len() {
        return None;
    }
    Some(Record { ino: rd_u32(buf, off), len, name_len, file_type: buf[off + 7] })
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
    let sb = read_superblock(rw)?;
    let (_, parent_inode) = resolve_path_ino(rw, &sb, parent_path)?;
    if rd_u16(&parent_inode, 0) & S_IFMT != S_IFDIR {
        return Err(FsError::NotFound);
    }
    if let Ok(existing) = dir_lookup(rw, &sb, &parent_inode, name) {
        return Ok(existing); // already exists — idempotent
    }
    if !writable_name(name) {
        return Err(FsError::Unsupported);
    }

    let ino = alloc_inode(rw, &sb)?;
    // Initialise the new inode: regular file, one link, empty depth-0 extent tree, size 0.
    let mut inode = [0u8; 256];
    inode[0..2].copy_from_slice(&REG_FILE_MODE.to_le_bytes()); // i_mode
    stamp(&mut inode, now, sb.inode_size, Stamp::Created);
    inode[26..28].copy_from_slice(&1u16.to_le_bytes()); // i_links_count
    inode[32..36].copy_from_slice(&EXTENTS_FL.to_le_bytes()); // i_flags
    extent::empty_root(&mut inode); // i_block: a leaf of no extents

    // Written, then linked into the parent directory, growing it by a block if it is full. **A
    // failed write or link gives the inode back**, rather than leaving it allocated and unattached.
    let written = inode_offset(rw, &sb, ino).and_then(|off| rw.write_at(off, &inode[..sb.inode_len()]));
    let linked = written.and_then(|()| resolve_path_ino(rw, &sb, parent_path)).and_then(|(parent_ino, mut dir)| {
        dir_insert(rw, &sb, parent_ino, &mut dir, name, ino, EXT4_FT_REG_FILE, now).map(|()| parent_ino)
    });
    let parent_ino = match linked {
        Ok(parent_ino) => parent_ino,
        Err(e) => {
            let _ = free_inode(rw, &sb, ino, false, now);
            return Err(e);
        }
    };
    // The parent's contents changed, so its times move; linked, the file is made whether they do.
    let _ = touch_inode(rw, &sb, parent_ino, now, Stamp::Modified);
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
/// `EXT4_LINK_MAX`: past it a directory counts 1, `dir_nlink`'s "too many to count".
const DIR_LINK_MAX: i32 = 65_000;

/// Fixed `i_dtime` stamp for a freed inode (2023-11-14T22:13:20Z), the **fallback** for a machine
/// whose wall clock could not be anchored; not a small value that `e2fsck` would read as an
/// orphan-list link.
const DELETION_TIME: u32 = 1_700_000_000;

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
    extent::truncate(rw, sb, &mut copy, 0, None).map(drop)
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
        while let Some(e) = record(&buf[..bs], off) {
            if e.ino != 0 && e.name_len == name.len() && &buf[off + 8..off + 8 + e.name_len] == name {
                match prev {
                    Some(p) => {
                        let prev_rec = rd_u16(&buf, p + 4) as usize + e.len;
                        buf[p + 4..p + 6].copy_from_slice(&(prev_rec as u16).to_le_bytes());
                    }
                    None => {
                        buf[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
                    }
                }
                rw.write_at(phys * sb.block_size as u64, &buf[..bs])?;
                return Ok((e.ino, e.file_type));
            }
            prev = Some(off);
            off += e.len;
        }
    }
    Err(FsError::NotFound)
}

/// Adjust inode `ino`'s `i_links_count` by `delta`: a directory's for a subdirectory's `..`, a
/// file's for a name a rename is moving. `Corrupt` for a count it would take past its range.
///
/// **A directory's count is `dir_nlink`'s**, as Linux keeps it: 1 means more subdirectories than
/// 65,000 could count, so it stays 1, a count past that becomes 1, and a directory never goes
/// below its own two, `.` and its name.
fn adjust_links<RW: BlockReader + BlockWriter>(rw: &RW, sb: &Superblock, ino: u32, delta: i32) -> Result<(), FsError> {
    let off = inode_offset(rw, sb, ino)?;
    let mut inode = [0u8; 256];
    let n = (sb.inode_size as usize).min(256);
    rw.read_meta(off, &mut inode[..n])?;
    let was = rd_u16(&inode, 26) as i32;
    let links = match rd_u16(&inode, 0) & S_IFMT == S_IFDIR {
        true if was == 1 || (delta < 0 && was <= 2) => was,
        true if was + delta > DIR_LINK_MAX => 1,
        _ => was + delta,
    };
    let links = u16::try_from(links).map_err(|_| FsError::Corrupt)?;
    inode[26..28].copy_from_slice(&links.to_le_bytes());
    rw.write_at(off, &inode[..n])?;
    Ok(())
}

/// Create a subdirectory `name` inside directory inode `dir_ino`: allocate a directory inode and
/// one data block holding `.`/`..`, link it into the parent, and bump the parent's link count
/// (the new `..`). `Exists` if `name` is taken; `TooLarge` if the filesystem is full. **A failed
/// `mkdir` gives back what it took**: the inode, its block, and its own group's directory count.
pub fn mkdir_at<RW: BlockReader + BlockWriter>(rw: &RW, dir_ino: u32, name: &[u8], now: i64) -> Result<(), FsError> {
    if !writable_name(name) || name == b"." || name == b".." {
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
    let mut parent_counted = false;
    let made = (|| {
        // The parent counts the new `..` before anything names it, so a stop leaves it too high.
        adjust_links(rw, &sb, dir_ino, 1)?;
        parent_counted = true;
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

        let mut parent = read_inode(rw, &sb, dir_ino)?;
        dir_insert(rw, &sb, dir_ino, &mut parent, name, new_ino, EXT4_FT_DIR, now)
    })();
    if let Err(e) = made {
        if parent_counted {
            let _ = adjust_links(rw, &sb, dir_ino, -1);
        }
        if let Some(dblock) = taken_block {
            let _ = free_block(rw, &sb, dblock);
        }
        let _ = free_inode(rw, &sb, new_ino, counted, now);
        return Err(e);
    }
    // Linked, so made: a time left unwritten does not unmake it.
    let _ = touch_inode(rw, &sb, dir_ino, now, Stamp::Modified);
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
    // The name is gone, so nothing left fails the unlink: the server releases an orphan only on
    // `Ok`. A link count or a time left unwritten leaks at worst.
    let orphan = drop_link(rw, &sb, target_ino, &target, now).unwrap_or(None);
    let _ = touch_inode(rw, &sb, dir_ino, now, Stamp::Modified);
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
/// if absent; `Unsupported` if not a directory (use [`unlink_at`]); `NotEmpty` if it has entries;
/// `ReadOnly` through a read-only mount. **`freeing` is given its inode** before the first write.
pub fn rmdir_at<RW: BlockReader + BlockWriter>(
    rw: &RW,
    dir_ino: u32,
    name: &[u8],
    now: i64,
    freeing: impl FnOnce(u32),
) -> Result<(), FsError> {
    if name == b"." || name == b".." {
        return Err(FsError::Unsupported);
    }
    if rw.read_only() {
        return Err(FsError::ReadOnly);
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

    let up = counted_parent(rw, &sb, &sub);
    freeing(sub_ino);
    dir_remove(rw, &sb, &parent, name)?;
    // The name is gone, so nothing left fails the removal; what is left unwritten leaks at worst.
    // Its blocks go only once the inode has.
    if free_inode(rw, &sb, sub_ino, true, now).is_ok() {
        let _ = free_inode_blocks(rw, &sb, &sub);
    }
    if let Some(up) = up {
        let _ = adjust_links(rw, &sb, up, -1);
    }
    let _ = touch_inode(rw, &sb, dir_ino, now, Stamp::Modified);
    Ok(())
}

/// Point the existing entry `name` in `dir_inode` at `new_ino`, in place, in one block write:
/// returns the inode and type it named.
fn dir_repoint<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    dir_inode: &[u8; 256],
    name: &[u8],
    new_ino: u32,
    new_ft: u8,
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
        while let Some(e) = record(&buf[..bs], off) {
            if e.ino != 0 && e.name_len == name.len() && &buf[off + 8..off + 8 + e.name_len] == name {
                buf[off..off + 4].copy_from_slice(&new_ino.to_le_bytes());
                buf[off + 7] = new_ft;
                rw.write_at(phys * sb.block_size as u64, &buf[..bs])?;
                return Ok((e.ino, e.file_type));
            }
            off += e.len;
        }
    }
    Err(FsError::NotFound)
}

/// **Whether directory `dir` is `ancestor` or under it**, by its `..` records up to the root: a
/// path's bytes name one directory many ways. A chain longer than any path's 512 components is
/// `Corrupt`, as a loop of `..` records would be.
fn within<R: BlockReader>(r: &R, sb: &Superblock, mut dir: u32, ancestor: u32) -> Result<bool, FsError> {
    for _ in 0..512 {
        if dir == ancestor {
            return Ok(true);
        }
        if dir == ROOT_INO {
            return Ok(false);
        }
        // A `..` its parent does not answer, as a move stopped between leaves one, is no path up.
        let up = dir_lookup(r, sb, &read_inode(r, sb, dir)?, b"..")?;
        if !names(r, sb, &read_inode(r, sb, up)?, dir)? {
            return Err(FsError::Corrupt);
        }
        dir = up;
    }
    Err(FsError::Corrupt)
}

/// Whether directory `dir` has an entry, other than `.` and `..`, naming inode `ino`.
fn names<R: BlockReader>(r: &R, sb: &Superblock, dir: &[u8; 256], ino: u32) -> Result<bool, FsError> {
    let bs = sb.block_size as usize;
    let mut buf = [0u8; MAX_BLOCK];
    let mut map = Lookup::new(dir);
    for lb in 0..(rd_u32(dir, 4) as u64).div_ceil(bs as u64) {
        let phys = map.block(r, sb, lb)?;
        if phys == 0 {
            continue;
        }
        r.read_meta(phys * bs as u64, &mut buf[..bs])?;
        let mut off = 0;
        while let Some(e) = record(&buf[..bs], off) {
            let name = &buf[off + 8..off + 8 + e.name_len];
            if e.ino == ino && name != b"." && name != b".." {
                return Ok(true);
            }
            off += e.len;
        }
    }
    Ok(false)
}

/// **The directory whose count `dir`'s `..` holds a link in**, if it names a live directory: the
/// parent, unless a move stopped between. A crafted `..` naming a file names none.
fn counted_parent<R: BlockReader>(r: &R, sb: &Superblock, dir: &[u8; 256]) -> Option<u32> {
    let up = dir_lookup(r, sb, dir, b"..").ok()?;
    read_inode(r, sb, up).ok().filter(live_dir).map(|_| up)
}

fn split_parent(path: &[u8]) -> Option<(&[u8], &[u8])> {
    let slash = path.iter().rposition(|&c| c == b'/')?;
    let leaf = &path[slash + 1..];
    if leaf.is_empty() || leaf == b"." || leaf == b".." {
        return None;
    }
    Some((if slash == 0 { b"/" } else { &path[..slash] }, leaf))
}

/// Rename `old_path` to `new_path` **anywhere within this filesystem**, optionally replacing a file
/// there; a session bound to one inode cannot name two directories. A replaced file's last link
/// returns its inode, for [`release_inode`] once `File::Forget` is answered, as [`unlink_at`] does.
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
    let (old_dir, _) = resolve_path_ino(rw, &sb, old_parent_path)?;
    let (new_dir, _) = resolve_path_ino(rw, &sb, new_parent_path)?;
    rename(rw, &sb, (old_dir, old_name), (new_dir, new_name), replace, now)
}

/// Rename `old` to `new` **within** directory inode `dir_ino`. `NotFound` if `old` is absent;
/// `Exists` if `new` is taken.
pub fn rename_at<RW: BlockReader + BlockWriter>(
    rw: &RW,
    dir_ino: u32,
    old: &[u8],
    new: &[u8],
    now: i64,
) -> Result<(), FsError> {
    if !libfsserver::serve::entry_name(old) || !libfsserver::serve::entry_name(new) {
        return Err(FsError::Unsupported);
    }
    let sb = read_superblock(rw)?;
    rename(rw, &sb, (dir_ino, old), (dir_ino, new), false, now).map(drop)
}

/// **The rename both make.** No journal, so refusals come first, a failed step is undone, and no
/// stop leaves a name reaching what is freed: a file counts its new name before it has it and its
/// old until it is gone; a directory, with one name, loses the old first. **Once the names are
/// moved, nothing fails it**, as the server releases a replaced file only on `Ok`.
fn rename<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    (old_dir_ino, old_name): (u32, &[u8]),
    (new_dir_ino, new_name): (u32, &[u8]),
    replace: bool,
    now: i64,
) -> Result<Option<u32>, FsError> {
    let old_dir = read_inode(rw, sb, old_dir_ino)?;
    let new_dir = read_inode(rw, sb, new_dir_ino)?;
    if !live_dir(&old_dir) || !live_dir(&new_dir) {
        return Err(FsError::NotFound);
    }
    // The type the source's record gives, which the mode's format bits alone would not: a symlink,
    // a device or a socket from Linux.
    let (src_ino, src_ft) = dir_find(rw, sb, &old_dir, old_name)?;
    if old_dir_ino == new_dir_ino && old_name == new_name {
        return Ok(None);
    }
    let src = read_inode(rw, sb, src_ino)?;
    let src_is_dir = rd_u16(&src, 0) & S_IFMT == S_IFDIR;
    let moves_up = src_is_dir && old_dir_ino != new_dir_ino;
    // Moving a directory into its own subtree would detach it: its `..` would point into the cycle.
    if src_is_dir && within(rw, sb, new_dir_ino, src_ino)? {
        return Err(FsError::Unsupported);
    }
    if moves_up {
        dir_lookup(rw, sb, &src, b"..")?; // the record the move repoints
    }
    let up = counted_parent(rw, sb, &src);
    let replaced = match dir_lookup(rw, sb, &new_dir, new_name) {
        Ok(_) if !replace => return Err(FsError::Exists),
        Ok(dest_ino) if dest_ino == src_ino => return Ok(None), // as POSIX leaves it
        Ok(dest_ino) => {
            let dest = read_inode(rw, sb, dest_ino)?;
            if rd_u16(&dest, 0) & S_IFMT == S_IFDIR {
                return Err(FsError::Unsupported); // replacing a directory is deferred
            }
            Some((dest_ino, dest))
        }
        // Only a name not on the disk yet is written, so only it is held to what may be written.
        Err(FsError::NotFound) if writable_name(new_name) => None,
        Err(FsError::NotFound) => return Err(FsError::Unsupported),
        Err(e) => return Err(e),
    };

    // Each step reads its directory afresh: an insert that grows one rewrites its inode.
    let unlink_old = || read_inode(rw, sb, old_dir_ino).and_then(|d| dir_remove(rw, sb, &d, old_name)).map(drop);
    let relink_old = || {
        let mut d = read_inode(rw, sb, old_dir_ino)?;
        dir_insert(rw, sb, old_dir_ino, &mut d, old_name, src_ino, src_ft, now)
    };
    // The new name, and the type the record it repointed had.
    let link_new = || -> Result<Option<u8>, FsError> {
        let mut d = read_inode(rw, sb, new_dir_ino)?;
        match replaced {
            Some(_) => dir_repoint(rw, sb, &d, new_name, src_ino, src_ft).map(|(_, ft)| Some(ft)),
            None => dir_insert(rw, sb, new_dir_ino, &mut d, new_name, src_ino, src_ft, now).map(|()| None),
        }
    };
    let unlink_new = |ft: Option<u8>| {
        let d = read_inode(rw, sb, new_dir_ino)?;
        match (replaced, ft) {
            (Some((dest_ino, _)), Some(ft)) => dir_repoint(rw, sb, &d, new_name, dest_ino, ft).map(drop),
            _ => dir_remove(rw, sb, &d, new_name).map(drop),
        }
    };

    if src_is_dir {
        unlink_old()?;
        let ft = link_new().inspect_err(|_| {
            let _ = relink_old();
        })?;
        if moves_up {
            // The new parent counts the `..` before it is named, and the old stops after.
            let counted = adjust_links(rw, sb, new_dir_ino, 1);
            let repointed = counted.and_then(|()| {
                let s = read_inode(rw, sb, src_ino)?;
                dir_repoint(rw, sb, &s, b"..", new_dir_ino, EXT4_FT_DIR).map(drop)
            });
            if let Err(e) = repointed {
                if unlink_new(ft).is_ok() && relink_old().is_ok() && counted.is_ok() {
                    let _ = adjust_links(rw, sb, new_dir_ino, -1);
                }
                return Err(e);
            }
            if let Some(up) = up {
                let _ = adjust_links(rw, sb, up, -1);
            }
        }
    } else {
        adjust_links(rw, sb, src_ino, 1)?;
        let ft = link_new().inspect_err(|_| {
            let _ = adjust_links(rw, sb, src_ino, -1);
        })?;
        if let Err(e) = unlink_old() {
            if unlink_new(ft).is_ok() {
                let _ = adjust_links(rw, sb, src_ino, -1);
            }
            return Err(e);
        }
        let _ = adjust_links(rw, sb, src_ino, -1);
    }

    let orphan = replaced.and_then(|(dest_ino, dest)| drop_link(rw, sb, dest_ino, &dest, now).unwrap_or(None));
    let _ = touch_inode(rw, sb, old_dir_ino, now, Stamp::Modified);
    if new_dir_ino != old_dir_ino {
        let _ = touch_inode(rw, sb, new_dir_ino, now, Stamp::Modified);
    }
    Ok(orphan)
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
mod tests;
