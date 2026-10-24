//! `Fat` on **bytes a correct writer never produces**: a FAT is anyone's
//! bytes, so a malformed one must be an `FsError`, never a panic — which, in the server, would leave
//! it spinning in its panic handler with a resolve waiting on it for ever.

use super::*;
use crate::bpb::Kind;
use crate::test_support::{FileImage, Overlay, mformat, mkfs, mmd, put};
use std::rc::Rc;

/// **A long-name entry of ordinal 0 after a complete long name** is stale: the short name stands.
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
    // A mount refuses every change once one finds it corrupt; each change here is to run anyway.
    let open = |fat: &Fat<Overlay>| fat.broken.set(false);
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
            open(fat);
            let _ = fat.mkdir_at(d, b"inner made here", NOW);
        }
        open(fat);
        let _ = fat.touch_file(1, NOW);
    }
    let _ = fat.mark_mounted();
    open(fat);
    let _ = fat.create_file(b"/", b"new file.txt", NOW);
    open(fat);
    let _ = fat.grow_file(b"/new file.txt", 6_000, NOW);
    open(fat);
    let _ = fat.truncate_file(b"/new file.txt", 100, NOW);
    if let Ok(root) = fat.resolve_dir(b"/") {
        open(fat);
        let _ = fat.mkdir_at(root, b"made", NOW);
        open(fat);
        let _ = fat.rename_path(b"/new file.txt", b"/made/moved.txt", false, NOW);
        for n in names.iter().take(4) {
            open(fat);
            let _ = fat.touch_at(root, n, NOW);
            open(fat);
            let _ = fat.rename_at(root, n, b"renamed by the test", NOW);
            open(fat);
            if let Ok(Some(id)) = fat.unlink_at(root, b"renamed by the test", NOW) {
                open(fat);
                let _ = fat.release(id, NOW);
            }
            open(fat);
            let _ = fat.rmdir_at(root, b"renamed by the test", NOW, |_| ());
        }
    }
    open(fat);
    let _ = fat.mark_clean();
}

const NOW: i64 = 1_791_244_800;

/// **Garbage anywhere a FAT keeps its structure is an error, never a panic**, on FAT12, FAT16 and
/// FAT32. Three kinds, each run on a fresh copy of an image mtools filled, then every operation:
/// - a random byte over the boot sector's fields, the FAT, the fixed root and the first clusters;
/// - **a field of a real entry** — its first byte, attributes, cluster or size — made random;
/// - **an entry turned into a long-name part** carrying its neighbour's checksum and a random
///   ordinal: random bytes alone almost never match a long name's checksum.
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

/// **A file whose chain is shorter than its size is `Corrupt` to read, as to map**: the read
/// stopped where the chain did and answered the whole length, the rest of the
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
    assert_eq!(
        fat.read_file_range(b"/short.bin", 8192, 4096, &mut buf),
        Err(FsError::Corrupt),
        "wholly past the chain"
    );
    assert_eq!(fat.read_file_range(b"/short.bin", 0, 100, &mut buf), Ok(100), "within the chain, a read is a read");
}

/// **A long name over 255 bytes of UTF-8 is listed by its short name, and reached by either**
/// a listing entry carries 255 bytes, and the lookup compared against
/// that same form, so such a file could be neither listed nor reached, and its directory never
/// removed. `名` is three bytes, so 85 of them and `.txt` are 259, which mtools stores whole
/// (`mdir` confirms it); a longer name it cuts at about 255 bytes, mid-character.
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
    let mut freed = None;
    assert_eq!(fat.rmdir_at(root, b"d", NOW, |id| freed = Some(id)), Ok(()), "empty once the file is gone");
    assert_eq!(freed, Some(d), "and its id given");
}

