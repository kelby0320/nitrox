//! **Partitioning and formatting, decided** (Phase 6 Part G.4): everything `disk --partition` and
//! `disk --format` decide before they write a byte — whether the target may be written at all,
//! which table, where its one partition goes, what is wiped, and the filesystem's label — from the
//! device's row in `/dev/devices` and the person's words. The binary writes; this decides, so each
//! refusal comes **before anything is written** and is a host test.
//!
//! - **The disk the machine started from is refused**, the target or its disk, by its `boot` flag:
//!   the `disks` grant withholds that disk and not its partitions (PR #368 review).
//! - **A disk must be removable** to be partitioned, or formatted whole: an internal disk is
//!   partitioned by `nxinstall`, which reboots after, since only a USB disk's table is read again.
//!   A partition of an internal disk may be formatted. **Nothing on a RAM disk is**: it is a
//!   module the bootloader loaded — the live root, or the installer's pristine copy (PR #369
//!   review).
//! - **The table follows the filesystem**: an MBR for FAT, which every camera and television
//!   reads, and a GPT for ext4; **a GPT for either from 2 TiB**, where an MBR cannot count, and
//!   `--mbr` there is refused, naming `--gpt` (PR #368 review). `--partition` alone types its
//!   partition for FAT, the default filesystem.
//! - **One partition, from 1 MiB** to the end of the disk — a GPT's to before its backup.
//! - **The first two mebibytes and the last are wiped** before a table is written: the old table's,
//!   an old filesystem at 1 MiB, and an old GPT's backup (PR #368 review).

use alloc::string::String;
use alloc::vec::Vec;
use libgpt::{mbr, table};

/// A sector, the unit every device here counts in.
pub const SECTOR: u64 = 512;
/// Where the one partition starts: 1 MiB.
pub const START: u64 = 2048;
/// What `--partition` wipes at the front of a disk: the table's mebibyte and the partition's first.
pub const FRONT_WIPE: u64 = 2 << 20;
/// What it wipes at the back: where a GPT keeps its backup.
pub const BACK_WIPE: u64 = 1 << 20;
/// Bytes a block of the ext4 `disk` makes — `nxinstall`'s.
pub const EXT4_BLOCK: u32 = 4096;
/// Bytes of the ext4 `disk` makes per inode — `nxinstall`'s.
pub const EXT4_BYTES_PER_INODE: u32 = 16384;
/// The label a FAT gets when none is given.
pub const FAT_LABEL: &str = "NITROX";
/// The label an ext4 gets when none is given.
pub const EXT4_LABEL: &str = "nitrox";

/// A filesystem `disk --format` makes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Fs {
    /// A FAT16 or FAT32, by size: `fs_server_fat::mkfs`.
    Fat,
    /// An ext4: `fs_server_ext4::mkfs`.
    Ext4,
}

impl Fs {
    /// The word on the command line, as the storage table names it.
    pub fn parse(word: &str) -> Option<Fs> {
        match word {
            "fat" => Some(Fs::Fat),
            "ext4" => Some(Fs::Ext4),
            _ => None,
        }
    }
}

/// A partition table.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Scheme {
    /// A master boot record: `libgpt::mbr`.
    Mbr,
    /// A GUID partition table: `libgpt::table`.
    Gpt,
}

/// What a block device is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A whole disk.
    Disk,
    /// A partition of a disk or a RAM disk.
    Partition,
    /// A module the bootloader loaded, published as a disk.
    RamDisk,
}

/// **A block device, as `/dev/devices` describes it**: its row, and for a partition its disk's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// `blk-<n>`.
    pub name: String,
    pub kind: Kind,
    /// Its length in sectors.
    pub sectors: u64,
    /// Whether it is behind USB mass storage — a partition's disk's driver for a partition.
    pub removable: bool,
    /// Whether it is on the disk the machine started from — a partition's disk's flag for a
    /// partition.
    pub boot: bool,
    /// Whether it is in memory: a RAM disk, or a partition of one.
    pub in_memory: bool,
    /// Its logical sector's bytes: 512 from [`target`], which the binary corrects from the
    /// device's own info before [`check`].
    pub sector_bytes: u32,
}

/// **The words a row has**, by the `/dev/devices` column names, for [`target`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    /// `blk-<n>`, or another device's name.
    pub name: String,
    /// `disk`, `partition`, `ramdisk`, or another device's kind.
    pub kind: String,
    /// Its size in bytes, if it has one.
    pub size: Option<i64>,
    /// The name of the device it belongs to.
    pub parent: Option<String>,
    /// The driver that published it.
    pub driver: Option<String>,
    /// Whether it is the disk the machine started from.
    pub boot: bool,
}

