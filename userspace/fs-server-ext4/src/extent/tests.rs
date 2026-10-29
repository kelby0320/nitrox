use super::*;
use crate::BlockRun;
use crate::ext4::{self, inode_offset, read_superblock, resolve_path_ino};
use crate::test_support::{Counting, Failing, ImageReader, RwImage, TEST_NOW, assert_e2fsck_clean, fixture_blocks};
use std::cell::RefCell;

fn depth(rw: &RwImage, path: &[u8]) -> u16 {
    let sb = read_superblock(rw).unwrap();
    let (_, inode) = resolve_path_ino(rw, &sb, path).unwrap();
    header(&inode[ROOT], ROOT_ENTRIES).unwrap().depth
}

fn free_blocks(rw: &RwImage) -> u32 {
    rd_u32(&rw.0.borrow()[1024..2048], 12)
}

fn free_inodes(rw: &RwImage) -> u32 {
    rd_u32(&rw.0.borrow()[1024..2048], 16)
}

/// Runs `(logical, device, length)` with each that continues the one before merged in.
fn merged(runs: impl IntoIterator<Item = (u64, u64, u64)>) -> Vec<(u64, u64, u64)> {
    let mut out: Vec<(u64, u64, u64)> = Vec::new();
    for (l, p, n) in runs {
        match out.last_mut() {
            Some(last) if last.0 + last.2 == l && last.1 + last.2 == p => last.2 += n,
            _ => out.push((l, p, n)),
        }
    }
    out
}

fn ours<R: BlockReader>(r: &R, path: &[u8]) -> Vec<(u64, u64, u64)> {
    let mut runs = vec![BlockRun::default(); 8192];
    let m = ext4::map_file(r, path, &mut runs).unwrap();
    merged(runs[..m.runs].iter().filter(|r| r.device_lba != 0).map(|r| (r.file_block, r.device_lba, r.length as u64)))
}

/// **`debugfs`' reading of `path`'s extents in `img`** — `(logical, device, length,
/// unwritten)` from its leaf rows: the oracle this tree is held to.
fn debugfs_extents(img: &[u8], path: &str) -> Vec<(u64, u64, u64, bool)> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let f = std::env::temp_dir().join(format!(
        "nitrox-extent-{}-{}.img",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&f, img).unwrap();
    let out = std::process::Command::new("debugfs")
        .args(["-R", &format!("ex {path}")])
        .arg(&f)
        .output()
        .expect("debugfs (e2fsprogs) must be installed to run fs-server-ext4 tests");
    let _ = std::fs::remove_file(&f);
    let mut found = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines().skip(1) {
        let row = line.replace('/', " ");
        let t: Vec<&str> = row.split_whitespace().collect();
        if t.len() < 11 || t[0] != t[1] {
            continue; // an index row
        }
        let n = |i: usize| t[i].parse::<u64>().unwrap();
        found.push((n(4), n(7), n(10), line.contains("Uninit")));
    }
    found
}

fn theirs(img: &[u8], path: &str) -> Vec<(u64, u64, u64)> {
    merged(debugfs_extents(img, path).into_iter().filter(|e| !e.3).map(|e| (e.0, e.1, e.2)))
}

/// This server's map of `path` is `debugfs`', and `e2fsck` finds nothing.
fn agrees(rw: &RwImage, path: &str, tag: &str) -> Vec<(u64, u64, u64)> {
    let img = rw.0.borrow().clone();
    let map = ours(rw, path.as_bytes());
    assert_eq!(map, theirs(&img, path), "{tag}: this server's map is not debugfs'");
    assert_e2fsck_clean(&img, tag);
    map
}

fn blocks_in(map: &[(u64, u64, u64)]) -> u64 {
    map.iter().map(|r| r.2).sum()
}

/// Remove `name` from directory `dir`, both halves: the name, then the inode.
fn remove(rw: &RwImage, dir: &[u8], name: &[u8]) {
    let d = ext4::resolve_dir(rw, dir).unwrap();
    if let Some(ino) = ext4::unlink_at(rw, d, name, TEST_NOW).unwrap() {
        ext4::release_inode(rw, ino, TEST_NOW).unwrap();
    }
}

/// **4,096 files of 16 KiB in one directory**, created, listed, looked up and removed, `e2fsck`
/// clean after each half; the inode's four extents ran out at about the 815th file.
#[test]
fn four_thousand_files_with_data_fit_in_one_directory() {
    let rw = RwImage(RefCell::new(fixture_blocks(4096, 20480, 0, b"x\n")));
    let name = |i: usize| format!("f{i:04}.bin");
    for i in 0..4096 {
        ext4::create_file(&rw, b"/system", name(i).as_bytes(), TEST_NOW)
            .unwrap_or_else(|e| panic!("create {i}: {e:?}"));
        ext4::grow_file(&rw, format!("/system/{}", name(i)).as_bytes(), 16384, TEST_NOW)
            .unwrap_or_else(|e| panic!("grow {i}: {e:?}"));
    }
    assert!(depth(&rw, b"/system") >= 1, "a directory of 4,096 files with data is a tree");

    let sys = ext4::resolve_dir(&rw, b"/system").unwrap();
    let count = |rw: &RwImage| {
        let mut n = 0;
        ext4::read_dir(rw, sys, 0, |_, _, name| {
            n += (name.starts_with(b"f") && name.ends_with(b".bin")) as usize;
            true
        })
        .unwrap();
        n
    };
    assert_eq!(count(&rw), 4096, "every file listed");
    for i in [0, 814, 815, 816, 2048, 4095] {
        let path = format!("/system/{}", name(i));
        assert_eq!(ext4::stat_file(&rw, path.as_bytes()), Ok(16384), "{path}");
    }
    assert_e2fsck_clean(&rw.0.borrow(), "small-made");

    for i in 0..4096 {
        remove(&rw, b"/system", name(i).as_bytes());
    }
    assert_eq!(count(&rw), 0, "every file removed");
    assert_e2fsck_clean(&rw.0.borrow(), "small-removed");
}

