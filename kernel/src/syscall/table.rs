//! The syscall number → handler table and the handlers themselves.
//!
//! Dispatch is a `match` on the number (per `kernel/CLAUDE.md`: match, not
//! `dyn`), keyed by the constants below. The stable ABI numbers
//! (`docs/spec/syscall-abi.md`) are allocated sequentially from `0`; the
//! **debug** syscalls this slice adds live in a high, deliberately
//! non-stable range so they can never shadow a future stable number. They
//! are excluded from the v1.0 ABI freeze and exist only to bootstrap and
//! exercise the entry/exit path before real syscalls land.

use super::error::{KError, SysResult, encode, from_user_access};
use crate::arch::Paging;
use crate::arch::Timer;
use crate::arch::abi::USER_VIRT_END;
use crate::arch::paging::ArchPaging;
use crate::arch::timer::ArchTimer;
use crate::handle::global;
use crate::handle::table::{HandleError, HandleTable, LookupOk};
use crate::libkern::clock::ClockId;
use crate::libkern::handle::{HandleInfo, KObjectType, NsEntry, RawHandle, Rights};
use crate::libkern::ipc::{IPC_DEFAULT_QUEUE_DEPTH, IPC_HANDLE_MAX, IPC_MAX_QUEUE_DEPTH, IPC_PAYLOAD_SIZE};
use crate::libkern::spawn::{SPAWN_MAX_HANDLES, SpawnArgs};
use crate::arch::RegisterValues;
use crate::arch::registers::ArchRegisters;
use crate::libkern::io_op::{IoOp, IoOpcode};
use crate::libkern::{
    ExitKind, ExitStatus, IoResult, KBox, MemFlags, Notification, SendMode, ThreadArgs, TimerFlags,
};
use crate::mm::addr_space::{AddressSpace, MapError};
use crate::mm::elf::load_elf;
use crate::mm::user_access::{
    UserMutPtr, UserPtr, copy_from_user, copy_slice_from_user, copy_slice_to_user, copy_to_user,
};
use crate::mm::vmm::{Protection, VAddrRange};
use crate::mm::{PAGE_SIZE, VirtAddr};
// `Timer` (the arch hardware-clock alias) is imported above; the Timer kernel
// object is referenced as `TimerObject` to avoid the name clash.
use crate::object::kernel_server::{self, OpStatus};
use crate::object::namespace::{NS_PATH_MAX, ResolvedTarget, validate_path};
use crate::object::device_node::DeviceClass;
use crate::object::{
    BlockSendOutcome, DeviceNode, EntropyObject, FileObject, IpcChannel, MAX_WAIT_HANDLES,
    MemoryObject, Namespace, NotificationChannel, NsError, ObjectRef, PendingOperation, Process,
    ReclaimedSend, RecvState, SendOutcome, StoredMsg, Timer as TimerObject, TransferRef,
    UserspaceServerReg,
};

// --- Stable ABI syscall numbers -----------------------------------------
//
// Sequential from `0`, frozen at v1.0 (`docs/spec/syscall-abi.md`). These
// four handle operations are the **first** stable syscalls. Syscall numbers
// are not part of the kernel ABI version hash.

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
/// `sys_wait` — block until ≥1 of the given handles signals or the deadline
/// elapses; the unified blocking primitive.
pub const SYS_WAIT: u64 = 10;
/// `sys_notif_recv` — dequeue one notification from a `NotificationChannel`.
pub const SYS_NOTIF_RECV: u64 = 11;
/// `sys_channel_create` — create an IPC channel, return its two endpoint handles.
pub const SYS_CHANNEL_CREATE: u64 = 12;
/// `sys_channel_send` — enqueue a message on a channel endpoint (NoBlock).
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
/// `sys_entropy_read` — fill a buffer with CSPRNG output (or return a PO if unseeded).
pub const SYS_ENTROPY_READ: u64 = 27;
/// `sys_io_submit` — initiate an async I/O operation, returning a `PendingOperation`.
pub const SYS_IO_SUBMIT: u64 = 28;
/// `sys_io_cancel` — request cancellation of an in-flight operation (Phase 2: `Unsupported`).
pub const SYS_IO_CANCEL: u64 = 29;
/// `sys_ns_enumerate` — list a namespace's bindings (mount points + kernel resources).
pub const SYS_NS_ENUMERATE: u64 = 30;

/// Debug: write a user byte buffer to the kernel serial log. Not ABI-stable.
pub const SYS_DEBUG_KPRINT: u64 = 0xFFFF_0000;

/// Integration-test only (`test-harness` feature): terminate the emulator with a
/// harness verdict (the low byte, via QEMU `isa-debug-exit`). The self-test build
/// calls this to report the run's pass/fail from userspace after the boot chain
/// completes. Absent from production kernels — not in the ABI hash, no backdoor.
/// (`0xFFFF_0001` was the retired process-exit `sys_debug_exit`; this is distinct.)
#[cfg(feature = "test-harness")]
pub const SYS_TEST_EXIT: u64 = 0xFFFF_0002;

/// Largest buffer `sys_kprint` will copy in one call. Bounds the on-stack
/// kernel buffer; well under `MAX_USER_COPY_SIZE`.
const KPRINT_MAX: usize = 4096;

/// Route a decoded syscall to its handler. `nr` is the number (from RAX);
/// `a0..a5` are the six argument registers (RDI, RSI, RDX, R10, R8, R9).
/// Returns the `isize` the ABI hands back in RAX.
pub fn dispatch(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> isize {
    // No explicit grace `quiesce` is needed on this dispatch path: every
    // handle syscall below routes through a `HandleTable` method that takes
    // and drops a read guard, which marks the calling context (ctx 0 in
    // Phase 1) quiescent on drop, so deferred closes are reclaimed on the
    // next allocate/close. TODO(smp): a future non-lookup syscall, or SMP,
    // may need an explicit `global::get().quiesce(current_ctx_id())` here.
    match nr {
        SYS_HANDLE_CLOSE => encode(sys_handle_close(a0)),
        SYS_HANDLE_DUPLICATE => encode(sys_handle_duplicate(a0, a1)),
        SYS_HANDLE_RESTRICT => encode(sys_handle_restrict(a0, a1)),
        SYS_HANDLE_STAT => encode(sys_handle_stat(a0, a1)),
        SYS_MEMORY_CREATE => encode(sys_memory_create(a0, a1)),
        SYS_MEMORY_MAP => encode(sys_memory_map(a0, a1, a2, a3)),
        SYS_MEMORY_UNMAP => encode(sys_memory_unmap(a0, a1)),
        SYS_CLOCK_READ => encode(sys_clock_read(a0, a1)),
        SYS_TIMER_CREATE => encode(sys_timer_create(a0)),
        SYS_TIMER_SET => encode(sys_timer_set(a0, a1, a2)),
        SYS_WAIT => encode(sys_wait(a0, a1 as usize, a2, a3)),
        SYS_NOTIF_RECV => encode(sys_notif_recv(a0, a1)),
        SYS_CHANNEL_CREATE => encode(sys_channel_create(a0, a1, a2)),
        SYS_CHANNEL_SEND => encode(sys_channel_send(a0, a1, a2, a3, a4, a5)),
        SYS_CHANNEL_RECV => encode(sys_channel_recv(a0, a1, a2, a3)),
        SYS_PROCESS_SPAWN => encode(sys_process_spawn(a0)),
        SYS_THREAD_SET_AFFINITY => encode(sys_thread_set_affinity(a0, a1)),
        SYS_THREAD_CREATE => encode(sys_thread_create(a0)),
        SYS_THREAD_GET_REGISTERS => encode(sys_thread_get_registers(a0, a1)),
        SYS_EXCEPTION_RESUME => encode(sys_exception_resume(a0, a1, a2)),
        SYS_NS_CREATE => encode(sys_ns_create()),
        SYS_NS_LOOKUP => encode(sys_ns_lookup(a0, a1, a2 as usize, a3)),
        SYS_NS_BIND => encode(sys_ns_bind(a0, a1, a2 as usize, a3)),
        SYS_NS_UNBIND => encode(sys_ns_unbind(a0, a1, a2 as usize)),
        SYS_NS_ENUMERATE => encode(sys_ns_enumerate(a0, a1, a2)),
        SYS_ENTROPY_CREATE => encode(sys_entropy_create()),
        SYS_ENTROPY_READ => encode(sys_entropy_read(a0, a1, a2 as usize)),
        SYS_IO_SUBMIT => encode(sys_io_submit(a0, a1)),
        SYS_IO_CANCEL => encode(sys_io_cancel(a0)),
        SYS_DEBUG_KPRINT => encode(sys_kprint(a0, a1 as usize)),
        // Integration-test build only: end the QEMU run with the caller's verdict.
        // Diverges (QEMU exits); never returns to dispatch/sysret.
        #[cfg(feature = "test-harness")]
        SYS_TEST_EXIT => crate::arch::debug_exit(a0 as u32),
        // These diverge into the scheduler; they never return to dispatch/sysret.
        SYS_PROCESS_EXIT => sys_process_exit(a0 as i32),
        SYS_THREAD_EXIT => sys_thread_exit(a0 as i32),
        _ => KError::Unsupported.as_isize(),
    }
}

/// `sys_process_exit(status)` — terminate the calling **process** with
/// `status`. The scheduler tears down every sibling thread of the caller's
/// process (an `owner_pid` scan of the run/blocked/suspended queues), delivers a
/// `ChildExited { pid, Normal(status) }` to the parent's notification channel
/// (if any), then switches away and reaps this thread — releasing the last
/// `Process` reference and freeing the address space. Never returns.
fn sys_process_exit(status: i32) -> ! {
    crate::sched::exit_process(ExitStatus { kind: ExitKind::Normal as u32, code: status })
}

/// `sys_thread_exit(status)` — terminate the **calling thread** with `status`.
/// Unlike [`sys_process_exit`], sibling threads keep running: a `ChildExited`
/// fires only if this was the process's last thread (the scheduler's last-thread
/// check). Never returns.
fn sys_thread_exit(status: i32) -> ! {
    crate::sched::exit_thread(ExitStatus { kind: ExitKind::Normal as u32, code: status })
}

/// Close every child-side handle a partially-built spawn allocated, so a
/// failure before the commit (the child thread enqueue) leaves no handles
/// tagged to a process that will never run. `ns_h` is the child's namespace
/// handle (`RawHandle::NULL` if none was installed).
fn spawn_rollback_child_handles(
    child_pid: u32,
    notif_h: RawHandle,
    ns_h: RawHandle,
    installed: &[RawHandle],
) {
    close_and_release(notif_h, child_pid);
    if !ns_h.is_null() {
        close_and_release(ns_h, child_pid);
    }
    for &h in installed {
        if !h.is_null() {
            close_and_release(h, child_pid);
        }
    }
}

/// `sys_process_spawn(args)` — create a child process from a kernel-embedded
/// image and start it. Returns a handle to the child `Process`
/// (`SIGNAL | TERMINATE`). The child's initial handle table is populated from
/// `args.handles` (each installed with `source_rights & args.rights[i]`; bit
/// `i` of `args.move_mask` chooses move vs. duplicate), plus a fresh
/// notification channel. The child learns its handle *values* via the bootstrap
/// registers seeded at entry: `rdi` = its notification-channel handle, `rsi` =
/// its first installed handle, `rdx` = `args.arg0`.
///
/// Atomic-or-fail: any failure before the child thread is enqueued rolls back
/// every child-side allocation and leaves the parent's handles untouched (moves
/// close the parent's source handle only after the spawn commits).
pub fn sys_process_spawn(args_ptr: u64) -> SysResult {
    let aptr = UserPtr::<SpawnArgs>::new(args_ptr).map_err(from_user_access)?;
    let args = copy_from_user(aptr).map_err(from_user_access)?;
    let count = args.handle_count as usize;
    if count > SPAWN_MAX_HANDLES {
        return Err(KError::TooLarge);
    }
    let parent_pid = crate::sched::current_owner_pid();

    // Resolve the image handle → a readable object holding the ELF (the spawner resolved
    // the executable path in userspace). Requires the caller hold it with `MAP_READ`.
    // Accept a `MemoryObject` (eager — e.g. an initramfs file) or a `FileObject` (a
    // demand-paged file on the fs-server — e.g. a program in the content-addressed store).
    let img = global::get()
        .lookup(RawHandle(args.image.bits()), parent_pid, Rights::MAP_READ)
        .map_err(map_handle_err)?;
    // The ELF loader needs one contiguous slice; materialize the image's bytes into one.
    // (A `MemoryObject`'s frames are page-fragmented; a `FileObject`'s pages are filled
    // on demand. Deferred: map the frames into a temporary kernel VMA instead of copying.)
    let img_bytes = match img.object.object_type() {
        // SAFETY: type checked above; `img.object` pins it live.
        KObjectType::MemoryObject => unsafe { &*(img.object.as_ptr() as *const MemoryObject) }
            .copy_to_kvec()
            .map_err(|_| KError::OutOfMemory)?,
        // Drives the fs-server producer per page, blocking on each fill — sound here on
        // the syscall thread (holds no AS/cache lock).
        KObjectType::FileObject => {
            FileObject::read_to_kvec(&img.object).map_err(|_| KError::OutOfMemory)?
        }
        _ => return Err(KError::InvalidArgument),
    };

    // Build the child address space from the image. Unlike a trusted kernel-embedded
    // image, a spawner-supplied ELF is untrusted userspace input, so a malformed one is
    // `InvalidArgument` (the loader bounds-checks all offsets → memory-safe on garbage).
    let asp = AddressSpace::new().map_err(|_| KError::OutOfMemory)?;
    let info = load_elf(&asp, &img_bytes).map_err(|_| KError::InvalidArgument)?;
    // The image object is no longer needed once its bytes are copied + loaded.
    drop(img);
    let child_pid = crate::sched::alloc_pid();

    // Attenuate the requested syscaps against the parent's: a parent can never grant a
    // capability it does not hold (`child = parent & requested`). No current process
    // (a spawn with no owning Process) yields an unprivileged child.
    // (docs/architecture/syscaps.md)
    let child_syscaps = {
        let requested = crate::libkern::SysCaps::from_bits_truncate(args.syscaps);
        match crate::sched::current_process() {
            // SAFETY: pins a live Process; read its immutable syscap set.
            Some(pp) => unsafe { &*(pp.as_ptr() as *const Process) }.syscaps() & requested,
            None => crate::libkern::SysCaps::empty(),
        }
    };

    // Build the child Process (owns the AS); record the parent's notification
    // channel as its `ChildExited` target.
    let mut proc_box =
        Process::try_new_user(child_pid, asp, child_syscaps).map_err(|_| KError::OutOfMemory)?;
    if let Some(parent_proc) = crate::sched::current_process() {
        // SAFETY: `parent_proc` pins a live Process; clone its channel ref.
        if let Some(c) =
            unsafe { &*(parent_proc.as_ptr() as *const Process) }.notification_channel_ref()
        {
            proc_box.set_parent_notif(c);
        }
    }

    // The child's own notification channel: the Process owns one reference; a
    // handle in the child's table owns the other (so the child can wait/recv).
    let chan = NotificationChannel::try_new().map_err(|_| KError::OutOfMemory)?;
    let chan_ptr = KBox::into_raw(chan).as_ptr() as *mut ();
    // SAFETY: `into_raw` yielded the single creation reference; adopt it.
    let chan_ref = unsafe { ObjectRef::from_raw(chan_ptr, KObjectType::NotificationChannel) };
    proc_box.set_notification_channel(chan_ref.clone()); // refcount 2
    let (cp, ct) = chan_ref.into_raw();
    let notif_rights = Rights::WAIT | Rights::DUPLICATE | Rights::INSPECT;
    let notif_h = match global::get().allocate(child_pid, cp, ct, notif_rights) {
        Ok(h) => h,
        Err(e) => {
            // SAFETY: reclaim the channel-handle reference; `proc_box` drops here
            // too, releasing its own channel reference and freeing the AS.
            drop(unsafe { ObjectRef::from_raw(cp, ct) });
            return Err(map_handle_err(e));
        }
    };

    // Determine the child's root namespace (sandbox-by-construction): an explicit
    // `args.namespace` the parent holds a `LOOKUP`-righted handle to, else inherit
    // the parent's own namespace. The child always gets a **LOOKUP-only** handle —
    // it resolves names but cannot rebind its root. `None` ⇒ the parent has no
    // namespace and supplied none (degenerate; pid 1 always has one).
    let ns_source: Option<ObjectRef> = if !args.namespace.is_null() {
        match lookup_typed(args.namespace.bits(), parent_pid, Rights::LOOKUP, KObjectType::Namespace)
        {
            Ok(ok) => Some(ok.object.clone()),
            Err(e) => {
                // SAFETY: reclaim the child's notif handle; `proc_box` drops the AS.
                spawn_rollback_child_handles(child_pid, notif_h, RawHandle::NULL, &[]);
                return Err(e);
            }
        }
    } else {
        crate::sched::current_process().and_then(|p| {
            // SAFETY: `p` pins a live Process; clone its namespace ref if any.
            unsafe { &*(p.as_ptr() as *const Process) }.namespace_ref()
        })
    };
    let child_ns_h = if let Some(ns_ref) = ns_source {
        proc_box.set_namespace(ns_ref.clone()); // the child owns a reference
        let (np, nt) = ns_ref.into_raw();
        match global::get().allocate(child_pid, np, nt, Rights::LOOKUP) {
            Ok(h) => h,
            Err(e) => {
                // SAFETY: reclaim the namespace-handle reference; `proc_box` drops
                // its own clone + the AS. Roll back the notif handle too.
                drop(unsafe { ObjectRef::from_raw(np, nt) });
                spawn_rollback_child_handles(child_pid, notif_h, RawHandle::NULL, &[]);
                return Err(map_handle_err(e));
            }
        }
    } else {
        RawHandle::NULL
    };

    // Install the transferred handles into the child's table (clone the source
    // reference + allocate for the child; the parent's source handle is closed
    // only after the spawn commits, so a failure here doesn't lose it).
    let mut installed = [RawHandle::NULL; SPAWN_MAX_HANDLES];
    for i in 0..count {
        let ok = match global::get().lookup(args.handles[i], parent_pid, Rights::TRANSFER) {
            Ok(ok) => ok,
            Err(e) => {
                spawn_rollback_child_handles(child_pid, notif_h, child_ns_h, &installed[..i]);
                return Err(map_handle_err(e));
            }
        };
        let child_rights = ok.rights & Rights::from_bits_truncate(args.rights[i]);
        // SAFETY: `ok.object` pins a live object; clone bumps the refcount for
        // the child's new handle.
        let (op, ot) = ok.object.clone().into_raw();
        match global::get().allocate(child_pid, op, ot, child_rights) {
            Ok(h) => installed[i] = h,
            Err(e) => {
                // SAFETY: reclaim the clone; `ok.object` drops at scope end.
                drop(unsafe { ObjectRef::from_raw(op, ot) });
                spawn_rollback_child_handles(child_pid, notif_h, child_ns_h, &installed[..i]);
                return Err(map_handle_err(e));
            }
        }
        // `ok.object` drops here, releasing the lookup reference.
    }

    // Wrap the child Process; keep a clone for the parent's process handle.
    let proc_obj = {
        let p = KBox::into_raw(proc_box).as_ptr() as *mut ();
        // SAFETY: `into_raw` yielded the single creation reference; adopt it.
        unsafe { ObjectRef::from_raw(p, KObjectType::Process) }
    };
    let proc_for_handle = proc_obj.clone();

    // Commit: enqueue the child's main thread with the bootstrap registers
    // (rdi=notif, rsi=namespace, rdx=installed[0], rcx=arg0).
    let endpoint_h = if count > 0 { installed[0].bits() } else { 0 };
    let boot = [notif_h.bits(), child_ns_h.bits(), endpoint_h, args.arg0];
    if crate::sched::spawn_user(proc_obj, info.entry_point.as_u64(), info.stack_top.as_u64(), boot)
        .is_err()
    {
        // `spawn_user` consumed (and dropped) `proc_obj`; release the clone to
        // free the Process + AS, and roll back the child handles.
        drop(proc_for_handle);
        spawn_rollback_child_handles(child_pid, notif_h, child_ns_h, &installed[..count]);
        return Err(KError::OutOfMemory);
    }

    // Allocate the parent's handle to the child Process.
    let (pp, pt) = proc_for_handle.into_raw();
    let parent_handle =
        match global::get().allocate(parent_pid, pp, pt, Rights::SIGNAL | Rights::TERMINATE) {
            Ok(h) => h,
            Err(e) => {
                // The child is already running; reclaim only the process-handle
                // reference (the child stays alive, just unreferenced by us).
                // SAFETY: adopt + drop the reference yielded by `into_raw`.
                drop(unsafe { ObjectRef::from_raw(pp, pt) });
                return Err(map_handle_err(e));
            }
        };

    // The spawn committed: finalise moves by closing the parent's source handle
    // for each move-marked entry (its reference already lives in the child).
    for i in 0..count {
        if (args.move_mask >> i) & 1 == 1 {
            close_and_release(args.handles[i], parent_pid);
        }
    }

    Ok(parent_handle.bits() as isize)
}

/// `sys_thread_set_affinity(thread, cpu_mask)` — restrict which CPUs a thread may
/// run on (bit `c` ⇒ may run on CPU `c`). Requires `SIGNAL` on the `Thread` handle.
/// Bits above `MAX_CPUS` are ignored; at least one valid CPU bit must be set (an
/// empty mask — a thread that could never be scheduled — is `InvalidArgument`).
/// Takes effect at the thread's next placement/steal/wake.
pub fn sys_thread_set_affinity(thread_h: u64, cpu_mask: u64) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    let ok = lookup_typed(thread_h, pid, Rights::SIGNAL, KObjectType::Thread)?;
    let mask = (cpu_mask & ((1u64 << crate::arch::MAX_CPUS) - 1)) as u8;
    if mask == 0 {
        return Err(KError::InvalidArgument);
    }
    crate::sched::set_thread_affinity(ok.object.as_ptr(), mask);
    Ok(0)
}

