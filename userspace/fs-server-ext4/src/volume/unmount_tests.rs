//! **What an unmount records**: clean only what its mount found clean, where no write failed and
//! no operation failed on the device or found the filesystem corrupt.

use super::*;
use crate::test_support::{Failing, RwImage, fixture};
use std::cell::RefCell;

const NOW: i64 = 1_791_244_800;

fn state(img: &[u8]) -> u16 {
    u16::from_le_bytes([img[1024 + 58], img[1024 + 59]])
}

/// **An unmount records clean only what its mount found clean and no change failed on**: found not
/// clean, or with errors, it is left as found, and after a failed change as the mount left it.
#[test]
fn an_unmount_records_clean_only_what_was_found_clean_and_never_failed() {
    for found in [0x0001u16, 0x0000, 0x0002, 0x0003] {
        let mut img = fixture(4096, b"seed\n");
        img[1024 + 58..1024 + 60].copy_from_slice(&found.to_le_bytes());
        let rw = crate::test_support::RwImage(std::cell::RefCell::new(img));
        let vol = Ext4::new(&rw);
        vol.mark_mounted().unwrap();
        assert_eq!(state(&rw.0.borrow()), found & !1, "{found:#06x}: mounted, never clean");
        vol.create_file(b"/system", b"made", NOW).unwrap();
        assert_eq!(vol.mkdir_at(2, b"system", NOW), Err(FsError::Exists));
        vol.mark_clean().unwrap();
        assert_eq!(state(&rw.0.borrow()), found, "{found:#06x}: unmounted as found");
        assert_eq!(vol.was_left_clean(), Ok(found == 1), "{found:#06x}");
    }
    type Change = fn(&Ext4<'_, Failing>) -> Result<(), FsError>;
    let changes: [(&str, Change, FsError); 2] = [
        ("a create the device refused", |v| v.create_file(b"/system", b"made", NOW), FsError::Io),
        (
            "a truncate over metadata",
            |v| v.truncate_file(b"/system/current-generation", 0, NOW).map(drop),
            FsError::Corrupt,
        ),
    ];
    for (what, change, why) in changes {
        let mut img = fixture(4096, b"seed\n");
        if why == FsError::Corrupt {
            let rw = crate::test_support::RwImage(std::cell::RefCell::new(img.clone()));
            let sb = crate::ext4::read_superblock(&rw).unwrap();
            let file = Ext4::new(&rw).map_file(b"/system/current-generation", &mut [BlockRun::default(); 1]);
            let at = crate::ext4::inode_offset(&rw, &sb, file.unwrap().id as u32).unwrap() as usize;
            img[at + 60..at + 64].copy_from_slice(&1u32.to_le_bytes()); // the descriptor table's block
        }
        let rw = if why == FsError::Io { Failing::once(img, 1) } else { Failing::after(img, u64::MAX) };
        let vol = Ext4::new(&rw);
        vol.mark_mounted().unwrap();
        assert_eq!(change(&vol), Err(why), "{what}");
        vol.create_file(b"/system", b"after", NOW).unwrap();
        vol.mark_clean().unwrap();
        assert_eq!(state(&rw.image.0.borrow()), 0x0000, "{what}: left as the mount left it");
    }
}

/// What the operations below name: `/d`, its file `f`, and `o`, a file unlinked and not released.
struct Ids {
    d: u64,
    f: u64,
    orphan: u64,
}

/// [`Failing`], whose reads fail too once [`Dev::fail_read`] is asked: the `n`th from then.
struct Dev(Failing, std::cell::Cell<Option<u64>>);
impl Dev {
    fn after(img: Vec<u8>) -> Dev {
        Dev(Failing::after(img, u64::MAX), None.into())
    }
    fn once(img: Vec<u8>, n: u64) -> Dev {
        Dev(Failing::once(img, n), None.into())
    }
    fn fail_read(&self, n: u64) {
        self.1.set(Some(n));
    }
    fn state(&self) -> u16 {
        state(&self.0.image.0.borrow())
    }
}
impl BlockReader for Dev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        match self.1.get() {
            Some(0) => {
                self.1.set(None);
                Err(FsError::Io)
            }
            n => {
                self.1.set(n.map(|n| n - 1));
                self.0.read_at(offset, buf)
            }
        }
    }
}
impl BlockWriter for Dev {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
        self.0.write_at(offset, buf)
    }
}

