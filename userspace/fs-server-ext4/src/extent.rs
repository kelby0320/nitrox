//! **The extent tree**: how an inode maps its logical blocks to the device — looked up,
//! appended to, cut, and freed.
//!
//! An inode's `i_block` is a node of four entries; a file with more extents is a tree: the
//! inode's node an index over blocks, each a node of `(block_size - 12) / 12` entries (340 at 4
//! KiB), up to five levels. Every node is a 12-byte header and 12-byte entries: a leaf's are
//! extents (a run of logical blocks and where it starts on the device), an index's name a child
//! block and the first logical block under it. A directory needs the tree, since each file's data
//! lands between its blocks and so every directory block is an extent.
//!
//! What changes a tree here:
//! - [`append`] adds an extent at the file's end, merging it into the last when they continue
//!   each other. A full leaf gains a sibling to its right holding the new extent alone, so filled
//!   leaves stay full as in ext4's own appends; a full inode moves its entries into a block below
//!   it. **It is atomic**: on an error nothing it wrote is reachable and every tree block it took
//!   is given back.
//! - [`truncate`] cuts the tree at a logical block, freeing what is past it and every tree block
//!   left empty, and folds what is left back into the inode while it fits. It also frees a file
//!   whole (cut at `0`) and undoes a failed grow.
//! - [`Lookup`] answers where a block is, reading each leaf once as a walk passes it.
//!
//! **An extent's length is ext4's**: an `ee_len` up to 32,768 is a written extent of that many
//! blocks, and one above it an *unwritten* extent of `ee_len - 32768` (allocated, never written,
//! as `fallocate` leaves one), which reads as zeroes and is a hole to the kernel too
//! (`filesystem-data-path.md`).
//!
//! No `alloc`: a walk holds one block per level on the stack, a cut two, at most [`MAX_DEPTH`]
//! levels below the inode.

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

fn at(i: usize) -> usize {
    ENTRY + i * ENTRY
}

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

fn write_index(node: &mut [u8], i: usize, first: u64, child: u64) {
    let e = at(i);
    node[e..e + 4].copy_from_slice(&(first as u32).to_le_bytes());
    node[e + 4..e + 8].copy_from_slice(&(child as u32).to_le_bytes());
    node[e + 8..e + 10].copy_from_slice(&((child >> 32) as u16).to_le_bytes());
    node[e + 10..e + 12].fill(0);
}

fn first_of(node: &[u8], h: Header, i: usize) -> Result<u64, FsError> {
    Ok(if h.depth == 0 { read_extent(node, i)?.lblock } else { read_index(node, i).0 })
}

/// Whether `len` blocks from `phys` are all on the filesystem, past its first data block.
fn on_device(sb: &Superblock, phys: u64, len: u64) -> bool {
    phys >= sb.first_data_block as u64 && phys != 0 && phys.saturating_add(len) <= sb.blocks_count
}

/// Check that `node`'s keys rise and lie in `keys`, the range its parent's entry gives it. So a
/// block holding entries has one place in the tree, and a walk meets it once.
fn keys_within(node: &[u8], h: Header, keys: (u64, u64)) -> Result<(), FsError> {
    let mut floor = keys.0;
    for i in 0..h.entries {
        let (first, end) = if h.depth == 0 {
            let x = read_extent(node, i)?;
            (x.lblock, x.end())
        } else {
            let first = read_index(node, i).0;
            (first, first + 1)
        };
        if first < floor || end > keys.1 {
            return Err(FsError::Corrupt);
        }
        floor = end;
    }
    Ok(())
}

/// Read tree block `blk` into `buf`: it must be a node `depth` levels above the leaves, its keys in
/// `keys`.
fn read_node<R: BlockReader>(
    r: &R,
    sb: &Superblock,
    blk: u64,
    depth: u16,
    keys: (u64, u64),
    buf: &mut [u8; MAX_BLOCK],
) -> Result<Header, FsError> {
    if !on_device(sb, blk, 1) {
        return Err(FsError::Corrupt);
    }
    let bs = sb.block_size as usize;
    r.read_meta(blk * bs as u64, &mut buf[..bs])?;
    let h = header(&buf[..bs], block_cap(sb))?;
    if h.depth != depth {
        return Err(FsError::Corrupt);
    }
    keys_within(&buf[..bs], h, keys)?;
    Ok(h)
}

