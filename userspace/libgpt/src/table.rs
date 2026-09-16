//! A GUID partition table, built whole or read whole.
//!
//! Layout, in logical blocks: block 0 is the **protective MBR** (one partition of type `0xEE`
//! spanning the disk, so a tool that understands only MBRs sees the disk as full rather than
//! empty); block 1 is the **header**; the **entry array** follows it; and the last block of the
//! disk holds a **backup header** with its own copy of the array below it. Every multi-byte field
//! is little-endian.
//!
//! **Both checksums are the point.** The header carries a CRC32 of itself (with the field zeroed)
//! and one of the entry array; firmware that finds either wrong falls back to the backup, and
//! firmware that finds both wrong boots nothing. They are the reason this crate exists rather than
//! a `write_all` of some bytes.

use crate::crc32::crc32;

/// The signature at the start of a header: `EFI PART`.
pub const SIGNATURE: &[u8; 8] = b"EFI PART";
/// Revision 1.0, as every GPT in the wild carries.
pub const REVISION: u32 = 0x0001_0000;
/// Header bytes that are defined; the rest of its block is zero.
pub const HEADER_LEN: usize = 92;
/// One entry's size. 128 is the minimum the standard allows and what every tool writes.
pub const ENTRY_LEN: usize = 128;
/// Entries in the array. 128 is the conventional count, and the 16 KiB it occupies is why the
/// first usable block is 34: one MBR, one header, 32 blocks of array.
pub const ENTRY_COUNT: u32 = 128;
/// Logical blocks the entry array occupies at 512 bytes a block.
pub const ARRAY_BLOCKS: u64 = (ENTRY_COUNT as u64 * ENTRY_LEN as u64) / 512;
/// The block the entry array starts at, and so the first block a partition may use.
pub const FIRST_USABLE: u64 = 2 + ARRAY_BLOCKS;
/// Bytes per logical block. **512 throughout**: the disks this writes to report 512, and a
/// 4096-byte-block disk would need every constant above recomputed rather than a flag.
pub const BLOCK: usize = 512;

/// The EFI system partition's type GUID, `C12A7328-F81F-11D2-BA4B-00A0C93EC93B`.
pub const TYPE_EFI_SYSTEM: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];
/// The Linux filesystem type GUID, `0FC63DAF-8483-4772-8E79-3D69D8477DE4` — what `mke2fs` images
/// and every Linux root carry, and what this system's own images already use.
pub const TYPE_LINUX_FS: [u8; 16] = [
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
];

/// Why a table could not be read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadError {
    /// The buffer given is smaller than the blocks the table occupies.
    Truncated,
    /// Block 1 does not begin `EFI PART`.
    NoSignature,
    /// The header's own checksum does not match its contents.
    HeaderChecksum,
    /// The entry array's checksum does not match the entries.
    ArrayChecksum,
    /// A field describes a table this code does not read: an entry size that is not 128, a header
    /// shorter than the fields it must carry, or an array that does not fit the buffer.
    Unsupported,
}

/// Why a table could not be built.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BuildError {
    /// More partitions than the array holds.
    TooManyPartitions,
    /// A partition starts before the first usable block, ends after the last, or ends before it
    /// starts.
    OutOfRange,
    /// Two partitions overlap. **Refused rather than written**: a table that overlaps is one where
    /// formatting the second destroys the first, and nothing downstream would notice.
    Overlap,
    /// The disk is too small to hold a table at all.
    DiskTooSmall,
    /// The output buffer is smaller than the bytes the table occupies.
    BufferTooSmall,
}

/// One partition to write, or one that was read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Partition {
    /// The type GUID: [`TYPE_EFI_SYSTEM`], [`TYPE_LINUX_FS`], or another the caller knows.
    pub type_guid: [u8; 16],
    /// This partition's own GUID. Callers that have no entropy may repeat one; firmware does not
    /// care, and the system selects partitions by label.
    pub unique_guid: [u8; 16],
    /// First logical block, inclusive.
    pub first_lba: u64,
    /// Last logical block, **inclusive** — the standard's convention, and the one every
    /// off-by-one in partitioning comes from.
    pub last_lba: u64,
    /// The label, UTF-16LE on disk and ASCII here: 36 code units, so 36 characters.
    pub name: [u8; 36],
    /// Bytes of `name` that are meaningful.
    pub name_len: usize,
}

