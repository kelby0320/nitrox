//! **Directories** (Phase 6 Part E.2): their 32-byte entries, long names assembled from the entries
//! before a short one, and paths resolved; and (E.3) entries written, a name's short form made
//! unique, and free slots found.
//!
//! **A directory is FAT12 and FAT16's fixed root**, a run of sectors after the FATs, **or a chain
//! of clusters**, as every other directory is and FAT32's root too. Its entries are numbered from
//! `0` — a *slot* — whichever it is.

use crate::bpb::{Geometry, Kind, SECTOR};
use crate::names::{self, UNITS_PER_ENTRY};
use crate::table::Cache;
use crate::{BlockReader, BlockWriter, FsError};

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
/// The most entries a name takes: twenty long-name entries, 260 units, and its short entry.
pub const MAX_NAME_SLOTS: usize = 21;
/// The most slots a directory may have, by the specification: 2 MiB of entries.
pub const MAX_SLOTS: u32 = 65_536;
/// A directory's first two entries' names.
pub const DOT: &[u8; 11] = b".          ";
pub const DOTDOT: &[u8; 11] = b"..         ";

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
    each: impl FnMut(u32, &[u8; 32]) -> bool,
) -> Result<(), FsError> {
    walk(r, g, cache, dir, from, true, each)
}