/// `sys_thread_create(args)` — start another thread in the **calling process**
/// and return a `Thread` handle (`SIGNAL | TERMINATE | INSPECT | DUPLICATE`) to
/// it. The new thread begins at `args.entry` (ring 3) with `rsp = args.user_sp`
/// and `rdx = args.arg0`; the caller owns the user stack (allocate + map it, pass
/// its top). The `entry`/`user_sp` must be non-null user-half addresses — that
/// they are *mapped* is the caller's responsibility, since an unmapped entry or
/// stack faults the new thread, which is then contained by suspend/terminate.
///
/// This is the supervisor primitive behind exception handling: a sibling thread
/// can hold a faulting thread's handle and act on it (`sys_thread_get_registers`
/// / `sys_exception_resume`).
pub fn sys_thread_create(args_ptr: u64) -> SysResult {
    let aptr = UserPtr::<ThreadArgs>::new(args_ptr).map_err(from_user_access)?;
    let args = copy_from_user(aptr).map_err(from_user_access)?;
    // Reserved bytes must be zero (forward-compat).
    if args._reserved != [0u8; 36] {
        return Err(KError::InvalidArgument);
    }
    // Reject null / kernel-half entry or stack before touching the scheduler.
    if args.entry == 0 || args.user_sp == 0 {
        return Err(KError::InvalidArgument);
    }
    UserPtr::<u8>::new(args.entry).map_err(|_| KError::InvalidArgument)?;
    UserPtr::<u8>::new(args.user_sp).map_err(|_| KError::InvalidArgument)?;

    // Parse + validate the scheduling parameters. A zeroed block is TimeShared / nice 0
    // / no affinity — the historical default. The RealTime class is REAL_TIME-syscap-
    // gated; `nice`/affinity are ungated (renicing/pinning your own thread isn't
    // privileged). See docs/architecture/syscaps.md.
    use crate::libkern::thread::{THREAD_CLASS_REALTIME, THREAD_CLASS_TIMESHARED};
    use crate::object::thread::SchedClass;
    let class = match args.class {
        THREAD_CLASS_TIMESHARED => SchedClass::TimeShared,
        THREAD_CLASS_REALTIME => {
            require_syscap(crate::libkern::SysCaps::REAL_TIME)?;
            if args.rt_priority > 99 {
                return Err(KError::InvalidArgument);
            }
            SchedClass::RealTime
        }
        _ => return Err(KError::InvalidArgument),
    };
    if args.nice < -20 || args.nice > 19 {
        return Err(KError::InvalidArgument);
    }
    // Affinity: `0` ⇒ no restriction; else the mask trimmed to valid CPU bits (must be
    // non-empty after trimming).
    let cpu_mask = if args.cpu_affinity == 0 {
        u8::MAX
    } else {
        let valid = ((1u16 << crate::arch::MAX_CPUS) - 1) as u8;
        let m = args.cpu_affinity & valid;
        if m == 0 {
            return Err(KError::InvalidArgument);
        }
        m
    };

    // Require a user process (a kernel/boot thread cannot create user threads).
    let proc = crate::sched::current_process().ok_or(KError::InvalidArgument)?;
    let pid = crate::sched::current_owner_pid();

    // Start the thread; the bootstrap delivers `arg0` in rcx (a new thread shares
    // the process, so it gets no notif/namespace/endpoint handoff — rdi/rsi/rdx 0).
    let th = crate::sched::spawn_user_sched(
        proc,
        args.entry,
        args.user_sp,
        [0, 0, 0, args.arg0],
        class,
        args.rt_priority,
        args.nice,
        cpu_mask,
    )
    .map_err(|_| KError::OutOfMemory)?;

    // Install a handle to it in the caller's table.
    let (tp, tt) = th.into_raw();
    let rights =
        Rights::SIGNAL | Rights::TERMINATE | Rights::INSPECT | Rights::DUPLICATE;
    match global::get().allocate(pid, tp, tt, rights) {
        Ok(h) => Ok(h.bits() as isize),
        Err(e) => {
            // The thread is already running (its `ready` entry keeps it alive);
            // reclaim only this surplus reference, off the rank-3 table lock.
            // SAFETY: adopt + drop the single reference `into_raw` yielded.
            drop(unsafe { ObjectRef::from_raw(tp, tt) });
            Err(map_handle_err(e))
        }
    }
}

/// `sys_thread_get_registers(thread, out)` — write the user registers of a
/// **suspended** (faulted) thread into the [`RegisterValues`] at `out`. The
/// thread handle must carry `SIGNAL` and name a `Thread`; the thread must be
/// suspended on a fault (else `InvalidArgument`). The captured `ExceptionFrame`
/// lives on the suspended thread's kernel stack (in the shared kernel half,
/// readable under the supervisor's address space); the thread stays parked, so
/// the frame is stable across the (lock-free) copy-out.
pub fn sys_thread_get_registers(thread_h: u64, out_ptr: u64) -> SysResult {
    // Validate the output pointer first — a pure range check, reachable without
    // a running thread (host-testable), and matching the spawn-args convention
    // of validating the user pointer before resolving the handle.
    let uptr = UserMutPtr::<RegisterValues>::new(out_ptr).map_err(from_user_access)?;

    let pid = crate::sched::current_owner_pid();
    let ok = lookup_typed(thread_h, pid, Rights::SIGNAL, KObjectType::Thread)?;

    // Must be suspended on a fault — yields the captured frame's address.
    let frame_ptr = crate::sched::thread_exception_frame(ok.object.as_ptr())
        .ok_or(KError::InvalidArgument)?;
    // Decode the registers out of the frame (SCHED already released), then copy
    // out to user memory (never under a lock).
    let regs: RegisterValues = crate::arch::Registers::read_from_exception_frame(frame_ptr);
    copy_to_user(uptr, &regs).map_err(from_user_access)?;
    Ok(0)
}

/// `sys_exception_resume(thread, disposition, code)` — act on a thread suspended
/// on a fault. `disposition` is `0` = **Resume** (re-enter the faulting
/// instruction — without fixing the fault's cause it simply re-faults) or `2` =
/// **Terminate** (exit the thread with `code`); other values are reserved
/// (`Unsupported`). The handle must carry `SIGNAL` and name a `Thread`; the
/// thread must currently be suspended (else `InvalidArgument`). Returns `0`.
pub fn sys_exception_resume(thread_h: u64, disposition: u64, code: u64) -> SysResult {
    // Validate the disposition first — a pure value check, reachable without a
    // running thread (host-testable). Phase 1 dispositions: 0 = Resume, 2 =
    // Terminate. The rest (ResumeSkip, ModifyAndResume, …) are Phase 2.
    let tag = match disposition {
        0 => 0u8,
        2 => 2u8,
        _ => return Err(KError::Unsupported),
    };

    let pid = crate::sched::current_owner_pid();
    let ok = lookup_typed(thread_h, pid, Rights::SIGNAL, KObjectType::Thread)?;
    if !crate::sched::resume_suspended(ok.object.as_ptr(), tag, code as i32) {
        // Not currently suspended (already resumed, or never faulted).
        return Err(KError::InvalidArgument);
    }
    Ok(0)
}

/// `sys_kprint(ptr, len)` — copy `len` bytes from the user buffer at `ptr`
/// and write them to the serial console. Debug-only. Returns the number of
/// bytes written.
///
/// The validation/bounds checks are ordered so the `len == 0` and
/// `len > KPRINT_MAX` paths are reachable without touching user memory or
/// the serial port (host-testable); the copy + serial write are exercised
/// only under QEMU.
pub fn sys_kprint(ptr: u64, len: usize) -> SysResult {
    if len == 0 {
        return Ok(0);
    }
    if len > KPRINT_MAX {
        return Err(KError::TooLarge);
    }
    let uptr = UserPtr::<u8>::new(ptr).map_err(from_user_access)?;

    let mut buf = [0u8; KPRINT_MAX];
    let dst = &mut buf[..len];
    // SMAP-safe, fault-recovering copy: a bad user buffer yields
    // `UserAccessError::Fault` (→ `FaultFromUser`), never a kernel halt.
    copy_slice_from_user(dst, uptr).map_err(from_user_access)?;

    // SERIAL is rank 7 (lowest); no other lock is held here. Translate `\n` → `\r\n`
    // (the terminal convention, as the kernel's own `kprint!` does) so userspace
    // output — eshell, `cat`, init — renders correctly on a real serial terminal.
    let serial = crate::arch::serial::SERIAL.lock();
    for &b in dst.iter() {
        if b == b'\n' {
            serial.write_byte(b'\r');
        }
        serial.write_byte(b);
    }
    Ok(len as isize)
}

// --- Handle operation syscalls ------------------------------------------
//
// Each public `sys_handle_*` wrapper resolves the calling process's pid and
// the global table, then defers to a pure `*_on(&HandleTable, …)` core that
// holds all the validation/refcount logic. The split keeps the global/sched
// coupling in the wrapper and lets the core be unit-tested against a local
// `HandleTable` (see the tests below). Wrappers must resolve the pid (rank-1
// lock) **before** entering the table (rank-3 lock); the two are never
// nested.

/// Map a handle-table error into the syscall error space.
///
/// `NotOwner` is collapsed to `InvalidHandle` (not `NoAccess`) for capability
/// hygiene: a handle owned by another process must be indistinguishable from
/// one that never existed, so a caller cannot probe the existence of other
/// processes' handles by observing a different error code. (The table keeps
/// the precise `NotOwner` for telemetry — see `HandleError`.)
fn map_handle_err(e: HandleError) -> KError {
    match e {
        HandleError::NullHandle | HandleError::InvalidHandle | HandleError::NotOwner => {
            KError::InvalidHandle
        }
        HandleError::NoAccess => KError::NoAccess,
        HandleError::OutOfHandles => KError::OutOfHandles,
        HandleError::OutOfMemory => KError::OutOfMemory,
        HandleError::BadRights => KError::InvalidArgument,
    }
}

/// Look up `h` for `pid`, require `required` rights, and confirm the object
/// is of type `expected`.
///
/// Collapses the lookup-then-type-check idiom shared by almost every
/// handle-taking syscall into one place, so the type-confusion check (wrong
/// `KObjectType` → `InvalidArgument`) cannot be forgotten on a new handler.
/// Returns the pinned [`LookupOk`]; its `ObjectRef` keeps the object alive
/// until the caller drops it, and `rights` carries the handle's full rights.
fn lookup_typed(
    h: u64,
    pid: u32,
    required: Rights,
    expected: KObjectType,
) -> Result<LookupOk, KError> {
    let ok = global::get()
        .lookup(RawHandle(h), pid, required)
        .map_err(map_handle_err)?;
    if ok.object.object_type() != expected {
        return Err(KError::InvalidArgument);
    }
    Ok(ok)
}

/// Core of `sys_handle_close`: close `h` in `t` on behalf of `pid`.
fn close_on(t: &HandleTable, h: RawHandle, pid: u32) -> SysResult {
    let co = t.close(h, pid).map_err(map_handle_err)?;
    // `close` transferred the handle's one reference into `co`; rebuild an
    // `ObjectRef` and drop it to release it (running the destructor if it was
    // the last). Done here — outside the table call — so object teardown
    // (rank-6 allocator) is not nested under the rank-3 handle-table lock.
    // SAFETY: `co` carries exactly the handle's one outstanding reference;
    // we account for it once.
    drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
    Ok(0)
}

