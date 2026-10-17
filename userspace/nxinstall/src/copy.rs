//! Copying one filesystem's contents into another, entry by entry.
//!
//! Not a sector copy: a filesystem records its own size, so a byte-for-byte copy of a 24 MiB root
//! would leave 24 MiB on a 931 GiB partition. This walks the source and creates each directory and
//! file in a filesystem made for the disk (`fs_server_ext4::mkfs`), so the result fills the
//! partition.
//!
//! The source is `install-root.img`, a pristine copy of the system that nothing mounts and nothing
//! writes. The live root would carry whatever the session had touched (a login under `/home`, a
//! shell's history), and `init` has it mounted, so a read could meet a directory block half-way
//! through a rewrite.
//!
//! Permissions are not carried: `create_file` writes `0644` and `mkdir_at` `0755`, and nothing
//! reads those bits. Every entry is stamped with the install's clock, not the source's.

use alloc::vec::Vec;

use fs_server_ext4::{BlockReader, BlockRun, BlockWriter, FsError, ext4};

/// `ext4_dir_entry_2`'s type byte for a regular file.
const FT_REG: u8 = 1;
/// …and for a directory.
const FT_DIR: u8 = 2;

/// How much file data moves per round trip: whole 4 KiB blocks, so a partial block only happens
/// at the end of a file.
const COPY_BUF: usize = 64 * 1024;

/// What a copy did.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Copied {
    /// Directories created.
    pub dirs: u32,
    /// Files created.
    pub files: u32,
    /// Bytes of file content written.
    pub bytes: u64,
}

/// Copy everything under the source's root into the destination's (a [`fs_server_ext4::mkfs`]
/// filesystem with an empty root), but what `pass_over` names: paths from the root, where a
/// directory is made empty and a file is not made at all. `say` is called once per directory.
pub fn copy_tree<S, D>(
    src: &S,
    dst: &D,
    now: i64,
    pass_over: &[&[u8]],
    say: &mut dyn FnMut(&str),
) -> Result<Copied, FsError>
where
    S: BlockReader,
    D: BlockReader + BlockWriter,
{
    let mut out = Copied::default();
    // An explicit stack: the depth is the tree's, and the fixed userspace stack already holds a
    // 64 KiB copy buffer.
    let mut pending: Vec<Vec<u8>> = Vec::new();
    pending.push(Vec::from(&b"/"[..]));
    let mut buf = alloc::vec![0u8; COPY_BUF];

    while let Some(dir) = pending.pop() {
        let src_ino = ext4::resolve_dir(src, &dir)?;
        // Gathered before anything is written, so a copy that fails part-way leaves the two
        // filesystems' state independent.
        let mut entries: Vec<(u8, Vec<u8>)> = Vec::new();
        let mut cursor = 0u64;
        loop {
            let next = ext4::read_dir(src, src_ino, cursor, |_ino, ft, name| {
                if name != b"." && name != b".." {
                    entries.push((ft, Vec::from(name)));
                }
                true
            })?;
            if next == 0 {
                break;
            }
            cursor = next;
        }

        for (ft, name) in entries {
            let child = join(&dir, &name);
            let passed = pass_over.contains(&child.as_slice());
            match ft {
                FT_DIR => {
                    let parent = ext4::resolve_dir(dst, &dir)?;
                    ext4::mkdir_at(dst, parent, &name, now)?;
                    out.dirs += 1;
                    if !passed {
                        pending.push(child);
                    }
                }
                FT_REG if passed => {}
                FT_REG => {
                    out.bytes += copy_file(src, dst, &dir, &name, &child, now, &mut buf)?;
                    out.files += 1;
                }
                // Refused, not skipped: a symlink or device node in our own root is a change
                // to what we ship, and dropping it would install a system missing something.
                _ => return Err(FsError::Unsupported),
            }
        }
        say(&alloc::format!("  {} ({} file(s) so far)", core::str::from_utf8(&dir).unwrap_or("?"), out.files));
    }
    Ok(out)
}

