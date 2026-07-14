//! The syscall seam.
//!
//! The three raw kernel calls the async core makes — `io_submit`, `wait`,
//! `handle_close` — behind a thin module so the executor is host-testable with no
//! kernel. `cfg(not(test))` forwards to `libkern`'s `syscallN`; `cfg(test)` swaps in
//! a scriptable mock (mirrors `libheap`'s `ArenaSource` seam). All returns are the
//! kernel's `i64` (`isize`): negative = a `KError` discriminant.

use libkern::{IoOp, IoResult};

// --- target: real syscalls ------------------------------------------------

/// `sys_io_submit(resource, &op)` — never blocks; returns a `PendingOperation`
/// handle (>= 0) or a negative error.
#[cfg(not(test))]
pub fn io_submit(resource: u64, op: &IoOp) -> i64 {
    // SAFETY: `resource` is a caller-supplied handle; `op` is a live `IoOp`. The
    // kernel copies the descriptor and returns synchronously (never blocks).
    unsafe { libkern::syscall2(libkern::SYS_IO_SUBMIT, resource, op as *const IoOp as u64) }
}

/// `sys_wait(handles, count, results, deadline)` — returns the count of signaled
/// handles (writing one `IoResult` per signaled handle), or a negative error.
/// `deadline`: `0` = poll (non-blocking), `u64::MAX` = forever.
#[cfg(not(test))]
pub fn wait(handles: &[u64], results: &mut [IoResult], deadline: u64) -> i64 {
    // SAFETY: `handles`/`results` are live slices; `results.len() >= handles.len()`
    // by construction at every call site. The kernel writes at most `count` results.
    unsafe {
        libkern::syscall4(
            libkern::SYS_WAIT,
            handles.as_ptr() as u64,
            handles.len() as u64,
            results.as_mut_ptr() as u64,
            deadline,
        )
    }
}

/// `sys_handle_close(h)` — release the caller's reference to a handle.
#[cfg(not(test))]
pub fn handle_close(h: u64) {
    // SAFETY: `h` is a handle the caller owns; closing is always sound.
    unsafe {
        libkern::syscall1(libkern::SYS_HANDLE_CLOSE, h);
    }
}

/// `sys_handle_restrict(h, mask)` — intersect a handle's rights with `mask` in place.
#[cfg(not(test))]
pub fn handle_restrict(h: u64, mask: u64) -> i64 {
    // SAFETY: `h` is owned; restrict only ever reduces rights.
    unsafe { libkern::syscall2(libkern::SYS_HANDLE_RESTRICT, h, mask) }
}

/// `sys_memory_create(size)` — allocate an anonymous `MemoryObject`; returns a handle.
#[cfg(not(test))]
pub fn memory_create(size: u64) -> i64 {
    // SAFETY: a plain create; `flags = 0`.
    unsafe { libkern::syscall2(libkern::SYS_MEMORY_CREATE, size, 0) }
}

/// `sys_memory_map(h, hint, size, rights)` — map into the address space; returns the
/// base address (`hint = 0` lets the kernel choose).
#[cfg(not(test))]
pub fn memory_map(h: u64, hint: u64, size: u64, rights: u64) -> i64 {
    // SAFETY: `h` is a mappable handle carrying `rights`; the kernel validates.
    unsafe { libkern::syscall4(libkern::SYS_MEMORY_MAP, h, hint, size, rights) }
}

/// `sys_memory_unmap(addr, size)` — unmap the region covering `addr`.
#[cfg(not(test))]
pub fn memory_unmap(addr: u64, size: u64) -> i64 {
    // SAFETY: `addr` is a previously-mapped region base in this address space.
    unsafe { libkern::syscall2(libkern::SYS_MEMORY_UNMAP, addr, size) }
}

/// `sys_ns_lookup(ns, path, path_len, rights)` — resolve a path; returns a
/// `PendingOperation` handle whose completion carries the resolved handle.
#[cfg(not(test))]
pub fn ns_lookup(ns: u64, path: u64, path_len: u64, rights: u64) -> i64 {
    // SAFETY: `path`/`path_len` name a live UTF-8 buffer; `ns` is a namespace handle.
    unsafe { libkern::syscall4(libkern::SYS_NS_LOOKUP, ns, path, path_len, rights) }
}

/// `sys_ns_enumerate(ns, index, out)` — write the `index`-th binding into `out` (an
/// `NsEntry`); returns `0`, or `NotFound` past the end.
#[cfg(not(test))]
pub fn ns_enumerate(ns: u64, index: u64, out: u64) -> i64 {
    // SAFETY: `out` points at a live `NsEntry`; `ns` is a namespace handle.
    unsafe { libkern::syscall3(libkern::SYS_NS_ENUMERATE, ns, index, out) }
}

/// `sys_notif_recv(queue, out)` — dequeue one notification into `out` (a
/// `Notification`); returns `0`, or `WouldBlock` if empty.
#[cfg(not(test))]
pub fn notif_recv(queue: u64, out: u64) -> i64 {
    // SAFETY: `out` points at a live `Notification`; `queue` is a notif-channel handle.
    unsafe { libkern::syscall2(libkern::SYS_NOTIF_RECV, queue, out) }
}

// --- host tests: a scriptable mock kernel ---------------------------------

