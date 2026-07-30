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

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use librsproto::file::{DIRENT_KIND_DIR, DIRENT_KIND_FILE};
use libkern::abi::{HandleInfo, NS_ENTRY_PATH_MAX, NS_KIND_MOUNT, NsEntry};
use libkern::error::KError;
use libkern::handle::{RIGHT_INSPECT, RIGHT_MAP_READ, RIGHT_MAP_WRITE};
use libkern::syscall::{
    SYS_FILE_CREATE, SYS_FILE_RENAME, SYS_FILE_SYNC, SYS_FILE_TRUNCATE, SYS_HANDLE_CLOSE,
    SYS_HANDLE_STAT, SYS_MEMORY_MAP, SYS_MEMORY_UNMAP, SYS_NS_ENUMERATE, SYS_NS_LOOKUP,
    SYS_WAIT, syscall1, syscall2, syscall3, syscall4, syscall5, syscall6,
};
use librsproto::namespace::RENAME_REPLACE;
use librsproto::session::Dir;

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
    /// A [`rename`]'s destination lives on a different filesystem, so the kernel cannot do
    /// it as one operation. Not a failure to report as-is — the caller falls back to
    /// [`copy_file`] + unlink, which is what `mv` does across devices.
    CrossDevice,
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

/// Create `path` as an empty file if it is not already there, then release the handle.
///
/// The underlying create is idempotent, so this is safe to call on a path that exists —
/// but callers that care about the distinction should test first, because this cannot
/// report which happened.
pub fn create_file(ns: u64, path: &[u8]) -> Result<(), FileError> {
    let handle = create(ns, path, 0)?;
    // SAFETY: closing a handle this function just created and owns.
    unsafe { syscall1(SYS_HANDLE_CLOSE, handle) };
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

/// The namespace bindings immediately beneath `path`, as `(name, kind)` pairs.
///
/// A path's listing is the **union** of two things: whatever filesystem lies under it, and
/// the namespace bindings directly beneath it. That is not a special case bolted on for
/// `/dev` — it is how mount points have always appeared in a parent directory's listing.
/// Framing it as a union is what removes the question the plan posed ("how does `list`
/// choose between namespace enumeration and a directory session?"): it does not choose.
/// It asks both and merges, and each source answers for the part it owns.
///
/// `/dev` is then unremarkable — nothing is mounted there, so its listing is entirely
/// bindings (`entropy`, `blk`, `console`, `log`). `/system` is equally unremarkable in the
/// other direction: nothing is bound beneath it, so its listing is entirely files. `/` is
/// the interesting one, and the union is exactly right there — the root filesystem's own
/// entries plus the mount points and kernel servers bound alongside them.
///
/// Enumeration is local and cheap (no IPC — the kernel walks the caller's own namespace),
/// so doing it unconditionally costs a syscall loop over a handful of bindings.
///
/// **Limitation, stated rather than hidden:** a kernel server that owns a *subtree*
/// (`/dev/blk/<n>`) appears as one binding, so `blk` is reported as a directory that then
/// lists as empty. The kernel generates those children on demand and the namespace has no
/// way to ask "what would you serve?" — enumerating them needs a protocol that does not
/// exist yet.
pub fn ns_children(ns: u64, path: &[u8]) -> Vec<(String, u8)> {
    // `path` with exactly one trailing slash, so a prefix test is unambiguous.
    let mut base = String::from_utf8_lossy(path).into_owned();
    while base.ends_with('/') {
        base.pop();
    }
    base.push('/');

    let mut out: Vec<(String, u8)> = Vec::new();
    let mut entry = NsEntry::zeroed();
    for index in 0u64.. {
        // SAFETY: `entry` is a valid writable out-param of exactly `NsEntry`'s layout.
        let r = unsafe {
            syscall3(
                SYS_NS_ENUMERATE,
                ns,
                index,
                (&raw mut entry) as *mut NsEntry as u64,
            )
        };
        if r != 0 {
            break; // NotFound ends the walk
        }
        let len = (entry.path_len as usize).min(NS_ENTRY_PATH_MAX);
        let bound = String::from_utf8_lossy(&entry.path[..len]).into_owned();
        let Some(rest) = bound.strip_prefix(&base) else {
            continue;
        };
        if rest.is_empty() {
            continue; // the binding *is* this path, not a child of it
        }
        // The first component, and whether the binding continues past it.
        let (name, deeper) = match rest.split_once('/') {
            Some((head, _)) => (head.to_string(), true),
            None => (rest.to_string(), false),
        };
        // An intermediate component is a directory by construction; a leaf binding is a
        // directory only if it is a filesystem mount.
        let kind = if deeper || entry.kind == NS_KIND_MOUNT {
            DIRENT_KIND_DIR
        } else {
            DIRENT_KIND_FILE
        };
        if !out.iter().any(|(n, _)| *n == name) {
            out.push((name, kind));
        }
    }
    out
}

/// Rename `src` to `dst`, both absolute namespace paths. With `replace`, an existing
/// destination is unlinked as part of the rename; without it, an existing destination
/// fails.
///
/// This is the whole operation in one syscall — no data moves and nothing is mapped, so it
/// is atomic from a reader's point of view in a way copy-then-unlink is not. It only works
/// **within one filesystem**: a destination under a different binding returns
/// [`FileError::CrossDevice`], which is a caller's cue to fall back to
/// [`copy_file`] + unlink rather than an error to report.
pub fn rename(ns: u64, src: &[u8], dst: &[u8], replace: bool) -> Result<(), FileError> {
    let flags = if replace { RENAME_REPLACE as u64 } else { 0 };
    // SAFETY: two valid path slices + a namespace handle.
    let po = unsafe {
        syscall6(
            SYS_FILE_RENAME,
            ns,
            src.as_ptr() as u64,
            src.len() as u64,
            dst.as_ptr() as u64,
            dst.len() as u64,
            flags,
        )
    };
    if po < 0 {
        return Err(map_rename_error(po as i32));
    }
    // Status-only: a rename resolves to no object, so a nonzero handle would be a bug.
    let (status, _) = po_wait(po as u64);
    if status != 0 {
        return Err(map_rename_error(status));
    }
    Ok(())
}

/// Pick out the one rename failure a caller acts on differently — a cross-filesystem
/// destination, which means "fall back to copy + unlink", not "report an error".
///
/// An *occupied* destination is deliberately not distinguished here: the server maps it to
/// `InvalidArgument`, which a malformed request also produces, so a caller that wants to
/// refuse an existing destination should test for it with [`file_size`] first (which is
/// what `copy` does) rather than infer it from this status.
fn map_rename_error(status: i32) -> FileError {
    match KError::from_i32(status) {
        KError::Unsupported => FileError::CrossDevice,
        KError::NotFound => FileError::NotFound,
        _ => FileError::Io(status),
    }
}

/// Resolve `path` to a handle with `rights`, returning `(status, handle)`.
///
/// The public form of [`lookup`], for callers that resolve something which is not a file
/// — `whoami` reading the session's `/session/user` binding, for instance. A binding may
/// be a direct handle or a userspace server that answers with one; this cannot tell, and
/// deliberately does not need to.
pub fn lookup_wait(ns: u64, path: &[u8], rights: u64) -> (i32, u64) {
    match lookup(ns, path, rights) {
        Ok(h) => (0, h),
        Err(e) => (e, 0),
    }
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
/// Does `path` name a directory?
///
/// Answered by *opening a session on it*, because that is the only question the
/// directory protocol answers directly: a session opens on a directory and fails on
/// anything else. Cheap enough for the uses here (one open + close), and it avoids
/// inferring a type from an error code — see the note on `FsError::Exists` and
/// `FsError::NotEmpty` both arriving as `InvalidArgument`.
pub fn is_dir(ns: u64, path: &[u8]) -> bool {
    let mut buf = [0u8; 4096];
    match Dir::open(ns, path, &mut buf) {
        Ok(d) => {
            d.close();
            true
        }
        Err(_) => false,
    }
}

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
