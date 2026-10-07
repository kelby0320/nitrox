//! **The boot sector** (Phase 6 Part E.2): a FAT's type and geometry, read from its BIOS parameter
//! block, and the reasons a server will not serve one.
//!
//! **The type is read as Linux, `fsck.fat` and mtools read it**: FAT32 when the boot sector's
//! 16-bit FAT size is zero, else FAT12 or FAT16 by cluster count. The specification decides by
//! count alone. The two agree on every volume it allows, and differ on a FAT32 with too few
//! clusters — which `mkfs.fat -F 32` writes, with a warning, on anything under 257 MiB of 4 KiB
//! clusters — so deciding by count would read such a volume's 32-bit entries as 16-bit ones (PR
//! #364 review).

use crate::FsError;

/// The one sector size served: what every stick this phase meets uses, and the kernel's block.
pub const SECTOR: u32 = 512;
/// The smallest cluster a server takes: a page, so no page of a file spans two runs of its map
/// (the kernel fills and writes a page as one device range).
pub const MIN_CLUSTER: u32 = 4096;
/// Below this many clusters a FAT that is not FAT32 is FAT12, at or above it FAT16: the
/// specification's bound.
pub const FAT12_CLUSTERS: u32 = 4085;

/// Which FAT.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Fat12,
    Fat16,
    Fat32,
}

/// **A FAT's geometry**, in sectors from the start of the volume unless it says otherwise.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub kind: Kind,
    pub sectors_per_cluster: u32,
    /// The reserved region: the boot sector, and FAT32's FSInfo and backup boot sector.
    pub reserved: u32,
    /// How many copies of the FAT, and each one's length.
    pub fats: u32,
    pub fat_sectors: u32,
    /// FAT12 and FAT16's **fixed root directory**: its entries, first sector and length. Zero on
    /// FAT32, whose root is a cluster chain.
    pub root_entries: u32,
    pub root_start: u32,
    pub root_sectors: u32,
    /// FAT32's root directory's first cluster; `0` on FAT12 and FAT16.
    pub root_cluster: u32,
    /// Where cluster 2 begins.
    pub data_start: u32,
    pub total_sectors: u32,
    /// **How many data clusters**: valid cluster numbers are `2..clusters + 2`.
    pub clusters: u32,
    /// FAT32's FSInfo sector, or `0` when there is none.
    pub fsinfo: u32,
    /// The byte, in the boot sector, whose bit 0 says the filesystem is mounted or was not
    /// cleanly unmounted — what `fsck.fat` and Linux read.
    pub state_at: u32,
    /// The volume label from the extended boot record, space-padded; all spaces when there is
    /// none, as there is no extended record.
    pub label: [u8; 11],
}

impl Geometry {
    /// A cluster's length in bytes.
    pub fn cluster_bytes(&self) -> u32 {
        self.sectors_per_cluster * SECTOR
    }

    /// Whether `c` names a data cluster.
    pub fn valid_cluster(&self, c: u32) -> bool {
        c >= 2 && c - 2 < self.clusters
    }

    /// The first sector of data cluster `c`, which must be [`Geometry::valid_cluster`].
    pub fn cluster_sector(&self, c: u32) -> u64 {
        self.data_start as u64 + (c as u64 - 2) * self.sectors_per_cluster as u64
    }

    /// The byte at which copy `n` of the FAT begins.
    pub fn fat_byte(&self, n: u32) -> u64 {
        (self.reserved as u64 + n as u64 * self.fat_sectors as u64) * SECTOR as u64
    }
}

/// **Why a device is not served.**
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Unservable {
    /// The boot sector would not read.
    Unreadable,
    /// No FAT boot sector: no signature, or none of a FAT's fields.
    NotFat,
    /// Sectors of other than 512 bytes.
    SectorSize { bytes: u32 },
    /// A field that makes no FAT.
    Malformed(&'static str),
    /// Clusters smaller than a page.
    SmallClusters { bytes: u32 },
    /// The volume's last sector would not read: it says it is larger than its device.
    Truncated,
    /// A writable mount could not record itself mounted.
    StateUnwritable,
}

impl Unservable {
    /// What a client is told it means.
    pub fn fs_error(self) -> FsError {
        match self {
            Unservable::Unreadable | Unservable::Truncated | Unservable::StateUnwritable => FsError::Io,
            Unservable::NotFat | Unservable::Malformed(_) => FsError::Corrupt,
            Unservable::SectorSize { .. } | Unservable::SmallClusters { .. } => FsError::Unsupported,
        }
    }
}

impl libfsserver::Refusal for Unservable {
    fn fs_error(&self) -> FsError {
        Unservable::fs_error(*self)
    }
}

impl core::fmt::Display for Unservable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Unservable::Unreadable => write!(f, "the boot sector could not be read off the device"),
            Unservable::NotFat => write!(f, "no FAT boot sector"),
            Unservable::SectorSize { bytes } => write!(f, "{bytes}-byte sectors; only 512-byte sectors are served"),
            Unservable::Malformed(what) => write!(f, "a FAT whose boot sector makes no sense: {what}"),
            Unservable::SmallClusters { bytes } => write!(f, "{bytes}-byte clusters, smaller than a page"),
            Unservable::Truncated => write!(f, "the filesystem says it is larger than its device"),
            Unservable::StateUnwritable => write!(f, "the state could not be written"),
        }
    }
}

