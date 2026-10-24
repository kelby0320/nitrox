//! `Fat`'s host tests for writing: each change made by the library, then the
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

/// **Every change, on each kind, leaves a filesystem `fsck.fat` finds clean and mtools reads**.
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
        fat.rmdir_at(root, b"empty", NOW, |_| ()).unwrap();
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

/// **Short names are unique**: past one mtools wrote, and back to `~1` once that is removed; a
/// name in another case is the same name, so creating it again is nothing.
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
        assert_eq!(
            fat.rename_path(b"/sub", b"/sub/b/sub", false, NOW),
            Err(FsError::Unsupported),
            "{what}: into itself"
        );
        settle(&fat, what);
        let want: BTreeSet<String> = ["a/", "sub/", "sub/b/", "sub/f.txt"].into_iter().map(str::to_string).collect();
        assert_eq!(mdir_all(&img), want, "{what}");
        assert_eq!(mtype(&img, "sub/f.txt"), b"inside\n", "{what}");
    }
}

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
    // Two empty files share cluster 0 and are still two files, not one under both names.
    fat.create_file(b"/", b"e1", NOW).unwrap();
    fat.create_file(b"/", b"e2", NOW).unwrap();
    assert_eq!(fat.rename_at(root, b"e1", b"e2", NOW), Err(FsError::Exists));
}

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

/// FAT32's free count is unknown while a filesystem is mounted and counted at the unmount.
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
/// zeroing writes, the FAT's sectors once per copy, and the entry: about twenty, not hundreds.
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
    assert_eq!(fat.rmdir_at(root, b"dir", NOW, |_| ()), Err(refused));
    assert_eq!(fat.touch_at(root, b"file.txt", NOW), Err(refused));
    assert_eq!(fat.rename_at(root, b"file.txt", b"other.txt", NOW), Err(refused));
    assert_eq!(fat.rename_path(b"/file.txt", b"/dir/file.txt", false, NOW), Err(refused));
    assert_eq!(fat.touch_file(id, NOW), Err(refused));
    assert_eq!(fat.release(id, NOW), Err(refused));
    assert!(std::fs::read(&img).unwrap() == before, "the image is unchanged");
}

/// **`File::Touch` stamps only the file the table says**: the entry at the place it holds must
/// still be a live file with that cluster. Here another system removed
/// the file and put an empty one in its slot; the table cannot know, and the touch must not stamp
/// the newcomer. The table's own `forget` never runs, so this is the guard alone.
#[test]
fn a_touch_by_id_stamps_nothing_where_another_file_took_the_place() {
    let img = mformat(16, &["-c", "4"]);
    put(&img, "OLD.TXT", b"old\n");
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let mut runs = [BlockRun::default(); 4];
    let id = fat.map_file(b"/OLD.TXT", &mut runs).unwrap().id;
    mdel(&img, "OLD.TXT");
    put(&img, "NEW.TXT", b"");
    let stamp = |fat: &Fat<FileImage>| {
        let mut m = 0;
        fat.read_dir(fat.resolve_dir(b"/").unwrap(), 0, |e| {
            m = e.mtime;
            false
        })
        .unwrap();
        m
    };
    let before = stamp(&Fat::new(&disk));
    assert_eq!(fat.touch_file(id, NOW + 7200), Err(FsError::NotFound));
    assert_eq!(stamp(&Fat::new(&disk)), before, "the newcomer unstamped");
}

/// **A change bigger than the FAT cache**: a 100 MiB grow on a 300 MiB FAT32 dirties 200 FAT
/// sectors where the cache holds 64, so sectors are evicted dirty. Then a shrink, a regrow and a
/// removal; `fsck.fat -n` clean after each, and the free count back where it started.
#[test]
fn a_change_bigger_than_the_fat_cache_evicts_and_stays_clean() {
    let img = mformat(300, &["-F", "-c", "8"]);
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let g = *fat.geometry().unwrap();
    let free = Cache::count_free(&disk, &g).unwrap();
    make(&fat, "/", "big.bin", b"");
    let steps: [(&str, &dyn Fn()); 3] = [
        ("a 100 MiB grow", &|| {
            fat.grow_file(b"/big.bin", 100 << 20, NOW).unwrap();
        }),
        ("a shrink to 10 MiB", &|| assert_eq!(fat.truncate_file(b"/big.bin", 10 << 20, NOW), Ok(None))),
        ("a regrow", &|| {
            fat.grow_file(b"/big.bin", 100 << 20, NOW).unwrap();
        }),
    ];
    for (what, step) in steps {
        step();
        settle(&fat, what);
    }
    let root = fat.resolve_dir(b"/").unwrap();
    let id = fat.unlink_at(root, b"big.bin", NOW).unwrap().unwrap();
    fat.release(id, NOW).unwrap();
    settle(&fat, "the removal");
    assert_eq!(Cache::count_free(&disk, &g).unwrap(), free);
}