type Op = fn(&Ext4<'_, Dev>, &Ids) -> Result<(), FsError>;

/// `/d` with `f`, `o` and an empty `sub`, and `/e`; `o` unlinked, waiting for its release.
fn base() -> (Vec<u8>, Ids) {
    let rw = RwImage(RefCell::new(fixture(4096, b"seed\n")));
    let vol = Ext4::new(&rw);
    vol.mkdir_at(2, b"d", NOW).unwrap();
    vol.mkdir_at(2, b"e", NOW).unwrap();
    let d = vol.resolve_dir(b"/d").unwrap();
    vol.mkdir_at(d, b"sub", NOW).unwrap();
    for name in [b"/d/f" as &[u8], b"/d/o"] {
        vol.create_file(b"/d", &name[3..], NOW).unwrap();
        vol.grow_file(name, 9000, NOW).unwrap();
    }
    let f = vol.map_file(b"/d/f", &mut [BlockRun::default(); 4]).unwrap().id;
    let orphan = vol.unlink_at(d, b"o", NOW).unwrap().unwrap();
    (rw.0.into_inner(), Ids { d, f, orphan })
}

/// Every change a client can make.
fn changes() -> [(&'static str, Op); 11] {
    [
        ("create", |v, _| v.create_file(b"/d", b"made", NOW)),
        ("grow", |v, _| v.grow_file(b"/d/f", 20000, NOW).map(drop)),
        ("truncate", |v, _| v.truncate_file(b"/d/f", 100, NOW).map(drop)),
        ("mkdir", |v, i| v.mkdir_at(i.d, b"made", NOW)),
        ("unlink", |v, i| v.unlink_at(i.d, b"f", NOW).map(drop)),
        ("release", |v, i| v.release(i.orphan, NOW)),
        ("rmdir", |v, i| v.rmdir_at(i.d, b"sub", NOW, |_| ())),
        ("touch a name", |v, i| v.touch_at(i.d, b"f", NOW)),
        ("rename within", |v, i| v.rename_at(i.d, b"f", b"g", NOW)),
        ("rename across", |v, _| v.rename_path(b"/d/f", b"/e/f", false, NOW).map(drop)),
        ("touch an id", |v, i| v.touch_file(i.f, NOW)),
    ]
}

/// Every read a client can make.
fn reads() -> [(&'static str, Op); 5] {
    [
        ("map", |v, _| v.map_file(b"/d/f", &mut [BlockRun::default(); 4]).map(drop)),
        ("read", |v, _| v.read_file(b"/d/f", &mut [0u8; 9000]).map(drop)),
        ("read a range", |v, _| v.read_file_range(b"/d/f", 0, 100, &mut [0u8; 100]).map(drop)),
        ("resolve a directory", |v, _| v.resolve_dir(b"/d").map(drop)),
        ("list a directory", |v, i| v.read_dir(i.d, 0, |_| true).map(drop)),
    ]
}

/// [`base`] with four blocks free, and a grow of `f` by ten: it takes what is left, is refused
/// `TooLarge`, and gives it back.
fn nearly_full() -> (Vec<u8>, Ids, Op) {
    let (img, ids) = base();
    let rw = RwImage(RefCell::new(img));
    let vol = Ext4::new(&rw);
    let free = crate::rd_u32(&rw.0.borrow(), 1024 + 12) as usize;
    vol.create_file(b"/e", b"fill", NOW).unwrap();
    vol.grow_file(b"/e/fill", (free - 4) * 4096, NOW).unwrap();
    let grow: Op = |v, _| v.grow_file(b"/d/f", 9000 + 10 * 4096, NOW).map(drop);
    assert_eq!(grow(&Ext4::new(&Dev::after(rw.0.borrow().clone())), &ids), Err(FsError::TooLarge));
    (rw.0.into_inner(), ids, grow)
}

/// **A write refused anywhere in a change leaves the filesystem marked for a check**, whatever the
/// change answers: an rmdir that had removed its name answered `Ok`, and a grow refused for space
/// answered `TooLarge`, each with a write lost, and the unmount recorded the damage clean.
#[test]
fn a_write_refused_anywhere_in_a_change_leaves_it_marked_for_a_check() {
    let (img, ids) = base();
    let (full, full_ids, grow) = nearly_full();
    let cases = changes().map(|(what, op)| (what, op, &img, &ids));
    let mut answers = std::collections::BTreeSet::new();
    for (what, op, img, ids) in cases.into_iter().chain([("a grow past the free space", grow, &full, &full_ids)]) {
        let total = {
            let rw = Dev::after(img.clone());
            Ext4::new(&rw).mark_mounted().unwrap();
            let mounted = rw.0.writes.get();
            op(&Ext4::new(&rw), ids).unwrap_or(());
            rw.0.writes.get() - mounted
        };
        assert!(total > 0, "{what} writes");
        for n in 0..total {
            let rw = Dev::once(img.clone(), 1 + n);
            let vol = Ext4::new(&rw);
            vol.mark_mounted().unwrap();
            let answer = op(&vol, ids);
            vol.mark_clean().unwrap();
            assert_eq!(rw.state(), 0, "{what}, write {n} of {total} refused, answered {answer:?}");
            answers.insert(format!("{answer:?}"));
        }
    }
    assert!(answers.contains("Ok(())") && answers.contains("Err(TooLarge)"), "{answers:?}");
}

/// **Any operation that finds the filesystem corrupt leaves it marked for a check**, a read's as a
/// change's, as Linux marks an error found on any path. Each is found so with no write refused.
#[test]
fn any_operation_finding_the_filesystem_corrupt_leaves_it_marked() {
    let (img, ids) = base();
    for (what, op) in changes().into_iter().chain(reads()) {
        let rw = Dev::after(img.clone());
        let vol = Ext4::new(&rw);
        vol.mark_mounted().unwrap();
        rw.0.image.0.borrow_mut()[1024 + 56] ^= 0xFF; // `s_magic`
        assert_eq!(op(&vol, &ids), Err(FsError::Corrupt), "{what}");
        rw.0.image.0.borrow_mut()[1024 + 56] ^= 0xFF;
        vol.mark_clean().unwrap();
        assert_eq!(rw.state(), 0, "{what}: left marked for a check");
    }
}

/// **Every operation done, the filesystem found clean is recorded clean.**
#[test]
fn every_operation_done_leaves_a_clean_filesystem_clean() {
    let (img, ids) = base();
    for (what, op) in changes().into_iter().chain(reads()) {
        let rw = Dev::after(img.clone());
        let vol = Ext4::new(&rw);
        vol.mark_mounted().unwrap();
        assert_eq!(op(&vol, &ids), Ok(()), "{what}");
        vol.mark_clean().unwrap();
        assert_eq!(rw.state(), 1, "{what}: recorded clean");
    }
}

/// **Any operation a device read fails leaves the filesystem marked for a check**: its first read,
/// the superblock's, with nothing yet written.
#[test]
fn any_operation_a_read_fails_leaves_it_marked() {
    let (img, ids) = base();
    for (what, op) in changes().into_iter().chain(reads()) {
        let rw = Dev::after(img.clone());
        let vol = Ext4::new(&rw);
        vol.mark_mounted().unwrap();
        rw.fail_read(0);
        assert_eq!(op(&vol, &ids), Err(FsError::Io), "{what}");
        vol.mark_clean().unwrap();
        assert_eq!(rw.state(), 0, "{what}: left marked for a check");
    }
}

/// **The device notes each write it refuses**, a run of zeroes as a write.
#[test]
fn the_device_notes_each_write_it_refuses() {
    for zeroes in [false, true] {
        let dev = Dev::once(fixture(4096, b"seed\n"), 0);
        let vol = Ext4::new(&dev);
        let refused = if zeroes { vol.dev.write_zeroes(1 << 20, 8192) } else { vol.dev.write_at(1 << 20, &[0; 512]) };
        assert_eq!(refused, Err(FsError::Io));
        assert!(vol.dev.failed.get(), "zeroes {zeroes}: noted");
    }
}
