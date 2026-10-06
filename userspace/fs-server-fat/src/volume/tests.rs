//! `Fat`'s host tests: images the host's tools build, read through the library.

use super::*;
use crate::BlockWriter;
use crate::test_support::{FileImage, mformat, mkfs, mmd, pattern, put};
use std::collections::BTreeSet;

/// Every entry of the directory at `path`, as `(name, is a directory, size)`, read a cursor at a
/// time with room for `per_call` entries each.
fn listing(fat: &Fat<FileImage>, path: &[u8], per_call: usize) -> BTreeSet<(String, bool, u64)> {
    let id = fat.resolve_dir(path).unwrap();
    let mut seen = BTreeSet::new();
    let mut cursor = 0;
    loop {
        let mut taken = 0;
        cursor = fat
            .read_dir(id, cursor, |e| {
                if taken == per_call {
                    return false;
                }
                taken += 1;
                assert!(seen.insert((String::from_utf8(e.name.to_vec()).unwrap(), e.kind == DIRENT_KIND_DIR, e.size)));
                true
            })
            .unwrap();
        if cursor == 0 {
            return seen;
        }
    }
}

/// A file's bytes read **through its map**, as the kernel would: each run's sectors off the device.
fn through_map(fat: &Fat<FileImage>, path: &[u8]) -> (Vec<u8>, usize) {
    let mut runs = [BlockRun::default(); 64];
    let m = fat.map_file(path, &mut runs).unwrap();
    assert_eq!(m.block_size, 512, "a map in sectors");
    let mut bytes = Vec::new();
    for (i, r) in runs[..m.runs].iter().enumerate() {
        assert_eq!(r.file_block, bytes.len() as u64 / 512, "run {i} begins where the last ended");
        let mut b = vec![0u8; r.length as usize * 512];
        fat.device().read_at(r.device_lba * 512, &mut b).unwrap();
        bytes.extend_from_slice(&b);
    }
    bytes.truncate(m.size);
    (bytes, m.runs)
}

/// **What mtools wrote, read back** on FAT12, FAT16 and FAT32 — the names, kinds and sizes a
/// listing gives, a short name with Windows' case bits, a long name, a Unicode one, a file in a
/// subdirectory — and each file's bytes, read whole, read in part, and read through its map.
#[test]
fn reads_names_kinds_sizes_and_contents_on_fat12_fat16_and_fat32() {
    for (what, img) in [
        ("FAT12", mkfs(2, &["-F", "12", "-s", "1"])),
        ("FAT16", mformat(16, &["-c", "4"])),
        ("FAT32", mformat(64, &["-F", "-c", "8"])),
    ] {
        let long = pattern(9_000, 1);
        let nested = pattern(3 * 4096 + 17, 2);
        put(&img, "README.TXT", b"hello\n");
        put(&img, "a long file name.txt", &long);
        put(&img, "naïve résumé.md", b"unicode\n");
        put(&img, "lower.txt", b"lower case\n");
        mmd(&img, "docs");
        put(&img, "docs/nested file.bin", &nested);
        let disk = FileImage::open(&img);
        let fat = Fat::new(&disk);
        let kind = fat.geometry().unwrap().kind;
        assert_eq!(format!("{kind:?}"), what.replace("FAT", "Fat"), "{what}");

        let want: BTreeSet<(String, bool, u64)> = [
            ("README.TXT", false, 6),
            ("a long file name.txt", false, 9_000),
            ("naïve résumé.md", false, 8),
            ("lower.txt", false, 11),
            ("docs", true, 0),
        ]
        .into_iter()
        .map(|(n, d, s)| (n.to_string(), d, s))
        .collect();
        assert_eq!(listing(&fat, b"/", 64), want, "{what}");
        assert_eq!(listing(&fat, b"/", 1), want, "{what}, a cursor at a time");
        assert_eq!(listing(&fat, b"/docs", 64).len(), 1, "{what}");

        let mut buf = vec![0u8; 1 << 16];
        let n = fat.read_file(b"/a long file name.txt", &mut buf).unwrap();
        assert_eq!(&buf[..n], &long[..], "{what}");
        let n = fat.read_file(b"/docs/nested file.bin", &mut buf).unwrap();
        assert_eq!(&buf[..n], &nested[..], "{what}");
        let n = fat.read_file_range(b"/docs/nested file.bin", 4000, 5000, &mut buf).unwrap();
        assert_eq!(&buf[..n], &nested[4000..9000], "{what}: a range across clusters");
        let n = fat.read_file_range(b"/docs/nested file.bin", 12_000, 4096, &mut buf).unwrap();
        assert_eq!(&buf[..n], &nested[12_000..], "{what}: a range clamped at the end");
        assert_eq!(through_map(&fat, b"/docs/nested file.bin").0, nested, "{what}");
        assert_eq!(through_map(&fat, b"/a long file name.txt").0, long, "{what}");
        let mut runs = [BlockRun::default(); 4];
        assert_ne!(fat.map_file(b"/README.TXT", &mut runs).unwrap().id, 0, "{what}: a file with bytes has an id");
    }
}

