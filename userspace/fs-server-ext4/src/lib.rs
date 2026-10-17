//! `fs-server-ext4` — an ext4 reader and writer behind the [`BlockReader`] and [`BlockWriter`]
//! traits, so it is host-testable against a fixture image built by `mke2fs`.
//!
//! The superblock, block-group descriptors, inodes, the **extent tree**, and directories as a
//! linear walk (which works on htree directories too). It skips the journal, bigalloc,
//! inline-data, 64-bit block numbers, xattrs, symlinks, and checksums. No `alloc`: [`read_file`]
//! reads into a caller buffer, and parsing uses one block of stack scratch per tree level.

#![cfg_attr(not(test), no_std)]

pub mod ext4;
mod extent;
pub mod mkfs;
pub mod volume;

pub use ext4::read_file;
pub use volume::Ext4;

/// **The block-device traits and their types live in `libfsserver`**, which serves ext4 and FAT
/// alike; re-exported so this library's users (the installer, the storage service, `boot-probe`)
/// name them as they always have.
pub use libfsserver::{BlockReader, BlockRun, BlockWriter, FsError, ReadOnly};

pub(crate) fn rd_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
pub(crate) fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Host-test fixtures shared by the parser and server-loop tests: an in-memory [`BlockReader`]
/// over an `mke2fs`-built image.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{BlockReader, BlockWriter, FsError};
    use std::cell::RefCell;
    use std::io::Write;
    use std::process::Command;

    /// A read-write in-memory image for the write-path tests.
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

    /// **Commands as `libfsserver::disk::Disk` sends them**, over an [`RwImage`]: a command a 4 KiB
    /// block touched, and a read first for each block written only in part. A count of commands,
    /// never a time, holds the run-at-a-time fixes.
    pub(crate) struct Counting {
        pub image: RwImage,
        pub reads: std::cell::Cell<u64>,
        pub writes: std::cell::Cell<u64>,
    }
    impl Counting {
        const IO_BLOCK: u64 = 4096;
        pub(crate) fn new(image: Vec<u8>) -> Counting {
            Counting { image: RwImage(RefCell::new(image)), reads: 0.into(), writes: 0.into() }
        }
        pub(crate) fn take(&self) -> (u64, u64) {
            (self.reads.take(), self.writes.take())
        }
        /// The 4 KiB blocks `[offset, offset + len)` touches, each with whether it is touched
        /// whole.
        fn blocks(offset: u64, len: usize) -> impl Iterator<Item = bool> {
            let end = offset + len as u64;
            let first = offset / Self::IO_BLOCK;
            let last = if len == 0 { first } else { (end - 1) / Self::IO_BLOCK + 1 };
            (first..last).map(move |b| offset <= b * Self::IO_BLOCK && (b + 1) * Self::IO_BLOCK <= end)
        }
    }
    impl BlockReader for Counting {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
            self.reads.set(self.reads.get() + Self::blocks(offset, buf.len()).count() as u64);
            self.image.read_at(offset, buf)
        }
    }
    impl BlockWriter for Counting {
        fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
            for whole in Self::blocks(offset, buf.len()) {
                self.reads.set(self.reads.get() + !whole as u64);
                self.writes.set(self.writes.get() + 1);
            }
            self.image.write_at(offset, buf)
        }
        /// As `Disk` writes them: a block's part at each end by `write_at`, whole blocks a
        /// [`libfsserver::disk::SPAN`] a command.
        /// [`libfsserver::disk::SPAN`] a command.
        fn write_zeroes(&self, offset: u64, len: u64) -> Result<(), FsError> {
            let (head, whole, tail) = libfsserver::disk::split_zeroes(offset, len);
            self.write_at(head.start, &vec![0; (head.end - head.start) as usize])?;
            let n = whole.end - whole.start;
            self.writes.set(self.writes.get() + n.div_ceil(libfsserver::disk::SPAN as u64));
            self.image.write_at(whole.start, &vec![0; n as usize])?;
            self.write_at(tail.start, &vec![0; (tail.end - tail.start) as usize])
        }
    }

    /// **An [`RwImage`] whose writes fail**: from the `n`th on, or the `n`th alone. A refused write
    /// is `Io` and changes nothing.
    pub(crate) struct Failing {
        pub image: RwImage,
        pub writes: std::cell::Cell<u64>,
        fail_at: u64,
        once: bool,
    }
    impl Failing {
        pub(crate) fn after(image: Vec<u8>, n: u64) -> Failing {
            Failing { image: RwImage(RefCell::new(image)), writes: 0.into(), fail_at: n, once: false }
        }
        pub(crate) fn once(image: Vec<u8>, n: u64) -> Failing {
            Failing { once: true, ..Failing::after(image, n) }
        }
    }
    impl BlockReader for Failing {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
            self.image.read_at(offset, buf)
        }
    }
    impl BlockWriter for Failing {
        fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
            let i = self.writes.get();
            self.writes.set(i + 1);
            if i == self.fail_at || (!self.once && i > self.fail_at) {
                return Err(FsError::Io);
            }
            self.image.write_at(offset, buf)
        }
    }

    /// A `BlockReader` over an in-memory image whose writes are refused, so a test reading through
    /// a server's [`crate::Ext4`] volume, which takes a writer, cannot change it.
    pub(crate) struct ImageReader(pub Vec<u8>);
    impl BlockWriter for ImageReader {
        fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<(), FsError> {
            Err(FsError::Io)
        }
    }
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

    /// A fixed wall-clock instant the mutation tests stamp with, so a test can assert the exact
    /// value that reached the inode.
    pub(crate) const TEST_NOW: i64 = 1_784_900_730;

    /// A minimal ext4 image from `mke2fs -d` holding `/system/current-generation`.
    pub(crate) fn fixture(block_size: u32, content: &[u8]) -> Vec<u8> {
        fixture_blocks(block_size, 4096, 0, content)
    }

    /// [`fixture`], with the filesystem's size in blocks chosen by the caller.
    ///
    /// **For crossing a block group.** `mke2fs` puts `8 * block_size` blocks in a group, so the
    /// 4096-block default is one group. At 1 KiB blocks 24,576 gives three, the last deliberately
    /// short: its bitmap's spare bits address nothing.
    ///
    /// `bytes_per_inode` is `mke2fs -i`, or `0` to let it choose; a large ratio gives few inodes
    /// per group, so a test can exhaust group 0's inodes cheaply.
    pub(crate) fn fixture_blocks(block_size: u32, blocks: u32, bytes_per_inode: u32, content: &[u8]) -> Vec<u8> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nitrox-ext4-fix-{}-{}", std::process::id(), id));
        let sysdir = dir.join("system");
        std::fs::create_dir_all(&sysdir).unwrap();
        std::fs::File::create(sysdir.join("current-generation")).unwrap().write_all(content).unwrap();
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

    /// **`e2fsck -fn` finds nothing** in the image `img`.
    pub(crate) fn assert_e2fsck_clean(img: &[u8], tag: &str) {
        let dir = std::env::temp_dir().join(format!("nitrox-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("img.ext4");
        std::fs::write(&p, img).unwrap();
        assert_e2fsck_clean_path(&p, tag);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// [`assert_e2fsck_clean`] for a filesystem already on disk (a sparse file too large for
    /// memory).
    pub(crate) fn assert_e2fsck_clean_path(p: &std::path::Path, tag: &str) {
        let out = std::process::Command::new("e2fsck").args(["-fn", p.to_str().unwrap()]).output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        // **The exit status is not the oracle; the output is.** A wrong superblock free-block count
        // prints "Free blocks count wrong (7669, counted=7662). Fix? no" and **exits 0** (e2fsck
        // 1.47.0). Every problem prints a prompt, which `-n` answers "no"; the summary line proves
        // it checked anything.
        assert!(out.status.success(), "e2fsck exited {:?}:\n{text}", out.status.code());
        assert!(!text.contains("? no"), "e2fsck found a problem in `{tag}`:\n{text}");
        assert!(text.contains(" files ("), "e2fsck never reached its summary:\n{text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        Counting, ImageReader, RwImage, TEST_NOW, assert_e2fsck_clean, assert_e2fsck_clean_path, fixture,
        fixture_blocks,
    };

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

    /// Collect every entry name of a directory, draining the cursor across calls.
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
        assert!(
            names.iter().any(|(n, ft)| n == "current-generation" && *ft == 1),
            "expected current-generation as a regular file, got {names:?}"
        );
        assert!(names.iter().any(|(n, _)| n == "."), "must include .");
        assert!(names.iter().any(|(n, _)| n == ".."), "must include ..");
    }

    #[test]
    fn read_dir_lists_root_directory() {
        let r = ImageReader(fixture(4096, b"gen\n"));
        let names = list_dir(&r, b"/");
        assert!(
            names.iter().any(|(n, ft)| n == "system" && *ft == ext4::EXT4_FT_DIR),
            "root must contain the `system` subdirectory, got {names:?}"
        );
    }

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
        let mut runs = [BlockRun::default(); 8];
        assert_eq!(ext4::map_file(&rw, b"/system/edited", &mut runs).unwrap().ino, ino);

        const LATER: i64 = TEST_NOW + 100;
        ext4::touch_file(&rw, ino, LATER).unwrap();

        assert_eq!(
            entry_stat(&rw, b"/system", b"edited"),
            (LATER, 1500),
            "touch must move mtime and leave the size alone"
        );
        // **Only a live regular file is stamped**: not a directory, a number past the table, or
        // `0`.
        let sys = ext4::resolve_dir(&rw, b"/system").unwrap();
        assert_eq!(ext4::touch_file(&rw, sys, LATER + 1), Err(FsError::NotFound));
        assert_eq!(ext4::touch_file(&rw, 0, LATER + 1), Err(FsError::NotFound));
        assert_eq!(ext4::touch_file(&rw, u32::MAX, LATER + 1), Err(FsError::NotFound));
        assert_e2fsck_clean(&rw.0.borrow(), "touch");

        // Nor a file unlinked since: stamping a freed inode would give e2fsck something to find.
        let orphan = ext4::unlink_at(&rw, sys, b"edited", LATER).unwrap().unwrap();
        ext4::release_inode(&rw, orphan, LATER).unwrap();
        assert_eq!(ext4::touch_file(&rw, ino, LATER + 2), Err(FsError::NotFound));
        assert_e2fsck_clean(&rw.0.into_inner(), "touch-after-unlink");
    }

    /// **A read-only mount refuses every mutation the server can reach, and writes nothing.** Each
    /// operation also succeeds on a writable copy, so read-only is the only thing that stopped it.
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
        let ro = crate::ReadOnly(&rw);
        let mut out = [0u8; 4];
        assert_eq!(ext4::read_file_range(&ro, b"/system/current-generation", 0, 4, &mut out), Ok(4));
        assert!(ro.read_only() && !rw.read_only());
    }

    /// Every mutating operation behind one object-safe face, so the test above can list them.
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

    /// **How the filesystem was left round-trips** through the writer, preserving the rest of
    /// `s_state`.
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
        rw.0.borrow_mut()[1024 + 58] |= 0x2;
        ext4::mark_mounted(&rw).unwrap();
        assert_eq!(rw.0.borrow()[1024 + 58], 0x2, "the clean bit cleared, the error bit kept");
        ext4::mark_clean(&rw).unwrap();
        assert_eq!(rw.0.borrow()[1024 + 58], 0x3);
    }

    /// **The state is read from bytes this writer never produced**, which a reader tested only on
    /// its own writer's output could misread alike.
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

    /// **The label is `s_volume_name` up to its first NUL**: nothing for an image made without
    /// `-L`, all sixteen bytes when there is no terminator, an error if not ext4.
    #[test]
    fn the_volume_label_reads_up_to_its_first_nul() {
        use std::cell::RefCell;
        let mut out = [0u8; 16];
        let made = RwImage(RefCell::new(format_image(16384, 4096).0));
        let n = ext4::volume_label(&made, &mut out).unwrap();
        assert_eq!(&out[..n], b"nitrox-root");

        let bare = RwImage(RefCell::new(fixture(4096, b"gen\n")));
        assert_eq!(ext4::volume_label(&bare, &mut out), Ok(0), "mke2fs without -L gives no label");

        bare.0.borrow_mut()[1024 + 120..1024 + 136].copy_from_slice(b"sixteen-bytes-ok");
        let n = ext4::volume_label(&bare, &mut out).unwrap();
        assert_eq!(&out[..n], b"sixteen-bytes-ok", "a full label has no terminator");

        bare.0.borrow_mut()[1024 + 56] = 0; // the magic: not ext4 at all
        assert!(ext4::volume_label(&bare, &mut out).is_err());
    }

    /// **A touch follows the file, not its old name**: by the flush a rename may have moved the
    /// file and a new file taken the name.
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

        assert_eq!(ext4::rename_path(&rw, b"/system/src", b"/system/victim", false, TEST_NOW), Err(FsError::Exists));
        assert_eq!(ext4::stat_file(&rw, b"/system/victim"), Ok(3000));
        assert_eq!(ext4::stat_file(&rw, b"/system/src"), Ok(1200));

        let free_before = free_inodes(&rw);
        let mut runs = [crate::BlockRun::default(); 4];
        let victim = ext4::map_file(&rw, b"/system/victim", &mut runs).unwrap().ino;
        let replaced = ext4::rename_path(&rw, b"/system/src", b"/system/victim", true, TEST_NOW).unwrap();
        assert_eq!(replaced, Some(victim), "the replaced inode waits for its release");
        assert_eq!(ext4::stat_file(&rw, b"/system/victim"), Ok(1200));
        assert_eq!(ext4::stat_file(&rw, b"/system/src"), Err(FsError::NotFound));
        assert_eq!(free_inodes(&rw), free_before, "not freed before the release");
        ext4::release_inode(&rw, victim, TEST_NOW).unwrap();
        assert_eq!(free_inodes(&rw), free_before + 1, "the replaced inode must be freed, not orphaned");
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

        ext4::rename_path(&rw, b"/system/outer", b"/system/target/inner", false, TEST_NOW).unwrap();

        assert!(ext4::resolve_dir(&rw, b"/system/target/inner").is_ok());
        assert_eq!(ext4::stat_file(&rw, b"/system/target/inner/payload"), Ok(0));
        assert!(ext4::resolve_dir(&rw, b"/system/outer").is_err());
        // `..` now names the new parent: a stale one is a link-count mismatch on both.
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

        assert_eq!(ext4::rename_path(&rw, b"/system/d", b"/system/d/self", false, TEST_NOW), Err(FsError::Unsupported));
        assert_eq!(ext4::rename_path(&rw, b"/system/f", b"/system/e", true, TEST_NOW), Err(FsError::Unsupported));
        assert_eq!(ext4::rename_path(&rw, b"/system/ghost", b"/system/g", false, TEST_NOW), Err(FsError::NotFound));
        ext4::rename_path(&rw, b"/system/f", b"/system/f", false, TEST_NOW).unwrap();
        assert_eq!(ext4::stat_file(&rw, b"/system/f"), Ok(0));
        assert_e2fsck_clean(&rw.0.into_inner(), "rename-refuse");
    }

    /// The superblock's free-inode count: proves a replaced inode was freed, not merely unlinked.
    fn free_inodes(rw: &RwImage) -> u32 {
        let img = rw.0.borrow();
        u32::from_le_bytes(img[1024 + 16..1024 + 20].try_into().unwrap())
    }

    #[test]
    fn a_directory_grows_past_its_first_block_and_stays_e2fsck_clean() {
        use std::cell::RefCell;
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

        let listed = list_dir(&ImageReader(rw.0.borrow().clone()), b"/system");
        for i in 0..40 {
            let want = name_of(i);
            let seen = listed.iter().filter(|(n, _)| *n == want).count();
            assert_eq!(seen, 1, "entry {i} appears {seen} times, want 1");
        }
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
        let rw = RwImage(RefCell::new(fixture(1024, b"gen\n")));
        let sys = ext4::resolve_dir(&rw, b"/system").unwrap();
        let name_of = |i: usize| {
            let mut n = std::format!("g{i:03}");
            while n.len() < 200 {
                n.push('y');
            }
            n
        };
        for i in 0..12 {
            ext4::mkdir_at(&rw, sys, name_of(i).as_bytes(), TEST_NOW)
                .unwrap_or_else(|e| panic!("mkdir {i} failed: {e:?}"));
        }
        let outer = ext4::resolve_dir(&rw, name_path(&name_of(11)).as_bytes()).unwrap();
        ext4::mkdir_at(&rw, outer, b"inner", TEST_NOW).unwrap();
        assert_eq!(ext4::rmdir_at(&rw, sys, name_of(11).as_bytes(), TEST_NOW), Err(FsError::NotEmpty));
        ext4::rmdir_at(&rw, outer, b"inner", TEST_NOW).unwrap();

        for i in 0..12 {
            ext4::rmdir_at(&rw, sys, name_of(i).as_bytes(), TEST_NOW)
                .unwrap_or_else(|e| panic!("rmdir {i} failed: {e:?}"));
        }
        let listed = list_dir(&ImageReader(rw.0.borrow().clone()), b"/system");
        assert!(!listed.iter().any(|(n, _)| n.starts_with('g')), "entries survived removal: {listed:?}");
        assert_e2fsck_clean(&rw.0.into_inner(), "dirgrow-rm");
    }

    fn name_path(name: &str) -> String {
        std::format!("/system/{name}")
    }

    #[test]
    fn creating_a_file_stamps_it_and_its_parent_directory() {
        use std::cell::RefCell;
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

        let later = TEST_NOW + 600;
        ext4::rmdir_at(&r, sys, b"kid", later).unwrap();
        assert_eq!(dir_entry_mtime(&r, sys, b"."), Some(later));
    }

    #[test]
    fn a_rename_stamps_the_directory_but_not_the_file() {
        use std::cell::RefCell;
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
        let content = b"generation-42\n";
        let r = ImageReader(fixture(1024, content));
        let dir_ino = ext4::resolve_dir(&r, b"/system").unwrap();
        let mut entries = Vec::new();
        ext4::read_dir_stat(&r, dir_ino, 0, |_i, _ft, name, st| {
            entries.push((String::from_utf8_lossy(name).into_owned(), *st));
            true
        })
        .unwrap();

        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;

        let (_, file) = entries.iter().find(|(n, _)| n == "current-generation").unwrap();
        assert_eq!(file.size, content.len() as u64, "exact file size, not a block count");
        assert_eq!(file.mode & 0xF000, 0x8000, "S_IFREG");
        assert_ne!(file.mode & 0o777, 0, "permission bits must survive");
        assert!(file.mtime > now - 3600 && file.mtime <= now + 60, "mtime {} is not near now ({now})", file.mtime);

        let (_, dot) = entries.iter().find(|(n, _)| n == ".").unwrap();
        assert_eq!(dot.mode & 0xF000, 0x4000, "S_IFDIR for the directory itself");
        assert_eq!(dot.ino, dir_ino, "`.` is the directory's own inode");
    }

    #[test]
    fn read_dir_cursor_resumes_when_emit_stops_early() {
        // Stop after the first entry, then resume from the returned cursor: no drop, no dup.
        let r = ImageReader(fixture(1024, b"gen\n"));
        let dir_ino = ext4::resolve_dir(&r, b"/system").unwrap();

        // `false` means the entry was NOT accepted (buffer full), as in `DirReplyWriter::push`.
        let mut first = Vec::new();
        let cursor = ext4::read_dir(&r, dir_ino, 0, |_i, _ft, name| {
            if !first.is_empty() {
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
        let n = ext4::read_file_range(&r, b"/system/current-generation", 4, 6, &mut out).unwrap();
        assert_eq!(&out[..n], b"456789");
        let n = ext4::read_file_range(&r, b"/system/current-generation", 10, 100, &mut out).unwrap();
        assert_eq!(&out[..n], b"ABCDEF\n");
        let n = ext4::read_file_range(&r, b"/system/current-generation", 0, 17, &mut out).unwrap();
        assert_eq!(&out[..n], content);
        assert_eq!(ext4::read_file_range(&r, b"/system/current-generation", 17, 8, &mut out), Ok(0));
    }

    #[test]
    fn read_range_spans_block_boundaries() {
        let mut content = std::vec::Vec::new();
        for i in 0..5000u32 {
            content.push((i & 0xFF) as u8);
        }
        let r = ImageReader(fixture(1024, &content));
        let mut out = [0u8; 2048];
        let n = ext4::read_file_range(&r, b"/system/current-generation", 1500, 2000, &mut out).unwrap();
        assert_eq!(n, 2000);
        assert_eq!(&out[..n], &content[1500..3500]);
    }

    #[test]
    fn map_range_maps_blocks_to_correct_device_data() {
        use crate::BlockRun;
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

        let mut next_fb = 0u64;
        for run in &runs[..n] {
            assert_eq!(run.file_block, next_fb);
            assert_ne!(run.device_lba, 0, "content is not sparse");
            next_fb += run.length as u64;
        }
        assert_eq!(next_fb, file_blocks);

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

    /// **A file bigger than block group 0.** The physical block numbers are the assertion: a size
    /// assertion passes with the allocator confined to group 0 (it would have failed earlier), and
    /// `e2fsck` alone would pass on a filesystem that allocated nothing.
    #[test]
    fn a_file_grows_past_block_group_0_and_stays_e2fsck_clean() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture_blocks(1024, 24576, 0, b"x\n")));
        ext4::create_file(&rw, b"/", b"big", TEST_NOW).unwrap();
        let size = 9 * 1024 * 1024; // 9,216 blocks — past what group 0 can hold
        assert_eq!(ext4::grow_file(&rw, b"/big", size, TEST_NOW).unwrap(), size);
        assert_eq!(ext4::stat_file(&rw, b"/big").unwrap(), size);

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
        assert!(highest < 24576, "block {highest} is past the end of the device");
        assert_e2fsck_clean(&rw.0.into_inner(), "grow-cross-group");
    }

    /// **More files than one group has inodes.** `-i 65536` gives 128 inodes per group, so the
    /// 129th file cannot come from group 0.
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

    /// **A directory made past group 0's inodes is counted in its own group**, not group 0's.
    #[test]
    fn a_directory_made_past_group_0s_inodes_is_counted_in_its_own_group() {
        use std::cell::RefCell;
        let img = fixture_blocks(1024, 24576, 65536, b"x\n");
        let per_group = rd_u32(&img[1024..2048], 40);
        let rw = RwImage(RefCell::new(img));
        let root = ext4::resolve_dir(&rw, b"/").unwrap();
        for i in 0..per_group {
            ext4::create_file(&rw, b"/", std::format!("f{i:04}").as_bytes(), TEST_NOW).unwrap();
        }
        ext4::mkdir_at(&rw, root, b"late", TEST_NOW).unwrap();
        let late = ext4::resolve_dir(&rw, b"/late").unwrap();
        assert!(late > per_group, "the directory's inode {late} is past group 0's {per_group}");
        assert_e2fsck_clean(&rw.0.borrow(), "mkdir-group-1");
        ext4::rmdir_at(&rw, root, b"late", TEST_NOW).unwrap();
        assert_e2fsck_clean(&rw.0.borrow(), "rmdir-group-1");
    }

    /// **A create, a rename and a `mkdir` that need a directory block the filesystem cannot give
    /// take nothing with them**: the create's inode, the rename's old name, and the `mkdir`'s block
    /// and inode all go back. `e2fsck` clean after each.
    #[test]
    fn what_a_full_directory_refuses_takes_nothing_with_it() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture_blocks(4096, 4096, 0, b"x\n")));
        let free = |rw: &RwImage| (free_blocks(rw), free_inodes(rw));
        let root = ext4::resolve_dir(&rw, b"/").unwrap();
        ext4::mkdir_at(&rw, root, b"d", TEST_NOW).unwrap();
        let d = ext4::resolve_dir(&rw, b"/d").unwrap();
        let long = |i: usize| {
            let mut n = std::format!("n{i:02}");
            while n.len() < 200 {
                n.push('x');
            }
            n
        };
        for i in 0..19 {
            ext4::create_file(&rw, b"/d", long(i).as_bytes(), TEST_NOW).unwrap();
        }
        let sb = ext4::read_superblock(&rw).unwrap();
        let (_, dir) = ext4::resolve_path_ino(&rw, &sb, b"/d").unwrap();
        assert_eq!(rd_u32(&dir, 4), 4096, "one block of entries");
        ext4::create_file(&rw, b"/", b"filler", TEST_NOW).unwrap();
        let mut size = 0;
        while ext4::grow_file(&rw, b"/filler", size + 4096, TEST_NOW).is_ok() {
            size += 4096;
        }
        let full = free(&rw);
        assert_eq!(full.0, 0, "the filesystem is full");

        assert_eq!(ext4::create_file(&rw, b"/d", long(19).as_bytes(), TEST_NOW), Err(FsError::TooLarge));
        assert_eq!(free(&rw), full, "the create gave its inode back");
        let longer = std::format!("{}{}", long(0), "y".repeat(20));
        assert_eq!(ext4::rename_at(&rw, d, long(0).as_bytes(), longer.as_bytes(), TEST_NOW), Err(FsError::TooLarge));
        assert_eq!(ext4::stat_file(&rw, std::format!("/d/{}", long(0)).as_bytes()), Ok(0), "the old name");
        assert_eq!(ext4::stat_file(&rw, std::format!("/d/{longer}").as_bytes()), Err(FsError::NotFound));
        assert_e2fsck_clean(&rw.0.borrow(), "full-create-rename");

        // One block free: the `mkdir` takes it for its own block, then cannot link the directory.
        ext4::truncate_file(&rw, b"/filler", size - 4096, TEST_NOW).unwrap();
        let one = free(&rw);
        assert_eq!(one.0, 1);
        assert_eq!(ext4::mkdir_at(&rw, d, long(19).as_bytes(), TEST_NOW), Err(FsError::TooLarge));
        assert_eq!(free(&rw), one, "the mkdir gave its block and its inode back");
        assert_e2fsck_clean(&rw.0.borrow(), "full-mkdir");
    }

    /// **`e2fsck -fn` on a filesystem we laid out ourselves**: the oracle is somebody else's
    /// implementation of the format.
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

            // Our own reader agrees it is an empty root: `e2fsck` alone would accept a filesystem
            // this parser cannot walk.
            let rw = RwImage(RefCell::new(img));
            assert_eq!(ext4::resolve_dir(&rw, b"/").unwrap(), 2, "the root is inode 2");
            assert_eq!(names_of(&rw, b"/"), [".", ".."], "a new root holds itself and nothing");
            assert!(geom.groups >= 1);

            ext4::create_file(&rw, b"/", b"hello", TEST_NOW).unwrap();
            ext4::grow_file(&rw, b"/hello", 40, TEST_NOW).unwrap();
            assert_eq!(names_of(&rw, b"/"), [".", "..", "hello"]);
            assert_e2fsck_clean(&rw.0.into_inner(), &std::format!("mkfs-{bs}-used"));
        }
    }

    /// **What the module's doc says it declares, read back off the image by `dumpe2fs`.**
    #[test]
    fn the_feature_set_is_exactly_what_the_module_says_it_makes() {
        let (img, _) = format_image(16384, 4096);
        let dir = std::env::temp_dir().join(std::format!("nitrox-mkfs-feat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("img.ext4");
        std::fs::write(&p, &img).unwrap();
        let out = std::process::Command::new("dumpe2fs").args(["-h", p.to_str().unwrap()]).output().unwrap();
        std::fs::remove_dir_all(&dir).ok();
        let text = String::from_utf8_lossy(&out.stdout);
        let line =
            text.lines().find(|l| l.starts_with("Filesystem features:")).expect("dumpe2fs printed no feature line");
        let mut got: Vec<&str> = line["Filesystem features:".len()..].split_whitespace().collect();
        got.sort_unstable();
        assert_eq!(
            got,
            ["dir_nlink", "extent", "extra_isize", "filetype", "huge_file", "large_file", "sparse_super"],
            "the declared set drifted from the module doc: {line}"
        );
        assert!(!line.contains("flex_bg"), "flex_bg is declared but not implemented: {line}");
        assert!(!line.contains("metadata_csum"), "checksums are not maintained: {line}");
        assert!(!line.contains("has_journal"), "there is no journal: {line}");
    }

    /// **More groups than one block of descriptors holds**: a 931 GiB root has 7,452, 59 blocks of
    /// table, and `format` once indexed past a one-block buffer at group 128. **A fixture smaller
    /// than the structure it tests proves it works at that size and nothing more.** Written to a
    /// sparse file, not memory: `mkfs` touches a few hundred blocks of 560 MiB.
    #[test]
    fn a_descriptor_table_spanning_several_blocks_is_written_whole() {
        use std::io::{Read, Seek, SeekFrom, Write};

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

        let dir = std::env::temp_dir().join(std::format!("nitrox-mkfs-wide-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wide.ext4");
        let blocks = 70u64 * 8192;
        let f = std::fs::File::options().create(true).truncate(true).read(true).write(true).open(&path).unwrap();
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
        assert!(geom.gdt_blocks > 1, "this test means nothing with a one-block table ({} blocks)", geom.gdt_blocks);

        // **The last group's descriptor, read back, before the oracle is asked**: a table truncated
        // at its first block leaves it zeroed, and `e2fsck` then spins rather than reporting. The
        // expected value is re-derived from the layout, not from the helper that wrote it.
        let last = geom.groups - 1;
        let expect = geom.group_start(last) + if geom.has_super(last) { 1 + geom.gdt_blocks as u64 } else { 0 };
        let mut d = [0u8; 32];
        img.read_at((geom.first_data_block as u64 + 1) * 1024 + last as u64 * 32, &mut d).unwrap();
        let bitmap = u32::from_le_bytes([d[0], d[1], d[2], d[3]]) as u64;
        assert_eq!(
            bitmap, expect,
            "group {last}'s descriptor is wrong ({bitmap}, expected {expect}) — the table is \
             {} blocks and only the first was written",
            geom.gdt_blocks
        );
        drop(img);

        assert_e2fsck_clean_path(&path, "mkfs-wide");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A filesystem large enough to need **backup superblocks past group 1**. `e2fsck -b` reads a
    /// backup rather than the primary, so it fails if the copy is wrong or misplaced.
    #[test]
    fn the_backup_superblocks_are_where_sparse_super_says_and_are_usable() {
        let blocks = 9 * 8192;
        let (img, geom) = format_image(blocks, 1024);
        assert_eq!(geom.groups, 9);
        assert!(geom.has_super(3) && geom.has_super(5) && geom.has_super(7));
        assert!(!geom.has_super(2) && !geom.has_super(4) && !geom.has_super(6));
        assert_e2fsck_clean(&img, "mkfs-backups");

        let dir = std::env::temp_dir().join(std::format!("nitrox-mkfs-backup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("img.ext4");
        std::fs::write(&p, &img).unwrap();
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

    fn dir_ino(rw: &RwImage, path: &[u8]) -> u32 {
        ext4::resolve_dir(rw, path).unwrap()
    }

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
        ext4::mkdir_at(&rw, sys, b"a", TEST_NOW).unwrap();
        ext4::rename_at(&rw, sys, b"a", b"b", TEST_NOW).unwrap();
        ext4::rmdir_at(&rw, sys, b"b", TEST_NOW).unwrap();
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
        assert!(names_of(&rw, b"/system").iter().any(|n| n == "sub"));
        let sub = ext4::resolve_dir(&rw, b"/system/sub").unwrap();
        assert!(sub > 10);
        let mut inner: Vec<String> = names_of(&rw, b"/system/sub");
        inner.sort();
        assert_eq!(inner, vec![".".to_string(), "..".to_string()]);

        assert_eq!(ext4::mkdir_at(&rw, sys, b"sub", TEST_NOW), Err(FsError::Exists));
        assert_eq!(ext4::mkdir_at(&rw, sys, b".", TEST_NOW), Err(FsError::Unsupported));

        assert_e2fsck_clean(&rw.0.into_inner(), "mkdir");
    }

    /// **An 8 MiB grow and its removal cost commands a run, not a block**, on the device as `Disk`
    /// drives it. A block at a time this grow was 10,251 reads and 8,193 writes and the removal
    /// 10,271 and 6,150; in runs, 26 and 2,058, and 61. Zeroes go 64 KiB a command.
    #[test]
    fn an_eight_mebibyte_grow_and_its_removal_cost_commands_a_run_not_a_block() {
        const BLOCKS: u64 = 2048;
        let rw = Counting::new(fixture(4096, b"seed\n"));
        let sys = ext4::resolve_dir(&rw, b"/system").unwrap();
        let ino = ext4::create_file(&rw, b"/system", b"big", TEST_NOW).unwrap();
        rw.take();

        ext4::grow_file(&rw, b"/system/big", (BLOCKS * 4096) as usize, TEST_NOW).unwrap();
        let (reads, writes) = rw.take();
        assert!(reads <= 32, "{reads} reads to grow {BLOCKS} blocks");
        assert!((BLOCKS / 16..=BLOCKS / 16 + 3 + 16).contains(&writes), "{writes} writes to grow {BLOCKS} blocks");
        let mut runs = [BlockRun::default(); 4];
        let mapped = ext4::map_file(&rw, b"/system/big", &mut runs).unwrap();
        let mapped_blocks: u64 = runs[..mapped.runs].iter().map(|r| r.length as u64).sum();
        assert_eq!((mapped.runs, mapped_blocks), (3, BLOCKS));

        assert_eq!(ext4::unlink_at(&rw, sys, b"big", TEST_NOW), Ok(Some(ino)));
        ext4::release_inode(&rw, ino, TEST_NOW).unwrap();
        let (reads, writes) = rw.take();
        assert!(reads + writes <= 96, "{reads} reads and {writes} writes to remove {BLOCKS} blocks");
        assert_e2fsck_clean(&rw.image.0.borrow(), "grow-run-remove");
    }

    /// **A run lands where a block at a time put each of its blocks**: [`ext4::alloc_run`] called
    /// until it has as many blocks as [`ext4::alloc_block`] called once a block takes the same
    /// blocks in the same order and leaves the image byte for byte the same — across holes, a run
    /// cut short by `want`, and a group's end. [`ext4::free_run`] likewise matches `free_block`.
    #[test]
    fn a_run_lands_where_a_block_at_a_time_put_each_of_its_blocks() {
        use std::cell::RefCell;
        let base = RwImage(RefCell::new(fixture_blocks(1024, 24576, 0, b"seed\n")));
        let sb = ext4::read_superblock(&base).unwrap();
        let first = sb.first_data_block as u64;
        let taken: Vec<u64> = (0..900).map(|_| ext4::alloc_block(&base, &sb, first).unwrap()).collect();
        for (i, &blk) in taken.iter().enumerate() {
            if i % 7 < 2 || i % 13 == 5 {
                ext4::free_block(&base, &sb, blk).unwrap();
            }
        }
        let image = base.0.borrow().clone();
        let free_0 = {
            let mut gd = [0u8; 32];
            base.read_at(2 * 1024, &mut gd).unwrap();
            rd_u16(&gd, 12) as u64
        };
        let want = free_0 + 600; // past group 0's free blocks, into group 1's

        let (a, b) = (RwImage(RefCell::new(image.clone())), RwImage(RefCell::new(image)));
        let (mut by_run, mut goal) = (Vec::new(), first + 100);
        while (by_run.len() as u64) < want {
            let (phys, len) = ext4::alloc_run(&a, &sb, goal, (want - by_run.len() as u64).min(37)).unwrap();
            assert!(len >= 1);
            by_run.extend(phys..phys + len);
            goal = phys + len;
        }
        let (mut by_block, mut goal) = (Vec::new(), first + 100);
        for _ in 0..want {
            let phys = ext4::alloc_block(&b, &sb, goal).unwrap();
            by_block.push(phys);
            goal = phys + 1;
        }
        assert_eq!(by_run, by_block, "the same blocks, in the same order");
        assert!(by_run.iter().any(|&p| p >= first + 8192), "the runs reached group 1");
        assert!(*a.0.borrow() == *b.0.borrow(), "the same bitmaps and counts");

        let end_0 = first + 8192;
        let runs = [(end_0 - 300, 900), (first + 120, 40)];
        for (phys, len) in runs {
            ext4::free_run(&a, &sb, phys, len).unwrap();
            for blk in phys..phys + len {
                ext4::free_block(&b, &sb, blk).unwrap();
            }
        }
        // Byte for byte is the oracle: `e2fsck` is not, on blocks taken for no file.
        assert!(*a.0.borrow() == *b.0.borrow(), "freed alike");
        let before = a.0.borrow().clone();
        assert_eq!(ext4::free_run(&a, &sb, sb.blocks_count - 2, 5), Err(FsError::Corrupt));
        assert_eq!(ext4::free_run(&a, &sb, 0, 1).is_err(), first > 0);
        assert!(*a.0.borrow() == before);
    }

    /// **Nothing the metadata cache holds goes stale.** Through a cache of eight blocks, so it
    /// evicts all the time: files made, grown and given data the way the kernel writes it (behind
    /// the server), renamed, removed and released, a directory filled, emptied and removed so its
    /// blocks become file data, files cut and grown back. Every file then reads what it should.
    #[test]
    fn every_change_through_the_metadata_cache_leaves_it_and_the_disk_agreeing() {
        use libfsserver::cache::{MetaCache, Store};
        use std::cell::RefCell;
        const SIZE: usize = 6000;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n")));
        let mut store = Box::new(Store::<8>::new());
        let c = MetaCache::new(&rw, &mut store);
        let kernel_writes = |path: &[u8], byte: u8| {
            let mut runs = [BlockRun::default(); 8];
            let m = ext4::map_file(&rw, path, &mut runs).unwrap();
            let mut img = rw.0.borrow_mut();
            let mut left = m.size;
            for r in &runs[..m.runs] {
                for b in 0..r.length as u64 {
                    let (at, n) = (((r.device_lba + b) * 4096) as usize, left.min(4096));
                    img[at..at + n].fill(byte);
                    left -= n;
                }
            }
        };
        let path = |dir: &str, name: &str| format!("/system/{dir}/{name}").into_bytes();

        let sys = ext4::resolve_dir(&c, b"/system").unwrap();
        ext4::mkdir_at(&c, sys, b"d1", TEST_NOW).unwrap();
        for i in 0..200u8 {
            let name = format!("f{i:03}");
            ext4::create_file(&c, b"/system/d1", name.as_bytes(), TEST_NOW).unwrap();
            ext4::grow_file(&c, &path("d1", &name), SIZE, TEST_NOW).unwrap();
            kernel_writes(&path("d1", &name), i);
        }
        for i in 0..50 {
            ext4::rename_path(&c, &path("d1", &format!("f{i:03}")), &path("d1", &format!("g{i:03}")), false, TEST_NOW)
                .unwrap();
        }
        let d1 = ext4::resolve_dir(&c, b"/system/d1").unwrap();
        for i in 50..100 {
            let ino = ext4::unlink_at(&c, d1, format!("f{i:03}").as_bytes(), TEST_NOW).unwrap().unwrap();
            ext4::release_inode(&c, ino, TEST_NOW).unwrap();
        }
        ext4::mkdir_at(&c, sys, b"d2", TEST_NOW).unwrap();
        let d2 = ext4::resolve_dir(&c, b"/system/d2").unwrap();
        for i in 0..100 {
            let name = format!("a-long-name-to-fill-blocks-{i:03}");
            ext4::create_file(&c, b"/system/d2", name.as_bytes(), TEST_NOW).unwrap();
        }
        for i in 0..100 {
            let name = format!("a-long-name-to-fill-blocks-{i:03}");
            let ino = ext4::unlink_at(&c, d2, name.as_bytes(), TEST_NOW).unwrap().unwrap();
            ext4::release_inode(&c, ino, TEST_NOW).unwrap();
        }
        ext4::rmdir_at(&c, sys, b"d2", TEST_NOW).unwrap();
        for i in 0..20 {
            let name = format!("n{i:02}");
            ext4::create_file(&c, b"/system/d1", name.as_bytes(), TEST_NOW).unwrap();
            ext4::grow_file(&c, &path("d1", &name), SIZE, TEST_NOW).unwrap();
            kernel_writes(&path("d1", &name), 0xEE);
        }
        // Cut and grown back: what the kernel wrote before the cut stays, the rest reads zero.
        for i in 150..160 {
            ext4::truncate_file(&c, &path("d1", &format!("f{i:03}")), 1000, TEST_NOW).unwrap();
            ext4::grow_file(&c, &path("d1", &format!("f{i:03}")), SIZE, TEST_NOW).unwrap();
        }
        let mut want: Vec<(String, Vec<u8>)> = Vec::new();
        for i in 0..50u8 {
            want.push((format!("g{i:03}"), vec![i; SIZE]));
        }
        for i in 100..200u8 {
            let cut = (150..160).contains(&i);
            let bytes = (0..SIZE).map(|at| if cut && at >= 1000 { 0 } else { i }).collect();
            want.push((format!("f{i:03}"), bytes));
        }
        for i in 0..20 {
            want.push((format!("n{i:02}"), vec![0xEE; SIZE]));
        }
        let mut out = vec![0u8; ext4::MAX_FILE];
        for (name, bytes) in &want {
            let n = ext4::read_file(&c, &path("d1", name), &mut out).unwrap();
            assert!(out[..n] == bytes[..], "{name} reads what was written");
        }
        let mut names = names_of(&rw, b"/system/d1");
        names.retain(|n| n != "." && n != "..");
        names.sort();
        let mut expected: Vec<String> = want.iter().map(|(n, _)| n.clone()).collect();
        expected.sort();
        assert_eq!(names, expected);
        assert!(!names_of(&rw, b"/system").iter().any(|n| n == "d2"));
        assert_e2fsck_clean(&rw.0.borrow(), "meta-cache");

        // **A file's data is never answered from the cache**: read, written again by the kernel,
        // and read at once — the new bytes.
        let mut store = Box::new(Store::<64>::new());
        let c = MetaCache::new(&rw, &mut store);
        for i in 0..5u8 {
            let p = path("d1", &format!("g{i:03}"));
            ext4::read_file(&c, &p, &mut out).unwrap();
            kernel_writes(&p, 0xA0 + i);
            let n = ext4::read_file(&c, &p, &mut out).unwrap();
            assert!(out[..n].iter().all(|&b| b == 0xA0 + i), "g{i:03} reads what the kernel wrote last");
        }
    }

    /// **A directory is read from the device once.** Through a server's cache, a miss in a
    /// directory of 4,096 names reads its twenty blocks, a hit at most its inode-table block, and
    /// every lookup again nothing.
    #[test]
    fn a_directory_is_read_from_the_device_once_through_the_cache() {
        use libfsserver::cache::{MetaCache, SERVER_BLOCKS, Store};
        let rw = Counting::new(fixture_blocks(4096, 16384, 0, b"seed\n"));
        let sys = ext4::resolve_dir(&rw, b"/system").unwrap();
        ext4::mkdir_at(&rw, sys, b"big", TEST_NOW).unwrap();
        for i in 0..4096 {
            ext4::create_file(&rw, b"/system/big", format!("S{i:04}.BIN").as_bytes(), TEST_NOW).unwrap();
        }
        // SAFETY: a `Store` of all zeroes is an empty one, and a megabyte is too much to build on a
        // test's stack, as `Box::new` would.
        let mut store = unsafe { Box::<Store<SERVER_BLOCKS>>::new_zeroed().assume_init() };
        let c = MetaCache::new(&rw, &mut store);
        let mut runs = [BlockRun::default(); 4];
        let lookups = ["S1000.BIN", "S3000.BIN", "S4095.BIN"];
        let uncached = || {
            rw.take();
            ext4::map_file(&rw, b"/system/big/S3000.BIN", &mut [BlockRun::default(); 4]).unwrap();
            rw.take().0
        };
        let scan = uncached();

        rw.take();
        assert_eq!(ext4::map_file(&c, b"/system/big/S9999.BIN", &mut runs).err(), Some(FsError::NotFound));
        let (missed, _) = rw.take();
        for name in lookups {
            ext4::map_file(&c, format!("/system/big/{name}").as_bytes(), &mut runs).unwrap();
            let (found, _) = rw.take();
            assert!(found <= 1, "{name}: {found} reads, its inode's block at most");
        }
        for _ in 0..3 {
            assert_eq!(ext4::map_file(&c, b"/system/big/S9999.BIN", &mut runs).err(), Some(FsError::NotFound));
            for name in lookups {
                ext4::map_file(&c, format!("/system/big/{name}").as_bytes(), &mut runs).unwrap();
            }
        }
        assert_eq!(rw.take().0, 0, "every lookup again reads nothing");
        assert!(missed >= 20 && scan >= 15, "a miss read the directory ({missed}); uncached, a hit did too ({scan})");
    }

    #[test]
    fn unlink_at_removes_a_file_and_stays_e2fsck_clean() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n")));
        let sys = dir_ino(&rw, b"/system");

        let ino = ext4::create_file(&rw, b"/system", b"scratch", TEST_NOW).unwrap();
        ext4::grow_file(&rw, b"/system/scratch", 4096, TEST_NOW).unwrap();
        assert!(names_of(&rw, b"/system").iter().any(|n| n == "scratch"));

        // **Two halves**: the name goes at once, but the inode and its block stay allocated until
        // `release_inode`, after `File::Forget`.
        let (blocks, inodes) = (free_blocks(&rw), free_inodes(&rw));
        assert_eq!(ext4::unlink_at(&rw, sys, b"scratch", TEST_NOW), Ok(Some(ino)));
        assert!(!names_of(&rw, b"/system").iter().any(|n| n == "scratch"));
        assert_eq!(ext4::stat_file(&rw, b"/system/scratch"), Err(FsError::NotFound));
        assert_eq!((free_blocks(&rw), free_inodes(&rw)), (blocks, inodes), "nothing freed yet");
        ext4::release_inode(&rw, ino, TEST_NOW).unwrap();
        assert_eq!((free_blocks(&rw), free_inodes(&rw)), (blocks + 1, inodes + 1), "freed on release");
        assert_eq!(ext4::release_inode(&rw, ino, TEST_NOW), Err(FsError::NotFound));
        let sys_ino = ext4::resolve_dir(&rw, b"/system").unwrap();
        assert_eq!(ext4::release_inode(&rw, sys_ino, TEST_NOW), Err(FsError::NotFound));

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
        ext4::create_file(&rw, b"/system/full", b"f", TEST_NOW).unwrap();

        let full = dir_ino(&rw, b"/system/full");
        let _ = full;
        assert_eq!(ext4::rmdir_at(&rw, sys, b"full", TEST_NOW), Err(FsError::NotEmpty));
        ext4::create_file(&rw, b"/system", b"afile", TEST_NOW).unwrap();
        assert_eq!(ext4::rmdir_at(&rw, sys, b"afile", TEST_NOW), Err(FsError::Unsupported));

        ext4::rmdir_at(&rw, sys, b"empty", TEST_NOW).unwrap();
        assert!(!names_of(&rw, b"/system").iter().any(|n| n == "empty"));

        assert_e2fsck_clean(&rw.0.into_inner(), "rmdir");
    }

    /// **A directory removed is no directory to a session still holding its inode**: every
    /// operation taking a directory by inode refuses it `NotFound`; the mode check alone let
    /// `mkdir_at` write into a freed block.
    #[test]
    fn a_removed_directory_is_refused_to_a_session_still_holding_it() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n")));
        let sys = dir_ino(&rw, b"/system");
        ext4::mkdir_at(&rw, sys, b"gone", TEST_NOW).unwrap();
        let gone = dir_ino(&rw, b"/system/gone");
        ext4::rmdir_at(&rw, sys, b"gone", TEST_NOW).unwrap();
        assert_eq!(ext4::mkdir_at(&rw, gone, b"x", TEST_NOW), Err(FsError::NotFound));
        assert_eq!(ext4::touch_at(&rw, gone, b".", TEST_NOW), Err(FsError::NotFound));
        assert_eq!(ext4::unlink_at(&rw, gone, b"x", TEST_NOW), Err(FsError::NotFound));
        assert_eq!(ext4::rmdir_at(&rw, gone, b"x", TEST_NOW), Err(FsError::NotFound));
        assert_eq!(ext4::rename_at(&rw, gone, b"x", b"y", TEST_NOW), Err(FsError::NotFound));
        assert_eq!(ext4::read_dir_stat(&rw, gone, 0, |_, _, _, _| true).err(), Some(FsError::NotFound));
        assert_e2fsck_clean(&rw.0.into_inner(), "a removed directory left alone");
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

        // Grow to 5 blocks, then cut back to 1.5: the shrink must drop whole extents *and* shorten
        // the one straddling the new end.
        ext4::grow_file(&rw, path, 5 * 4096, TEST_NOW).unwrap();
        let before_free = free_blocks(&rw);

        assert_eq!(ext4::truncate_file(&rw, path, 6000, TEST_NOW), Ok(6000));
        assert_eq!(ext4::stat_file(&rw, path), Ok(6000));

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

        assert_eq!(ext4::truncate_file(&rw, path, 1, TEST_NOW), Ok(1));
        assert_eq!(ext4::stat_file(&rw, path), Ok(1));
        assert_e2fsck_clean(&rw.0.borrow(), "truncate-partial");

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

        // At or above the current size is a no-op reporting the current size; growing is
        // `grow_file`'s job.
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

        assert_eq!(ext4::truncate_file(&rw, b"/system", 0, TEST_NOW), Err(FsError::NotFound));
    }

    /// The superblock's free-block count.
    fn free_blocks(rw: &RwImage) -> u32 {
        let img = rw.0.borrow();
        u32::from_le_bytes(img[1024 + 12..1024 + 16].try_into().unwrap())
    }

    /// **A truncate and then a grow read zero over the regrown range, on the device**, a whole
    /// block and a partial tail. Both halves reuse what the truncate left (asserted: a grow into a
    /// fresh block would pass with no zeroing at all).
    #[test]
    fn a_truncate_then_a_grow_reads_zero_over_the_regrown_range() {
        use crate::BlockRun;
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n")));
        let path = b"/system/current-generation";
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

        assert_eq!(ext4::grow_file(&rw, path, 5000, TEST_NOW), Ok(5000));
        assert_eq!(ext4::stat_file(&rw, path), Ok(5000));

        let mut runs = [BlockRun::default(); 8];
        let ext4::MappedFile { size, runs: n, .. } = ext4::map_file(&rw, path, &mut runs).unwrap();
        assert_eq!(size, 5000);
        let covered: u64 = runs[..n].iter().map(|r| r.length as u64).sum();
        assert_eq!(covered, 2);
        for r in &runs[..n] {
            assert_ne!(r.device_lba, 0);
        }

        assert_e2fsck_clean(&rw.0.into_inner(), "grow");
    }

    #[test]
    fn create_file_links_grows_and_stays_e2fsck_clean() {
        use std::cell::RefCell;
        let rw = RwImage(RefCell::new(fixture(4096, b"seed\n")));

        let ino = ext4::create_file(&rw, b"/system", b"newfile", TEST_NOW).unwrap();
        assert!(ino > 10, "should not reuse a reserved inode");
        assert_eq!(ext4::stat_file(&rw, b"/system/newfile"), Ok(0));
        assert_eq!(ext4::create_file(&rw, b"/system", b"newfile", TEST_NOW), Ok(ino));
        assert_eq!(ext4::grow_file(&rw, b"/system/newfile", 100, TEST_NOW), Ok(100));
        assert_eq!(ext4::stat_file(&rw, b"/system/newfile"), Ok(100));

        assert_e2fsck_clean(&rw.0.into_inner(), "create");
    }
}
