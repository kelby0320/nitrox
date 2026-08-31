//! The syscall-backed [`Transport`] — an IPC channel to the compositor.
//!
//! Obtained by resolving `/dev/draw/new`, which the compositor answers with a channel
//! endpoint (`rsproto-surface-ops.md`, "How a client obtains a connection"). The forwarded
//! resolve is the introduction; this channel is the conversation.

use libkern::error::KError;
use libkern::{
    SENDMODE_NOBLOCK, SYS_CHANNEL_RECV, SYS_CHANNEL_SEND, SYS_CLOCK_READ, SYS_WAIT,
    abi::CLOCK_MONOTONIC, syscall2, syscall4, syscall5,
};
use librsproto::surface::OP_RELEASE;
use librsproto::{RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};

use crate::{Transport, UiError};

/// IPC message buffer length.
const MSG_LEN: usize = 4096;
/// Offset of the rsproto payload inside an `IpcMsg`.
const PAYLOAD_OFF: usize = 24;
/// Largest event body parked while waiting for a reply.
const MAX_BODY: usize = 64;
/// How many out-of-order messages can be parked while waiting for a reply.
///
/// Overflow **never discards a `Release`**, and errors rather than doing so. A lost
/// `Release` leaves a buffer marked busy that will never be freed, and since
/// `Window::acquire` blocks with no timeout that is not a recoverable `None` — it is a
/// permanent hang, at a point arbitrarily far from the discard that caused it (PR #175
/// review, finding 9).
///
/// **Everything else is dropped rather than reported**, which is a narrowing of that rule
/// made in M4 Part B and not a reversal of it. Failing the *request* punishes an operation
/// for traffic it has nothing to do with: `ui-testclient`'s churn probe makes requests
/// without draining events and died on its ninth window the moment `FocusEvent` gave it a
/// second thing to receive. So the oldest **losable** entry is evicted, and only a queue
/// consisting entirely of `Release`s falls back to the error (PR #184 re-review, finding 1).
///
/// The line is drawn at recoverability, not at importance. A lost `FocusEvent` leaves a
/// client wrong about whether it has the keyboard until the next focus change; a lost key or
/// motion degrades an event stream `WindowEvent::InputLost` already warns about. A lost
/// `Release` has no next anything — there is no resync op, and the buffer never comes back.
///
/// **It is reachable, and `parked_len` never resets between requests.** It accumulates for
/// the life of the connection for any client that does not drain events, and the ops that
/// fill it are the ones the protocol calls silent: `AttachBuffer`, `Commit` and
/// `DestroyWindow` send nothing on success and an error reply on failure. A client that
/// ignores those replies — which the spec, until this was written down, gave it no reason
/// not to — dies at the ninth rejection, and it surfaces on an unrelated later request as
/// `Transport` with nothing pointing at the cause. Drain with `Window::pump` or
/// `poll_event` (PR #175 review, finding 3).
const MAX_PARKED: usize = 8;

/// Whether a parked entry may be discarded to make room.
///
/// Only `Release` may not. Everything else either supersedes itself (motion), degrades an
/// event stream the client is already told about (`WindowEvent::InputLost`), or corrects itself
/// on the next change (`FocusEvent`). A `Release` has no next: there is no resync op, and
/// `Window::acquire` waits on it with no timeout.
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
    /// Sized by the kernel's `IPC_HANDLE_MAX`, not by what a compositor is expected to
    /// send: `sys_channel_recv` takes no capacity argument and copies the *sender's*
    /// handle count here (PR #175 review, finding 1).
    recv_handles: [u64; libkern::abi::IPC_HANDLE_MAX],
    recv_count: u64,
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
    /// `channel` must be a live IPC endpoint connected to a compositor, and the caller must
    /// be **giving it away**: the transport takes ownership and its `Drop` closes it.
    /// Passing a borrowed handle is a double close, and passing one the caller closes
    /// separately is worse — the id can be reused by then.
    pub const unsafe fn new(channel: u64) -> Self {
        Self {
            channel,
            request_id: 1,
            msg: [0; MSG_LEN],
            handles: [0; 4],
            recv_msg: [0; MSG_LEN],
            recv_handles: [0; libkern::abi::IPC_HANDLE_MAX],
            recv_count: 0,
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

    /// Resolve `/dev/draw/manage` — **the** manager channel.
    ///
    /// A window manager speaks the `Manage` ops over this about *any* window, not just windows
    /// it created. There is one: a second resolve is refused, because two managers placing
    /// windows is a race with no arbiter.
    ///
    /// **The capability is the binding**, and as of M7 Part E it gates: an application runs in
    /// a namespace `desktop-shell` built, which binds `/dev/draw/new` alone, so `manage`
    /// resolves to nothing there. It still gates nothing for anything spawned with
    /// `namespace: 0`, which inherits the root where `/dev/draw` is bound unscoped — a
    /// property of the selftest path rather than of the design.
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
        let ns =
            unsafe { Handle::<Namespace, NsReadOnly>::borrow(RawHandle(root_ns), Rights::LOOKUP) };
        // SAFETY: both paths resolve to a channel endpoint, asserted by the type arguments.
        let ch = block_on(unsafe {
            ns.lookup::<Resource, Only>(path, Rights::SEND | Rights::RECV | Rights::WAIT)
        })
        .map_err(|_| UiError::Transport)?;
        // SAFETY: a live endpoint this process now owns.
        Ok(unsafe { Self::new(ch.into_raw().0) })
    }
}

