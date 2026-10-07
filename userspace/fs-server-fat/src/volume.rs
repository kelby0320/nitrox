//! **A FAT filesystem on a device** (Phase 6 Part E.2), and what can be done with it.
//!
//! [`Fat`] holds the device, the geometry its boot sector gave, and the cache of FAT sectors; its
//! methods are what `fs-server-fat`'s binary hands `libfsserver`'s loop. **The map it replies is in
//! 512-byte sectors** — `block_size` 512, every run's start and length in sectors from the volume's
//! start — because a FAT's data region need not begin on a 4 KiB boundary. The kernel fills and
//! writes a page as one device range, so no page may span two runs: a cluster of at least a page,
//! the server's rule ([`crate::bpb::servable`]). **A file's id is its first cluster**, the same
//! through a rename or a move; an empty file has none, and its id is `0`.
//!
//! **Writing** (Phase 6 Part E.3) orders what it writes so a crash between writes loses clusters
//! and never gives one to two files: **data, then the chain, then the directory entry**. A grow
//! zeroes the clusters it takes, writes the FAT, and only then the entry that makes them the
//! file's; a removal writes the entry first and frees the chain after. The FAT's dirty sectors
//! are written at those points and at the end of each change — adjacent ones together, to every
//! copy — so a request costs a few writes rather than one per cluster.

use core::cell::{Cell, RefCell};

use crate::bpb::{self, Geometry, Kind, SECTOR, Unservable};
use crate::dir::{self, ATTR_ARCHIVE, ATTR_DIR, DELETED, DOT, DOTDOT, Dir, ENTRY, Found, MAX_SLOTS, Node};
use crate::table::{self, Cache, Next};
use crate::{BlockReader, BlockRun, BlockWriter, FsError};
use libfsserver::disk::SPAN;
use libfsserver::{DirEntry, Mapped};
use librsproto::file::{DIRENT_KIND_DIR, DIRENT_KIND_FILE};

/// The mode a listing gives a directory, a file, and a file marked read-only: FAT keeps no
/// permissions.
const MODE_DIR: u16 = 0o040755;
const MODE_FILE: u16 = 0o100644;
const MODE_READ_ONLY: u16 = 0o100444;

/// Zeroes, written over what a grow adds.
static ZEROES: [u8; SPAN] = [0; SPAN];

/// **How many files [`Fat::touch_file`] can find by id.** FAT has no table from a first cluster
/// to its entry, so the volume keeps one, filled as files are mapped: a file it no longer holds
/// misses a stamp, which `File::Touch` is allowed to.
pub const ID_TABLE: usize = 256;
/// How many files removed and not yet released are held for [`Fat::release`]. The server releases
/// each straight after the kernel forgets it, so one is the most ever waiting.
const ORPHANS: usize = 8;

/// **Where the short entry of the file with id `id` is.**
#[derive(Copy, Clone)]
struct Place {
    id: u32,
    dir: Dir,
    slot: u32,
}

/// **The files [`Fat::touch_file`] can find**: the last [`ID_TABLE`] mapped, the oldest let go.
struct Ids {
    places: [Place; ID_TABLE],
    next: usize,
}

impl Ids {
    fn new() -> Ids {
        Ids { places: [Place { id: 0, dir: Dir::Root, slot: 0 }; ID_TABLE], next: 0 }
    }

    /// Record where file `id` is: where it was if it is held, else in place of the oldest.
    fn put(&mut self, id: u32, dir: Dir, slot: u32) {
        if id == 0 {
            return;
        }
        if let Some(p) = self.places.iter_mut().find(|p| p.id == id) {
            (p.dir, p.slot) = (dir, slot);
            return;
        }
        self.places[self.next] = Place { id, dir, slot };
        self.next = (self.next + 1) % ID_TABLE;
    }

    /// Where file `id` is, if it is held.
    fn get(&self, id: u32) -> Option<(Dir, u32)> {
        self.places.iter().find(|p| id != 0 && p.id == id).map(|p| (p.dir, p.slot))
    }

    /// A file moved: where it is now, if it is held.
    fn moved(&mut self, id: u32, dir: Dir, slot: u32) {
        if let Some(p) = self.places.iter_mut().find(|p| id != 0 && p.id == id) {
            (p.dir, p.slot) = (dir, slot);
        }
    }

