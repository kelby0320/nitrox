//! FAT's guards at their bounds: what a listing leaves out, a long name's parts, a rewrite's
//! reach, the next-free hint, the reserved bits, and a short name's numeric tail.

use super::*;
use crate::names;
use crate::test_support::{FileImage, Overlay, fsck, mformat, put};
use std::cell::Cell;
use std::rc::Rc;

const NOW: i64 = 1_791_244_800;

/// The names a listing of `/` gives.
fn listed<R: BlockReader + BlockWriter>(fat: &Fat<R>) -> Vec<String> {
    let root = fat.resolve_dir(b"/").unwrap();
    let mut names = Vec::new();
    fat.read_dir(root, 0, |e| {
        names.push(String::from_utf8_lossy(e.name).into_owned());
        true
    })
    .unwrap();
    names
}

/// **A volume's label is not a name**: its entry in the root is in no listing, and no lookup
/// finds it.
#[test]
fn the_volume_label_is_not_a_name() {
    let img = mformat(16, &["-c", "4", "-v", "NXFAT"]);
    put(&img, "file.txt", b"f\n");
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let g = *fat.geometry().unwrap();
    let mut labels = 0;
    dir::walk_slots(&disk, &g, &mut fat.cache.borrow_mut(), Dir::root(&g), 0, |_, e| {
        labels += usize::from(e[0] != 0 && e[11] & 0x3F == dir::ATTR_VOLUME);
        true
    })
    .unwrap();
    assert_eq!(labels, 1, "the root holds the label's entry");
    assert_eq!(listed(&fat), ["file.txt"]);
    assert_eq!(fat.read_file(b"/NXFAT", &mut [0u8; 16]), Err(FsError::NotFound));
}

/// **A long name with a part out of place is no long name**: a part with another checksum, or
/// with a type or a cluster that is not zero, leaves the short entry standing alone, as a stale
/// name does. The part changed is the last read, so only its own check can drop it.
#[test]
fn a_long_name_with_a_part_out_of_place_is_dropped() {
    let long = "a long name in three parts, past twenty-six.txt";
    let img = mformat(16, &["-c", "4"]);
    put(&img, long, b"x");
    let base = Rc::new(std::fs::read(&img).unwrap());
    let _ = std::fs::remove_file(&img);
    let whole = Overlay::new(base.clone());
    assert_eq!(listed(&Fat::new(&whole)), [long], "whole, as written");
    for (what, at, byte) in
        [("a checksum", 13, 0x5A), ("a type", 12, 1), ("a cluster", 26, 1), ("a cluster's high byte", 27, 1)]
    {
        let disk = Overlay::new(base.clone());
        let fat = Fat::new(&disk);
        let g = *fat.geometry().unwrap();
        let f = dir::lookup(&disk, &g, &mut fat.cache.borrow_mut(), Dir::root(&g), long.as_bytes()).unwrap();
        let short = dir::slot_byte(&disk, &g, &mut fat.cache.borrow_mut(), f.dir, f.slot).unwrap().unwrap();
        let mut part = [0u8; 32];
        disk.read_at(short - 32, &mut part).unwrap();
        assert_eq!(part[0], 1, "the part beside the short entry is the first");
        assert_ne!(part[at], byte);
        part[at] = byte;
        disk.write_at(short - 32, &part).unwrap();
        let fat = Fat::new(&disk);
        let shown = listed(&fat);
        assert_eq!(shown.len(), 1, "{what}");
        assert_ne!(shown[0], long, "{what}: the short name alone");
        assert_eq!(fat.read_file(&[b"/", long.as_bytes()].concat(), &mut [0u8; 4]), Err(FsError::NotFound), "{what}");
        assert_eq!(fat.read_file(&[b"/", shown[0].as_bytes()].concat(), &mut [0u8; 4]), Ok(1), "{what}");
    }
}

