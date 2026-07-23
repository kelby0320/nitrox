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
    use crate::test_support::{ImageReader, RwImage, fixture};

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
        ext4::mkdir_at(&rw, sys, b"a").unwrap();
        ext4::rename_at(&rw, sys, b"a", b"b").unwrap();
        ext4::rmdir_at(&rw, sys, b"b").unwrap();
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

        ext4::mkdir_at(&rw, sys, b"sub").unwrap();
        // It appears in /system, is itself a directory, and lists exactly `.`/`..`.
        assert!(names_of(&rw, b"/system").iter().any(|n| n == "sub"));
        let sub = ext4::resolve_dir(&rw, b"/system/sub").unwrap();
        assert!(sub > 10);
        let mut inner: Vec<String> = names_of(&rw, b"/system/sub");
        inner.sort();
        assert_eq!(inner, vec![".".to_string(), "..".to_string()]);

        // Duplicate is rejected; `.`/`..` are rejected.
        assert_eq!(ext4::mkdir_at(&rw, sys, b"sub"), Err(FsError::Exists));
        assert_eq!(ext4::mkdir_at(&rw, sys, b"."), Err(FsError::Unsupported));

        assert_e2fsck_clean(&rw.0.into_inner(), "mkdir");
    }

    #[test]
    fn unlink_at_removes_a_file_and_stays_e2fsck_clean() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n")));
        let sys = dir_ino(&rw, b"/system");

        // Create a file with content (so it owns a data block to free), then unlink it.
        let ino = ext4::create_file(&rw, b"/system", b"scratch").unwrap();
        ext4::grow_file(&rw, b"/system/scratch", 4096).unwrap();
        assert!(names_of(&rw, b"/system").iter().any(|n| n == "scratch"));

        ext4::unlink_at(&rw, sys, b"scratch").unwrap();
        assert!(!names_of(&rw, b"/system").iter().any(|n| n == "scratch"));
        // The name is gone; the inode was freed (a fresh create can reuse it).
        assert_eq!(ext4::stat_file(&rw, b"/system/scratch"), Err(FsError::NotFound));
        let _ = ino;

        // Unlink of a directory is rejected (use rmdir); missing name is NotFound.
        ext4::mkdir_at(&rw, sys, b"adir").unwrap();
        assert_eq!(ext4::unlink_at(&rw, sys, b"adir"), Err(FsError::Unsupported));
        assert_eq!(ext4::unlink_at(&rw, sys, b"nope"), Err(FsError::NotFound));

        assert_e2fsck_clean(&rw.0.into_inner(), "unlink");
    }

    #[test]
    fn rmdir_at_removes_empty_dir_rejects_nonempty_and_stays_e2fsck_clean() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n")));
        let sys = dir_ino(&rw, b"/system");

        ext4::mkdir_at(&rw, sys, b"empty").unwrap();
        ext4::mkdir_at(&rw, sys, b"full").unwrap();
        // Put a file inside `full` so it is non-empty.
        ext4::create_file(&rw, b"/system/full", b"f").unwrap();

        // Non-empty rmdir is refused; a regular file is refused (use unlink).
        let full = dir_ino(&rw, b"/system/full");
        let _ = full;
        assert_eq!(ext4::rmdir_at(&rw, sys, b"full"), Err(FsError::NotEmpty));
        ext4::create_file(&rw, b"/system", b"afile").unwrap();
        assert_eq!(ext4::rmdir_at(&rw, sys, b"afile"), Err(FsError::Unsupported));

        ext4::rmdir_at(&rw, sys, b"empty").unwrap();
        assert!(!names_of(&rw, b"/system").iter().any(|n| n == "empty"));

        assert_e2fsck_clean(&rw.0.into_inner(), "rmdir");
    }

    #[test]
    fn rename_at_moves_within_a_dir_and_stays_e2fsck_clean() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n")));
        let sys = dir_ino(&rw, b"/system");

        ext4::create_file(&rw, b"/system", b"before").unwrap();
        ext4::rename_at(&rw, sys, b"before", b"after").unwrap();
        let names = names_of(&rw, b"/system");
        assert!(names.iter().any(|n| n == "after"));
        assert!(!names.iter().any(|n| n == "before"));

        // Renaming onto an existing name is refused; a missing source is NotFound.
        ext4::create_file(&rw, b"/system", b"other").unwrap();
        assert_eq!(ext4::rename_at(&rw, sys, b"after", b"other"), Err(FsError::Exists));
        assert_eq!(ext4::rename_at(&rw, sys, b"ghost", b"x"), Err(FsError::NotFound));

        assert_e2fsck_clean(&rw.0.into_inner(), "rename");
    }

    #[test]
    fn grow_file_appends_blocks_and_stays_e2fsck_clean() {
        use crate::BlockRun;
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n"))); // 5-byte file → 1 block
        let path = b"/system/current-generation";

        // Grow 5 → 5000 bytes (1 → 2 blocks): allocate + extend the extent tree + inode.
        assert_eq!(ext4::grow_file(&rw, path, 5000), Ok(5000));
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
        let ino = ext4::create_file(&rw, b"/system", b"newfile").unwrap();
        assert!(ino > 10, "should not reuse a reserved inode");
        // It resolves and is empty.
        assert_eq!(ext4::stat_file(&rw, b"/system/newfile"), Ok(0));
        // Idempotent: creating again returns the same inode.
        assert_eq!(ext4::create_file(&rw, b"/system", b"newfile"), Ok(ino));
        // Grow + write path works on the freshly-created file.
        assert_eq!(ext4::grow_file(&rw, b"/system/newfile", 100), Ok(100));
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
