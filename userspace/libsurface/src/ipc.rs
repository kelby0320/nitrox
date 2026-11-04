//! The syscall-backed [`Transport`] — an IPC channel to the compositor.
//!
//! Obtained by resolving `/dev/draw/new`, which the compositor answers with a channel
//! endpoint (`rsproto-surface-ops.md`, "How a client obtains a connection").

use libkern::error::KError;
use libkern::handle::{RawHandle, Rights};
use libos::{
    CanLookup, Carried, Channel, Deadline, Handle, MapReadWrite, Memory, Msg, Namespace, NsReadOnly, Only, Ref,
    WaitSet, block_on,
};
use librsproto::surface::OP_RELEASE;
use librsproto::{RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};

use crate::{Transport, UiError};

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
///
/// Dropping it closes the endpoint. The compositor frees a session slot only on `PeerClosed`, and
/// there are `MAX_WAIT_HANDLES - 2` = 30 for the whole machine: a leaked endpoint eventually
/// makes `/dev/draw/new` fail for every process.
pub struct ChannelTransport {
    channel: Handle<Channel, Only>,
    request_id: u64,
    msg: Msg,
    recv_msg: Msg,
    /// Messages that arrived while waiting for a reply — drained by `poll_event`.
    parked: [(u16, [u8; MAX_BODY], usize); MAX_PARKED],
    /// Whether a parked event was discarded since the last time anyone asked.
    lost: bool,
    parked_len: usize,
}

impl ChannelTransport {
    /// Talk to the compositor through `channel`, an endpoint obtained by resolving
    /// `/dev/draw/new`, which the transport owns and closes.
    pub fn from_endpoint(channel: Handle<Channel, Only>) -> Self {
        Self {
            channel,
            request_id: 1,
            msg: Msg::new(),
            recv_msg: Msg::new(),
            parked: [(0, [0; MAX_BODY], 0); MAX_PARKED],
            lost: false,
            parked_len: 0,
        }
    }

    /// Wrap an endpoint obtained by resolving `/dev/draw/new`. For callers not yet on libos; the
    /// typed form is [`from_endpoint`](Self::from_endpoint).
    ///
    /// # Safety
    ///
    /// `channel` must be a live IPC endpoint connected to a compositor, given away: the
    /// transport's `Drop` closes it, so a borrowed handle is a double close.
    // TODO(raw-handle-shims): goes when its last caller is on libos.
    pub unsafe fn new(channel: u64) -> Self {
        // SAFETY: the caller gives the endpoint away, as this has always asked.
        Self::from_endpoint(unsafe { Handle::from_raw(RawHandle(channel), Rights::empty()) })
    }

    /// Resolve `/dev/draw/new` in `ns` and wrap the endpoint it answers with.
    pub fn open<'a, M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>) -> Result<Self, UiError> {
        Self::resolve(ns.into(), "/dev/draw/new")
    }

    /// Resolve `/dev/draw/manage`, the manager channel for the `Manage` ops on any window.
    ///
    /// There is one: a second resolve is refused, since two managers placing windows is a race
    /// with no arbiter. The capability is the binding: an application's namespace binds
    /// `/dev/draw/new` alone. A process spawned with `namespace: 0` inherits the root, where
    /// `/dev/draw` is unscoped, and is not gated.
    pub fn open_manager<'a, M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>) -> Result<Self, UiError> {
        Self::resolve(ns.into(), "/dev/draw/manage")
    }

    /// [`open`](Self::open) for a raw namespace handle. For callers not yet on libos.
    ///
    /// # Safety
    ///
    /// `root_ns` must be a live namespace handle owned by the caller.
    // TODO(raw-handle-shims): goes when its last caller is on libos.
    pub unsafe fn connect(root_ns: u64) -> Result<Self, UiError> {
        // SAFETY: the caller guarantees `root_ns` is live and owned.
        Self::open(unsafe { Ref::<Namespace, NsReadOnly>::from_raw(RawHandle(root_ns), Rights::LOOKUP) })
    }

    /// [`open_manager`](Self::open_manager) for a raw namespace handle. For callers not yet on
    /// libos.
    ///
    /// # Safety
    ///
    /// `root_ns` must be a live namespace handle owned by the caller.
    // TODO(raw-handle-shims): goes when its last caller is on libos.
    pub unsafe fn manage(root_ns: u64) -> Result<Self, UiError> {
        // SAFETY: the caller guarantees `root_ns` is live and owned.
        Self::open_manager(unsafe { Ref::<Namespace, NsReadOnly>::from_raw(RawHandle(root_ns), Rights::LOOKUP) })
    }

    fn resolve<M: CanLookup>(ns: Ref<'_, Namespace, M>, path: &str) -> Result<Self, UiError> {
        let ch = block_on(ns.lookup::<Channel, Only>(path, Rights::SEND | Rights::RECV | Rights::WAIT))
            .map_err(|_| UiError::Transport)?;
        Ok(Self::from_endpoint(ch))
    }

    /// The endpoint, to wait on beside a client's own handles.
    pub fn endpoint(&self) -> Ref<'_, Channel, Only> {
        self.channel.as_ref()
    }
}

