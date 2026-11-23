use std::cell::Cell;
use std::vec::Vec;

use libkern::KError;
use librsproto::file::DirEntry;
use librsproto::session::DirError;

use super::{Entry, Indexes, View, has_none, merge, read_package};

fn entry(name: &str) -> Entry {
    Entry { name: name.into(), pkg: usize::MAX, inode: 1, kind: 0, mode: 0, size: 0, mtime: 0 }
}

fn names(view: &[Entry]) -> Vec<(&str, usize)> {
    view.iter().map(|e| (e.name.as_str(), e.pkg)).collect()
}

fn listed(name: &[u8]) -> DirEntry<'_> {
    DirEntry { inode: 4, kind: 1, mode: 0o755, size: 9, mtime: 7, name }
}

/// Two packages, the first refused until `refused` clears, counting each build in `builds`.
fn two<'a>(refused: &'a Cell<bool>, builds: &'a Cell<u32>) -> impl FnMut() -> (Vec<Entry>, bool) + 'a {
    move || {
        builds.set(builds.get() + 1);
        merge(2, |i| match i {
            0 if refused.get() => (Vec::new(), false),
            0 => (vec![entry("list")], true),
            _ => (vec![entry("nxsh")], true),
        })
    }
}

/// **A package's directory refused, or its listing cut short, reads short**; one it does not
/// have reads whole, with nothing in it. `.`, `..` and a name that is not UTF-8 are no program.
#[test]
fn a_package_reads_whole_only_when_listed_to_its_end_or_holding_none() {
    let no_list = |_: &mut (), _: &mut dyn FnMut(&DirEntry<'_>) -> bool| -> Result<bool, DirError> { unreachable!() };
    let refused = |k: KError| read_package(3, Err::<(), _>(DirError::Transport(k.as_i32())), no_list);
    assert_eq!(refused(KError::NotFound).1, true);
    for k in [KError::WouldBlock, KError::IoError] {
        assert_eq!(refused(k).1, false, "{k:?}");
    }
    let (got, whole) = read_package(3, Ok(()), |_, f| {
        for name in [&b"."[..], b"..", b"list", b"\xff"] {
            f(&listed(name));
        }
        Ok(true)
    });
    assert!(whole);
    assert_eq!(names(&got), [("list", 3)]);
    assert_eq!((got[0].inode, got[0].kind, got[0].mode, got[0].size, got[0].mtime), (4, 1, 0o755, 9, 7));
    let (got, whole) = read_package(3, Ok(()), |_, f| {
        f(&listed(b"list"));
        Err(DirError::Transport(KError::PeerClosed.as_i32()))
    });
    assert_eq!((names(&got), whole), (vec![("list", 3)], false), "what was read, short");
}

/// **A name an unkept view lacks builds it again**: found once the refusal clears, `WouldBlock`
/// while it has not, as the package refused may hold it. A name it holds builds nothing.
#[test]
fn a_view_with_a_package_refused_is_built_again_for_a_name_it_lacks() {
    let (refused, builds) = (Cell::new(true), Cell::new(0));
    let mut indexes = Indexes::default();
    assert_eq!(indexes.resolve("bin", "nxsh", two(&refused, &builds)), Ok(1));
    assert_eq!(indexes.resolve("bin", "nxsh", two(&refused, &builds)), Ok(1));
    assert_eq!(builds.get(), 1, "a name it holds");
    assert_eq!(indexes.resolve("bin", "list", two(&refused, &builds)), Err(KError::WouldBlock));
    assert_eq!(builds.get(), 2);
    refused.set(false);
    assert_eq!(indexes.resolve("bin", "list", two(&refused, &builds)), Ok(0));
    assert_eq!(indexes.resolve("bin", "nope", two(&refused, &builds)), Err(KError::NotFound));
    assert_eq!(builds.get(), 3, "a whole view is kept");
}

/// **A listing reads the view it began with**: a page after the refusal clears does not read the
/// view built since, which would skip names and repeat others; the next listing reads that one.
#[test]
fn a_listings_pages_read_one_view() {
    let (refused, builds) = (Cell::new(true), Cell::new(0));
    let mut indexes = Indexes::default();
    let mut held: Option<View> = None;
    let first = indexes.listing("bin", 0, &mut held, two(&refused, &builds));
    assert_eq!(names(&first), [("nxsh", 1)]);
    refused.set(false);
    assert_eq!(indexes.resolve("bin", "list", two(&refused, &builds)), Ok(0), "built again meanwhile");
    assert_eq!(names(&indexes.listing("bin", 1, &mut held, two(&refused, &builds))), [("nxsh", 1)]);
    let mut next: Option<View> = None;
    assert_eq!(names(&indexes.listing("bin", 0, &mut next, two(&refused, &builds))), [("list", 0), ("nxsh", 1)]);
    assert_eq!(builds.get(), 2, "the whole view kept, and not built for the next listing");
}

/// A listing's first page builds an unkept view again; a whole one is never built again.
#[test]
fn a_listing_builds_an_unkept_view_again_and_keeps_a_whole_one() {
    let (refused, builds) = (Cell::new(true), Cell::new(0));
    let mut indexes = Indexes::default();
    for _ in 0..2 {
        let _ = indexes.listing("bin", 0, &mut None, two(&refused, &builds));
    }
    assert_eq!(builds.get(), 2);
    refused.set(false);
    for _ in 0..2 {
        let _ = indexes.listing("bin", 0, &mut None, two(&refused, &builds));
    }
    assert_eq!(builds.get(), 3);
}

/// **A package with no such directory is read whole**: most packages have no `applications/` or
/// `manual/`, and a view that counted them as refused would be built on every listing.
#[test]
fn a_package_with_no_such_directory_is_read_whole() {
    assert!(has_none(DirError::Transport(KError::NotFound.as_i32())));
    for e in [KError::WouldBlock, KError::PeerClosed, KError::IoError] {
        assert!(!has_none(DirError::Transport(e.as_i32())), "{e:?}");
    }
    assert!(!has_none(DirError::Server(KError::NotFound.as_i32())), "a server's reply, not the open");
    assert!(!has_none(DirError::Protocol));
}

/// The first package in manifest order to provide a name wins, as a resolve finds it; each
/// projection is its own view.
#[test]
fn the_first_provider_of_a_name_wins_and_each_projection_is_its_own() {
    let (view, whole) = merge(3, |i| match i {
        0 => (vec![entry("list")], true),
        1 => (vec![entry("list"), entry("cat")], true),
        _ => (vec![entry("cat"), entry("man")], true),
    });
    assert!(whole);
    assert_eq!(names(&view), [("list", 0), ("cat", 1), ("man", 2)]);

    let mut indexes = Indexes::default();
    assert_eq!(indexes.resolve("bin", "list", || (vec![entry("list")], true)), Ok(usize::MAX));
    assert_eq!(indexes.resolve("manual", "list", || (vec![entry("man.1.md")], true)), Err(KError::NotFound));
    assert_eq!(indexes.resolve("bin", "list", || unreachable!("kept")), Ok(usize::MAX));
}
