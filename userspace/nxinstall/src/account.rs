//! **The new machine's own account** (administration Part G.2): what the copy leaves out of the
//! pristine root, and what the installer writes in its place.
//!
//! In the lib rather than the program so a host test can hold the two halves to each other (PR
//! #348 review). They are one contract — every path [`PASS_OVER`] names is one [`write`] fills —
//! and a mistake in either is invisible to every gate CI runs: a pass-over list that lost `/home`
//! would ship the build's demo home onto every installed machine, with no account to use it, and
//! only the on-demand `check-install` would see it.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use fs_server_ext4::{BlockReader, BlockWriter};

use crate::copy;

/// **What the copy leaves for the installer to write**: the build's accounts' homes — `/home` is
/// made, empty — and its users and policy, which name the build's demo account. The new machine
/// gets its own.
pub const PASS_OVER: &[&[u8]] = &[b"/home", b"/system/users", b"/system/views.toml"];

/// The user database's first lines, as the build writes them.
const USERS_HEADER: &[u8] = b"# Nitrox user database (auth-service).\n# name:salt_hex:iterations:verifier_hex:home\n";

/// **Write the account onto the new root**: `/system/users` with its one record under `salt`, the
/// seeded policy naming it administrator, and `/home/<name>` with the three folders a home has
/// (`libfs::HOME_FOLDERS`, as the view broker makes them for `account --add`).
///
/// `name` must be one `libusers::valid_name` admits; the caller asked for it and checked it. The
/// record's buffers go back to the heap scrubbed, since one holds a password's verifier.
pub fn write<D>(dst: &D, name: &[u8], password: &[u8], salt: &[u8], now: i64) -> Result<(), String>
where
    D: BlockReader + BlockWriter,
{
    let mut home = [0u8; 64];
    let n = libusers::home_for(name, &mut home).ok_or("the account's home would not fit")?;
    let home = &home[..n];
    let mut line = alloc::vec![0u8; libusers::MAX_FILE];
    let len = libusers::write_record(&mut line, name, home, password, salt)
        .map_err(|r| String::from_utf8_lossy(r.why()).into_owned())?;
    let mut users = Vec::from(USERS_HEADER);
    users.extend_from_slice(&line[..len]);
    libkern::scrub(&mut line);
    let wrote = copy::put_file(dst, b"/system", b"users", &users, now);
    libkern::scrub(&mut users);
    wrote.map_err(|e| format!("/system/users: {e:?}"))?;

    let text = core::str::from_utf8(name).map_err(|_| "the account's name is not text")?;
    copy::put_file(dst, b"/system", b"views.toml", view_broker::policy::seed(text).as_bytes(), now)
        .map_err(|e| format!("/system/views.toml: {e:?}"))?;

    copy::put_dir(dst, b"/home", name, now).map_err(|e| format!("{}: {e:?}", String::from_utf8_lossy(home)))?;
    for folder in libfs::HOME_FOLDERS {
        copy::put_dir(dst, home, folder.as_bytes(), now).map_err(|e| format!("{folder}: {e:?}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;
    use fs_server_ext4::{FsError, ext4, mkfs};

    /// An in-memory disk, both halves of the traits the copy takes.
    struct Mem(RefCell<Vec<u8>>);

    impl BlockReader for Mem {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
            let v = self.0.borrow();
            let at = offset as usize;
            let bytes = v.get(at..at + buf.len()).ok_or(FsError::Io)?;
            buf.copy_from_slice(bytes);
            Ok(())
        }
    }

    impl BlockWriter for Mem {
        fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
            let mut v = self.0.borrow_mut();
            let at = offset as usize;
            v.get_mut(at..at + buf.len()).ok_or(FsError::Io)?.copy_from_slice(buf);
            Ok(())
        }
    }

    const NOW: i64 = 1_790_000_000;

    /// An empty filesystem of `mib` MiB, laid out the way the installer lays out its root.
    fn formatted(mib: u64) -> Mem {
        let disk = Mem(RefCell::new(alloc::vec![0u8; (mib << 20) as usize]));
        let params = mkfs::Params {
            blocks: (mib << 20) / 4096,
            block_size: 4096,
            bytes_per_inode: 16384,
            uuid: [7; 16],
            label: *b"nitrox-root\0\0\0\0\0",
            now: NOW,
        };
        mkfs::format(&disk, &params, &mut |_, _| {}).unwrap();
        disk
    }

    /// The names in `dir`, sorted.
    fn names(fs: &Mem, dir: &[u8]) -> Vec<String> {
        let ino = ext4::resolve_dir(fs, dir).unwrap();
        let mut out = Vec::new();
        let mut cursor = 0;
        loop {
            let next = ext4::read_dir(fs, ino, cursor, |_, _, name| {
                if name != b"." && name != b".." {
                    out.push(String::from_utf8_lossy(name).into_owned());
                }
                true
            })
            .unwrap();
            if next == 0 {
                break;
            }
            cursor = next;
        }
        out.sort();
        out
    }

    /// The whole of the file at `path`.
    fn read(fs: &Mem, path: &[u8]) -> Vec<u8> {
        let size = ext4::stat_file(fs, path).unwrap();
        let mut buf = alloc::vec![0u8; size.div_ceil(4096) * 4096];
        let mut at = 0;
        while at < size {
            let got = ext4::read_file_range(fs, path, at as u64, size - at, &mut buf[at..]).unwrap();
            assert!(got > 0, "a short read of {}", String::from_utf8_lossy(path));
            at += got;
        }
        buf.truncate(size);
        buf
    }

    /// **A pristine root as the build makes one**, in miniature: a program, the demo account's
    /// home with a file in it, its record, the policy naming it, and a declaration the copy must
    /// carry.
    fn pristine() -> Mem {
        let src = formatted(8);
        copy::put_dir(&src, b"/", b"bin", NOW).unwrap();
        copy::put_dir(&src, b"/", b"home", NOW).unwrap();
        copy::put_dir(&src, b"/", b"system", NOW).unwrap();
        copy::put_dir(&src, b"/home", b"alice", NOW).unwrap();
        copy::put_dir(&src, b"/home/alice", b"Documents", NOW).unwrap();
        let program: Vec<u8> = (0..10_000u32).map(|i| (i * 7) as u8).collect();
        copy::put_file(&src, b"/bin", b"tool", &program, NOW).unwrap();
        copy::put_file(&src, b"/home/alice/Documents", b"note", b"the build's", NOW).unwrap();
        let mut line = alloc::vec![0u8; libusers::MAX_FILE];
        let n = libusers::write_record_with(&mut line, b"alice", b"/home/alice", b"x", &[1; 16], 1).unwrap();
        let mut users = Vec::from(USERS_HEADER);
        users.extend_from_slice(&line[..n]);
        copy::put_file(&src, b"/system", b"users", &users, NOW).unwrap();
        copy::put_file(&src, b"/system", b"views.toml", view_broker::policy::seed("alice").as_bytes(), NOW)
            .unwrap();
        copy::put_file(&src, b"/system", b"services.toml", b"# the services\n", NOW).unwrap();
        src
    }

    /// **The install's root holds the new account and nothing of the build's** — the copy and
    /// the write run as the installer runs them, over the pass-over list it passes.
    #[test]
    fn an_installed_root_holds_its_own_account_alone() {
        let src = pristine();
        let dst = formatted(8);
        copy::copy_tree(&src, &dst, NOW, PASS_OVER, &mut |_| {}).unwrap();
        write(&dst, b"dana", b"a quiet password", &[9; 16], NOW).unwrap();

        // The build's home is gone, and the new one has a home's folders.
        assert_eq!(names(&dst, b"/home"), ["dana"]);
        let mut folders: Vec<String> = libfs::HOME_FOLDERS.iter().map(|f| String::from(*f)).collect();
        folders.sort();
        assert_eq!(names(&dst, b"/home/dana"), folders);

        // One record, the new account's, under the salt it was given.
        let users = read(&dst, b"/system/users");
        let records: Vec<_> = libusers::records(&users).collect();
        assert_eq!(records.len(), 1, "{}", String::from_utf8_lossy(&users));
        assert_eq!(records[0].name, b"dana");
        assert_eq!(records[0].home, b"/home/dana");
        assert_eq!(records[0].salt_hex, b"09090909090909090909090909090909");

        // The policy is the seed, naming the new account.
        assert_eq!(read(&dst, b"/system/views.toml"), view_broker::policy::seed("dana").as_bytes());

        // And everything else came across whole.
        assert_eq!(read(&dst, b"/bin/tool"), read(&src, b"/bin/tool"));
        assert_eq!(read(&dst, b"/system/services.toml"), b"# the services\n");
        assert_eq!(names(&dst, b"/system"), ["services.toml", "users", "views.toml"]);
    }
}
