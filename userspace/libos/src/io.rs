//! I/O on a resource through `io_submit`, and the notification queue.

use libkern::{IO_OPCODE_READ, IO_OPCODE_WRITE, IoOp, KError, Notification};

use crate::error::{Result, check};
use crate::exec::Op;
use crate::handle::{CanMapRead, CanMapWrite, CanRead, CanWrite, Handle, Memory, Notify, Only, Resource, both};
use crate::sys;

both!([M: CanRead] Resource, M {
    /// Read up to `len` bytes from the resource at `offset` into `buf` at `buf_offset`: how many
    /// it read.
    pub async fn read<MB: CanMapWrite>(&self, buf: &Handle<Memory, MB>, buf_offset: u64, offset: u64, len: u64) -> Result<usize> {
        let op = IoOp { opcode: IO_OPCODE_READ, flags: 0, buffer: buf.raw().0, buf_offset, offset, length: len };
        let done = Op::submit(self.raw(), &op)?.await?;
        Ok(done.result as usize)
    }
});

both!([M: CanWrite] Resource, M {
    /// Write up to `len` bytes from `buf` at `buf_offset` to the resource at `offset`: how many
    /// it wrote.
    pub async fn write<MB: CanMapRead>(&self, buf: &Handle<Memory, MB>, buf_offset: u64, offset: u64, len: u64) -> Result<usize> {
        let op = IoOp { opcode: IO_OPCODE_WRITE, flags: 0, buffer: buf.raw().0, buf_offset, offset, length: len };
        let done = Op::submit(self.raw(), &op)?.await?;
        Ok(done.result as usize)
    }
});

both!([] Notify, Only {
    /// The next notification, or `None` while the queue is empty: wait on the queue to block.
    pub fn recv(&self) -> Result<Option<Notification>> {
        let mut n = Notification::zeroed();
        match check(sys::notif_recv(self.raw().0, &mut n)) {
            Ok(_) => Ok(Some(n)),
            Err(e) if e.code() == KError::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MapReadWrite, ReadOnly, Ref, block_on};
    use libkern::{RawHandle, Rights};

    #[test]
    fn a_read_builds_an_op_and_completes() {
        sys::reset();
        sys::script_next(0, 12, true);
        let dev = sys::device();
        // SAFETY: the mock's device, open for the test.
        let res = unsafe { Ref::<Resource, ReadOnly>::from_raw(RawHandle(dev), Rights::READ) };
        let buf = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        assert_eq!(block_on(res.read(&buf, 0, 0, 64)).unwrap(), 12);
    }

    #[test]
    fn a_queue_answers_each_notification_then_none() {
        sys::reset();
        let mut n = Notification::zeroed();
        n.kind = 7;
        let q: Handle<Notify, Only> = Handle::fresh(sys::notify(&[n]), Rights::WAIT);
        assert_eq!(q.recv().unwrap().map(|n| n.kind), Some(7));
        assert!(q.recv().unwrap().is_none());
    }
}