/// **A read never loads over a dirty sector**: with every slot of the cache dirty, a read of a
/// sector it does not hold is refused, and the dirty sectors are still there to be written.
#[test]
fn a_read_with_every_slot_dirty_is_refused_and_loses_nothing() {
    let img = mformat(300, &["-F", "-c", "8"]);
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let g = *fat.geometry().unwrap();
    let mut cache = Cache::new();
    let per = 128; // FAT32 entries a sector
    for s in 0..crate::table::CACHE_SECTORS as u32 {
        cache.set(&disk, &g, (s * per).max(2), table::end_mark(&g)).unwrap();
    }
    let beyond = crate::table::CACHE_SECTORS as u32 * per + 5;
    assert_eq!(cache.next(&disk, &g, beyond), Err(FsError::Io));
    assert_eq!(cache.flush(&disk, &g).unwrap(), 2, "all 64 still dirty: one run, to each copy");
}

/// **Nothing the metadata cache holds goes stale**, as `fs-server-ext4`'s test holds it for ext4:
/// through a cache of eight blocks, so it evicts all the time, files are made, written behind the
/// server, renamed, removed, cut and grown back, and a directory's clusters become file data. Every
/// file and listing then reads right and `fsck.fat -n` finds the volume clean.
#[test]
fn every_change_through_the_metadata_cache_leaves_it_and_the_disk_agreeing() {
    use libfsserver::cache::{MetaCache, Store};
    const SIZE: usize = 6000;
    let img = mformat(64, &["-F", "-c", "8"]);
    let disk = FileImage::open(&img);
    let mut store = Box::new(Store::<8>::new());
    let cached = MetaCache::new(&disk, &mut store);
    let fat = Fat::new(&cached);
    // The kernel's write-back: straight to the image, over the sectors the file's map names.
    let kernel_writes = |path: &[u8], byte: u8| {
        let mut runs = [BlockRun::default(); 64];
        let m = fat.map_file(path, &mut runs).unwrap();
        for r in &runs[..m.runs] {
            let from = r.file_block as usize * 512;
            let to = (from + r.length as usize * 512).min(m.size);
            if from < to {
                disk.write_at(r.device_lba * 512, &vec![byte; to - from]).unwrap();
            }
        }
    };
    let path = |dir: &str, name: &str| format!("/{dir}/{name}").into_bytes();

    let root = fat.resolve_dir(b"/").unwrap();
    fat.mkdir_at(root, b"d1", NOW).unwrap();
    for i in 0..150u8 {
        let name = format!("f{i:03}.bin");
        fat.create_file(b"/d1", name.as_bytes(), NOW).unwrap();
        fat.grow_file(&path("d1", &name), SIZE, NOW).unwrap();
        kernel_writes(&path("d1", &name), i);
    }
    for i in 0..30 {
        let (from, to) = (path("d1", &format!("f{i:03}.bin")), path("d1", &format!("g{i:03}.bin")));
        fat.rename_path(&from, &to, false, NOW).unwrap();
    }
    let d1 = fat.resolve_dir(b"/d1").unwrap();
    for i in 30..60 {
        if let Some(id) = fat.unlink_at(d1, format!("f{i:03}.bin").as_bytes(), NOW).unwrap() {
            fat.release(id, NOW).unwrap();
        }
    }
    // A directory of eighty long names, emptied and removed: its clusters are free for data.
    fat.mkdir_at(root, b"d2", NOW).unwrap();
    let d2 = fat.resolve_dir(b"/d2").unwrap();
    for i in 0..80 {
        fat.create_file(b"/d2", format!("a long name to fill a directory {i:03}.txt").as_bytes(), NOW).unwrap();
    }
    for i in 0..80 {
        let name = format!("a long name to fill a directory {i:03}.txt");
        if let Some(id) = fat.unlink_at(d2, name.as_bytes(), NOW).unwrap() {
            fat.release(id, NOW).unwrap();
        }
    }
    fat.rmdir_at(root, b"d2", NOW, |_| ()).unwrap();
    for i in 0..20 {
        let name = format!("n{i:02}.bin");
        fat.create_file(b"/d1", name.as_bytes(), NOW).unwrap();
        fat.grow_file(&path("d1", &name), SIZE, NOW).unwrap();
        kernel_writes(&path("d1", &name), 0xEE);
    }
    // Cut and grown back: what the kernel wrote before the cut stays, and the rest reads zero.
    for i in 120..130 {
        if let Some(id) = fat.truncate_file(&path("d1", &format!("f{i:03}.bin")), 1000, NOW).unwrap() {
            fat.release(id, NOW).unwrap();
        }
        fat.grow_file(&path("d1", &format!("f{i:03}.bin")), SIZE, NOW).unwrap();
    }
    let mut want: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..30u8 {
        want.push((format!("g{i:03}.bin"), vec![i; SIZE]));
    }
    for i in 60..150u8 {
        let cut = (120..130).contains(&i);
        want.push((format!("f{i:03}.bin"), (0..SIZE).map(|at| if cut && at >= 1000 { 0 } else { i }).collect()));
    }
    for i in 0..20 {
        want.push((format!("n{i:02}.bin"), vec![0xEE; SIZE]));
    }
    let mut out = vec![0u8; libfsserver::serve::MAX_FILE];
    for (name, bytes) in &want {
        let n = fat.read_file(&path("d1", name), &mut out).unwrap();
        assert!(out[..n] == bytes[..], "{name} reads what was written");
    }
    let mut names = Vec::new();
    fat.read_dir(d1, 0, |e| {
        names.push(String::from_utf8_lossy(e.name).into_owned());
        true
    })
    .unwrap();
    let mut names: BTreeSet<String> = names.into_iter().filter(|n| n != "." && n != "..").collect();
    let expected: BTreeSet<String> = want.iter().map(|(n, _)| n.clone()).collect();
    assert_eq!(names, expected);
    names.clear();
    fat.mark_clean().unwrap();
    let (code, said) = fsck(&img);
    assert_eq!(code, 0, "fsck.fat -n said\n{said}");

    // **A file's data is never answered from the cache**: read through one large enough to keep it,
    // written again by the kernel, and read at once — the new bytes, not the ones read.
    let mut big = Box::new(Store::<64>::new());
    let cached = MetaCache::new(&disk, &mut big);
    let fat = Fat::new(&cached);
    for i in 0..5u8 {
        let p = path("d1", &format!("g{i:03}.bin"));
        fat.read_file(&p, &mut out).unwrap();
        let mut runs = [BlockRun::default(); 64];
        let m = fat.map_file(&p, &mut runs).unwrap();
        for r in &runs[..m.runs] {
            let from = r.file_block as usize * 512;
            let to = (from + r.length as usize * 512).min(m.size);
            if from < to {
                disk.write_at(r.device_lba * 512, &vec![0xA0 + i; to - from]).unwrap();
            }
        }
        let n = fat.read_file(&p, &mut out).unwrap();
        assert!(out[..n].iter().all(|&b| b == 0xA0 + i), "g{i:03}.bin reads what the kernel wrote last");
    }
}

