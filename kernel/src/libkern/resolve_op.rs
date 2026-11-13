//! `ResolveOp`, `sys_ns_lookup`'s fifth argument, as `docs/spec/syscall-abi.md` lays it out.

/// **What a resolve does beyond resolving**; a field its `code` does not use must be `0`.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ResolveOp {
    /// A `RESOLVE_*` code.
    pub code: u32,
    /// A rename's flags.
    pub flags: u32,
    /// A size change's size.
    pub size: u64,
    /// A rename's destination path.
    pub dest_ptr: u64,
    /// Its length.
    pub dest_len: u64,
}
crate::libkern::pod::pod!(ResolveOp { code, flags, size, dest_ptr, dest_len });

/// Make the file, at least `size` bytes, and resolve it.
pub const RESOLVE_OP_CREATE: u32 = 1;
/// Resolve the file, grown to `size` first if it is shorter.
pub const RESOLVE_OP_GROW: u32 = 2;
/// Resolve the file, shrunk to `size` first if it is longer.
pub const RESOLVE_OP_TRUNCATE: u32 = 3;
/// Rename the path to the destination, on one filesystem.
pub const RESOLVE_OP_RENAME: u32 = 4;
/// Write back every dirty file of the mount the path resolves through: how many it wrote.
pub const RESOLVE_OP_FLUSH: u32 = 5;
/// How many of the mount's files something still holds.
pub const RESOLVE_OP_HELD: u32 = 6;

const _: () = {
    use core::mem::{align_of, offset_of, size_of};
    assert!(offset_of!(ResolveOp, code) == 0);
    assert!(offset_of!(ResolveOp, flags) == 4);
    assert!(offset_of!(ResolveOp, size) == 8);
    assert!(offset_of!(ResolveOp, dest_ptr) == 16);
    assert!(offset_of!(ResolveOp, dest_len) == 24);
    assert!(size_of::<ResolveOp>() == 32);
    assert!(align_of::<ResolveOp>() == 8);
};
