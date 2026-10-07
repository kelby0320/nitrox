//! `Fat` on **bytes a correct writer never produces** (PR #365 review, finding 1): a FAT is anyone's
//! bytes, so a malformed one must be an `FsError`, never a panic — which, in the server, would leave
//! it spinning in its panic handler with a resolve waiting on it for ever.

use super::*;
use crate::test_support::{FileImage, Overlay, mformat, mkfs, mmd, put};
use std::rc::Rc;

/// **A long-name entry of ordinal 0 after a complete long name** is stale, and the short name
/// stands alone. Its ordinal once passed as the next one expected — `0`, once a name is whole — and
/// its slot in the name was computed as `0 - 1`.
#[test]
fn a_long_name_entry_of_ordinal_zero_is_stale() {
    let img = mformat(64, &["-F", "-c", "8"]);
    put(&img, "a long file name.txt", b"long\n");
    put(&img, "OTHER.TXT", b"other\n");
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let g = *fat.geometry().unwrap();
    let f = dir::lookup(&disk, &g, &mut fat.cache.borrow_mut(), Dir::root(&g), b"a long file name.txt").unwrap();
    let at = dir::slot_byte(&disk, &g, &mut fat.cache.borrow_mut(), f.dir, f.slot).unwrap().unwrap();
    let mut e = [0u8; 32];
    disk.read_at(at, &mut e).unwrap();
    let mut lfn = [0u8; 32];
    disk.read_at(at - 32, &mut lfn).unwrap();
    // The short entry becomes a long-name entry of ordinal 0, with the checksum the name's carry.
    e[0] = 0x20;
    e[11] = dir::ATTR_LONG;
    e[12] = 0;
    e[13] = lfn[13];
    e[26] = 0;
    e[27] = 0;
    disk.write_at(at, &e).unwrap();
    let fat = Fat::new(&disk);
    let mut buf = [0u8; 16];
    assert_eq!(fat.read_file(b"/OTHER.TXT", &mut buf), Ok(6), "the next file, past the bad entry");
    assert_eq!(fat.read_file(b"/a long file name.txt", &mut buf), Err(FsError::NotFound));
}

/// A small, seeded generator: the same garbage every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// **Everything a request can do**, on whatever the image holds: each result is ignored, since
/// only a panic is wrong here.
fn exercise(fat: &Fat<Overlay>) {
    // What the server serves: nothing its check refuses.
    if fat.check().is_err() {
        return;
    }
    let _ = (fat.label(), fat.was_left_clean());
    let mut names: Vec<Vec<u8>> = Vec::new();
    if let Ok(root) = fat.resolve_dir(b"/") {
        let mut cursor = 0;
        for _ in 0..64 {
            let Ok(next) = fat.read_dir(root, cursor, |e| {
                if names.len() < 16 {
                    names.push(e.name.to_vec());
                }
                true
            }) else {
                break;
            };
            if next == 0 {
                break;
            }
            cursor = next;
        }
    }
    let mut buf = vec![0u8; 64 * 1024];
    let mut runs = [BlockRun::default(); 64];
    for n in &names {
        let mut path = b"/".to_vec();
        path.extend_from_slice(n);
        let _ = fat.read_file(&path, &mut buf);
        let _ = fat.read_file_range(&path, 4096, 8192, &mut buf);
        let _ = fat.map_file(&path, &mut runs);
        if let Ok(d) = fat.resolve_dir(&path) {
            let _ = fat.read_dir(d, 0, |_| true);
            let _ = fat.mkdir_at(d, b"inner made here", NOW);
        }
        let _ = fat.touch_file(1, NOW);
    }
    let _ = fat.mark_mounted();
    let _ = fat.create_file(b"/", b"new file.txt", NOW);
    let _ = fat.grow_file(b"/new file.txt", 6_000, NOW);
    let _ = fat.truncate_file(b"/new file.txt", 100, NOW);
    if let Ok(root) = fat.resolve_dir(b"/") {
        let _ = fat.mkdir_at(root, b"made", NOW);
        let _ = fat.rename_path(b"/new file.txt", b"/made/moved.txt", false, NOW);
        for n in names.iter().take(4) {
            let _ = fat.touch_at(root, n, NOW);
            let _ = fat.rename_at(root, n, b"renamed by the test", NOW);
            if let Ok(Some(id)) = fat.unlink_at(root, b"renamed by the test", NOW) {
                let _ = fat.release(id, NOW);
            }
            let _ = fat.rmdir_at(root, b"renamed by the test", NOW);
        }
    }
    let _ = fat.mark_clean();
}

const NOW: i64 = 1_791_244_800;