/// **A rewrite reaches at most a name's slots**: the longest name's 21, and refuses one more
/// before it reads or writes any.
#[test]
fn a_rewrite_reaches_a_names_slots_and_no_further() {
    let img = mformat(16, &["-c", "4"]);
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let g = *fat.geometry().unwrap();
    let mut cache = fat.cache.borrow_mut();
    let last = dir::MAX_NAME_SLOTS as u32 - 1;
    let mut seen = 0;
    assert_eq!(dir::rewrite(&disk, &g, &mut cache, Dir::root(&g), 0, last, |_, _| seen += 1), Ok(()));
    assert_eq!(seen, dir::MAX_NAME_SLOTS);
    assert_eq!(
        dir::rewrite(&disk, &g, &mut cache, Dir::root(&g), 0, last + 1, |_, _| seen += 1),
        Err(FsError::Corrupt)
    );
    assert_eq!(seen, dir::MAX_NAME_SLOTS, "none of the 22 rewritten");
}

/// The first cluster of the file at `path`.
fn first_cluster<R: BlockReader + BlockWriter>(fat: &Fat<R>, path: &[u8]) -> u64 {
    let mut runs = [BlockRun::default(); 4];
    fat.map_file(path, &mut runs).unwrap().id
}

/// **The next-free hint an unmount writes is where the next mount allocates**: past a cluster
/// freed before it, which a hint unwritten or unread would have the allocation take.
#[test]
fn a_mount_allocates_from_the_hint_its_unmount_wrote() {
    let img = mformat(64, &["-F", "-c", "8"]);
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    fat.mark_mounted().unwrap();
    let root = fat.resolve_dir(b"/").unwrap();
    for name in [&b"a.bin"[..], b"b.bin"] {
        fat.create_file(b"/", name, NOW).unwrap();
        fat.grow_file(&[b"/", name].concat(), 4096, NOW).unwrap();
    }
    let (a, b) = (first_cluster(&fat, b"/a.bin"), first_cluster(&fat, b"/b.bin"));
    let id = fat.unlink_at(root, b"a.bin", NOW).unwrap().unwrap();
    fat.release(id, NOW).unwrap();
    fat.mark_clean().unwrap();

    let fat = Fat::new(&disk);
    fat.mark_mounted().unwrap();
    fat.create_file(b"/", b"c.bin", NOW).unwrap();
    fat.grow_file(b"/c.bin", 4096, NOW).unwrap();
    assert_eq!(first_cluster(&fat, b"/c.bin"), b + 1, "past b, at {b}, not a's freed {a}");
    fat.mark_clean().unwrap();
    assert_eq!(fsck(&img).0, 0, "{}", fsck(&img).1);
}

/// **A hint outside the volume is not written back**: an unmount writes where its allocations
/// ended, or the volume's first cluster, never a number no cluster has.
#[test]
fn a_hint_outside_the_volume_is_not_written_back() {
    let img = mformat(64, &["-F", "-c", "8"]);
    let disk = FileImage::open(&img);
    let g = *Fat::new(&disk).geometry().unwrap();
    let hint_at = g.fsinfo as u64 * 512 + 492;
    disk.write_at(hint_at, &0x7FFF_FFF0u32.to_le_bytes()).unwrap();
    let fat = Fat::new(&disk);
    fat.mark_mounted().unwrap();
    fat.mark_clean().unwrap();
    let mut hint = [0u8; 4];
    disk.read_at(hint_at, &mut hint).unwrap();
    assert!(g.valid_cluster(u32::from_le_bytes(hint)), "{hint:x?}");
}

/// **FAT32's top four bits are reserved, and kept**: the clusters a grow takes and links keep
/// what their entries held there, in every copy of the FAT.
#[test]
fn a_grow_keeps_fat32s_reserved_bits() {
    let img = mformat(64, &["-F", "-c", "8"]);
    let disk = FileImage::open(&img);
    let g = *Fat::new(&disk).geometry().unwrap();
    let taken = g.root_cluster + 1..g.root_cluster + 4;
    for copy in 0..g.fats {
        for c in taken.clone() {
            disk.write_at(g.fat_byte(copy) + c as u64 * 4 + 3, &[0xA0]).unwrap();
        }
    }
    let fat = Fat::new(&disk);
    fat.create_file(b"/", b"a.bin", NOW).unwrap();
    fat.grow_file(b"/a.bin", 3 * 4096, NOW).unwrap();
    assert_eq!(first_cluster(&fat, b"/a.bin"), taken.start as u64);
    for copy in 0..g.fats {
        for c in taken.clone() {
            let mut e = [0u8; 4];
            disk.read_at(g.fat_byte(copy) + c as u64 * 4, &mut e).unwrap();
            let v = u32::from_le_bytes(e);
            assert_eq!(v >> 28, 0xA, "copy {copy}, cluster {c}: {v:#x}");
            let next = if c + 1 == taken.end { v & 0x0FFF_FFFF >= 0x0FFF_FFF8 } else { v & 0x0FFF_FFFF == c + 1 };
            assert!(next, "copy {copy}, cluster {c}: {v:#x}");
        }
    }
}