/// **What FAT frees leaves the metadata cache**, as ext4's test holds
/// it: a directory's clusters, read through the cache and so held, are not held once the directory
/// is removed — clusters that may next be a file's data, which the kernel writes behind the server.
/// Nothing reads such a cluster back today, since a cluster that becomes a directory's is written
/// whole first: without `forget` every other test passes.
#[test]
fn what_fat_frees_leaves_the_metadata_cache() {
    use libfsserver::cache::{MetaCache, Store};
    let img = mformat(64, &["-F", "-c", "8"]);
    let disk = FileImage::open(&img);
    let mut store = Box::new(Store::<64>::new());
    let cached = MetaCache::new(&disk, &mut store);
    let fat = Fat::new(&cached);
    let root = fat.resolve_dir(b"/").unwrap();
    fat.mkdir_at(root, b"d", NOW).unwrap();
    let d = fat.resolve_dir(b"/d").unwrap();
    let name = |i: usize| format!("a long name to fill a directory {i:03}.txt");
    for i in 0..80 {
        fat.create_file(b"/d", name(i).as_bytes(), NOW).unwrap();
    }
    for i in 0..80 {
        if let Some(id) = fat.unlink_at(d, name(i).as_bytes(), NOW).unwrap() {
            fat.release(id, NOW).unwrap();
        }
    }
    let g = *fat.geometry().unwrap();
    let mut clusters = Vec::new();
    fat.cache
        .borrow_mut()
        .walk(&cached, &g, d as u32, |_, c| {
            clusters.push(c);
            true
        })
        .unwrap();
    fat.read_dir(d, 0, |_| true).unwrap();
    let at = |c: u32| g.cluster_sector(c) * 512;
    assert!(clusters.len() > 1, "a directory of {} clusters", clusters.len());
    assert!(clusters.iter().all(|&c| cached.holds(at(c))), "each read through the cache, so held");

    fat.rmdir_at(root, b"d", NOW, |_| ()).unwrap();
    for c in clusters {
        let (first, last) = (at(c), at(c) + g.cluster_bytes() as u64 - 1);
        assert!(!cached.holds(first) && !cached.holds(last), "cluster {c} is free and still held");
    }
}

