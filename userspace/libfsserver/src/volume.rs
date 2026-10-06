//! **The filesystem, as the protocol sees it** (Phase 6 Part E.1): what a server's library must do
//! for the server loop, whatever the filesystem.
//!
//! A server implements [`Volume`] over its library and a device — the read-write [`Disk`], or the
//! [`ReadOnly`] over it a read-only mount is served through — and hands it to
//! [`crate::server::run`]. The methods are the calls `fs-server-ext4`'s loop made into its library
//! before the loop moved here.
//!
//! [`Disk`]: crate::disk::Disk
//! [`ReadOnly`]: crate::ReadOnly

use crate::{BlockRun, FsError};

/// **Why a device cannot be served**, said in place of `Meta::Ready`: its text for a person, and
/// what it means to a client.
pub trait Refusal: core::fmt::Display {
    /// The error a client is told.
    fn fs_error(&self) -> FsError;
}

/// **A file's map**, as a resolve replies it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Mapped {
    /// The file's size in bytes.
    pub size: usize,
    /// The unit of the map's runs, in bytes: what the kernel multiplies a run's `device_lba` by.
    pub block_size: u32,
    /// How many runs were written to the caller's buffer.
    pub runs: usize,
    /// **The file's id**, which the kernel keeps one page-cache object per: ext4's inode number.
    /// `0` is none, and the kernel gives such a file an object of its own, uncached.
    pub id: u64,
}

/// **One entry a directory listing yields**, as `File::ReadDir` carries it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DirEntry<'a> {
    /// The entry's id on its filesystem, informational: ext4's inode number.
    pub id: u32,
    /// One of `librsproto::file::DIRENT_KIND_*`.
    pub kind: u8,
    /// Permission and format bits, in POSIX `st_mode` encoding.
    pub mode: u16,
    /// The entry's size in bytes.
    pub size: u64,
    /// Modification time, seconds since the Unix epoch; `0` if unknown.
    pub mtime: i64,
    /// The entry's name.
    pub name: &'a [u8],
}

/// **A filesystem a server serves.** Every path is absolute within the filesystem — `/` and the
/// suffix the kernel forwarded — and every `now` is the server's own clock, which a client never
/// supplies. A directory id is what [`Volume::resolve_dir`] answered, and a file id what
/// [`Volume::map_file`] did.
pub trait Volume {
    /// The server's name, as `Meta::Ready` carries it and its own log lines begin: `fs-server-ext4`.
    const NAME: &'static [u8];
    /// The filesystem's kind, as the server's ready line says it: `ext4`.
    const KIND: &'static [u8];
    /// Why a device cannot be served.
    type Unservable: Refusal;

    /// Whether this is a read-only mount: served through [`crate::ReadOnly`].
    fn read_only(&self) -> bool;
    /// **Whether there is a filesystem here to serve**, checked before `Ready` is said.
    fn check(&self) -> Result<(), Self::Unservable>;
    /// The refusal for a writable mount that could not record itself mounted.
    fn state_unwritable() -> Self::Unservable;
    /// Whether the filesystem was left cleanly unmounted.
    fn was_left_clean(&self) -> Result<bool, FsError>;
    /// Record the filesystem mounted, before `Ready`, so it never looks clean while it can change.
    fn mark_mounted(&self) -> Result<(), FsError>;
    /// Record the filesystem cleanly unmounted, as an unmount's last write.
    fn mark_clean(&self) -> Result<(), FsError>;

    /// **Map the regular file at `path`** for the Model A data path: its runs into `runs`.
    fn map_file(&self, path: &[u8], runs: &mut [BlockRun]) -> Result<Mapped, FsError>;
    /// Read the whole regular file at `path` into `out`: the eager resolve. Its length.
    fn read_file(&self, path: &[u8], out: &mut [u8]) -> Result<usize, FsError>;
    /// Read `len` bytes of the file at `path` from `offset` into `out`: `File::ReadRange`. How
    /// many were read, fewer at the file's end.
    fn read_file_range(&self, path: &[u8], offset: u64, len: usize, out: &mut [u8]) -> Result<usize, FsError>;
    /// Create the regular file `name` in the directory at `parent`, empty; an existing one is
    /// left as it is.
    fn create_file(&self, parent: &[u8], name: &[u8], now: i64) -> Result<(), FsError>;
    /// Grow the file at `path` to `size` bytes, what it adds reading as zeroes.
    fn grow_file(&self, path: &[u8], size: usize, now: i64) -> Result<(), FsError>;
    /// Shrink the file at `path` to `size` bytes. **A file id to forget and then release**, if the
    /// shrink ended one: the server sends `File::Forget` for it, then calls [`Volume::release`].
    fn truncate_file(&self, path: &[u8], size: usize, now: i64) -> Result<Option<u64>, FsError>;

    /// The directory at `path`: its id, or `NotFound` if `path` is not a directory.
    fn resolve_dir(&self, path: &[u8]) -> Result<u64, FsError>;
    /// **List directory `dir` from `cursor`**, an entry at a time into `emit` until it answers
    /// `false`. Where to resume, or `0` at the end.
    fn read_dir(&self, dir: u64, cursor: u64, emit: impl FnMut(&DirEntry) -> bool) -> Result<u64, FsError>;
    /// Make the directory `name` in directory `dir`.
    fn mkdir_at(&self, dir: u64, name: &[u8], now: i64) -> Result<(), FsError>;
    /// Remove the regular file `name` from directory `dir`. **A file id to forget and then
    /// release**, if that was its last name.
    fn unlink_at(&self, dir: u64, name: &[u8], now: i64) -> Result<Option<u64>, FsError>;
    /// Remove the empty directory `name` from directory `dir`.
    fn rmdir_at(&self, dir: u64, name: &[u8], now: i64) -> Result<(), FsError>;
    /// Stamp `name` in directory `dir` modified now.
    fn touch_at(&self, dir: u64, name: &[u8], now: i64) -> Result<(), FsError>;
    /// Rename `old` to `new`, both in directory `dir`.
    fn rename_at(&self, dir: u64, old: &[u8], new: &[u8], now: i64) -> Result<(), FsError>;
    /// Rename the entry at `old` to `new`, replacing a file there only if `replace`. **A file id
    /// to forget and then release**, if a replaced file had no other name.
    fn rename_path(&self, old: &[u8], new: &[u8], replace: bool, now: i64) -> Result<Option<u64>, FsError>;
    /// Stamp the file with id `id` modified now: the kernel reporting a write it made.
    fn touch_file(&self, id: u64, now: i64) -> Result<(), FsError>;
    /// **Free the file with id `id`**, which no name reaches any more, once the kernel has
    /// forgotten it.
    fn release(&self, id: u64, now: i64) -> Result<(), FsError>;
}
