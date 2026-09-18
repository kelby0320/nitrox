//! Making an empty ext4 filesystem — **layout only** (Phase 5 Part H.2).
//!
//! The writer beside it ([`crate::ext4`]) populates a filesystem; this one creates the thing
//! it populates: superblock and backups, the group-descriptor table, per-group bitmaps and
//! inode tables, and a root directory. Nothing here allocates a file — `create_file` and
//! `mkdir_at` do that, against exactly the structures this laid out.
//!
//! It exists because `nxinstall` has to make a filesystem the size of a disk, and the only
//! other way to get one is `mke2fs` on a build machine — which is how H.1 shipped: it copied
//! a 24 MiB filesystem onto a 931 GiB partition and the filesystem did not know about the
//! space around it.
//!
//! ## What it declares, and therefore what it must produce
//!
//! ```text
//! incompat:   filetype extent
//! ro_compat:  sparse_super large_file huge_file dir_nlink extra_isize
//! ```
//!
//! **`sparse_super` is implemented**, because it is a rule rather than a layout: backup
//! superblocks and descriptor tables go in groups 0, 1, and the powers of 3, 5 and 7. On a
//! 931 GiB disk that is 15 groups rather than 7,600, which is 1.8 GiB of backups not written.
//!
//! **`flex_bg` is not**, and the feature bit is not set. It packs several groups' bitmaps and
//! inode tables together to make metadata sequential; it is an optimisation, and declaring it
//! without doing it would describe a layout that is not there. Our images from `mke2fs` do
//! have it, which is why `check-images` compares filesystems by their *contents* and not
//! their bytes.
//!
//! **`metadata_csum` is not**, matching every other image in this tree: the reader maintains
//! no checksums, and a filesystem that claimed them would be one `e2fsck` calls corrupt after
//! the first write.
//!
//! **No journal.** `has_journal` needs jbd2 and replay, which the server does not have.
//!
//! ## The oracle
//!
//! `e2fsck -fn` accepts what this writes, and [`crate::ext4`] reads it back. Neither alone is
//! enough: `e2fsck` would accept a filesystem our reader cannot parse, and our reader would
//! accept its own mistakes.

use crate::{BlockWriter, FsError};

/// Reserved inode numbers. 1 is the bad-blocks inode, 2 the root directory, 3–10 are
/// reserved by the format; the first inode a file may use is 11 (`s_first_ino`).
const FIRST_INO: u32 = 11;
/// Inode 2 is the root directory, by definition of the format.
const ROOT_INO: u32 = 2;
/// Bytes per inode record. 256 like every other image in this tree — 128 would drop the
/// nanosecond timestamps and the `extra_isize` the writer stamps.
const INODE_SIZE: u32 = 256;
/// `i_extra_isize`: how much of a 256-byte inode past the 128-byte base is in use.
const EXTRA_ISIZE: u16 = 32;
/// Group-descriptor size without `64bit`.
const DESC_SIZE: u32 = 32;

/// `s_feature_incompat`: `filetype` (directory entries carry a type byte) and `extent`.
const INCOMPAT: u32 = 0x0002 | 0x0040;
/// `s_feature_ro_compat`: `sparse_super`, `large_file`, `huge_file`, `dir_nlink`,
/// `extra_isize`.
const RO_COMPAT: u32 = 0x0001 | 0x0002 | 0x0008 | 0x0020 | 0x0040;

/// `EXT4_EXTENTS_FL`.
const EXTENTS_FL: u32 = 0x0008_0000;
/// The extent header's magic.
const EXTENT_MAGIC: u16 = 0xF30A;
/// `S_IFDIR | 0755`.
const ROOT_MODE: u16 = 0x41ED;

/// The smallest filesystem this will make: enough blocks for one group's metadata and a root
/// directory with somewhere to put entries.
const MIN_BLOCKS: u64 = 64;