/// **A directory is read from the device once**, as ext4's test holds it. Through a server's
/// cache, a lookup that misses reads the directory in 4 KiB blocks, and every lookup after it
/// reads nothing — here in a directory of 1,024, which a debug build makes in seconds.
#[test]
fn a_directory_is_read_from_the_device_once_through_the_cache() {
    use libfsserver::cache::{MetaCache, SERVER_BLOCKS, Store};
    use std::cell::Cell;
    struct Counted<'a>(&'a FileImage, Cell<u32>);
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
    let img = mformat(64, &["-F", "-c", "8"]);
    let disk = FileImage::open(&img);
    {
        // SAFETY: a `Store` of all zeroes is an empty one, and a megabyte is too much to build on a
        // test's stack, as `Box::new` would.
        let mut store = unsafe { Box::<Store<SERVER_BLOCKS>>::new_zeroed().assume_init() };
        let cached = MetaCache::new(&disk, &mut store);
        let fat = Fat::new(&cached);
        let root = fat.resolve_dir(b"/").unwrap();
        fat.mkdir_at(root, b"small", NOW).unwrap();
        for i in 0..1024 {
            fat.create_file(b"/small", format!("S{i:04}.BIN").as_bytes(), NOW).unwrap();
        }
        fat.mark_clean().unwrap();
    }
    let counted = Counted(&disk, Cell::new(0));
    let mut runs = [BlockRun::default(); 64];
    let uncached = Fat::new(&counted);
    uncached.map_file(b"/small/S0900.BIN", &mut runs).unwrap();
    let scan = counted.1.take();

    // SAFETY: a `Store` of all zeroes is an empty one, and a megabyte is too much to build on a
    // test's stack, as `Box::new` would.
    let mut store = unsafe { Box::<Store<SERVER_BLOCKS>>::new_zeroed().assume_init() };
    let cached = MetaCache::new(&counted, &mut store);
    let fat = Fat::new(&cached);
    assert_eq!(fat.map_file(b"/small/S9999.BIN", &mut runs).err(), Some(FsError::NotFound));
    let missed = counted.1.take();
    for _ in 0..3 {
        assert_eq!(fat.map_file(b"/small/S9999.BIN", &mut runs).err(), Some(FsError::NotFound));
        for name in ["S0000.BIN", "S0500.BIN", "S1023.BIN"] {
            fat.map_file(format!("/small/{name}").as_bytes(), &mut runs).unwrap();
        }
    }
    assert_eq!(counted.1.take(), 0, "every lookup again reads nothing");
    assert!(scan >= 50 && missed <= 16, "uncached, a lookup took {scan} commands; cached, the first read {missed}");
}

