//! **Directories** (Phase 6 Part E.2): their 32-byte entries, long names assembled from the entries
//! before a short one, and paths resolved.
//!
//! **A directory is FAT12 and FAT16's fixed root**, a run of sectors after the FATs, **or a chain
//! of clusters**, as every other directory is and FAT32's root too. Its entries are numbered from
//! `0` — a *slot* — whichever it is.

use crate::bpb::{Geometry, Kind, SECTOR};
use crate::names::{self, UNITS_PER_ENTRY};
use crate::table::Cache;
use crate::{BlockReader, FsError};

/// One entry's length.
pub const ENTRY: u32 = 32;
/// The first byte of an entry deleted.
pub const DELETED: u8 = 0xE5;
/// An entry's attributes.
pub const ATTR_READ_ONLY: u8 = 0x01;
pub const ATTR_VOLUME: u8 = 0x08;
pub const ATTR_DIR: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;
/// A long-name entry's attributes: read-only, hidden, system and volume together.
pub const ATTR_LONG: u8 = 0x0F;

/// **A directory**: FAT12 and FAT16's fixed root, or the chain from a cluster.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Dir {
    Root,
    Chain(u32),
}

impl Dir {
    /// The volume's root directory.
    pub fn root(g: &Geometry) -> Dir {
        if g.kind == Kind::Fat32 { Dir::Chain(g.root_cluster) } else { Dir::Root }
    }

    /// **The directory's id**, as a session holds it: its first cluster, or `0` for a fixed root.
    pub fn id(self) -> u64 {
        match self {
            Dir::Root => 0,
            Dir::Chain(c) => c as u64,
        }
    }

    /// The directory an id names, or `NotFound` if it names none this volume can have.
    pub fn from_id(g: &Geometry, id: u64) -> Result<Dir, FsError> {
        match id {
            0 if g.kind != Kind::Fat32 => Ok(Dir::Root),
            c if c <= u32::MAX as u64 && g.valid_cluster(c as u32) => Ok(Dir::Chain(c as u32)),
            _ => Err(FsError::NotFound),
        }
    }
}

/// **The byte at which slot `slot` of `dir` is**, or `None` past the directory's end.
pub fn slot_byte<R: BlockReader>(r: &R, g: &Geometry, cache: &mut Cache, dir: Dir, slot: u32) -> Result<Option<u64>, FsError> {
    match dir {
        Dir::Root => Ok((slot < g.root_entries).then(|| g.root_start as u64 * SECTOR as u64 + slot as u64 * ENTRY as u64)),
        Dir::Chain(first) => {
            let per = g.cluster_bytes() / ENTRY;
            Ok(cache.nth(r, g, first, slot / per)?.map(|c| g.cluster_sector(c) * SECTOR as u64 + (slot % per) as u64 * ENTRY as u64))
        }
    }
}

/// **Walk `dir`'s slots from `from`**, handing each slot and its raw entry to `each` until it
/// answers `false`, an entry beginning `0` ends the directory, or its last slot is passed.
pub fn walk_slots<R: BlockReader>(
    r: &R,
    g: &Geometry,
    cache: &mut Cache,
    dir: Dir,
    from: u32,
    mut each: impl FnMut(u32, &[u8; 32]) -> bool,
) -> Result<(), FsError> {
    let mut sector = [0u8; SECTOR as usize];
    let per_sector = SECTOR / ENTRY;
    // Every sector of the directory from the one holding `from`, as (its byte, its first slot).
    let mut visit = |at: u64, first: u32, sector: &mut [u8; SECTOR as usize]| -> Result<bool, FsError> {
        r.read_at(at, sector)?;
        for k in 0..per_sector {
            let slot = first + k;
            if slot < from {
                continue;
            }
            let e: &[u8; 32] = sector[(k * ENTRY) as usize..((k + 1) * ENTRY) as usize].try_into().unwrap();
            if e[0] == 0 || !each(slot, e) {
                return Ok(false);
            }
        }
        Ok(true)
    };
    match dir {
        Dir::Root => {
            for s in from / per_sector..g.root_sectors {
                if !visit((g.root_start + s) as u64 * SECTOR as u64, s * per_sector, &mut sector)? {
                    break;
                }
            }
            Ok(())
        }
        Dir::Chain(first) => {
            let per_cluster = g.sectors_per_cluster * per_sector;
            let skip = from / per_cluster;
            let mut err = None;
            cache.walk(r, g, first, |k, c| {
                if k < skip {
                    return true;
                }
                for s in 0..g.sectors_per_cluster {
                    let at = (g.cluster_sector(c) + s as u64) * SECTOR as u64;
                    match visit(at, k * per_cluster + s * per_sector, &mut sector) {
                        Ok(true) => {}
                        Ok(false) => return false,
                        Err(e) => {
                            err = Some(e);
                            return false;
                        }
                    }
                }
                true
            })?;
            err.map_or(Ok(()), Err)
        }
    }
}

