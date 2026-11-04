//! One request, one reply. A password passes through these buffers, so **every buffer here is
//! zeroed once the kernel has the message**: a stack copy would outlive it.

use alloc::vec::Vec;
use libkern::{KError, RawHandle, Rights, scrub};
use libos::{
    CanLookup, Carried, Channel, Deadline, Error, Handle, Msg as WireMsg, Namespace, NsReadOnly, Only, Ref, WaitSet,
    block_on,
};

/// A received message: `(op, request_id, is_error, body)`.
pub type Msg = (u16, u64, bool, Vec<u8>);

/// A message buffer scrubbed when it goes out of scope, on every way out: a password passed
/// through it. A `drop` of the message would move it, so the scrub is in this guard, where it lies.
pub(crate) struct Scrubbed(pub(crate) WireMsg);

impl Drop for Scrubbed {
    fn drop(&mut self) {
        scrub(self.0.payload_mut());
    }
}

/// Close `h`, if it is a handle. For callers not yet on libos; the typed form is to drop the
/// [`Handle`](libos::Handle).
// TODO(raw-handle-shims): goes when its last caller is on libos.
pub fn close(h: u64) {
    if h != 0 {
        // SAFETY: the caller's own handle, which this closes as it always has: ownership moves here.
        drop(unsafe { Handle::<Channel, Only>::from_raw(RawHandle(h), Rights::empty()) });
    }
}

/// The index in `set` of a handle that is ready, once one is or `deadline` has passed: `None`
/// then too.
pub fn ready<const N: usize>(set: &WaitSet<'_, N>, deadline: Deadline) -> Option<usize> {
    set.wait(deadline).ok()?.iter().next().map(|(i, _)| i)
}

/// Wait on `handles`; the index of one that is ready. At most four. For callers not yet on libos;
/// the typed form is [`ready`].
// TODO(raw-handle-shims): goes when its last caller is on libos.
pub fn wait(handles: &[u64]) -> Option<usize> {
    wait_until(handles, u64::MAX)
}

/// [`wait`], giving up at `deadline` on the monotonic clock: `None` then too. For callers not yet
/// on libos; the typed form is [`ready`].
// TODO(raw-handle-shims): goes when its last caller is on libos.
pub fn wait_until(handles: &[u64], deadline: u64) -> Option<usize> {
    let mut set = WaitSet::<4>::new();
    for &h in handles {
        // SAFETY: the caller's own handle, open for the call, as this has always asked.
        set.add(unsafe { Ref::<Channel, Only>::from_raw(RawHandle(h), Rights::empty()) }).ok()?;
    }
    ready(&set, Deadline::at(deadline))
}

/// Receive one message on `ch`: `Ok(None)` if none queued, `Err` if the peer has gone.
pub fn receive<'a>(ch: impl Into<Ref<'a, Channel, Only>>) -> Result<Option<Msg>, Error> {
    receive_into(ch.into(), &mut Scrubbed(WireMsg::new()))
}

pub(crate) fn receive_into(ch: Ref<'_, Channel, Only>, msg: &mut Scrubbed) -> Result<Option<Msg>, Error> {
    match ch.recv(&mut msg.0) {
        Ok(_brought) => {}
        Err(e) if e.code() == KError::PeerClosed => return Err(e),
        Err(_) => return Ok(None),
    }
    // A line read at a password prompt came through here; the caller has its copy.
    Ok(librsproto::decode(msg.0.payload()).ok().map(|m| (m.op, m.request_id, m.is_error(), m.body.to_vec())))
}

/// Receive one message on `ch`: `Ok(None)` if none queued, `Err(())` if the peer has gone. For
/// callers not yet on libos; the typed form is [`receive`].
// TODO(raw-handle-shims): goes when its last caller is on libos.
#[allow(clippy::result_unit_err, reason = "the only error is the peer having gone")]
pub fn recv(ch: u64) -> Result<Option<Msg>, ()> {
    receive(borrowed(ch)).map_err(|_| ())
}

/// Send `op` on `ch`, moving `handles`, which a refusal leaves where they were.
pub fn transmit<'a>(
    ch: impl Into<Ref<'a, Channel, Only>>,
    op: u16,
    request_id: u64,
    body: &[u8],
    handles: &mut Carried,
) -> bool {
    send_from(ch.into(), &mut Scrubbed(WireMsg::new()), op, request_id, body, handles)
}

pub(crate) fn send_from(
    ch: Ref<'_, Channel, Only>,
    msg: &mut Scrubbed,
    op: u16,
    request_id: u64,
    body: &[u8],
    handles: &mut Carried,
) -> bool {
    let Some(n) = librsproto::encode(msg.0.payload_mut(), op, request_id, 0, body, handles.len() as u16) else {
        return false;
    };
    msg.0.set_len(n).is_ok() && ch.send(&msg.0, handles).is_ok()
}

