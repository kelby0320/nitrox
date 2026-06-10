//! IPC wire types — [`IpcMsg`], [`IpcMsgHeader`], and [`SendMode`].
//!
//! These are the kernel-level message envelope that `sys_channel_send` /
//! `sys_channel_recv` move across an [`IpcChannel`](crate::object::IpcChannel)
//! endpoint pair. `docs/spec/ipc-message-format.md` is the normative source;
//! this module is its in-kernel embodiment (the value types only — the channel
//! object lives in [`crate::object::ipc_channel`]).
//!
//! ## Layout
//!
//! An [`IpcMsg`] is **exactly one page** (4096 bytes), `#[repr(C, align(4096))]`:
//! a 24-byte [`IpcMsgHeader`], a 4008-byte inline `payload`, then an 8-entry
//! transferable-handle array. (The spec's earlier draft listed a 4032-byte
//! payload, which made the three regions sum to 4120 ≠ 4096; this reconciles it
//! to a clean one-page envelope — `payload = 4096 − 24 − 64 = 4008`. Source
//! wins; the spec doc is updated to match. See the decision log.)
//!
//! ## ABI
//!
//! `IpcMsg` / `IpcMsgHeader` / `SendMode` cross the kernel/userspace boundary,
//! so their layouts are kernel-ABI-hash inputs (like
//! [`Notification`](crate::libkern::Notification) /
//! [`IoResult`](crate::libkern::IoResult)). The hash is not yet computed in
//! code, so nothing is enforced today — but the offsets here are a contract and
//! the compile-time asserts pin them.

use crate::libkern::handle::RawHandle;

/// Total size of an [`IpcMsg`], in bytes — one page on x86_64.
pub const IPC_MSG_SIZE: usize = 4096;
/// Size of the [`IpcMsgHeader`] prefix, in bytes.
pub const IPC_HEADER_SIZE: usize = 24;
/// Maximum transferable handles carried by one message.
pub const IPC_HANDLE_MAX: usize = 8;
/// Bytes of inline payload per message (`IPC_MSG_SIZE − IPC_HEADER_SIZE −
/// IPC_HANDLE_MAX × 8`).
pub const IPC_PAYLOAD_SIZE: usize = IPC_MSG_SIZE - IPC_HEADER_SIZE - IPC_HANDLE_MAX * 8;

/// Default per-direction queue depth when the caller passes `0` to
/// `sys_channel_create`.
pub const IPC_DEFAULT_QUEUE_DEPTH: u32 = 16;
/// Largest per-direction queue depth `sys_channel_create` will honour.
pub const IPC_MAX_QUEUE_DEPTH: u32 = 1024;

/// The fixed 24-byte message header (`docs/spec/ipc-message-format.md`).
///
/// `sender_pid` and `timestamp` are stamped by the kernel at send time and
/// cannot be forged by the sender; `handle_count` is forced to `0` this slice
/// (handle transfer lands with process spawn).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct IpcMsgHeader {
    /// Sending process's PID; set by the kernel at send (offset 0).
    pub sender_pid: u32,
    /// Valid bytes in `payload[0..payload_len]`; `≤ IPC_PAYLOAD_SIZE` (offset 4).
    pub payload_len: u32,
    /// Valid handles in the message's handle array; `≤ IPC_HANDLE_MAX` (offset 8).
    pub handle_count: u8,
    /// Padding (offset 9).
    pub _pad1: u8,
    /// `IpcMsgFlags` bitfield (offset 10).
    pub flags: u16,
    /// Padding to 8-byte-align `timestamp` (offset 12).
    pub _pad2: [u8; 4],
    /// Monotonic nanoseconds at enqueue; set by the kernel (offset 16).
    pub timestamp: u64,
}

/// One IPC message: header + inline payload + transferable-handle array.
/// `#[repr(C, align(4096))]` — exactly one page.
#[repr(C, align(4096))]
#[derive(Copy, Clone)]
pub struct IpcMsg {
    /// Fixed 24-byte header (offset 0).
    pub header: IpcMsgHeader,
    /// Inline payload bytes (offset 24).
    pub payload: [u8; IPC_PAYLOAD_SIZE],
    /// Transferable handles (offset 4032); unused until handle transfer lands.
    pub handles: [RawHandle; IPC_HANDLE_MAX],
}

