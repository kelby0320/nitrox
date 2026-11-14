//! Syscall numbers and the raw `syscall`-instruction wrappers.
//!
//! The `SYS_*` numbers are pinned to `kernel/src/syscall/table.rs` (the canonical
//! source); they are sequential from `0` and frozen at v1.0 (`docs/spec/syscall-abi.md`).
//! Each `syscallN` is `unsafe`: the caller is responsible for valid arguments and pointers.
//! Higher layers wrap these in `Result`-returning helpers (see [`crate::error::from_raw`]).
//!
//! Registers: `rax` = number, args in `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9`; `rcx`/`r11` are
//! clobbered by `syscall`; the `isize` result returns in `rax`.

#[cfg(not(miri))]
use core::arch::asm;

// --- Stable ABI syscall numbers (mirror kernel/src/syscall/table.rs) -------

/// `sys_handle_close` — release the caller's reference to a handle.
pub const SYS_HANDLE_CLOSE: u64 = 0;
/// `sys_handle_duplicate` — new handle to the same object, attenuated rights.
pub const SYS_HANDLE_DUPLICATE: u64 = 1;
/// `sys_handle_restrict` — attenuate a handle's rights in place.
pub const SYS_HANDLE_RESTRICT: u64 = 2;
/// `sys_handle_stat` — write a handle's metadata to user memory.
pub const SYS_HANDLE_STAT: u64 = 3;
/// `sys_memory_create` — allocate a `MemoryObject`, return its handle.
pub const SYS_MEMORY_CREATE: u64 = 4;
/// `sys_memory_map` — map a `MemoryObject` into the caller's address space.
pub const SYS_MEMORY_MAP: u64 = 5;
/// `sys_memory_unmap` — unmap a region of the caller's address space.
pub const SYS_MEMORY_UNMAP: u64 = 6;
/// `sys_clock_read` — read a clock's value (nanoseconds) into user memory.
pub const SYS_CLOCK_READ: u64 = 7;
/// `sys_timer_create` — create a `Timer` kernel object, return its handle.
pub const SYS_TIMER_CREATE: u64 = 8;
/// `sys_timer_set` — arm/disarm a timer at an absolute monotonic deadline.
pub const SYS_TIMER_SET: u64 = 9;
/// `sys_wait` — block until ≥1 of the given handles signals or the deadline elapses.
pub const SYS_WAIT: u64 = 10;
/// `sys_notif_recv` — dequeue one notification from a `NotificationChannel`.
pub const SYS_NOTIF_RECV: u64 = 11;
/// `sys_channel_create` — create an IPC channel, return its two endpoint handles.
pub const SYS_CHANNEL_CREATE: u64 = 12;
/// `sys_channel_send` — enqueue a message on a channel endpoint.
pub const SYS_CHANNEL_SEND: u64 = 13;
/// `sys_channel_recv` — dequeue a message from a channel endpoint.
pub const SYS_CHANNEL_RECV: u64 = 14;
/// `sys_process_spawn` — create a child process, return a handle to it.
pub const SYS_PROCESS_SPAWN: u64 = 15;
/// `sys_process_exit` — terminate the calling process with a status.
pub const SYS_PROCESS_EXIT: u64 = 16;
/// `sys_thread_exit` — terminate the calling thread with a status.
pub const SYS_THREAD_EXIT: u64 = 17;
/// `sys_thread_create` — start another thread in the calling process.
pub const SYS_THREAD_CREATE: u64 = 19;
/// `sys_thread_get_registers` — read a suspended (faulted) thread's registers.
pub const SYS_THREAD_GET_REGISTERS: u64 = 20;
/// `sys_exception_resume` — resume or terminate a thread suspended on a fault.
pub const SYS_EXCEPTION_RESUME: u64 = 21;
/// `sys_ns_create(from)` — create a `Namespace`, empty if `from` is null, else a **snapshot of
/// `from`'s bindings** (so it can be transferred, which a process's own root cannot), which needs
/// `LOOKUP` on `from`. A full-rights handle.
pub const SYS_NS_CREATE: u64 = 22;
/// `sys_ns_lookup` — resolve a path in a namespace, return a `PendingOperation`.
pub const SYS_NS_LOOKUP: u64 = 23;
/// `sys_ns_bind` — bind a resource handle at a path in a namespace.
pub const SYS_NS_BIND: u64 = 24;
/// `sys_ns_unbind` — remove the binding at a path in a namespace.
pub const SYS_NS_UNBIND: u64 = 25;
/// `sys_io_submit` — initiate an async I/O operation, returning a `PendingOperation`.
pub const SYS_IO_SUBMIT: u64 = 28;
/// `sys_ns_enumerate(ns, index, out, count)` — up to `count` of a namespace's bindings from
/// `index`, at most [`NS_ENUMERATE_MAX`](crate::NS_ENUMERATE_MAX): how many it wrote.
pub const SYS_NS_ENUMERATE: u64 = 30;
/// `sys_process_terminate` — **ask** a process to exit; it is delivered as a
/// `TerminateRequested` notification and nothing stops a process that ignores it (§11h).
pub const SYS_PROCESS_TERMINATE: u64 = 36;
/// `sys_power(system_control, op)` — flush every disk, stop every processor, then halt
/// ([`POWER_HALT`]) or reset ([`POWER_REBOOT`]). Needs `WRITE` on the system-control object,
/// which only `init` holds, and **returns only to refuse**.
pub const SYS_POWER: u64 = 40;
/// `sys_power`'s halt: the machine stays on, saying it is safe to turn it off.
pub const POWER_HALT: u64 = 0;
/// `sys_power`'s reboot: the FADT's reset register, then the 8042, then a triple fault.
pub const POWER_REBOOT: u64 = 1;
/// `sys_clock_set(clock, ns)` — set the wall clock to `ns` since the epoch, and the machine's RTC
/// under it. `clock` must be [`CLOCK_REALTIME`](crate::abi::CLOCK_REALTIME) and `ns` in
/// 2000–2099, or `InvalidArgument`; then the caller needs `SYSTEM_CLOCK`, or `NoAccess`. Returns
/// [`CLOCK_SET_KEPT`](crate::abi::CLOCK_SET_KEPT) or
/// [`CLOCK_SET_THIS_BOOT`](crate::abi::CLOCK_SET_THIS_BOOT).
pub const SYS_CLOCK_SET: u64 = 41;
/// Debug: write a user byte buffer to the kernel serial log. Not ABI-stable.
pub const SYS_DEBUG_KPRINT: u64 = 0xFFFF_0000;
/// Integration-test only: report a harness verdict (the argument's low byte) to
/// `isa-debug-exit`, which terminates QEMU. Implemented by the kernel **only** under its
/// `test-harness` feature; a production kernel returns `Unsupported`. Not ABI-stable.
///
/// **This returns whenever the device is absent, which is the normal case for every gate
/// except `test-qemu`**: they need the guest alive afterwards. The answer is `Unsupported`
/// there too, so it means "no verdict was delivered". Do not wrap this in a `-> !` helper.
/// See `docs/conventions/qemu-integration-tests.md`.
pub const SYS_TEST_EXIT: u64 = 0xFFFF_0002;

