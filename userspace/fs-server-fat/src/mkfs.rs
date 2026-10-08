//! **Making an empty FAT** (Phase 6 Part G.2): what `disk --format DEVICE fat` writes.
//!
//! ## What it makes, by size
//!
//! **Clusters are never under 4 KiB** — [`MIN_CLUSTER`], the server's floor — so everything this
//! makes, the server serves:
//!
//! - **FAT32** wherever 4 KiB clusters reach FAT32's 65,525, from [`FAT32_FROM`] sectors (about
//!   256.5 MiB), with Microsoft's steps: 4 KiB clusters up to 8 GiB, 8 KiB up to 16, 16 KiB up to 32
//!   and 32 KiB above.
//! - **FAT16** below that, at 4 KiB clusters while they keep its count under 65,525 — **and at 8 KiB
//!   in the quarter-mebibyte below FAT32's line**, from [`FAT16_BAND`] sectors, where at 4 KiB
//!   neither type is valid: FAT16 passes 65,524 clusters there and FAT32 has not reached 65,525.
//! - **Refused under [`MIN_SECTORS`]**, 16 MiB, a round floor 8 KiB above where 4 KiB clusters stop
//!   making a FAT16 at all; and **over 2 TiB**, FAT32's sector count being 32 bits.
//!
//! Each type is what Linux, `fsck.fat` and mtools read it as too ([`crate::bpb`]): a FAT32's 16-bit
//! FAT size is zero, and a FAT16's cluster count is in FAT16's range.
//!
//! ## The layout
//!
//! Two FATs, as every system expects. **The data region begins on a cluster**, so a cluster begins
//! on a cluster's multiple from the volume's start — and a volume at 1 MiB, where `disk` puts
//! every partition, has its clusters aligned on the disk too. The padding goes into the reserved
//! region, which a reader skips by its count. Each FAT is sized for every cluster the volume could
//! hold before the FATs are taken from it, a few sectors more than it strictly needs, so it is
//! never short.
//!
//! **The first mebibyte is zeroed first**, so no old boot sector or superblock outlives the format
//! for a reader to find — an ext4's is at 1024 — and so are the FATs and the root directory, which
//! a reader trusts entirely. **The boot sector is written last**: until it is, the volume reads as
//! nothing, rather than as a FAT whose tables are half written. Data clusters are left as they
//! were; the FAT says they are free.
//!
//! ## The oracle
//!
//! `fsck.fat -n` finds what this writes clean, mtools reads what the library then writes to it,
//! and [`crate::Fat`] serves it — none alone being enough, as for `fs-server-ext4`'s formatter.

use crate::bpb::{self, Geometry, Kind, MIN_CLUSTER, SECTOR};
use crate::{BlockWriter, FsError};

/// The fewest sectors formatted: 16 MiB. See the module doc.
pub const MIN_SECTORS: u64 = 32 * 1024;
/// Where FAT16 takes 8 KiB clusters, since 4 KiB would pass its 65,524: 256.22 MiB.
pub const FAT16_BAND: u64 = 524_752;
/// Where FAT32 begins, its 4 KiB clusters reaching 65,525: 256.47 MiB.
pub const FAT32_FROM: u64 = 525_264;

/// A GiB, in sectors.
const GIB: u64 = 1 << 21;
/// FAT16's fixed root directory: 512 entries, the usual count, 32 sectors.
const ROOT_ENTRIES: u32 = 512;
/// The reserved sectors FAT32 starts from, before the data region's padding: Microsoft's count,
/// room for the FSInfo and the backup boot sector.
const FAT32_RESERVED: u32 = 32;
/// FAT32's FSInfo sector, and its backup boot sector — whose FSInfo copy follows it.
const FSINFO: u32 = 1;
const BACKUP_BOOT: u32 = 6;
/// The fewest and most clusters each type may have, by the specification.
const FAT16_CLUSTERS: core::ops::RangeInclusive<u32> = bpb::FAT12_CLUSTERS..=65_524;
const FAT32_CLUSTERS: core::ops::RangeInclusive<u32> = 65_525..=0x0FFF_FFF5;
/// The fixed-disk media byte, which entry 0 of each FAT repeats.
const MEDIA: u8 = 0xF8;
/// How much of the volume is zeroed at a time.
const ZERO_SPAN: usize = 64 * 1024;
/// The volume label every system reads as none.
pub const NO_NAME: [u8; 11] = *b"NO NAME    ";

