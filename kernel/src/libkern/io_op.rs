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

/// The operation selector. `#[repr(u32)]`; part of the ABI version hash.
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IoOpcode {
    /// Device → buffer.
    Read = 0,
    /// Buffer → device.
    Write = 1,
}

impl IoOpcode {
    /// Decode a `u32` discriminant, or `None` if unrecognised.
    pub const fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Read),
            1 => Some(Self::Write),
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
        assert_eq!(IoOpcode::from_u32(2), None);
    }
}