/// **Two files grown a block at a time in turn**, so every block of each is an extent of its
/// own: past a full depth 1 into depth 2, at both block sizes, with the map agreeing with
/// `debugfs`' and `e2fsck` clean at every step (grown, cut inside each level, regrown, emptied,
/// removed).
#[test]
fn files_grown_in_turn_build_a_tree_two_levels_deep_and_cut_back() {
    for (bs, blocks, fs_blocks) in [(1024u32, 400u64, 8192u32), (4096, 1400, 8192)] {
        let rw = RwImage(RefCell::new(fixture_blocks(bs, fs_blocks, 0, b"x\n")));
        let tag = |step: &str| format!("turns-{bs}-{step}");
        let (blocks_before, inodes_before) = (free_blocks(&rw), free_inodes(&rw));
        for f in [&b"a"[..], b"b"] {
            ext4::create_file(&rw, b"/", f, TEST_NOW).unwrap();
        }
        for i in 1..=blocks {
            for f in [&b"/a"[..], b"/b"] {
                ext4::grow_file(&rw, f, (i * bs as u64) as usize, TEST_NOW)
                    .unwrap_or_else(|e| panic!("{bs}: growing to {i} blocks: {e:?}"));
            }
        }
        assert_eq!(depth(&rw, b"/a"), 2, "{bs}: {blocks} extents need two levels");
        let map = agrees(&rw, "/a", &tag("grown"));
        assert_eq!((map.len() as u64, blocks_in(&map)), (blocks, blocks), "{bs}: a block per extent");
        agrees(&rw, "/b", &tag("grown-b"));

        // Inside depth 1: three leaves, so the inode folds back to holding their index.
        ext4::truncate_file(&rw, b"/a", (blocks / 2 * bs as u64) as usize, TEST_NOW).unwrap();
        assert_eq!(depth(&rw, b"/a"), 1, "{bs}: cut to three leaves");
        assert_eq!(blocks_in(&agrees(&rw, "/a", &tag("half"))), blocks / 2);
        ext4::truncate_file(&rw, b"/a", 3 * bs as usize, TEST_NOW).unwrap();
        assert_eq!(depth(&rw, b"/a"), 0, "{bs}: cut to three extents");
        assert_eq!(blocks_in(&agrees(&rw, "/a", &tag("three"))), 3);
        ext4::grow_file(&rw, b"/a", (blocks * bs as u64) as usize, TEST_NOW).unwrap();
        assert_eq!(blocks_in(&agrees(&rw, "/a", &tag("regrown"))), blocks);
        ext4::truncate_file(&rw, b"/a", 0, TEST_NOW).unwrap();
        assert_eq!((depth(&rw, b"/a"), blocks_in(&agrees(&rw, "/a", &tag("empty")))), (0, 0));

        remove(&rw, b"/", b"a");
        remove(&rw, b"/", b"b");
        assert_eq!((free_blocks(&rw), free_inodes(&rw)), (blocks_before, inodes_before), "{bs}");
        assert_e2fsck_clean(&rw.0.borrow(), &tag("removed"));
    }
}

/// **A tree e2fsprogs wrote**: `mke2fs -d` writes a file whose every other block is sparse, an
/// extent per data block, 450 of them two levels deep at 1 KiB. Read whole, mapped as `debugfs`
/// maps it, cut through each level, and — on a second copy — removed whole.
#[test]
fn a_tree_e2fsprogs_wrote_is_read_cut_and_freed() {
    let mut content = vec![0u8; 900 * 1024];
    for b in (0..900).step_by(2) {
        content[b * 1024..(b + 1) * 1024].fill(0x5A);
    }
    let img = fixture_blocks(1024, 8192, 0, &content);
    let path = "/system/current-generation";

    let rw = RwImage(RefCell::new(img.clone()));
    assert_eq!(depth(&rw, path.as_bytes()), 2, "e2fsprogs wrote 450 extents two levels deep");
    let mut out = vec![0x11u8; content.len()];
    assert_eq!(ext4::read_file_range(&rw, path.as_bytes(), 0, out.len(), &mut out), Ok(out.len()));
    assert!(out == content, "the file reads as written, holes as zeroes");
    assert_eq!(blocks_in(&agrees(&rw, path, "e2fs-read")), 450);
    // Its leaves hold 83 each. Inside depth 2, then inside depth 1, then inside the inode.
    for (keep, want_depth) in [(801u64, 2u16), (600, 1), (5, 0), (0, 0)] {
        ext4::truncate_file(&rw, path.as_bytes(), (keep * 1024) as usize, TEST_NOW).unwrap();
        assert_eq!(depth(&rw, path.as_bytes()), want_depth, "cut to {keep} blocks");
        assert_eq!(blocks_in(&agrees(&rw, path, &format!("e2fs-cut-{keep}"))), keep.div_ceil(2));
    }

    let rw = RwImage(RefCell::new(img));
    let (blocks_before, inodes_before) = (free_blocks(&rw), free_inodes(&rw));
    remove(&rw, b"/system", b"current-generation");
    assert_eq!(free_blocks(&rw), blocks_before + 457);
    assert_eq!(free_inodes(&rw), inodes_before + 1);
    assert_e2fsck_clean(&rw.0.borrow(), "e2fs-removed");
}