/// **A directory removed is no directory to a session still holding its id**: a FAT
/// directory's id is its first cluster, which a removal frees and a grow may take.
/// Every operation by id refuses it `NotFound`, freed or taken by a file; the volume stays clean.
#[test]
fn a_removed_directory_is_refused_to_a_session_still_holding_it() {
    let img = mformat(16, &["-c", "8"]);
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let root = fat.resolve_dir(b"/").unwrap();
    fat.mkdir_at(root, b"d", NOW).unwrap();
    let d = fat.resolve_dir(b"/d").unwrap();
    fat.rmdir_at(root, b"d", NOW, |_| ()).unwrap();
    let refused = |fat: &Fat<FileImage>, what: &str| {
        assert_eq!(fat.mkdir_at(d, b"x", NOW), Err(FsError::NotFound), "{what}");
        assert_eq!(fat.touch_at(d, b"x", NOW), Err(FsError::NotFound), "{what}");
        assert_eq!(fat.unlink_at(d, b"x", NOW), Err(FsError::NotFound), "{what}");
        assert_eq!(fat.rmdir_at(d, b"x", NOW, |_| ()), Err(FsError::NotFound), "{what}");
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

/// **A long name is FAT's to limit, not the protocol's**: 100 CJK characters are 300 bytes of
/// UTF-8 and 100 units, which FAT holds, so a session may name them as FAT can.
#[test]
fn a_long_name_past_255_bytes_of_utf8_passes_the_protocol_and_is_made() {
    let img = mformat(16, &["-c", "8"]);
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let root = fat.resolve_dir(b"/").unwrap();
    let long = "名".repeat(100);
    assert!(libfsserver::serve::entry_name(long.as_bytes()), "a session may name it");
    fat.mkdir_at(root, long.as_bytes(), NOW).unwrap();
    let mut freed = None;
    assert_eq!(fat.rmdir_at(root, long.as_bytes(), NOW, |id| freed = Some(id)), Ok(()), "and by it removed");
    assert!(freed.is_some());
    assert_eq!(fat.mkdir_at(root, "名".repeat(256).as_bytes(), NOW), Err(FsError::InvalidName), "past 255 units");
}

/// **A loop is refused within a directory's reach, not the volume's**: a directory whose chain
/// comes back on itself after as many clusters as `MAX_SLOTS` entries fill, and a `..` naming its
/// own directory after as many steps up as a path can take. Each read the whole volume first.
#[test]
fn a_loop_in_a_directory_is_refused_within_a_directorys_reach() {
    use std::cell::Cell;
    struct Counted<'a>(&'a Overlay, Cell<u64>);
    impl BlockReader for Counted<'_> {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
            self.1.set(self.1.get() + 1);
            self.0.read_at(offset, buf)
        }
    }
    impl BlockWriter for Counted<'_> {
        fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
            self.0.write_at(offset, buf)
        }
    }
    let img = mformat(64, &["-c", "8"]);
    mmd(&img, "dir");
    mmd(&img, "dir/sub");
    let base = Rc::new(std::fs::read(&img).unwrap());
    let _ = std::fs::remove_file(&img);
    let found = |over: &Overlay, path: &[u8]| {
        let fat = Fat::new(over);
        let g = *fat.geometry().unwrap();
        let (parent, name) = path.split_at(path.iter().rposition(|&b| b == b'/').unwrap());
        let d = dir::resolve_dir(over, &g, &mut fat.cache.borrow_mut(), if parent.is_empty() { b"/" } else { parent });
        (g, dir::lookup(over, &g, &mut fat.cache.borrow_mut(), d.unwrap(), &name[1..]).unwrap())
    };

    let over = Overlay::new(base.clone());
    let (g, d) = found(&over, b"/dir");
    let fat = Fat::new(&over);
    fat.cache.borrow_mut().set(&over, &g, d.cluster, d.cluster).unwrap();
    fat.cache.borrow_mut().flush(&over, &g).unwrap();
    // Deleted entries fill the cluster, so a walk goes on round the loop.
    for slot in 2..g.cluster_bytes() / ENTRY {
        let at = dir::slot_byte(&over, &g, &mut fat.cache.borrow_mut(), Dir::Chain(d.cluster), slot).unwrap();
        over.write_at(at.unwrap(), &[DELETED]).unwrap();
    }
    let counted = Counted(&over, Cell::new(0));
    assert_eq!(Fat::new(&counted).resolve_dir(b"/dir/missing"), Err(FsError::Corrupt));
    let reach = (MAX_SLOTS * ENTRY).div_ceil(g.cluster_bytes()) as u64 * g.sectors_per_cluster as u64;
    assert!(counted.1.get() <= reach + 64, "a looping directory cost {} reads, its reach {reach}", counted.1.get());

    let over = Overlay::new(base);
    let (g, sub) = found(&over, b"/dir/sub");
    let fat = Fat::new(&over);
    let at = dir::slot_byte(&over, &g, &mut fat.cache.borrow_mut(), Dir::Chain(sub.cluster), 1).unwrap().unwrap();
    over.write_at(at + 26, &(sub.cluster as u16).to_le_bytes()).unwrap();
    let counted = Counted(&over, Cell::new(0));
    assert_eq!(Fat::new(&counted).rename_path(b"/dir", b"/dir/sub/x", false, NOW), Err(FsError::Corrupt));
    assert!(counted.1.get() <= 1000, "a looping .. cost {} reads", counted.1.get());
}