/// **A long name being assembled** from the entries before a short one, last part first.
struct Lfn {
    units: [u16; 20 * UNITS_PER_ENTRY],
    /// The part expected next, counting down to `1`; `0` once complete.
    expect: u8,
    /// How many parts, and the checksum they all carry.
    parts: u8,
    sum: u8,
    /// The slot of its first entry, where a deletion or a resumed listing begins.
    start: u32,
    active: bool,
}

impl Lfn {
    fn new() -> Lfn {
        Lfn { units: [0xFFFF; 20 * UNITS_PER_ENTRY], expect: 0, parts: 0, sum: 0, start: 0, active: false }
    }

    /// Take one long-name entry. Anything out of order, or with a different checksum, abandons
    /// what was assembled: such a name is stale, and the short entry stands alone.
    fn feed(&mut self, slot: u32, e: &[u8; 32]) {
        let ord = e[0] & 0x1F;
        if e[0] & 0x40 != 0 {
            if ord == 0 || ord > 20 {
                self.active = false;
                return;
            }
            *self = Lfn::new();
            self.active = true;
            self.expect = ord;
            self.parts = ord;
            self.sum = e[13];
            self.start = slot;
        }
        if !self.active || ord != self.expect || e[13] != self.sum || e[12] != 0 || e[26] != 0 || e[27] != 0 {
            self.active = false;
            return;
        }
        let at = (ord as usize - 1) * UNITS_PER_ENTRY;
        let mut k = 0;
        for range in [1..11, 14..26, 28..32] {
            for pair in e[range].chunks(2) {
                self.units[at + k] = u16::from_le_bytes([pair[0], pair[1]]);
                k += 1;
            }
        }
        self.expect -= 1;
    }

    /// **The long name of the short entry `short`**, if one was assembled whole for it: its units,
    /// up to the terminating zero.
    fn take(&mut self, short: &[u8; 11]) -> Option<(&[u16], u32)> {
        let whole = self.active && self.expect == 0 && self.sum == names::checksum(short);
        self.active = false;
        if !whole {
            return None;
        }
        let all = &self.units[..self.parts as usize * UNITS_PER_ENTRY];
        let len = all.iter().position(|&u| u == 0).unwrap_or(all.len());
        Some((&all[..len], self.start))
    }
}

/// **One file or directory found in a directory**: where its entries are, and what its short
/// entry says.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// Its directory.
    pub dir: Dir,
    /// Its first entry — its first long-name entry, or its short entry if it has no long name.
    pub first_slot: u32,
    /// Its short entry.
    pub slot: u32,
    pub short: [u8; 11],
    /// Windows' case bits for the short name.
    pub case: u8,
    pub attr: u8,
    /// Its first cluster; `0` for an empty file.
    pub cluster: u32,
    pub size: u32,
    /// When it was last written, seconds since the epoch; `0` if unknown.
    pub mtime: i64,
    long: [u8; 255],
    /// Its long name's length in UTF-8: `Some` when it has one that fits [`Found::name`].
    long_len: Option<usize>,
    /// Whether it has a long name at all, fitting or not.
    has_long: bool,
}

impl Found {
    /// Whether it is a directory.
    pub fn is_dir(&self) -> bool {
        self.attr & ATTR_DIR != 0
    }

    /// **Its name as the system shows it**: its long name, or its short one shown, into `out`. Its
    /// length, or `None` for a long name too long for 255 bytes of UTF-8 — such a file is not
    /// listed.
    pub fn name(&self, out: &mut [u8; 255]) -> Option<usize> {
        if self.has_long {
            let n = self.long_len?;
            out[..n].copy_from_slice(&self.long[..n]);
            return Some(n);
        }
        let mut s = [0u8; 12];
        let n = names::short_display(&self.short, self.case, &mut s);
        out[..n].copy_from_slice(&s[..n]);
        Some(n)
    }

    /// **Whether `name` names it**: its long name, or its short one, without ASCII case.
    pub fn is_named(&self, name: &[u8]) -> bool {
        if let Some(n) = self.long_len
            && names::eq_fold(&self.long[..n], name)
        {
            return true;
        }
        let mut s = [0u8; 12];
        let n = names::short_display(&self.short, 0, &mut s);
        names::eq_fold(&s[..n], name)
    }
}