/// Patch the extent of the fixture's one-block file to `ee_len` and its size to `size`,
/// returning the image and the block the extent starts at.
fn patched(ee_len: u16, size: u64) -> (Vec<u8>, u64) {
    let mut img = fixture_blocks(4096, 40000, 0, &[0xAB; 4096]);
    let r = ImageReader(img.clone());
    let sb = read_superblock(&r).unwrap();
    let (ino, inode) = resolve_path_ino(&r, &sb, b"/system/current-generation").unwrap();
    let x = read_extent(&inode[ROOT], 0).unwrap();
    assert_eq!((rd_u16(&inode, 42), x.lblock, x.len), (1, 0, 1), "one extent of one block");
    let at = inode_offset(&r, &sb, ino).unwrap() as usize;
    img[at + 4..at + 8].copy_from_slice(&(size as u32).to_le_bytes()); // i_size_lo
    img[at + 40 + 16..at + 40 + 18].copy_from_slice(&ee_len.to_le_bytes()); // ee_len
    (img, x.phys)
}

/// **`ee_len` as ext4 defines it**, read against `debugfs` on bytes this server did not write:
/// `0x8000` is a written extent of 32,768 blocks; `0x8001` is an unwritten extent of one
/// block, which reads as zeroes and maps as a hole.
#[test]
fn a_full_length_extent_and_an_unwritten_one_read_as_e2fsprogs_reads_them() {
    let path = "/system/current-generation";
    let (img, p) = patched(0x8000, MAX_LEN * 4096);
    assert_eq!(debugfs_extents(&img, path), vec![(0, p, MAX_LEN, false)], "e2fsprogs' reading");
    let r = ImageReader(img);
    assert_eq!(ours(&r, path.as_bytes()), vec![(0, p, MAX_LEN)]);
    let mut block = [0u8; 4096];
    assert_eq!(ext4::read_file_range(&r, path.as_bytes(), 0, 4096, &mut block), Ok(4096));
    assert_eq!(block, [0xAB; 4096], "the extent's first block, not a hole");

    let (img, p) = patched(0x8001, 4096);
    assert_eq!(debugfs_extents(&img, path), vec![(0, p, 1, true)], "e2fsprogs' reading");
    let r = ImageReader(img);
    let mut runs = [BlockRun::default(); 4];
    let m = ext4::map_file(&r, path.as_bytes(), &mut runs).unwrap();
    assert_eq!((m.runs, runs[0].device_lba), (1, 0), "an unwritten extent maps as a hole");
    assert_eq!(ext4::read_file_range(&r, path.as_bytes(), 0, 4096, &mut block), Ok(4096));
    assert_eq!(block, [0; 4096], "and reads as zeroes, not as the 0xAB its block holds");
}

/// **The writer's half of the same rule**: an extent grows by appends to 32,768 blocks, stored
/// as `0x8000`, and the next block starts another.
#[test]
fn an_extent_grows_to_32768_blocks_and_the_next_block_starts_another() {
    let rw = RwImage(RefCell::new(fixture_blocks(1024, 40000, 0, b"x\n")));
    let sb = read_superblock(&rw).unwrap();
    let mut inode = [0u8; 256];
    empty_root(&mut inode);
    let p = 2000;
    append(&rw, &sb, &mut inode, Extent::written(0, p, MAX_LEN - 1)).unwrap();
    append(&rw, &sb, &mut inode, Extent::written(MAX_LEN - 1, p + MAX_LEN - 1, 1)).unwrap();
    assert_eq!(rd_u16(&inode, 42), 1, "merged into one extent");
    assert_eq!(rd_u16(&inode, 56), 0x8000, "32,768 written blocks are stored as 0x8000");
    append(&rw, &sb, &mut inode, Extent::written(MAX_LEN, p + MAX_LEN, 1)).unwrap();
    assert_eq!(rd_u16(&inode, 42), 2, "one block past the longest is another extent");

    let mut map = Lookup::new(&inode);
    assert_eq!(map.seg(&rw, &sb, 0).unwrap(), Seg { phys: p, len: MAX_LEN });
    assert_eq!(map.block(&rw, &sb, MAX_LEN - 1).unwrap(), p + MAX_LEN - 1);
    assert_eq!(map.block(&rw, &sb, MAX_LEN).unwrap(), p + MAX_LEN);
    assert_eq!(rd_u32(&inode, 28) as u64, (MAX_LEN + 1) * 2, "i_blocks, in 512-byte units");
}

/// **Past the last block an extent can address, the rest is one hole**: a file whose size runs
/// past 2^32 blocks is mapped there in a step, where a block at a time took six minutes.
#[test]
fn past_the_last_block_an_extent_can_address_the_rest_is_one_hole() {
    let rw = RwImage(RefCell::new(fixture_blocks(1024, 4096, 0, b"x\n")));
    let sb = read_superblock(&rw).unwrap();
    let mut inode = [0u8; 256];
    empty_root(&mut inode);
    let mut map = Lookup::new(&inode);
    assert_eq!(map.seg(&rw, &sb, END).unwrap(), Seg { phys: 0, len: u64::MAX - END });
    assert_eq!(map.seg(&rw, &sb, END - 1).unwrap(), Seg { phys: 0, len: 1 }, "the tree's own hole ends there");
}

