//! The read-only ext4 parser. See the crate docs for scope.
//!
//! On-disk layout (all little-endian): the superblock at byte 1024; block-group
//! descriptors in the GDT after it; inodes located via the group descriptor's
//! inode-table block; file/directory data located via the inode's **extent
//! tree**; directories scanned as a linear list of `ext4_dir_entry_2`.

use crate::{BlockReader, BlockWriter, FsError, rd_u16, rd_u32};

/// Phase-2 cap on a served file's size (the read model's 64 KiB limit).
pub const MAX_FILE: usize = 64 * 1024;

/// Largest filesystem block the reader supports (its block scratch buffer).
const MAX_BLOCK: usize = 4096;

const SUPER_MAGIC: u16 = 0xEF53;
const ROOT_INO: u32 = 2;
const EXTENT_MAGIC: u16 = 0xF30A;
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

// --- inode timestamp fields -------------------------------------------------
//
// `ext4_inode` byte offsets. The four 32-bit second counts sit in the base
// (128-byte) inode; the matching `*_extra` words only exist when the filesystem's
// inode is larger, and carry the post-2038 epoch extension in their low two bits
// plus nanoseconds above that. Spelled out as constants because they are trivially
// confusable — `i_ctime_extra` (132) and `i_mtime_extra` (136) are adjacent, and
// reading one for the other is invisible for any date before 2038.

/// `i_atime` — last access.
const I_ATIME: usize = 8;
/// `i_ctime` — last inode (metadata) change.
const I_CTIME: usize = 12;
/// `i_mtime` — last content modification.
const I_MTIME: usize = 16;
/// `i_ctime_extra`.
const I_CTIME_EXTRA: usize = 132;
/// `i_mtime_extra`.
const I_MTIME_EXTRA: usize = 136;
/// `i_atime_extra`.
const I_ATIME_EXTRA: usize = 140;
/// `i_crtime` — creation time (large inodes only).
const I_CRTIME: usize = 144;
/// `i_crtime_extra`.
const I_CRTIME_EXTRA: usize = 148;
/// An inode larger than this base size carries the `*_extra` words.
const INODE_BASE_SIZE: u32 = 128;

/// Decode an ext4 timestamp: `secs` plus the two epoch-extension bits in the low
/// bits of its `extra` word (`0` when the inode has no extra fields).
///
/// The extension is what keeps timestamps correct past 2038: ext4 widens the
/// 32-bit second count by two high bits rather than moving to 64-bit fields.
fn decode_time(secs: u32, extra: u32) -> i64 {
    secs as i64 | ((extra & 0x3) as i64) << 32
}

/// Write `now` (Unix epoch seconds) into an inode's timestamp fields.
///
/// `which` selects which of the three are stamped — a content change touches
/// mtime and ctime, a pure metadata change only ctime — and `crtime` is set on
/// creation. `atime` is stamped at creation and never updated afterwards:
/// updating it on every read is the `noatime`-by-default choice every modern
/// filesystem has converged on, and this one has no read path through the server
/// to hook anyway (the kernel owns the data path).
///
/// The epoch-extension bits are written alongside, so a timestamp past 2038 is
/// stored as ext4 defines rather than wrapping.
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
    // Every case touches ctime: it is "the inode changed", which is true whenever
    // anything here is being written at all.
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
struct Superblock {
    block_size: u32,
    inodes_per_group: u32,
    inode_size: u32,
    desc_size: u32,
    /// First block of the group-descriptor table.
    first_gdt_block: u64,
    /// Blocks per block group (for locating a block's group + its bitmap).
    blocks_per_group: u32,
    /// The first data block (`1` for 1 KiB blocks, else `0`) — block numbering origin.
    first_data_block: u32,
}

fn read_superblock<R: BlockReader>(r: &R) -> Result<Superblock, FsError> {
    let mut sb = [0u8; 1024];
    r.read_at(1024, &mut sb)?;
    if rd_u16(&sb, 56) != SUPER_MAGIC {
        return Err(FsError::Corrupt);
    }
    if rd_u32(&sb, 96) & INCOMPAT_64BIT != 0 {
        return Err(FsError::Unsupported); // 64-bit changes the descriptor layout
    }
    let log_bs = rd_u32(&sb, 24);
    if log_bs > 2 {
        return Err(FsError::Unsupported); // > 4 KiB blocks exceed the scratch buffer
    }
    let block_size = 1024u32 << log_bs;
    let inode_size = rd_u16(&sb, 88) as u32;
    let inodes_per_group = rd_u32(&sb, 40);
    if inode_size == 0 || inodes_per_group == 0 {
        return Err(FsError::Corrupt);
    }
    let desc_size = match rd_u16(&sb, 254) as u32 {
        0 => 32,
        d => d,
    };
    let blocks_per_group = rd_u32(&sb, 32);
    let first_data_block = rd_u32(&sb, 20);
    if blocks_per_group == 0 {
        return Err(FsError::Corrupt);
    }
    Ok(Superblock {
        block_size,
        inodes_per_group,
        inode_size,
        desc_size,
        first_gdt_block: if block_size == 1024 { 2 } else { 1 },
        blocks_per_group,
        first_data_block,
    })
}

/// Read inode `ino` into a fixed 256-byte buffer (inodes are ≤ 256 bytes here).
fn read_inode<R: BlockReader>(r: &R, sb: &Superblock, ino: u32) -> Result<[u8; 256], FsError> {
    if ino == 0 {
        return Err(FsError::Corrupt);
    }
    let group = (ino - 1) / sb.inodes_per_group;
    let index = (ino - 1) % sb.inodes_per_group;
    // The group descriptor holds the inode-table block.
    let gd_off =
        sb.first_gdt_block * sb.block_size as u64 + group as u64 * sb.desc_size as u64;
    let mut gd = [0u8; 32];
    r.read_at(gd_off, &mut gd)?;
    let inode_table = rd_u32(&gd, 8) as u64;
    let off = inode_table * sb.block_size as u64 + index as u64 * sb.inode_size as u64;
    let mut inode = [0u8; 256];
    let n = (sb.inode_size as usize).min(256);
    r.read_at(off, &mut inode[..n])?;
    Ok(inode)
}

/// Read inode `ino` and decode the metadata a directory listing reports.
///
/// `i_size` is the 32-bit `i_size_lo` plus `i_size_hi` (offset 108) when the inode is
/// larger than the 128-byte base — for a directory that high half is `i_dir_acl`, so it
/// is taken only for non-directories. `i_mtime` (offset 16) is a 32-bit epoch second
/// count; `i_mtime_extra` carries two epoch-extension bits above it, which is what keeps
/// timestamps correct past 2038.
fn stat_inode<R: BlockReader>(r: &R, sb: &Superblock, ino: u32) -> Result<InodeStat, FsError> {
    let inode = read_inode(r, sb, ino)?;
    let mode = rd_u16(&inode, 0);
    let large = sb.inode_size > 128;
    let size_hi = if large && mode & S_IFMT != S_IFDIR {
        rd_u32(&inode, 108) as u64
    } else {
        0
    };
    let size = (rd_u32(&inode, 4) as u64) | (size_hi << 32);
    let extra = if large { rd_u32(&inode, I_MTIME_EXTRA) } else { 0 };
    let mtime = decode_time(rd_u32(&inode, I_MTIME), extra);
    Ok(InodeStat { ino, mode, size, mtime })
}

/// Map an inode's logical block `logical` to a physical block by walking its
/// extent tree. `node` starts at an extent header (the inode's `i_block`, or a
/// child extent block). Returns `0` for a hole (sparse).
fn extent_find<R: BlockReader>(
    r: &R,
    sb: &Superblock,
    node: &[u8],
    logical: u64,
) -> Result<u64, FsError> {
    if node.len() < 12 || rd_u16(node, 0) != EXTENT_MAGIC {
        return Err(FsError::Corrupt);
    }
    let entries = rd_u16(node, 2) as usize;
    let depth = rd_u16(node, 6);
    if 12 + entries * 12 > node.len() {
        return Err(FsError::Corrupt);
    }
    if depth == 0 {
        for i in 0..entries {
            let e = 12 + i * 12;
            let ee_block = rd_u32(node, e) as u64;
            let ee_len = (rd_u16(node, e + 4) & 0x7FFF) as u64; // high bit = uninitialised
            let phys = rd_u32(node, e + 8) as u64 | ((rd_u16(node, e + 6) as u64) << 32);
            if logical >= ee_block && logical < ee_block + ee_len {
                return Ok(phys + (logical - ee_block));
            }
        }
        Ok(0)
    } else {
        // Index node: pick the last child whose key ≤ logical, read it, recurse.
        let mut leaf: Option<u64> = None;
        for i in 0..entries {
            let e = 12 + i * 12;
            let ei_block = rd_u32(node, e) as u64;
            if logical >= ei_block {
                leaf = Some(rd_u32(node, e + 4) as u64 | ((rd_u16(node, e + 8) as u64) << 32));
            } else {
                break;
            }
        }
        let leaf = leaf.ok_or(FsError::Corrupt)?;
        let bs = sb.block_size as usize;
        let mut buf = [0u8; MAX_BLOCK];
        r.read_at(leaf * sb.block_size as u64, &mut buf[..bs])?;
        extent_find(r, sb, &buf[..bs], logical)
    }
}

