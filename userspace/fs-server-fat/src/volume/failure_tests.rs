//! **A change stopped by a failed write gives back what it took**: each of its writes refused in
//! turn, and the image after held to `fsck.fat -n`, which finds a cluster lost or in two chains.

use super::*;
use crate::test_support::{Failing, FileImage, Overlay, fsck, mformat, mmd, put};
use std::rc::Rc;

const NOW: i64 = 1_791_244_800;

/// **How a write fails**: once, refused and changing nothing, or landed and answered `Io` all the
/// same, as a device may answer a write it made; or every write from one on, refused, the first of
/// them landed or not; or a write landed and the next refused, the rest made. **One failed write
/// alone never leaves a mount refusing changes**: its undo can be written.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Once,
    On,
    Landed,
    LandedThenOne,
    LandedOn,
}

impl Mode {
    /// Whether an undo can be written, so nothing may be lost nor the mount left refusing.
    fn undoable(self) -> bool {
        matches!(self, Mode::Once | Mode::Landed)
    }
}

fn under(base: &Rc<Vec<u8>>, mode: Mode, n: u64) -> Failing {
    match mode {
        Mode::Once => Failing::once(base.clone(), n),
        Mode::On => Failing::after(base.clone(), n),
        Mode::Landed => Failing::landed(base.clone(), n),
        Mode::LandedThenOne => Failing::landed_then_one(base.clone(), n),
        Mode::LandedOn => Failing::landed_then_after(base.clone(), n),
    }
}

