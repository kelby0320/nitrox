//! The read-only ext4 parser. See the crate docs for scope.
//!
//! On-disk layout (all little-endian): the superblock at byte 1024; block-group
//! descriptors in the GDT after it; inodes located via the group descriptor's
//! inode-table block; file/directory data located via the inode's **extent
//! tree**; directories scanned as a linear list of `ext4_dir_entry_2`.

use crate::{BlockReader, FsError, rd_u16, rd_u32};

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
const S_IFDIR: u16 = 0x4000;

/// The parsed superblock facts the reader needs.
struct Superblock {
    block_size: u32,
    inodes_per_group: u32,
    inode_size: u32,
    desc_size: u32,
    /// First block of the group-descriptor table.
    first_gdt_block: u64,
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
    Ok(Superblock {
        block_size,
        inodes_per_group,
        inode_size,
        desc_size,
        first_gdt_block: if block_size == 1024 { 2 } else { 1 },
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