/// Find `name` in directory inode `dir`, returning its inode number.
fn dir_lookup<R: BlockReader>(
    r: &R,
    sb: &Superblock,
    dir: &[u8; 256],
    name: &[u8],
) -> Result<u32, FsError> {
    let size = rd_u32(dir, 4) as u64;
    let bs = sb.block_size as usize;
    let nblocks = size.div_ceil(sb.block_size as u64);
    let mut buf = [0u8; MAX_BLOCK];
    for lb in 0..nblocks {
        let phys = extent_find(r, sb, &dir[40..100], lb)?;
        if phys == 0 {
            continue;
        }
        r.read_at(phys * sb.block_size as u64, &mut buf[..bs])?;
        let mut off = 0;
        while off + 8 <= bs {
            let e_ino = rd_u32(&buf, off);
            let rec_len = rd_u16(&buf, off + 4) as usize;
            let name_len = buf[off + 6] as usize;
            if rec_len < 8 || off + rec_len > bs {
                break; // malformed / end of block
            }
            if e_ino != 0 && name_len > 0 && off + 8 + name_len <= bs {
                if &buf[off + 8..off + 8 + name_len] == name {
                    return Ok(e_ino);
                }
            }
            off += rec_len;
        }
    }
    Err(FsError::NotFound)
}

/// Resolve an absolute path to `(inode_number, inode_bytes)`, walking directories
/// from the root inode.
fn resolve_path<R: BlockReader>(
    r: &R,
    sb: &Superblock,
    path: &[u8],
) -> Result<[u8; 256], FsError> {
    let mut inode = read_inode(r, sb, ROOT_INO)?;
    for comp in path.split(|&c| c == b'/').filter(|c| !c.is_empty()) {
        if rd_u16(&inode, 0) & S_IFMT != S_IFDIR {
            return Err(FsError::NotFound); // a path component is not a directory
        }
        let ino = dir_lookup(r, sb, &inode, comp)?;
        inode = read_inode(r, sb, ino)?;
    }
    Ok(inode)
}

/// Resolve `path` (absolute) to a **regular extent file**, returning its inode
/// bytes and exact size. Errors: `NotFound` (missing path / not a regular file),
/// `Unsupported` (non-extent or inline-data inode), `Corrupt` / `Io`.
fn resolve_regular_file<R: BlockReader>(
    r: &R,
    sb: &Superblock,
    path: &[u8],
) -> Result<([u8; 256], usize), FsError> {
    let inode = resolve_path(r, sb, path)?;
    if rd_u16(&inode, 0) & S_IFMT != S_IFREG {
        return Err(FsError::NotFound);
    }
    let flags = rd_u32(&inode, 32);
    if flags & EXTENTS_FL == 0 || flags & INLINE_DATA_FL != 0 {
        return Err(FsError::Unsupported);
    }
    let size_hi = if sb.inode_size > 128 { rd_u32(&inode, 108) as u64 } else { 0 };
    let size = ((rd_u32(&inode, 4) as u64) | (size_hi << 32)) as usize;
    Ok((inode, size))
}

/// Resolve `path` (absolute) to a regular file and return its **size** without
/// reading any content — the size the kernel's lazy resolve needs to build the
/// page-cache object. No [`MAX_FILE`] cap (lazy faulting handles large files).
/// Errors as [`resolve_regular_file`].
pub fn stat_file<R: BlockReader>(r: &R, path: &[u8]) -> Result<usize, FsError> {
    let sb = read_superblock(r)?;
    let (_, size) = resolve_regular_file(r, &sb, path)?;
    Ok(size)
}

/// ext4 `ext4_dir_entry_2.file_type` values (the ones we surface). Others map to
/// [`EXT4_FT_UNKNOWN`].
pub const EXT4_FT_UNKNOWN: u8 = 0;
/// A directory.
pub const EXT4_FT_DIR: u8 = 2;
/// A symbolic link.
pub const EXT4_FT_SYMLINK: u8 = 7;

/// Resolve `path` (absolute) to a **directory** inode number, for binding a directory
/// handle (`RESOLVE_DIR_OPEN`). Errors `NotFound` if the path is missing or is not a
/// directory; the caller then enumerates it by inode via [`read_dir`], addressing entries
/// by name — so the handle can never reach outside this directory.
pub fn resolve_dir<R: BlockReader>(r: &R, path: &[u8]) -> Result<u32, FsError> {
    let sb = read_superblock(r)?;
    let (ino, inode) = resolve_path_ino(r, &sb, path)?;
    if rd_u16(&inode, 0) & S_IFMT != S_IFDIR {
        return Err(FsError::NotFound);
    }
    Ok(ino)
}

/// One directory entry's inode metadata, resolved by [`read_dir_stat`].
///
/// `mtime` is seconds since the Unix epoch, decoded from `i_mtime` plus the low two bits
/// of `i_mtime_extra` (the post-2038 epoch extension) when the inode is large enough to
/// carry it — ext4's own encoding, not an approximation of it.
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

/// Enumerate directory inode `dir_ino` starting at the opaque `cursor` (a logical byte
/// offset into the directory's data; `0` starts from the beginning), calling `emit(inode,
/// file_type, name)` for each live entry. `emit` returns `false` to stop early (the reply
/// buffer is full); enumeration then resumes from the returned cursor. Returns the
/// **next cursor** — `0` once every entry has been emitted.
///
/// `.` and `..` are included (they are real directory entries; the caller decides whether
/// to show them). Entries never span a block; a block's slack rides in the last entry's
/// `rec_len`, so iteration steps by `rec_len` and rounds up to the next block at its end.
///
/// This form reads **only the directory's own blocks**. Callers that need each entry's
/// size/mtime/mode want [`read_dir_stat`], which additionally reads one inode per entry;
/// a caller that only needs names (an emptiness scan) should stay here and not pay for it.
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

/// [`read_dir`], plus each entry's [`InodeStat`] — the listing form (`File::ReadDir`),
/// which reports size/kind/modified per entry. Costs one extra inode read per entry
/// (the inode table is not cached; entries of one directory usually share a few blocks).
///
/// # Errors
/// As [`read_dir`]. An entry whose inode cannot be read is emitted with a **default**
/// (zeroed) `InodeStat` rather than failing the whole listing — one damaged inode must
/// not make a directory unlistable.
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
    if rd_u16(&inode, 0) & S_IFMT != S_IFDIR {
        return Err(FsError::NotFound);
    }
    let size = rd_u32(&inode, 4) as u64;
    let bs = sb.block_size as u64;
    let mut pos = cursor;
    let mut buf = [0u8; MAX_BLOCK];
    let mut loaded_block = u64::MAX; // which logical block `buf` currently holds

    while pos < size {
        let lb = pos / bs;
        let in_block = (pos % bs) as usize;
        if loaded_block != lb {
            let phys = extent_find(r, &sb, &inode[40..100], lb)?;
            if phys == 0 {
                // Sparse directory block (no live entries) — skip to the next block.
                pos = (lb + 1) * bs;
                continue;
            }
            r.read_at(phys * bs, &mut buf[..bs as usize])?;
            loaded_block = lb;
        }
        // Bounds: an entry needs at least its 8-byte header within the block.
        if in_block + 8 > bs as usize {
            pos = (lb + 1) * bs;
            continue;
        }
        let e_ino = rd_u32(&buf, in_block);
        let rec_len = rd_u16(&buf, in_block + 4) as usize;
        let name_len = buf[in_block + 6] as usize;
        let file_type = buf[in_block + 7];
        if rec_len < 8 || in_block + rec_len > bs as usize {
            // Malformed / no valid tail in this block — advance to the next block.
            pos = (lb + 1) * bs;
            continue;
        }
        // A live entry has a non-zero inode and a name that fits; `e_ino == 0` marks a
        // deleted/gap slot (skipped, its `rec_len` still consumed).
        if e_ino != 0 && name_len > 0 && in_block + 8 + name_len <= bs as usize {
            let name = &buf[in_block + 8..in_block + 8 + name_len];
            // The entry's inode metadata, if the caller asked for it. A single bad inode
            // degrades to a zeroed stat rather than failing the listing. This reads into
            // its own small buffers, so the cached directory block in `buf` survives.
            let stat = if want_stat {
                stat_inode(r, &sb, e_ino).unwrap_or_default()
            } else {
                InodeStat::default()
            };
            if !emit(e_ino, file_type, name, &stat) {
                // Buffer full: resume *at this same entry* next call.
                return Ok(pos);
            }
        }
        pos += rec_len as u64;
    }
    Ok(0)
}

