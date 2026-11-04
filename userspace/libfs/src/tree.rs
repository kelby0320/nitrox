//! Directories and trees: listing, and the walks that copy, remove and make them.
//!
//! The typed forms take a namespace and `&str` paths; each raw shim keeps the `ns: u64` and
//! `&[u8]` signature its callers outside this crate still use.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use libkern::abi::{NS_ENTRY_PATH_MAX, NS_KIND_MOUNT};
use libkern::error::KError;
use libos::{CanLookup, Msg, Namespace, Ref};
use librsproto::file::{DIRENT_KIND_DIR, DIRENT_KIND_FILE, DirEntry, OwnedEntry};
use librsproto::session::{Dir, DirError};

use crate::files::{FileError, borrowed, copy, text};
use crate::{basename, join, parent_str};

/// Does `path` name a directory?
///
/// Answered by opening a session on it, which succeeds only on a directory; this avoids inferring
/// a type from an error code.
pub fn is_directory<'a, M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>, path: &str) -> bool {
    Dir::resolve(ns, path, &mut Msg::new()).is_ok()
}

/// The namespace bindings immediately beneath `path`, as `(name, kind)` pairs.
///
/// A listing is the union of the filesystem under a path and the bindings beneath it; this is the
/// bindings half. A kernel server owning a subtree (`/dev/blk/<n>`) appears as one binding that
/// lists as empty; `/dev/devices` answers that, not this.
pub fn bindings_under<'a, M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>, path: &str) -> Vec<(String, u8)> {
    // `path` with exactly one trailing slash, so a prefix test is unambiguous.
    let mut base = String::from(path);
    while base.ends_with('/') {
        base.pop();
    }
    base.push('/');

    let ns = ns.into();
    let mut out: Vec<(String, u8)> = Vec::new();
    // An error ends the walk, as `NotFound` does.
    for entry in ns.bindings().map_while(Result::ok) {
        let len = (entry.path_len as usize).min(NS_ENTRY_PATH_MAX);
        let bound = String::from_utf8_lossy(&entry.path[..len]).into_owned();
        let Some(rest) = bound.strip_prefix(&base) else {
            continue;
        };
        if rest.is_empty() {
            continue; // the binding *is* this path, not a child of it
        }
        let (name, deeper) = match rest.split_once('/') {
            Some((head, _)) => (head.to_string(), true),
            None => (rest.to_string(), false),
        };
        // An intermediate component is a directory; a leaf is one only if it is a mount.
        let kind = if deeper || entry.kind == NS_KIND_MOUNT { DIRENT_KIND_DIR } else { DIRENT_KIND_FILE };
        if !out.iter().any(|(n, _)| *n == name) {
            out.push((name, kind));
        }
    }
    out
}

/// Every entry directly under `path`: the filesystem's, plus the namespace bindings mounted
/// there, with bindings shadowing same-named files, as a mount point shadows what it covers.
///
/// `Err` only when there is neither a filesystem here nor a binding beneath.
pub fn list_directory<'a, M: CanLookup + 'a>(
    ns: impl Into<Ref<'a, Namespace, M>>,
    path: &str,
) -> Result<Vec<OwnedEntry>, DirError> {
    let ns = ns.into();
    let ns_entries = bindings_under(ns, path);

    let mut entries: Vec<OwnedEntry> = Vec::new();
    let mut msg = Msg::new();
    match Dir::resolve(ns, path, &mut msg) {
        Ok(mut dir) => {
            dir.read_dir(|e| {
                if e.name != b"." && e.name != b".." {
                    entries.push(OwnedEntry::from_entry(e));
                }
                true
            })?;
        }
        Err(e) => {
            if ns_entries.is_empty() {
                return Err(e);
            }
        }
    }

    for (name, kind) in &ns_entries {
        entries.retain(|e| e.name() != name.as_bytes());
        entries.push(OwnedEntry::binding(name.as_bytes(), *kind));
    }
    Ok(entries)
}

/// Deepest tree either walk descends; a runaway guard, not a resource limit.
pub const MAX_TREE_DEPTH: u32 = 32;

/// Why a tree walk stopped; each names the step that failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TreeError {
    /// [`MAX_TREE_DEPTH`] exceeded.
    TooDeep,
    /// A directory session could not be opened.
    OpenDir,
    /// A directory could not be enumerated.
    ReadDir,
    /// A directory could not be created; the payload is the server's `KError`.
    MakeDir(i32),
    /// A file copy failed.
    Copy(FileError),
    /// An entry could not be unlinked.
    Unlink,
    /// A directory could not be removed once emptied; `NotEmpty` means an entry appeared mid-walk.
    Rmdir(i32),
    /// A copy's destination is its source or under it, where the walk would copy what it made.
    IntoItself,
    /// A listed name no single entry can have, empty or holding a `/`: joined to the path, it
    /// would name another file than the one listed.
    Name,
}