/// What to make.
#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// Sectors in the volume: its partition's length over 512.
    pub sectors: u64,
    /// The volume label, from [`label`]: in the boot sector, and as the root directory's first
    /// entry unless it is [`NO_NAME`].
    pub label: [u8; 11],
    /// The volume ID, which systems tell volumes apart by. **The caller's**, from the kernel's
    /// entropy, as `fs-server-ext4`'s formatter takes its UUID.
    pub volume_id: u32,
    /// The volume's first sector on its disk: the boot sector's hidden sectors, which only
    /// booting reads.
    pub hidden: u32,
    /// Seconds since the epoch, for the label entry's times.
    pub now: i64,
}

/// Why a FAT cannot be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MkfsError {
    /// Under [`MIN_SECTORS`].
    TooSmall {
        /// Sectors asked for.
        have: u64,
        /// The fewest made.
        need: u64,
    },
    /// Over FAT32's 32-bit sector count.
    TooLarge,
    /// A label over 11 bytes.
    LabelTooLong,
    /// A byte a FAT label may not hold: punctuation a short name forbids, a control character,
    /// or anything not ASCII.
    LabelChar(u8),
}

impl MkfsError {
    /// The error a caller sees from [`format`].
    pub fn fs_error(self) -> FsError {
        match self {
            MkfsError::TooSmall { .. } | MkfsError::TooLarge => FsError::TooLarge,
            MkfsError::LabelTooLong | MkfsError::LabelChar(_) => FsError::InvalidName,
        }
    }
}

impl core::fmt::Display for MkfsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            MkfsError::TooSmall { have, need } => {
                write!(f, "{} KiB is too small for a FAT; it needs {} MiB", have / 2, need >> 11)
            }
            MkfsError::TooLarge => write!(f, "a FAT holds at most 2 TiB"),
            MkfsError::LabelTooLong => write!(f, "a FAT label is at most 11 characters"),
            MkfsError::LabelChar(c) if c.is_ascii_graphic() => {
                write!(f, "a FAT label cannot hold {:?}", c as char)
            }
            MkfsError::LabelChar(c) => write!(f, "a FAT label cannot hold the byte {c:#04x}"),
        }
    }
}

/// **A label as FAT keeps it**: uppercased, space-padded to 11 bytes; [`NO_NAME`] for an empty
/// one. Refused past 11 bytes, and for any byte a short name may not hold — a FAT label is one.
pub fn label(text: &[u8]) -> Result<[u8; 11], MkfsError> {
    if text.len() > 11 {
        return Err(MkfsError::LabelTooLong);
    }
    let mut out = [b' '; 11];
    for (o, &c) in out.iter_mut().zip(text) {
        let ok = c.is_ascii_alphanumeric() || c == b' ' || b"!#$%&'()-@^_`{}~".contains(&c);
        if !ok {
            return Err(MkfsError::LabelChar(c));
        }
        *o = c.to_ascii_uppercase();
    }
    Ok(if out == [b' '; 11] { NO_NAME } else { out })
}

/// FAT32's sectors per cluster for a volume of `sectors`: Microsoft's steps, from 4 KiB.
fn fat32_cluster(sectors: u64) -> u32 {
    match sectors {
        s if s <= 8 * GIB => 8,
        s if s <= 16 * GIB => 16,
        s if s <= 32 * GIB => 32,
        _ => 64,
    }
}

