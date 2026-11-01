//! **The control channel a supervisor spawned a service with**, read from the service's side: one
//! op byte per message ([`CTRL_OP_SHUTDOWN`](crate::abi::CTRL_OP_SHUTDOWN)).
//!
//! **A closed channel is an answer, not an empty one.** Its supervisor has gone, and the channel
//! stays signalled for good, so a service that went on waiting on it would spin. [`Control::Closed`]
//! says to take it out of the wait set.

use crate::abi::{IPC_HEADER_SIZE, IPC_MSG_SIZE};
use crate::channel::{RecvFailure, channel_recv, close, pause_held};

/// What a control channel held.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Control {
    /// An op byte: [`CTRL_OP_SHUTDOWN`](crate::abi::CTRL_OP_SHUTDOWN), or one this service does
    /// not know and should ignore.
    Op(u8),
    /// A message with no op in it, nothing queued, or a receive that failed and has paused.
    Nothing,
    /// The supervisor's end has gone, and this end is closed: stop waiting on this channel.
    Closed,
}

/// Take one message from `control` without blocking. Any handle it carried is closed: no op here
/// carries one.
pub fn recv(control: u64) -> Control {
    let mut msg = [0u8; IPC_MSG_SIZE];
    let _sent = match channel_recv(control, &mut msg) {
        Ok(sent) => sent,
        Err(status) => return failed(control, status, pause_held),
    };
    let len = u32::from_le_bytes([msg[4], msg[5], msg[6], msg[7]]) as usize;
    op_of(&msg[IPC_HEADER_SIZE..IPC_HEADER_SIZE + len.min(IPC_MSG_SIZE - IPC_HEADER_SIZE)])
}

/// What a receive on `control` that failed with `status` means, `pause` run first if its message
/// is queued. A channel that is closed for good is this process's to close.
fn failed(control: u64, status: i64, pause: impl FnOnce()) -> Control {
    match RecvFailure::of(status) {
        RecvFailure::Closed => {
            close(control);
            Control::Closed
        }
        RecvFailure::Empty => Control::Nothing,
        RecvFailure::Held => {
            pause();
            Control::Nothing
        }
    }
}

/// A control message's payload as an op: its first byte, if it has one.
pub fn op_of(payload: &[u8]) -> Control {
    payload.first().map_or(Control::Nothing, |&op| Control::Op(op))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::CTRL_OP_SHUTDOWN;
    use crate::channel::tests::closed;
    use crate::error::KError;

    #[test]
    fn a_payload_is_its_first_byte() {
        assert_eq!(op_of(&[CTRL_OP_SHUTDOWN]), Control::Op(CTRL_OP_SHUTDOWN));
        assert_eq!(op_of(&[7, 1]), Control::Op(7));
        assert_eq!(op_of(&[]), Control::Nothing);
    }

    #[test]
    fn only_a_held_receive_pauses() {
        let status = |e: KError| e.as_i32() as i64;
        let paused = |s: i64| {
            let mut ran = false;
            (failed(7, s, || ran = true), ran)
        };
        assert_eq!(paused(status(KError::OutOfMemory)), (Control::Nothing, true));
        assert_eq!(paused(status(KError::WouldBlock)), (Control::Nothing, false));
        assert_eq!(paused(status(KError::PeerClosed)), (Control::Closed, false));
    }

    #[test]
    fn only_a_channel_closed_for_good_is_closed() {
        let status = |e: KError| e.as_i32() as i64;
        failed(7, status(KError::OutOfMemory), || ());
        failed(7, status(KError::WouldBlock), || ());
        assert_eq!(closed(), []);
        failed(7, status(KError::PeerClosed), || ());
        assert_eq!(closed(), [7]);
    }
}
