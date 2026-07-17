//! Syscall numbers and the raw `syscall`-instruction wrappers.
//!
//! The `SYS_*` numbers are pinned to `kernel/src/syscall/table.rs` (the canonical
//! source); they are sequential from `0` and frozen at v1.0
//! (`docs/spec/syscall-abi.md`). Each `syscallN` is `unsafe` — the `syscall`
//! instruction is unsafe and the caller is responsible for valid arguments and
//! pointers. Higher layers wrap these in safe `Result`-returning helpers (see
//! [`crate::error::from_raw`]).
//!
//! Register convention (System V-derived, matching the kernel entry stub): `rax`
//! = number, args in `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9`; `rcx`/`r11` are
//! clobbered by the `syscall` instruction; the `isize` result returns in `rax`.

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
/// `sys_thread_set_affinity` — restrict a thread's CPU set (no-op until SMP).
pub const SYS_THREAD_SET_AFFINITY: u64 = 18;
/// `sys_thread_create` — start another thread in the calling process.
pub const SYS_THREAD_CREATE: u64 = 19;
/// `sys_thread_get_registers` — read a suspended (faulted) thread's registers.
pub const SYS_THREAD_GET_REGISTERS: u64 = 20;
/// `sys_exception_resume` — resume or terminate a thread suspended on a fault.
pub const SYS_EXCEPTION_RESUME: u64 = 21;
/// `sys_ns_create` — create an empty `Namespace`, return a full-rights handle.
pub const SYS_NS_CREATE: u64 = 22;
/// `sys_ns_lookup` — resolve a path in a namespace, return a `PendingOperation`.
pub const SYS_NS_LOOKUP: u64 = 23;
/// `sys_ns_bind` — bind a resource handle at a path in a namespace.
pub const SYS_NS_BIND: u64 = 24;
/// `sys_ns_unbind` — remove the binding at a path in a namespace.
pub const SYS_NS_UNBIND: u64 = 25;
/// `sys_entropy_create` — create an `EntropyObject` handle onto the kernel CSPRNG.
pub const SYS_ENTROPY_CREATE: u64 = 26;
/// `sys_entropy_read` — fill a buffer with CSPRNG output (or a PO if unseeded).
pub const SYS_ENTROPY_READ: u64 = 27;
/// `sys_io_submit` — initiate an async I/O operation, returning a `PendingOperation`.
pub const SYS_IO_SUBMIT: u64 = 28;
/// `sys_io_cancel` — request cancellation (Phase 2: `Unsupported`).
pub const SYS_IO_CANCEL: u64 = 29;
/// `sys_ns_enumerate` — list a namespace's bindings (mount points + kernel resources).
pub const SYS_NS_ENUMERATE: u64 = 30;
/// `sys_file_sync` — flush a writable file mapping's pages to the device.
pub const SYS_FILE_SYNC: u64 = 31;
/// `sys_file_grow` — resolve a file, growing it to a target size first.
pub const SYS_FILE_GROW: u64 = 32;
/// `sys_file_create` — create a file, grow it to a target size, then resolve.
pub const SYS_FILE_CREATE: u64 = 33;
/// Debug: write a user byte buffer to the kernel serial log. Not ABI-stable.
pub const SYS_DEBUG_KPRINT: u64 = 0xFFFF_0000;
/// Integration-test only: end the QEMU run with a harness verdict (the argument's
/// low byte, via `isa-debug-exit`). Implemented by the kernel **only** under its
/// `test-harness` feature (`cargo xtask test-qemu`); a production kernel returns
/// `Unsupported`. Not ABI-stable. (`0xFFFF_0001` was the long-retired process-exit
/// `sys_debug_exit`; this is a distinct syscall at the next number.) See
/// `docs/conventions/qemu-integration-tests.md`.
pub const SYS_TEST_EXIT: u64 = 0xFFFF_0002;

/// `SYS_TEST_EXIT` verdict: the integration-test run passed. QEMU exits `(0x10 <<
/// 1) | 1 = 33`, which the xtask runner maps to success.
pub const TEST_EXIT_SUCCESS: u32 = 0x10;
/// `SYS_TEST_EXIT` verdict: the integration-test run failed. QEMU exits `(0x11 <<
/// 1) | 1 = 35`, which the xtask runner maps to failure.
pub const TEST_EXIT_FAILURE: u32 = 0x11;

// --- Raw `syscall`-instruction wrappers ------------------------------------

/// The 6-argument base wrapper; the others zero unused argument registers and
/// delegate here. Returns the kernel's `isize` result (as `i64`).
///
/// # Safety
/// `nr` must be a valid syscall number and `a0..a5` valid for it (including any
/// pointers being live user buffers of the required size/alignment).
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

/// Zero-argument syscall. # Safety: see [`syscall6`].
#[inline]
pub unsafe fn syscall0(nr: u64) -> i64 {
    // SAFETY: delegates to `syscall6` with zeroed unused argument registers.
    unsafe { syscall6(nr, 0, 0, 0, 0, 0, 0) }
}

/// One-argument syscall. # Safety: see [`syscall6`].
#[inline]
pub unsafe fn syscall1(nr: u64, a0: u64) -> i64 {
    // SAFETY: delegates to `syscall6` with zeroed unused argument registers.
    unsafe { syscall6(nr, a0, 0, 0, 0, 0, 0) }
}

/// Two-argument syscall. # Safety: see [`syscall6`].
#[inline]
pub unsafe fn syscall2(nr: u64, a0: u64, a1: u64) -> i64 {
    // SAFETY: delegates to `syscall6` with zeroed unused argument registers.
    unsafe { syscall6(nr, a0, a1, 0, 0, 0, 0) }
}

/// Three-argument syscall. # Safety: see [`syscall6`].
#[inline]
pub unsafe fn syscall3(nr: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    // SAFETY: delegates to `syscall6` with zeroed unused argument registers.
    unsafe { syscall6(nr, a0, a1, a2, 0, 0, 0) }
}

/// Four-argument syscall. # Safety: see [`syscall6`].
#[inline]
pub unsafe fn syscall4(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    // SAFETY: delegates to `syscall6` with zeroed unused argument registers.
    unsafe { syscall6(nr, a0, a1, a2, a3, 0, 0) }
}

/// Five-argument syscall. # Safety: see [`syscall6`].
#[inline]
pub unsafe fn syscall5(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> i64 {
    // SAFETY: delegates to `syscall6` with zeroed unused argument registers.
    unsafe { syscall6(nr, a0, a1, a2, a3, a4, 0) }
}
