//! The device registry's records, mirroring `kernel/src/libkern/device.rs` — what
//! `/dev/registry` serves (administration Part B) — and a reader for them.
//!
//! The layout asserts on both sides keep the structs in step; `cargo xtask abi-sync-check`
//! compares the discriminants and constants. Specified in `docs/spec/device-node.md`
//! § *The registry*.
//!
//! **Read with [`records`], which trusts the header's count, never the object's size.** A memory
//! object is page-rounded, so the bytes a lookup maps run on past the last record in zeros, and a
//! reader that divided the size by a record's would find phantom devices of kind `Unknown`.
//!
//! **The reader is here rather than in `libos`**, where this crate's rules would put a parser,
//! because its consumers sit below `libos`: `eshell` and `libsession` link `libkern` alone. It
//! allocates nothing — an iterator over the caller's bytes.

use core::mem::{align_of, offset_of, size_of};

use crate::abi::MAX_DEVICE_NAME;

/// What a node is, as the registry reports it.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DeviceKind {
    /// Registered without saying.
    #[default]
    Unknown = 0,
    /// A PCI(e) function.
    PciFunction = 1,
    /// A whole disk.
    Disk = 2,
    /// One partition of a disk.
    Partition = 3,
    /// Memory published as a disk: a bootloader module.
    RamDisk = 4,
    /// A keyboard.
    Keyboard = 5,
    /// A mouse, or anything that reports like one.
    Mouse = 6,
    /// The serial console.
    Console = 7,
}

impl DeviceKind {
    /// The discriminant, for the wire struct.
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Decode a discriminant; one this build does not name is `Unknown`.
    pub const fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::PciFunction,
            2 => Self::Disk,
            3 => Self::Partition,
            4 => Self::RamDisk,
            5 => Self::Keyboard,
            6 => Self::Mouse,
            7 => Self::Console,
            _ => Self::Unknown,
        }
    }
}

/// The first four bytes of a snapshot, `"DREG"` read as a little-endian `u32`.
pub const REGISTRY_MAGIC: u32 = 0x4745_5244;
/// The snapshot layout this build reads.
pub const REGISTRY_VERSION: u32 = 1;
/// [`DeviceRecord::served`] for a node no indexed path serves.
pub const NOT_SERVED: u32 = 0xFFFF_FFFF;
/// [`DeviceRecord::parent`] for a node that belongs to nothing else.
pub const NO_PARENT: u32 = 0xFFFF_FFFF;
/// [`DeviceRecord::outcome`]: no driver reported anything.
pub const OUTCOME_NONE: u32 = 0;
/// [`DeviceRecord::outcome`]: a driver took the function.
pub const OUTCOME_CLAIMED: u32 = 1;
/// [`DeviceRecord::outcome`]: a driver matched the function and gave it up.
pub const OUTCOME_DECLINED: u32 = 2;
/// Longest driver name served, in bytes.
pub const MAX_DRIVER_NAME: usize = 16;

/// The start of a snapshot.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RegistryHeader {
    /// [`REGISTRY_MAGIC`].
    pub magic: u32,
    /// [`REGISTRY_VERSION`].
    pub version: u32,
    /// How many records follow — the length.
    pub count: u32,
    /// `size_of::<DeviceRecord>()` as the kernel wrote it.
    pub record_size: u32,
}

const _: () = assert!(size_of::<RegistryHeader>() == 16);
const _: () = assert!(align_of::<RegistryHeader>() == 4);
const _: () = assert!(offset_of!(RegistryHeader, count) == 8);
const _: () = assert!(offset_of!(RegistryHeader, record_size) == 12);

