//! **A FAT filesystem on a device** (Phase 6 Part E.2), and what can be done with it.
//!
//! [`Fat`] holds the device, the geometry its boot sector gave, and the cache of FAT sectors; its
//! methods are what `fs-server-fat`'s binary hands `libfsserver`'s loop. **The map it replies is in
//! 512-byte sectors** — `block_size` 512, every run's start and length in sectors from the volume's
//! start — because a FAT's data region need not begin on a 4 KiB boundary. The kernel fills and
//! writes a page as one device range, so no page may span two runs: a cluster of at least a page,
//! the server's rule ([`crate::bpb::servable`]). **A file's id is its first cluster**, the same
//! through a rename or a move; an empty file has none, and its id is `0`.

use core::cell::RefCell;

use crate::bpb::{self, Geometry, SECTOR, Unservable};
use crate::dir::{self, Dir, Node};
use crate::table::Cache;
use crate::{BlockReader, BlockRun, FsError};
use libfsserver::{DirEntry, Mapped};
use librsproto::file::{DIRENT_KIND_DIR, DIRENT_KIND_FILE};

/// The mode a listing gives a directory, a file, and a file marked read-only: FAT keeps no
/// permissions.
const MODE_DIR: u16 = 0o040755;
const MODE_FILE: u16 = 0o100644;
const MODE_READ_ONLY: u16 = 0o100444;

/// **A FAT filesystem on `R`**: the device, or the `ReadOnly` over it a read-only mount is served
/// through.
pub struct Fat<'a, R> {
    r: &'a R,
    geometry: Result<Geometry, Unservable>,
    cache: RefCell<Cache>,
}