fn copy_file<S, D>(
    src: &S,
    dst: &D,
    parent: &[u8],
    name: &[u8],
    path: &[u8],
    now: i64,
    buf: &mut [u8],
) -> Result<u64, FsError>
where
    S: BlockReader,
    D: BlockReader + BlockWriter,
{
    let size = ext4::stat_file(src, path)?;
    ext4::create_file(dst, parent, name, now)?;
    if size == 0 {
        return Ok(0);
    }
    // Allocating the whole file first keeps it in one extent: `alloc_block` follows the
    // previous block.
    ext4::grow_file(dst, path, size, now)?;

    let mut at = 0u64;
    while at < size as u64 {
        let want = ((size as u64 - at) as usize).min(buf.len());
        let got = ext4::read_file_range(src, path, at, want, buf)?;
        if got == 0 {
            return Err(FsError::Io); // a short read of a file we just sized
        }
        write_range(dst, path, at, buf, got)?;
        at += got as u64;
    }
    Ok(size as u64)
}

/// Make `dir/name` in the destination holding `bytes`: a small file the installer writes rather
/// than copies.
///
/// `Exists` if the name is taken, as [`put_dir`] is. `create_file` hands back a file already
/// there, and writing over it would leave the copied file's tail past a shorter replacement, so a
/// pass-over list that lost this path would silently install a file that is neither the build's
/// nor the installer's.
pub fn put_file<D>(dst: &D, dir: &[u8], name: &[u8], bytes: &[u8], now: i64) -> Result<(), FsError>
where
    D: BlockReader + BlockWriter,
{
    if taken(dst, dir, name)? {
        return Err(FsError::Exists);
    }
    let path = join(dir, name);
    ext4::create_file(dst, dir, name, now)?;
    if bytes.is_empty() {
        return Ok(());
    }
    ext4::grow_file(dst, &path, bytes.len(), now)?;
    let bs = ext4::block_size(dst)? as usize;
    let mut buf = alloc::vec![0u8; bytes.len().div_ceil(bs) * bs];
    buf[..bytes.len()].copy_from_slice(bytes);
    let written = write_range(dst, &path, 0, &mut buf, bytes.len());
    // This includes a password's verifier; the buffer is zeroed on success or failure.
    buf.fill(0);
    written
}

/// Write `buf[..got]`, the file's bytes from `at`, to the blocks the destination gave `path`
/// there. `buf` must hold `got` rounded up to a block; the tail past `got` is zeroed here.
fn write_range<D>(dst: &D, path: &[u8], at: u64, buf: &mut [u8], got: usize) -> Result<(), FsError>
where
    D: BlockReader + BlockWriter,
{
    let bs = ext4::block_size(dst)? as u64;
    // The block is written whole; zero past `got` so stale disk contents are not kept.
    let end = (got as u64).div_ceil(bs) * bs;
    buf[got..end as usize].fill(0);

    let mut runs = [BlockRun::default(); 8];
    let first_block = at / bs;
    let count = end / bs;
    let n = ext4::map_range(dst, path, first_block, count, &mut runs)?;
    let mut written = 0u64;
    for run in &runs[..n] {
        if run.device_lba == 0 {
            return Err(FsError::Corrupt); // a hole in a file we just grew
        }
        let bytes = run.length as u64 * bs;
        let take = bytes.min(end - written);
        dst.write_at(run.device_lba * bs, &buf[written as usize..(written + take) as usize])?;
        written += take;
        if written >= end {
            break;
        }
    }
    if written < end {
        return Err(FsError::TooLarge); // more runs than the map buffer holds
    }
    Ok(())
}

/// Make `dir/name` an empty directory in the destination.
pub fn put_dir<D>(dst: &D, dir: &[u8], name: &[u8], now: i64) -> Result<(), FsError>
where
    D: BlockReader + BlockWriter,
{
    let parent = ext4::resolve_dir(dst, dir)?;
    ext4::mkdir_at(dst, parent, name, now)?;
    Ok(())
}

fn taken<D: BlockReader>(dst: &D, dir: &[u8], name: &[u8]) -> Result<bool, FsError> {
    let ino = ext4::resolve_dir(dst, dir)?;
    let mut found = false;
    let mut cursor = 0u64;
    loop {
        let next = ext4::read_dir(dst, ino, cursor, |_ino, _ft, entry| {
            found = entry == name;
            !found
        })?;
        if found || next == 0 {
            return Ok(found);
        }
        cursor = next;
    }
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut p = Vec::from(dir);
    if p.last() != Some(&b'/') {
        p.push(b'/');
    }
    p.extend_from_slice(name);
    p
}
