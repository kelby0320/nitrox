use std::cell::Cell;
use std::vec::Vec;

use libkern::KError;
use librsproto::session::DirError;

use super::{Entry, Indexes, has_none, merge};

fn entry(name: &str) -> Entry {
    Entry { name: name.into(), pkg: usize::MAX, inode: 1, kind: 0, mode: 0, size: 0, mtime: 0 }
}

fn names(view: &[Entry]) -> Vec<(&str, usize)> {
    view.iter().map(|e| (e.name.as_str(), e.pkg)).collect()
}

/// **A package refused leaves the view unkept**: the next ask builds it again, and once both
/// packages read, the view is kept and never built again.
#[test]
fn a_view_with_a_package_refused_is_built_again_until_every_package_reads() {
    let refused = Cell::new(true);
    let builds = Cell::new(0);
    let build = || {
        builds.set(builds.get() + 1);
        merge(2, |i| match i {
            0 if refused.get() => (Vec::new(), false),
            0 => (vec![entry("list")], true),
            _ => (vec![entry("nxsh")], true),
        })
    };
    let mut indexes = Indexes::default();
    assert_eq!(names(indexes.of("bin", build)), [("nxsh", 1)], "the refused package's programs are missing");
    refused.set(false);
    assert_eq!(names(indexes.of("bin", build)), [("list", 0), ("nxsh", 1)], "the refusal cleared");
    assert_eq!(builds.get(), 2);
    assert_eq!(names(indexes.of("bin", build)), [("list", 0), ("nxsh", 1)]);
    assert_eq!(builds.get(), 2, "a whole view is kept");
}

/// A listing cut short keeps what it read for this ask, and is not kept.
#[test]
fn a_listing_cut_short_serves_what_it_read_and_is_not_kept() {
    let builds = Cell::new(0);
    let build = || {
        builds.set(builds.get() + 1);
        merge(1, |_| (vec![entry("list")], false))
    };
    let mut indexes = Indexes::default();
    assert_eq!(names(indexes.of("bin", build)), [("list", 0)]);
    let _ = indexes.of("bin", build);
    assert_eq!(builds.get(), 2);
}

/// **A package with no such directory is read whole**: most packages have no `applications/` or
/// `manual/`, and a view that counted them as refused would be built on every ask.
#[test]
fn a_package_with_no_such_directory_is_read_whole() {
    assert!(has_none(DirError::Transport(KError::NotFound.as_i32())));
    for e in [KError::WouldBlock, KError::PeerClosed, KError::IoError] {
        assert!(!has_none(DirError::Transport(e.as_i32())), "{e:?}");
    }
    assert!(!has_none(DirError::Server(KError::NotFound.as_i32())), "a server's reply, not the open");
    assert!(!has_none(DirError::Protocol));

    let builds = Cell::new(0);
    let build = || {
        builds.set(builds.get() + 1);
        merge(2, |i| if i == 0 { (Vec::new(), true) } else { (vec![entry("files.toml")], true) })
    };
    let mut indexes = Indexes::default();
    assert_eq!(names(indexes.of("applications", build)), [("files.toml", 1)]);
    let _ = indexes.of("applications", build);
    assert_eq!(builds.get(), 1);
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
    let _ = indexes.of("bin", || (vec![entry("list")], true));
    assert_eq!(names(indexes.of("manual", || (vec![entry("man.1.md")], true))), [("man.1.md", usize::MAX)]);
    assert_eq!(names(indexes.of("bin", || unreachable!("kept"))), [("list", usize::MAX)]);
}
