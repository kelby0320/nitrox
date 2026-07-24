//! File-level filesystem helpers shared by the coreutils.
//!
//! The *directory* operations live in [`librsproto::session::Dir`] (name-addressed RPC on
//! a directory session). This module covers the other half — whole-file read and write —
//! which does not go through that protocol at all: a file resolves to a **page-cache
//! object** the process maps, so copying a file is a `memcpy` between two mappings and the
//! kernel moves the data (Model A; `docs/architecture/filesystem-data-path.md`).
//!
//! Kept here rather than in `librsproto` because there is no protocol involved: these are
//! namespace + memory syscalls.

use alloc::string::String;
use libkern::abi::HandleInfo;
use libkern::handle::{RIGHT_INSPECT, RIGHT_MAP_READ, RIGHT_MAP_WRITE};
use libkern::syscall::{
    SYS_FILE_CREATE, SYS_FILE_SYNC, SYS_FILE_TRUNCATE, SYS_HANDLE_CLOSE, SYS_HANDLE_STAT, SYS_MEMORY_MAP,
    SYS_MEMORY_UNMAP, SYS_NS_LOOKUP, SYS_WAIT, syscall1, syscall2, syscall4, syscall5,
};

/// What a file operation can fail with.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FileError {
    /// The path does not resolve.
    NotFound,
    /// The destination already exists and no overwrite was requested.
    Exists,
    /// The destination could not be shrunk to the source's length, so overwriting it
    /// would have left the old tail behind. See [`copy_file`].
    TruncateFailed,
    /// The file exceeds [`MAX_COPY`].
    TooLarge,
    /// A syscall failed; the payload is its negative return.
    Io(i32),
}

/// The largest file a coreutil copies in one mapping.
///
/// A file maps whole (there is no offset argument to `sys_memory_map`), so a copy costs
/// two mappings of the file's size in *address space* — the pages themselves are
/// demand-paged, so this bounds VA, not RAM. 8 MiB is far above anything in the current
/// image and far below anything that would strain a 47-bit user half; a windowed copy is
/// the refinement if a real workload ever exceeds it.
pub const MAX_COPY: u64 = 8 * 1024 * 1024;

/// Resolve `path` and return its size, or `None` if it does not resolve to a file.
///
/// Used to answer "does the destination exist?" — a directory does not resolve to a file
/// object, so this is `None` for one (ask [`librsproto::session::Dir::open`] instead).
pub fn file_size(ns: u64, path: &[u8]) -> Option<u64> {
    let handle = lookup(ns, path, RIGHT_MAP_READ | RIGHT_INSPECT).ok()?;
    let mut info = HandleInfo {
        rights: 0,
        object_type: 0,
        generation: 0,
        size: 0,
    };
    // SAFETY: `info` is a valid, correctly sized `HandleInfo` out-param. (Never a
    // hand-sized byte array — see the 2026-07-24 stack-smash entry in the decision log.)
    let r = unsafe { syscall2(SYS_HANDLE_STAT, handle, (&raw mut info) as u64) };
    // SAFETY: closing the handle we just resolved.
    unsafe { syscall1(SYS_HANDLE_CLOSE, handle) };
    if r != 0 { None } else { Some(info.size) }
}