/// **A grow the filesystem cannot hold gives back everything it took**: on a file with no
/// blocks, one whose four extents fill its inode (the grow moved them into a leaf before it
/// failed, and the undo must fold them back), and one already a tree.
#[test]
fn a_grow_the_filesystem_cannot_hold_gives_back_everything_it_took() {
    let rw = RwImage(RefCell::new(fixture_blocks(4096, 4096, 0, b"x\n")));
    ext4::create_file(&rw, b"/", b"big", TEST_NOW).unwrap();
    let before = free_blocks(&rw);
    assert_eq!(ext4::grow_file(&rw, b"/big", 32 << 20, TEST_NOW), Err(FsError::TooLarge));
    assert_eq!(free_blocks(&rw), before, "every block the grow took went back");
    assert_eq!(ext4::stat_file(&rw, b"/big"), Ok(0));
    assert_e2fsck_clean(&rw.0.borrow(), "grow-refused-empty");

    for (extents, want_depth) in [(4u64, 0u16), (12, 1)] {
        let rw = RwImage(RefCell::new(fixture_blocks(4096, 4096, 0, b"x\n")));
        for f in [&b"a"[..], b"b"] {
            ext4::create_file(&rw, b"/", f, TEST_NOW).unwrap();
        }
        for i in 1..=extents as usize {
            for f in [&b"/a"[..], b"/b"] {
                ext4::grow_file(&rw, f, i * 4096, TEST_NOW).unwrap();
            }
        }
        assert_eq!(depth(&rw, b"/a"), want_depth);
        let map = agrees(&rw, "/a", &format!("grow-refused-{extents}-before"));
        let before = free_blocks(&rw);
        assert_eq!(ext4::grow_file(&rw, b"/a", 32 << 20, TEST_NOW), Err(FsError::TooLarge));
        assert_eq!(free_blocks(&rw), before, "{extents} extents: every block went back");
        assert_eq!(depth(&rw, b"/a"), want_depth, "{extents} extents: the tree's shape is back");
        assert_eq!(agrees(&rw, "/a", &format!("grow-refused-{extents}")), map);
        assert_eq!(ext4::stat_file(&rw, b"/a"), Ok(extents as usize * 4096));
    }
}

/// **A grow whose last append is lost gives that run back**: the fifth extent of a full inode
/// goes into a new leaf, and losing that write must leave no run in use that nothing maps.
#[test]
fn a_grow_whose_last_append_fails_gives_its_run_back() {
    let rw = RwImage(RefCell::new(fixture_blocks(4096, 4096, 0, b"x\n")));
    for f in [&b"a"[..], b"b"] {
        ext4::create_file(&rw, b"/", f, TEST_NOW).unwrap();
    }
    for i in 1..=4 {
        for f in [&b"/a"[..], b"/b"] {
            ext4::grow_file(&rw, f, i * 4096, TEST_NOW).unwrap();
        }
    }
    let img = rw.0.borrow().clone();
    let before = free_blocks(&rw);
    for n in [4, 7, 8] {
        let failing = Failing::once(img.clone(), n);
        assert_eq!(ext4::grow_file(&failing, b"/a", 5 * 4096, TEST_NOW), Err(FsError::Io), "write {n}");
        assert_eq!(free_blocks(&failing.image), before, "write {n}: the run went back");
        assert_e2fsck_clean(&failing.image.0.borrow(), &format!("grow-last-append-{n}"));
    }
}

/// **Blocks past a file's end go before a grow appends**: an extent that runs past `i_size`
/// (`fallocate` with the size kept) would otherwise sit under the appended blocks, and the tree
/// would map one logical block twice.
#[test]
fn blocks_past_the_end_go_before_a_grow() {
    let path = "/system/current-generation";
    let rw = RwImage(RefCell::new(fixture_blocks(4096, 4096, 0, b"x\n")));
    ext4::grow_file(&rw, path.as_bytes(), 3 * 4096, TEST_NOW).unwrap();
    let sb = read_superblock(&rw).unwrap();
    let (ino, _) = resolve_path_ino(&rw, &sb, path.as_bytes()).unwrap();
    let at = inode_offset(&rw, &sb, ino).unwrap();
    BlockWriter::write_at(&rw, at + 4, &4096u32.to_le_bytes()).unwrap();
    let before = free_blocks(&rw);

    ext4::grow_file(&rw, path.as_bytes(), 5 * 4096, TEST_NOW).unwrap();
    let map = agrees(&rw, path, "past-the-end");
    assert_eq!(blocks_in(&map), 5);
    assert_eq!(free_blocks(&rw), before + 2 - 4);
}

/// **A grow that fails after cutting the blocks past the end leaves the filesystem clean**: the
/// undo writes the inode, else it keeps the `i_blocks` of the blocks the undo gave back.
#[test]
fn a_grow_that_fails_after_cutting_past_the_end_leaves_the_filesystem_clean() {
    let path = "/system/current-generation";
    let rw = RwImage(RefCell::new(fixture_blocks(4096, 4096, 0, b"x\n")));
    ext4::grow_file(&rw, path.as_bytes(), 3 * 4096, TEST_NOW).unwrap();
    let sb = read_superblock(&rw).unwrap();
    let (ino, _) = resolve_path_ino(&rw, &sb, path.as_bytes()).unwrap();
    let at = inode_offset(&rw, &sb, ino).unwrap();
    BlockWriter::write_at(&rw, at + 4, &4096u32.to_le_bytes()).unwrap();
    let before = free_blocks(&rw);

    assert_eq!(ext4::grow_file(&rw, path.as_bytes(), 32 << 20, TEST_NOW), Err(FsError::TooLarge));
    assert_eq!(free_blocks(&rw), before + 2, "the two past the end went back, and all it took");
    assert_eq!(blocks_in(&agrees(&rw, path, "past-the-end-refused")), 1);
    assert_e2fsck_clean(&rw.0.borrow(), "past-the-end-refused");
}