/// **Run `change` on `base` with each of its writes failing once in turn, refused and landed**,
/// until a run fails none, checking each image: one the change succeeded on has `takes` fewer
/// clusters free. **A removal that fails may lose clusters**, since it writes its entry before it
/// frees, and so may a change whose undo cannot be written: those images are held to the crash
/// tests' rule, which allows that, and no more.
fn each_write_failing(
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
    for how in [Mode::Once, Mode::Landed, Mode::LandedThenOne, Mode::LandedOn] {
        for n in 0.. {
            let disk = under(base, how, n);
            let fat = Fat::new(&disk);
            let out = change(&fat);
            assert!(!how.undoable() || !Volume::refusing_changes(&fat), "{what}, write {n} {how:?}: refusing");
            let failed = disk.writes.get() > n;
            if !failed {
                assert_eq!(out, Ok(()), "{what}: with nothing failing");
            }
            if out.is_err() && (takes < 0 || !how.undoable()) {
                let kept = super::crash_tests::consistent(&disk.image);
                assert_eq!(kept, Ok(()), "{what}, write {n} {how:?} ({out:?})");
                continue;
            }
            let saved = disk.image.save(what);
            let (status, said) = fsck(&saved);
            let _ = std::fs::remove_file(&saved);
            assert_eq!(status, 0, "{what}, write {n} {how:?} ({out:?}):\n{said}");
            if out.is_ok() {
                assert_eq!(free(&disk), had - takes, "{what}, write {n} {how:?}");
            }
            if !failed {
                break;
            }
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
    each_write_failing(&base, "a grow", 24, |fat| Volume::grow_file(fat, b"/A.TXT", size, NOW).map(drop));
    each_write_failing(&base, "an empty file's grow", 25, |fat| Volume::grow_file(fat, b"/E.TXT", size, NOW).map(drop));
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
    each_write_failing(&base, "a new name", 1, |fat| fat.create_file(b"/D", b"NEW.TXT", NOW));
}

/// **A rename stopped at any write leaves the file under a name**: undone where its undo can be
/// written, made where the write that makes it landed, and otherwise under both names on a mount
/// refusing changes, finished by its retry, which releases nothing its other name still holds. A
/// retry once answered the file's own id as the replaced one, and the server freed it under its
/// new name; an undo once deleted the new name after the old one's deletion had landed.
#[test]
fn a_rename_stopped_at_any_write_leaves_the_file_under_a_name() {
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
        for mode in [Mode::Once, Mode::On, Mode::Landed, Mode::LandedThenOne, Mode::LandedOn] {
            for n in 0.. {
                let disk = under(&base, mode, n);
                let what = format!("{} write {n} {mode:?}", String::from_utf8_lossy(to));
                let fat = Fat::new(&disk);
                let first = fat.rename_path(b"/F.TXT", to, replace, NOW);
                if disk.writes.get() <= n {
                    assert!(first.is_ok(), "{what}: {first:?}");
                    break;
                }
                let fresh = Fat::new(&disk.image);
                let (old, new) = (read(&fresh, b"/F.TXT") == Ok(data.clone()), read(&fresh, to) == Ok(data.clone()));
                assert!(old || new, "{what} ({first:?}): the file under neither name");
                if mode.undoable() {
                    assert_eq!(first.is_ok(), new && !old, "{what}: Ok exactly when it was made");
                }
                if old && new {
                    assert!(Volume::refusing_changes(&fat), "{what}: under both names, on a mount still changing");
                }
                if mode.undoable() && first.is_err() {
                    assert!(old && read(&fresh, to).ok().as_deref() == there, "{what}: undone");
                    assert!(!Volume::refusing_changes(&fat), "{what}: undone, and refusing");
                }
                match first {
                    // Every write after the one that failed is refused: the release, too.
                    Ok(Some(_)) if mode == Mode::LandedOn || mode == Mode::On => {}
                    Ok(Some(id)) => fat.release(id, NOW).unwrap(),
                    Ok(None) => {}
                    Err(_) if old => {
                        let id = fresh.rename_path(b"/F.TXT", to, replace, NOW);
                        if let Some(id) = id.unwrap_or_else(|e| panic!("{what}: the retry: {e:?}")) {
                            fresh.release(id, NOW).unwrap();
                        }
                    }
                    Err(_) => {}
                }
                let after = Fat::new(&disk.image);
                after.create_file(b"/", b"OTHER.TXT", NOW).unwrap();
                Volume::grow_file(&after, b"/OTHER.TXT", 1 << 20, NOW).unwrap();
                assert!(read(&after, to) == Ok(data.clone()), "{what}: the file kept its clusters");
                if mode.undoable() {
                    let saved = disk.image.save("rename");
                    let (status, said) = fsck(&saved);
                    let _ = std::fs::remove_file(&saved);
                    assert_eq!(status, 0, "{what}:\n{said}");
                }
            }
        }
    }
}

/// **A directory moved and stopped at any write is under a name, its `..` naming a parent holding
/// it**, as a rename is, and its retry finishes it: the old name went before `..` was rewritten,
/// so a move refused there was made anyway, its `..` naming the parent it had left. One whose `..`
/// is not one is refused before anything is written.
#[test]
fn a_directory_moved_and_stopped_at_any_write_is_under_a_name() {
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
    for mode in [Mode::Once, Mode::On, Mode::Landed, Mode::LandedThenOne, Mode::LandedOn] {
        for n in 0.. {
            let disk = under(&base, mode, n);
            let what = format!("write {n} {mode:?}");
            let fat = Fat::new(&disk);
            let first = fat.rename_path(b"/A/D", b"/B/D", false, NOW);
            if disk.writes.get() <= n {
                assert!(first.is_ok(), "{what}: {first:?}");
                break;
            }
            let fresh = Fat::new(&disk.image);
            let (old, new) = (fresh.resolve_dir(b"/A/D").is_ok(), fresh.resolve_dir(b"/B/D").is_ok());
            assert!(old || new, "{what} ({first:?}): the directory under neither name");
            if mode.undoable() {
                assert_eq!(first.is_ok(), new && !old, "{what}: Ok exactly when it was made");
            }
            if old && new {
                assert!(Volume::refusing_changes(&fat), "{what}: under both names, on a mount still changing");
            }
            let held = if old { parent_is(&fresh, b"/A/D", b"/A") } else { parent_is(&fresh, b"/B/D", b"/B") };
            assert!(held || (old && new), "{what}: `..` names a parent that lost it");
            if mode.undoable() && first.is_err() {
                assert!(old && !new && parent_is(&fresh, b"/A/D", b"/A"), "{what}: undone, `..` too");
                assert!(!Volume::refusing_changes(&fat), "{what}: undone, and refusing");
            }
            if first.is_err() && old {
                let retried = fresh.rename_path(b"/A/D", b"/B/D", false, NOW);
                assert_eq!(retried, Ok(None), "{what}: the retry");
            }
            assert!(fresh.resolve_dir(b"/A/D").is_err(), "{what}: one name");
            assert!(parent_is(&fresh, b"/B/D", b"/B"), "{what}: `..` names the parent its name is in");
            if mode.undoable() {
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
    each_write_failing(&base, "a create", 0, |fat| fat.create_file(b"/", b"a file with a long name", NOW));
    each_write_failing(&base, "a mkdir", 1, |fat| fat.mkdir_at(root(fat)?, b"a directory", NOW));
    each_write_failing(&base, "an rmdir", -1, |fat| fat.rmdir_at(root(fat)?, b"EMPTY", NOW, drop));
    each_write_failing(&base, "an unlink", -10, |fat| {
        let id = fat.unlink_at(root(fat)?, b"GONE.BIN", NOW)?;
        fat.release(id.unwrap(), NOW)
    });
    each_write_failing(&base, "a truncate", -9, |fat| {
        let id = Volume::truncate_file(fat, b"/BIG.BIN", 100, NOW)?;
        id.map_or(Ok(()), |id| fat.release(id, NOW))
    });
}

/// **A mount that finds its filesystem corrupt refuses every change after**, as Linux's FAT
/// remounts itself read-only: after a rename whose undo failed too, leaving the file under both
/// names, and after a change that found the volume corrupt. Nothing more is written, its unmount
/// included, and the next mount changes it again. A failed write alone refuses nothing.
#[test]
fn a_mount_that_finds_its_filesystem_corrupt_refuses_every_change_after() {
    // FAT32, whose unmount writes FSInfo: an unmount that wrote anything would show.
    let img = mformat(64, &["-F", "-c", "8"]);
    put(&img, "F.TXT", b"f");
    let base = Rc::new(std::fs::read(&img).unwrap());
    let _ = std::fs::remove_file(&img);
    let refuses = |fat: &Fat<Failing>, disk: &Failing, what: &str| {
        let writes = disk.writes.get();
        assert_eq!(fat.create_file(b"/", b"after.txt", NOW), Err(FsError::ReadOnly), "{what}: a change after");
        assert!(Volume::refusing_changes(fat), "{what}: said");
        assert_eq!(fat.mark_clean(), Ok(()), "{what}: its unmount");
        assert_eq!(disk.writes.get(), writes, "{what}: nothing more written");
        assert_eq!(fat.was_left_clean(), Ok(false), "{what}: left marked for a check");
        let again = Fat::new(&disk.image);
        assert_eq!(again.create_file(b"/", b"after.txt", NOW), Ok(()), "{what}: the next mount");
    };

    let both = (1..32).find_map(|n| {
        let disk = Failing::after(base.clone(), n);
        let fat = Fat::new(&disk);
        fat.mark_mounted().ok()?;
        let moved = fat.rename_path(b"/F.TXT", b"/a new name.txt", false, NOW);
        let image = Fat::new(&disk.image);
        let twice =
            image.read_file(b"/F.TXT", &mut [0; 8]).is_ok() && image.read_file(b"/a new name.txt", &mut [0; 8]).is_ok();
        (moved.is_err() && twice).then_some(n)
    });
    let n = both.expect("a write whose refusal leaves the file under both names");
    let disk = Failing::after(base.clone(), n);
    let fat = Fat::new(&disk);
    fat.mark_mounted().unwrap();
    assert!(fat.rename_path(b"/F.TXT", b"/a new name.txt", false, NOW).is_err());
    refuses(&fat, &disk, &format!("the undo refused from write {n}"));

    let disk = Failing::once(base.clone(), u64::MAX);
    let fat = Fat::new(&disk);
    let g = *fat.geometry().unwrap();
    let f = dir::lookup(&disk, &g, &mut fat.cache.borrow_mut(), Dir::root(&g), b"F.TXT").unwrap();
    let at = dir::slot_byte(&disk, &g, &mut fat.cache.borrow_mut(), f.dir, f.slot).unwrap().unwrap();
    disk.image.write_at(at + 28, &(3 * g.cluster_bytes()).to_le_bytes()).unwrap();
    let fat = Fat::new(&disk);
    fat.mark_mounted().unwrap();
    assert_eq!(Volume::grow_file(&fat, b"/F.TXT", 20_000, NOW), Err(FsError::Corrupt));
    refuses(&fat, &disk, "a chain shorter than its size");

    let mounting = Failing::once(base.clone(), u64::MAX);
    Fat::new(&mounting).mark_mounted().unwrap();
    let disk = Failing::once(base, mounting.writes.get());
    let fat = Fat::new(&disk);
    fat.mark_mounted().unwrap();
    assert_eq!(fat.create_file(b"/", b"x", NOW), Err(FsError::Io), "the change's first write refused");
    assert_eq!(fat.create_file(b"/", b"y", NOW), Ok(()), "and the next change made");
    assert!(!Volume::refusing_changes(&fat));
}

/// **A FAT12 of 512-byte clusters whose `/p` has one slot left in its first cluster**, and a big
/// file after it, so the next long name made there straddles two clusters that are not adjacent:
/// two writes. `make` makes something there, or nothing.
fn straddling(make: impl FnOnce(&Fat<FileImage>)) -> Rc<Vec<u8>> {
    let img = crate::test_support::mkfs(2, &["-F", "12", "-s", "1"]);
    mmd(&img, "p");
    for i in 0..13 {
        put(&img, &format!("p/F{i}.TXT"), b"x");
    }
    put(&img, "big.bin", &[6u8; 50 * 512]);
    let disk = FileImage::open(&img);
    make(&Fat::new(&disk));
    Rc::new(std::fs::read(&img).unwrap())
}

/// **A long name across two clusters is written, and unwritten, whole**: a create, a rename, an
/// unlink and an rmdir, each of their writes refused and landed in turn. A failed create left the
/// name's first part orphaned; a failed rename, and a failed unlink, left the file under its short
/// alias alone, its long name's first part deleted. `fsck.fat -n` passes that, so names are read.
#[test]
fn a_long_name_across_two_clusters_is_written_and_unwritten_whole() {
    let long = b"a long source name.txt";
    let listed = |img: &Overlay| {
        let fat = Fat::new(img);
        let mut names = Vec::new();
        fat.read_dir(fat.resolve_dir(b"/p").unwrap(), 0, |e| {
            names.push(String::from_utf8_lossy(e.name).into_owned());
            true
        })
        .unwrap();
        names
    };
    // Where no undo can be written, clusters may be lost; else nothing may be.
    let clean = |img: &Overlay, mode: Mode, what: &str| {
        if !mode.undoable() {
            assert_eq!(super::crash_tests::consistent(img), Ok(()), "{what}");
            return;
        }
        let saved = img.save("straddle");
        let (status, said) = fsck(&saved);
        let _ = std::fs::remove_file(&saved);
        assert_eq!(status, 0, "{what}:\n{said}");
    };
    let made = straddling(|fat| {
        fat.create_file(b"/p", long, NOW).unwrap();
        Volume::grow_file(fat, b"/p/a long source name.txt", 1000, NOW).unwrap();
    });
    let dir_made =
        straddling(|fat| fat.mkdir_at(fat.resolve_dir(b"/p").unwrap(), b"a long directory name", NOW).unwrap());
    let empty = straddling(|_| {});
    for mode in [Mode::Once, Mode::Landed, Mode::LandedThenOne, Mode::LandedOn] {
        for n in 0.. {
            let disk = under(&empty, mode, n);
            let fat = Fat::new(&disk);
            let out = fat.create_file(b"/p", long, NOW);
            let refusing = Volume::refusing_changes(&fat);
            let names = listed(&disk.image);
            let what = format!("a create, write {n} {mode:?} ({out:?}): {names:?}");
            assert!(!(mode.undoable() && refusing), "{what}: refusing");
            let there = names.iter().any(|s| s.as_bytes() == long);
            assert!(out.is_ok() == there || refusing, "{what}: Ok exactly when made");
            assert!(there || !names.iter().any(|s| s.starts_with("ALONG")), "{what}: a short alias alone");
            if !refusing {
                clean(&disk.image, mode, &what);
            }
            if disk.writes.get() <= n {
                break;
            }
        }
        for n in 0.. {
            let disk = under(&made, mode, n);
            let fat = Fat::new(&disk);
            let out = fat.rename_path(b"/p/a long source name.txt", b"/p/X.TXT", false, NOW);
            let refusing = Volume::refusing_changes(&fat);
            let names = listed(&disk.image);
            let what = format!("a rename, write {n} {mode:?} ({out:?}): {names:?}");
            assert!(!(mode.undoable() && refusing), "{what}: refusing");
            let alias = names.iter().any(|s| s.starts_with("ALONGS"));
            let (old, new) = (names.iter().any(|s| s.as_bytes() == long), names.iter().any(|s| s == "X.TXT"));
            assert!(old || new || alias, "{what}: under no name");
            assert!(!alias || refusing, "{what}: a short alias, on a mount still changing");
            assert!(!(old && new) || refusing, "{what}: under both names, on a mount still changing");
            if mode.undoable() {
                assert_eq!(out.is_ok(), new && !old && !alias, "{what}: Ok exactly when made");
            }
            if !refusing {
                clean(&disk.image, mode, &what);
            }
            if disk.writes.get() <= n {
                break;
            }
        }
        for n in 0.. {
            let disk = under(&made, mode, n);
            let fat = Fat::new(&disk);
            let p = fat.resolve_dir(b"/p").unwrap();
            let out = fat.unlink_at(p, long, NOW);
            let names = listed(&disk.image);
            let what = format!("an unlink, write {n} {mode:?} ({out:?}): {names:?}");
            // Its short alias alone is left only where the part deleted could not be written back.
            let (there, alias) =
                (names.iter().any(|s| s.as_bytes() == long), names.iter().any(|s| s.starts_with("ALONGS")));
            assert!(!(out.is_ok() && (there || alias)), "{what}: Ok, and still there");
            if mode.undoable() {
                assert_eq!(out.is_ok(), !there && !alias, "{what}: Ok exactly when removed");
            }
            assert!(!alias || (!mode.undoable() && out.is_err()), "{what}: a short alias");
            if disk.writes.get() <= n {
                break;
            }
        }
        for n in 0.. {
            let disk = under(&dir_made, mode, n);
            let fat = Fat::new(&disk);
            let p = fat.resolve_dir(b"/p").unwrap();
            let out = fat.rmdir_at(p, b"a long directory name", NOW, drop);
            let names = listed(&disk.image);
            let what = format!("an rmdir, write {n} {mode:?} ({out:?}): {names:?}");
            // Its name goes before its chain is freed: a failure freeing it loses the clusters.
            let (there, alias) =
                (names.iter().any(|s| s == "a long directory name"), names.iter().any(|s| s.starts_with("ALONGD")));
            assert!(!(out.is_ok() && (there || alias)), "{what}: Ok, and still there");
            assert!(!alias || (!mode.undoable() && out.is_err()), "{what}: a short alias");
            assert_eq!(super::crash_tests::consistent(&disk.image), Ok(()), "{what}");
            if disk.writes.get() <= n {
                break;
            }
        }
    }
}