/// What to make.
#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// Blocks in the filesystem — the partition's size divided by `block_size`.
    pub blocks: u64,
    /// Bytes per block: 1024, 2048 or 4096. The reader's scratch is one block.
    pub block_size: u32,
    /// Bytes of filesystem per inode, `mke2fs -i`. See [`Geometry::inodes_count`] for the cap
    /// that bounds what this can cost.
    pub bytes_per_inode: u32,
    /// The filesystem's UUID. **Caller-supplied**, like `libgpt`'s: this module has no
    /// entropy, and a filesystem whose UUID is a constant is one that collides with every
    /// other machine's.
    pub uuid: [u8; 16],
    /// The volume label, cut to 16 bytes. Not what a partition is found by — that is the GPT
    /// name — so this is for a person reading `dumpe2fs`.
    pub label: [u8; 16],
    /// Seconds since the epoch, for the superblock's times. The caller reads the clock.
    pub now: i64,
}

/// Why a filesystem cannot be laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MkfsError {
    /// A block size other than 1024, 2048 or 4096.
    BlockSize(u32),
    /// Fewer blocks than one group's metadata and a root directory need.
    TooSmall {
        /// Blocks asked for.
        have: u64,
        /// The minimum.
        need: u64,
    },
    /// The geometry needs more than 2^32 blocks — `64bit`, which the reader refuses.
    TooLarge,
}

impl MkfsError {
    /// The error a caller sees from [`format`].
    fn fs_error(self) -> FsError {
        match self {
            MkfsError::BlockSize(_) => FsError::Unsupported,
            MkfsError::TooSmall { .. } => FsError::TooLarge,
            MkfsError::TooLarge => FsError::TooLarge,
        }
    }
}

/// Where everything goes. Derived from [`Params`] and nothing else, so it is a pure function
/// of the request and can be checked without writing a byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// Bytes per block.
    pub block_size: u32,
    /// Blocks in the filesystem.
    pub blocks_count: u64,
    /// Block numbering origin: 1 for 1 KiB blocks (the superblock occupies block 0's second
    /// half and nothing else fits), 0 otherwise.
    pub first_data_block: u32,
    /// Blocks per group: `8 * block_size`, one bit per block in a one-block bitmap.
    pub blocks_per_group: u32,
    /// How many groups.
    pub groups: u32,
    /// Inodes per group.
    pub inodes_per_group: u32,
    /// Inodes in the filesystem.
    pub inodes_count: u32,
    /// Blocks of inode table per group.
    pub itable_blocks: u32,
    /// Blocks the group-descriptor table occupies.
    pub gdt_blocks: u32,
}

