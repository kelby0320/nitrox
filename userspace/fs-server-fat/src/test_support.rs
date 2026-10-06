//! **Host-test fixtures**: FAT images the host's own tools build — `mformat` and `mkfs.fat`, with
//! files put on them by `mmd` and `mcopy` — read and written through a [`FileImage`], which reads
//! the image's file rather than loading it, so a 300 MiB image costs a sparse file.

use crate::{BlockReader, BlockWriter, FsError};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// A FAT image in a file, removed when dropped.
pub(crate) struct FileImage {
    pub file: std::fs::File,
    pub path: PathBuf,
}

impl FileImage {
    pub fn open(path: &Path) -> FileImage {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
        FileImage { file, path: path.to_path_buf() }
    }
}

impl Drop for FileImage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl BlockReader for FileImage {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        self.file.read_exact_at(buf, offset).map_err(|_| FsError::Io)
    }
}

impl BlockWriter for FileImage {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
        self.file.write_all_at(buf, offset).map_err(|_| FsError::Io)
    }
}

/// **A fresh path under the temp directory**, unique per call: cargo runs tests in parallel.
pub(crate) fn scratch(what: &str) -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("nitrox-fat-{}-{}-{what}", std::process::id(), n))
}

/// **A blank, sparse image** of `mib` MiB.
pub(crate) fn blank(mib: u64) -> PathBuf {
    let p = scratch("img");
    std::fs::File::create(&p).unwrap().set_len(mib << 20).unwrap();
    p
}

/// Run an mtools command on `img`, with names in UTF-8 and no complaint about geometry.
pub(crate) fn mtools(tool: &str, img: &Path, args: &[&str]) -> std::process::Output {
    let out = Command::new(tool)
        .arg("-i")
        .arg(img)
        .args(args)
        .env("MTOOLS_SKIP_CHECK", "1")
        .env("LC_ALL", "C.UTF-8")
        .output()
        .unwrap_or_else(|e| panic!("{tool} must be installed (mtools) to run fs-server-fat's tests: {e}"));
    assert!(out.status.success(), "{tool} {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    out
}

/// **`mformat` an image** of `mib` MiB with `args` — `-F` for FAT32, `-c` for sectors a cluster.
pub(crate) fn mformat(mib: u64, args: &[&str]) -> PathBuf {
    let p = blank(mib);
    let mut all: Vec<&str> = args.to_vec();
    all.push("::");
    mtools("mformat", &p, &all);
    p
}

/// **`mkfs.fat` an image** of `mib` MiB with `args`.
pub(crate) fn mkfs(mib: u64, args: &[&str]) -> PathBuf {
    let p = blank(mib);
    let out = Command::new("mkfs.fat")
        .args(args)
        .arg(&p)
        .output()
        .expect("mkfs.fat must be installed (dosfstools) to run fs-server-fat's tests");
    assert!(out.status.success(), "mkfs.fat {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    p
}

/// Make the directory `dir` on `img`.
pub(crate) fn mmd(img: &Path, dir: &str) {
    mtools("mmd", img, &[&format!("::{dir}")]);
}

/// **Put `bytes` on `img` at `dest`** with `mcopy`.
pub(crate) fn put(img: &Path, dest: &str, bytes: &[u8]) {
    let src = scratch("src");
    std::fs::write(&src, bytes).unwrap();
    mtools("mcopy", img, &["-o", src.to_str().unwrap(), &format!("::{dest}")]);
    let _ = std::fs::remove_file(&src);
}

/// Bytes a test can tell apart: position-dependent, and different per `seed`.
pub(crate) fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u32).wrapping_mul(2_654_435_761).wrapping_shr(24) as u8 ^ seed).collect()
}
