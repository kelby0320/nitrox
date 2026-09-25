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
pub mod mkfs;
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

    /// Whether this is a **read-only mount** — `true` only for [`ReadOnly`]. What the block-file
    /// reply marks a file with comes from here, so the mark and the refusal of every write are
    /// one fact, the type the server serves through.
    fn read_only(&self) -> bool {
        false
    }
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
    /// A write to a **read-only mount** ([`ReadOnly`], administration Part C.3). What the
    /// server answers is `NoAccess`.
    ReadOnly,
}

/// A block-device **writer** — the read-write counterpart of [`BlockReader`], for the
/// metadata mutation the write path needs (block/inode bitmaps, extent tree, inode,
/// superblock). `write_at` writes `buf` at absolute byte `offset` (device-block aligned in
/// practice). Read-only builds never require this; the RW server implements it over
/// `sys_io_submit` writes.
pub trait BlockWriter {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError>;
}

/// **A read-only mount** (administration Part C.3): reads pass through to the device, and every
/// write is refused with [`FsError::ReadOnly`] before it reaches it.
///
/// The server serves a read-only mount through this type, so read-only is not a check each
/// mutating operation has to remember. Any mutation, reached any way, fails at its first write
/// having changed nothing: a mutation only reads before it writes, and none of its writes
/// happen. The host tests hold every mutating operation to that against this same type.
pub struct ReadOnly<'a, R: BlockReader>(pub &'a R);

impl<R: BlockReader> BlockReader for ReadOnly<'_, R> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        self.0.read_at(offset, buf)
    }

    fn read_only(&self) -> bool {
        true
    }
}