/// **Garbage anywhere a FAT keeps its structure is an error, never a panic**, on FAT12, FAT16 and
/// FAT32. Three kinds, each run on a fresh copy of an image mtools filled, then every operation:
/// - a random byte over the boot sector's fields, the FAT, the fixed root and the first clusters;
/// - **a field of a real entry** — its first byte, attributes, cluster or size — made random;
/// - **an entry turned into a long-name part** carrying its neighbour's checksum and a random
///   ordinal, the shape that found the ordinal-0 underflow: random bytes alone almost never match a
///   long name's checksum.
#[test]
fn garbage_in_any_structure_is_an_error_never_a_panic() {
    for (what, img) in [
        ("FAT12", mkfs(2, &["-F", "12", "-s", "8"])),
        ("FAT16", mformat(16, &["-c", "4"])),
        ("FAT32", mformat(64, &["-F", "-c", "8"])),
    ] {
        put(&img, "a long file name.txt", &[7u8; 9000]);
        put(&img, "README.TXT", b"readme\n");
        mmd(&img, "dir");
        put(&img, "dir/an inner file with a long name.bin", &[9u8; 5000]);
        put(&img, "naïve résumé.md", b"unicode\n");
        let base = Rc::new(std::fs::read(&img).unwrap());
        let _ = std::fs::remove_file(&img);
        let pristine = Overlay::new(base.clone());
        let fat = Fat::new(&pristine);
        let g = *fat.geometry().unwrap();
        // Every slot in use, in the root and in `dir`, by its byte.
        let mut slots: Vec<u64> = Vec::new();
        let sub = dir::resolve_dir(&pristine, &g, &mut fat.cache.borrow_mut(), b"/dir").unwrap();
        for d in [Dir::root(&g), sub] {
            let mut cache = fat.cache.borrow_mut();
            let mut found = Vec::new();
            dir::walk_slots(&pristine, &g, &mut cache, d, 0, |slot, _| {
                found.push(slot);
                true
            })
            .unwrap();
            for slot in found {
                slots.push(dir::slot_byte(&pristine, &g, &mut cache, d, slot).unwrap().unwrap());
            }
        }
        let data = g.data_start as u64 * 512;
        let regions =
            [(11u64, 90u64), (g.reserved as u64 * 512, 4096), (g.root_start as u64 * 512, 4096), (data, 64 * 1024)];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for seed in 0..150 {
            let img = Overlay::new(base.clone());
            let at = slots[rng.below(slots.len() as u64) as usize];
            let mut e = [0u8; 32];
            img.read_at(at, &mut e).unwrap();
            match seed % 3 {
                0 => {
                    for _ in 0..1 + rng.below(8) {
                        let (start, len) = regions[rng.below(regions.len() as u64) as usize];
                        img.write_at(start + rng.below(len), &[rng.next() as u8]).unwrap();
                    }
                }
                1 => {
                    let field = [0usize, 11, 20, 26, 28][rng.below(5) as usize];
                    e[field] = rng.next() as u8;
                    e[field + 1] = rng.next() as u8;
                    img.write_at(at, &e).unwrap();
                }
                _ => {
                    let mut before = [0u8; 32];
                    img.read_at(at.saturating_sub(32), &mut before).unwrap();
                    // An ordinal past a name's end, at its start, in its middle, or none — with
                    // the last-part flag or not, and the bits above it, since an entry whose first
                    // byte is `0` ends the directory before anything reads it as a long name.
                    let ord = [0u8, 0, 1, 2, 20, 21, 31][rng.below(7) as usize];
                    e[0] = ord | [0x00, 0x20, 0x40, 0x60, 0x80, 0xA0][rng.below(6) as usize];
                    e[11] = dir::ATTR_LONG;
                    e[12] = 0;
                    e[13] = before[13];
                    e[26] = 0;
                    e[27] = 0;
                    img.write_at(at, &e).unwrap();
                }
            }
            let fat = Fat::new(&img);
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| exercise(&fat)));
            assert!(ran.is_ok(), "{what}, seed {seed}: a request panicked on garbage");
        }
    }
}

/// **A file whose chain is shorter than its size is `Corrupt` to read, as to map** (PR #365 review,
/// finding 7): the read stopped where the chain did and answered the whole length, the rest of the
/// buffer whatever it held — in the server, the last request's bytes.
#[test]
fn a_chain_shorter_than_its_file_is_corrupt_to_read() {
    let img = mformat(64, &["-F", "-c", "8"]);
    put(&img, "short.bin", &[5u8; 100]);
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let g = *fat.geometry().unwrap();
    let f = dir::lookup(&disk, &g, &mut fat.cache.borrow_mut(), Dir::root(&g), b"short.bin").unwrap();
    let at = dir::slot_byte(&disk, &g, &mut fat.cache.borrow_mut(), f.dir, f.slot).unwrap().unwrap();
    disk.write_at(at + 28, &40_000u32.to_le_bytes()).unwrap();
    let fat = Fat::new(&disk);
    let mut buf = vec![0xEEu8; 40_000];
    let mut runs = [BlockRun::default(); 64];
    assert_eq!(fat.map_file(b"/short.bin", &mut runs).err(), Some(FsError::Corrupt));
    assert_eq!(fat.read_file(b"/short.bin", &mut buf), Err(FsError::Corrupt));
    assert_eq!(fat.read_file_range(b"/short.bin", 8192, 4096, &mut buf), Err(FsError::Corrupt), "wholly past the chain");
    assert_eq!(fat.read_file_range(b"/short.bin", 0, 100, &mut buf), Ok(100), "within the chain, a read is a read");
}

