//! `IoOp` — the `sys_io_submit` operation descriptor (kernel mirror).
//!
//! Normative layout: `docs/spec/io-operation.md`. This is the kernel's copy of
//! the `#[repr(C)]` block userspace passes by `UserPtr<IoOp>`; `userspace/libkern`
//! carries the matching mirror. Both are ABI version-hash inputs
//! (`docs/spec/abi-version-hash.md` § "IoOp and IoResult layouts").

/// One asynchronous I/O operation. `#[repr(C)]`, 40 bytes, 8-byte aligned, no
/// interior padding (pinned by the asserts below).
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct IoOp {
    /// [`IoOpcode`] discriminant.
    pub opcode: u32,
    /// Reserved; must be 0.
    pub flags: u32,
    /// `MemoryObject` handle providing the data buffer (`RawHandle` value).
    pub buffer: u64,
    /// Byte offset within `buffer`.
    pub buf_offset: u64,
    /// Byte offset within the resource (the device).
    pub offset: u64,
    /// Bytes to transfer.
    pub length: u64,
}

/// [`IoOpcode::Read`]'s value — named, so `abi-sync-check` can pair it with `libkern`'s.
pub const IO_OPCODE_READ: u32 = 0;
/// [`IoOpcode::Write`]'s value.
pub const IO_OPCODE_WRITE: u32 = 1;
/// [`IoOpcode::Flush`]'s value.
pub const IO_OPCODE_FLUSH: u32 = 2;
/// [`IoOpcode::Rescan`]'s value.
pub const IO_OPCODE_RESCAN: u32 = 3;

/// The operation selector. `#[repr(u32)]`; part of the ABI version hash. Each value is stated
/// once, above, and the variants take it.
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IoOpcode {
    /// Device → buffer.
    Read = IO_OPCODE_READ,
    /// Buffer → device.
    Write = IO_OPCODE_WRITE,
    /// **Make what has been written durable** — the device's volatile write cache written to
    /// its medium (administration Part C.2). No buffer and no range: `buffer`, `buf_offset`,
    /// `offset` and `length` are all `0`. Needs `WRITE` on the device, since only a writer
    /// has anything to make durable.
    Flush = IO_OPCODE_FLUSH,
    /// **Read a disk's partition table again** and publish what it says now (Phase 6 Part G): its
    /// old partitions' windows retired and their records departed, the new ones published as
    /// arrivals. No buffer and no range, as for [`Flush`](Self::Flush), and needs `WRITE` on the
    /// disk. **A partition answers `Unsupported`**, as does a disk whose driver cannot read its
    /// table again — today every one but a USB disk.
    Rescan = IO_OPCODE_RESCAN,
}

impl IoOpcode {
    /// Decode a `u32` discriminant, or `None` if unrecognised.
    pub const fn from_u32(v: u32) -> Option<Self> {
        match v {
            IO_OPCODE_READ => Some(Self::Read),
            IO_OPCODE_WRITE => Some(Self::Write),
            IO_OPCODE_FLUSH => Some(Self::Flush),
            IO_OPCODE_RESCAN => Some(Self::Rescan),
            _ => None,
        }
    }
}

const _: () = {
    use core::mem::{align_of, offset_of, size_of};
    assert!(offset_of!(IoOp, opcode) == 0);
    assert!(offset_of!(IoOp, flags) == 4);
    assert!(offset_of!(IoOp, buffer) == 8);
    assert!(offset_of!(IoOp, buf_offset) == 16);
    assert!(offset_of!(IoOp, offset) == 24);
    assert!(offset_of!(IoOp, length) == 32);
    assert!(size_of::<IoOp>() == 40);
    assert!(align_of::<IoOp>() == 8);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcode_round_trips() {
        assert_eq!(IoOpcode::from_u32(0), Some(IoOpcode::Read));
        assert_eq!(IoOpcode::from_u32(1), Some(IoOpcode::Write));
        assert_eq!(IoOpcode::from_u32(2), Some(IoOpcode::Flush));
        assert_eq!(IoOpcode::from_u32(3), Some(IoOpcode::Rescan));
        assert_eq!(IoOpcode::from_u32(4), None, "the first value no opcode has");
    }
}