/// The parts of a short entry a [`Found`] carries. **The cluster's high half is FAT32's alone**: on
/// FAT12 and FAT16 those bytes were OS/2's extended-attribute index, and are not a cluster.
fn found(kind: Kind, dir: Dir, slot: u32, e: &[u8; 32], long: Option<(&[u16], u32)>) -> Found {
    let mut short = [0u8; 11];
    short.copy_from_slice(&e[..11]);
    let hi = if kind == Kind::Fat32 { u16::from_le_bytes([e[20], e[21]]) as u32 } else { 0 };
    let lo = u16::from_le_bytes([e[26], e[27]]) as u32;
    let mut f = Found {
        dir,
        first_slot: slot,
        slot,
        short,
        case: e[12],
        attr: e[11],
        cluster: (hi << 16) | lo,
        size: u32::from_le_bytes([e[28], e[29], e[30], e[31]]),
        mtime: crate::time::from_fat(u16::from_le_bytes([e[24], e[25]]), u16::from_le_bytes([e[22], e[23]])),
        long: [0; 255],
        long_len: None,
        has_long: false,
    };
    if let Some((units, start)) = long {
        f.has_long = true;
        f.first_slot = start;
        f.long_len = names::from_utf16(units, &mut f.long);
    }
    f
}

/// Whether a short entry is `.` or `..`.
fn is_dot(e: &[u8; 32]) -> bool {
    &e[..11] == b".          " || &e[..11] == b"..         "
}

/// **Walk `dir`'s files and directories from slot `from`**, handing each to `each` until it
/// answers `false`. Deleted entries, long-name parts, the volume label and `.` and `..` are passed
/// over. **Where to resume**: `None` once the directory is done, or the first slot of the entry
/// `each` declined.
pub fn entries<R: BlockReader>(
    r: &R,
    g: &Geometry,
    cache: &mut Cache,
    dir: Dir,
    from: u32,
    mut each: impl FnMut(&Found) -> bool,
) -> Result<Option<u32>, FsError> {
    let mut lfn = Lfn::new();
    let mut resume = None;
    walk_slots(r, g, cache, dir, from, |slot, e| {
        if e[0] == DELETED {
            lfn.active = false;
            return true;
        }
        if e[11] & 0x3F == ATTR_LONG {
            lfn.feed(slot, e);
            return true;
        }
        let short: [u8; 11] = e[..11].try_into().unwrap();
        let long = lfn.take(&short);
        if e[11] & ATTR_VOLUME != 0 || is_dot(e) {
            return true;
        }
        let f = found(g.kind, dir, slot, e, long);
        if each(&f) {
            true
        } else {
            resume = Some(f.first_slot);
            false
        }
    })?;
    Ok(resume)
}

/// **The entry `name` names in `dir`**, or `NotFound`.
pub fn lookup<R: BlockReader>(r: &R, g: &Geometry, cache: &mut Cache, dir: Dir, name: &[u8]) -> Result<Found, FsError> {
    let mut hit = None;
    entries(r, g, cache, dir, 0, |f| {
        if f.is_named(name) {
            hit = Some(*f);
            false
        } else {
            true
        }
    })?;
    hit.ok_or(FsError::NotFound)
}

/// What a path names.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Node {
    /// The volume's root directory.
    Root,
    /// A file or a directory, and where its entry is.
    Entry(Found),
}

/// **Resolve `path`**, absolute within the volume. `.` and `..` are not followed: a path the
/// kernel forwards has none.
pub fn resolve<R: BlockReader>(r: &R, g: &Geometry, cache: &mut Cache, path: &[u8]) -> Result<Node, FsError> {
    let mut dir = Dir::root(g);
    let mut node = Node::Root;
    for part in path.split(|&c| c == b'/').filter(|p| !p.is_empty()) {
        if let Node::Entry(f) = node {
            dir = subdir(g, &f)?;
        }
        if part == b"." || part == b".." {
            return Err(FsError::NotFound);
        }
        node = Node::Entry(lookup(r, g, cache, dir, part)?);
    }
    Ok(node)
}

/// **The directory a found entry is**, or `NotFound` if it is a file.
pub fn subdir(g: &Geometry, f: &Found) -> Result<Dir, FsError> {
    if !f.is_dir() {
        return Err(FsError::NotFound);
    }
    if !g.valid_cluster(f.cluster) {
        return Err(FsError::Corrupt);
    }
    Ok(Dir::Chain(f.cluster))
}

/// The directory `path` names: the root, or a directory's chain.
pub fn resolve_dir<R: BlockReader>(r: &R, g: &Geometry, cache: &mut Cache, path: &[u8]) -> Result<Dir, FsError> {
    match resolve(r, g, cache, path)? {
        Node::Root => Ok(Dir::root(g)),
        Node::Entry(f) => subdir(g, &f),
    }
}
