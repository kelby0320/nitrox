//! I/O on a resource through `io_submit`, and the notification queue.

use libkern::{IO_OPCODE_READ, IO_OPCODE_WRITE, IoOp, KError, Notification};

use crate::Deadline;

use crate::error::{Result, check};
use crate::exec::Op;
use crate::handle::{CanMapRead, CanMapWrite, CanRead, CanWrite, File, Handle, Memory, Notify, Only, Resource, both};
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

both!([M: CanMapRead] File, M {
    /// Read up to `len` bytes of the file from `offset` into `buf` at `buf_offset`: how many, short
    /// at the file's end.
    pub async fn read<MB: CanMapWrite>(&self, buf: &Handle<Memory, MB>, buf_offset: u64, offset: u64, len: u64) -> Result<usize> {
        let op = IoOp { opcode: IO_OPCODE_READ, flags: 0, buffer: buf.raw().0, buf_offset, offset, length: len };
        Ok(Op::submit_by(self.raw(), &op, Deadline::NEVER)?.await?.result as usize)
    }
});

both!([M: CanMapWrite] File, M {
    /// Write `len` bytes at `offset` from `buf` at `buf_offset`, inside the file: a grow moves its end.
    pub async fn write<MB: CanMapRead>(&self, buf: &Handle<Memory, MB>, buf_offset: u64, offset: u64, len: u64) -> Result<usize> {
        let op = IoOp { opcode: IO_OPCODE_WRITE, flags: 0, buffer: buf.raw().0, buf_offset, offset, length: len };
        Ok(Op::submit_by(self.raw(), &op, Deadline::NEVER)?.await?.result as usize)
    }

    /// **Copy `len` bytes of `src` from `src_offset` to `offset`**, at most
    /// [`MAX_USER_COPY_SIZE`](libkern::MAX_USER_COPY_SIZE) a call: a window of a stream, whose pages
    /// are let go once it completes (`IO_NOCACHE`), the destination's once on the device, and the
    /// source's if it is clean. Onto its own overlapping bytes it is refused `InvalidArgument`.
    pub async fn copy_from<MS: CanMapRead>(&self, src: &Handle<File, MS>, src_offset: u64, offset: u64, len: u64) -> Result<usize> {
        let (buffer, buf_offset, flags) = (src.raw().0, src_offset, libkern::IO_NOCACHE);
        let op = IoOp { opcode: IO_OPCODE_WRITE, flags, buffer, buf_offset, offset, length: len };
        Ok(Op::submit_by(self.raw(), &op, Deadline::NEVER)?.await?.result as usize)
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
    use crate::{MapReadWrite, ReadWrite, Ref, block_on};
    use libkern::{RawHandle, Rights};

    #[test]
    fn a_file_is_written_read_short_at_its_end_and_copied() {
        sys::reset();
        let ns = crate::Handle::<crate::Namespace, crate::NsMutable>::create().unwrap();
        let rights = Rights::MAP_READ | Rights::MAP_WRITE;
        let a: Handle<File, MapReadWrite> = block_on(crate::fs::create(&ns, "/a", 6000, rights)).unwrap();
        let b: Handle<File, MapReadWrite> = block_on(crate::fs::create(&ns, "/b", 6000, rights)).unwrap();
        let buf = Handle::<Memory, MapReadWrite>::create(8192).unwrap();
        buf.map(8192).unwrap().write_from(0, &[7; 8192]).unwrap();
        assert_eq!(block_on(a.write(&buf, 0, 5000, 1000)).unwrap(), 1000);
        assert_eq!(block_on(a.write(&buf, 0, 5001, 1000)).err().map(|e| e.code()), Some(KError::InvalidArgument));
        assert_eq!(block_on(b.copy_from(&a, 4000, 0, 2000)).unwrap(), 2000);
        let back = Handle::<Memory, MapReadWrite>::create(8192).unwrap();
        assert_eq!(block_on(b.read(&back, 0, 0, 8192)).unwrap(), 6000, "short at the end");
        let mut got = [0; 2000];
        back.map(8192).unwrap().read_into(0, &mut got).unwrap();
        assert_eq!(got[..1000], [0; 1000], "a's bytes before the write");
        assert_eq!(got[1000..], [7; 1000]);
        let copy = sys::submitted().into_iter().rfind(|op| op.buffer == a.raw().0).unwrap();
        assert_eq!(copy.flags, libkern::IO_NOCACHE, "a copy streams, its windows let go");
        assert_eq!(block_on(b.copy_from(&b, 0, 8, 64)).err().map(|e| e.code()), Some(KError::InvalidArgument));
    }

    #[test]
    fn a_read_and_a_write_build_their_ops_and_complete() {
        sys::reset();
        let dev = sys::device();
        // SAFETY: the mock's device, open for the test.
        let res = unsafe { Ref::<Resource, ReadWrite>::from_raw(RawHandle(dev), Rights::READ) };
        let buf = Handle::<Memory, MapReadWrite>::create(8192).unwrap();
        sys::script_next(0, 12, true);
        assert_eq!(block_on(res.read(&buf, 100, 4096, 64)).unwrap(), 12);
        sys::script_next(0, 7, true);
        assert_eq!(block_on(res.write(&buf, 200, 512, 7)).unwrap(), 7);
        let op = |opcode, buf_offset, offset, length| IoOp {
            opcode,
            flags: 0,
            buffer: buf.raw().0,
            buf_offset,
            offset,
            length,
        };
        assert_eq!(sys::submitted(), [op(IO_OPCODE_READ, 100, 4096, 64), op(IO_OPCODE_WRITE, 200, 512, 7)]);
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
