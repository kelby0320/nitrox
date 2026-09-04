//! `libfs` — whole-file and path helpers, for anything that touches the filesystem.
//!
//! **Whole-file operations, and the composition of directory ones.** A file's *contents* do not
//! go through the directory protocol at all: a file resolves to a **page-cache object** the
//! process maps, so copying one is a `memcpy` between two mappings and the kernel moves the data
//! (Model A; `docs/architecture/filesystem-data-path.md`). That half is the bulk of this crate.
//!
//! The directory *protocol* is [`librsproto::session::Dir`] and stays there, beside the wire
//! format it speaks. What lives here is the rule for putting its answers together —
//! [`list_dir`], where a path's entries are the filesystem's **plus** the namespace bindings
//! mounted there, with bindings shadowing. That is not a protocol operation; it is a decision
//! about two sources, and it had two consumers (PR #257 review, finding 6, which caught this
//! paragraph still claiming the crate held no directory operations at all).
//!
//! Not in `librsproto` either way, because this crate owns no wire format: what is below is
//! namespace and memory syscalls, and composition on top of somebody else's protocol.
//!
//! ## Why it is a crate rather than a module in `coreutils`
//!
//! It was `coreutils::fs` until M10 Part A, and moved when a **second consumer** arrived.
//! `coreutils`' library is otherwise shell-program infrastructure — the Tier-1 stage
//! prologue, GNU-style argument parsing, TSM1 stdout plumbing — and a graphical file
//! browser wants the filesystem half and none of that. One application depending on
//! another application's crate to reach it is the wrong shape, and copying the helpers is
//! the shape that produces two implementations of `rename`.
//!
//! **Moved rather than rewritten**, deliberately: every function below is the code the
//! coreutils have been running since Milestone 3.5, and the gates that already drive that code
//! are what a move has to survive. **Which gate is which matters, so it is spelled out**:
//! `test-qemu` runs `copy`, `move`, `rename` and `remove` against a real filesystem through
//! `test-harness`'s demos, and `test-interactive` types `list` at a real prompt. An earlier
//! version of this note credited `test-interactive` with all four — it types none of the other
//! three — which would have sent somebody editing `copy_tree` to the gate that cannot see it
//! (PR #256 review, blocking 1).

#![cfg_attr(not(test), no_std)]

extern crate alloc;

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
use librsproto::file::OwnedEntry;
use librsproto::session::{Dir, DirError};

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

/// Read the whole file at `path`.
///
/// **The other half of [`copy_file`], for a caller whose destination is memory.** A file
/// resolves to a page-cache object and the read is a `memcpy` out of a mapping of it — there is
/// no read syscall to make and no offset to track, which is Model A's whole point.
///
/// Empty files are `Ok(empty)` rather than an error: `sys_memory_map` has no meaning for a
/// zero-length object, so there is nothing to map and nothing to copy, and an editor opening a
/// file `touch` just made is the ordinary case rather than an edge one.
///
/// **In `libfs` rather than in its one consumer**, which is worth stating because the rule that
/// moved this crate out of `coreutils` says the opposite: a helper with one consumer belongs to
/// that consumer. The rule is about *policy* — where a decision lives — and this is mechanism
/// whose primitives (`lookup`, `create`, `truncate`, the PO wait) are private to this module. A
/// copy outside it would be a second implementation of the mapping dance, which is the thing
/// the rule exists to prevent.
pub fn read_file(ns: u64, path: &[u8]) -> Result<Vec<u8>, FileError> {
    let size = file_size(ns, path).ok_or(FileError::NotFound)?;
    if size > MAX_COPY {
        return Err(FileError::TooLarge);
    }
    if size == 0 {
        return Ok(Vec::new());
    }
    let handle = lookup(ns, path, RIGHT_MAP_READ | RIGHT_INSPECT).map_err(FileError::Io)?;
    // SAFETY: mapping an object we hold a handle to, with a right it carries.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, handle, 0, size, RIGHT_MAP_READ) };
    if addr < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, handle) };
        return Err(FileError::Io(addr as i32));
    }
    let mut out = alloc::vec![0u8; size as usize];
    // SAFETY: `size` bytes are mapped at `addr`, and `out` is a fresh allocation of exactly
    // that length, so the ranges are distinct.
    unsafe {
        core::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), size as usize);
        syscall2(SYS_MEMORY_UNMAP, addr as u64, size);
        syscall1(SYS_HANDLE_CLOSE, handle);
    }
    Ok(out)
}

