//! Growing a file: blocks allocated, zeroed and appended to its tree a run at a time.

use super::*;

/// Zeroes for what a grow adds ([`grow_file`]); static, since a server thread's stack is small.
static ZERO_BLOCK: [u8; MAX_BLOCK] = [0; MAX_BLOCK];

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
        // Blocks past the end (an `fallocate` that kept the size leaves them, and so does a growth
        // whose size write was lost) would sit under the extents this appends, so they go first.
        extent::cut_past(rw, &sb, &mut inode, cur_blocks, Some(off))?;
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