impl Geometry {
    /// Work out the layout for `p`, or say why it cannot be laid out.
    pub fn new(p: &Params) -> Result<Geometry, MkfsError> {
        if !matches!(p.block_size, 1024 | 2048 | 4096) {
            return Err(MkfsError::BlockSize(p.block_size));
        }
        if p.blocks > u32::MAX as u64 {
            return Err(MkfsError::TooLarge);
        }
        let first_data_block = if p.block_size == 1024 { 1 } else { 0 };
        let blocks_per_group = 8 * p.block_size;
        let addressable = p.blocks.saturating_sub(first_data_block as u64);
        if addressable < MIN_BLOCKS {
            return Err(MkfsError::TooSmall { have: p.blocks, need: MIN_BLOCKS });
        }
        // **The filesystem may be smaller than the partition**, by up to one short final
        // group — see the loop at the end of this function, which is why `blocks_count` is a
        // variable rather than `p.blocks`.
        let mut blocks_count = p.blocks;
        let mut groups =
            blocks_count.saturating_sub(first_data_block as u64).div_ceil(blocks_per_group as u64)
                as u32;
        let mut gdt_blocks =
            (groups as u64 * DESC_SIZE as u64).div_ceil(p.block_size as u64) as u32;

        // **Inodes: the ratio, then a cap.** The table is written out in full — there is no
        // `lazy_itable_init` without `metadata_csum` — so every inode costs 256 bytes of
        // zeroes written at install time. At `mke2fs`'s default ratio a 931 GiB disk asks for
        // 61 million of them, which is 16 GiB to write before the first file exists. A
        // million inodes is a generous ceiling for a personal machine's root and costs 256
        // MiB, so that is the cap.
        let by_ratio = p
            .blocks
            .saturating_mul(p.block_size as u64)
            / p.bytes_per_inode.max(1) as u64;
        let wanted = by_ratio.clamp(FIRST_INO as u64 + 1, MAX_INODES);
        // Round the per-group count so an inode table is a whole number of blocks, and keep
        // it at least one block: a group with a fraction of a block of inodes has nowhere to
        // put them. **Rounded down**, so the cap above is a ceiling rather than a target that
        // the rounding then overshoots — at one inode more per group than asked for, a
        // 7,452-group disk ends up 25,000 inodes over.
        let per_block = p.block_size / INODE_SIZE;
        let mut inodes_per_group = (wanted / groups as u64) as u32 / per_block * per_block;
        inodes_per_group = inodes_per_group.max(per_block);
        // A group cannot hold more inodes than it has blocks to describe them with.
        inodes_per_group = inodes_per_group.min(blocks_per_group / 2 * per_block);
        // **And no more than a group descriptor can count.** `bg_free_inodes_count_lo` is 16
        // bits, so a group of more than 65,535 inodes would have its free count truncated on
        // the way to disk and every descriptor would claim a number that is not true. No
        // in-tree caller gets near it — `bytes_per_inode` is 16384 everywhere — but `Params`
        // is public, and `bytes_per_inode = 512` on a single-group 4 KiB filesystem asks for
        // 262,144 (PR #310 review). Clamped here, where the number is chosen, rather than
        // where it is written out.
        inodes_per_group = inodes_per_group.min(u16::MAX as u32 / per_block * per_block);
        let itable_blocks = inodes_per_group / per_block;

        // **Drop a final group too short to hold its own metadata**, as `mke2fs` does.
        //
        // Group 0 carries the most *overhead* and the last group can carry the fewest
        // *blocks*, and those are independent: a filesystem can end with a group of 7 blocks
        // that needs 68. The first version of this compared group 0 against itself and called
        // that sufficient — "group 0 carries the most metadata, so if the root fits there it
        // fits" — which is true of overhead and says nothing about the tail. `format` then
        // wrote past the end of the filesystem, and `descriptor`'s free-block count
        // underflowed. At `nxinstall`'s parameters roughly **one partition size in 400** lands
        // there (PR #310 review, finding 1).
        //
        // Dropping costs at most one short group, which is by definition fewer blocks than its
        // own metadata — about 70 on a 4 KiB filesystem, a quarter of a megabyte. Refusing
        // instead would mean an installer that turns down one disk in 400 for a reason nobody
        // could act on. Looping because dropping a group can shrink the descriptor table,
        // which lowers every group's overhead; it terminates because `blocks_count` strictly
        // decreases.
        let g = loop {
            let g = Geometry {
                block_size: p.block_size,
                blocks_count,
                first_data_block,
                blocks_per_group,
                groups,
                inodes_per_group,
                inodes_count: inodes_per_group
                    .checked_mul(groups)
                    .ok_or(MkfsError::TooLarge)?,
                itable_blocks,
                gdt_blocks,
            };
            // Group 0 also holds the root directory's block, so it needs two more than its
            // metadata.
            if g.overhead(0) + 2 >= g.blocks_in_group(0) as u64 {
                return Err(MkfsError::TooSmall { have: p.blocks, need: g.overhead(0) + 3 });
            }
            let last = groups - 1;
            if groups == 1 || g.overhead(last) < g.blocks_in_group(last) as u64 {
                break g;
            }
            // Cut the filesystem at the start of the group being dropped.
            blocks_count = g.group_start(last);
            groups -= 1;
            gdt_blocks =
                (groups as u64 * DESC_SIZE as u64).div_ceil(p.block_size as u64) as u32;
        };
        Ok(g)
    }

    /// The first block of `group`.
    pub fn group_start(&self, group: u32) -> u64 {
        self.first_data_block as u64 + group as u64 * self.blocks_per_group as u64
    }

