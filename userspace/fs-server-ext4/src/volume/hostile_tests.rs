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
            Ext4(&RwImage(RefCell::new(v))).check()
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