    /// Let file `id` go: it has no entry any more.
    fn forget(&mut self, id: u32) {
        for p in self.places.iter_mut().filter(|p| p.id == id) {
            p.id = 0;
        }
    }
}

/// **A FAT filesystem on `R`**: the device, or the `ReadOnly` over it a read-only mount is served
/// through.
pub struct Fat<'a, R> {
    r: &'a R,
    geometry: Result<Geometry, Unservable>,
    cache: RefCell<Cache>,
    /// Whether the filesystem was found not cleanly unmounted, so its unmount leaves it so.
    found_dirty: Cell<bool>,
    /// Where the next allocation's search begins: FAT32's next-free hint, then past the last.
    hint: Cell<u32>,
    ids: RefCell<Ids>,
    /// Files removed whose clusters wait for [`Fat::release`]; `0` is none.
    orphans: RefCell<[u32; ORPHANS]>,
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
        Fat {
            r,
            geometry,
            cache: RefCell::new(Cache::new()),
            found_dirty: Cell::new(false),
            hint: Cell::new(2),
            ids: RefCell::new(Ids::new()),
            orphans: RefCell::new([0; ORPHANS]),
        }
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

    /// **How long the chain from `first` is, and its last cluster**: `(0, 0)` for none.
    fn chain_end(&self, g: &Geometry, cache: &mut Cache, first: u32) -> Result<(u32, u32), FsError> {
        let (mut n, mut last) = (0, 0);
        if first != 0 {
            cache.walk(self.r, g, first, |_, c| {
                n += 1;
                last = c;
                true
            })?;
        }
        Ok((n, last))
    }

    /// The short entry at `slot` of `dir`, as its bytes.
    fn raw_entry(&self, g: &Geometry, cache: &mut Cache, dir: Dir, slot: u32) -> Result<[u8; 32], FsError> {
        let at = dir::slot_byte(self.r, g, cache, dir, slot)?.ok_or(FsError::Corrupt)?;
        let mut e = [0u8; 32];
        self.r.read_at(at, &mut e)?;
        Ok(e)
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
        self.ids.borrow_mut().put(f.cluster, f.dir, f.slot);
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

/// **Zeroes written in runs**: ranges handed one at a time, written as each contiguous stretch of
/// them ends or fills [`SPAN`].
struct Zeroer<'f, W> {
    w: &'f W,
    at: u64,
    len: u64,
}

impl<'f, W: BlockWriter> Zeroer<'f, W> {
    fn new(w: &'f W) -> Zeroer<'f, W> {
        Zeroer { w, at: 0, len: 0 }
    }

    /// Zero device bytes `[at, at + len)`, now or with what follows.
    fn add(&mut self, at: u64, len: u64) -> Result<(), FsError> {
        if self.len > 0 && self.at + self.len == at {
            self.len += len;
        } else {
            self.finish()?;
            (self.at, self.len) = (at, len);
        }
        while self.len >= SPAN as u64 {
            self.w.write_at(self.at, &ZEROES)?;
            self.at += SPAN as u64;
            self.len -= SPAN as u64;
        }
        Ok(())
    }

    /// Write what is held.
    fn finish(&mut self) -> Result<(), FsError> {
        if self.len > 0 {
            self.w.write_at(self.at, &ZEROES[..self.len as usize])?;
            self.len = 0;
        }
        Ok(())
    }
}

/// **A path's parent and last name**: `NotFound` for one with no name, the root.
fn split(path: &[u8]) -> Result<(&[u8], &[u8]), FsError> {
    let slash = path.iter().rposition(|&c| c == b'/').ok_or(FsError::NotFound)?;
    let name = &path[slash + 1..];
    if name.is_empty() {
        return Err(FsError::NotFound);
    }
    Ok((if slash == 0 { &b"/"[..] } else { &path[..slash] }, name))
}

impl<'a, R: BlockReader + BlockWriter> Fat<'a, R> {
    /// **Run a change to the filesystem**: refused on a read-only mount before anything is read,
    /// and the FAT's dirty sectors written at its end. **A failure is written too**: one that is
    /// not the device's leaves the cache as it found it, or with what it took given back; one that
    /// is the device's leaves nothing certain. A write that fails has the cache forgotten, so it
    /// never holds what the device does not.
    fn change<T>(&self, op: impl FnOnce(&Geometry, &mut Cache) -> Result<T, FsError>) -> Result<T, FsError> {
        if self.r.read_only() {
            return Err(FsError::ReadOnly);
        }
        let g = self.geometry()?;
        let mut cache = self.cache.borrow_mut();
        let out = op(g, &mut cache);
        if let Err(e) = cache.flush(self.r, g) {
            cache.clear();
            return out.and(Err(e));
        }
        out
    }