/// Write `bytes` to `path`, replacing whatever was there.
///
/// **Shrinks before it writes**, for [`copy_file`]'s reason and with the same verification: a
/// file grown to a smaller size is a no-op, so without the truncate the old tail would survive
/// past the new content — a file that is neither what it was nor what it was meant to become.
///
/// **This is not a safe save on its own**, and no caller should treat it as one: it truncates
/// the destination, so a failure between the truncate and the flush leaves a file shorter than
/// both versions. The safe sequence is to write a temporary and [`rename`] it over the target,
/// which is atomic on the server side — see `nxedit`, which does exactly that and is the reason
/// this exists.
pub fn write_file(ns: u64, path: &[u8], bytes: &[u8]) -> Result<(), FileError> {
    let size = bytes.len() as u64;
    if size > MAX_COPY {
        return Err(FileError::TooLarge);
    }
    if let Some(existing) = file_size(ns, path)
        && existing > size
    {
        truncate(ns, path, size)?;
        if file_size(ns, path) != Some(size) {
            return Err(FileError::TruncateFailed);
        }
    }
    // An empty file still has to come into existence; there is nothing to map.
    if size == 0 {
        let handle = create(ns, path, 0)?;
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, handle) };
        return Ok(());
    }
    let handle = create(ns, path, size)?;
    // SAFETY: mapping an object created with MAP_READ | MAP_WRITE.
    let addr = unsafe {
        syscall4(SYS_MEMORY_MAP, handle, 0, size, RIGHT_MAP_READ | RIGHT_MAP_WRITE)
    };
    if addr < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, handle) };
        return Err(FileError::Io(addr as i32));
    }
    // SAFETY: `size` bytes are mapped at `addr`, `bytes` holds exactly that many, and a caller's
    // slice cannot alias a mapping this call just made.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, size as usize) };
    // Flush before dropping the mapping, or the write lives only in the page cache and a reader
    // that re-resolves the path sees a short file — `copy_file`'s hard-won line.
    // SAFETY: `handle` is our writable file handle.
    let synced = unsafe { syscall1(SYS_FILE_SYNC, handle) };
    // SAFETY: unmapping our own mapping and closing our own handle.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, addr as u64, size);
        syscall1(SYS_HANDLE_CLOSE, handle);
    }
    if synced != 0 {
        return Err(FileError::Io(synced as i32));
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

/// How a directory listing is ordered.
///
/// **Here rather than in each viewer** (M14 decision 3). Two consumers order the same listing —
/// the browser and the file chooser — and sorting in two places is how two directory views come
/// to disagree about what "newest" means. An enum rather than a comparator because the set is the
/// one a person can choose from, not an open extension point.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Order {
    /// Name, ascending. The default, and the only order stable under everything but a rename.
    #[default]
    NameAsc,
    /// Name, descending.
    NameDesc,
    /// Oldest modification first.
    OldestFirst,
    /// Newest modification first.
    NewestFirst,
}

/// Sort `entries` in place, **directories first** and then by `order`.
///
/// **Directories first is not part of the order**, and that is deliberate: it is a fact about
/// reading a listing rather than something somebody chose, and every file manager does it
/// whichever sort is selected. Reversing the *name* order does not put files above directories.
///
/// **A tie breaks on name, always.** `mtime` is `0` when a server does not report one, and a
/// directory of zeroes ordered by "newest" would otherwise come back in whatever order the
/// enumeration produced — not an order anybody chose, and one that would appear to shuffle
/// between listings of the same directory.
pub fn sort(entries: &mut [OwnedEntry], order: Order) {
    entries.sort_by(|a, b| {
        let dir = |e: &OwnedEntry| e.kind == librsproto::file::DIRENT_KIND_DIR;
        dir(b).cmp(&dir(a)).then_with(|| match order {
            Order::NameAsc => a.name().cmp(b.name()),
            Order::NameDesc => b.name().cmp(a.name()),
            Order::OldestFirst => a.mtime.cmp(&b.mtime).then_with(|| a.name().cmp(b.name())),
            Order::NewestFirst => b.mtime.cmp(&a.mtime).then_with(|| a.name().cmp(b.name())),
        })
    });
}

