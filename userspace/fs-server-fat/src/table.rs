//! **The file allocation table** (Phase 6 Parts E.2 and E.3), read and written through a cache of
//! its sectors.
//!
//! An entry says what follows a cluster in its chain: another cluster, the end of the chain,
//! nothing (the cluster is free), or that the cluster is bad. FAT12 packs entries in 12 bits — so
//! one can straddle two sectors — FAT16 in 16, FAT32 in the low 28 of 32.
//!
//! **The cache is where the write path batches**: entries are changed in it, and its dirty sectors
//! written at the points a change orders them — adjacent ones in one write, to every copy of the
//! FAT. A slot is evicted dirty only by a write path, which writes it out first.

use crate::bpb::{Geometry, Kind, SECTOR};
use crate::{BlockReader, BlockWriter, FsError};

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
    /// Whether it holds entries not yet written to the device.
    dirty: bool,
    data: [u8; SECTOR as usize],
}

/// **A cache of the FAT's sectors**: the first copy's, read; written to every copy.
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
        Cache { slots: [Slot { sector: u32::MAX, used: 0, dirty: false, data: [0; SECTOR as usize] }; CACHE_SECTORS], clock: 0 }
    }

    /// **Forget everything held**, written or not: after a failed write, so what is held never
    /// differs from the device, or when the filesystem underneath may have changed.
    pub fn clear(&mut self) {
        for s in &mut self.slots {
            s.sector = u32::MAX;
            s.dirty = false;
        }
    }

    /// The slot holding `sector`, if one does, its use recorded.
    fn hit(&mut self, sector: u32) -> Option<usize> {
        self.clock = self.clock.wrapping_add(1);
        let i = self.slots.iter().position(|s| s.sector == sector)?;
        self.slots[i].used = self.clock;
        Some(i)
    }

    /// **The slot to reuse**: an empty one, else the least recently used — never a dirty one
    /// unless `dirty_too`, since only a writer can write it out first.
    fn victim(&self, dirty_too: bool) -> Option<usize> {
        (0..CACHE_SECTORS)
            .filter(|&i| dirty_too || !self.slots[i].dirty)
            .min_by_key(|&i| (self.slots[i].sector != u32::MAX, self.slots[i].used))
    }

    /// Read FAT sector `sector` into slot `i`.
    fn load<R: BlockReader>(&mut self, r: &R, g: &Geometry, i: usize, sector: u32) -> Result<usize, FsError> {
        self.slots[i].sector = u32::MAX;
        r.read_at(g.fat_byte(0) + sector as u64 * SECTOR as u64, &mut self.slots[i].data)?;
        self.slots[i].sector = sector;
        self.slots[i].used = self.clock;
        Ok(i)
    }

    /// **The slot holding FAT sector `sector`**, read into the least recently used clean one on a
    /// miss. Reads alone come this way; a write path's reads come through [`Cache::slot_rw`].
    fn slot<R: BlockReader>(&mut self, r: &R, g: &Geometry, sector: u32) -> Result<usize, FsError> {
        if sector >= g.fat_sectors {
            return Err(FsError::Corrupt);
        }
        if let Some(i) = self.hit(sector) {
            return Ok(i);
        }
        // Every slot dirty is a write path that did not use `slot_rw`: refuse, not overwrite.
        let i = self.victim(false).ok_or(FsError::Io)?;
        self.load(r, g, i, sector)
    }

    /// **[`Cache::slot`] for a write path**, which may evict a dirty slot: written out first, to
    /// every copy of the FAT.
    fn slot_rw<RW: BlockReader + BlockWriter>(&mut self, rw: &RW, g: &Geometry, sector: u32) -> Result<usize, FsError> {
        if sector >= g.fat_sectors {
            return Err(FsError::Corrupt);
        }
        if let Some(i) = self.hit(sector) {
            return Ok(i);
        }
        let i = self.victim(true).ok_or(FsError::Io)?;
        if self.slots[i].dirty {
            let at = self.slots[i].sector as u64 * SECTOR as u64;
            for n in 0..g.fats {
                rw.write_at(g.fat_byte(n) + at, &self.slots[i].data)?;
            }
            self.slots[i].dirty = false;
        }
        self.load(rw, g, i, sector)
    }

    /// Byte `at` of the first FAT.
    fn byte<R: BlockReader>(&mut self, r: &R, g: &Geometry, at: u64) -> Result<u8, FsError> {
        let i = self.slot(r, g, (at / SECTOR as u64) as u32)?;
        Ok(self.slots[i].data[(at % SECTOR as u64) as usize])
    }

    /// [`Cache::byte`] for a write path.
    fn byte_rw<RW: BlockReader + BlockWriter>(&mut self, rw: &RW, g: &Geometry, at: u64) -> Result<u8, FsError> {
        let i = self.slot_rw(rw, g, (at / SECTOR as u64) as u32)?;
        Ok(self.slots[i].data[(at % SECTOR as u64) as usize])
    }

    /// Byte `at` of the first FAT, to be changed: its slot marked dirty.
    fn byte_mut<RW: BlockReader + BlockWriter>(&mut self, rw: &RW, g: &Geometry, at: u64) -> Result<&mut u8, FsError> {
        let i = self.slot_rw(rw, g, (at / SECTOR as u64) as u32)?;
        self.slots[i].dirty = true;
        Ok(&mut self.slots[i].data[(at % SECTOR as u64) as usize])
    }

    /// **The raw entry for cluster `c`**, as its FAT stores it: 12, 16 or 28 bits.
    pub fn raw<R: BlockReader>(&mut self, r: &R, g: &Geometry, c: u32) -> Result<u32, FsError> {
        entry(g.kind, c, |at| self.byte(r, g, at))
    }

    /// **What follows cluster `c`**, which must be a data cluster. An entry naming a cluster
    /// outside the volume is `Corrupt`.
    pub fn next<R: BlockReader>(&mut self, r: &R, g: &Geometry, c: u32) -> Result<Next, FsError> {
        if !g.valid_cluster(c) {
            return Err(FsError::Corrupt);
        }
        classify(g, self.raw(r, g, c)?)
    }

    /// [`Cache::next`] for a write path, which may have every slot dirty.
    fn next_rw<RW: BlockReader + BlockWriter>(&mut self, rw: &RW, g: &Geometry, c: u32) -> Result<Next, FsError> {
        if !g.valid_cluster(c) {
            return Err(FsError::Corrupt);
        }
        classify(g, entry(g.kind, c, |at| self.byte_rw(rw, g, at))?)
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

    /// **Set cluster `c`'s entry to `v`** — in the cache, written by the next [`Cache::flush`]. A
    /// FAT12 entry shares a byte with its neighbour, kept; a FAT32 entry keeps its top four bits,
    /// which are reserved.
    pub fn set<RW: BlockReader + BlockWriter>(&mut self, rw: &RW, g: &Geometry, c: u32, v: u32) -> Result<(), FsError> {
        if !g.valid_cluster(c) {
            return Err(FsError::Corrupt);
        }
        let c64 = c as u64;
        match g.kind {
            Kind::Fat12 => {
                let at = c64 + c64 / 2;
                let v = v & 0xFFF;
                if c & 1 == 1 {
                    let b = self.byte_mut(rw, g, at)?;
                    *b = (*b & 0x0F) | ((v & 0xF) << 4) as u8;
                    *self.byte_mut(rw, g, at + 1)? = (v >> 4) as u8;
                } else {
                    *self.byte_mut(rw, g, at)? = v as u8;
                    let b = self.byte_mut(rw, g, at + 1)?;
                    *b = (*b & 0xF0) | (v >> 8) as u8;
                }
            }
            Kind::Fat16 => {
                *self.byte_mut(rw, g, c64 * 2)? = v as u8;
                *self.byte_mut(rw, g, c64 * 2 + 1)? = (v >> 8) as u8;
            }
            Kind::Fat32 => {
                for k in 0..3 {
                    *self.byte_mut(rw, g, c64 * 4 + k)? = (v >> (8 * k)) as u8;
                }
                let b = self.byte_mut(rw, g, c64 * 4 + 3)?;
                *b = (*b & 0xF0) | ((v >> 24) & 0x0F) as u8;
            }
        }
        Ok(())
    }

    /// **Write every dirty sector to every copy of the FAT**, a run of adjacent ones in one write
    /// per copy: what batching the FAT's sectors means. How many writes it took.
    pub fn flush<RW: BlockReader + BlockWriter>(&mut self, rw: &RW, g: &Geometry) -> Result<usize, FsError> {
        let mut order = [0usize; CACHE_SECTORS];
        let mut n = 0;
        for (i, s) in self.slots.iter().enumerate() {
            if s.dirty {
                order[n] = i;
                n += 1;
            }
        }
        order[..n].sort_unstable_by_key(|&i| self.slots[i].sector);
        let mut buf = [0u8; CACHE_SECTORS * SECTOR as usize];
        let mut writes = 0;
        let mut k = 0;
        while k < n {
            let first = self.slots[order[k]].sector;
            let mut len = 0;
            while k + len < n && self.slots[order[k + len]].sector == first + len as u32 {
                let at = len * SECTOR as usize;
                buf[at..at + SECTOR as usize].copy_from_slice(&self.slots[order[k + len]].data);
                len += 1;
            }
            for copy in 0..g.fats {
                rw.write_at(g.fat_byte(copy) + first as u64 * SECTOR as u64, &buf[..len * SECTOR as usize])?;
                writes += 1;
            }
            for j in 0..len {
                self.slots[order[k + j]].dirty = false;
            }
            k += len;
        }
        Ok(writes)
    }

    /// **Allocate `n` clusters**, the first free ones from `hint` on, wrapping: each handed to
    /// `each` as it is taken, so a caller can zero them, then linked in the order found and ended.
    /// So they are contiguous wherever the free space is. The first, and where the next search
    /// should begin. **All or nothing**: a volume that runs out gives back what it took, and the
    /// allocation is `TooLarge`, as a full ext4's is.
    pub fn allocate<RW: BlockReader + BlockWriter>(
        &mut self,
        rw: &RW,
        g: &Geometry,
        n: u32,
        hint: u32,
        mut each: impl FnMut(u32) -> Result<(), FsError>,
    ) -> Result<(u32, u32), FsError> {
        if n == 0 {
            return Err(FsError::Corrupt);
        }
        let after = |c: u32| if g.valid_cluster(c + 1) { c + 1 } else { 2 };
        let (mut first, mut last, mut got) = (0u32, 0u32, 0u32);
        let mut c = if g.valid_cluster(hint) { hint } else { 2 };
        for _ in 0..g.clusters {
            if self.next_rw(rw, g, c)? == Next::Free {
                self.set(rw, g, c, end_mark(g))?;
                if got == 0 {
                    first = c;
                } else {
                    self.set(rw, g, last, c)?;
                }
                last = c;
                got += 1;
                if let Err(e) = each(c) {
                    self.free_chain(rw, g, first)?;
                    return Err(e);
                }
                if got == n {
                    return Ok((first, after(c)));
                }
            }
            c = after(c);
        }
        if got > 0 {
            self.free_chain(rw, g, first)?;
        }
        Err(FsError::TooLarge)
    }

    /// **Free the chain from `first`**, every cluster of it, bounded as a walk is.
    pub fn free_chain<RW: BlockReader + BlockWriter>(&mut self, rw: &RW, g: &Geometry, first: u32) -> Result<(), FsError> {
        let mut c = first;
        for _ in 0..g.clusters {
            let next = self.next_rw(rw, g, c)?;
            self.set(rw, g, c, 0)?;
            match next {
                Next::Cluster(n) => c = n,
                Next::End => return Ok(()),
                Next::Free | Next::Bad => return Err(FsError::Corrupt),
            }
        }
        Err(FsError::Corrupt)
    }

    /// **How many clusters are free**, counted off the device's first FAT a window at a time. What
    /// the cache holds is not counted, so a caller flushes first.
    pub fn count_free<R: BlockReader>(r: &R, g: &Geometry) -> Result<u32, FsError> {
        const WINDOW: u64 = 32 * 1024;
        let mut buf = [0u8; WINDOW as usize];
        let fat_bytes = g.fat_sectors as u64 * SECTOR as u64;
        let (mut base, mut len) = (0u64, 0u64);
        let mut free = 0;
        for c in 2..g.clusters + 2 {
            let v = entry(g.kind, c, |at| {
                if at < base || at >= base + len {
                    base = at - at % SECTOR as u64;
                    len = (fat_bytes - base).min(WINDOW);
                    if at >= base + len {
                        return Err(FsError::Corrupt);
                    }
                    r.read_at(g.fat_byte(0) + base, &mut buf[..len as usize])?;
                }
                Ok(buf[(at - base) as usize])
            })?;
            if v == 0 {
                free += 1;
            }
        }
        Ok(free)
    }
}