    /// How many blocks `group` has — the last one is short when the size does not divide.
    pub fn blocks_in_group(&self, group: u32) -> u32 {
        let start = self.group_start(group);
        self.blocks_count.saturating_sub(start).min(self.blocks_per_group as u64) as u32
    }

    /// Does `group` carry a superblock and a descriptor-table copy?
    ///
    /// **`sparse_super`'s rule**: group 0, group 1, and every power of 3, 5 and 7. Anything
    /// else holds only its own bitmaps and inode table.
    pub fn has_super(&self, group: u32) -> bool {
        if group <= 1 {
            return true;
        }
        if group % 2 == 0 {
            return false; // no even group above 0 is a power of 3, 5 or 7
        }
        [3u32, 5, 7].iter().any(|&base| {
            let mut n = base;
            loop {
                if n == group {
                    return true;
                }
                match n.checked_mul(base) {
                    Some(next) if next <= group => n = next,
                    _ => return false,
                }
            }
        })
    }

    /// Blocks of `group` taken by metadata: its superblock and descriptor copy if it has one,
    /// then its two bitmaps and its inode table.
    pub fn overhead(&self, group: u32) -> u64 {
        let sb = if self.has_super(group) { 1 + self.gdt_blocks as u64 } else { 0 };
        sb + 2 + self.itable_blocks as u64
    }

    /// The first block of `group`'s block bitmap, inode bitmap and inode table.
    fn metadata_start(&self, group: u32) -> u64 {
        let sb = if self.has_super(group) { 1 + self.gdt_blocks as u64 } else { 0 };
        self.group_start(group) + sb
    }

    /// Total free blocks once the layout is written and the root directory placed.
    fn free_blocks(&self) -> u64 {
        let used: u64 = (0..self.groups).map(|g| self.overhead(g)).sum();
        let total: u64 = (0..self.groups).map(|g| self.blocks_in_group(g) as u64).sum();
        total - used - 1 // the root directory's one data block
    }
}

/// The most inodes this will make, whatever the ratio asks for. See [`Geometry::new`].
const MAX_INODES: u64 = 1 << 20;

