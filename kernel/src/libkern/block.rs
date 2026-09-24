//! What a block device is, for a program that has to choose one (Phase 5 Part H.1).
//!
//! `/dev/blk/<n>` hands out the *n*-th block device, and until now nothing said what that device
//! **was**: partitions and module RAM disks are registered in the same table as disks, so an
//! installer given an index could write a partition table over the running root — which the Part H
//! detail pass's own example command would have done (PR #307 review). Nor was there a capacity or
//! a name, so "refuse anything that is not a whole disk" and "type the target's identity back"
//! were both unimplementable.
//!
//! The facts live here rather than in the installer because every tool
//! `docs/planning/administration.md` describes needs the same three: a partitioner, a mount tool
//! and a disk list all have to tell a disk from a slice of one, say how big it is, and name it to a
//! person.
//!
//! Served as one `BlockDeviceInfo` at `/dev/blk/<n>/info`, the shape `/dev/framebuffer/info`
//! already uses. `#[repr(C)]` and mirrored byte-for-byte in `userspace/libkern/src/abi.rs`; the
//! layout asserts on both sides are what keeps them in step.

use core::mem::{align_of, offset_of, size_of};

/// Longest device name served, in bytes. ATA's model field is 40 and its serial 20, so a disk's
/// `model (serial)` fits with room for the space and brackets; a partition's is its GPT label,
/// capped at the same length by the same field.
pub const MAX_DEVICE_NAME: usize = 72;

/// What kind of block device a node is. **Not a hint**: the installer refuses to partition
/// anything that is not [`Disk`](BlockKind::Disk), so a wrong answer here writes over a
/// filesystem.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BlockKind {
    /// A driver claimed it but said nothing about what it is. Refused by anything destructive.
    #[default]
    Unknown = 0,
    /// A whole disk — the thing a partition table goes on.
    Disk = 1,
    /// One partition of a disk, published by the GPT scan.
    Partition = 2,
    /// Memory published as a disk: a bootloader module. Vanishes at power-off, so nothing
    /// installed should ever be written to one.
    RamDisk = 3,
}

impl BlockKind {
    /// The discriminant, for the wire struct.
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Decode a discriminant; one this kernel does not name is `Unknown`.
    pub const fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::Disk,
            2 => Self::Partition,
            3 => Self::RamDisk,
            _ => Self::Unknown,
        }
    }

    /// A word for a log line or a disk list.
    pub const fn name(self) -> &'static str {
        match self {
            BlockKind::Unknown => "unknown",
            BlockKind::Disk => "disk",
            BlockKind::Partition => "partition",
            BlockKind::RamDisk => "ram disk",
        }
    }
}

/// What `/dev/blk/<n>/info` serves: one of these, then zero padding to the page.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BlockDeviceInfo {
    /// A [`BlockKind`] discriminant. A value this kernel does not name reads as `Unknown`, which
    /// is the safe answer for a reader that refuses what it does not recognise.
    pub kind: u32,
    /// Bytes per logical block — 512 or 4096.
    pub logical_block_size: u32,
    /// Total addressable logical blocks. The capacity in bytes is this times the block size, and
    /// `sys_handle_stat` reports that product as the handle's `size`.
    pub block_count: u64,
    /// Bytes of [`name`](Self::name) that are meaningful.
    pub name_len: u32,
    /// Reserved; zero.
    pub _reserved: u32,
    /// What to call it: a disk's model and serial, a partition's label, a module's path. **Not a
    /// path and not an identifier** — it exists so a person confirming a destructive operation
    /// recognises the thing they are about to lose.
    pub name: [u8; MAX_DEVICE_NAME],
}

const _: () = assert!(size_of::<BlockDeviceInfo>() == 96);
const _: () = assert!(align_of::<BlockDeviceInfo>() == 8);
const _: () = assert!(offset_of!(BlockDeviceInfo, kind) == 0);
const _: () = assert!(offset_of!(BlockDeviceInfo, logical_block_size) == 4);
const _: () = assert!(offset_of!(BlockDeviceInfo, block_count) == 8);
const _: () = assert!(offset_of!(BlockDeviceInfo, name_len) == 16);
const _: () = assert!(offset_of!(BlockDeviceInfo, _reserved) == 20);
const _: () = assert!(offset_of!(BlockDeviceInfo, name) == 24);

impl Default for BlockDeviceInfo {
    fn default() -> Self {
        Self {
            kind: BlockKind::Unknown.as_u32(),
            logical_block_size: 0,
            block_count: 0,
            name_len: 0,
            _reserved: 0,
            name: [0; MAX_DEVICE_NAME],
        }
    }
}

