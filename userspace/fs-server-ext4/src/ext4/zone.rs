//! **The system zone**: each group's superblock and descriptor copies, bitmaps and inode table,
//! which no extent may map, no free give back and no allocation hand out. With no `alloc` and no
//! mount for `nxinstall`, each question reads the superblock and one flex group's descriptors.

use super::*;

/// The bytes of descriptors [`clear`] reads at a time: a flex group's of 16, the usual size.
const CHUNK: usize = 512;

/// Groups whose descriptors [`clear`] reads for a block's: its flex group, up to a block of them.
fn flex_span(sb: &Superblock) -> u64 {
    sb.groups_per_flex.min((sb.block_size / sb.desc_size) as u64).max(1)
}

/// Each table a group's descriptor names, as `(first block, blocks)`.
fn tables(sb: &Superblock, d: &[u8]) -> [(u64, u64); 3] {
    [(rd_u32(d, 0) as u64, 1), (rd_u32(d, 4) as u64, 1), (rd_u32(d, 8) as u64, sb.table_blocks())]
}

/// Hand each descriptor of groups `from..to`, read `buf` at a time, to `each` until one answers
/// `false`, and answer that.
fn descriptors<R: BlockReader>(
    r: &R,
    sb: &Superblock,
    (from, to): (u64, u64),
    buf: &mut [u8],
    mut each: impl FnMut(u64, &[u8]) -> bool,
) -> Result<bool, FsError> {
    let size = sb.desc_size as usize;
    let mut g = from;
    while g < to {
        let n = (to - g).min((buf.len() / size) as u64);
        let bytes = &mut buf[..n as usize * size];
        r.read_meta(sb.group_desc_offset(g as u32), bytes)?;
        for (i, d) in bytes.chunks_exact(size).enumerate() {
            if !each(g + i as u64, d) {
                return Ok(false);
            }
        }
        g += n;
    }
    Ok(true)
}

/// **Whether `len` blocks from `phys` are on the filesystem and clear of every group's metadata**:
/// of its copies, by the superblock, and of the tables, by its flex group's descriptors
/// ([`layout`]).
pub(crate) fn clear<R: BlockReader>(r: &R, sb: &Superblock, phys: u64, len: u64) -> Result<bool, FsError> {
    let first = sb.first_data_block as u64;
    let Some(end) = phys.checked_add(len).filter(|&end| phys >= first && end <= sb.blocks_count) else {
        return Ok(false);
    };
    let per_group = sb.blocks_per_group as u64;
    let span = flex_span(sb);
    let groups = sb.group_count() as u64;
    let mut at = phys;
    while at < end {
        let group = (at - first) / per_group;
        let start = sb.group_start(group as u32);
        let stop = end.min(start + per_group);
        if sb.has_super(group as u32) && at < start + sb.super_blocks() {
            return Ok(false);
        }
        let flex = group / span * span;
        let apart = |_: u64, d: &[u8]| tables(sb, d).iter().all(|&(s, n)| stop <= s || s.saturating_add(n) <= at);
        if !descriptors(r, sb, (flex, (flex + span).min(groups)), &mut [0u8; CHUNK], apart)? {
            return Ok(false);
        }
        at = stop;
    }
    Ok(true)
}

/// **Whether [`clear`] sees every group's tables**: each inside its flex group, whose descriptors
/// fit a block, as `mke2fs` and `mkfs` make them. Otherwise (a `resize2fs`, say) it is served
/// read-only.
pub(crate) fn layout<R: BlockReader>(r: &R, sb: &Superblock) -> Result<(), Unservable> {
    if sb.groups_per_flex > flex_span(sb) {
        return Err(Unservable::FlexTooWide { groups: sb.groups_per_flex });
    }
    let groups = sb.group_count() as u64;
    let span = flex_span(sb);
    let mut outside = None;
    let inside = |g: u64, d: &[u8]| {
        let flex = g / span * span;
        let from = sb.group_start(flex as u32);
        let to = sb.group_start((flex + span).min(groups) as u32).min(sb.blocks_count);
        let ok = tables(sb, d).iter().all(|&(s, n)| from <= s && s.saturating_add(n) <= to);
        if !ok {
            outside = Some(g as u32);
        }
        ok
    };
    // A block at a time: the probe reads a disk with no cache, and a 931 GiB root has 7,448 groups.
    let block = &mut [0u8; MAX_BLOCK][..sb.block_size as usize];
    descriptors(r, sb, (0, groups), block, inside).map_err(|_| Unservable::Unreadable)?;
    match outside {
        Some(group) => Err(Unservable::Layout { group }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests;
