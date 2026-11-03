//! IPC: a [`Msg`], and sending and receiving one on a [`Channel`] endpoint.
//!
//! **The handles a message carries move with it.** A send takes them from a [`Handles`], which
//! keeps them if the send is refused, so a refusal loses none and leaks none. A receive answers
//! the handles the message brought as a [`Handles`] too, which closes each not taken from it: a
//! sender cannot fill the receiver's table by attaching what it never asked for.

use libkern::{
    IPC_HANDLE_MAX, IPC_PAYLOAD_SIZE, IpcMsg, KError, Rights, SENDMODE_BLOCK, SENDMODE_BLOCKBOUNDED, SENDMODE_NOBLOCK,
};

use crate::error::{Error, Result, check};
use crate::exec::Op;
use crate::handle::{Channel, Handle, Handles, Only, both};
use crate::sys;
use crate::time::Deadline;

/// The handles one message carries.
pub type Carried = Handles<IPC_HANDLE_MAX>;

/// One IPC message: a header the kernel stamps, and up to [`IPC_PAYLOAD_SIZE`] bytes of payload.
#[repr(transparent)]
#[derive(Clone)]
pub struct Msg(IpcMsg);

impl Default for Msg {
    fn default() -> Self {
        Self::new()
    }
}

impl Msg {
    /// An empty message.
    pub const fn new() -> Self {
        Msg(IpcMsg::ZEROED)
    }

    /// The payload, as long as the header says and never longer than a message holds.
    pub fn payload(&self) -> &[u8] {
        &self.0.payload[..(self.0.header.payload_len as usize).min(IPC_PAYLOAD_SIZE)]
    }

    /// The whole payload area, to write into before [`set_len`](Self::set_len).
    pub fn payload_mut(&mut self) -> &mut [u8; IPC_PAYLOAD_SIZE] {
        &mut self.0.payload
    }

    /// Say the payload is its first `len` bytes; `TooLarge` past what a message holds.
    pub fn set_len(&mut self, len: usize) -> Result<()> {
        if len > IPC_PAYLOAD_SIZE {
            return Err(Error::from(KError::TooLarge));
        }
        self.0.header.payload_len = len as u32;
        Ok(())
    }