impl<'a, R: BlockReader> Fat<'a, R> {
    /// **The FAT on `r`**, its boot sector read. One that is not a FAT is still a value: what
    /// [`Fat::check`] reports, and what every other method refuses.
    pub fn new(r: &'a R) -> Fat<'a, R> {
        let mut b = [0u8; SECTOR as usize];
        let geometry = match r.read_at(0, &mut b) {
            Ok(()) => bpb::parse(&b),
            Err(_) => Err(Unservable::Unreadable),
        };
        Fat { r, geometry, cache: RefCell::new(Cache::new()) }
    }

    /// The device.
    pub fn device(&self) -> &R {
        self.r
    }

    /// **The geometry**, or the error a client is told for a device that has none.
    pub fn geometry(&self) -> Result<&Geometry, FsError> {
        self.geometry.as_ref().map_err(|e| e.fs_error())
    }

    /// **Whether a server serves this**: a FAT, with clusters of at least a page, whose last sector
    /// reads — so it is not larger than its device.
    pub fn check(&self) -> Result<(), Unservable> {
        let g = self.geometry.as_ref().map_err(|e| *e)?;
        bpb::servable(g)?;
        let mut s = [0u8; SECTOR as usize];
        self.r.read_at((g.total_sectors as u64 - 1) * SECTOR as u64, &mut s).map_err(|_| Unservable::Truncated)
    }

    /// The volume's label, padding trimmed; empty when it has none.
    pub fn label(&self) -> &[u8] {
        self.geometry.as_ref().map_or(&[], bpb::label)
    }

    /// **Whether the filesystem was left cleanly unmounted**: the boot sector's state byte, whose
    /// bit 0 a mount sets and an unmount clears.
    pub fn was_left_clean(&self) -> Result<bool, FsError> {
        let g = self.geometry()?;
        let mut b = [0u8; 1];
        self.r.read_at(g.state_at as u64, &mut b)?;
        Ok(b[0] & 1 == 0)
    }

    /// The regular file `path` names, or `NotFound`.
    fn file(&self, g: &Geometry, cache: &mut Cache, path: &[u8]) -> Result<dir::Found, FsError> {
        match dir::resolve(self.r, g, cache, path)? {
            Node::Entry(f) if !f.is_dir() => Ok(f),
            _ => Err(FsError::NotFound),
        }
    }

    /// **Map the file at `path`** into `runs`: its clusters in sectors, a run per contiguous
    /// stretch. A file in more fragments than `runs` holds is `TooLarge`; a chain shorter than the
    /// file's size is `Corrupt`.
    pub fn map_file(&self, path: &[u8], runs: &mut [BlockRun]) -> Result<Mapped, FsError> {
        let g = self.geometry()?;
        let mut cache = self.cache.borrow_mut();
        let f = self.file(g, &mut cache, path)?;
        let need = f.size.div_ceil(g.cluster_bytes());
        let spc = g.sectors_per_cluster;
        let mut n = 0usize;
        let mut got = 0u32;
        let mut too_many = false;
        if need > 0 {
            cache.walk(self.r, g, f.cluster, |k, c| {
                if k == need {
                    return false;
                }
                got += 1;
                let lba = g.cluster_sector(c);
                if n > 0 && runs[n - 1].device_lba + runs[n - 1].length as u64 == lba {
                    runs[n - 1].length += spc;
                    return true;
                }
                if n == runs.len() {
                    too_many = true;
                    return false;
                }
                runs[n] = BlockRun { file_block: k as u64 * spc as u64, device_lba: lba, length: spc, flags: 0 };
                n += 1;
                true
            })?;
        }
        if too_many {
            return Err(FsError::TooLarge);
        }
        if got < need {
            return Err(FsError::Corrupt);
        }
        Ok(Mapped { size: f.size as usize, block_size: SECTOR, runs: n, id: f.cluster as u64 })
    }

    /// **Read `len` bytes of the file at `path` from `offset`** into `out`: how many, fewer at the
    /// file's end.
    pub fn read_file_range(&self, path: &[u8], offset: u64, len: usize, out: &mut [u8]) -> Result<usize, FsError> {
        let g = self.geometry()?;
        let mut cache = self.cache.borrow_mut();
        let f = self.file(g, &mut cache, path)?;
        let end = (offset.saturating_add(len.min(out.len()) as u64)).min(f.size as u64);
        if offset >= end {
            return Ok(0);
        }
        let cb = g.cluster_bytes() as u64;
        let (first, last) = ((offset / cb) as u32, ((end - 1) / cb) as u32);
        let mut err = None;
        cache.walk(self.r, g, f.cluster, |k, c| {
            if k < first {
                return true;
            }
            let from = (k as u64 * cb).max(offset);
            let to = ((k as u64 + 1) * cb).min(end);
            let at = g.cluster_sector(c) * SECTOR as u64 + (from - k as u64 * cb);
            if let Err(e) = self.r.read_at(at, &mut out[(from - offset) as usize..(to - offset) as usize]) {
                err = Some(e);
                return false;
            }
            k < last
        })?;
        err.map_or(Ok((end - offset) as usize), Err)
    }

    /// **Read the whole file at `path`** into `out`; `TooLarge` if it does not fit.
    pub fn read_file(&self, path: &[u8], out: &mut [u8]) -> Result<usize, FsError> {
        let size = {
            let g = self.geometry()?;
            let mut cache = self.cache.borrow_mut();
            self.file(g, &mut cache, path)?.size as usize
        };
        if size > out.len() {
            return Err(FsError::TooLarge);
        }
        self.read_file_range(path, 0, size, out)
    }

    /// **The directory at `path`**: its id, or `NotFound` if `path` is not a directory.
    pub fn resolve_dir(&self, path: &[u8]) -> Result<u64, FsError> {
        let g = self.geometry()?;
        Ok(dir::resolve_dir(self.r, g, &mut self.cache.borrow_mut(), path)?.id())
    }

    /// **List directory `id` from `cursor`**, each file and directory into `emit` until it answers
    /// `false`. The cursor to resume from — one past the slot to begin at, so `0` is both the start
    /// and the end — or `0` when the directory is done. A file whose long name is too long for a
    /// listing entry is passed over.
    pub fn read_dir(&self, id: u64, cursor: u64, mut emit: impl FnMut(&DirEntry) -> bool) -> Result<u64, FsError> {
        let g = self.geometry()?;
        let d = Dir::from_id(g, id)?;
        let from = cursor.saturating_sub(1).min(u32::MAX as u64) as u32;
        let resume = dir::entries(self.r, g, &mut self.cache.borrow_mut(), d, from, |f| {
            let mut name = [0u8; 255];
            let Some(n) = f.name(&mut name) else {
                return true;
            };
            let (kind, mode, size) = if f.is_dir() {
                (DIRENT_KIND_DIR, MODE_DIR, 0)
            } else if f.attr & dir::ATTR_READ_ONLY != 0 {
                (DIRENT_KIND_FILE, MODE_READ_ONLY, f.size as u64)
            } else {
                (DIRENT_KIND_FILE, MODE_FILE, f.size as u64)
            };
            emit(&DirEntry { id: f.cluster, kind, mode, size, mtime: f.mtime, name: &name[..n] })
        })?;
        Ok(resume.map_or(0, |slot| slot as u64 + 1))
    }
}

#[cfg(test)]
mod tests;
