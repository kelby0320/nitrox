//! `Fat`'s host tests for writing (Phase 6 Part E.3): each change made by the library, then the
//! image settled as an unmount leaves it and handed to `fsck.fat -n`, which must find it clean, and
//! to mtools, which must read back what was written.

use super::*;
use crate::ReadOnly;
use crate::test_support::{FileImage, fsck, mdel, mdir_all, mformat, mkfs, mtools, mtype, pattern, put};
use std::collections::BTreeSet;
use std::path::Path;

/// 2026-10-06, midnight UTC: an even second, which a FAT time can hold exactly.
const NOW: i64 = 1_791_244_800;

/// The three kinds, at the cluster sizes the library is tested at: 512 bytes, 2 KiB and 4 KiB.
fn images() -> [(&'static str, std::path::PathBuf); 3] {
    [
        ("FAT12", mkfs(2, &["-F", "12", "-s", "1"])),
        ("FAT16", mformat(16, &["-c", "4"])),
        ("FAT32", mformat(64, &["-F", "-c", "8"])),
    ]
}

/// **Settle the volume as an unmount would, and require `fsck.fat -n` to find it clean.**
fn settle(fat: &Fat<FileImage>, what: &str) {
    fat.mark_clean().unwrap();
    let (code, said) = fsck(&fat.device().path);
    assert_eq!(code, 0, "{what}: fsck.fat -n said\n{said}");
}

/// **Write `data` into the file at `path` through its map**, as the kernel writes a page.
fn write_through_map<R: BlockReader + BlockWriter>(fat: &Fat<R>, path: &[u8], data: &[u8]) {
    let mut runs = [BlockRun::default(); 64];
    let m = fat.map_file(path, &mut runs).unwrap();
    assert!(data.len() <= m.size);
    for r in &runs[..m.runs] {
        let from = r.file_block as usize * 512;
        let to = (from + r.length as usize * 512).min(data.len());
        if from < to {
            fat.device().write_at(r.device_lba * 512, &data[from..to]).unwrap();
        }
    }
}

/// **A file's bytes read through its map**, as the kernel fills a page.
fn read_through_map(fat: &Fat<FileImage>, path: &[u8]) -> Vec<u8> {
    let mut runs = [BlockRun::default(); 64];
    let m = fat.map_file(path, &mut runs).unwrap();
    let mut bytes = vec![0u8; m.size];
    for r in &runs[..m.runs] {
        let from = r.file_block as usize * 512;
        let to = (from + r.length as usize * 512).min(m.size);
        if from < to {
            fat.device().read_at(r.device_lba * 512, &mut bytes[from..to]).unwrap();
        }
    }
    bytes
}

/// Create the file `name` in `parent` holding `data`: created, grown, written through its map.
fn make<R: BlockReader + BlockWriter>(fat: &Fat<R>, parent: &str, name: &str, data: &[u8]) {
    fat.create_file(parent.as_bytes(), name.as_bytes(), NOW).unwrap();
    let path = if parent == "/" { format!("/{name}") } else { format!("{parent}/{name}") };
    fat.grow_file(path.as_bytes(), data.len(), NOW).unwrap();
    write_through_map(fat, path.as_bytes(), data);
}

/// **The short names in `dir` beside their long names**, from `mdir`'s listing: ASCII names only.
fn short_names(img: &Path, dir: &str) -> BTreeSet<(String, String)> {
    let out = String::from_utf8(mtools("mdir", img, &[&format!("::{dir}")]).stdout).unwrap();
    out.lines()
        .filter(|l| l.len() >= 12 && !l.starts_with(' ') && !l.starts_with("Directory") && l.is_ascii())
        .map(|l| {
            let (base, ext) = (l[..8].trim(), l[9..12].trim());
            let short = if ext.is_empty() { base.to_string() } else { format!("{base}.{ext}") };
            (short, l.get(42..).unwrap_or("").trim().to_string())
        })
        .collect()
}

/// **Every change, on each kind, leaves a filesystem `fsck.fat` finds clean and mtools reads**:
/// a short name, a long one and a Unicode one created, grown and written; a directory and a file
/// in it; a file shrunk, one emptied, one removed and an empty directory removed.
#[test]
fn every_change_leaves_a_clean_filesystem_that_mtools_reads_back() {
    for (what, img) in images() {
        let disk = FileImage::open(&img);
        let fat = Fat::new(&disk);
        let root = fat.resolve_dir(b"/").unwrap();
        let (long, nested) = (pattern(9_000, 3), pattern(3 * 4096 + 17, 4));

        make(&fat, "/", "README.TXT", b"hello\n");
        settle(&fat, what);
        make(&fat, "/", "a long file name.txt", &long);
        settle(&fat, what);
        make(&fat, "/", "naïve résumé 名前.md", b"unicode\n");
        settle(&fat, what);
        fat.mkdir_at(root, b"docs", NOW).unwrap();
        settle(&fat, what);
        make(&fat, "/docs", "nested file.bin", &nested);
        settle(&fat, what);

        assert_eq!(fat.truncate_file(b"/a long file name.txt", 100, NOW), Ok(None), "{what}: a shrink ends no id");
        settle(&fat, what);
        let id = fat.truncate_file(b"/README.TXT", 0, NOW).unwrap().expect("emptied, its id ends");
        fat.release(id, NOW).unwrap();
        settle(&fat, what);
        make(&fat, "/", "doomed.txt", &pattern(5_000, 5));
        let id = fat.unlink_at(root, b"doomed.txt", NOW).unwrap().expect("its last name, so an id to release");
        fat.release(id, NOW).unwrap();
        settle(&fat, what);
        fat.mkdir_at(root, b"empty", NOW).unwrap();
        fat.rmdir_at(root, b"empty", NOW).unwrap();
        settle(&fat, what);

        let want: BTreeSet<String> =
            ["README.TXT", "a long file name.txt", "naïve résumé 名前.md", "docs/", "docs/nested file.bin"]
                .into_iter()
                .map(str::to_string)
                .collect();
        assert_eq!(mdir_all(&img), want, "{what}");
        assert_eq!(mtype(&img, "README.TXT"), b"", "{what}");
        assert_eq!(mtype(&img, "a long file name.txt"), &long[..100], "{what}");
        assert_eq!(mtype(&img, "naïve résumé 名前.md"), b"unicode\n", "{what}");
        assert_eq!(mtype(&img, "docs/nested file.bin"), nested, "{what}");
        let mtimes: Vec<i64> = {
            let mut m = Vec::new();
            fat.read_dir(root, 0, |e| {
                m.push(e.mtime);
                true
            })
            .unwrap();
            m
        };
        assert!(mtimes.iter().all(|&t| t == NOW), "{what}: every entry written at NOW: {mtimes:?}");
    }
}

/// **A name a FAT cannot hold is refused**, `InvalidName`, before anything is written.
#[test]
fn a_name_a_fat_cannot_hold_is_refused() {
    let img = mformat(16, &["-c", "4"]);
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let root = fat.resolve_dir(b"/").unwrap();
    make(&fat, "/", "fine.txt", b"x");
    for bad in ["a:b", "star*", "trailing.", "tab\tname"] {
        assert_eq!(fat.create_file(b"/", bad.as_bytes(), NOW), Err(FsError::InvalidName), "{bad:?}");
        assert_eq!(fat.mkdir_at(root, bad.as_bytes(), NOW), Err(FsError::InvalidName), "{bad:?}");
        assert_eq!(fat.rename_at(root, b"fine.txt", bad.as_bytes(), NOW), Err(FsError::InvalidName), "{bad:?}");
    }
    let too_long = "n".repeat(256);
    assert_eq!(fat.create_file(b"/", too_long.as_bytes(), NOW), Err(FsError::InvalidName));
    settle(&fat, "refusals");
    assert_eq!(mdir_all(&img), BTreeSet::from(["fine.txt".to_string()]));
}

/// **Short names are unique**: a generated one takes the lowest tail no entry has — past one
/// mtools wrote, and back to `~1` once that is removed — and a name in another case is the same
/// name, so creating it again is nothing.
#[test]
fn a_generated_short_name_takes_the_lowest_tail_free() {
    let img = mformat(16, &["-c", "4"]);
    put(&img, "longname zero.txt", b"0");
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let root = fat.resolve_dir(b"/").unwrap();
    make(&fat, "/", "longname one.txt", b"1");
    make(&fat, "/", "longname two.txt", b"2");
    let id = fat.unlink_at(root, b"longname zero.txt", NOW).unwrap().unwrap();
    fat.release(id, NOW).unwrap();
    make(&fat, "/", "longname three.txt", b"3");
    make(&fat, "/", "lower.txt", b"l");
    fat.create_file(b"/", b"LOWER.TXT", NOW).unwrap();
    settle(&fat, "short names");
    let want: BTreeSet<(String, String)> = [
        ("LONGNA~1.TXT", "longname three.txt"),
        ("LONGNA~2.TXT", "longname one.txt"),
        ("LONGNA~3.TXT", "longname two.txt"),
        ("LOWER~1.TXT", "lower.txt"),
    ]
    .into_iter()
    .map(|(s, l)| (s.to_string(), l.to_string()))
    .collect();
    assert_eq!(short_names(&img, "/"), want);
}

/// **What a grow adds reads as zeroes**: clusters a deleted file left its bytes in, taken again,
/// and the tail of a file's own last cluster past a shrink.
#[test]
fn what_a_grow_adds_reads_as_zeroes() {
    for (what, img) in images() {
        put(&img, "old.bin", &pattern(20_000, 9));
        let disk = FileImage::open(&img);
        let mut runs = [BlockRun::default(); 64];
        // Read before the delete by a volume of its own: this one's cache would not see mtools'.
        Fat::new(&disk).map_file(b"/old.bin", &mut runs).unwrap();
        let old_start = runs[0].device_lba;
        mdel(&img, "old.bin");
        let fat = Fat::new(&disk);

        fat.create_file(b"/", b"new.bin", NOW).unwrap();
        fat.grow_file(b"/new.bin", 20_000, NOW).unwrap();
        fat.map_file(b"/new.bin", &mut runs).unwrap();
        assert_eq!(runs[0].device_lba, old_start, "{what}: the deleted file's clusters, taken again");
        assert!(read_through_map(&fat, b"/new.bin").iter().all(|&b| b == 0), "{what}: a deleted file's bytes");

        write_through_map(&fat, b"/new.bin", &pattern(20_000, 10));
        fat.truncate_file(b"/new.bin", 100, NOW).unwrap();
        fat.grow_file(b"/new.bin", 20_000, NOW).unwrap();
        let back = read_through_map(&fat, b"/new.bin");
        assert_eq!(&back[..100], &pattern(20_000, 10)[..100], "{what}");
        assert!(back[100..].iter().all(|&b| b == 0), "{what}: the file's own bytes past its shrink");
        settle(&fat, what);
    }
}

/// **A full volume is `TooLarge`**, and takes nothing; so is a full fixed root. A directory that
/// is a chain grows instead.
#[test]
fn a_full_volume_and_a_full_fixed_root_are_too_large_and_a_directory_grows() {
    let img = mkfs(2, &["-F", "12", "-s", "1", "-r", "32"]);
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let g = *fat.geometry().unwrap();
    let root = fat.resolve_dir(b"/").unwrap();
    let free = Cache::count_free(&disk, &g).unwrap();

    fat.create_file(b"/", b"BIG.BIN", NOW).unwrap();
    assert_eq!(fat.grow_file(b"/BIG.BIN", (free as usize + 1) * 512, NOW), Err(FsError::TooLarge));
    settle(&fat, "a grow past the free space");
    assert_eq!(Cache::count_free(&disk, &g).unwrap(), free, "nothing taken");
    fat.grow_file(b"/BIG.BIN", free as usize * 512, NOW).unwrap();
    assert_eq!(Cache::count_free(&disk, &g).unwrap(), 0);
    fat.create_file(b"/", b"ONE.BIN", NOW).unwrap();
    assert_eq!(fat.grow_file(b"/ONE.BIN", 1, NOW), Err(FsError::TooLarge));
    assert_eq!(fat.mkdir_at(root, b"DIR", NOW), Err(FsError::TooLarge), "a directory needs a cluster");
    settle(&fat, "a full volume");
    let id = fat.truncate_file(b"/BIG.BIN", 0, NOW).unwrap().unwrap();
    fat.release(id, NOW).unwrap();

    // The root holds 32 entries, two of them taken.
    for i in 0..30 {
        fat.create_file(b"/", format!("F{i}.TXT").as_bytes(), NOW).unwrap();
    }
    assert_eq!(fat.create_file(b"/", b"F30.TXT", NOW), Err(FsError::TooLarge), "a full fixed root");
    let free = Cache::count_free(&disk, &g).unwrap();
    assert_eq!(fat.mkdir_at(root, b"D30", NOW), Err(FsError::TooLarge), "a full fixed root");
    settle(&fat, "a full root");
    assert_eq!(Cache::count_free(&disk, &g).unwrap(), free, "the directory's cluster given back");

    fat.unlink_at(root, b"F0.TXT", NOW).unwrap();
    fat.mkdir_at(root, b"MANY", NOW).unwrap();
    let names: Vec<String> = (0..40).map(|i| format!("file number {i} with a long name.txt")).collect();
    for n in &names {
        fat.create_file(b"/MANY", n.as_bytes(), NOW).unwrap();
    }
    settle(&fat, "a directory grown");
    let listed: BTreeSet<String> = mdir_all(&img)
        .into_iter()
        .filter_map(|p| p.strip_prefix("MANY/").filter(|n| !n.is_empty()).map(str::to_string))
        .collect();
    assert_eq!(listed, names.into_iter().collect::<BTreeSet<_>>());
}

/// **A directory moved has its `..` pointed at its new parent** — another directory, and the
/// root — and one cannot move into itself.
#[test]
fn a_directory_moved_has_its_dotdot_repointed() {
    for (what, img) in images() {
        let disk = FileImage::open(&img);
        let fat = Fat::new(&disk);
        let g = *fat.geometry().unwrap();
        let root = fat.resolve_dir(b"/").unwrap();
        fat.mkdir_at(root, b"a", NOW).unwrap();
        fat.mkdir_at(root, b"b", NOW).unwrap();
        let a = fat.resolve_dir(b"/a").unwrap();
        fat.mkdir_at(a, b"sub", NOW).unwrap();
        make(&fat, "/a/sub", "f.txt", b"inside\n");
        settle(&fat, what);

        let parent_of = |path: &[u8]| {
            let d = Dir::from_id(&g, fat.resolve_dir(path).unwrap()).unwrap();
            dir::parent(&disk, &g, &mut fat.cache.borrow_mut(), d).unwrap().id()
        };
        assert_eq!(fat.rename_path(b"/a/sub", b"/b/sub", false, NOW), Ok(None));
        assert_eq!(parent_of(b"/b/sub"), fat.resolve_dir(b"/b").unwrap(), "{what}");
        settle(&fat, what);
        assert_eq!(fat.rename_path(b"/b/sub", b"/sub", false, NOW), Ok(None));
        assert_eq!(parent_of(b"/sub"), root, "{what}: the root");
        settle(&fat, what);
        assert_eq!(fat.rename_path(b"/b", b"/sub/b", false, NOW), Ok(None));
        assert_eq!(fat.rename_path(b"/sub", b"/sub/b/sub", false, NOW), Err(FsError::Unsupported), "{what}: into itself");
        settle(&fat, what);
        let want: BTreeSet<String> = ["a/", "sub/", "sub/b/", "sub/f.txt"].into_iter().map(str::to_string).collect();
        assert_eq!(mdir_all(&img), want, "{what}");
        assert_eq!(mtype(&img, "sub/f.txt"), b"inside\n", "{what}");
    }
}

/// **Renames**: in another case, to a long name, refused onto a name taken, and replacing a file
/// when asked — whose id comes back to be released.
#[test]
fn a_rename_keeps_the_file_and_a_replace_returns_the_replaced_id() {
    let img = mformat(64, &["-F", "-c", "8"]);
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let root = fat.resolve_dir(b"/").unwrap();
    make(&fat, "/", "notes.txt", b"notes\n");
    fat.rename_at(root, b"notes.txt", b"Notes.TXT", NOW).unwrap();
    settle(&fat, "another case");
    assert_eq!(mdir_all(&img), BTreeSet::from(["Notes.TXT".to_string()]));
    fat.rename_at(root, b"notes.txt", b"renamed with a long name.txt", NOW).unwrap();
    make(&fat, "/", "other.txt", b"other\n");
    assert_eq!(fat.rename_at(root, b"other.txt", b"Renamed With A Long Name.txt", NOW), Err(FsError::Exists));
    let mut runs = [BlockRun::default(); 64];
    let replaced = fat.map_file(b"/renamed with a long name.txt", &mut runs).unwrap().id;
    let id = fat.rename_path(b"/other.txt", b"/renamed with a long name.txt", true, NOW).unwrap();
    assert_eq!(id, Some(replaced), "the replaced file's id, to forget and release");
    fat.release(replaced, NOW).unwrap();
    settle(&fat, "a replace");
    assert_eq!(mdir_all(&img), BTreeSet::from(["renamed with a long name.txt".to_string()]));
    assert_eq!(mtype(&img, "renamed with a long name.txt"), b"other\n");
    assert_eq!(fat.release(replaced, NOW), Err(FsError::NotFound), "released once");
}

/// **`File::Touch` finds a file by its id** — through a rename — and misses one removed.
#[test]
fn a_touch_by_id_finds_the_file_through_a_rename() {
    let img = mformat(16, &["-c", "4"]);
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let root = fat.resolve_dir(b"/").unwrap();
    make(&fat, "/", "touched.txt", b"t");
    let mut runs = [BlockRun::default(); 64];
    let id = fat.map_file(b"/touched.txt", &mut runs).unwrap().id;
    fat.rename_at(root, b"touched.txt", b"moved.txt", NOW).unwrap();
    fat.touch_file(id, NOW + 3600).unwrap();
    let mut mtime = 0;
    fat.read_dir(root, 0, |e| {
        mtime = e.mtime;
        false
    })
    .unwrap();
    assert_eq!(mtime, NOW + 3600);
    assert_eq!(fat.touch_file(id + 1, NOW), Err(FsError::NotFound), "an id never mapped");
    fat.unlink_at(root, b"moved.txt", NOW).unwrap();
    assert_eq!(fat.touch_file(id, NOW), Err(FsError::NotFound), "removed");
    fat.release(id, NOW).unwrap();
    settle(&fat, "touched");
}

/// **A filesystem found dirty is left dirty by its unmount**, and a clean one left clean; FAT32's
/// free count is unknown while mounted and counted at the unmount.
#[test]
fn a_dirty_filesystem_stays_dirty_and_a_clean_one_clean() {
    for (what, img) in images() {
        let disk = FileImage::open(&img);
        let fat = Fat::new(&disk);
        let g = *fat.geometry().unwrap();
        fat.mark_mounted().unwrap();
        assert_eq!(fat.was_left_clean(), Ok(false), "{what}: mounted");
        assert_ne!(fsck(&img).0, 0, "{what}: fsck.fat sees it mounted");
        make(&fat, "/", "file.bin", &pattern(30_000, 1));
        if g.fsinfo != 0 {
            let mut info = [0u8; 512];
            disk.read_at(g.fsinfo as u64 * 512, &mut info).unwrap();
            assert_eq!(&info[488..492], &[0xFF; 4], "{what}: the free count is not kept while mounted");
        }
        fat.mark_clean().unwrap();
        assert_eq!(fat.was_left_clean(), Ok(true), "{what}");
        if g.fsinfo != 0 {
            let mut info = [0u8; 512];
            disk.read_at(g.fsinfo as u64 * 512, &mut info).unwrap();
            let count = Cache::count_free(&disk, &g).unwrap();
            assert_eq!(&info[488..492], &count.to_le_bytes(), "{what}: counted at the unmount");
        }
        assert_eq!(fsck(&img).0, 0, "{what}: {}", fsck(&img).1);

        let mut b = [0u8; 1];
        disk.read_at(g.state_at as u64, &mut b).unwrap();
        disk.write_at(g.state_at as u64, &[b[0] | 1]).unwrap();
        let again = Fat::new(&disk);
        again.mark_mounted().unwrap();
        again.mark_clean().unwrap();
        assert_eq!(again.was_left_clean(), Ok(false), "{what}: found dirty, left dirty");
    }
}

/// A writer that counts its writes.
struct Counting<'a> {
    inner: &'a FileImage,
    writes: Cell<usize>,
}

