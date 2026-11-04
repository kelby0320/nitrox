//! **The control channel a supervisor spawned a service with**, read from the service's side: one
//! op byte a message ([`CTRL_OP_SHUTDOWN`](libkern::CTRL_OP_SHUTDOWN)).
//!
//! **A closed channel is an answer, not an empty one.** Its supervisor has gone, and the channel
//! stays signalled for good, so a service that went on waiting on it would spin: [`recv`] takes
//! it out of the service's hands.

use libkern::KError;

use crate::handle::{Channel, Handle, Only};
use crate::{Msg, time};

/// What a control channel held.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Control {
    /// An op byte: [`CTRL_OP_SHUTDOWN`](libkern::CTRL_OP_SHUTDOWN), or one this service does not
    /// know and should ignore.
    Op(u8),
    /// A message with no op in it, nothing queued, or a receive that failed and has paused.
    Nothing,
    /// The supervisor's end has gone, and this end is closed: there is no channel to wait on.
    Closed,
}

/// How long a receive that left its message queued pauses, as a wait on the channel would fire
/// again at once.
const HELD_PAUSE_NS: u64 = 10_000_000;

/// Take one message from `control` without blocking. Any handle it carried is closed: no op here
/// carries one. **[`Control::Closed`] takes the channel**, closing it and leaving `None`, as does
/// a `None` given.
pub fn recv(control: &mut Option<Handle<Channel, Only>>) -> Control {
    let Some(ch) = control else {
        return Control::Closed;
    };
    let mut msg = Msg::new();
    match ch.recv(&mut msg) {
        Ok(_carried) => msg.payload().first().map_or(Control::Nothing, |&op| Control::Op(op)),
        Err(e) if e.code() == KError::PeerClosed => {
            *control = None;
            Control::Closed
        }
        Err(e) if e.code() == KError::WouldBlock => Control::Nothing,
        Err(_) => {
            let _ = time::sleep(HELD_PAUSE_NS);
            Control::Nothing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::mock;
    use crate::{Carried, Handles};
    use libkern::CTRL_OP_SHUTDOWN;

    fn pair() -> (Handle<Channel, Only>, Option<Handle<Channel, Only>>) {
        mock::reset();
        let (supervisor, service) = Handle::pair(4).unwrap();
        (supervisor, Some(service))
    }

    fn send(to: &Handle<Channel, Only>, payload: &[u8], handles: &mut Carried) {
        let mut msg = Msg::new();
        msg.set(payload).unwrap();
        to.send(&msg, handles).unwrap();
    }

    #[test]
    fn an_op_is_its_first_byte_and_an_empty_message_is_none() {
        let (supervisor, mut control) = pair();
        send(&supervisor, &[CTRL_OP_SHUTDOWN, 9], &mut Handles::new());
        send(&supervisor, &[], &mut Handles::new());
        assert_eq!(recv(&mut control), Control::Op(CTRL_OP_SHUTDOWN));
        assert_eq!(recv(&mut control), Control::Nothing, "a message with no op");
        assert_eq!(recv(&mut control), Control::Nothing, "nothing queued");
        assert!(control.is_some());
    }

    #[test]
    fn a_handle_sent_with_an_op_is_closed() {
        let (supervisor, mut control) = pair();
        let (extra, peer) = Handle::pair(1).unwrap();
        let mut carried = Handles::new();
        assert!(carried.push(extra).is_ok());
        send(&supervisor, &[CTRL_OP_SHUTDOWN], &mut carried);
        assert_eq!(recv(&mut control), Control::Op(CTRL_OP_SHUTDOWN));
        let gone = peer.recv(&mut Msg::new()).map(|_| ()).map_err(|e| e.code());
        assert_eq!(gone, Err(KError::PeerClosed), "its peer sees it closed");
        assert_eq!(mock::stray_closes(), 0);
    }

    #[test]
    fn a_closed_supervisor_takes_the_channel_once() {
        let (supervisor, mut control) = pair();
        let raw = control.as_ref().map(|c| c.raw().0).unwrap();
        drop(supervisor);
        assert_eq!(recv(&mut control), Control::Closed);
        assert!(control.is_none() && !mock::is_open(raw), "closed, with no copy left");
        assert_eq!(recv(&mut control), Control::Closed, "and none to wait on after");
        assert_eq!(mock::stray_closes(), 0);
    }

    #[test]
    fn a_receive_that_left_its_message_queued_pauses_and_keeps_the_channel() {
        let (supervisor, mut control) = pair();
        send(&supervisor, &[CTRL_OP_SHUTDOWN], &mut Handles::new());
        mock::fail_next_recv(KError::OutOfMemory.as_i32());
        let slept = mock::sleeps();
        assert_eq!(recv(&mut control), Control::Nothing);
        assert_eq!(mock::sleeps(), slept + 1, "a pause, not a spin");
        assert_eq!(recv(&mut control), Control::Op(CTRL_OP_SHUTDOWN), "the message was still there");
        assert!(control.is_some());
    }
}
