//! The syscall-backed [`Transport`] — an IPC channel to the compositor.
//!
//! Obtained by resolving `/dev/draw/new`, which the compositor answers with a channel
//! endpoint (`rsproto-surface-ops.md`, "How a client obtains a connection").

use libkern::error::KError;
use libkern::{
    SENDMODE_NOBLOCK, SYS_CHANNEL_SEND, SYS_CLOCK_READ, SYS_WAIT, abi::CLOCK_MONOTONIC, syscall2, syscall4, syscall5,
};
use librsproto::surface::OP_RELEASE;
use librsproto::{RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};

use crate::{Transport, UiError};

/// IPC message buffer length.
const MSG_LEN: usize = 4096;
/// Offset of the rsproto payload inside an `IpcMsg`.
const PAYLOAD_OFF: usize = 24;
/// Largest event body parked while waiting for a reply.
///
/// Sized from the protocol: a buffer smaller than a permitted record hands the client a
/// shortened body that parses as something else or not at all.
const MAX_BODY: usize = librsproto::surface::MAX_EVENT_BODY;
/// How many out-of-order messages can be parked while waiting for a reply.
///
/// Overflow never discards a `Release`: a lost one leaves a buffer busy forever, and
/// `Window::acquire` blocks with no timeout, so that is a permanent hang far from its cause.
/// Anything else is recoverable (a `FocusEvent` corrects on the next change, a lost key is
/// reported as `WindowEvent::InputLost`), so the oldest losable entry goes and only a queue of
/// nothing but `Release`s errors.
///
/// `parked_len` never resets between requests: a client that does not drain events fills it with
/// the error replies to the silent ops (`AttachBuffer`, `Commit`, `DestroyWindow`), and the
/// ninth surfaces as `Transport` on an unrelated request.
const MAX_PARKED: usize = 8;

/// Whether a parked entry may be discarded to make room: everything but `Release`.
fn losable(entry: &(u16, [u8; MAX_BODY], usize)) -> bool {
    entry.0 != OP_RELEASE
}

/// A connection to the compositor over an IPC channel.
pub struct ChannelTransport {
    channel: u64,
    request_id: u64,
    msg: [u8; MSG_LEN],
    handles: [u64; 4],
    recv_msg: [u8; MSG_LEN],
    /// Messages that arrived while waiting for a reply — drained by `poll_event`.
    parked: [(u16, [u8; MAX_BODY], usize); MAX_PARKED],
    /// Whether a parked event was discarded since the last time anyone asked.
    lost: bool,
    parked_len: usize,
}

impl ChannelTransport {
    /// Wrap an endpoint obtained by resolving `/dev/draw/new`.
    ///
    /// # Safety
    ///
    /// `channel` must be a live IPC endpoint connected to a compositor, given away: the
    /// transport's `Drop` closes it, so a borrowed handle is a double close.
    pub const unsafe fn new(channel: u64) -> Self {
        Self {
            channel,
            request_id: 1,
            msg: [0; MSG_LEN],
            handles: [0; 4],
            recv_msg: [0; MSG_LEN],
            parked: [(0, [0; MAX_BODY], 0); MAX_PARKED],
            lost: false,
            parked_len: 0,
        }
    }

    /// Resolve `/dev/draw/new` and wrap the endpoint it answers with.
    ///
    /// # Safety
    ///
    /// `root_ns` must be a live namespace handle owned by the caller.
    pub unsafe fn connect(root_ns: u64) -> Result<Self, UiError> {
        // SAFETY: the caller guarantees `root_ns` is live and owned.
        unsafe { Self::resolve(root_ns, "/dev/draw/new") }
    }

    /// Resolve `/dev/draw/manage`, the manager channel for the `Manage` ops on any window.
    ///
    /// There is one: a second resolve is refused, since two managers placing windows is a race
    /// with no arbiter. The capability is the binding: an application's namespace binds
    /// `/dev/draw/new` alone. A process spawned with `namespace: 0` inherits the root, where
    /// `/dev/draw` is unscoped, and is not gated.
    ///
    /// # Safety
    ///
    /// `root_ns` must be a live namespace handle owned by the caller.
    pub unsafe fn manage(root_ns: u64) -> Result<Self, UiError> {
        // SAFETY: the caller guarantees `root_ns` is live and owned.
        unsafe { Self::resolve(root_ns, "/dev/draw/manage") }
    }

