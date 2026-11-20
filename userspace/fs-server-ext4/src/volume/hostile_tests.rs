//! `Ext4` on **bytes a correct writer never produces**: a removable disk is anyone's bytes, so a
//! malformed one must be an `FsError`, never a panic, which in the server ends every client's
//! requests with `PeerClosed`.

use super::*;
use crate::ext4::{inode_offset, read_inode, read_superblock};
use crate::extent::Lookup;
use crate::test_support::{RwImage, fixture_blocks};
use crate::{rd_u16, rd_u32};
use std::cell::RefCell;

const NOW: i64 = 1_791_244_800;

/// A deterministic generator, so a failing seed can be run again.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// An image with a little of everything a request walks: files with data, a directory two deep, and
/// names of several lengths, so the directories hold more than `.` and `..`.
fn populated(block_size: u32) -> Vec<u8> {
    let rw = RwImage(RefCell::new(fixture_blocks(block_size, 2 * 1024 * 1024 / block_size, 0, b"seed\n")));
    let vol = Ext4(&rw);
    let d = {
        vol.mkdir_at(2, b"d", NOW).unwrap();
        vol.resolve_dir(b"/d").unwrap()
    };
    vol.mkdir_at(d, b"inner", NOW).unwrap();
    for (parent, name, size) in [
        (b"/system" as &[u8], b"a" as &[u8], 3000),
        (b"/system", b"b", 9000),
        (b"/d", b"f", 20000),
        (b"/d", b"a name long enough to take a record of its own", 10),
    ] {
        vol.create_file(parent, name, NOW).unwrap();
        vol.grow_file(&[parent, b"/", name].concat(), size, NOW).unwrap();
    }
    rw.0.into_inner()
}

/// Every request a client can make, each allowed to fail.
fn exercise(vol: &Ext4<'_, RwImage>) {
    let _ = vol.check();
    let _ = vol.was_left_clean();
    let _ = vol.mark_mounted();
    for path in [b"/" as &[u8], b"/system", b"/d", b"/d/inner"] {
        if let Ok(dir) = vol.resolve_dir(path) {
            let _ = vol.read_dir(dir, 0, |_| true);
        }
    }
    let mut runs = [BlockRun::default(); 8];
    let mut out = vec![0u8; 64 * 1024];
    for path in [b"/system/current-generation" as &[u8], b"/system/a", b"/d/f"] {
        let _ = vol.map_file(path, &mut runs);
        let _ = vol.read_file(path, &mut out);
        let _ = vol.read_file_range(path, 100, 5000, &mut out);
    }
    let _ = vol.create_file(b"/d", b"new", NOW);
    let _ = vol.grow_file(b"/d/new", 9000, NOW);
    let _ = vol.truncate_file(b"/d/new", 100, NOW);
    let _ = vol.grow_file(b"/d/f", 40000, NOW);
    let _ = vol.truncate_file(b"/d/f", 0, NOW);
    if let Ok(d) = vol.resolve_dir(b"/d") {
        let _ = vol.mkdir_at(d, b"made", NOW);
        let _ = vol.rename_at(d, b"made", b"renamed", NOW);
        let _ = vol.touch_at(d, b"f", NOW);
    }
    let _ = vol.rename_path(b"/system/a", b"/d/moved", false, NOW);
    if let Ok(Some(id)) = vol.rename_path(b"/system/b", b"/d/f", true, NOW) {
        let _ = vol.release(id, NOW);
    }
    let _ = vol.rename_path(b"/d/inner", b"/system/inner", false, NOW);
    if let Ok(sys) = vol.resolve_dir(b"/system") {
        if let Ok(Some(id)) = vol.unlink_at(sys, b"current-generation", NOW) {
            let _ = vol.release(id, NOW);
        }
        let _ = vol.rmdir_at(sys, b"inner", NOW, |_| ());
    }
    let _ = vol.touch_file(12, NOW);
    let _ = vol.mark_clean();
}