/// Every entry directly under `path`: the filesystem's, plus the namespace bindings mounted
/// there, with bindings shadowing same-named files.
///
/// **A path's listing is a union, and that is not a policy choice — it is how mount points
/// appear in a parent's listing.** The filesystem under a path and the namespace bindings
/// directly beneath it are two sources for one question, so this asks both and lets each answer
/// for the part it owns: `/dev` is all bindings and no filesystem, `/system` is the other way,
/// and `/` genuinely needs both. A binding shadows a same-named filesystem entry exactly as a
/// mount point shadows the directory it covers.
///
/// `Err` only when there is neither — no filesystem here *and* nothing bound beneath. A
/// kernel-served directory like `/dev` is an ordinary success with the bindings as the whole
/// answer.
///
/// **Moved out of `list` in M10 Part B**, when the file browser became the second consumer. The
/// union rule is the kind that gets re-derived slightly differently by whoever needs it next —
/// and a browser that forgot the shadowing would show a mount point twice, once as the directory
/// it covers (M10 Part B; the rule is `userspace/CLAUDE.md`'s).
pub fn list_dir(ns: u64, path: &[u8]) -> Result<Vec<OwnedEntry>, DirError> {
    let ns_entries = ns_children(ns, path);

    let mut entries: Vec<OwnedEntry> = Vec::new();
    let mut buf = [0u8; 4096];
    match Dir::open(ns, path, &mut buf) {
        Ok(mut dir) => {
            let r = dir.read_dir(|e| {
                if e.name != b"." && e.name != b".." {
                    entries.push(OwnedEntry::from_entry(e));
                }
                true
            });
            dir.close();
            r?;
        }
        Err(e) => {
            if ns_entries.is_empty() {
                return Err(e);
            }
        }
    }

    for (name, kind) in &ns_entries {
        entries.retain(|e| e.name() != name.as_bytes());
        entries.push(OwnedEntry::binding(name.as_bytes(), *kind));
    }
    Ok(entries)
}

/// The final component of a path (`"/a/b/c"` → `"c"`).
///
/// **Trailing separators are ignored, and that is the rule worth stating**: `"/a/b/"` is `"b"`,
/// not the empty string. A caller that took the empty name would build `"dir/"` and ask a
/// directory server to create an entry with no name. `"/"` is itself, since there is no
/// component to take.
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