/// **Names are case-insensitive** for ASCII letters — a short name, a long name and a path — and
/// exact beyond ASCII. A file is found by its short name too.
#[test]
fn a_name_is_found_in_another_case() {
    let img = mformat(16, &["-c", "4"]);
    put(&img, "a long file name.txt", b"long\n");
    put(&img, "README.TXT", b"readme\n");
    put(&img, "naïve.txt", b"naive\n");
    mmd(&img, "Docs");
    put(&img, "Docs/Inner.TXT", b"inner\n");
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let mut buf = [0u8; 64];
    for (path, want) in [
        (&b"/readme.txt"[..], &b"readme\n"[..]),
        (b"/A LONG FILE NAME.TXT", b"long\n"),
        (b"/ALONGF~1.TXT", b"long\n"),
        (b"/docs/inner.txt", b"inner\n"),
        ("/NAÏVE.TXT".as_bytes(), b"naive\n"),
    ] {
        let got = fat.read_file(path, &mut buf);
        if path == "/NAÏVE.TXT".as_bytes() {
            assert_eq!(got, Err(FsError::NotFound), "Ï is not ï: exact beyond ASCII");
        } else {
            assert_eq!(&buf[..got.unwrap()], want, "{}", String::from_utf8_lossy(path));
        }
    }
}

/// **A long name whose checksum does not match its short entry is stale**, left by a system that
/// renamed the file without knowing long names: the short name stands alone.
#[test]
fn a_long_name_with_another_short_names_checksum_is_ignored() {
    let img = mformat(16, &["-c", "4"]);
    put(&img, "a long file name.txt", b"long\n");
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let g = *fat.geometry().unwrap();
    let f = dir::lookup(&disk, &g, &mut fat.cache.borrow_mut(), Dir::root(&g), b"a long file name.txt").unwrap();
    assert_eq!(&f.short, b"ALONGF~1TXT");
    let at = dir::slot_byte(&disk, &g, &mut fat.cache.borrow_mut(), f.dir, f.slot).unwrap().unwrap();
    disk.write_at(at + 7, b"2").unwrap();
    let names: Vec<String> = listing(&fat, b"/", 64).into_iter().map(|(n, ..)| n).collect();
    assert_eq!(names, ["ALONGF~2.TXT"]);
    let mut buf = [0u8; 16];
    assert_eq!(fat.read_file(b"/a long file name.txt", &mut buf), Err(FsError::NotFound));
    assert_eq!(fat.read_file(b"/ALONGF~2.TXT", &mut buf), Ok(5));
}