/// Each record of a directory, as `(its byte on the device, its block's byte)`.
fn records(img: &RwImage, dir: u32) -> Vec<(u64, u64)> {
    let sb = read_superblock(img).unwrap();
    let inode = read_inode(img, &sb, dir).unwrap();
    let bs = sb.block_size as u64;
    let mut map = Lookup::new(&inode);
    let mut found = Vec::new();
    for lb in 0..(rd_u32(&inode, 4) as u64).div_ceil(bs) {
        let block = map.block(img, &sb, lb).unwrap() * bs;
        let mut buf = vec![0u8; bs as usize];
        img.read_at(block, &mut buf).unwrap();
        let mut off = 0;
        while off + 8 <= bs as usize {
            found.push((block + off as u64, block));
            off += (rd_u16(&buf, off + 4) as usize).max(8);
        }
    }
    found
}

/// Write over `at` a live record of its whole length whose name may run past it.
fn cut(at: &mut [u8], ino: u32, name_len: u8) {
    let len = at.len();
    at[..4].copy_from_slice(&ino.to_le_bytes());
    at[4..6].copy_from_slice(&(len as u16).to_le_bytes());
    at[6] = name_len;
    at[7] = 2;
    at[8..].fill(b'.');
}

/// **A record cut at its block's end, its name running past it, is refused to rename over** before
/// anything is written: an 8-byte record whose two-byte name, `..`'s length, ends past the block.
#[test]
fn a_record_whose_name_runs_past_its_block_is_an_error_to_rename_over() {
    for bs in [1024u32, 4096] {
        let img = RwImage(RefCell::new(populated(bs)));
        let vol = Ext4(&img);
        let inner = vol.resolve_dir(b"/d/inner").unwrap() as u32;
        let (first, block) = records(&img, inner)[0];
        let end = (block + bs as u64) as usize;
        let mut v = img.0.borrow_mut();
        v[first as usize + 4..first as usize + 6].copy_from_slice(&((end - 8 - first as usize) as u16).to_le_bytes());
        cut(&mut v[end - 8..end], 2, 2);
        drop(v);
        let before = img.0.borrow().clone();
        let moved = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            vol.rename_path(b"/d/inner", b"/system/inner", false, NOW)
        }));
        assert!(matches!(&moved, Ok(Err(_))), "{bs}-byte blocks: {moved:?}");
        assert!(*img.0.borrow() == before, "{bs}-byte blocks: refused, it wrote nothing");
    }
}

/// **A loop of `..` records is `Corrupt` to move a directory into**, not a walk for ever.
#[test]
fn a_loop_of_parent_records_is_corrupt_to_move_a_directory_into() {
    let img = RwImage(RefCell::new(populated(4096)));
    let vol = Ext4(&img);
    let inner = vol.resolve_dir(b"/d/inner").unwrap() as u32;
    let at = records(&img, inner)[1].0 as usize;
    img.0.borrow_mut()[at..at + 4].copy_from_slice(&inner.to_le_bytes());
    assert_eq!(vol.rename_path(b"/system", b"/d/inner/system", false, NOW), Err(FsError::Corrupt));
}

