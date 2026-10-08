//! The FAT formatter's host tests: its choices at every neighbour of a boundary, and what it writes
//! judged by `fsck.fat -n`, mtools and the library that serves it.

use super::*;
use crate::test_support::{FileImage, fsck, mtools, mtype, pattern, scratch};
use crate::{BlockReader, BlockRun, Fat};
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::unix::fs::FileExt;

/// 2026-10-07, midnight UTC: an even second, which a FAT time holds exactly.
const NOW: i64 = 1_791_331_200;

fn params(sectors: u64) -> Params {
    Params { sectors, label: label(b"nitrox").unwrap(), volume_id: 0x1234_ABCD, hidden: 2048, now: NOW }
}

/// **A sparse image of `sectors`**, in a file removed when dropped.
fn image(sectors: u64) -> FileImage {
    let p = scratch("mkfs");
    std::fs::File::create(&p).unwrap().set_len(sectors * 512).unwrap();
    FileImage::open(&p)
}

/// **Sectors in memory, zero until written**: a device a test can stop writing to.
struct Sparse {
    sectors: RefCell<HashMap<u64, [u8; 512]>>,
    /// Writes taken before every later one fails; `usize::MAX` for none failing.
    writes_left: RefCell<usize>,
}

impl Sparse {
    fn failing_after(n: usize) -> Sparse {
        Sparse { sectors: RefCell::default(), writes_left: RefCell::new(n) }
    }

    fn sector(&self, s: u64) -> [u8; 512] {
        self.sectors.borrow().get(&s).copied().unwrap_or([0; 512])
    }
}

impl BlockReader for Sparse {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let (s, within) = (at / 512, (at % 512) as usize);
            let n = (512 - within).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&self.sector(s)[within..within + n]);
            done += n;
        }
        Ok(())
    }
}

impl BlockWriter for Sparse {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
        let mut left = self.writes_left.borrow_mut();
        if *left == 0 {
            return Err(FsError::Io);
        }
        *left -= 1;
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let (s, within) = (at / 512, (at % 512) as usize);
            let n = (512 - within).min(buf.len() - done);
            let mut d = self.sector(s);
            d[within..within + n].copy_from_slice(&buf[done..done + n]);
            self.sectors.borrow_mut().insert(s, d);
            done += n;
        }
        Ok(())
    }
}

/// **The type and the cluster at each neighbour of every boundary**: the floor, both edges of the
/// FAT16/FAT32 band, FAT32's steps at 8, 16 and 32 GiB, and FAT32's last sector count.
#[test]
fn the_type_and_cluster_change_at_each_neighbour() {
    let at = |sectors: u64| {
        let g = plan(&params(sectors)).unwrap();
        (g.kind, g.cluster_bytes())
    };
    assert_eq!(plan(&params(MIN_SECTORS - 1)), Err(MkfsError::TooSmall { have: MIN_SECTORS - 1, need: MIN_SECTORS }));
    assert_eq!(at(MIN_SECTORS), (Kind::Fat16, 4096));
    assert_eq!(at(FAT16_BAND - 1), (Kind::Fat16, 4096));
    assert_eq!(at(FAT16_BAND), (Kind::Fat16, 8192));
    assert_eq!(at(FAT32_FROM - 1), (Kind::Fat16, 8192));
    assert_eq!(at(FAT32_FROM), (Kind::Fat32, 4096));
    assert_eq!(at(8 * GIB), (Kind::Fat32, 4096));
    assert_eq!(at(8 * GIB + 1), (Kind::Fat32, 8192));
    assert_eq!(at(16 * GIB), (Kind::Fat32, 8192));
    assert_eq!(at(16 * GIB + 1), (Kind::Fat32, 16384));
    assert_eq!(at(32 * GIB), (Kind::Fat32, 16384));
    assert_eq!(at(32 * GIB + 1), (Kind::Fat32, 32768));
    assert_eq!(at(u32::MAX as u64), (Kind::Fat32, 32768));
    assert_eq!(plan(&params(u32::MAX as u64 + 1)), Err(MkfsError::TooLarge));
}

/// **The band's edges are the specification's, not this module's choice**: at 4 KiB clusters
/// FAT16's last valid count is one sector below the band and FAT32's first is at its top — so each
/// constant is where a type stops or starts being valid, and nowhere else.
#[test]
fn the_band_is_where_neither_type_is_valid_at_four_kib() {
    let fat16 = |s: u64| layout(Kind::Fat16, s as u32, 8).map(|g| g.clusters);
    let fat32 = |s: u64| layout(Kind::Fat32, s as u32, 8).map(|g| g.clusters);
    assert_eq!(fat16(FAT16_BAND - 1), Some(65_524));
    assert_eq!(fat16(FAT16_BAND), None, "65,525 clusters is no FAT16");
    assert_eq!(fat32(FAT32_FROM - 1), None, "65,524 clusters is no FAT32");
    assert_eq!(fat32(FAT32_FROM), Some(65_525));
    // And the floor: 4 KiB clusters still make a FAT16 at it.
    assert_eq!(layout(Kind::Fat16, MIN_SECTORS as u32, 8).map(|g| g.clusters), Some(4087));
}