/// Read the byte range `[offset, offset + len)` of the regular file at `path` into
/// `out`, returning the number of bytes read — the page-cache fill (`File::ReadRange`)
/// primitive. The range is clamped to the file size and `out.len()`; a request past
/// end-of-file returns `0`. No [`MAX_FILE`] cap (the caller bounds `len` to a page).
/// Sparse holes read as zero. Errors as [`resolve_regular_file`] / `Io` / `Corrupt`.
pub fn read_file_range<R: BlockReader>(
    r: &R,
    path: &[u8],
    offset: u64,
    len: usize,
    out: &mut [u8],
) -> Result<usize, FsError> {
    let sb = read_superblock(r)?;
    let (inode, size) = resolve_regular_file(r, &sb, path)?;
    if offset >= size as u64 {
        return Ok(0);
    }
    let avail = (size as u64 - offset) as usize;
    let want = len.min(avail).min(out.len());
    let bs = sb.block_size as usize;
    let mut buf = [0u8; MAX_BLOCK];
    let mut done = 0;
    while done < want {
        let pos = offset as usize + done; // absolute file byte position
        let lb = (pos / bs) as u64; // logical block
        let in_block = pos % bs; // byte offset within that block
        let n = (bs - in_block).min(want - done);
        let phys = extent_find(r, &sb, &inode[40..100], lb)?;
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

/// Map the logical block range `[start_block, start_block + count)` of the regular file at
/// `path` to device block runs (the **Model A** data path), writing them into `out` and
/// returning the run count. Runs coalesce contiguous mappings — and contiguous holes
/// (`device_lba = 0`). The range is clamped to the file's block count; blocks past EOF are
/// omitted. Bounded by `out.len()` (a short return means re-request from the first
/// uncovered block). Errors as [`resolve_regular_file`] / `Io` / `Corrupt`.
pub fn map_range<R: BlockReader>(
    r: &R,
    path: &[u8],
    start_block: u64,
    count: u64,
    out: &mut [crate::BlockRun],
) -> Result<usize, FsError> {
    let sb = read_superblock(r)?;
    let (inode, size) = resolve_regular_file(r, &sb, path)?;
    let bs = sb.block_size as u64;
    let file_blocks = size.div_ceil(bs as usize) as u64;
    let hdr = &inode[40..100];
    let end = start_block.saturating_add(count).min(file_blocks);
    let mut n = 0;
    let mut lb = start_block;
    while lb < end && n < out.len() {
        let phys = extent_find(r, &sb, hdr, lb)?;
        // Extend the run while the mapping stays contiguous (a hole extends over holes).
        let mut len = 1u64;
        while lb + len < end {
            let next = extent_find(r, &sb, hdr, lb + len)?;
            let contiguous = if phys == 0 { next == 0 } else { next == phys + len };
            if !contiguous {
                break;
            }
            len += 1;
        }
        out[n] = crate::BlockRun { file_block: lb, device_lba: phys, length: len as u32, flags: 0 };
        n += 1;
        lb += len;
    }
    Ok(n)
}

/// Resolve `path` to a regular file and map its **entire** block range to device runs (the
/// **Model A** resolve): returns `(size, block_size, run_count)` with the runs in `out`.
/// Coalesces contiguous runs. `Err(TooLarge)` if the file needs more runs than `out` holds
/// (too fragmented to inline in a resolve reply — the standalone `MapRange` op handles that,
/// deferred). Errors otherwise as [`resolve_regular_file`].
pub fn map_file<R: BlockReader>(
    r: &R,
    path: &[u8],
    out: &mut [crate::BlockRun],
) -> Result<(usize, u32, usize), FsError> {
    let sb = read_superblock(r)?;
    let (inode, size) = resolve_regular_file(r, &sb, path)?;
    let bs = sb.block_size;
    let file_blocks = size.div_ceil(bs as usize) as u64;
    let hdr = &inode[40..100];
    let mut n = 0;
    let mut lb = 0u64;
    while lb < file_blocks {
        if n >= out.len() {
            return Err(FsError::TooLarge); // too fragmented to inline in the resolve reply
        }
        let phys = extent_find(r, &sb, hdr, lb)?;
        let mut len = 1u64;
        while lb + len < file_blocks {
            let next = extent_find(r, &sb, hdr, lb + len)?;
            let contiguous = if phys == 0 { next == 0 } else { next == phys + len };
            if !contiguous {
                break;
            }
            len += 1;
        }
        out[n] = crate::BlockRun { file_block: lb, device_lba: phys, length: len as u32, flags: 0 };
        n += 1;
        lb += len;
    }
    Ok((size, bs, n))
}

// --- write path: block allocation + file growth (Part D) --------------------

/// A bitmap bit is clear (the block/inode is free).
fn bit_clear(map: &[u8], i: usize) -> bool {
    map[i / 8] & (1 << (i % 8)) == 0
}
/// Set a bitmap bit (mark allocated).
fn bit_set(map: &mut [u8], i: usize) {
    map[i / 8] |= 1 << (i % 8);
}

/// Resolve a path to `(inode_number, inode_bytes)` — like [`resolve_path`] but keeps the
/// number (the write path needs it to locate the inode on disk for write-back).
fn resolve_path_ino<R: BlockReader>(
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

/// The absolute device byte offset of inode `ino` (for writing it back).
/// Stamp `path`'s modification time as `now` — the `File::Touch` entry point.
///
/// The one filesystem mutation that changes **no** content and no structure. It exists
/// because Model A puts the kernel, not this server, on the file-data path: a same-length
/// in-place overwrite reaches the device without any resolve, so nothing here would
/// otherwise learn the file changed and `mtime` would keep reporting the last *size*
/// change. The kernel sends this after flushing such a write.
///
/// `now` comes from the server's own clock reading, never from the wire — a writer does not
/// get to choose what time it wrote.
pub fn touch_path<RW: BlockReader + BlockWriter>(
    rw: &RW,
    path: &[u8],
    now: i64,
) -> Result<(), FsError> {
    let sb = read_superblock(rw)?;
    let (ino, _) = resolve_path_ino(rw, &sb, path)?;
    touch_inode(rw, &sb, ino, now, Stamp::Modified)
}

/// Re-stamp an existing inode in place — read it, write its timestamps, write it
/// back.
///
/// Used for the *containing directory* after a link, unlink, or rename: a
/// directory's contents changed, so its mtime and ctime move even though nothing
/// about the entries' own inodes did. Callers that are already holding a modified
/// inode buffer stamp it directly instead.
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

fn inode_offset<R: BlockReader>(r: &R, sb: &Superblock, ino: u32) -> Result<u64, FsError> {
    let group = (ino - 1) / sb.inodes_per_group;
    let index = (ino - 1) % sb.inodes_per_group;
    let gd_off = sb.first_gdt_block * sb.block_size as u64 + group as u64 * sb.desc_size as u64;
    let mut gd = [0u8; 32];
    r.read_at(gd_off, &mut gd)?;
    let inode_table = rd_u32(&gd, 8) as u64;
    Ok(inode_table * sb.block_size as u64 + index as u64 * sb.inode_size as u64)
}

/// Allocate one free filesystem block, preferring `goal` (for contiguity). Reads the goal
/// block's group bitmap, sets a free bit (goal if free, else the first free bit in that
/// group), and updates the group-descriptor + superblock free-block counts. Returns the
/// allocated block number. `TooLarge` if the group is full (cross-group allocation is a
/// later refinement). `metadata_csum` is off (fixtures), so no bitmap/desc checksums.
fn alloc_block<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    goal: u64,
) -> Result<u64, FsError> {
    let bs = sb.block_size as usize;
    let group = ((goal - sb.first_data_block as u64) / sb.blocks_per_group as u64) as u32;
    let group_start = sb.first_data_block as u64 + group as u64 * sb.blocks_per_group as u64;
    let gd_off = sb.first_gdt_block * sb.block_size as u64 + group as u64 * sb.desc_size as u64;
    let mut gd = [0u8; 64];
    let dsz = (sb.desc_size as usize).min(64);
    rw.read_at(gd_off, &mut gd[..dsz])?;
    let bitmap_block = rd_u32(&gd, 0) as u64; // bg_block_bitmap_lo

    let mut bitmap = [0u8; MAX_BLOCK];
    rw.read_at(bitmap_block * sb.block_size as u64, &mut bitmap[..bs])?;

    let goal_idx = (goal - group_start) as usize;
    let idx = if goal_idx < sb.blocks_per_group as usize && bit_clear(&bitmap, goal_idx) {
        goal_idx
    } else {
        (0..sb.blocks_per_group as usize)
            .find(|&i| bit_clear(&bitmap, i))
            .ok_or(FsError::TooLarge)?
    };
    bit_set(&mut bitmap, idx);
    rw.write_at(bitmap_block * sb.block_size as u64, &bitmap[..bs])?;

    // Decrement free-block counts: group descriptor (bg_free_blocks_count_lo @12, u16) and
    // superblock (s_free_blocks_count_lo @12, u32).
    let gfree = rd_u16(&gd, 12).wrapping_sub(1);
    gd[12..14].copy_from_slice(&gfree.to_le_bytes());
    rw.write_at(gd_off, &gd[..dsz])?;
    let mut sbbuf = [0u8; 1024];
    rw.read_at(1024, &mut sbbuf)?;
    let sfree = rd_u32(&sbbuf, 12).wrapping_sub(1);
    sbbuf[12..16].copy_from_slice(&sfree.to_le_bytes());
    rw.write_at(1024, &sbbuf)?;

    Ok(group_start + idx as u64)
}

/// Grow the regular file at `path` to `new_size` bytes by allocating blocks and extending
/// its extent tree in place, updating the inode size + block count. Only **grows** (a
/// `new_size <= cur_size` is a no-op). Depth-0 extent trees only (small files); a new extent
/// is added only if the inline `i_block` header has room — otherwise `Unsupported` (extent-
/// tree splitting / index nodes are deferred). Returns the new size. Metadata is written via
/// the `BlockWriter`. See `docs/architecture/ext4-fs-server-rw.md`.
pub fn grow_file<RW: BlockReader + BlockWriter>(
    rw: &RW,
    path: &[u8],
    new_size: usize,
    now: i64,
) -> Result<usize, FsError> {
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
        return Ok(cur_size);
    }
    let cur_blocks = cur_size.div_ceil(bs);
    let new_blocks = new_size.div_ceil(bs);

    for lb in cur_blocks..new_blocks {
        append_block(rw, &sb, &mut inode, lb as u64)?;
    }

    // Update inode size (i_size_lo @4, hi @108) + block count (i_blocks_lo @28, 512-B units).
    inode[4..8].copy_from_slice(&(new_size as u32).to_le_bytes());
    if sb.inode_size > 128 {
        inode[108..112].copy_from_slice(&((new_size as u64 >> 32) as u32).to_le_bytes());
    }
    let added_sectors = ((new_blocks - cur_blocks) * bs / 512) as u32;
    let i_blocks = rd_u32(&inode, 28).wrapping_add(added_sectors);
    inode[28..32].copy_from_slice(&i_blocks.to_le_bytes());
    // A size change is a content change.
    stamp(&mut inode, now, sb.inode_size, Stamp::Modified);

    let off = inode_offset(rw, &sb, ino)?;
    rw.write_at(off, &inode[..(sb.inode_size as usize).min(256)])?;
    Ok(new_size)
}

/// Round `n` up to the 4-byte alignment ext4 directory entries use.
fn round4(n: usize) -> usize {
    (n + 3) & !3
}

/// Allocate one free inode from **group 0** (small fixtures keep everything there;
/// cross-group is deferred, as with [`alloc_block`]). Sets the inode-bitmap bit and
/// decrements the group-descriptor + superblock free-inode counts. Returns the inode
/// number. `TooLarge` if group 0's inodes are exhausted.
fn alloc_inode<RW: BlockReader + BlockWriter>(rw: &RW, sb: &Superblock) -> Result<u32, FsError> {
    let bs = sb.block_size as usize;
    let gd_off = sb.first_gdt_block * sb.block_size as u64; // group 0 descriptor
    let mut gd = [0u8; 64];
    let dsz = (sb.desc_size as usize).min(64);
    rw.read_at(gd_off, &mut gd[..dsz])?;
    let ibitmap_block = rd_u32(&gd, 4) as u64; // bg_inode_bitmap_lo

    let mut bitmap = [0u8; MAX_BLOCK];
    rw.read_at(ibitmap_block * sb.block_size as u64, &mut bitmap[..bs])?;
    let idx = (0..sb.inodes_per_group as usize)
        .find(|&i| bit_clear(&bitmap, i))
        .ok_or(FsError::TooLarge)?;
    bit_set(&mut bitmap, idx);
    rw.write_at(ibitmap_block * sb.block_size as u64, &bitmap[..bs])?;

    // Free-inode counts: group descriptor (bg_free_inodes_count_lo @14, u16) + superblock
    // (s_free_inodes_count @16, u32).
    let gfree = rd_u16(&gd, 14).wrapping_sub(1);
    gd[14..16].copy_from_slice(&gfree.to_le_bytes());
    rw.write_at(gd_off, &gd[..dsz])?;
    let mut sbbuf = [0u8; 1024];
    rw.read_at(1024, &mut sbbuf)?;
    let sfree = rd_u32(&sbbuf, 16).wrapping_sub(1);
    sbbuf[16..20].copy_from_slice(&sfree.to_le_bytes());
    rw.write_at(1024, &sbbuf)?;

    Ok(idx as u32 + 1) // inode numbers are 1-based; group 0
}

/// Insert a directory entry `(name → ino, file_type)` into directory `dir_inode` by
/// splitting the slack of an existing entry (the last entry in a block carries the free
/// tail as extra `rec_len`). `TooLarge` if no block has room (allocating a new directory
/// block is deferred). Writes the modified block via the `BlockWriter`.
/// Insert `name` → `ino` into directory `dir_ino`, **growing the directory by a block when
/// every existing one is full**.
///
/// `dir_inode` is the caller's copy of the directory inode and is updated in place (extent
/// tree, size, block count) and written back **only if the directory grew** — the common
/// path finds slack in an existing block and touches no inode field at all.
///
/// ext4 stores a directory as a list of blocks, each a self-contained chain of
/// `ext4_dir_entry_2` records whose `rec_len` covers its own slack; an insert either splits
/// a record with enough spare room or claims a deleted slot. When neither exists in any
/// block, a new block is appended and formatted as one free record spanning it — which is
/// exactly what a block looks like after everything in it is deleted, so nothing
/// downstream needs to know the difference.
///
/// ## The remaining ceiling, measured
///
/// Growth stops where the **inline extent header** does: `i_block` holds four leaf
/// extents, and depth > 0 (an index node) is deferred. Whether that bites depends on
/// fragmentation, and the two cases differ sharply — measured on a 4 KiB-block fixture:
///
/// - **Creating files**: unbounded in practice (2000+ tested). Nothing allocates between
///   the parent's growth blocks, so they are contiguous and one extent covers them all.
/// - **Creating subdirectories**: **~814**. Each `mkdir` allocates the child's own data
///   block *between* the parent's, so every parent block starts a new extent and the
///   fourth exhausts the header.
///
/// Both are far beyond anything on the path to a shell or a desktop, so the extent-tree
/// split stays deferred — but the number is recorded rather than left as "some limit".
/// See `deferred-decisions.md`.
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
    for lb in 0..nblocks {
        let phys = extent_find(rw, sb, &dir_inode[40..100], lb)?;
        if phys == 0 {
            continue;
        }
        rw.read_at(phys * sb.block_size as u64, &mut buf[..bs])?;
        let mut off = 0;
        while off + 8 <= bs {
            let e_ino = rd_u32(&buf, off);
            let rec_len = rd_u16(&buf, off + 4) as usize;
            let e_name_len = buf[off + 6] as usize;
            if rec_len < 8 || off + rec_len > bs {
                break; // malformed / end of block
            }
            // Space this entry actually needs (0 for a deleted slot, `ino == 0`).
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

    // Every block is full: append one and put the entry at its head.
    let phys = append_block(rw, sb, dir_inode, nblocks)?;
    // A fresh directory block is a single **unused** record spanning it: inode 0,
    // `rec_len` = block size. Zero the rest so no stale bytes are read as entries.
    buf[..bs].fill(0);
    buf[4..6].copy_from_slice(&(bs as u16).to_le_bytes()); // rec_len covers the block
    buf[..4].copy_from_slice(&ino.to_le_bytes());
    buf[6] = name.len() as u8;
    buf[7] = file_type;
    buf[8..8 + name.len()].copy_from_slice(name);
    rw.write_at(phys * sb.block_size as u64, &buf[..bs])?;

    // The directory is one block longer. `i_size` for a directory is always a whole
    // number of blocks, and `i_blocks` counts 512-byte units.
    let new_size = size + bs as u64;
    dir_inode[4..8].copy_from_slice(&(new_size as u32).to_le_bytes());
    let i_blocks = rd_u32(dir_inode, 28).wrapping_add((bs / 512) as u32);
    dir_inode[28..32].copy_from_slice(&i_blocks.to_le_bytes());
    stamp(dir_inode, now, sb.inode_size, Stamp::Modified);
    let off = inode_offset(rw, sb, dir_ino)?;
    rw.write_at(off, &dir_inode[..(sb.inode_size as usize).min(256)])?;
    Ok(())
}

/// Create an empty regular file `name` in the directory at `parent_path`: allocate + init
/// an inode (regular, empty extent tree, size 0) and link it into the parent directory.
/// Idempotent — if `name` already exists, returns its inode. The caller grows + writes the
/// file afterwards (metadata-only here). `NotFound` if the parent is not a directory.
/// Depth-0 dirs with slack only (a new directory block is deferred). See
/// `docs/architecture/ext4-fs-server-rw.md`.
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
    // Extent header at i_block (offset 40): magic, 0 entries, max 4, depth 0.
    inode[40..42].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
    inode[44..46].copy_from_slice(&4u16.to_le_bytes()); // eh_max = (60 - 12) / 12
    let off = inode_offset(rw, &sb, ino)?;
    rw.write_at(off, &inode[..(sb.inode_size as usize).min(256)])?;

    // Link it into the parent directory, growing it by a block if it is full. (On
    // failure the inode is allocated-but-unlinked — acceptable here; a full
    // orphan-reclaim pass is a separate concern.)
    let (parent_ino, mut parent_inode) = resolve_path_ino(rw, &sb, parent_path)?;
    dir_insert(rw, &sb, parent_ino, &mut parent_inode, name, ino, EXT4_FT_REG_FILE, now)?;
    // The parent's contents changed, so its own mtime/ctime move even though nothing
    // about its inode's other fields did.
    touch_inode(rw, &sb, parent_ino, now, Stamp::Modified)?;
    Ok(ino)
}

/// A directory's default mode: `S_IFDIR | 0o755`.
const DIR_MODE: u16 = S_IFDIR | 0o755;

/// Fixed `i_dtime` stamp for a freed inode (2023-11-14T22:13:20Z). The server has no wall
/// clock; the value only needs to be a plausible timestamp (not a small inode-number-like
/// value that `e2fsck` would read as an orphan-list link).
///
/// Retained as the **fallback** for a machine whose wall clock could not be anchored: the
/// server now stamps `i_dtime` with the real deletion time when it has one.
const DELETION_TIME: u32 = 1_700_000_000;

/// Clear a bitmap bit (mark a block/inode free).
fn bit_unset(map: &mut [u8], i: usize) {
    map[i / 8] &= !(1 << (i % 8));
}

/// Allocate one block and attach it to `inode` as logical block `next_lb`, extending the
/// last extent when the allocation happens to be contiguous and adding a leaf otherwise.
/// Returns the physical block.
///
/// Shared by [`grow_file`] (appending file data) and [`dir_insert`] (appending a directory
/// block when every existing one is full) — the extent bookkeeping is identical, and the
/// duplicate was what made "grow a full directory" look like a bigger job than it is.
///
/// The caller owns `inode`: this mutates the in-memory copy (extent tree only) and writes
/// nothing back, because both callers have further inode fields to update — size, block
/// count, timestamps — and one write is better than three.
fn append_block<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    inode: &mut [u8; 256],
    next_lb: u64,
) -> Result<u64, FsError> {
    let eh = 40; // extent header at i_block
    if rd_u16(inode, eh) != EXTENT_MAGIC {
        return Err(FsError::Corrupt);
    }
    if rd_u16(inode, eh + 6) != 0 {
        return Err(FsError::Unsupported); // index nodes (depth > 0) are deferred
    }
    let mut entries = rd_u16(inode, eh + 2) as usize;
    let max_entries = rd_u16(inode, eh + 4) as usize;
    let ent = |i: usize| eh + 12 + i * 12;

    // The append point: the end of the last extent, logical and physical.
    let (last_log_end, last_phys_end) = if entries == 0 {
        (0u64, 0u64)
    } else {
        let e = ent(entries - 1);
        let ee_block = rd_u32(inode, e) as u64;
        let ee_len = (rd_u16(inode, e + 4) & 0x7FFF) as u64;
        let phys = rd_u32(inode, e + 8) as u64 | ((rd_u16(inode, e + 6) as u64) << 32);
        (ee_block + ee_len, phys + ee_len)
    };

    let goal = if last_phys_end != 0 { last_phys_end } else { sb.first_data_block as u64 };
    let phys = alloc_block(rw, sb, goal)?;
    if entries > 0 && next_lb == last_log_end && phys == last_phys_end {
        // Contiguous with the last extent — just lengthen it.
        let e = ent(entries - 1);
        let new_len = (rd_u16(inode, e + 4) & 0x7FFF) + 1;
        inode[e + 4..e + 6].copy_from_slice(&new_len.to_le_bytes());
    } else {
        if entries >= max_entries {
            return Err(FsError::Unsupported); // needs a tree split (deferred)
        }
        let e = ent(entries);
        inode[e..e + 4].copy_from_slice(&(next_lb as u32).to_le_bytes()); // ee_block
        inode[e + 4..e + 6].copy_from_slice(&1u16.to_le_bytes()); // ee_len
        inode[e + 6..e + 8].copy_from_slice(&((phys >> 32) as u16).to_le_bytes()); // start_hi
        inode[e + 8..e + 12].copy_from_slice(&(phys as u32).to_le_bytes()); // start_lo
        entries += 1;
        inode[eh + 2..eh + 4].copy_from_slice(&(entries as u16).to_le_bytes()); // eh_entries
    }
    Ok(phys)
}

