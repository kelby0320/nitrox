//! **The file allocation table** (Phase 6 Part E.2), read through a cache of its sectors.
//!
//! An entry says what follows a cluster in its chain: another cluster, the end of the chain,
//! nothing (the cluster is free), or that the cluster is bad. FAT12 packs entries in 12 bits — so
//! one can straddle two sectors — FAT16 in 16, FAT32 in the low 28 of 32.
//!
//! **The cache is the first piece of the batched write path** the plan asks for: entries are read
//! and, from E.3, written through it, and a request's dirty sectors are written once each.

use crate::bpb::{Geometry, Kind, SECTOR};
use crate::{BlockReader, FsError};

/// How many FAT sectors the cache holds: 32 KiB, which covers 8,192 FAT32 clusters — 32 MiB of a
/// 4 KiB-cluster volume — without a miss.
pub const CACHE_SECTORS: usize = 64;

/// What follows a cluster in its chain.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Next {
    /// The cluster is free.
    Free,
    /// The chain ends here.
    End,
    /// The cluster is marked bad.
    Bad,
    /// Another cluster follows.
    Cluster(u32),
}

#[derive(Copy, Clone)]
struct Slot {
    /// The sector within the FAT this slot holds, or `u32::MAX` when it holds none.
    sector: u32,
    /// When it was last used, for eviction.
    used: u32,
    data: [u8; SECTOR as usize],
}

/// **A cache of the FAT's sectors**: the first copy's, read; from E.3, written to every copy.
pub struct Cache {
    slots: [Slot; CACHE_SECTORS],
    clock: u32,
}

impl Default for Cache {
    fn default() -> Cache {
        Cache::new()
    }
}

impl Cache {
    /// An empty cache.
    pub const fn new() -> Cache {
        Cache { slots: [Slot { sector: u32::MAX, used: 0, data: [0; SECTOR as usize] }; CACHE_SECTORS], clock: 0 }
    }

    /// Forget everything held: after a failed read, or when the filesystem underneath may have
    /// changed.
    pub fn clear(&mut self) {
        for s in &mut self.slots {
            s.sector = u32::MAX;
        }
    }

    /// **The slot holding FAT sector `sector`**, read into the least recently used one on a miss.
    fn slot<R: BlockReader>(&mut self, r: &R, g: &Geometry, sector: u32) -> Result<usize, FsError> {
        if sector >= g.fat_sectors {
            return Err(FsError::Corrupt);
        }
        self.clock = self.clock.wrapping_add(1);
        if let Some(i) = self.slots.iter().position(|s| s.sector == sector) {
            self.slots[i].used = self.clock;
            return Ok(i);
        }
        let i = (0..CACHE_SECTORS).min_by_key(|&i| (self.slots[i].sector != u32::MAX, self.slots[i].used)).unwrap_or(0);
        self.slots[i].sector = u32::MAX;
        r.read_at(g.fat_byte(0) + sector as u64 * SECTOR as u64, &mut self.slots[i].data)?;
        self.slots[i].sector = sector;
        self.slots[i].used = self.clock;
        Ok(i)
    }

    /// Byte `at` of the first FAT.
    fn byte<R: BlockReader>(&mut self, r: &R, g: &Geometry, at: u64) -> Result<u8, FsError> {
        let i = self.slot(r, g, (at / SECTOR as u64) as u32)?;
        Ok(self.slots[i].data[(at % SECTOR as u64) as usize])
    }

    /// **The raw entry for cluster `c`**, as its FAT stores it: 12, 16 or 28 bits.
    pub fn raw<R: BlockReader>(&mut self, r: &R, g: &Geometry, c: u32) -> Result<u32, FsError> {
        let c64 = c as u64;
        Ok(match g.kind {
            Kind::Fat12 => {
                let at = c64 + c64 / 2;
                let v = self.byte(r, g, at)? as u32 | (self.byte(r, g, at + 1)? as u32) << 8;
                if c & 1 == 1 { v >> 4 } else { v & 0xFFF }
            }
            Kind::Fat16 => self.byte(r, g, c64 * 2)? as u32 | (self.byte(r, g, c64 * 2 + 1)? as u32) << 8,
            Kind::Fat32 => {
                let mut v = 0u32;
                for k in 0..4 {
                    v |= (self.byte(r, g, c64 * 4 + k)? as u32) << (8 * k);
                }
                v & 0x0FFF_FFFF
            }
        })
    }

    /// **What follows cluster `c`**, which must be a data cluster. An entry naming a cluster
    /// outside the volume is `Corrupt`.
    pub fn next<R: BlockReader>(&mut self, r: &R, g: &Geometry, c: u32) -> Result<Next, FsError> {
        if !g.valid_cluster(c) {
            return Err(FsError::Corrupt);
        }
        let v = self.raw(r, g, c)?;
        let (bad, end) = match g.kind {
            Kind::Fat12 => (0xFF7, 0xFF8),
            Kind::Fat16 => (0xFFF7, 0xFFF8),
            Kind::Fat32 => (0x0FFF_FFF7, 0x0FFF_FFF8),
        };
        Ok(match v {
            0 => Next::Free,
            v if v >= end => Next::End,
            v if v == bad => Next::Bad,
            v if g.valid_cluster(v) => Next::Cluster(v),
            _ => return Err(FsError::Corrupt),
        })
    }

    /// **Walk the chain from `first`**, handing each cluster to `each` with its index in the chain
    /// until `each` answers `false` or the chain ends. **Bounded by the volume's cluster count**, so
    /// a chain that loops is `Corrupt` rather than endless; a chain reaching a free or bad cluster
    /// is `Corrupt` too.
    pub fn walk<R: BlockReader>(
        &mut self,
        r: &R,
        g: &Geometry,
        first: u32,
        mut each: impl FnMut(u32, u32) -> bool,
    ) -> Result<(), FsError> {
        let mut c = first;
        for i in 0..g.clusters {
            if !g.valid_cluster(c) {
                return Err(FsError::Corrupt);
            }
            if !each(i, c) {
                return Ok(());
            }
            match self.next(r, g, c)? {
                Next::End => return Ok(()),
                Next::Cluster(n) => c = n,
                Next::Free | Next::Bad => return Err(FsError::Corrupt),
            }
        }
        Err(FsError::Corrupt)
    }

    /// **The cluster at index `k` of the chain from `first`**, or `None` if the chain is shorter.
    pub fn nth<R: BlockReader>(&mut self, r: &R, g: &Geometry, first: u32, k: u32) -> Result<Option<u32>, FsError> {
        let mut found = None;
        self.walk(r, g, first, |i, c| {
            if i == k {
                found = Some(c);
                false
            } else {
                true
            }
        })?;
        Ok(found)
    }
}