/// Write an empty filesystem over `w`, and return the layout it wrote.
///
/// **Writes metadata only.** Data blocks are left as they were — an install writes over them
/// immediately, and zeroing a terabyte to make an empty filesystem is time spent for nothing.
/// Every structure a reader or `e2fsck` looks at *is* written, including the parts of a
/// bitmap and an inode table that describe nothing yet, because those are read.
pub fn format<W: BlockWriter>(
    w: &W,
    p: &Params,
    progress: &mut dyn FnMut(u32, u32),
) -> Result<Geometry, FsError> {
    let g = Geometry::new(p).map_err(MkfsError::fs_error)?;
    let bs = p.block_size as usize;

    // The root directory's data block: the first block after group 0's metadata.
    let root_block = g.metadata_start(0) + 2 + g.itable_blocks as u64;

    let mut block = [0u8; MAX_BLOCK];
    for group in 0..g.groups {
        // --- the two bitmaps and the inode table -------------------------------------
        let meta = g.metadata_start(group);
        let (bbitmap, ibitmap, itable) = (meta, meta + 1, meta + 2);

        // Block bitmap: this group's own metadata, the root block if it is here, and every
        // bit past the group's real length — a short final group's spare bits address blocks
        // that are not there, and a filesystem that left them clear would hand one out.
        block[..bs].fill(0);
        let overhead = g.overhead(group) as usize;
        for i in 0..overhead {
            bit_set(&mut block, i);
        }
        if group == 0 {
            bit_set(&mut block, (root_block - g.group_start(0)) as usize);
        }
        for i in g.blocks_in_group(group) as usize..bs * 8 {
            bit_set(&mut block, i);
        }
        w.write_at(bbitmap * p.block_size as u64, &block[..bs])?;

        // Inode bitmap: the reserved inodes in group 0, and the padding past the group's
        // share everywhere.
        block[..bs].fill(0);
        if group == 0 {
            for i in 0..(FIRST_INO - 1) as usize {
                bit_set(&mut block, i);
            }
        }
        for i in g.inodes_per_group as usize..bs * 8 {
            bit_set(&mut block, i);
        }
        w.write_at(ibitmap * p.block_size as u64, &block[..bs])?;

        // The inode table, zeroed. `e2fsck` reads every inode, so a slot holding whatever the
        // disk held before is a slot it may decide is a file.
        //
        // **This is where the time goes.** A 931 GiB disk has 7,452 groups, and writing each
        // one's two bitmaps and inode table is about 300 MiB in total — minutes on a spinning
        // disk, one command at a time, with nothing else to show for it. Hence `progress`:
        // silence for that long is indistinguishable from a hang.
        block[..bs].fill(0);
        for b in 0..g.itable_blocks as u64 {
            w.write_at((itable + b) * p.block_size as u64, &block[..bs])?;
        }
        progress(group + 1, g.groups);
    }

    // --- the root directory ----------------------------------------------------------
    // Two entries: `.` and `..`, both the root itself. The second one's `rec_len` covers the
    // rest of the block, which is what makes the block a complete chain.
    block[..bs].fill(0);
    write_dirent(&mut block[0..], ROOT_INO, 12, b".", 2);
    write_dirent(&mut block[12..], ROOT_INO, (bs - 12) as u16, b"..", 2);
    w.write_at(root_block * p.block_size as u64, &block[..bs])?;

    let mut inode = [0u8; INODE_SIZE as usize];
    wr_u16(&mut inode, 0, ROOT_MODE);
    wr_u32(&mut inode, 4, p.block_size); // i_size_lo: one block
    wr_u32(&mut inode, 8, p.now as u32); // atime
    wr_u32(&mut inode, 12, p.now as u32); // ctime
    wr_u32(&mut inode, 16, p.now as u32); // mtime
    wr_u16(&mut inode, 26, 2); // i_links_count: `.` and `..`
    wr_u32(&mut inode, 28, p.block_size / 512); // i_blocks_lo, in 512-byte units
    wr_u32(&mut inode, 32, EXTENTS_FL);
    // One inline extent covering the single block.
    wr_u16(&mut inode, 40, EXTENT_MAGIC);
    wr_u16(&mut inode, 42, 1); // eh_entries
    wr_u16(&mut inode, 44, 4); // eh_max — four fit in i_block
    wr_u16(&mut inode, 46, 0); // eh_depth: a leaf
    wr_u32(&mut inode, 52, 0); // ee_block: logical 0
    wr_u16(&mut inode, 56, 1); // ee_len
    wr_u16(&mut inode, 58, 0); // ee_start_hi
    wr_u32(&mut inode, 60, root_block as u32); // ee_start_lo
    wr_u16(&mut inode, 128, EXTRA_ISIZE);
    let itable0 = g.metadata_start(0) + 2;
    let root_off = itable0 * p.block_size as u64 + (ROOT_INO as u64 - 1) * INODE_SIZE as u64;
    w.write_at(root_off, &inode)?;

    // --- the group descriptors, then the superblock and its backups -------------------
    let free = g.free_blocks();
    let mut sb = [0u8; 1024];
    wr_u32(&mut sb, 0, g.inodes_count);
    wr_u32(&mut sb, 4, g.blocks_count as u32);
    // **No reserved blocks**, where `mke2fs` reserves 5% for the superuser. That reservation
    // exists so a full disk still leaves root able to log in and clean up; this system has no
    // root account and no privileged process to reserve for, so the 5% would be 46 GiB of a
    // terabyte withheld from its owner on behalf of nobody. `alloc_block` ignores the field
    // either way, so this is a statement about whose disk it is rather than a behaviour change.
    wr_u32(&mut sb, 8, 0); // s_r_blocks_count_lo
    wr_u32(&mut sb, 12, free as u32);
    wr_u32(&mut sb, 16, g.inodes_count - (FIRST_INO - 1));
    wr_u32(&mut sb, 20, g.first_data_block);
    let log = g.block_size.trailing_zeros() - 10;
    wr_u32(&mut sb, 24, log); // s_log_block_size
    wr_u32(&mut sb, 28, log); // s_log_cluster_size (no bigalloc)
    wr_u32(&mut sb, 32, g.blocks_per_group);
    wr_u32(&mut sb, 36, g.blocks_per_group); // s_clusters_per_group
    wr_u32(&mut sb, 40, g.inodes_per_group);
    wr_u32(&mut sb, 44, p.now as u32); // s_mtime
    wr_u32(&mut sb, 48, p.now as u32); // s_wtime
    wr_u16(&mut sb, 54, 0xFFFF); // s_max_mnt_count: never force a check on mount count
    wr_u16(&mut sb, 56, 0xEF53); // s_magic
    wr_u16(&mut sb, 58, 1); // s_state: clean
    wr_u16(&mut sb, 60, 1); // s_errors: continue
    wr_u32(&mut sb, 64, p.now as u32); // s_lastcheck
    wr_u32(&mut sb, 76, 1); // s_rev_level: dynamic, which is what inode_size needs
    wr_u32(&mut sb, 84, FIRST_INO);
    wr_u16(&mut sb, 88, INODE_SIZE as u16);
    wr_u32(&mut sb, 96, INCOMPAT);
    wr_u32(&mut sb, 100, RO_COMPAT);
    sb[104..120].copy_from_slice(&p.uuid);
    sb[120..136].copy_from_slice(&p.label);
    wr_u32(&mut sb, 264, p.now as u32); // s_mkfs_time
    wr_u16(&mut sb, 348, EXTRA_ISIZE); // s_min_extra_isize
    wr_u16(&mut sb, 350, EXTRA_ISIZE); // s_want_extra_isize
    write_super_and_gdt(w, &g, &sb)?;
    Ok(g)
}