impl crate::Session<ChannelTransport> {
    /// The compositor's channel, to wait on beside a client's own handles: the typed form of
    /// [`wait_handle`](crate::Session::wait_handle).
    pub fn waitable(&self) -> Ref<'_, Channel, Only> {
        self.transport.endpoint()
    }
}

impl ChannelTransport {
    /// Read the next event, waiting up to `timeout_ns` from now; `Ok(None)` is a timeout.
    ///
    /// Blocks rather than spinning on [`poll_event`](Transport::poll_event), which on a
    /// single-CPU guest starves the process waited for. A wait takes an absolute deadline, so
    /// this adds the monotonic clock: passed straight through, every wait would time out.
    pub fn wait_event_timeout(&mut self, buf: &mut [u8], timeout_ns: u64) -> Result<Option<(u16, usize)>, UiError> {
        let deadline = Deadline::after(timeout_ns).map_err(|_| UiError::Transport)?;
        loop {
            // A parked message is older than anything on the wire.
            if let Some(ev) = self.poll_event(buf)? {
                return Ok(Some(ev));
            }
            // The thread blocks here, never inside the recv.
            let signalled = self.wait(deadline)?;
            if signalled.is_empty() {
                // Poll once more: an event landing between the poll and the wait is not a timeout.
                return self.poll_event(buf);
            }
        }
    }

    /// Wait on the endpoint until `deadline`: which signalled, none at the deadline.
    fn wait(&self, deadline: Deadline) -> Result<libos::Signalled<1>, UiError> {
        let mut set = WaitSet::<1>::new();
        set.add(&self.channel).map_err(|_| UiError::Transport)?;
        set.wait(deadline).map_err(|_| UiError::Transport)
    }

