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
