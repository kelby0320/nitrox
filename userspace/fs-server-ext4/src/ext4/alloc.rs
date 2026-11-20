//! Allocating and freeing blocks and inodes: the bitmaps, and the counts beside them.

use super::*;

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
        let mut map = Bitmap(&mut bitmap[..bs]);

        let group_start = sb.group_start(group);
        let goal_idx = goal.checked_sub(group_start).unwrap_or(u64::MAX) as usize;
        let start = if n == 0 && goal_idx < capacity && map.is_free(goal_idx)? {
            goal_idx
        } else {
            match map.first_free(capacity)? {
                Some(i) => i,
                // The descriptor's count disagreed with its bitmap. Not fatal, and not this
                // function's to repair: move on rather than fail with space elsewhere.
                None => continue,
            }
        };
        // No more than the descriptor says is free, so its count cannot wrap below zero.
        let most = want.max(1).min(gfree as u64) as usize;
        let mut len = 0;
        while len < most && start + len < capacity && map.is_free(start + len)? {
            map.set(start + len)?;
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

/// Allocate one free inode, taking the **first group that has one**: sets the inode-bitmap bit
/// and decrements the group-descriptor and superblock free-inode counts. `TooLarge` only when
/// every group is exhausted. **Locality is not attempted**: real ext4 places a new inode near its
/// parent (the Orlov allocator); this takes the lowest free inode, worse for a spinning disk's
/// seeks, and nothing has measured it yet.
pub(super) fn alloc_inode<RW: BlockReader + BlockWriter>(rw: &RW, sb: &Superblock) -> Result<u32, FsError> {
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
        let mut map = Bitmap(&mut bitmap[..bs]);
        let Some(idx) = map.first_free(capacity)? else {
            continue; // the count disagreed with the bitmap; try the next group
        };
        map.set(idx)?;
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

/// Free inode `ino`: clear its inode-bitmap bit and increment the group-descriptor and superblock
/// free-inode counts. If `is_dir`, also decrements the group's `bg_used_dirs_count`.
pub(super) fn free_inode<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    ino: u32,
    is_dir: bool,
    now: i64,
) -> Result<(), FsError> {
    let bs = sb.block_size as usize;
    let (group, index) = sb.inode_place(ino)?;
    let gd_off = sb.group_desc_offset(group);
    let dsz = (sb.desc_size as usize).min(64);
    let mut gd = [0u8; 64];
    rw.read_meta(gd_off, &mut gd[..dsz])?;
    let ibitmap_block = rd_u32(&gd, 4) as u64;
    let mut bitmap = [0u8; MAX_BLOCK];
    rw.read_meta(ibitmap_block * sb.block_size as u64, &mut bitmap[..bs])?;
    Bitmap(&mut bitmap[..bs]).unset(index)?;

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
        let mut map = Bitmap(&mut bitmap[..bs]);
        let mut cleared = 0u16;
        for i in index..index + n {
            if !map.is_free(i)? {
                map.unset(i)?;
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