/// **Whether `path` is `dir` or under it**, a component at a time, so `/abc` is not under `/ab`
/// and `/a//b` is under `/a`. Textual: it resolves no `..`, which the kernel refuses in a path, and
/// takes no directory to be bound at two paths, which no namespace here does.
pub fn within(path: &[u8], dir: &[u8]) -> bool {
    fn parts(p: &[u8]) -> impl Iterator<Item = &[u8]> {
        p.split(|&c| c == b'/').filter(|c| !c.is_empty() && *c != b".")
    }
    let mut path = parts(path);
    parts(dir).all(|d| path.next() == Some(d))
}

/// Enumerate `path`'s children, closing the session before returning.
///
/// Filesystem entries only, not [`bindings_under`]: a descent following a mount point would copy
/// or delete through it into another server's tree. Closing first keeps sessions (`MAX_SESSIONS`)
/// from capping the depth.
fn children<M: CanLookup>(ns: Ref<'_, Namespace, M>, path: &str) -> Result<Vec<OwnedEntry>, TreeError> {
    let mut msg = Msg::new();
    let mut dir = Dir::resolve(ns, path, &mut msg).map_err(|_| TreeError::OpenDir)?;
    let listed = listing(|f| dir.read_dir(f).map(drop));
    drop(dir);
    listed
}

/// The entries `list` gives, `.` and `..` left out; [`TreeError::Name`] for a name no entry has.
pub(crate) fn listing(
    list: impl FnOnce(&mut dyn FnMut(&DirEntry<'_>) -> bool) -> Result<(), DirError>,
) -> Result<Vec<OwnedEntry>, TreeError> {
    let mut entries: Vec<OwnedEntry> = Vec::new();
    let mut named = true;
    list(&mut |e| {
        named = !e.name.is_empty() && !e.name.contains(&b'/');
        if named && e.name != b"." && e.name != b".." {
            entries.push(OwnedEntry::from_entry(e));
        }
        named
    })
    .map_err(|_| TreeError::ReadDir)?;
    if named { Ok(entries) } else { Err(TreeError::Name) }
}

/// Copy `src` to `dst`, recursing if `src` is a directory; `on_file` gets `(src, dst, bytes)`.
///
/// Without `force` an existing destination is an error: merging into a tree should not be silent.
pub fn copy_recursive<'a, M: CanLookup + 'a>(
    ns: impl Into<Ref<'a, Namespace, M>>,
    src: &str,
    dst: &str,
    force: bool,
    on_file: &mut impl FnMut(&str, &str, u64),
) -> Result<(), TreeError> {
    copy_tree_at(ns.into(), src, dst, force, 0, on_file)
}

fn copy_tree_at<M: CanLookup>(
    ns: Ref<'_, Namespace, M>,
    src: &str,
    dst: &str,
    force: bool,
    depth: u32,
    on_file: &mut impl FnMut(&str, &str, u64),
) -> Result<(), TreeError> {
    if depth > MAX_TREE_DEPTH {
        return Err(TreeError::TooDeep);
    }
    if within(dst.as_bytes(), src.as_bytes()) {
        return Err(TreeError::IntoItself);
    }
    if !is_directory(ns, src) {
        let bytes = copy(ns, src, dst, force).map_err(TreeError::Copy)?;
        on_file(src, dst, bytes);
        return Ok(());
    }

    // `force` lets an existing destination directory through: this walk's rule, not `mkdir`'s.
    if let Err(e) = make_dir(ns, dst)
        && !(force && is_directory(ns, dst))
    {
        return Err(e);
    }

    for e in &children(ns, src)? {
        let child_src = join(src.as_bytes(), e.name());
        let child_dst = join(dst.as_bytes(), e.name());
        copy_tree_at(ns, &child_src, &child_dst, force, depth + 1, on_file)?;
    }
    Ok(())
}

/// Remove `path` and everything under it, depth-first; `on_entry` gets `(path, is_dir)`, children
/// first. The caller must refuse a `path` that is a namespace binding; this cannot tell.
pub fn remove_recursive<'a, M: CanLookup + 'a>(
    ns: impl Into<Ref<'a, Namespace, M>>,
    path: &str,
    on_entry: &mut impl FnMut(&str, bool),
) -> Result<(), TreeError> {
    remove_tree_at(ns.into(), path, 0, on_entry)
}

fn remove_tree_at<M: CanLookup>(
    ns: Ref<'_, Namespace, M>,
    path: &str,
    depth: u32,
    on_entry: &mut impl FnMut(&str, bool),
) -> Result<(), TreeError> {
    if depth > MAX_TREE_DEPTH {
        return Err(TreeError::TooDeep);
    }
    for e in &children(ns, path)? {
        let child = join(path.as_bytes(), e.name());
        if e.kind == DIRENT_KIND_DIR {
            remove_tree_at(ns, &child, depth + 1, on_entry)?;
        } else {
            remove_entry(ns, &child)?;
            on_entry(&child, false);
        }
    }
    remove_empty_dir(ns, path)?;
    on_entry(path, true);
    Ok(())
}

/// Create the directory named by `path`, via its parent's session.
///
/// [`TreeError::MakeDir`] carries the server's `KError` (`0` if none).
pub fn make_dir<'a, M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>, path: &str) -> Result<(), TreeError> {
    let name = basename(path.as_bytes());
    if name.is_empty() {
        // The root has no parent to create it in; `basename` of `/` is empty.
        return Err(TreeError::MakeDir(0));
    }
    let mut msg = Msg::new();
    let mut dir = Dir::resolve(ns, parent_str(path), &mut msg).map_err(|_| TreeError::OpenDir)?;
    match dir.mkdir(name) {
        Ok(()) => Ok(()),
        Err(DirError::Server(k)) => Err(TreeError::MakeDir(k)),
        Err(_) => Err(TreeError::MakeDir(0)),
    }
}

/// `unlink` the entry named by `path`, via its parent's session.
pub fn remove_entry<'a, M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>, path: &str) -> Result<(), TreeError> {
    let mut msg = Msg::new();
    let mut dir = Dir::resolve(ns, parent_str(path), &mut msg).map_err(|_| TreeError::OpenDir)?;
    dir.unlink(basename(path.as_bytes())).map_err(|_| TreeError::Unlink)
}

