//! **A change stopped at any write**: each write refused once, as a device that fails and recovers
//! would, or every write from it on, as a crash would leave the disk.

use super::*;
use crate::test_support::{Failing, fixture};

const NOW: i64 = 1_791_244_800;

/// `/d/src` and `/e/dst`, files with data, and `/d/sub`, an empty directory; `/e/keep` lifts `/e`'s
/// count off the two a directory never goes below.
fn base() -> Vec<u8> {
    let rw = Failing::after(fixture(4096, b"seed\n"), u64::MAX);
    let vol = Ext4(&rw);
    vol.mkdir_at(2, b"d", NOW).unwrap();
    vol.mkdir_at(2, b"e", NOW).unwrap();
    vol.mkdir_at(vol.resolve_dir(b"/d").unwrap(), b"sub", NOW).unwrap();
    vol.mkdir_at(vol.resolve_dir(b"/e").unwrap(), b"keep", NOW).unwrap();
    for (parent, name, size) in [(b"/d" as &[u8], b"src" as &[u8], 9000), (b"/e", b"dst", 5000)] {
        vol.create_file(parent, name, NOW).unwrap();
        vol.grow_file(&[parent, b"/", name].concat(), size, NOW).unwrap();
    }
    rw.image.0.into_inner()
}

/// What `e2fsck -fn` says of `img`.
fn e2fsck(img: &[u8]) -> String {
    let dir = std::env::temp_dir().join(format!("nitrox-ext4-crash-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("{:?}.img", std::thread::current().id()));
    std::fs::write(&p, img).unwrap();
    let out = std::process::Command::new("e2fsck").args(["-fn", p.to_str().unwrap()]).output().unwrap();
    let _ = std::fs::remove_file(&p);
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

/// How many writes `op` makes on a device that refuses none.
fn writes(base: &[u8], op: impl Fn(&Ext4<'_, Failing>)) -> u64 {
    let rw = Failing::after(base.to_vec(), u64::MAX);
    op(&Ext4(&rw));
    rw.writes.get()
}

/// Remove whatever `path` names, file or directory, releasing what that frees as the server does.
fn remove(vol: &Ext4<'_, crate::test_support::RwImage>, path: &[u8]) {
    let slash = path.iter().rposition(|&b| b == b'/').unwrap();
    let Ok(dir) = vol.resolve_dir(&path[..slash]) else { return };
    let name = &path[slash + 1..];
    match vol.unlink_at(dir, name, NOW) {
        Ok(Some(id)) => drop(vol.release(id, NOW)),
        Ok(None) => {}
        Err(_) => {
            let _ = vol.rmdir_at(dir, name, NOW, |_| ());
        }
    }
}

/// What no stop may leave: a name reaching a freed inode, a block in two files, a directory twice.
const NEVER: [&str; 3] = ["deleted/unused inode", "Multiply-claimed", "is a link to directory"];

/// Whether `e2fsck` counted an inode more names than links, so one name's removal frees what
/// another reaches. More links than names only leaks.
fn fewer_links_than_names(text: &str) -> bool {
    text.lines().filter_map(|l| l.split_once("ref count is ")).any(|(_, rest)| {
        let n: Vec<u32> = rest.split(|c: char| !c.is_ascii_digit()).filter_map(|s| s.parse().ok()).collect();
        n.len() >= 2 && n[0] < n[1]
    })
}

/// **A rename stopped at any write never leaves a name reaching what is freed**: then, each on its
/// own copy, the rename asked again or either name removed, and a file grown into what was freed.
#[test]
fn a_rename_stopped_at_any_write_never_leaves_a_name_reaching_what_is_freed() {
    let base = base();
    for (from, to, replace) in
        [(b"/d/src" as &[u8], b"/e/dst" as &[u8], true), (b"/d/src", b"/e/new", false), (b"/d/sub", b"/e/sub", false)]
    {
        let total = writes(&base, |vol| {
            let _ = vol.rename_path(from, to, replace, NOW);
        });
        for n in 0..=total {
            for once in [true, false] {
                let rw = if once { Failing::once(base.clone(), n) } else { Failing::after(base.clone(), n) };
                let first = Ext4(&rw).rename_path(from, to, replace, NOW);
                if let Ok(Some(id)) = first {
                    let _ = Ext4(&rw.image).release(id, NOW);
                }
                let stopped = rw.image.0.into_inner();
                let thens = ["asked again", "the old name removed", "the new name removed", "moved on", "moved into"];
                for then in thens {
                    let rw = crate::test_support::RwImage(std::cell::RefCell::new(stopped.clone()));
                    let vol = Ext4(&rw);
                    match then {
                        "asked again" => {
                            if let Ok(Some(id)) = vol.rename_path(from, to, replace, NOW) {
                                let _ = vol.release(id, NOW);
                            }
                        }
                        "the old name removed" => remove(&vol, from),
                        "the new name removed" => remove(&vol, to),
                        "moved on" => {
                            if let Ok(Some(id)) = vol.rename_path(to, b"/d/onward", false, NOW) {
                                let _ = vol.release(id, NOW);
                            }
                        }
                        _ => {
                            let into = vol.rename_path(b"/e", b"/e/sub/e", false, NOW);
                            assert!(into.is_err(), "{from:?} -> {to:?}, write {n} refused: /e moved into itself");
                        }
                    }
                    let _ = vol.create_file(b"/e", b"fresh", NOW);
                    let _ = vol.grow_file(b"/e/fresh", 20000, NOW);
                    let text = e2fsck(&rw.0.borrow());
                    let how = if once { "once" } else { "and every one after" };
                    let ok = NEVER.iter().all(|bad| !text.contains(bad)) && !fewer_links_than_names(&text);
                    assert!(ok, "{from:?} -> {to:?}, write {n} of {total} refused {how}, {then}:\n{text}");
                }
            }
        }
    }
}

/// **A name taken away returns what it freed, whatever write is refused after**: the server
/// releases an orphan only on `Ok`, so an error once the name was gone lost the file for good.
#[test]
fn a_name_taken_away_returns_what_it_freed_whatever_write_is_refused_after() {
    let base = base();
    let dst = Ext4(&crate::test_support::RwImage(std::cell::RefCell::new(base.clone())))
        .map_file(b"/e/dst", &mut [BlockRun::default(); 4])
        .unwrap()
        .id;
    type Op = fn(&Ext4<'_, Failing>) -> Result<Option<u64>, FsError>;
    let ops: [(&str, Op); 2] = [
        ("unlink", |vol| vol.unlink_at(vol.resolve_dir(b"/e")?, b"dst", NOW)),
        ("rename over", |vol| vol.rename_path(b"/d/src", b"/e/dst", true, NOW)),
    ];
    for (what, op) in ops {
        let total = writes(&base, |vol| {
            let _ = op(vol);
        });
        for n in 0..total {
            let rw = Failing::once(base.clone(), n);
            if let Ok(Some(id)) = op(&Ext4(&rw)) {
                Ext4(&rw.image).release(id, NOW).unwrap();
            }
            let text = e2fsck(&rw.image.0.borrow());
            let lost = format!("Unattached inode {dst}\n");
            assert!(!text.contains(&lost), "{what}, write {n} of {total} refused:\n{text}");
        }
    }
}

/// **What a change answers is what it did**, with every write refused once: `Ok` exactly when a
/// name changed. An error after a change told the client the opposite, or was not undone.
#[test]
fn what_a_change_answers_is_what_it_did() {
    type Op = fn(&Ext4<'_, Failing>) -> Result<(), FsError>;
    type Done = fn(&Ext4<'_, crate::test_support::RwImage>) -> bool;
    fn file(vol: &Ext4<'_, crate::test_support::RwImage>, p: &[u8]) -> bool {
        vol.map_file(p, &mut [BlockRun::default(); 4]).is_ok()
    }
    let base = base();
    let changes: [(&str, Op, Done); 7] = [
        ("create", |v| v.create_file(b"/e", b"made", NOW), |v| file(v, b"/e/made")),
        ("mkdir", |v| v.mkdir_at(v.resolve_dir(b"/e")?, b"made", NOW), |v| v.resolve_dir(b"/e/made").is_ok()),
        ("unlink", |v| v.unlink_at(v.resolve_dir(b"/e")?, b"dst", NOW).map(drop), |v| !file(v, b"/e/dst")),
        ("rmdir", |v| v.rmdir_at(v.resolve_dir(b"/d")?, b"sub", NOW, |_| ()), |v| v.resolve_dir(b"/d/sub").is_err()),
        (
            "rename within",
            |v| v.rename_at(v.resolve_dir(b"/e")?, b"dst", b"moved", NOW),
            |v| file(v, b"/e/moved") || !file(v, b"/e/dst"),
        ),
        (
            "rename over",
            |v| v.rename_path(b"/d/src", b"/e/dst", true, NOW).map(drop),
            |v| v.map_file(b"/e/dst", &mut [BlockRun::default(); 4]).map(|m| m.size) == Ok(9000) || !file(v, b"/d/src"),
        ),
        (
            "directory moved",
            |v| v.rename_path(b"/d/sub", b"/e/sub", false, NOW).map(drop),
            |v| v.resolve_dir(b"/e/sub").is_ok() || v.resolve_dir(b"/d/sub").is_err(),
        ),
    ];
    for (what, op, done) in changes {
        let total = writes(&base, |vol| {
            let _ = op(vol);
        });
        for n in 0..total {
            let rw = Failing::once(base.clone(), n);
            let answered = op(&Ext4(&rw));
            let changed = done(&Ext4(&rw.image));
            assert_eq!(answered.is_ok(), changed, "{what}, write {n} of {total} refused: answered {answered:?}");
        }
    }
}

/// **A mkdir stopped at any write leaves its parent no fewer links than names**: the parent counts
/// the new `..` before it is named.
#[test]
fn a_mkdir_stopped_at_any_write_leaves_no_count_short() {
    let base = base();
    let total = writes(&base, |vol| {
        let _ = vol.mkdir_at(vol.resolve_dir(b"/e").unwrap(), b"made", NOW);
    });
    for n in 0..total {
        for once in [true, false] {
            let rw = if once { Failing::once(base.clone(), n) } else { Failing::after(base.clone(), n) };
            let vol = Ext4(&rw);
            let _ = vol.mkdir_at(vol.resolve_dir(b"/e").unwrap(), b"made", NOW);
            let text = e2fsck(&rw.image.0.borrow());
            let ok = NEVER.iter().all(|bad| !text.contains(bad)) && !fewer_links_than_names(&text);
            assert!(ok, "mkdir, write {n} of {total} refused {}:\n{text}", if once { "once" } else { "on" });
        }
    }
}

/// A 250-byte name: three fill a 1 KiB block, and its slack holds no fourth.
fn long_name(i: u32) -> Vec<u8> {
    format!("{i:0>250}").into_bytes()
}

fn dir_inode(rw: &crate::test_support::RwImage, path: &[u8]) -> [u8; 256] {
    let sb = crate::ext4::read_superblock(rw).unwrap();
    let ino = Ext4(rw).resolve_dir(path).unwrap() as u32;
    crate::ext4::read_inode(rw, &sb, ino).unwrap()
}

/// `/g` on 1 KiB blocks: fifteen files with data, five full blocks, a tree of depth 1.
fn full_directory() -> Vec<u8> {
    let rw = crate::test_support::RwImage(std::cell::RefCell::new(fixture(1024, b"seed\n")));
    let vol = Ext4(&rw);
    vol.mkdir_at(2, b"g", NOW).unwrap();
    for i in 0..15 {
        vol.create_file(b"/g", &long_name(i), NOW).unwrap();
        vol.grow_file(&[b"/g/" as &[u8], &long_name(i)].concat(), 1, NOW).unwrap();
    }
    let g = dir_inode(&rw, b"/g");
    assert_eq!((crate::rd_u32(&g, 4), crate::rd_u16(&g, 46)), (5 * 1024, 1), "five blocks, a tree of depth 1");
    rw.0.into_inner()
}

/// Whether `e2fsck` finds a block or inode in use and unmarked: a bitmap difference with `+`.
fn marks_too_few(text: &str) -> bool {
    text.lines().filter_map(|l| l.split_once("bitmap differences:")).any(|(_, rest)| rest.contains('+'))
}

/// What `e2fsck` must never say after a stopped growth: [`NEVER`], a size or count wrong, a block
/// in use unmarked, and, for a write refused once, an inode never linked.
fn growth_left_wrong(text: &str, once: bool) -> bool {
    let wrong = ["end of extent exceeds", "i_blocks is", "i_size is"];
    wrong.iter().chain(NEVER.iter()).any(|bad| text.contains(bad))
        || marks_too_few(text)
        || (once && (text.contains("Unattached") || text.contains("Unconnected")))
}

/// **A directory growth stopped at any write leaves it able to grow**: each write of a growing
/// create, mkdir and rename refused once and from then on, then a create and a mkdir. A lost write
/// may still cost what `TODO(ext4-failed-write-undo)` records: a block or inode marked and unused,
/// and, from then on, an inode never linked.
#[test]
fn a_directory_growth_stopped_at_any_write_leaves_it_able_to_grow() {
    type Op = fn(&Ext4<'_, Failing>) -> Result<(), FsError>;
    let growths: [(&str, Op); 3] = [
        ("create", |v| v.create_file(b"/g", &long_name(15), NOW)),
        ("mkdir", |v| v.mkdir_at(v.resolve_dir(b"/g")?, &long_name(15), NOW)),
        ("rename", |v| {
            let to = [b"/g/" as &[u8], &long_name(15)].concat();
            v.rename_path(b"/system/current-generation", &to, false, NOW).map(drop)
        }),
    ];
    let base = full_directory();
    for (what, op) in growths {
        let total = writes(&base, |vol| op(vol).unwrap());
        assert!(what != "create" || total == 11, "the growing create's writes: {total}");
        for n in 0..total {
            for once in [true, false] {
                let rw = if once { Failing::once(base.clone(), n) } else { Failing::after(base.clone(), n) };
                let _ = op(&Ext4(&rw));
                let how = if once { "once" } else { "and every one after" };
                let text = e2fsck(&rw.image.0.borrow());
                assert!(
                    !(once && growth_left_wrong(&text, once)),
                    "the growing {what}, write {n} refused once:\n{text}"
                );
                let then = crate::test_support::RwImage(std::cell::RefCell::new(rw.image.0.into_inner()));
                let vol = Ext4(&then);
                assert_eq!(
                    vol.create_file(b"/g", &long_name(16), NOW),
                    Ok(()),
                    "{what}, write {n} refused {how}: a create"
                );
                let g = vol.resolve_dir(b"/g").unwrap();
                assert_eq!(vol.mkdir_at(g, &long_name(17), NOW), Ok(()), "{what}, write {n} refused {how}: a mkdir");
                let text = e2fsck(&then.0.borrow());
                let ok = !growth_left_wrong(&text, once);
                assert!(
                    ok,
                    "the growing {what}, write {n} of {total} refused {how}, then a create and a mkdir:\n{text}"
                );
            }
        }
    }
}

/// **A file grown after a growth lost its size write counts its blocks** as `e2fsck` counts them.
#[test]
fn a_file_grown_after_a_growth_lost_its_size_write_counts_its_blocks() {
    let rw = crate::test_support::RwImage(std::cell::RefCell::new(fixture(1024, b"seed\n")));
    let vol = Ext4(&rw);
    vol.create_file(b"/system", b"a", NOW).unwrap();
    vol.create_file(b"/system", b"b", NOW).unwrap();
    // Grown in turn a block at a time, so each of `a`'s blocks is an extent of its own.
    for i in 1..=6 {
        vol.grow_file(b"/system/a", i * 1024, NOW).unwrap();
        vol.grow_file(b"/system/b", i * 1024, NOW).unwrap();
    }
    let sb = crate::ext4::read_superblock(&rw).unwrap();
    let a = vol.map_file(b"/system/a", &mut [BlockRun::default(); 8]).unwrap().id as u32;
    assert_eq!(crate::rd_u16(&crate::ext4::read_inode(&rw, &sb, a).unwrap(), 46), 1, "a tree of depth 1");
    let base = rw.0.into_inner();
    let grow = |vol: &Ext4<'_, Failing>| vol.grow_file(b"/system/a", 7 * 1024, NOW).map(drop);
    let total = writes(&base, |vol| grow(vol).unwrap());
    let lost = Failing::after(base, total - 1);
    assert_eq!(grow(&Ext4(&lost)), Err(FsError::Io), "the size write, last, is lost");
    let then = crate::test_support::RwImage(std::cell::RefCell::new(lost.image.0.into_inner()));
    Ext4(&then).grow_file(b"/system/a", 9 * 1024, NOW).unwrap();
    crate::test_support::assert_e2fsck_clean(&then.0.borrow(), "grown-after-a-lost-size");
}

/// A device that keeps the offset of every write, in order.
struct Recording(crate::test_support::RwImage, std::cell::RefCell<Vec<u64>>);
impl BlockReader for Recording {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        self.0.read_at(offset, buf)
    }
}
impl BlockWriter for Recording {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
        self.1.borrow_mut().push(offset);
        self.0.write_at(offset, buf)
    }
}

/// **A create whose inode write is refused gives the inode back**, rather than leaving it taken.
#[test]
fn a_create_whose_inode_write_is_refused_gives_the_inode_back() {
    let base = fixture(4096, b"seed\n");
    let made = Recording(crate::test_support::RwImage(std::cell::RefCell::new(base.clone())), Default::default());
    Ext4(&made).create_file(b"/system", b"new", NOW).unwrap();
    let ino = Ext4(&made).map_file(b"/system/new", &mut [BlockRun::default(); 1]).unwrap().id as u32;
    let sb = crate::ext4::read_superblock(&made).unwrap();
    let at = crate::ext4::inode_offset(&made, &sb, ino).unwrap();
    let n = made.1.borrow().iter().position(|&off| off == at).expect("the create writes its inode") as u64;
    let rw = Failing::once(base, n);
    assert_eq!(Ext4(&rw).create_file(b"/system", b"new", NOW), Err(FsError::Io));
    crate::test_support::assert_e2fsck_clean(&rw.image.0.borrow(), "inode-write-refused");
}
