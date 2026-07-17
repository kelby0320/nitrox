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
const S_IFDIR: u16 = 0x4000;

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

    // Parse the depth-0 extent header + leaf entries from `i_block` (inode[40..100]).
    let eh = 40; // extent header offset in the inode
    if rd_u16(&inode, eh) != EXTENT_MAGIC {
        return Err(FsError::Corrupt);
    }
    if rd_u16(&inode, eh + 6) != 0 {
        return Err(FsError::Unsupported); // index nodes (depth > 0) are deferred
    }
    let mut entries = rd_u16(&inode, eh + 2) as usize;
    let max_entries = rd_u16(&inode, eh + 4) as usize;
    // Last extent (highest ee_block) — the append point. Empty file → no extents yet.
    let ent = |i: usize| eh + 12 + i * 12; // i-th leaf entry offset
    let (mut last_log_end, mut last_phys_end) = if entries == 0 {
        (0u64, 0u64)
    } else {
        let e = ent(entries - 1);
        let ee_block = rd_u32(&inode, e) as u64;
        let ee_len = (rd_u16(&inode, e + 4) & 0x7FFF) as u64;
        let phys = rd_u32(&inode, e + 8) as u64 | ((rd_u16(&inode, e + 6) as u64) << 32);
        (ee_block + ee_len, phys + ee_len)
    };

    for lb in cur_blocks..new_blocks {
        let goal = if last_phys_end != 0 { last_phys_end } else { sb.first_data_block as u64 };
        let phys = alloc_block(rw, &sb, goal)?;
        let contiguous = entries > 0 && lb as u64 == last_log_end && phys == last_phys_end;
        if contiguous {
            // Extend the last extent: bump its ee_len.
            let e = ent(entries - 1);
            let new_len = (rd_u16(&inode, e + 4) & 0x7FFF) + 1;
            inode[e + 4..e + 6].copy_from_slice(&new_len.to_le_bytes());
        } else {
            // Add a new leaf extent, if the inline header has room.
            if entries >= max_entries {
                return Err(FsError::Unsupported); // needs a tree split (deferred)
            }
            let e = ent(entries);
            inode[e..e + 4].copy_from_slice(&(lb as u32).to_le_bytes()); // ee_block
            inode[e + 4..e + 6].copy_from_slice(&1u16.to_le_bytes()); // ee_len
            inode[e + 6..e + 8].copy_from_slice(&((phys >> 32) as u16).to_le_bytes()); // start_hi
            inode[e + 8..e + 12].copy_from_slice(&(phys as u32).to_le_bytes()); // start_lo
            entries += 1;
            inode[eh + 2..eh + 4].copy_from_slice(&(entries as u16).to_le_bytes()); // eh_entries
        }
        last_log_end = lb as u64 + 1;
        last_phys_end = phys + 1;
    }

    // Update inode size (i_size_lo @4, hi @108) + block count (i_blocks_lo @28, 512-B units).
    inode[4..8].copy_from_slice(&(new_size as u32).to_le_bytes());
    if sb.inode_size > 128 {
        inode[108..112].copy_from_slice(&((new_size as u64 >> 32) as u32).to_le_bytes());
    }
    let added_sectors = ((new_blocks - cur_blocks) * bs / 512) as u32;
    let i_blocks = rd_u32(&inode, 28).wrapping_add(added_sectors);
    inode[28..32].copy_from_slice(&i_blocks.to_le_bytes());

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