/// One node of the device table. See the kernel's mirror for each field.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeviceRecord {
    /// Its place in the table: `/dev/registry/<id>` is this node.
    pub id: u32,
    /// Its `DeviceClass` discriminant: 0 other, 1 block, 2 char.
    pub class: u32,
    /// Its [`DeviceKind`] discriminant.
    pub kind: u32,
    /// The `<n>` of `/dev/blk/<n>` or `/dev/input/raw/<n>`, or [`NOT_SERVED`].
    pub served: u32,
    /// The `id` of the node it belongs to, or [`NO_PARENT`].
    pub parent: u32,
    /// For a PCI function, what its driver did with it.
    pub outcome: u32,
    /// PCI vendor id; `0xFFFF` for a node that is not a PCI function.
    pub vendor: u16,
    /// PCI device id.
    pub device: u16,
    /// PCI base class.
    pub pci_class: u8,
    /// PCI subclass.
    pub subclass: u8,
    /// PCI programming interface.
    pub prog_if: u8,
    /// PCI revision.
    pub revision: u8,
    /// PCIe segment group.
    pub seg: u16,
    /// PCI bus.
    pub bus: u8,
    /// PCI device.
    pub dev: u8,
    /// PCI function.
    pub func: u8,
    /// Reserved; zero.
    pub _pad: [u8; 3],
    /// Bytes per logical block, for a block device.
    pub logical_block_size: u32,
    /// Bytes of [`name`](Self::name) that are meaningful.
    pub name_len: u32,
    /// Logical blocks, for a block device.
    pub block_count: u64,
    /// The driver that published the node or took the function, NUL-padded.
    pub driver: [u8; MAX_DRIVER_NAME],
    /// What to call it.
    pub name: [u8; MAX_DEVICE_NAME],
}

const _: () = assert!(size_of::<DeviceRecord>() == 144);
const _: () = assert!(align_of::<DeviceRecord>() == 8);
const _: () = assert!(offset_of!(DeviceRecord, served) == 12);
const _: () = assert!(offset_of!(DeviceRecord, parent) == 16);
const _: () = assert!(offset_of!(DeviceRecord, vendor) == 24);
const _: () = assert!(offset_of!(DeviceRecord, seg) == 32);
const _: () = assert!(offset_of!(DeviceRecord, logical_block_size) == 40);
const _: () = assert!(offset_of!(DeviceRecord, block_count) == 48);
const _: () = assert!(offset_of!(DeviceRecord, driver) == 56);
const _: () = assert!(offset_of!(DeviceRecord, name) == 72);

impl DeviceRecord {
    /// Its kind.
    pub fn kind(&self) -> DeviceKind {
        DeviceKind::from_u32(self.kind)
    }

    /// Its name's meaningful bytes.
    pub fn name(&self) -> &[u8] {
        &self.name[..(self.name_len as usize).min(MAX_DEVICE_NAME)]
    }

    /// Its driver's name, without the padding.
    pub fn driver(&self) -> &[u8] {
        let n = self.driver.iter().position(|&b| b == 0).unwrap_or(MAX_DRIVER_NAME);
        &self.driver[..n]
    }
}

/// Why a snapshot would not read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SnapshotError {
    /// Shorter than a header.
    Short,
    /// Not a registry snapshot.
    BadMagic,
    /// A layout this build does not read.
    BadVersion,
    /// Records of another size than this build's.
    BadRecordSize,
    /// The header counts more records than the bytes hold.
    Truncated,
}

/// The records of a snapshot, in table order.
pub struct Records<'a> {
    bytes: &'a [u8],
    count: usize,
    next: usize,
}

impl Iterator for Records<'_> {
    type Item = DeviceRecord;

    fn next(&mut self) -> Option<DeviceRecord> {
        if self.next >= self.count {
            return None;
        }
        let off = size_of::<RegistryHeader>() + self.next * size_of::<DeviceRecord>();
        self.next += 1;
        // `records` checked every counted record fits, so this slice is whole.
        let raw = &self.bytes[off..off + size_of::<DeviceRecord>()];
        // SAFETY: `raw` is exactly `size_of::<DeviceRecord>()` bytes, and every bit pattern is a
        // valid `DeviceRecord` (integers and byte arrays only). `read_unaligned` because a mapped
        // snapshot is only page-aligned by accident of how it was obtained.
        Some(unsafe { core::ptr::read_unaligned(raw.as_ptr().cast::<DeviceRecord>()) })
    }
}

