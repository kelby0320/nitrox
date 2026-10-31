//! `libfs` — whole-file and path helpers, for anything that touches the filesystem.
//!
//! A file's contents bypass the directory protocol: a file resolves to a page-cache object the
//! process maps, so a copy is a `memcpy` between two mappings. The directory protocol itself is
//! [`librsproto::session::Dir`]; this crate owns no wire format, only namespace and memory
//! syscalls and the composition of directory answers ([`list_dir`]).

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use libkern::abi::{HandleInfo, NS_ENTRY_PATH_MAX, NS_KIND_MOUNT, NsEntry};
use libkern::error::KError;
use libkern::handle::{KOBJ_FILE_OBJECT, KOBJ_MEMORY_OBJECT, RIGHT_INSPECT, RIGHT_MAP_READ, RIGHT_MAP_WRITE};
use libkern::syscall::{
    SYS_FILE_CREATE, SYS_FILE_RENAME, SYS_FILE_SYNC, SYS_FILE_TRUNCATE, SYS_HANDLE_CLOSE, SYS_HANDLE_STAT,
    SYS_MEMORY_MAP, SYS_MEMORY_UNMAP, SYS_NS_ENUMERATE, SYS_NS_LOOKUP, SYS_WAIT, syscall1, syscall2, syscall3,
    syscall4, syscall5, syscall6,
};
use librsproto::file::{DIRENT_KIND_DIR, DIRENT_KIND_FILE};
use librsproto::file::{DirEntry, OwnedEntry};
use librsproto::namespace::RENAME_REPLACE;
use librsproto::session::{Dir, DirError};

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
    /// A [`rename`] across filesystems; the caller falls back to [`copy_file`] + unlink.
    CrossDevice,
    /// A syscall failed; the payload is its negative return.
    Io(i32),
}

/// The largest file copied in one mapping.
///
/// A file maps whole (`sys_memory_map` has no offset), so this bounds address space, not RAM; a
/// windowed copy would not lift it, as unmapping leaves the pages with the page-cache object.
/// A copy through `sys_io_submit` will (`copy-limit` in `deferred-decisions.md`).
pub const MAX_COPY: u64 = 8 * 1024 * 1024;

/// The size of the file `handle` names, or `None` if it does not stat as a file or memory
/// object; a directory's session endpoint stats with size 0.
fn file_handle_size(handle: u64) -> Option<u64> {
    let mut info = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
    // SAFETY: `info` is a valid, correctly sized `HandleInfo` out-param. (Never a
    // hand-sized byte array — see the 2026-07-24 stack-smash entry in the decision log.)
    let r = unsafe { syscall2(SYS_HANDLE_STAT, handle, (&raw mut info) as u64) };
    let file = matches!(info.object_type, KOBJ_FILE_OBJECT | KOBJ_MEMORY_OBJECT);
    if r == 0 && file { Some(info.size) } else { None }
}

/// Resolve `path` and return its size, or `None` if it does not resolve to a file (a directory
/// does not).
pub fn file_size(ns: u64, path: &[u8]) -> Option<u64> {
    let handle = lookup(ns, path, RIGHT_MAP_READ | RIGHT_INSPECT).ok()?;
    let size = file_handle_size(handle);
    // SAFETY: closing the handle we just resolved.
    unsafe { syscall1(SYS_HANDLE_CLOSE, handle) };
    size
}

/// Copy the regular file at `src` to `dst`, returning the bytes copied.
///
/// [`FileError::Exists`] if `dst` exists and `overwrite` is false. A longer `dst` is shrunk first,
/// else its old tail would survive.
pub fn copy_file(ns: u64, src: &[u8], dst: &[u8], overwrite: bool) -> Result<u64, FileError> {
    let src_handle = lookup(ns, src, RIGHT_MAP_READ | RIGHT_INSPECT).map_err(|_| FileError::NotFound)?;
    let result = copy_from(ns, src_handle, dst, overwrite);
    // SAFETY: closing the handle resolved above.
    unsafe { syscall1(SYS_HANDLE_CLOSE, src_handle) };
    result
}