/// **Every size from the floor makes a FAT the server takes**, its boot sector parsing back to the
/// geometry planned: every sector across the band, a step through the small sizes, and sizes up to
/// FAT32's last sector count from a fixed sequence.
#[test]
fn every_size_from_the_floor_makes_a_fat_the_server_takes() {
    let mut sizes: Vec<u64> = (FAT16_BAND - 2000..FAT32_FROM + 2000).collect();
    sizes.extend((MIN_SECTORS..2 * FAT32_FROM).step_by(97));
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..20_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        sizes.push(MIN_SECTORS + x % (u32::MAX as u64 - MIN_SECTORS + 1));
    }
    for s in sizes {
        let p = params(s);
        let g = plan(&p).unwrap_or_else(|e| panic!("{s} sectors refused: {e:?}"));
        assert_eq!(bpb::parse(&boot_sector(&g, &p)), Ok(g), "{s} sectors");
        assert_eq!(bpb::servable(&g), Ok(()), "{s} sectors");
        assert_eq!(g.data_start % g.sectors_per_cluster, 0, "{s} sectors: data off a cluster");
        assert!(g.data_start as u64 + g.clusters as u64 * g.sectors_per_cluster as u64 <= s);
        let range = if g.kind == Kind::Fat32 { FAT32_CLUSTERS } else { FAT16_CLUSTERS };
        assert!(range.contains(&g.clusters), "{s} sectors: {} clusters for {:?}", g.clusters, g.kind);
    }
}

/// Write `data` to the new file `/name` through its map, as the kernel writes a page.
fn make(fat: &Fat<FileImage>, name: &str, data: &[u8]) {
    fat.create_file(b"/", name.as_bytes(), NOW).unwrap();
    let path = format!("/{name}");
    fat.grow_file(path.as_bytes(), data.len(), NOW).unwrap();
    let mut runs = [BlockRun::default(); 64];
    let m = fat.map_file(path.as_bytes(), &mut runs).unwrap();
    for r in &runs[..m.runs] {
        let from = r.file_block as usize * 512;
        let to = (from + r.length as usize * 512).min(data.len());
        if from < to {
            fat.device().write_at(r.device_lba * 512, &data[from..to]).unwrap();
        }
    }
}

/// **What it makes at each neighbour, `fsck.fat -n` finds clean, mtools reads, and the library
/// serves**: the label in the boot sector and the root directory, then a file written through the
/// library and the volume settled as an unmount leaves it, clean again and read back by `mtype`.
/// Sparse images, the largest 32 GiB.
#[test]
fn what_it_makes_fsck_finds_clean_mtools_reads_and_the_library_serves() {
    let sizes = [
        MIN_SECTORS,
        FAT16_BAND - 1,
        FAT16_BAND,
        FAT32_FROM - 1,
        FAT32_FROM,
        8 * GIB,
        8 * GIB + 1,
        16 * GIB,
        16 * GIB + 1,
        32 * GIB,
        32 * GIB + 1,
    ];
    for s in sizes {
        let img = image(s);
        let g = format(&img, &params(s), &mut |_, _| {}).unwrap();
        let (code, said) = fsck(&img.path);
        assert_eq!(code, 0, "{s} sectors, as made: fsck.fat -n said\n{said}");
        let named = String::from_utf8(mtools("mlabel", &img.path, &["-s", "::"]).stdout).unwrap();
        assert!(named.contains("NITROX"), "{s} sectors: mlabel said {named:?}");

        let fat = Fat::new(&img);
        assert_eq!(fat.check(), Ok(()), "{s} sectors");
        assert_eq!(fat.geometry(), Ok(&g));
        assert_eq!(fat.label(), b"NITROX");
        assert_eq!(fat.was_left_clean(), Ok(true));
        fat.mark_mounted().unwrap();
        let data = pattern(3 * g.cluster_bytes() as usize + 77, s as u8);
        make(&fat, "written here.bin", &data);
        fat.mark_clean().unwrap();
        let (code, said) = fsck(&img.path);
        assert_eq!(code, 0, "{s} sectors, written: fsck.fat -n said\n{said}");
        assert_eq!(mtype(&img.path, "/written here.bin"), data, "{s} sectors");
    }
}

