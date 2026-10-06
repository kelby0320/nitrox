//! **A disk's partition table, parsed** (Phase 6 Part D.1): GPT, MBR, or none.
//!
//! Whoever can read the disk reads it: the boot's polled read for a SATA disk or a RAM disk
//! ([`crate::drivers::gpt::init`]), and a USB disk's binding, which reads before the disk is
//! published. Both hand [`read`] a function that reads 512-byte blocks, and get back what the table
//! says. **Nothing here touches a device**, so every rule below is a host test over bytes.
//!
//! - **GPT** first: a header at block 1 signed `EFI PART`, its entries where it says, and the
//!   disk's own GUID, which is how the boot disk is recognised ([`is_boot`]).
//! - **MBR** otherwise: block 0 signed `0x55AA`, its four primary entries. An extended entry is
//!   passed over and counted: its logical partitions are not read. **A filesystem's boot sector is
//!   not an MBR**, though it carries the same signature: a FAT, NTFS or exFAT volume written to a
//!   whole stick starts with one, and its boot code where the entries would be.
//! - **None**: the disk is the filesystem, or blank. The storage service probes it as it is.
//!
//! An entry that does not fit the disk is passed over, in either scheme: a window past the disk's
//! end would forward reads to blocks that do not exist.

use crate::libkern::lockrank::LockRank;
use crate::libkern::{KVec, SpinLock};

/// The block size every table here is read in.
pub const BLOCK: usize = 512;

/// The most blocks a GPT's entries are read from: 128 entries of 128 bytes.
pub const GPT_ENTRY_BLOCKS_MAX: u64 = 32;

/// GPT's header signature.
const GPT_SIG: &[u8; 8] = b"EFI PART";

/// MBR partition types that are extended partitions, whose logical partitions are not read.
const MBR_EXTENDED: [u8; 3] = [0x05, 0x0F, 0x85];

/// The MBR partition type of a GPT's protective entry.
const MBR_PROTECTIVE: u8 = 0xEE;

/// One partition the table describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    /// Its first block on the disk.
    pub first_lba: u64,
    /// How many blocks it spans.
    pub count: u64,
    /// **Its number, from 1**: a GPT partition's place among the entries in use, as the kernel has
    /// always numbered them; an MBR partition's slot, as every other system numbers them.
    pub number: u32,
    /// What the scheme says of it.
    pub kind: PartKind,
}

/// What a partition's entry carries beyond its extent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartKind {
    /// A GPT entry: its unique GUID, and its name as stored, UTF-16LE.
    Gpt { guid: [u8; 16], name: [u8; 72] },
    /// An MBR entry: its type byte.
    Mbr { kind: u8 },
}

/// Which scheme the disk uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    /// A GPT, and the disk's GUID from its header.
    Gpt { disk_guid: [u8; 16] },
    /// An MBR.
    Mbr,
    /// No table: the disk holds a filesystem as it is, or nothing.
    None,
}

/// What a disk's table says.
pub struct Table {
    /// Which scheme it is, with a GPT's disk GUID.
    pub scheme: Scheme,
    /// Its partitions, in table order.
    pub parts: KVec<Part>,
    /// MBR entries passed over for being extended partitions.
    pub extended: u32,
}

/// Why no table could be read.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Unread {
    /// A read the table needed failed.
    Io,
    /// A GPT header this parser does not take: its entry size, say.
    Unsupported(&'static str),
    /// Memory ran out for the entries.
    NoMemory,
}

/// **Read and parse a disk's table** through `read_blocks(lba, count, out)`, which fills `out`
/// (`count` × [`BLOCK`] bytes) and says whether it could. `blocks` is the disk's size in blocks.
pub fn read(blocks: u64, read_blocks: &mut dyn FnMut(u64, u64, &mut [u8]) -> bool) -> Result<Table, Unread> {
    let mut first = [0u8; 2 * BLOCK];
    if !read_blocks(0, 2, &mut first) {
        return Err(Unread::Io);
    }
    let (mbr, header) = first.split_at(BLOCK);
    if &header[..8] == GPT_SIG {
        return gpt(header, blocks, read_blocks);
    }
    mbr_table(mbr, blocks)
}