/// **A long name over 255 bytes of UTF-8 is listed by its short name, and reached by either**
/// (PR #365 review, finding 6): a listing entry carries 255 bytes, and the lookup compared against
/// that same form, so such a file could be neither listed nor reached, and its directory never
/// removed. `名` is three bytes, so 85 of them and `.txt` are 259 — and stored whole: mtools cuts a
/// longer name at about 255 bytes, mid-character, so the 274-byte name this test first used was
/// never on the disk, which `mdir` confirms here for the one it does use.
#[test]
fn a_long_name_too_long_for_a_listing_is_listed_short_and_reached_by_either() {
    let img = mformat(16, &["-c", "4"]);
    mmd(&img, "d");
    let long = format!("{}.txt", "名".repeat(85));
    assert_eq!(long.len(), 259);
    put(&img, &format!("d/{long}"), b"far east\n");
    assert!(crate::test_support::mdir_all(&img).contains(&format!("d/{long}")), "stored whole");
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let d = fat.resolve_dir(b"/d").unwrap();
    let mut listed = Vec::new();
    fat.read_dir(d, 0, |e| {
        listed.push(String::from_utf8(e.name.to_vec()).unwrap());
        true
    })
    .unwrap();
    assert_eq!(listed.len(), 1, "listed: {listed:?}");
    let short = listed[0].clone();
    assert!(short.contains('~') && short.len() <= 12, "by its short name: {short}");
    let mut buf = [0u8; 16];
    assert_eq!(fat.read_file(format!("/d/{short}").as_bytes(), &mut buf), Ok(9), "by the name it is listed under");
    assert_eq!(fat.read_file(format!("/d/{long}").as_bytes(), &mut buf), Ok(9), "by its long name");
    let upper = format!("{}.TXT", "名".repeat(85));
    assert_eq!(fat.read_file(format!("/d/{upper}").as_bytes(), &mut buf), Ok(9), "its ASCII in another case");
    let id = fat.unlink_at(d, long.as_bytes(), NOW).unwrap().unwrap();
    fat.release(id, NOW).unwrap();
    let root = fat.resolve_dir(b"/").unwrap();
    assert_eq!(fat.rmdir_at(root, b"d", NOW), Ok(()), "empty once the file is gone");
}

/// **A directory removed is no directory to a session still holding its id** (PR #365 review,
/// finding 5): a FAT directory's id is its first cluster, which a removal frees and a grow may take.
/// Every operation by id refuses it `NotFound` — freed, or taken by a file — and the filesystem
/// stays clean. Made again at the same cluster, it is a directory again.
#[test]
fn a_removed_directory_is_refused_to_a_session_still_holding_it() {
    let img = mformat(16, &["-c", "8"]);
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let root = fat.resolve_dir(b"/").unwrap();
    fat.mkdir_at(root, b"d", NOW).unwrap();
    let d = fat.resolve_dir(b"/d").unwrap();
    fat.rmdir_at(root, b"d", NOW).unwrap();
    let refused = |fat: &Fat<FileImage>, what: &str| {
        assert_eq!(fat.mkdir_at(d, b"x", NOW), Err(FsError::NotFound), "{what}");
        assert_eq!(fat.touch_at(d, b"x", NOW), Err(FsError::NotFound), "{what}");
        assert_eq!(fat.unlink_at(d, b"x", NOW), Err(FsError::NotFound), "{what}");
        assert_eq!(fat.rmdir_at(d, b"x", NOW), Err(FsError::NotFound), "{what}");
        assert_eq!(fat.rename_at(d, b"x", b"y", NOW), Err(FsError::NotFound), "{what}");
        assert_eq!(fat.read_dir(d, 0, |_| true), Err(FsError::NotFound), "{what}");
    };
    refused(&fat, "freed");
    // A volume of its own, whose allocation starts at cluster 2 rather than past the last one, so
    // the grow takes the cluster the removal freed.
    let fat = Fat::new(&disk);
    fat.create_file(b"/", b"taker.bin", NOW).unwrap();
    fat.grow_file(b"/taker.bin", 3 * 4096, NOW).unwrap();
    let mut runs = [BlockRun::default(); 4];
    let m = fat.map_file(b"/taker.bin", &mut runs).unwrap();
    assert_eq!(m.id, d, "the file took the directory's cluster");
    refused(&fat, "taken by a file");
    fat.mark_clean().unwrap();
    let (code, said) = crate::test_support::fsck(&img);
    assert_eq!(code, 0, "{said}");
}