    /// Resolve `path` and wrap the endpoint it answers with.
    ///
    /// # Safety
    ///
    /// `root_ns` must be a live namespace handle owned by the caller.
    unsafe fn resolve(root_ns: u64, path: &str) -> Result<Self, UiError> {
        use libkern::handle::{RawHandle, Rights};
        use libos::{Handle, Namespace, NsReadOnly, Only, Resource, block_on};

        // SAFETY: the caller guarantees `root_ns` is live and owned; `borrow` never closes.
        let ns = unsafe { Handle::<Namespace, NsReadOnly>::borrow(RawHandle(root_ns), Rights::LOOKUP) };
        // SAFETY: both paths resolve to a channel endpoint, asserted by the type arguments.
        let ch = block_on(unsafe { ns.lookup::<Resource, Only>(path, Rights::SEND | Rights::RECV | Rights::WAIT) })
            .map_err(|_| UiError::Transport)?;
        // SAFETY: a live endpoint this process now owns.
        Ok(unsafe { Self::new(ch.into_raw().0) })
    }
}

impl Drop for ChannelTransport {
    /// Close the endpoint. The compositor frees a session slot only on `PeerClosed`, and there
    /// are `MAX_WAIT_HANDLES - 2` = 30 for the whole machine: a leaked endpoint eventually
    /// makes `/dev/draw/new` fail for every process.
    fn drop(&mut self) {
        if self.channel != 0 {
            // SAFETY: closing an endpoint this transport owns and no longer uses.
            unsafe { syscall4(libkern::SYS_HANDLE_CLOSE, self.channel, 0, 0, 0) };
        }
    }
}