/// Core of `sys_handle_duplicate`: duplicate `h` in `t` with attenuated
/// rights, returning the new handle value.
fn duplicate_on(t: &HandleTable, h: RawHandle, pid: u32, new_rights: Rights) -> SysResult {
    let dup = t.duplicate(h, pid, new_rights).map_err(map_handle_err)?;
    // A valid handle's bits are non-negative as an `isize` in Phase 1 (the
    // generation counter never reaches its top bit). See the slice's known
    // ABI-tension note in the decision log.
    Ok(dup.bits() as isize)
}

/// Core of `sys_handle_restrict`: attenuate `h`'s rights in place.
fn restrict_on(t: &HandleTable, h: RawHandle, pid: u32, new_rights: Rights) -> SysResult {
    t.restrict(h, pid, new_rights).map_err(map_handle_err)?;
    Ok(0)
}

/// Core of `sys_handle_stat`: build the user-facing `HandleInfo` for `h`.
/// Separated from the user copy-out so the metadata logic is host-testable. One
/// `INSPECT` lookup yields type/rights + the object, from which the per-type byte
/// `size` is read (object-aware logic stays here, not in the type-agnostic handle
/// table); the generation comes from the handle bits.
fn stat_on(t: &HandleTable, h: RawHandle, pid: u32) -> Result<HandleInfo, KError> {
    let ok = t.lookup(h, pid, Rights::INSPECT).map_err(map_handle_err)?;
    let object_type = ok.object.object_type();
    let (_, _, generation) = h.decode();
    let size = object_byte_size(object_type, &ok.object);
    Ok(HandleInfo::from_stat(object_type, ok.rights, generation, size))
}

/// The byte size reported in `HandleInfo.size` for a sized resource, else `0`. A
/// `MemoryObject`'s page-rounded size; a `FileObject`'s exact file size.
fn object_byte_size(ty: KObjectType, obj: &ObjectRef) -> u64 {
    match ty {
        // SAFETY: `obj` pins a live object of the matched type.
        KObjectType::MemoryObject => {
            unsafe { &*(obj.as_ptr() as *const MemoryObject) }.size() as u64
        }
        KObjectType::FileObject => unsafe { &*(obj.as_ptr() as *const FileObject) }.size() as u64,
        _ => 0,
    }
}

/// `sys_handle_close(h)` — release the caller's reference to `h`. After this
/// returns, the handle value is invalid for the caller. Requires no right
/// (authorisation is the ownership check). Returns 0.
pub fn sys_handle_close(h: u64) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    close_on(global::get(), RawHandle(h), pid)
}

/// `sys_handle_duplicate(h, new_rights)` — return a new handle to the same
/// object with rights `h.rights & new_rights`. `h` stays valid. Requires
/// `DUPLICATE` on `h`. Returns the new handle value.
pub fn sys_handle_duplicate(h: u64, new_rights: u64) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    duplicate_on(
        global::get(),
        RawHandle(h),
        pid,
        Rights::from_bits_truncate(new_rights),
    )
}

/// `sys_handle_restrict(h, new_rights)` — attenuate `h`'s rights in place to
/// `h.rights & new_rights`. Cannot amplify. Requires no right
/// (self-attenuation). Returns 0.
pub fn sys_handle_restrict(h: u64, new_rights: u64) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    restrict_on(
        global::get(),
        RawHandle(h),
        pid,
        Rights::from_bits_truncate(new_rights),
    )
}

/// `sys_handle_stat(h, out)` — write `HandleInfo` for `h` to user memory at
/// `out`. Requires `INSPECT` on `h`. Returns 0.
pub fn sys_handle_stat(h: u64, out: u64) -> SysResult {
    // Validate the user pointer first: a bad pointer is a cheap,
    // side-effect-free failure that never churns an object reference.
    let uptr = UserMutPtr::<HandleInfo>::new(out).map_err(from_user_access)?;
    let pid = crate::sched::current_owner_pid();
    let info = stat_on(global::get(), RawHandle(h), pid)?;
    copy_to_user(uptr, &info).map_err(from_user_access)?;
    Ok(0)
}

// --- Memory object syscalls ---------------------------------------------

/// Rights minted on a fresh `MemoryObject` handle: the full map band plus the
/// generic rights that let the owner duplicate, inspect, and transfer it.
///
/// Principal `READ`/`WRITE`/`EXECUTE` are deliberately excluded — only the
/// `MAP_*` band is valid on a `MemoryObject` (see `handle/type_rights.rs`), so
/// including them would make `allocate` reject the set as `BadRights`.
fn full_mem_rights() -> Rights {
    Rights::MAP_READ
        | Rights::MAP_WRITE
        | Rights::MAP_EXEC
        | Rights::DUPLICATE
        | Rights::INSPECT
        | Rights::TRANSFER
}

/// Map an [`AddressSpace`] mapping failure into the syscall error space.
fn map_mem_map_err(e: MapError) -> KError {
    match e {
        MapError::Overlap | MapError::NotCanonical | MapError::NotUserHalf => {
            KError::InvalidArgument
        }
        MapError::OutOfMemory => KError::OutOfMemory,
    }
}

/// Round `n` up to a whole number of pages, saturating rather than wrapping
/// (a near-`u64::MAX` request rounds to a huge value the size checks reject).
fn round_up_page(n: u64) -> u64 {
    n.saturating_add(PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1)
}

/// Borrow the calling process's address space through its `ObjectRef`. The
/// returned reference is tied to `proc_ref`, which the caller holds for the
/// syscall's duration (keeping the process and its address space alive).
fn current_address_space(proc_ref: &ObjectRef) -> Option<&AddressSpace> {
    debug_assert_eq!(proc_ref.object_type(), KObjectType::Process);
    // SAFETY: `proc_ref` references a live `Process` (its `KObjectHeader` is at
    // offset 0), pinned by the current user thread for the syscall's duration.
    // The returned borrow is tied to `proc_ref`'s lifetime.
    let proc: &Process = unsafe { &*(proc_ref.as_ptr() as *const Process) };
    proc.address_space()
}

/// `sys_memory_create(size, flags)` — allocate a zero-filled `MemoryObject` of
/// `size` bytes (rounded up to a page) and return a handle to it (full rights).
/// `flags` must be a valid [`MemFlags`] (no flags defined yet → must be 0).
pub fn sys_memory_create(size: u64, flags: u64) -> SysResult {
    if MemFlags::from_bits(flags).is_none() {
        return Err(KError::InvalidArgument);
    }
    if size == 0 {
        return Err(KError::InvalidArgument);
    }
    if size as usize > MemoryObject::MAX_SIZE {
        return Err(KError::TooLarge);
    }
    let obj = MemoryObject::try_new(size as usize).map_err(|_| KError::OutOfMemory)?;
    let ptr = KBox::into_raw(obj).as_ptr() as *mut ();
    let pid = crate::sched::current_owner_pid();
    match global::get().allocate(pid, ptr, KObjectType::MemoryObject, full_mem_rights()) {
        Ok(h) => Ok(h.bits() as isize),
        Err(e) => {
            // `allocate` did not adopt the creation reference; reclaim and drop
            // it (running `MemoryObject::Drop`, freeing the frames). Done
            // outside the table call so teardown isn't nested under rank-3.
            // SAFETY: `ptr` carries the single outstanding creation reference.
            drop(unsafe { ObjectRef::from_raw(ptr, KObjectType::MemoryObject) });
            Err(map_handle_err(e))
        }
    }
}

/// `sys_memory_map(obj, hint, size, rights)` — map `obj`'s frames into the
/// calling process's address space with `rights` (a subset of the `MAP_*`
/// band). `hint == 0` picks any free range; otherwise `hint` is the requested
/// (page-aligned) base. Returns the mapped base virtual address.
pub fn sys_memory_map(obj_h: u64, hint: u64, size: u64, rights: u64) -> SysResult {
    let req = Rights::from_bits_truncate(rights);
    // Require the handle to actually carry the MAP_* bits being installed — a
    // lookup with `required` rejects amplification (e.g. mapping writable
    // without `MAP_WRITE`) as `NoAccess`.
    let required = req & (Rights::MAP_READ | Rights::MAP_WRITE | Rights::MAP_EXEC);

    let pid = crate::sched::current_owner_pid();
    let proc_ref = crate::sched::current_process().ok_or(KError::KernelError)?;
    let asp = current_address_space(&proc_ref).ok_or(KError::KernelError)?;

    // A mappable handle is either an (eager) `MemoryObject` or a (lazy, demand-
    // paged) `FileObject`; both accept the `MAP_*` rights band. Look the handle up
    // generically — a non-mappable type lacks the `MAP_*` right `required`, so it
    // fails here — then branch on the object type for the size cap + the mapping.
    let ok = global::get()
        .lookup(RawHandle(obj_h), pid, required)
        .map_err(map_handle_err)?;
    let obj_ty = ok.object.object_type();
    // The object's mappable extent in bytes (page-rounded). The size requested
    // must not exceed it.
    let max_bytes = match obj_ty {
        // SAFETY: type-confirmed live object pinned by `lookup`; read its size.
        KObjectType::MemoryObject => unsafe { &*(ok.object.as_ptr() as *const MemoryObject) }.size(),
        KObjectType::FileObject => {
            unsafe { &*(ok.object.as_ptr() as *const FileObject) }.npages() * PAGE_SIZE
        }
        // Not a mappable object (and it should not have reached here without a
        // `MAP_*` right, which only Memory/File objects carry).
        _ => return Err(KError::InvalidHandle),
    };

    let size = round_up_page(size);
    if size == 0 || size as usize > max_bytes {
        return Err(KError::InvalidArgument);
    }

    let range = if hint == 0 {
        asp.find_free_range(size).ok_or(KError::OutOfMemory)?
    } else {
        let start = VirtAddr::new(hint);
        if !start.is_page_aligned() {
            return Err(KError::InvalidArgument);
        }
        let end = hint.checked_add(size).ok_or(KError::InvalidArgument)?;
        if end > USER_VIRT_END {
            return Err(KError::InvalidArgument);
        }
        VAddrRange::new(start, VirtAddr::new(end)).ok_or(KError::InvalidArgument)?
    };

    // Build the protection from the requested rights. USER is always set; the
    // bits can never exceed the handle's (lookup confirmed `required` ⊆ rights).
    let mut prot = Protection::USER;
    if req.contains(Rights::MAP_WRITE) {
        prot = prot | Protection::WRITE;
    }
    if req.contains(Rights::MAP_EXEC) {
        prot = prot | Protection::EXEC;
    }

    // Move the looked-up reference into the mapping. A `MemoryObject` maps eagerly
    // (`map_object` installs `range.pages()` PTEs against the object's frames); a
    // `FileObject` maps **lazily** (`map_file` reserves the range with no PTEs —
    // `fault_in` pages each in from the file's cache on first touch).
    let result = match obj_ty {
        KObjectType::FileObject => asp.map_file(range, prot, ok.object),
        // MemoryObject (the only other type that reached here).
        _ => asp.map_object(range, prot, ok.object),
    };
    match result {
        Ok(()) => {
            // The calling process's AS is active; make any new PTEs visible. (For a
            // FileBacked map there are none yet, but flushing is harmless + uniform.)
            // SAFETY: ring-0 TLB flush; reloads the active root with itself.
            unsafe { Paging::flush_tlb_all() };
            Ok(range.start().as_u64() as isize)
        }
        Err((returned, e)) => {
            drop(returned);
            Err(map_mem_map_err(e))
        }
    }
}

/// `sys_memory_unmap(addr, size)` — unmap the region at `addr` from the
/// calling process's address space. Phase 1 unmaps the **whole** VMA covering
/// `addr` (the `size` argument is not yet honored for partial/split unmaps —
/// TODO(mm)). Returns 0, or `InvalidArgument` if nothing is mapped at `addr`.
pub fn sys_memory_unmap(addr: u64, _size: u64) -> SysResult {
    let va = VirtAddr::new(addr);
    if !va.is_page_aligned() || addr >= USER_VIRT_END {
        return Err(KError::InvalidArgument);
    }
    let proc_ref = crate::sched::current_process().ok_or(KError::KernelError)?;
    let asp = current_address_space(&proc_ref).ok_or(KError::KernelError)?;
    match asp.unmap_covering(va) {
        Some(_vma) => {
            // `_vma` drops here, releasing its object reference (for object
            // mappings) or freeing its anonymous frames. The AS is active;
            // flush the removed PTEs.
            // SAFETY: ring-0 TLB flush; reloads the active root with itself.
            unsafe { Paging::flush_tlb_all() };
            Ok(0)
        }
        None => Err(KError::InvalidArgument),
    }
}

// --- Clock syscall ------------------------------------------------------

/// `sys_clock_read(clock, out)` — write the selected clock's value, in
/// nanoseconds, to the user `u64` at `out`. Returns 0.
///
/// Only [`ClockId::Monotonic`] is serviced this slice; the rest return
/// `Unsupported`. The selector and pointer are validated before any clock is
/// read, so the invalid-selector and unsupported-clock paths are reachable
/// without touching user memory (host-testable).
///
/// TODO(realtime): `Realtime` needs a wall-clock offset service (none yet).
/// TODO(sched-acct): `ProcessCpu`/`ThreadCpu` need per-thread CPU accounting
/// from the scheduler tick (none yet). See docs/planning/implementation-plan.md.
pub fn sys_clock_read(clock: u64, out: u64) -> SysResult {
    let id = u32::try_from(clock)
        .ok()
        .and_then(ClockId::from_u32)
        .ok_or(KError::InvalidArgument)?;
    let uptr = UserMutPtr::<u64>::new(out).map_err(from_user_access)?;
    let ns = match id {
        ClockId::Monotonic => Timer::read_ns(),
        ClockId::Realtime | ClockId::ProcessCpu | ClockId::ThreadCpu => {
            return Err(KError::Unsupported);
        }
    };
    copy_to_user(uptr, &ns).map_err(from_user_access)?;
    Ok(0)
}

// --- Timer + wait syscalls ----------------------------------------------

/// Rights minted on a fresh `Timer` handle: `WAIT` (it is a waitable) plus the
/// generic management band. No principal rights — a `Timer`'s principal mask is
/// empty (`handle/type_rights.rs`), so this set is allocatable.
fn timer_rights() -> Rights {
    Rights::WAIT | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER
}

/// Rights for a freshly-created `PendingOperation` handle. Wait-only (its
/// principal mask is empty, like a Timer — `handle/type_rights.rs`), plus the
/// generic management band so the completion can be duplicated/transferred and
/// inspected.
fn pending_op_rights() -> Rights {
    Rights::WAIT | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER
}

/// Rights a freshly-created `Namespace` handle carries: the full namespace
/// principal set (`LOOKUP | BIND`), the `UNBIND` modifier, and the generic
/// management band (duplicate / transfer / inspect). A `Namespace` is not a
/// waitable, so no `WAIT`. (`UNBIND` is a modifier-band right, already
/// allocatable on a `Namespace` — `handle/type_rights.rs`.)
fn namespace_rights() -> Rights {
    Rights::LOOKUP
        | Rights::BIND
        | Rights::UNBIND
        | Rights::DUPLICATE
        | Rights::TRANSFER
        | Rights::INSPECT
}

/// Map a [`NsError`] to the user-facing [`KError`]. `AlreadyBound`/`InvalidPath`
/// are caller errors (`InvalidArgument`); `NotBound` is `NotFound`.
fn map_ns_err(e: NsError) -> KError {
    match e {
        NsError::InvalidPath | NsError::AlreadyBound => KError::InvalidArgument,
        NsError::NotBound => KError::NotFound,
        NsError::OutOfMemory => KError::OutOfMemory,
    }
}

/// Copy a namespace path (`path_ptr`, `path_len`) from user memory into `buf`,
/// returning the populated byte slice. `path_len` must be in `1..=NS_PATH_MAX`
/// (`InvalidArgument` / `TooLarge` otherwise). Validation of the path *grammar*
/// is the caller's job (via [`validate_path`] / `Namespace::bind`).
fn copy_ns_path<'a>(
    path_ptr: u64,
    path_len: usize,
    buf: &'a mut [u8; NS_PATH_MAX],
) -> Result<&'a [u8], KError> {
    if path_len == 0 {
        return Err(KError::InvalidArgument);
    }
    if path_len > NS_PATH_MAX {
        return Err(KError::TooLarge);
    }
    let p = UserPtr::<u8>::new(path_ptr).map_err(from_user_access)?;
    copy_slice_from_user(&mut buf[..path_len], p).map_err(from_user_access)?;
    Ok(&buf[..path_len])
}

