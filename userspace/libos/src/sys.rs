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
/// handles (one `IoResult` each), or an error. `deadline`: `0` = poll, `u64::MAX` = forever; on
/// no handles, a sleep until a deadline between, which the kernel never refuses.
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

/// `sys_clock_read(CLOCK_MONOTONIC)` — nanoseconds since boot, if the clock could be read.
#[cfg(not(test))]
pub fn monotonic_ns() -> Option<u64> {
    let mut ns = 0u64;
    // SAFETY: `ns` is a live `u64`, which the kernel writes.
    let r = unsafe { libkern::syscall2(libkern::SYS_CLOCK_READ, libkern::CLOCK_MONOTONIC, &raw mut ns as u64) };
    (r >= 0).then_some(ns)
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
/// # Safety
/// Nothing still uses that mapping.
#[cfg(not(test))]
pub unsafe fn memory_unmap(addr: u64, size: u64) -> i64 {
    // SAFETY: forwarded from this fn's contract.
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
/// # Safety
/// As [`crate::thread_create`].
#[cfg(not(test))]
pub unsafe fn thread_create(args: &libkern::ThreadArgs) -> i64 {
    // SAFETY: forwarded from this fn's contract; the kernel copies `args` in.
    unsafe { libkern::syscall1(libkern::SYS_THREAD_CREATE, args as *const _ as u64) }
}

// --- host tests: a scriptable mock kernel ---------------------------------

#[cfg(test)]
use mock::wait_exact;
#[cfg(test)]
pub use mock::{
    handle_close, handle_restrict, io_submit, memory_create, memory_map, memory_unmap, monotonic_ns, notif_recv,
    ns_bind, ns_enumerate, ns_lookup, process_spawn, thread_create,
};

#[cfg(test)]
pub use mock::{
    closed_in_flight, counts, fail_next_bind, fail_next_submit, fail_next_wait, installed, last_wait_count, reset,
    script_never, script_next, sleeps,
};

#[cfg(test)]
mod mock {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet, VecDeque};

    /// One in-flight `PendingOperation` in the mock: signalled, or not yet, or never.
    struct Po {
        ready: bool,
        never: bool,
        lookup: bool,
        status: i32,
        result: u64,
    }

    #[derive(Default)]
    struct Mock {
        next_handle: u64,
        pos: BTreeMap<u64, Po>,
        /// Completions armed for upcoming `io_submit`s: `(status, result, ready_now, never)`.
        script: VecDeque<(i32, u64, bool, bool)>,
        /// Handles lookups resolved to, installed by the first wait that read them, as the kernel's.
        installed: BTreeSet<u64>,
        fail_wait: Option<i64>,
        /// Waits that slept until a deadline, neither polls nor waits for ever.
        sleeps: u32,
        clock: u64,
        /// A one-shot forced `io_submit` failure (negative status).
        fail_submit: Option<i32>,
        /// A one-shot forced `ns_bind` failure (negative status).
        bind_err: Option<i32>,
        submits: u32,
        waits: u32,
        closes: u32,
        /// How many handles the last `wait` was handed, as the kernel would be.
        last_wait_count: usize,
        closed_in_flight: u32,
    }

    fn fresh() -> Mock {
        Mock { next_handle: 1000, ..Default::default() }
    }

    thread_local! {
        static MOCK: RefCell<Mock> = RefCell::new(fresh());
    }

    pub fn io_submit(_resource: u64, _op: &IoOp) -> i64 {
        submit(false)
    }

    fn submit(lookup: bool) -> i64 {
        MOCK.with(|m| {
            let mut m = m.borrow_mut();
            m.submits += 1;
            if let Some(status) = m.fail_submit.take() {
                return status as i64;
            }
            let (status, result, ready, never) = m.script.pop_front().unwrap_or((0, 0, true, false));
            let h = m.next_handle;
            m.next_handle += 1;
            m.pos.insert(h, Po { ready, never, lookup, status, result });
            h as i64
        })
    }

    /// As the kernel answers (`InvalidArgument`, `InvalidHandle`, an empty poll's `WouldBlock`, a
    /// sleep on no handles); a short `results` panics. A deadline lets time pass; a wait for ever that nothing ends panics.
    pub fn wait_exact(handles: &[u64], results: &mut [IoResult], deadline: u64) -> i64 {
        MOCK.with(|m| {
            let m = &mut *m.borrow_mut();
            m.waits += 1;
            assert!(m.waits < 10_000, "a loop of waits that never ends");
            m.last_wait_count = handles.len();
            if handles.is_empty() && (deadline == 0 || deadline == u64::MAX) {
                return -30;
            }
            if handles.iter().any(|h| !m.pos.contains_key(h)) {
                return -1;
            }
            if deadline != 0 {
                if let Some(e) = m.fail_wait.take() {
                    return e;
                }
                m.pos.values_mut().filter(|po| !po.never).for_each(|po| po.ready = true);
            }
            let mut n = 0usize;
            for &h in handles {
                if let Some(po) = m.pos.get(&h)
                    && po.ready
                {
                    results[n] = IoResult { handle: h, status: po.status, reserved: 0, result: po.result };
                    n += 1;
                    if po.lookup && po.status == 0 {
                        m.installed.insert(po.result);
                    }
                }
            }
            match (n, deadline) {
                (0, 0) => -11,
                (0, u64::MAX) => panic!("a wait for ever on handles none of which will signal"),
                (0, _) => {
                    m.sleeps += 1;
                    -12
                }
                _ => n as i64,
            }
        })
    }

    pub fn monotonic_ns() -> Option<u64> {
        MOCK.with(|m| {
            let mut m = m.borrow_mut();
            m.clock += 1_000;
            Some(m.clock)
        })
    }

    pub fn handle_close(h: u64) {
        MOCK.with(|m| {
            let mut m = m.borrow_mut();
            m.closes += 1;
            m.installed.remove(&h);
            if m.pos.remove(&h).is_some_and(|po| !po.ready) {
                m.closed_in_flight += 1;
            }
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

    pub unsafe fn memory_unmap(_addr: u64, _size: u64) -> i64 {
        0
    }

    pub fn ns_lookup(_ns: u64, _path: &[u8], _rights: u64) -> i64 {
        // The scripted completion's `result` is the resolved handle.
        submit(true)
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

    pub unsafe fn thread_create(_args: &libkern::ThreadArgs) -> i64 {
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
        MOCK.with(|m| m.borrow_mut().script.push_back((status, result, ready_now, false)));
    }

    /// Arm the next `io_submit` with a PO that never signals.
    pub fn script_never() {
        MOCK.with(|m| m.borrow_mut().script.push_back((0, 0, false, true)));
    }

    /// Fail the next wait with a deadline with `status`.
    pub fn fail_next_wait(status: i64) {
        MOCK.with(|m| m.borrow_mut().fail_wait = Some(status));
    }

    /// Waits that slept until their deadline.
    pub fn sleeps() -> u32 {
        MOCK.with(|m| m.borrow().sleeps)
    }

    /// Force the next `io_submit` to fail with `status`.
    pub fn fail_next_submit(status: i32) {
        MOCK.with(|m| m.borrow_mut().fail_submit = Some(status));
    }

    /// Force the next `ns_bind` to fail with `status` (e.g. `-2` = NoAccess).
    pub fn fail_next_bind(status: i32) {
        MOCK.with(|m| m.borrow_mut().bind_err = Some(status));
    }

    /// How many POs were closed before they signalled.
    pub fn closed_in_flight() -> u32 {
        MOCK.with(|m| m.borrow().closed_in_flight)
    }

    /// Whether a lookup's handle `h` was installed by a wait and not yet closed.
    pub fn installed(h: u64) -> bool {
        MOCK.with(|m| m.borrow().installed.contains(&h))
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