    /// Take one message into `recv_msg`: `None` if the ring is empty.
    fn recv(&mut self) -> Result<Option<()>, UiError> {
        match self.channel.recv(&mut self.recv_msg) {
            Ok(_) => Ok(Some(())),
            Err(e) if e.code() == KError::WouldBlock => Ok(None),
            Err(_) => Err(UiError::Transport),
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
        let rs_len = encode(self.msg.payload_mut(), op, id, 0, body, hcount).ok_or(UiError::Malformed)?;
        self.msg.set_len(rs_len).map_err(|_| UiError::Malformed)?;
        let mut carried = Carried::new();
        if let Some(h) = handle {
            // SAFETY: the caller's own handle, given with the message as this always asked; a send
            // that fails hands it back below rather than closing it.
            let h = unsafe { Handle::<Memory, MapReadWrite>::from_raw(RawHandle(h), Rights::empty()) };
            if let Err(h) = carried.push(h) {
                h.into_raw();
                return Err(UiError::Malformed);
            }
        }
        if self.channel.send(&self.msg, &mut carried).is_err() {
            if let Some(h) = carried.take::<Memory, MapReadWrite>(0) {
                h.into_raw();
            }
            return Err(UiError::Transport);
        }
        if reply.is_empty() {
            return Ok(None);
        }

        // A recv never blocks, so a bare send-then-recv fails unless the server was scheduled in
        // the gap; the wait is where the thread blocks.
        //
        // Messages that are not this reply are parked: a `Release` can arrive first, and
        // discarding it would leave the buffer busy forever. `poll_event` drains them.
        loop {
            if self.wait(Deadline::NEVER)?.is_empty() {
                return Err(UiError::Transport);
            }
            if self.recv()?.is_none() {
                continue;
            }
            let m = decode(self.recv_msg.payload()).map_err(|_| UiError::BadReply)?;

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
        self.channel.raw().0
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
        if self.recv()?.is_none() {
            return Ok(None);
        }
        let m = decode(self.recv_msg.payload()).map_err(|_| UiError::BadReply)?;
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

/// A transport over the model of the kernel, and the compositor's end of it.
#[cfg(test)]
pub(crate) mod harness {
    use super::*;

    /// A transport and the peer endpoint where its requests arrive.
    pub fn connected() -> (ChannelTransport, Handle<Channel, Only>) {
        let (ours, peer) = Handle::<Channel, Only>::pair(8).expect("a channel");
        (ChannelTransport::from_endpoint(ours), peer)
    }

    /// Queue one message from the compositor.
    pub fn push<'a>(peer: impl Into<Ref<'a, Channel, Only>>, op: u16, id: u64, flags: u32, body: &[u8]) {
        let mut msg = Msg::new();
        let n = encode(msg.payload_mut(), op, id, flags, body, 0).expect("fits");
        msg.set_len(n).expect("fits");
        peer.into().send(&msg, &mut Carried::new()).expect("room in the ring");
    }

    /// Queue the reply to request `id`.
    pub fn reply<'a>(peer: impl Into<Ref<'a, Channel, Only>>, op: u16, id: u64, body: &[u8]) {
        push(peer, op, id, RS_FLAG_REPLY, body);
    }

    /// Take what a client sent: the op, the request id, the body, and the handles it carried.
    pub fn taken<'a>(peer: impl Into<Ref<'a, Channel, Only>>) -> Option<(u16, u64, std::vec::Vec<u8>, Carried)> {
        let mut msg = Msg::new();
        let carried = peer.into().recv(&mut msg).ok()?;
        let m = decode(msg.payload()).ok()?;
        Some((m.op, m.request_id, m.body.to_vec(), carried))
    }
}

#[cfg(test)]
mod tests {
    use super::harness::*;
    use super::*;
    use libos::mock;