    /// **FAT32's FSInfo sector**, if it has one whose signatures are right.
    fn fsinfo(&self, g: &Geometry) -> Result<Option<[u8; SECTOR as usize]>, FsError> {
        if g.fsinfo == 0 {
            return Ok(None);
        }
        let mut s = [0u8; SECTOR as usize];
        self.r.read_at(g.fsinfo as u64 * SECTOR as u64, &mut s)?;
        let sig = |at: usize| u32::from_le_bytes([s[at], s[at + 1], s[at + 2], s[at + 3]]);
        Ok((sig(0) == 0x4161_5252 && sig(484) == 0x6141_7272 && sig(508) == 0xAA55_0000).then_some(s))
    }

    /// **Record the filesystem mounted**, before `Ready`: the state byte's bit set, and FAT32's free
    /// count made unknown, since it is not kept while mounted. A filesystem found with the bit set
    /// is remembered as not cleanly unmounted.
    pub fn mark_mounted(&self) -> Result<(), FsError> {
        if self.r.read_only() {
            return Err(FsError::ReadOnly);
        }
        let g = self.geometry()?;
        let mut b = [0u8; 1];
        self.r.read_at(g.state_at as u64, &mut b)?;
        self.found_dirty.set(b[0] & 1 != 0);
        b[0] |= 1;
        self.r.write_at(g.state_at as u64, &b)?;
        if let Some(mut info) = self.fsinfo(g)? {
            let next = u32::from_le_bytes([info[492], info[493], info[494], info[495]]);
            if g.valid_cluster(next) {
                self.hint.set(next);
            }
            info[488..492].copy_from_slice(&u32::MAX.to_le_bytes());
            self.r.write_at(g.fsinfo as u64 * SECTOR as u64, &info)?;
        }
        Ok(())
    }

    /// **Record the filesystem cleanly unmounted**, as an unmount's last writes: FAT32's free count,
    /// counted, and its next-free hint; then the state byte's bit cleared — **unless the
    /// filesystem was found with it set**. Nothing here repairs one, so the next system that can
    /// check it should still be told to.
    pub fn mark_clean(&self) -> Result<(), FsError> {
        if self.r.read_only() {
            return Err(FsError::ReadOnly);
        }
        let g = self.geometry()?;
        self.cache.borrow_mut().flush(self.r, g)?;
        if let Some(mut info) = self.fsinfo(g)? {
            info[488..492].copy_from_slice(&Cache::count_free(self.r, g)?.to_le_bytes());
            info[492..496].copy_from_slice(&self.hint.get().to_le_bytes());
            self.r.write_at(g.fsinfo as u64 * SECTOR as u64, &info)?;
        }
        if !self.found_dirty.get() {
            let mut b = [0u8; 1];
            self.r.read_at(g.state_at as u64, &mut b)?;
            b[0] &= !1;
            self.r.write_at(g.state_at as u64, &b)?;
        }
        Ok(())
    }

    /// Hold `c` for [`Fat::release`], in place of the oldest if every place is taken: that one's
    /// clusters are lost, which a check reclaims.
    fn orphan(&self, c: u32) {
        let mut o = self.orphans.borrow_mut();
        o.copy_within(1.., 0);
        o[ORPHANS - 1] = c;
    }

    /// **A file's last name gone**: its id let go, and its clusters held for release. The id the
    /// server forgets then releases, or none for an empty file.
    fn removed(&self, f: &Found) -> Option<u64> {
        if f.cluster == 0 {
            return None;
        }
        self.ids.borrow_mut().forget(f.cluster);
        self.orphan(f.cluster);
        Some(f.cluster as u64)
    }