/// `rmdir` the empty directory at `path`; `NotEmpty` after a descent means a concurrent mutator.
fn remove_empty_dir<M: CanLookup>(ns: Ref<'_, Namespace, M>, path: &str) -> Result<(), TreeError> {
    let mut msg = Msg::new();
    let mut dir = Dir::resolve(ns, parent_str(path), &mut msg).map_err(|_| TreeError::OpenDir)?;
    match dir.rmdir(basename(path.as_bytes())) {
        Ok(()) => Ok(()),
        Err(DirError::Server(k)) => Err(TreeError::Rmdir(k)),
        Err(_) => Err(TreeError::Rmdir(0)),
    }
}

/// [`is_directory`] for a raw namespace handle. For callers not yet on libos; the typed form is
/// [`is_directory`].
// TODO(raw-handle-shims): goes when its last caller is on libos.
pub fn is_dir(ns: u64, path: &[u8]) -> bool {
    text(path).is_some_and(|path| is_directory(borrowed(ns), path))
}

/// [`bindings_under`] for a raw namespace handle. For callers not yet on libos; the typed form is
/// [`bindings_under`].
// TODO(raw-handle-shims): goes when its last caller is on libos.
pub fn ns_children(ns: u64, path: &[u8]) -> Vec<(String, u8)> {
    bindings_under(borrowed(ns), &String::from_utf8_lossy(path))
}

/// [`list_directory`] for a raw namespace handle. For callers not yet on libos; the typed form is
/// [`list_directory`].
// TODO(raw-handle-shims): goes when its last caller is on libos.
pub fn list_dir(ns: u64, path: &[u8]) -> Result<Vec<OwnedEntry>, DirError> {
    let path = text(path).ok_or(DirError::Transport(KError::InvalidArgument.as_i32()))?;
    list_directory(borrowed(ns), path)
}

/// [`copy_recursive`] for a raw namespace handle. For callers not yet on libos; the typed form is
/// [`copy_recursive`].
// TODO(raw-handle-shims): goes when its last caller is on libos.
pub fn copy_tree(
    ns: u64,
    src: &[u8],
    dst: &[u8],
    force: bool,
    on_file: &mut impl FnMut(&[u8], &[u8], u64),
) -> Result<(), TreeError> {
    let (Some(src), Some(dst)) = (text(src), text(dst)) else {
        return Err(TreeError::Copy(FileError::Io(KError::InvalidArgument.as_i32())));
    };
    copy_recursive(borrowed(ns), src, dst, force, &mut |s, d, n| on_file(s.as_bytes(), d.as_bytes(), n))
}