/// One group's descriptor.
///
/// Computed on demand rather than held in a table, which is the whole reason this is a
/// function: the table for a 931 GiB disk is 7,452 descriptors — **238 KiB, or 59 blocks** —
/// and the first version of this built it in a single one-block buffer. It indexed past the
/// end at group 128 and the installer died on a real disk with no message, because a panic in
/// a program whose diagnostics go to the terminal prints through `kprint` instead. The gate's
/// filesystem has four groups, 128 bytes of descriptors, so nothing under QEMU could reach it
/// — the same shape of blindness as the single-group fixture that hid group-0-only allocation
/// (2026-09-17).
fn descriptor(g: &Geometry, group: u32) -> [u8; DESC_SIZE as usize] {
    let meta = g.metadata_start(group);
    let mut d = [0u8; DESC_SIZE as usize];
    wr_u32(&mut d, 0, meta as u32); // bg_block_bitmap_lo
    wr_u32(&mut d, 4, (meta + 1) as u32); // bg_inode_bitmap_lo
    wr_u32(&mut d, 8, (meta + 2) as u32); // bg_inode_table_lo
    let mut gfree = g.blocks_in_group(group) as u64 - g.overhead(group);
    let mut ifree = g.inodes_per_group;
    if group == 0 {
        gfree -= 1; // the root directory's block
        ifree -= FIRST_INO - 1;
        wr_u16(&mut d, 16, 1); // bg_used_dirs_count_lo: the root
    }
    wr_u16(&mut d, 12, gfree as u16); // bg_free_blocks_count_lo
    wr_u16(&mut d, 14, ifree as u16); // bg_free_inodes_count_lo
    d
}

