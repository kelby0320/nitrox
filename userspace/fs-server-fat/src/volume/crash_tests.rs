//! **A crash between any two writes** (PR #365 review, finding 2). Each change's writes are
//! recorded as it makes them, then replayed onto the image it started from one at a time, and after
//! each the image is checked as a crash there would leave it: every entry's chain allocated, no
//! cluster in two chains, no file longer than its chain, and what a grow adds reading as zeroes,
//! over free clusters filled with garbage first. A settled image cannot show order — `fsck.fat` on
//! it passed with every flush that orders the writes removed — and this can.

use super::*;
use crate::test_support::{FileImage, Overlay, mformat, mkfs, mmd, put};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

const NOW: i64 = 1_791_244_800;

/// A writer that records each write, in order, and passes it on.
struct Recorder<'a> {
    inner: &'a Overlay,
    log: RefCell<Vec<(u64, Vec<u8>)>>,
}

impl BlockReader for Recorder<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        self.inner.read_at(offset, buf)
    }
}

impl BlockWriter for Recorder<'_> {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
        self.log.borrow_mut().push((offset, buf.to_vec()));
        self.inner.write_at(offset, buf)
    }
}

/// **An image's bytes, its free clusters filled with `0xAB`**, so a cluster linked before it is
/// zeroed shows.
fn with_garbage_in_free_clusters(path: &std::path::Path) -> Rc<Vec<u8>> {
    let mut bytes = std::fs::read(path).unwrap();
    let probe = Overlay::new(Rc::new(bytes.clone()));
    let fat = Fat::new(&probe);
    let g = *fat.geometry().unwrap();
    let cb = g.cluster_bytes() as usize;
    for c in 2..g.clusters + 2 {
        if fat.cache.borrow_mut().next(&probe, &g, c).unwrap() == Next::Free {
            let at = (g.cluster_sector(c) * SECTOR as u64) as usize;
            bytes[at..at + cb].fill(0xAB);
        }
    }
    Rc::new(bytes)
}

/// **What a crash at this point would leave is consistent**: every entry's chain allocated and
/// ending, no cluster in two files' chains, no file longer than its chain. Lost clusters are
/// allowed — a check reclaims them — and so are orphaned long-name parts. **So is one file under
/// two names**, two entries starting at the same cluster: a rename writes the new name before it
/// deletes the old, so a crash between leaves the file twice rather than nowhere, as Linux's vfat
/// does. Any other overlap is two files sharing a cluster, and fails.
fn consistent(img: &Overlay) -> Result<(), String> {
    let fat = Fat::new(img);
    let g = *fat.geometry().unwrap();
    let cb = g.cluster_bytes() as u64;
    let mut cache = fat.cache.borrow_mut();
    // Each cluster's owner: the first cluster of the chain it is in, and a name for messages.
    let mut owner: HashMap<u32, (u32, String)> = HashMap::new();
    let mut claim = |c: u32, first: u32, who: &str| match owner.insert(c, (first, who.to_string())) {
        Some((other_first, other)) if other_first != first => {
            Err(format!("cluster {c} is in {other}'s chain and {who}'s"))
        }
        _ => Ok(()),
    };
    let mut walked: Vec<u32> = Vec::new();
    if let Dir::Chain(rc) = Dir::root(&g) {
        let mut chain = Vec::new();
        cache.walk(img, &g, rc, |_, c| {
            chain.push(c);
            true
        })
        .map_err(|e| format!("the root's chain: {e:?}"))?;
        for c in chain {
            claim(c, rc, "/")?;
        }
    }
    let mut dirs = vec![(Dir::root(&g), String::from("/"))];
    while let Some((d, path)) = dirs.pop() {
        let mut found = Vec::new();
        dir::entries(img, &g, &mut cache, d, 0, |f| {
            found.push(*f);
            true
        })
        .map_err(|e| format!("{path}: {e:?}"))?;
        for f in found {
            let mut s = [0u8; 12];
            let n = crate::names::short_display(&f.short, 0, &mut s);
            let name = format!("{path}{}", String::from_utf8_lossy(&s[..n]));
            if f.cluster == 0 {
                if f.size > 0 {
                    return Err(format!("{name} holds {} bytes and no cluster", f.size));
                }
                continue;
            }
            let mut chain = Vec::new();
            cache
                .walk(img, &g, f.cluster, |_, c| {
                    chain.push(c);
                    true
                })
                .map_err(|e| format!("{name}'s chain runs into a free cluster, or loops: {e:?}"))?;
            for &c in &chain {
                claim(c, f.cluster, &name)?;
            }
            if f.is_dir() {
                // A directory under two names, mid-move, is walked once.
                if !walked.contains(&f.cluster) {
                    walked.push(f.cluster);
                    dirs.push((Dir::Chain(f.cluster), format!("{name}/")));
                }
            } else if chain.len() as u64 * cb < f.size as u64 {
                return Err(format!("{name} is {} bytes over a chain of {}", f.size, chain.len()));
            }
        }
    }
    Ok(())
}