/// **The layout of `kind` at `spc` sectors a cluster** on `total` sectors, or `None` when its
/// cluster count is outside the type's range.
fn layout(kind: Kind, total: u32, spc: u32) -> Option<Geometry> {
    let (reserved_min, root_entries, entry_bytes) = match kind {
        Kind::Fat32 => (FAT32_RESERVED, 0, 4u64),
        _ => (1, ROOT_ENTRIES, 2),
    };
    let root_sectors = root_entries * 32 / SECTOR;
    // Sized for every cluster the volume could hold before the FATs are taken from it.
    let most = (total - reserved_min - root_sectors) / spc;
    let fat_sectors = ((most as u64 + 2) * entry_bytes).div_ceil(SECTOR as u64) as u32;
    let data_start = (reserved_min + 2 * fat_sectors + root_sectors).next_multiple_of(spc);
    if data_start >= total {
        return None;
    }
    let clusters = (total - data_start) / spc;
    let range = if kind == Kind::Fat32 { FAT32_CLUSTERS } else { FAT16_CLUSTERS };
    if !range.contains(&clusters) {
        return None;
    }
    let reserved = data_start - 2 * fat_sectors - root_sectors;
    let is32 = kind == Kind::Fat32;
    Some(Geometry {
        kind,
        sectors_per_cluster: spc,
        reserved,
        fats: 2,
        fat_sectors,
        root_entries,
        root_start: reserved + 2 * fat_sectors,
        root_sectors,
        root_cluster: if is32 { 2 } else { 0 },
        data_start,
        total_sectors: total,
        clusters,
        fsinfo: if is32 { FSINFO } else { 0 },
        state_at: if is32 { 0x41 } else { 0x25 },
        label: NO_NAME,
    })
}

/// **Where everything goes**, from the request alone, so it can be checked without writing a byte.
pub fn plan(p: &Params) -> Result<Geometry, MkfsError> {
    if p.sectors < MIN_SECTORS {
        return Err(MkfsError::TooSmall { have: p.sectors, need: MIN_SECTORS });
    }
    let total = u32::try_from(p.sectors).map_err(|_| MkfsError::TooLarge)?;
    let min = MIN_CLUSTER / SECTOR;
    let g = layout(Kind::Fat32, total, fat32_cluster(p.sectors))
        .or_else(|| layout(Kind::Fat16, total, min))
        .or_else(|| layout(Kind::Fat16, total, 2 * min))
        // Unreachable from `MIN_SECTORS` up — the sweep in the tests holds that — but a size no
        // type takes is a refusal, not a panic.
        .ok_or(MkfsError::TooSmall { have: p.sectors, need: MIN_SECTORS })?;
    Ok(Geometry { label: p.label, ..g })
}