/// `sys_timer_create(flags)` — create an unarmed `Timer` and return a handle
/// (with [`timer_rights`]). `flags` must be a valid [`TimerFlags`] (none defined
/// yet → must be 0).
pub fn sys_timer_create(flags: u64) -> SysResult {
    if TimerFlags::from_bits(flags).is_none() {
        return Err(KError::InvalidArgument);
    }
    let obj = TimerObject::try_new().map_err(|_| KError::OutOfMemory)?;
    let ptr = KBox::into_raw(obj).as_ptr() as *mut ();
    let pid = crate::sched::current_owner_pid();
    match global::get().allocate(pid, ptr, KObjectType::Timer, timer_rights()) {
        Ok(h) => Ok(h.bits() as isize),
        Err(e) => {
            // `allocate` did not adopt the creation reference; reclaim + drop it
            // (running `Timer::Drop`) outside the table call.
            // SAFETY: `ptr` carries the single outstanding creation reference.
            drop(unsafe { ObjectRef::from_raw(ptr, KObjectType::Timer) });
            Err(map_handle_err(e))
        }
    }
}

/// `sys_timer_set(timer, deadline_ns, interval_ns)` — arm `timer` to fire at the
/// absolute monotonic deadline `deadline_ns` (`0` disarms), re-arming every
/// `interval_ns` thereafter (`0` = one-shot). Returns 0. Requires only handle
/// ownership (no special right — `WAIT` gates `sys_wait`, not arming).
pub fn sys_timer_set(timer_h: u64, deadline_ns: u64, interval_ns: u64) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    let ok = lookup_typed(timer_h, pid, Rights::empty(), KObjectType::Timer)?;
    // `ok.object` (an ObjectRef) is held across the arm, keeping the Timer alive.
    crate::sched::timer_arm(ok.object.as_ptr(), deadline_ns, interval_ns)
        .map_err(|()| KError::OutOfMemory)?;
    Ok(0)
}

/// `sys_ns_create()` — create an empty [`Namespace`] kernel object and return a
/// handle carrying [`namespace_rights`]. The `sys_timer_create` pattern.
pub fn sys_ns_create() -> SysResult {
    let obj = Namespace::try_new().map_err(|_| KError::OutOfMemory)?;
    let ptr = KBox::into_raw(obj).as_ptr() as *mut ();
    let pid = crate::sched::current_owner_pid();
    match global::get().allocate(pid, ptr, KObjectType::Namespace, namespace_rights()) {
        Ok(h) => Ok(h.bits() as isize),
        Err(e) => {
            // `allocate` did not adopt the creation reference; reclaim + drop it
            // (running `Namespace::Drop`) outside the table call.
            // SAFETY: `ptr` carries the single outstanding creation reference.
            drop(unsafe { ObjectRef::from_raw(ptr, KObjectType::Namespace) });
            Err(map_handle_err(e))
        }
    }
}

/// `sys_ns_bind(ns, path, path_len, resource)` — bind `resource` (a direct
/// kernel-object handle in slice 1) at `path` in namespace `ns`. Requires the
/// `BIND` right on `ns`. The binding takes a **clone** of the resource's
/// reference (the caller's handle stays valid) and records the resource handle's
/// current rights as the binding's rights cap. Returns 0.
/// Require the calling process to hold system capability `cap`, else [`KError::NoAccess`].
/// The process-level authority gate (`docs/architecture/syscaps.md`) — checked after the
/// caller's `Process` is resolved, alongside (not instead of) any per-handle `Rights`.
fn require_syscap(cap: crate::libkern::SysCaps) -> Result<(), KError> {
    let proc = crate::sched::current_process().ok_or(KError::KernelError)?;
    // SAFETY: `proc` pins a live `Process`; read its immutable syscap set.
    let held = unsafe { &*(proc.as_ptr() as *const Process) }.syscaps();
    if held.contains(cap) {
        Ok(())
    } else {
        Err(KError::NoAccess)
    }
}

pub fn sys_ns_bind(ns_h: u64, path_ptr: u64, path_len: usize, resource_h: u64) -> SysResult {
    // Namespace construction is a supervisor-only privilege: BIND_NAMESPACE is an
    // *additional* gate atop the `BIND` handle right below (a process cannot bind even
    // into a namespace it created without it). See docs/architecture/syscaps.md.
    require_syscap(crate::libkern::SysCaps::BIND_NAMESPACE)?;
    let pid = crate::sched::current_owner_pid();
    let ns_ok = lookup_typed(ns_h, pid, Rights::BIND, KObjectType::Namespace)?;
    let mut buf = [0u8; NS_PATH_MAX];
    let path = copy_ns_path(path_ptr, path_len, &mut buf)?;
    // Resolve the resource handle (any type; ownership is the authority).
    let res_ok = global::get()
        .lookup(RawHandle(resource_h), pid, Rights::empty())
        .map_err(map_handle_err)?;
    let rights = res_ok.rights;
    // SAFETY: `ns_ok.object` addresses a live `Namespace` — `lookup_typed`
    // verified the type tag and the refcount pins it for this call.
    let ns: &Namespace = unsafe { &*(ns_ok.object.as_ptr() as *const Namespace) };

    // An `IpcChannel` resource binds as a **Userspace Server**: the kernel adopts
    // the endpoint into a registration it forwards lookups to (the supervisor gives
    // the *peer* of this endpoint to the server process). Any other type binds as a
    // direct handle (slice-1 behaviour). See
    // `docs/architecture/namespace-and-resource-servers.md`.
    if res_ok.object.object_type() == KObjectType::IpcChannel {
        // Wrap a clone of the endpoint in a registration record.
        let reg_box = UserspaceServerReg::try_new(res_ok.object.clone())
            .map_err(|_| KError::OutOfMemory)?;
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        let reg_ref = unsafe {
            ObjectRef::from_raw(
                KBox::into_raw(reg_box).as_ptr() as *mut (),
                KObjectType::UserspaceServerReg,
            )
        };
        let reg_ptr = reg_ref.as_ptr();
        return match ns.bind_userspace_server(path, reg_ref, rights) {
            Ok(()) => {
                // Now that the binding owns the registration, record the endpoint →
                // registration back-pointer so the server's reply reaches it. (Set
                // only on success: on failure no back-pointer is left dangling.)
                crate::sched::us_server_attach(res_ok.object.as_ptr(), reg_ptr);
                Ok(0)
            }
            Err((returned, e)) => {
                // Drop the handed-back registration **outside** the namespace lock;
                // its `Drop` releases the endpoint clone (no back-pointer was set).
                drop(returned);
                Err(map_ns_err(e))
            }
        };
    }

    // Direct-handle bind: clone the resource for the binding; the binding's rights
    // are the handle's rights.
    let target = res_ok.object.clone();
    match ns.bind(path, target, rights) {
        Ok(()) => Ok(0),
        Err((returned, e)) => {
            // Drop the handed-back reference **outside** the namespace lock
            // (`bind` released it before returning).
            drop(returned);
            Err(map_ns_err(e))
        }
    }
    // `res_ok` / `ns_ok` drop here, releasing the lookup references outside any
    // namespace lock (the caller's resource + namespace handles still pin them).
}

/// `sys_ns_unbind(ns, path, path_len)` — remove the binding at `path` in `ns`.
/// Requires the `UNBIND` right on `ns`. `NotFound` if nothing is bound there.
/// Returns 0.
pub fn sys_ns_unbind(ns_h: u64, path_ptr: u64, path_len: usize) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    let ns_ok = lookup_typed(ns_h, pid, Rights::UNBIND, KObjectType::Namespace)?;
    let mut buf = [0u8; NS_PATH_MAX];
    let path = copy_ns_path(path_ptr, path_len, &mut buf)?;
    // SAFETY: live `Namespace` (type verified by `lookup_typed`).
    let ns: &Namespace = unsafe { &*(ns_ok.object.as_ptr() as *const Namespace) };
    match ns.unbind(path) {
        // Drop the removed binding's target outside the lock (`unbind` returned it
        // for exactly this): a `DirectHandle` releases its `ObjectRef` here; a
        // `KernelServer` target is drop-free.
        Some(target) => {
            drop(target);
            Ok(0)
        }
        None => Err(KError::NotFound),
    }
}

/// `sys_ns_enumerate(ns, index, out)` — write the `index`-th binding of namespace
/// `ns` (insertion order) to the user [`NsEntry`] at `out`: its path, target kind
/// (`NS_KIND_*`), and rights. Requires `LOOKUP` on `ns`. Returns `0`, or `NotFound`
/// when `index` is past the binding count (the iteration terminator). Lists a
/// namespace's **bindings** (mount points + kernel resources) — not filesystem
/// `readdir`. (Syscall number `30`.)
pub fn sys_ns_enumerate(ns_h: u64, index: u64, out: u64) -> SysResult {
    // Validate the user pointer first (cheap, side-effect-free).
    let uptr = UserMutPtr::<NsEntry>::new(out).map_err(from_user_access)?;
    let pid = crate::sched::current_owner_pid();
    let ns_ok = lookup_typed(ns_h, pid, Rights::LOOKUP, KObjectType::Namespace)?;
    // SAFETY: live `Namespace` (type verified by `lookup_typed`).
    let ns: &Namespace = unsafe { &*(ns_ok.object.as_ptr() as *const Namespace) };
    let mut entry = NsEntry::zeroed();
    // Fill the entry under the namespace lock; copy out below, outside it.
    if !ns.enumerate(index as usize, &mut entry) {
        return Err(KError::NotFound);
    }
    copy_to_user(uptr, &entry).map_err(from_user_access)?;
    Ok(0)
}

/// Per-call cap on `sys_entropy_read`. Entropy reads are small (seeds, keys,
/// nonces); userspace loops for more. Bounds the on-stack bounce buffer.
const ENTROPY_READ_MAX: usize = 256;

/// Rights minted on a fresh `EntropyObject` handle: `READ` (the principal right)
/// plus the generic management band. Not waitable (the `PendingOperation` is the
/// wait primitive for the unseeded path), so no `WAIT`.
fn entropy_rights() -> Rights {
    Rights::READ | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER
}

/// `sys_entropy_create()` — create an [`EntropyObject`] capability token onto the
/// kernel CSPRNG and return a handle carrying [`entropy_rights`]. The source is the
/// global singleton; every token reads from it. The `sys_ns_create` pattern.
pub fn sys_entropy_create() -> SysResult {
    let obj = EntropyObject::try_new().map_err(|_| KError::OutOfMemory)?;
    let ptr = KBox::into_raw(obj).as_ptr() as *mut ();
    let pid = crate::sched::current_owner_pid();
    match global::get().allocate(pid, ptr, KObjectType::EntropyObject, entropy_rights()) {
        Ok(h) => Ok(h.bits() as isize),
        Err(e) => {
            // `allocate` did not adopt the creation reference; reclaim + drop it.
            // SAFETY: `ptr` carries the single outstanding creation reference.
            drop(unsafe { ObjectRef::from_raw(ptr, KObjectType::EntropyObject) });
            Err(map_handle_err(e))
        }
    }
}

/// `sys_entropy_read(ent, buf, len)` — fill `buf[0..len]` with CSPRNG output.
/// Requires `READ` on `ent`. `len` is capped at [`ENTROPY_READ_MAX`].
///
/// **Return contract:** `0` = the buffer was filled synchronously (the common case
/// — the pool seeds at boot, before userspace). A **positive** value is a
/// [`PendingOperation`] handle: the pool is not yet seeded (only on hardware
/// without `RDSEED`/`RDRAND`), so the caller `sys_wait`s on it and **re-reads** once
/// it completes. A negative value is a [`KError`]. Unambiguous: handles are ≥ 1.
pub fn sys_entropy_read(ent_h: u64, buf_ptr: u64, len: usize) -> SysResult {
    // Synchronous validation (no PO created on these).
    if len == 0 {
        return Ok(0);
    }
    if len > ENTROPY_READ_MAX {
        return Err(KError::TooLarge);
    }
    let pid = crate::sched::current_owner_pid();
    let _ent = lookup_typed(ent_h, pid, Rights::READ, KObjectType::EntropyObject)?;
    let uptr = UserMutPtr::<u8>::new(buf_ptr).map_err(from_user_access)?;

    if crate::entropy::is_seeded() {
        // Fill from the CSPRNG, copy out, then wipe the bounce buffer so no entropy
        // lingers on the kernel stack for a later frame to observe.
        let mut buf = [0u8; ENTROPY_READ_MAX];
        crate::entropy::fill(&mut buf[..len]);
        let r = copy_slice_to_user(uptr, &buf[..len]).map_err(from_user_access);
        buf.iter_mut().for_each(|b| *b = 0);
        r?;
        return Ok(0);
    }

    // Unseeded: hand back a PendingOperation completed when the pool seeds. Create
    // the PO + handle (one ref to the handle, one cloned for the seed-waiter list).
    let po = PendingOperation::try_new().map_err(|_| KError::OutOfMemory)?;
    let po_ref = {
        let ptr = KBox::into_raw(po).as_ptr() as *mut ();
        // SAFETY: `into_raw` yielded the single creation reference; adopt it.
        unsafe { ObjectRef::from_raw(ptr, KObjectType::PendingOperation) }
    };
    let waiter_ref = po_ref.clone();
    let (pp, pt) = po_ref.into_raw();
    let po_h = match global::get().allocate(pid, pp, pt, pending_op_rights()) {
        Ok(h) => h,
        Err(e) => {
            // SAFETY: `allocate` did not adopt the reference; reclaim it. Drop the
            // clone too — both outside any lock.
            drop(unsafe { ObjectRef::from_raw(pp, pt) });
            drop(waiter_ref);
            return Err(map_handle_err(e));
        }
    };
    match crate::entropy::register_seed_waiter(waiter_ref) {
        crate::entropy::SeedWaitReg::Queued => {}
        crate::entropy::SeedWaitReg::AlreadySeeded(r) => {
            // Seeded between the check and the register: complete now so the wait
            // returns at once and the caller re-reads (→ synchronous fill).
            crate::sched::complete_pending_op(r.as_ptr(), 0, 0);
            drop(r);
        }
        crate::entropy::SeedWaitReg::Full(r) => {
            // Too many concurrent unseeded readers (pathological): tell the caller
            // to retry. `WouldBlock` is the negative status it reads from the PO.
            crate::sched::complete_pending_op(r.as_ptr(), KError::WouldBlock as i32, 0);
            drop(r);
        }
    }
    Ok(po_h.bits() as isize)
}

