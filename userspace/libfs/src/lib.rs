//! `libfs` — whole-file and path helpers, for anything that touches the filesystem.
//!
//! A file's contents bypass the directory protocol: a file resolves to a page-cache object the
//! process maps, so a copy is a `memcpy` between two mappings. The directory protocol itself is
//! [`librsproto::session::Dir`]; this crate owns no wire format, only namespace and memory
//! syscalls and the composition of directory answers ([`list_directory`]). Each function has a typed
//! form over [`libos`]; the raw `u64` forms beside it are for callers not yet on libos.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod files;
#[cfg(test)]
mod files_tests;
mod tree;

pub use files::*;
pub use tree::*;

use alloc::string::String;
use alloc::vec::Vec;
use librsproto::file::OwnedEntry;

/// How a directory listing is ordered. An enum: the set is what a person can choose from.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Order {
    /// Name, ascending. The default.
    #[default]
    NameAsc,
    /// Name, descending.
    NameDesc,
    /// Oldest modification first.
    OldestFirst,
    /// Newest modification first.
    NewestFirst,
}

/// What [`sort`] needs to know about an entry.
///
/// A trait so one comparison serves `OwnedEntry` and a caller's own row type, without copying
/// names into a `Vec<OwnedEntry>` first.
pub trait Listed {
    /// The entry's name, with no path in it.
    fn sort_name(&self) -> &[u8];
    /// Whether it is a directory; decides the group, not the order within it.
    fn sort_is_dir(&self) -> bool;
    /// Modification time, or `0` where the server reports none.
    fn sort_mtime(&self) -> i64;
}

impl Listed for OwnedEntry {
    fn sort_name(&self) -> &[u8] {
        self.name()
    }
    fn sort_is_dir(&self) -> bool {
        self.kind == librsproto::file::DIRENT_KIND_DIR
    }
    fn sort_mtime(&self) -> i64 {
        self.mtime
    }
}

/// Sort `entries` in place, directories first (whatever the order) and then by `order`.
///
/// Ties break on name: `mtime` is `0` when a server reports none, and enumeration order would show.
pub fn sort<T: Listed>(entries: &mut [T], order: Order) {
    entries.sort_by(|a, b| {
        b.sort_is_dir().cmp(&a.sort_is_dir()).then_with(|| match order {
            Order::NameAsc => a.sort_name().cmp(b.sort_name()),
            Order::NameDesc => b.sort_name().cmp(a.sort_name()),
            Order::OldestFirst => a.sort_mtime().cmp(&b.sort_mtime()).then_with(|| a.sort_name().cmp(b.sort_name())),
            Order::NewestFirst => b.sort_mtime().cmp(&a.sort_mtime()).then_with(|| a.sort_name().cmp(b.sort_name())),
        })
    });
}

/// The final component of a path (`"/a/b/c"` → `"c"`).
///
/// Trailing separators are ignored (`"/a/b/"` is `"b"`, not empty); `"/"` is itself.
pub fn basename(path: &[u8]) -> &[u8] {
    let end = match path.iter().rposition(|&c| c != b'/') {
        Some(i) => i + 1,
        None => return path, // all separators (e.g. "/"): nothing to take
    };
    match path[..end].iter().rposition(|&c| c == b'/') {
        Some(i) => &path[i + 1..end],
        None => &path[..end],
    }
}

/// [`basename`] on a `str`. The `from_utf8` cannot fail: `/` never occurs inside a multi-byte
/// sequence, so the subslice is still UTF-8 and the fallback is unreachable.
pub fn basename_str(path: &str) -> &str {
    core::str::from_utf8(basename(path.as_bytes())).unwrap_or(path)
}

/// Everything before the final component (`"/a/b/c"` → `"/a/b"`), or `"/"` for a single
/// component: the root is its own parent.
pub fn parent(path: &[u8]) -> &[u8] {
    let end = match path.iter().rposition(|&c| c != b'/') {
        Some(i) => i + 1,
        None => return b"/",
    };
    match path[..end].iter().rposition(|&c| c == b'/') {
        Some(0) => b"/",
        Some(i) => &path[..i],
        None => b".",
    }
}