/// [`copy_file`] from the file `src_handle` names, stated and mapped through that one handle.
fn copy_from(ns: u64, src_handle: u64, dst: &[u8], overwrite: bool) -> Result<u64, FileError> {
    let size = file_handle_size(src_handle).ok_or(FileError::NotFound)?;
    if size > MAX_COPY {
        return Err(FileError::TooLarge);
    }
    if let Some(existing) = file_size(ns, dst) {
        if !overwrite {
            return Err(FileError::Exists);
        }
        if existing > size {
            // Shrink before writing and verify: a silent truncate failure would keep the tail.
            truncate(ns, dst, size)?;
            if file_size(ns, dst) != Some(size) {
                return Err(FileError::TruncateFailed);
            }
        }
    }

    // A zero-length object cannot be mapped, but the destination must still exist.
    if size == 0 {
        let dst_handle = create(ns, dst, 0)?;
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, dst_handle) };
        return Ok(0);
    }

    let dst_handle = create(ns, dst, size)?;
    let result = map_copy(src_handle, dst_handle, size);
    // SAFETY: closing the handle `create` gave us.
    unsafe { syscall1(SYS_HANDLE_CLOSE, dst_handle) };
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
    let dst_addr = unsafe { syscall4(SYS_MEMORY_MAP, dst_handle, 0, size, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if dst_addr < 0 {
        // SAFETY: unmapping the mapping we just made.
        unsafe { syscall2(SYS_MEMORY_UNMAP, src_addr as u64, size) };
        return Err(FileError::Io(dst_addr as i32));
    }

    // SAFETY: `size` bytes are mapped at both addresses, and the regions are disjoint.
    unsafe {
        core::ptr::copy_nonoverlapping(src_addr as *const u8, dst_addr as *mut u8, size as usize);
    }

    // Unmap before the flush: a writable mapping still in place leaves the file dirty, and the
    // kernel keeps a dirty file's pages until the next flush.
    // SAFETY: unmapping our own mappings.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, dst_addr as u64, size);
        syscall2(SYS_MEMORY_UNMAP, src_addr as u64, size);
    }
    // SAFETY: `dst_handle` is our writable file handle.
    let synced = unsafe { syscall1(SYS_FILE_SYNC, dst_handle) };
    if synced != 0 {
        return Err(FileError::Io(synced as i32));
    }
    Ok(())
}

/// Shrink `path` to `size` bytes, a no-op if it is no longer; growing is [`create`]'s job.
fn truncate(ns: u64, path: &[u8], size: u64) -> Result<(), FileError> {
    // SAFETY: valid path slice + namespace handle.
    let po = unsafe {
        syscall5(SYS_FILE_TRUNCATE, ns, path.as_ptr() as u64, path.len() as u64, RIGHT_MAP_READ | RIGHT_MAP_WRITE, size)
    };
    if po < 0 {
        return Err(FileError::Io(po as i32));
    }
    let (status, handle) = po_wait(po as u64);
    if handle != 0 {
        // The resolve hands back a file handle we do not want; close it.
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
/// An empty file is `Ok(empty)`: there is nothing to map. Lives here rather than with its one
/// consumer because it is mechanism over this module's private primitives, not policy.
pub fn read_file(ns: u64, path: &[u8]) -> Result<Vec<u8>, FileError> {
    let handle = lookup(ns, path, RIGHT_MAP_READ | RIGHT_INSPECT).map_err(|_| FileError::NotFound)?;
    let size = match file_handle_size(handle) {
        Some(size) if size <= MAX_COPY => size,
        other => {
            // SAFETY: closing our own handle.
            unsafe { syscall1(SYS_HANDLE_CLOSE, handle) };
            return Err(if other.is_some() { FileError::TooLarge } else { FileError::NotFound });
        }
    };
    if size == 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, handle) };
        return Ok(Vec::new());
    }
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

/// `bytes` without the trailing NULs of a snapshot (sized in pages, so `/dev/log` is zero-padded).
/// For text only: a binary file may end in zeroes it means.
pub fn unpadded(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1)]
}