/// The driver a disk behind USB mass storage is published by.
pub const USB_STORAGE: &str = "usb-storage";

/// **The target called `name`** among `rows`, a partition's disk's driver and flag folded in; or
/// `None` if no block device is called that.
pub fn target(rows: &[Row], name: &str) -> Option<Target> {
    let r = rows.iter().find(|r| r.name == name)?;
    let kind = match r.kind.as_str() {
        "disk" => Kind::Disk,
        "partition" => Kind::Partition,
        "ramdisk" => Kind::RamDisk,
        _ => return None,
    };
    let disk = if kind == Kind::Partition { r.parent.as_ref().and_then(|p| rows.iter().find(|d| &d.name == p)) } else { None };
    let usb = |r: &Row| r.driver.as_deref() == Some(USB_STORAGE);
    Some(Target {
        name: r.name.clone(),
        kind,
        sectors: r.size.unwrap_or(0).max(0) as u64 / SECTOR,
        removable: usb(r) || disk.is_some_and(usb),
        boot: r.boot || disk.is_some_and(|d| d.boot),
        in_memory: kind == Kind::RamDisk || disk.is_some_and(|d| d.kind == "ramdisk"),
        sector_bytes: SECTOR as u32,
    })
}

/// **The partitions of `disk`** among `rows`, by name: what a rescan published.
pub fn partitions_of(rows: &[Row], disk: &str) -> Vec<String> {
    rows.iter().filter(|r| r.kind == "partition" && r.parent.as_deref() == Some(disk)).map(|r| r.name.clone()).collect()
}

/// Why nothing was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// It, or its disk, is the disk the machine started from.
    BootDisk,
    /// It is a RAM disk, or on one.
    InMemory,
    /// A disk not behind USB mass storage, for a table.
    NotRemovable,
    /// `--partition` of a partition.
    NotADisk,
    /// `--mbr` or `--gpt` with a partition: its table is there already.
    TableOnPartition,
    /// Both `--mbr` and `--gpt`.
    BothTables,
    /// `--mbr` on a disk of 2 TiB or more.
    MbrTooLarge,
    /// Logical sectors other than 512 bytes, which every layout here counts in.
    SectorSize(u32),
    /// Too small for a partition at 1 MiB, or for the filesystem; or too large for it.
    TooSmall(String),
    /// A label the filesystem cannot hold.
    Label(String),
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Refusal::BootDisk => write!(f, "it is on the disk the machine started from"),
            Refusal::InMemory => write!(f, "it is in memory, a module the bootloader loaded, not a disk"),
            Refusal::NotRemovable => write!(
                f,
                "it is not a removable disk: only a USB disk's table is read again, and an internal disk is partitioned by nxinstall"
            ),
            Refusal::NotADisk => write!(f, "it is a partition: a partition table goes on a disk"),
            Refusal::TableOnPartition => write!(f, "it is a partition, whose table is its disk's: --mbr and --gpt are for a disk"),
            Refusal::BothTables => write!(f, "--mbr and --gpt are one or the other"),
            Refusal::MbrTooLarge => write!(f, "an MBR cannot count past 2 TiB, and this disk is larger: use --gpt"),
            Refusal::SectorSize(n) => write!(f, "its sectors are {n} bytes, and only 512-byte sectors are written here"),
            Refusal::TooSmall(why) | Refusal::Label(why) => write!(f, "{why}"),
        }
    }
}

/// **The table for a disk of `sectors`**: `--mbr` or `--gpt`, else the default, which follows the
/// filesystem — `None` for `--partition` alone, typed for FAT — and is a GPT from 2 TiB.
pub fn scheme(fs: Option<Fs>, sectors: u64, mbr: bool, gpt: bool) -> Result<Scheme, Refusal> {
    let fits = sectors <= mbr::MAX_BLOCKS;
    match (mbr, gpt) {
        (true, true) => Err(Refusal::BothTables),
        (true, false) if !fits => Err(Refusal::MbrTooLarge),
        (true, false) => Ok(Scheme::Mbr),
        (false, true) => Ok(Scheme::Gpt),
        (false, false) if !fits || fs == Some(Fs::Ext4) => Ok(Scheme::Gpt),
        (false, false) => Ok(Scheme::Mbr),
    }
}

