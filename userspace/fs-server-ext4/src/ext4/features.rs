//! The superblock's feature words: what this server reads, what its writes keep, and their names.

const FILETYPE: u32 = 0x0002;
const EXTENTS: u32 = 0x0040;

/// `s_feature_incompat` bits this reader reads: typed entries, extents, and `flex_bg`, since it
/// finds every table through its descriptor. Any other is a layout or encoding it would misread.
pub(super) const INCOMPAT_READ: u32 = FILETYPE | EXTENTS | 0x0200;
/// `needs_recovery`: the journal holds changes not yet replayed, which a write here would undercut.
pub(super) const INCOMPAT_RECOVER: u32 = 0x0004;
/// What every write here makes, so a filesystem must declare it: typed entries, and extents.
pub(super) const INCOMPAT_WRITTEN: u32 = FILETYPE | EXTENTS;
/// `s_feature_ro_compat` bits this writer keeps. Any other is state a write would leave wrong, a
/// checksum (`metadata_csum`, `uninit_bg`), a cluster or a quota, so its filesystem is only read.
pub(super) const RO_COMPAT_KEPT: u32 = 0x0001 | 0x0002 | 0x0008 | 0x0020 | 0x0040;

const INCOMPAT_NAMES: [(u32, &str); 13] = [
    (0x0001, "compression"),
    (FILETYPE, "filetype"),
    (0x0008, "journal_dev"),
    (0x0010, "meta_bg"),
    (EXTENTS, "extent"),
    (0x0100, "mmp"),
    (0x0400, "ea_inode"),
    (0x1000, "dirdata"),
    (0x2000, "metadata_csum_seed"),
    (0x4000, "large_dir"),
    (0x8000, "inline_data"),
    (0x1_0000, "encrypt"),
    (0x2_0000, "casefold"),
];

const RO_COMPAT_NAMES: [(u32, &str); 10] = [
    (0x0010, "uninit_bg"),
    (0x0100, "quota"),
    (0x0200, "bigalloc"),
    (0x0400, "metadata_csum"),
    (0x0800, "replica"),
    (0x1000, "read-only"),
    (0x2000, "project"),
    (0x4000, "shared_blocks"),
    (0x8000, "verity"),
    (0x1_0000, "orphan_present"),
];

/// Write the names `mke2fs -O` gives the bits set in `bits` of the `ro_compat` word, or else of the
/// `incompat` one, comma-separated, a bit with no name here in hex.
pub(super) fn names(f: &mut core::fmt::Formatter<'_>, bits: u32, ro_compat: bool) -> core::fmt::Result {
    let table: &[(u32, &str)] = if ro_compat { &RO_COMPAT_NAMES } else { &INCOMPAT_NAMES };
    let mut sep = "";
    let mut named = 0;
    for &(bit, name) in table.iter().filter(|(bit, _)| bits & bit != 0) {
        write!(f, "{sep}{name}")?;
        (sep, named) = (", ", named | bit);
    }
    match bits & !named {
        0 => Ok(()),
        rest => write!(f, "{sep}{rest:#x}"),
    }
}