impl Drop for ChannelTransport {
    /// Close the endpoint — **a dropped transport must not strand a compositor session**.
    ///
    /// `connect` takes ownership via `Handle::into_raw`, which suppresses the close, and
    /// nothing closed it afterwards. The compositor frees a session slot only on
    /// `PeerClosed`, which never arrives while the endpoint is open, so every dropped
    /// transport cost a slot for the compositor's life. There are `MAX_WAIT_HANDLES - 2` =
    /// 30 of them — the forwarding endpoint and the input-server consumer channel take the
    /// other two — shared by the whole machine: one client opening and dropping 30
    /// connections makes `/dev/draw/new` fail for **every** process, permanently, while the
    /// offending client keeps running and looks healthy (PR #175 review, finding 4).
    fn drop(&mut self) {
        if self.channel != 0 {
            // SAFETY: closing an endpoint this transport owns and no longer uses.
            unsafe { syscall4(libkern::SYS_HANDLE_CLOSE, self.channel, 0, 0, 0) };
        }
    }
}

impl ChannelTransport {
    /// Read the next event, waiting up to `timeout_ns` from now for one to arrive.
    ///
    /// `Ok(None)` means the time passed with nothing to read — a timeout, not an error.
    ///
    /// **Blocking with a deadline rather than spinning on
    /// [`poll_event`](Transport::poll_event).** A client waiting for an event the compositor
    /// has not sent yet must not busy-poll: on a single-CPU guest that starves the very
    /// process it is waiting for, and the wait would then expire because of the spin rather
    /// than because the event was missing.
    ///
    /// **`timeout_ns` is relative and `sys_wait` takes an absolute deadline**, so this reads
    /// the monotonic clock and adds. Passing the relative value straight through is a
    /// deadline a fraction of a second after boot — permanently in the past, so every wait
    /// returns `TimedOut` at once and the call silently degrades to a non-blocking poll that
    /// still *looks* like it works wherever the event happened to be queued already.
    pub fn wait_event_timeout(
        &mut self,
        buf: &mut [u8],
        timeout_ns: u64,
    ) -> Result<Option<(u16, usize)>, UiError> {
        let mut now: u64 = 0;
        // SAFETY: a valid out-pointer for one u64; `sys_clock_read` writes exactly that.
        unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut now) as u64) };
        let deadline = now.saturating_add(timeout_ns);
        loop {
            // A parked message counts: it is already here and older than anything on the
            // wire, so blocking first would be wrong.
            if let Some(ev) = self.poll_event(buf)? {
                return Ok(Some(ev));
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
                    deadline,
                )
            };
            if waited == KError::TimedOut.as_i32() as i64 {
                // The deadline, not a failure. Poll once more: an event that landed between
                // the poll above and the wait would otherwise be reported as a timeout.
                return self.poll_event(buf);
            }
            if waited != 1 {
                return Err(UiError::Transport);
            }
        }
    }

    /// Hold an event that arrived while waiting for a reply, dropping the oldest if full.
    ///
    /// **Drop; do not fail the request.** These are server-initiated messages that happened
    /// to arrive at the wrong moment, and failing here makes an unrelated request fail
    /// because of traffic it has nothing to do with. Not hypothetical: a client that makes
    /// requests without draining events — the compositor's own churn probe — died on its
    /// ninth window the moment `FocusEvent` gave it a second thing to receive.
    ///
    /// **Oldest losable**, not simply oldest. A `Release` is never evicted: losing one
    /// strands a buffer forever with no resync op to recover it, which is the argument
    /// `MAX_PARKED`'s own doc has carried since PR #175 and which a first version of this
    /// function overturned without reading (PR #184 re-review, finding 1). Among losable
    /// entries the oldest goes, for the same reason `Window`'s queue drops oldest: the newest
    /// describes the world as it is now.
    ///
    /// The loss is reported through [`took_loss`](libsurface::Transport::took_loss), so it
    /// surfaces as a `WindowEvent::InputLost` rather than vanishing.
    ///
    /// Split out of `request` so it can be tested: everything around it issues syscalls and
    /// this does not. The shift arithmetic rested on the boot gate not hanging until it was
    /// (PR #184 review).
    fn park(&mut self, op: u16, body: [u8; MAX_BODY], n: usize) -> Result<(), UiError> {
        if self.parked_len < self.parked.len() {
            self.parked[self.parked_len] = (op, body, n);
            self.parked_len += 1;
            return Ok(());
        }
        // Full. Evict the oldest entry that can be lost without stranding anything.
        if let Some(i) = self.parked[..self.parked_len].iter().position(losable) {
            self.parked.copy_within(i + 1.., i);
            self.parked_len -= 1;
            self.lost = true;
            self.parked[self.parked_len] = (op, body, n);
            self.parked_len += 1;
            return Ok(());
        }
        // Every parked entry is a `Release`. If the *arriving* message is losable, lose that
        // instead — dropping a keystroke is survivable and failing the request is not.
        if losable(&(op, body, n)) {
            self.lost = true;
            return Ok(());
        }
        // A `Release` arriving on a queue of eight `Release`s. Nothing here can be dropped,
        // so this is the one case that still reports, which is PR #175's original answer.
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
                // A refusal is still this request's reply — consumed here, not parked, or
                // it would sit in the queue and eventually overflow it.
                if m.flags & RS_FLAG_ERROR != 0 {
                    return Err(UiError::Server);
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
            self.park(m.op, body, n)?;
        }
    }

    fn wait_handle(&self) -> u64 {
        self.channel
    }

    fn wait_event(&mut self, buf: &mut [u8]) -> Result<(u16, usize), UiError> {
        match self.wait_event_timeout(buf, u64::MAX)? {
            Some(ev) => Ok(ev),
            // Unreachable with no deadline: `sys_wait` returns 1 or the loop goes round again.
            None => Err(UiError::Transport),
        }
    }

    fn took_loss(&mut self) -> bool {
        core::mem::take(&mut self.lost)
    }

    /// Take the next event if one is already here, without blocking.
    ///
    /// `Ok(None)` means nothing is waiting, which is the common case and not an error.
    ///
    /// Callable from outside because [`manage`](ChannelTransport::manage) hands back a
    /// transport whose entire purpose is to *receive* — a manager is told about windows it
    /// did not create — and a caller with no way to read it holds half an API. Bring
    /// [`Transport`] into scope to use it.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A transport on handle `0`, which is safe to construct and drop in a host test:
    /// `Drop` skips the close for `0`, and every path exercised below returns before it
    /// would issue a syscall.
    fn transport() -> ChannelTransport {
        // SAFETY: `0` is not a live endpoint, and nothing here sends or receives on it —
        // `poll_event` drains parked entries before touching the channel, and `Drop` checks
        // for `0`. This is the seam that lets the parking logic be tested at all.
        unsafe { ChannelTransport::new(0) }
    }

    /// Park an event whose op is `n`, so the drain order is legible. Ops are chosen well
    /// away from `OP_RELEASE` so they are losable.
    fn park(t: &mut ChannelTransport, n: u16) {
        t.park(0x8000 | n, [0; MAX_BODY], 0).expect("losable, so it fits");
    }

    /// Park a `Release`, which is the one thing overflow may not discard.
    fn park_release(t: &mut ChannelTransport) -> Result<(), UiError> {
        t.park(OP_RELEASE, [0; MAX_BODY], 0)
    }

    /// Drain every parked event, oldest first.
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
        // The boundary the overflow branch must not claim: `MAX_PARKED` entries fit.
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
        // The shift arithmetic: `copy_within(1.., 0)` then `parked_len -= 1` then write at
        // the top. Getting it wrong duplicates an entry or loses the newest instead of the
        // oldest, and neither is visible from outside — the boot gate only notices if the
        // *request* fails, which is exactly what this branch stopped doing (PR #184 review).
        let mut t = transport();
        for n in 0..(MAX_PARKED as u16 + 3) {
            park(&mut t, n);
        }
        let got = drain(&mut t);
        assert_eq!(got.len(), MAX_PARKED, "still exactly full");
        // The three oldest went; the rest kept their order and the newest survived.
        let expected: alloc::vec::Vec<u16> =
            (3..(MAX_PARKED as u16 + 3)).map(|n| 0x8000 | n).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn a_drop_is_reported_once_and_then_cleared() {
        // `took_loss` is take-and-clear, because `Window::pump` folds it into a flag that
        // produces exactly one `WindowEvent::InputLost`. Reporting on every call would emit a
        // `Dropped` per pump for the rest of the window's life.
        let mut t = transport();
        for n in 0..(MAX_PARKED as u16 + 1) {
            park(&mut t, n);
        }
        assert!(t.took_loss(), "the overflow is reported");
        assert!(!t.took_loss(), "and only once");
    }

    #[test]
    fn a_release_is_never_evicted_to_make_room() {
        // **The rule a first version of `park` broke without noticing it existed.** A lost
        // `Release` strands a buffer forever: `acquire` blocks with no timeout and there is
        // no resync op. So the oldest *losable* entry goes, and the `Release` stays even
        // though it is the oldest thing in the queue (PR #184 re-review, finding 1).
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
        // Every parked entry is a `Release`, so nothing in the queue may go — but the
        // arriving keystroke may. Failing here would punish a request for traffic it has
        // nothing to do with, which is the churn probe's death all over again.
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
        // The one case that cannot be resolved by dropping: nothing here is losable and
        // neither is the arrival. PR #175's original answer, narrowed to where it belongs.
        let mut t = transport();
        for _ in 0..MAX_PARKED {
            park_release(&mut t).expect("room");
        }
        assert_eq!(park_release(&mut t), Err(UiError::Transport));
    }

    #[test]
    fn a_drained_queue_makes_room_again() {
        // Parking is not a one-way ratchet: a client that catches up stops losing events.
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