/// Copy the regular file at `src` to `dst`, returning the bytes copied.
///
/// Fails with [`FileError::Exists`] if `dst` is already there and `overwrite` is false —
/// the fail-loud default (design §10d: "fail loud if destination exists, unless
/// `--force`").
///
/// **Overwriting a longer file shrinks it first.** Creating an existing file is
/// idempotent and growing it to a smaller size is a no-op, so without an explicit
/// truncate the destination's old tail would survive past the new content — a file that
/// is neither the old one nor the new one. `copy` refused that case outright until the
/// filesystem gained a truncate (decision log, 2026-07-24).
pub fn copy_file(ns: u64, src: &[u8], dst: &[u8], overwrite: bool) -> Result<u64, FileError> {
    let size = file_size(ns, src).ok_or(FileError::NotFound)?;
    if size > MAX_COPY {
        return Err(FileError::TooLarge);
    }
    if let Some(existing) = file_size(ns, dst) {
        if !overwrite {
            return Err(FileError::Exists);
        }
        if existing > size {
            // Shrink to the source's length *before* writing, so no byte of the old
            // tail can survive — and verify it, since a silently-failed truncate would
            // leave exactly the corruption this is here to prevent.
            truncate(ns, dst, size)?;
            if file_size(ns, dst) != Some(size) {
                return Err(FileError::TruncateFailed);
            }

        }
    }

    // An empty source needs no mapping at all — `sys_memory_map` has no meaning for a
    // zero-length object — but the destination must still come into existence.
    if size == 0 {
        let dst_handle = create(ns, dst, 0)?;
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, dst_handle) };
        return Ok(0);
    }

    let src_handle = lookup(ns, src, RIGHT_MAP_READ | RIGHT_INSPECT).map_err(FileError::Io)?;
    let dst_handle = match create(ns, dst, size) {
        Ok(h) => h,
        Err(e) => {
            // SAFETY: closing our own handle before bailing.
            unsafe { syscall1(SYS_HANDLE_CLOSE, src_handle) };
            return Err(e);
        }
    };

    let result = map_copy(src_handle, dst_handle, size);
    // SAFETY: closing handles this function owns.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, src_handle);
        syscall1(SYS_HANDLE_CLOSE, dst_handle);
    }
    result.map(|()| size)
}

/// Map both files, copy the bytes, flush the destination to the device, unmap.
fn map_copy(src_handle: u64, dst_handle: u64, size: u64) -> Result<(), FileError> {
    // SAFETY: mapping objects we hold handles to, with rights they carry.
    let src_addr = unsafe { syscall4(SYS_MEMORY_MAP, src_handle, 0, size, RIGHT_MAP_READ) };
    if src_addr < 0 {
        return Err(FileError::Io(src_addr as i32));
    }
    // SAFETY: as above; the destination was created with MAP_READ | MAP_WRITE.
    let dst_addr = unsafe {
        syscall4(SYS_MEMORY_MAP, dst_handle, 0, size, RIGHT_MAP_READ | RIGHT_MAP_WRITE)
    };
    if dst_addr < 0 {
        // SAFETY: unmapping the mapping we just made.
        unsafe { syscall2(SYS_MEMORY_UNMAP, src_addr as u64, size) };
        return Err(FileError::Io(dst_addr as i32));
    }

    // Both mappings are `size` bytes of the respective page-cache objects, and they are
    // distinct objects (a self-copy is refused by the caller's path comparison), so the
    // ranges cannot overlap.
    // SAFETY: `size` bytes are mapped at both addresses, and the regions are disjoint.
    unsafe {
        core::ptr::copy_nonoverlapping(
            src_addr as *const u8,
            dst_addr as *mut u8,
            size as usize,
        );
    }

    // Flush the written pages before dropping the mapping: without this the copy lives
    // only in the page cache, and a reader that re-resolves the path sees a short file.
    // SAFETY: `dst_handle` is our writable file handle.
    let synced = unsafe { syscall1(SYS_FILE_SYNC, dst_handle) };
    // SAFETY: unmapping our own mappings.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, dst_addr as u64, size);
        syscall2(SYS_MEMORY_UNMAP, src_addr as u64, size);
    }
    if synced != 0 {
        return Err(FileError::Io(synced as i32));
    }
    Ok(())
}

/// Shrink `path` to `size` bytes, freeing the blocks past the new end. A no-op if the
/// file is already that size or shorter — growing is [`create`]'s job.
fn truncate(ns: u64, path: &[u8], size: u64) -> Result<(), FileError> {
    // SAFETY: valid path slice + namespace handle.
    let po = unsafe {
        syscall5(
            SYS_FILE_TRUNCATE,
            ns,
            path.as_ptr() as u64,
            path.len() as u64,
            RIGHT_MAP_READ | RIGHT_MAP_WRITE,
            size,
        )
    };
    if po < 0 {
        return Err(FileError::Io(po as i32));
    }
    let (status, handle) = po_wait(po as u64);
    if handle != 0 {
        // The resolve hands back a file handle as a side effect; the caller only wanted
        // the size change, so close it rather than leak one per overwrite.
        // SAFETY: closing a handle just installed into our table.
        unsafe { syscall1(SYS_HANDLE_CLOSE, handle) };
    }
    if status != 0 {
        return Err(FileError::Io(status));
    }
    Ok(())
}