/// The keys entry `i` of `node` gives its child: from its own to the next entry's, or to `hi`.
fn child_keys(node: &[u8], h: Header, i: usize, hi: u64) -> (u64, u64) {
    (read_index(node, i).0, if i + 1 < h.entries { read_index(node, i + 1).0 } else { hi })
}

/// Move an inode's `i_blocks`, in 512-byte units, by `blocks` filesystem blocks.
fn count(inode: &mut [u8; 256], sb: &Superblock, blocks: i64) {
    let units = blocks * (sb.block_size / 512) as i64;
    let v = (rd_u32(inode, 28) as i64 + units).clamp(0, u32::MAX as i64) as u32;
    inode[28..32].copy_from_slice(&v.to_le_bytes());
}

// --- looking blocks up ---

/// **Where a file's blocks are**, for a walk through them in order: a leaf is read when the walk
/// reaches it and kept while it stays in it, so a tree costs a read per leaf, not a descent per
/// block.
pub(crate) struct Lookup {
    root: [u8; 60],
    leaf: [u8; MAX_BLOCK],
    /// The leaf in `leaf`, once one is read.
    span: Option<Span>,
}

/// A leaf held by a [`Lookup`]: its bytes, entry capacity, and the logical blocks `lo..hi` it
/// covers.
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

    pub(crate) fn block<R: BlockReader>(&mut self, r: &R, sb: &Superblock, lb: u64) -> Result<u64, FsError> {
        Ok(self.seg(r, sb, lb)?.phys)
    }

    /// What is at logical block `lb`, and how far it goes on.
    pub(crate) fn seg<R: BlockReader>(&mut self, r: &R, sb: &Superblock, lb: u64) -> Result<Seg, FsError> {
        if lb >= END {
            return Ok(Seg { phys: 0, len: u64::MAX - lb }); // no extent reaches past 2^32 blocks
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

    /// Walk from the root to the leaf covering `lb`, keeping each index's bounds so the leaf's span
    /// is exact.
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
            let mut pick = None;
            for i in 0..h.entries {
                if read_index(node, i).0 > lb {
                    break;
                }
                pick = Some(i);
            }
            let Some(i) = pick else {
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
            r.read_meta(child * bs as u64, &mut self.leaf[..bs])?;
            (bytes, cap) = (bs, block_cap(sb));
            depth -= 1;
        }
    }
}

/// The device block just past the file's last extent, where a block that continues it would go.
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
        let (first, child) = read_index(&buf, h.entries - 1);
        h = read_node(r, sb, child, h.depth - 1, (first, END), &mut buf)?;
    }
}

// --- appending ---

/// The tree blocks an [`append`] has taken, for giving back if it fails: a split at every level
/// below the root, and one to deepen it.
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
    /// The node was full: a new sibling to its right (block `.1`, first logical block `.0`) holds
    /// the extent, and the node itself is as it was.
    Split(u64, u64),
}

/// **Append `x` at the file's end**, at or past the end of the last extent. Merged into the last
/// extent when it continues it; otherwise added beside it, splitting a full leaf or deepening a
/// full inode. `i_blocks` counts `x` and every tree block taken.
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
    // A full leaf in the inode moves below it first, so the leaf `x` joins has room (the shape
    // ext4's `ext4_ext_grow_indepth` leaves).
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
    let (first, child) = read_index(node, h.entries - 1);
    let mut buf = [0u8; MAX_BLOCK];
    let ch = read_node(rw, sb, child, h.depth - 1, (first, END), &mut buf)?;
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

/// The root one level deeper: its entries moved into a new block below it (with `extra`, an index
/// entry for a new sibling, after them) and the root an index over that block alone.
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
/// writes (it returns how many), and write it. **Its goal is the first data block**, so tree blocks
/// fill the earliest gaps rather than the block a file's next data would continue into.
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

