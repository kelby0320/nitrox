//! **A master boot record**, built whole (Phase 6 Part G.2): what `disk --partition` writes for a
//! stick, which cameras, televisions and every other system read.
//!
//! **Only building.** The kernel reads tables at a disk's arrival (`kernel/src/drivers/partitions.rs`);
//! nothing in userspace reads an MBR. Entries are **LBA-only** in every sense that matters: a reader
//! goes by the first sector and the count, and the CHS fields are written as `fdisk` writes them —
//! the usual 255 heads and 63 sectors, clamped at cylinder 1023 — for the BIOSes that still look.
//!
//! **No MBR on a disk of 2 TiB or more**: an entry counts sectors in 32 bits, so a partition past
//! that would be cast to a smaller one, or clamped and the rest of the drive left unused, saying
//! nothing. [`build`] refuses such a disk, and a caller writes a GPT there.

/// FAT32 with LBA addressing: what a FAT on a stick is typed, and what `disk --partition` types its
/// partition for the default filesystem.
pub const TYPE_FAT32_LBA: u8 = 0x0C;
/// FAT16 with LBA addressing.
pub const TYPE_FAT16_LBA: u8 = 0x0E;
/// A Linux filesystem.
pub const TYPE_LINUX: u8 = 0x83;

/// Bytes in the record: one 512-byte sector.
pub const LEN: usize = 512;
/// The most sectors a disk may have for an MBR to describe all of it: 2 TiB less a sector.
pub const MAX_BLOCKS: u64 = u32::MAX as u64;
/// Where the four entries begin.
const ENTRIES: usize = 0x1BE;

/// One partition: its type byte, its first sector and how many.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partition {
    /// Its type byte: [`TYPE_FAT32_LBA`], [`TYPE_LINUX`], …; `0` marks an unused entry.
    pub kind: u8,
    /// Its first sector.
    pub first_lba: u64,
    /// How many sectors it holds.
    pub blocks: u64,
}

/// Why a record could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildError {
    /// A disk of more than [`MAX_BLOCKS`] sectors: one for a GPT.
    DiskTooLarge,
    /// More than four partitions; there are no extended partitions here.
    TooManyPartitions,
    /// A partition starting at sector 0, the record's own, holding nothing, or running past the
    /// disk's end; or a type of `0`, which marks an unused entry.
    OutOfRange,
    /// Two partitions sharing a sector.
    Overlap,
}

/// The CHS triple an entry carries for `lba`, as `fdisk` writes it: 255 heads and 63 sectors a
/// track, and cylinder 1023's last sector for anything past it.
fn chs(lba: u64) -> [u8; 3] {
    let (heads, sectors) = (255u64, 63u64);
    let cylinder = lba / (heads * sectors);
    if cylinder > 1023 {
        return [0xFE, 0xFF, 0xFF];
    }
    let head = (lba / sectors) % heads;
    let sector = lba % sectors + 1;
    [head as u8, (sector as u8) | ((cylinder >> 2) as u8 & 0xC0), cylinder as u8]
}