/// Create (or open, idempotently) `path` and grow it to `size`, returning a read-write
/// handle.
fn create(ns: u64, path: &[u8], size: u64) -> Result<u64, FileError> {
    // SAFETY: valid path slice + namespace handle.
    let po = unsafe {
        syscall5(
            SYS_FILE_CREATE,
            ns,
            path.as_ptr() as u64,
            path.len() as u64,
            RIGHT_MAP_READ | RIGHT_MAP_WRITE,
            size,
        )
    };
    if po < 0 {
        return Err(FileError::Io(po as i32));
    }
    let (status, handle) = po_wait(po as u64);
    if status != 0 || handle == 0 {
        return Err(FileError::Io(status));
    }
    Ok(handle)
}

/// Resolve `path` to a handle with `rights`.
fn lookup(ns: u64, path: &[u8], rights: u64) -> Result<u64, i32> {
    // SAFETY: valid path slice + namespace handle.
    let po = unsafe {
        syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights)
    };
    if po < 0 {
        return Err(po as i32);
    }
    let (status, handle) = po_wait(po as u64);
    if status != 0 || handle == 0 {
        return Err(if status != 0 { status } else { -1 });
    }
    Ok(handle)
}

/// Wait for a `PendingOperation`, returning `(status, result)` and closing it.
fn po_wait(po: u64) -> (i32, u64) {
    let handles = [po];
    let mut r = [0u8; 24];
    // SAFETY: valid handle array + result out-buffer for a single waiter.
    let waited =
        unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, r.as_mut_ptr() as u64, u64::MAX) };
    // SAFETY: closing the PO we own (a resolved handle is separate).
    unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
    if waited != 1 {
        return (-1, 0);
    }
    let status = i32::from_le_bytes([r[8], r[9], r[10], r[11]]);
    let result = u64::from_le_bytes([r[16], r[17], r[18], r[19], r[20], r[21], r[22], r[23]]);
    (status, result)
}

// --- path helpers -----------------------------------------------------------

/// The final component of `path` (`"/a/b/c"` → `"c"`), or the whole path if it has no
/// separator. A trailing separator is ignored (`"/a/b/"` → `"b"`), so a caller that writes
/// a directory with a trailing slash still gets a usable name.
pub fn basename(path: &[u8]) -> &[u8] {
    let end = match path.iter().rposition(|&c| c != b'/') {
        Some(i) => i + 1,
        None => return path, // all separators (e.g. "/"): nothing to take
    };
    match path[..end].iter().rposition(|&c| c == b'/') {
        Some(i) => &path[i + 1..end],
        None => &path[..end],
    }
}

/// Everything before the final component (`"/a/b/c"` → `"/a/b"`), or `"/"` when the path
/// has a single component — the root is its own parent, which is what a caller opening the
/// parent directory needs.
pub fn parent(path: &[u8]) -> &[u8] {
    let end = match path.iter().rposition(|&c| c != b'/') {
        Some(i) => i + 1,
        None => return b"/",
    };
    match path[..end].iter().rposition(|&c| c == b'/') {
        Some(0) => b"/",
        Some(i) => &path[..i],
        None => b".",
    }
}

/// Join a directory path and a name with a single separator.
pub fn join(dir: &[u8], name: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(dir).into_owned();
    if !s.ends_with('/') {
        s.push('/');
    }
    s.push_str(&String::from_utf8_lossy(name));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_takes_the_final_component() {
        assert_eq!(basename(b"/a/b/c"), b"c");
        assert_eq!(basename(b"file"), b"file");
        // A trailing separator must not yield an empty name — a caller would then build
        // "dir/" and create a nameless entry.
        assert_eq!(basename(b"/a/b/"), b"b");
        assert_eq!(basename(b"/"), b"/");
    }

    #[test]
    fn parent_drops_the_final_component() {
        assert_eq!(parent(b"/a/b/c"), b"/a/b");
        // A single-component absolute path's parent is the root, not the empty string:
        // opening "" as a directory would fail where "/" is what was meant.
        assert_eq!(parent(b"/system"), b"/");
        assert_eq!(parent(b"/a/b/"), b"/a");
        assert_eq!(parent(b"relative"), b".");
    }

    #[test]
    fn join_uses_exactly_one_separator() {
        assert_eq!(join(b"/a", b"b"), "/a/b");
        assert_eq!(join(b"/a/", b"b"), "/a/b");
        assert_eq!(join(b"/", b"system"), "/system");
    }
}