// --- cutting and freeing ---

/// **Cut the tree at logical block `keep`**: free every block of the file from `keep` on, drop the
/// extents that held them and shorten the one across it, free each tree block left empty, and fold
/// what is left back into the inode while it fits. `i_blocks` loses everything freed.
///
/// **`at` is where the inode is on the disk**: whenever the root drops something, the inode is
/// written there with the new root before what it dropped is freed, as each tree block is, so no
/// write lost after leaves it mapping a freed block. Its caller writes it again afterwards, with
/// `i_blocks` and whatever else changed. `None` for an inode already freed, whose blocks an error
/// here only leaks.
pub(crate) fn truncate<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    inode: &mut [u8; 256],
    keep: u64,
    at: Option<u64>,
) -> Result<(), FsError> {
    let mut root = [0u8; 60];
    root.copy_from_slice(&inode[ROOT]);
    let h = header(&root, ROOT_ENTRIES)?;
    keys_within(&root, h, (0, END))?;
    let mut freed = 0u64;
    let before = *inode;
    let mut persist = |root: &[u8]| match at {
        Some(off) => {
            let mut bytes = before;
            bytes[ROOT].copy_from_slice(&root[..60]);
            rw.write_at(off, &bytes[..sb.inode_len()])
        }
        None => Ok(()),
    };
    let done = cut(rw, sb, &mut root, h, END, keep, &mut freed, &mut persist)
        .and_then(|_| fold(rw, sb, &mut root, &mut freed, &mut persist));
    // Whatever was freed before an error is gone from the tree too, so the inode takes both.
    inode[ROOT].copy_from_slice(&root);
    count(inode, sb, -(freed as i64));
    done
}

/// Cut `node`, whose keys run below `hi`, at `keep`, returning whether it changed. A child left
/// empty is freed and dropped; a node left empty itself is its caller's to free. `persist` writes
/// `node` where it lives.
///
/// **Out of the tree, then freed**: what `node` drops is gone from it, in its header and on the
/// disk, before any of it is freed, so a cut stopped by an error leaves a tree that maps nothing
/// it freed. An error freeing leaks what is left, which `e2fsck` gives back, rather than leaving
/// blocks free and still mapped for the next file.
#[allow(clippy::too_many_arguments, reason = "a node, its keys' bound, the cut, and where it lives")]
fn cut<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    node: &mut [u8],
    h: Header,
    hi: u64,
    keep: u64,
    freed: &mut u64,
    persist: &mut dyn FnMut(&[u8]) -> Result<(), FsError>,
) -> Result<bool, FsError> {
    let bs = sb.block_size as usize;
    // What the node keeps: the entries that begin below `keep`, each dropped one checked first.
    let mut n = h.entries;
    while n > 0 {
        if h.depth == 0 {
            let x = read_extent(node, n - 1)?;
            if x.lblock < keep {
                break;
            }
            if !on_device(sb, x.phys, x.len) {
                return Err(FsError::Corrupt);
            }
        } else if read_index(node, n - 1).0 < keep {
            break;
        }
        n -= 1;
    }
    let dropped = n;
    // The last entry kept may run past `keep`: a leaf's extent is shortened, freeing its tail; an
    // index's child is cut on its own block first, and dropped if that leaves it empty.
    let mut tail = None;
    let mut emptied = None;
    if n > 0 && h.depth == 0 {
        let x = read_extent(node, n - 1)?;
        if x.end() > keep {
            let kept = keep - x.lblock;
            if !on_device(sb, x.phys + kept, x.len - kept) {
                return Err(FsError::Corrupt);
            }
            write_extent(node, n - 1, Extent { len: kept, ..x });
            tail = Some((x.phys + kept, x.len - kept));
        }
    } else if n > 0 {
        let child = read_index(node, n - 1).1;
        let keys = child_keys(node, h, n - 1, hi);
        let mut buf = [0u8; MAX_BLOCK];
        let ch = read_node(rw, sb, child, h.depth - 1, keys, &mut buf)?;
        // Emptied only when its index key is below its first extent, which `e2fsck` calls invalid
        // and nothing here writes, but which reads: freed rather than left in the tree empty.
        let mut write = |node: &[u8]| rw.write_at(child * bs as u64, &node[..bs]);
        if cut(rw, sb, &mut buf[..bs], ch, keys.1, keep, freed, &mut write)? && rd_u16(&buf, 2) == 0 {
            emptied = Some(child);
            n -= 1;
        }
    }
    if n == h.entries && tail.is_none() {
        return Ok(false);
    }
    // What goes, kept aside, then out of the node: zeroed and counted gone, and on the disk.
    let mut gone = [0u8; MAX_BLOCK];
    gone[at(dropped)..at(h.entries)].copy_from_slice(&node[at(dropped)..at(h.entries)]);
    node[at(n)..at(h.entries)].fill(0);
    node[2..4].copy_from_slice(&(n as u16).to_le_bytes());
    persist(node)?;
    // Then freed.
    if let Some((phys, len)) = tail {
        free_run(rw, sb, phys, len, freed)?;
    }
    if let Some(child) = emptied {
        *freed += 1;
        free_block(rw, sb, child)?;
    }
    for i in dropped..h.entries {
        if h.depth == 0 {
            let x = read_extent(&gone, i)?;
            free_run(rw, sb, x.phys, x.len, freed)?;
        } else {
            let keys = child_keys(&gone, h, i, hi);
            free_tree(rw, sb, read_index(&gone, i).1, h.depth - 1, keys, freed)?;
        }
    }
    Ok(true)
}