    /// **`need` free slots in a row in `dir`**: its first. A chain directory without them grows
    /// by zeroed clusters, taken and written before they are linked to it. A full fixed root is
    /// `TooLarge`, as is a directory at its largest.
    fn place(&self, g: &Geometry, cache: &mut Cache, d: Dir, need: u32) -> Result<u32, FsError> {
        let (trailing, total) = match dir::free_run(self.r, g, cache, d, need)? {
            Ok(slot) => return Ok(slot),
            Err(t) => t,
        };
        let Dir::Chain(first) = d else {
            return Err(FsError::TooLarge);
        };
        let per = g.cluster_bytes() / ENTRY;
        let more = (need - trailing).div_ceil(per);
        if total as u64 + more as u64 * per as u64 > MAX_SLOTS as u64 {
            return Err(FsError::TooLarge);
        }
        let (_, last) = self.chain_end(g, cache, first)?;
        let cb = g.cluster_bytes() as u64;
        let mut z = Zeroer::new(self.r);
        let (new, _) = cache.allocate(self.r, g, more, last + 1, |c| z.add(g.cluster_sector(c) * SECTOR as u64, cb))?;
        z.finish()?;
        cache.flush(self.r, g)?;
        cache.set(self.r, g, last, new)?;
        Ok(total - trailing)
    }

    /// **Write the entries naming `name` into `d`**, its short entry made by `short` from its
    /// short name: the directory grown first if it must be, and the FAT written before the
    /// entries. Where its short entry is.
    fn insert(
        &self,
        g: &Geometry,
        cache: &mut Cache,
        d: Dir,
        name: &[u8],
        short: impl FnOnce(&[u8; 11]) -> [u8; 32],
    ) -> Result<u32, FsError> {
        let mut entries = [[0u8; 32]; dir::MAX_NAME_SLOTS];
        let n = dir::name_entries(self.r, g, cache, d, name, short, &mut entries)? as u32;
        let first = self.place(g, cache, d, n)?;
        cache.flush(self.r, g)?;
        dir::rewrite(self.r, g, cache, d, first, first + n - 1, |s, e| *e = entries[(s - first) as usize])?;
        Ok(first + n - 1)
    }