/// Write `bytes` to `path`, replacing whatever was there, shrinking first as [`copy_file`] does.
///
/// Not a safe save: a failure after the truncate leaves a file shorter than both versions.
/// [`save_file`] is one.
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
    if size == 0 {
        let handle = create(ns, path, 0)?;
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, handle) };
        return Ok(());
    }
    let handle = create(ns, path, size)?;
    // SAFETY: mapping an object created with MAP_READ | MAP_WRITE.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, handle, 0, size, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, handle) };
        return Err(FileError::Io(addr as i32));
    }
    // SAFETY: `size` bytes are mapped at `addr`, `bytes` holds exactly that many, and a caller's
    // slice cannot alias a mapping this call just made.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, size as usize) };
    // SAFETY: unmapping our own mapping.
    unsafe { syscall2(SYS_MEMORY_UNMAP, addr as u64, size) };
    // SAFETY: `handle` is our writable file handle.
    let synced = unsafe { syscall1(SYS_FILE_SYNC, handle) };
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, handle) };
    if synced != 0 {
        return Err(FileError::Io(synced as i32));
    }
    Ok(())
}

/// The suffix of [`save_file`]'s temporary. Beside the file it replaces, as a rename is only
/// atomic within one filesystem.
pub const SAVE_SUFFIX: &str = ".save-tmp";

/// Which step of [`save_file`] failed; either way the file is as it was.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SaveError {
    /// The temporary could not be written.
    Write(FileError),
    /// The temporary could not be renamed over the file.
    Replace(FileError),
}

/// Replace the file at `path` with `bytes` the safe way: a temporary beside it, written whole,
/// then renamed over it. A failure at either step leaves `path` as it was and removes the
/// temporary, so no file is left holding neither version.
pub fn save_file(ns: u64, path: &[u8], bytes: &[u8]) -> Result<(), SaveError> {
    let mut temp = path.to_vec();
    temp.extend_from_slice(SAVE_SUFFIX.as_bytes());
    if let Err(e) = write_file(ns, &temp, bytes) {
        let _ = unlink_at(ns, &temp);
        return Err(SaveError::Write(e));
    }
    if let Err(e) = rename(ns, &temp, path, true) {
        let _ = unlink_at(ns, &temp);
        return Err(SaveError::Replace(e));
    }
    Ok(())
}

/// Create `path` as an empty file if absent; idempotent, so it cannot report which happened.
pub fn create_file(ns: u64, path: &[u8]) -> Result<(), FileError> {
    let handle = create(ns, path, 0)?;
    // SAFETY: closing a handle this function just created and owns.
    unsafe { syscall1(SYS_HANDLE_CLOSE, handle) };
    Ok(())
}

/// Create (or open) `path`, grow it to `size`, and return a read-write handle.
fn create(ns: u64, path: &[u8], size: u64) -> Result<u64, FileError> {
    // SAFETY: valid path slice + namespace handle.
    let po = unsafe {
        syscall5(SYS_FILE_CREATE, ns, path.as_ptr() as u64, path.len() as u64, RIGHT_MAP_READ | RIGHT_MAP_WRITE, size)
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
/// A listing is the union of the filesystem under a path and the bindings beneath it; this is the
/// bindings half. A kernel server owning a subtree (`/dev/blk/<n>`) appears as one binding that
/// lists as empty; `/dev/devices` answers that, not this.
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
        let r = unsafe { syscall3(SYS_NS_ENUMERATE, ns, index, (&raw mut entry) as u64) };
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
        let (name, deeper) = match rest.split_once('/') {
            Some((head, _)) => (head.to_string(), true),
            None => (rest.to_string(), false),
        };
        // An intermediate component is a directory; a leaf is one only if it is a mount.
        let kind = if deeper || entry.kind == NS_KIND_MOUNT { DIRENT_KIND_DIR } else { DIRENT_KIND_FILE };
        if !out.iter().any(|(n, _)| *n == name) {
            out.push((name, kind));
        }
    }
    out
}

/// Rename `src` to `dst`, absolute namespace paths; `replace` unlinks an existing destination.
///
/// One syscall, atomic to readers. Only within one filesystem: elsewhere it returns
/// [`FileError::CrossDevice`], the cue to fall back to [`copy_file`] + unlink.
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

