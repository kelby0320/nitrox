//! The syscall-backed [`Transport`] — an IPC channel to the compositor.
//!
//! Obtained by resolving `/dev/draw/new`, which the compositor answers with a channel
//! endpoint (`rsproto-surface-ops.md`, "How a client obtains a connection"). The forwarded
//! resolve is the introduction; this channel is the conversation.

use libkern::error::KError;
use libkern::{
    SENDMODE_NOBLOCK, SYS_CHANNEL_RECV, SYS_CHANNEL_SEND, syscall4, syscall5,
};
use librsproto::{RS_FLAG_REPLY, decode, encode};

use crate::{Transport, UiError};

/// IPC message buffer length.
const MSG_LEN: usize = 4096;
/// Offset of the rsproto payload inside an `IpcMsg`.
const PAYLOAD_OFF: usize = 24;

/// A connection to the compositor over an IPC channel.
pub struct ChannelTransport {
    channel: u64,
    request_id: u64,
    msg: [u8; MSG_LEN],
    handles: [u64; 4],
    recv_msg: [u8; MSG_LEN],
    recv_handles: [u64; 4],
    recv_count: u64,
}

impl ChannelTransport {
    /// Wrap an endpoint obtained by resolving `/dev/draw/new`.
    ///
    /// # Safety
    ///
    /// `channel` must be a live IPC endpoint owned by this process for the transport's
    /// lifetime, connected to a compositor.
    pub const unsafe fn new(channel: u64) -> Self {
        Self {
            channel,
            request_id: 1,
            msg: [0; MSG_LEN],
            handles: [0; 4],
            recv_msg: [0; MSG_LEN],
            recv_handles: [0; 4],
            recv_count: 0,
        }
    }

    /// Resolve `/dev/draw/new` and wrap the endpoint it answers with.
    ///
    /// # Safety
    ///
    /// `root_ns` must be a live namespace handle owned by the caller.
    pub unsafe fn connect(root_ns: u64) -> Result<Self, UiError> {
        use libkern::handle::{RawHandle, Rights};
        use libos::{Handle, Namespace, NsReadOnly, Resource, Only, block_on};

        // SAFETY: the caller guarantees `root_ns` is live and owned; `borrow` never closes.
        let ns =
            unsafe { Handle::<Namespace, NsReadOnly>::borrow(RawHandle(root_ns), Rights::LOOKUP) };
        // SAFETY: `/dev/draw/new` resolves to a channel endpoint, asserted by the type
        // arguments. The compositor mints one session per resolve.
        let ch = block_on(unsafe {
            ns.lookup::<Resource, Only>(
                "/dev/draw/new",
                Rights::SEND | Rights::RECV | Rights::WAIT,
            )
        })
        .map_err(|_| UiError::Transport)?;
        // SAFETY: a live endpoint this process now owns.
        Ok(unsafe { Self::new(ch.into_raw().0) })
    }
}

impl Transport for ChannelTransport {
    fn request(
        &mut self,
        op: u16,
        body: &[u8],
        handle: Option<u64>,
        reply: &mut [u8],
    ) -> Result<Option<usize>, UiError> {
        let id = self.request_id;
        self.request_id = self.request_id.wrapping_add(1);

        let hcount = if handle.is_some() { 1u16 } else { 0 };
        let rs_len = encode(&mut self.msg[PAYLOAD_OFF..], op, id, 0, body, hcount)
            .ok_or(UiError::Malformed)?;
        self.msg[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        self.msg[8] = hcount as u8;
        if let Some(h) = handle {
            self.handles[0] = h;
        }

        // SAFETY: a live endpoint, a valid message buffer, and `hcount` handles in
        // `self.handles`.
        let sr = unsafe {
            syscall5(
                SYS_CHANNEL_SEND,
                self.channel,
                (&raw const self.msg) as u64,
                (&raw const self.handles) as u64,
                hcount as u64,
                SENDMODE_NOBLOCK,
            )
        };
        if sr != 0 {
            return Err(UiError::Transport);
        }
        if reply.is_empty() {
            return Ok(None);
        }

        // SAFETY: valid recv out-params.
        let rr = unsafe {
            syscall4(
                SYS_CHANNEL_RECV,
                self.channel,
                (&raw mut self.recv_msg) as u64,
                (&raw mut self.recv_handles) as u64,
                (&raw mut self.recv_count) as u64,
            )
        };
        if rr != 0 {
            return Err(UiError::Transport);
        }
        let payload_len = u32::from_le_bytes([
            self.recv_msg[4],
            self.recv_msg[5],
            self.recv_msg[6],
            self.recv_msg[7],
        ]) as usize;
        let req = &self.recv_msg[PAYLOAD_OFF..PAYLOAD_OFF + payload_len.min(MSG_LEN - PAYLOAD_OFF)];
        let m = decode(req).map_err(|_| UiError::BadReply)?;
        if m.flags & RS_FLAG_REPLY == 0 || m.op != op {
            return Err(UiError::BadReply);
        }
        let n = m.body.len().min(reply.len());
        reply[..n].copy_from_slice(&m.body[..n]);
        Ok(Some(n))
    }

    fn poll_event(&mut self, buf: &mut [u8]) -> Result<Option<(u16, usize)>, UiError> {
        // Non-blocking: `WouldBlock` means no event is waiting, which is the common case
        // and not an error. Anything else is.
        // SAFETY: valid recv out-params.
        let rr = unsafe {
            syscall4(
                SYS_CHANNEL_RECV,
                self.channel,
                (&raw mut self.recv_msg) as u64,
                (&raw mut self.recv_handles) as u64,
                (&raw mut self.recv_count) as u64,
            )
        };
        if rr == KError::WouldBlock.as_i32() as i64 {
            return Ok(None);
        }
        if rr != 0 {
            return Err(UiError::Transport);
        }
        let payload_len = u32::from_le_bytes([
            self.recv_msg[4],
            self.recv_msg[5],
            self.recv_msg[6],
            self.recv_msg[7],
        ]) as usize;
        let req = &self.recv_msg[PAYLOAD_OFF..PAYLOAD_OFF + payload_len.min(MSG_LEN - PAYLOAD_OFF)];
        let m = decode(req).map_err(|_| UiError::BadReply)?;
        let n = m.body.len().min(buf.len());
        buf[..n].copy_from_slice(&m.body[..n]);
        Ok(Some((m.op, n)))
    }
}