/// **The boot sector** for `g`.
pub fn boot_sector(g: &Geometry, p: &Params) -> [u8; 512] {
    let mut b = [0u8; 512];
    let is32 = g.kind == Kind::Fat32;
    // A jump past the parameter block, to two instructions: `int 0x18`, which tells a BIOS that
    // booted this to try the next device, then a spin.
    let code = if is32 { 0x5A } else { 0x3E };
    b[0..3].copy_from_slice(&[0xEB, code as u8 - 2, 0x90]);
    b[code..code + 4].copy_from_slice(&[0xCD, 0x18, 0xEB, 0xFE]);
    b[3..11].copy_from_slice(b"NITROX  ");
    b[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
    b[13] = g.sectors_per_cluster as u8;
    b[14..16].copy_from_slice(&(g.reserved as u16).to_le_bytes());
    b[16] = g.fats as u8;
    b[17..19].copy_from_slice(&(g.root_entries as u16).to_le_bytes());
    if !is32 && g.total_sectors < 0x1_0000 {
        b[19..21].copy_from_slice(&(g.total_sectors as u16).to_le_bytes());
    } else {
        b[32..36].copy_from_slice(&g.total_sectors.to_le_bytes());
    }
    b[21] = MEDIA;
    if !is32 {
        b[22..24].copy_from_slice(&(g.fat_sectors as u16).to_le_bytes());
    }
    // A geometry only a CHS reader looks at: the usual translation.
    b[24..26].copy_from_slice(&63u16.to_le_bytes());
    b[26..28].copy_from_slice(&255u16.to_le_bytes());
    b[28..32].copy_from_slice(&p.hidden.to_le_bytes());
    let ext = if is32 {
        b[36..40].copy_from_slice(&g.fat_sectors.to_le_bytes());
        // Flags 0 (the FATs mirrored) and version 0.0 at 40 and 42.
        b[44..48].copy_from_slice(&g.root_cluster.to_le_bytes());
        b[48..50].copy_from_slice(&(FSINFO as u16).to_le_bytes());
        b[50..52].copy_from_slice(&(BACKUP_BOOT as u16).to_le_bytes());
        0x40
    } else {
        0x24
    };
    // The extended boot record: drive, the state byte (`state_at`, clean), its signature, the
    // volume ID, the label and the type.
    b[ext] = 0x80;
    b[ext + 2] = 0x29;
    b[ext + 3..ext + 7].copy_from_slice(&p.volume_id.to_le_bytes());
    b[ext + 7..ext + 18].copy_from_slice(&g.label);
    b[ext + 18..ext + 26].copy_from_slice(if is32 { b"FAT32   " } else { b"FAT16   " });
    b[510] = 0x55;
    b[511] = 0xAA;
    b
}

/// FAT32's FSInfo: every cluster free but the root directory's, and the next search at the one
/// after it.
fn fsinfo(g: &Geometry) -> [u8; 512] {
    let mut s = [0u8; 512];
    s[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
    s[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
    s[488..492].copy_from_slice(&(g.clusters - 1).to_le_bytes());
    s[492..496].copy_from_slice(&3u32.to_le_bytes());
    s[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes());
    s
}

/// **Write an empty FAT over `w`**, and return the geometry it wrote. `progress` is told the
/// zeroing's progress in steps of 64 KiB — the FATs of a large drive are hundreds of mebibytes, and
/// a still screen for that long looks like a hang.
pub fn format<W: BlockWriter>(w: &W, p: &Params, progress: &mut dyn FnMut(u32, u32)) -> Result<Geometry, FsError> {
    let g = plan(p).map_err(MkfsError::fs_error)?;
    let s = SECTOR as u64;

    // The first mebibyte, the FATs and the root directory — FAT32's, its first cluster — zeroed.
    let meta_end = g.data_start as u64 + if g.kind == Kind::Fat32 { g.sectors_per_cluster as u64 } else { 0 };
    let zero_end = (meta_end * s).max(1 << 20).min(g.total_sectors as u64 * s);
    let zeros = [0u8; ZERO_SPAN];
    let steps = zero_end.div_ceil(ZERO_SPAN as u64) as u32;
    for i in 0..steps {
        let at = i as u64 * ZERO_SPAN as u64;
        w.write_at(at, &zeros[..(zero_end - at).min(ZERO_SPAN as u64) as usize])?;
        progress(i + 1, steps);
    }

    // Each FAT's first entries: the media byte, the clean entry 1, and FAT32's root directory's
    // end-of-chain.
    let mut fat = [0u8; 512];
    match g.kind {
        Kind::Fat32 => {
            fat[0..4].copy_from_slice(&(0x0FFF_FF00 | MEDIA as u32).to_le_bytes());
            fat[4..8].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
            fat[8..12].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
        }
        _ => {
            fat[0..2].copy_from_slice(&(0xFF00 | MEDIA as u16).to_le_bytes());
            fat[2..4].copy_from_slice(&0xFFFFu16.to_le_bytes());
        }
    }
    for n in 0..g.fats {
        w.write_at(g.fat_byte(n), &fat)?;
    }

    // The label, as the root directory's first entry.
    if g.label != NO_NAME {
        let root = match g.kind {
            Kind::Fat32 => g.cluster_sector(g.root_cluster),
            _ => g.root_start as u64,
        };
        let mut e = [0u8; 32];
        e[0..11].copy_from_slice(&g.label);
        e[11] = 0x08;
        let (date, time) = crate::time::to_fat(p.now);
        for at in [14, 22] {
            e[at..at + 2].copy_from_slice(&time.to_le_bytes());
            e[at + 2..at + 4].copy_from_slice(&date.to_le_bytes());
        }
        e[18..20].copy_from_slice(&date.to_le_bytes());
        w.write_at(root * s, &e)?;
    }

    // The boot sector, last: FAT32's FSInfo and backup first.
    let boot = boot_sector(&g, p);
    if g.kind == Kind::Fat32 {
        let info = fsinfo(&g);
        w.write_at(FSINFO as u64 * s, &info)?;
        w.write_at((BACKUP_BOOT + FSINFO) as u64 * s, &info)?;
        w.write_at(BACKUP_BOOT as u64 * s, &boot)?;
    }
    w.write_at(0, &boot)?;
    Ok(g)
}

#[cfg(test)]
mod tests;
