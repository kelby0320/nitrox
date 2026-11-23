//! Writing the user database: a new copy installed over the old, atomically.

use libkern::Rights;
use libos::{CanLookup, File, Handle, MapReadWrite, Namespace, Ref, block_on, fs};
use librsproto::namespace::RENAME_REPLACE;

/// One page; the map granularity for reading the user DB.
pub const PAGE: usize = 4096;
/// The user database.
pub const USERS: &str = "/system/users";
/// The name a new copy is written under before it is renamed over [`USERS`].
pub const USERS_NEW: &str = "/system/users.new";

/// **Replace `/system/users` with `bytes`, atomically.** A leftover `users.new`, from a write that
/// died, is cut to nothing; the new copy is created at its size, written through a mapping,
/// unmapped, synced, and then renamed over the file. A crash before the rename leaves the old file
/// as it was, and the rename is the filesystem's one operation. `Err` names the step that failed.
pub fn install<'a, M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>, bytes: &[u8]) -> Result<(), &'static [u8]> {
    let ns = ns.into();
    let rights = Rights::MAP_READ | Rights::MAP_WRITE;
    // A leftover, cut to nothing. Absent is the usual case, and not an error.
    let _: libos::Result<Handle<File, MapReadWrite>> = block_on(fs::truncate(ns, USERS_NEW, 0, rights));
    let made: libos::Result<Handle<File, MapReadWrite>> =
        block_on(fs::create(ns, USERS_NEW, bytes.len() as u64, rights));
    let Ok(file) = made else {
        return Err(b"users.new could not be created");
    };
    if !bytes.is_empty() {
        let Ok(mut mapping) = file.map(PAGE) else {
            return Err(b"users.new could not be mapped");
        };
        // The file is at most a page, `libusers`' bound. Unmapped before the sync, as `libfs` does: a
        // write-back that begins with a writable mapping cannot call the file clean.
        if mapping.write_from(0, bytes).is_err() {
            return Err(b"users.new could not be mapped");
        }
    }
    let synced = block_on(fs::flush(&file));
    drop(file);
    if synced.is_err() {
        return Err(b"users.new could not be written back");
    }
    block_on(fs::rename(ns, USERS_NEW, USERS, RENAME_REPLACE))
        .map_err(|_| &b"users.new could not be renamed over users"[..])
}