/// The GPT whose header is `hdr`: its entries, read where it says.
fn gpt(hdr: &[u8], blocks: u64, read_blocks: &mut dyn FnMut(u64, u64, &mut [u8]) -> bool) -> Result<Table, Unread> {
    let array_lba = u64_at(hdr, 72);
    let entries = u32_at(hdr, 80);
    let entry_size = u32_at(hdr, 84) as usize;
    if entry_size < 128 || entry_size > BLOCK || BLOCK % entry_size != 0 {
        return Err(Unread::Unsupported("its entries are not a size that divides a block"));
    }
    let mut disk_guid = [0u8; 16];
    disk_guid.copy_from_slice(&hdr[56..72]);
    let per_block = BLOCK / entry_size;
    let array_blocks = (entries as u64).div_ceil(per_block as u64).min(GPT_ENTRY_BLOCKS_MAX);
    // On the heap: 16 KiB at most, which a kernel stack has no room for.
    let mut array: KVec<u8> = KVec::new();
    let len = array_blocks as usize * BLOCK;
    array.try_reserve(len).map_err(|_| Unread::NoMemory)?;
    while array.len() < len {
        array.try_push(0).map_err(|_| Unread::NoMemory)?;
    }
    if array_blocks > 0 && !fits(array_lba, array_blocks, blocks) {
        return Err(Unread::Unsupported("its entries are off the disk"));
    }
    if array_blocks > 0 && !read_blocks(array_lba, array_blocks, &mut array) {
        return Err(Unread::Io);
    }
    let mut parts = KVec::new();
    let in_array = (array_blocks as usize * per_block).min(entries as usize);
    for e in array.chunks(entry_size).take(in_array) {
        // An all-zero type GUID is an unused entry.
        if e[..16].iter().all(|&b| b == 0) {
            continue;
        }
        let (first, last) = (u64_at(e, 32), u64_at(e, 40));
        // **Counted only where it cannot overflow**: a stick's table is anyone's bytes, and an
        // entry from 0 to the last LBA there could be would panic the add (PR #363 review).
        let Some(count) = last.checked_sub(first).and_then(|n| n.checked_add(1)) else {
            continue;
        };
        if !fits(first, count, blocks) {
            continue;
        }
        let mut guid = [0u8; 16];
        guid.copy_from_slice(&e[16..32]);
        let mut name = [0u8; 72];
        name.copy_from_slice(&e[56..128]);
        let number = parts.len() as u32 + 1;
        let part = Part { first_lba: first, count, number, kind: PartKind::Gpt { guid, name } };
        parts.try_push(part).map_err(|_| Unread::NoMemory)?;
    }
    Ok(Table { scheme: Scheme::Gpt { disk_guid }, parts, extended: 0 })
}

/// The MBR in block 0, if it is one, else no table.
fn mbr_table(b: &[u8], blocks: u64) -> Result<Table, Unread> {
    let none = || Table { scheme: Scheme::None, parts: KVec::new(), extended: 0 };
    if !is_mbr(b) {
        return Ok(none());
    }
    let mut parts = KVec::new();
    let mut extended = 0;
    for slot in 0..4 {
        let e = &b[0x1BE + 16 * slot..0x1BE + 16 * (slot + 1)];
        let kind = e[4];
        if kind == 0 || kind == MBR_PROTECTIVE {
            continue;
        }
        if MBR_EXTENDED.contains(&kind) {
            extended += 1;
            continue;
        }
        let (first, count) = (u32_at(e, 8) as u64, u32_at(e, 12) as u64);
        if first == 0 || count == 0 || !fits(first, count, blocks) {
            continue;
        }
        let part = Part { first_lba: first, count, number: slot as u32 + 1, kind: PartKind::Mbr { kind } };
        parts.try_push(part).map_err(|_| Unread::NoMemory)?;
    }
    Ok(Table { scheme: Scheme::Mbr, parts, extended })
}

