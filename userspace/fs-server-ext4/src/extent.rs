//! **The extent tree** (Phase 6 Part H.1): how an inode maps its logical blocks to the device —
//! looked up, appended to, cut, and freed.
//!
//! An inode's `i_block` is a node of four entries. A file with more extents than that is a tree:
//! the inode's node an index over blocks, each a node of `(block_size - 12) / 12` entries — 340 at
//! 4 KiB — and ext4 allows five levels. Every node is a 12-byte header and 12-byte entries: a
//! leaf's entries are extents, a run of logical blocks and where it starts on the device; an
//! index's name a child block and the first logical block under it.
//!
//! **Until Part H this server wrote only the inode's four.** A fifth extent was refused, a tree
//! was read and never freed, and about 815 files with data filled a directory, since each file's
//! data lands between the directory's own blocks and every directory block became an extent.
//!
//! What changes a tree here:
//! - [`append`] adds an extent at the file's end, merging it into the last one when the two
//!   continue each other. A full leaf gains a sibling to its right holding the new extent alone,
//!   so the leaves an append fills stay full, as ext4's own appends leave them; a full inode moves
//!   its entries into a block below it, one level deeper. **It is atomic**: on an error nothing it
//!   wrote is reachable and every tree block it took is given back.
//! - [`truncate`] cuts the tree at a logical block, freeing what is past it and every tree block
//!   left empty, and folds what is left back into the inode while it fits there. It is also how a
//!   file is freed whole, cut at `0`, and how a failed grow is undone, cut where the grow began.
//! - [`Lookup`] answers where a block is, reading each leaf once as a walk passes it.
//!
//! **An extent's length is ext4's**: an `ee_len` up to 32,768 is a written extent of that many
//! blocks, and one above it an *unwritten* extent of `ee_len - 32768` — allocated and never
//! written, as `fallocate` leaves one — which reads as zeroes, whatever its blocks hold. Until
//! Part H the reader masked the top bit, so a full 32,768-block extent read as a hole and an
//! unwritten one as its blocks' old contents. An unwritten extent is a hole to the kernel too,
//! which writes back no page over one (`filesystem-data-path.md`).
//!
//! No `alloc`, as the rest of the library: a walk holds one block per level of the tree on the
//! stack, and there are at most [`MAX_DEPTH`] levels below the inode.

use crate::ext4::{MAX_BLOCK, Superblock, alloc_block, free_block};
use crate::{BlockReader, BlockWriter, FsError, rd_u16, rd_u32};

/// `eh_magic`.
pub(crate) const MAGIC: u16 = 0xF30A;
/// The deepest tree ext4 makes (`EXT4_MAX_EXTENT_DEPTH`).
const MAX_DEPTH: u16 = 5;
/// The longest written extent, in blocks, and the `ee_len` above which an extent is unwritten.
pub(crate) const MAX_LEN: u64 = 32768;
/// Entries in the inode's own node: `(60 - 12) / 12`.
pub(crate) const ROOT_ENTRIES: usize = 4;
/// The inode's own node: `i_block`, sixty bytes at offset 40.
const ROOT: core::ops::Range<usize> = 40..100;
/// One past the last logical block an extent can name: `ee_block` is 32 bits.
const END: u64 = 1 << 32;
/// The size of a node's header, and of each of its entries.
const ENTRY: usize = 12;

/// One extent: `len` blocks from logical block `lblock`, on the device from block `phys`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Extent {
    pub(crate) lblock: u64,
    pub(crate) len: u64,
    pub(crate) phys: u64,
    /// Allocated and never written: it reads as zeroes, whatever its blocks hold.
    pub(crate) unwritten: bool,
}

impl Extent {
    /// A written extent of `len` blocks from logical block `lblock`, at device block `phys`.
    pub(crate) fn written(lblock: u64, phys: u64, len: u64) -> Self {
        Extent { lblock, len, phys, unwritten: false }
    }

    /// One past its last logical block.
    fn end(&self) -> u64 {
        self.lblock + self.len
    }

    /// Whether `next` continues this extent, logically and on the device, and the two fit in one.
    fn continued_by(&self, next: &Extent) -> bool {
        !self.unwritten
            && !next.unwritten
            && next.lblock == self.end()
            && next.phys == self.phys + self.len
            && self.len + next.len <= MAX_LEN
    }
}

/// A node's header.
#[derive(Copy, Clone)]
struct Header {
    entries: usize,
    max: usize,
    /// Levels above the leaves: `0` for a leaf.
    depth: u16,
}

/// Read and check the header of `node`, whose bytes have room for `cap` entries.
fn header(node: &[u8], cap: usize) -> Result<Header, FsError> {
    if node.len() < ENTRY || rd_u16(node, 0) != MAGIC {
        return Err(FsError::Corrupt);
    }
    let h = Header { entries: rd_u16(node, 2) as usize, max: rd_u16(node, 4) as usize, depth: rd_u16(node, 6) };
    if h.max > cap || h.entries > h.max || h.depth > MAX_DEPTH {
        return Err(FsError::Corrupt);
    }
    Ok(h)
}

/// Write a node's header: `entries` of a possible `max`, `depth` levels above the leaves.
fn set_header(node: &mut [u8], entries: usize, max: usize, depth: u16) {
    node[0..2].copy_from_slice(&MAGIC.to_le_bytes());
    node[2..4].copy_from_slice(&(entries as u16).to_le_bytes());
    node[4..6].copy_from_slice(&(max as u16).to_le_bytes());
    node[6..8].copy_from_slice(&depth.to_le_bytes());
}

/// Where entry `i` of a node begins.
fn at(i: usize) -> usize {
    ENTRY + i * ENTRY
}

/// How many entries a tree block holds.
fn block_cap(sb: &Superblock) -> usize {
    (sb.block_size as usize - ENTRY) / ENTRY
}

/// The empty root an inode with no blocks has: a leaf of no entries.
pub(crate) fn empty_root(inode: &mut [u8; 256]) {
    inode[ROOT].fill(0);
    set_header(&mut inode[ROOT], 0, ROOT_ENTRIES, 0);
}

/// Leaf entry `i`. An `ee_len` of `0` is not an extent.
fn read_extent(node: &[u8], i: usize) -> Result<Extent, FsError> {
    let e = at(i);
    let raw = rd_u16(node, e + 4) as u64;
    let (len, unwritten) = if raw <= MAX_LEN { (raw, false) } else { (raw - MAX_LEN, true) };
    if len == 0 {
        return Err(FsError::Corrupt);
    }
    let phys = rd_u32(node, e + 8) as u64 | (rd_u16(node, e + 6) as u64) << 32;
    Ok(Extent { lblock: rd_u32(node, e) as u64, len, phys, unwritten })
}

