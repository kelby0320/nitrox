//! One request, one reply. A password passes through these buffers, so **every buffer here is
//! zeroed once the kernel has the message**: a stack copy would outlive it.

use alloc::vec::Vec;
use libkern::{KError, Rights, scrub};
use libos::{
    CanLookup, Carried, Channel, Deadline, Error, Handle, Msg as WireMsg, Namespace, Only, Ref, WaitSet, block_on,
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

/// The index in `set` of a handle that is ready, once one is or `deadline` has passed: `None`
/// then too.
pub fn ready<const N: usize>(set: &WaitSet<'_, N>, deadline: Deadline) -> Option<usize> {
    set.wait(deadline).ok()?.iter().next().map(|(i, _)| i)
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

/// Resolve `path` in `ns` with `rights`; `None` if it is not there.
pub fn lookup_in<'a, M: CanLookup + 'a>(
    ns: impl Into<Ref<'a, Namespace, M>>,
    path: &str,
    rights: Rights,
) -> Option<Handle<Channel, Only>> {
    block_on(ns.into().lookup(path, rights)).ok()
}