#[cfg(test)]
pub use mock::{
    handle_close, handle_restrict, io_submit, memory_create, memory_map, memory_unmap, ns_enumerate,
    ns_lookup, notif_recv, wait,
};

#[cfg(test)]
mod mock {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, VecDeque};

    /// One in-flight `PendingOperation` in the mock.
    struct Po {
        ready: bool,
        status: i32,
        result: u64,
    }

    #[derive(Default)]
    struct Mock {
        next_handle: u64,
        pos: BTreeMap<u64, Po>,
        /// Completions armed for upcoming `io_submit`s: `(status, result, ready_now)`.
        script: VecDeque<(i32, u64, bool)>,
        /// A one-shot forced `io_submit` failure (negative status).
        fail_submit: Option<i32>,
        submits: u32,
        waits: u32,
        closes: u32,
    }

    fn fresh() -> Mock {
        Mock {
            next_handle: 1000,
            ..Default::default()
        }
    }

    thread_local! {
        static MOCK: RefCell<Mock> = RefCell::new(fresh());
    }

    pub fn io_submit(_resource: u64, _op: &IoOp) -> i64 {
        MOCK.with(|m| {
            let mut m = m.borrow_mut();
            m.submits += 1;
            if let Some(status) = m.fail_submit.take() {
                return status as i64;
            }
            let (status, result, ready_now) = m.script.pop_front().unwrap_or((0, 0, true));
            let h = m.next_handle;
            m.next_handle += 1;
            m.pos.insert(
                h,
                Po {
                    ready: ready_now,
                    status,
                    result,
                },
            );
            h as i64
        })
    }

    pub fn wait(handles: &[u64], results: &mut [IoResult], deadline: u64) -> i64 {
        MOCK.with(|m| {
            let mut m = m.borrow_mut();
            m.waits += 1;
            let mut n = 0usize;
            for &h in handles {
                if let Some(po) = m.pos.get(&h) {
                    if po.ready {
                        results[n] = IoResult {
                            handle: h,
                            status: po.status,
                            reserved: 0,
                            result: po.result,
                        };
                        n += 1;
                    }
                }
            }
            // A blocking wait (deadline != 0) with nothing ready simulates the first
            // pending handle completing during the wait — so block_on always makes
            // progress. A poll (deadline == 0) never fabricates a completion.
            if n == 0 && deadline != 0 && !handles.is_empty() {
                let h = handles[0];
                if let Some(po) = m.pos.get_mut(&h) {
                    po.ready = true;
                    results[0] = IoResult {
                        handle: h,
                        status: po.status,
                        reserved: 0,
                        result: po.result,
                    };
                    n = 1;
                }
            }
            n as i64
        })
    }

    pub fn handle_close(h: u64) {
        MOCK.with(|m| {
            let mut m = m.borrow_mut();
            m.closes += 1;
            m.pos.remove(&h);
        });
    }

    pub fn handle_restrict(_h: u64, _mask: u64) -> i64 {
        0
    }

    pub fn memory_create(_size: u64) -> i64 {
        MOCK.with(|m| {
            let mut m = m.borrow_mut();
            let h = m.next_handle;
            m.next_handle += 1;
            h as i64
        })
    }

    pub fn memory_map(_h: u64, _hint: u64, _size: u64, _rights: u64) -> i64 {
        // A fake non-null, page-aligned address (never dereferenced by tests).
        0x1_0000_0000i64
    }

    pub fn memory_unmap(_addr: u64, _size: u64) -> i64 {
        0
    }

    pub fn ns_lookup(_ns: u64, _path: u64, _plen: u64, _rights: u64) -> i64 {
        // Resolve through the same PO machinery as `io_submit`: the scripted
        // completion's `result` is the resolved handle.
        let dummy = IoOp {
            opcode: 0,
            flags: 0,
            buffer: 0,
            buf_offset: 0,
            offset: 0,
            length: 0,
        };
        io_submit(0, &dummy)
    }

    pub fn ns_enumerate(_ns: u64, _index: u64, _out: u64) -> i64 {
        -10 // KError::NotFound (past the end)
    }

    pub fn notif_recv(_queue: u64, _out: u64) -> i64 {
        -11 // KError::WouldBlock (empty)
    }

    /// Test controls.
    pub fn reset() {
        MOCK.with(|m| *m.borrow_mut() = fresh());
    }

    /// Arm the next `io_submit` with a completion. `ready_now` = the PO is signaled
    /// immediately (a poll finds it); otherwise it completes on the next blocking wait.
    pub fn script_next(status: i32, result: u64, ready_now: bool) {
        MOCK.with(|m| m.borrow_mut().script.push_back((status, result, ready_now)));
    }

    /// Force the next `io_submit` to fail with `status`.
    pub fn fail_next_submit(status: i32) {
        MOCK.with(|m| m.borrow_mut().fail_submit = Some(status));
    }

    /// `(submits, waits, closes)` issued since the last `reset`.
    pub fn counts() -> (u32, u32, u32) {
        MOCK.with(|m| {
            let m = m.borrow();
            (m.submits, m.waits, m.closes)
        })
    }
}

#[cfg(test)]
pub use mock::{counts, fail_next_submit, reset, script_next};