/// Write the superblock and descriptor table into group 0 and every backup group.
///
/// A backup's `s_block_group_nr` names its own group, which is how `e2fsck -b` knows which
/// copy it is reading.
fn write_super_and_gdt<W: BlockWriter>(
    w: &W,
    g: &Geometry,
    sb: &[u8; 1024],
) -> Result<(), FsError> {
    let bs = g.block_size as u64;
    // **A block of descriptors at a time**, filled from [`descriptor`]. The table is 59 blocks
    // on a terabyte, and this library holds one block of scratch.
    let per_block = (g.block_size / DESC_SIZE) as usize;
    let mut block = [0u8; MAX_BLOCK];
    for group in 0..g.groups {
        if !g.has_super(group) {
            continue;
        }
        let mut copy = *sb;
        wr_u16(&mut copy, 90, group as u16); // s_block_group_nr
        // The primary superblock lives at byte 1024 whatever the block size; a backup starts
        // at its group's first block.
        let (sb_at, gdt_at) = if group == 0 {
            (1024, (g.first_data_block as u64 + 1) * bs)
        } else {
            let start = g.group_start(group);
            (start * bs, (start + 1) * bs)
        };
        w.write_at(sb_at, &copy)?;
        for b in 0..g.gdt_blocks as usize {
            block[..g.block_size as usize].fill(0);
            for i in 0..per_block {
                let described = (b * per_block + i) as u32;
                if described >= g.groups {
                    break;
                }
                let at = i * DESC_SIZE as usize;
                block[at..at + DESC_SIZE as usize]
                    .copy_from_slice(&descriptor(g, described));
            }
            w.write_at(gdt_at + b as u64 * bs, &block[..g.block_size as usize])?;
        }
    }
    Ok(())
}

/// One `ext4_dir_entry_2` at the start of `out`.
fn write_dirent(out: &mut [u8], ino: u32, rec_len: u16, name: &[u8], file_type: u8) {
    wr_u32(out, 0, ino);
    wr_u16(out, 4, rec_len);
    out[6] = name.len() as u8;
    out[7] = file_type;
    out[8..8 + name.len()].copy_from_slice(name);
}

/// The reader's one-block scratch, which bounds a bitmap and a descriptor table here too.
const MAX_BLOCK: usize = 4096;

fn bit_set(b: &mut [u8], i: usize) {
    b[i / 8] |= 1 << (i % 8);
}

