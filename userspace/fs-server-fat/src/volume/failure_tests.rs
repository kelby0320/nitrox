//! **A change stopped by a failed write gives back what it took**: each of its writes refused in
//! turn, and the image after held to `fsck.fat -n`, which finds a cluster lost or in two chains.

use super::*;
use crate::test_support::{Failing, FileImage, fsck, mformat, mmd, put};
use std::rc::Rc;

const NOW: i64 = 1_791_244_800;

/// **Run `change` on `base` with each of its writes refused once in turn**, until a run refuses
/// none, checking each image: one the change succeeded on has `takes` fewer clusters free.
fn each_write_refused(
    base: &Rc<Vec<u8>>,
    what: &str,
    takes: u32,
    change: impl Fn(&Fat<Failing>) -> Result<(), FsError>,
) {
    let free = |disk: &Failing| {
        let g = *Fat::new(&disk.image).geometry().unwrap();
        Cache::count_free(&disk.image, &g).unwrap()
    };
    let had = free(&Failing::once(base.clone(), u64::MAX));
    for n in 0.. {
        let disk = Failing::once(base.clone(), n);
        let out = change(&Fat::new(&disk));
        let saved = disk.image.save(what);
        let (status, said) = fsck(&saved);
        let _ = std::fs::remove_file(&saved);
        assert_eq!(status, 0, "{what}, write {n} refused ({out:?}):\n{said}");
        if out.is_ok() {
            assert_eq!(free(&disk), had - takes, "{what}, write {n} refused");
        }
        if disk.writes.get() <= n {
            assert_eq!(out, Ok(()), "{what}: with nothing refused");
            break;
        }
    }
}

/// **A grow stopped at any write loses no cluster**: a file with a cluster and an empty one, each
/// grown past one zeroing write, so a write inside the allocation is refused too.
#[test]
fn a_grow_stopped_at_any_write_loses_no_cluster() {
    let img = mformat(64, &["-c", "8"]);
    put(&img, "A.TXT", b"a");
    put(&img, "E.TXT", b"");
    let base = Rc::new(std::fs::read(&img).unwrap());
    let _ = std::fs::remove_file(&img);
    let size = 100 * 1024;
    each_write_refused(&base, "a grow", 24, |fat| Volume::grow_file(fat, b"/A.TXT", size, NOW).map(drop));
    each_write_refused(&base, "an empty file's grow", 25, |fat| Volume::grow_file(fat, b"/E.TXT", size, NOW).map(drop));
}

/// **A directory grown for a new name, stopped at any write, loses no cluster**: the cluster taken
/// for it was left chained to nothing when its zeroing or the FAT's write failed.
#[test]
fn a_directory_grown_and_stopped_at_any_write_loses_no_cluster() {
    let img = mformat(64, &["-c", "8"]);
    mmd(&img, "D");
    let disk = FileImage::open(&img);
    let fat = Fat::new(&disk);
    let per = fat.geometry().unwrap().cluster_bytes() / ENTRY;
    for i in 0..per - 2 {
        fat.create_file(b"/D", format!("F{i:03}.TXT").as_bytes(), NOW).unwrap();
    }
    let base = Rc::new(std::fs::read(&img).unwrap());
    each_write_refused(&base, "a new name", 1, |fat| fat.create_file(b"/D", b"NEW.TXT", NOW));
}