impl<R: BlockReader> BlockWriter for ReadOnly<'_, R> {
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }
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
        fixture_blocks(block_size, 4096, 0, content)
    }

    /// [`fixture`], with the filesystem's size in blocks chosen by the caller.
    ///
    /// **For crossing a block group.** `mke2fs` puts `8 * block_size` blocks in a group, so
    /// the 4096-block default is a single group and no test built on it can reach a second
    /// one — which is how "creation is group 0 only" went unnoticed until a real disk was
    /// partitioned (Phase 5 Part H.2). At 1 KiB blocks a group is 8,192 blocks, so 24,576
    /// gives three, the last of them deliberately short: 24,575 addressable blocks do not
    /// divide by 8,192, and the bitmap's spare bits address nothing.
    ///
    /// `bytes_per_inode` is `mke2fs -i`, or `0` to let it choose. A large ratio gives *few*
    /// inodes per group, which is how a test can exhaust group 0's inodes by creating a
    /// couple of hundred files instead of eight thousand.
    pub(crate) fn fixture_blocks(
        block_size: u32,
        blocks: u32,
        bytes_per_inode: u32,
        content: &[u8],
    ) -> Vec<u8> {
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
        let mut cmd = Command::new("mke2fs");
        cmd.args(["-q", "-F", "-t", "ext4"])
            .args(["-O", "^has_journal,^64bit,^metadata_csum,^resize_inode"])
            .args(["-b", &block_size.to_string()]);
        if bytes_per_inode != 0 {
            cmd.args(["-i", &bytes_per_inode.to_string()]);
        }
        let status = cmd
            .arg("-d")
            .arg(&dir)
            .arg(&img)
            .arg(blocks.to_string())
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
    use crate::test_support::{ImageReader, RwImage, TEST_NOW, fixture, fixture_blocks};

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

    /// `(mtime, size)` of `name` inside the directory `dir`, via the stat-bearing walk.
    fn entry_stat(r: &RwImage, dir: &[u8], name: &[u8]) -> (i64, u64) {
        let dir_ino = ext4::resolve_dir(r, dir).unwrap();
        let mut found = None;
        let mut cursor = 0u64;
        loop {
            let next = ext4::read_dir_stat(r, dir_ino, cursor, |_ino, _ft, n, st| {
                if n == name {
                    found = Some((st.mtime, st.size));
                }
                true
            })
            .unwrap();
            if next == 0 {
                break;
            }
            cursor = next;
        }
        found.unwrap_or_else(|| panic!("no entry {:?} in {:?}", name, dir))
    }

    #[test]
    fn touch_file_moves_mtime_without_touching_content_and_stays_e2fsck_clean() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(1024, b"gen\n")));
        let ino = ext4::create_file(&rw, b"/system", b"edited", TEST_NOW).unwrap();
        ext4::grow_file(&rw, b"/system/edited", 1500, TEST_NOW).unwrap();
        assert_eq!(entry_stat(&rw, b"/system", b"edited"), (TEST_NOW, 1500));
        // The id the kernel will touch by is the one the block-file reply gave it.
        let mut runs = [BlockRun::default(); 8];
        assert_eq!(ext4::map_file(&rw, b"/system/edited", &mut runs).unwrap().ino, ino);

        // The Model A case: the kernel wrote the bytes itself and is telling us so. No
        // size change, no structural change — only the timestamp moves.
        const LATER: i64 = TEST_NOW + 100;
        ext4::touch_file(&rw, ino, LATER).unwrap();

        assert_eq!(
            entry_stat(&rw, b"/system", b"edited"),
            (LATER, 1500),
            "touch must move mtime and leave the size alone"
        );
        // **Only a live regular file is stamped** — the id comes off the wire. Not a
        // directory, not a number past the table, not `0`.
        let sys = ext4::resolve_dir(&rw, b"/system").unwrap();
        assert_eq!(ext4::touch_file(&rw, sys, LATER + 1), Err(FsError::NotFound));
        assert_eq!(ext4::touch_file(&rw, 0, LATER + 1), Err(FsError::NotFound));
        assert_eq!(ext4::touch_file(&rw, u32::MAX, LATER + 1), Err(FsError::NotFound));
        assert_e2fsck_clean(&rw.0.borrow(), "touch");

        // And not a file unlinked since — a touch can trail the unlink of what it names, and
        // stamping a freed inode would give e2fsck something to find.
        let orphan = ext4::unlink_at(&rw, sys, b"edited", LATER).unwrap().unwrap();
        ext4::release_inode(&rw, orphan, LATER).unwrap();
        assert_eq!(ext4::touch_file(&rw, ino, LATER + 2), Err(FsError::NotFound));
        assert_e2fsck_clean(&rw.0.into_inner(), "touch-after-unlink");
    }

    /// **A read-only mount refuses every mutation the server can reach, and writes nothing**
    /// (administration Part C.3). Each operation is run twice: through [`ReadOnly`], where it
    /// must fail with `ReadOnly`, and on a writable copy of the same image, where it must
    /// succeed — so read-only is the only thing that stopped it, not a missing name or a full
    /// directory. The image is byte-for-byte what it was afterwards.
    #[test]
    fn a_read_only_mount_refuses_every_mutation_and_writes_nothing() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"gen\n")));
        let sys = ext4::resolve_dir(&rw, b"/system").unwrap();
        let victim = ext4::create_file(&rw, b"/system", b"victim", TEST_NOW).unwrap();
        ext4::grow_file(&rw, b"/system/victim", 5000, TEST_NOW).unwrap();
        ext4::mkdir_at(&rw, sys, b"adir", TEST_NOW).unwrap();
        let orphan_ino = ext4::create_file(&rw, b"/system", b"orphan", TEST_NOW).unwrap();
        let orphan = ext4::unlink_at(&rw, sys, b"orphan", TEST_NOW).unwrap().unwrap();
        assert_eq!(orphan, orphan_ino);
        let before = rw.0.borrow().clone();

        const NOW: i64 = TEST_NOW + 50;
        type Op<'a> = &'a dyn Fn(&dyn Mutate) -> Result<(), FsError>;
        let ops: [(&str, Op); 13] = [
            ("mark_mounted", &|m| m.mark_mounted()),
            ("mark_clean", &|m| m.mark_clean()),
            ("touch_at", &|m| m.touch_at(sys, b"victim", NOW)),
            ("touch_file", &|m| m.touch_file(victim, NOW)),
            ("grow_file", &|m| m.grow_file(b"/system/victim", 9000, NOW)),
            ("truncate_file", &|m| m.truncate_file(b"/system/victim", 10, NOW)),
            ("create_file", &|m| m.create_file(b"/system", b"newfile", NOW)),
            ("mkdir_at", &|m| m.mkdir_at(sys, b"newdir", NOW)),
            ("unlink_at", &|m| m.unlink_at(sys, b"victim", NOW)),
            ("release_inode", &|m| m.release_inode(orphan, NOW)),
            ("rmdir_at", &|m| m.rmdir_at(sys, b"adir", NOW)),
            ("rename_path", &|m| m.rename_path(b"/system/victim", b"/system/moved", NOW)),
            ("rename_at", &|m| m.rename_at(sys, b"victim", b"renamed", NOW)),
        ];
        for (name, op) in ops {
            let ro = crate::ReadOnly(&rw);
            assert_eq!(op(&ro), Err(FsError::ReadOnly), "{name} through a read-only mount");
            let copy = RwImage(RefCell::new(before.clone()));
            assert_eq!(op(&copy), Ok(()), "{name} on a writable copy — else read-only is not what stopped it");
        }
        assert!(*rw.0.borrow() == before, "a read-only mount wrote nothing");
        // Reading through it still works.
        let ro = crate::ReadOnly(&rw);
        let mut out = [0u8; 4];
        assert_eq!(ext4::read_file_range(&ro, b"/system/current-generation", 0, 4, &mut out), Ok(4));
        assert!(ro.read_only() && !rw.read_only());
    }

    /// Every mutating operation, callable through one object-safe face so the test above can
    /// list them. Each discards what its operation returns on success.
    trait Mutate {
        fn mark_mounted(&self) -> Result<(), FsError>;
        fn mark_clean(&self) -> Result<(), FsError>;
        fn touch_at(&self, dir: u32, name: &[u8], now: i64) -> Result<(), FsError>;
        fn touch_file(&self, ino: u32, now: i64) -> Result<(), FsError>;
        fn grow_file(&self, path: &[u8], size: usize, now: i64) -> Result<(), FsError>;
        fn truncate_file(&self, path: &[u8], size: usize, now: i64) -> Result<(), FsError>;
        fn create_file(&self, parent: &[u8], name: &[u8], now: i64) -> Result<(), FsError>;
        fn mkdir_at(&self, dir: u32, name: &[u8], now: i64) -> Result<(), FsError>;
        fn unlink_at(&self, dir: u32, name: &[u8], now: i64) -> Result<(), FsError>;
        fn release_inode(&self, ino: u32, now: i64) -> Result<(), FsError>;
        fn rmdir_at(&self, dir: u32, name: &[u8], now: i64) -> Result<(), FsError>;
        fn rename_path(&self, from: &[u8], to: &[u8], now: i64) -> Result<(), FsError>;
        fn rename_at(&self, dir: u32, from: &[u8], to: &[u8], now: i64) -> Result<(), FsError>;
    }

    impl<T: BlockReader + BlockWriter> Mutate for T {
        fn mark_mounted(&self) -> Result<(), FsError> {
            ext4::mark_mounted(self)
        }
        fn mark_clean(&self) -> Result<(), FsError> {
            ext4::mark_clean(self)
        }
        fn touch_at(&self, dir: u32, name: &[u8], now: i64) -> Result<(), FsError> {
            ext4::touch_at(self, dir, name, now)
        }
        fn touch_file(&self, ino: u32, now: i64) -> Result<(), FsError> {
            ext4::touch_file(self, ino, now)
        }
        fn grow_file(&self, path: &[u8], size: usize, now: i64) -> Result<(), FsError> {
            ext4::grow_file(self, path, size, now).map(drop)
        }
        fn truncate_file(&self, path: &[u8], size: usize, now: i64) -> Result<(), FsError> {
            ext4::truncate_file(self, path, size, now).map(drop)
        }
        fn create_file(&self, parent: &[u8], name: &[u8], now: i64) -> Result<(), FsError> {
            ext4::create_file(self, parent, name, now).map(drop)
        }
        fn mkdir_at(&self, dir: u32, name: &[u8], now: i64) -> Result<(), FsError> {
            ext4::mkdir_at(self, dir, name, now)
        }
        fn unlink_at(&self, dir: u32, name: &[u8], now: i64) -> Result<(), FsError> {
            ext4::unlink_at(self, dir, name, now).map(drop)
        }
        fn release_inode(&self, ino: u32, now: i64) -> Result<(), FsError> {
            ext4::release_inode(self, ino, now)
        }
        fn rmdir_at(&self, dir: u32, name: &[u8], now: i64) -> Result<(), FsError> {
            ext4::rmdir_at(self, dir, name, now)
        }
        fn rename_path(&self, from: &[u8], to: &[u8], now: i64) -> Result<(), FsError> {
            ext4::rename_path(self, from, to, false, now).map(drop)
        }
        fn rename_at(&self, dir: u32, from: &[u8], to: &[u8], now: i64) -> Result<(), FsError> {
            ext4::rename_at(self, dir, from, to, now)
        }
    }

    /// **How the filesystem was left round-trips** through the writer, and preserves the rest of
    /// `s_state` — a filesystem with its error bit set keeps it across a mount and an unmount.
    #[test]
    fn the_state_round_trips_through_a_mount_and_an_unmount() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"gen\n")));
        assert_eq!(ext4::was_left_clean(&rw), Ok(true), "mke2fs leaves it clean");
        ext4::mark_mounted(&rw).unwrap();
        assert_eq!(ext4::was_left_clean(&rw), Ok(false));
        ext4::mark_clean(&rw).unwrap();
        assert_eq!(ext4::was_left_clean(&rw), Ok(true));
        assert_e2fsck_clean(&rw.0.borrow(), "state-round-trip");
        // The error bit (`EXT4_ERROR_FS`, 0x2) survives both, set by hand.
        rw.0.borrow_mut()[1024 + 58] |= 0x2;
        ext4::mark_mounted(&rw).unwrap();
        assert_eq!(rw.0.borrow()[1024 + 58], 0x2, "the clean bit cleared, the error bit kept");
        ext4::mark_clean(&rw).unwrap();
        assert_eq!(rw.0.borrow()[1024 + 58], 0x3);
    }

    /// **The state is read from bytes this writer never produced**: a superblock left mounted
    /// by someone else — `s_state` written by hand — reads as not clean, and one with only the
    /// clean bit reads as clean. A reader tested only on its own writer's output could share
    /// that writer's mistake.
    #[test]
    fn a_superblock_left_mounted_reads_as_not_clean() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"gen\n")));
        for (state, clean) in [(0x0000u16, false), (0x0002, false), (0x0004, false), (0x0001, true), (0x0005, true)] {
            rw.0.borrow_mut()[1024 + 58..1024 + 60].copy_from_slice(&state.to_le_bytes());
            assert_eq!(ext4::was_left_clean(&rw), Ok(clean), "s_state {state:#06x}");
        }
        rw.0.borrow_mut()[1024 + 56] = 0; // the magic: not ext4 at all
        assert!(ext4::was_left_clean(&rw).is_err());
    }

    /// **A touch follows the file, not its old name** — the reason it is by id. The kernel
    /// flushes a cached file long after the resolve that named it; by then a rename has moved
    /// it, and a new file has taken the name.
    #[test]
    fn a_touch_by_id_after_a_rename_stamps_the_renamed_file() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(1024, b"gen\n")));
        let ino = ext4::create_file(&rw, b"/system", b"draft", TEST_NOW).unwrap();
        ext4::rename_path(&rw, b"/system/draft", b"/system/final", false, TEST_NOW).unwrap();
        let other = ext4::create_file(&rw, b"/system", b"draft", TEST_NOW).unwrap();
        assert_ne!(other, ino, "the name now belongs to another file");

        const LATER: i64 = TEST_NOW + 100;
        ext4::touch_file(&rw, ino, LATER).unwrap();
        assert_eq!(entry_stat(&rw, b"/system", b"final").0, LATER, "the written file");
        assert_eq!(entry_stat(&rw, b"/system", b"draft").0, TEST_NOW, "not the name's new owner");
        assert_e2fsck_clean(&rw.0.into_inner(), "touch-after-rename");
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

        // With it, the destination becomes the source, and the replaced inode is handed back —
        // unfreed until the kernel has forgotten it — then freed by `release_inode`.
        let free_before = free_inodes(&rw);
        let mut runs = [crate::BlockRun::default(); 4];
        let victim = ext4::map_file(&rw, b"/system/victim", &mut runs).unwrap().ino;
        let replaced = ext4::rename_path(&rw, b"/system/src", b"/system/victim", true, TEST_NOW).unwrap();
        assert_eq!(replaced, Some(victim), "the replaced inode waits for its release");
        assert_eq!(ext4::stat_file(&rw, b"/system/victim"), Ok(1200));
        assert_eq!(ext4::stat_file(&rw, b"/system/src"), Err(FsError::NotFound));
        assert_eq!(free_inodes(&rw), free_before, "not freed before the release");
        ext4::release_inode(&rw, victim, TEST_NOW).unwrap();
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
    /// **A file bigger than block group 0**, which is the whole of Part H.2's first box.
    ///
    /// Before cross-group allocation this failed with `TooLarge` while `dumpe2fs` showed most
    /// of the filesystem free — and the number that matters is not this fixture's but the
    /// laptop's: a 931 GiB root confined to group 0 held about 112 MiB. **No existing test
    /// could have caught it**, because every fixture was 4,096 blocks and a group is 8,192, so
    /// the second group did not exist to fail to reach.
    ///
    /// The physical block numbers are the assertion. A size assertion passes with the
    /// allocator confined to group 0 — it would simply have failed earlier — and `e2fsck`
    /// alone would pass on a filesystem that allocated nothing.
    #[test]
    fn a_file_grows_past_block_group_0_and_stays_e2fsck_clean() {
        use std::cell::RefCell;
        // Three groups of 8,192 1 KiB blocks. `flex_bg` packs all three groups' metadata into
        // group 0, so group 0 has roughly 6,600 free data blocks and groups 1 and 2 are empty.
        let rw = RwImage(RefCell::new(fixture_blocks(1024, 24576, 0, b"x\n")));
        ext4::create_file(&rw, b"/", b"big", TEST_NOW).unwrap();
        let size = 9 * 1024 * 1024; // 9,216 blocks — past what group 0 can hold
        assert_eq!(ext4::grow_file(&rw, b"/big", size, TEST_NOW).unwrap(), size);
        assert_eq!(ext4::stat_file(&rw, b"/big").unwrap(), size);

        // Every block the file got, and the highest of them.
        let mut runs = [BlockRun::default(); 8];
        let n = ext4::map_range(&rw, b"/big", 0, 16384, &mut runs).unwrap();
        let highest = runs[..n]
            .iter()
            .filter(|r| r.device_lba != 0)
            .map(|r| r.device_lba + r.length as u64 - 1)
            .max()
            .expect("a grown file has blocks");
        assert!(
            highest > 8192,
            "every block landed inside group 0 (highest {highest}), so the file cannot be \
             {size} bytes — group 0 holds 8,192 blocks including all three groups' metadata"
        );
        // And nothing was placed past the end of the filesystem, which is the other half of
        // the last group's short tail.
        assert!(highest < 24576, "block {highest} is past the end of the device");
        assert_e2fsck_clean(&rw.0.into_inner(), "grow-cross-group");
    }

    /// **More files than one group has inodes**, the other half of the same box.
    ///
    /// `-i 65536` gives this fixture 128 inodes per group, so the 129th file is the first that
    /// cannot come from group 0. The assertion is that every create succeeds *and* that an
    /// inode number exceeds what a group holds — read out of the superblock rather than
    /// written down here, since `mke2fs` chooses it.
    #[test]
    fn more_files_than_group_0_holds_inodes_for_and_stays_e2fsck_clean() {
        use std::cell::RefCell;
        let img = fixture_blocks(1024, 24576, 65536, b"x\n");
        let per_group = rd_u32(&img[1024..2048], 40);
        assert!(per_group > 0 && per_group < 200, "expected a small inode ratio, got {per_group}");
        let rw = RwImage(RefCell::new(img));

        let want = per_group as usize + 40;
        let mut highest = 0u32;
        for i in 0..want {
            let name = std::format!("f{i:04}");
            let ino = ext4::create_file(&rw, b"/", name.as_bytes(), TEST_NOW)
                .unwrap_or_else(|e| panic!("create {name} (number {i}) failed: {e:?}"));
            highest = highest.max(ino);
        }
        assert!(
            highest > per_group,
            "{want} files all fit in the {per_group} inodes of group 0 (highest inode \
             {highest}), which cannot be true"
        );
        assert_e2fsck_clean(&rw.0.into_inner(), "inodes-cross-group");
    }

    /// **`e2fsck -fn` on a filesystem we laid out ourselves**, which is the whole point of
    /// `mkfs`: the oracle is somebody else's implementation of the format, and agreeing only
    /// with our own reader would prove nothing.
    fn format_image(blocks: u64, block_size: u32) -> (Vec<u8>, crate::mkfs::Geometry) {
        use std::cell::RefCell;
        let img = RwImage(RefCell::new(std::vec![0u8; (blocks * block_size as u64) as usize]));
        let geom = crate::mkfs::format(
            &img,
            &crate::mkfs::Params {
                blocks,
                block_size,
                bytes_per_inode: 16384,
                uuid: *b"nitrox-test-uuid",
                label: *b"nitrox-root\0\0\0\0\0",
                now: TEST_NOW,
            },
            &mut |_, _| {},
        )
        .unwrap();
        (img.0.into_inner(), geom)
    }

    #[test]
    fn a_filesystem_we_made_is_clean_and_empty_at_three_block_sizes() {
        for (blocks, bs) in [(24576u64, 1024u32), (16384, 2048), (16384, 4096)] {
            use std::cell::RefCell;
            let (img, geom) = format_image(blocks, bs);
            assert_e2fsck_clean(&img.clone(), &std::format!("mkfs-{bs}"));

            // And our own reader agrees it is an empty root directory. `e2fsck` accepting a
            // filesystem this parser cannot walk would be half an answer.
            let rw = RwImage(RefCell::new(img));
            assert_eq!(ext4::resolve_dir(&rw, b"/").unwrap(), 2, "the root is inode 2");
            assert_eq!(names_of(&rw, b"/"), [".", ".."], "a new root holds itself and nothing");
            assert!(geom.groups >= 1);

            // And it takes a file, which is the other half of "empty": the structures are
            // not merely self-consistent, they are the ones the writer allocates into.
            ext4::create_file(&rw, b"/", b"hello", TEST_NOW).unwrap();
            ext4::grow_file(&rw, b"/hello", 40, TEST_NOW).unwrap();
            assert_eq!(names_of(&rw, b"/"), [".", "..", "hello"]);
            assert_e2fsck_clean(&rw.0.into_inner(), &std::format!("mkfs-{bs}-used"));
        }
    }

    /// **What the module's doc says it declares, read back off the image by `dumpe2fs`.**
    ///
    /// The feature list is a promise about layout: `sparse_super` says where the backups are,
    /// `extent` says how an inode addresses blocks, and a bit set for something not done
    /// describes a filesystem that is not there. A comment claiming the set is an unrun
    /// claim; this runs it.
    #[test]
    fn the_feature_set_is_exactly_what_the_module_says_it_makes() {
        let (img, _) = format_image(16384, 4096);
        let dir = std::env::temp_dir()
            .join(std::format!("nitrox-mkfs-feat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("img.ext4");
        std::fs::write(&p, &img).unwrap();
        let out = std::process::Command::new("dumpe2fs")
            .args(["-h", p.to_str().unwrap()])
            .output()
            .unwrap();
        std::fs::remove_dir_all(&dir).ok();
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text
            .lines()
            .find(|l| l.starts_with("Filesystem features:"))
            .expect("dumpe2fs printed no feature line");
        let mut got: Vec<&str> = line["Filesystem features:".len()..].split_whitespace().collect();
        got.sort_unstable();
        assert_eq!(
            got,
            ["dir_nlink", "extent", "extra_isize", "filetype", "huge_file", "large_file",
             "sparse_super"],
            "the declared set drifted from the module doc: {line}"
        );
        // And the two that are deliberately absent, named so their absence is deliberate
        // rather than forgotten: packed metadata, and checksums the writer does not maintain.
        assert!(!line.contains("flex_bg"), "flex_bg is declared but not implemented: {line}");
        assert!(!line.contains("metadata_csum"), "checksums are not maintained: {line}");
        assert!(!line.contains("has_journal"), "there is no journal: {line}");
    }

    /// **More groups than one block of descriptors holds**, which is the case the installer
    /// died on and no test could reach.
    ///
    /// A group-descriptor table is 32 bytes per group: one 1 KiB block describes 32 groups,
    /// one 4 KiB block describes 128. The laptop's 931 GiB root has **7,452 groups — 59 blocks
    /// of table** — and the first version of `format` built the whole thing in a single
    /// one-block buffer, so it indexed past the end at group 128 and panicked. Every fixture
    /// here was four groups or fewer, 128 bytes of descriptors, so nothing failed until a real
    /// disk (2026-09-17). This is the same shape as the single-group fixture that hid
    /// group-0-only allocation, and the lesson is the same: **a fixture smaller than the
    /// structure it is testing proves the structure works at that size and nothing more.**
    ///
    /// **Written to a sparse file, not to memory.** 70 groups of 8 MiB is a 560 MiB filesystem;
    /// `mkfs` touches a few hundred blocks of it, and the holes read as zeros, so this costs
    /// about a megabyte of disk and no RAM at all. That is what makes testing a *large*
    /// filesystem affordable, and the absence of it is why this bug shipped.
    #[test]
    fn a_descriptor_table_spanning_several_blocks_is_written_whole() {
        use std::io::{Read, Seek, SeekFrom, Write};

        /// A [`BlockWriter`] straight onto a file, so unwritten regions stay holes.
        struct SparseFile(std::cell::RefCell<std::fs::File>);
        impl BlockReader for SparseFile {
            fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
                let mut f = self.0.borrow_mut();
                f.seek(SeekFrom::Start(offset)).map_err(|_| FsError::Io)?;
                f.read_exact(buf).map_err(|_| FsError::Io)
            }
        }
        impl BlockWriter for SparseFile {
            fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
                let mut f = self.0.borrow_mut();
                f.seek(SeekFrom::Start(offset)).map_err(|_| FsError::Io)?;
                f.write_all(buf).map_err(|_| FsError::Io)
            }
        }

        let dir = std::env::temp_dir()
            .join(std::format!("nitrox-mkfs-wide-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wide.ext4");
        // 70 groups at 1 KiB blocks: 3 blocks of descriptors, and backups in 0, 1, 3, 5, 7,
        // 9, 25, 27, 49 — nine copies of a table that is not one block.
        let blocks = 70u64 * 8192;
        let f = std::fs::File::options()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        f.set_len(blocks * 1024).unwrap();
        let img = SparseFile(std::cell::RefCell::new(f));
        let geom = crate::mkfs::format(
            &img,
            &crate::mkfs::Params {
                blocks,
                block_size: 1024,
                bytes_per_inode: 16384,
                uuid: *b"nitrox-wide-uuid",
                label: *b"nitrox-root\0\0\0\0\0",
                now: TEST_NOW,
            },
            &mut |_, _| {},
        )
        .unwrap();
        assert_eq!(geom.groups, 70);
        assert!(
            geom.gdt_blocks > 1,
            "this test means nothing with a one-block table ({} blocks)",
            geom.gdt_blocks
        );

        // **The last group's descriptor, read back, before the oracle is asked.** A table
        // truncated at its first block leaves this one zeroed — and `e2fsck` on a filesystem
        // that broken does not report it, it *spins*: measured at ten minutes of CPU before
        // being killed. A test whose failure mode is a hang is not a test, so the cheap,
        // deterministic claim goes first and the oracle confirms the rest.
        //
        // The expected value is re-derived from the documented layout rather than read from
        // the private helper that wrote it: a group's metadata starts after its superblock
        // copy and descriptor table, if it has one.
        let last = geom.groups - 1;
        let expect = geom.group_start(last)
            + if geom.has_super(last) { 1 + geom.gdt_blocks as u64 } else { 0 };
        let mut d = [0u8; 32];
        img.read_at(
            (geom.first_data_block as u64 + 1) * 1024 + last as u64 * 32,
            &mut d,
        )
        .unwrap();
        let bitmap = u32::from_le_bytes([d[0], d[1], d[2], d[3]]) as u64;
        assert_eq!(
            bitmap, expect,
            "group {last}'s descriptor is wrong ({bitmap}, expected {expect}) — the table is \
             {} blocks and only the first was written",
            geom.gdt_blocks
        );
        drop(img);

        // The oracle reads every copy of the table it can find, so a truncated or misplaced
        // one is its business, not ours to re-derive.
        assert_e2fsck_clean_path(&path, "mkfs-wide");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A filesystem large enough to need **backup superblocks past group 1**, which is where
    /// `sparse_super` stops being a rule about small numbers. `e2fsck -b` reads one of the
    /// backups rather than the primary, so it fails if the copy is wrong or in the wrong place
    /// — which a check of the primary alone would never notice.
    #[test]
    fn the_backup_superblocks_are_where_sparse_super_says_and_are_usable() {
        // Nine groups at 1 KiB blocks, so backups land in 0, 1, 3, 5 and 7.
        let blocks = 9 * 8192;
        let (img, geom) = format_image(blocks, 1024);
        assert_eq!(geom.groups, 9);
        assert!(geom.has_super(3) && geom.has_super(5) && geom.has_super(7));
        assert!(!geom.has_super(2) && !geom.has_super(4) && !geom.has_super(6));
        assert_e2fsck_clean(&img, "mkfs-backups");

        let dir = std::env::temp_dir()
            .join(std::format!("nitrox-mkfs-backup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("img.ext4");
        std::fs::write(&p, &img).unwrap();
        // Group 3's copy starts at its first block: 1 + 3 * 8192.
        let out = std::process::Command::new("e2fsck")
            .args(["-fn", "-b", "24577", "-B", "1024", p.to_str().unwrap()])
            .output()
            .unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            out.status.success(),
            "e2fsck could not use the backup in group 3:\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    fn assert_e2fsck_clean(img: &[u8], tag: &str) {
        let dir = std::env::temp_dir()
            .join(std::format!("nitrox-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("img.ext4");
        std::fs::write(&p, img).unwrap();
        assert_e2fsck_clean_path(&p, tag);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// [`assert_e2fsck_clean`] for a filesystem already on disk — a sparse file too large to
    /// want in memory, or one a gate carved out of an image.
    ///
    /// **Split rather than copied.** Two tests had their own hand-rolled invocation, which is
    /// how the status-only check below survived being noticed; a third copy would be the same
    /// mistake with the ink still wet (PR #310 review).
    fn assert_e2fsck_clean_path(p: &std::path::Path, tag: &str) {
        let out = std::process::Command::new("e2fsck")
            .args(["-fn", p.to_str().unwrap()])
            .output()
            .unwrap();
        let text = std::format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        // **The exit status is not the oracle; the output is.** Measured against e2fsck
        // 1.47.0: a filesystem whose superblock free-block count is wrong by 7 prints
        // "Free blocks count wrong (7669, counted=7662). Fix? no" and **exits 0**. This
        // helper trusted the status from the day it was written (2026-07-24), so every test
        // using it was blind to the whole class of summary-information corruption — which is
        // exactly what a filesystem writer gets wrong (Phase 5 Part H.2).
        //
        // Every problem e2fsck finds prints a prompt, and `-n` answers each one "no", so the
        // prompt is what to look for. The summary line is the other half: without it e2fsck
        // stopped early and found nothing because it checked nothing.
        assert!(out.status.success(), "e2fsck exited {:?}:\n{text}", out.status.code());
        assert!(!text.contains("? no"), "e2fsck found a problem in `{tag}`:\n{text}");
        assert!(text.contains(" files ("), "e2fsck never reached its summary:\n{text}");
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

        // **Two halves** (administration Part C.1b): the name goes at once, but the inode and
        // its block stay allocated until `release_inode` — which a server calls only once the
        // kernel has answered `File::Forget`, since it may still be writing that block.
        let (blocks, inodes) = (free_blocks(&rw), free_inodes(&rw));
        assert_eq!(ext4::unlink_at(&rw, sys, b"scratch", TEST_NOW), Ok(Some(ino)));
        assert!(!names_of(&rw, b"/system").iter().any(|n| n == "scratch"));
        assert_eq!(ext4::stat_file(&rw, b"/system/scratch"), Err(FsError::NotFound));
        assert_eq!((free_blocks(&rw), free_inodes(&rw)), (blocks, inodes), "nothing freed yet");
        ext4::release_inode(&rw, ino, TEST_NOW).unwrap();
        assert_eq!((free_blocks(&rw), free_inodes(&rw)), (blocks + 1, inodes + 1), "freed on release");
        // A second release, or one of a live file, is refused rather than freeing twice.
        assert_eq!(ext4::release_inode(&rw, ino, TEST_NOW), Err(FsError::NotFound));
        let sys_ino = ext4::resolve_dir(&rw, b"/system").unwrap();
        assert_eq!(ext4::release_inode(&rw, sys_ino, TEST_NOW), Err(FsError::NotFound));

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
        let ext4::MappedFile { size, runs: n, .. } = ext4::map_file(&rw, path, &mut runs).unwrap();
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
        let ext4::MappedFile { size, runs: n, .. } = ext4::map_file(&rw, path, &mut runs).unwrap();
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

    /// **A truncate and then a grow read zero over the regrown range, on the device** — a whole
    /// block and a partial tail. The kernel fills a page it does not hold from the blocks the
    /// map names, so what the device holds there is what a reader sees. Both halves reuse what
    /// the truncate left: the tail stays in the kept block, and the freed block is where the
    /// allocator's goal points — asserted, since a test that grew into a fresh block would pass
    /// with no zeroing at all.
    #[test]
    fn a_truncate_then_a_grow_reads_zero_over_the_regrown_range() {
        use crate::BlockRun;
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n")));
        let path = b"/system/current-generation";
        // Two blocks of a pattern, written to the device as the kernel's write-back would.
        ext4::grow_file(&rw, path, 8192, TEST_NOW).unwrap();
        let mut runs = [BlockRun::default(); 8];
        let m = ext4::map_file(&rw, path, &mut runs).unwrap();
        let lbas: std::vec::Vec<u64> =
            runs[..m.runs].iter().flat_map(|r| (0..r.length as u64).map(move |i| r.device_lba + i)).collect();
        assert_eq!(lbas.len(), 2);
        for &lba in &lbas {
            BlockWriter::write_at(&rw, lba * 4096, &[0xAB; 4096]).unwrap();
        }

        ext4::truncate_file(&rw, path, 10, TEST_NOW).unwrap();
        ext4::grow_file(&rw, path, 8192, TEST_NOW).unwrap();
        let m = ext4::map_file(&rw, path, &mut runs).unwrap();
        let regrown: std::vec::Vec<u64> =
            runs[..m.runs].iter().flat_map(|r| (0..r.length as u64).map(move |i| r.device_lba + i)).collect();
        assert_eq!(regrown, lbas, "the grow took back the block the truncate freed");

        let mut out = [0x55u8; 8192];
        assert_eq!(ext4::read_file_range(&rw, path, 0, 8192, &mut out), Ok(8192));
        assert_eq!(&out[..10], &[0xAB; 10], "the kept bytes");
        assert!(out[10..4096].iter().all(|&b| b == 0), "the partial tail reads zero");
        assert!(out[4096..].iter().all(|&b| b == 0), "the whole regrown block reads zero");
        assert_e2fsck_clean(&rw.0.into_inner(), "truncate-grow");
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
        let ext4::MappedFile { size, runs: n, .. } = ext4::map_file(&rw, path, &mut runs).unwrap();
        assert_eq!(size, 5000);
        let covered: u64 = runs[..n].iter().map(|r| r.length as u64).sum();
        assert_eq!(covered, 2);
        for r in &runs[..n] {
            assert_ne!(r.device_lba, 0);
        }

        // e2fsck the mutated image: the metadata (extent tree, bitmap, free counts, inode)
        // must be fully consistent. **Through the shared helper** — this test and the one
        // below each had their own copy of the invocation, which is how they went on
        // trusting an exit status the helper had to stop trusting (Phase 5 Part H.2).
        assert_e2fsck_clean(&rw.0.into_inner(), "grow");
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
        assert_e2fsck_clean(&rw.0.into_inner(), "create");
    }
}
