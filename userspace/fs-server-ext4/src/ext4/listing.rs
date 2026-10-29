//! Listing a directory: its live entries from a cursor, with each inode's stat if asked.

use super::{InodeStat, Lookup, MAX_BLOCK, live_dir, read_inode, read_superblock, record, stat_inode};
use crate::{BlockReader, FsError, rd_u32};

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
            // The cursor is the client's, and the block may have changed since it was handed out:
            // resume at the first record at or past it, found from the block's start, as Linux does.
            if pos == cursor && in_block > 0 {
                let mut at = 0;
                while at < in_block {
                    match record(&buf[..bs as usize], at) {
                        Some(e) => at += e.len,
                        None => at = bs as usize,
                    }
                }
                pos = lb * bs + at as u64;
                continue;
            }
        }
        let Some(e) = record(&buf[..bs as usize], in_block) else {
            pos = (lb + 1) * bs;
            continue;
        };
        if e.ino != 0 && e.name_len > 0 {
            let name = &buf[in_block + 8..in_block + 8 + e.name_len];
            // A single bad inode degrades to a zeroed stat. This reads into its own buffers, so the
            // cached directory block in `buf` survives.
            let stat = if want_stat { stat_inode(r, &sb, e.ino).unwrap_or_default() } else { InodeStat::default() };
            if !emit(e.ino, e.file_type, name, &stat) {
                return Ok(pos);
            }
        }
        pos += e.len as u64;
    }
    Ok(0)
}
