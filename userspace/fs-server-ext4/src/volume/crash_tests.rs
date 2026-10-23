//! **A change stopped at any write**: each write refused once, as a device that fails and recovers
//! would, or every write from it on, as a crash would leave the disk.

use super::*;
use crate::test_support::{Failing, fixture};

const NOW: i64 = 1_791_244_800;

/// `/d/src` and `/e/dst`, files with data, and `/d/sub`, an empty directory.
fn base() -> Vec<u8> {
    let rw = Failing::after(fixture(4096, b"seed\n"), u64::MAX);
    let vol = Ext4(&rw);
    vol.mkdir_at(2, b"d", NOW).unwrap();
    vol.mkdir_at(2, b"e", NOW).unwrap();
    vol.mkdir_at(vol.resolve_dir(b"/d").unwrap(), b"sub", NOW).unwrap();
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

/// What no stop may leave, as `e2fsck` says it: a name reaching a freed inode, a block in two
/// files, a directory under two names.
const NEVER: [&str; 3] = ["deleted/unused inode", "Multiply-claimed", "is a link to directory"];

/// Whether `e2fsck` counted an inode more names than links, so one name's removal frees what another
/// reaches. More links than names only leaks.
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
                for then in ["asked again", "the old name removed", "the new name removed"] {
                    let rw = crate::test_support::RwImage(std::cell::RefCell::new(stopped.clone()));
                    let vol = Ext4(&rw);
                    match then {
                        "asked again" => {
                            if let Ok(Some(id)) = vol.rename_path(from, to, replace, NOW) {
                                let _ = vol.release(id, NOW);
                            }
                        }
                        "the old name removed" => remove(&vol, from),
                        _ => remove(&vol, to),
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

/// **A name taken away returns what it freed, whatever write is refused after**: an unlink of a
/// file's last name, and a rename over one, each with every write refused once. The server
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

/// **What a change answers is what it did**: create, mkdir, unlink, rmdir and three renames, each
/// with every write refused once, answer `Ok` exactly when a name changed. An error after one
/// changed told the client the opposite, and an error that left one changed was not undone.
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