impl Partition {
    /// A partition of `type_guid` spanning `first_lba..=last_lba`, labelled `name` (cut to 36).
    pub fn new(type_guid: [u8; 16], unique_guid: [u8; 16], first_lba: u64, last_lba: u64, name: &[u8]) -> Self {
        let mut p = Self {
            type_guid,
            unique_guid,
            first_lba,
            last_lba,
            name: [0; 36],
            name_len: 0,
        };
        let n = name.len().min(36);
        p.name[..n].copy_from_slice(&name[..n]);
        p.name_len = n;
        p
    }

    /// The label's meaningful bytes.
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len.min(36)]
    }

    /// Blocks this partition covers.
    pub fn blocks(&self) -> u64 {
        self.last_lba.saturating_sub(self.first_lba).saturating_add(1)
    }
}

/// A table read off a disk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Table {
    /// Total logical blocks the header says the disk has, derived from its backup location.
    pub disk_blocks: u64,
    /// First block a partition may use.
    pub first_usable: u64,
    /// Last block a partition may use.
    pub last_usable: u64,
    /// The partitions in use, in array order.
    pub partitions: [Partition; MAX_PARTITIONS],
    /// How many of `partitions` are in use.
    pub count: usize,
}

/// Partitions this crate reads or writes. The array holds 128; nothing this system builds needs
/// more than a handful, and a fixed buffer is what keeps the crate `alloc`-free.
pub const MAX_PARTITIONS: usize = 8;

impl Table {
    /// The partitions in use.
    pub fn partitions(&self) -> &[Partition] {
        &self.partitions[..self.count]
    }

    /// The first partition whose label is `name`.
    pub fn by_name(&self, name: &[u8]) -> Option<&Partition> {
        self.partitions().iter().find(|p| p.name() == name)
    }
}

/// Bytes a table occupies at the **front** of a disk: the protective MBR, the header and the
/// array.
pub const FRONT_BYTES: usize = (2 + ARRAY_BLOCKS as usize) * BLOCK;
/// Bytes a table occupies at the **back**: the array again, then the backup header.
pub const BACK_BYTES: usize = (ARRAY_BLOCKS as usize + 1) * BLOCK;

