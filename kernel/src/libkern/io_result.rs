//! [`IoResult`] — the per-signaled-handle completion record `sys_wait` writes.
//!
//! `sys_wait` writes one `IoResult` per signaled handle into the caller's
//! `results` array. It is a boundary type the kernel and userspace agree on;
//! its `#[repr(C)]` layout is part of the kernel ABI version hash (see
//! `docs/spec/abi-version-hash.md` § "IoOp and IoResult layouts"), so the
//! field offsets/sizes below are a contract — the compile-time asserts pin
//! them.
//!
//! Form: the signaled handle, a status word, and a `result` payload word. The
//! `result` was added with the namespace slice (Phase 2 slice 1) — a completion
//! that returns a *value* rather than just a status (a namespace lookup's
//! resolved handle) needs more than the `i32` `status` can hold. It sits past
//! `status`/`reserved` so the earlier offsets are unchanged; edge-style
//! waitables (Timer, channel, notification) report `result = 0`.

/// One completion record written by `sys_wait` per signaled handle.
/// `#[repr(C)]`, 24 bytes, 8-byte aligned, no interior padding.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct IoResult {
    /// The signaled handle, as `RawHandle::bits()`.
    pub handle: u64,
    /// Completion status: `0` = signaled/ready. A negative value is a
    /// [`KError`](crate::syscall::error::KError) discriminant (e.g. a namespace
    /// lookup that resolved to nothing reports `NotFound` here).
    pub status: i32,
    /// Reserved; written as `0`. Keeps `result` 8-aligned and leaves room for
    /// future per-result flags without moving `handle`/`status`.
    pub reserved: u32,
    /// Result payload, valid when `status == 0` for completions that return a
    /// value: a namespace lookup delivers its **resolved handle** here. `0` for
    /// edge-style waitables (Timer/channel/notification) and for any error
    /// completion. See `docs/spec/syscall-abi.md` § `sys_wait` / `sys_ns_lookup`.
    pub result: u64,
}

const _: () = assert!(core::mem::size_of::<IoResult>() == 24);
const _: () = assert!(core::mem::align_of::<IoResult>() == 8);
const _: () = assert!(core::mem::offset_of!(IoResult, handle) == 0);
const _: () = assert!(core::mem::offset_of!(IoResult, status) == 8);
const _: () = assert!(core::mem::offset_of!(IoResult, reserved) == 12);
const _: () = assert!(core::mem::offset_of!(IoResult, result) == 16);

impl IoResult {
    /// A "ready" result for `handle` (status 0, no payload). Used for edge-style
    /// waitables (a Timer firing, a channel going non-empty) that carry no
    /// operation status or value of their own.
    pub const fn ready(handle: u64) -> Self {
        Self { handle, status: 0, reserved: 0, result: 0 }
    }

    /// A completion result for `handle` carrying an operation `status` (`0` =
    /// success; a negative value is a [`KError`](crate::syscall::error::KError)
    /// discriminant) and no result payload. Used for a signaled
    /// [`PendingOperation`] whose completion reports only a status — e.g. the
    /// `TimedOut` / `PeerClosed` outcome of a blocking IPC send.
    ///
    /// [`PendingOperation`]: crate::object::PendingOperation
    pub const fn completed(handle: u64, status: i32) -> Self {
        Self { handle, status, reserved: 0, result: 0 }
    }

    /// A completion result for `handle` carrying both a `status` and a `result`
    /// payload. Used for a signaled [`PendingOperation`] whose completion returns
    /// a value — e.g. a namespace lookup, which delivers its resolved handle in
    /// `result` (with `status == 0`). On error `status` is the negative
    /// [`KError`](crate::syscall::error::KError) and `result` should be `0`.
    ///
    /// [`PendingOperation`]: crate::object::PendingOperation
    pub const fn completed_with_result(handle: u64, status: i32, result: u64) -> Self {
        Self { handle, status, reserved: 0, result }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_stable() {
        assert_eq!(core::mem::size_of::<IoResult>(), 24);
        assert_eq!(core::mem::align_of::<IoResult>(), 8);
        assert_eq!(core::mem::offset_of!(IoResult, handle), 0);
        assert_eq!(core::mem::offset_of!(IoResult, status), 8);
        assert_eq!(core::mem::offset_of!(IoResult, reserved), 12);
        assert_eq!(core::mem::offset_of!(IoResult, result), 16);
    }

    #[test]
    fn ready_sets_handle_zero_status() {
        let r = IoResult::ready(0xABCD);
        assert_eq!(r.handle, 0xABCD);
        assert_eq!(r.status, 0);
        assert_eq!(r.reserved, 0);
        assert_eq!(r.result, 0);
    }

    #[test]
    fn completed_with_result_carries_handle_and_payload() {
        let r = IoResult::completed_with_result(0x11, 0, 0xBEEF);
        assert_eq!(r.handle, 0x11);
        assert_eq!(r.status, 0);
        assert_eq!(r.reserved, 0);
        assert_eq!(r.result, 0xBEEF);
        // An error completion carries the negative status and no payload.
        let e = IoResult::completed_with_result(0x11, -10, 0);
        assert_eq!(e.status, -10);
        assert_eq!(e.result, 0);
    }
}