/// **Garbage anywhere ext4 keeps its structure is an error, never a panic**, at 1 KiB and 4 KiB
/// blocks: random metadata bytes, then fields of real records and inodes, and records cut at a
/// block's end, since random bytes alone rarely make a record a scan reads.
#[test]
fn garbage_in_any_structure_is_an_error_never_a_panic() {
    for bs in [1024u32, 4096] {
        let base = populated(bs);
        let pristine = RwImage(RefCell::new(base.clone()));
        let vol = Ext4(&pristine);
        let dirs: Vec<u32> = [b"/" as &[u8], b"/system", b"/d", b"/d/inner"]
            .iter()
            .map(|p| vol.resolve_dir(p).unwrap() as u32)
            .collect();
        let recs: Vec<(u64, u64)> = dirs.iter().flat_map(|&d| records(&pristine, d)).collect();
        let mut blocks: Vec<u64> = recs.iter().map(|r| r.1).collect();
        blocks.dedup();
        let sb = read_superblock(&pristine).unwrap();
        let inodes: Vec<u64> = (2..=20).map(|ino| inode_offset(&pristine, &sb, ino).unwrap()).collect();
        let meta = (1024u64, 4 * bs as u64 + inodes[inodes.len() - 1] - 1024);
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ bs as u64);
        for seed in 0..400 {
            let img = RwImage(RefCell::new(base.clone()));
            let mut v = img.0.borrow_mut();
            match seed % 4 {
                0 => {
                    for _ in 0..1 + rng.below(8) {
                        v[(meta.0 + rng.below(meta.1)) as usize] = rng.next() as u8;
                    }
                }
                1 => {
                    let (at, _) = recs[rng.below(recs.len() as u64) as usize];
                    let field = [0usize, 1, 4, 5, 6, 7][rng.below(6) as usize];
                    v[at as usize + field] = rng.next() as u8;
                }
                2 => {
                    let block = blocks[rng.below(blocks.len() as u64) as usize];
                    let in_block: Vec<u64> = recs.iter().filter(|r| r.1 == block).map(|r| r.0).collect();
                    // The first made to swallow the rest, so a lookup reaches the cut record.
                    let from = if rng.below(2) == 0 { in_block[0] } else { in_block[in_block.len() - 1] } as usize;
                    let end = (block + bs as u64) as usize;
                    let tail = [8usize, 12][rng.below(2) as usize];
                    let (ino, name_len) = (2 + rng.below(18) as u32, [1u8, 2, 5, 18][rng.below(4) as usize]);
                    if end - tail >= from + 12 {
                        v[from + 4..from + 6].copy_from_slice(&((end - tail - from) as u16).to_le_bytes());
                        cut(&mut v[end - tail..end], ino, name_len);
                    }
                }
                _ => {
                    let at = inodes[rng.below(inodes.len() as u64) as usize] as usize;
                    let field = [0usize, 1, 4, 5, 7, 26, 27, 28, 32, 34][rng.below(10) as usize];
                    let field = if rng.below(3) == 0 { 40 + rng.below(60) as usize } else { field };
                    v[at + field] = rng.next() as u8;
                }
            }
            drop(v);
            let vol = Ext4(&img);
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| exercise(&vol)));
            assert!(ran.is_ok(), "{bs}-byte blocks, seed {seed}: a request panicked on garbage");
        }
    }
}

/// **An rmdir takes a link only from a directory**: a crafted `..` naming a file took that file's
/// one link, while a name still reached it.
#[test]
fn an_rmdir_takes_a_link_only_from_a_directory() {
    let img = RwImage(RefCell::new(populated(4096)));
    let vol = Ext4(&img);
    let file = vol.map_file(b"/system/a", &mut [BlockRun::default(); 4]).unwrap().id as u32;
    let inner = vol.resolve_dir(b"/d/inner").unwrap() as u32;
    let at = records(&img, inner)[1].0 as usize;
    img.0.borrow_mut()[at..at + 4].copy_from_slice(&file.to_le_bytes());
    let d = vol.resolve_dir(b"/d").unwrap();
    assert_eq!(vol.rmdir_at(d, b"inner", NOW, |_| ()), Ok(()));
    let sb = read_superblock(&img).unwrap();
    assert_eq!(rd_u16(&read_inode(&img, &sb, file).unwrap(), 26), 1, "the file keeps its link");
}

/// Byte offset of `group`'s descriptor in an image of `bs`-byte blocks.
fn descriptor(bs: u32, group: u32) -> usize {
    (if bs == 1024 { 2 } else { 1 }) * bs as usize + 32 * group as usize
}