/// `SYS_TEST_EXIT` verdict: the run passed. QEMU exits `(0x10 << 1) | 1 = 33`.
pub const TEST_EXIT_SUCCESS: u32 = 0x10;
/// `SYS_TEST_EXIT` verdict: the run failed. QEMU exits `(0x11 << 1) | 1 = 35`.
pub const TEST_EXIT_FAILURE: u32 = 0x11;

// --- Raw `syscall`-instruction wrappers ------------------------------------

/// The 6-argument base wrapper; the others delegate here. Returns the kernel's `isize` result.
///
/// # Safety
/// `nr` must be a valid syscall number and `a0..a5` valid for it (including any
/// pointers being live user buffers of the required size/alignment).
#[cfg(not(miri))]
#[inline]
pub unsafe fn syscall6(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let ret;
    // SAFETY: register-only `syscall`; clobbers only the documented scratch
    // registers (`rcx`, `r11`). Argument validity is the caller's contract.
    unsafe {
        asm!(
            "syscall",
            in("rax") nr,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            in("r8") a4,
            in("r9") a5,
            out("rcx") _,
            out("r11") _,
            lateout("rax") ret,
        );
    }
    ret
}

/// [`syscall6`] under Miri, which runs no assembly: a host test that reaches a syscall is refused.
///
/// # Safety
/// None of its own: it touches nothing, and is `unsafe` to be called as the real one is.
#[cfg(miri)]
pub unsafe fn syscall6(_nr: u64, _a0: u64, _a1: u64, _a2: u64, _a3: u64, _a4: u64, _a5: u64) -> i64 {
    crate::error::KError::Unsupported.as_i32() as i64
}

/// Zero-argument syscall.
///
/// # Safety
/// See [`syscall6`].
#[inline]
pub unsafe fn syscall0(nr: u64) -> i64 {
    // SAFETY: delegates to `syscall6` with zeroed unused argument registers.
    unsafe { syscall6(nr, 0, 0, 0, 0, 0, 0) }
}

/// One-argument syscall.
///
/// # Safety
/// See [`syscall6`].
#[inline]
pub unsafe fn syscall1(nr: u64, a0: u64) -> i64 {
    // SAFETY: delegates to `syscall6` with zeroed unused argument registers.
    unsafe { syscall6(nr, a0, 0, 0, 0, 0, 0) }
}

/// Two-argument syscall.
///
/// # Safety
/// See [`syscall6`].
#[inline]
pub unsafe fn syscall2(nr: u64, a0: u64, a1: u64) -> i64 {
    // SAFETY: delegates to `syscall6` with zeroed unused argument registers.
    unsafe { syscall6(nr, a0, a1, 0, 0, 0, 0) }
}

/// Three-argument syscall.
///
/// # Safety
/// See [`syscall6`].
#[inline]
pub unsafe fn syscall3(nr: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    // SAFETY: delegates to `syscall6` with zeroed unused argument registers.
    unsafe { syscall6(nr, a0, a1, a2, 0, 0, 0) }
}

/// Four-argument syscall.
///
/// # Safety
/// See [`syscall6`].
#[inline]
pub unsafe fn syscall4(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    // SAFETY: delegates to `syscall6` with zeroed unused argument registers.
    unsafe { syscall6(nr, a0, a1, a2, a3, 0, 0) }
}

/// Five-argument syscall.
///
/// # Safety
/// See [`syscall6`].
#[inline]
pub unsafe fn syscall5(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> i64 {
    // SAFETY: delegates to `syscall6` with zeroed unused argument registers.
    unsafe { syscall6(nr, a0, a1, a2, a3, a4, 0) }
}