/// **Whether block 0 is an MBR**: signed `0x55AA`, not a filesystem's boot sector, and with every
/// entry's status byte one an MBR has, `0x00` or `0x80` — the check Linux makes, since boot code
/// where the entries would be rarely passes it.
pub fn is_mbr(b: &[u8]) -> bool {
    if b.len() < BLOCK || b[510] != 0x55 || b[511] != 0xAA || is_volume_boot_sector(b) {
        return false;
    }
    (0..4).all(|slot| matches!(b[0x1BE + 16 * slot], 0x00 | 0x80))
}

/// **Whether block 0 is a filesystem's own boot sector**: FAT's, by its jump, its bytes per
/// sector, and the extended boot signature with its type string where FAT12/16 or FAT32 put them —
/// the storage service's own FAT check; or NTFS's or exFAT's, by the name after the jump.
fn is_volume_boot_sector(b: &[u8]) -> bool {
    let jump = b[0] == 0xEB || b[0] == 0xE9;
    if !jump {
        return false;
    }
    if &b[3..11] == b"NTFS    " || &b[3..11] == b"EXFAT   " {
        return true;
    }
    let bytes_per_sector = u16::from_le_bytes([b[11], b[12]]);
    if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096) {
        return false;
    }
    let fat16 = b[0x26] == 0x29 && &b[0x36..0x39] == b"FAT";
    let fat32 = b[0x42] == 0x29 && &b[0x52..0x57] == b"FAT32";
    fat16 || fat32
}

/// Whether `[first, first + count)` lies on a disk of `blocks` blocks. Every caller knows the disk's
/// size, so a disk of none holds nothing.
fn fits(first: u64, count: u64, blocks: u64) -> bool {
    first.checked_add(count).is_some_and(|end| end <= blocks)
}

/// **The GPT GUID of the disk the machine started from**, as Limine's module records give it:
/// `None` until [`set_boot_disk`] runs, and for a volume Limine names no GUID for.
static BOOT_DISK: SpinLock<Option<[u8; 16]>> = SpinLock::new(LockRank::Leaf, None);

/// Record the boot disk's GPT GUID, from the first module's Limine record. Before `drivers::probe`.
pub fn set_boot_disk(guid: [u8; 16]) {
    *BOOT_DISK.lock() = (guid != [0; 16]).then_some(guid);
}

/// The boot disk's GPT GUID, if Limine named one.
pub fn boot_disk() -> Option<[u8; 16]> {
    *BOOT_DISK.lock()
}

/// **Whether a disk whose GPT names `disk_guid` is the one the machine started from**, whose GUID
/// Limine gave as `boot`. An all-zero GUID is no one's: Limine leaves the field zero for a volume
/// that is not GPT.
pub fn is_boot(disk_guid: &[u8; 16], boot: Option<[u8; 16]>) -> bool {
    boot.is_some_and(|b| b != [0; 16] && b == *disk_guid)
}

/// A GUID as stored, shown in its canonical form: the first three fields are little-endian on disk.
pub struct Guid<'a>(pub &'a [u8; 16]);

