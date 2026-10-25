//! **The control channel a supervisor spawned a service with**, read from the service's side: one
//! op byte per message ([`CTRL_OP_SHUTDOWN`](crate::abi::CTRL_OP_SHUTDOWN)).
//!
//! **A closed channel is an answer, not an empty one.** Its supervisor has gone, and the channel
//! stays signalled for good, so a service that went on waiting on it would spin. [`Control::Closed`]
//! says to take it out of the wait set.

use crate::abi::{IPC_HEADER_SIZE, IPC_MSG_SIZE};
use crate::channel::channel_recv;
use crate::error::KError;

/// What a control channel held.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Control {
    /// An op byte: [`CTRL_OP_SHUTDOWN`](crate::abi::CTRL_OP_SHUTDOWN), or one this service does
    /// not know and should ignore.
    Op(u8),
    /// A message with no op in it, or nothing queued.
    Nothing,
    /// The supervisor's end has gone: stop waiting on this channel.
    Closed,
}

/// Take one message from `control` without blocking. Any handle it carried is closed: no op here
/// carries one.
pub fn recv(control: u64) -> Control {
    let mut msg = [0u8; IPC_MSG_SIZE];
    let _sent = match channel_recv(control, &mut msg) {
        Ok(sent) => sent,
        Err(r) if r == KError::PeerClosed.as_i32() as i64 => return Control::Closed,
        Err(_) => return Control::Nothing,
    };
    let len = u32::from_le_bytes([msg[4], msg[5], msg[6], msg[7]]) as usize;
    op_of(&msg[IPC_HEADER_SIZE..IPC_HEADER_SIZE + len.min(IPC_MSG_SIZE - IPC_HEADER_SIZE)])
}

/// A control message's payload as an op: its first byte, if it has one.
pub fn op_of(payload: &[u8]) -> Control {
    payload.first().map_or(Control::Nothing, |&op| Control::Op(op))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::CTRL_OP_SHUTDOWN;

    #[test]
    fn a_payload_is_its_first_byte() {
        assert_eq!(op_of(&[CTRL_OP_SHUTDOWN]), Control::Op(CTRL_OP_SHUTDOWN));
        assert_eq!(op_of(&[7, 1]), Control::Op(7));
        assert_eq!(op_of(&[]), Control::Nothing);
    }
}
