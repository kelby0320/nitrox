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
/// died, is cut to nothing; the new copy is created at its size, refused unless it is that size,
/// written through a mapping, unmapped, synced, and then renamed over the file. A crash before the rename leaves the old file
/// as it was, and the rename is the filesystem's one operation. `Err` names the step that failed.
pub fn install<'a, M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>, bytes: &[u8]) -> Result<(), &'static [u8]> {
    let ns = ns.into();
    let rights = Rights::MAP_READ | Rights::MAP_WRITE | Rights::INSPECT;
    // A leftover, cut to nothing. Absent is the usual case, and not an error.
    let _: libos::Result<Handle<File, MapReadWrite>> = block_on(fs::truncate(ns, USERS_NEW, 0, rights));
    let made: libos::Result<Handle<File, MapReadWrite>> =
        block_on(fs::create(ns, USERS_NEW, bytes.len() as u64, rights));
    let Ok(file) = made else {
        return Err(b"users.new could not be created");
    };
    // **A create only grows**: a leftover the truncate did not cut keeps its tail, which the rename
    // would make the file's, and the next boot would read.
    if file.stat().ok().map(|info| info.size) != Some(bytes.len() as u64) {
        return Err(b"users.new kept a leftover's tail");
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use libos::{MapRead, NsMutable, mock};

    fn fresh() -> Handle<Namespace, NsMutable> {
        mock::reset();
        Handle::<Namespace, NsMutable>::create().unwrap()
    }

    fn read(ns: &Handle<Namespace, NsMutable>, path: &str) -> std::vec::Vec<u8> {
        let file = block_on(ns.lookup::<File, MapRead>(path, Rights::MAP_READ | Rights::INSPECT)).unwrap();
        let mut out = std::vec![0u8; file.stat().unwrap().size as usize];
        file.map(PAGE).unwrap().read_into(0, &mut out).unwrap();
        out
    }

    #[test]
    fn an_install_writes_exactly_the_new_bytes() {
        let ns = fresh();
        install(&ns, b"the database as it was, longer\n").unwrap();
        install(&ns, b"the new one\n").unwrap();
        assert_eq!(read(&ns, USERS), b"the new one\n");
    }

    /// **A leftover longer than the new copy, which the truncate did not cut, fails the install**,
    /// and the file is as it was: renamed over it, its tail would come back at the next boot.
    #[test]
    fn a_leftover_the_truncate_did_not_cut_fails_the_install() {
        let ns = fresh();
        install(&ns, b"the database as it was\n").unwrap();
        let leftover: Handle<File, MapReadWrite> =
            block_on(fs::create(&ns, USERS_NEW, 64, Rights::MAP_READ | Rights::MAP_WRITE)).unwrap();
        leftover.map(PAGE).unwrap().write_from(0, &[b'x'; 64]).unwrap();
        drop(leftover);
        mock::ignore_next_truncate();
        assert_eq!(install(&ns, b"shorter\n"), Err(&b"users.new kept a leftover's tail"[..]));
        assert_eq!(read(&ns, USERS), b"the database as it was\n");
    }

    /// **A write-back that fails leaves the file as it was**, and nothing open or mapped, as the
    /// server's copy follows the file. The model cannot fail a file's create or a rename.
    #[test]
    fn a_write_back_that_fails_leaves_the_file_as_it_was() {
        let ns = fresh();
        install(&ns, b"as it was\n").unwrap();
        let held = (mock::open_handles(), mock::mappings());
        mock::fail_next_submit(libkern::KError::IoError.as_i32());
        assert_eq!(install(&ns, b"new\n"), Err(&b"users.new could not be written back"[..]));
        assert_eq!(read(&ns, USERS), b"as it was\n");
        assert_eq!((mock::open_handles(), mock::mappings()), held);
    }
}