/// [`basename`] on a `str`, for the callers whose paths are already text.
///
/// **Not a second rule, and that is the point of it being here.** The two graphical
/// applications each grew a private copy — byte-identical, each with a comment explaining that
/// this half of the application never sees a path as bytes, which was true when there was one
/// of them (PR #259 review, optional 6). One consumer's helper belongs to that consumer; two
/// belong below both, which is the rule this crate itself was extracted under.
///
/// The conversion is free rather than lossy: a `str` is UTF-8, `/` never occurs inside a
/// multi-byte sequence, and the bytes handed back are a subslice cut only at one — so the
/// `from_utf8` **cannot fail**, and the fallback below is unreachable rather than a repair.
/// Changing what it returns changes no test, which is the evidence for that claim rather than a
/// gap in the tests: what the test pins is that a non-ASCII name comes back whole.
pub fn basename_str(path: &str) -> &str {
    core::str::from_utf8(basename(path.as_bytes())).unwrap_or(path)
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
    fn basename_str_is_the_same_rule_and_never_loses_the_text() {
        assert_eq!(basename_str("/a/b/c"), "c");
        assert_eq!(basename_str("/a/b/"), "b", "a trailing separator is not the name");
        assert_eq!(basename_str("/"), "/");
        // **The claim worth pinning**: the byte version can only cut at an ASCII `/`, and a `/`
        // byte never occurs inside a multi-byte UTF-8 sequence — so the subslice it hands back is
        // always still text, and the fallback in `basename_str` is unreachable rather than a
        // silent repair. A name that is not ASCII is what would expose the difference.
        assert_eq!(basename_str("/home/notes-café.txt"), "notes-café.txt");
        assert_eq!(basename_str("café"), "café");
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

    // --- ordering a listing (M14 Part C) ------------------------------------

    /// An entry with a name, a kind and an mtime — everything `sort` reads.
    fn ent(name: &str, dir: bool, mtime: i64) -> OwnedEntry {
        let kind = if dir {
            librsproto::file::DIRENT_KIND_DIR
        } else {
            librsproto::file::DIRENT_KIND_FILE
        };
        let mut e = OwnedEntry::binding(name.as_bytes(), kind);
        e.mtime = mtime;
        e
    }

    fn names(v: &[OwnedEntry]) -> Vec<String> {
        v.iter().map(|e| String::from_utf8_lossy(e.name()).into_owned()).collect()
    }

    /// Directories come first whatever the order, including the reversed one.
    ///
    /// **The reversal is the case worth writing.** "Z–A" reverses the *names*; a comparison that
    /// reversed the whole key would put files above directories, which no file manager does and
    /// which reads as the listing having changed shape rather than order.
    #[test]
    fn directories_lead_every_order() {
        let mut v = alloc::vec![
            ent("beta.txt", false, 10),
            ent("alpha", true, 20),
            ent("acme.txt", false, 30),
            ent("zeta", true, 5),
        ];
        for order in [Order::NameAsc, Order::NameDesc, Order::OldestFirst, Order::NewestFirst] {
            sort(&mut v, order);
            let got = names(&v);
            assert!(
                got[..2].iter().all(|n| n == "alpha" || n == "zeta"),
                "{order:?} put a file above a directory: {got:?}"
            );
        }
    }

    #[test]
    fn the_four_orders_are_the_four_orders() {
        let mut v = alloc::vec![
            ent("b.txt", false, 200),
            ent("a.txt", false, 300),
            ent("c.txt", false, 100),
        ];
        sort(&mut v, Order::NameAsc);
        assert_eq!(names(&v), ["a.txt", "b.txt", "c.txt"]);
        sort(&mut v, Order::NameDesc);
        assert_eq!(names(&v), ["c.txt", "b.txt", "a.txt"]);
        sort(&mut v, Order::OldestFirst);
        assert_eq!(names(&v), ["c.txt", "b.txt", "a.txt"]);
        sort(&mut v, Order::NewestFirst);
        assert_eq!(names(&v), ["a.txt", "b.txt", "c.txt"]);
    }

    /// A whole directory with no `mtime` still comes back in a *stable* order.
    ///
    /// **`0` is what a server reports when it does not know**, and it is common rather than
    /// exotic — the namespace half of a listing has no mtime at all. Ordering by date with every
    /// key equal would otherwise leave the enumeration's own order showing through, which is not
    /// an order anybody chose and appears to shuffle between listings of the same directory.
    #[test]
    fn an_undated_listing_is_still_ordered() {
        let mut v = alloc::vec![ent("c", false, 0), ent("a", false, 0), ent("b", false, 0)];
        sort(&mut v, Order::NewestFirst);
        assert_eq!(names(&v), ["a", "b", "c"], "a tie falls back to the name");
        sort(&mut v, Order::OldestFirst);
        assert_eq!(names(&v), ["a", "b", "c"]);
    }
}

// --- recursive tree walks ---------------------------------------------------
//
// `copy`, `remove` and `move` all walk trees, and they walked three separate copies of
// the same loop until `move` needed the recursive cross-mount case (2026-07-30). The
// walks live here now, one each, because the parts that are easy to get subtly wrong are
// not the loop — they are the session discipline and what the descent is allowed to see,
// and neither should be re-derived per utility.

/// Deepest tree either walk will descend. Not a resource limit — a runaway guard, since
/// a cycle cannot occur (no hard links to directories, no symlinks) but a bug could.
pub const MAX_TREE_DEPTH: u32 = 32;

/// Why a tree walk stopped. Each names the step that failed, so a caller can report
/// something truer than "it went wrong" without inspecting the tree itself.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TreeError {
    /// [`MAX_TREE_DEPTH`] exceeded.
    TooDeep,
    /// A directory session could not be opened.
    OpenDir,
    /// A directory could not be enumerated.
    ReadDir,
    /// A directory could not be created. The payload is the server's `KError`, so a caller
    /// can tell an occupied name from anything else without a confirming round trip — the
    /// same reason [`TreeError::Rmdir`] carries one.
    MakeDir(i32),
    /// A file copy failed.
    Copy(FileError),
    /// An entry could not be unlinked.
    Unlink,
    /// A directory could not be removed once emptied. `NotEmpty` here means something
    /// added an entry during the walk — see `remove`'s handling.
    Rmdir(i32),
}