/// **An append that loses a write gives back the tree blocks it took**: a fifth extent on a
/// full inode moves them into a new leaf, and a write lost from the leaf's own on fails the
/// append.
#[test]
fn an_append_that_loses_a_write_gives_back_the_tree_blocks_it_took() {
    let (img, inode, was) = interleaved(4);
    let sb = read_superblock(&ImageReader(img.clone())).unwrap();
    let x = Extent::written(4, was[3] + 2, 1);
    let all = Failing::after(img.clone(), u64::MAX);
    let mut grown = inode;
    append(&all, &sb, &mut grown, x).unwrap();
    assert_eq!(header(&grown[ROOT], ROOT_ENTRIES).unwrap().depth, 1, "the four moved below the inode");
    let before = free_blocks(&RwImage(RefCell::new(img.clone())));
    for n in 3..all.writes.get() {
        let rw = Failing::once(img.clone(), n);
        let mut after = inode;
        assert_eq!(append(&rw, &sb, &mut after, x), Err(FsError::Io), "write {n}");
        assert_eq!(after, inode, "write {n}: the inode as it was");
        assert_eq!(free_blocks(&rw.image), before, "write {n}: the leaf given back");
        assert_e2fsck_clean(&rw.image.0.borrow(), &format!("append-lost-{n}"));
    }
}

/// **What ext4 frees leaves the metadata cache**: a freed block may next be a file's data,
/// which the kernel writes behind the server. Nothing reads such a block back today, so without
/// `forget` every other test passes. In 4 KiB blocks, the cache's own.
#[test]
fn what_ext4_frees_leaves_the_metadata_cache() {
    use libfsserver::cache::{MetaCache, Store};
    let rw = RwImage(RefCell::new(fixture_blocks(4096, 4096, 0, b"x\n")));
    let mut store = Box::new(Store::<64>::new());
    let c = MetaCache::new(&rw, &mut store);
    let sb = read_superblock(&c).unwrap();
    let root = ext4::resolve_dir(&c, b"/").unwrap();
    for f in [&b"a"[..], b"b"] {
        ext4::create_file(&c, b"/", f, TEST_NOW).unwrap();
    }
    for i in 1..=6 {
        for f in [&b"/a"[..], b"/b"] {
            ext4::grow_file(&c, f, i * 4096, TEST_NOW).unwrap();
        }
    }
    ext4::mkdir_at(&c, root, b"d", TEST_NOW).unwrap();
    let name = |i: usize| format!("a name long enough that a hundred fill a block {i:03}");
    for i in 0..200 {
        ext4::create_file(&c, b"/d", name(i).as_bytes(), TEST_NOW).unwrap();
    }
    let d = ext4::resolve_dir(&c, b"/d").unwrap();
    for i in 0..200 {
        let ino = ext4::unlink_at(&c, d, name(i).as_bytes(), TEST_NOW).unwrap().unwrap();
        ext4::release_inode(&c, ino, TEST_NOW).unwrap();
    }
    let (a, inode) = resolve_path_ino(&c, &sb, b"/a").unwrap();
    let tree = tree_blocks(&c, &sb, &inode);
    let (_, inode) = resolve_path_ino(&c, &sb, b"/d").unwrap();
    let mut map = Lookup::new(&inode);
    let dir: Vec<u64> = (0..3).map(|lb| map.block(&c, &sb, lb).unwrap()).collect();
    ext4::read_dir(&c, d, 0, |_, _, _| true).unwrap();
    let freed: Vec<u64> = tree.iter().chain(&dir).copied().collect();
    assert_eq!((tree.len(), dir.iter().all(|&b| b != 0)), (1, true), "a leaf, and three directory blocks");
    assert!(freed.iter().all(|&b| c.holds(b * 4096)), "each read through the cache, so held");

    assert_eq!(ext4::unlink_at(&c, root, b"a", TEST_NOW), Ok(Some(a)));
    ext4::release_inode(&c, a, TEST_NOW).unwrap();
    ext4::rmdir_at(&c, root, b"d", TEST_NOW, |_| ()).unwrap();
    for b in freed {
        assert!(!c.holds(b * 4096), "block {b} is free and still held");
    }
}