/// Free inode `ino`: clear its inode-bitmap bit and increment the group-descriptor +
/// superblock free-inode counts. The inverse of [`alloc_inode`], generic over the inode's
/// group. If `is_dir`, also decrements the group's `bg_used_dirs_count`.
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
    rw.read_at(gd_off, &mut gd[..dsz])?;
    let ibitmap_block = rd_u32(&gd, 4) as u64;

    // Mark the inode itself deleted: `e2fsck` decides an inode is in use from its
    // `i_links_count` + `i_dtime`, *not* the bitmap. Clearing only the bitmap leaves a live
    // inode (links > 0, dtime = 0) whose bitmap says free — an inconsistency. Zero
    // `i_links_count` (@26) and stamp a nonzero `i_dtime` (@20); the value only has to be
    // nonzero for `-fn` (no wall clock in the server), so use a fixed sentinel.
    let ioff = inode_offset(rw, sb, ino)?;
    let mut inode = [0u8; 256];
    let n = (sb.inode_size as usize).min(256);
    rw.read_at(ioff, &mut inode[..n])?;
    // `i_dtime` doubles as the orphan-list "next" pointer while `i_links_count == 0`; a
    // small value looks like an inode number and `e2fsck` reads it as a corrupted orphan
    // chain. The real deletion time is both correct and unambiguously not an inode ref;
    // `DELETION_TIME` stands in when the machine has no anchored wall clock (`now == 0`).
    let dtime = if now > 0 { now as u32 } else { DELETION_TIME };
    inode[20..24].copy_from_slice(&dtime.to_le_bytes()); // i_dtime
    inode[26..28].copy_from_slice(&0u16.to_le_bytes()); // i_links_count = 0
    rw.write_at(ioff, &inode[..n])?;

    let mut bitmap = [0u8; MAX_BLOCK];
    rw.read_at(ibitmap_block * sb.block_size as u64, &mut bitmap[..bs])?;
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
    // Superblock free-inode count (@16, u32).
    let mut sbbuf = [0u8; 1024];
    rw.read_at(1024, &mut sbbuf)?;
    let sfree = rd_u32(&sbbuf, 16).wrapping_add(1);
    sbbuf[16..20].copy_from_slice(&sfree.to_le_bytes());
    rw.write_at(1024, &sbbuf)?;
    Ok(())
}

