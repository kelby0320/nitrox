//! The broker's side of a channel: messages in and out, a wait with a deadline, a lookup.

use alloc::vec::Vec;
use libkern::{KError, Rights, scrub};
use libos::{CanLookup, Carried, Channel, Deadline, Handle, Msg, Namespace, NsReadOnly, Only, WaitSet, block_on, time};
use librsproto::views::{Outcome, build_outcome};
use librsproto::{RS_FLAG_REPLY, decode, server};

/// A channel endpoint this process owns.
pub type Ch = Handle<Channel, Only>;

/// The root namespace this process was started with.
pub type Root = Handle<Namespace, NsReadOnly>;

/// A message scrubbed where it lies on every way out: a password passes through it, and a move
/// would leave a copy.
pub struct Scrubbed(Msg);

impl Scrubbed {
    /// An empty message.
    pub fn new() -> Self {
        Scrubbed(Msg::new())
    }
}

impl Drop for Scrubbed {
    fn drop(&mut self) {
        scrub(self.0.payload_mut());
    }
}

/// A message as received: its op, request id, body, and the handles it carried, closed unless taken.
pub type Message = (u16, u64, Vec<u8>, Carried);
/// [`Message`] with its flags: an answer from a server may be an error reply.
pub type Incoming = (u16, u64, u32, Vec<u8>, Carried);

/// Receive one message on `ch`. `Ok(None)` if nothing was queued or it did not decode, `Err(())`
/// if the peer has gone. The message is scrubbed as this returns: a body may carry a password,
/// and the copy returned is the caller's to scrub.
pub fn recv_full(ch: &Ch) -> Result<Option<Incoming>, ()> {
    let mut msg = Scrubbed::new();
    let sent = match ch.recv(&mut msg.0) {
        Ok(sent) => sent,
        Err(e) if e.code() == KError::PeerClosed => return Err(()),
        Err(_) => return Ok(None),
    };
    // A message that does not decode closes what it carried, as `sent` drops.
    Ok(decode(msg.0.payload()).ok().map(|m| (m.op, m.request_id, m.flags, m.body.to_vec(), sent)))
}

/// [`recv_full`], without the flags.
pub fn recv(ch: &Ch) -> Result<Option<Message>, ()> {
    recv_full(ch).map(|m| m.map(|(op, request_id, _, body, sent)| (op, request_id, body, sent)))
}

/// Send one message on `ch` without blocking; whether it went.
pub fn send(ch: &Ch, op: u16, request_id: u64, flags: u32, body: &[u8]) -> bool {
    server::send(ch, op, request_id, flags, body, &mut Carried::new()).is_ok()
}

/// [`send`] for a request whose body may carry a password: built in a message scrubbed on every
/// way out. The caller scrubs its own copy of `body`.
pub fn send_request(ch: &Ch, op: u16, request_id: u64, body: &[u8]) -> bool {
    let mut msg = Scrubbed::new();
    let Some(len) = librsproto::encode(msg.0.payload_mut(), op, request_id, 0, body, 0) else {
        return false;
    };
    msg.0.set_len(len).is_ok() && ch.send(&msg.0, &mut Carried::new()).is_ok()
}

/// An error reply with no reason: an `ErrorBody`, as every server sends one.
pub fn reply_error(ch: &Ch, op: u16, request_id: u64, err: KError) {
    let _ = server::reply_error(ch, op, request_id, err, b"");
}

/// Answer request `op` with `outcome` and `reason`, the reason cut to a note when it will not fit.
pub fn reply_outcome(ch: &Ch, op: u16, request_id: u64, outcome: Outcome, reason: &str) {
    let mut body = [0u8; 512];
    let n = build_outcome(&mut body, outcome, reason.as_bytes())
        .or_else(|| build_outcome(&mut body, outcome, b"(reason too long)"))
        .unwrap_or(0);
    let _ = send(ch, op, request_id, RS_FLAG_REPLY, &body[..n]);
}

/// Nanoseconds on the monotonic clock, `0` if it cannot be read.
pub fn now_ns() -> u64 {
    time::now().unwrap_or(0)
}

/// Wait for `ch` to signal, until `deadline` on the monotonic clock; whether it did.
pub fn wait_until(ch: &Ch, deadline: u64) -> bool {
    let mut set = WaitSet::<1>::new();
    set.add(ch).is_ok() && set.wait(Deadline::at(deadline)).is_ok_and(|s| !s.is_empty())
}

/// Resolve `path` in `ns` with `rights`, waiting for the answer.
pub fn lookup<T, M, N: CanLookup>(ns: &Handle<Namespace, N>, path: &str, rights: Rights) -> Option<Handle<T, M>> {
    block_on(ns.lookup(path, rights)).ok()
}