    /// Make `bytes` the payload; `TooLarge` past what a message holds.
    pub fn set(&mut self, bytes: &[u8]) -> Result<()> {
        self.set_len(bytes.len())?;
        self.0.payload[..bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    /// The sender's pid, stamped by the kernel.
    pub fn sender_pid(&self) -> u32 {
        self.0.header.sender_pid
    }

    /// The sender's flags, carried as sent.
    pub fn flags(&self) -> u16 {
        self.0.header.flags
    }

    /// Set the flags the receiver will read.
    pub fn set_flags(&mut self, flags: u16) {
        self.0.header.flags = flags;
    }

    /// When the kernel queued it, in monotonic nanoseconds.
    pub fn timestamp(&self) -> u64 {
        self.0.header.timestamp
    }
}

impl Handle<Channel, Only> {
    /// A connected pair of endpoints whose rings hold `depth` messages each (`0` for the
    /// kernel's default).
    pub fn pair(depth: u32) -> Result<(Self, Self)> {
        let mut ends = [0; 2];
        check(sys::channel_create(depth as u64, &mut ends))?;
        let rights = Rights::DUPLICATE | Rights::TRANSFER | Rights::INSPECT | Rights::WAIT;
        Ok((Handle::fresh(ends[0], rights), Handle::fresh(ends[1], rights)))
    }
}

both!([] Channel, Only {
    /// Send `msg` and `handles`, refused at once with `WouldBlock` if the peer's ring is full.
    pub fn send(&self, msg: &Msg, handles: &mut Carried) -> Result<()> {
        send(self.raw().0, msg, handles, SENDMODE_NOBLOCK, 0).map(|_| ())
    }

    /// Send `msg` and `handles`, held by the kernel while the ring is full: the `Op` completes
    /// when the peer has it.
    pub fn send_held(&self, msg: &Msg, handles: &mut Carried) -> Result<Op> {
        send(self.raw().0, msg, handles, SENDMODE_BLOCK, 0).map(Op::status)
    }

    /// As [`send_held`](Self::send_held), given up at `deadline`: the `Op` completes `TimedOut`,
    /// and the message and the handles it carried are dropped.
    pub fn send_by(&self, msg: &Msg, handles: &mut Carried, deadline: Deadline) -> Result<Op> {
        send(self.raw().0, msg, handles, SENDMODE_BLOCKBOUNDED, deadline.ns()).map(Op::status)
    }

    /// Receive one message into `msg`, and the handles it brought. `WouldBlock` while the ring
    /// is empty; `PeerClosed` once the peer has gone and nothing is left.
    pub fn recv(&self, msg: &mut Msg) -> Result<Carried> {
        let mut raw = [0; IPC_HANDLE_MAX];
        let mut count = 0;
        check(sys::channel_recv(self.raw().0, &mut msg.0, &mut raw, &mut count))?;
        Ok(Handles::arrived(raw, count))
    }
});

fn send(ch: u64, msg: &Msg, handles: &mut Carried, mode: u64, deadline: u64) -> Result<u64> {
    if handles.raw().contains(&0) {
        return Err(Error::from(KError::InvalidArgument));
    }
    let r = check(sys::channel_send(ch, &msg.0, handles.raw(), mode, deadline))?;
    handles.moved();
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MapReadWrite, Memory, block_on};

    fn pair() -> (Handle<Channel, Only>, Handle<Channel, Only>) {
        Handle::<Channel, Only>::pair(2).unwrap()
    }

    #[test]
    fn a_message_and_its_handles_arrive() {
        sys::reset();
        let (a, b) = pair();
        let mut msg = Msg::new();
        msg.set(b"ping").unwrap();
        let mem = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        let sent = mem.raw();
        let mut out = Carried::new();
        out.push(mem).ok().unwrap();
        a.send(&msg, &mut out).unwrap();
        assert!(out.is_empty() && !sys::is_open(sent.0), "moved, not kept or closed twice");
        let mut got = Msg::new();
        let mut brought = b.recv(&mut got).unwrap();
        assert_eq!((got.payload(), got.sender_pid()), (&b"ping"[..], 1));
        let mem: Handle<Memory, MapReadWrite> = brought.take(0).unwrap();
        assert_eq!(mem.stat().unwrap().size, 4096);
        drop((out, brought));
        assert_eq!(sys::stray_closes(), 0);
    }

    #[test]
    fn a_refused_send_keeps_its_handles_and_closes_them_once() {
        sys::reset();
        let (a, b) = pair();
        let mut out = Carried::new();
        out.push(Handle::<Memory, MapReadWrite>::create(4096).unwrap()).ok().unwrap();
        let held = out.raw()[0];
        drop(b);
        assert_eq!(a.send(&Msg::new(), &mut out).err().unwrap().code(), KError::PeerClosed);
        assert!(sys::is_open(held) && out.len() == 1, "nothing lost");
        drop(out);
        assert!(!sys::is_open(held), "and nothing leaked");
    }

    #[test]
    fn a_full_ring_refuses_a_send_and_holds_a_held_one() {
        sys::reset();
        let (a, b) = pair();
        let mut none = Carried::new();
        a.send(&Msg::new(), &mut none).unwrap();
        a.send(&Msg::new(), &mut none).unwrap();
        assert_eq!(a.send(&Msg::new(), &mut none).err().unwrap().code(), KError::WouldBlock);
        let mut op = a.send_held(&Msg::new(), &mut none).unwrap();
        let mut cx = core::task::Context::from_waker(core::task::Waker::noop());
        assert!(core::pin::Pin::new(&mut op).poll(&mut cx).is_pending(), "held until there is room");
        let mut m = Msg::new();
        for _ in 0..3 {
            b.recv(&mut m).unwrap();
        }
        assert!(block_on(op).is_ok(), "delivered");
        assert_eq!(b.recv(&mut m).err().unwrap().code(), KError::WouldBlock);
    }

    #[test]
    fn a_bounded_send_held_past_its_deadline_times_out_and_drops_its_handles() {
        sys::reset();
        let (a, _b) = pair();
        let mut none = Carried::new();
        a.send(&Msg::new(), &mut none).unwrap();
        a.send(&Msg::new(), &mut none).unwrap();
        let mut out = Carried::new();
        out.push(Handle::<Memory, MapReadWrite>::create(4096).unwrap()).ok().unwrap();
        let held = out.raw()[0];
        let op = a.send_by(&Msg::new(), &mut out, Deadline::after(1).unwrap()).unwrap();
        assert!(out.is_empty() && !sys::is_open(held), "the kernel took them at the hold");
        assert_eq!(block_on(op).err().unwrap().code(), KError::TimedOut);
    }

    #[test]
    fn a_receive_says_why_it_has_nothing() {
        sys::reset();
        let (a, b) = pair();
        let mut m = Msg::new();
        assert_eq!(b.recv(&mut m).err().unwrap().code(), KError::WouldBlock);
        a.send(&Msg::new(), &mut Carried::new()).unwrap();
        drop(a);
        assert!(b.recv(&mut m).is_ok(), "what was sent before the close still arrives");
        assert_eq!(b.recv(&mut m).err().unwrap().code(), KError::PeerClosed);
    }

    #[test]
    fn a_payload_is_bounded_however_the_header_reads() {
        let mut m = Msg::new();
        assert_eq!(m.set_len(IPC_PAYLOAD_SIZE + 1).err().unwrap().code(), KError::TooLarge);
        m.0.header.payload_len = u32::MAX;
        assert_eq!(m.payload().len(), IPC_PAYLOAD_SIZE);
    }

    #[test]
    fn a_send_of_an_endpoint_into_its_own_ring_or_a_handle_twice_is_refused() {
        sys::reset();
        let (a, b) = pair();
        let mut out = Carried::new();
        out.push(b).ok().unwrap();
        assert_eq!(a.send(&Msg::new(), &mut out).err().unwrap().code(), KError::InvalidArgument);
        assert_eq!(out.len(), 1, "kept");
        let mut taken = Carried::new();
        let mem = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        taken.push(mem).ok().unwrap();
        let _back: Option<Handle<Memory, MapReadWrite>> = taken.take(0);
        assert_eq!(a.send(&Msg::new(), &mut taken).err().unwrap().code(), KError::InvalidArgument, "a hole");
    }
}