/// `sys_ns_lookup(ns, path, path_len, rights)` — resolve `path` in `ns`
/// (longest-prefix match), requesting at most `rights`. Requires `LOOKUP` on
/// `ns`. Returns a [`PendingOperation`] handle; the completion delivers the
/// resolved resource handle in `IoResult.result` (rights = `requested ∩
/// binding.rights`) with `status == 0`, or a `NotFound` `status`. A slice-1
/// direct-handle binding resolves entirely in this syscall context, so the PO is
/// **pre-signalled** and the caller's `sys_wait` returns immediately — the full
/// async result path, exercised before any resource server exists.
///
/// Argument / permission / allocation failures return **synchronously** (a
/// negative isize, no PO). Resolution failures (no covering binding, or a
/// non-empty suffix on a direct-handle leaf) are delivered **through** the PO.
pub fn sys_ns_lookup(ns_h: u64, path_ptr: u64, path_len: usize, rights_bits: u64) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    // --- synchronous validation (no PO created on these) ---
    let requested = Rights::from_bits_truncate(rights_bits);
    let ns_ok = lookup_typed(ns_h, pid, Rights::LOOKUP, KObjectType::Namespace)?;
    let mut buf = [0u8; NS_PATH_MAX];
    let path = copy_ns_path(path_ptr, path_len, &mut buf)?;
    validate_path(path).map_err(|_| KError::InvalidArgument)?;

    // Create the PO and its handle FIRST, so every *resolution* outcome (success
    // or not-found) is delivered through the PO; only the pre-PO failures above
    // return synchronously. Keep a kernel-held `po_ref` (a clone of the handle's
    // reference): the forwarding path needs it to pin the PO across the async gap
    // until the server's reply completes it.
    let po = PendingOperation::try_new().map_err(|_| KError::OutOfMemory)?;
    // SAFETY: `into_raw` yields the single creation reference; adopt it.
    let po_ref = unsafe {
        ObjectRef::from_raw(
            KBox::into_raw(po).as_ptr() as *mut (),
            KObjectType::PendingOperation,
        )
    };
    let po_ptr = po_ref.as_ptr();
    let (pp, pt) = po_ref.clone().into_raw();
    let po_h = match global::get().allocate(pid, pp, pt, pending_op_rights()) {
        Ok(h) => h,
        Err(e) => {
            // SAFETY: `allocate` did not adopt the clone; reclaim it. `po_ref` then
            // drops at return, destroying the PO (no waiters yet — its assert holds).
            drop(unsafe { ObjectRef::from_raw(pp, pt) });
            return Err(map_handle_err(e));
        }
    };

    // --- resolve + install (failures delivered via the PO) ---
    // SAFETY: live `Namespace` (type verified by `lookup_typed`).
    let ns: &Namespace = unsafe { &*(ns_ok.object.as_ptr() as *const Namespace) };

    // Install an `obj` into the caller's table with `requested ∩ binding_rights`,
    // returning the `(status, result)` pair the PO is signalled with. Shared by the
    // direct-handle and kernel-server-`Completed` paths.
    let install = |obj: ObjectRef, binding_rights: Rights| -> (i32, u64) {
        let attenuated = requested & binding_rights;
        // Hand `allocate` the reference via `into_raw`.
        let (tptr, tty) = obj.into_raw();
        match global::get().allocate(pid, tptr, tty, attenuated) {
            Ok(h) => (0, h.bits()),
            Err(e) => {
                // SAFETY: `allocate` did not adopt the reference; reclaim it.
                drop(unsafe { ObjectRef::from_raw(tptr, tty) });
                (map_handle_err(e) as i32, 0)
            }
        }
    };

    // The resolution outcome: `Some((status, result))` completes the PO now (the
    // synchronous direct-handle / kernel-server paths); `None` leaves it **pending**
    // (a forwarded userspace lookup — the server's reply completes it later).
    let outcome: Option<(i32, u64)> = match ns.resolve(path) {
        None => Some((KError::NotFound as i32, 0)),
        Some((ResolvedTarget::DirectHandle(target), binding_rights, suffix)) => {
            if suffix.is_empty() {
                Some(install(target, binding_rights))
            } else {
                // Direct-handle leaf: a non-empty suffix has no sub-resource.
                drop(target);
                Some((KError::NotFound as i32, 0))
            }
        }
        Some((ResolvedTarget::KernelServer(id), binding_rights, suffix)) => {
            // Call the in-kernel server in this syscall context; it produces a
            // handle (or rejects). Suffix interpretation (leaf vs. subtree) is the
            // server's policy. The result still flows through the pre-signalled PO.
            match kernel_server::dispatch(id, suffix, requested) {
                OpStatus::Completed(obj) => Some(install(obj, binding_rights)),
                OpStatus::Rejected(err) => Some((err as i32, 0)),
                OpStatus::Pending => {
                    // A Kernel Server never forwards; treat the impossible as an
                    // internal error rather than panicking a syscall.
                    debug_assert!(false, "kernel-server dispatch returned Pending");
                    Some((KError::KernelError as i32, 0))
                }
            }
        }
        Some((ResolvedTarget::UserspaceServer(reg), _binding_rights, suffix)) => {
            // Forward the lookup to the userspace server over IPC and leave the PO
            // pending (the reply completes it inline — see `sys_channel_send`). A
            // synchronous failure (server busy / full / gone) completes it now.
            forward_userspace_lookup(reg, &po_ref, pid, requested, suffix)
        }
    };
    match outcome {
        // Pre-signal the PO with the outcome (no waiters yet → just records it).
        // `ns_ok` / `po_ref` are still held here (refcounts, no lock), so this —
        // which takes `SCHED` — performs no `ObjectRef` drop under the lock.
        Some((status, result)) => crate::sched::complete_pending_op(po_ptr, status, result),
        // Left pending: the forwarded request was delivered; the server's reply
        // completes the PO. `po_ref` drops at return (the forwarding path cloned
        // its own reference into the registration's pending-lookup table).
        None => {}
    }
    Ok(po_h.bits() as isize)
}

/// Forward a namespace lookup that resolved to a [`UserspaceServer`] binding to
/// its server process over IPC: build the `Namespace::Resolve` request, originate
/// it (recording the lookup in the registration's pending table), and report
/// whether the lookup PO was left **pending** (`None`) or must be completed now
/// with an error (`Some((status, 0))`). `reg` is the resolved registration
/// reference (dropped here, outside the namespace lock); `po_ref` pins the lookup
/// PO; `suffix` is the path past the mount prefix.
///
/// [`UserspaceServer`]: crate::object::namespace::ResolvedTarget::UserspaceServer
fn forward_userspace_lookup(
    reg: ObjectRef,
    po_ref: &ObjectRef,
    pid: u32,
    requested: Rights,
    suffix: &[u8],
) -> Option<(i32, u64)> {
    // Build the request in a heap-bounced message (4 KiB — never on the stack).
    let mut msg = match KBox::try_new(StoredMsg::zeroed()) {
        Ok(m) => m,
        Err(_) => return Some((KError::OutOfMemory as i32, 0)),
    };
    let body_len = match crate::rsproto::build_resolve_request(
        &mut msg.payload,
        requested.bits(),
        suffix,
    ) {
        Some(n) => n,
        // The suffix is longer than the rsproto request can carry.
        None => return Some((KError::TooLarge as i32, 0)),
    };
    msg.header.payload_len = body_len as u32;
    msg.header.handle_count = 0;

    // Originate (assigns the request id, records the pending lookup + its suffix,
    // sends). The suffix is stored so a lazy `FILE` reply can name the file in the
    // page-cache producer.
    match crate::sched::us_forward_originate(reg.as_ptr(), &mut msg, po_ref, pid, requested, suffix)
    {
        crate::sched::ForwardOutcome::Pending => None,
        crate::sched::ForwardOutcome::Busy | crate::sched::ForwardOutcome::Full => {
            Some((KError::WouldBlock as i32, 0))
        }
        crate::sched::ForwardOutcome::PeerClosed => Some((KError::PeerClosed as i32, 0)),
    }
    // `reg` / `msg` drop here, outside the namespace lock.
}

/// `sys_io_submit(resource, op)` — initiate the [`IoOp`] `*op` against
/// `resource` (a block [`DeviceNode`](crate::object::DeviceNode)) and return a
/// `PendingOperation` handle. Never blocks: the operation completes
/// asynchronously and its outcome (status + bytes transferred) is delivered
/// through the PO. Argument / permission / allocation failures return a negative
/// `KError` synchronously with no PO; device/medium failures arrive through the
/// PO (see `docs/spec/io-operation.md`). (Syscall number `28`.)
pub fn sys_io_submit(resource_h: u64, op_ptr: u64) -> SysResult {
    let pid = crate::sched::current_owner_pid();

    // Read + decode the descriptor (host-reachable failures, no PO).
    let optr = UserPtr::<IoOp>::new(op_ptr).map_err(from_user_access)?;
    let op = copy_from_user(optr).map_err(from_user_access)?;
    if op.flags != 0 {
        return Err(KError::InvalidArgument);
    }
    let opcode = IoOpcode::from_u32(op.opcode).ok_or(KError::InvalidArgument)?;

    // Resolve the resource (a block device) and buffer with opcode-appropriate
    // rights: a read writes into the buffer (needs MAP_WRITE) and reads the
    // device (needs READ); a write is the mirror.
    let (dev_right, buf_right) = match opcode {
        IoOpcode::Read => (Rights::READ, Rights::MAP_WRITE),
        IoOpcode::Write => (Rights::WRITE, Rights::MAP_READ),
    };
    let dev_ok = lookup_typed(resource_h, pid, dev_right, KObjectType::DeviceNode)?;
    let buf_ok = lookup_typed(op.buffer, pid, buf_right, KObjectType::MemoryObject)?;

    // Bounds-check the buffer range (sync error).
    // SAFETY: `buf_ok` pins a live `MemoryObject`.
    let buffer_size = unsafe { &*(buf_ok.object.as_ptr() as *const MemoryObject) }.size() as u64;
    let buf_end = op
        .buf_offset
        .checked_add(op.length)
        .ok_or(KError::InvalidArgument)?;
    if buf_end > buffer_size {
        return Err(KError::InvalidArgument);
    }

    // Create the PO + handle (mirrors `sys_ns_lookup`): keep one reference for
    // the IRP dispatch, hand one to the caller's table.
    let po_box = PendingOperation::try_new().map_err(|_| KError::OutOfMemory)?;
    // SAFETY: adopt the single creation reference.
    let po_ref = unsafe {
        ObjectRef::from_raw(
            KBox::into_raw(po_box).as_ptr() as *mut (),
            KObjectType::PendingOperation,
        )
    };
    let (tptr, tty) = po_ref.clone().into_raw();
    let po_h = match global::get().allocate(pid, tptr, tty, pending_op_rights()) {
        Ok(h) => h,
        Err(e) => {
            // SAFETY: `allocate` did not adopt this reference; reclaim it. Then
            // `po_ref` drops at return, destroying the PO.
            drop(unsafe { ObjectRef::from_raw(tptr, tty) });
            return Err(map_handle_err(e));
        }
    };

    // A zero-length request is a legal no-op: pre-signal the PO and return.
    if op.length == 0 {
        crate::sched::complete_pending_op(po_ref.as_ptr(), 0, 0);
        return Ok(po_h.bits() as isize);
    }

    // Dispatch by device class. A block device goes through the IRP path; the
    // console (a char device) goes through its stream-read backend (no block
    // alignment, no device offset). On failure, roll back the PO handle (and
    // `po_ref` drops at return → PO destroyed).
    // SAFETY: `dev_ok.object` pins a live `DeviceNode` (type-checked above).
    let dn: &DeviceNode = unsafe { &*(dev_ok.object.as_ptr() as *const DeviceNode) };
    let dispatched: Result<(), KError> = match dn.class() {
        DeviceClass::Block => crate::io::block::dispatch_block_irp(
            &dev_ok.object,
            &buf_ok.object,
            &po_ref,
            opcode,
            op.offset,
            op.buf_offset,
            op.length,
        ),
        // The console: stream Read only (input). Write/Other are not supported yet.
        DeviceClass::Char if opcode == IoOpcode::Read => match dn.char_backend() {
            Some(backend) => (backend.submit_read)(
                &buf_ok.object,
                &po_ref,
                op.buf_offset,
                op.length,
                backend.ctx,
            ),
            None => Err(KError::Unsupported),
        },
        DeviceClass::Char | DeviceClass::Other => Err(KError::Unsupported),
    };
    match dispatched {
        Ok(()) => Ok(po_h.bits() as isize),
        Err(e) => {
            close_and_release(po_h, pid);
            Err(e)
        }
    }
}

/// `sys_io_cancel(pending)` — request cancellation of an in-flight operation.
/// **Phase 2: `Unsupported`** (IRP cancellation is deferred; the number is
/// reserved). (Syscall number `29`.)
pub fn sys_io_cancel(_pending_h: u64) -> SysResult {
    Err(KError::Unsupported)
}

/// `sys_wait(handles, count, results, deadline)` — block until ≥1 of
/// `handles[0..count]` signals or `deadline` (absolute monotonic ns; `0` =
/// poll, `u64::MAX` = no timeout) elapses. Writes one [`IoResult`] per signaled
/// handle to `results` and returns the count signaled (≥1), or `TimedOut` /
/// `WouldBlock`. This slice only supports `Timer` handles (others →
/// `Unsupported`).
pub fn sys_wait(handles_ptr: u64, count: usize, results_ptr: u64, deadline_ns: u64) -> SysResult {
    // Validate count + pointers before any lock (host-reachable failures).
    if count == 0 {
        return Err(KError::InvalidArgument);
    }
    if count > MAX_WAIT_HANDLES {
        return Err(KError::TooLarge);
    }
    let hptr = UserPtr::<u8>::new(handles_ptr).map_err(from_user_access)?;
    let rptr = UserMutPtr::<u8>::new(results_ptr).map_err(from_user_access)?;

    // Copy in the handle array (count × 8 bytes) and decode.
    let mut hbytes = [0u8; MAX_WAIT_HANDLES * 8];
    copy_slice_from_user(&mut hbytes[..count * 8], hptr).map_err(from_user_access)?;
    let mut handles = [0u64; MAX_WAIT_HANDLES];
    for i in 0..count {
        handles[i] = u64::from_ne_bytes(hbytes[i * 8..i * 8 + 8].try_into().unwrap());
    }

    // Resolve each handle (requires `WAIT`) to an ObjectRef held for the call.
    let pid = crate::sched::current_owner_pid();
    let mut refs: [Option<ObjectRef>; MAX_WAIT_HANDLES] = core::array::from_fn(|_| None);
    let mut objs = [0usize; MAX_WAIT_HANDLES];
    let mut types = [KObjectType::Invalid; MAX_WAIT_HANDLES];
    for i in 0..count {
        let ok = global::get()
            .lookup(RawHandle(handles[i]), pid, Rights::WAIT)
            .map_err(map_handle_err)?;
        match ok.object.object_type() {
            KObjectType::Timer
            | KObjectType::NotificationChannel
            | KObjectType::IpcChannel
            | KObjectType::PendingOperation
            | KObjectType::InterruptObject => {}
            _ => return Err(KError::Unsupported),
        }
        types[i] = ok.object.object_type();
        objs[i] = ok.object.as_ptr() as usize;
        refs[i] = Some(ok.object);
    }

    // Block (or fast-path). `refs` stay alive across the block (they live on
    // this thread's kernel stack, pinned while it is parked), keeping the
    // waited Timers alive for the wakeup path.
    let now = Timer::read_ns();
    let result = crate::sched::wait_on(&objs[..count], deadline_ns, now);
    drop(refs); // release the extra lookup references (handles still pin them)

    match result {
        crate::sched::WaitResult::Signaled(bits) => {
            let mut out = [IoResult::ready(0); MAX_WAIT_HANDLES];
            let mut k = 0usize;
            for i in 0..count {
                if bits[i] {
                    // A `PendingOperation` reports its completion status and
                    // result payload (a namespace lookup's resolved handle); the
                    // edge-style waitables (Timer/channel/notification) are an
                    // unconditional "ready" (status 0, no payload). The handle
                    // still pins the object, so reading the (one-shot, now-stable)
                    // completion is sound.
                    out[k] = if types[i] == KObjectType::PendingOperation {
                        let (status, result) =
                            crate::sched::pending_op_completion(objs[i] as *mut ());
                        IoResult::completed_with_result(handles[i], status, result)
                    } else {
                        // An `InterruptObject` consumes one pending interrupt on
                        // return, so a driver's wait→service→wait loop wakes once
                        // per interrupt; the edge-style waitables are an
                        // unconditional "ready".
                        if types[i] == KObjectType::InterruptObject {
                            crate::sched::interrupt_consume(objs[i] as *mut ());
                        }
                        IoResult::ready(handles[i])
                    };
                    k += 1;
                }
            }
            // SAFETY: `IoResult` is `#[repr(C)]` with every byte initialised and
            // no padding (asserted in `libkern::io_result`); reinterpret the
            // first `k` records as a byte slice to copy out.
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    out.as_ptr() as *const u8,
                    k * core::mem::size_of::<IoResult>(),
                )
            };
            copy_slice_to_user(rptr, bytes).map_err(from_user_access)?;
            Ok(k as isize)
        }
        crate::sched::WaitResult::TimedOut => {
            if deadline_ns == 0 {
                Err(KError::WouldBlock) // poll found nothing ready
            } else {
                Err(KError::TimedOut)
            }
        }
        crate::sched::WaitResult::OutOfMemory => Err(KError::OutOfMemory),
    }
}

/// `sys_notif_recv(channel, out)` — copy the oldest notification on `channel`
/// to the 64-byte [`Notification`] at `out`, remove it, return 0. `WouldBlock`
/// if the queue is empty. The handle must reference a `NotificationChannel`.
///
/// Receiving is gated by handle ownership (the lookup enforces
/// `owner_pid == caller`), not by a right — `WAIT` gates *blocking* on the
/// channel via `sys_wait`, not draining it.
pub fn sys_notif_recv(channel_h: u64, out: u64) -> SysResult {
    // Validate the user pointer first (cheap, side-effect-free; host-reachable).
    let uptr = UserMutPtr::<Notification>::new(out).map_err(from_user_access)?;
    let pid = crate::sched::current_owner_pid();
    let ok = lookup_typed(channel_h, pid, Rights::empty(), KObjectType::NotificationChannel)?;
    // Pop under SCHED into a kernel-local notification; `ok.object` (held here)
    // pins the channel across the pop. Copy out AFTER the lock is released
    // (never hold the IrqSpinLock across a faulting user copy).
    let n = crate::sched::notif_try_recv(ok.object.as_ptr()).ok_or(KError::WouldBlock)?;
    copy_to_user(uptr, &n).map_err(from_user_access)?;
    Ok(0)
}

/// The full rights a freshly created channel endpoint carries
/// (`docs/spec/ipc-message-format.md` § "Channel creation"). The creator
/// attenuates before handing an endpoint to another party.
fn channel_endpoint_rights() -> Rights {
    Rights::SEND
        | Rights::RECV
        | Rights::DUPLICATE
        | Rights::TRANSFER
        | Rights::INSPECT
        | Rights::WAIT
}

/// Close a handle the kernel just allocated and release the reference it held —
/// the rollback primitive for `sys_channel_create`'s partial-failure paths.
fn close_and_release(h: RawHandle, pid: u32) {
    if let Ok(co) = global::get().close(h, pid) {
        // SAFETY: `close` transferred one reference into the token without
        // decrementing; adopt it into an `ObjectRef` and drop to release.
        drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
    }
}