/// **No label is no label**: [`NO_NAME`] in the boot sector, no entry in the root, and mtools and
/// the library both say there is none.
#[test]
fn an_empty_label_writes_no_name_and_no_root_entry() {
    let img = image(MIN_SECTORS);
    format(&img, &Params { label: label(b"").unwrap(), ..params(MIN_SECTORS) }, &mut |_, _| {}).unwrap();
    assert_eq!(fsck(&img.path).0, 0);
    let named = String::from_utf8(mtools("mlabel", &img.path, &["-s", "::"]).stdout).unwrap();
    assert!(named.contains("has no label"), "mlabel said {named:?}");
    assert_eq!(Fat::new(&img).label(), b"");
}

/// **Nothing an older filesystem left in the first mebibyte, or in the FATs past it, survives** —
/// an ext4's superblock is at 1024 — and **data clusters past them are left as they were**. The
/// volume is filled with a pattern first; what the format leaves must equal a format of a blank
/// one up to where the metadata ends, and the pattern after it. A FAT16 at the floor, whose
/// metadata ends inside the first mebibyte, and a FAT32 whose FATs run past it.
#[test]
fn a_format_leaves_nothing_old_in_the_first_mebibyte_or_the_fats() {
    for s in [MIN_SECTORS, 8 * GIB + 1] {
        let (blank, old) = (image(s), image(s));
        let span = 20u64 << 20;
        old.file.write_all_at(&pattern(span as usize, 0x5A), 0).unwrap();
        let g = format(&blank, &params(s), &mut |_, _| {}).unwrap();
        format(&old, &params(s), &mut |_, _| {}).unwrap();
        let meta = (g.data_start as u64 + if g.kind == Kind::Fat32 { g.sectors_per_cluster as u64 } else { 0 }) * 512;
        let end = meta.max(1 << 20);
        assert!(end < span, "{s} sectors: the pattern must reach past the zeroed region");
        let read = |img: &FileImage, len: u64| {
            let mut b = vec![0u8; len as usize];
            img.file.read_exact_at(&mut b, 0).unwrap();
            b
        };
        assert!(read(&old, end) == read(&blank, end), "{s} sectors: something old survived in the metadata");
        let mut after = vec![0u8; (span - end) as usize];
        old.file.read_exact_at(&mut after, end).unwrap();
        assert!(after == pattern(span as usize, 0x5A)[end as usize..], "{s} sectors: data past the metadata changed");
    }
}

/// **A format cut short leaves no FAT**: stopped at every write in turn, the boot sector is still
/// zero, so nothing reads half-written tables as a filesystem. A FAT16 and a FAT32.
#[test]
fn a_format_cut_short_at_any_write_leaves_no_boot_sector() {
    for s in [MIN_SECTORS, FAT32_FROM] {
        let whole = Sparse::failing_after(usize::MAX);
        format(&whole, &params(s), &mut |_, _| {}).unwrap();
        let writes = usize::MAX - *whole.writes_left.borrow();
        assert!(writes > 4, "{s} sectors: {writes} writes");
        for n in 0..writes {
            let cut = Sparse::failing_after(n);
            assert_eq!(format(&cut, &params(s), &mut |_, _| {}), Err(FsError::Io));
            assert_eq!(cut.sector(0), [0; 512], "{s} sectors, cut after {n} of {writes} writes");
        }
    }
}

/// **A label as FAT keeps it**, and the bytes refused.
#[test]
fn a_label_is_uppercased_padded_and_refused_past_eleven_or_for_a_forbidden_byte() {
    assert_eq!(label(b"nitrox"), Ok(*b"NITROX     "));
    assert_eq!(label(b"my stick_1"), Ok(*b"MY STICK_1 "));
    assert_eq!(label(b"12345678901"), Ok(*b"12345678901"));
    assert_eq!(label(b""), Ok(NO_NAME));
    assert_eq!(label(b"   "), Ok(NO_NAME));
    assert_eq!(label(b"123456789012"), Err(MkfsError::LabelTooLong));
    for bad in [b'.', b'/', b'"', b'*', b'?', b':', b'\\', b'|', b'+', b'=', b'<', b'[', 0x00, 0x7F, 0xC3] {
        assert_eq!(label(&[b'A', bad]), Err(MkfsError::LabelChar(bad)), "{bad:#04x}");
    }
    assert_eq!(format!("{}", MkfsError::LabelChar(b'.')), "a FAT label cannot hold '.'");
    assert_eq!(format!("{}", MkfsError::LabelChar(0xC3)), "a FAT label cannot hold the byte 0xc3");
    assert_eq!(
        format!("{}", MkfsError::TooSmall { have: 20_000, need: MIN_SECTORS }),
        "10000 KiB is too small for a FAT; it needs 16 MiB"
    );
}