impl BlockReader for Counting<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        self.inner.read_at(offset, buf)
    }
}

impl BlockWriter for Counting<'_> {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
        self.writes.set(self.writes.get() + 1);
        self.inner.write_at(offset, buf)
    }
}

/// **The write path batches**: a 1 MiB grow — 256 clusters, 2,048 sectors — costs sixteen 64 KiB
/// zeroing writes, the FAT's sectors once per copy, and the entry: about twenty, where a write per
/// cluster would be hundreds.
#[test]
fn a_one_mebibyte_grow_costs_a_handful_of_writes() {
    let img = mformat(64, &["-F", "-c", "8"]);
    let disk = FileImage::open(&img);
    let counting = Counting { inner: &disk, writes: Cell::new(0) };
    let fat = Fat::new(&counting);
    fat.create_file(b"/", b"big.bin", NOW).unwrap();
    counting.writes.set(0);
    fat.grow_file(b"/big.bin", 1 << 20, NOW).unwrap();
    let writes = counting.writes.get();
    assert!(writes <= 24, "a 1 MiB grow took {writes} writes");
    let mut runs = [BlockRun::default(); 64];
    assert_eq!(fat.map_file(b"/big.bin", &mut runs).unwrap().runs, 1, "one run, on a fresh volume");
    fat.mark_clean().unwrap();
    assert_eq!(fsck(&img).0, 0, "{}", fsck(&img).1);
}