/// Build a table for a disk of `disk_blocks` blocks holding `partitions`.
///
/// Writes the front of the disk into `front` and the back into `back`; the caller writes `front`
/// at block 0 and `back` at `disk_blocks - ARRAY_BLOCKS - 1`. Two buffers rather than one disk
/// image, because the caller is writing to a device rather than building a file.
///
/// `disk_guid` identifies the disk; this crate does not invent GUIDs, since it has no entropy of
/// its own and a caller that has some should decide.
pub fn build(
    disk_blocks: u64,
    disk_guid: [u8; 16],
    partitions: &[Partition],
    front: &mut [u8],
    back: &mut [u8],
) -> Result<(), BuildError> {
    if partitions.len() > ENTRY_COUNT as usize {
        return Err(BuildError::TooManyPartitions);
    }
    if front.len() < FRONT_BYTES || back.len() < BACK_BYTES {
        return Err(BuildError::BufferTooSmall);
    }
    // The backup header takes the last block, the backup array the blocks before it.
    let last_usable = disk_blocks
        .checked_sub(ARRAY_BLOCKS + 2)
        .ok_or(BuildError::DiskTooSmall)?;
    if last_usable < FIRST_USABLE {
        return Err(BuildError::DiskTooSmall);
    }
    for (i, p) in partitions.iter().enumerate() {
        if p.first_lba < FIRST_USABLE || p.last_lba > last_usable || p.last_lba < p.first_lba {
            return Err(BuildError::OutOfRange);
        }
        // **Overlap is refused, not rounded.** Two partitions sharing a block means formatting one
        // corrupts the other, and no later step could tell.
        for q in &partitions[..i] {
            if p.first_lba <= q.last_lba && q.first_lba <= p.last_lba {
                return Err(BuildError::Overlap);
            }
        }
    }

    front[..FRONT_BYTES].fill(0);
    back[..BACK_BYTES].fill(0);

    // Block 0: the protective MBR. One entry of type 0xEE covering the disk (capped at the 32-bit
    // field's maximum, which is what every implementation does for a disk over 2 TiB).
    let mbr = &mut front[..BLOCK];
    mbr[446] = 0x00; // not bootable
    mbr[447..450].copy_from_slice(&[0x00, 0x02, 0x00]); // CHS start, legacy and ignored
    mbr[450] = 0xEE; // type: GPT protective
    mbr[451..454].copy_from_slice(&[0xFF, 0xFF, 0xFF]); // CHS end, likewise
    mbr[454..458].copy_from_slice(&1u32.to_le_bytes());
    let span = u32::try_from(disk_blocks - 1).unwrap_or(u32::MAX);
    mbr[458..462].copy_from_slice(&span.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;

    // The entry array, written once into each buffer.
    let array_len = ENTRY_COUNT as usize * ENTRY_LEN;
    let entries_crc = {
        let array = &mut front[2 * BLOCK..2 * BLOCK + array_len];
        for (i, p) in partitions.iter().enumerate() {
            write_entry(&mut array[i * ENTRY_LEN..(i + 1) * ENTRY_LEN], p);
        }
        let crc = crc32(array);
        back[..array_len].copy_from_slice(array);
        crc
    };

    // The two headers differ in three fields: which block each lives at, where the other is, and
    // where its own array starts.
    write_header(
        &mut front[BLOCK..2 * BLOCK],
        1,
        disk_blocks - 1,
        2,
        disk_guid,
        last_usable,
        entries_crc,
    );
    write_header(
        &mut back[array_len..array_len + BLOCK],
        disk_blocks - 1,
        1,
        disk_blocks - 1 - ARRAY_BLOCKS,
        disk_guid,
        last_usable,
        entries_crc,
    );
    Ok(())
}

/// Write one 128-byte entry.
fn write_entry(e: &mut [u8], p: &Partition) {
    e[0..16].copy_from_slice(&p.type_guid);
    e[16..32].copy_from_slice(&p.unique_guid);
    e[32..40].copy_from_slice(&p.first_lba.to_le_bytes());
    e[40..48].copy_from_slice(&p.last_lba.to_le_bytes());
    // Attributes: none. Nothing here marks a partition required or read-only.
    e[48..56].copy_from_slice(&0u64.to_le_bytes());
    // The label is UTF-16LE. ASCII in, so each byte becomes a code unit with a zero above it.
    for (i, &b) in p.name().iter().enumerate() {
        e[56 + i * 2] = b;
        e[56 + i * 2 + 1] = 0;
    }
}

/// Write one header block, then its own checksum over the defined bytes.
fn write_header(
    block: &mut [u8],
    my_lba: u64,
    other_lba: u64,
    array_lba: u64,
    disk_guid: [u8; 16],
    last_usable: u64,
    entries_crc: u32,
) {
    block[0..8].copy_from_slice(SIGNATURE);
    block[8..12].copy_from_slice(&REVISION.to_le_bytes());
    block[12..16].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
    block[16..20].copy_from_slice(&0u32.to_le_bytes()); // own CRC, zero while computing
    block[20..24].copy_from_slice(&0u32.to_le_bytes()); // reserved
    block[24..32].copy_from_slice(&my_lba.to_le_bytes());
    block[32..40].copy_from_slice(&other_lba.to_le_bytes());
    block[40..48].copy_from_slice(&FIRST_USABLE.to_le_bytes());
    block[48..56].copy_from_slice(&last_usable.to_le_bytes());
    block[56..72].copy_from_slice(&disk_guid);
    block[72..80].copy_from_slice(&array_lba.to_le_bytes());
    block[80..84].copy_from_slice(&ENTRY_COUNT.to_le_bytes());
    block[84..88].copy_from_slice(&(ENTRY_LEN as u32).to_le_bytes());
    block[88..92].copy_from_slice(&entries_crc.to_le_bytes());
    // **The header's checksum covers exactly `header_size` bytes**, with the field itself zero —
    // not the whole block. A sum over 512 bytes validates against itself and fails everywhere else.
    let crc = crc32(&block[..HEADER_LEN]);
    block[16..20].copy_from_slice(&crc.to_le_bytes());
}

/// Read a table from the front of a disk: `front` must hold block 0 onwards, far enough to cover
/// the header's array.
///
/// **Both checksums are verified**, which is what makes this a reader rather than a parser: an
/// installer that copied a filesystem out of a table it had not checked would copy whatever the
/// bytes happened to say.
pub fn read(front: &[u8]) -> Result<Table, ReadError> {
    if front.len() < 2 * BLOCK {
        return Err(ReadError::Truncated);
    }
    let header = &front[BLOCK..2 * BLOCK];
    if &header[0..8] != SIGNATURE {
        return Err(ReadError::NoSignature);
    }
    let header_size = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    if header_size < HEADER_LEN || header_size > BLOCK {
        return Err(ReadError::Unsupported);
    }
    // The stored sum is over the header with its own field zeroed.
    let stored = u32::from_le_bytes(header[16..20].try_into().unwrap());
    let mut scratch = [0u8; BLOCK];
    scratch[..header_size].copy_from_slice(&header[..header_size]);
    scratch[16..20].copy_from_slice(&0u32.to_le_bytes());
    if crc32(&scratch[..header_size]) != stored {
        return Err(ReadError::HeaderChecksum);
    }

    let array_lba = u64::from_le_bytes(header[72..80].try_into().unwrap());
    let count = u32::from_le_bytes(header[80..84].try_into().unwrap()) as usize;
    let entry_len = u32::from_le_bytes(header[84..88].try_into().unwrap()) as usize;
    let entries_crc = u32::from_le_bytes(header[88..92].try_into().unwrap());
    if entry_len != ENTRY_LEN || count == 0 || count > 1024 {
        return Err(ReadError::Unsupported);
    }
    let start = (array_lba as usize).checked_mul(BLOCK).ok_or(ReadError::Unsupported)?;
    let end = start.checked_add(count * entry_len).ok_or(ReadError::Unsupported)?;
    if front.len() < end {
        return Err(ReadError::Truncated);
    }
    if crc32(&front[start..end]) != entries_crc {
        return Err(ReadError::ArrayChecksum);
    }

    let mut table = Table {
        disk_blocks: u64::from_le_bytes(header[32..40].try_into().unwrap()) + 1,
        first_usable: u64::from_le_bytes(header[40..48].try_into().unwrap()),
        last_usable: u64::from_le_bytes(header[48..56].try_into().unwrap()),
        partitions: [Partition::new([0; 16], [0; 16], 0, 0, b""); MAX_PARTITIONS],
        count: 0,
    };
    for i in 0..count {
        let e = &front[start + i * entry_len..start + (i + 1) * entry_len];
        // An all-zero type GUID is an unused slot.
        if e[0..16].iter().all(|&b| b == 0) {
            continue;
        }
        if table.count == MAX_PARTITIONS {
            break;
        }
        let mut name = [0u8; 36];
        let mut name_len = 0;
        for k in 0..36 {
            let (lo, hi) = (e[56 + k * 2], e[56 + k * 2 + 1]);
            if lo == 0 && hi == 0 {
                break;
            }
            // Non-ASCII becomes `?`: this name is shown to a person and matched against labels
            // this system writes, both of which are ASCII.
            name[k] = if hi == 0 && lo.is_ascii_graphic() { lo } else { b'?' };
            name_len = k + 1;
        }
        table.partitions[table.count] = Partition {
            type_guid: e[0..16].try_into().unwrap(),
            unique_guid: e[16..32].try_into().unwrap(),
            first_lba: u64::from_le_bytes(e[32..40].try_into().unwrap()),
            last_lba: u64::from_le_bytes(e[40..48].try_into().unwrap()),
            name,
            name_len,
        };
        table.count += 1;
    }
    Ok(table)
}


#[cfg(test)]
mod tests {
    use super::*;

    const DISK: u64 = 262_144; // 128 MiB at 512-byte blocks, the images' size.
    const GUID: [u8; 16] = [0x11; 16];

    fn two_partitions() -> [Partition; 2] {
        [
            Partition::new(TYPE_EFI_SYSTEM, [0x22; 16], 2048, 100_000, b"NITROX_ESP"),
            Partition::new(TYPE_LINUX_FS, [0x33; 16], 100_001, DISK - 40, b"nitrox-root"),
        ]
    }

    fn build_disk(parts: &[Partition]) -> Vec<u8> {
        let mut disk = vec![0u8; DISK as usize * BLOCK];
        let mut front = vec![0u8; FRONT_BYTES];
        let mut back = vec![0u8; BACK_BYTES];
        build(DISK, GUID, parts, &mut front, &mut back).expect("build");
        disk[..FRONT_BYTES].copy_from_slice(&front);
        let back_at = (DISK - ARRAY_BLOCKS - 1) as usize * BLOCK;
        disk[back_at..back_at + BACK_BYTES].copy_from_slice(&back);
        disk
    }

    #[test]
    fn a_table_built_here_reads_back_as_what_was_asked_for() {
        let parts = two_partitions();
        let disk = build_disk(&parts);
        let t = read(&disk).expect("read");
        assert_eq!(t.count, 2);
        assert_eq!(t.first_usable, FIRST_USABLE);
        assert_eq!(t.disk_blocks, DISK);
        for (got, want) in t.partitions().iter().zip(parts.iter()) {
            assert_eq!(got.first_lba, want.first_lba);
            assert_eq!(got.last_lba, want.last_lba);
            assert_eq!(got.name(), want.name());
            assert_eq!(got.type_guid, want.type_guid);
        }
        assert_eq!(t.by_name(b"nitrox-root").map(|p| p.first_lba), Some(100_001));
        assert!(t.by_name(b"nitrox-live").is_none());
    }

    /// **The backup is a table too**, and firmware falls back to it. A backup that did not parse
    /// would be found only by a disk whose front had been damaged — which is the one moment it
    /// matters.
    #[test]
    fn the_backup_header_is_valid_and_points_the_other_way() {
        let disk = build_disk(&two_partitions());
        let back_at = (DISK - ARRAY_BLOCKS - 1) as usize * BLOCK;
        let backup = &disk[back_at + ARRAY_BLOCKS as usize * BLOCK..];
        assert_eq!(&backup[0..8], SIGNATURE);
        assert_eq!(u64::from_le_bytes(backup[24..32].try_into().unwrap()), DISK - 1, "its own block");
        assert_eq!(u64::from_le_bytes(backup[32..40].try_into().unwrap()), 1, "the primary's");
        assert_eq!(
            u64::from_le_bytes(backup[72..80].try_into().unwrap()),
            DISK - 1 - ARRAY_BLOCKS,
            "its own array"
        );
        // Its self-checksum verifies the same way the primary's does.
        let mut scratch = [0u8; BLOCK];
        scratch[..HEADER_LEN].copy_from_slice(&backup[..HEADER_LEN]);
        scratch[16..20].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            crc32(&scratch[..HEADER_LEN]),
            u32::from_le_bytes(backup[16..20].try_into().unwrap())
        );
    }

    #[test]
    fn the_protective_mbr_claims_the_whole_disk() {
        let disk = build_disk(&two_partitions());
        assert_eq!(disk[450], 0xEE, "type: GPT protective");
        assert_eq!(u32::from_le_bytes(disk[454..458].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(disk[458..462].try_into().unwrap()), (DISK - 1) as u32);
        assert_eq!((disk[510], disk[511]), (0x55, 0xAA));
    }

    /// **Corruption is refused, not parsed.** An installer that copied a filesystem out of a table
    /// it had not checked would copy whatever the bytes happened to say.
    #[test]
    fn a_damaged_table_is_refused_and_says_which_half() {
        let good = build_disk(&two_partitions());

        let mut no_sig = good.clone();
        no_sig[BLOCK] ^= 0xFF;
        assert_eq!(read(&no_sig), Err(ReadError::NoSignature));

        // A header field changed without its checksum: the disk GUID.
        let mut bad_header = good.clone();
        bad_header[BLOCK + 56] ^= 0xFF;
        assert_eq!(read(&bad_header), Err(ReadError::HeaderChecksum));

        // An entry changed without the array's checksum — the case that matters most, because the
        // header still verifies and the partition now points somewhere else.
        let mut bad_entry = good.clone();
        bad_entry[2 * BLOCK + 32] ^= 0xFF;
        assert_eq!(read(&bad_entry), Err(ReadError::ArrayChecksum));

        assert_eq!(read(&good[..BLOCK]), Err(ReadError::Truncated));
    }

    #[test]
    fn a_table_that_would_lose_data_is_not_built() {
        let mut front = vec![0u8; FRONT_BYTES];
        let mut back = vec![0u8; BACK_BYTES];

        let overlapping = [
            Partition::new(TYPE_EFI_SYSTEM, [0x22; 16], 2048, 100_000, b"a"),
            Partition::new(TYPE_LINUX_FS, [0x33; 16], 100_000, 200_000, b"b"),
        ];
        assert_eq!(
            build(DISK, GUID, &overlapping, &mut front, &mut back),
            Err(BuildError::Overlap),
            "one shared block means formatting one destroys the other"
        );

        let before_usable = [Partition::new(TYPE_LINUX_FS, [0x33; 16], 2, 100, b"a")];
        assert_eq!(build(DISK, GUID, &before_usable, &mut front, &mut back), Err(BuildError::OutOfRange));

        let past_end = [Partition::new(TYPE_LINUX_FS, [0x33; 16], 2048, DISK, b"a")];
        assert_eq!(build(DISK, GUID, &past_end, &mut front, &mut back), Err(BuildError::OutOfRange));

        let backwards = [Partition::new(TYPE_LINUX_FS, [0x33; 16], 5000, 4000, b"a")];
        assert_eq!(build(DISK, GUID, &backwards, &mut front, &mut back), Err(BuildError::OutOfRange));

        let parts = two_partitions();
        assert_eq!(build(64, GUID, &parts, &mut front, &mut back), Err(BuildError::DiskTooSmall));
        assert_eq!(
            build(DISK, GUID, &parts, &mut front[..10], &mut back),
            Err(BuildError::BufferTooSmall)
        );
    }

    // ---- `sgdisk` as the oracle ----
    //
    // **The kernel's parser is a weak second opinion**: it checks neither CRC and reads no backup
    // header, so agreeing with it would prove little. `sgdisk` is what wrote every partition table
    // this system has ever booted, and what a person would reach for to inspect a disk this
    // installer wrote. Both directions are checked, because the installer does both: it writes a
    // table firmware must accept, and reads one the image builder made with `sgdisk`.
    //
    // Skipped with a clear panic if `sgdisk` is missing, as `fs-server-ext4`'s fixtures do with
    // `mke2fs` — both are project dependencies.

    /// A scratch file path unique to this test process.
    fn scratch(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("nitrox-gpt-{}-{}-{}.img", std::process::id(), tag, id))
    }

    fn sgdisk(args: &[&str], path: &std::path::Path) -> std::process::Output {
        std::process::Command::new("sgdisk")
            .args(args)
            .arg(path)
            .output()
            .expect("sgdisk must be installed (gdisk) to run libgpt's tests")
    }

    #[test]
    fn sgdisk_verifies_a_table_this_crate_wrote() {
        let path = scratch("written");
        std::fs::write(&path, build_disk(&two_partitions())).unwrap();

        let verify = sgdisk(&["--verify"], &path);
        let out = String::from_utf8_lossy(&verify.stdout);
        assert!(verify.status.success(), "sgdisk --verify failed: {out}");
        assert!(out.contains("No problems found"), "sgdisk found problems: {out}");

        // And it reads back the same partitions — type codes included, since firmware finds the
        // ESP by its type GUID and nothing else.
        let print = sgdisk(&["--print"], &path);
        let out = String::from_utf8_lossy(&print.stdout);
        assert!(out.contains("EF00  NITROX_ESP"), "the ESP's type or label: {out}");
        assert!(out.contains("8300  nitrox-root"), "the root's type or label: {out}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn this_crate_reads_a_table_sgdisk_wrote() {
        let path = scratch("read");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(DISK * BLOCK as u64)
            .unwrap();
        let made = sgdisk(
            &[
                "--clear",
                "-n", "1:2048:+48M", "-t", "1:ef00", "-c", "1:NITROX_ESP",
                "-n", "2:0:0", "-t", "2:8300", "-c", "2:nitrox-root",
            ],
            &path,
        );
        assert!(made.status.success(), "sgdisk failed: {}", String::from_utf8_lossy(&made.stderr));

        let disk = std::fs::read(&path).unwrap();
        let t = read(&disk).expect("libgpt must read what sgdisk writes");
        assert_eq!(t.count, 2);
        assert_eq!(t.first_usable, FIRST_USABLE, "and agrees about where partitions may start");
        let esp = t.by_name(b"NITROX_ESP").expect("the ESP, by its label");
        assert_eq!(esp.first_lba, 2048);
        assert_eq!(esp.type_guid, TYPE_EFI_SYSTEM);
        let root = t.by_name(b"nitrox-root").expect("the root, by its label");
        assert_eq!(root.type_guid, TYPE_LINUX_FS);
        assert!(root.first_lba > esp.last_lba, "and they do not overlap");
        let _ = std::fs::remove_file(&path);
    }

    /// A label longer than the field is cut on a character, and read back as what was written.
    #[test]
    fn a_long_label_is_cut_to_the_field() {
        let long = [b'x'; 64];
        let p = Partition::new(TYPE_LINUX_FS, [0x33; 16], 2048, 4096, &long);
        assert_eq!(p.name().len(), 36);
        let disk = build_disk(&[p]);
        let t = read(&disk).unwrap();
        assert_eq!(t.partitions()[0].name(), &long[..36]);
    }
}