/// **A file in many fragments is mapped run by run**, and one in more than a reply holds is
/// `TooLarge`. mtools fills the holes left by deleting every other one-cluster file.
#[test]
fn a_fragmented_file_is_mapped_run_by_run_and_too_many_runs_are_refused() {
    let img = mformat(64, &["-c", "8"]);
    let names: Vec<String> = (0..300).map(|i| format!("f{i:03}")).collect();
    let host = crate::test_support::scratch("many");
    std::fs::create_dir_all(&host).unwrap();
    let mut srcs = Vec::new();
    for n in &names {
        let p = host.join(n);
        std::fs::write(&p, pattern(4096, 3)).unwrap();
        srcs.push(p.to_str().unwrap().to_string());
    }
    let mut args: Vec<&str> = srcs.iter().map(String::as_str).collect();
    args.push("::");
    crate::test_support::mtools("mcopy", &img, &args);
    let odd: Vec<String> = names.iter().skip(1).step_by(2).map(|n| format!("::{n}")).collect();
    let args: Vec<&str> = odd.iter().map(String::as_str).collect();
    crate::test_support::mtools("mdel", &img, &args);
    let _ = std::fs::remove_dir_all(&host);

    let frag = pattern(40 * 4096, 4);
    put(&img, "frag.bin", &frag);
    // A copy for the second half: `FileImage` removes its image when dropped.
    let again = img.with_extension("again");
    std::fs::copy(&img, &again).unwrap();
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let (bytes, runs) = through_map(&fat, b"/frag.bin");
    assert!(runs >= 30, "the fixture fragmented the file: {runs} runs");
    assert_eq!(bytes, frag);
    put(&again, "huge.bin", &pattern(100 * 4096, 5));
    let disk = FileImage::open(&again);
    let fat = Fat::new(&disk);
    let mut runs = [BlockRun::default(); 64];
    assert_eq!(fat.map_file(b"/huge.bin", &mut runs), Err(FsError::TooLarge));
}

/// **The type, both ways a FAT32 can be one**: 300 MiB of 4 KiB clusters is FAT32 by count, and
/// 64 MiB of them is FAT32 by its boot sector alone — what `mformat -F` and `mkfs.fat -F 32`
/// write. Either reads back.
#[test]
fn a_fat32_by_count_and_one_by_its_boot_sector_alone_both_read() {
    for (mib, by_count) in [(300, true), (64, false)] {
        let img = mformat(mib, &["-F", "-c", "8"]);
        let bytes = pattern(70_000, 6);
        put(&img, "data.bin", &bytes);
        let disk = FileImage::open(&img);
        let fat = Fat::new(&disk);
        let g = fat.geometry().unwrap();
        assert_eq!(g.kind, crate::bpb::Kind::Fat32);
        assert_eq!(g.clusters >= 65_525, by_count, "{mib} MiB: {} clusters", g.clusters);
        assert_eq!(through_map(&fat, b"/data.bin").0, bytes, "{mib} MiB");
    }
}

/// **What the server's check refuses**: clusters under a page, what is not FAT, and a volume larger
/// than its device; and a page-sized cluster taken.
#[test]
fn the_check_refuses_small_clusters_no_fat_and_a_volume_past_its_device() {
    let small = mformat(16, &["-c", "4"]);
    let disk = FileImage::open(&small);
    assert_eq!(Fat::new(&disk).check(), Err(Unservable::SmallClusters { bytes: 2048 }));
    let ok = mformat(64, &["-c", "8"]);
    let disk = FileImage::open(&ok);
    assert_eq!(Fat::new(&disk).check(), Ok(()));
    drop(disk);
    let blank = crate::test_support::blank(16);
    let disk = FileImage::open(&blank);
    assert_eq!(Fat::new(&disk).check(), Err(Unservable::NotFat));
    let cut = mformat(64, &["-c", "8"]);
    std::fs::OpenOptions::new().write(true).open(&cut).unwrap().set_len(32 << 20).unwrap();
    let disk = FileImage::open(&cut);
    assert_eq!(Fat::new(&disk).check(), Err(Unservable::Truncated));
}

