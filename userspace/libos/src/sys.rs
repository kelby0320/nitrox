//! The syscall seam.
//!
//! The raw kernel calls libos makes, behind a thin module so everything above it is
//! host-testable with no kernel. `cfg(not(test))` forwards to `libkern`'s `syscallN`;
//! `cfg(test)` swaps in [`mock`], a small model of the kernel. All returns are the kernel's `i64`:
//! negative = a `KError` discriminant. Each takes typed arguments, so what the kernel is handed is
//! what the types say.

#[cfg(not(test))]
use libkern::{
    HandleInfo, IPC_HANDLE_MAX, IoOp, IpcMsg, Notification, NsEntry, SpawnArgs, ThreadArgs, syscall1, syscall2,
    syscall3, syscall4, syscall5, syscall6,
};
use libkern::{IoResult, SYS_FILE_CREATE, SYS_FILE_GROW, SYS_FILE_TRUNCATE};

/// The three file verbs that resolve a path to a file of a size: what each does to the size.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resize {
    /// Make the file, then grow it to the size.
    Create,
    /// Grow an existing file to at least the size.
    Grow,
    /// Shrink an existing file to the size.
    Truncate,
}

impl Resize {
    #[cfg_attr(test, allow(dead_code, reason = "the mock answers by the variant"))]
    fn number(self) -> u64 {
        match self {
            Resize::Create => SYS_FILE_CREATE,
            Resize::Grow => SYS_FILE_GROW,
            Resize::Truncate => SYS_FILE_TRUNCATE,
        }
    }
}

/// `sys_wait` on as many of `handles` as `results` has room for — returns the count of signaled
/// handles (one `IoResult` each), or an error. `deadline`: `0` = poll, `u64::MAX` = forever; on
/// no handles, a sleep until a deadline between, which the kernel never refuses.
pub fn wait(handles: &[u64], results: &mut [IoResult], deadline: u64) -> i64 {
    let count = handles.len().min(results.len());
    wait_exact(&handles[..count], &mut results[..count], deadline)
}

#[cfg(test)]
pub use mock::*;

#[cfg(test)]
mod mock;

// --- target: real syscalls ------------------------------------------------

#[cfg(not(test))]
fn wait_exact(handles: &[u64], results: &mut [IoResult], deadline: u64) -> i64 {
    let count = handles.len().min(results.len());
    // SAFETY: the kernel reads `count` handles and writes at most `count` results, and both
    // slices hold at least that many.
    unsafe { syscall4(libkern::SYS_WAIT, handles.as_ptr() as u64, count as u64, results.as_mut_ptr() as u64, deadline) }
}

/// `sys_io_submit` — never blocks; returns a `PendingOperation` handle or an error.
#[cfg(not(test))]
pub fn io_submit(resource: u64, op: &IoOp) -> i64 {
    // SAFETY: `op` is a live `IoOp`, which the kernel copies in.
    unsafe { syscall2(libkern::SYS_IO_SUBMIT, resource, op as *const IoOp as u64) }
}

/// `sys_clock_read` into `out`, which is written only on success.
#[cfg(not(test))]
pub fn clock_read(clock: u64, out: &mut u64) -> i64 {
    // SAFETY: `out` is a live `u64`, which the kernel writes.
    unsafe { syscall2(libkern::SYS_CLOCK_READ, clock, out as *mut u64 as u64) }
}

/// `sys_handle_close(h)` — release the caller's reference to a handle.
#[cfg(not(test))]
pub fn handle_close(h: u64) {
    // SAFETY: a register-only call; closing names no memory.
    unsafe { syscall1(libkern::SYS_HANDLE_CLOSE, h) };
}

/// `sys_handle_restrict(h, mask)` — intersect a handle's rights with `mask` in place.
#[cfg(not(test))]
pub fn handle_restrict(h: u64, mask: u64) -> i64 {
    // SAFETY: a register-only call.
    unsafe { syscall2(libkern::SYS_HANDLE_RESTRICT, h, mask) }
}

/// `sys_handle_duplicate(h, rights)` — a second handle to `h`'s object, with at most `rights`.
#[cfg(not(test))]
pub fn handle_duplicate(h: u64, rights: u64) -> i64 {
    // SAFETY: a register-only call.
    unsafe { syscall2(libkern::SYS_HANDLE_DUPLICATE, h, rights) }
}

/// `sys_handle_stat(h)` into `out`.
#[cfg(not(test))]
pub fn handle_stat(h: u64, out: &mut HandleInfo) -> i64 {
    // SAFETY: `out` is a live `HandleInfo`, the 24 bytes the kernel writes.
    unsafe { syscall2(libkern::SYS_HANDLE_STAT, h, out as *mut HandleInfo as u64) }
}

