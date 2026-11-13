//! Whole files by path, over [`libos`]: sizes, reads, writes, copies, replacing saves and renames.

use alloc::vec::Vec;
use libkern::Rights;
use libkern::abi::HandleInfo;
use libkern::error::KError;
use libkern::handle::{KOBJ_FILE_OBJECT, KOBJ_MEMORY_OBJECT};
use libos::{CanLookup, File, Handle, MapRead, MapReadWrite, Namespace, Ref, block_on, fs};
use librsproto::namespace::RENAME_REPLACE;

use crate::tree::remove_entry;

/// What a file operation can fail with.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FileError {
    /// The path does not resolve.
    NotFound,
    /// The destination already exists and no overwrite was requested.
    Exists,
    /// The destination could not be shrunk to the source's length.
    TruncateFailed,
    /// The file exceeds [`MAX_COPY`].
    TooLarge,
    /// A [`rename_path`] across filesystems; the caller falls back to [`copy`] + unlink.
    CrossDevice,
    /// A syscall failed; the payload is its negative return.
    Io(i32),
}

/// The largest file copied in one mapping.
///
/// A copy maps its file whole, so this bounds address space, not RAM; a
/// windowed copy would not lift it, as unmapping leaves the pages with the page-cache object.
/// A copy through `sys_io_submit` will (`copy-limit` in `deferred-decisions.md`).
pub const MAX_COPY: u64 = 8 * 1024 * 1024;

/// The suffix of [`save`]'s temporary. Beside the file it replaces, as a rename is only
/// atomic within one filesystem.
pub const SAVE_SUFFIX: &str = ".save-tmp";

/// Which step of [`save`] failed; either way the file is as it was.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SaveError {
    /// The temporary could not be written.
    Write(FileError),
    /// The temporary could not be renamed over the file.
    Replace(FileError),
}

fn look() -> Rights {
    Rights::MAP_READ | Rights::INSPECT
}

/// `INSPECT` too: a handle the caller will not map is still stated.
fn rw() -> Rights {
    Rights::MAP_READ | Rights::MAP_WRITE | Rights::INSPECT
}

fn io(e: libos::Error) -> FileError {
    FileError::Io(e.code().as_i32())
}

fn resolve<'a, M: CanLookup + 'a>(ns: Ref<'a, Namespace, M>, path: &str) -> libos::Result<Handle<File, MapRead>> {
    block_on(ns.lookup(path, look()))
}

/// The size of the object `handle` names, or `None` if it does not stat as a file or memory
/// object; a directory's session endpoint stats with size 0.
fn stated<T, M>(handle: &Handle<T, M>) -> Option<u64> {
    let HandleInfo { object_type, size, .. } = handle.stat().ok()?;
    matches!(object_type, KOBJ_FILE_OBJECT | KOBJ_MEMORY_OBJECT).then_some(size)
}

/// Resolve `path` and return its size, or `None` if it does not resolve to a file (a directory
/// does not).
pub fn file_len<'a, M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>, path: &str) -> Option<u64> {
    stated(&resolve(ns.into(), path).ok()?)
}

/// Copy the regular file at `src` to `dst`, returning the bytes copied.
///
/// [`FileError::Exists`] if `dst` exists and `overwrite` is false. A longer `dst` is shrunk first,
/// else its old tail would survive. `dst` is resolved twice at most: once to look, once to shrink
/// or to create.
pub fn copy<'a, M: CanLookup + 'a>(
    ns: impl Into<Ref<'a, Namespace, M>>,
    src: &str,
    dst: &str,
    overwrite: bool,
) -> Result<u64, FileError> {
    let ns = ns.into();
    let src = resolve(ns, src).map_err(|_| FileError::NotFound)?;
    copy_from(ns, &src, dst, overwrite)
}

/// [`copy`] from the file `src` names, stated and mapped through that one handle.
fn copy_from<M: CanLookup>(
    ns: Ref<'_, Namespace, M>,
    src: &Handle<File, MapRead>,
    dst: &str,
    overwrite: bool,
) -> Result<u64, FileError> {
    let size = stated(src).ok_or(FileError::NotFound)?;
    if size > MAX_COPY {
        return Err(FileError::TooLarge);
    }
    let existing = file_len(ns, dst);
    if existing.is_some() && !overwrite {
        return Err(FileError::Exists);
    }
    let out = open_sized(ns, dst, size, existing)?;
    // A zero-length object cannot be mapped, but the destination must still exist.
    if size == 0 {
        return Ok(0);
    }
    copy_bytes(src, &out, size as usize)?;
    Ok(size)
}

/// `path` as a read-write handle of exactly `size` bytes, given `existing`, its size now.
///
/// A longer file is shrunk and its size read back from the handle the shrink gave: a silent
/// failure would keep the tail. Anything else is a create, which grows a shorter file.
fn open_sized<M: CanLookup>(
    ns: Ref<'_, Namespace, M>,
    path: &str,
    size: u64,
    existing: Option<u64>,
) -> Result<Handle<File, MapReadWrite>, FileError> {
    if existing.is_some_and(|len| len > size) {
        let shrunk: Handle<File, MapReadWrite> = block_on(fs::truncate(ns, path, size, rw())).map_err(io)?;
        return if stated(&shrunk) == Some(size) { Ok(shrunk) } else { Err(FileError::TruncateFailed) };
    }
    block_on(fs::create(ns, path, size, rw())).map_err(io)
}

