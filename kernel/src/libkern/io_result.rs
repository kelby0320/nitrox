//! [`IoResult`] — the per-signaled-handle completion record `sys_wait` writes.
//!
//! `sys_wait` writes one `IoResult` per signaled handle into the caller's
//! `results` array. It is a boundary type the kernel and userspace agree on;
//! its `#[repr(C)]` layout is part of the kernel ABI version hash (see
//! `docs/spec/abi-version-hash.md` § "IoOp and IoResult layouts"), so the
//! field offsets/sizes below are a contract — the compile-time asserts pin
//! them.
//!
//! Phase-1 minimal form: the signaled handle plus a status word. It grows
//! (without breaking the existing offsets) when richer waitables —
//! `PendingOperation`, IPC, notifications — land and need to report payloads.

/// One completion record written by `sys_wait` per signaled handle.
/// `#[repr(C)]`, 16 bytes, 8-byte aligned, no interior padding.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct IoResult {
    /// The signaled handle, as `RawHandle::bits()`.
    pub handle: u64,
    /// Completion status: `0` = signaled/ready. A negative value is a
    /// [`KError`](crate::syscall::error::KError) discriminant. Phase 1 only
    /// emits `0` (a Timer firing is an unconditional "ready").
    pub status: i32,
    /// Reserved; written as `0`. Keeps the record 16 bytes and 8-aligned and
    /// leaves room for future per-result flags without moving `handle`.
    pub reserved: u32,
}

const _: () = assert!(core::mem::size_of::<IoResult>() == 16);
const _: () = assert!(core::mem::align_of::<IoResult>() == 8);
const _: () = assert!(core::mem::offset_of!(IoResult, handle) == 0);
const _: () = assert!(core::mem::offset_of!(IoResult, status) == 8);
const _: () = assert!(core::mem::offset_of!(IoResult, reserved) == 12);

impl IoResult {
    /// A "ready" result for `handle` (status 0, reserved 0).
    pub const fn ready(handle: u64) -> Self {
        Self { handle, status: 0, reserved: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_stable() {
        assert_eq!(core::mem::size_of::<IoResult>(), 16);
        assert_eq!(core::mem::align_of::<IoResult>(), 8);
        assert_eq!(core::mem::offset_of!(IoResult, handle), 0);
        assert_eq!(core::mem::offset_of!(IoResult, status), 8);
        assert_eq!(core::mem::offset_of!(IoResult, reserved), 12);
    }

    #[test]
    fn ready_sets_handle_zero_status() {
        let r = IoResult::ready(0xABCD);
        assert_eq!(r.handle, 0xABCD);
        assert_eq!(r.status, 0);
        assert_eq!(r.reserved, 0);
    }
}