/// Write leaf entry `i`. `x.len` is at most [`MAX_LEN`] written, one less unwritten.
fn write_extent(node: &mut [u8], i: usize, x: Extent) {
    let e = at(i);
    let raw = if x.unwritten { x.len + MAX_LEN } else { x.len } as u16;
    node[e..e + 4].copy_from_slice(&(x.lblock as u32).to_le_bytes());
    node[e + 4..e + 6].copy_from_slice(&raw.to_le_bytes());
    node[e + 6..e + 8].copy_from_slice(&((x.phys >> 32) as u16).to_le_bytes());
    node[e + 8..e + 12].copy_from_slice(&(x.phys as u32).to_le_bytes());
}

/// Index entry `i`: the first logical block under it, and its child block.
fn read_index(node: &[u8], i: usize) -> (u64, u64) {
    let e = at(i);
    (rd_u32(node, e) as u64, rd_u32(node, e + 4) as u64 | (rd_u16(node, e + 8) as u64) << 32)
}

/// Write index entry `i`.
fn write_index(node: &mut [u8], i: usize, first: u64, child: u64) {
    let e = at(i);
    node[e..e + 4].copy_from_slice(&(first as u32).to_le_bytes());
    node[e + 4..e + 8].copy_from_slice(&(child as u32).to_le_bytes());
    node[e + 8..e + 10].copy_from_slice(&((child >> 32) as u16).to_le_bytes());
    node[e + 10..e + 12].fill(0);
}

/// The first logical block under entry `i` of a node.
fn first_of(node: &[u8], h: Header, i: usize) -> Result<u64, FsError> {
    Ok(if h.depth == 0 { read_extent(node, i)?.lblock } else { read_index(node, i).0 })
}

/// Whether `len` blocks from `phys` are all on the filesystem, past its first data block.
fn on_device(sb: &Superblock, phys: u64, len: u64) -> bool {
    phys >= sb.first_data_block as u64 && phys != 0 && phys.saturating_add(len) <= sb.blocks_count
}

/// Read tree block `blk` into `buf`: it must be a node `depth` levels above the leaves.
fn read_node<R: BlockReader>(
    r: &R,
    sb: &Superblock,
    blk: u64,
    depth: u16,
    buf: &mut [u8; MAX_BLOCK],
) -> Result<Header, FsError> {
    if !on_device(sb, blk, 1) {
        return Err(FsError::Corrupt);
    }
    let bs = sb.block_size as usize;
    r.read_at(blk * bs as u64, &mut buf[..bs])?;
    let h = header(&buf[..bs], block_cap(sb))?;
    if h.depth != depth {
        return Err(FsError::Corrupt);
    }
    Ok(h)
}

/// Move an inode's `i_blocks`, in 512-byte units, by `blocks` filesystem blocks.
fn count(inode: &mut [u8; 256], sb: &Superblock, blocks: i64) {
    let units = blocks * (sb.block_size / 512) as i64;
    let v = (rd_u32(inode, 28) as i64 + units).clamp(0, u32::MAX as i64) as u32;
    inode[28..32].copy_from_slice(&v.to_le_bytes());
}

// --- looking blocks up ---------------------------------------------------------

/// **Where a file's blocks are**, for a walk through them in order: a leaf is read when the walk
/// reaches it and kept while the walk stays in it, so a tree costs a read per leaf rather than a
/// descent per block.
pub(crate) struct Lookup {
    root: [u8; 60],
    leaf: [u8; MAX_BLOCK],
    /// The leaf in `leaf`, once one is read.
    span: Option<Span>,
}

/// A leaf held by a [`Lookup`]: how many bytes of `leaf` it is, how many entries they hold, and
/// the logical blocks `lo..hi` it covers.
#[derive(Copy, Clone)]
struct Span {
    bytes: usize,
    cap: usize,
    lo: u64,
    hi: u64,
}

/// What a [`Lookup`] finds at a logical block.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Seg {
    /// The device block, or `0` for a hole: no extent there, or an unwritten one.
    pub(crate) phys: u64,
    /// How many blocks from there go on alike: on through the device, or as a hole.
    pub(crate) len: u64,
}

/// Where a descent ended: in a leaf, or in a hole before every child of an index.
enum Descent {
    Leaf(Span),
    Hole(Seg),
}

impl Lookup {
    /// A lookup over `inode`'s tree. The root is copied, so the inode is the caller's again.
    pub(crate) fn new(inode: &[u8; 256]) -> Self {
        let mut root = [0u8; 60];
        root.copy_from_slice(&inode[ROOT]);
        Lookup { root, leaf: [0; MAX_BLOCK], span: None }
    }

    /// The device block holding logical block `lb`, or `0` for a hole.
    pub(crate) fn block<R: BlockReader>(&mut self, r: &R, sb: &Superblock, lb: u64) -> Result<u64, FsError> {
        Ok(self.seg(r, sb, lb)?.phys)
    }

    /// What is at logical block `lb`, and how far it goes on.
    pub(crate) fn seg<R: BlockReader>(&mut self, r: &R, sb: &Superblock, lb: u64) -> Result<Seg, FsError> {
        if lb >= END {
            return Ok(Seg { phys: 0, len: 1 }); // no extent reaches past 2^32 blocks
        }
        let span = match self.span {
            Some(s) if s.lo <= lb && lb < s.hi => s,
            _ => match self.descend(r, sb, lb)? {
                Descent::Leaf(s) => s,
                Descent::Hole(seg) => return Ok(seg),
            },
        };
        let node = &self.leaf[..span.bytes];
        let h = header(node, span.cap)?;
        let mut prev_end = span.lo;
        for i in 0..h.entries {
            let x = read_extent(node, i)?;
            if x.lblock < prev_end || x.end() > span.hi || !on_device(sb, x.phys, x.len) {
                return Err(FsError::Corrupt); // overlapping, out of order, or off the device
            }
            if lb < x.lblock {
                return Ok(Seg { phys: 0, len: x.lblock - lb });
            }
            if lb < x.end() {
                let len = x.end() - lb;
                let phys = if x.unwritten { 0 } else { x.phys + (lb - x.lblock) };
                return Ok(Seg { phys, len });
            }
            prev_end = x.end();
        }
        Ok(Seg { phys: 0, len: span.hi - lb })
    }