/// [`remove_recursive`] for a raw namespace handle. For callers not yet on libos; the typed form
/// is [`remove_recursive`].
// TODO(raw-handle-shims): goes when its last caller is on libos.
pub fn remove_tree(ns: u64, path: &[u8], on_entry: &mut impl FnMut(&[u8], bool)) -> Result<(), TreeError> {
    let path = text(path).ok_or(TreeError::OpenDir)?;
    remove_recursive(borrowed(ns), path, &mut |p, dir| on_entry(p.as_bytes(), dir))
}

/// [`make_dir`] for a raw namespace handle. For callers not yet on libos; the typed form is
/// [`make_dir`].
// TODO(raw-handle-shims): goes when its last caller is on libos.
pub fn mkdir(ns: u64, path: &[u8]) -> Result<(), TreeError> {
    make_dir(borrowed(ns), text(path).ok_or(TreeError::OpenDir)?)
}

/// [`remove_entry`] for a raw namespace handle. For callers not yet on libos; the typed form is
/// [`remove_entry`].
// TODO(raw-handle-shims): goes when its last caller is on libos.
pub fn unlink_at(ns: u64, path: &[u8]) -> Result<(), TreeError> {
    remove_entry(borrowed(ns), text(path).ok_or(TreeError::OpenDir)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A path is under a directory a component at a time**: `/abc` is not under `/ab`, and a
    /// copy of `/a` to `/a//b` would copy into itself as surely as to `/a/b`.
    #[test]
    fn a_path_is_within_a_directory_a_component_at_a_time() {
        assert!(within(b"/a/b", b"/a") && within(b"/a", b"/a") && within(b"/a//b/", b"/a/"));
        assert!(within(b"/a/./b/c", b"/a/b"));
        assert!(!within(b"/abc", b"/ab") && !within(b"/a", b"/a/b") && !within(b"/b/a", b"/a"));
    }

    /// The names a walk takes from a listing of `names`, or why it stops.
    fn walked(names: &[&[u8]], fails: bool) -> Result<Vec<String>, TreeError> {
        let listed = listing(|f| {
            for name in names {
                let e = DirEntry { inode: 1, kind: DIRENT_KIND_FILE, mode: 0, size: 0, mtime: 0, name };
                if !f(&e) {
                    break;
                }
            }
            if fails { Err(DirError::Protocol) } else { Ok(()) }
        });
        listed.map(|v| v.iter().map(|e| String::from_utf8_lossy(e.name()).into_owned()).collect())
    }

    /// **A listed name no entry can have stops a walk**, empty or holding a `/`: joined to the
    /// path it names another file than the one listed, which a copy or a removal would then take.
    #[test]
    fn a_listed_name_no_entry_can_have_stops_a_walk() {
        assert_eq!(walked(&[b".", b"..", b"a", b"b"], false), Ok(vec![String::from("a"), String::from("b")]));
        assert_eq!(walked(&[b"a", b"x/y", b"b"], false), Err(TreeError::Name));
        assert_eq!(walked(&[b"a", b""], false), Err(TreeError::Name));
        assert_eq!(walked(&[b"a"], true), Err(TreeError::ReadDir));
    }

    /// **The bindings beneath a path are its children by component**: a deeper binding shows its
    /// first component as a directory, the path itself and a sibling prefix show nothing.
    #[test]
    fn the_bindings_beneath_a_path_are_its_children_by_component() {
        use libos::{Handle, MapReadWrite, Memory, NsMutable, mock};
        mock::reset();
        let ns = Handle::<Namespace, NsMutable>::create().unwrap();
        let mem = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        for path in ["/a", "/a/b/c", "/a/b/e", "/a/d", "/ab/x"] {
            ns.bind(path, &mem).unwrap();
        }
        let want = alloc::vec![(String::from("b"), DIRENT_KIND_DIR), (String::from("d"), DIRENT_KIND_FILE)];
        assert_eq!(bindings_under(&ns, "/a"), want);
        assert_eq!(bindings_under(&ns, "/a//"), want, "trailing slashes do not matter");
        assert_eq!(ns_children(ns.raw().0, b"/a"), want);
        assert_eq!(bindings_under(&ns, "/none"), Vec::new());
    }
}
