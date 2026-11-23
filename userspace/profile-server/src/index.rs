//! Each projection's merged view of the packages, when it is built again, and what it answers.

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use libkern::KError;
use librsproto::file::DirEntry;
use librsproto::session::DirError;

/// One projected program, with the metadata the store's own directory reported for it: the
/// view carries real sizes and timestamps rather than inventing numbers for files it does
/// not own.
pub struct Entry {
    /// Its name in the projection.
    pub name: String,
    /// Index into the manifest's package list.
    pub pkg: usize,
    /// The store's inode number for it.
    pub inode: u32,
    /// Its `DIRENT_KIND_*`.
    pub kind: u8,
    /// Its `st_mode` bits.
    pub mode: u16,
    /// Its size in bytes.
    pub size: u64,
    /// Its modification time, seconds since the Unix epoch.
    pub mtime: i64,
}

/// A projection's merged view.
pub type View = Rc<[Entry]>;

/// Whether opening a package's directory failed only because it has none: such a package is
/// read whole, with nothing in it. Any other failure may pass, so the view is not kept.
pub fn has_none(e: DirError) -> bool {
    e == DirError::Transport(KError::NotFound.as_i32())
}

/// Package `pkg`'s entries, from `opened`, its directory's open, listed by `list`; and whether it
/// read whole, listed to its end or holding no such directory.
pub fn read_package<D>(
    pkg: usize,
    opened: Result<D, DirError>,
    list: impl FnOnce(&mut D, &mut dyn FnMut(&DirEntry<'_>) -> bool) -> Result<bool, DirError>,
) -> (Vec<Entry>, bool) {
    let mut dir = match opened {
        Ok(dir) => dir,
        Err(e) => return (Vec::new(), has_none(e)),
    };
    let mut out = Vec::new();
    let listed = list(&mut dir, &mut |e| {
        // A non-UTF-8 program name is not addressable.
        if let (false, Ok(name)) = (e.name == b"." || e.name == b"..", core::str::from_utf8(e.name)) {
            let (inode, kind, mode, size, mtime) = (e.inode, e.kind, e.mode, e.size, e.mtime);
            out.push(Entry { name: String::from(name), pkg, inode, kind, mode, size, mtime });
        }
        true
    });
    (out, listed.is_ok())
}

/// Merge `count` packages' listings in manifest order, `read(i)` giving package `i`'s entries and
/// whether it read whole: the first provider of a name wins, as a resolve finds it.
pub fn merge(count: usize, mut read: impl FnMut(usize) -> (Vec<Entry>, bool)) -> (Vec<Entry>, bool) {
    let mut out: Vec<Entry> = Vec::new();
    let mut whole = true;
    for i in 0..count {
        let (entries, read_whole) = read(i);
        whole &= read_whole;
        for mut e in entries {
            if !out.iter().any(|x| x.name == e.name) {
                e.pkg = i;
                out.push(e);
            }
        }
    }
    (out, whole)
}

/// Each projection's view, **kept once every package read whole**: one refused while the root
/// filesystem's table was full would otherwise stay unlisted and unresolved until a restart. One
/// not whole is built again only where that could change an answer, a listing's first page or a
/// name it lacks, and never under a listing begun.
///
/// Caching needs no invalidation: a store path is content-addressed, so its contents
/// cannot change, and the manifest (the profile's membership) is read once at startup.
/// See `TODO(profile-generation-refresh)`.
#[derive(Default)]
pub struct Indexes {
    views: Vec<(String, View, bool)>,
}

impl Indexes {
    /// Projection `dir`'s view and whether it is whole: the one held, or what `build` makes when
    /// none is held, or when it is not whole and `fresh`.
    fn view(&mut self, dir: &str, fresh: bool, build: impl FnOnce() -> (Vec<Entry>, bool)) -> (View, bool) {
        let found = self.views.iter().position(|(d, _, _)| d == dir);
        if let Some(at) = found
            && (self.views[at].2 || !fresh)
        {
            return (Rc::clone(&self.views[at].1), self.views[at].2);
        }
        let (built, whole) = build();
        let view: View = built.into();
        let held = (String::from(dir), Rc::clone(&view), whole);
        match found {
            Some(at) => self.views[at] = held,
            None => self.views.push(held),
        }
        (view, whole)
    }

    /// The package providing `name` in projection `dir`. A name the view lacks is `NotFound` once
    /// every package has read, and `WouldBlock` while one that may hold it has not.
    pub fn resolve(
        &mut self,
        dir: &str,
        name: &str,
        mut build: impl FnMut() -> (Vec<Entry>, bool),
    ) -> Result<usize, KError> {
        for fresh in [false, true] {
            let (view, whole) = self.view(dir, fresh, &mut build);
            match view.iter().find(|e| e.name == name) {
                Some(e) => return Ok(e.pkg),
                None if whole => return Err(KError::NotFound),
                None => {}
            }
        }
        Err(KError::WouldBlock)
    }

    /// The view a listing's page at `cursor` reads: from its first page on, the one it began
    /// with, `held`, so no listing reads two.
    pub fn listing(
        &mut self,
        dir: &str,
        cursor: u64,
        held: &mut Option<View>,
        build: impl FnOnce() -> (Vec<Entry>, bool),
    ) -> View {
        match held {
            Some(view) if cursor != 0 => Rc::clone(view),
            _ => Rc::clone(held.insert(self.view(dir, true, build).0)),
        }
    }
}

#[cfg(test)]
mod tests;
