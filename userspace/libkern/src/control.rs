//! **The control channel a supervisor spawned a service with**, read from the service's side: one
//! op byte per message ([`CTRL_OP_SHUTDOWN`](crate::abi::CTRL_OP_SHUTDOWN)).
//!
//! A thin wrapper over `sys_channel_recv`, here rather than copied into each service because five
//! read it: `heartbeat`, and since administration Part E.2 every server `service --stop` can stop
//! — the terminal server, the clipboard, the input server and the compositor.
//!
//! **A closed channel is an answer, not an empty one.** Its supervisor has gone, and the channel
//! stays signalled for good, so a service that went on waiting on it would spin. [`Control::Closed`]
//! says to take it out of the wait set; `heartbeat`'s own copy of this read did not, which is how
//! that was found.

use crate::abi::{IPC_HEADER_SIZE, IPC_MSG_SIZE};
use crate::error::KError;
use crate::syscall::{SYS_CHANNEL_RECV, SYS_HANDLE_CLOSE, syscall1, syscall4};

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
    let mut handles = [0u64; 8];
    let mut count = 0usize;
    // SAFETY: valid recv out-params on this frame; a non-blocking receive on a handle the caller
    // owns.
    let r = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            control,
            msg.as_mut_ptr() as u64,
            handles.as_mut_ptr() as u64,
            (&raw mut count) as u64,
        )
    };
    if r == KError::PeerClosed.as_i32() as i64 {
        return Control::Closed;
    }
    if r != 0 {
        return Control::Nothing;
    }
    for &h in &handles[..count.min(handles.len())] {
        // SAFETY: a handle the kernel just installed in this process.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
    }
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
