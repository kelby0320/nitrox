//! `Fat` through `libfsserver`'s request core (Phase 6 Part E.4): what the kernel is replied for a
//! FAT, as `fs-server-ext4`'s volume tests check it for ext4.

use super::*;
use crate::ReadOnly;
use crate::test_support::{FileImage, mformat, mtools, pattern, put, scratch};
use libfsserver::serve::{MAX_FILE, MAX_REASON, Served, encode_refusal, serve};
use libkern::KError;
use librsproto::error::parse_error;
use librsproto::file::{parse_read_range_reply, read_range_request};
use librsproto::namespace::{
    FILE_BLOCKS_READ_ONLY, OBJECT_KIND_FILE_BLOCKS, RESOLVE_FILE_LAZY, parse_resolve_reply, resolve_request,
};
use librsproto::{OP_FILE_READ_RANGE, OP_NS_RESOLVE, OP_READY, RS_FLAG_ERROR, decode, encode};

/// A `RESOLVE_FILE_LAZY` resolve for `suffix`: the kernel's form for a Model A file.
fn lazy_request(request_id: u64, suffix: &[u8]) -> Vec<u8> {
    let mut body = [0u8; 256];
    let n = resolve_request(&mut body, 0x8000, RESOLVE_FILE_LAZY, suffix).unwrap();
    let mut buf = [0u8; 512];
    let len = encode(&mut buf, OP_NS_RESOLVE, request_id, 0, &body[..n], 0).unwrap();
    buf[..len].to_vec()
}

/// What a block-file reply says: its size, block size, runs as `(file block, device block,
/// length)`, file id and flags.
struct Blocks {
    size: u32,
    block_size: u32,
    runs: Vec<(u64, u64, u32)>,
    id: u64,
    flags: u32,
}

/// **Serve `suffix` lazily through `vol`** and read the block-file reply, or the error's code.
fn resolve<V: Volume>(vol: &V, suffix: &[u8]) -> Result<Blocks, i32> {
    let mut content = [0u8; MAX_FILE];
    let mut reply = [0u8; 4096];
    match serve(vol, &lazy_request(7, suffix), &mut content, &mut reply) {
        Served::LazyBlocks { reply_len } => {
            let m = decode(&reply[..reply_len]).unwrap();
            assert_eq!((m.op, m.request_id, m.handle_count), (OP_NS_RESOLVE, 7, 1), "a reply with the device");
            let b = m.body;
            let rr = parse_resolve_reply(b).unwrap();
            assert_eq!(rr.object_kind, OBJECT_KIND_FILE_BLOCKS);
            let u32_at = |at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
            let u64_at = |at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
            let runs = (0..u32_at(12) as usize).map(|i| 32 + 24 * i).map(|at| (u64_at(at), u64_at(at + 8), u32_at(at + 16)));
            Ok(Blocks { size: rr.content_len, block_size: u32_at(8), runs: runs.collect(), id: u64_at(16), flags: u32_at(24) })
        }
        Served::Error { reply_len } => {
            let m = decode(&reply[..reply_len]).unwrap();
            assert!(m.is_error());
            Err(parse_error(m.body).unwrap().kerror)
        }
        _ => panic!("expected a block-file reply or an error"),
    }
}

/// **A FAT file is replied as sectors**: block size 512, its clusters as runs of sectors from the
/// volume's start, and its first cluster as its id; an empty file has no runs and the id `0`,
/// uncached. A read-only mount marks what it replies.
#[test]
fn a_file_is_replied_in_sectors_with_its_first_cluster_as_its_id() {
    let img = mformat(64, &["-F", "-c", "8"]);
    let data = pattern(3 * 4096 + 100, 1);
    put(&img, "file.bin", &data);
    put(&img, "empty.txt", b"");
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let g = *fat.geometry().unwrap();

    let b = resolve(&fat, b"file.bin").unwrap();
    assert_eq!((b.size as usize, b.block_size, b.flags), (data.len(), 512, 0));
    let first = b.id as u32;
    assert!(g.valid_cluster(first), "the id is a cluster: {}", b.id);
    assert_eq!(b.runs, vec![(0, g.cluster_sector(first), 4 * 8)], "four clusters of eight sectors, from the first");
    let mut back = vec![0u8; data.len()];
    disk.read_at(b.runs[0].1 * 512, &mut back).unwrap();
    assert_eq!(back, data, "the run is where the bytes are");

    let e = resolve(&fat, b"empty.txt").unwrap();
    assert_eq!((e.size, e.runs.len(), e.id), (0, 0, 0), "an empty file: no runs, no id");

    let ro = ReadOnly(&disk);
    assert_eq!(resolve(&Fat::new(&ro), b"file.bin").unwrap().flags, FILE_BLOCKS_READ_ONLY);
}