/// Free tree block `blk`, `depth` levels above the leaves with its keys in `keys`, and everything
/// under it. **An empty one is `Corrupt`**: no keys place it, so it may be named elsewhere too.
fn free_tree<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    blk: u64,
    depth: u16,
    keys: (u64, u64),
    freed: &mut u64,
) -> Result<(), FsError> {
    let mut buf = [0u8; MAX_BLOCK];
    let h = read_node(rw, sb, blk, depth, keys, &mut buf)?;
    if h.entries == 0 {
        return Err(FsError::Corrupt);
    }
    for i in 0..h.entries {
        if depth == 0 {
            let x = read_extent(&buf, i)?;
            free_run(rw, sb, x.phys, x.len, freed)?;
        } else {
            free_tree(rw, sb, read_index(&buf, i).1, depth - 1, child_keys(&buf, h, i, keys.1), freed)?;
        }
    }
    *freed += 1;
    free_block(rw, sb, blk)
}

/// Free `len` blocks from `phys`, refusing a run not all on the filesystem. **Counted before it
/// is freed**: its caller has already taken it out of the tree, so `i_blocks` agrees with the
/// tree whether the free succeeds or not.
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
    *freed += len;
    crate::ext4::free_run(rw, sb, phys, len)
}

/// Fold the tree back into the inode while it fits there: an index root over one child of four
/// entries or fewer takes them in the child's place, and an index root over nothing becomes an
/// empty leaf; else a cut leaves a tree `e2fsck` calls one level too deep. The root is
/// persisted, as [`cut`] persists it, before the child it took in is freed.
fn fold<RW: BlockReader + BlockWriter>(
    rw: &RW,
    sb: &Superblock,
    root: &mut [u8; 60],
    freed: &mut u64,
    persist: &mut dyn FnMut(&[u8]) -> Result<(), FsError>,
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
        let (first, child) = read_index(root, 0);
        let mut buf = [0u8; MAX_BLOCK];
        let ch = read_node(rw, sb, child, h.depth - 1, (first, END), &mut buf)?;
        if ch.entries > ROOT_ENTRIES {
            return Ok(());
        }
        set_header(&mut new, ch.entries, ROOT_ENTRIES, ch.depth);
        new[ENTRY..at(ch.entries)].copy_from_slice(&buf[ENTRY..at(ch.entries)]);
        *root = new;
        persist(root)?;
        *freed += 1;
        free_block(rw, sb, child)?;
    }
}

#[cfg(test)]
mod tests;