/// An image that counts its reads from `from` on: a directory's, past the FAT's.
struct Reads<'a> {
    bytes: &'a [u8],
    from: u64,
    n: Cell<u32>,
}

impl BlockReader for Reads<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        self.n.set(self.n.get() + u32::from(offset >= self.from));
        buf.copy_from_slice(self.bytes.get(offset as usize..offset as usize + buf.len()).ok_or(FsError::Io)?);
        Ok(())
    }
}

/// **A FAT32 root of `MAX_SLOTS` short entries**, slot `i` `LONGNA~<i + 1>.TXT`, but at tail
/// `free` a name of another basis.
fn crowded_root(free: u32) -> (Vec<u8>, Geometry) {
    let img = mformat(64, &["-F", "-c", "8"]);
    let mut bytes = std::fs::read(&img).unwrap();
    let _ = std::fs::remove_file(&img);
    let g = *Fat::new(&Overlay::new(Rc::new(bytes.clone()))).geometry().unwrap();
    let clusters = dir::MAX_SLOTS / (g.cluster_bytes() / ENTRY);
    for copy in 0..g.fats {
        for k in 0..clusters {
            let c = g.root_cluster + k;
            let next = if k + 1 == clusters { 0x0FFF_FFFF } else { c + 1 };
            let at = (g.fat_byte(copy) + c as u64 * 4) as usize;
            bytes[at..at + 4].copy_from_slice(&next.to_le_bytes());
        }
    }
    let (b, bn, e, en) = names::basis(b"long name.txt");
    let root = (g.cluster_sector(g.root_cluster) * 512) as usize;
    for i in 0..dir::MAX_SLOTS {
        let short = if i + 1 == free { *b"OTHER   TXT" } else { names::with_tail(&b[..bn], &e[..en], i + 1) };
        let at = root + i as usize * ENTRY as usize;
        bytes[at..at + 32].copy_from_slice(&dir::short_entry(g.kind, &short, ATTR_ARCHIVE, 0, 0, NOW));
    }
    (bytes, g)
}

/// **A short name's numeric tail is found among all a directory can hold, a thousand and
/// twenty-four a walk**: at once for a basis no entry has, the last for a root of `MAX_SLOTS`
/// entries of one basis but that one, and none when it has every one.
#[test]
fn a_numeric_tail_is_found_up_to_a_full_directory() {
    let tails = |free: u32, name: &[u8]| {
        let (bytes, g) = crowded_root(free);
        let r = Reads { bytes: &bytes, from: g.data_start as u64 * 512, n: Cell::new(0) };
        let mut out = [[0u8; 32]; dir::MAX_NAME_SLOTS];
        let short = |s: &[u8; 11]| core::array::from_fn(|i| if i < 11 { s[i] } else { 0 });
        let made = dir::name_entries(&r, &g, &mut Cache::new(), Dir::root(&g), name, short, &mut out);
        (made.map(|n| out[n - 1][..11].to_vec()), r.n.get())
    };
    let (alone, walk) = tails(1, b"zz top.txt");
    assert_eq!(alone, Ok(names::with_tail(b"ZZTOP", b"TXT", 1).to_vec()), "a basis no entry has");
    let (last, reads) = tails(dir::MAX_SLOTS, b"long name.txt");
    assert_eq!(last, Ok(names::with_tail(b"LONGNA", b"TXT", dir::MAX_SLOTS).to_vec()));
    assert_eq!(reads, 64 * walk, "{} walks", reads / walk);
    let (none, reads) = tails(0, b"long name.txt");
    assert_eq!(none, Err(FsError::TooLarge));
    assert_eq!(reads, 64 * walk, "{} walks", reads / walk);
}