/// **A full index below the inode splits too**, and the inode takes the new index beside the
/// old. Built with nodes of two entries (otherwise 7,056 extents away); `e2fsck` and `debugfs`
/// refuse a node that small, so it is read back through [`Lookup`], and cut back.
#[test]
fn a_full_index_below_the_inode_splits_too() {
    let rw = RwImage(RefCell::new(fixture_blocks(1024, 8192, 0, b"x\n")));
    let sb = read_superblock(&rw).unwrap();
    let initial = free_blocks(&rw);
    let take = || ext4::alloc_block(&rw, &sb, sb.first_data_block as u64).unwrap();
    let data: Vec<u64> = (0..5).map(|_| take()).collect();
    let (l1, l2, index) = (take(), take(), take());
    let mut node = [0u8; 1024];
    for (blk, from) in [(l1, 0usize), (l2, 2)] {
        node.fill(0);
        set_header(&mut node, 2, 2, 0);
        for i in 0..2 {
            write_extent(&mut node, i, Extent::written((from + i) as u64 * 2, data[from + i], 1));
        }
        BlockWriter::write_at(&rw, blk * 1024, &node).unwrap();
    }
    node.fill(0);
    set_header(&mut node, 2, 2, 1);
    write_index(&mut node, 0, 0, l1);
    write_index(&mut node, 1, 4, l2);
    BlockWriter::write_at(&rw, index * 1024, &node).unwrap();
    let mut inode = [0u8; 256];
    set_header(&mut inode[ROOT], 1, ROOT_ENTRIES, 2);
    write_index(&mut inode[ROOT], 0, 0, index);

    let before = free_blocks(&rw);
    append(&rw, &sb, &mut inode, Extent::written(8, data[4], 1)).unwrap();
    assert_eq!(free_blocks(&rw), before - 2);
    let root = header(&inode[ROOT], ROOT_ENTRIES).unwrap();
    assert_eq!((root.entries, root.depth), (2, 2));
    assert_eq!(read_index(&inode[ROOT], 1).0, 8, "the new index starts at the new extent");
    let mut map = Lookup::new(&inode);
    for (lb, &phys) in [0u64, 2, 4, 6, 8].iter().zip(&data) {
        assert_eq!(map.block(&rw, &sb, *lb).unwrap(), phys, "block {lb}");
        assert_eq!(map.block(&rw, &sb, lb + 1).unwrap(), 0, "the hole after block {lb}");
    }

    // Cut back to the four: the new leaf and index go, and the inode folds the old index in.
    truncate(&rw, &sb, &mut inode, 8, None).unwrap();
    assert_eq!(free_blocks(&rw), before + 1 + 1, "the extent's block, and the folded index");
    let root = header(&inode[ROOT], ROOT_ENTRIES).unwrap();
    assert_eq!((root.entries, root.depth), (2, 1), "the inode indexes the two leaves");
    let mut map = Lookup::new(&inode);
    assert_eq!(map.block(&rw, &sb, 6).unwrap(), data[3]);
    assert_eq!(map.block(&rw, &sb, 8).unwrap(), 0);
    truncate(&rw, &sb, &mut inode, 0, None).unwrap();
    assert_eq!(free_blocks(&rw), initial);
    assert_eq!(header(&inode[ROOT], ROOT_ENTRIES).unwrap().entries, 0);
}

fn tree_blocks<R: BlockReader>(r: &R, sb: &Superblock, inode: &[u8; 256]) -> Vec<u64> {
    fn walk<R: BlockReader>(r: &R, sb: &Superblock, node: &[u8], h: Header, hi: u64, out: &mut Vec<u64>) {
        for i in (0..h.entries).filter(|_| h.depth > 0) {
            let child = read_index(node, i).1;
            out.push(child);
            let keys = child_keys(node, h, i, hi);
            let mut buf = [0u8; MAX_BLOCK];
            let ch = read_node(r, sb, child, h.depth - 1, keys, &mut buf).unwrap();
            walk(r, sb, &buf, ch, keys.1, out);
        }
    }
    let mut out = Vec::new();
    walk(r, sb, &inode[ROOT], header(&inode[ROOT], ROOT_ENTRIES).unwrap(), END, &mut out);
    out
}

/// **What a cut stopped by an error must leave**: `inode`'s tree reads at every level, maps
/// each block of `was` as it did or, from `keep` on, not at all, and is in use wherever it
/// maps.
fn left_whole<R: BlockReader>(r: &R, sb: &Superblock, inode: &[u8; 256], was: &[u64], keep: u64, tag: &str) {
    let mut map = Lookup::new(inode);
    for (lb, &before) in (0u64..).zip(was) {
        let now = map.block(r, sb, lb).unwrap_or_else(|e| panic!("{tag}: block {lb} reads {e:?}"));
        assert!(now == before || (now == 0 && lb >= keep), "{tag}: block {lb} maps {now}, was {before}");
        assert!(now == 0 || ext4::block_in_use(r, sb, now), "{tag}: block {lb} maps {now}, which is free");
    }
    for blk in tree_blocks(r, sb, inode) {
        assert!(ext4::block_in_use(r, sb, blk), "{tag}: tree block {blk} is free");
    }
}

/// An image whose `/a` and `/b` were grown a block at a time in turn, to `blocks` one-block
/// extents each (1 KiB blocks), with `/a`'s inode and map.
fn interleaved(blocks: u64) -> (Vec<u8>, [u8; 256], Vec<u64>) {
    let rw = RwImage(RefCell::new(fixture_blocks(1024, 4096, 0, b"x\n")));
    for f in [&b"a"[..], b"b"] {
        ext4::create_file(&rw, b"/", f, TEST_NOW).unwrap();
    }
    for i in 1..=blocks as usize {
        for f in [&b"/a"[..], b"/b"] {
            ext4::grow_file(&rw, f, i * 1024, TEST_NOW).unwrap();
        }
    }
    let sb = read_superblock(&rw).unwrap();
    let (_, inode) = resolve_path_ino(&rw, &sb, b"/a").unwrap();
    let mut map = Lookup::new(&inode);
    let was = (0..blocks).map(|lb| map.block(&rw, &sb, lb).unwrap()).collect();
    let img = rw.0.borrow().clone();
    (img, inode, was)
}

/// **A cut stopped at any write leaves a tree that maps nothing it freed**, every write from
/// that one on refused: two extents in the inode, and twelve in a leaf below it.
#[test]
fn a_cut_stopped_at_any_write_leaves_a_tree_that_maps_nothing_it_freed() {
    for (blocks, keeps, want_depth) in [(2u64, &[0u64, 1][..], 0u16), (12, &[0, 3, 5], 1)] {
        let (img, inode, was) = interleaved(blocks);
        assert_eq!(header(&inode[ROOT], ROOT_ENTRIES).unwrap().depth, want_depth);
        let sb = read_superblock(&ImageReader(img.clone())).unwrap();
        for &keep in keeps {
            let all = Failing::after(img.clone(), u64::MAX);
            truncate(&all, &sb, &mut inode.clone(), keep, None).unwrap();
            for n in 0..all.writes.get() {
                let rw = Failing::after(img.clone(), n);
                let mut cut = inode;
                let tag = format!("{blocks} extents cut to {keep}, every write from {n} refused");
                assert_eq!(truncate(&rw, &sb, &mut cut, keep, None), Err(FsError::Io), "{tag}");
                left_whole(&rw, &sb, &cut, &was, keep, &tag);
            }
        }
    }
}

