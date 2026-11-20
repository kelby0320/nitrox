//! The superblock: the facts the reader and the write path take from it, and whether a device
//! holds a filesystem this server serves, and writes.

use super::*;

/// The parsed superblock facts the reader (and the write path) need.
pub(crate) struct Superblock {
    pub(crate) block_size: u32,
    pub(crate) inodes_per_group: u32,
    pub(crate) inode_size: u32,
    pub(crate) desc_size: u32,
    pub(crate) first_gdt_block: u64,
    pub(crate) blocks_per_group: u32,
    /// The first data block (`1` for 1 KiB blocks, else `0`) — block numbering origin.
    pub(crate) first_data_block: u32,
    /// Blocks in the filesystem (`s_blocks_count_lo`; the `_hi` half needs `64bit`, which
    /// [`parse_superblock`] refuses). **The ceiling the allocator must not cross.**
    pub(crate) blocks_count: u64,
    pub(crate) inodes_count: u32,
}

impl Superblock {
    pub(crate) fn group_count(&self) -> u32 {
        let data = self.blocks_count.saturating_sub(self.first_data_block as u64);
        data.div_ceil(self.blocks_per_group as u64).max(1) as u32
    }

    /// How much of an inode this server reads and writes: its size, up to 256 bytes.
    pub(crate) fn inode_len(&self) -> usize {
        (self.inode_size as usize).min(256)
    }

    pub(crate) fn group_start(&self, group: u32) -> u64 {
        self.first_data_block as u64 + group as u64 * self.blocks_per_group as u64
    }

    /// How many blocks `group` has: `blocks_per_group` except for a final group cut short. **Not
    /// the bitmap's width**: a bit set past the end of the filesystem is a block number no reader
    /// can fetch, written into an extent that `e2fsck` calls corrupt.
    pub(crate) fn blocks_in_group(&self, group: u32) -> usize {
        let start = self.group_start(group);
        self.blocks_count.saturating_sub(start).min(self.blocks_per_group as u64) as usize
    }

    pub(crate) fn inodes_in_group(&self, group: u32) -> usize {
        let first = group as u64 * self.inodes_per_group as u64;
        (self.inodes_count as u64).saturating_sub(first).min(self.inodes_per_group as u64) as usize
    }

    /// Byte offset of `group`'s descriptor; the table is contiguous, so this spans blocks.
    pub(crate) fn group_desc_offset(&self, group: u32) -> u64 {
        self.first_gdt_block * self.block_size as u64 + group as u64 * self.desc_size as u64
    }

    /// Inode `ino`'s group and its index there. `Corrupt` for 0, or past `s_inodes_count`, where
    /// the descriptor would be read from past the table.
    pub(crate) fn inode_place(&self, ino: u32) -> Result<(u32, usize), FsError> {
        if ino == 0 || ino > self.inodes_count {
            return Err(FsError::Corrupt);
        }
        Ok(((ino - 1) / self.inodes_per_group, ((ino - 1) % self.inodes_per_group) as usize))
    }
}