/// Send `op` on `ch`, moving `handles`. For callers not yet on libos; the typed form is
/// [`transmit`].
// TODO(raw-handle-shims): goes when its last caller is on libos.
pub fn send(ch: u64, op: u16, request_id: u64, body: &[u8], handles: &[u64]) -> bool {
    let mut carried = Carried::new();
    for &h in handles {
        // SAFETY: the caller's own handle, which a send moves; one a refusal leaves is handed back
        // below rather than closed.
        let h = unsafe { Handle::<Channel, Only>::from_raw(RawHandle(h), Rights::empty()) };
        if let Err(h) = carried.push(h) {
            h.into_raw();
            break;
        }
    }
    let sent = carried.len() == handles.len() && transmit(borrowed(ch), op, request_id, body, &mut carried);
    // What the kernel did not take is still the caller's.
    for i in 0..carried.len() {
        if let Some(h) = carried.take::<Channel, Only>(i) {
            h.into_raw();
        }
    }
    sent
}

/// Send `op` and wait for its reply, as `(is_error, body)`; `None` on a send failure or dead peer.
pub fn call_on<'a>(
    ch: impl Into<Ref<'a, Channel, Only>>,
    op: u16,
    request_id: u64,
    body: &[u8],
    handles: &mut Carried,
) -> Option<(bool, Vec<u8>)> {
    let ch = ch.into();
    if !transmit(ch, op, request_id, body, handles) {
        return None;
    }
    answer_on(ch, request_id, Deadline::NEVER)
}

/// Send `op` and wait for its reply, as `(is_error, body)`; `None` on a send failure or dead peer.
/// For callers not yet on libos; the typed form is [`call_on`].
// TODO(raw-handle-shims): goes when its last caller is on libos.
pub fn call(ch: u64, op: u16, request_id: u64, body: &[u8], handles: &[u64]) -> Option<(bool, Vec<u8>)> {
    if !send(ch, op, request_id, body, handles) {
        return None;
    }
    answer(ch, request_id, u64::MAX)
}

/// The reply to `request_id` on `ch`, skipping other messages; `None` on a gone peer or `deadline`.
pub fn answer_on<'a>(
    ch: impl Into<Ref<'a, Channel, Only>>,
    request_id: u64,
    deadline: Deadline,
) -> Option<(bool, Vec<u8>)> {
    let ch = ch.into();
    let mut set = WaitSet::<1>::new();
    set.add(ch).ok()?;
    loop {
        ready(&set, deadline)?;
        match receive(ch) {
            Ok(Some((_, rid, err, body))) if rid == request_id => return Some((err, body)),
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

/// The reply to `request_id` on `ch`, skipping other messages; `None` on a gone peer or `deadline`.
/// For callers not yet on libos; the typed form is [`answer_on`].
// TODO(raw-handle-shims): goes when its last caller is on libos.
pub fn answer(ch: u64, request_id: u64, deadline: u64) -> Option<(bool, Vec<u8>)> {
    answer_on(borrowed(ch), request_id, Deadline::at(deadline))
}

/// Resolve `path` in `ns` with `rights`; `None` if it is not there.
pub fn lookup_in<'a, M: CanLookup + 'a>(
    ns: impl Into<Ref<'a, Namespace, M>>,
    path: &str,
    rights: Rights,
) -> Option<Handle<Channel, Only>> {
    block_on(ns.into().lookup(path, rights)).ok()
}

/// Resolve `path` in `ns` with `rights`; `0` if it is not there. For callers not yet on libos; the
/// typed form is [`lookup_in`].
// TODO(raw-handle-shims): goes when its last caller is on libos.
pub fn lookup(ns: u64, path: &[u8], rights: u64) -> u64 {
    // SAFETY: the caller's own namespace handle, open for the call, as this has always asked.
    let ns = unsafe { Ref::<Namespace, NsReadOnly>::from_raw(RawHandle(ns), Rights::empty()) };
    let Ok(path) = core::str::from_utf8(path) else {
        return 0;
    };
    lookup_in(ns, path, Rights::from_bits(rights)).map_or(0, |h| h.into_raw().0)
}

/// Borrow the caller's own channel endpoint for one call.
pub(crate) fn borrowed<'a>(ch: u64) -> Ref<'a, Channel, Only> {
    // SAFETY: the caller's own handle, open for the call, as the raw functions have always asked.
    unsafe { Ref::from_raw(RawHandle(ch), Rights::empty()) }
}