/// `sys_channel_create(end0, end1, queue_depth)` — create a bidirectional IPC
/// channel and write its two endpoint handles to `*end0` / `*end1`. Each
/// direction gets a `queue_depth`-slot ring (`0` → default; `> IPC_MAX_QUEUE_DEPTH`
/// rejected). Both endpoints carry [`channel_endpoint_rights`].
pub fn sys_channel_create(end0: u64, end1: u64, queue_depth: u64) -> SysResult {
    // Validate the two out-pointers + depth before allocating (host-reachable).
    let e0 = UserMutPtr::<RawHandle>::new(end0).map_err(from_user_access)?;
    let e1 = UserMutPtr::<RawHandle>::new(end1).map_err(from_user_access)?;
    let depth = if queue_depth == 0 {
        IPC_DEFAULT_QUEUE_DEPTH
    } else if queue_depth > IPC_MAX_QUEUE_DEPTH as u64 {
        return Err(KError::InvalidArgument);
    } else {
        queue_depth as u32
    };

    let pid = crate::sched::current_owner_pid();
    let (a, b) = IpcChannel::try_new_pair(depth).map_err(|_| KError::OutOfMemory)?;
    let a_ptr = KBox::into_raw(a).as_ptr() as *mut ();
    let b_ptr = KBox::into_raw(b).as_ptr() as *mut ();
    let rights = channel_endpoint_rights();

    // Install endpoint 0. On failure, reclaim both creation references.
    let h0 = match global::get().allocate(pid, a_ptr, KObjectType::IpcChannel, rights) {
        Ok(h) => h,
        Err(e) => {
            // SAFETY: each pointer still owns its single creation reference.
            drop(unsafe { ObjectRef::from_raw(a_ptr, KObjectType::IpcChannel) });
            drop(unsafe { ObjectRef::from_raw(b_ptr, KObjectType::IpcChannel) });
            return Err(map_handle_err(e));
        }
    };
    // Install endpoint 1. On failure, close `h0` (drops endpoint a, which nulls
    // b's peer) and reclaim b's still-held creation reference.
    let h1 = match global::get().allocate(pid, b_ptr, KObjectType::IpcChannel, rights) {
        Ok(h) => h,
        Err(e) => {
            close_and_release(h0, pid);
            // SAFETY: b_ptr still owns its creation reference.
            drop(unsafe { ObjectRef::from_raw(b_ptr, KObjectType::IpcChannel) });
            return Err(map_handle_err(e));
        }
    };

    // Publish the handles. A faulting copy rolls both back so we never leak an
    // endpoint into a process that cannot observe it.
    if let Err(e) = copy_to_user(e0, &h0) {
        close_and_release(h0, pid);
        close_and_release(h1, pid);
        return Err(from_user_access(e));
    }
    if let Err(e) = copy_to_user(e1, &h1) {
        close_and_release(h0, pid);
        close_and_release(h1, pid);
        return Err(from_user_access(e));
    }
    Ok(0)
}

/// `sys_channel_send(ch, msg, handles, count, mode, deadline)` — enqueue `*msg`
/// on the peer of endpoint `ch`, **moving** `handles[0..count]` to the receiver.
/// Each transferred handle must carry the `TRANSFER` right; the move is committed
/// (the sender's handles closed) only after the message is queued/held — a failed
/// send loses no capability.
///
/// - `NoBlock`: returns `0`, `WouldBlock` if the peer ring is full, or `PeerClosed`.
/// - `Block` / `BlockBounded`: returns a `PendingOperation` handle that completes
///   when the message is delivered (the message is held in the peer's pending-send
///   queue if the ring is full). `BlockBounded` additionally completes the PO
///   `TimedOut` (reclaiming the message) if undelivered by `deadline` (absolute
///   monotonic ns); `deadline` is ignored for `NoBlock`/`Block`.
pub fn sys_channel_send(
    ch: u64,
    msg: u64,
    handles: u64,
    count: u64,
    mode: u64,
    deadline: u64,
) -> SysResult {
    // Mode gating first (host-reachable, no pointer/lock use). `deadline` (absolute
    // monotonic ns) is consumed only by `BlockBounded`; ignored for `NoBlock`/`Block`.
    let send_mode = match SendMode::from_u32(mode as u32) {
        Some(m) => m,
        None => return Err(KError::InvalidArgument),
    };
    let count = count as usize;
    if count > IPC_HANDLE_MAX {
        return Err(KError::TooLarge);
    }
    let mptr = UserPtr::<u8>::new(msg).map_err(from_user_access)?;

    // Copy in the transferred-handle array (count × 8 bytes), if any.
    let mut h_raw = [0u64; IPC_HANDLE_MAX];
    if count > 0 {
        let hptr = UserPtr::<u8>::new(handles).map_err(from_user_access)?;
        let mut hbytes = [0u8; IPC_HANDLE_MAX * 8];
        copy_slice_from_user(&mut hbytes[..count * 8], hptr).map_err(from_user_access)?;
        for i in 0..count {
            h_raw[i] = u64::from_ne_bytes(hbytes[i * 8..i * 8 + 8].try_into().unwrap());
        }
    }

    let pid = crate::sched::current_owner_pid();
    let ok = lookup_typed(ch, pid, Rights::SEND, KObjectType::IpcChannel)?;

    // Look up + pin each transferred handle (requires `TRANSFER`). Collect the
    // in-flight references; on any failure, the collected refs drop here (outside
    // SCHED) and nothing in the sender's table has changed — atomic-or-fail.
    let mut transfers: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    for i in 0..count {
        let h = RawHandle(h_raw[i]);
        let t = global::get()
            .lookup(h, pid, Rights::TRANSFER)
            .map_err(map_handle_err)?;
        transfers[i] = Some(TransferRef { obj: t.object, rights: t.rights });
    }

    // Bounce the 4096-byte message in from user memory (outside any lock — a
    // user copy may fault). One heap slot per send; optimisation target later.
    let mut bounce = KBox::try_new(StoredMsg::zeroed()).map_err(|_| KError::OutOfMemory)?;
    copy_slice_from_user(bounce.as_bytes_mut(), mptr).map_err(from_user_access)?;
    if bounce.header.payload_len as usize > IPC_PAYLOAD_SIZE {
        return Err(KError::InvalidArgument);
    }
    // Stamp the kernel-controlled header fields; the receiver fills the wire
    // `handles[]` with its own handle values at recv.
    bounce.header.sender_pid = pid;
    bounce.header.timestamp = Timer::read_ns();
    bounce.header.handle_count = count as u8;
    bounce.handles = [0u64; IPC_HANDLE_MAX];

    // If this send's peer is the kernel's end of a Userspace Server channel, the
    // message is a forwarded-lookup **reply**: complete the waiting lookup inline
    // (cross-context install + PO signal) rather than enqueuing it — nothing in the
    // kernel receives that ring. The reply is consumed regardless of `send_mode`
    // (servers reply `NoBlock`). See `docs/spec/rsproto-namespace-ops.md`.
    if let Some(reg) = crate::sched::us_forward_reg_for_send(ok.object.as_ptr()) {
        return complete_forwarded_reply(reg, &bounce, &mut transfers, &h_raw, count, pid);
    }

    // `ok.object` (held here) pins the endpoint across the push.
    match send_mode {
        // Synchronous attempt: `Sent` commits the transfer move (close the
        // sender's handles); a full ring / dead peer leaves the transfers untaken
        // (they drop here, outside SCHED) so no capability is lost.
        SendMode::NoBlock => {
            match crate::sched::ipc_send_push(ok.object.as_ptr(), &bounce, &mut transfers) {
                SendOutcome::Sent { .. } => {
                    for i in 0..count {
                        close_and_release(RawHandle(h_raw[i]), pid);
                    }
                    Ok(0)
                }
                SendOutcome::Full => Err(KError::WouldBlock),
                SendOutcome::PeerClosed => Err(KError::PeerClosed),
            }
        }
        // Blocking: return a `PendingOperation` that completes when the message
        // is delivered — immediately if the ring has space, else when the peer
        // next receives. The message is committed either way, so the transfer
        // move is committed too (handles closed). Honors the async-first rule:
        // the syscall never parks; the caller `sys_wait`s on the returned handle.
        // `BlockBounded` additionally times out (PO completes `TimedOut`, message
        // reclaimed) if undelivered by `deadline`; `Block` waits unbounded.
        SendMode::Block | SendMode::BlockBounded => {
            let deadline_ns = if send_mode == SendMode::BlockBounded {
                deadline
            } else {
                u64::MAX
            };
            let po = PendingOperation::try_new().map_err(|_| KError::OutOfMemory)?;
            // SAFETY: `into_raw` yields the single creation reference; adopt it.
            let po_ref = unsafe {
                ObjectRef::from_raw(
                    KBox::into_raw(po).as_ptr() as *mut (),
                    KObjectType::PendingOperation,
                )
            };
            match crate::sched::ipc_send_push_blocking(
                ok.object.as_ptr(),
                &bounce,
                &mut transfers,
                &po_ref,
                deadline_ns,
            ) {
                BlockSendOutcome::Sent { .. } | BlockSendOutcome::Queued => {
                    // Message committed (delivered or held) — commit the transfers.
                    for i in 0..count {
                        close_and_release(RawHandle(h_raw[i]), pid);
                    }
                    // Install the PO handle (adopting the creation reference) and
                    // return it for the caller to `sys_wait` on.
                    let (op, ot) = po_ref.into_raw();
                    match global::get().allocate(pid, op, ot, pending_op_rights()) {
                        Ok(h) => Ok(h.bits() as isize),
                        // Handle table full: the message was still committed; we
                        // just can't hand back the PO. Drop our creation ref — a
                        // `Queued` PO stays alive via the pending-send queue's ref
                        // (signalled on delivery); a `Sent` PO was pre-signalled
                        // and is reclaimed here. SAFETY: reclaim `into_raw`'s ref.
                        Err(e) => {
                            drop(unsafe { ObjectRef::from_raw(op, ot) });
                            Err(map_handle_err(e))
                        }
                    }
                }
                // Back-pressure / dead peer: nothing committed — `transfers` drop
                // here (outside SCHED), the PO is unused, sender keeps its handles.
                BlockSendOutcome::PendingFull => {
                    drop(po_ref);
                    Err(KError::WouldBlock)
                }
                BlockSendOutcome::PeerClosed => {
                    drop(po_ref);
                    Err(KError::PeerClosed)
                }
            }
        }
    }
}

/// Complete a forwarded **reply** inline in the server's `sys_channel_send` (the
/// [`us_forward_reg_for_send`](crate::sched::us_forward_reg_for_send) path). The
/// kernel forwards two request kinds on a Userspace Server endpoint — a
/// `Namespace::Resolve` (a lookup) and a `File::ReadRange` (a page-cache fill) — so
/// this routes the reply by its op to the matching completion. A reply whose op is
/// neither (or that is too short to read an op) is consumed: its transfers drop on
/// return and the sender's handles are closed (the capability is consumed).
///
/// `bounce` is the copied reply message; `transfers` its in-flight transferred
/// references (`transfers[i]` ↔ the sender handle `h_raw[i]`, `count` of them);
/// `pid` is the sending server. Runs in syscall context (no `SCHED` held): the
/// cross-context `allocate` and the `ObjectRef` drops happen outside the lock.
fn complete_forwarded_reply(
    reg: *mut (),
    bounce: &StoredMsg,
    transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    h_raw: &[u64; IPC_HANDLE_MAX],
    count: usize,
    pid: u32,
) -> SysResult {
    let payload_len = (bounce.header.payload_len as usize).min(IPC_PAYLOAD_SIZE);
    match crate::rsproto::reply_op(&bounce.payload[..payload_len]) {
        Some(crate::rsproto::RESOLVE_OP) => {
            complete_resolve_reply(reg, bounce, transfers, h_raw, count, pid, payload_len)
        }
        Some(crate::rsproto::READ_RANGE_OP) => {
            complete_read_range_reply(reg, bounce, transfers, h_raw, count, pid, payload_len)
        }
        // Not a reply we forwarded (or unreadable): consume it.
        _ => {
            for i in 0..count {
                close_and_release(RawHandle(h_raw[i]), pid);
            }
            Ok(0)
        }
    }
}

/// Complete a forwarded `Namespace::Resolve` reply: correlate it to the
/// registration's pending lookup and deliver its outcome through that lookup's
/// `PendingOperation`. An `OBJECT_KIND_MEMOBJ` reply installs the transferred
/// `MemoryObject` into the **original caller's** table (rights = `requested ∩ the
/// rights the server granted`); an `OBJECT_KIND_FILE` reply builds a lazy
/// [`FileObject`] page-cache object (no transferred handle — `content_len` is the
/// file size) pointed back at this server and installs *that*; an error/other kind
/// completes the PO with a `KError`. A reply that does not correlate (duplicate /
/// stale) is dropped, its transfers released and the sender's handles closed.
fn complete_resolve_reply(
    reg: *mut (),
    bounce: &StoredMsg,
    transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    h_raw: &[u64; IPC_HANDLE_MAX],
    count: usize,
    pid: u32,
    payload_len: usize,
) -> SysResult {
    use crate::rsproto::{
        OBJECT_KIND_CHANNEL, OBJECT_KIND_FILE, OBJECT_KIND_MEMOBJ, ReplyKind, parse_reply,
    };

    // Parse the reply (recovers the `request_id` even on a malformed body).
    let reply = parse_reply(&bounce.payload[..payload_len]);
    let pl = match reply {
        Some(r) => crate::sched::us_forward_take_pending(reg, r.request_id),
        None => None,
    };
    let Some(pl) = pl else {
        for i in 0..count {
            close_and_release(RawHandle(h_raw[i]), pid);
        }
        return Ok(0);
    };
    let reply = reply.expect("pending taken ⇒ reply parsed");

    let (status, result): (i32, u64) = match reply.kind {
        ReplyKind::Error { kerror } => (kerror, 0),
        ReplyKind::Malformed => (KError::KernelError as i32, 0),
        // A lazy file: build the page-cache object (no transferred handle) and
        // install it. `content_len` is the total file size.
        ReplyKind::Success { object_kind, content_len } if object_kind == OBJECT_KIND_FILE => {
            build_and_install_file(reg, &pl, content_len)
        }
        ReplyKind::Success { object_kind, .. }
            if object_kind != OBJECT_KIND_MEMOBJ && object_kind != OBJECT_KIND_CHANNEL =>
        {
            (KError::Unsupported as i32, 0)
        }
        ReplyKind::Success { .. } => match transfers[0].take() {
            None => (KError::InvalidArgument as i32, 0), // success but no handle
            // The resolved object is a capability the server hands back. A `MemoryObject`
            // is the fs-server's eager reply; a `FileObject` is a **re-exported** handle
            // (an indirection server like the profile server resolves onward and passes
            // the store `FileObject` through); an `IpcChannel` is a live **connection** to
            // the server (the "resolve a service path → get a channel to it" case, e.g. the
            // logging service handing back a per-principal log channel). All install
            // identically below; other object types are not valid resolve results.
            Some(tr)
                if !matches!(
                    tr.obj.object_type(),
                    KObjectType::MemoryObject | KObjectType::FileObject | KObjectType::IpcChannel
                ) =>
            {
                drop(tr.obj);
                (KError::Unsupported as i32, 0)
            }
            Some(tr) => {
                // Install the resolved object into the original caller's table with
                // `requested ∩ (the rights the server granted on the transfer)`.
                let attenuated = pl.requested & tr.rights;
                // SAFETY: `tr.obj` owns the in-flight reference; hand it to `allocate`.
                let (op, ot) = tr.obj.into_raw();
                match global::get().allocate(pl.owner_pid, op, ot, attenuated) {
                    Ok(h) => (0, h.bits()),
                    Err(e) => {
                        // SAFETY: `allocate` did not adopt the reference; reclaim it.
                        // (A dead caller pid takes this path — fails cleanly.)
                        drop(unsafe { ObjectRef::from_raw(op, ot) });
                        (map_handle_err(e) as i32, 0)
                    }
                }
            }
        },
    };

    // Complete the lookup PO (one-shot); release the kernel's PO clone outside
    // `SCHED`; commit the transfer move by closing the sender's handles.
    crate::sched::complete_pending_op(pl.po.as_ptr(), status, result);
    drop(pl);
    for i in 0..count {
        close_and_release(RawHandle(h_raw[i]), pid);
    }
    Ok(0)
}

