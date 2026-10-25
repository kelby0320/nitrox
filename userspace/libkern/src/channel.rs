//! **Receiving a message, and the handles it carried.** `sys_channel_recv` installs every handle a
//! message carries in the receiver's table, whether or not the receiver asked for it, so a receiver
//! that leaves them lets any sender fill its table, and keep alive whatever they pin. [`Received`]
//! closes, when it drops, every handle not taken from it: a receiver forgets nothing by ignoring it.

use crate::abi::{IPC_HANDLE_MAX, IPC_MSG_SIZE};
use crate::syscall::{SYS_CHANNEL_RECV, SYS_HANDLE_CLOSE, syscall1, syscall4};

/// The handles one message brought, closed when this drops unless taken.
pub struct Received {
    handles: [u64; IPC_HANDLE_MAX],
    count: usize,
}

impl Received {
    /// How many handles the message carried, taken or not.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Take handle `i`: it is the caller's now, to keep or close. `None` past the count, or if
    /// taken already.
    pub fn take(&mut self, i: usize) -> Option<u64> {
        let h = core::mem::take(self.handles[..self.count].get_mut(i)?);
        (h != 0).then_some(h)
    }

    /// Take every handle still here, in order, with `0` for one taken already or past the count.
    pub fn take_all(&mut self) -> [u64; IPC_HANDLE_MAX] {
        core::mem::replace(&mut self.handles, [0; IPC_HANDLE_MAX])
    }
}

impl Drop for Received {
    fn drop(&mut self) {
        for &h in &self.handles[..self.count] {
            if h != 0 {
                // SAFETY: a handle the kernel installed in this process for this message, which no
                // one took.
                unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
            }
        }
    }
}

/// Receive one message on `ch` into `msg`. `Err` with the syscall's status: `WouldBlock` for an
/// empty ring, `PeerClosed`, or a refusal, none of which installs anything.
pub fn channel_recv(ch: u64, msg: &mut [u8; IPC_MSG_SIZE]) -> Result<Received, i64> {
    // SAFETY: a unique reference is valid for writes and aliased by nothing during the call.
    unsafe { channel_recv_raw(ch, msg) }
}

/// [`channel_recv`] into a buffer reached by pointer, such as a `static mut`.
///
/// # Safety
///
/// `msg` is valid for writes, and nothing else reads or writes it during the call.
pub unsafe fn channel_recv_raw(ch: u64, msg: *mut [u8; IPC_MSG_SIZE]) -> Result<Received, i64> {
    let mut got = Received { handles: [0; IPC_HANDLE_MAX], count: 0 };
    // SAFETY: a whole message's buffer, by the caller's contract; room for every handle a message
    // can carry, and a count.
    let rr = unsafe {
        syscall4(SYS_CHANNEL_RECV, ch, msg as u64, got.handles.as_mut_ptr() as u64, (&raw mut got.count) as u64)
    };
    if rr != 0 {
        got.count = 0;
        return Err(rr);
    }
    got.count = got.count.min(IPC_HANDLE_MAX);
    Ok(got)
}