/// **A group wider than its bitmap block is refused**, by the check and by each request, as Linux
/// refuses it. Past the bits read, the allocator took its scratch's unread zeroes for free blocks,
/// giving one block to two files at 1 KiB blocks, and at 4 KiB indexed past the scratch.
#[test]
fn a_group_wider_than_its_bitmap_block_is_refused() {
    for (bs, blocks, per_group) in [(4096u32, 4096u32, 65536u32), (1024, 16384, 16384)] {
        let img = RwImage(RefCell::new(fixture_blocks(bs, blocks, 0, b"g\n")));
        let vol = Ext4(&img);
        vol.create_file(b"/system", b"a", NOW).unwrap();
        vol.create_file(b"/system", b"b", NOW).unwrap();
        let mut v = img.0.borrow_mut();
        v[1024 + 32..1024 + 36].copy_from_slice(&per_group.to_le_bytes());
        if bs == 4096 {
            v[1024 + 4..1024 + 8].copy_from_slice(&per_group.to_le_bytes());
        }
        let gd = descriptor(bs, 0);
        let bitmap = rd_u32(&v, gd) as usize * bs as usize;
        v[bitmap..bitmap + bs as usize].fill(0xFF);
        v[gd + 12..gd + 14].copy_from_slice(&100u16.to_le_bytes());
        drop(v);
        assert_eq!(vol.check(), Err(Unservable::Geometry("s_blocks_per_group")), "{bs}-byte blocks");
        for name in [b"/system/a" as &[u8], b"/system/b"] {
            let grown = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| vol.grow_file(name, 100_000, NOW)));
            assert!(matches!(grown, Ok(Err(FsError::Corrupt))), "{bs}-byte blocks: {grown:?}");
        }
    }
}

/// Each of the superblock's group counts is held to its bitmap and to the groups, at its edge.
#[test]
fn a_superblock_is_held_to_its_groups_at_each_edge() {
    for bs in [1024u32, 4096] {
        let good = populated(bs);
        let sb = |v: &[u8], off: usize| rd_u32(v, 1024 + off);
        let with = |pokes: &[(usize, u32)]| {
            let mut v = good.clone();
            for &(off, value) in pokes {
                v[1024 + off..1024 + off + 4].copy_from_slice(&value.to_le_bytes());
            }
            Ext4(&crate::ReadOnly(&RwImage(RefCell::new(v)))).check()
        };
        let (bits, inodes) = (bs * 8, sb(&good, 0));
        assert_eq!(sb(&good, 32), bits, "mke2fs fills a group's bitmap");
        assert_eq!(with(&[(32, bits + 8)]), Err(Unservable::Geometry("s_blocks_per_group")));
        assert_eq!(with(&[(40, bits), (0, bits)]), Ok(()), "{bs}: a full inode bitmap is a group");
        assert_eq!(with(&[(40, bits + 8), (0, bits + 8)]), Err(Unservable::Geometry("s_inodes_per_group")));
        assert_eq!(with(&[(0, inodes + 1)]), Err(Unservable::Geometry("s_inodes_count")));
        assert_eq!(with(&[(0, inodes - 1)]), Err(Unservable::Geometry("s_inodes_count")));
    }
}

/// **A record naming an inode past `s_inodes_count` is `Corrupt`**, and a removal of it writes
/// nothing: its descriptor lies past the table, so whatever was there was taken for one, and the
/// "inode" and bitmap a removal wrote were wherever that pointed.
#[test]
fn a_record_naming_an_inode_past_the_last_is_corrupt() {
    for bs in [1024u32, 4096] {
        for past in [1u32, 0x8000_0000] {
            let img = RwImage(RefCell::new(populated(bs)));
            let vol = Ext4(&img);
            let inodes = rd_u32(&img.0.borrow(), 1024);
            let d = vol.resolve_dir(b"/d").unwrap();
            let at = records(&img, d as u32)
                .into_iter()
                .map(|r| r.0 as usize)
                .find(|&at| img.0.borrow()[at + 6] == 1 && img.0.borrow()[at + 8] == b'f')
                .unwrap();
            img.0.borrow_mut()[at..at + 4].copy_from_slice(&(inodes.wrapping_add(past)).to_le_bytes());
            let before = img.0.borrow().clone();
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mapped = vol.map_file(b"/d/f", &mut [BlockRun::default(); 4]).map(|_| ());
                (mapped, vol.unlink_at(d, b"f", NOW).map(|_| ()))
            }));
            assert!(ran.is_ok(), "{bs}, +{past}: panicked");
            let (mapped, removed) = ran.unwrap();
            assert_eq!(mapped, Err(FsError::Corrupt), "{bs}, +{past}: map");
            assert_eq!(removed, Err(FsError::Corrupt), "{bs}, +{past}: remove");
            assert!(*img.0.borrow() == before, "{bs}, +{past}: refused, it wrote nothing");
        }
    }
}

