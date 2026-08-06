//! The syscall-backed [`Transport`] — an IPC channel to the compositor.
//!
//! Obtained by resolving `/dev/draw/new`, which the compositor answers with a channel
//! endpoint (`rsproto-surface-ops.md`, "How a client obtains a connection"). The forwarded
//! resolve is the introduction; this channel is the conversation.

use libkern::error::KError;
use libkern::{
    SENDMODE_NOBLOCK, SYS_CHANNEL_RECV, SYS_CHANNEL_SEND, SYS_WAIT, syscall4, syscall5,
};
use librsproto::{RS_FLAG_REPLY, decode, encode};

use crate::{Transport, UiError};

/// IPC message buffer length.
const MSG_LEN: usize = 4096;
/// Offset of the rsproto payload inside an `IpcMsg`.
const PAYLOAD_OFF: usize = 24;
/// Largest event body parked while waiting for a reply.
const MAX_BODY: usize = 64;
/// How many out-of-order messages can be parked while waiting for a reply.
///
/// Overflow is an **error, not a drop**. The obvious cheap behaviour — discard one and
/// carry on — is wrong here because the messages that park are mostly `Release`, and a lost
/// `Release` leaves a buffer marked busy that will never be freed. Since `Window::acquire`
/// blocks with no timeout, that is not a recoverable `None`: it is a permanent hang, at a
/// point arbitrarily far from the discard that caused it. Reporting it turns a silent hang
/// into a loud failure (PR #175 review, finding 9).
///
/// Nothing reaches this today — only `Window::new` waits for a reply, so at most a couple
/// of messages can park — which is exactly why it is worth making the unreachable case
/// noisy while it is still unreachable.
const MAX_PARKED: usize = 8;

/// A connection to the compositor over an IPC channel.
pub struct ChannelTransport {
    channel: u64,
    request_id: u64,
    msg: [u8; MSG_LEN],
    handles: [u64; 4],
    recv_msg: [u8; MSG_LEN],
    recv_handles: [u64; 4],
    recv_count: u64,
    /// Messages that arrived while waiting for a reply — drained by `poll_event`.
    parked: [(u16, [u8; MAX_BODY], usize); MAX_PARKED],
    parked_len: usize,
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
            parked: [(0, [0; MAX_BODY], 0); MAX_PARKED],
            parked_len: 0,
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

        // **Park until the reply lands.** `sys_channel_recv` is non-blocking — an empty
        // queue is `WouldBlock` — so a bare send-then-recv fails unless the server happened
        // to be scheduled in the gap. `sys_wait` is where the thread blocks, never inside
        // the send or the recv (the idiom `librsproto::session` already uses).
        //
        // Messages that are not this reply are **parked, not dropped**: a server-initiated
        // `Release` can arrive first, and discarding it would leave the client's buffer
        // busy forever. `poll_event` drains the queue afterwards.
        loop {
            let handles = [self.channel];
            let mut results = [0u8; 24];
            // SAFETY: a valid handle array and result buffer for one waiter.
            let waited = unsafe {
                syscall4(
                    SYS_WAIT,
                    handles.as_ptr() as u64,
                    1,
                    results.as_mut_ptr() as u64,
                    u64::MAX,
                )
            };
            if waited != 1 {
                return Err(UiError::Transport);
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
            if rr == KError::WouldBlock.as_i32() as i64 {
                continue; // woken with nothing to take; wait again
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
            let req =
                &self.recv_msg[PAYLOAD_OFF..PAYLOAD_OFF + payload_len.min(MSG_LEN - PAYLOAD_OFF)];
            let m = decode(req).map_err(|_| UiError::BadReply)?;

            // Our reply: matched by request id, not merely by op, so a stale reply for an
            // earlier request cannot be mistaken for this one.
            if m.flags & RS_FLAG_REPLY != 0 && m.request_id == id {
                if m.op != op {
                    return Err(UiError::BadReply);
                }
                let n = m.body.len().min(reply.len());
                reply[..n].copy_from_slice(&m.body[..n]);
                return Ok(Some(n));
            }

            // Anything else — a `Release`, or a reply to a request we have given up on —
            // goes to the parked queue rather than being lost.
            let n = m.body.len().min(MAX_BODY);
            let mut body = [0u8; MAX_BODY];
            body[..n].copy_from_slice(&m.body[..n]);
            if self.parked_len >= self.parked.len() {
                return Err(UiError::Transport);
            }
            self.parked[self.parked_len] = (m.op, body, n);
            self.parked_len += 1;
        }
    }

    fn wait_event(&mut self, buf: &mut [u8]) -> Result<(u16, usize), UiError> {
        loop {
            // A parked message counts: it is already here and older than anything on the
            // wire, so blocking first would be wrong.
            if let Some(ev) = self.poll_event(buf)? {
                return Ok(ev);
            }
            let handles = [self.channel];
            let mut results = [0u8; 24];
            // SAFETY: a valid handle array and result buffer for one waiter. `sys_wait` is
            // where the thread blocks — never inside the recv.
            let waited = unsafe {
                syscall4(
                    SYS_WAIT,
                    handles.as_ptr() as u64,
                    1,
                    results.as_mut_ptr() as u64,
                    u64::MAX,
                )
            };
            if waited != 1 {
                return Err(UiError::Transport);
            }
        }
    }

    fn poll_event(&mut self, buf: &mut [u8]) -> Result<Option<(u16, usize)>, UiError> {
        // Parked messages first: a `Release` that arrived while we were waiting for a reply
        // is still an event the client needs, and it is older than anything on the wire.
        if self.parked_len > 0 {
            let (op, body, n) = self.parked[0];
            self.parked.copy_within(1..self.parked_len, 0);
            self.parked_len -= 1;
            let k = n.min(buf.len());
            buf[..k].copy_from_slice(&body[..k]);
            return Ok(Some((op, k)));
        }
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