/// **A removal gives its directory's id before any write it can fail after**, since the server
/// ends the sessions on that id with it: FAT writes a change's sectors as it ends, and with each
/// failing in turn, through the server's call, a directory whose id was not given is still there
/// and its first cluster not the next directory's. A refusal gives nothing.
#[test]
fn a_removal_gives_its_id_before_any_write_it_can_fail_after() {
    use crate::test_support::{Overlay, mmd};
    use std::cell::Cell;
    use std::rc::Rc;
    struct Failing {
        image: Overlay,
        writes: Cell<u64>,
        at: u64,
    }
    impl BlockReader for Failing {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
            self.image.read_at(offset, buf)
        }
    }
    impl BlockWriter for Failing {
        fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
            self.writes.set(self.writes.get() + 1);
            if self.writes.get() == self.at + 1 {
                return Err(FsError::Io);
            }
            self.image.write_at(offset, buf)
        }
    }
    let img = mformat(16, &["-c", "8"]);
    mmd(&img, "full");
    mmd(&img, "full/inner");
    mmd(&img, "gone");
    put(&img, "afile", b"x");
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let root = fat.resolve_dir(b"/").unwrap();
    let gone = fat.resolve_dir(b"/gone").unwrap();
    let mut given = None;
    assert_eq!(fat.rmdir_at(root, b"full", NOW, |id| given = Some(id)), Err(FsError::NotEmpty));
    assert_eq!(fat.rmdir_at(root, b"afile", NOW, |id| given = Some(id)), Err(FsError::Unsupported));
    let ro = ReadOnly(&disk);
    assert_eq!(Fat::new(&ro).rmdir_at(root, b"gone", NOW, |id| given = Some(id)), Err(FsError::ReadOnly));
    assert_eq!(given, None, "a refusal gives no id");

    let image = Rc::new(std::fs::read(&img).unwrap());
    for n in 0.. {
        let f = Failing { image: Overlay::new(image.clone()), writes: 0.into(), at: n };
        let mut given = None;
        let r = Volume::rmdir_at(&Fat::new(&f), root, b"gone", NOW, |id| given = Some(id));
        let after = Fat::new(&f.image);
        match given {
            Some(id) => assert_eq!(id, gone, "write {n} failing"),
            None => {
                assert_eq!(after.resolve_dir(b"/gone"), Ok(gone), "write {n} failing, nothing given");
                after.mkdir_at(root, b"fresh", NOW).unwrap();
                assert_ne!(after.resolve_dir(b"/fresh"), Ok(gone), "write {n}: its cluster reused");
            }
        }
        if r.is_ok() {
            assert_eq!(given, Some(gone), "a removal that succeeded");
            break;
        }
    }
}

/// **A truncate stopped at any write is answered with the file as it now is**, as on ext4: an
/// error kept the kernel on a map whose freed clusters the next file takes, so the answer, an error
/// or a map, names no cluster the FAT holds free.
#[test]
fn a_truncate_stopped_at_any_write_is_answered_with_the_file_as_it_is() {
    use crate::test_support::Overlay;
    use librsproto::namespace::{RESOLVE_FILE_LAZY, RESOLVE_TRUNCATE, resolve_request};
    use std::cell::Cell;
    use std::rc::Rc;
    struct Failing {
        image: Overlay,
        writes: Cell<u64>,
        at: u64,
    }
    impl BlockReader for Failing {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
            self.image.read_at(offset, buf)
        }
    }
    impl BlockWriter for Failing {
        fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
            self.writes.set(self.writes.get() + 1);
            if self.writes.get() == self.at + 1 {
                return Err(FsError::Io);
            }
            self.image.write_at(offset, buf)
        }
    }
    let mut body = [0u8; 64];
    let n = resolve_request(&mut body, 0, RESOLVE_TRUNCATE | RESOLVE_FILE_LAZY, b"BIG.BIN").unwrap();
    body[n..n + 4].copy_from_slice(&4096u32.to_le_bytes());
    let mut req = [0u8; 256];
    let len = librsproto::encode(&mut req, librsproto::OP_NS_RESOLVE, 7, 0, &body[..n + 4], 0).unwrap();
    let img = mformat(16, &["-c", "8"]);
    put(&img, "BIG.BIN", &[7u8; 40960]);
    let base = Rc::new(std::fs::read(&img).unwrap());
    let clusters = |fat: &Fat<'_, Failing>| {
        let g = *fat.geometry().unwrap();
        let mut runs = [BlockRun::default(); 8];
        let m = Volume::map_file(fat, b"/BIG.BIN", &mut runs).unwrap();
        let sectors = runs[..m.runs].iter().filter(|r| r.device_lba != 0);
        let at = sectors.flat_map(|r| r.device_lba..r.device_lba + r.length as u64).collect::<BTreeSet<_>>();
        at.into_iter()
            .map(|s| 2 + ((s - g.data_start as u64) / g.sectors_per_cluster as u64) as u32)
            .collect::<BTreeSet<_>>()
    };
    let had = clusters(&Fat::new(&Failing { image: Overlay::new(base.clone()), writes: 0.into(), at: u64::MAX }));
    for at in 0..12 {
        let disk = Failing { image: Overlay::new(base.clone()), writes: 0.into(), at };
        let fat = Fat::new(&disk);
        let named = match libfsserver::serve::apply_size(&fat, &req[..len], NOW) {
            Err(_) => had.clone(),
            Ok(_) => clusters(&fat),
        };
        let g = *fat.geometry().unwrap();
        let free: Vec<u32> =
            named.into_iter().filter(|&c| fat.cache.borrow_mut().next(&disk, &g, c) == Ok(Next::Free)).collect();
        assert!(free.is_empty(), "write {at} refused: answered over free clusters {free:?}");
    }
}