/// **A ReadDir cursor inside a record resumes at the next one**, as Linux's readdir does: a cursor
/// into a name the client chose parsed the name as a record, and stat'ed an inode outside the
/// directory the session is confined to. A cursor on a record still starts there.
#[test]
fn a_cursor_inside_a_record_resumes_at_the_next() {
    let img = RwImage(RefCell::new(fixture_blocks(4096, 4096, 0, b"g\n")));
    let vol = Ext4(&img);
    vol.mkdir_at(2, b"box", NOW).unwrap();
    let boxed = vol.resolve_dir(b"/box").unwrap();
    vol.mkdir_at(boxed, b"twelve bytes", NOW).unwrap();
    vol.mkdir_at(boxed, b"after", NOW).unwrap();
    let at = records(&img, boxed as u32)[2].0 as usize;
    // The name's bytes as a record of inode 14, `/system/current-generation`, named "X".
    img.0.borrow_mut()[at + 8..at + 20].copy_from_slice(&[14, 0, 0, 0, 0x0C, 0x0C, 1, 1, b'X', b'Y', b'Z', b'W']);
    let listed = |cursor: u64| {
        let mut ids = Vec::new();
        vol.read_dir(boxed, cursor, |e| {
            ids.push(e.id);
            true
        })
        .unwrap();
        ids
    };
    let after = *listed(0).last().unwrap();
    assert_eq!(listed(at as u64 % 4096 + 8), [after], "from inside the name");
    assert_eq!(listed(at as u64 % 4096 + 20), [after], "from the next record");
    assert_eq!(listed(at as u64 % 4096).len(), 2, "from the record itself");
}

/// **A name already on the disk with a control character is still opened and replaced**, as Linux
/// wrote it and `e2fsck` passes it: only a new entry is refused one.
#[test]
fn a_control_character_in_an_existing_name_is_still_opened_and_replaced() {
    let img = RwImage(RefCell::new(populated(4096)));
    let vol = Ext4(&img);
    vol.create_file(b"/d", b"aXb", NOW).unwrap();
    vol.grow_file(b"/d/aXb", 5000, NOW).unwrap();
    let d = vol.resolve_dir(b"/d").unwrap();
    let at = records(&img, d as u32)
        .into_iter()
        .map(|r| r.0 as usize)
        .find(|&at| &img.0.borrow()[at + 8..at + 11] == b"aXb")
        .unwrap();
    img.0.borrow_mut()[at + 9] = b'\t';
    crate::test_support::assert_e2fsck_clean(&img.0.borrow(), "tab-name");
    let made = vol.map_file(b"/d/a\tb", &mut [BlockRun::default(); 4]).unwrap().id;
    assert_eq!(vol.create_file(b"/d", b"a\tb", NOW), Ok(()), "a create finds it");
    assert_eq!(vol.rename_path(b"/d/f", b"/d/a\tb", true, NOW), Ok(Some(made)), "a rename replaces it");
    assert_eq!(vol.create_file(b"/d", b"c\td", NOW), Err(FsError::Unsupported), "a new one is refused");
}

/// Two groups of 1 KiB blocks, `flex_bg` putting group 1's tables in group 0, group 1 holding
/// backups: `/d/x`, a file of one block, and `/e`, a directory whose one block is full.
fn zoned() -> RwImage {
    let img = RwImage(RefCell::new(fixture_blocks(1024, 9216, 0, b"seed\n")));
    let vol = Ext4(&img);
    vol.mkdir_at(2, b"d", NOW).unwrap();
    vol.mkdir_at(2, b"e", NOW).unwrap();
    vol.create_file(b"/d", b"x", NOW).unwrap();
    vol.grow_file(b"/d/x", 1024, NOW).unwrap();
    for i in 0..3 {
        vol.create_file(b"/e", format!("{i:0>250}").as_bytes(), NOW).unwrap();
    }
    img
}