    /// Walk from the root to the leaf covering `lb`, keeping each index's bounds for the child it
    /// chooses, so the leaf's span is exact.
    fn descend<R: BlockReader>(&mut self, r: &R, sb: &Superblock, lb: u64) -> Result<Descent, FsError> {
        self.span = None;
        self.leaf[..60].copy_from_slice(&self.root);
        let (mut bytes, mut cap) = (60, ROOT_ENTRIES);
        let (mut lo, mut hi) = (0u64, END);
        let mut depth = header(&self.leaf[..60], ROOT_ENTRIES)?.depth;
        loop {
            let h = header(&self.leaf[..bytes], cap)?;
            if h.depth != depth {
                return Err(FsError::Corrupt);
            }
            if depth == 0 {
                let span = Span { bytes, cap, lo, hi };
                self.span = Some(span);
                return Ok(Descent::Leaf(span));
            }
            let node = &self.leaf[..bytes];
            // The last child whose first block is at or before `lb`.
            let mut pick = None;
            for i in 0..h.entries {
                if read_index(node, i).0 > lb {
                    break;
                }
                pick = Some(i);
            }
            let Some(i) = pick else {
                // Before everything under this node: a hole up to its first child.
                let first = if h.entries > 0 { read_index(node, 0).0.min(hi) } else { hi };
                return Ok(Descent::Hole(Seg { phys: 0, len: first - lb }));
            };
            let (first, child) = read_index(node, i);
            let next = if i + 1 < h.entries { read_index(node, i + 1).0 } else { hi };
            if first < lo || next <= first || next > hi {
                return Err(FsError::Corrupt); // index entries out of order
            }
            (lo, hi) = (first, next);
            if !on_device(sb, child, 1) {
                return Err(FsError::Corrupt);
            }
            let bs = sb.block_size as usize;
            r.read_at(child * bs as u64, &mut self.leaf[..bs])?;
            (bytes, cap) = (bs, block_cap(sb));
            depth -= 1;
        }
    }
}

/// The device block just past the file's last extent — where a block that continues it would go —
/// or `None` when it has none.
pub(crate) fn last_end<R: BlockReader>(r: &R, sb: &Superblock, inode: &[u8; 256]) -> Result<Option<u64>, FsError> {
    let mut buf = [0u8; MAX_BLOCK];
    buf[..60].copy_from_slice(&inode[ROOT]);
    let mut h = header(&buf[..60], ROOT_ENTRIES)?;
    loop {
        if h.entries == 0 {
            return Ok(None);
        }
        if h.depth == 0 {
            let x = read_extent(&buf, h.entries - 1)?;
            return Ok(Some(x.phys + x.len));
        }
        let (_, child) = read_index(&buf, h.entries - 1);
        h = read_node(r, sb, child, h.depth - 1, &mut buf)?;
    }
}

// --- appending -----------------------------------------------------------------

/// The tree blocks an [`append`] has taken, for giving back if it fails. A split at every level
/// below the root, and one more to deepen it.
struct Taken {
    blocks: [u64; MAX_DEPTH as usize + 1],
    n: usize,
}

/// What adding an extent under a node did to it.
enum Added {
    /// The node changed, and its caller writes it.
    Changed,
    /// Everything that changed is written already.
    Unchanged,
    /// The node was full: a new sibling to its right — at `.1`, its first block `.0` — holds the
    /// extent, and the node itself is as it was.
    Split(u64, u64),
}

/// **Append `x` at the file's end**: it must begin at or past the end of the last extent.
///
/// Merged into the last extent when it continues it; otherwise added beside it — a full leaf
/// gaining a sibling to its right that holds `x` alone, and a full inode moving its entries into a
/// block below it, one level deeper. `i_blocks` counts `x` and every tree block taken.
///
/// **Atomic**: on an error the tree is as it was, nothing written is reachable from it, and every
/// tree block taken is given back. `x`'s own blocks are the caller's to give back.
pub(crate) fn append<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    inode: &mut [u8; 256],
    x: Extent,
) -> Result<(), FsError> {
    let limit = if x.unwritten { MAX_LEN - 1 } else { MAX_LEN };
    if x.len == 0 || x.len > limit || x.end() > END {
        return Err(FsError::TooLarge);
    }
    let mut taken = Taken { blocks: [0; MAX_DEPTH as usize + 1], n: 0 };
    let mut root = [0u8; 60];
    root.copy_from_slice(&inode[ROOT]);
    match append_to(rw, sb, &mut root, x, &mut taken) {
        Ok(()) => {
            inode[ROOT].copy_from_slice(&root);
            count(inode, sb, (x.len + taken.n as u64) as i64);
            Ok(())
        }
        Err(e) => {
            for &blk in &taken.blocks[..taken.n] {
                let _ = free_block(rw, sb, blk);
            }
            Err(e)
        }
    }
}

/// [`append`]'s work on a copy of the root, recording the tree blocks it takes.
fn append_to<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    root: &mut [u8; 60],
    x: Extent,
    taken: &mut Taken,
) -> Result<(), FsError> {
    let h = header(root, ROOT_ENTRIES)?;
    // A full leaf in the inode moves below it first, so the leaf `x` joins has room — the shape
    // ext4's own `ext4_ext_grow_indepth` leaves, one leaf of five rather than one of four and one
    // of one.
    if h.depth == 0 && h.entries == h.max {
        let merges = h.entries > 0 && read_extent(root, h.entries - 1)?.continued_by(&x);
        if !merges {
            *root = deeper(rw, sb, root, h, None, taken)?;
        }
    }
    let h = header(root, ROOT_ENTRIES)?;
    if let Added::Split(first, blk) = add(rw, sb, root, h, x, taken, true)? {
        // A full index in the inode: down a level, the new sibling beside its old entries.
        *root = deeper(rw, sb, root, h, Some((first, blk)), taken)?;
    }
    Ok(())
}