/// **The one partition**, as `(first sector, sectors)`: from [`START`] to the disk's end, or to a
/// GPT's last usable sector, before its backup. `None` if that leaves nothing.
pub fn extent(scheme: Scheme, sectors: u64) -> Option<(u64, u64)> {
    let end = match scheme {
        Scheme::Mbr => sectors,
        Scheme::Gpt => sectors.checked_sub(table::ARRAY_BLOCKS + 1)?,
    };
    end.checked_sub(START).filter(|&n| n > 0).map(|n| (START, n))
}

/// **What a table's writing wipes first**, as byte ranges `(offset, length)` on a disk of `sectors`:
/// the first two mebibytes and the last, each clamped to the disk.
pub fn wipes(sectors: u64) -> [(u64, u64); 2] {
    let bytes = sectors * SECTOR;
    let front = FRONT_WIPE.min(bytes);
    let back = BACK_WIPE.min(bytes);
    [(0, front), (bytes - back, back)]
}

/// **What the storage service can name a mount by** (PR #369 review): a label beginning with `.` or
/// a space, or ending with a space, would be passed over for a fallback name, so it is refused here,
/// before anything is written, rather than met as a surprise in `/storage`. Its rule is
/// `storage_service::labels::valid`; an empty label is none, and named by the partition.
fn mount_name(text: &str) -> Result<(), Refusal> {
    let bad = text.starts_with('.') || text.starts_with(' ') || text.ends_with(' ');
    if bad {
        return Err(Refusal::Label(String::from(
            "a label is what the stick is mounted under, so it cannot begin with '.' or a space, or end with a space",
        )));
    }
    Ok(())
}

/// **A FAT label** as `fs-server-fat`'s formatter keeps it, the default for none.
pub fn fat_label(given: Option<&str>) -> Result<[u8; 11], Refusal> {
    let text = given.unwrap_or(FAT_LABEL);
    mount_name(text)?;
    fs_server_fat::mkfs::label(text.as_bytes()).map_err(|e| Refusal::Label(alloc::format!("{e}")))
}

/// **An ext4 label**, NUL-padded to its 16 bytes, the default for none: printable ASCII a person
/// can type and no `/`, as the storage service names a mount by it — refused past 16 bytes.
pub fn ext4_label(given: Option<&str>) -> Result<[u8; 16], Refusal> {
    let text = given.unwrap_or(EXT4_LABEL);
    mount_name(text)?;
    let text = text.as_bytes();
    if text.len() > 16 {
        return Err(Refusal::Label(String::from("an ext4 label is at most 16 characters")));
    }
    if let Some(&c) = text.iter().find(|&&c| !(0x20..0x7F).contains(&c) || c == b'/') {
        return Err(Refusal::Label(if c.is_ascii_graphic() {
            alloc::format!("an ext4 label here cannot hold {:?}", c as char)
        } else {
            alloc::format!("an ext4 label here cannot hold the byte {c:#04x}")
        }));
    }
    let mut out = [0u8; 16];
    out[..text.len()].copy_from_slice(text);
    Ok(out)
}

/// **May `t` be written, by `--partition` or a `--format`?** The disk the machine started from
/// never, nor anything in memory, nor a disk of other than 512-byte sectors; a table only on a
/// removable disk, and only on a disk.
pub fn check(t: &Target, table: bool) -> Result<(), Refusal> {
    if t.boot {
        return Err(Refusal::BootDisk);
    }
    if t.in_memory {
        return Err(Refusal::InMemory);
    }
    if t.sector_bytes != SECTOR as u32 {
        return Err(Refusal::SectorSize(t.sector_bytes));
    }
    let whole = t.kind != Kind::Partition;
    if table && !whole {
        return Err(Refusal::NotADisk);
    }
    if whole && !t.removable {
        return Err(Refusal::NotRemovable);
    }
    Ok(())
}

/// **What `--partition` or `--format` will write**, decided whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The table, and its one partition's first sector and length, for a disk.
    pub table: Option<(Scheme, u64, u64)>,
    /// The filesystem, its label, and where it goes on the device, in bytes `(offset, length)`.
    pub fs: Option<(Fs, Label, u64, u64)>,
}

/// A filesystem's label, in its own form.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Label {
    /// A FAT's, uppercased and space-padded.
    Fat([u8; 11]),
    /// An ext4's, NUL-padded.
    Ext4([u8; 16]),
}