/// `sys_memory_create(size)` — an anonymous `MemoryObject`; returns a handle.
#[cfg(not(test))]
pub fn memory_create(size: u64) -> i64 {
    // SAFETY: a register-only call; `flags = 0`.
    unsafe { syscall2(libkern::SYS_MEMORY_CREATE, size, 0) }
}

/// `sys_memory_map` — returns the base address (`hint = 0` lets the kernel choose).
#[cfg(not(test))]
pub fn memory_map(h: u64, hint: u64, size: u64, rights: u64) -> i64 {
    // SAFETY: a register-only call; the kernel places the mapping where nothing is mapped.
    unsafe { syscall4(libkern::SYS_MEMORY_MAP, h, hint, size, rights) }
}

/// `sys_memory_unmap(addr, size)` — unmap the region covering `addr`.
/// # Safety
/// Nothing still uses that mapping.
#[cfg(not(test))]
pub unsafe fn memory_unmap(addr: u64, size: u64) -> i64 {
    // SAFETY: forwarded from this fn's contract.
    unsafe { syscall2(libkern::SYS_MEMORY_UNMAP, addr, size) }
}

/// `sys_notif_recv` — writes a `Notification` to `out`; `WouldBlock` if empty.
#[cfg(not(test))]
pub fn notif_recv(queue: u64, out: &mut Notification) -> i64 {
    // SAFETY: `out` is a live `Notification`, which the kernel writes whole or not at all.
    unsafe { syscall2(libkern::SYS_NOTIF_RECV, queue, out as *mut Notification as u64) }
}

/// `sys_channel_create(depth)` — the two endpoints into `ends`.
#[cfg(not(test))]
pub fn channel_create(depth: u64, ends: &mut [u64; 2]) -> i64 {
    let (a, b) = ends.split_at_mut(1);
    // SAFETY: two live `u64`s, which the kernel writes.
    unsafe { syscall4(libkern::SYS_CHANNEL_CREATE, a.as_mut_ptr() as u64, b.as_mut_ptr() as u64, depth, 0) }
}

/// `sys_channel_send` of `msg` and `handles`, in `mode`, by `deadline` for a bounded one.
#[cfg(not(test))]
pub fn channel_send(ch: u64, msg: &IpcMsg, handles: &[u64], mode: u64, deadline: u64) -> i64 {
    // SAFETY: the kernel reads one message from `msg` and `handles.len()` handles.
    unsafe {
        syscall6(
            libkern::SYS_CHANNEL_SEND,
            ch,
            msg as *const IpcMsg as u64,
            handles.as_ptr() as u64,
            handles.len() as u64,
            mode,
            deadline,
        )
    }
}

/// `sys_channel_recv` into `msg`, the handles the message brought into `handles`, the first
/// `*count` of them. Through `libkern`'s receive, the one place allowed to make it.
#[cfg(not(test))]
pub fn channel_recv(ch: u64, msg: &mut IpcMsg, handles: &mut [u64; IPC_HANDLE_MAX], count: &mut usize) -> i64 {
    // SAFETY: `msg` is a unique reference to a whole message, and an `IpcMsg` is its 4096 bytes.
    match unsafe { libkern::channel_recv_raw(ch, msg as *mut IpcMsg as *mut [u8; libkern::IPC_MSG_SIZE]) } {
        Ok(mut got) => {
            *count = got.count();
            *handles = got.take_all();
            0
        }
        Err(e) => e,
    }
}

/// `sys_ns_create` — a new empty namespace.
#[cfg(not(test))]
pub fn ns_create() -> i64 {
    // SAFETY: a call with no arguments.
    unsafe { libkern::syscall0(libkern::SYS_NS_CREATE) }
}

/// `sys_ns_derive(ns)` — a new namespace holding a copy of `ns`'s bindings.
#[cfg(not(test))]
pub fn ns_derive(ns: u64) -> i64 {
    // SAFETY: a register-only call.
    unsafe { syscall1(libkern::SYS_NS_DERIVE, ns) }
}

/// `sys_ns_lookup` — returns a `PendingOperation` whose completion carries the handle.
#[cfg(not(test))]
pub fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> i64 {
    // SAFETY: `path` is a live buffer of its length, which the kernel copies in.
    unsafe { syscall4(libkern::SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) }
}

/// `sys_ns_bind` of `resource` at `path`, scoped to the server's `base` when that is not empty.
#[cfg(not(test))]
pub fn ns_bind(ns: u64, path: &[u8], resource: u64, base: &[u8]) -> i64 {
    // SAFETY: `path` and `base` are live buffers of their lengths, which the kernel copies in.
    unsafe {
        syscall6(
            libkern::SYS_NS_BIND,
            ns,
            path.as_ptr() as u64,
            path.len() as u64,
            resource,
            base.as_ptr() as u64,
            base.len() as u64,
        )
    }
}

