//! Inserting a name into a directory, growing it by a block when every one it has is full.

use super::*;

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
pub(super) fn dir_insert<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    dir_ino: u32,
    dir_inode: &mut [u8; 256],
    name: &[u8],
    ino: u32,
    file_type: u8,
    now: i64,
) -> Result<(), FsError> {
    if name.len() > 255 {
        return Err(FsError::Unsupported); // an entry's length is one byte
    }
    // A hash tree would not hold the name, and its root lies in the slack an insert takes. Made
    // linear first, as an older writer makes one, the directory reads as its blocks already do.
    let flags = rd_u32(dir_inode, 32);
    if flags & INDEX_FL != 0 {
        dir_inode[32..36].copy_from_slice(&(flags & !INDEX_FL).to_le_bytes());
        rw.write_at(inode_offset(rw, sb, dir_ino)?, &dir_inode[..sb.inode_len()])?;
    }
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
        while let Some(e) = record(&buf[..bs], off) {
            let used = if e.ino != 0 { round4(8 + e.name_len) } else { 0 };
            if e.len - used >= need {
                let new_off = off + used;
                let new_rec = e.len - used;
                if e.ino != 0 {
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
            off += e.len;
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