/// Free filesystem block `block`: clear its block-bitmap bit and increment the
/// group-descriptor + superblock free-block counts. The inverse of the allocation in
/// [`alloc_block`], generic over the block's group.
fn free_block<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    block: u64,
) -> Result<(), FsError> {
    let bs = sb.block_size as usize;
    let rel = block - sb.first_data_block as u64;
    let group = (rel / sb.blocks_per_group as u64) as u32;
    let index = (rel % sb.blocks_per_group as u64) as usize;
    let gd_off = sb.first_gdt_block * sb.block_size as u64 + group as u64 * sb.desc_size as u64;
    let dsz = (sb.desc_size as usize).min(64);
    let mut gd = [0u8; 64];
    rw.read_at(gd_off, &mut gd[..dsz])?;
    let bitmap_block = rd_u32(&gd, 0) as u64;

    let mut bitmap = [0u8; MAX_BLOCK];
    rw.read_at(bitmap_block * sb.block_size as u64, &mut bitmap[..bs])?;
    bit_unset(&mut bitmap, index);
    rw.write_at(bitmap_block * sb.block_size as u64, &bitmap[..bs])?;

    let gfree = rd_u16(&gd, 12).wrapping_add(1);
    gd[12..14].copy_from_slice(&gfree.to_le_bytes());
    rw.write_at(gd_off, &gd[..dsz])?;
    let mut sbbuf = [0u8; 1024];
    rw.read_at(1024, &mut sbbuf)?;
    let sfree = rd_u32(&sbbuf, 12).wrapping_add(1);
    sbbuf[12..16].copy_from_slice(&sfree.to_le_bytes());
    rw.write_at(1024, &sbbuf)?;
    Ok(())
}