/// One block of each kind of metadata in [`zoned`], by what it is.
fn metadata(img: &RwImage) -> Vec<(&'static str, u64)> {
    let v = img.0.borrow();
    let gd = |g: usize, at: usize| rd_u32(&v, descriptor(1024, g as u32) + at) as u64;
    let table = rd_u32(&v, 1024 + 40) as u64 * 256 / 1024;
    vec![
        ("the primary superblock", 1),
        ("the descriptor table", 2),
        ("group 0's block bitmap", gd(0, 0)),
        ("group 0's inode bitmap", gd(0, 4)),
        ("group 0's inode table", gd(0, 8)),
        ("group 1's block bitmap, in group 0", gd(1, 0)),
        ("group 1's inode table's last block, in group 0", gd(1, 8) + table - 1),
        ("group 1's backup superblock", 8193),
        ("group 1's backup descriptor table", 8194),
    ]
}

/// Where `/d/x`'s inode is on the device.
fn x_inode(img: &RwImage) -> usize {
    let sb = read_superblock(img).unwrap();
    inode_offset(img, &sb, dir_entry(img, b"/d", b"x")).unwrap() as usize
}

/// Point `/d/x`'s one extent at `phys`.
fn repoint(img: &RwImage, phys: u64) {
    let at = x_inode(img) + 40 + 12;
    img.0.borrow_mut()[at + 8..at + 12].copy_from_slice(&(phys as u32).to_le_bytes());
}