/// **Bytes `[from, size)` of the file at `path` read as zeroes**, at whatever size its entry says.
fn zero_past(img: &Overlay, path: &[u8], from: u64) -> Result<(), String> {
    let fat = Fat::new(img);
    let mut buf = vec![0u8; 64 * 1024];
    let size = fat.read_file(path, &mut buf).map_err(|e| format!("{e:?}"))? as u64;
    match buf[from.min(size) as usize..size as usize].iter().position(|&b| b != 0) {
        Some(at) => {
            let path = String::from_utf8_lossy(path);
            Err(format!("byte {} of {path} reads {:#04x}, not zero", from + at as u64, buf[from as usize + at]))
        }
        None => Ok(()),
    }
}

/// **Run `op` on `base`, then crash it after every write it made**, checking each crash with
/// `check`. How many writes there were.
fn crash_everywhere(
    base: &Rc<Vec<u8>>,
    op: impl FnOnce(&Fat<Recorder>),
    check: impl Fn(&Overlay) -> Result<(), String>,
) -> usize {
    let live = Overlay::new(base.clone());
    let rec = Recorder { inner: &live, log: RefCell::new(Vec::new()) };
    op(&Fat::new(&rec));
    let writes = rec.log.into_inner();
    let replay = Overlay::new(base.clone());
    check(&replay).unwrap_or_else(|e| panic!("before any write: {e}"));
    for (i, (at, bytes)) in writes.iter().enumerate() {
        replay.write_at(*at, bytes).unwrap();
        if let Err(e) = check(&replay) {
            panic!("a crash after write {} of {} (at byte {at:#x}, {} bytes): {e}", i + 1, writes.len(), bytes.len());
        }
    }
    writes.len()
}

/// **A grow of a file with bytes**: zeroes its tail and the clusters it takes, then the chain,
/// then the entry. A crash anywhere leaves its entry within its chain, and what it adds zero.
#[test]
fn a_crash_anywhere_in_a_grow_leaves_its_file_whole_and_its_new_bytes_zero() {
    let img = mformat(64, &["-F", "-c", "8"]);
    put(&img, "f.bin", &[0x11u8; 6000]);
    put(&img, "empty.bin", b"");
    let base = with_garbage_in_free_clusters(&img);
    let writes = crash_everywhere(
        &base,
        |fat| fat.grow_file(b"/f.bin", 20_000, NOW).unwrap(),
        |img| consistent(img).and_then(|()| zero_past(img, b"/f.bin", 6000)),
    );
    assert!(writes >= 3, "{writes} writes");
    crash_everywhere(
        &base,
        |fat| fat.grow_file(b"/empty.bin", 20_000, NOW).unwrap(),
        |img| consistent(img).and_then(|()| zero_past(img, b"/empty.bin", 0)),
    );
}