/// Add `x` under `node`, a node `h` describes; `is_root` for the inode's own, which cannot gain a
/// sibling.
fn add<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    node: &mut [u8],
    h: Header,
    x: Extent,
    taken: &mut Taken,
    is_root: bool,
) -> Result<Added, FsError> {
    if h.depth == 0 {
        if h.entries > 0 {
            let last = read_extent(node, h.entries - 1)?;
            if x.lblock < last.end() {
                return Err(FsError::Corrupt); // not an append: it would overlap
            }
            if last.continued_by(&x) {
                write_extent(node, h.entries - 1, Extent { len: last.len + x.len, ..last });
                return Ok(Added::Changed);
            }
        }
        if h.entries < h.max {
            write_extent(node, h.entries, x);
            node[2..4].copy_from_slice(&((h.entries + 1) as u16).to_le_bytes());
            return Ok(Added::Changed);
        }
        if is_root {
            return Err(FsError::Corrupt); // `append_to` deepens a full leaf root before adding
        }
        let blk = new_node(rw, sb, taken, 0, |n| {
            write_extent(n, 0, x);
            1
        })?;
        return Ok(Added::Split(x.lblock, blk));
    }
    if h.entries == 0 {
        return Err(FsError::Corrupt); // an index over nothing
    }
    let (_, child) = read_index(node, h.entries - 1);
    let mut buf = [0u8; MAX_BLOCK];
    let ch = read_node(rw, sb, child, h.depth - 1, &mut buf)?;
    let bs = sb.block_size as usize;
    match add(rw, sb, &mut buf[..bs], ch, x, taken, false)? {
        Added::Changed => {
            rw.write_at(child * bs as u64, &buf[..bs])?;
            Ok(Added::Unchanged)
        }
        Added::Unchanged => Ok(Added::Unchanged),
        Added::Split(first, blk) => {
            if h.entries < h.max {
                write_index(node, h.entries, first, blk);
                node[2..4].copy_from_slice(&((h.entries + 1) as u16).to_le_bytes());
                Ok(Added::Changed)
            } else if is_root {
                Ok(Added::Split(first, blk))
            } else {
                let up = new_node(rw, sb, taken, h.depth, |n| {
                    write_index(n, 0, first, blk);
                    1
                })?;
                Ok(Added::Split(first, up))
            }
        }
    }
}

/// The root one level deeper: its entries moved into a new block below it — with `extra`, an index
/// entry for a new sibling, after them — and the root an index over that block alone.
fn deeper<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    root: &[u8; 60],
    h: Header,
    extra: Option<(u64, u64)>,
    taken: &mut Taken,
) -> Result<[u8; 60], FsError> {
    if h.depth >= MAX_DEPTH {
        return Err(FsError::Unsupported); // ext4 makes no deeper tree
    }
    if h.entries == 0 || (extra.is_some() && h.depth == 0) {
        return Err(FsError::Corrupt);
    }
    let first = first_of(root, h, 0)?;
    let blk = new_node(rw, sb, taken, h.depth, |n| {
        n[ENTRY..at(h.entries)].copy_from_slice(&root[ENTRY..at(h.entries)]);
        match extra {
            Some((f, child)) => {
                write_index(n, h.entries, f, child);
                h.entries + 1
            }
            None => h.entries,
        }
    })?;
    let mut new = [0u8; 60];
    set_header(&mut new, 1, ROOT_ENTRIES, h.depth + 1);
    write_index(&mut new, 0, first, blk);
    Ok(new)
}

/// Take a block for a new tree node `depth` levels above the leaves, holding the entries `fill`
/// writes — it returns how many — and write it.
///
/// **Its goal is the first data block**, so tree blocks fill the earliest gaps rather than the
/// block a file's next data would continue into.
fn new_node<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    taken: &mut Taken,
    depth: u16,
    fill: impl FnOnce(&mut [u8]) -> usize,
) -> Result<u64, FsError> {
    let bs = sb.block_size as usize;
    let blk = alloc_block(rw, sb, sb.first_data_block as u64)?;
    let mut buf = [0u8; MAX_BLOCK];
    let n = fill(&mut buf[..bs]);
    set_header(&mut buf[..bs], n, block_cap(sb), depth);
    if let Err(e) = rw.write_at(blk * bs as u64, &buf[..bs]) {
        let _ = free_block(rw, sb, blk);
        return Err(e);
    }
    taken.blocks[taken.n] = blk;
    taken.n += 1;
    Ok(blk)
}

// --- cutting and freeing ---------------------------------------------------------

/// **Cut the tree at logical block `keep`**: free every block of the file from `keep` on, drop the
/// extents that held them and shorten the one across it, free each tree block left empty, and fold
/// what is left back into the inode while it fits there. `i_blocks` loses everything freed.
///
/// Cut at `0`, it frees the file whole and leaves the inode an empty leaf.
pub(crate) fn truncate<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    inode: &mut [u8; 256],
    keep: u64,
) -> Result<(), FsError> {
    let mut root = [0u8; 60];
    root.copy_from_slice(&inode[ROOT]);
    let h = header(&root, ROOT_ENTRIES)?;
    let mut freed = 0u64;
    let done = cut(rw, sb, &mut root, h, keep, &mut freed).and_then(|_| fold(rw, sb, &mut root, &mut freed));
    // Whatever was freed before an error is gone from the tree too, so the inode takes both.
    inode[ROOT].copy_from_slice(&root);
    count(inode, sb, -(freed as i64));
    done
}

/// Cut `node` at `keep`, returning whether it changed. A child left empty is freed and dropped; a
/// node left empty itself is its caller's to free.
fn cut<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    node: &mut [u8],
    h: Header,
    keep: u64,
    freed: &mut u64,
) -> Result<bool, FsError> {
    let bs = sb.block_size as usize;
    let mut n = h.entries;
    let mut changed = false;
    while n > 0 {
        if h.depth == 0 {
            let x = read_extent(node, n - 1)?;
            if x.end() <= keep {
                break;
            }
            if x.lblock >= keep {
                free_run(rw, sb, x.phys, x.len, freed)?;
                node[at(n - 1)..at(n)].fill(0);
                n -= 1;
                changed = true;
                continue;
            }
            let kept = keep - x.lblock;
            free_run(rw, sb, x.phys + kept, x.len - kept, freed)?;
            write_extent(node, n - 1, Extent { len: kept, ..x });
            changed = true;
            break;
        }
        let (first, child) = read_index(node, n - 1);
        if first >= keep {
            free_tree(rw, sb, child, h.depth - 1, freed)?;
            node[at(n - 1)..at(n)].fill(0);
            n -= 1;
            changed = true;
            continue;
        }
        // The child across `keep`. Every child before it ends before `first`, so it is the last.
        let mut buf = [0u8; MAX_BLOCK];
        let ch = read_node(rw, sb, child, h.depth - 1, &mut buf)?;
        if cut(rw, sb, &mut buf[..bs], ch, keep, freed)? {
            // Emptied only when its index entry's key is below its first extent, which `e2fsck`
            // calls invalid and neither ext4 nor this server writes — but which reads, so it is
            // freed rather than left in the tree with nothing in it.
            if rd_u16(&buf, 2) == 0 {
                free_block(rw, sb, child)?;
                *freed += 1;
                node[at(n - 1)..at(n)].fill(0);
                n -= 1;
                changed = true;
            } else {
                rw.write_at(child * bs as u64, &buf[..bs])?;
            }
        }
        break;
    }
    if changed {
        node[2..4].copy_from_slice(&(n as u16).to_le_bytes());
    }
    Ok(changed)
}

