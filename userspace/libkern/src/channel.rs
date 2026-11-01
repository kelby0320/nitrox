//! **Receiving a message, and the handles it carried.** `sys_channel_recv` installs every handle a
//! message carries in the receiver's table, whether or not the receiver asked for it, so a receiver
//! that leaves them lets any sender fill its table, and keep alive whatever they pin. [`Received`]
//! closes, when it drops, every handle not taken from it: a receiver forgets nothing by ignoring it.

use crate::abi::{CLOCK_MONOTONIC, IPC_HANDLE_MAX, IPC_MSG_SIZE};
use crate::error::KError;
use crate::syscall::{SYS_CHANNEL_RECV, SYS_CLOCK_READ, SYS_WAIT, syscall2, syscall4};

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
                close(h);
            }
        }
    }
}

/// Close `h`, one of a message's handles that no one took.
#[cfg(not(test))]
fn close(h: u64) {
    use crate::syscall::{SYS_HANDLE_CLOSE, syscall1};
    // SAFETY: a handle the kernel installed in this process for this message. `take` and
    // `take_all` zero each handle they hand out, so one still here is no one else's, and this
    // closes it once.
    unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
}

/// On the host, where handle-close is Linux's `read`, a close is recorded instead.
#[cfg(test)]
fn close(h: u64) {
    tests::CLOSED.with(|c| c.borrow_mut().push(h));
}

/// **What a failed receive says about its channel.**
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RecvFailure {
    /// The peer has gone and the ring is empty, for good: stop waiting on the channel.
    Closed,
    /// The ring is empty and the peer is there.
    Empty,
    /// Anything else: `OutOfMemory` leaves the message queued, so a wait on the channel fires
    /// again at once. [`pause_held`] before it, or that wait is a spin.
    Held,
}

impl RecvFailure {
    /// The failure a receive's `status` means.
    pub fn of(status: i64) -> Self {
        match status {
            s if s == KError::PeerClosed.as_i32() as i64 => Self::Closed,
            s if s == KError::WouldBlock.as_i32() as i64 => Self::Empty,
            _ => Self::Held,
        }
    }
}

/// How long a [`RecvFailure::Held`] pauses.
const HELD_PAUSE_NS: u64 = 10_000_000;

/// Wait out a [`RecvFailure::Held`], on no handles, which the kernel never refuses.
pub fn pause_held() {
    let mut now = 0u64;
    // SAFETY: `now` is a writable u64 out-param on this frame.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut now) as u64) };
    // SAFETY: no handles and no results, so the kernel reads and writes neither.
    unsafe { syscall4(SYS_WAIT, 0, 0, 0, now.saturating_add(HELD_PAUSE_NS)) };
}

/// Receive one message on `ch` into `msg`. `Err` with the syscall's status: `WouldBlock` for an
/// empty ring, `PeerClosed`, or a refusal, none of which installs anything.
// TODO(recv-kerror): an `i64` status, not a `KError`; the typed error comes with libos's receive.
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
    let mut handles = [0; IPC_HANDLE_MAX];
    let mut count = 0usize;
    // SAFETY: a whole message's buffer, by the caller's contract; room for every handle a message
    // can carry, and a count.
    let rr =
        unsafe { syscall4(SYS_CHANNEL_RECV, ch, msg as u64, handles.as_mut_ptr() as u64, (&raw mut count) as u64) };
    settle(rr, handles, count)
}

/// What a receive that returned `rr` holds. On an error nothing: the kernel installed no handle,
/// or reclaimed what it had written out.
fn settle(rr: i64, handles: [u64; IPC_HANDLE_MAX], count: usize) -> Result<Received, i64> {
    if rr != 0 {
        return Err(rr);
    }
    Ok(Received { handles, count: count.min(IPC_HANDLE_MAX) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::vec::Vec;

    std::thread_local! {
        /// The handles [`close`] was asked to close, in order, on this test's thread.
        pub(super) static CLOSED: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    }

    fn closed() -> Vec<u64> {
        CLOSED.with(|c| c.borrow().clone())
    }

    fn received(handles: &[u64]) -> Received {
        let mut all = [0; IPC_HANDLE_MAX];
        all[..handles.len()].copy_from_slice(handles);
        settle(0, all, handles.len()).unwrap()
    }

    #[test]
    fn a_drop_closes_each_handle_not_taken_once_and_no_other() {
        let mut r = received(&[11, 12, 13]);
        assert_eq!(r.take(1), Some(12));
        drop(r);
        assert_eq!(closed(), [11, 13]);
    }

    #[test]
    fn a_handle_is_taken_once_and_none_past_the_count() {
        let mut r = received(&[11, 12]);
        assert_eq!((r.take(0), r.take(0)), (Some(11), None));
        assert_eq!((r.take(2), r.take(IPC_HANDLE_MAX)), (None, None));
        assert_eq!(r.count(), 2, "the count is what came, taken or not");
        drop(r);
        assert_eq!(closed(), [12]);
    }

    #[test]
    fn take_all_leaves_nothing_to_close() {
        let mut r = received(&[11, 12, 13]);
        assert_eq!(r.take(0), Some(11));
        assert_eq!(r.take_all()[..3], [0, 12, 13]);
        drop(r);
        assert_eq!(closed(), []);
    }

    #[test]
    fn an_error_holds_nothing_whatever_was_written_out() {
        let written = [21; IPC_HANDLE_MAX];
        assert_eq!(settle(-13, written, 3).err(), Some(-13));
        assert_eq!(closed(), [], "the kernel reclaimed them");
    }

    #[test]
    fn a_count_past_what_a_message_carries_is_held_to_it() {
        let r = settle(0, [7; IPC_HANDLE_MAX], IPC_HANDLE_MAX + 5).unwrap();
        assert_eq!(r.count(), IPC_HANDLE_MAX);
        drop(r);
        assert_eq!(closed().len(), IPC_HANDLE_MAX);
    }
}