/// Build a lazy [`FileObject`] for an `OBJECT_KIND_FILE` resolve reply and install
/// it into the caller's table; returns the `(status, handle)` the lookup PO
/// completes with. The object's producer points back at this registration (`reg`)
/// and names the file by the lookup's stored suffix, so a later page fault fills it
/// via `File::ReadRange`. Fails `TooLarge` if the suffix overran the inline buffer
/// (the path can't be recovered), or `OutOfMemory` on allocation failure.
fn build_and_install_file(
    reg: *mut (),
    pl: &crate::object::userspace_server::PendingLookup,
    content_len: u32,
) -> (i32, u64) {
    use crate::libkern::KString;
    use crate::object::{FileObject, Producer};

    let Some(suffix) = pl.suffix() else {
        return (KError::TooLarge as i32, 0);
    };
    let Ok(s) = core::str::from_utf8(suffix) else {
        return (KError::InvalidArgument as i32, 0);
    };
    let kstr = match KString::try_from_str(s) {
        Ok(k) => k,
        Err(_) => return (KError::OutOfMemory as i32, 0),
    };
    // Pin the registration in the producer (so it outlives the file). The reg is
    // live — we reached it through the send's endpoint peer.
    // SAFETY: `reg` addresses the live `UserspaceServerReg` for this send.
    let reg_ref = match unsafe {
        ObjectRef::try_acquire(reg, KObjectType::UserspaceServerReg)
    } {
        Some(r) => r,
        None => return (KError::KernelError as i32, 0),
    };
    let fobj = match FileObject::try_new(
        content_len as usize,
        Producer::FsServer { reg: reg_ref, suffix: kstr },
    ) {
        Ok(f) => f,
        Err(_) => return (KError::OutOfMemory as i32, 0),
    };
    // SAFETY: `into_raw` yields the single creation reference; adopt it.
    let fref = unsafe {
        ObjectRef::from_raw(KBox::into_raw(fobj).as_ptr() as *mut (), KObjectType::FileObject)
    };
    let (op, ot) = fref.into_raw();
    // Grant `INSPECT` alongside the requested rights so a client can `sys_handle_stat`
    // the lazy file for its size before mapping (e.g. eshell `cat`) — `INSPECT` is a
    // generic right, benign on a handle the client already maps.
    let rights = pl.requested | Rights::INSPECT;
    match global::get().allocate(pl.owner_pid, op, ot, rights) {
        Ok(h) => (0, h.bits()),
        Err(e) => {
            // SAFETY: `allocate` did not adopt the reference; reclaim it.
            drop(unsafe { ObjectRef::from_raw(op, ot) });
            (map_handle_err(e) as i32, 0)
        }
    }
}

/// Complete a forwarded `File::ReadRange` reply: correlate it to the registration's
/// pending fill, copy the replied bytes (a transferred ≤1-page `MemoryObject`) into
/// the cache frame, mark the page ready, and complete the fill PO (waking the parked
/// faulter). An error/malformed reply completes the PO with a `KError` (failing the
/// fault). A reply that does not correlate is dropped (transfers released, handles
/// closed).
fn complete_read_range_reply(
    reg: *mut (),
    bounce: &StoredMsg,
    transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    h_raw: &[u64; IPC_HANDLE_MAX],
    count: usize,
    pid: u32,
    payload_len: usize,
) -> SysResult {
    use crate::rsproto::{RangeReplyKind, parse_read_range_reply};

    let reply = parse_read_range_reply(&bounce.payload[..payload_len]);
    let pf = match reply {
        Some(r) => crate::sched::us_forward_take_pending_fill(reg, r.request_id),
        None => None,
    };
    let Some(pf) = pf else {
        for i in 0..count {
            close_and_release(RawHandle(h_raw[i]), pid);
        }
        return Ok(0);
    };
    let reply = reply.expect("pending taken ⇒ reply parsed");

    let status: i32 = match reply.kind {
        RangeReplyKind::Error { kerror } => kerror,
        RangeReplyKind::Malformed => KError::KernelError as i32,
        RangeReplyKind::Success { content_len } => match transfers[0].take() {
            None => KError::InvalidArgument as i32, // success but no bytes
            Some(tr) if tr.obj.object_type() != KObjectType::MemoryObject => {
                drop(tr.obj);
                KError::Unsupported as i32
            }
            Some(tr) => {
                // Copy the file bytes into the cache frame (the frame was zeroed at
                // reserve, so a short tail stays zero-padded), then mark the page
                // ready so the faulter maps it on wake.
                copy_fill_into_frame(&tr.obj, pf.frame, content_len as usize);
                // SAFETY: `pf.file_obj` pins the live `FileObject` (header at 0).
                let fo: &crate::object::FileObject =
                    unsafe { &*(pf.file_obj.as_ptr() as *const crate::object::FileObject) };
                fo.mark_ready(pf.index);
                drop(tr.obj);
                0
            }
        },
    };

    // Complete the fill PO (one-shot); release the kernel's clones outside `SCHED`;
    // commit the transfer move by closing the sender's handles.
    crate::sched::complete_pending_op(pf.po.as_ptr(), status, 0);
    drop(pf);
    for i in 0..count {
        close_and_release(RawHandle(h_raw[i]), pid);
    }
    Ok(0)
}

/// Copy `n` bytes (clamped to one page) from the first frame of the transferred
/// fill `MemoryObject` `src` into the cache frame `dst`, both via the HHDM. A
/// no-op if `src` has no frames or `n == 0`.
fn copy_fill_into_frame(src: &ObjectRef, dst: crate::mm::PhysAddr, n: usize) {
    use crate::mm::{PAGE_SIZE, heap};
    // SAFETY: `src` pins the live `MemoryObject` (header at offset 0).
    let mo: &crate::object::MemoryObject =
        unsafe { &*(src.as_ptr() as *const crate::object::MemoryObject) };
    let frames = mo.frames();
    let n = n.min(PAGE_SIZE);
    if frames.is_empty() || n == 0 {
        return;
    }
    let src_va = (frames[0].as_u64() + heap::hhdm_offset()) as *const u8;
    let dst_va = (dst.as_u64() + heap::hhdm_offset()) as *mut u8;
    // SAFETY: both frames are page-aligned, HHDM-mapped, and distinct (one is the
    // server's just-transferred object, the other the cache frame); `n ≤ PAGE`.
    unsafe {
        core::ptr::copy_nonoverlapping(src_va, dst_va, n);
    }
}

/// Close every handle a partial `sys_channel_recv` installed in the receiver's
/// table, so a copy-out fault or a mid-install error after the message was
/// dequeued doesn't leak the installed handles.
fn recv_rollback_installed(installed: &[RawHandle], pid: u32) {
    for &h in installed {
        if !h.is_null() {
            close_and_release(h, pid);
        }
    }
}