/// Free every data block of the depth-0 extent inode `inode` (its regular-file or
/// directory data). Depth > 0 (index nodes) is `Unsupported`, as elsewhere in the write
/// path. Does not touch the inode itself.
fn free_inode_blocks<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    inode: &[u8; 256],
) -> Result<(), FsError> {
    let eh = 40;
    if rd_u16(inode, eh) != EXTENT_MAGIC {
        return Err(FsError::Corrupt);
    }
    if rd_u16(inode, eh + 6) != 0 {
        return Err(FsError::Unsupported); // index nodes deferred
    }
    let entries = rd_u16(inode, eh + 2) as usize;
    for i in 0..entries {
        let e = eh + 12 + i * 12;
        let ee_len = (rd_u16(inode, e + 4) & 0x7FFF) as u64;
        let phys = rd_u32(inode, e + 8) as u64 | ((rd_u16(inode, e + 6) as u64) << 32);
        for b in 0..ee_len {
            free_block(rw, sb, phys + b)?;
        }
    }
    Ok(())
}

/// Remove the directory entry `name` from directory-inode bytes `dir_inode`, returning the
/// removed entry's `(inode, file_type)`. Merges the entry's `rec_len` into the previous
/// entry in its block; if it is the first entry in a block, tombstones it (`e_ino = 0`).
/// Both forms are the standard ext2/3/4 removal and stay `e2fsck`-clean. `NotFound` if the
/// name is absent. The inverse of [`dir_insert`].
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
    for lb in 0..nblocks {
        let phys = extent_find(rw, sb, &dir_inode[40..100], lb)?;
        if phys == 0 {
            continue;
        }
        rw.read_at(phys * sb.block_size as u64, &mut buf[..bs])?;
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
            if e_ino != 0 && name_len == name.len() && off + 8 + name_len <= bs
                && &buf[off + 8..off + 8 + name_len] == name
            {
                match prev {
                    Some(p) => {
                        // Absorb this entry into the previous one's rec_len.
                        let prev_rec = rd_u16(&buf, p + 4) as usize + rec_len;
                        buf[p + 4..p + 6].copy_from_slice(&(prev_rec as u16).to_le_bytes());
                    }
                    None => {
                        // First entry in the block — tombstone it.
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

/// Adjust directory inode `ino`'s `i_links_count` by `delta` (+1 / -1), reading and writing
/// it back. Used when a subdirectory's `..` is created/removed (which links/unlinks the
/// parent).
fn adjust_links<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    ino: u32,
    delta: i32,
) -> Result<(), FsError> {
    let off = inode_offset(rw, sb, ino)?;
    let mut inode = [0u8; 256];
    let n = (sb.inode_size as usize).min(256);
    rw.read_at(off, &mut inode[..n])?;
    let links = (rd_u16(&inode, 26) as i32 + delta) as u16;
    inode[26..28].copy_from_slice(&links.to_le_bytes());
    rw.write_at(off, &inode[..n])?;
    Ok(())
}

/// Create a subdirectory `name` inside directory inode `dir_ino`: allocate a directory
/// inode + one data block, initialise the block with `.`/`..`, link it into the parent, and
/// bump the parent's link count (for the new dir's `..`). Name-addressed — the caller holds
/// an open handle to `dir_ino`, so `name` cannot escape it. `Exists` if `name` is taken;
/// `TooLarge` if the parent directory has no slack (a new parent block is deferred).
pub fn mkdir_at<RW: BlockReader + BlockWriter>(
    rw: &RW,
    dir_ino: u32,
    name: &[u8],
    now: i64,
) -> Result<(), FsError> {
    if name.is_empty() || name.len() > 255 || name.contains(&b'/') || name == b"." || name == b".."
    {
        return Err(FsError::Unsupported);
    }
    let sb = read_superblock(rw)?;
    let parent = read_inode(rw, &sb, dir_ino)?;
    if rd_u16(&parent, 0) & S_IFMT != S_IFDIR {
        return Err(FsError::NotFound);
    }
    if dir_lookup(rw, &sb, &parent, name).is_ok() {
        return Err(FsError::Exists);
    }
    let bs = sb.block_size as usize;

    let new_ino = alloc_inode(rw, &sb)?;
    let dblock = alloc_block(rw, &sb, sb.first_data_block as u64)?;

    // Initialise the new directory's data block: `.` → self, `..` → parent.
    let mut db = [0u8; MAX_BLOCK];
    // `.` at offset 0 (rec_len 12).
    db[0..4].copy_from_slice(&new_ino.to_le_bytes());
    db[4..6].copy_from_slice(&12u16.to_le_bytes());
    db[6] = 1;
    db[7] = EXT4_FT_DIR;
    db[8] = b'.';
    // `..` at offset 12 (rec_len fills the rest of the block).
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
    // Extent header (magic, 1 entry, max 4, depth 0) + one leaf extent → `dblock`.
    inode[40..42].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
    inode[42..44].copy_from_slice(&1u16.to_le_bytes()); // eh_entries
    inode[44..46].copy_from_slice(&4u16.to_le_bytes()); // eh_max
    inode[52..56].copy_from_slice(&0u32.to_le_bytes()); // ee_block = 0
    inode[56..58].copy_from_slice(&1u16.to_le_bytes()); // ee_len = 1
    inode[58..60].copy_from_slice(&((dblock >> 32) as u16).to_le_bytes()); // start_hi
    inode[60..64].copy_from_slice(&(dblock as u32).to_le_bytes()); // start_lo
    let ioff = inode_offset(rw, &sb, new_ino)?;
    rw.write_at(ioff, &inode[..(sb.inode_size as usize).min(256)])?;

    // `bg_used_dirs_count` (@16, u16) for the new inode's group (group 0 — alloc_inode).
    let gd_off = sb.first_gdt_block * sb.block_size as u64; // group 0
    let dsz = (sb.desc_size as usize).min(64);
    let mut gd = [0u8; 64];
    rw.read_at(gd_off, &mut gd[..dsz])?;
    let used = rd_u16(&gd, 16).wrapping_add(1);
    gd[16..18].copy_from_slice(&used.to_le_bytes());
    rw.write_at(gd_off, &gd[..dsz])?;

    // Link into the parent + bump the parent's link count (the new dir's `..`).
    let mut parent = parent;
    dir_insert(rw, &sb, dir_ino, &mut parent, name, new_ino, EXT4_FT_DIR, now)?;
    adjust_links(rw, &sb, dir_ino, 1)?;
    touch_inode(rw, &sb, dir_ino, now, Stamp::Modified)?;
    Ok(())
}

/// Remove the **regular file** `name` from directory inode `dir_ino`: unlink the directory
/// entry, decrement the target's link count, and — when it reaches zero — free the target's
/// data blocks and inode. Name-addressed. `NotFound` if absent; `Unsupported` if `name` is a
/// directory (use [`rmdir_at`]).
pub fn unlink_at<RW: BlockReader + BlockWriter>(
    rw: &RW,
    dir_ino: u32,
    name: &[u8],
    now: i64,
) -> Result<(), FsError> {
    let sb = read_superblock(rw)?;
    let parent = read_inode(rw, &sb, dir_ino)?;
    if rd_u16(&parent, 0) & S_IFMT != S_IFDIR {
        return Err(FsError::NotFound);
    }
    let target_ino = dir_lookup(rw, &sb, &parent, name)?;
    let target = read_inode(rw, &sb, target_ino)?;
    if rd_u16(&target, 0) & S_IFMT == S_IFDIR {
        return Err(FsError::Unsupported); // rmdir handles directories
    }

    dir_remove(rw, &sb, &parent, name)?;

    let links = rd_u16(&target, 26).wrapping_sub(1);
    if links == 0 {
        free_inode_blocks(rw, &sb, &target)?;
        free_inode(rw, &sb, target_ino, false, now)?;
    } else {
        let off = inode_offset(rw, &sb, target_ino)?;
        let mut t = target;
        t[26..28].copy_from_slice(&links.to_le_bytes());
        // A surviving hard link: the file's *contents* did not change, only its
        // link count — so ctime moves and mtime does not.
        stamp(&mut t, now, sb.inode_size, Stamp::MetadataOnly);
        rw.write_at(off, &t[..(sb.inode_size as usize).min(256)])?;
    }
    touch_inode(rw, &sb, dir_ino, now, Stamp::Modified)?;
    Ok(())
}

/// Remove the **empty subdirectory** `name` from directory inode `dir_ino`: verify it holds
/// only `.`/`..`, unlink it, free its data block + inode, and decrement the parent's link
/// count (the removed `..`). Name-addressed. `NotFound` if absent; `Unsupported` if `name`
/// is not a directory (use [`unlink_at`]); `NotEmpty` if it still has entries.
pub fn rmdir_at<RW: BlockReader + BlockWriter>(
    rw: &RW,
    dir_ino: u32,
    name: &[u8],
    now: i64,
) -> Result<(), FsError> {
    if name == b"." || name == b".." {
        return Err(FsError::Unsupported);
    }
    let sb = read_superblock(rw)?;
    let parent = read_inode(rw, &sb, dir_ino)?;
    if rd_u16(&parent, 0) & S_IFMT != S_IFDIR {
        return Err(FsError::NotFound);
    }
    let sub_ino = dir_lookup(rw, &sb, &parent, name)?;
    let sub = read_inode(rw, &sb, sub_ino)?;
    if rd_u16(&sub, 0) & S_IFMT != S_IFDIR {
        return Err(FsError::Unsupported);
    }
    // Empty iff its only entries are `.` and `..`.
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
    free_inode_blocks(rw, &sb, &sub)?;
    free_inode(rw, &sb, sub_ino, true, now)?;
    adjust_links(rw, &sb, dir_ino, -1)?; // the subdir's `..` no longer links the parent
    touch_inode(rw, &sb, dir_ino, now, Stamp::Modified)?;
    Ok(())
}

/// Point the existing entry `name` in `dir_inode` at a different inode, in place.
///
/// One block write, no record shuffling — which is what makes an overwriting rename's
/// crash window benign: after this, both the old and new names refer to the source inode
/// and the replaced inode is merely unreferenced (its link count still ≥ 1), so a crash
/// leaves work for `e2fsck` rather than a lost file. Removing the destination entry first
/// and re-inserting would open a window with *no* name for either.
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
    for lb in 0..nblocks {
        let phys = extent_find(rw, sb, &dir_inode[40..100], lb)?;
        if phys == 0 {
            continue;
        }
        rw.read_at(phys * sb.block_size as u64, &mut buf[..bs])?;
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

/// Split an absolute path into `(parent, leaf)` — `"/a/b/c"` → `("/a/b", "c")`.
fn split_parent(path: &[u8]) -> Option<(&[u8], &[u8])> {
    let slash = path.iter().rposition(|&c| c == b'/')?;
    let leaf = &path[slash + 1..];
    if leaf.is_empty() || leaf == b"." || leaf == b".." {
        return None;
    }
    Some((if slash == 0 { b"/" } else { &path[..slash] }, leaf))
}

/// Rename `old_path` to `new_path` **anywhere within this filesystem**, optionally
/// replacing an existing destination.
///
/// The path-addressed counterpart to [`rename_at`] (which is name-addressed within one
/// directory session). A cross-directory rename inherently names two directories, which a
/// session — bound to exactly one inode — cannot express; see the decision log
/// (2026-07-29).
///
/// Ordering is chosen so that a crash cannot lose the **source**, since no journal exists:
///
/// 1. Point the destination name at the source inode — repointing an existing entry when
///    replacing, otherwise inserting a new one.
/// 2. Remove the source's old entry.
/// 3. Release the replaced inode's link (freeing it if that was the last).
///
/// A crash between 1 and 2 leaves the file reachable under *both* names; between 2 and 3 it
/// leaves the replaced inode unreferenced with a positive link count. `e2fsck` repairs both
/// (the latter into `lost+found`), and neither loses the file being moved.
///
/// Moving a **directory** additionally repoints its `..` and shifts one link from the old
/// parent to the new. Replacing a directory is refused (`Unsupported`) — that needs the
/// emptiness check and link bookkeeping of `rmdir` folded in, and nothing needs it yet.
pub fn rename_path<RW: BlockReader + BlockWriter>(
    rw: &RW,
    old_path: &[u8],
    new_path: &[u8],
    replace: bool,
    now: i64,
) -> Result<(), FsError> {
    let sb = read_superblock(rw)?;
    let (old_parent_path, old_name) = split_parent(old_path).ok_or(FsError::Unsupported)?;
    let (new_parent_path, new_name) = split_parent(new_path).ok_or(FsError::Unsupported)?;
    let (old_dir_ino, old_dir) = resolve_path_ino(rw, &sb, old_parent_path)?;
    let (new_dir_ino, mut new_dir) = resolve_path_ino(rw, &sb, new_parent_path)?;
    if rd_u16(&old_dir, 0) & S_IFMT != S_IFDIR || rd_u16(&new_dir, 0) & S_IFMT != S_IFDIR {
        return Err(FsError::NotFound);
    }
    // A no-op rename must not unlink anything.
    if old_dir_ino == new_dir_ino && old_name == new_name {
        return Ok(());
    }

    let src_ino = dir_lookup(rw, &sb, &old_dir, old_name)?;
    let src = read_inode(rw, &sb, src_ino)?;
    let src_is_dir = rd_u16(&src, 0) & S_IFMT == S_IFDIR;
    let src_ft = if src_is_dir { EXT4_FT_DIR } else { EXT4_FT_REG_FILE };

    // Moving a directory into its own subtree would detach it from the tree entirely
    // (its `..` would point into the cycle). Cheap guard: refuse the parent-into-child
    // case, which is the reachable one — a full ancestor walk is deferred with the rest
    // of the deep-tree work.
    if src_is_dir && new_parent_path.starts_with(old_path) {
        return Err(FsError::Unsupported);
    }

    // Step 1: make the destination name refer to the source.
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

    // Step 2: drop the source's old name. Re-read the old parent — `dir_insert` may have
    // grown it (when both paths share a parent), which rewrote its inode.
    let (_, old_dir) = resolve_path_ino(rw, &sb, old_parent_path)?;
    dir_remove(rw, &sb, &old_dir, old_name)?;

    // Step 3: release the inode the destination name used to hold.
    if let Some(dest_ino) = replaced {
        let dest = read_inode(rw, &sb, dest_ino)?;
        let links = rd_u16(&dest, 26).wrapping_sub(1);
        if links == 0 {
            free_inode_blocks(rw, &sb, &dest)?;
            free_inode(rw, &sb, dest_ino, false, now)?;
        } else {
            let off = inode_offset(rw, &sb, dest_ino)?;
            let mut d = dest;
            d[26..28].copy_from_slice(&links.to_le_bytes());
            stamp(&mut d, now, sb.inode_size, Stamp::MetadataOnly);
            rw.write_at(off, &d[..(sb.inode_size as usize).min(256)])?;
        }
    }

    // A directory carries a link to its parent through `..`, so a move between parents
    // shifts one link and rewrites that entry.
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
    Ok(())
}

/// Rename `old` to `new` **within** directory inode `dir_ino` (the session's bound
/// directory): move the entry, preserving its target inode + type. `new` must not already
/// exist (overwrite is deferred). Cross-directory rename needs a second handle and is
/// deferred. Name-addressed. `NotFound` if `old` is absent; `Exists` if `new` is taken.
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
    if rd_u16(&parent, 0) & S_IFMT != S_IFDIR {
        return Err(FsError::NotFound);
    }
    if dir_lookup(rw, &sb, &parent, new).is_ok() {
        return Err(FsError::Exists);
    }
    // Remove `old` (yielding its inode + type), then insert it under `new`. Re-read the
    // parent bytes between the two: `dir_remove` rewrote a directory block, but the parent
    // *inode* (extent map) is unchanged, so the cached bytes still locate the blocks.
    let (ino, ft) = dir_remove(rw, &sb, &parent, old)?;
    let mut parent = parent;
    dir_insert(rw, &sb, dir_ino, &mut parent, new, ino, ft, now)?;
    // The directory's contents changed. The renamed inode is untouched — its name
    // is not part of it, it lives in the directory entry.
    touch_inode(rw, &sb, dir_ino, now, Stamp::Modified)?;
    Ok(())
}

/// Shrink the regular file at `path` to `new_size` bytes, freeing the blocks past the
/// new end, and return the resulting size.
///
/// Shrink **only**: growing a file allocates, which is [`grow_file`]'s job, so a
/// `new_size` at or above the current size is a no-op that reports the current size
/// rather than an error. That keeps a caller which just wants "make this file exactly
/// N bytes" from having to know which direction it is going.
///
/// Why this exists: without it, creating a file is idempotent and growing it to a
/// smaller size does nothing, so writing short content over a long file would leave the
/// old tail in place — a file that is neither the old one nor the new one. `copy --force`
/// refused that case outright until this landed (decision log, 2026-07-24).
///
/// The extent walk handles the three cases an extent can be in relative to the new end:
/// entirely past it (freed and dropped), straddling it (shortened, its tail freed), or
/// entirely within it (untouched). Depth-0 extent trees only, as elsewhere in this
/// server; an index node returns `Unsupported` rather than silently corrupting the tree.
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
    // Blocks the file keeps: everything holding a byte below `new_size`. A partial
    // final block stays — the bytes past `new_size` inside it are simply no longer
    // part of the file, exactly as a regular write leaves slack in the last block.
    let keep_blocks = new_size.div_ceil(bs) as u64;

    let eh = 40;
    if rd_u16(&inode, eh) != EXTENT_MAGIC {
        return Err(FsError::Corrupt);
    }
    if rd_u16(&inode, eh + 6) != 0 {
        return Err(FsError::Unsupported); // index nodes (depth > 0) deferred
    }
    let entries = rd_u16(&inode, eh + 2) as usize;
    let mut kept = 0usize;
    let mut freed_blocks = 0u64;

    for i in 0..entries {
        let e = eh + 12 + i * 12;
        let ee_block = rd_u32(&inode, e) as u64; // first logical block of the extent
        let ee_len = (rd_u16(&inode, e + 4) & 0x7FFF) as u64;
        let phys = rd_u32(&inode, e + 8) as u64 | ((rd_u16(&inode, e + 6) as u64) << 32);

        if ee_block >= keep_blocks {
            // Entirely past the new end — free every block and drop the entry.
            for b in 0..ee_len {
                free_block(rw, &sb, phys + b)?;
            }
            freed_blocks += ee_len;
            continue;
        }
        let keep_len = (keep_blocks - ee_block).min(ee_len);
        if keep_len < ee_len {
            // Straddles the new end — free the tail and shorten it.
            for b in keep_len..ee_len {
                free_block(rw, &sb, phys + b)?;
            }
            freed_blocks += ee_len - keep_len;
        }
        // Compact surviving entries toward the front of the tree so the kept ones stay
        // contiguous; `kept` is where this one lands.
        let dst = eh + 12 + kept * 12;
        let mut entry = [0u8; 12];
        entry.copy_from_slice(&inode[e..e + 12]);
        entry[4..6].copy_from_slice(&(keep_len as u16).to_le_bytes()); // ee_len
        inode[dst..dst + 12].copy_from_slice(&entry);
        kept += 1;
    }

    // Zero the entries the walk dropped, so a stale copy of a freed extent cannot be
    // read back as live if `eh_entries` were ever mis-set.
    for i in kept..entries {
        let e = eh + 12 + i * 12;
        inode[e..e + 12].fill(0);
    }
    inode[eh + 2..eh + 4].copy_from_slice(&(kept as u16).to_le_bytes()); // eh_entries

    // i_size (lo @4, hi @108) + i_blocks (@28, in 512-byte units).
    inode[4..8].copy_from_slice(&(new_size as u32).to_le_bytes());
    if sb.inode_size > 128 {
        inode[108..112].copy_from_slice(&((new_size as u64 >> 32) as u32).to_le_bytes());
    }
    let freed_sectors = (freed_blocks * bs as u64 / 512) as u32;
    let i_blocks = rd_u32(&inode, 28).saturating_sub(freed_sectors);
    inode[28..32].copy_from_slice(&i_blocks.to_le_bytes());
    stamp(&mut inode, now, sb.inode_size, Stamp::Modified);

    let off = inode_offset(rw, &sb, ino)?;
    rw.write_at(off, &inode[..(sb.inode_size as usize).min(256)])?;
    Ok(new_size)
}

/// Resolve `path` (absolute) to a **regular file** and read its content into
/// `out`, returning the file size. The file's content occupies `out[..size]`;
/// the caller (the fs-server) sizes its `MemoryObject` to `size`. The eager
/// slice-7 path — kept for an `AS_MEMOBJ` resolve. Errors: as
/// [`resolve_regular_file`], plus `TooLarge` (file > [`MAX_FILE`] or > `out`).
pub fn read_file<R: BlockReader>(r: &R, path: &[u8], out: &mut [u8]) -> Result<usize, FsError> {
    let sb = read_superblock(r)?;
    let (inode, size) = resolve_regular_file(r, &sb, path)?;
    if size > MAX_FILE || size > out.len() {
        return Err(FsError::TooLarge);
    }

    let bs = sb.block_size as usize;
    let mut buf = [0u8; MAX_BLOCK];
    let mut copied = 0;
    let mut lb = 0u64;
    while copied < size {
        let n = bs.min(size - copied);
        let phys = extent_find(r, &sb, &inode[40..100], lb)?;
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
    /// `decode_time` must take the epoch-extension bits from the *matching* `extra`
    /// word. The fields are adjacent (`i_ctime_extra` at 132, `i_mtime_extra` at 136)
    /// and reading one for the other is invisible for any date before 2038 — which is
    /// exactly how that bug survived until now.
    #[test]
    fn decode_time_applies_the_epoch_extension() {
        // No extension: a plain 32-bit second count.
        assert_eq!(super::decode_time(1_784_900_730, 0), 1_784_900_730);
        // The low two bits of `extra` are the epoch, added above bit 31. Bit 0 set
        // pushes the timestamp past 2038 instead of wrapping.
        assert_eq!(super::decode_time(0, 1), 1i64 << 32);
        assert_eq!(super::decode_time(7, 2), (2i64 << 32) | 7);
        // The upper bits of `extra` are nanoseconds and must be ignored here.
        assert_eq!(super::decode_time(5, 0xFFFF_FFFC), 5);
    }

    /// A stamped inode must place each timestamp at its own offset — the check that
    /// would have caught reading `i_ctime_extra` as `i_mtime_extra`.
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
        // The epoch bit lands in mtime's own extra word, and the round trip through
        // `decode_time` recovers the full value.
        assert_eq!(at(I_MTIME_EXTRA) & 0x3, 1);
        assert_eq!(super::decode_time(at(I_MTIME), at(I_MTIME_EXTRA)), now);
    }

    /// A metadata-only change moves ctime and leaves mtime alone — the distinction
    /// `unlink` relies on for a file that still has other links.
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

    /// A base-size (128-byte) inode has no `*_extra` words; stamping must not write
    /// past the fields that exist.
    #[test]
    fn a_small_inode_gets_no_extra_words() {
        use super::{I_CTIME_EXTRA, Stamp, stamp};
        let mut inode = [0u8; 256];
        stamp(&mut inode, 1_784_900_730, 128, Stamp::Created);
        assert_eq!(&inode[I_CTIME_EXTRA..I_CTIME_EXTRA + 4], &[0, 0, 0, 0]);
    }
}
