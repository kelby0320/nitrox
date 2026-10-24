//! **A change stopped by a failed write gives back what it took**: each of its writes refused in
//! turn, and the image after held to `fsck.fat -n`, which finds a cluster lost or in two chains.

use super::*;
use crate::test_support::{Failing, FileImage, Overlay, fsck, mformat, mmd, put};
use std::rc::Rc;

const NOW: i64 = 1_791_244_800;

/// **Run `change` on `base` with each of its writes refused once in turn**, until a run refuses
/// none, checking each image: one the change succeeded on has `takes` fewer clusters free. **A
/// removal that fails may lose clusters**, since it writes its entry before it frees: its image
/// is held to the crash tests' rule, which allows that, and no more.
fn each_write_refused(
    base: &Rc<Vec<u8>>,
    what: &str,
    takes: i64,
    change: impl Fn(&Fat<Failing>) -> Result<(), FsError>,
) {
    let free = |disk: &Failing| {
        let g = *Fat::new(&disk.image).geometry().unwrap();
        Cache::count_free(&disk.image, &g).unwrap() as i64
    };
    let had = free(&Failing::once(base.clone(), u64::MAX));
    for n in 0.. {
        let disk = Failing::once(base.clone(), n);
        let out = change(&Fat::new(&disk));
        let refused = disk.writes.get() > n;
        if !refused {
            assert_eq!(out, Ok(()), "{what}: with nothing refused");
        }
        if out.is_err() && takes < 0 {
            let kept = super::crash_tests::consistent(&disk.image);
            assert_eq!(kept, Ok(()), "{what}, write {n} refused ({out:?})");
            continue;
        }
        let saved = disk.image.save(what);
        let (status, said) = fsck(&saved);
        let _ = std::fs::remove_file(&saved);
        assert_eq!(status, 0, "{what}, write {n} refused ({out:?}):\n{said}");
        if out.is_ok() {
            assert_eq!(free(&disk), had - takes, "{what}, write {n} refused");
        }
        if !refused {
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

/// **A rename stopped at any write is undone, or, where its undo could not be written either,
/// finished by its retry**, which releases nothing its other name still holds: a retry once
/// answered the file's own id as the replaced one, and the server freed it under its new name.
#[test]
fn a_rename_stopped_at_any_write_is_undone_or_finished_by_its_retry() {
    let img = mformat(64, &["-c", "8"]);
    let data = crate::test_support::pattern(10_000, 5);
    put(&img, "F.TXT", &data);
    put(&img, "G.TXT", b"old");
    let base = Rc::new(std::fs::read(&img).unwrap());
    let _ = std::fs::remove_file(&img);
    let read = |fat: &Fat<Overlay>, path: &[u8]| {
        let mut out = vec![0u8; 64 * 1024];
        fat.read_file(path, &mut out).map(|n| out[..n].to_vec())
    };
    for (to, replace, there) in [(&b"/a new name.txt"[..], false, None), (b"/G.TXT", true, Some(&b"old"[..]))] {
        for once in [true, false] {
            for n in 0.. {
                let disk = if once { Failing::once(base.clone(), n) } else { Failing::after(base.clone(), n) };
                let what =
                    format!("{} write {n} refused, {}", String::from_utf8_lossy(to), ["on", "once"][once as usize]);
                let first = Fat::new(&disk).rename_path(b"/F.TXT", to, replace, NOW);
                if disk.writes.get() <= n {
                    assert!(first.is_ok(), "{what}: {first:?}");
                    break;
                }
                let fresh = Fat::new(&disk.image);
                if once {
                    assert!(read(&fresh, b"/F.TXT") == Ok(data.clone()), "{what}: undone");
                    assert!(read(&fresh, to).ok().as_deref() == there, "{what}: undone, the new name as it was");
                }
                let id = fresh.rename_path(b"/F.TXT", to, replace, NOW);
                let id = id.unwrap_or_else(|e| panic!("{what}: the retry: {e:?}"));
                if let Some(id) = id {
                    fresh.release(id, NOW).unwrap();
                }
                fresh.create_file(b"/", b"OTHER.TXT", NOW).unwrap();
                Volume::grow_file(&fresh, b"/OTHER.TXT", 1 << 20, NOW).unwrap();
                assert!(read(&fresh, to) == Ok(data.clone()), "{what}: the file kept its clusters");
                if once {
                    let saved = disk.image.save("rename");
                    let (status, said) = fsck(&saved);
                    let _ = std::fs::remove_file(&saved);
                    assert_eq!(status, 0, "{what}:\n{said}");
                }
            }
        }
    }
}

/// **A directory moved and stopped at any write is undone, or finished by its retry**, with its `..`
/// naming the parent its name is in: the old name went before `..` was rewritten, so a move refused
/// there was made anyway, its `..` naming the parent it had left. One whose `..` is not one is
/// refused before anything is written.
#[test]
fn a_directory_moved_and_stopped_at_any_write_is_undone_or_finished_by_its_retry() {
    let img = mformat(64, &["-c", "8"]);
    mmd(&img, "A");
    mmd(&img, "B");
    mmd(&img, "A/D");
    put(&img, "A/D/IN.TXT", b"in");
    let base = Rc::new(std::fs::read(&img).unwrap());
    let _ = std::fs::remove_file(&img);
    let parent_is = |fat: &Fat<Overlay>, path: &[u8], p: &[u8]| {
        let g = *fat.geometry().unwrap();
        let d = Dir::from_id(&g, fat.resolve_dir(path).unwrap()).unwrap();
        let p = fat.resolve_dir(p).unwrap();
        dir::parent(fat.r, &g, &mut fat.cache.borrow_mut(), d).map(Dir::id) == Ok(p)
    };
    for once in [true, false] {
        for n in 0.. {
            let disk = if once { Failing::once(base.clone(), n) } else { Failing::after(base.clone(), n) };
            let what = format!("write {n} refused, {}", ["on", "once"][once as usize]);
            let first = Fat::new(&disk).rename_path(b"/A/D", b"/B/D", false, NOW);
            if disk.writes.get() <= n {
                assert!(first.is_ok(), "{what}: {first:?}");
                break;
            }
            let fresh = Fat::new(&disk.image);
            if once {
                assert!(fresh.resolve_dir(b"/A/D").is_ok() && fresh.resolve_dir(b"/B/D").is_err(), "{what}: undone");
                assert!(parent_is(&fresh, b"/A/D", b"/A"), "{what}: undone, `..` too");
            }
            let retried = fresh.rename_path(b"/A/D", b"/B/D", false, NOW);
            assert_eq!(retried, Ok(None), "{what}: the retry");
            assert!(fresh.resolve_dir(b"/A/D").is_err(), "{what}: one name");
            assert!(parent_is(&fresh, b"/B/D", b"/B"), "{what}: `..` names the parent its name is in");
            if once {
                let saved = disk.image.save("move");
                let (status, said) = fsck(&saved);
                let _ = std::fs::remove_file(&saved);
                assert_eq!(status, 0, "{what}:\n{said}");
            }
        }
    }

    let disk = Failing::after(base.clone(), 0);
    let fat = Fat::new(&disk.image);
    let d = fat.resolve_dir(b"/A/D").unwrap();
    let g = *fat.geometry().unwrap();
    let d = Dir::from_id(&g, d).unwrap();
    let at = dir::slot_byte(&disk.image, &g, &mut fat.cache.borrow_mut(), d, 1).unwrap().unwrap();
    disk.image.write_at(at, b"NOTDOTDOT  ").unwrap();
    assert_eq!(Fat::new(&disk).rename_path(b"/A/D", b"/B/D", false, NOW), Err(FsError::Corrupt));
    assert_eq!(disk.writes.get(), 0, "refused before anything is written");
}

/// **A refusal comes before anything is written**: each answered as itself on a device refusing
/// every write, where a write made first would answer `Io` and, unrefused, change the volume.
#[test]
fn a_refusal_comes_before_anything_is_written() {
    let img = mformat(64, &["-c", "8"]);
    mmd(&img, "D");
    put(&img, "D/IN.TXT", b"in");
    let disk = Failing::after(Rc::new(std::fs::read(&img).unwrap()), 0);
    let _ = std::fs::remove_file(&img);
    let fat = Fat::new(&disk);
    let root = fat.resolve_dir(b"/").unwrap();
    assert_eq!(fat.unlink_at(root, b"D", NOW), Err(FsError::Unsupported), "a directory unlinked");
    assert_eq!(fat.mkdir_at(root, b"a:b", NOW), Err(FsError::InvalidName), "a name a FAT cannot hold");
    assert_eq!(disk.writes.get(), 0, "nothing written");
}

/// **Every other change stopped at any write loses no cluster and leaves none in two chains**: a
/// file made, a directory made and removed, a file removed and released, and one truncated.
#[test]
fn every_change_stopped_at_any_write_leaves_a_clean_volume() {
    let img = mformat(64, &["-c", "8"]);
    mmd(&img, "EMPTY");
    put(&img, "BIG.BIN", &[9u8; 40_000]);
    put(&img, "GONE.BIN", &[8u8; 40_000]);
    let base = Rc::new(std::fs::read(&img).unwrap());
    let _ = std::fs::remove_file(&img);
    let root = |fat: &Fat<Failing>| fat.resolve_dir(b"/");
    each_write_refused(&base, "a create", 0, |fat| fat.create_file(b"/", b"a file with a long name", NOW));
    each_write_refused(&base, "a mkdir", 1, |fat| fat.mkdir_at(root(fat)?, b"a directory", NOW));
    each_write_refused(&base, "an rmdir", -1, |fat| fat.rmdir_at(root(fat)?, b"EMPTY", NOW, drop));
    each_write_refused(&base, "an unlink", -10, |fat| {
        let id = fat.unlink_at(root(fat)?, b"GONE.BIN", NOW)?;
        fat.release(id.unwrap(), NOW)
    });
    each_write_refused(&base, "a truncate", -9, |fat| {
        let id = Volume::truncate_file(fat, b"/BIG.BIN", 100, NOW)?;
        id.map_or(Ok(()), |id| fat.release(id, NOW))
    });
}