/// **An allocation passes over a reserved FAT entry**, neither taking its cluster nor failing: it
/// was `Corrupt`, losing the clusters it had ended, and so was every allocation after.
#[test]
fn an_allocation_passes_over_a_reserved_entry() {
    let img = mformat(64, &["-c", "8"]);
    put(&img, "A.TXT", b"a");
    let over = Overlay::new(Rc::new(std::fs::read(&img).unwrap()));
    let _ = std::fs::remove_file(&img);
    let fat = Fat::new(&over);
    let g = *fat.geometry().unwrap();
    assert_eq!(g.kind, Kind::Fat16, "a 16-bit entry is poked");
    let a = dir::lookup(&over, &g, &mut fat.cache.borrow_mut(), Dir::root(&g), b"A.TXT").unwrap().cluster;
    let reserved = a + 2;
    over.write_at(g.fat_byte(0) + reserved as u64 * 2, &0xFFF1u16.to_le_bytes()).unwrap();
    let free = Cache::count_free(&over, &g).unwrap();
    let cb = g.cluster_bytes() as usize;
    assert_eq!(Volume::grow_file(&fat, b"/A.TXT", 4 * cb, NOW), Ok(1));
    let mut chain = Vec::new();
    fat.cache
        .borrow_mut()
        .walk(&over, &g, a, |_, c| {
            chain.push(c);
            true
        })
        .unwrap();
    assert_eq!(chain, [a, a + 1, a + 3, a + 4], "the reserved cluster passed over");
    assert_eq!(fat.cache.borrow_mut().raw(&over, &g, reserved), Ok(0xFFF1), "and left as it was");
    assert_eq!(Cache::count_free(&over, &g), Ok(free - 3), "nothing taken but what the file holds");
    let root = fat.resolve_dir(b"/").unwrap();
    assert_eq!(fat.mkdir_at(root, b"after", NOW), Ok(()), "and the volume still written");
}

/// **A rename to a name a FAT cannot hold is refused** though an entry already has it, which a
/// replace would otherwise take over without writing a name of its own.
#[test]
fn a_rename_onto_an_entry_a_fat_cannot_name_is_refused() {
    let img = mformat(64, &["-c", "8"]);
    put(&img, "F.TXT", b"f");
    put(&img, "AXB", b"x");
    let over = Overlay::new(Rc::new(std::fs::read(&img).unwrap()));
    let _ = std::fs::remove_file(&img);
    let fat = Fat::new(&over);
    let g = *fat.geometry().unwrap();
    let f = dir::lookup(&over, &g, &mut fat.cache.borrow_mut(), Dir::root(&g), b"AXB").unwrap();
    let at = dir::slot_byte(&over, &g, &mut fat.cache.borrow_mut(), f.dir, f.slot).unwrap().unwrap();
    over.write_at(at + 1, b"*").unwrap();
    let fat = Fat::new(&over);
    assert!(fat.read_file(b"/A*B", &mut [0u8; 8]).is_ok(), "the entry found by its name");
    assert_eq!(fat.rename_path(b"/F.TXT", b"/A*B", true, NOW), Err(FsError::InvalidName));
}