/// **Cluster `c`'s entry**, its bytes read through `byte`: 12, 16 or 28 bits.
fn entry(kind: Kind, c: u32, mut byte: impl FnMut(u64) -> Result<u8, FsError>) -> Result<u32, FsError> {
    let c64 = c as u64;
    Ok(match kind {
        Kind::Fat12 => {
            let at = c64 + c64 / 2;
            let v = byte(at)? as u32 | (byte(at + 1)? as u32) << 8;
            if c & 1 == 1 { v >> 4 } else { v & 0xFFF }
        }
        Kind::Fat16 => byte(c64 * 2)? as u32 | (byte(c64 * 2 + 1)? as u32) << 8,
        Kind::Fat32 => {
            let mut v = 0u32;
            for k in 0..4 {
                v |= (byte(c64 * 4 + k)? as u32) << (8 * k);
            }
            v & 0x0FFF_FFFF
        }
    })
}

/// What an entry's value says follows its cluster.
fn classify(g: &Geometry, v: u32) -> Result<Next, FsError> {
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

/// **The entry that ends a chain**, as `mkfs.fat` writes it.
pub fn end_mark(g: &Geometry) -> u32 {
    match g.kind {
        Kind::Fat12 => 0xFFF,
        Kind::Fat16 => 0xFFFF,
        Kind::Fat32 => 0x0FFF_FFFF,
    }
}