/// The FAT32 entry of cluster `c` in every copy of the FAT, set to `v` in `bytes`.
fn poke32(bytes: &mut [u8], g: &crate::bpb::Geometry, c: u32, v: u32) {
    for copy in 0..g.fats {
        let at = (g.fat_byte(copy) + c as u64 * 4) as usize;
        bytes[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
}

/// **A truncate cuts the chain, writes the cut, then frees** — so with the freed cluster's entry in
/// a FAT sector before the cut's, which the sector-ordered write would otherwise put on the disk
/// first, no crash leaves the file's chain running into a free cluster. The file's chain is made by
/// hand: from cluster 302 (the FAT's third sector) to its second cluster (the first).
#[test]
fn a_crash_anywhere_in_a_truncate_leaves_no_chain_into_a_free_cluster() {
    let img = mformat(64, &["-F", "-c", "8"]);
    put(&img, "b.bin", &[0x22u8; 8192]);
    let mut bytes = std::fs::read(&img).unwrap();
    let probe = Overlay::new(Rc::new(bytes.clone()));
    let fat = Fat::new(&probe);
    let g = *fat.geometry().unwrap();
    let f = dir::lookup(&probe, &g, &mut fat.cache.borrow_mut(), Dir::root(&g), b"b.bin").unwrap();
    let second = fat.cache.borrow_mut().nth(&probe, &g, f.cluster, 1).unwrap().unwrap();
    let high = 302;
    assert!(high * 4 / 512 > second * 4 / 512, "the cut's sector after the freed one's");
    poke32(&mut bytes, &g, f.cluster, 0);
    poke32(&mut bytes, &g, high, second);
    let at = dir::slot_byte(&probe, &g, &mut fat.cache.borrow_mut(), f.dir, f.slot).unwrap().unwrap() as usize;
    bytes[at + 20..at + 22].copy_from_slice(&((high >> 16) as u16).to_le_bytes());
    bytes[at + 26..at + 28].copy_from_slice(&(high as u16).to_le_bytes());
    let base = Rc::new(bytes);
    consistent(&Overlay::new(base.clone())).expect("the chain made by hand");
    crash_everywhere(&base, |fat| assert_eq!(fat.truncate_file(b"/b.bin", 100, NOW), Ok(None)), consistent);
}

/// **A directory made**, where its parent has room, and where its parent must grow: the child's
/// cluster on the disk before the entry naming it, and a grown parent's new cluster zeroed and
/// taken before it is linked. On 512-byte clusters a directory is sixteen slots, so fourteen files
/// fill one. **A 700-cluster file after them** puts the parent's new cluster in the FAT's third
/// sector and its link in the first: not adjacent, so not one write, which a link flushed with the
/// allocation, in sector order, would otherwise make atomic by luck.
#[test]
fn a_crash_anywhere_in_a_mkdir_is_consistent() {
    let img = mkfs(2, &["-F", "12", "-s", "1"]);
    mmd(&img, "p");
    for i in 0..14 {
        put(&img, &format!("p/F{i}.TXT"), b"x");
    }
    put(&img, "big.bin", &[6u8; 700 * 512]);
    let base = with_garbage_in_free_clusters(&img);
    let _ = std::fs::remove_file(&img);
    crash_everywhere(
        &base,
        |fat| {
            let root = fat.resolve_dir(b"/").unwrap();
            fat.mkdir_at(root, b"flat", NOW).unwrap();
        },
        consistent,
    );
    crash_everywhere(
        &base,
        |fat| {
            let p = fat.resolve_dir(b"/p").unwrap();
            fat.mkdir_at(p, b"a directory with a long name", NOW).unwrap();
        },
        consistent,
    );
}

/// **Removing a file and releasing it, and a rename across directories**, crashed everywhere: at
/// worst the moved file is under both names for a moment, never two files in one cluster.
#[test]
fn a_crash_anywhere_in_a_removal_or_a_move_is_consistent() {
    let img = mformat(16, &["-c", "8"]);
    mmd(&img, "a");
    mmd(&img, "b");
    put(&img, "a/moved file with a long name.txt", &[3u8; 10_000]);
    put(&img, "doomed.bin", &[4u8; 10_000]);
    let base = with_garbage_in_free_clusters(&img);
    let _ = FileImage::open(&img);
    crash_everywhere(
        &base,
        |fat| {
            let root = fat.resolve_dir(b"/").unwrap();
            let id = fat.unlink_at(root, b"doomed.bin", NOW).unwrap().unwrap();
            fat.release(id, NOW).unwrap();
            fat.rename_path(b"/a/moved file with a long name.txt", b"/b/moved.txt", false, NOW).unwrap();
        },
        consistent,
    );
}