/// Enumerate `path`'s children, closing the session before returning.
///
/// **Filesystem entries only — deliberately not [`ns_children`].** A binding beneath a
/// path is a mount point, not content: a descent that followed one would copy or delete
/// *through* a mount into another server's tree. Every recursive walker in the coreutils
/// depends on this being the only enumeration it does.
///
/// The session is closed before the caller recurses because a session is a scarce server
/// resource (`MAX_SESSIONS`); holding one per level would cap the depth at the session
/// table rather than at [`MAX_TREE_DEPTH`].
fn children(ns: u64, path: &[u8]) -> Result<Vec<OwnedEntry>, TreeError> {
    let mut entries: Vec<OwnedEntry> = Vec::new();
    let mut buf = [0u8; 4096];
    let mut dir = Dir::open(ns, path, &mut buf).map_err(|_| TreeError::OpenDir)?;
    let r = dir.read_dir(|e| {
        if e.name != b"." && e.name != b".." {
            entries.push(OwnedEntry::from_entry(e));
        }
        true
    });
    dir.close();
    r.map_err(|_| TreeError::ReadDir)?;
    Ok(entries)
}

/// Copy `src` to `dst`, recursing if `src` is a directory.
///
/// `on_file` is called for each **file** copied, with `(src, dst, bytes)` — directories
/// are structure, not content, and a caller that wants to report them can see them in the
/// paths. `force` permits copying *into* an existing destination directory; without it a
/// pre-existing destination is an error, because merging into someone else's tree is
/// exactly the surprise a fail-loud default should not spring.
pub fn copy_tree(
    ns: u64,
    src: &[u8],
    dst: &[u8],
    force: bool,
    on_file: &mut impl FnMut(&[u8], &[u8], u64),
) -> Result<(), TreeError> {
    copy_tree_at(ns, src, dst, force, 0, on_file)
}

fn copy_tree_at(
    ns: u64,
    src: &[u8],
    dst: &[u8],
    force: bool,
    depth: u32,
    on_file: &mut impl FnMut(&[u8], &[u8], u64),
) -> Result<(), TreeError> {
    if depth > MAX_TREE_DEPTH {
        return Err(TreeError::TooDeep);
    }
    if !is_dir(ns, src) {
        let bytes = copy_file(ns, src, dst, force).map_err(TreeError::Copy)?;
        on_file(src, dst, bytes);
        return Ok(());
    }

    // Create the destination directory. **Through [`mkdir`] rather than open-coded**: this was
    // the same three lines a third time — open the parent, ask it for the basename, close — and
    // the browser's *new folder* made it a fourth (M12 Part B). An existing directory is allowed
    // through under `force`, which is this walk's rule rather than `mkdir`'s.
    if let Err(e) = mkdir(ns, dst)
        && !(force && is_dir(ns, dst))
    {
        return Err(e);
    }

    for e in &children(ns, src)? {
        let child_src = join(src, e.name());
        let child_dst = join(dst, e.name());
        copy_tree_at(ns, child_src.as_bytes(), child_dst.as_bytes(), force, depth + 1, on_file)?;
    }
    Ok(())
}