impl core::fmt::Display for Guid<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let g = self.0;
        write!(f, "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-", g[3], g[2], g[1], g[0], g[5], g[4], g[7], g[6])?;
        write!(f, "{:02X}{:02X}-", g[8], g[9])?;
        g[10..].iter().try_for_each(|b| write!(f, "{b:02X}"))
    }
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn u64_at(b: &[u8], off: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;

    /// A disk image in memory, read as a device would be: its first [`STORED`] blocks held, and its
    /// size what it says, since a table is read from its first blocks alone.
    struct Image(KVec<u8>, u64);

    /// Blocks an image holds: a GPT's header and entries, and room.
    const STORED: usize = 64;

    impl Image {
        fn blank(blocks: usize) -> Image {
            let mut v = KVec::new();
            let held = blocks.min(STORED);
            v.try_reserve(held * BLOCK).unwrap();
            while v.len() < held * BLOCK {
                v.try_extend_from_slice(&[0u8; BLOCK][..]).unwrap();
            }
            Image(v, blocks as u64)
        }

        fn at(&mut self, off: usize) -> &mut [u8] {
            &mut self.0[off..]
        }

        fn read(&self) -> Result<Table, Unread> {
            read(self.1, &mut |lba, count, out: &mut [u8]| {
                let (start, len) = (lba as usize * BLOCK, count as usize * BLOCK);
                match self.0.get(start..start + len) {
                    Some(src) => {
                        out[..len].copy_from_slice(src);
                        true
                    }
                    None => false,
                }
            })
        }
    }

    /// An MBR entry at `slot`: status, type, first block, block count.
    fn mbr_entry(img: &mut Image, slot: usize, status: u8, kind: u8, first: u32, count: u32) {
        let e = img.at(0x1BE + 16 * slot);
        e[0] = status;
        e[4] = kind;
        e[8..12].copy_from_slice(&first.to_le_bytes());
        e[12..16].copy_from_slice(&count.to_le_bytes());
    }

    fn sign(img: &mut Image) {
        img.at(510)[..2].copy_from_slice(&[0x55, 0xAA]);
    }

    const DISK_GUID: [u8; 16] = [0x7f, 0x38, 0xe3, 0x54, 0x6e, 0x15, 0xd0, 0x4c, 0xa6, 0x89, 0xe2, 0x07, 0x58, 0xfe, 0xea, 0xb8];

    /// A GPT: a header at block 1 naming `entries` entries of 128 bytes at block 2, and the entries
    /// given, each as (type byte, first, last, name).
    fn gpt_image(blocks: usize, entries: u32, given: &[(u8, u64, u64, &str)]) -> Image {
        let mut img = Image::blank(blocks);
        sign(&mut img);
        mbr_entry(&mut img, 0, 0, MBR_PROTECTIVE, 1, blocks as u32 - 1);
        let h = img.at(BLOCK);
        h[..8].copy_from_slice(GPT_SIG);
        h[56..72].copy_from_slice(&DISK_GUID);
        h[72..80].copy_from_slice(&2u64.to_le_bytes());
        h[80..84].copy_from_slice(&entries.to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        for (i, &(kind, first, last, name)) in given.iter().enumerate() {
            let e = img.at(2 * BLOCK + 128 * i);
            e[0] = kind;
            e[16] = 0xA0 + i as u8;
            e[32..40].copy_from_slice(&first.to_le_bytes());
            e[40..48].copy_from_slice(&last.to_le_bytes());
            for (j, c) in name.bytes().enumerate() {
                e[56 + 2 * j] = c;
            }
        }
        img
    }

    /// **GPT as the kernel has always read it**, and now its disk's GUID: entries in use numbered
    /// by their place among those in use, an unused entry between them skipped.
    #[test]
    fn a_gpt_gives_its_entries_in_use_and_its_disk_guid() {
        init_global_heap();
        let img = gpt_image(4096, 128, &[(0xEF, 2048, 2559, "NITROX_ESP"), (0, 0, 0, ""), (0x83, 2560, 4000, "nitrox-root")]);
        let t = img.read().unwrap();
        assert_eq!(t.scheme, Scheme::Gpt { disk_guid: DISK_GUID });
        assert_eq!(t.parts.len(), 2);
        assert_eq!((t.parts[0].first_lba, t.parts[0].count, t.parts[0].number), (2048, 512, 1));
        assert_eq!((t.parts[1].first_lba, t.parts[1].count, t.parts[1].number), (2560, 1441, 2));
        let PartKind::Gpt { guid, name } = t.parts[1].kind else { panic!("a GPT entry") };
        assert_eq!(guid[0], 0xA2, "the entry's own GUID");
        assert_eq!(&name[..6], b"n\0i\0t\0", "its name as stored");
    }

    /// **A GPT entry past the disk's end is passed over**, and one whose last block precedes its
    /// first: a window there would forward reads to blocks that do not exist.
    #[test]
    fn a_gpt_entry_off_the_disk_is_passed_over() {
        init_global_heap();
        let img = gpt_image(4096, 8, &[
            (0x83, 2048, 4095, "fits"),
            (0x83, 2048, 4096, "past"),
            (0x83, 3000, 2999, "back"),
            // **Every LBA there could be** (PR #363 review): its count overflows, and must not panic.
            (0x83, 0, u64::MAX, "all"),
            (0x83, 1, u64::MAX, "rest"),
        ]);
        let t = img.read().unwrap();
        assert_eq!(t.parts.len(), 1);
        assert_eq!(t.parts[0].count, 2048, "the last block is the disk's last");
    }

    /// **A GPT whose entries are off the disk is not read** (PR #363 review): the header says where
    /// they are, and a stick's header is anyone's bytes. The disk's last block is on it.
    #[test]
    fn a_gpt_whose_entries_are_off_the_disk_is_unsupported() {
        init_global_heap();
        let mut img = gpt_image(4096, 4, &[]);
        for (lba, on) in [(4096u64, false), (u64::MAX, false), (4095, true)] {
            img.at(BLOCK + 72)[..8].copy_from_slice(&lba.to_le_bytes());
            match img.read() {
                Err(Unread::Unsupported(_)) => assert!(!on, "block {lba} is on the disk"),
                // The image stores its first blocks only, so a read of the last fails.
                Err(Unread::Io) => assert!(on, "block {lba} is off the disk, yet was read"),
                _ => panic!("block {lba}: neither refused nor read"),
            }
        }
    }

    /// **A disk of no blocks holds no partition**: every caller knows its disk's size, and an entry
    /// cannot lie on none.
    #[test]
    fn a_disk_of_no_blocks_holds_no_partition() {
        init_global_heap();
        let mut img = Image::blank(8192);
        sign(&mut img);
        mbr_entry(&mut img, 0, 0x80, 0x0C, 2048, 2048);
        img.1 = 0;
        assert_eq!(img.read().unwrap().parts.len(), 0);
    }

    /// An entry size that does not divide a block is not a table this parser takes.
    #[test]
    fn a_gpt_with_odd_entries_is_unsupported() {
        init_global_heap();
        let mut img = gpt_image(64, 4, &[]);
        img.at(BLOCK + 84)[..4].copy_from_slice(&96u32.to_le_bytes());
        assert!(matches!(img.read(), Err(Unread::Unsupported(_))));
    }

    /// **An MBR's primary entries**: a protective one with no GPT behind it skipped, an extended one
    /// counted and passed over, and each partition numbered by its slot.
    #[test]
    fn an_mbr_gives_its_primary_entries_by_slot() {
        init_global_heap();
        let mut img = Image::blank(8192);
        sign(&mut img);
        mbr_entry(&mut img, 0, 0x80, 0x0C, 2048, 2048);
        mbr_entry(&mut img, 1, 0x00, MBR_PROTECTIVE, 1, 2047);
        mbr_entry(&mut img, 2, 0x00, 0x05, 4096, 1024);
        mbr_entry(&mut img, 3, 0x00, 0x83, 6144, 2048);
        let t = img.read().unwrap();
        assert_eq!(t.scheme, Scheme::Mbr);
        assert_eq!(t.extended, 1);
        assert_eq!(t.parts.len(), 2);
        assert_eq!((t.parts[0].number, t.parts[0].kind), (1, PartKind::Mbr { kind: 0x0C }));
        assert_eq!((t.parts[1].number, t.parts[1].first_lba, t.parts[1].count), (4, 6144, 2048));
    }

    /// An MBR entry starting at block 0, empty, or running past the disk is passed over. The last
    /// block of the disk is the first that does not fit.
    #[test]
    fn an_mbr_entry_off_the_disk_is_passed_over() {
        init_global_heap();
        let mut img = Image::blank(4096);
        sign(&mut img);
        mbr_entry(&mut img, 0, 0, 0x83, 0, 100);
        mbr_entry(&mut img, 1, 0, 0x83, 100, 0);
        mbr_entry(&mut img, 2, 0, 0x83, 2048, 2049);
        mbr_entry(&mut img, 3, 0, 0x83, 2048, 2048);
        let t = img.read().unwrap();
        assert_eq!(t.parts.len(), 1);
        assert_eq!(t.parts[0].number, 4, "ending on the disk's last block fits");
    }

    /// **No signature, no table**: a whole-disk ext4's block 0 is zeros, and a blank stick's too.
    #[test]
    fn a_block_without_the_signature_is_no_table() {
        init_global_heap();
        let mut img = Image::blank(64);
        mbr_entry(&mut img, 0, 0, 0x83, 1, 10);
        let t = img.read().unwrap();
        assert_eq!(t.scheme, Scheme::None);
        assert!(t.parts.is_empty());
    }

    /// **A status byte an MBR never has means boot code, not entries**: no table.
    #[test]
    fn a_status_byte_an_mbr_never_has_is_no_table() {
        init_global_heap();
        let mut img = Image::blank(64);
        sign(&mut img);
        mbr_entry(&mut img, 0, 0x80, 0x83, 1, 10);
        mbr_entry(&mut img, 1, 0x4F, 0x83, 11, 10);
        assert_eq!(img.read().unwrap().scheme, Scheme::None);
        mbr_entry(&mut img, 1, 0x00, 0x83, 11, 10);
        assert_eq!(img.read().unwrap().scheme, Scheme::Mbr, "every status 0x00 or 0x80");
    }

    /// **A FAT volume written to a whole stick is not an MBR**, though it is signed as one and its
    /// boot code may pass the status check: its jump, sector size and extended boot signature say
    /// what it is, for FAT32 and for FAT12/16 alike.
    #[test]
    fn a_fat_boot_sector_is_no_table() {
        init_global_heap();
        for (sig_at, ty_at, ty) in [(0x42usize, 0x52usize, &b"FAT32   "[..]), (0x26, 0x36, &b"FAT16   "[..])] {
            let mut img = Image::blank(8192);
            sign(&mut img);
            let b = img.at(0);
            b[..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
            b[11..13].copy_from_slice(&512u16.to_le_bytes());
            b[sig_at] = 0x29;
            b[ty_at..ty_at + ty.len()].copy_from_slice(ty);
            // Boot code that happens to read as a plausible entry.
            mbr_entry(&mut img, 0, 0x00, 0x0C, 2048, 2048);
            assert_eq!(img.read().unwrap().scheme, Scheme::None, "{}", core::str::from_utf8(ty).unwrap());
        }
        // The jump alone does not make a boot sector: an MBR's own code may begin with one.
        let mut img = Image::blank(8192);
        sign(&mut img);
        img.at(0)[..3].copy_from_slice(&[0xEB, 0x63, 0x90]);
        mbr_entry(&mut img, 0, 0x80, 0x83, 2048, 2048);
        assert_eq!(img.read().unwrap().scheme, Scheme::Mbr);
    }

    /// NTFS and exFAT, named after the jump, are not MBRs either.
    #[test]
    fn an_ntfs_or_exfat_boot_sector_is_no_table() {
        init_global_heap();
        for name in [b"NTFS    ", b"EXFAT   "] {
            let mut img = Image::blank(64);
            sign(&mut img);
            img.at(0)[..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
            img.at(3)[..8].copy_from_slice(name);
            assert!(!is_mbr(&img.0[..BLOCK]));
        }
    }

    /// A read that fails is said, not taken as no table: a disk that cannot be read is not blank.
    #[test]
    fn a_failed_read_is_not_no_table() {
        init_global_heap();
        assert!(matches!(read(64, &mut |_, _, _| false), Err(Unread::Io)));
    }

    /// **A GUID shows as `sgdisk` prints it**: the bytes the spike read off the live stick's record
    /// are the GUID `sgdisk -p` named for that image.
    #[test]
    fn a_guid_shows_in_its_canonical_form() {
        assert_eq!(format!("{}", Guid(&DISK_GUID)), "54E3387F-156E-4CD0-A689-E20758FEEAB8");
    }

    /// **The boot disk is the one whose GUID Limine named**, and an all-zero GUID — Limine's for a
    /// volume that is not GPT — is no one's.
    #[test]
    fn the_boot_disk_is_the_one_limine_named() {
        let other = [1u8; 16];
        assert!(is_boot(&DISK_GUID, Some(DISK_GUID)));
        assert!(!is_boot(&other, Some(DISK_GUID)), "another disk");
        assert!(!is_boot(&DISK_GUID, None), "no GUID recorded");
        assert!(!is_boot(&[0; 16], Some([0; 16])), "zero is no one's");
    }
}