fn u16_at(b: &[u8], at: usize) -> u32 {
    u16::from_le_bytes([b[at], b[at + 1]]) as u32
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// **Parse a boot sector**: the geometry it describes, or why it describes none. Takes any
/// cluster size; [`servable`] is the server's rule.
pub fn parse(b: &[u8; 512]) -> Result<Geometry, Unservable> {
    if b[510] != 0x55 || b[511] != 0xAA || !matches!(b[0], 0xEB | 0xE9) {
        return Err(Unservable::NotFat);
    }
    let bytes = u16_at(b, 11);
    if !matches!(bytes, 512 | 1024 | 2048 | 4096) {
        return Err(Unservable::NotFat);
    }
    if bytes != SECTOR {
        return Err(Unservable::SectorSize { bytes });
    }
    let spc = b[13] as u32;
    if spc == 0 || !spc.is_power_of_two() {
        return Err(Unservable::Malformed("sectors per cluster is not a power of two"));
    }
    let reserved = u16_at(b, 14);
    let fats = b[16] as u32;
    let root_entries = u16_at(b, 17);
    let total16 = u16_at(b, 19);
    let fat16 = u16_at(b, 22);
    let total32 = u32_at(b, 32);
    let fat32 = u32_at(b, 36);
    if reserved == 0 {
        return Err(Unservable::Malformed("no reserved sectors"));
    }
    if fats == 0 {
        return Err(Unservable::Malformed("no FATs"));
    }
    let total = if total16 != 0 { total16 } else { total32 };
    let is32 = fat16 == 0;
    let fat_sectors = if is32 { fat32 } else { fat16 };
    if total == 0 || fat_sectors == 0 {
        return Err(Unservable::Malformed("a size of zero"));
    }
    if is32 && root_entries != 0 {
        return Err(Unservable::Malformed("a FAT32 with a fixed root directory"));
    }
    if !is32 && root_entries == 0 {
        return Err(Unservable::Malformed("a FAT12 or FAT16 with no root directory"));
    }
    let root_sectors = (root_entries * 32).div_ceil(SECTOR);
    let fats_end = (reserved as u64) + (fats as u64) * (fat_sectors as u64);
    let data_start = fats_end + root_sectors as u64;
    if data_start >= total as u64 {
        return Err(Unservable::Malformed("no room for data"));
    }
    let clusters = ((total as u64 - data_start) / spc as u64) as u32;
    if clusters == 0 {
        return Err(Unservable::Malformed("no data clusters"));
    }
    let kind = if is32 {
        Kind::Fat32
    } else if clusters < FAT12_CLUSTERS {
        Kind::Fat12
    } else {
        Kind::Fat16
    };
    // **The FAT must hold an entry for every cluster**: 12, 16 or 32 bits each, from entry 2.
    let need = match kind {
        Kind::Fat12 => (clusters as u64 + 2) * 3 / 2 + 1,
        Kind::Fat16 => (clusters as u64 + 2) * 2,
        Kind::Fat32 => (clusters as u64 + 2) * 4,
    };
    if (fat_sectors as u64) * (SECTOR as u64) < need {
        return Err(Unservable::Malformed("a FAT too short for its clusters"));
    }
    let (root_cluster, fsinfo, state_at, ext) = if is32 {
        let rc = u32_at(b, 44) & 0x0FFF_FFFF;
        let fi = u16_at(b, 48);
        (rc, if fi == 0 || fi == 0xFFFF || fi >= reserved { 0 } else { fi }, 0x41, 0x40usize)
    } else {
        (0, 0, 0x25, 0x24usize)
    };
    let mut label = [b' '; 11];
    if b[ext + 2] == 0x29 {
        label.copy_from_slice(&b[ext + 7..ext + 18]);
    }
    let g = Geometry {
        kind,
        sectors_per_cluster: spc,
        reserved,
        fats,
        fat_sectors,
        root_entries,
        root_start: fats_end as u32,
        root_sectors,
        root_cluster,
        data_start: data_start as u32,
        total_sectors: total,
        clusters,
        fsinfo,
        state_at,
        label,
    };
    if is32 && !g.valid_cluster(root_cluster) {
        return Err(Unservable::Malformed("a root directory outside the volume"));
    }
    Ok(g)
}

/// **The server's rule** on a geometry that parsed: clusters of at least a page.
pub fn servable(g: &Geometry) -> Result<(), Unservable> {
    if g.cluster_bytes() < MIN_CLUSTER {
        return Err(Unservable::SmallClusters { bytes: g.cluster_bytes() });
    }
    Ok(())
}

/// The label, its padding trimmed, `NO NAME` read as none: the bytes the storage service names a
/// mount by.
pub fn label(g: &Geometry) -> &[u8] {
    let end = g.label.iter().rposition(|&c| c != b' ' && c != 0).map_or(0, |p| p + 1);
    let l = &g.label[..end];
    if l == b"NO NAME" { &[] } else { l }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A FAT16-shaped boot sector: `total` sectors, one reserved, two FATs of `fat` sectors, a
    /// root of 512 entries, `spc` sectors a cluster.
    fn boot(total: u32, spc: u8, fat: u16) -> [u8; 512] {
        let mut b = [0u8; 512];
        b[0] = 0xEB;
        b[11..13].copy_from_slice(&512u16.to_le_bytes());
        b[13] = spc;
        b[14..16].copy_from_slice(&1u16.to_le_bytes());
        b[16] = 2;
        b[17..19].copy_from_slice(&512u16.to_le_bytes());
        if total < 0x10000 {
            b[19..21].copy_from_slice(&(total as u16).to_le_bytes());
        } else {
            b[32..36].copy_from_slice(&total.to_le_bytes());
        }
        b[22..24].copy_from_slice(&fat.to_le_bytes());
        b[510] = 0x55;
        b[511] = 0xAA;
        b
    }

    /// **The FAT12 bound, at its neighbours** (PR #364's review asked for the type both ways):
    /// 4,084 clusters is FAT12 and 4,085 is FAT16. One reserved sector, two FATs of 16 sectors —
    /// enough for either — and a root of 32 sectors put cluster 2 at sector 65; a cluster a sector.
    #[test]
    fn a_fat_is_fat12_below_4085_clusters_and_fat16_from_it() {
        let g = parse(&boot(65 + 4084, 1, 16)).unwrap();
        assert_eq!((g.clusters, g.kind), (4084, Kind::Fat12));
        let g = parse(&boot(65 + 4085, 1, 16)).unwrap();
        assert_eq!((g.clusters, g.kind), (4085, Kind::Fat16));
    }

    /// **A FAT32 by its boot sector, whatever its count** — what `mkfs.fat -F 32` writes on a small
    /// volume, read as Linux and the host tools read it.
    #[test]
    fn a_zero_16_bit_fat_size_is_fat32_whatever_the_count() {
        let mut b = boot(131_072, 8, 0);
        b[17..19].copy_from_slice(&0u16.to_le_bytes());
        b[36..40].copy_from_slice(&128u32.to_le_bytes());
        b[44..48].copy_from_slice(&2u32.to_le_bytes());
        let g = parse(&b).unwrap();
        assert!(g.clusters < 65_525, "few enough clusters that a count would say FAT16");
        assert_eq!(g.kind, Kind::Fat32);
    }

    /// **What a server refuses, and why**: another sector size, clusters under a page, and a
    /// sector that is no FAT's; and a page-sized cluster taken.
    #[test]
    fn a_server_refuses_other_sectors_small_clusters_and_what_is_not_fat() {
        let mut b = boot(32_768, 4, 32);
        b[11..13].copy_from_slice(&1024u16.to_le_bytes());
        assert_eq!(parse(&b), Err(Unservable::SectorSize { bytes: 1024 }));
        let g = parse(&boot(32_768, 4, 32)).unwrap();
        assert_eq!(servable(&g), Err(Unservable::SmallClusters { bytes: 2048 }));
        assert_eq!(format!("{}", servable(&g).unwrap_err()), "2048-byte clusters, smaller than a page");
        let g = parse(&boot(32_768, 8, 32)).unwrap();
        assert_eq!(servable(&g), Ok(()), "a page is enough");
        let mut b = boot(32_768, 8, 32);
        b[510] = 0;
        assert_eq!(parse(&b), Err(Unservable::NotFat));
    }

    /// **A boot sector's fields are anyone's bytes**: none of these may panic, and each is refused.
    #[test]
    fn a_malformed_boot_sector_is_refused_not_trusted() {
        let mut b = boot(32_768, 3, 32);
        assert!(matches!(parse(&b), Err(Unservable::Malformed(_))), "three sectors a cluster");
        b = boot(32_768, 8, 32);
        b[16] = 0;
        assert!(matches!(parse(&b), Err(Unservable::Malformed(_))), "no FATs");
        b = boot(100, 8, 200);
        assert!(matches!(parse(&b), Err(Unservable::Malformed(_))), "FATs past the end");
        b = boot(32_768, 8, 1);
        assert!(matches!(parse(&b), Err(Unservable::Malformed(_))), "a FAT too short for its clusters");
    }
}