/// **A file in more fragments than a reply holds is refused `TooLarge`**, as ext4's is: mtools
/// fills the 70 holes left by deleting every other one-cluster file.
#[test]
fn a_file_in_more_than_64_fragments_is_refused_too_large() {
    let img = mformat(64, &["-c", "8"]);
    let host = scratch("frag");
    std::fs::create_dir_all(&host).unwrap();
    let srcs: Vec<String> = (0..140)
        .map(|i| {
            let p = host.join(format!("f{i:03}"));
            std::fs::write(&p, pattern(4096, 2)).unwrap();
            p.to_str().unwrap().to_string()
        })
        .collect();
    let mut args: Vec<&str> = srcs.iter().map(String::as_str).collect();
    args.push("::");
    mtools("mcopy", &img, &args);
    let odd: Vec<String> = (1..140).step_by(2).map(|i| format!("::f{i:03}")).collect();
    mtools("mdel", &img, &odd.iter().map(String::as_str).collect::<Vec<_>>());
    let _ = std::fs::remove_dir_all(&host);
    put(&img, "huge.bin", &pattern(100 * 4096, 3));
    let disk = FileImage::open(&img);
    assert_eq!(resolve(&Fat::new(&disk), b"huge.bin").err(), Some(KError::TooLarge.as_i32()));
}

/// **A range read** — the page-cache fill's other path — serves the window asked for.
#[test]
fn a_range_read_serves_the_window() {
    let img = mformat(16, &["-c", "4"]);
    let data = pattern(10_000, 4);
    put(&img, "a long name.bin", &data);
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let mut body = [0u8; 256];
    let n = read_range_request(&mut body, 4_000, 4096, b"A LONG NAME.BIN").unwrap();
    let mut req = [0u8; 512];
    let len = encode(&mut req, OP_FILE_READ_RANGE, 9, 0, &body[..n], 0).unwrap();
    let mut content = [0u8; MAX_FILE];
    let mut reply = [0u8; 4096];
    match serve(&fat, &req[..len], &mut content, &mut reply) {
        Served::File { reply_len, content_len } => {
            assert_eq!(&content[..content_len], &data[4_000..8_096], "found in another case");
            assert_eq!(parse_read_range_reply(decode(&reply[..reply_len]).unwrap().body).unwrap().content_len, 4096);
        }
        _ => panic!("expected a File reply"),
    }
}

/// **A refusal says why, whole**: every reason fits a refusal uncut, and is what the storage
/// service will print.
#[test]
fn every_reason_fits_a_refusal_uncut() {
    let all = [
        Unservable::Unreadable,
        Unservable::NotFat,
        Unservable::SectorSize { bytes: u32::MAX },
        Unservable::Malformed("sectors per cluster is not a power of two"), // the longest
        Unservable::SmallClusters { bytes: 2048 },
        Unservable::Truncated,
        Unservable::StateUnwritable,
    ];
    for why in all {
        let text = format!("{why}");
        assert!(text.len() < MAX_REASON, "{} bytes: {text}", text.len());
        let mut out = [0u8; 512];
        let n = encode_refusal(&mut out, &why).unwrap();
        let m = decode(&out[..n]).unwrap();
        assert_eq!((m.op, m.flags, m.handle_count), (OP_READY, RS_FLAG_ERROR, 0));
        assert_eq!(parse_error(m.body).unwrap().msg, text.as_bytes());
    }
    assert_eq!(format!("{}", Unservable::SmallClusters { bytes: 512 }), "512-byte clusters, smaller than a page");
}