/// Remove `path` and everything under it, depth-first.
///
/// `on_entry` is called for each entry removed, with `(path, is_dir)`, in the order they
/// go — children before their parent, which is the order the removal actually happens and
/// therefore the order a report should show.
///
/// The caller is responsible for refusing a `path` that is a **namespace binding**: this
/// walks what is beneath a path, and cannot tell whether the path itself was handed to it
/// by mistake. See `remove`'s operand check.
pub fn remove_tree(
    ns: u64,
    path: &[u8],
    on_entry: &mut impl FnMut(&[u8], bool),
) -> Result<(), TreeError> {
    remove_tree_at(ns, path, 0, on_entry)
}

fn remove_tree_at(
    ns: u64,
    path: &[u8],
    depth: u32,
    on_entry: &mut impl FnMut(&[u8], bool),
) -> Result<(), TreeError> {
    if depth > MAX_TREE_DEPTH {
        return Err(TreeError::TooDeep);
    }
    for e in &children(ns, path)? {
        let child = join(path, e.name());
        if e.kind == DIRENT_KIND_DIR {
            remove_tree_at(ns, child.as_bytes(), depth + 1, on_entry)?;
        } else {
            unlink_at(ns, child.as_bytes())?;
            on_entry(child.as_bytes(), false);
        }
    }
    rmdir_at(ns, path)?;
    on_entry(path, true);
    Ok(())
}

/// Create the directory named by `path`, via its parent's session.
///
/// **The sibling of [`unlink_at`]**, and it moved down here when a second consumer arrived
/// (M12 Part B): `mkdir`'s `make_one` was the only caller until `nxfiles` grew a *new folder*
/// command, and a browser open-coding the same three lines would be the second implementation
/// of "which directory does this name live in" — which is the rule `libfs` exists to keep in one
/// place. The `--parents` walk stays in `mkdir`, because creating intermediates is that
/// program's flag rather than a fact about making a directory.
///
/// [`TreeError::MakeDir`] carries the server's `KError`, so a caller can tell an occupied name from
/// anything else without a confirming round trip — the same reason [`rmdir_at`] carries one.
pub fn mkdir(ns: u64, path: &[u8]) -> Result<(), TreeError> {
    let name = basename(path);
    if name.is_empty() {
        // The root has no parent to create it in, and `basename` of `/` is empty rather than
        // an error — so this is the one path that cannot be made.
        return Err(TreeError::MakeDir(0));
    }
    let mut buf = [0u8; 4096];
    let mut dir = Dir::open(ns, parent(path), &mut buf).map_err(|_| TreeError::OpenDir)?;
    let r = dir.mkdir(name);
    dir.close();
    match r {
        Ok(()) => Ok(()),
        Err(DirError::Server(k)) => Err(TreeError::MakeDir(k)),
        Err(_) => Err(TreeError::MakeDir(0)),
    }
}

/// `unlink` the entry named by `path`, via its parent's session.
pub fn unlink_at(ns: u64, path: &[u8]) -> Result<(), TreeError> {
    let mut buf = [0u8; 4096];
    let mut dir = Dir::open(ns, parent(path), &mut buf).map_err(|_| TreeError::OpenDir)?;
    let r = dir.unlink(basename(path));
    dir.close();
    r.map_err(|_| TreeError::Unlink)
}

/// `rmdir` the (empty) directory named by `path`, via its parent's session. The payload
/// on failure is the server's `KError`, so a caller can tell `NotEmpty` — which after a
/// completed descent means a concurrent mutator — from anything else.
fn rmdir_at(ns: u64, path: &[u8]) -> Result<(), TreeError> {
    let mut buf = [0u8; 4096];
    let mut dir = Dir::open(ns, parent(path), &mut buf).map_err(|_| TreeError::OpenDir)?;
    let r = dir.rmdir(basename(path));
    dir.close();
    match r {
        Ok(()) => Ok(()),
        Err(DirError::Server(k)) => Err(TreeError::Rmdir(k)),
        Err(_) => Err(TreeError::Rmdir(0)),
    }
}