    fn transport() -> ChannelTransport {
        connected().0
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

    const OP: u16 = 0x0901;

    /// A memory object to send, as the raw handle a caller gives.
    fn memory() -> u64 {
        Handle::<Memory, MapReadWrite>::create(4096).unwrap().into_raw().0
    }

    #[test]
    fn a_request_arrives_with_its_handle_and_returns_its_reply() {
        mock::reset();
        let (mut t, peer) = connected();
        reply(&peer, OP, 1, b"ok");
        let mem = memory();
        let mut out = [0u8; 8];
        assert_eq!(t.request(OP, b"abc", Some(mem), &mut out), Ok(Some(2)));
        assert_eq!(&out[..2], b"ok");
        let (op, id, body, carried) = taken(&peer).expect("the request");
        assert_eq!((op, id, body.as_slice(), carried.len()), (OP, 1, &b"abc"[..], 1));
        assert_eq!(t.request(OP, b"", None, &mut []), Ok(None), "no reply asked for, none waited for");
        assert_eq!(taken(&peer).map(|r| (r.1, r.3.len())), Some((2, 0)), "the next id, no handle");
    }

    #[test]
    fn a_refused_send_leaves_the_handle_with_the_caller() {
        mock::reset();
        let (mut t, peer) = connected();
        let mem = memory();
        drop(peer);
        assert_eq!(t.request(OP, b"abc", Some(mem), &mut []), Err(UiError::Transport));
        assert!(mock::is_open(mem), "only a send that lands moves it");
        assert_eq!(mock::stray_closes(), 0);
    }

    #[test]
    fn an_event_before_the_reply_is_kept_for_poll_event() {
        mock::reset();
        let (mut t, peer) = connected();
        push(&peer, OP_RELEASE, 0, 0, b"r");
        reply(&peer, OP, 1, b"ok");
        let mut out = [0u8; 8];
        assert_eq!(t.request(OP, b"", None, &mut out), Ok(Some(2)));
        let mut buf = [0u8; 8];
        assert_eq!(t.poll_event(&mut buf), Ok(Some((OP_RELEASE, 1))));
        assert_eq!(buf[0], b'r');
    }

    #[test]
    fn an_error_reply_is_a_server_refusal_and_a_wrong_op_a_bad_reply() {
        mock::reset();
        let (mut t, peer) = connected();
        push(&peer, OP, 1, RS_FLAG_REPLY | RS_FLAG_ERROR, b"");
        assert_eq!(t.request(OP, b"", None, &mut [0u8; 4]), Err(UiError::Server));
        reply(&peer, OP + 1, 2, b"");
        assert_eq!(t.request(OP, b"", None, &mut [0u8; 4]), Err(UiError::BadReply));
    }

    #[test]
    fn polling_an_empty_endpoint_is_none_and_a_closed_peer_an_error() {
        mock::reset();
        let (mut t, peer) = connected();
        let mut buf = [0u8; 8];
        assert_eq!(t.poll_event(&mut buf), Ok(None));
        push(&peer, 0x8001, 0, 0, b"xy");
        assert_eq!(t.poll_event(&mut buf), Ok(Some((0x8001, 2))));
        drop(peer);
        assert_eq!(t.poll_event(&mut buf), Err(UiError::Transport));
    }

    #[test]
    fn an_event_too_big_for_the_buffer_is_a_loss_not_a_short_one() {
        mock::reset();
        let (mut t, peer) = connected();
        push(&peer, 0x8001, 0, 0, b"too long");
        assert_eq!(t.poll_event(&mut [0u8; 2]), Ok(None));
        assert!(t.took_loss());
    }

    #[test]
    fn a_timed_wait_gives_up_with_none_and_returns_an_event_that_is_there() {
        mock::reset();
        let (mut t, peer) = connected();
        let mut buf = [0u8; 8];
        assert_eq!(t.wait_event_timeout(&mut buf, 1_000), Ok(None));
        assert_eq!(mock::sleeps(), 1, "it waited out its deadline");
        push(&peer, 0x8002, 0, 0, b"z");
        assert_eq!(t.wait_event_timeout(&mut buf, 1_000), Ok(Some((0x8002, 1))));
        assert_eq!(mock::sleeps(), 1, "an event already there is not waited for");
    }

    #[test]
    fn the_endpoint_is_waited_on_and_closed_with_the_transport() {
        mock::reset();
        let (t, _peer) = connected();
        assert_eq!(t.wait_handle(), t.endpoint().raw().0);
        assert!(mock::is_open(t.wait_handle()));
        let open = mock::open_handles();
        drop(t);
        assert_eq!(mock::open_handles(), open - 1);
    }

    #[test]
    fn a_session_lends_the_channel_its_raw_form_names() {
        mock::reset();
        let (t, _peer) = connected();
        let session = crate::Session::new(t);
        assert_eq!(session.waitable().raw().0, session.wait_handle());
        assert_ne!(session.wait_handle(), 0);
    }

    #[test]
    fn open_resolves_the_compositor_and_the_manager_path_separately() {
        mock::reset();
        let (ns, peer) = mock::answering("/dev/draw/new");
        // SAFETY: the mock's handles, open for the test.
        let (ns, peer) = unsafe {
            (
                Ref::<Namespace, NsReadOnly>::from_raw(RawHandle(ns), Rights::LOOKUP),
                Ref::<Channel, Only>::from_raw(RawHandle(peer), Rights::empty()),
            )
        };
        let mut t = ChannelTransport::open(ns).expect("resolves");
        assert!(ChannelTransport::open_manager(ns).is_err(), "nothing answers /dev/draw/manage");
        t.request(OP, b"hi", None, &mut []).unwrap();
        assert_eq!(taken(peer).map(|r| r.2), Some(b"hi".to_vec()));
    }
}