/// **`disk --partition`'s plan** for `t`.
pub fn partition(t: &Target, mbr: bool, gpt: bool) -> Result<Plan, Refusal> {
    check(t, true)?;
    let s = scheme(None, t.sectors, mbr, gpt)?;
    let (first, n) = extent(s, t.sectors).ok_or_else(|| Refusal::TooSmall(String::from("it is too small for a partition at 1 MiB")))?;
    Ok(Plan { table: Some((s, first, n)), fs: None })
}

/// **`disk --format`'s plan** for `t`: on a partition, the filesystem filling it; on a disk, the
/// default table first and the filesystem in its one partition. The filesystem's geometry is
/// worked out here, so one too small is refused before a table is written for it.
pub fn format(t: &Target, fs: Fs, label: Option<&str>, mbr: bool, gpt: bool) -> Result<Plan, Refusal> {
    check(t, false)?;
    let (table, offset, sectors) = if t.kind == Kind::Partition {
        if mbr || gpt {
            return Err(Refusal::TableOnPartition);
        }
        (None, 0, t.sectors)
    } else {
        let s = scheme(Some(fs), t.sectors, mbr, gpt)?;
        let (first, n) =
            extent(s, t.sectors).ok_or_else(|| Refusal::TooSmall(String::from("it is too small for a partition at 1 MiB")))?;
        (Some((s, first, n)), first * SECTOR, n)
    };
    let label = match fs {
        Fs::Fat => {
            let l = fat_label(label)?;
            let p = fs_server_fat::mkfs::Params { sectors, label: l, volume_id: 0, hidden: 0, now: 0 };
            fs_server_fat::mkfs::plan(&p).map_err(|e| Refusal::TooSmall(alloc::format!("{e}")))?;
            Label::Fat(l)
        }
        Fs::Ext4 => {
            let l = ext4_label(label)?;
            ext4_geometry(sectors * SECTOR, l, [0; 16], 0).map_err(ext4_refusal)?;
            Label::Ext4(l)
        }
    };
    Ok(Plan { table, fs: Some((fs, label, offset, sectors * SECTOR)) })
}

/// The ext4 `disk` makes in `bytes`: [`EXT4_BLOCK`]-byte blocks, an inode per
/// [`EXT4_BYTES_PER_INODE`].
pub fn ext4_params(bytes: u64, label: [u8; 16], uuid: [u8; 16], now: i64) -> fs_server_ext4::mkfs::Params {
    fs_server_ext4::mkfs::Params {
        blocks: bytes / EXT4_BLOCK as u64,
        block_size: EXT4_BLOCK,
        bytes_per_inode: EXT4_BYTES_PER_INODE,
        uuid,
        label,
        now,
    }
}

/// What a person is told for an ext4 that cannot be laid out.
fn ext4_refusal(e: fs_server_ext4::mkfs::MkfsError) -> Refusal {
    use fs_server_ext4::mkfs::MkfsError;
    Refusal::TooSmall(String::from(match e {
        MkfsError::TooSmall { .. } => "it is too small for an ext4",
        MkfsError::TooLarge => "it is too large for the ext4 made here, which holds at most 16 TiB",
        MkfsError::BlockSize(_) => "the ext4 made here has no such block size",
    }))
}

fn ext4_geometry(
    bytes: u64,
    label: [u8; 16],
    uuid: [u8; 16],
    now: i64,
) -> Result<fs_server_ext4::mkfs::Geometry, fs_server_ext4::mkfs::MkfsError> {
    fs_server_ext4::mkfs::Geometry::new(&ext4_params(bytes, label, uuid, now))
}

/// **The MBR type byte** for a partition holding `fs`, or the FAT `--partition` types for: FAT32's
/// or FAT16's by the FAT that will be made, and Linux's for ext4.
pub fn mbr_type(fs: Option<Fs>, fat_kind: Option<fs_server_fat::bpb::Kind>) -> u8 {
    match (fs, fat_kind) {
        (Some(Fs::Ext4), _) => mbr::TYPE_LINUX,
        (_, Some(fs_server_fat::bpb::Kind::Fat16)) => mbr::TYPE_FAT16_LBA,
        _ => mbr::TYPE_FAT32_LBA,
    }
}

/// **The GPT type** for a partition holding `fs`: Microsoft basic data for FAT, and for
/// `--partition` alone; Linux filesystem for ext4.
pub fn gpt_type(fs: Option<Fs>) -> [u8; 16] {
    if fs == Some(Fs::Ext4) { table::TYPE_LINUX_FS } else { table::TYPE_BASIC_DATA }
}

#[cfg(test)]
mod tests;