/// Parse the 1024 bytes at device byte 1024 into the facts the reader needs.
fn parse_superblock(sb: &[u8; 1024]) -> Result<Superblock, Unservable> {
    let magic = rd_u16(sb, 56);
    if magic != SUPER_MAGIC {
        return Err(Unservable::NoMagic { found: magic });
    }
    let incompat = rd_u32(sb, 96);
    if incompat & INCOMPAT_64BIT != 0 {
        return Err(Unservable::Wide64Bit); // 64-bit changes the descriptor layout
    }
    if incompat & features::INCOMPAT_RECOVER != 0 {
        return Err(Unservable::NeedsRecovery);
    }
    if incompat & !features::INCOMPAT_READ != 0 {
        return Err(Unservable::Incompatible { features: incompat & !features::INCOMPAT_READ });
    }
    let log = rd_u32(sb, 24);
    if log > 2 {
        return Err(Unservable::BlockTooLarge { log }); // > 4 KiB blocks exceed the scratch buffer
    }
    let block_size = 1024u32 << log;
    let inode_size = rd_u16(sb, 88) as u32;
    if inode_size == 0 {
        return Err(Unservable::ZeroField("s_inode_size"));
    }
    let inodes_per_group = rd_u32(sb, 40);
    if inodes_per_group == 0 {
        return Err(Unservable::ZeroField("s_inodes_per_group"));
    }
    // Without `64bit`, which is refused, a descriptor is 32 bytes whatever `s_desc_size` says.
    let desc_size = 32;
    let blocks_per_group = rd_u32(sb, 32);
    let first_data_block = rd_u32(sb, 20);
    if blocks_per_group == 0 {
        return Err(Unservable::ZeroField("s_blocks_per_group"));
    }
    // A group's bitmaps are a block each, so a group holds no more than a block's bits: Linux
    // refuses either count over it.
    if blocks_per_group > block_size * 8 {
        return Err(Unservable::Geometry("s_blocks_per_group"));
    }
    if inodes_per_group > block_size * 8 {
        return Err(Unservable::Geometry("s_inodes_per_group"));
    }
    let parsed = Superblock {
        block_size,
        inodes_per_group,
        inode_size,
        desc_size,
        first_gdt_block: if block_size == 1024 { 2 } else { 1 },
        blocks_per_group,
        first_data_block,
        blocks_count: rd_u32(sb, 4) as u64,
        inodes_count: rd_u32(sb, 0),
    };
    if parsed.inodes_count as u64 != parsed.group_count() as u64 * inodes_per_group as u64 {
        return Err(Unservable::Geometry("s_inodes_count"));
    }
    Ok(parsed)
}

/// The filesystem's block size, for a caller that places file data itself (`nxinstall`).
pub fn block_size<R: BlockReader>(r: &R) -> Result<u32, FsError> {
    Ok(read_superblock(r)?.block_size)
}

pub(crate) fn read_superblock<R: BlockReader>(r: &R) -> Result<Superblock, FsError> {
    let mut sb = [0u8; 1024];
    r.read_meta(1024, &mut sb)?;
    parse_superblock(&sb).map_err(Unservable::fs_error)
}

/// Check that `r` holds a filesystem this reader can serve, before the server says it can: the
/// superblock, its features (those a write must keep, unless `r` is read-only), then the root
/// directory. Nothing deeper is read; a filesystem wrong there fails the request that reaches it.
pub fn check_device<R: BlockReader>(r: &R) -> Result<(), Unservable> {
    let mut raw = [0u8; 1024];
    r.read_at(1024, &mut raw).map_err(|_| Unservable::Unreadable)?;
    let sb = parse_superblock(&raw)?;
    if !r.read_only() {
        writable(&raw)?;
    }
    let root = read_inode(r, &sb, ROOT_INO).map_err(|_| Unservable::RootUnreadable)?;
    let mode = rd_u16(&root, 0);
    if mode & S_IFMT != S_IFDIR {
        return Err(Unservable::RootNotDirectory { mode });
    }
    Ok(())
}

/// **Whether a write here keeps the filesystem `r` holds**: `NotWritable` naming what it would not,
/// for a supervisor to mount it read-only instead.
pub fn check_writable<R: BlockReader>(r: &R) -> Result<(), Unservable> {
    let mut raw = [0u8; 1024];
    r.read_at(1024, &mut raw).map_err(|_| Unservable::Unreadable)?;
    writable(&raw)
}

fn writable(sb: &[u8; 1024]) -> Result<(), Unservable> {
    let ro_compat = rd_u32(sb, 100) & !features::RO_COMPAT_KEPT;
    let lacking = features::INCOMPAT_WRITTEN & !rd_u32(sb, 96);
    match ro_compat | lacking {
        0 => Ok(()),
        _ => Err(Unservable::NotWritable { ro_compat, lacking }),
    }
}