/// [`parent`] on a `str`; the fallback is unreachable for the reason [`basename_str`]'s is.
fn parent_str(path: &str) -> &str {
    core::str::from_utf8(parent(path.as_bytes())).unwrap_or(path)
}

/// Join a directory path and a name with a single separator.
pub fn join(dir: &[u8], name: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(dir).into_owned();
    if !s.ends_with('/') {
        s.push('/');
    }
    s.push_str(&String::from_utf8_lossy(name));
    s
}

/// The folders a session's home holds for a person's own files, between Home and Root in
/// [`places`].
///
/// A place whose directory is absent is still offered; opening it says so, as a typed path does.
pub const HOME_FOLDERS: &[&str] = &["Documents", "Downloads", "Pictures"];

/// Somewhere a person goes often: what it is called, and the path it opens.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Place {
    /// What a person reads — `Home`, `Documents`, `Root`.
    pub name: &'static str,
    /// Where it goes.
    pub path: String,
}

/// The places for a session whose home is `home`: Home, the [`HOME_FOLDERS`] beneath it, and Root.
///
/// Built rather than stored: each is a function of `home`, so a held list could go stale.
pub fn places(home: &str) -> Vec<Place> {
    let mut out = alloc::vec![Place { name: "Home", path: String::from(home) }];
    for name in HOME_FOLDERS {
        out.push(Place { name, path: join(home.as_bytes(), name.as_bytes()) });
    }
    out.push(Place { name: "Root", path: String::from("/") });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_places_are_built_from_the_home_they_are_given() {
        let places = places("/home/someone-else");
        assert_eq!(places[0], Place { name: "Home", path: String::from("/home/someone-else") });
        assert_eq!(places.last().map(|p| (p.name, p.path.as_str())), Some(("Root", "/")));
        assert_eq!(places.len(), HOME_FOLDERS.len() + 2);
        for name in HOME_FOLDERS {
            let want = alloc::format!("/home/someone-else/{name}");
            assert!(
                places.iter().any(|p| p.name == *name && p.path == want),
                "{name} should be under this home, not under /home: {places:?}"
            );
        }
        assert_eq!(super::places("/home/")[1].path, "/home/Documents");
    }

    #[test]
    fn unpadded_trims_the_nuls_that_end_it() {
        assert_eq!(unpadded(b"a\nb\n\0\0\0"), b"a\nb\n");
        assert_eq!(unpadded(b"a\0b\0"), b"a\0b", "a NUL inside stays");
        assert_eq!(unpadded(b"a\nb"), b"a\nb", "nothing to trim");
        assert_eq!(unpadded(b"\0\0"), b"");
        assert_eq!(unpadded(b""), b"");
    }

    #[test]
    fn basename_takes_the_final_component() {
        assert_eq!(basename(b"/a/b/c"), b"c");
        assert_eq!(basename(b"file"), b"file");
        assert_eq!(basename(b"/a/b/"), b"b");
        assert_eq!(basename(b"/"), b"/");
    }

    #[test]
    fn basename_str_is_the_same_rule_and_never_loses_the_text() {
        assert_eq!(basename_str("/a/b/c"), "c");
        assert_eq!(basename_str("/a/b/"), "b", "a trailing separator is not the name");
        assert_eq!(basename_str("/"), "/");
        assert_eq!(basename_str("/home/notes-café.txt"), "notes-café.txt");
        assert_eq!(basename_str("café"), "café");
    }

    #[test]
    fn parent_drops_the_final_component() {
        assert_eq!(parent(b"/a/b/c"), b"/a/b");
        // The root, not the empty string: opening "" as a directory would fail.
        assert_eq!(parent(b"/system"), b"/");
        assert_eq!(parent(b"/a/b/"), b"/a");
        assert_eq!(parent(b"relative"), b".");
    }

    #[test]
    fn join_uses_exactly_one_separator() {
        assert_eq!(join(b"/a", b"b"), "/a/b");
        assert_eq!(join(b"/a/", b"b"), "/a/b");
        assert_eq!(join(b"/", b"system"), "/system");
    }

    fn ent(name: &str, dir: bool, mtime: i64) -> OwnedEntry {
        let kind = if dir { librsproto::file::DIRENT_KIND_DIR } else { librsproto::file::DIRENT_KIND_FILE };
        let mut e = OwnedEntry::binding(name.as_bytes(), kind);
        e.mtime = mtime;
        e
    }

    fn names(v: &[OwnedEntry]) -> Vec<String> {
        v.iter().map(|e| String::from_utf8_lossy(e.name()).into_owned()).collect()
    }

    #[test]
    fn a_different_shape_takes_the_same_order() {
        struct Row {
            label: String,
            folder: bool,
            when: i64,
        }
        impl Listed for Row {
            fn sort_name(&self) -> &[u8] {
                self.label.as_bytes()
            }
            fn sort_is_dir(&self) -> bool {
                self.folder
            }
            fn sort_mtime(&self) -> i64 {
                self.when
            }
        }

        let fixture = [
            ("beta.txt", false, 10),
            ("alpha", true, 20),
            ("acme.txt", false, 30),
            ("zeta", true, 5),
            ("undated", false, 0),
        ];
        for order in [Order::NameAsc, Order::NameDesc, Order::OldestFirst, Order::NewestFirst] {
            let mut wire: Vec<OwnedEntry> = fixture.iter().map(|&(n, d, m)| ent(n, d, m)).collect();
            let mut rows: Vec<Row> =
                fixture.iter().map(|&(n, d, m)| Row { label: String::from(n), folder: d, when: m }).collect();
            sort(&mut wire, order);
            sort(&mut rows, order);
            let a = names(&wire);
            let b: Vec<String> = rows.iter().map(|r| r.label.clone()).collect();
            assert_eq!(a, b, "{order:?} ordered the two shapes differently");
        }
    }

    #[test]
    fn directories_lead_every_order() {
        let mut v = alloc::vec![
            ent("beta.txt", false, 10),
            ent("alpha", true, 20),
            ent("acme.txt", false, 30),
            ent("zeta", true, 5),
        ];
        for order in [Order::NameAsc, Order::NameDesc, Order::OldestFirst, Order::NewestFirst] {
            sort(&mut v, order);
            let got = names(&v);
            assert!(
                got[..2].iter().all(|n| n == "alpha" || n == "zeta"),
                "{order:?} put a file above a directory: {got:?}"
            );
        }
    }

    #[test]
    fn the_four_orders_are_the_four_orders() {
        let mut v = alloc::vec![ent("b.txt", false, 200), ent("a.txt", false, 300), ent("c.txt", false, 100),];
        sort(&mut v, Order::NameAsc);
        assert_eq!(names(&v), ["a.txt", "b.txt", "c.txt"]);
        sort(&mut v, Order::NameDesc);
        assert_eq!(names(&v), ["c.txt", "b.txt", "a.txt"]);
        sort(&mut v, Order::OldestFirst);
        assert_eq!(names(&v), ["c.txt", "b.txt", "a.txt"]);
        sort(&mut v, Order::NewestFirst);
        assert_eq!(names(&v), ["a.txt", "b.txt", "c.txt"]);
    }

    #[test]
    fn an_undated_listing_is_still_ordered() {
        let mut v = alloc::vec![ent("c", false, 0), ent("a", false, 0), ent("b", false, 0)];
        sort(&mut v, Order::NewestFirst);
        assert_eq!(names(&v), ["a", "b", "c"], "a tie falls back to the name");
        sort(&mut v, Order::OldestFirst);
        assert_eq!(names(&v), ["a", "b", "c"]);
    }
}