/// Free tree block `blk`, `depth` levels above the leaves, and everything under it.
fn free_tree<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    blk: u64,
    depth: u16,
    freed: &mut u64,
) -> Result<(), FsError> {
    let mut buf = [0u8; MAX_BLOCK];
    let h = read_node(rw, sb, blk, depth, &mut buf)?;
    for i in 0..h.entries {
        if depth == 0 {
            let x = read_extent(&buf, i)?;
            free_run(rw, sb, x.phys, x.len, freed)?;
        } else {
            free_tree(rw, sb, read_index(&buf, i).1, depth - 1, freed)?;
        }
    }
    free_block(rw, sb, blk)?;
    *freed += 1;
    Ok(())
}

/// Free `len` blocks from `phys`, refusing a run that is not all on the filesystem: one
/// [`ext4::free_run`], a bitmap pass a group (Phase 6 Part H.4), where it was one a block.
fn free_run<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    phys: u64,
    len: u64,
    freed: &mut u64,
) -> Result<(), FsError> {
    if !on_device(sb, phys, len) {
        return Err(FsError::Corrupt);
    }
    crate::ext4::free_run(rw, sb, phys, len)?;
    *freed += len;
    Ok(())
}

/// Fold the tree back into the inode while it fits there: an index root over one child of four
/// entries or fewer takes them in the child's place, and an index root over nothing becomes an
/// empty leaf.
///
/// Without it a cut leaves a tree `e2fsck` calls one level too deep, which it offers to rebuild —
/// and an undone grow a tree the inode on the disk does not describe.
fn fold<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    root: &mut [u8; 60],
    freed: &mut u64,
) -> Result<(), FsError> {
    loop {
        let h = header(root, ROOT_ENTRIES)?;
        if h.depth == 0 || h.entries > 1 {
            return Ok(());
        }
        let mut new = [0u8; 60];
        if h.entries == 0 {
            set_header(&mut new, 0, ROOT_ENTRIES, 0);
            *root = new;
            return Ok(());
        }
        let (_, child) = read_index(root, 0);
        let mut buf = [0u8; MAX_BLOCK];
        let ch = read_node(rw, sb, child, h.depth - 1, &mut buf)?;
        if ch.entries > ROOT_ENTRIES {
            return Ok(());
        }
        set_header(&mut new, ch.entries, ROOT_ENTRIES, ch.depth);
        new[ENTRY..at(ch.entries)].copy_from_slice(&buf[ENTRY..at(ch.entries)]);
        *root = new;
        free_block(rw, sb, child)?;
        *freed += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ext4::{self, inode_offset, read_superblock, resolve_path_ino};
    use crate::test_support::{ImageReader, RwImage, TEST_NOW, assert_e2fsck_clean, fixture_blocks};
    use crate::BlockRun;
    use std::cell::RefCell;

    /// How many levels the tree at `path` has below its inode.
    fn depth(rw: &RwImage, path: &[u8]) -> u16 {
        let sb = read_superblock(rw).unwrap();
        let (_, inode) = resolve_path_ino(rw, &sb, path).unwrap();
        header(&inode[ROOT], ROOT_ENTRIES).unwrap().depth
    }

    fn free_blocks(rw: &RwImage) -> u32 {
        rd_u32(&rw.0.borrow()[1024..2048], 12)
    }

    fn free_inodes(rw: &RwImage) -> u32 {
        rd_u32(&rw.0.borrow()[1024..2048], 16)
    }

    /// Runs `(logical, device, length)` with each that continues the one before it merged in, so
    /// two descriptions of the same blocks compare equal however they split them.
    fn merged(runs: impl IntoIterator<Item = (u64, u64, u64)>) -> Vec<(u64, u64, u64)> {
        let mut out: Vec<(u64, u64, u64)> = Vec::new();
        for (l, p, n) in runs {
            match out.last_mut() {
                Some(last) if last.0 + last.2 == l && last.1 + last.2 == p => last.2 += n,
                _ => out.push((l, p, n)),
            }
        }
        out
    }

    /// The written blocks of `path` as this server maps them for the kernel.
    fn ours<R: BlockReader>(r: &R, path: &[u8]) -> Vec<(u64, u64, u64)> {
        let mut runs = vec![BlockRun::default(); 8192];
        let m = ext4::map_file(r, path, &mut runs).unwrap();
        merged(
            runs[..m.runs]
                .iter()
                .filter(|r| r.device_lba != 0)
                .map(|r| (r.file_block, r.device_lba, r.length as u64)),
        )
    }

    /// **`debugfs`' reading of `path`'s extents in `img`** — `(logical, device, length,
    /// unwritten)` from its leaf rows: e2fsprogs' reading of the tree, the oracle this one is held
    /// to. A row is `level/max entry/entries logical - end physical - end length [Uninit]`; an
    /// index row's level is below its max, and it names one block rather than a range.
    fn debugfs_extents(img: &[u8], path: &str) -> Vec<(u64, u64, u64, bool)> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let f = std::env::temp_dir().join(format!(
            "nitrox-extent-{}-{}.img",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&f, img).unwrap();
        let out = std::process::Command::new("debugfs")
            .args(["-R", &format!("ex {path}")])
            .arg(&f)
            .output()
            .expect("debugfs (e2fsprogs) must be installed to run fs-server-ext4 tests");
        let _ = std::fs::remove_file(&f);
        let mut found = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines().skip(1) {
            let row = line.replace('/', " ");
            let t: Vec<&str> = row.split_whitespace().collect();
            if t.len() < 11 || t[0] != t[1] {
                continue; // an index row
            }
            let n = |i: usize| t[i].parse::<u64>().unwrap();
            found.push((n(4), n(7), n(10), line.contains("Uninit")));
        }
        found
    }

    /// The written blocks of `path` as `debugfs` reads them.
    fn theirs(img: &[u8], path: &str) -> Vec<(u64, u64, u64)> {
        merged(debugfs_extents(img, path).into_iter().filter(|e| !e.3).map(|e| (e.0, e.1, e.2)))
    }

    /// This server's map of `path` is `debugfs`', and `e2fsck` finds nothing.
    fn agrees(rw: &RwImage, path: &str, tag: &str) -> Vec<(u64, u64, u64)> {
        let img = rw.0.borrow().clone();
        let map = ours(rw, path.as_bytes());
        assert_eq!(map, theirs(&img, path), "{tag}: this server's map is not debugfs'");
        assert_e2fsck_clean(&img, tag);
        map
    }

    /// How many blocks a map covers.
    fn blocks_in(map: &[(u64, u64, u64)]) -> u64 {
        map.iter().map(|r| r.2).sum()
    }

    /// Remove `name` from directory `dir`, both halves: the name, then the inode.
    fn remove(rw: &RwImage, dir: &[u8], name: &[u8]) {
        let d = ext4::resolve_dir(rw, dir).unwrap();
        if let Some(ino) = ext4::unlink_at(rw, d, name, TEST_NOW).unwrap() {
            ext4::release_inode(rw, ino, TEST_NOW).unwrap();
        }
    }

    /// **Part H's `small/`: 4,096 files of 16 KiB in one directory.** Each file's data lands
    /// between the directory's growth blocks, so every directory block is an extent of its own, and
    /// until the tree the inode's four ran out at about the 815th file. Created, listed, looked up
    /// and removed, with `e2fsck` clean after each half.
    #[test]
    fn four_thousand_files_with_data_fit_in_one_directory() {
        // 64 MiB of data and the inodes for it, in 80 MiB.
        let rw = RwImage(RefCell::new(fixture_blocks(4096, 20480, 0, b"x\n")));
        let name = |i: usize| format!("f{i:04}.bin");
        for i in 0..4096 {
            ext4::create_file(&rw, b"/system", name(i).as_bytes(), TEST_NOW)
                .unwrap_or_else(|e| panic!("create {i}: {e:?}"));
            ext4::grow_file(&rw, format!("/system/{}", name(i)).as_bytes(), 16384, TEST_NOW)
                .unwrap_or_else(|e| panic!("grow {i}: {e:?}"));
        }
        assert!(depth(&rw, b"/system") >= 1, "a directory of 4,096 files with data is a tree");

        let sys = ext4::resolve_dir(&rw, b"/system").unwrap();
        let count = |rw: &RwImage| {
            let mut n = 0;
            ext4::read_dir(rw, sys, 0, |_, _, name| {
                n += (name.starts_with(b"f") && name.ends_with(b".bin")) as usize;
                true
            })
            .unwrap();
            n
        };
        assert_eq!(count(&rw), 4096, "every file listed");
        for i in [0, 814, 815, 816, 2048, 4095] {
            let path = format!("/system/{}", name(i));
            assert_eq!(ext4::stat_file(&rw, path.as_bytes()), Ok(16384), "{path}");
        }
        assert_e2fsck_clean(&rw.0.borrow(), "small-made");

        for i in 0..4096 {
            remove(&rw, b"/system", name(i).as_bytes());
        }
        assert_eq!(count(&rw), 0, "every file removed");
        assert_e2fsck_clean(&rw.0.borrow(), "small-removed");
    }

    /// **Two files grown a block at a time in turn**, so every block of each is an extent of its
    /// own: past a full depth 1 — four leaves — into depth 2, at both block sizes. The map agrees
    /// with `debugfs`' and `e2fsck` is clean at every step: grown, cut to a point inside each
    /// level, grown again, emptied, and both files removed with every block and inode given back.
    #[test]
    fn files_grown_in_turn_build_a_tree_two_levels_deep_and_cut_back() {
        // A leaf holds 84 extents at 1 KiB and 340 at 4 KiB, so depth 1 ends at 336 and 1,360.
        for (bs, blocks, fs_blocks) in [(1024u32, 400u64, 8192u32), (4096, 1400, 8192)] {
            let rw = RwImage(RefCell::new(fixture_blocks(bs, fs_blocks, 0, b"x\n")));
            let tag = |step: &str| format!("turns-{bs}-{step}");
            let (blocks_before, inodes_before) = (free_blocks(&rw), free_inodes(&rw));
            for f in [&b"a"[..], b"b"] {
                ext4::create_file(&rw, b"/", f, TEST_NOW).unwrap();
            }
            for i in 1..=blocks {
                for f in [&b"/a"[..], b"/b"] {
                    ext4::grow_file(&rw, f, (i * bs as u64) as usize, TEST_NOW)
                        .unwrap_or_else(|e| panic!("{bs}: growing to {i} blocks: {e:?}"));
                }
            }
            assert_eq!(depth(&rw, b"/a"), 2, "{bs}: {blocks} extents need two levels");
            let map = agrees(&rw, "/a", &tag("grown"));
            assert_eq!((map.len() as u64, blocks_in(&map)), (blocks, blocks), "{bs}: a block per extent");
            agrees(&rw, "/b", &tag("grown-b"));

            // Inside depth 1: three leaves, so the inode folds back to holding their index.
            ext4::truncate_file(&rw, b"/a", (blocks / 2 * bs as u64) as usize, TEST_NOW).unwrap();
            assert_eq!(depth(&rw, b"/a"), 1, "{bs}: cut to three leaves");
            assert_eq!(blocks_in(&agrees(&rw, "/a", &tag("half"))), blocks / 2);
            // Three extents fit in the inode itself.
            ext4::truncate_file(&rw, b"/a", 3 * bs as usize, TEST_NOW).unwrap();
            assert_eq!(depth(&rw, b"/a"), 0, "{bs}: cut to three extents");
            assert_eq!(blocks_in(&agrees(&rw, "/a", &tag("three"))), 3);
            // Grown again into the gaps the cut left between `b`'s blocks, and emptied.
            ext4::grow_file(&rw, b"/a", (blocks * bs as u64) as usize, TEST_NOW).unwrap();
            assert_eq!(blocks_in(&agrees(&rw, "/a", &tag("regrown"))), blocks);
            ext4::truncate_file(&rw, b"/a", 0, TEST_NOW).unwrap();
            assert_eq!((depth(&rw, b"/a"), blocks_in(&agrees(&rw, "/a", &tag("empty")))), (0, 0));

            remove(&rw, b"/", b"a");
            remove(&rw, b"/", b"b");
            assert_eq!((free_blocks(&rw), free_inodes(&rw)), (blocks_before, inodes_before), "{bs}");
            assert_e2fsck_clean(&rw.0.borrow(), &tag("removed"));
        }
    }

    /// **A tree e2fsprogs wrote**: `mke2fs -d` writes a file whose every other block is zero
    /// sparse, an extent per data block — 450 of them, two levels deep at 1 KiB. Read whole and
    /// mapped as `debugfs` maps it, cut through each level, and — on a second copy — removed whole,
    /// which until Part H took the file's name and freed nothing.
    #[test]
    fn a_tree_e2fsprogs_wrote_is_read_cut_and_freed() {
        let mut content = vec![0u8; 900 * 1024];
        for b in (0..900).step_by(2) {
            content[b * 1024..(b + 1) * 1024].fill(0x5A);
        }
        let img = fixture_blocks(1024, 8192, 0, &content);
        let path = "/system/current-generation";

        let rw = RwImage(RefCell::new(img.clone()));
        assert_eq!(depth(&rw, path.as_bytes()), 2, "e2fsprogs wrote 450 extents two levels deep");
        let mut out = vec![0x11u8; content.len()];
        assert_eq!(ext4::read_file_range(&rw, path.as_bytes(), 0, out.len(), &mut out), Ok(out.len()));
        assert!(out == content, "the file reads as written, holes as zeroes");
        assert_eq!(blocks_in(&agrees(&rw, path, "e2fs-read")), 450);
        // Its leaves hold 83 each. Inside depth 2, then inside depth 1, then inside the inode.
        for (keep, want_depth) in [(801u64, 2u16), (600, 1), (5, 0), (0, 0)] {
            ext4::truncate_file(&rw, path.as_bytes(), (keep * 1024) as usize, TEST_NOW).unwrap();
            assert_eq!(depth(&rw, path.as_bytes()), want_depth, "cut to {keep} blocks");
            assert_eq!(blocks_in(&agrees(&rw, path, &format!("e2fs-cut-{keep}"))), keep.div_ceil(2));
        }

        let rw = RwImage(RefCell::new(img));
        let (blocks_before, inodes_before) = (free_blocks(&rw), free_inodes(&rw));
        remove(&rw, b"/system", b"current-generation");
        // 450 data blocks, six leaves and an index block.
        assert_eq!(free_blocks(&rw), blocks_before + 457);
        assert_eq!(free_inodes(&rw), inodes_before + 1);
        assert_e2fsck_clean(&rw.0.borrow(), "e2fs-removed");
    }

    /// Patch the extent of the fixture's one-block file to `ee_len` and its size to `size`,
    /// returning the image and the block the extent starts at. The file's block holds `0xAB`, and
    /// the filesystem has room past it for an extent of 32,768 blocks.
    fn patched(ee_len: u16, size: u64) -> (Vec<u8>, u64) {
        let mut img = fixture_blocks(4096, 40000, 0, &[0xAB; 4096]);
        let r = ImageReader(img.clone());
        let sb = read_superblock(&r).unwrap();
        let (ino, inode) = resolve_path_ino(&r, &sb, b"/system/current-generation").unwrap();
        let x = read_extent(&inode[ROOT], 0).unwrap();
        assert_eq!((rd_u16(&inode, 42), x.lblock, x.len), (1, 0, 1), "one extent of one block");
        let at = inode_offset(&r, &sb, ino).unwrap() as usize;
        img[at + 4..at + 8].copy_from_slice(&(size as u32).to_le_bytes()); // i_size_lo
        img[at + 40 + 16..at + 40 + 18].copy_from_slice(&ee_len.to_le_bytes()); // ee_len
        (img, x.phys)
    }

    /// **`ee_len` as ext4 defines it**, read against `debugfs` on bytes this server did not write:
    /// `0x8000` is a written extent of 32,768 blocks, which the reader once masked to nothing and
    /// read as a hole; `0x8001` is an unwritten extent of one block, which reads as zeroes and
    /// maps as a hole — the reader once returned the block's old contents.
    #[test]
    fn a_full_length_extent_and_an_unwritten_one_read_as_e2fsprogs_reads_them() {
        let path = "/system/current-generation";
        let (img, p) = patched(0x8000, MAX_LEN * 4096);
        assert_eq!(debugfs_extents(&img, path), vec![(0, p, MAX_LEN, false)], "e2fsprogs' reading");
        let r = ImageReader(img);
        assert_eq!(ours(&r, path.as_bytes()), vec![(0, p, MAX_LEN)]);
        let mut block = [0u8; 4096];
        assert_eq!(ext4::read_file_range(&r, path.as_bytes(), 0, 4096, &mut block), Ok(4096));
        assert_eq!(block, [0xAB; 4096], "the extent's first block, not a hole");

        let (img, p) = patched(0x8001, 4096);
        assert_eq!(debugfs_extents(&img, path), vec![(0, p, 1, true)], "e2fsprogs' reading");
        let r = ImageReader(img);
        let mut runs = [BlockRun::default(); 4];
        let m = ext4::map_file(&r, path.as_bytes(), &mut runs).unwrap();
        assert_eq!((m.runs, runs[0].device_lba), (1, 0), "an unwritten extent maps as a hole");
        assert_eq!(ext4::read_file_range(&r, path.as_bytes(), 0, 4096, &mut block), Ok(4096));
        assert_eq!(block, [0; 4096], "and reads as zeroes, not as the 0xAB its block holds");
    }

    /// **The writer's half of the same rule**: an extent grows by appends to 32,768 blocks, stored
    /// as `0x8000`, and the next block starts another — where the reader that masked the top bit
    /// read the full extent's length as nothing.
    #[test]
    fn an_extent_grows_to_32768_blocks_and_the_next_block_starts_another() {
        let rw = RwImage(RefCell::new(fixture_blocks(1024, 40000, 0, b"x\n")));
        let sb = read_superblock(&rw).unwrap();
        let mut inode = [0u8; 256];
        empty_root(&mut inode);
        let p = 2000;
        append(&rw, &sb, &mut inode, Extent::written(0, p, MAX_LEN - 1)).unwrap();
        append(&rw, &sb, &mut inode, Extent::written(MAX_LEN - 1, p + MAX_LEN - 1, 1)).unwrap();
        assert_eq!(rd_u16(&inode, 42), 1, "merged into one extent");
        assert_eq!(rd_u16(&inode, 56), 0x8000, "32,768 written blocks are stored as 0x8000");
        append(&rw, &sb, &mut inode, Extent::written(MAX_LEN, p + MAX_LEN, 1)).unwrap();
        assert_eq!(rd_u16(&inode, 42), 2, "one block past the longest is another extent");

        let mut map = Lookup::new(&inode);
        assert_eq!(map.seg(&rw, &sb, 0).unwrap(), Seg { phys: p, len: MAX_LEN });
        assert_eq!(map.block(&rw, &sb, MAX_LEN - 1).unwrap(), p + MAX_LEN - 1);
        assert_eq!(map.block(&rw, &sb, MAX_LEN).unwrap(), p + MAX_LEN);
        assert_eq!(rd_u32(&inode, 28) as u64, (MAX_LEN + 1) * 2, "i_blocks, in 512-byte units");
    }

    /// **A grow the filesystem cannot hold gives back everything it took**: on a file with no
    /// blocks, one whose four extents fill its inode — so the grow moved them into a leaf before
    /// it failed, and the undo must fold them back — and one that is already a tree. Free blocks,
    /// the map and `e2fsck` are as they were. Until Part H every block a failed grow had taken
    /// stayed in use, here all of the filesystem's free space.
    #[test]
    fn a_grow_the_filesystem_cannot_hold_gives_back_everything_it_took() {
        let rw = RwImage(RefCell::new(fixture_blocks(4096, 4096, 0, b"x\n")));
        ext4::create_file(&rw, b"/", b"big", TEST_NOW).unwrap();
        let before = free_blocks(&rw);
        assert_eq!(ext4::grow_file(&rw, b"/big", 32 << 20, TEST_NOW), Err(FsError::TooLarge));
        assert_eq!(free_blocks(&rw), before, "every block the grow took went back");
        assert_eq!(ext4::stat_file(&rw, b"/big"), Ok(0));
        assert_e2fsck_clean(&rw.0.borrow(), "grow-refused-empty");

        for (extents, want_depth) in [(4u64, 0u16), (12, 1)] {
            let rw = RwImage(RefCell::new(fixture_blocks(4096, 4096, 0, b"x\n")));
            for f in [&b"a"[..], b"b"] {
                ext4::create_file(&rw, b"/", f, TEST_NOW).unwrap();
            }
            for i in 1..=extents as usize {
                for f in [&b"/a"[..], b"/b"] {
                    ext4::grow_file(&rw, f, i * 4096, TEST_NOW).unwrap();
                }
            }
            assert_eq!(depth(&rw, b"/a"), want_depth);
            let map = agrees(&rw, "/a", &format!("grow-refused-{extents}-before"));
            let before = free_blocks(&rw);
            assert_eq!(ext4::grow_file(&rw, b"/a", 32 << 20, TEST_NOW), Err(FsError::TooLarge));
            assert_eq!(free_blocks(&rw), before, "{extents} extents: every block went back");
            assert_eq!(depth(&rw, b"/a"), want_depth, "{extents} extents: the tree's shape is back");
            assert_eq!(agrees(&rw, "/a", &format!("grow-refused-{extents}")), map);
            assert_eq!(ext4::stat_file(&rw, b"/a"), Ok(extents as usize * 4096));
        }
    }

    /// **Blocks past a file's end go before a grow appends**: an extent that runs past `i_size`
    /// — `fallocate` with the size kept leaves one — would otherwise sit under the blocks the grow
    /// appends, and the tree would map one logical block twice.
    #[test]
    fn blocks_past_the_end_go_before_a_grow() {
        let path = "/system/current-generation";
        let rw = RwImage(RefCell::new(fixture_blocks(4096, 4096, 0, b"x\n")));
        ext4::grow_file(&rw, path.as_bytes(), 3 * 4096, TEST_NOW).unwrap();
        // The file says one block; its extent still holds three.
        let sb = read_superblock(&rw).unwrap();
        let (ino, _) = resolve_path_ino(&rw, &sb, path.as_bytes()).unwrap();
        let at = inode_offset(&rw, &sb, ino).unwrap();
        BlockWriter::write_at(&rw, at + 4, &4096u32.to_le_bytes()).unwrap();
        let before = free_blocks(&rw);

        ext4::grow_file(&rw, path.as_bytes(), 5 * 4096, TEST_NOW).unwrap();
        let map = agrees(&rw, path, "past-the-end");
        assert_eq!(blocks_in(&map), 5);
        // Two past the end went back, four were taken.
        assert_eq!(free_blocks(&rw), before + 2 - 4);
    }

    /// **A full index below the inode splits too**, and the inode takes the new index beside the
    /// old. Built with nodes of two entries, where ext4's hold 84 at the least: a full index below
    /// the inode is otherwise 7,056 extents away. `e2fsck` and `debugfs` refuse a node that small,
    /// so the tree is read back through [`Lookup`] — and cut back, folding two levels.
    #[test]
    fn a_full_index_below_the_inode_splits_too() {
        let rw = RwImage(RefCell::new(fixture_blocks(1024, 8192, 0, b"x\n")));
        let sb = read_superblock(&rw).unwrap();
        let initial = free_blocks(&rw);
        let take = || ext4::alloc_block(&rw, &sb, sb.first_data_block as u64).unwrap();
        let data: Vec<u64> = (0..5).map(|_| take()).collect();
        let (l1, l2, index) = (take(), take(), take());
        // Two leaves of two extents each, at logical 0, 2, 4 and 6, under an index of two.
        let mut node = [0u8; 1024];
        for (blk, from) in [(l1, 0usize), (l2, 2)] {
            node.fill(0);
            set_header(&mut node, 2, 2, 0);
            for i in 0..2 {
                write_extent(&mut node, i, Extent::written((from + i) as u64 * 2, data[from + i], 1));
            }
            BlockWriter::write_at(&rw, blk * 1024, &node).unwrap();
        }
        node.fill(0);
        set_header(&mut node, 2, 2, 1);
        write_index(&mut node, 0, 0, l1);
        write_index(&mut node, 1, 4, l2);
        BlockWriter::write_at(&rw, index * 1024, &node).unwrap();
        let mut inode = [0u8; 256];
        set_header(&mut inode[ROOT], 1, ROOT_ENTRIES, 2);
        write_index(&mut inode[ROOT], 0, 0, index);

        let before = free_blocks(&rw);
        append(&rw, &sb, &mut inode, Extent::written(8, data[4], 1)).unwrap();
        // A leaf for the extent and an index for that leaf, beside the old one in the inode.
        assert_eq!(free_blocks(&rw), before - 2);
        let root = header(&inode[ROOT], ROOT_ENTRIES).unwrap();
        assert_eq!((root.entries, root.depth), (2, 2));
        assert_eq!(read_index(&inode[ROOT], 1).0, 8, "the new index starts at the new extent");
        let mut map = Lookup::new(&inode);
        for (lb, &phys) in [0u64, 2, 4, 6, 8].iter().zip(&data) {
            assert_eq!(map.block(&rw, &sb, *lb).unwrap(), phys, "block {lb}");
            assert_eq!(map.block(&rw, &sb, lb + 1).unwrap(), 0, "the hole after block {lb}");
        }

        // Cut back to the four: the new leaf and index go, and the inode folds the old index in.
        truncate(&rw, &sb, &mut inode, 8).unwrap();
        assert_eq!(free_blocks(&rw), before + 1 + 1, "the extent's block, and the folded index");
        let root = header(&inode[ROOT], ROOT_ENTRIES).unwrap();
        assert_eq!((root.entries, root.depth), (2, 1), "the inode indexes the two leaves");
        let mut map = Lookup::new(&inode);
        assert_eq!(map.block(&rw, &sb, 6).unwrap(), data[3]);
        assert_eq!(map.block(&rw, &sb, 8).unwrap(), 0);
        // And to nothing: every block given back.
        truncate(&rw, &sb, &mut inode, 0).unwrap();
        assert_eq!(free_blocks(&rw), initial);
        assert_eq!(header(&inode[ROOT], ROOT_ENTRIES).unwrap().entries, 0);
    }
}