/// **A move as deep as a path can reach is made, not refused as a loop**: its target's parent is
/// climbed to the root in as many steps as the longest path the protocol carries has components.
#[test]
fn a_move_as_deep_as_a_path_reaches_is_made() {
    let img = mformat(64, &["-c", "8"]);
    mmd(&img, "x");
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let g = *fat.geometry().unwrap();
    let mut d = fat.resolve_dir(b"/").unwrap();
    let mut path = Vec::new();
    for _ in 0..libfsserver::serve::MAX_SUFFIX.div_ceil(2) - 1 {
        fat.mkdir_at(d, b"d", NOW).unwrap();
        let parent = Dir::from_id(&g, d).unwrap();
        d = dir::lookup(&disk, &g, &mut fat.cache.borrow_mut(), parent, b"d").unwrap().cluster as u64;
        path.extend_from_slice(b"/d");
    }
    path.extend_from_slice(b"/x");
    assert!(path.len() - 1 <= libfsserver::serve::MAX_SUFFIX, "a suffix the protocol carries");
    assert_eq!(fat.rename_path(b"/x", &path, false, NOW), Ok(None));
}

/// **A directory as large as one may be is walked to its last entry**, and one a cluster larger is
/// refused: the walk's bound is `MAX_SLOTS` entries, held at its neighbours.
#[test]
fn a_directory_as_large_as_one_may_be_is_walked_to_its_last_entry() {
    let img = mformat(64, &["-c", "8"]);
    mmd(&img, "D");
    let base = std::fs::read(&img).unwrap();
    let _ = std::fs::remove_file(&img);
    let probe = Overlay::new(Rc::new(base.clone()));
    let fat = Fat::new(&probe);
    let g = *fat.geometry().unwrap();
    assert_eq!(g.kind, Kind::Fat16, "16-bit entries are written");
    let d = dir::lookup(&probe, &g, &mut fat.cache.borrow_mut(), Dir::root(&g), b"D").unwrap().cluster;
    let cb = g.cluster_bytes() as usize;
    // `D`'s chain made `clusters` long, every slot past `..` deleted but the last, a file's.
    let built = |clusters: u32| {
        let mut b = base.clone();
        for k in 0..clusters {
            let next = if k + 1 == clusters { 0xFFFF } else { (d + k + 1) as u16 };
            for copy in 0..g.fats {
                let at = (g.fat_byte(copy) + (d + k) as u64 * 2) as usize;
                b[at..at + 2].copy_from_slice(&next.to_le_bytes());
            }
            let at = (g.cluster_sector(d + k) * SECTOR as u64) as usize;
            for s in (at + if k == 0 { 2 * ENTRY as usize } else { 0 }..at + cb).step_by(ENTRY as usize) {
                b[s] = DELETED;
            }
        }
        let last = (g.cluster_sector(d + clusters - 1) * SECTOR as u64) as usize + cb - ENTRY as usize;
        b[last..last + 32].copy_from_slice(&dir::short_entry(g.kind, b"LAST    TXT", ATTR_ARCHIVE, 0, 0, NOW));
        Overlay::new(Rc::new(b))
    };
    let full = MAX_SLOTS / (g.cluster_bytes() / ENTRY);
    let over = built(full);
    assert_eq!(Fat::new(&over).read_file(b"/D/LAST.TXT", &mut [0; 8]), Ok(0), "MAX_SLOTS entries");
    let over = built(full + 1);
    assert_eq!(Fat::new(&over).read_file(b"/D/LAST.TXT", &mut [0; 8]), Err(FsError::Corrupt), "a cluster more");
}
