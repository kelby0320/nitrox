//! Copying one filesystem's contents into another, entry by entry.
//!
//! **Not a sector copy.** H.1 copied the live root's bytes onto the target, which put a 24 MiB
//! filesystem on a 931 GiB partition: the filesystem records its own size, so it did not know
//! about the space around it, and nothing could grow it. This walks the source and *creates*
//! each directory and file in a filesystem made for the disk (`fs_server_ext4::mkfs`), so the
//! result is as large as the partition.
//!
//! ## What it copies from, which had to be decided
//!
//! **The source's bytes on the device, not the running system's view of them.** The live root
//! is a RAM disk this session has been writing to, and a write reaches the device only when
//! something syncs the file — so the two differ, and the difference is whatever the session
//! has touched: a login writing under `/home`, a shell's history. Reading the device installs
//! **the system as it shipped**, which is what an installer should produce; reading the live
//! view would install a machine carrying one session's accidents.
//!
//! ## What it does not carry
//!
//! **Permissions.** `create_file` writes `0644` and `mkdir_at` writes `0755`, and nothing in
//! this system reads those bits: what a program may do comes from the namespace it was given,
//! not from a mode on a file. Copying them would be copying a field no reader consults.
//!
//! **Timestamps.** Every entry is stamped with the install's own clock rather than the
//! source's, because that is what happened: these files were created now.

extern crate alloc;

use alloc::vec::Vec;

use fs_server_ext4::{BlockReader, BlockWriter, BlockRun, FsError, ext4};

/// `ext4_dir_entry_2`'s type byte for a regular file.
const FT_REG: u8 = 1;
/// …and for a directory.
const FT_DIR: u8 = 2;

/// How much file data moves per round trip. Whole 4 KiB blocks, so a partial block only ever
/// happens at the end of a file.
const COPY_BUF: usize = 64 * 1024;

/// What a copy did, for the report and for a gate to assert on.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Copied {
    /// Directories created.
    pub dirs: u32,
    /// Files created.
    pub files: u32,
    /// Bytes of file content written.
    pub bytes: u64,
}

/// Copy everything under the source's root into the destination's.
///
/// The destination must be a filesystem with an empty root — [`fs_server_ext4::mkfs::format`]
/// makes one. `say` is called once per directory so a person watching sees progress; a tree of
/// a few hundred entries takes long enough on real hardware to need it.
pub fn copy_tree<S, D>(
    src: &S,
    dst: &D,
    now: i64,
    say: &mut dyn FnMut(&str),
) -> Result<Copied, FsError>
where
    S: BlockReader,
    D: BlockReader + BlockWriter,
{
    let mut out = Copied::default();
    // **An explicit stack, not recursion.** The depth is the tree's, which is data — and this
    // runs on a fixed userspace stack with a 64 KiB copy buffer already on it.
    let mut pending: Vec<Vec<u8>> = Vec::new();
    pending.push(Vec::from(&b"/"[..]));
    let mut buf = Vec::new();
    buf.resize(COPY_BUF, 0u8);

    while let Some(dir) = pending.pop() {
        let src_ino = ext4::resolve_dir(src, &dir)?;
        // Read the whole directory before writing anything into the destination: the walk
        // holds a cursor into the *source*, and creating entries in the destination between
        // reads would be fine — but gathering first keeps the two filesystems' state
        // independent, which is one less thing to reason about when a copy fails part-way.
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
            match ft {
                FT_DIR => {
                    let parent = ext4::resolve_dir(dst, &dir)?;
                    ext4::mkdir_at(dst, parent, &name, now)?;
                    out.dirs += 1;
                    pending.push(child);
                }
                FT_REG => {
                    out.bytes += copy_file(src, dst, &dir, &name, &child, now, &mut buf)?;
                    out.files += 1;
                }
                // **Anything else is refused, not skipped.** The source is our own root and
                // holds regular files and directories; a symlink or a device node appearing
                // in it is a change to what we ship, and an installer that silently dropped
                // it would produce a system missing something nobody looked for.
                _ => return Err(FsError::Unsupported),
            }
        }
        say(&alloc::format!(
            "  {} ({} file(s) so far)",
            core::str::from_utf8(&dir).unwrap_or("?"),
            out.files
        ));
    }
    Ok(out)
}

/// Create `parent/name` in the destination and copy its contents across. Returns the bytes.
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
    // Allocating the whole file first is what keeps it in one extent: `alloc_block` follows
    // the previous block, and on a filesystem this empty that is the next one.
    ext4::grow_file(dst, path, size, now)?;

    let bs = ext4::block_size(dst)? as u64;
    let mut at = 0u64;
    while at < size as u64 {
        let want = ((size as u64 - at) as usize).min(buf.len());
        let got = ext4::read_file_range(src, path, at, want, buf)?;
        if got == 0 {
            return Err(FsError::Io); // a short read of a file we just sized
        }
        // **Zero the tail of the final block.** Only `size` bytes are the file's, but the
        // block is written whole, and what follows them would otherwise be whatever the disk
        // held before — invisible through the filesystem, and still on the disk.
        let end = (got as u64).div_ceil(bs) * bs;
        buf[got..end as usize].fill(0);

        // Where the destination put those blocks. Re-mapped per chunk rather than once,
        // because a file large enough to need several extents has no single run.
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
            dst.write_at(
                run.device_lba * bs,
                &buf[written as usize..(written + take) as usize],
            )?;
            written += take;
            if written >= end {
                break;
            }
        }
        if written < end {
            return Err(FsError::TooLarge); // more runs than the map buffer holds
        }
        at += got as u64;
    }
    Ok(size as u64)
}

/// `dir` + `/` + `name`, without a double slash at the root.
fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut p = Vec::from(dir);
    if p.last() != Some(&b'/') {
        p.push(b'/');
    }
    p.extend_from_slice(name);
    p
}