/// [`walk_slots`], and past an entry beginning `0` too when `stop_at_end` is not set.
fn walk<R: BlockReader>(
    r: &R,
    g: &Geometry,
    cache: &mut Cache,
    dir: Dir,
    from: u32,
    stop_at_end: bool,
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
            if (stop_at_end && e[0] == 0) || !each(slot, e) {
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
    &e[..11] == DOT || &e[..11] == DOTDOT
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

/// **Set an entry's first cluster.** The high half is FAT32's alone; FAT12 and FAT16 keep what
/// those bytes hold.
pub fn set_cluster(kind: Kind, e: &mut [u8; 32], c: u32) {
    if kind == Kind::Fat32 {
        e[20..22].copy_from_slice(&((c >> 16) as u16).to_le_bytes());
    }
    e[26..28].copy_from_slice(&(c as u16).to_le_bytes());
}

/// **Stamp an entry written at `now`**: its write time and date, and its access date.
pub fn stamp(e: &mut [u8; 32], now: i64) {
    let (date, time) = crate::time::to_fat(now);
    e[18..20].copy_from_slice(&date.to_le_bytes());
    e[22..24].copy_from_slice(&time.to_le_bytes());
    e[24..26].copy_from_slice(&date.to_le_bytes());
}

/// **A short entry**: `short`, with `attr`, its first cluster and size, made and written at `now`.
/// A name beginning `0xE5`, which would read as deleted, is stored beginning `0x05`.
pub fn short_entry(kind: Kind, short: &[u8; 11], attr: u8, cluster: u32, size: u32, now: i64) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[..11].copy_from_slice(short);
    if e[0] == DELETED {
        e[0] = 0x05;
    }
    e[11] = attr;
    let (date, time) = crate::time::to_fat(now);
    e[14..16].copy_from_slice(&time.to_le_bytes());
    e[16..18].copy_from_slice(&date.to_le_bytes());
    stamp(&mut e, now);
    set_cluster(kind, &mut e, cluster);
    e[28..32].copy_from_slice(&size.to_le_bytes());
    e
}

/// **The long-name entries for `units`**, tied to the short name whose checksum is `sum`, into
/// `out` in the order they are written — the last part first. How many.
fn long_entries(units: &[u16], sum: u8, out: &mut [[u8; 32]]) -> usize {
    let parts = units.len().div_ceil(UNITS_PER_ENTRY);
    for p in 0..parts {
        let mut e = [0u8; 32];
        e[0] = (p + 1) as u8 | if p + 1 == parts { 0x40 } else { 0 };
        e[11] = ATTR_LONG;
        e[13] = sum;
        let mut k = 0;
        for range in [1..11, 14..26, 28..32] {
            for at in range.step_by(2) {
                // The name, its terminating zero if there is room, then padding.
                let i = p * UNITS_PER_ENTRY + k;
                let u = match i.cmp(&units.len()) {
                    core::cmp::Ordering::Less => units[i],
                    core::cmp::Ordering::Equal => 0,
                    core::cmp::Ordering::Greater => 0xFFFF,
                };
                e[at..at + 2].copy_from_slice(&u.to_le_bytes());
                k += 1;
            }
        }
        out[parts - 1 - p] = e;
    }
    parts
}

/// **The entries that name a file `name` in `dir`**: a short entry alone for an upper-case 8.3
/// name, else long-name entries and a short name made unique in `dir` — `LONGNA~1.TXT`, or the
/// next free tail. `short` fills in the short entry's other fields given its name. Into `out`;
/// how many. `InvalidName` for a name a FAT cannot hold.
pub fn name_entries<R: BlockReader>(
    r: &R,
    g: &Geometry,
    cache: &mut Cache,
    dir: Dir,
    name: &[u8],
    short: impl FnOnce(&[u8; 11]) -> [u8; 32],
    out: &mut [[u8; 32]; MAX_NAME_SLOTS],
) -> Result<usize, FsError> {
    if !names::valid(name) {
        return Err(FsError::InvalidName);
    }
    if let Some(exact) = names::short_exact(name) {
        out[0] = short(&exact);
        return Ok(1);
    }
    let mut units = [0u16; names::MAX_UNITS];
    let n = names::to_utf16(name, &mut units).ok_or(FsError::InvalidName)?;
    let s = unique_short(r, g, cache, dir, name)?;
    let parts = long_entries(&units[..n], names::checksum(&s), &mut out[..]);
    out[parts] = short(&s);
    Ok(parts + 1)
}

/// **A short name for `name` no entry of `dir` has**: its basis with the lowest numeric tail free.
/// Found a thousand tails a pass, so a directory is read once for any but a crowded basis.
fn unique_short<R: BlockReader>(
    r: &R,
    g: &Geometry,
    cache: &mut Cache,
    dir: Dir,
    name: &[u8],
) -> Result<[u8; 11], FsError> {
    const WINDOW: u32 = 1024;
    let (b, bn, e, en) = names::basis(name);
    let (base, ext) = (&b[..bn], &e[..en]);
    let mut lo = 1;
    while lo < 1_000_000 {
        let mut used = [0u64; (WINDOW / 64) as usize];
        walk_slots(r, g, cache, dir, 0, |_, ent| {
            if ent[0] != DELETED && ent[11] & 0x3F != ATTR_LONG {
                let short: &[u8; 11] = ent[..11].try_into().unwrap();
                if let Some(n) = tail_of(short, base, ext)
                    && (lo..lo + WINDOW).contains(&n)
                {
                    used[((n - lo) / 64) as usize] |= 1 << ((n - lo) % 64);
                }
            }
            true
        })?;
        if let Some(k) = (0..WINDOW).find(|&k| used[(k / 64) as usize] & (1 << (k % 64)) == 0) {
            return Ok(names::with_tail(base, ext, lo + k));
        }
        lo += WINDOW;
    }
    Err(FsError::TooLarge)
}

/// **The tail `n` for which `short` is `base` and `ext` with that tail**, if it is one.
fn tail_of(short: &[u8; 11], base: &[u8], ext: &[u8]) -> Option<u32> {
    let tilde = short[..8].iter().rposition(|&c| c == b'~')?;
    let digits = &short[tilde + 1..8];
    let len = digits.iter().position(|&c| c == b' ').unwrap_or(digits.len());
    let digits = &digits[..len];
    if digits.is_empty() || digits[0] == b'0' || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let n = digits.iter().fold(0u32, |n, &d| n * 10 + (d - b'0') as u32);
    (names::with_tail(base, ext, n) == *short).then_some(n)
}

/// **Where `need` free slots in a row are in `dir`**: deleted entries, or any at or past the one
/// that ends it. `Ok` with the first; else `Err` with how many free slots end the directory and
/// how many slots it has, for a caller that grows it.
pub fn free_run<R: BlockReader>(
    r: &R,
    g: &Geometry,
    cache: &mut Cache,
    dir: Dir,
    need: u32,
) -> Result<Result<u32, (u32, u32)>, FsError> {
    let (mut ended, mut start, mut len, mut total) = (false, 0, 0, 0);
    let mut hit = None;
    walk(r, g, cache, dir, 0, false, |slot, e| {
        total = slot + 1;
        ended |= e[0] == 0;
        if ended || e[0] == DELETED {
            if len == 0 {
                start = slot;
            }
            len += 1;
            if len == need {
                hit = Some(start);
                return false;
            }
        } else {
            len = 0;
        }
        true
    })?;
    Ok(hit.ok_or((len, total)))
}

/// **Rewrite slots `first..=last` of `dir`** — at most a name's [`MAX_NAME_SLOTS`] — through
/// `f`, each stretch of them contiguous on the device read once and written once.
pub fn rewrite<RW: BlockReader + BlockWriter>(
    rw: &RW,
    g: &Geometry,
    cache: &mut Cache,
    dir: Dir,
    first: u32,
    last: u32,
    mut f: impl FnMut(u32, &mut [u8; 32]),
) -> Result<(), FsError> {
    if last < first || last - first >= MAX_NAME_SLOTS as u32 {
        return Err(FsError::Corrupt);
    }
    let mut buf = [0u8; MAX_NAME_SLOTS * ENTRY as usize];
    let mut s = first;
    while s <= last {
        let at = slot_byte(rw, g, cache, dir, s)?.ok_or(FsError::Corrupt)?;
        let mut k = 1;
        while s + k <= last && slot_byte(rw, g, cache, dir, s + k)? == Some(at + (k * ENTRY) as u64) {
            k += 1;
        }
        let bytes = &mut buf[..(k * ENTRY) as usize];
        rw.read_at(at, bytes)?;
        for (j, e) in bytes.chunks_exact_mut(ENTRY as usize).enumerate() {
            f(s + j as u32, e.try_into().unwrap());
        }
        rw.write_at(at, bytes)?;
        s += k;
    }
    Ok(())
}

/// **The cluster a directory's `..` names for `parent`**: `0` for the root, FAT32's included.
pub fn dotdot_cluster(g: &Geometry, parent: Dir) -> u32 {
    match parent {
        Dir::Chain(c) if !(g.kind == Kind::Fat32 && c == g.root_cluster) => c,
        _ => 0,
    }
}

/// **The directory `dir`'s `..` names**: its parent. `Corrupt` if its second entry is not `..`.
pub fn parent<R: BlockReader>(r: &R, g: &Geometry, cache: &mut Cache, dir: Dir) -> Result<Dir, FsError> {
    let Dir::Chain(_) = dir else {
        return Ok(Dir::Root);
    };
    if dir == Dir::root(g) {
        return Ok(dir);
    }
    let at = slot_byte(r, g, cache, dir, 1)?.ok_or(FsError::Corrupt)?;
    let mut e = [0u8; 32];
    r.read_at(at, &mut e)?;
    if &e[..11] != DOTDOT {
        return Err(FsError::Corrupt);
    }
    let hi = if g.kind == Kind::Fat32 { u16::from_le_bytes([e[20], e[21]]) as u32 } else { 0 };
    match (hi << 16) | u16::from_le_bytes([e[26], e[27]]) as u32 {
        0 => Ok(Dir::root(g)),
        c if g.valid_cluster(c) => Ok(Dir::Chain(c)),
        _ => Err(FsError::Corrupt),
    }
}

/// **Whether `dir` is the directory whose chain begins `ancestor`, or within it**: its `..`
/// followed to the root, bounded as a chain is.
pub fn within<R: BlockReader>(r: &R, g: &Geometry, cache: &mut Cache, dir: Dir, ancestor: u32) -> Result<bool, FsError> {
    let mut d = dir;
    for _ in 0..g.clusters {
        if d == Dir::Chain(ancestor) {
            return Ok(true);
        }
        if d == Dir::root(g) {
            return Ok(false);
        }
        d = parent(r, g, cache, d)?;
    }
    Err(FsError::Corrupt)
}
