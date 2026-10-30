//! The syscall seam.
//!
//! The raw kernel calls libos makes, behind a thin module so the executor is
//! host-testable with no kernel. `cfg(not(test))` forwards to `libkern`'s `syscallN`;
//! `cfg(test)` swaps in a scriptable mock. All returns are the kernel's `i64`:
//! negative = a `KError` discriminant.

use libkern::{IoOp, IoResult, Notification, NsEntry};

// --- target: real syscalls ------------------------------------------------

/// `sys_io_submit` — never blocks; returns a `PendingOperation` handle or an error.
#[cfg(not(test))]
pub fn io_submit(resource: u64, op: &IoOp) -> i64 {
    // SAFETY: `resource` is a caller-supplied handle; `op` is a live `IoOp`. The
    // kernel copies the descriptor and returns synchronously (never blocks).
    unsafe { libkern::syscall2(libkern::SYS_IO_SUBMIT, resource, op as *const IoOp as u64) }
}

/// `sys_wait` on as many of `handles` as `results` has room for — returns the count of signaled
/// handles (one `IoResult` each), or an error. `deadline`: `0` = poll, `u64::MAX` = forever.
pub fn wait(handles: &[u64], results: &mut [IoResult], deadline: u64) -> i64 {
    let count = handles.len().min(results.len());
    wait_exact(&handles[..count], &mut results[..count], deadline)
}