/// **And the inode on the disk agrees**, every write from one on refused or that one alone
/// lost: a truncate writes the inode before freeing what its root drops, a release frees the
/// inode before its blocks.
#[test]
fn a_truncate_or_a_release_stopped_at_any_write_leaves_no_inode_mapping_what_it_freed() {
    type Model = (&'static str, fn(Vec<u8>, u64) -> Failing);
    let models: [Model; 2] = [("refused from", Failing::after), ("lost", Failing::once)];
    // Four extents in the inode; twelve in a leaf, cut inside it and to three (folded).
    for (blocks, keeps) in [(4u64, &[1u64][..]), (12, &[5, 3])] {
        let (img, _, was) = interleaved(blocks);
        let sb = read_superblock(&ImageReader(img.clone())).unwrap();
        for &keep in keeps {
            let size = keep as usize * 1024;
            let all = Failing::after(img.clone(), u64::MAX);
            ext4::truncate_file(&all, b"/a", size, TEST_NOW).unwrap();
            for (how, failing) in models {
                for n in 0..all.writes.get() {
                    let rw = failing(img.clone(), n);
                    let tag = format!("{blocks} extents truncated to {keep}, write {n} {how}");
                    assert_eq!(ext4::truncate_file(&rw, b"/a", size, TEST_NOW), Err(FsError::Io), "{tag}");
                    left_whole(&rw, &sb, &resolve_path_ino(&rw, &sb, b"/a").unwrap().1, &was, keep, &tag);
                }
            }
        }
    }
    let (img, _, was) = interleaved(12);
    let sb = read_superblock(&ImageReader(img.clone())).unwrap();
    let (ino, _) = resolve_path_ino(&ImageReader(img.clone()), &sb, b"/a").unwrap();
    let all = Failing::after(img.clone(), u64::MAX);
    ext4::release_inode(&all, ino, TEST_NOW).unwrap();
    for (how, failing) in models {
        for n in 0..all.writes.get() {
            let rw = failing(img.clone(), n);
            let tag = format!("a release, write {n} {how}");
            assert_eq!(ext4::release_inode(&rw, ino, TEST_NOW), Err(FsError::Io), "{tag}");
            let inode = resolve_path_ino(&rw, &sb, b"/a").unwrap().1;
            // A freed inode maps nothing `e2fsck` counts; a live one must map only what is in
            // use.
            if rd_u16(&inode, 26) > 0 {
                left_whole(&rw, &sb, &inode, &was, 0, &tag);
            }
        }
    }

    let made = RwImage(RefCell::new(img));
    let root = ext4::resolve_dir(&made, b"/").unwrap();
    ext4::mkdir_at(&made, root, b"d", TEST_NOW).unwrap();
    let d = ext4::resolve_dir(&made, b"/d").unwrap();
    let was = [Lookup::new(&ext4::read_inode(&made, &sb, d).unwrap()).block(&made, &sb, 0).unwrap()];
    let img = made.0.into_inner();
    let all = Failing::after(img.clone(), u64::MAX);
    ext4::rmdir_at(&all, root, b"d", TEST_NOW, |_| ()).unwrap();
    for (how, failing) in models {
        for n in 0..all.writes.get() {
            let rw = failing(img.clone(), n);
            let tag = format!("a directory's removal, write {n} {how}");
            assert_eq!(ext4::rmdir_at(&rw, root, b"d", TEST_NOW, |_| ()).is_ok(), n > 0, "{tag}");
            let inode = ext4::read_inode(&rw, &sb, d).unwrap();
            if rd_u16(&inode, 26) > 0 {
                left_whole(&rw, &sb, &inode, &was, 0, &tag);
            }
        }
    }
}

/// `inode` with its root an index over `children`, entry `i` keyed `100 * i`.
fn rerooted(inode: &[u8; 256], children: &[u64]) -> [u8; 256] {
    let mut root = [0u8; 60];
    set_header(&mut root, children.len(), ROOT_ENTRIES, 1);
    for (i, &child) in children.iter().enumerate() {
        write_index(&mut root, i, 100 * i as u64, child);
    }
    let mut out = *inode;
    out[ROOT].copy_from_slice(&root);
    out
}

/// What cutting `inode` to nothing writes, on the tree [`interleaved`] made honestly.
fn honest_writes() -> u64 {
    let (img, mut inode, _) = interleaved(12);
    let sb = read_superblock(&ImageReader(img.clone())).unwrap();
    let rw = Counting::new(img);
    truncate(&rw, &sb, &mut inode, 0, None).unwrap();
    rw.writes.get()
}

/// **A leaf four index entries name is `Corrupt` to cut, not walked and freed four times**, and a
/// cut keeping part of it frees nothing it keeps: each entry's keys bound its child's, so a block
/// holding entries has one place in the tree.
#[test]
fn a_leaf_named_by_four_entries_is_corrupt_to_cut() {
    let (img, inode, was) = interleaved(12);
    let leaf = read_index(&inode[ROOT], 0).1;
    let shared = rerooted(&inode, &[leaf; 4]);
    let sb = read_superblock(&ImageReader(img.clone())).unwrap();
    let most = honest_writes();
    for keep in [0u64, 3] {
        let rw = Counting::new(img.clone());
        let mut cut = shared;
        assert_eq!(truncate(&rw, &sb, &mut cut, keep, None), Err(FsError::Corrupt), "cut to {keep}");
        assert!(rw.writes.get() <= most, "cut to {keep}: {} writes, the honest tree's {most}", rw.writes.get());
        left_whole(&rw.image, &sb, &cut, &was, keep, &format!("cut to {keep}"));
    }
}

/// **Nor one an index block's every entry names**, which multiplied the walk at each level.
#[test]
fn a_leaf_named_by_every_entry_of_an_index_block_is_corrupt_to_cut() {
    let (img, inode, _) = interleaved(12);
    let leaf = read_index(&inode[ROOT], 0).1;
    let rw = Counting::new(img);
    let sb = read_superblock(&rw.image).unwrap();
    let index = ext4::alloc_block(&rw.image, &sb, sb.first_data_block as u64).unwrap();
    let cap = block_cap(&sb);
    let mut node = vec![0u8; sb.block_size as usize];
    set_header(&mut node, cap, cap, 1);
    for i in 0..cap {
        write_index(&mut node, i, 100 * i as u64, leaf);
    }
    rw.image.write_at(index * sb.block_size as u64, &node).unwrap();
    let mut root = [0u8; 60];
    set_header(&mut root, 1, ROOT_ENTRIES, 2);
    write_index(&mut root, 0, 0, index);
    let mut deep = inode;
    deep[ROOT].copy_from_slice(&root);
    rw.take();
    assert_eq!(truncate(&rw, &sb, &mut deep, 0, None), Err(FsError::Corrupt));
    let most = honest_writes() + 2;
    assert!(rw.writes.get() <= most, "{} writes, against {most} for one walk", rw.writes.get());
}

/// **An empty leaf is `Corrupt` to free**, the one node keys cannot place: named twice, one entry
/// kept and one dropped, it was freed while the tree still named it.
#[test]
fn an_empty_leaf_named_twice_is_not_freed_while_named() {
    let (img, inode, _) = interleaved(12);
    let rw = RwImage(RefCell::new(img));
    let sb = read_superblock(&rw).unwrap();
    let empty = ext4::alloc_block(&rw, &sb, sb.first_data_block as u64).unwrap();
    let mut node = vec![0u8; sb.block_size as usize];
    set_header(&mut node, 0, block_cap(&sb), 0);
    rw.write_at(empty * sb.block_size as u64, &node).unwrap();
    let mut cut = rerooted(&inode, &[empty, empty]);
    assert_eq!(truncate(&rw, &sb, &mut cut, 3, None), Err(FsError::Corrupt));
    assert!(ext4::block_in_use(&rw, &sb, empty), "the leaf the root still names is in use");
}

/// **Nor one whose keys pass every entry's lower bound**: moved to 300, the leaf is past the first
/// three entries' ranges only at their tops.
#[test]
fn a_leaf_named_below_its_keys_is_corrupt_to_cut() {
    let (img, inode, _) = interleaved(12);
    let leaf = read_index(&inode[ROOT], 0).1;
    let rw = Counting::new(img);
    let sb = read_superblock(&rw.image).unwrap();
    let at = (leaf * sb.block_size as u64) as usize;
    {
        let mut v = rw.image.0.borrow_mut();
        for i in 0..rd_u16(&v[at..], 2) as usize {
            let e = at + ENTRY + i * ENTRY;
            let moved = rd_u32(&v, e) + 300;
            v[e..e + 4].copy_from_slice(&moved.to_le_bytes());
        }
    }
    let mut cut = rerooted(&inode, &[leaf; 4]);
    assert_eq!(truncate(&rw, &sb, &mut cut, 0, None), Err(FsError::Corrupt));
    assert!(rw.writes.get() <= honest_writes(), "{} writes", rw.writes.get());
}

/// **A root whose keys do not rise is `Corrupt` to cut**, before anything is written: a cut finds
/// what it drops by its keys, and keyed 0, 300, 100 it dropped nothing and kept block 300 mapped.
#[test]
fn a_root_whose_keys_do_not_rise_is_corrupt_to_cut() {
    let (img, inode, _) = interleaved(12);
    let rw = RwImage(RefCell::new(img));
    let sb = read_superblock(&rw).unwrap();
    let leaf_at = |lblock: u64| {
        let data = ext4::alloc_block(&rw, &sb, sb.first_data_block as u64).unwrap();
        let blk = ext4::alloc_block(&rw, &sb, sb.first_data_block as u64).unwrap();
        let mut node = vec![0u8; sb.block_size as usize];
        set_header(&mut node, 1, block_cap(&sb), 0);
        write_extent(&mut node, 0, Extent::written(lblock, data, 1));
        rw.write_at(blk * sb.block_size as u64, &node).unwrap();
        blk
    };
    let leaves = [(0, leaf_at(0)), (300, leaf_at(300)), (100, leaf_at(100))];
    let mut root = [0u8; 60];
    set_header(&mut root, 3, ROOT_ENTRIES, 1);
    for (i, &(key, blk)) in leaves.iter().enumerate() {
        write_index(&mut root, i, key, blk);
    }
    let mut cut = inode;
    cut[ROOT].copy_from_slice(&root);
    let before = rw.0.borrow().clone();
    assert_eq!(truncate(&rw, &sb, &mut cut, 150, None), Err(FsError::Corrupt));
    assert!(*rw.0.borrow() == before, "refused, it wrote nothing");
}