impl BlockDeviceInfo {
    /// Build an info record, cutting `name` to what the field holds.
    ///
    /// **Cut rather than refused**: a name is for recognising a device, and a device with an
    /// over-long model is still a device somebody has to choose. Nothing parses this field.
    pub fn new(kind: BlockKind, logical_block_size: u32, block_count: u64, name: &[u8]) -> Self {
        let mut info = Self {
            kind: kind.as_u32(),
            logical_block_size,
            block_count,
            ..Self::default()
        };
        let n = name.len().min(MAX_DEVICE_NAME);
        info.name[..n].copy_from_slice(&name[..n]);
        info.name_len = n as u32;
        info
    }

    /// The name, as the bytes that are meaningful.
    pub fn name(&self) -> &[u8] {
        &self.name[..(self.name_len as usize).min(MAX_DEVICE_NAME)]
    }

    /// Capacity in bytes — what `sys_handle_stat` reports for a block device handle.
    ///
    /// Saturating, because the product of two fields a driver filled in is not a place to trust
    /// arithmetic: a bogus geometry should read as an implausible size, not wrap to a small one.
    pub fn byte_capacity(&self) -> u64 {
        self.block_count.saturating_mul(self.logical_block_size as u64)
    }

    /// Reinterpret as bytes, for copying into a `MemoryObject`.
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `Self` is `#[repr(C)]` with every offset and the total size pinned by the
        // asserts above, holds no pointers, and is valid for `size_of::<Self>()` bytes.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, size_of::<Self>()) }
    }
}

/// A bounded writer for building a device name with `write!`, for a driver that has to compose
/// one. Anything past the buffer is dropped: a name is for recognising a device, and a cut one
/// still does that.
pub struct NameBuf<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> NameBuf<'a> {
    /// Write into `buf`, from its start.
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }

    /// How many bytes were written.
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` if nothing was written.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl core::fmt::Write for NameBuf<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::fmt::Write;

    #[test]
    fn a_name_longer_than_the_field_is_cut_not_refused() {
        let long = [b'x'; MAX_DEVICE_NAME + 16];
        let info = BlockDeviceInfo::new(BlockKind::Disk, 512, 8, &long);
        assert_eq!(info.name().len(), MAX_DEVICE_NAME);
        assert_eq!(info.name_len as usize, MAX_DEVICE_NAME);
    }

    #[test]
    fn the_name_is_what_was_given_and_the_rest_stays_zero() {
        let info = BlockDeviceInfo::new(BlockKind::Disk, 512, 8, b"ST1000LM035");
        assert_eq!(info.name(), b"ST1000LM035");
        assert!(info.name[11..].iter().all(|&b| b == 0), "no stale bytes past the name");
    }

    /// The product is the capacity every caller wants, and a driver that filled the geometry in
    /// wrongly must not produce a *small* number — which is what a wrap would do.
    #[test]
    fn capacity_is_the_product_and_saturates_rather_than_wrapping() {
        let disk = BlockDeviceInfo::new(BlockKind::Disk, 512, 1_953_525_168, b"");
        assert_eq!(disk.byte_capacity(), 1_000_204_886_016);
        let absurd = BlockDeviceInfo::new(BlockKind::Disk, u32::MAX, u64::MAX, b"");
        assert_eq!(absurd.byte_capacity(), u64::MAX);
    }

    #[test]
    fn a_name_buffer_keeps_what_fits_and_drops_the_rest() {
        let mut buf = [0u8; 16];
        let mut w = NameBuf::new(&mut buf);
        write!(w, "partition {}", 3).unwrap();
        assert_eq!(w.len(), 11);
        assert_eq!(&buf[..11], b"partition 3");

        let mut small = [0u8; 4];
        let mut w = NameBuf::new(&mut small);
        write!(w, "partition {}", 3).unwrap();
        assert_eq!(w.len(), 4, "a cut name is still a name");
        assert_eq!(&small, b"part");
    }

    #[test]
    fn as_bytes_round_trips_the_declared_layout() {
        let info = BlockDeviceInfo::new(BlockKind::Partition, 512, 1024, b"nitrox-root");
        let b = info.as_bytes();
        assert_eq!(b.len(), size_of::<BlockDeviceInfo>());
        assert_eq!(u32::from_le_bytes(b[0..4].try_into().unwrap()), BlockKind::Partition.as_u32());
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 512);
        assert_eq!(u64::from_le_bytes(b[8..16].try_into().unwrap()), 1024);
        assert_eq!(u32::from_le_bytes(b[16..20].try_into().unwrap()), 11);
        assert_eq!(&b[24..24 + 11], b"nitrox-root");
        // Every byte is accounted for by a field, so a zeroed record serialises as zeros.
        assert!(BlockDeviceInfo::default().as_bytes().iter().all(|&b| b == 0));
    }
}