/// **Build the record** for a disk of `disk_blocks` sectors holding `partitions`, into `out`.
///
/// `disk_id` is the 32-bit disk signature systems tell disks apart by — the caller's, from its
/// entropy, as [`crate::table::build`] takes its GUID. No partition is marked active, and the boot
/// code is two instructions: `int 0x18`, telling a BIOS that tried to boot this to try the next
/// device, then a spin.
pub fn build(disk_blocks: u64, disk_id: u32, partitions: &[Partition], out: &mut [u8; LEN]) -> Result<(), BuildError> {
    if disk_blocks > MAX_BLOCKS {
        return Err(BuildError::DiskTooLarge);
    }
    if partitions.len() > 4 {
        return Err(BuildError::TooManyPartitions);
    }
    for (i, p) in partitions.iter().enumerate() {
        let end = p.first_lba.checked_add(p.blocks).ok_or(BuildError::OutOfRange)?;
        if p.kind == 0 || p.first_lba == 0 || p.blocks == 0 || end > disk_blocks {
            return Err(BuildError::OutOfRange);
        }
        if partitions[..i].iter().any(|q| p.first_lba < q.first_lba + q.blocks && q.first_lba < end) {
            return Err(BuildError::Overlap);
        }
    }
    out.fill(0);
    out[0..4].copy_from_slice(&[0xCD, 0x18, 0xEB, 0xFE]);
    out[440..444].copy_from_slice(&disk_id.to_le_bytes());
    for (i, p) in partitions.iter().enumerate() {
        let e = &mut out[ENTRIES + 16 * i..ENTRIES + 16 * (i + 1)];
        e[1..4].copy_from_slice(&chs(p.first_lba));
        e[4] = p.kind;
        e[5..8].copy_from_slice(&chs(p.first_lba + p.blocks - 1));
        // Both fit: the disk is at most `MAX_BLOCKS` sectors, checked above.
        e[8..12].copy_from_slice(&(p.first_lba as u32).to_le_bytes());
        e[12..16].copy_from_slice(&(p.blocks as u32).to_le_bytes());
    }
    out[510] = 0x55;
    out[511] = 0xAA;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64 MiB disk.
    const DISK: u64 = 131_072;

    fn one(kind: u8, first_lba: u64, blocks: u64) -> Partition {
        Partition { kind, first_lba, blocks }
    }

    fn built(disk: u64, parts: &[Partition]) -> [u8; LEN] {
        let mut out = [0x77u8; LEN];
        build(disk, 0x1234_ABCD, parts, &mut out).unwrap();
        out
    }

    /// **The bytes, laid out by hand**: a FAT32 partition from 1 MiB to the end of a 64 MiB disk,
    /// as the specification places each field — so the builder is held to the format, not to its
    /// own idea of it. The entry is byte for byte the one `sfdisk` writes for that layout.
    #[test]
    fn a_record_is_laid_out_as_the_format_says() {
        let mut want = [0u8; LEN];
        want[0..4].copy_from_slice(&[0xCD, 0x18, 0xEB, 0xFE]);
        want[440..444].copy_from_slice(&[0xCD, 0xAB, 0x34, 0x12]);
        want[446..462].copy_from_slice(&[
            0x00, // not active
            0x20, 0x21, 0x00, // CHS of 2048: head 32, sector 33, cylinder 0
            0x0C, // FAT32, LBA
            0x28, 0x20, 0x08, // CHS of 131,071: head 40, sector 32, cylinder 8
            0x00, 0x08, 0x00, 0x00, // first sector, 2048
            0x00, 0xF8, 0x01, 0x00, // 129,024 sectors
        ]);
        want[510] = 0x55;
        want[511] = 0xAA;
        assert_eq!(built(DISK, &[one(TYPE_FAT32_LBA, 2048, DISK - 2048)]), want);
    }

    /// **A record built here is what `sfdisk` reads**: its id, and each partition's start, size and
    /// type.
    #[test]
    fn sfdisk_reads_a_record_built_here_as_written() {
        let path = std::env::temp_dir().join(format!("nitrox-mbr-{}.img", std::process::id()));
        let parts = [one(TYPE_FAT32_LBA, 2048, 60_000), one(TYPE_LINUX, 64_000, DISK - 64_000)];
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(DISK * 512).unwrap();
        std::os::unix::fs::FileExt::write_all_at(&f, &built(DISK, &parts), 0).unwrap();
        let out = std::process::Command::new("sfdisk")
            .arg("-J")
            .arg(&path)
            .output()
            .expect("sfdisk must be installed (util-linux) to run libgpt's tests");
        let _ = std::fs::remove_file(&path);
        assert!(out.status.success(), "sfdisk: {}", String::from_utf8_lossy(&out.stderr));
        let said: String = String::from_utf8(out.stdout).unwrap().split_whitespace().collect();
        assert!(said.contains(r#""label":"dos","id":"0x1234abcd""#), "{said}");
        let first = r#""start":2048,"size":60000,"type":"c"}"#;
        let second = format!(r#""start":64000,"size":{},"type":"83"}}"#, DISK - 64_000);
        assert!(said.contains(first) && said.contains(&second), "{said}");
        assert_eq!(said.matches(r#""start""#).count(), 2, "{said}");
    }

    /// **No record for a disk of 2 TiB or more** — at its neighbours: 2 TiB less a sector is
    /// described whole, and 2 TiB is refused rather than wrapped or clamped.
    #[test]
    fn a_disk_of_two_tib_or_more_is_refused() {
        let all = MAX_BLOCKS - 2048;
        let r = built(MAX_BLOCKS, &[one(TYPE_LINUX, 2048, all)]);
        assert_eq!(u32::from_le_bytes(r[458..462].try_into().unwrap()), all as u32, "the whole disk");
        assert_eq!(r[451..454], [0xFE, 0xFF, 0xFF], "its end past cylinder 1023");
        let mut out = [0u8; LEN];
        let p = [one(TYPE_LINUX, 2048, MAX_BLOCKS + 1 - 2048)];
        assert_eq!(build(MAX_BLOCKS + 1, 1, &p, &mut out), Err(BuildError::DiskTooLarge));
        assert_eq!(build(MAX_BLOCKS + 1, 1, &[], &mut out), Err(BuildError::DiskTooLarge));
    }

    /// **What would describe a broken disk is refused**, and four partitions are taken.
    #[test]
    fn a_record_that_would_lose_data_is_not_built() {
        let mut out = [0u8; LEN];
        let mut refused = |parts: &[Partition]| build(DISK, 1, parts, &mut out).unwrap_err();
        assert_eq!(refused(&[one(TYPE_LINUX, 0, 100)]), BuildError::OutOfRange, "sector 0 is the record");
        assert_eq!(refused(&[one(TYPE_LINUX, 2048, 0)]), BuildError::OutOfRange, "empty");
        assert_eq!(refused(&[one(0, 2048, 100)]), BuildError::OutOfRange, "type 0 is unused");
        assert_eq!(refused(&[one(TYPE_LINUX, 2048, DISK - 2047)]), BuildError::OutOfRange, "a sector past the end");
        assert_eq!(refused(&[one(TYPE_LINUX, u64::MAX, 2)]), BuildError::OutOfRange, "no overflow");
        let shared = [one(TYPE_LINUX, 2048, 1000), one(TYPE_LINUX, 3047, 1000)];
        assert_eq!(refused(&shared), BuildError::Overlap, "one shared sector");
        let five = [1u64, 2, 3, 4, 5].map(|i| one(TYPE_LINUX, i * 1000, 10));
        assert_eq!(refused(&five), BuildError::TooManyPartitions);
        assert!(build(DISK, 1, &five[..4], &mut out).is_ok());
        assert!(build(DISK, 1, &[one(TYPE_LINUX, 2048, DISK - 2048)], &mut out).is_ok(), "to the last sector");
        let touching = [one(TYPE_LINUX, 2048, 1000), one(TYPE_LINUX, 3048, 1000)];
        assert!(build(DISK, 1, &touching, &mut out).is_ok(), "adjacent, not overlapping");
    }

    /// The CHS triple at the edges of its range.
    #[test]
    fn chs_is_fdisks_and_clamps_past_cylinder_1023() {
        assert_eq!(chs(0), [0, 1, 0]);
        assert_eq!(chs(62), [0, 63, 0]);
        assert_eq!(chs(63), [1, 1, 0]);
        let last = 1024 * 255 * 63 - 1;
        assert_eq!(chs(last), [254, 63 | 0xC0, 0xFF], "cylinder 1023, its high bits in the sector byte");
        assert_eq!(chs(last + 1), [0xFE, 0xFF, 0xFF]);
    }
}