/// Map a cross-filesystem destination to `CrossDevice`. An occupied one is not distinguished: the
/// server reports it as `InvalidArgument`, as it does a malformed request; use [`file_size`] first.
fn map_rename_error(status: i32) -> FileError {
    match KError::from_i32(status) {
        KError::Unsupported => FileError::CrossDevice,
        KError::NotFound => FileError::NotFound,
        _ => FileError::Io(status),
    }
}

/// Resolve `path` to a handle with `rights`, returning `(status, handle)`; not only for files.
pub fn lookup_wait(ns: u64, path: &[u8], rights: u64) -> (i32, u64) {
    match lookup(ns, path, rights) {
        Ok(h) => (0, h),
        Err(e) => (e, 0),
    }
}

/// Resolve `path` to a handle with `rights`.
fn lookup(ns: u64, path: &[u8], rights: u64) -> Result<u64, i32> {
    // SAFETY: valid path slice + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
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
    let waited = unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, r.as_mut_ptr() as u64, u64::MAX) };
    // SAFETY: closing the PO we own (a resolved handle is separate).
    unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
    if waited != 1 {
        return (-1, 0);
    }
    let status = i32::from_le_bytes([r[8], r[9], r[10], r[11]]);
    let result = u64::from_le_bytes([r[16], r[17], r[18], r[19], r[20], r[21], r[22], r[23]]);
    (status, result)
}

/// Does `path` name a directory?
///
/// Answered by opening a session on it, which succeeds only on a directory; this avoids inferring
/// a type from an error code.
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

/// How a directory listing is ordered. An enum: the set is what a person can choose from.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Order {
    /// Name, ascending. The default.
    #[default]
    NameAsc,
    /// Name, descending.
    NameDesc,
    /// Oldest modification first.
    OldestFirst,
    /// Newest modification first.
    NewestFirst,
}

/// What [`sort`] needs to know about an entry.
///
/// A trait so one comparison serves `OwnedEntry` and a caller's own row type, without copying
/// names into a `Vec<OwnedEntry>` first.
pub trait Listed {
    /// The entry's name, with no path in it.
    fn sort_name(&self) -> &[u8];
    /// Whether it is a directory; decides the group, not the order within it.
    fn sort_is_dir(&self) -> bool;
    /// Modification time, or `0` where the server reports none.
    fn sort_mtime(&self) -> i64;
}

impl Listed for OwnedEntry {
    fn sort_name(&self) -> &[u8] {
        self.name()
    }
    fn sort_is_dir(&self) -> bool {
        self.kind == librsproto::file::DIRENT_KIND_DIR
    }
    fn sort_mtime(&self) -> i64 {
        self.mtime
    }
}

/// Sort `entries` in place, directories first (whatever the order) and then by `order`.
///
/// Ties break on name: `mtime` is `0` when a server reports none, and enumeration order would show.
pub fn sort<T: Listed>(entries: &mut [T], order: Order) {
    entries.sort_by(|a, b| {
        b.sort_is_dir().cmp(&a.sort_is_dir()).then_with(|| match order {
            Order::NameAsc => a.sort_name().cmp(b.sort_name()),
            Order::NameDesc => b.sort_name().cmp(a.sort_name()),
            Order::OldestFirst => a.sort_mtime().cmp(&b.sort_mtime()).then_with(|| a.sort_name().cmp(b.sort_name())),
            Order::NewestFirst => b.sort_mtime().cmp(&a.sort_mtime()).then_with(|| a.sort_name().cmp(b.sort_name())),
        })
    });
}

/// Every entry directly under `path`: the filesystem's, plus the namespace bindings mounted
/// there, with bindings shadowing same-named files, as a mount point shadows what it covers.
///
/// `Err` only when there is neither a filesystem here nor a binding beneath.
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
/// Trailing separators are ignored (`"/a/b/"` is `"b"`, not empty); `"/"` is itself.
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

/// [`basename`] on a `str`. The `from_utf8` cannot fail: `/` never occurs inside a multi-byte
/// sequence, so the subslice is still UTF-8 and the fallback is unreachable.
pub fn basename_str(path: &str) -> &str {
    core::str::from_utf8(basename(path.as_bytes())).unwrap_or(path)
}

/// Everything before the final component (`"/a/b/c"` → `"/a/b"`), or `"/"` for a single
/// component: the root is its own parent.
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