    /// `name` in `d`, if anything has it.
    fn find(&self, g: &Geometry, cache: &mut Cache, d: Dir, name: &[u8]) -> Result<Option<Found>, FsError> {
        match dir::lookup(self.r, g, cache, d, name) {
            Ok(f) => Ok(Some(f)),
            Err(FsError::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// **Create the empty file `name` in the directory at `parent`**; one that exists is left as
    /// it is. An empty file has no cluster.
    pub fn create_file(&self, parent: &[u8], name: &[u8], now: i64) -> Result<(), FsError> {
        self.change(|g, cache| {
            let d = dir::resolve_dir(self.r, g, cache, parent)?;
            if self.find(g, cache, d, name)?.is_some() {
                return Ok(());
            }
            self.insert(g, cache, d, name, |s| dir::short_entry(g.kind, s, ATTR_ARCHIVE, 0, 0, now)).map(drop)
        })
    }

    /// **Grow the file at `path` to `size` bytes**, what it adds reading as zeroes on the device:
    /// what its own clusters held past its end, and every cluster taken — as near its last one as
    /// the free space allows, so it stays in few runs. A size at or below the file's is nothing to
    /// do; one past FAT's 4 GiB less a byte, or more than the volume has free, is `TooLarge`.
    pub fn grow_file(&self, path: &[u8], size: usize, now: i64) -> Result<(), FsError> {
        self.change(|g, cache| {
            let f = self.file(g, cache, path)?;
            if size <= f.size as usize {
                return Ok(());
            }
            let size = u32::try_from(size).map_err(|_| FsError::TooLarge)?;
            let (old, new_end) = (f.size as u64, size as u64);
            let cb = g.cluster_bytes() as u64;
            let (len, last) = self.chain_end(g, cache, f.cluster)?;
            if (len as u64) < old.div_ceil(cb) {
                return Err(FsError::Corrupt);
            }
            let mut z = Zeroer::new(self.r);
            let held = new_end.min(len as u64 * cb);
            if old < held {
                let mut err = Ok(());
                cache.walk(self.r, g, f.cluster, |k, c| {
                    let (from, to) = ((k as u64 * cb).max(old), ((k as u64 + 1) * cb).min(held));
                    if from < to {
                        err = z.add(g.cluster_sector(c) * SECTOR as u64 + from - k as u64 * cb, to - from);
                    }
                    err.is_ok() && to < held
                })?;
                err?;
            }
            let need = new_end.div_ceil(cb) as u32;
            let mut first = f.cluster;
            if need > len {
                let hint = if len > 0 { last + 1 } else { self.hint.get() };
                let (taken, after) =
                    cache.allocate(self.r, g, need - len, hint, |c| z.add(g.cluster_sector(c) * SECTOR as u64, cb))?;
                z.finish()?;
                self.hint.set(after);
                cache.flush(self.r, g)?;
                if len == 0 {
                    first = taken;
                } else {
                    cache.set(self.r, g, last, taken)?;
                    cache.flush(self.r, g)?;
                }
            } else {
                z.finish()?;
            }
            dir::rewrite(self.r, g, cache, f.dir, f.slot, f.slot, |_, e| {
                dir::set_cluster(g.kind, e, first);
                e[28..32].copy_from_slice(&size.to_le_bytes());
                dir::stamp(e, now);
            })?;
            if first != f.cluster {
                self.ids.borrow_mut().put(first, f.dir, f.slot);
            }
            Ok(())
        })
    }

    /// **Shrink the file at `path` to `size` bytes**: its entry first, then the clusters past
    /// the new end freed — after the chain is cut, written, so no crash leaves the file's chain
    /// running into free clusters. **A truncate to zero ends the file's id**: its first cluster,
    /// returned for the server to forget and then release. A size at or above the file's is
    /// nothing to do.
    pub fn truncate_file(&self, path: &[u8], size: usize, now: i64) -> Result<Option<u64>, FsError> {
        self.change(|g, cache| {
            let f = self.file(g, cache, path)?;
            if size >= f.size as usize {
                return Ok(None);
            }
            let keep = (size as u64).div_ceil(g.cluster_bytes() as u64) as u32;
            let cut = match keep {
                0 => None,
                k => Some(cache.nth(self.r, g, f.cluster, k - 1)?.ok_or(FsError::Corrupt)?),
            };
            dir::rewrite(self.r, g, cache, f.dir, f.slot, f.slot, |_, e| {
                if keep == 0 {
                    dir::set_cluster(g.kind, e, 0);
                }
                e[28..32].copy_from_slice(&(size as u32).to_le_bytes());
                dir::stamp(e, now);
            })?;
            let Some(cut) = cut else {
                return Ok(self.removed(&f));
            };
            if let Next::Cluster(rest) = cache.next(self.r, g, cut)? {
                cache.set(self.r, g, cut, table::end_mark(g))?;
                cache.flush(self.r, g)?;
                cache.free_chain(self.r, g, rest)?;
            }
            Ok(None)
        })
    }

    /// **Make the directory `name` in directory `dir`**: its cluster taken and written — `.`, `..`
    /// and zeroes — before the entry that names it. `Exists` if the name is taken.
    pub fn mkdir_at(&self, dir: u64, name: &[u8], now: i64) -> Result<(), FsError> {
        self.change(|g, cache| {
            let d = Dir::from_id(g, dir)?;
            if self.find(g, cache, d, name)?.is_some() {
                return Err(FsError::Exists);
            }
            if !crate::names::valid(name) {
                return Err(FsError::InvalidName);
            }
            let (new, after) = cache.allocate(self.r, g, 1, self.hint.get(), |_| Ok(()))?;
            self.hint.set(after);
            let mut made = || -> Result<(), FsError> {
                let mut s = [0u8; SECTOR as usize];
                s[..32].copy_from_slice(&dir::short_entry(g.kind, DOT, ATTR_DIR, new, 0, now));
                s[32..64].copy_from_slice(&dir::short_entry(g.kind, DOTDOT, ATTR_DIR, dir::dotdot_cluster(g, d), 0, now));
                let at = g.cluster_sector(new) * SECTOR as u64;
                self.r.write_at(at, &s)?;
                let mut z = Zeroer::new(self.r);
                z.add(at + SECTOR as u64, g.cluster_bytes() as u64 - SECTOR as u64)?;
                z.finish()?;
                cache.flush(self.r, g)?;
                self.insert(g, cache, d, name, |s| dir::short_entry(g.kind, s, ATTR_DIR, new, 0, now)).map(drop)
            };
            let out = made();
            if out.is_err() {
                cache.free_chain(self.r, g, new)?;
            }
            out
        })
    }

    /// **Remove the regular file `name` from directory `dir`**: its entries marked deleted. Its
    /// clusters are not freed here — the kernel may be writing to them — but held, and its id
    /// returned for the server to forget and then release. `Unsupported` for a directory.
    pub fn unlink_at(&self, dir: u64, name: &[u8], _now: i64) -> Result<Option<u64>, FsError> {
        self.change(|g, cache| {
            let d = Dir::from_id(g, dir)?;
            let f = dir::lookup(self.r, g, cache, d, name)?;
            if f.is_dir() {
                return Err(FsError::Unsupported);
            }
            dir::rewrite(self.r, g, cache, d, f.first_slot, f.slot, |_, e| e[0] = DELETED)?;
            Ok(self.removed(&f))
        })
    }

    /// **Remove the empty directory `name` from directory `dir`**: its entries, then its clusters.
    /// `NotEmpty` if it holds anything; `Unsupported` for a file.
    pub fn rmdir_at(&self, dir: u64, name: &[u8], _now: i64) -> Result<(), FsError> {
        self.change(|g, cache| {
            let d = Dir::from_id(g, dir)?;
            let f = dir::lookup(self.r, g, cache, d, name)?;
            if !f.is_dir() {
                return Err(FsError::Unsupported);
            }
            let mut empty = true;
            dir::entries(self.r, g, cache, dir::subdir(g, &f)?, 0, |_| {
                empty = false;
                false
            })?;
            if !empty {
                return Err(FsError::NotEmpty);
            }
            dir::rewrite(self.r, g, cache, d, f.first_slot, f.slot, |_, e| e[0] = DELETED)?;
            cache.free_chain(self.r, g, f.cluster)
        })
    }

    /// Stamp `name` in directory `dir` written at `now`.
    pub fn touch_at(&self, dir: u64, name: &[u8], now: i64) -> Result<(), FsError> {
        self.change(|g, cache| {
            let d = Dir::from_id(g, dir)?;
            let f = dir::lookup(self.r, g, cache, d, name)?;
            dir::rewrite(self.r, g, cache, d, f.slot, f.slot, |_, e| dir::stamp(e, now))
        })
    }

    /// **Stamp the file with id `id` written at `now`**: the kernel reporting a write it made. Found
    /// through the id table, and stamped only if the entry there is still that file's; `NotFound`
    /// otherwise, which costs the file its stamp and nothing else.
    pub fn touch_file(&self, id: u64, now: i64) -> Result<(), FsError> {
        self.change(|g, cache| {
            let id = u32::try_from(id).map_err(|_| FsError::NotFound)?;
            let (d, slot) = self.ids.borrow().get(id).ok_or(FsError::NotFound)?;
            let e = self.raw_entry(g, cache, d, slot)?;
            let hi = if g.kind == Kind::Fat32 { u16::from_le_bytes([e[20], e[21]]) as u32 } else { 0 };
            let cluster = (hi << 16) | u16::from_le_bytes([e[26], e[27]]) as u32;
            let live = e[0] != 0
                && e[0] != DELETED
                && e[11] & 0x3F != dir::ATTR_LONG
                && e[11] & (ATTR_DIR | dir::ATTR_VOLUME) == 0;
            if !live || cluster != id {
                self.ids.borrow_mut().forget(id);
                return Err(FsError::NotFound);
            }
            dir::rewrite(self.r, g, cache, d, slot, slot, |_, e| dir::stamp(e, now))
        })
    }

    /// **Free the file with id `id`**, which no name reaches any more, once the kernel has
    /// forgotten it: its chain. `NotFound` for an id no removal returned.
    pub fn release(&self, id: u64, _now: i64) -> Result<(), FsError> {
        self.change(|g, cache| {
            let c = u32::try_from(id).map_err(|_| FsError::NotFound)?;
            let mut o = self.orphans.borrow_mut();
            let i = o.iter().position(|&x| c != 0 && x == c).ok_or(FsError::NotFound)?;
            o[i] = 0;
            drop(o);
            cache.free_chain(self.r, g, c)
        })
    }

    /// **Rename `old` to `new`, both in directory `dir`.** `Exists` if `new` is taken by another.
    pub fn rename_at(&self, dir: u64, old: &[u8], new: &[u8], now: i64) -> Result<(), FsError> {
        self.change(|g, cache| {
            let d = Dir::from_id(g, dir)?;
            self.rename(g, cache, (d, old), (d, new), false, now).map(drop)
        })
    }

    /// **Rename the entry at `old` to `new`**, replacing a file there only if `replace`. The id of
    /// a replaced file, for the server to forget and then release.
    pub fn rename_path(&self, old: &[u8], new: &[u8], replace: bool, now: i64) -> Result<Option<u64>, FsError> {
        self.change(|g, cache| {
            let ((op, on), (np, nn)) = (split(old)?, split(new)?);
            let od = dir::resolve_dir(self.r, g, cache, op)?;
            let nd = dir::resolve_dir(self.r, g, cache, np)?;
            self.rename(g, cache, (od, on), (nd, nn), replace, now)
        })
    }

    /// **Move `from` to `to`**, each a directory and a name, in an order that never loses the
    /// source: the new name first — a replaced file's entry pointed at the source, or new entries
    /// — then the old name deleted, then a moved directory's `..` pointed at its new parent. A crash
    /// between leaves the file under both names, which a check sorts out.
    ///
    /// A directory cannot move into itself, nor replace or be replaced (`Unsupported`), as on ext4
    /// here. A name the same as the source's but for case is the source renamed in place.
    fn rename(
        &self,
        g: &Geometry,
        cache: &mut Cache,
        (od, on): (Dir, &[u8]),
        (nd, nn): (Dir, &[u8]),
        replace: bool,
        _now: i64,
    ) -> Result<Option<u64>, FsError> {
        if od == nd && on == nn {
            return Ok(None);
        }
        let src = dir::lookup(self.r, g, cache, od, on)?;
        if !crate::names::valid(nn) {
            return Err(FsError::InvalidName);
        }
        if src.is_dir() && od != nd && dir::within(self.r, g, cache, nd, src.cluster)? {
            return Err(FsError::Unsupported);
        }
        let target = match self.find(g, cache, nd, nn)? {
            Some(t) if t.dir == src.dir && t.slot == src.slot => None,
            Some(_) if !replace => return Err(FsError::Exists),
            Some(t) if t.is_dir() || src.is_dir() => return Err(FsError::Unsupported),
            other => other,
        };
        let raw = self.raw_entry(g, cache, od, src.slot)?;
        let slot = match target {
            Some(t) => {
                // Everything but the name, and the case bits that belong to the name.
                dir::rewrite(self.r, g, cache, nd, t.slot, t.slot, |_, e| {
                    e[11] = raw[11];
                    e[13..].copy_from_slice(&raw[13..]);
                })?;
                t.slot
            }
            None => self.insert(g, cache, nd, nn, |s| {
                let mut e = raw;
                e[..11].copy_from_slice(s);
                if e[0] == DELETED {
                    e[0] = 0x05;
                }
                e[12] = 0;
                e
            })?,
        };
        dir::rewrite(self.r, g, cache, od, src.first_slot, src.slot, |_, e| e[0] = DELETED)?;
        if src.is_dir() && od != nd {
            let mut dotdot = false;
            dir::rewrite(self.r, g, cache, dir::subdir(g, &src)?, 1, 1, |_, e| {
                dotdot = &e[..11] == DOTDOT;
                if dotdot {
                    dir::set_cluster(g.kind, e, dir::dotdot_cluster(g, nd));
                }
            })?;
            if !dotdot {
                return Err(FsError::Corrupt);
            }
        } else {
            self.ids.borrow_mut().moved(src.cluster, nd, slot);
        }
        Ok(target.and_then(|t| self.removed(&t)))
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod write_tests;