const _: () = assert!(core::mem::size_of::<IpcMsgHeader>() == IPC_HEADER_SIZE);
const _: () = assert!(core::mem::align_of::<IpcMsgHeader>() == 8);
const _: () = assert!(core::mem::offset_of!(IpcMsgHeader, sender_pid) == 0);
const _: () = assert!(core::mem::offset_of!(IpcMsgHeader, payload_len) == 4);
const _: () = assert!(core::mem::offset_of!(IpcMsgHeader, handle_count) == 8);
const _: () = assert!(core::mem::offset_of!(IpcMsgHeader, flags) == 10);
const _: () = assert!(core::mem::offset_of!(IpcMsgHeader, timestamp) == 16);

const _: () = assert!(core::mem::size_of::<IpcMsg>() == IPC_MSG_SIZE);
const _: () = assert!(core::mem::align_of::<IpcMsg>() == 4096);
const _: () = assert!(core::mem::offset_of!(IpcMsg, header) == 0);
const _: () = assert!(core::mem::offset_of!(IpcMsg, payload) == IPC_HEADER_SIZE);
const _: () = assert!(core::mem::offset_of!(IpcMsg, handles) == IPC_HEADER_SIZE + IPC_PAYLOAD_SIZE);
// The three regions tile the page exactly — no interior padding, so a
// `StoredMsg` byte view (see `object::ipc_channel`) is fully initialised.
const _: () = assert!(IPC_HEADER_SIZE + IPC_PAYLOAD_SIZE + IPC_HANDLE_MAX * 8 == IPC_MSG_SIZE);

/// How `sys_channel_send` behaves when the destination queue is full
/// (`docs/spec/ipc-message-format.md` § "Send modes").
///
/// This slice implements [`NoBlock`](SendMode::NoBlock) only; `Block` /
/// `BlockBounded` (which block via a `PendingOperation`) land with the async-I/O
/// slice and return `Unsupported` until then.
#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SendMode {
    /// Block via a `PendingOperation` until the queue has space (deferred).
    Block = 0,
    /// Return `WouldBlock` immediately if the queue is full.
    NoBlock = 1,
    /// Block up to a deadline, then time out (deferred).
    BlockBounded = 2,
}

impl SendMode {
    /// Decode a `u32` send-mode selector, or `None` if unrecognised.
    pub const fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Block),
            1 => Some(Self::NoBlock),
            2 => Some(Self::BlockBounded),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipc_msg_is_one_page_with_spec_offsets() {
        assert_eq!(core::mem::size_of::<IpcMsg>(), 4096);
        assert_eq!(core::mem::align_of::<IpcMsg>(), 4096);
        assert_eq!(core::mem::offset_of!(IpcMsg, header), 0);
        assert_eq!(core::mem::offset_of!(IpcMsg, payload), 24);
        assert_eq!(core::mem::offset_of!(IpcMsg, handles), 4032);
        assert_eq!(IPC_PAYLOAD_SIZE, 4008);
    }

    #[test]
    fn header_is_24_bytes_with_spec_offsets() {
        assert_eq!(core::mem::size_of::<IpcMsgHeader>(), 24);
        assert_eq!(core::mem::offset_of!(IpcMsgHeader, sender_pid), 0);
        assert_eq!(core::mem::offset_of!(IpcMsgHeader, payload_len), 4);
        assert_eq!(core::mem::offset_of!(IpcMsgHeader, handle_count), 8);
        assert_eq!(core::mem::offset_of!(IpcMsgHeader, flags), 10);
        assert_eq!(core::mem::offset_of!(IpcMsgHeader, timestamp), 16);
    }

    #[test]
    fn send_mode_round_trips() {
        assert_eq!(SendMode::from_u32(0), Some(SendMode::Block));
        assert_eq!(SendMode::from_u32(1), Some(SendMode::NoBlock));
        assert_eq!(SendMode::from_u32(2), Some(SendMode::BlockBounded));
        assert_eq!(SendMode::from_u32(3), None);
        assert_eq!(SendMode::NoBlock as u32, 1);
    }
}