fn wr_u16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn wr_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(blocks: u64, block_size: u32) -> Params {
        Params {
            blocks,
            block_size,
            bytes_per_inode: 16384,
            uuid: [0x5A; 16],
            label: *b"nitrox-root\0\0\0\0\0",
            now: 1_784_900_730,
        }
    }

    /// `sparse_super`'s rule, which decides where 1.8 GiB of backups do not go on a large
    /// disk. Stated as the set it produces rather than as the predicate, so a wrong
    /// predicate has to produce the right set to pass.
    #[test]
    fn backups_land_on_0_1_and_the_powers_of_3_5_and_7() {
        let g = Geometry::new(&params(1 << 20, 4096)).unwrap();
        let with: Vec<u32> = (0..400u32).filter(|&n| g.has_super(n)).collect();
        assert_eq!(
            with,
            [0, 1, 3, 5, 7, 9, 25, 27, 49, 81, 125, 243, 343],
            "0 and 1 always, then 3^n, 5^n and 7^n"
        );
    }

    /// The last group is short whenever the size does not divide, and every other group is
    /// full. Getting this wrong hands out a block past the end of the device.
    #[test]
    fn the_final_group_holds_only_what_is_left() {
        let g = Geometry::new(&params(24576, 1024)).unwrap();
        assert_eq!(g.groups, 3);
        assert_eq!(g.blocks_in_group(0), 8192);
        assert_eq!(g.blocks_in_group(2), 8191, "24,575 addressable does not divide by 8,192");
        let total: u64 = (0..g.groups).map(|n| g.blocks_in_group(n) as u64).sum();
        assert_eq!(total, g.blocks_count - g.first_data_block as u64);
    }

    /// The inode count is capped however generous the ratio, because the table is written
    /// out in full: at `mke2fs`'s default ratio a 931 GiB disk asks for 61 million inodes,
    /// which is 16 GiB of zeroes before the first file exists.
    #[test]
    fn the_inode_count_is_capped_for_a_large_disk() {
        let big = Geometry::new(&params(244_190_000, 4096)).unwrap(); // ~931 GiB
        assert!(big.inodes_count as u64 <= MAX_INODES, "{} inodes", big.inodes_count);
        assert!(big.inodes_count > 100_000, "still a usable number of files");
        // The table it costs, which is the number the cap exists to bound.
        let itable_bytes = big.itable_blocks as u64 * big.block_size as u64 * big.groups as u64;
        assert!(itable_bytes < 512 * 1024 * 1024, "{itable_bytes} bytes of inode table");
        // A small filesystem is governed by the ratio instead, not the cap.
        let small = Geometry::new(&params(24576, 1024)).unwrap();
        assert!((small.inodes_count as u64) < MAX_INODES);
    }

    /// **A final group shorter than its own metadata is dropped, not written past.**
    ///
    /// Both sizes are the reviewer's, reproduced (PR #310, finding 1): 8,200 blocks at 1 KiB
    /// gives a second group of 7 blocks needing 68, and 26,214,401 blocks at 4 KiB — a size
    /// `nxinstall` can be handed — gives group 800 a single block needing 83. The old guard
    /// compared group 0 against itself and accepted both; `format` then wrote past the end of
    /// the filesystem, and `descriptor`'s free-block count underflowed.
    #[test]
    fn a_final_group_too_short_for_its_metadata_is_dropped() {
        for (blocks, bs) in [(8200u64, 1024u32), (26_214_401, 4096)] {
            let g = Geometry::new(&params(blocks, bs)).expect("this is a usable disk");
            let last = g.groups - 1;
            assert!(
                g.overhead(last) < g.blocks_in_group(last) as u64,
                "{blocks} blocks at {bs}: the last group has {} blocks and {} of overhead",
                g.blocks_in_group(last),
                g.overhead(last)
            );
            // **Every group, not just the last** — the loop lowers the descriptor table as it
            // drops groups, so the claim is about the geometry it settles on.
            for n in 0..g.groups {
                assert!(
                    g.overhead(n) < g.blocks_in_group(n) as u64,
                    "{blocks} blocks at {bs}: group {n} cannot hold its own metadata"
                );
            }
            // The filesystem never claims more than the partition, and gives up at most the
            // one short group — far less than a whole one.
            assert!(g.blocks_count <= blocks);
            assert!(
                blocks - g.blocks_count < g.blocks_per_group as u64,
                "{blocks} blocks at {bs}: gave up {} blocks, which is more than a tail",
                blocks - g.blocks_count
            );
        }
    }

    /// The same property over a sweep, because the reviewer found it at 0.25% of sizes and a
    /// hand-picked pair proves only the two that were picked.
    #[test]
    fn no_size_produces_a_group_that_cannot_hold_its_metadata() {
        // `nxinstall`'s parameters, around 100 GiB, stepping by a group so the tail varies.
        let mut checked = 0;
        for n in 0..2000u64 {
            let blocks = 26_214_400 + n * 7 + 1;
            let g = match Geometry::new(&params(blocks, 4096)) {
                Ok(g) => g,
                Err(e) => panic!("{blocks} blocks refused: {e:?}"),
            };
            for i in 0..g.groups {
                assert!(
                    g.overhead(i) < g.blocks_in_group(i) as u64,
                    "{blocks} blocks: group {i} has {} blocks, {} of overhead",
                    g.blocks_in_group(i),
                    g.overhead(i)
                );
            }
            checked += 1;
        }
        assert_eq!(checked, 2000);
    }

    #[test]
    fn a_filesystem_too_small_to_hold_its_own_metadata_is_refused() {
        assert_eq!(
            Geometry::new(&params(16, 1024)),
            Err(MkfsError::TooSmall { have: 16, need: MIN_BLOCKS })
        );
        assert!(matches!(Geometry::new(&params(4096, 512)), Err(MkfsError::BlockSize(512))));
        assert_eq!(Geometry::new(&params(1 << 33, 4096)), Err(MkfsError::TooLarge));
    }
}