/// **An extent over metadata is never mapped or freed**, whichever kind and however it is reached:
/// one over an inode table was written back into, and freed for the next file to zero.
#[test]
fn an_extent_over_metadata_is_never_mapped_or_freed() {
    let base = zoned();
    for (what, block) in metadata(&base) {
        let sb = read_superblock(&base).unwrap();
        assert!(crate::ext4::block_in_use(&base, &sb, block), "{what}: {block} is marked");
        let fresh = || RwImage(RefCell::new(base.0.borrow().clone()));
        type Way = fn(&Ext4<'_, RwImage>) -> Result<(), FsError>;
        let ways: [(&str, Way); 5] = [
            ("mapped", |v| v.map_file(b"/d/x", &mut [BlockRun::default(); 4]).map(drop)),
            ("unlinked and released", |v| match v.unlink_at(v.resolve_dir(b"/d")?, b"x", NOW)? {
                Some(id) => v.release(id, NOW),
                None => Ok(()),
            }),
            ("truncated", |v| v.truncate_file(b"/d/x", 0, NOW).map(drop)),
            ("grown over, past its size", |v| v.grow_file(b"/d/x", 9000, NOW).map(drop)),
            ("a directory grown over, past its size", |v| v.create_file(b"/e", &[b'4'; 250], NOW)),
        ];
        for (how, way) in ways {
            let img = fresh();
            if how.starts_with("a directory") {
                // A second extent, past the directory's one block, over the metadata.
                let sb = read_superblock(&img).unwrap();
                let e = Ext4(&img).resolve_dir(b"/e").unwrap() as u32;
                let at = inode_offset(&img, &sb, e).unwrap() as usize + 40;
                let mut v = img.0.borrow_mut();
                v[at + 2..at + 4].copy_from_slice(&2u16.to_le_bytes());
                let x = at + 24;
                v[x..x + 4].copy_from_slice(&1u32.to_le_bytes());
                v[x + 4..x + 6].copy_from_slice(&1u16.to_le_bytes());
                v[x + 6..x + 8].fill(0);
                v[x + 8..x + 12].copy_from_slice(&(block as u32).to_le_bytes());
            } else {
                repoint(&img, block);
                if how.starts_with("grown") {
                    let at = x_inode(&img);
                    img.0.borrow_mut()[at + 4..at + 8].fill(0); // `i_size` 0: the extent is past it
                }
            }
            // A cut refuses before it writes; a create or unlink has written already when it is
            // reached.
            let cut = how == "truncated" || how.starts_with("grown");
            let before = cut.then(|| img.0.borrow().clone());
            let got = way(&Ext4(&img));
            assert_eq!(got, Err(FsError::Corrupt), "{what} ({block}), {how}");
            if let Some(before) = before {
                assert!(*img.0.borrow() == before, "{what} ({block}), {how}: refused before any write");
            }
            let sb = read_superblock(&img).unwrap();
            assert!(crate::ext4::block_in_use(&img, &sb, block), "{what} ({block}), {how}: its bit is kept");
        }
    }
}

/// The inode `name` in the directory at `dir` names, read from its record.
fn dir_entry(img: &RwImage, dir: &[u8], name: &[u8]) -> u32 {
    let d = Ext4(img).resolve_dir(dir).unwrap() as u32;
    let found = records(img, d).into_iter().map(|r| r.0 as usize);
    let v = img.0.borrow();
    let at = found.into_iter().find(|&at| &v[at + 8..at + 8 + v[at + 6] as usize] == name);
    rd_u32(&v, at.unwrap())
}

/// **A free bit over an inode table is refused, not handed out**: the allocator gave the block to
/// the next file, whose zeroes wiped the root inode.
#[test]
fn a_free_bit_over_an_inode_table_is_refused_and_the_root_inode_kept() {
    for bs in [1024u32, 4096] {
        let img = RwImage(RefCell::new(fixture_blocks(bs, 4096, 0, b"seed\n")));
        Ext4(&img).create_file(b"/system", b"new", NOW).unwrap();
        let sb = read_superblock(&img).unwrap();
        let (bitmap, table) = {
            let v = img.0.borrow();
            let gd = descriptor(bs, 0);
            (rd_u32(&v, gd) as usize * bs as usize, rd_u32(&v, gd + 8) as u64)
        };
        let index = (table - sb.first_data_block as u64) as usize;
        img.0.borrow_mut()[bitmap + index / 8] &= !(1 << (index % 8));
        assert_eq!(crate::ext4::alloc_run(&img, &sb, table, 1), Err(FsError::Corrupt), "{bs}: the goal is the table");
        // Far enough to reach the table past the blocks `flex_bg` leaves free below it.
        let reach = (table as usize + 8) * bs as usize;
        assert_eq!(Ext4(&img).grow_file(b"/system/new", reach, NOW), Err(FsError::Corrupt), "{bs}: a grow");
        assert_eq!(Ext4(&img).check(), Ok(()), "{bs}: the root inode is a directory still");
        assert!(Ext4(&img).resolve_dir(b"/system").is_ok(), "{bs}: and reaches its entries");
    }
}

/// **Beside metadata, blocks map, free and allocate as before**: either side of group 1's backups.
#[test]
fn the_blocks_beside_metadata_map_free_and_allocate() {
    for block in [8192u64, 8195] {
        let img = zoned();
        let sb = read_superblock(&img).unwrap();
        assert_eq!(crate::ext4::alloc_run(&img, &sb, block, 1), Ok((block, 1)), "{block} is allocated");
        repoint(&img, block);
        let mut runs = [BlockRun::default(); 4];
        Ext4(&img).map_file(b"/d/x", &mut runs).unwrap();
        assert_eq!(runs[0].device_lba, block, "{block} is mapped");
        assert_eq!(Ext4(&img).truncate_file(b"/d/x", 0, NOW), Ok(None));
        assert!(!crate::ext4::block_in_use(&img, &sb, block), "{block} is freed");
    }
}

/// **A writable mount of a layout the check does not cover is served read-only**, saying why: a
/// group's tables outside its flex group, or flex groups wider than a block of descriptors.
#[test]
fn a_layout_the_check_does_not_cover_is_served_read_only() {
    let made = zoned();
    assert_eq!(Ext4(&made).check(), Ok(()), "mke2fs's own layout");
    // `s_log_groups_per_flex`: flex groups of one, so group 1 keeps its tables outside its own.
    for (log, why) in [(0u8, Unservable::Layout { group: 1 }), (6, Unservable::FlexTooWide { groups: 64 })] {
        let img = RwImage(RefCell::new(made.0.borrow().clone()));
        img.0.borrow_mut()[1024 + 372] = log;
        assert_eq!(crate::ext4::check_writable(&img), Err(why), "flex groups of 2^{log}");
        assert_eq!(Ext4(&img).check(), Err(why), "2^{log}: the server's own check");
        assert_eq!(Ext4(&crate::ReadOnly(&img)).check(), Ok(()), "2^{log}: served read-only");
        assert!(format!("{why}").starts_with("it can only be read here"), "{why}");
    }
}

/// Make `/d/x` five blocks whose tree is one level deep: its root an index over `child`, made a
/// leaf of five one-block extents from `phys`, more than a cut can fold back into the inode.
fn deepen(img: &RwImage, child: u64, phys: u64) {
    let at = x_inode(img);
    let node = child as usize * 1024;
    let mut v = img.0.borrow_mut();
    let header = |entries: u16, max: u16, depth: u16| {
        [0x0Au8, 0xF3]
            .into_iter()
            .chain([entries, max, depth].iter().flat_map(|n| n.to_le_bytes()))
            .collect::<Vec<u8>>()
    };
    v[node..node + 8].copy_from_slice(&header(5, 84, 0));
    for i in 0..5 {
        let x = node + 12 + 12 * i;
        v[x..x + 12].fill(0);
        v[x..x + 4].copy_from_slice(&(i as u32).to_le_bytes());
        v[x + 4..x + 6].copy_from_slice(&1u16.to_le_bytes());
        v[x + 8..x + 12].copy_from_slice(&(phys as u32 + i as u32).to_le_bytes());
    }
    v[at + 4..at + 8].copy_from_slice(&(5u32 * 1024).to_le_bytes());
    v[at + 40..at + 48].copy_from_slice(&header(1, 4, 1));
    v[at + 52..at + 64].fill(0);
    v[at + 56..at + 60].copy_from_slice(&(child as u32).to_le_bytes());
}

/// **A tree over metadata is never read as a node or freed**: an index over the inode table, made
/// to read as a leaf, is refused before an append writes it; a leaf over it, freed with the file.
#[test]
fn a_tree_over_metadata_is_never_read_as_a_node_or_freed() {
    let base = zoned();
    let sb = read_superblock(&base).unwrap();
    let table = rd_u32(&base.0.borrow(), descriptor(1024, 0) + 8) as u64;
    let data = Ext4(&base).map_file(b"/d/x", &mut [BlockRun::default(); 1]).map(|_| ());
    assert_eq!(data, Ok(()));

    // The index over the table's first block, inode 1's place made a leaf.
    let img = RwImage(RefCell::new(base.0.borrow().clone()));
    let phys = {
        let mut runs = [BlockRun::default(); 1];
        Ext4(&img).map_file(b"/d/x", &mut runs).unwrap();
        runs[0].device_lba
    };
    deepen(&img, table, phys);
    let node = img.0.borrow()[table as usize * 1024..][..1024].to_vec();
    let mapped = Ext4(&img).map_file(b"/d/x", &mut [BlockRun::default(); 4]).map(drop);
    assert_eq!(mapped, Err(FsError::Corrupt), "mapped through an index over the table");
    assert_eq!(Ext4(&img).grow_file(b"/d/x", 9000, NOW), Err(FsError::Corrupt), "grown through it");
    assert!(img.0.borrow()[table as usize * 1024..][..1024] == node[..], "the table is as it was");

    // A real leaf, mapping the table's first block, freed with the file.
    let img = RwImage(RefCell::new(base.0.borrow().clone()));
    let (leaf, _) = crate::ext4::alloc_run(&img, &sb, 0, 1).unwrap();
    deepen(&img, leaf, table);
    assert_eq!(Ext4(&img).truncate_file(b"/d/x", 0, NOW), Err(FsError::Corrupt), "a leaf over the table");
    assert!(crate::ext4::block_in_use(&img, &sb, table), "the table's block stays marked");

    // An extent of two of the table's blocks, cut between them.
    let img = RwImage(RefCell::new(base.0.borrow().clone()));
    repoint(&img, table);
    let at = x_inode(&img);
    img.0.borrow_mut()[at + 4..at + 8].copy_from_slice(&2048u32.to_le_bytes());
    img.0.borrow_mut()[at + 56..at + 58].copy_from_slice(&2u16.to_le_bytes());
    let before = img.0.borrow().clone();
    assert_eq!(Ext4(&img).truncate_file(b"/d/x", 1024, NOW), Err(FsError::Corrupt), "a tail over the table");
    assert!(*img.0.borrow() == before, "refused before any write");
}