/// `sys_channel_recv(ch, msg, handles, count)` — dequeue the oldest message on
/// endpoint `ch` into `*msg` (4096 bytes), **install** any transferred handles
/// into the caller's table (writing their values to `handles[0..*count]` and into
/// the in-message `handles[]`), and write the count to `*count`. `WouldBlock` if
/// the inbox is empty (caller `sys_wait`s on `ch`); `PeerClosed` if empty and the
/// peer has gone.
pub fn sys_channel_recv(ch: u64, msg: u64, handles: u64, count: u64) -> SysResult {
    let mptr = UserMutPtr::<u8>::new(msg).map_err(from_user_access)?;
    let cptr = UserMutPtr::<usize>::new(count).map_err(from_user_access)?;

    let pid = crate::sched::current_owner_pid();
    let ok = lookup_typed(ch, pid, Rights::RECV, KObjectType::IpcChannel)?;

    // Peek under SCHED so the empty-poll path allocates no bounce buffer.
    match crate::sched::ipc_recv_peek(ok.object.as_ptr()) {
        RecvState::Empty => return Err(KError::WouldBlock),
        RecvState::PeerClosed => return Err(KError::PeerClosed),
        RecvState::HasMsg => {}
    }
    let mut bounce = KBox::try_new(StoredMsg::zeroed()).map_err(|_| KError::OutOfMemory)?;
    let mut transfers: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    // Popping frees a ring slot, so a held blocking sender may be promoted into
    // it; `_promoted_po` is that sender's now-completed `PendingOperation`, and
    // `_reclaimed` collects any timed-out (`BlockBounded`) sends swept out of the
    // queue. Both drop here at scope exit — **outside** `SCHED` (no `ObjectRef`
    // Drop under the lock).
    let mut _reclaimed: [Option<ReclaimedSend>; IpcChannel::MAX_PENDING_SENDS] =
        core::array::from_fn(|_| None);
    let (popped, _promoted_po) = crate::sched::ipc_recv_pop_into(
        ok.object.as_ptr(),
        &mut bounce,
        &mut transfers,
        &mut _reclaimed,
    );
    if !popped {
        // Drained between the peek and the pop (only possible under future SMP).
        return Err(KError::WouldBlock);
    }
    // The message is now dequeued. From here, any error must reclaim what we've
    // installed and drop the remaining transfers (which the `transfers` array
    // does on scope exit, outside SCHED).
    let n = bounce.header.handle_count as usize;
    // `handle_count` is stamped at send time and bounded to `IPC_HANDLE_MAX`
    // (`sys_channel_send` rejects more), so a larger value here can only mean
    // a corrupted stored message. Refuse it rather than trust it: the loops
    // below index fixed-size `[_; IPC_HANDLE_MAX]` buffers and the copy-out
    // slices `hbytes[..n * 8]`, both of which would panic the kernel for
    // `n > IPC_HANDLE_MAX`. `transfers[]` drops on this return (outside SCHED),
    // reclaiming any in-flight references.
    if n > IPC_HANDLE_MAX {
        return Err(KError::KernelError);
    }
    let mut installed = [RawHandle::NULL; IPC_HANDLE_MAX];
    for i in 0..n {
        if let Some(tr) = transfers[i].take() {
            // SAFETY: `tr.obj` owns the in-flight reference; hand it to `allocate`.
            let (op, ot) = tr.obj.into_raw();
            match global::get().allocate(pid, op, ot, tr.rights) {
                Ok(h) => {
                    installed[i] = h;
                    bounce.handles[i] = h.bits();
                }
                Err(e) => {
                    // SAFETY: reclaim the reference `into_raw` yielded.
                    drop(unsafe { ObjectRef::from_raw(op, ot) });
                    recv_rollback_installed(&installed[..i], pid);
                    return Err(map_handle_err(e));
                }
            }
        }
    }

    // Copy out AFTER the lock is released (never hold SCHED across a user copy).
    if let Err(e) = copy_slice_to_user(mptr, bounce.as_bytes()) {
        recv_rollback_installed(&installed[..n.min(IPC_HANDLE_MAX)], pid);
        return Err(from_user_access(e));
    }
    if n > 0 {
        let hptr = UserMutPtr::<u8>::new(handles).map_err(|e| {
            recv_rollback_installed(&installed[..n.min(IPC_HANDLE_MAX)], pid);
            from_user_access(e)
        })?;
        let mut hbytes = [0u8; IPC_HANDLE_MAX * 8];
        for i in 0..n.min(IPC_HANDLE_MAX) {
            hbytes[i * 8..i * 8 + 8].copy_from_slice(&installed[i].bits().to_le_bytes());
        }
        if let Err(e) = copy_slice_to_user(hptr, &hbytes[..n * 8]) {
            recv_rollback_installed(&installed[..n.min(IPC_HANDLE_MAX)], pid);
            return Err(from_user_access(e));
        }
    }
    if let Err(e) = copy_to_user(cptr, &n) {
        recv_rollback_installed(&installed[..n.min(IPC_HANDLE_MAX)], pid);
        return Err(from_user_access(e));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libkern::KBox;
    use crate::libkern::handle::KObjectType;
    use crate::mm::test_support::init_global_heap;
    use crate::object::Process;

    #[test]
    fn unknown_number_is_unsupported() {
        assert_eq!(dispatch(0xDEAD, 0, 0, 0, 0, 0, 0), KError::Unsupported.as_isize());
    }

    #[test]
    fn clock_read_invalid_selector_is_invalid_argument() {
        // An unknown ClockId is rejected before the pointer is examined, so the
        // `out` address is never touched (host-reachable).
        assert_eq!(sys_clock_read(99, 0xDEAD_BEEF), Err(KError::InvalidArgument));
        assert_eq!(
            dispatch(SYS_CLOCK_READ, 99, 0xDEAD_BEEF, 0, 0, 0, 0),
            KError::InvalidArgument.as_isize(),
        );
    }

    #[test]
    fn clock_read_unsupported_clocks_are_unsupported() {
        // A valid user pointer (never dereferenced here) plus a not-yet-serviced
        // clock returns Unsupported before any clock read or copy-out.
        let valid_out = 0x1000u64; // page-aligned, u64-aligned, user-half
        assert_eq!(sys_clock_read(1, valid_out), Err(KError::Unsupported)); // Realtime
        assert_eq!(sys_clock_read(2, valid_out), Err(KError::Unsupported)); // ProcessCpu
        assert_eq!(sys_clock_read(3, valid_out), Err(KError::Unsupported)); // ThreadCpu
    }

    #[test]
    fn timer_create_rejects_unknown_flags() {
        // Unknown flag bits are rejected before `current_owner_pid` (which needs
        // a running thread), so this is host-reachable.
        assert_eq!(sys_timer_create(1), Err(KError::InvalidArgument));
        assert_eq!(
            dispatch(SYS_TIMER_CREATE, 0x8000_0000, 0, 0, 0, 0, 0),
            KError::InvalidArgument.as_isize(),
        );
    }

    #[test]
    fn wait_rejects_bad_count() {
        // count == 0 and count > MAX are checked before any lock / pointer use.
        assert_eq!(sys_wait(0xDEAD, 0, 0xBEEF, 0), Err(KError::InvalidArgument));
        assert_eq!(
            sys_wait(0xDEAD, MAX_WAIT_HANDLES + 1, 0xBEEF, 0),
            Err(KError::TooLarge),
        );
    }

    #[test]
    fn wait_rejects_bad_pointers() {
        // A valid count then an out-of-range user pointer fails before locks.
        // USER_VIRT_END is the user-half ceiling; anything ≥ it is rejected.
        let bad = USER_VIRT_END;
        assert_eq!(sys_wait(bad, 1, 0x1000, 1000), Err(KError::FaultFromUser));
        assert_eq!(sys_wait(0x1000, 1, bad, 1000), Err(KError::FaultFromUser));
    }

    #[test]
    fn timer_rights_are_allocatable_on_a_timer() {
        // The minted Timer rights must be accepted by the handle table for a
        // Timer (all generic; Timer's principal mask is empty).
        init_global_heap();
        let t = HandleTable::try_new(0x1234_5678_9ABC_DEF0).unwrap();
        let timer = crate::object::Timer::try_new().unwrap();
        let ptr = KBox::into_raw(timer).as_ptr() as *mut ();
        let h = t
            .allocate(1, ptr, KObjectType::Timer, timer_rights())
            .expect("timer_rights must be valid for a Timer");
        // Clean up: close releases the handle's ref, freeing the object.
        let co = t.close(h, 1).unwrap();
        drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
    }

    #[test]
    fn notif_recv_rejects_bad_pointer() {
        // An out-of-range `out` pointer fails at UserMutPtr::new, before any lock.
        assert_eq!(sys_notif_recv(0xDEAD, USER_VIRT_END), Err(KError::FaultFromUser));
    }

    #[test]
    fn notif_channel_rights_are_allocatable_on_a_channel() {
        // WAIT + the generic band (no principal rights) must be allocatable on a
        // NotificationChannel (wait-only type, empty principal mask).
        init_global_heap();
        let t = HandleTable::try_new(0x0FED_CBA9_8765_4321).unwrap();
        let chan = crate::object::NotificationChannel::try_new().unwrap();
        let ptr = KBox::into_raw(chan).as_ptr() as *mut ();
        let rights = Rights::WAIT | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
        let h = t
            .allocate(1, ptr, KObjectType::NotificationChannel, rights)
            .expect("WAIT + generic rights must be valid for a NotificationChannel");
        let co = t.close(h, 1).unwrap();
        drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
    }

    // --- Namespace syscalls --------------------------------------------------
    //
    // The full create → bind → lookup → wait round-trip needs the scheduler
    // (`current_owner_pid`, PO completion, `sys_wait`) and the global handle
    // table, so it is exercised by the QEMU `ns_demo` in `userspace/parent`, not
    // here. The host-reachable parts are the rights-allocatability, the error
    // map, and the path-length bounds that precede any pointer dereference.

    #[test]
    fn namespace_rights_are_allocatable_on_a_namespace() {
        // The rights `sys_ns_create` mints (LOOKUP|BIND principals + UNBIND
        // modifier + generic band) must be accepted by the handle table for a
        // Namespace.
        init_global_heap();
        let t = HandleTable::try_new(0x1357_9BDF_0246_8ACE).unwrap();
        let ns = Namespace::try_new().unwrap();
        let ptr = KBox::into_raw(ns).as_ptr() as *mut ();
        let h = t
            .allocate(1, ptr, KObjectType::Namespace, namespace_rights())
            .expect("namespace_rights must be valid for a Namespace");
        let co = t.close(h, 1).unwrap();
        drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
    }

    #[test]
    fn entropy_rights_are_allocatable_on_an_entropy_object() {
        // The rights `sys_entropy_create` mints (READ + generic band) must be
        // accepted by the handle table for an EntropyObject.
        init_global_heap();
        let t = HandleTable::try_new(0x2468_ACE0_1357_9BDF).unwrap();
        let e = EntropyObject::try_new().unwrap();
        let ptr = KBox::into_raw(e).as_ptr() as *mut ();
        let h = t
            .allocate(1, ptr, KObjectType::EntropyObject, entropy_rights())
            .expect("entropy_rights must be valid for an EntropyObject");
        let co = t.close(h, 1).unwrap();
        drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
    }

    #[test]
    fn entropy_read_rejects_zero_and_oversize_len() {
        // The length bounds are checked before the handle lookup / pointer use, so
        // these are host-reachable (no running thread needed).
        assert_eq!(sys_entropy_read(0xDEAD, 0x1000, 0), Ok(0));
        assert_eq!(
            sys_entropy_read(0xDEAD, 0x1000, ENTROPY_READ_MAX + 1),
            Err(KError::TooLarge),
        );
    }

    #[test]
    fn map_ns_err_maps_each_variant() {
        assert_eq!(map_ns_err(NsError::InvalidPath), KError::InvalidArgument);
        assert_eq!(map_ns_err(NsError::AlreadyBound), KError::InvalidArgument);
        assert_eq!(map_ns_err(NsError::NotBound), KError::NotFound);
        assert_eq!(map_ns_err(NsError::OutOfMemory), KError::OutOfMemory);
    }

    #[test]
    fn copy_ns_path_rejects_zero_and_oversize_len() {
        // The length bounds are checked before the user pointer is dereferenced,
        // so a dummy pointer/buffer suffices (host-reachable).
        let mut buf = [0u8; NS_PATH_MAX];
        assert_eq!(copy_ns_path(0x1000, 0, &mut buf), Err(KError::InvalidArgument));
        assert_eq!(
            copy_ns_path(0x1000, NS_PATH_MAX + 1, &mut buf),
            Err(KError::TooLarge),
        );
    }

    #[test]
    fn channel_create_rejects_bad_pointer_and_oversize_depth() {
        // The out-pointers are validated first (USER_VIRT_END is out of range),
        // then the depth bound — both before `current_owner_pid`/allocation.
        assert_eq!(
            sys_channel_create(USER_VIRT_END, 0x1000, 4),
            Err(KError::FaultFromUser),
        );
        assert_eq!(
            sys_channel_create(0x1000, USER_VIRT_END, 4),
            Err(KError::FaultFromUser),
        );
        // Valid pointers + a depth past the cap → InvalidArgument.
        assert_eq!(
            sys_channel_create(0x1000, 0x1000, IPC_MAX_QUEUE_DEPTH as u64 + 1),
            Err(KError::InvalidArgument),
        );
    }

    #[test]
    fn channel_send_rejects_mode_and_oversize_count_before_pointers() {
        // An unknown mode selector is InvalidArgument before any pointer/lock.
        // All three real modes (NoBlock/Block/BlockBounded) proceed past gating —
        // exercised end-to-end in the QEMU demo, not here. The deadline arg (6th)
        // is ignored at this stage.
        assert_eq!(
            sys_channel_send(0xDEAD, 0xDEAD, 0, 0, 99, 0),
            Err(KError::InvalidArgument),
        );
        // More transferred handles than a message can carry → TooLarge (checked
        // before any pointer use).
        assert_eq!(
            sys_channel_send(0xDEAD, 0xDEAD, 0, (IPC_HANDLE_MAX + 1) as u64, 1, 0),
            Err(KError::TooLarge),
        );
    }

    #[test]
    fn channel_send_rejects_bad_message_pointer() {
        // NoBlock + count 0, then a bad `msg` pointer fails before lookup/lock.
        assert_eq!(
            sys_channel_send(0xDEAD, USER_VIRT_END, 0, 0, 1, 0),
            Err(KError::FaultFromUser),
        );
    }

    #[test]
    fn channel_recv_rejects_bad_pointers() {
        // Either out-pointer being out of range fails before lookup/lock.
        assert_eq!(sys_channel_recv(0xDEAD, USER_VIRT_END, 0, 0x1000), Err(KError::FaultFromUser));
        assert_eq!(sys_channel_recv(0xDEAD, 0x1000, 0, USER_VIRT_END), Err(KError::FaultFromUser));
    }

    #[test]
    fn channel_endpoint_rights_are_allocatable_on_a_channel() {
        // The minted endpoint rights (SEND|RECV principal + the generic band)
        // must be accepted by the handle table for an IpcChannel.
        init_global_heap();
        let t = HandleTable::try_new(0x1357_9BDF_0246_8ACE).unwrap();
        let (a, b) = IpcChannel::try_new_pair(4).unwrap();
        let a_ptr = KBox::into_raw(a).as_ptr() as *mut ();
        let h = t
            .allocate(1, a_ptr, KObjectType::IpcChannel, channel_endpoint_rights())
            .expect("channel_endpoint_rights must be valid for an IpcChannel");
        // Close endpoint a (frees it, nulling b's peer), then drop b's box.
        let co = t.close(h, 1).unwrap();
        drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
        drop(b);
    }

    #[test]
    fn process_spawn_rejects_bad_args_pointer() {
        // The `SpawnArgs` pointer is validated first (before `current_owner_pid`
        // / the embedded image), so an out-of-range pointer is host-reachable.
        assert_eq!(sys_process_spawn(USER_VIRT_END), Err(KError::FaultFromUser));
        assert_eq!(
            dispatch(SYS_PROCESS_SPAWN, USER_VIRT_END, 0, 0, 0, 0, 0),
            KError::FaultFromUser.as_isize(),
        );
    }

    #[test]
    fn thread_create_rejects_bad_args_pointer() {
        // The `ThreadArgs` pointer is validated before `current_process` / the
        // scheduler, so an out-of-range pointer is host-reachable.
        assert_eq!(sys_thread_create(USER_VIRT_END), Err(KError::FaultFromUser));
        assert_eq!(
            dispatch(SYS_THREAD_CREATE, USER_VIRT_END, 0, 0, 0, 0, 0),
            KError::FaultFromUser.as_isize(),
        );
    }

    #[test]
    fn thread_get_registers_rejects_bad_out_pointer() {
        // The output pointer is validated first (before the handle lookup needs
        // a running thread), so a kernel-half pointer is host-reachable.
        assert_eq!(
            sys_thread_get_registers(0, USER_VIRT_END),
            Err(KError::FaultFromUser),
        );
        assert_eq!(
            dispatch(SYS_THREAD_GET_REGISTERS, 0, USER_VIRT_END, 0, 0, 0, 0),
            KError::FaultFromUser.as_isize(),
        );
    }

    #[test]
    fn exception_resume_rejects_unknown_disposition() {
        // The disposition is validated first (a pure value check), so an unknown
        // disposition is rejected without a running thread (host-reachable).
        // 1 (ResumeSkip) and 3+ are Phase 2 / reserved → Unsupported.
        assert_eq!(sys_exception_resume(0, 1, 0), Err(KError::Unsupported));
        assert_eq!(sys_exception_resume(0, 9, 0), Err(KError::Unsupported));
        assert_eq!(
            dispatch(SYS_EXCEPTION_RESUME, 0, 5, 0, 0, 0, 0),
            KError::Unsupported.as_isize(),
        );
    }

    #[test]
    fn kprint_zero_len_is_ok_without_touching_memory() {
        // len == 0 returns before building a UserPtr or touching serial.
        assert_eq!(dispatch(SYS_DEBUG_KPRINT, 0xDEAD_BEEF, 0, 0, 0, 0, 0), 0);
        assert_eq!(sys_kprint(0xDEAD_BEEF, 0), Ok(0));
    }

    #[test]
    fn kprint_oversize_is_too_large_without_touching_memory() {
        let too_big = (KPRINT_MAX + 1) as u64;
        assert_eq!(
            dispatch(SYS_DEBUG_KPRINT, 0xDEAD_BEEF, too_big, 0, 0, 0, 0),
            KError::TooLarge.as_isize(),
        );
        assert_eq!(sys_kprint(0xDEAD_BEEF, KPRINT_MAX + 1), Err(KError::TooLarge));
    }

    // --- Handle syscall cores ----------------------------------------
    //
    // These exercise the pure `*_on(&HandleTable, …)` cores against a local
    // table, covering the success path, owner/right enforcement, and the
    // error mapping — none of which the host can reach through the
    // `global::get()`/`current_owner_pid()` wrappers (those need a live
    // boot-time table and a running thread; they are QEMU-only).

    fn fresh_table() -> HandleTable {
        init_global_heap();
        HandleTable::try_new(0x0123_4567_89AB_CDEF).unwrap()
    }

    /// A real `Process` kernel object carrying its single creation reference,
    /// ready to transfer to `allocate`.
    fn mk_process(pid: u32) -> *mut () {
        KBox::into_raw(Process::try_new(pid).unwrap()).as_ptr() as *mut ()
    }

    /// Rights commonly granted to a `Process` handle in these tests (generic
    /// band + the two principal rights `Process` allows).
    fn full() -> Rights {
        Rights::DUPLICATE | Rights::INSPECT | Rights::SIGNAL | Rights::TERMINATE
    }

    #[test]
    fn map_handle_err_collapses_notowner_to_invalid() {
        assert_eq!(map_handle_err(HandleError::NullHandle), KError::InvalidHandle);
        assert_eq!(map_handle_err(HandleError::InvalidHandle), KError::InvalidHandle);
        // Capability hygiene: NotOwner is indistinguishable from nonexistent.
        assert_eq!(map_handle_err(HandleError::NotOwner), KError::InvalidHandle);
        assert_eq!(map_handle_err(HandleError::NoAccess), KError::NoAccess);
        assert_eq!(map_handle_err(HandleError::OutOfHandles), KError::OutOfHandles);
        assert_eq!(map_handle_err(HandleError::OutOfMemory), KError::OutOfMemory);
        assert_eq!(map_handle_err(HandleError::BadRights), KError::InvalidArgument);
    }

    #[test]
    fn close_on_success_invalidates_handle() {
        let t = fresh_table();
        let h = t
            .allocate(1, mk_process(1), KObjectType::Process, full())
            .unwrap();
        assert_eq!(close_on(&t, h, 1), Ok(0));
        assert_eq!(
            t.lookup(h, 1, Rights::empty()).unwrap_err(),
            HandleError::InvalidHandle,
        );
    }

    #[test]
    fn close_on_wrong_owner_is_invalid_handle() {
        let t = fresh_table();
        let h = t
            .allocate(1, mk_process(1), KObjectType::Process, full())
            .unwrap();
        assert_eq!(close_on(&t, h, 2), Err(KError::InvalidHandle));
        // Still valid for the real owner; clean up.
        assert_eq!(close_on(&t, h, 1), Ok(0));
    }

    #[test]
    fn close_on_null_handle_is_invalid() {
        let t = fresh_table();
        assert_eq!(close_on(&t, RawHandle::NULL, 1), Err(KError::InvalidHandle));
    }

    #[test]
    fn duplicate_on_returns_new_handle_with_intersected_rights() {
        let t = fresh_table();
        let h = t
            .allocate(1, mk_process(1), KObjectType::Process, full())
            .unwrap();
        let ret = duplicate_on(&t, h, 1, Rights::DUPLICATE | Rights::INSPECT).unwrap();
        assert!(ret >= 0, "a valid handle value is non-negative");
        let dup = RawHandle(ret as u64);
        assert_ne!(dup, h);
        // The duplicate carries exactly the intersection of the requested and
        // existing rights.
        let info = stat_on(&t, dup, 1).unwrap();
        assert_eq!(info.rights, (Rights::DUPLICATE | Rights::INSPECT).bits());
        // Close both references → the Process is destroyed (no leak).
        close_on(&t, dup, 1).unwrap();
        close_on(&t, h, 1).unwrap();
    }

    #[test]
    fn duplicate_on_without_duplicate_right_is_no_access() {
        let t = fresh_table();
        let h = t
            .allocate(
                1,
                mk_process(1),
                KObjectType::Process,
                Rights::INSPECT | Rights::SIGNAL,
            )
            .unwrap();
        assert_eq!(duplicate_on(&t, h, 1, Rights::INSPECT), Err(KError::NoAccess));
        close_on(&t, h, 1).unwrap();
    }

    #[test]
    fn restrict_on_attenuates_in_place() {
        let t = fresh_table();
        let h = t
            .allocate(1, mk_process(1), KObjectType::Process, full())
            .unwrap();
        assert_eq!(restrict_on(&t, h, 1, Rights::INSPECT), Ok(0));
        // INSPECT survives (so stat still works); everything else is gone.
        let info = stat_on(&t, h, 1).unwrap();
        assert_eq!(info.rights, Rights::INSPECT.bits());
        close_on(&t, h, 1).unwrap();
    }

    #[test]
    fn restrict_on_cannot_amplify() {
        let t = fresh_table();
        let h = t
            .allocate(1, mk_process(1), KObjectType::Process, Rights::INSPECT)
            .unwrap();
        // Requesting SIGNAL (not currently held) cannot add it.
        assert_eq!(restrict_on(&t, h, 1, Rights::SIGNAL | Rights::INSPECT), Ok(0));
        let info = stat_on(&t, h, 1).unwrap();
        assert_eq!(info.rights, Rights::INSPECT.bits());
        close_on(&t, h, 1).unwrap();
    }

    #[test]
    fn stat_on_reports_type_rights_generation() {
        let t = fresh_table();
        let rights = Rights::INSPECT | Rights::SIGNAL;
        let h = t
            .allocate(1, mk_process(1), KObjectType::Process, rights)
            .unwrap();
        let info = stat_on(&t, h, 1).unwrap();
        assert_eq!(info.object_type, KObjectType::Process as u32);
        assert_eq!(info.rights, rights.bits());
        let (_, _, generation) = h.decode();
        assert_eq!(info.generation, generation);
        close_on(&t, h, 1).unwrap();
    }

    #[test]
    fn stat_on_without_inspect_is_no_access() {
        let t = fresh_table();
        let h = t
            .allocate(1, mk_process(1), KObjectType::Process, Rights::SIGNAL)
            .unwrap();
        assert_eq!(stat_on(&t, h, 1), Err(KError::NoAccess));
        close_on(&t, h, 1).unwrap();
    }

    #[test]
    fn stat_on_wrong_owner_is_invalid_handle() {
        let t = fresh_table();
        let h = t
            .allocate(1, mk_process(1), KObjectType::Process, Rights::INSPECT)
            .unwrap();
        assert_eq!(stat_on(&t, h, 2), Err(KError::InvalidHandle));
        close_on(&t, h, 1).unwrap();
    }

    // --- Memory syscall helpers --------------------------------------
    //
    // The create/map/unmap handlers themselves depend on `global::get()` and
    // the current thread, so they are exercised end-to-end under QEMU; the
    // mapping mechanics are host-tested in `mm::addr_space` (map_object
    // aliasing) and `object::memory_object` (create/zero/drop). Here we cover
    // the pure helpers.

    #[test]
    fn round_up_page_rounds_and_saturates() {
        assert_eq!(round_up_page(0), 0);
        assert_eq!(round_up_page(1), PAGE_SIZE as u64);
        assert_eq!(round_up_page(PAGE_SIZE as u64), PAGE_SIZE as u64);
        assert_eq!(round_up_page(PAGE_SIZE as u64 + 1), 2 * PAGE_SIZE as u64);
        // Near u64::MAX it saturates (no wrap/panic) to a huge, page-aligned
        // value the size checks reject.
        let big = round_up_page(u64::MAX);
        assert_eq!(big & (PAGE_SIZE as u64 - 1), 0);
    }

    #[test]
    fn map_mem_map_err_mapping() {
        assert_eq!(map_mem_map_err(MapError::Overlap), KError::InvalidArgument);
        assert_eq!(map_mem_map_err(MapError::NotCanonical), KError::InvalidArgument);
        assert_eq!(map_mem_map_err(MapError::NotUserHalf), KError::InvalidArgument);
        assert_eq!(map_mem_map_err(MapError::OutOfMemory), KError::OutOfMemory);
    }

    #[test]
    fn full_mem_rights_are_allocatable_on_a_memory_object() {
        // The minted rights must be accepted by the handle table for a
        // MemoryObject (no principal bits outside the MAP_* band).
        init_global_heap();
        let t = HandleTable::try_new(0xFEED_FACE_CAFE_BEEF).unwrap();
        let m = crate::object::MemoryObject::try_new(PAGE_SIZE).unwrap();
        let ptr = KBox::into_raw(m).as_ptr() as *mut ();
        let h = t
            .allocate(1, ptr, KObjectType::MemoryObject, full_mem_rights())
            .expect("full_mem_rights must be valid for a MemoryObject");
        // Clean up: close releases the handle's ref, freeing the object.
        let co = t.close(h, 1).unwrap();
        drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
    }
}