/// The folders a session's home holds for a person's own files, between Home and Root in
/// [`places`].
///
/// A place whose directory is absent is still offered; opening it says so, as a typed path does.
pub const HOME_FOLDERS: &[&str] = &["Documents", "Downloads", "Pictures"];

/// Somewhere a person goes often: what it is called, and the path it opens.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Place {
    /// What a person reads — `Home`, `Documents`, `Root`.
    pub name: &'static str,
    /// Where it goes.
    pub path: String,
}

/// The places for a session whose home is `home`: Home, the [`HOME_FOLDERS`] beneath it, and Root.
///
/// Built rather than stored: each is a function of `home`, so a held list could go stale.
pub fn places(home: &str) -> Vec<Place> {
    let mut out = alloc::vec![Place { name: "Home", path: String::from(home) }];
    for name in HOME_FOLDERS {
        out.push(Place { name, path: join(home.as_bytes(), name.as_bytes()) });
    }
    out.push(Place { name: "Root", path: String::from("/") });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A path is under a directory a component at a time**: `/abc` is not under `/ab`, and a
    /// copy of `/a` to `/a//b` would copy into itself as surely as to `/a/b`.
    #[test]
    fn a_path_is_within_a_directory_a_component_at_a_time() {
        assert!(within(b"/a/b", b"/a") && within(b"/a", b"/a") && within(b"/a//b/", b"/a/"));
        assert!(within(b"/a/./b/c", b"/a/b"));
        assert!(!within(b"/abc", b"/ab") && !within(b"/a", b"/a/b") && !within(b"/b/a", b"/a"));
    }

    /// The names a walk takes from a listing of `names`, or why it stops.
    fn walked(names: &[&[u8]], fails: bool) -> Result<Vec<String>, TreeError> {
        let listed = listing(|f| {
            for name in names {
                let e = DirEntry { inode: 1, kind: DIRENT_KIND_FILE, mode: 0, size: 0, mtime: 0, name };
                if !f(&e) {
                    break;
                }
            }
            if fails { Err(DirError::Protocol) } else { Ok(()) }
        });
        listed.map(|v| v.iter().map(|e| String::from_utf8_lossy(e.name()).into_owned()).collect())
    }

    /// **A listed name no entry can have stops a walk**, empty or holding a `/`: joined to the
    /// path it names another file than the one listed, which a copy or a removal would then take.
    #[test]
    fn a_listed_name_no_entry_can_have_stops_a_walk() {
        assert_eq!(walked(&[b".", b"..", b"a", b"b"], false), Ok(vec![String::from("a"), String::from("b")]));
        assert_eq!(walked(&[b"a", b"x/y", b"b"], false), Err(TreeError::Name));
        assert_eq!(walked(&[b"a", b""], false), Err(TreeError::Name));
        assert_eq!(walked(&[b"a"], true), Err(TreeError::ReadDir));
    }

    #[test]
    fn the_places_are_built_from_the_home_they_are_given() {
        let places = places("/home/someone-else");
        assert_eq!(places[0], Place { name: "Home", path: String::from("/home/someone-else") });
        assert_eq!(places.last().map(|p| (p.name, p.path.as_str())), Some(("Root", "/")));
        assert_eq!(places.len(), HOME_FOLDERS.len() + 2);
        for name in HOME_FOLDERS {
            let want = alloc::format!("/home/someone-else/{name}");
            assert!(
                places.iter().any(|p| p.name == *name && p.path == want),
                "{name} should be under this home, not under /home: {places:?}"
            );
        }
        assert_eq!(super::places("/home/")[1].path, "/home/Documents");
    }

    #[test]
    fn unpadded_trims_the_nuls_that_end_it() {
        assert_eq!(unpadded(b"a\nb\n\0\0\0"), b"a\nb\n");
        assert_eq!(unpadded(b"a\0b\0"), b"a\0b", "a NUL inside stays");
        assert_eq!(unpadded(b"a\nb"), b"a\nb", "nothing to trim");
        assert_eq!(unpadded(b"\0\0"), b"");
        assert_eq!(unpadded(b""), b"");
    }

    #[test]
    fn basename_takes_the_final_component() {
        assert_eq!(basename(b"/a/b/c"), b"c");
        assert_eq!(basename(b"file"), b"file");
        assert_eq!(basename(b"/a/b/"), b"b");
        assert_eq!(basename(b"/"), b"/");
    }

    #[test]
    fn basename_str_is_the_same_rule_and_never_loses_the_text() {
        assert_eq!(basename_str("/a/b/c"), "c");
        assert_eq!(basename_str("/a/b/"), "b", "a trailing separator is not the name");
        assert_eq!(basename_str("/"), "/");
        assert_eq!(basename_str("/home/notes-café.txt"), "notes-café.txt");
        assert_eq!(basename_str("café"), "café");
    }

    #[test]
    fn parent_drops_the_final_component() {
        assert_eq!(parent(b"/a/b/c"), b"/a/b");
        // The root, not the empty string: opening "" as a directory would fail.
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

    fn ent(name: &str, dir: bool, mtime: i64) -> OwnedEntry {
        let kind = if dir { librsproto::file::DIRENT_KIND_DIR } else { librsproto::file::DIRENT_KIND_FILE };
        let mut e = OwnedEntry::binding(name.as_bytes(), kind);
        e.mtime = mtime;
        e
    }

    fn names(v: &[OwnedEntry]) -> Vec<String> {
        v.iter().map(|e| String::from_utf8_lossy(e.name()).into_owned()).collect()
    }

    #[test]
    fn a_different_shape_takes_the_same_order() {
        struct Row {
            label: String,
            folder: bool,
            when: i64,
        }
        impl Listed for Row {
            fn sort_name(&self) -> &[u8] {
                self.label.as_bytes()
            }
            fn sort_is_dir(&self) -> bool {
                self.folder
            }
            fn sort_mtime(&self) -> i64 {
                self.when
            }
        }

        let fixture = [
            ("beta.txt", false, 10),
            ("alpha", true, 20),
            ("acme.txt", false, 30),
            ("zeta", true, 5),
            ("undated", false, 0),
        ];
        for order in [Order::NameAsc, Order::NameDesc, Order::OldestFirst, Order::NewestFirst] {
            let mut wire: Vec<OwnedEntry> = fixture.iter().map(|&(n, d, m)| ent(n, d, m)).collect();
            let mut rows: Vec<Row> =
                fixture.iter().map(|&(n, d, m)| Row { label: String::from(n), folder: d, when: m }).collect();
            sort(&mut wire, order);
            sort(&mut rows, order);
            let a = names(&wire);
            let b: Vec<String> = rows.iter().map(|r| r.label.clone()).collect();
            assert_eq!(a, b, "{order:?} ordered the two shapes differently");
        }
    }

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
        let mut v = alloc::vec![ent("b.txt", false, 200), ent("a.txt", false, 300), ent("c.txt", false, 100),];
        sort(&mut v, Order::NameAsc);
        assert_eq!(names(&v), ["a.txt", "b.txt", "c.txt"]);
        sort(&mut v, Order::NameDesc);
        assert_eq!(names(&v), ["c.txt", "b.txt", "a.txt"]);
        sort(&mut v, Order::OldestFirst);
        assert_eq!(names(&v), ["c.txt", "b.txt", "a.txt"]);
        sort(&mut v, Order::NewestFirst);
        assert_eq!(names(&v), ["a.txt", "b.txt", "c.txt"]);
    }

    #[test]
    fn an_undated_listing_is_still_ordered() {
        let mut v = alloc::vec![ent("c", false, 0), ent("a", false, 0), ent("b", false, 0)];
        sort(&mut v, Order::NewestFirst);
        assert_eq!(names(&v), ["a", "b", "c"], "a tie falls back to the name");
        sort(&mut v, Order::OldestFirst);
        assert_eq!(names(&v), ["a", "b", "c"]);
    }
}