/// Copy `size` bytes between the files' mappings, then flush the destination to the device.
fn copy_bytes(src: &Handle<File, MapRead>, dst: &Handle<File, MapReadWrite>, size: usize) -> Result<(), FileError> {
    {
        let from = src.map(size).map_err(io)?;
        dst.map(size).map_err(io)?.copy_from(0, &from, 0, size).map_err(io)?;
    }
    // After the unmaps: a writable mapping still in place leaves the file dirty, and the kernel
    // keeps a dirty file's pages until the next flush.
    block_on(fs::flush(dst)).map_err(io)
}

/// Read the whole file at `path`.
///
/// An empty file is `Ok(empty)`: there is nothing to map. Lives here rather than with its one
/// consumer because it is mechanism over this module's private primitives, not policy.
pub fn read<'a, M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>, path: &str) -> Result<Vec<u8>, FileError> {
    let file = resolve(ns.into(), path).map_err(|_| FileError::NotFound)?;
    let size = match stated(&file) {
        Some(size) if size <= MAX_COPY => size as usize,
        Some(_) => return Err(FileError::TooLarge),
        None => return Err(FileError::NotFound),
    };
    let mut out = alloc::vec![0u8; size];
    if size > 0 {
        file.map(size).map_err(io)?.read_into(0, &mut out).map_err(io)?;
    }
    Ok(out)
}

/// `bytes` without the trailing NULs of a snapshot (sized in pages, so `/dev/log` is zero-padded).
/// For text only: a binary file may end in zeroes it means.
pub fn unpadded(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1)]
}

/// Write `bytes` to `path`, replacing whatever was there, shrinking first as [`copy`] does.
///
/// Not a safe save: a failure after the truncate leaves a file shorter than both versions.
/// [`save`] is one.
pub fn write<'a, M: CanLookup + 'a>(
    ns: impl Into<Ref<'a, Namespace, M>>,
    path: &str,
    bytes: &[u8],
) -> Result<(), FileError> {
    let ns = ns.into();
    let size = bytes.len() as u64;
    if size > MAX_COPY {
        return Err(FileError::TooLarge);
    }
    let out = open_sized(ns, path, size, file_len(ns, path))?;
    if size == 0 {
        return Ok(());
    }
    let mut mapped = out.map(bytes.len()).map_err(io)?;
    mapped.write_from(0, bytes).map_err(io)?;
    drop(mapped);
    block_on(fs::flush(&out)).map_err(io)
}

/// Replace the file at `path` with `bytes` the safe way: a temporary beside it, written whole,
/// then renamed over it. A failure at either step leaves `path` as it was and removes the
/// temporary, so no file is left holding neither version.
pub fn save<'a, M: CanLookup + 'a>(
    ns: impl Into<Ref<'a, Namespace, M>>,
    path: &str,
    bytes: &[u8],
) -> Result<(), SaveError> {
    let ns = ns.into();
    let temp = alloc::format!("{path}{SAVE_SUFFIX}");
    if let Err(e) = write(ns, &temp, bytes) {
        let _ = remove_entry(ns, &temp);
        return Err(SaveError::Write(e));
    }
    if let Err(e) = rename_path(ns, &temp, path, true) {
        let _ = remove_entry(ns, &temp);
        return Err(SaveError::Replace(e));
    }
    Ok(())
}

/// Create `path` as an empty file if absent; idempotent, so it cannot report which happened.
pub fn ensure_file<'a, M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>, path: &str) -> Result<(), FileError> {
    let _: Handle<File, MapReadWrite> = block_on(fs::create(ns, path, 0, rw())).map_err(io)?;
    Ok(())
}

/// Rename `src` to `dst`, absolute namespace paths; `replace` unlinks an existing destination.
///
/// One syscall, atomic to readers. Only within one filesystem: elsewhere it returns
/// [`FileError::CrossDevice`], the cue to fall back to [`copy`] + unlink.
pub fn rename_path<'a, M: CanLookup + 'a>(
    ns: impl Into<Ref<'a, Namespace, M>>,
    src: &str,
    dst: &str,
    replace: bool,
) -> Result<(), FileError> {
    let flags = if replace { RENAME_REPLACE } else { 0 };
    block_on(fs::rename(ns, src, dst, flags)).map_err(rename_error)
}

/// Map a cross-filesystem destination to `CrossDevice`. An occupied one is not distinguished: the
/// server reports it as `InvalidArgument`, as it does a malformed request; use [`file_len`] first.
pub(crate) fn rename_error(e: libos::Error) -> FileError {
    match e.code() {
        KError::Unsupported => FileError::CrossDevice,
        KError::NotFound => FileError::NotFound,
        code => FileError::Io(code.as_i32()),
    }
}
