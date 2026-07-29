//! `fs-server-ext4` — a read-only ext4 filesystem reader.
//!
//! Slice 7 Part 2: the parsing core, behind a [`BlockReader`] trait so it is
//! 100% host-testable against a fixture image. The real fs-server (Part 4)
//! implements `BlockReader` over `sys_io_submit`; the host tests implement it
//! over an in-memory image built by `mke2fs`.
//!
//! **Read-only, minimal** (`docs/planning/implementation-plan.md` slice 7): the
//! superblock, block-group descriptors, inodes, the **extent tree**, and a linear
//! directory walk — enough to resolve a path to a regular file and read its bytes.
//! Skips the journal, bigalloc, inline-data, htree-specific layout (a linear walk
//! still works), 64-bit block numbers, RW, xattrs, symlinks, and checksums.
//!
//! No `alloc`: [`read_file`] reads into a caller-provided buffer (the fs-server
//! passes a bounded scratch ≤ 64 KiB; see [`ext4::MAX_FILE`]). Parsing uses
//! bounded stack scratch (≤ one filesystem block).

#![cfg_attr(not(test), no_std)]

pub mod ext4;
pub mod serve;

pub use ext4::read_file;
pub use serve::{Served, serve_resolve};

/// Random-access read of the underlying block device, by byte offset. The reader
/// translates filesystem structures (the superblock at byte 1024, blocks at
/// `block_no * block_size`, …) into `read_at` calls; the implementor maps them to
/// device reads (the fs-server: `sys_io_submit` over the 512-byte sectors that
/// cover the range; host tests: a slice of an in-memory image).
pub trait BlockReader {
    /// Fill `buf` with the bytes at device byte `offset`. `Err` on any short or
    /// failed read.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError>;
}

/// A read failure.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FsError {
    /// A device read failed or returned short.
    Io,
    /// Not an ext4 filesystem (bad superblock magic), or a structure was
    /// malformed (bad extent magic, truncated directory, …).
    Corrupt,
    /// A feature this minimal reader does not support (an unknown `incompat`
    /// flag, a non-extent inode, a 64-bit filesystem, …).
    Unsupported,
    /// A path component was not found, or the path named a non-regular file.
    NotFound,
    /// The file is larger than the caller's buffer (the 64 KiB Phase-2 cap).
    TooLarge,
    /// A create/rename target already exists (POSIX `EEXIST`).
    Exists,
    /// An `rmdir` target directory is not empty (POSIX `ENOTEMPTY`).
    NotEmpty,
}

/// A block-device **writer** — the read-write counterpart of [`BlockReader`], for the
/// metadata mutation the write path needs (block/inode bitmaps, extent tree, inode,
/// superblock). `write_at` writes `buf` at absolute byte `offset` (device-block aligned in
/// practice). Read-only builds never require this; the RW server implements it over
/// `sys_io_submit` writes.
pub trait BlockWriter {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError>;
}

/// One contiguous mapping from a file's blocks to the device, for the **Model A** data
/// path (`docs/architecture/filesystem-data-path.md`). `device_lba` is a **filesystem
/// block** number (`0` = a hole → reads as zero); the kernel scales it to a byte offset by
/// the filesystem block size. Mirrors the wire `BlockRun` (`docs/spec/rsproto-block-ops.md`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct BlockRun {
    pub file_block: u64,
    pub device_lba: u64,
    pub length: u32,
    pub flags: u32,
}

// --- little-endian byte helpers (shared by the ext4 parser) -----------------

