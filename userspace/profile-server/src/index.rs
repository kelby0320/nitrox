//! Each projection's merged view of the packages, and when it is kept.

use alloc::string::String;
use alloc::vec::Vec;

use libkern::KError;
use librsproto::session::DirError;

/// One projected program, with the metadata the store's own directory reported for it: the
/// view carries real sizes and timestamps rather than inventing numbers for files it does
/// not own.
pub struct Entry {
    pub name: String,
    /// Index into the manifest's package list.
    pub pkg: usize,
    pub inode: u32,
    pub kind: u8,
    pub mode: u16,
    pub size: u64,
    pub mtime: i64,
}

/// Whether opening a package's directory failed only because it has none: such a package is
/// read whole, with nothing in it. Any other failure may pass, so the view is not kept.
pub fn has_none(e: DirError) -> bool {
    e == DirError::Transport(KError::NotFound.as_i32())
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

/// Each projection's view, **kept only once every package read whole**: one refused while the root
/// filesystem's table was full would otherwise stay unlisted and unresolved until a restart.
///
/// Caching needs no invalidation: a store path is content-addressed, so its contents
/// cannot change, and the manifest (the profile's membership) is read once at startup.
/// See `TODO(profile-generation-refresh)`.
#[derive(Default)]
pub struct Indexes {
    views: Vec<(String, Vec<Entry>, bool)>,
}

impl Indexes {
    /// Projection `dir`'s view: the one kept, or what `build` makes now.
    pub fn of(&mut self, dir: &str, build: impl FnOnce() -> (Vec<Entry>, bool)) -> &[Entry] {
        let found = self.views.iter().position(|(d, _, _)| d == dir);
        let at = match found {
            Some(at) if self.views[at].2 => at,
            _ => {
                let (built, whole) = build();
                let view = (String::from(dir), built, whole);
                match found {
                    Some(at) => {
                        self.views[at] = view;
                        at
                    }
                    None => {
                        self.views.push(view);
                        self.views.len() - 1
                    }
                }
            }
        };
        &self.views[at].1
    }
}

#[cfg(test)]
mod tests;
