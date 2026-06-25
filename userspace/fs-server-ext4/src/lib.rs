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
    use super::{BlockReader, FsError};
    use std::io::Write;
    use std::process::Command;

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
    use crate::test_support::{ImageReader, fixture};

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
}