/// **A mount refused for a failed write leaves the filesystem as clean as it was found**: FAT32's
/// FSInfo is written before the state bit. The bit went first, and a refused FSInfo write left a
/// stick that read as not cleanly unmounted ever after.
#[test]
fn a_mount_refused_for_a_failed_write_leaves_it_as_clean_as_found() {
    use crate::test_support::Failing;
    let img = mformat(64, &["-F", "-c", "8"]);
    let base = std::rc::Rc::new(std::fs::read(&img).unwrap());
    for (n, once) in [(0, true), (1, true), (0, false), (1, false)] {
        let disk = if once { Failing::once(base.clone(), n) } else { Failing::after(base.clone(), n) };
        let fat = Fat::new(&disk);
        assert_eq!(fat.mark_mounted(), Err(FsError::Io), "write {n}");
        assert_eq!(fat.was_left_clean(), Ok(true), "write {n} refused: as clean as found");
    }
}

/// **A change that fails on the device, or finds the volume corrupt, leaves it marked for a check**
/// at its unmount: what is on the device is no longer certain, and clearing the bit said it was.
#[test]
fn a_change_that_fails_on_the_device_leaves_the_volume_marked() {
    use crate::test_support::Failing;
    let img = mformat(64, &["-c", "8"]);
    put(&img, "A.TXT", b"a");
    let base = std::rc::Rc::new(std::fs::read(&img).unwrap());
    let unmounted = |fault: u64, poke: Option<(u64, u32)>| {
        let disk = Failing::once(base.clone(), fault);
        if let Some((at, v)) = poke {
            disk.image.write_at(at, &v.to_le_bytes()).unwrap();
        }
        let fat = Fat::new(&disk);
        fat.mark_mounted().unwrap();
        let grown = Volume::grow_file(&fat, b"/A.TXT", 20_000, NOW).map(drop);
        fat.mark_clean().unwrap();
        (grown, fat.was_left_clean().unwrap())
    };
    assert_eq!(unmounted(u64::MAX, None), (Ok(()), true), "a sound change leaves it clean");
    assert_eq!(unmounted(1, None), (Err(FsError::Io), false), "the grow's first write refused");
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let g = *fat.geometry().unwrap();
    let f = dir::lookup(&disk, &g, &mut fat.cache.borrow_mut(), Dir::root(&g), b"A.TXT").unwrap();
    let at = dir::slot_byte(&disk, &g, &mut fat.cache.borrow_mut(), f.dir, f.slot).unwrap().unwrap();
    // Three clusters' worth of bytes over a chain of one.
    let short = Some((at + 28, 3 * g.cluster_bytes()));
    assert_eq!(unmounted(u64::MAX, short), (Err(FsError::Corrupt), false), "a chain shorter than its size");
}
