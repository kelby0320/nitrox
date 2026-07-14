//! Per-object operations on typed [`Handle`]s.
//!
//! Async methods (`lookup`, `read`, `write`) return futures resolving to their result
//! — drive them with [`block_on`](crate::block_on). Synchronous kernel calls
//! (`create`/`map`, `enumerate`, `recv`) return directly. Op availability is gated by
//! the sealed capability traits in [`crate::handle`], so misuse is a compile error.

use libkern::{IO_OPCODE_READ, IO_OPCODE_WRITE, IoOp, NsEntry, Notification, RawHandle, Rights};

use crate::error::{Error, ErrorKind, Result};
use crate::exec::Op;
use crate::handle::{
    CanLookup, CanMapRead, CanMapWrite, CanRead, CanWrite, Handle, MapReadWrite, MemMode, Memory,
    Namespace, Notify, Only, Resource,
};
use crate::sys;

// --- Memory ---------------------------------------------------------------

impl Handle<Memory, MapReadWrite> {
    /// Create an anonymous read-write `MemoryObject` of `size` bytes.
    pub fn create(size: usize) -> Result<Handle<Memory, MapReadWrite>> {
        let h = sys::memory_create(size as u64);
        if h < 0 {
            return Err(Error::from_status(h as i32));
        }
        // SAFETY: `sys_memory_create` returned a fresh full-rights MemoryObject handle.
        Ok(unsafe { Handle::from_raw(RawHandle(h as u64), Rights::MAP_READ | Rights::MAP_WRITE) })
    }
}

impl<M: MemMode> Handle<Memory, M> {
    /// Map the object into the address space (kernel-chosen address), returning the
    /// base pointer. Mapped with the mode's `MAP_*` rights. Valid while `self` lives.
    pub fn map(&self, len: usize) -> Result<*mut u8> {
        let addr = sys::memory_map(self.raw().0, 0, len as u64, M::map_rights());
        if addr < 0 {
            return Err(Error::from_status(addr as i32));
        }
        Ok(addr as usize as *mut u8)
    }

    /// Unmap a region previously returned by [`map`](Self::map).
    pub fn unmap(&self, addr: *mut u8, len: usize) -> Result<()> {
        let r = sys::memory_unmap(addr as usize as u64, len as u64);
        if r < 0 {
            return Err(Error::from_status(r as i32));
        }
        Ok(())
    }
}

// --- Resource (device / file) I/O -----------------------------------------

impl<M: CanRead> Handle<Resource, M> {
    /// Read up to `len` bytes from the resource (at `offset`) into `buf` (at
    /// `buf_offset`). Returns the number of bytes read.
    pub async fn read<MB: CanMapWrite>(
        &self,
        buf: &Handle<Memory, MB>,
        buf_offset: u64,
        offset: u64,
        len: u64,
    ) -> Result<usize> {
        let op = IoOp {
            opcode: IO_OPCODE_READ,
            flags: 0,
            buffer: buf.raw().0,
            buf_offset,
            offset,
            length: len,
        };
        let done = Op::submit(self.raw(), &op)?.await?;
        Ok(done.result as usize)
    }
}

impl<M: CanWrite> Handle<Resource, M> {
    /// Write up to `len` bytes from `buf` (at `buf_offset`) to the resource (at
    /// `offset`). Returns the number of bytes written.
    pub async fn write<MB: CanMapRead>(
        &self,
        buf: &Handle<Memory, MB>,
        buf_offset: u64,
        offset: u64,
        len: u64,
    ) -> Result<usize> {
        let op = IoOp {
            opcode: IO_OPCODE_WRITE,
            flags: 0,
            buffer: buf.raw().0,
            buf_offset,
            offset,
            length: len,
        };
        let done = Op::submit(self.raw(), &op)?.await?;
        Ok(done.result as usize)
    }
}

// --- Namespace ------------------------------------------------------------

impl<M: CanLookup> Handle<Namespace, M> {
    /// Resolve `path` to a handle, granting it `rights`. The resolved object's type
    /// is asserted by the caller via the `T2`/`M2` type arguments.
    ///
    /// # Safety
    /// The caller asserts the resolved object is of type `T2` with mode `M2` (the same
    /// trust assertion as [`Handle::from_raw`]).
    pub async unsafe fn lookup<T2, M2>(
        &self,
        path: &str,
        rights: Rights,
    ) -> Result<Handle<T2, M2>> {
        let po = sys::ns_lookup(self.raw().0, path.as_ptr() as u64, path.len() as u64, rights.bits());
        if po < 0 {
            return Err(Error::from_status(po as i32));
        }
        // The lookup completes through the PO; its `result` is the resolved handle
        // (or the completion carries a `NotFound`-style status).
        let done = Op::from_po(po as u64).await?;
        // SAFETY: forwarded from the caller's type assertion; `result` is the handle
        // the kernel resolved and installed into our table.
        Ok(unsafe { Handle::from_raw(RawHandle(done.result), rights) })
    }

    /// Write the `index`-th binding into `out`. `Ok(true)` if written, `Ok(false)`
    /// past the end. Iterate `0, 1, 2, …` until `false` (the `mounts`/`lsblk` shape).
    pub fn enumerate(&self, index: u32, out: &mut NsEntry) -> Result<bool> {
        let r = sys::ns_enumerate(self.raw().0, index as u64, out as *mut NsEntry as u64);
        if r < 0 {
            let e = Error::from_status(r as i32);
            return if e.kind() == ErrorKind::NotFound {
                Ok(false)
            } else {
                Err(e)
            };
        }
        Ok(true)
    }
}

// --- Notification queue ---------------------------------------------------

impl Handle<Notify, Only> {
    /// Dequeue one notification into `out`. `Ok(true)` if one was written, `Ok(false)`
    /// if the queue is empty. Block first by `sys_wait`-ing on this handle (the queue
    /// is waitable) — a libos async `next()` is a later addition.
    pub fn recv(&self, out: &mut Notification) -> Result<bool> {
        let r = sys::notif_recv(self.raw().0, out as *mut Notification as u64);
        if r < 0 {
            let e = Error::from_status(r as i32);
            return if e.kind() == ErrorKind::WouldBlock {
                Ok(false)
            } else {
                Err(e)
            };
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_on;
    use crate::handle::{MapReadWrite, ReadOnly};

    #[test]
    fn memory_create_and_map_return_handle_and_address() {
        sys::reset();
        let mem = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        let addr = mem.map(4096).unwrap();
        assert!(!addr.is_null());
    }

    #[test]
    fn lookup_resolves_to_a_typed_handle() {
        sys::reset();
        // The lookup PO completes with `result` = the resolved handle (777).
        sys::script_next(0, 777, false);
        let ns = unsafe {
            Handle::<Namespace, crate::handle::NsReadOnly>::from_raw(RawHandle(1), Rights::LOOKUP)
        };
        let resolved: Handle<Resource, ReadOnly> =
            block_on(unsafe { ns.lookup::<Resource, ReadOnly>("/dev/console", Rights::READ) })
                .unwrap();
        assert_eq!(resolved.raw(), RawHandle(777));
    }

    #[test]
    fn read_builds_an_op_and_completes() {
        sys::reset();
        sys::script_next(0, 12, true); // 12 bytes read, ready immediately
        let res =
            unsafe { Handle::<Resource, ReadOnly>::from_raw(RawHandle(1), Rights::READ) };
        let buf = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        let n = block_on(res.read(&buf, 0, 0, 64)).unwrap();
        assert_eq!(n, 12);
    }
}
