//! The zone held to `dumpe2fs`, which names each group's metadata, at every block of each layout.

use super::*;
use crate::test_support::RwImage;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::process::Command;

/// A scratch file of this test's own, holding `img`.
fn scratch(img: &[u8]) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("nitrox-zone-{}-{n}.img", std::process::id()));
    std::fs::write(&p, img).unwrap();
    p
}

/// An empty filesystem from `mke2fs`, with `features` past the fixtures' own.
fn made(block_size: u32, blocks: u32, features: &str) -> Vec<u8> {
    let p = scratch(&[]);
    let status = Command::new("mke2fs")
        .args(["-q", "-F", "-t", "ext4", "-O", &format!("^has_journal,^64bit,^metadata_csum,{features}")])
        .args(["-b", &block_size.to_string(), p.to_str().unwrap(), &blocks.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "mke2fs -O {features}");
    let img = std::fs::read(&p).unwrap();
    let _ = std::fs::remove_file(&p);
    img
}

/// Every block `dumpe2fs` names as a group's superblock, descriptors, reserve, bitmap or table.
fn dumped(img: &[u8]) -> BTreeSet<u64> {
    let p = scratch(img);
    let out = Command::new("dumpe2fs").arg(&p).output().unwrap();
    let _ = std::fs::remove_file(&p);
    let keys = ["superblock at ", "descriptors at ", "GDT blocks at ", "bitmap at ", "Inode table at "];
    let mut blocks = BTreeSet::new();
    for part in String::from_utf8_lossy(&out.stdout).lines().flat_map(|l| l.split(", ")) {
        for key in keys.iter().filter_map(|k| part.find(k).map(|i| &part[i + k.len()..])) {
            let range = key.split_whitespace().next().unwrap();
            let (a, b) = range.split_once('-').unwrap_or((range, range));
            blocks.extend(a.parse::<u64>().unwrap()..=b.parse::<u64>().unwrap());
        }
    }
    blocks
}

/// **The zone is the metadata `dumpe2fs` names**, at every block and over runs across each edge:
/// with and without `flex_bg`, with `sparse_super`, `sparse_super2` or a backup in every group, a
/// reserve after each descriptor table, 4 KiB blocks, and this crate's `mkfs`.
#[test]
fn the_zone_is_the_metadata_dumpe2fs_names() {
    let ours = RwImage(RefCell::new(vec![0u8; 24576 * 1024]));
    let params = crate::mkfs::Params {
        blocks: 24576,
        block_size: 1024,
        bytes_per_inode: 16384,
        uuid: *b"nitrox-test-uuid",
        label: [0; 16],
        now: 0,
    };
    crate::mkfs::format(&ours, &params, &mut |_, _| {}).unwrap();
    let layouts = [
        ("flex_bg", made(1024, 24576, "^resize_inode")),
        ("no flex_bg", made(1024, 24576, "^resize_inode,^flex_bg")),
        ("a backup in every group", made(1024, 24576, "^resize_inode,^sparse_super")),
        ("sparse_super2", made(1024, 24576, "^resize_inode,sparse_super2")),
        ("a reserve after each table", made(1024, 24576, "resize_inode")),
        ("4 KiB blocks", made(4096, 4096, "^resize_inode")),
        ("this crate's mkfs", ours.0.into_inner()),
    ];
    for (what, img) in layouts {
        let meta = dumped(&img);
        let rw = RwImage(RefCell::new(img));
        let sb = read_superblock(&rw).unwrap();
        assert_eq!(layout(&rw, &sb), Ok(()), "{what}: a layout the zone covers");
        assert!(meta.len() > 3 && sb.group_count() >= 1, "{what}: dumpe2fs named {}", meta.len());
        let first = sb.first_data_block as u64;
        let open = |b: u64| b >= first && b < sb.blocks_count && !meta.contains(&b);
        for b in 0..sb.blocks_count + 2 {
            assert_eq!(clear(&rw, &sb, b, 1), Ok(open(b)), "{what}: block {b}");
        }
        let edges = meta.iter().flat_map(|&b| [b.saturating_sub(2), b + 1]);
        let starts = edges.chain((first..sb.blocks_count).step_by(997));
        for (start, len) in starts.flat_map(|s| [(s, 2), (s, 40), (s, 9000)]) {
            let want = (start..start + len).all(open);
            assert_eq!(clear(&rw, &sb, start, len), Ok(want), "{what}: {len} blocks from {start}");
        }
    }
}
