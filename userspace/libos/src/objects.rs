//! Per-object operations on typed [`Handle`]s.
//!
//! Async methods (`lookup`, `read`, `write`) return futures resolving to their result
//! — drive them with [`block_on`](crate::block_on). Synchronous kernel calls
//! (`create`/`map`, `enumerate`, `recv`) return directly. Op availability is gated by
//! the sealed capability traits in [`crate::handle`], so misuse is a compile error.

use libkern::{
    IO_OPCODE_READ, IO_OPCODE_WRITE, IoOp, NsEntry, Notification, RawHandle, Rights, SpawnArgs,
    ThreadArgs,
};

use crate::error::{Error, ErrorKind, Result};
use crate::exec::Op;
use crate::handle::{
    CanBind, CanLookup, CanMapRead, CanMapWrite, CanRead, CanWrite, Handle, MapReadWrite, MemMode,
    Memory, Namespace, Notify, Only, Process, Resource, Thread,
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

impl<M: CanBind> Handle<Namespace, M> {
    /// Bind `resource` at `path` in this namespace. Requires the `BIND` handle right
    /// (this method's `CanBind` bound) **and**, at runtime, the `BIND_NAMESPACE` syscap
    /// — a caller without it gets `Err` with [`ErrorKind::PermissionDenied`]. See
    /// `docs/architecture/syscaps.md`.
    pub fn bind(&self, path: &str, resource: RawHandle) -> Result<()> {
        let r = sys::ns_bind(
            self.raw().0,
            path.as_ptr() as u64,
            path.len() as u64,
            resource.0,
        );
        if r < 0 {
            return Err(Error::from_status(r as i32));
        }
        Ok(())
    }
}

// --- Process / thread spawning --------------------------------------------

/// Spawn a child process from `args` (a libkern [`SpawnArgs`]), returning an **owning**
/// handle — dropping it reaps the child (closes the process handle). The kernel grants
/// the parent `SIGNAL | TERMINATE` on the child.
pub fn spawn(args: &SpawnArgs) -> Result<Handle<Process, Only>> {
    let h = sys::process_spawn(args);
    if h < 0 {
        return Err(Error::from_status(h as i32));
    }
    // SAFETY: `sys_process_spawn` returned a fresh child-process handle we own.
    Ok(unsafe { Handle::from_raw(RawHandle(h as u64), Rights::SIGNAL | Rights::TERMINATE) })
}

/// Start a thread in this process from `args` (a libkern [`ThreadArgs`]), returning an
/// owning handle. The `RealTime` class in `args` requires the `REAL_TIME` syscap
/// (else `Err`). The kernel grants `SIGNAL | TERMINATE | INSPECT | DUPLICATE`.
pub fn thread_create(args: &ThreadArgs) -> Result<Handle<Thread, Only>> {
    let h = sys::thread_create(args);
    if h < 0 {
        return Err(Error::from_status(h as i32));
    }
    // SAFETY: `sys_thread_create` returned a fresh thread handle we own.
    Ok(unsafe {
        Handle::from_raw(
            RawHandle(h as u64),
            Rights::SIGNAL | Rights::TERMINATE | Rights::INSPECT | Rights::DUPLICATE,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_on;
    use crate::handle::{MapReadWrite, NsMutable, ReadOnly};

    fn zeroed_spawn() -> SpawnArgs {
        SpawnArgs {
            image: 0,
            handle_count: 0,
            move_mask: 0,
            arg0: 0,
            handles: [0; 4],
            rights: [0; 4],
            namespace: 0,
            syscaps: 0,
        }
    }

    fn zeroed_thread() -> ThreadArgs {
        ThreadArgs {
            entry: 0,
            user_sp: 0,
            arg0: 0,
            class: 0,
            rt_priority: 0,
            nice: 0,
            cpu_affinity: 0,
            _reserved: [0; 36],
        }
    }

    #[test]
    fn spawn_returns_an_owning_process_handle() {
        sys::reset();
        let p = spawn(&zeroed_spawn()).unwrap();
        assert!(p.extra_rights().contains(Rights::TERMINATE));
        drop(p);
        let (_s, _w, closes) = sys::counts();
        assert_eq!(closes, 1, "dropping the Process handle reaps (closes) it");
    }

    #[test]
    fn thread_create_returns_an_owning_handle() {
        sys::reset();
        let t = thread_create(&zeroed_thread()).unwrap();
        drop(t);
        let (_s, _w, closes) = sys::counts();
        assert_eq!(closes, 1);
    }

    #[test]
    fn ns_bind_succeeds_and_denial_maps_to_permission_denied() {
        sys::reset();
        // SAFETY: test handle; borrow does not own it.
        let ns = unsafe { Handle::<Namespace, NsMutable>::borrow(RawHandle(1), Rights::BIND) };
        ns.bind("/store", RawHandle(9)).unwrap(); // mock: success

        sys::fail_next_bind(-2); // KError::NoAccess
        let e = ns.bind("/store", RawHandle(9)).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::PermissionDenied);
    }

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