/// Deepest tree either walk descends; a runaway guard, not a resource limit.
pub const MAX_TREE_DEPTH: u32 = 32;

/// Why a tree walk stopped; each names the step that failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TreeError {
    /// [`MAX_TREE_DEPTH`] exceeded.
    TooDeep,
    /// A directory session could not be opened.
    OpenDir,
    /// A directory could not be enumerated.
    ReadDir,
    /// A directory could not be created; the payload is the server's `KError`.
    MakeDir(i32),
    /// A file copy failed.
    Copy(FileError),
    /// An entry could not be unlinked.
    Unlink,
    /// A directory could not be removed once emptied; `NotEmpty` means an entry appeared mid-walk.
    Rmdir(i32),
    /// A copy's destination is its source or under it, where the walk would copy what it made.
    IntoItself,
    /// A listed name no single entry can have, empty or holding a `/`: joined to the path, it
    /// would name another file than the one listed.
    Name,
}

/// **Whether `path` is `dir` or under it**, a component at a time, so `/abc` is not under `/ab`
/// and `/a//b` is under `/a`. Textual: it resolves no `..`, which the kernel refuses in a path, and
/// takes no directory to be bound at two paths, which no namespace here does.
pub fn within(path: &[u8], dir: &[u8]) -> bool {
    fn parts(p: &[u8]) -> impl Iterator<Item = &[u8]> {
        p.split(|&c| c == b'/').filter(|c| !c.is_empty() && *c != b".")
    }
    let mut path = parts(path);
    parts(dir).all(|d| path.next() == Some(d))
}