#[cfg(not(test))]
fn wait_exact(handles: &[u64], results: &mut [IoResult], deadline: u64) -> i64 {
    let count = handles.len().min(results.len());
    // SAFETY: the kernel reads `count` handles and writes at most `count` results, and both
    // slices hold at least that many.
    unsafe {
        libkern::syscall4(
            libkern::SYS_WAIT,
            handles.as_ptr() as u64,
            count as u64,
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

/// `sys_memory_map` — returns the base address (`hint = 0` lets the kernel choose).
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

/// `sys_ns_lookup` — returns a `PendingOperation` whose completion carries the handle.
#[cfg(not(test))]
pub fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> i64 {
    // SAFETY: `path` is a live buffer of its length, which the kernel copies in.
    unsafe { libkern::syscall4(libkern::SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) }
}

/// `sys_ns_enumerate` — writes an `NsEntry` to `out`; `NotFound` past the end.
#[cfg(not(test))]
pub fn ns_enumerate(ns: u64, index: u64, out: &mut NsEntry) -> i64 {
    // SAFETY: `out` is a live `NsEntry`, which the kernel writes whole or not at all.
    unsafe { libkern::syscall3(libkern::SYS_NS_ENUMERATE, ns, index, out as *mut NsEntry as u64) }
}

/// `sys_notif_recv` — writes a `Notification` to `out`; `WouldBlock` if empty.
#[cfg(not(test))]
pub fn notif_recv(queue: u64, out: &mut Notification) -> i64 {
    // SAFETY: `out` is a live `Notification`, which the kernel writes whole or not at all.
    unsafe { libkern::syscall2(libkern::SYS_NOTIF_RECV, queue, out as *mut Notification as u64) }
}

/// `sys_ns_bind` — gated by the `BIND` handle right *and* the `BIND_NAMESPACE` syscap.
#[cfg(not(test))]
pub fn ns_bind(ns: u64, path: &[u8], resource: u64) -> i64 {
    // SAFETY: `path` is a live buffer of its length, which the kernel copies in.
    unsafe { libkern::syscall4(libkern::SYS_NS_BIND, ns, path.as_ptr() as u64, path.len() as u64, resource) }
}

/// `sys_process_spawn(&args)` — spawn a child process; returns a `Process` handle.
#[cfg(not(test))]
pub fn process_spawn(args: &libkern::SpawnArgs) -> i64 {
    // SAFETY: `args` is a live `SpawnArgs`; the kernel copies it in.
    unsafe { libkern::syscall1(libkern::SYS_PROCESS_SPAWN, args as *const _ as u64) }
}

/// `sys_thread_create(&args)` — start a thread in this process; returns a `Thread` handle.
#[cfg(not(test))]
pub fn thread_create(args: &libkern::ThreadArgs) -> i64 {
    // SAFETY: `args` is a live `ThreadArgs`; the kernel copies it in.
    unsafe { libkern::syscall1(libkern::SYS_THREAD_CREATE, args as *const _ as u64) }
}

// --- host tests: a scriptable mock kernel ---------------------------------

#[cfg(test)]
use mock::wait_exact;
#[cfg(test)]
pub use mock::{
    handle_close, handle_restrict, io_submit, memory_create, memory_map, memory_unmap, notif_recv, ns_bind,
    ns_enumerate, ns_lookup, process_spawn, thread_create,
};

#[cfg(test)]
pub use mock::{counts, fail_next_bind, fail_next_submit, last_wait_count, reset, script_next};

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
        /// A one-shot forced `ns_bind` failure (negative status).
        bind_err: Option<i32>,
        submits: u32,
        waits: u32,
        closes: u32,
        /// How many handles the last `wait` was handed, as the kernel would be.
        last_wait_count: usize,
    }

    fn fresh() -> Mock {
        Mock { next_handle: 1000, ..Default::default() }
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
            m.pos.insert(h, Po { ready: ready_now, status, result });
            h as i64
        })
    }

    /// As the kernel answers: `InvalidArgument` for no handles, `InvalidHandle` for one it does
    /// not hold, and `WouldBlock` for a poll that finds nothing ready. A `results` shorter than
    /// `handles` panics, where the kernel would write past it.
    pub fn wait_exact(handles: &[u64], results: &mut [IoResult], deadline: u64) -> i64 {
        MOCK.with(|m| {
            let mut m = m.borrow_mut();
            m.waits += 1;
            m.last_wait_count = handles.len();
            if handles.is_empty() {
                return -30;
            }
            if handles.iter().any(|h| !m.pos.contains_key(h)) {
                return -1;
            }
            let mut n = 0usize;
            for &h in handles {
                if let Some(po) = m.pos.get(&h)
                    && po.ready
                {
                    results[n] = IoResult { handle: h, status: po.status, reserved: 0, result: po.result };
                    n += 1;
                }
            }
            // A blocking wait with nothing ready completes the first handle, so block_on
            // makes progress; a poll (deadline == 0) never fabricates a completion.
            if n == 0 && deadline != 0 {
                let h = handles[0];
                if let Some(po) = m.pos.get_mut(&h) {
                    po.ready = true;
                    results[0] = IoResult { handle: h, status: po.status, reserved: 0, result: po.result };
                    n = 1;
                }
            }
            if n == 0 { -11 } else { n as i64 }
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
        0x1_0000_0000i64
    }

    pub fn memory_unmap(_addr: u64, _size: u64) -> i64 {
        0
    }

    pub fn ns_lookup(_ns: u64, _path: &[u8], _rights: u64) -> i64 {
        // The scripted completion's `result` is the resolved handle.
        let dummy = IoOp { opcode: 0, flags: 0, buffer: 0, buf_offset: 0, offset: 0, length: 0 };
        io_submit(0, &dummy)
    }

    pub fn ns_enumerate(_ns: u64, _index: u64, _out: &mut NsEntry) -> i64 {
        -10 // KError::NotFound (past the end)
    }

    pub fn notif_recv(_queue: u64, _out: &mut Notification) -> i64 {
        -11 // KError::WouldBlock (empty)
    }

    pub fn ns_bind(_ns: u64, _path: &[u8], _resource: u64) -> i64 {
        MOCK.with(|m| {
            let mut m = m.borrow_mut();
            match m.bind_err.take() {
                Some(status) => status as i64,
                None => 0, // success
            }
        })
    }

    pub fn process_spawn(_args: &libkern::SpawnArgs) -> i64 {
        mint_handle()
    }

    pub fn thread_create(_args: &libkern::ThreadArgs) -> i64 {
        mint_handle()
    }

    /// A fresh fake handle for the spawn and thread mocks.
    fn mint_handle() -> i64 {
        MOCK.with(|m| {
            let mut m = m.borrow_mut();
            let h = m.next_handle;
            m.next_handle += 1;
            h as i64
        })
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

    /// Force the next `ns_bind` to fail with `status` (e.g. `-2` = NoAccess).
    pub fn fail_next_bind(status: i32) {
        MOCK.with(|m| m.borrow_mut().bind_err = Some(status));
    }

    /// How many handles the last `wait` was handed.
    pub fn last_wait_count() -> usize {
        MOCK.with(|m| m.borrow().last_wait_count)
    }

    /// `(submits, waits, closes)` issued since the last `reset`.
    pub fn counts() -> (u32, u32, u32) {
        MOCK.with(|m| {
            let m = m.borrow();
            (m.submits, m.waits, m.closes)
        })
    }
}