impl ChannelTransport {
    /// Read the next event, waiting up to `timeout_ns` from now; `Ok(None)` is a timeout.
    ///
    /// Blocks rather than spinning on [`poll_event`](Transport::poll_event), which on a
    /// single-CPU guest starves the process waited for. `sys_wait` takes an absolute deadline,
    /// so this adds the monotonic clock: passed straight through, every wait would time out.
    pub fn wait_event_timeout(&mut self, buf: &mut [u8], timeout_ns: u64) -> Result<Option<(u16, usize)>, UiError> {
        let mut now: u64 = 0;
        // SAFETY: a valid out-pointer for one u64; `sys_clock_read` writes exactly that.
        unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut now) as u64) };
        let deadline = now.saturating_add(timeout_ns);
        loop {
            // A parked message is older than anything on the wire.
            if let Some(ev) = self.poll_event(buf)? {
                return Ok(Some(ev));
            }
            let handles = [self.channel];
            let mut results = [0u8; 24];
            // SAFETY: a valid handle array and result buffer for one waiter. `sys_wait` is
            // where the thread blocks — never inside the recv.
            let waited =
                unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, results.as_mut_ptr() as u64, deadline) };
            if waited == KError::TimedOut.as_i32() as i64 {
                // Poll once more: an event landing between the poll and the wait is not a timeout.
                return self.poll_event(buf);
            }
            if waited != 1 {
                return Err(UiError::Transport);
            }
        }
    }

    /// Hold an event that arrived while waiting for a reply, evicting the oldest losable entry
    /// if full (see `MAX_PARKED`). The loss surfaces through
    /// [`took_loss`](libsurface::Transport::took_loss) as a `WindowEvent::InputLost`.
    fn park(&mut self, op: u16, body: [u8; MAX_BODY], n: usize) -> Result<(), UiError> {
        if self.parked_len < self.parked.len() {
            self.parked[self.parked_len] = (op, body, n);
            self.parked_len += 1;
            return Ok(());
        }
        if let Some(i) = self.parked[..self.parked_len].iter().position(losable) {
            self.parked.copy_within(i + 1.., i);
            self.parked_len -= 1;
            self.lost = true;
            self.parked[self.parked_len] = (op, body, n);
            self.parked_len += 1;
            return Ok(());
        }
        // Every parked entry is a `Release`: lose the arrival if it is losable.
        if losable(&(op, body, n)) {
            self.lost = true;
            return Ok(());
        }
        // A `Release` on a queue of eight `Release`s: nothing can be dropped, so report.
        Err(UiError::Transport)
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
        let rs_len = encode(&mut self.msg[PAYLOAD_OFF..], op, id, 0, body, hcount).ok_or(UiError::Malformed)?;
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

        // `sys_channel_recv` is non-blocking, so a bare send-then-recv fails unless the server
        // was scheduled in the gap; `sys_wait` is where the thread blocks.
        //
        // Messages that are not this reply are parked: a `Release` can arrive first, and
        // discarding it would leave the buffer busy forever. `poll_event` drains them.
        loop {
            let handles = [self.channel];
            let mut results = [0u8; 24];
            // SAFETY: a valid handle array and result buffer for one waiter.
            let waited =
                unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, results.as_mut_ptr() as u64, u64::MAX) };
            if waited != 1 {
                return Err(UiError::Transport);
            }
            let _sent = match libkern::channel_recv(self.channel, &mut self.recv_msg) {
                Ok(sent) => sent,
                Err(rr) if rr == KError::WouldBlock.as_i32() as i64 => continue, // nothing to take
                Err(_) => return Err(UiError::Transport),
            };
            let payload_len =
                u32::from_le_bytes([self.recv_msg[4], self.recv_msg[5], self.recv_msg[6], self.recv_msg[7]]) as usize;
            let req = &self.recv_msg[PAYLOAD_OFF..PAYLOAD_OFF + payload_len.min(MSG_LEN - PAYLOAD_OFF)];
            let m = decode(req).map_err(|_| UiError::BadReply)?;

            // Matched by request id, so a stale reply cannot be mistaken for this one.
            if m.flags & RS_FLAG_REPLY != 0 && m.request_id == id {
                if m.op != op {
                    return Err(UiError::BadReply);
                }
                // A refusal is still this request's reply: parking it would fill the queue.
                if m.flags & RS_FLAG_ERROR != 0 {
                    return Err(UiError::Server);
                }
                let n = m.body.len().min(reply.len());
                reply[..n].copy_from_slice(&m.body[..n]);
                return Ok(Some(n));
            }

            // Anything else — a `Release`, or a reply to a request we gave up on — is parked.
            // A body too large to park is discarded whole, as in `poll_event`.
            if m.body.len() > MAX_BODY {
                self.lost = true;
                continue;
            }
            let n = m.body.len();
            let mut body = [0u8; MAX_BODY];
            body[..n].copy_from_slice(&m.body[..n]);
            self.park(m.op, body, n)?;
        }
    }

    fn wait_handle(&self) -> u64 {
        self.channel
    }

    fn wait_event(&mut self, buf: &mut [u8]) -> Result<(u16, usize), UiError> {
        match self.wait_event_timeout(buf, u64::MAX)? {
            Some(ev) => Ok(ev),
            None => Err(UiError::Transport),
        }
    }

    fn took_loss(&mut self) -> bool {
        core::mem::take(&mut self.lost)
    }

    /// Take the next event if one is already here, without blocking.
    ///
    /// Callable from outside because a [`manage`](ChannelTransport::manage) transport exists to
    /// receive.
    fn poll_event(&mut self, buf: &mut [u8]) -> Result<Option<(u16, usize)>, UiError> {
        // Parked messages first: they are older than anything on the wire.
        if self.parked_len > 0 {
            let (op, body, n) = self.parked[0];
            self.parked.copy_within(1..self.parked_len, 0);
            self.parked_len -= 1;
            if n > buf.len() {
                self.lost = true;
                return Ok(None);
            }
            buf[..n].copy_from_slice(&body[..n]);
            return Ok(Some((op, n)));
        }
        let _sent = match libkern::channel_recv(self.channel, &mut self.recv_msg) {
            Ok(sent) => sent,
            Err(rr) if rr == KError::WouldBlock.as_i32() as i64 => return Ok(None),
            Err(_) => return Err(UiError::Transport),
        };
        let payload_len =
            u32::from_le_bytes([self.recv_msg[4], self.recv_msg[5], self.recv_msg[6], self.recv_msg[7]]) as usize;
        let req = &self.recv_msg[PAYLOAD_OFF..PAYLOAD_OFF + payload_len.min(MSG_LEN - PAYLOAD_OFF)];
        let m = decode(req).map_err(|_| UiError::BadReply)?;
        // A body that does not fit is a loss, not a shorter one: every variable-length field is
        // length-prefixed, so a cut record can parse as a different event.
        if m.body.len() > buf.len() {
            self.lost = true;
            return Ok(None);
        }
        let n = m.body.len();
        buf[..n].copy_from_slice(&m.body[..n]);
        Ok(Some((m.op, n)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport() -> ChannelTransport {
        // SAFETY: `0` is not a live endpoint, and nothing here sends or receives on it:
        // `poll_event` drains parked entries before touching the channel, and `Drop` checks
        // for `0`.
        unsafe { ChannelTransport::new(0) }
    }

    fn park(t: &mut ChannelTransport, n: u16) {
        t.park(0x8000 | n, [0; MAX_BODY], 0).expect("losable, so it fits");
    }

    fn park_release(t: &mut ChannelTransport) -> Result<(), UiError> {
        t.park(OP_RELEASE, [0; MAX_BODY], 0)
    }

    fn drain(t: &mut ChannelTransport) -> alloc::vec::Vec<u16> {
        let mut out = alloc::vec::Vec::new();
        let mut buf = [0u8; 64];
        while let Ok(Some((op, _))) = t.poll_event(&mut buf) {
            out.push(op);
        }
        out
    }

    #[test]
    fn parked_events_drain_oldest_first() {
        let mut t = transport();
        for n in 0..4 {
            park(&mut t, n);
        }
        assert_eq!(drain(&mut t), [0x8000, 0x8001, 0x8002, 0x8003]);
        assert!(!t.took_loss(), "nothing was dropped");
    }

    #[test]
    fn filling_exactly_to_capacity_drops_nothing() {
        let mut t = transport();
        for n in 0..MAX_PARKED as u16 {
            park(&mut t, n);
        }
        assert!(!t.took_loss());
        let got = drain(&mut t);
        assert_eq!(got.len(), MAX_PARKED);
        assert_eq!(got[0], 0x8000, "the first one is still there");
    }

    #[test]
    fn overflowing_drops_the_oldest_and_keeps_the_order_of_the_rest() {
        let mut t = transport();
        for n in 0..(MAX_PARKED as u16 + 3) {
            park(&mut t, n);
        }
        let got = drain(&mut t);
        assert_eq!(got.len(), MAX_PARKED, "still exactly full");
        let expected: alloc::vec::Vec<u16> = (3..(MAX_PARKED as u16 + 3)).map(|n| 0x8000 | n).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn a_drop_is_reported_once_and_then_cleared() {
        let mut t = transport();
        for n in 0..(MAX_PARKED as u16 + 1) {
            park(&mut t, n);
        }
        assert!(t.took_loss(), "the overflow is reported");
        assert!(!t.took_loss(), "and only once");
    }

    #[test]
    fn a_release_is_never_evicted_to_make_room() {
        let mut t = transport();
        park_release(&mut t).expect("room");
        for n in 0..(MAX_PARKED as u16 + 3) {
            park(&mut t, n);
        }
        assert!(t.took_loss(), "something was dropped");
        let got = drain(&mut t);
        assert!(got.contains(&OP_RELEASE), "the Release survived: {got:?}");
        assert_eq!(got[0], OP_RELEASE, "and kept its place at the front");
    }

    #[test]
    fn a_losable_arrival_is_dropped_rather_than_failing_a_request() {
        let mut t = transport();
        for _ in 0..MAX_PARKED {
            park_release(&mut t).expect("room");
        }
        park(&mut t, 99);
        assert!(t.took_loss(), "the arrival was dropped");
        assert_eq!(drain(&mut t).len(), MAX_PARKED, "and every Release is still here");
    }

    #[test]
    fn a_release_arriving_on_a_queue_of_releases_still_reports() {
        let mut t = transport();
        for _ in 0..MAX_PARKED {
            park_release(&mut t).expect("room");
        }
        assert_eq!(park_release(&mut t), Err(UiError::Transport));
    }

    #[test]
    fn a_drained_queue_makes_room_again() {
        let mut t = transport();
        for n in 0..MAX_PARKED as u16 {
            park(&mut t, n);
        }
        drain(&mut t);
        for n in 100..(100 + MAX_PARKED as u16) {
            park(&mut t, n);
        }
        assert!(!t.took_loss(), "nothing dropped after draining");
        assert_eq!(drain(&mut t).len(), MAX_PARKED);
    }

    #[test]
    fn the_body_and_length_travel_with_the_op() {
        // A shift that moved the ops but not their bodies would pass every test above.
        let mut t = transport();
        for n in 0..(MAX_PARKED as u16 + 1) {
            let mut body = [0u8; MAX_BODY];
            body[0] = n as u8;
            t.park(0x8000 | n, body, 1).expect("losable");
        }
        let mut buf = [0u8; 64];
        let (op, len) = t.poll_event(&mut buf).unwrap().expect("an event");
        assert_eq!(op, 0x8001, "the oldest was the one dropped");
        assert_eq!(len, 1);
        assert_eq!(buf[0], 1, "and its body came with it");
    }
}