/// **A read-only mount refuses every change**, `ReadOnly`, and the image is as it was.
#[test]
fn a_read_only_mount_refuses_every_change() {
    let img = mformat(16, &["-c", "4"]);
    put(&img, "file.txt", b"file\n");
    mtools("mmd", &img, &["::dir"]);
    let before = std::fs::read(&img).unwrap();
    let disk = FileImage::open(&img);
    let ro = ReadOnly(&disk);
    let fat = Fat::new(&ro);
    let root = fat.resolve_dir(b"/").unwrap();
    let mut runs = [BlockRun::default(); 64];
    let id = fat.map_file(b"/file.txt", &mut runs).unwrap().id;
    let refused = FsError::ReadOnly;
    assert_eq!(fat.mark_mounted(), Err(refused));
    assert_eq!(fat.mark_clean(), Err(refused));
    assert_eq!(fat.create_file(b"/", b"new.txt", NOW), Err(refused));
    // Refused before anything is read: a change that would be nothing is refused too.
    assert_eq!(fat.create_file(b"/", b"file.txt", NOW), Err(refused), "one that exists");
    assert_eq!(fat.grow_file(b"/file.txt", 1, NOW), Err(refused), "a grow to less");
    assert_eq!(fat.grow_file(b"/file.txt", 9_000, NOW), Err(refused));
    assert_eq!(fat.truncate_file(b"/file.txt", 0, NOW), Err(refused));
    assert_eq!(fat.mkdir_at(root, b"new", NOW), Err(refused));
    assert_eq!(fat.unlink_at(root, b"file.txt", NOW), Err(refused));
    assert_eq!(fat.rmdir_at(root, b"dir", NOW), Err(refused));
    assert_eq!(fat.touch_at(root, b"file.txt", NOW), Err(refused));
    assert_eq!(fat.rename_at(root, b"file.txt", b"other.txt", NOW), Err(refused));
    assert_eq!(fat.rename_path(b"/file.txt", b"/dir/file.txt", false, NOW), Err(refused));
    assert_eq!(fat.touch_file(id, NOW), Err(refused));
    assert_eq!(fat.release(id, NOW), Err(refused));
    drop(fat);
    assert!(std::fs::read(&img).unwrap() == before, "the image is unchanged");
}