impl Records<'_> {
    /// How many records the snapshot holds.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether it holds none.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// Read `bytes`, as `/dev/registry` serves them — padding and all.
pub fn records(bytes: &[u8]) -> Result<Records<'_>, SnapshotError> {
    if bytes.len() < size_of::<RegistryHeader>() {
        return Err(SnapshotError::Short);
    }
    let word = |off: usize| u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
    if word(0) != REGISTRY_MAGIC {
        return Err(SnapshotError::BadMagic);
    }
    if word(4) != REGISTRY_VERSION {
        return Err(SnapshotError::BadVersion);
    }
    if word(12) as usize != size_of::<DeviceRecord>() {
        return Err(SnapshotError::BadRecordSize);
    }
    let count = word(8) as usize;
    let need = count
        .checked_mul(size_of::<DeviceRecord>())
        .and_then(|n| n.checked_add(size_of::<RegistryHeader>()))
        .ok_or(SnapshotError::Truncated)?;
    if need > bytes.len() {
        return Err(SnapshotError::Truncated);
    }
    Ok(Records { bytes, count, next: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(count: u32) -> [u8; 16] {
        let mut h = [0u8; 16];
        h[0..4].copy_from_slice(&REGISTRY_MAGIC.to_le_bytes());
        h[4..8].copy_from_slice(&REGISTRY_VERSION.to_le_bytes());
        h[8..12].copy_from_slice(&count.to_le_bytes());
        h[12..16].copy_from_slice(&(size_of::<DeviceRecord>() as u32).to_le_bytes());
        h
    }

    fn record(id: u32, kind: DeviceKind) -> [u8; 144] {
        let mut r = [0u8; 144];
        r[0..4].copy_from_slice(&id.to_le_bytes());
        r[8..12].copy_from_slice(&kind.as_u32().to_le_bytes());
        r
    }

    /// **The header's count, not the page.** A page as the kernel hands it over — a header, two
    /// records, and zeros to the end — reads as exactly two. A reader that divided the size would
    /// find twenty-six, the rest of them all-zero `Unknown` devices (PR #332 review, finding 5).
    #[test]
    fn a_padded_snapshot_reads_exactly_its_count() {
        let mut page = [0u8; 4096];
        page[..16].copy_from_slice(&header(2));
        page[16..160].copy_from_slice(&record(0, DeviceKind::Disk));
        page[160..304].copy_from_slice(&record(1, DeviceKind::Keyboard));
        let rs = records(&page).unwrap();
        assert_eq!(rs.len(), 2);
        let kinds: [DeviceKind; 2] = {
            let mut it = records(&page).unwrap().map(|r| r.kind());
            [it.next().unwrap(), it.next().unwrap()]
        };
        assert_eq!(kinds, [DeviceKind::Disk, DeviceKind::Keyboard]);
        assert_eq!(records(&page).unwrap().count(), 2, "and no more after them");
    }

    /// **What a correct kernel never writes is refused, not guessed at**: a count the bytes cannot
    /// hold, another magic, another version, another record size, and fewer bytes than a header.
    #[test]
    fn a_snapshot_that_does_not_add_up_is_refused() {
        let mut short = [0u8; 16 + 144];
        short[..16].copy_from_slice(&header(2));
        assert_eq!(records(&short).err(), Some(SnapshotError::Truncated));
        let mut huge = [0u8; 16];
        huge.copy_from_slice(&header(u32::MAX));
        assert_eq!(records(&huge).err(), Some(SnapshotError::Truncated));
        let mut magic = header(0);
        magic[0] ^= 1;
        assert_eq!(records(&magic).err(), Some(SnapshotError::BadMagic));
        let mut version = header(0);
        version[4] = 2;
        assert_eq!(records(&version).err(), Some(SnapshotError::BadVersion));
        let mut size = header(0);
        size[12] = 1;
        assert_eq!(records(&size).err(), Some(SnapshotError::BadRecordSize));
        assert_eq!(records(&[0u8; 15]).err(), Some(SnapshotError::Short));
        assert_eq!(records(&header(0)).unwrap().len(), 0, "an empty table reads");
    }
}