pub(crate) fn rd_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
pub(crate) fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Host-test fixtures shared by the parser tests ([`ext4`]) and the server-loop
/// tests ([`serve`]): an in-memory [`BlockReader`] over an `mke2fs`-built image.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{BlockReader, BlockWriter, FsError};
    use std::cell::RefCell;
    use std::io::Write;
    use std::process::Command;

    /// A read-write in-memory image (`BlockReader` + `BlockWriter`) for the write-path
    /// tests. Interior mutability (`RefCell`) so `write_at(&self, …)` matches the traits.
    pub(crate) struct RwImage(pub RefCell<Vec<u8>>);
    impl BlockReader for RwImage {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
            let v = self.0.borrow();
            let start = offset as usize;
            let end = start.checked_add(buf.len()).ok_or(FsError::Io)?;
            if end > v.len() {
                return Err(FsError::Io);
            }
            buf.copy_from_slice(&v[start..end]);
            Ok(())
        }
    }
    impl BlockWriter for RwImage {
        fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
            let mut v = self.0.borrow_mut();
            let start = offset as usize;
            let end = start.checked_add(buf.len()).ok_or(FsError::Io)?;
            if end > v.len() {
                return Err(FsError::Io);
            }
            v[start..end].copy_from_slice(buf);
            Ok(())
        }
    }

    /// A `BlockReader` over an in-memory image.
    pub(crate) struct ImageReader(pub Vec<u8>);
    impl BlockReader for ImageReader {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
            let start = offset as usize;
            let end = start.checked_add(buf.len()).ok_or(FsError::Io)?;
            if end > self.0.len() {
                return Err(FsError::Io);
            }
            buf.copy_from_slice(&self.0[start..end]);
            Ok(())
        }
    }

    /// Build a minimal ext4 image with `mke2fs -d` (no root, no mount) whose
    /// content tree holds `/system/current-generation`. The feature flags mirror
    /// the slice-5/Part-5 disk so the reader's supported feature set is exercised
    /// against a real e2fsprogs image. Panics with a clear message if `mke2fs` is
    /// unavailable (e2fsprogs is a project dependency — see Part 5).
    /// A fixed wall-clock instant the mutation tests stamp with: 2026-07-24
    /// 13:45:30 UTC. Fixed rather than "now" so a test can assert the exact
    /// value that reached the inode — the fs-server is handed the time, it does
    /// not read a clock itself.
    pub(crate) const TEST_NOW: i64 = 1_784_900_730;

    pub(crate) fn fixture(block_size: u32, content: &[u8]) -> Vec<u8> {
        // A unique dir per call (cargo runs tests in parallel threads) so they
        // never share / remove each other's staging tree.
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("nitrox-ext4-fix-{}-{}", std::process::id(), id));
        let sysdir = dir.join("system");
        std::fs::create_dir_all(&sysdir).unwrap();
        std::fs::File::create(sysdir.join("current-generation"))
            .unwrap()
            .write_all(content)
            .unwrap();
        let img = dir.join("rootfs.ext4");
        let status = Command::new("mke2fs")
            .args(["-q", "-F", "-t", "ext4"])
            .args(["-O", "^has_journal,^64bit,^metadata_csum,^resize_inode"])
            .args(["-b", &block_size.to_string()])
            .arg("-d")
            .arg(&dir)
            .arg(&img)
            .arg("4096") // blocks
            .status()
            .expect("mke2fs must be installed (e2fsprogs) to run fs-server-ext4 tests");
        assert!(status.success(), "mke2fs failed");
        let bytes = std::fs::read(&img).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ImageReader, RwImage, TEST_NOW, fixture};

    #[test]
    fn reads_current_generation_1k_blocks() {
        let r = ImageReader(fixture(1024, b"nitrox-gen-0001\n"));
        let mut out = [0u8; 256];
        let n = read_file(&r, b"/system/current-generation", &mut out).unwrap();
        assert_eq!(&out[..n], b"nitrox-gen-0001\n");
    }

    #[test]
    fn reads_current_generation_4k_blocks() {
        let r = ImageReader(fixture(4096, b"gen-4k\n"));
        let mut out = [0u8; 256];
        let n = read_file(&r, b"/system/current-generation", &mut out).unwrap();
        assert_eq!(&out[..n], b"gen-4k\n");
    }

    #[test]
    fn missing_path_is_not_found() {
        let r = ImageReader(fixture(1024, b"x\n"));
        let mut out = [0u8; 256];
        assert_eq!(read_file(&r, b"/system/nope", &mut out), Err(FsError::NotFound));
        assert_eq!(read_file(&r, b"/nope/file", &mut out), Err(FsError::NotFound));
    }

    #[test]
    fn directory_is_not_a_regular_file() {
        let r = ImageReader(fixture(1024, b"x\n"));
        let mut out = [0u8; 256];
        assert_eq!(read_file(&r, b"/system", &mut out), Err(FsError::NotFound));
    }

    /// Collect every entry name of a directory (draining the cursor across calls, as the
    /// server does when a listing spans messages).
    fn list_dir(r: &ImageReader, path: &[u8]) -> Vec<(String, u8)> {
        let dir_ino = ext4::resolve_dir(r, path).unwrap();
        let mut names = Vec::new();
        let mut cursor = 0u64;
        loop {
            let next = ext4::read_dir(r, dir_ino, cursor, |_ino, ft, name| {
                names.push((String::from_utf8_lossy(name).into_owned(), ft));
                true
            })
            .unwrap();
            if next == 0 {
                break;
            }
            cursor = next;
        }
        names
    }

    #[test]
    fn read_dir_lists_system_directory() {
        let r = ImageReader(fixture(1024, b"gen\n"));
        let names = list_dir(&r, b"/system");
        // ext4 `file_type` 1 = regular file.
        assert!(names.iter().any(|(n, ft)| n == "current-generation" && *ft == 1),
            "expected current-generation as a regular file, got {names:?}");
        assert!(names.iter().any(|(n, _)| n == "."), "must include .");
        assert!(names.iter().any(|(n, _)| n == ".."), "must include ..");
    }

    #[test]
    fn read_dir_lists_root_directory() {
        let r = ImageReader(fixture(4096, b"gen\n"));
        let names = list_dir(&r, b"/");
        assert!(names.iter().any(|(n, ft)| n == "system" && *ft == ext4::EXT4_FT_DIR),
            "root must contain the `system` subdirectory, got {names:?}");
    }

    #[test]
    fn rename_path_moves_a_file_across_directories_and_stays_e2fsck_clean() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(1024, b"gen\n")));
        let sys = ext4::resolve_dir(&rw, b"/system").unwrap();
        ext4::mkdir_at(&rw, sys, b"dst", TEST_NOW).unwrap();
        ext4::create_file(&rw, b"/system", b"mover", TEST_NOW).unwrap();
        ext4::grow_file(&rw, b"/system/mover", 1500, TEST_NOW).unwrap();

        ext4::rename_path(&rw, b"/system/mover", b"/system/dst/moved", false, TEST_NOW).unwrap();

        // Gone from the source, present at the destination, and still the same file —
        // the size proves the inode moved rather than a fresh empty one being created.
        assert_eq!(ext4::stat_file(&rw, b"/system/mover"), Err(FsError::NotFound));
        assert_eq!(ext4::stat_file(&rw, b"/system/dst/moved"), Ok(1500));
        assert_e2fsck_clean(&rw.0.into_inner(), "rename-cross");
    }

    #[test]
    fn rename_path_replaces_an_existing_file_only_when_asked() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(1024, b"gen\n")));
        ext4::create_file(&rw, b"/system", b"src", TEST_NOW).unwrap();
        ext4::grow_file(&rw, b"/system/src", 1200, TEST_NOW).unwrap();
        ext4::create_file(&rw, b"/system", b"victim", TEST_NOW).unwrap();
        ext4::grow_file(&rw, b"/system/victim", 3000, TEST_NOW).unwrap();

        // Without `replace` the destination is untouched — fail loud, as everywhere else.
        assert_eq!(
            ext4::rename_path(&rw, b"/system/src", b"/system/victim", false, TEST_NOW),
            Err(FsError::Exists)
        );
        assert_eq!(ext4::stat_file(&rw, b"/system/victim"), Ok(3000));
        assert_eq!(ext4::stat_file(&rw, b"/system/src"), Ok(1200));

        // With it, the destination becomes the source and the replaced inode is freed.
        let free_before = free_inodes(&rw);
        ext4::rename_path(&rw, b"/system/src", b"/system/victim", true, TEST_NOW).unwrap();
        assert_eq!(ext4::stat_file(&rw, b"/system/victim"), Ok(1200));
        assert_eq!(ext4::stat_file(&rw, b"/system/src"), Err(FsError::NotFound));
        assert_eq!(
            free_inodes(&rw),
            free_before + 1,
            "the replaced inode must be freed, not orphaned"
        );
        assert_e2fsck_clean(&rw.0.into_inner(), "rename-replace");
    }

    #[test]
    fn rename_path_moves_a_directory_and_fixes_its_parent_link() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(1024, b"gen\n")));
        let sys = ext4::resolve_dir(&rw, b"/system").unwrap();
        ext4::mkdir_at(&rw, sys, b"outer", TEST_NOW).unwrap();
        ext4::mkdir_at(&rw, sys, b"target", TEST_NOW).unwrap();
        let outer = ext4::resolve_dir(&rw, b"/system/outer").unwrap();
        ext4::create_file(&rw, b"/system/outer", b"payload", TEST_NOW).unwrap();
        let _ = outer;

        ext4::rename_path(&rw, b"/system/outer", b"/system/target/inner", false, TEST_NOW)
            .unwrap();

        // The directory and its contents moved wholesale…
        assert!(ext4::resolve_dir(&rw, b"/system/target/inner").is_ok());
        assert_eq!(ext4::stat_file(&rw, b"/system/target/inner/payload"), Ok(0));
        assert!(ext4::resolve_dir(&rw, b"/system/outer").is_err());
        // …and `..` now names the new parent, which is what `e2fsck` checks link counts
        // against — a stale `..` shows up as a link-count mismatch on both directories.
        assert_e2fsck_clean(&rw.0.into_inner(), "rename-dir");
    }

    #[test]
    fn rename_path_refuses_the_cases_that_would_corrupt() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(1024, b"gen\n")));
        let sys = ext4::resolve_dir(&rw, b"/system").unwrap();
        ext4::mkdir_at(&rw, sys, b"d", TEST_NOW).unwrap();
        ext4::mkdir_at(&rw, sys, b"e", TEST_NOW).unwrap();
        ext4::create_file(&rw, b"/system", b"f", TEST_NOW).unwrap();

        // Moving a directory inside itself would detach the subtree from the root.
        assert_eq!(
            ext4::rename_path(&rw, b"/system/d", b"/system/d/self", false, TEST_NOW),
            Err(FsError::Unsupported)
        );
        // Replacing a directory needs rmdir's emptiness + link bookkeeping; deferred.
        assert_eq!(
            ext4::rename_path(&rw, b"/system/f", b"/system/e", true, TEST_NOW),
            Err(FsError::Unsupported)
        );
        // A missing source is NotFound, not a silent success.
        assert_eq!(
            ext4::rename_path(&rw, b"/system/ghost", b"/system/g", false, TEST_NOW),
            Err(FsError::NotFound)
        );
        // Renaming onto itself is a no-op, and must not unlink the file.
        ext4::rename_path(&rw, b"/system/f", b"/system/f", false, TEST_NOW).unwrap();
        assert_eq!(ext4::stat_file(&rw, b"/system/f"), Ok(0));
        assert_e2fsck_clean(&rw.0.into_inner(), "rename-refuse");
    }

    /// The superblock's free-inode count — proves a replaced inode was actually freed
    /// rather than merely unlinked.
    fn free_inodes(rw: &RwImage) -> u32 {
        let img = rw.0.borrow();
        u32::from_le_bytes(img[1024 + 16..1024 + 20].try_into().unwrap())
    }

    #[test]
    fn a_directory_grows_past_its_first_block_and_stays_e2fsck_clean() {
        use std::cell::RefCell;
        // 1 KiB blocks and 200-byte names, so each record costs ~208 bytes and four fill
        // a block: 40 entries force the directory to grow several times. Before growth
        // existed this returned `TooLarge` on the fifth entry.
        let rw = RwImage(RefCell::new(fixture(1024, b"gen\n")));
        let name_of = |i: usize| {
            let mut n = std::format!("f{i:03}");
            while n.len() < 200 {
                n.push('x');
            }
            n
        };

        for i in 0..40 {
            ext4::create_file(&rw, b"/system", name_of(i).as_bytes(), TEST_NOW)
                .unwrap_or_else(|e| panic!("create {i} failed: {e:?}"));
        }

        // Every name present exactly once, through the paginated walk the server's
        // `ReadDir` uses — so the added blocks are reachable, not merely written.
        let listed = list_dir(&ImageReader(rw.0.borrow().clone()), b"/system");
        for i in 0..40 {
            let want = name_of(i);
            let seen = listed.iter().filter(|(n, _)| *n == want).count();
            assert_eq!(seen, 1, "entry {i} appears {seen} times, want 1");
        }
        // And `dir_lookup`'s multi-block walk finds them, which enumeration does not prove.
        for i in [0usize, 17, 39] {
            let path = std::format!("/system/{}", name_of(i));
            assert_eq!(
                ext4::stat_file(&rw, path.as_bytes()),
                Ok(0),
                "lookup of entry {i} failed after the directory grew"
            );
        }

        assert_e2fsck_clean(&rw.0.into_inner(), "dirgrow");
    }

    #[test]
    fn a_grown_directory_still_removes_cleanly() {
        use std::cell::RefCell;
        // Growing is half of it: `rmdir`'s emptiness scan and `unlink` must walk the added
        // blocks too, and the directory has to survive being emptied again.
        let rw = RwImage(RefCell::new(fixture(1024, b"gen\n")));
        let sys = ext4::resolve_dir(&rw, b"/system").unwrap();
        let name_of = |i: usize| {
            let mut n = std::format!("g{i:03}");
            while n.len() < 200 {
                n.push('y');
            }
            n
        };
        // 12 subdirectories at ~208 bytes each spans three 1 KiB blocks, and stays inside
        // the inline extent header's four entries (see `dir_insert` on that ceiling —
        // each `mkdir` allocates the child's own block between the parent's, so a parent
        // block costs an extent).
        for i in 0..12 {
            ext4::mkdir_at(&rw, sys, name_of(i).as_bytes(), TEST_NOW)
                .unwrap_or_else(|e| panic!("mkdir {i} failed: {e:?}"));
        }
        // A non-empty subdirectory living in a *later* block must still be refused, which
        // only works if the emptiness scan reaches past block 0.
        let outer = ext4::resolve_dir(&rw, name_path(&name_of(11)).as_bytes()).unwrap();
        ext4::mkdir_at(&rw, outer, b"inner", TEST_NOW).unwrap();
        assert_eq!(
            ext4::rmdir_at(&rw, sys, name_of(11).as_bytes(), TEST_NOW),
            Err(FsError::NotEmpty)
        );
        ext4::rmdir_at(&rw, outer, b"inner", TEST_NOW).unwrap();

        for i in 0..12 {
            ext4::rmdir_at(&rw, sys, name_of(i).as_bytes(), TEST_NOW)
                .unwrap_or_else(|e| panic!("rmdir {i} failed: {e:?}"));
        }
        let listed = list_dir(&ImageReader(rw.0.borrow().clone()), b"/system");
        assert!(
            !listed.iter().any(|(n, _)| n.starts_with('g')),
            "entries survived removal: {listed:?}"
        );
        assert_e2fsck_clean(&rw.0.into_inner(), "dirgrow-rm");
    }

    /// `/system/<name>` — the absolute path of an entry in the fixture's directory.
    fn name_path(name: &str) -> String {
        std::format!("/system/{name}")
    }

    #[test]
    fn creating_a_file_stamps_it_and_its_parent_directory() {
        use std::cell::RefCell;
        // The whole point of threading a clock into the mutation ops: a new inode
        // carries the time it was made, and the directory that now contains it
        // records that its contents changed.
        let r = RwImage(RefCell::new(fixture(1024, b"gen\n")));
        let sys = ext4::resolve_dir(&r, b"/system").unwrap();
        let before = dir_entry_mtime(&r, sys, b".").unwrap();

        ext4::create_file(&r, b"/system", b"stamped", TEST_NOW).unwrap();

        assert_eq!(
            dir_entry_mtime(&r, sys, b"stamped"),
            Some(TEST_NOW),
            "the new file must carry exactly the time it was created with"
        );
        let after = dir_entry_mtime(&r, sys, b".").unwrap();
        assert_eq!(after, TEST_NOW, "the parent directory's mtime must move");
        assert_ne!(before, after, "…and it must actually have changed");
    }

    #[test]
    fn mkdir_and_rmdir_stamp_the_parent() {
        use std::cell::RefCell;
        let r = RwImage(RefCell::new(fixture(1024, b"gen\n")));
        let sys = ext4::resolve_dir(&r, b"/system").unwrap();

        ext4::mkdir_at(&r, sys, b"kid", TEST_NOW).unwrap();
        assert_eq!(dir_entry_mtime(&r, sys, b"kid"), Some(TEST_NOW));
        assert_eq!(dir_entry_mtime(&r, sys, b"."), Some(TEST_NOW));

        // Removing an entry changes the directory's contents too — stamp it with a
        // later time and require the parent to move forward.
        let later = TEST_NOW + 600;
        ext4::rmdir_at(&r, sys, b"kid", later).unwrap();
        assert_eq!(dir_entry_mtime(&r, sys, b"."), Some(later));
    }

    #[test]
    fn a_rename_stamps_the_directory_but_not_the_file() {
        use std::cell::RefCell;
        // A file's name lives in the directory entry, not in its inode, so renaming
        // it changes the directory and leaves the file's own mtime alone.
        let r = RwImage(RefCell::new(fixture(1024, b"gen\n")));
        let sys = ext4::resolve_dir(&r, b"/system").unwrap();
        ext4::create_file(&r, b"/system", b"before", TEST_NOW).unwrap();

        let later = TEST_NOW + 3600;
        ext4::rename_at(&r, sys, b"before", b"after", later).unwrap();

        assert_eq!(dir_entry_mtime(&r, sys, b"after"), Some(TEST_NOW), "file untouched");
        assert_eq!(dir_entry_mtime(&r, sys, b"."), Some(later), "directory stamped");
    }

    #[test]
    fn growing_a_file_moves_its_mtime() {
        use std::cell::RefCell;
        let r = RwImage(RefCell::new(fixture(1024, b"gen\n")));
        let sys = ext4::resolve_dir(&r, b"/system").unwrap();
        ext4::create_file(&r, b"/system", b"grows", TEST_NOW).unwrap();

        let later = TEST_NOW + 42;
        ext4::grow_file(&r, b"/system/grows", 4096, later).unwrap();
        assert_eq!(dir_entry_mtime(&r, sys, b"grows"), Some(later));
    }

    /// The `mtime` a directory listing reports for `name` in directory `dir_ino`.
    fn dir_entry_mtime<R: crate::BlockReader>(r: &R, dir_ino: u32, name: &[u8]) -> Option<i64> {
        let mut found = None;
        ext4::read_dir_stat(r, dir_ino, 0, |_i, _ft, ename, st| {
            if ename == name {
                found = Some(st.mtime);
                return false;
            }
            true
        })
        .unwrap();
        found
    }

    #[test]
    fn read_dir_stat_reports_size_mode_and_mtime() {
        // The listing form must resolve each entry's inode: `list` reports
        // `Table<{name, size, kind, modified}>` straight off these fields.
        let content = b"generation-42\n";
        let r = ImageReader(fixture(1024, content));
        let dir_ino = ext4::resolve_dir(&r, b"/system").unwrap();
        let mut entries = Vec::new();
        ext4::read_dir_stat(&r, dir_ino, 0, |_i, _ft, name, st| {
            entries.push((String::from_utf8_lossy(name).into_owned(), *st));
            true
        })
        .unwrap();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let (_, file) = entries.iter().find(|(n, _)| n == "current-generation").unwrap();
        assert_eq!(file.size, content.len() as u64, "exact file size, not a block count");
        assert_eq!(file.mode & 0xF000, 0x8000, "S_IFREG");
        assert_ne!(file.mode & 0o777, 0, "permission bits must survive");
        // The fixture is built moments ago: a plausible, non-zero, non-future timestamp
        // catches a mis-decoded field (a zeroed or byte-swapped mtime fails this).
        assert!(
            file.mtime > now - 3600 && file.mtime <= now + 60,
            "mtime {} is not near now ({now})",
            file.mtime
        );

        let (_, dot) = entries.iter().find(|(n, _)| n == ".").unwrap();
        assert_eq!(dot.mode & 0xF000, 0x4000, "S_IFDIR for the directory itself");
        assert_eq!(dot.ino, dir_ino, "`.` is the directory's own inode");
    }

    #[test]
    fn read_dir_cursor_resumes_when_emit_stops_early() {
        // Stop after the first entry, then resume from the returned cursor and confirm the
        // union covers every entry exactly once (no drop, no dup at the boundary).
        let r = ImageReader(fixture(1024, b"gen\n"));
        let dir_ino = ext4::resolve_dir(&r, b"/system").unwrap();

        // Mirror the server's `DirReplyWriter::push` contract: returning `false` means
        // "this entry was NOT accepted (buffer full) — resume at it", so accept one entry
        // then reject the next.
        let mut first = Vec::new();
        let cursor = ext4::read_dir(&r, dir_ino, 0, |_i, _ft, name| {
            if first.len() >= 1 {
                return false; // reject (do not consume) the second entry
            }
            first.push(String::from_utf8_lossy(name).into_owned());
            true
        })
        .unwrap();
        assert_eq!(first.len(), 1);
        assert_ne!(cursor, 0, "a stop-early must report a resumable cursor");

        let mut rest = Vec::new();
        let done = ext4::read_dir(&r, dir_ino, cursor, |_i, _ft, name| {
            rest.push(String::from_utf8_lossy(name).into_owned());
            true
        })
        .unwrap();
        assert_eq!(done, 0);

        let full = list_dir(&r, b"/system");
        let mut union: Vec<String> = first;
        union.extend(rest);
        union.sort();
        let mut expected: Vec<String> = full.into_iter().map(|(n, _)| n).collect();
        expected.sort();
        assert_eq!(union, expected, "cursor split must partition the entries exactly");
    }

    #[test]
    fn resolve_dir_rejects_a_regular_file_and_missing_path() {
        let r = ImageReader(fixture(1024, b"gen\n"));
        assert_eq!(ext4::resolve_dir(&r, b"/system/current-generation"), Err(FsError::NotFound));
        assert_eq!(ext4::resolve_dir(&r, b"/nope"), Err(FsError::NotFound));
    }

    #[test]
    fn buffer_too_small_is_too_large() {
        let r = ImageReader(fixture(1024, b"0123456789\n"));
        let mut out = [0u8; 4]; // smaller than the 11-byte file
        assert_eq!(read_file(&r, b"/system/current-generation", &mut out), Err(FsError::TooLarge));
    }

    #[test]
    fn non_ext4_image_is_corrupt() {
        let r = ImageReader(vec![0u8; 8192]);
        let mut out = [0u8; 256];
        assert_eq!(read_file(&r, b"/x", &mut out), Err(FsError::Corrupt));
    }

    #[test]
    fn stat_returns_size_without_reading_content() {
        let r = ImageReader(fixture(1024, b"nitrox-gen-0001\n")); // 16 bytes
        assert_eq!(ext4::stat_file(&r, b"/system/current-generation"), Ok(16));
        assert_eq!(ext4::stat_file(&r, b"/system/nope"), Err(FsError::NotFound));
        assert_eq!(ext4::stat_file(&r, b"/system"), Err(FsError::NotFound)); // a dir
    }

    #[test]
    fn read_range_covers_offsets_tails_and_eof() {
        let content = b"0123456789ABCDEF\n"; // 17 bytes
        let r = ImageReader(fixture(1024, content));
        let mut out = [0u8; 32];
        // A mid-file window.
        let n = ext4::read_file_range(&r, b"/system/current-generation", 4, 6, &mut out).unwrap();
        assert_eq!(&out[..n], b"456789");
        // A tail clamped to the file size (ask 100 from offset 10 → 7 bytes).
        let n = ext4::read_file_range(&r, b"/system/current-generation", 10, 100, &mut out).unwrap();
        assert_eq!(&out[..n], b"ABCDEF\n");
        // The whole file from 0.
        let n = ext4::read_file_range(&r, b"/system/current-generation", 0, 17, &mut out).unwrap();
        assert_eq!(&out[..n], content);
        // Past end-of-file → zero bytes.
        assert_eq!(ext4::read_file_range(&r, b"/system/current-generation", 17, 8, &mut out), Ok(0));
    }

    #[test]
    fn read_range_spans_block_boundaries() {
        // A multi-block file (5000 bytes > one 1 KiB block) so a range crosses
        // block boundaries and exercises per-block extent lookup.
        let mut content = std::vec::Vec::new();
        for i in 0..5000u32 {
            content.push((i & 0xFF) as u8);
        }
        let r = ImageReader(fixture(1024, &content));
        let mut out = [0u8; 2048];
        // A 2000-byte window starting at 1500 spans blocks 1..4 (1 KiB blocks).
        let n = ext4::read_file_range(&r, b"/system/current-generation", 1500, 2000, &mut out)
            .unwrap();
        assert_eq!(n, 2000);
        assert_eq!(&out[..n], &content[1500..3500]);
    }

    #[test]
    fn map_range_maps_blocks_to_correct_device_data() {
        use crate::BlockRun;
        // A ~3.02-block file (4 KiB blocks) so runs span multiple blocks + a tail.
        let mut content = std::vec::Vec::new();
        for i in 0..(4096 * 3 + 100) {
            content.push((i * 7 % 251) as u8);
        }
        let r = ImageReader(fixture(4096, &content));
        let path = b"/system/current-generation";
        let bs = 4096usize;
        let file_blocks = content.len().div_ceil(bs) as u64; // 4

        let mut runs = [BlockRun::default(); 16];
        let n = ext4::map_range(&r, path, 0, file_blocks, &mut runs).unwrap();
        assert!(n >= 1);

        // Runs cover [0, file_blocks) contiguously in file-block space, none sparse.
        let mut next_fb = 0u64;
        for run in &runs[..n] {
            assert_eq!(run.file_block, next_fb);
            assert_ne!(run.device_lba, 0, "content is not sparse");
            next_fb += run.length as u64;
        }
        assert_eq!(next_fb, file_blocks);

        // Cross-check: each mapped device block holds the file's bytes for that block.
        for run in &runs[..n] {
            for k in 0..run.length as u64 {
                let fb = run.file_block + k;
                let dev_block = run.device_lba + k;
                let mut dev = std::vec![0u8; bs];
                r.read_at(dev_block * bs as u64, &mut dev).unwrap();
                let mut want = std::vec![0u8; bs];
                let got = ext4::read_file_range(&r, path, fb * bs as u64, bs, &mut want).unwrap();
                assert_eq!(&dev[..got], &want[..got], "file block {fb} device data mismatch");
            }
        }
    }

    /// Run `e2fsck -fn` over an image and assert it is clean (no changes needed, no errors).
    fn assert_e2fsck_clean(img: &[u8], tag: &str) {
        let dir = std::env::temp_dir()
            .join(std::format!("nitrox-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("img.ext4");
        std::fs::write(&p, img).unwrap();
        let out = std::process::Command::new("e2fsck")
            .args(["-fn", p.to_str().unwrap()])
            .output()
            .unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            out.status.success(),
            "e2fsck reported errors:\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    /// The inode number of a directory path (for the name-addressed mutation ops).
    fn dir_ino(rw: &RwImage, path: &[u8]) -> u32 {
        ext4::resolve_dir(rw, path).unwrap()
    }

    /// The entry names of a directory, as owned strings.
    fn names_of(rw: &RwImage, path: &[u8]) -> Vec<String> {
        let ino = dir_ino(rw, path);
        let mut names = Vec::new();
        let mut cursor = 0u64;
        loop {
            let next = ext4::read_dir(rw, ino, cursor, |_i, _ft, name| {
                names.push(String::from_utf8_lossy(name).into_owned());
                true
            })
            .unwrap();
            if next == 0 {
                break;
            }
            cursor = next;
        }
        names
    }


    #[test]
    fn mkdir_rename_rmdir_sequence_stays_readable_and_e2fsck_clean() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n")));
        let sys = dir_ino(&rw, b"/system");
        // The exact demo sequence on one directory.
        ext4::mkdir_at(&rw, sys, b"a", TEST_NOW).unwrap();
        ext4::rename_at(&rw, sys, b"a", b"b", TEST_NOW).unwrap();
        ext4::rmdir_at(&rw, sys, b"b", TEST_NOW).unwrap();
        // The directory must still enumerate cleanly (terminating), with a/b gone.
        let names = names_of(&rw, b"/system");
        assert!(!names.iter().any(|n| n == "a" || n == "b"), "a/b linger: {names:?}");
        assert!(names.iter().any(|n| n == "current-generation"));
        assert_e2fsck_clean(&rw.0.into_inner(), "seq");
    }

    #[test]
    fn mkdir_at_creates_a_subdir_and_stays_e2fsck_clean() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n")));
        let sys = dir_ino(&rw, b"/system");

        ext4::mkdir_at(&rw, sys, b"sub", TEST_NOW).unwrap();
        // It appears in /system, is itself a directory, and lists exactly `.`/`..`.
        assert!(names_of(&rw, b"/system").iter().any(|n| n == "sub"));
        let sub = ext4::resolve_dir(&rw, b"/system/sub").unwrap();
        assert!(sub > 10);
        let mut inner: Vec<String> = names_of(&rw, b"/system/sub");
        inner.sort();
        assert_eq!(inner, vec![".".to_string(), "..".to_string()]);

        // Duplicate is rejected; `.`/`..` are rejected.
        assert_eq!(ext4::mkdir_at(&rw, sys, b"sub", TEST_NOW), Err(FsError::Exists));
        assert_eq!(ext4::mkdir_at(&rw, sys, b".", TEST_NOW), Err(FsError::Unsupported));

        assert_e2fsck_clean(&rw.0.into_inner(), "mkdir");
    }

    #[test]
    fn unlink_at_removes_a_file_and_stays_e2fsck_clean() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n")));
        let sys = dir_ino(&rw, b"/system");

        // Create a file with content (so it owns a data block to free), then unlink it.
        let ino = ext4::create_file(&rw, b"/system", b"scratch", TEST_NOW).unwrap();
        ext4::grow_file(&rw, b"/system/scratch", 4096, TEST_NOW).unwrap();
        assert!(names_of(&rw, b"/system").iter().any(|n| n == "scratch"));

        ext4::unlink_at(&rw, sys, b"scratch", TEST_NOW).unwrap();
        assert!(!names_of(&rw, b"/system").iter().any(|n| n == "scratch"));
        // The name is gone; the inode was freed (a fresh create can reuse it).
        assert_eq!(ext4::stat_file(&rw, b"/system/scratch"), Err(FsError::NotFound));
        let _ = ino;

        // Unlink of a directory is rejected (use rmdir); missing name is NotFound.
        ext4::mkdir_at(&rw, sys, b"adir", TEST_NOW).unwrap();
        assert_eq!(ext4::unlink_at(&rw, sys, b"adir", TEST_NOW), Err(FsError::Unsupported));
        assert_eq!(ext4::unlink_at(&rw, sys, b"nope", TEST_NOW), Err(FsError::NotFound));

        assert_e2fsck_clean(&rw.0.into_inner(), "unlink");
    }

    #[test]
    fn rmdir_at_removes_empty_dir_rejects_nonempty_and_stays_e2fsck_clean() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n")));
        let sys = dir_ino(&rw, b"/system");

        ext4::mkdir_at(&rw, sys, b"empty", TEST_NOW).unwrap();
        ext4::mkdir_at(&rw, sys, b"full", TEST_NOW).unwrap();
        // Put a file inside `full` so it is non-empty.
        ext4::create_file(&rw, b"/system/full", b"f", TEST_NOW).unwrap();

        // Non-empty rmdir is refused; a regular file is refused (use unlink).
        let full = dir_ino(&rw, b"/system/full");
        let _ = full;
        assert_eq!(ext4::rmdir_at(&rw, sys, b"full", TEST_NOW), Err(FsError::NotEmpty));
        ext4::create_file(&rw, b"/system", b"afile", TEST_NOW).unwrap();
        assert_eq!(ext4::rmdir_at(&rw, sys, b"afile", TEST_NOW), Err(FsError::Unsupported));

        ext4::rmdir_at(&rw, sys, b"empty", TEST_NOW).unwrap();
        assert!(!names_of(&rw, b"/system").iter().any(|n| n == "empty"));

        assert_e2fsck_clean(&rw.0.into_inner(), "rmdir");
    }

    #[test]
    fn rename_at_moves_within_a_dir_and_stays_e2fsck_clean() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n")));
        let sys = dir_ino(&rw, b"/system");

        ext4::create_file(&rw, b"/system", b"before", TEST_NOW).unwrap();
        ext4::rename_at(&rw, sys, b"before", b"after", TEST_NOW).unwrap();
        let names = names_of(&rw, b"/system");
        assert!(names.iter().any(|n| n == "after"));
        assert!(!names.iter().any(|n| n == "before"));

        // Renaming onto an existing name is refused; a missing source is NotFound.
        ext4::create_file(&rw, b"/system", b"other", TEST_NOW).unwrap();
        assert_eq!(ext4::rename_at(&rw, sys, b"after", b"other", TEST_NOW), Err(FsError::Exists));
        assert_eq!(ext4::rename_at(&rw, sys, b"ghost", b"x", TEST_NOW), Err(FsError::NotFound));

        assert_e2fsck_clean(&rw.0.into_inner(), "rename");
    }

    #[test]
    fn truncate_frees_blocks_and_stays_e2fsck_clean() {
        use crate::BlockRun;
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n")));
        let path = b"/system/current-generation";

        // Grow to 5 blocks, then cut back to 1.5 — so the shrink must drop whole
        // extents *and* shorten the one straddling the new end, which is where a
        // naive "free everything past the last kept extent" gets it wrong.
        ext4::grow_file(&rw, path, 5 * 4096, TEST_NOW).unwrap();
        let before_free = free_blocks(&rw);

        assert_eq!(ext4::truncate_file(&rw, path, 6000, TEST_NOW), Ok(6000));
        assert_eq!(ext4::stat_file(&rw, path), Ok(6000));

        // 6000 bytes needs 2 blocks; the other 3 must have come back.
        let mut runs = [BlockRun::default(); 8];
        let (size, _, n) = ext4::map_file(&rw, path, &mut runs).unwrap();
        assert_eq!(size, 6000);
        let covered: u64 = runs[..n].iter().map(|r| r.length as u64).sum();
        assert_eq!(covered, 2, "only the blocks holding live bytes are mapped");
        assert_eq!(
            free_blocks(&rw),
            before_free + 3,
            "freed blocks must return to the allocator, not just leave the extent tree"
        );

        assert_e2fsck_clean(&rw.0.into_inner(), "truncate");
    }

    #[test]
    fn truncate_to_zero_and_partial_blocks() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n")));
        let path = b"/system/current-generation";
        ext4::grow_file(&rw, path, 3 * 4096, TEST_NOW).unwrap();

        // A size inside the first block keeps exactly that block: the bytes past the
        // new end are slack, as they are after any short write.
        assert_eq!(ext4::truncate_file(&rw, path, 1, TEST_NOW), Ok(1));
        assert_eq!(ext4::stat_file(&rw, path), Ok(1));
        assert_e2fsck_clean(&rw.0.borrow(), "truncate-partial");

        // Zero keeps nothing at all.
        assert_eq!(ext4::truncate_file(&rw, path, 0, TEST_NOW), Ok(0));
        assert_eq!(ext4::stat_file(&rw, path), Ok(0));
        let mut runs = [crate::BlockRun::default(); 8];
        let (size, _, n) = ext4::map_file(&rw, path, &mut runs).unwrap();
        assert_eq!(size, 0);
        assert_eq!(runs[..n].iter().map(|r| r.length as u64).sum::<u64>(), 0);
        assert_e2fsck_clean(&rw.0.into_inner(), "truncate-zero");
    }

    #[test]
    fn truncate_never_grows_and_reports_the_current_size() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n")));
        let path = b"/system/current-generation";
        ext4::grow_file(&rw, path, 4096, TEST_NOW).unwrap();

        // At or above the current size is a no-op reporting the current size —
        // growing allocates, which is `grow_file`'s job. Silently *extending* here
        // would hand back a file whose tail was never written.
        assert_eq!(ext4::truncate_file(&rw, path, 4096, TEST_NOW), Ok(4096));
        assert_eq!(ext4::truncate_file(&rw, path, 999_999, TEST_NOW), Ok(4096));
        assert_eq!(ext4::stat_file(&rw, path), Ok(4096));
    }

    #[test]
    fn truncate_moves_mtime_and_rejects_a_directory() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(1024, b"seed\n")));
        let sys = ext4::resolve_dir(&rw, b"/system").unwrap();
        ext4::grow_file(&rw, b"/system/current-generation", 2048, TEST_NOW).unwrap();

        let later = TEST_NOW + 900;
        ext4::truncate_file(&rw, b"/system/current-generation", 10, later).unwrap();
        assert_eq!(dir_entry_mtime(&rw, sys, b"current-generation"), Some(later));

        // A directory is not truncatable — its size is its data, not a byte count a
        // caller may set.
        assert_eq!(
            ext4::truncate_file(&rw, b"/system", 0, TEST_NOW),
            Err(FsError::NotFound)
        );
    }

    /// The superblock's free-block count — what proves a freed block reached the
    /// allocator rather than merely leaving the inode's extent tree.
    fn free_blocks(rw: &RwImage) -> u32 {
        let img = rw.0.borrow();
        u32::from_le_bytes(img[1024 + 12..1024 + 16].try_into().unwrap())
    }

    #[test]
    fn grow_file_appends_blocks_and_stays_e2fsck_clean() {
        use crate::BlockRun;
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n"))); // 5-byte file → 1 block
        let path = b"/system/current-generation";

        // Grow 5 → 5000 bytes (1 → 2 blocks): allocate + extend the extent tree + inode.
        assert_eq!(ext4::grow_file(&rw, path, 5000, TEST_NOW), Ok(5000));
        assert_eq!(ext4::stat_file(&rw, path), Ok(5000));

        // The block map now covers 2 blocks, none sparse.
        let mut runs = [BlockRun::default(); 8];
        let (size, _, n) = ext4::map_file(&rw, path, &mut runs).unwrap();
        assert_eq!(size, 5000);
        let covered: u64 = runs[..n].iter().map(|r| r.length as u64).sum();
        assert_eq!(covered, 2);
        for r in &runs[..n] {
            assert_ne!(r.device_lba, 0);
        }

        // e2fsck the mutated image: the metadata (extent tree, bitmap, free counts, inode)
        // must be fully consistent. `-fn` makes no changes and exits non-zero on any error.
        let img = rw.0.into_inner();
        let dir = std::env::temp_dir().join(std::format!("nitrox-grow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("img.ext4");
        std::fs::write(&p, &img).unwrap();
        let out = std::process::Command::new("e2fsck")
            .args(["-fn", p.to_str().unwrap()])
            .output()
            .unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            out.status.success(),
            "e2fsck reported errors:\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    #[test]
    fn create_file_links_grows_and_stays_e2fsck_clean() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n")));

        // Create a new regular file in /system.
        let ino = ext4::create_file(&rw, b"/system", b"newfile", TEST_NOW).unwrap();
        assert!(ino > 10, "should not reuse a reserved inode");
        // It resolves and is empty.
        assert_eq!(ext4::stat_file(&rw, b"/system/newfile"), Ok(0));
        // Idempotent: creating again returns the same inode.
        assert_eq!(ext4::create_file(&rw, b"/system", b"newfile", TEST_NOW), Ok(ino));
        // Grow + write path works on the freshly-created file.
        assert_eq!(ext4::grow_file(&rw, b"/system/newfile", 100, TEST_NOW), Ok(100));
        assert_eq!(ext4::stat_file(&rw, b"/system/newfile"), Ok(100));

        // e2fsck the mutated image: the new inode, its dir entry, the bitmaps + counts, and
        // the extent must all be consistent.
        let img = rw.0.into_inner();
        let dir = std::env::temp_dir().join(std::format!("nitrox-create-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("img.ext4");
        std::fs::write(&p, &img).unwrap();
        let out = std::process::Command::new("e2fsck")
            .args(["-fn", p.to_str().unwrap()])
            .output()
            .unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            out.status.success(),
            "e2fsck reported errors:\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}