/// **A clean filesystem reads clean**, and one whose state byte says mounted does not.
#[test]
fn the_state_byte_says_how_it_was_left() {
    let img = mformat(64, &["-F", "-c", "8"]);
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    assert_eq!(fat.was_left_clean(), Ok(true));
    disk.write_at(0x41, &[1]).unwrap();
    assert_eq!(fat.was_left_clean(), Ok(false));
}

/// **On FAT12 and FAT16 an entry's high cluster word is not a cluster**: OS/2 kept its
/// extended-attribute index there, so a file whose entry has one still reads from its low word.
#[test]
fn fat16_takes_no_cluster_from_the_high_word() {
    let img = mformat(16, &["-c", "4"]);
    put(&img, "OS2.TXT", b"extended\n");
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let g = *fat.geometry().unwrap();
    let f = dir::lookup(&disk, &g, &mut fat.cache.borrow_mut(), Dir::root(&g), b"OS2.TXT").unwrap();
    let at = dir::slot_byte(&disk, &g, &mut fat.cache.borrow_mut(), f.dir, f.slot).unwrap().unwrap();
    disk.write_at(at + 20, &[0x34, 0x12]).unwrap();
    let mut buf = [0u8; 16];
    assert_eq!(fat.read_file(b"/OS2.TXT", &mut buf), Ok(9));
    assert_eq!(&buf[..9], b"extended\n");
}

/// Write `value` into FAT16 entry `c` of every copy, as a corrupting system might.
fn poke16(disk: &FileImage, g: &crate::bpb::Geometry, c: u32, value: u16) {
    for n in 0..g.fats {
        disk.write_at(g.fat_byte(n) + c as u64 * 2, &value.to_le_bytes()).unwrap();
    }
}

/// **A chain is anyone's bytes**: a directory whose chain loops back on itself is `Corrupt` rather
/// than a listing that never ends — the walk is bounded by the volume's clusters — and a file whose
/// chain names a cluster outside the volume is `Corrupt` too.
#[test]
fn a_chain_that_loops_or_leaves_the_volume_is_corrupt() {
    let img = mformat(16, &["-c", "4"]);
    mmd(&img, "loop");
    // Seventy entries fill the directory's first 2 KiB cluster, so no end-of-directory entry stops
    // the walk before it follows the chain.
    let host = crate::test_support::scratch("seventy");
    std::fs::create_dir_all(&host).unwrap();
    let srcs: Vec<String> = (0..70)
        .map(|i| {
            let p = host.join(format!("e{i:02}"));
            std::fs::write(&p, b"x").unwrap();
            p.to_str().unwrap().to_string()
        })
        .collect();
    let mut args: Vec<&str> = srcs.iter().map(String::as_str).collect();
    args.push("::loop");
    crate::test_support::mtools("mcopy", &img, &args);
    let _ = std::fs::remove_dir_all(&host);
    put(&img, "far.bin", &pattern(3 * 2048, 7));
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let g = *fat.geometry().unwrap();
    let d = dir::lookup(&disk, &g, &mut fat.cache.borrow_mut(), Dir::root(&g), b"loop").unwrap();
    poke16(&disk, &g, d.cluster, d.cluster as u16);
    fat.cache.borrow_mut().clear();
    let id = fat.resolve_dir(b"/loop").unwrap();
    let mut emitted = 0;
    let r = fat.read_dir(id, 0, |_| {
        emitted += 1;
        true
    });
    assert_eq!(r, Err(FsError::Corrupt), "a looping directory, after {emitted} entries");

    let f = dir::lookup(&disk, &g, &mut fat.cache.borrow_mut(), Dir::root(&g), b"far.bin").unwrap();
    poke16(&disk, &g, f.cluster, 0xFFF0);
    fat.cache.borrow_mut().clear();
    let mut buf = vec![0u8; 8192];
    assert_eq!(fat.read_file(b"/far.bin", &mut buf), Err(FsError::Corrupt), "a cluster past the volume");
    let mut runs = [BlockRun::default(); 8];
    assert_eq!(fat.map_file(b"/far.bin", &mut runs), Err(FsError::Corrupt));
}