/// Enumerate `path`'s children, closing the session before returning.
///
/// Filesystem entries only, not [`ns_children`]: a descent following a mount point would copy or
/// delete through it into another server's tree. Closing first keeps sessions (`MAX_SESSIONS`)
/// from capping the depth.
fn children(ns: u64, path: &[u8]) -> Result<Vec<OwnedEntry>, TreeError> {
    let mut buf = [0u8; 4096];
    let mut dir = Dir::open(ns, path, &mut buf).map_err(|_| TreeError::OpenDir)?;
    let listed = listing(|f| dir.read_dir(f).map(drop));
    dir.close();
    listed
}

/// The entries `list` gives, `.` and `..` left out; [`TreeError::Name`] for a name no entry has.
fn listing(
    list: impl FnOnce(&mut dyn FnMut(&DirEntry<'_>) -> bool) -> Result<(), DirError>,
) -> Result<Vec<OwnedEntry>, TreeError> {
    let mut entries: Vec<OwnedEntry> = Vec::new();
    let mut named = true;
    list(&mut |e| {
        named = !e.name.is_empty() && !e.name.contains(&b'/');
        if named && e.name != b"." && e.name != b".." {
            entries.push(OwnedEntry::from_entry(e));
        }
        named
    })
    .map_err(|_| TreeError::ReadDir)?;
    if named { Ok(entries) } else { Err(TreeError::Name) }
}

/// Copy `src` to `dst`, recursing if `src` is a directory; `on_file` gets `(src, dst, bytes)`.
///
/// Without `force` an existing destination is an error: merging into a tree should not be silent.
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
    if within(dst, src) {
        return Err(TreeError::IntoItself);
    }
    if !is_dir(ns, src) {
        let bytes = copy_file(ns, src, dst, force).map_err(TreeError::Copy)?;
        on_file(src, dst, bytes);
        return Ok(());
    }

    // `force` lets an existing destination directory through: this walk's rule, not `mkdir`'s.
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

/// Remove `path` and everything under it, depth-first; `on_entry` gets `(path, is_dir)`, children
/// first. The caller must refuse a `path` that is a namespace binding; this cannot tell.
pub fn remove_tree(ns: u64, path: &[u8], on_entry: &mut impl FnMut(&[u8], bool)) -> Result<(), TreeError> {
    remove_tree_at(ns, path, 0, on_entry)
}

fn remove_tree_at(ns: u64, path: &[u8], depth: u32, on_entry: &mut impl FnMut(&[u8], bool)) -> Result<(), TreeError> {
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
/// [`TreeError::MakeDir`] carries the server's `KError` (`0` if none).
pub fn mkdir(ns: u64, path: &[u8]) -> Result<(), TreeError> {
    let name = basename(path);
    if name.is_empty() {
        // The root has no parent to create it in; `basename` of `/` is empty.
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

/// `rmdir` the empty directory at `path`; `NotEmpty` after a descent means a concurrent mutator.
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