/// `sys_ns_unbind` — remove the binding at `path`.
#[cfg(not(test))]
pub fn ns_unbind(ns: u64, path: &[u8]) -> i64 {
    // SAFETY: `path` is a live buffer of its length, which the kernel copies in.
    unsafe { syscall3(libkern::SYS_NS_UNBIND, ns, path.as_ptr() as u64, path.len() as u64) }
}

/// `sys_ns_enumerate` — writes an `NsEntry` to `out`; `NotFound` past the end.
#[cfg(not(test))]
pub fn ns_enumerate(ns: u64, index: u64, out: &mut NsEntry) -> i64 {
    // SAFETY: `out` is a live `NsEntry`, which the kernel writes whole or not at all.
    unsafe { syscall3(libkern::SYS_NS_ENUMERATE, ns, index, out as *mut NsEntry as u64) }
}

/// `sys_ns_sync` — write back the server `path` resolves to; how many files it wrote.
#[cfg(not(test))]
pub fn ns_sync(ns: u64, path: &[u8]) -> i64 {
    // SAFETY: `path` is a live buffer of its length, which the kernel copies in.
    unsafe { syscall3(libkern::SYS_NS_SYNC, ns, path.as_ptr() as u64, path.len() as u64) }
}

/// `sys_ns_held` — how many of that server's files something holds.
#[cfg(not(test))]
pub fn ns_held(ns: u64, path: &[u8]) -> i64 {
    // SAFETY: `path` is a live buffer of its length, which the kernel copies in.
    unsafe { syscall3(libkern::SYS_NS_HELD, ns, path.as_ptr() as u64, path.len() as u64) }
}

/// `sys_file_create`, `sys_file_grow` or `sys_file_truncate` — a lookup's `PendingOperation`.
#[cfg(not(test))]
pub fn file_resize(verb: Resize, ns: u64, path: &[u8], rights: u64, size: u64) -> i64 {
    // SAFETY: `path` is a live buffer of its length, which the kernel copies in.
    unsafe { syscall5(verb.number(), ns, path.as_ptr() as u64, path.len() as u64, rights, size) }
}

/// `sys_file_rename` of `from` to `to` — a `PendingOperation` carrying status only.
#[cfg(not(test))]
pub fn file_rename(ns: u64, from: &[u8], to: &[u8], flags: u64) -> i64 {
    // SAFETY: `from` and `to` are live buffers of their lengths, which the kernel copies in.
    unsafe {
        syscall6(
            libkern::SYS_FILE_RENAME,
            ns,
            from.as_ptr() as u64,
            from.len() as u64,
            to.as_ptr() as u64,
            to.len() as u64,
            flags,
        )
    }
}

/// `sys_file_sync(file)` — returns once its pages are on the device.
#[cfg(not(test))]
pub fn file_sync(file: u64) -> i64 {
    // SAFETY: a register-only call.
    unsafe { syscall1(libkern::SYS_FILE_SYNC, file) }
}

/// `sys_process_spawn(&args)` — spawn a child process; returns a `Process` handle.
#[cfg(not(test))]
pub fn process_spawn(args: &SpawnArgs) -> i64 {
    // SAFETY: `args` is a live `SpawnArgs`; the kernel copies it in.
    unsafe { syscall1(libkern::SYS_PROCESS_SPAWN, args as *const SpawnArgs as u64) }
}

/// `sys_process_exit(status)`.
#[cfg(not(test))]
pub fn process_exit(status: i32) -> ! {
    libkern::exit(status as i64)
}

/// `sys_thread_create(&args)` — start a thread in this process; returns a `Thread` handle.
/// # Safety
/// As [`crate::thread_create`].
#[cfg(not(test))]
pub unsafe fn thread_create(args: &ThreadArgs) -> i64 {
    // SAFETY: forwarded from this fn's contract; the kernel copies `args` in.
    unsafe { syscall1(libkern::SYS_THREAD_CREATE, args as *const ThreadArgs as u64) }
}

/// `sys_entropy_create` — a handle onto the kernel's generator.
#[cfg(not(test))]
pub fn entropy_create() -> i64 {
    // SAFETY: a call with no arguments.
    unsafe { libkern::syscall0(libkern::SYS_ENTROPY_CREATE) }
}

/// `sys_entropy_read` into `buf`: `0` filled, a `PendingOperation` while unseeded, or an error.
#[cfg(not(test))]
pub fn entropy_read(h: u64, buf: &mut [u8]) -> i64 {
    // SAFETY: `buf` is a live buffer of its length, which the kernel writes.
    unsafe { syscall3(libkern::SYS_ENTROPY_READ, h, buf.as_mut_ptr() as u64, buf.len() as u64) }
}

/// `sys_debug_kprint` of `bytes`.
#[cfg(not(test))]
pub fn kprint(bytes: &[u8]) {
    // SAFETY: `bytes` is a live buffer of its length, which the kernel copies.
    unsafe { syscall2(libkern::SYS_DEBUG_KPRINT, bytes.as_ptr() as u64, bytes.len() as u64) };
}
