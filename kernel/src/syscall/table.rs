//! The syscall number → handler table and the handlers themselves. Dispatch is a `match` on
//! the number (`kernel/CLAUDE.md`: match, not `dyn`). Stable ABI numbers
//! (`docs/spec/syscall-abi.md`) count up from `0`; the **debug** syscalls sit in a high,
//! non-stable range.

use super::error::{KError, SysResult, encode, from_user_access};
use crate::arch::Paging;
use crate::arch::Timer;
use crate::arch::abi::USER_VIRT_END;
use crate::arch::paging::ArchPaging;
use crate::arch::timer::ArchTimer;
use crate::handle::global;
use crate::handle::table::{HandleError, HandleTable, LookupOk};
use crate::libkern::clock::ClockId;
use crate::rsproto::SizeChange;

/// What a resolve is being asked to do beyond resolving, one intent per call site.
#[derive(Copy, Clone)]
pub enum ResolveOp {
    /// A plain lookup.
    Plain,
    /// Change the file's size first (`sys_file_create` / `_grow` / `_truncate`).
    Size(u32, SizeChange),
    /// Rename the resolved path to `dest_ptr`/`dest_len` (`sys_file_rename`). Both paths must
    /// resolve through the **same binding**, so a server never sees a cross-filesystem rename.
    Rename { dest_ptr: u64, dest_len: usize, flags: u16 },
}
use crate::arch::RegisterValues;
use crate::arch::registers::ArchRegisters;
use crate::libkern::handle::{HandleInfo, KObjectType, NsEntry, RawHandle, Rights};
use crate::libkern::io_op::{IoOp, IoOpcode};
use crate::libkern::ipc::{IPC_DEFAULT_QUEUE_DEPTH, IPC_HANDLE_MAX, IPC_MAX_QUEUE_DEPTH, IPC_PAYLOAD_SIZE};
use crate::libkern::spawn::{SPAWN_MAX_HANDLES, SpawnArgs};
use crate::libkern::{ExitKind, ExitStatus, IoResult, KBox, MemFlags, Notification, SendMode, ThreadArgs, TimerFlags};
use crate::mm::addr_space::{AddressSpace, MapError};
use crate::mm::elf::load_elf;
use crate::mm::user_access::{
    UserMutPtr, UserPtr, copy_from_user, copy_slice_from_user, copy_slice_to_user, copy_to_user,
};
use crate::mm::vmm::{Protection, VAddrRange};
use crate::mm::{PAGE_SIZE, VirtAddr};
use crate::object::device_node::DeviceClass;
use crate::object::kernel_server::{self, OpStatus};
use crate::object::namespace::{NS_PATH_MAX, ResolvedTarget, SubtreeBase, validate_path};
use crate::object::userspace_server::{Forward, ForwardOp};
use crate::object::{
    BlockSendOutcome, DeviceNode, EntropyObject, FileObject, IpcChannel, MAX_WAIT_HANDLES, MemoryObject, Namespace,
    NotificationChannel, NsError, ObjectRef, PendingOperation, Process, ReclaimedSend, RecvState, SendOutcome,
    StoredMsg, Timer as TimerObject, TransferRef, UserspaceServerReg,
};

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
/// Spawn counters: `IMAGE_NS` isolates materialising the image into one contiguous kernel
/// buffer from the rest of `sys_process_spawn`.
pub mod spawn_stats {
    use core::sync::atomic::{AtomicU64, Ordering};

    pub static SPAWNS: AtomicU64 = AtomicU64::new(0);
    /// Total nanoseconds in `sys_process_spawn`, entry to return.
    pub static SPAWN_NS: AtomicU64 = AtomicU64::new(0);
    /// Of that, nanoseconds spent materialising the image bytes.
    pub static IMAGE_NS: AtomicU64 = AtomicU64::new(0);
    /// Total image bytes materialised.
    pub static IMAGE_BYTES: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn record(spawn_ns: u64, image_ns: u64, image_bytes: u64) {
        SPAWNS.fetch_add(1, Ordering::Relaxed);
        SPAWN_NS.fetch_add(spawn_ns, Ordering::Relaxed);
        IMAGE_NS.fetch_add(image_ns, Ordering::Relaxed);
        IMAGE_BYTES.fetch_add(image_bytes, Ordering::Relaxed);
    }

    /// `(spawns, spawn_ns, image_ns, image_bytes)`.
    pub fn snapshot() -> (u64, u64, u64, u64) {
        (
            SPAWNS.load(Ordering::Relaxed),
            SPAWN_NS.load(Ordering::Relaxed),
            IMAGE_NS.load(Ordering::Relaxed),
            IMAGE_BYTES.load(Ordering::Relaxed),
        )
    }
}

/// Print the page-cache fill and spawn counters at the end of an adjudicated run
/// (`test-harness` only); averages are integer-divided nanoseconds.
#[cfg(feature = "test-harness")]
fn report_fill_stats() {
    let s = crate::object::file_object::fill_stats::snapshot();
    let boot_ns = Timer::read_ns();
    let avg = s.fill_ns.checked_div(s.fills).unwrap_or(0);
    let permille = (s.fill_ns * 1000).checked_div(boot_ns).unwrap_or(0);
    crate::kprintln!(
        "page-cache fills: {} io ({} ns avg, {} ns max, {} ns total = {}/1000 of {} ns boot), \
         {} holes, {} hits, {} spins",
        s.fills,
        avg,
        s.fill_max_ns,
        s.fill_ns,
        permille,
        boot_ns,
        s.hole_fills,
        s.hits,
        s.spins,
    );
    let (spawns, spawn_ns, image_ns, image_bytes) = spawn_stats::snapshot();
    let spawn_avg = spawn_ns.checked_div(spawns).unwrap_or(0);
    let image_share = (image_ns * 100).checked_div(spawn_ns).unwrap_or(0);
    crate::kprintln!(
        "spawns: {} ({} ns avg, {} ns total = {}/1000 of boot); image materialise {} ns \
         ({}% of spawn) for {} bytes",
        spawns,
        spawn_avg,
        spawn_ns,
        (spawn_ns * 1000).checked_div(boot_ns).unwrap_or(0),
        image_ns,
        image_share,
        image_bytes,
    );
}

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
/// `sys_io_cancel` — request cancellation of an in-flight operation (always `Unsupported`).
pub const SYS_IO_CANCEL: u64 = 29;
/// `sys_ns_enumerate` — list a namespace's bindings (mount points + kernel resources).
pub const SYS_NS_ENUMERATE: u64 = 30;
/// `sys_file_sync` — flush a writable file mapping's dirty pages to the device.
pub const SYS_FILE_SYNC: u64 = 31;
/// `sys_file_grow` — resolve a file, growing it to a target size first (a5 = new size).
pub const SYS_FILE_GROW: u64 = 32;
/// `sys_file_create` — create a file, grow it to a target size (a5 = new size), then resolve.
pub const SYS_FILE_CREATE: u64 = 33;
/// `sys_file_truncate` — resolve a file, shrinking it to a target size first (a5 = new size).
pub const SYS_FILE_TRUNCATE: u64 = 34;
/// `sys_file_rename` — rename a path within one filesystem (a3/a4 = dest ptr/len, a5 = flags).
pub const SYS_FILE_RENAME: u64 = 35;
/// `sys_process_terminate` — **ask** a process to exit (§11h) via a `TerminateRequested` notice.
pub const SYS_PROCESS_TERMINATE: u64 = 36;
/// `sys_ns_derive` — a new namespace holding a copy of another's bindings, full-rights handle.
pub const SYS_NS_DERIVE: u64 = 37;
/// `sys_ns_sync(ns, path, path_len)` — write back every dirty file under a mount.
pub const SYS_NS_SYNC: u64 = 38;
/// `sys_ns_held(ns, path, path_len)` — how many of a mount's files something still holds.
pub const SYS_NS_HELD: u64 = 39;
/// `sys_power(system_control, op)` — flush every disk, stop every processor, halt or reset.
pub const SYS_POWER: u64 = 40;
/// `sys_clock_set(clock, ns)` — set the wall clock, and the RTC under it. Needs `SYSTEM_CLOCK`.
pub const SYS_CLOCK_SET: u64 = 41;

/// Debug: write a user byte buffer to the kernel serial log. Not ABI-stable.
pub const SYS_DEBUG_KPRINT: u64 = 0xFFFF_0000;

/// Integration-test only (`test-harness`): end the emulator with a verdict (the low byte, via
/// QEMU `isa-debug-exit`). Absent from production kernels.
#[cfg(feature = "test-harness")]
pub const SYS_TEST_EXIT: u64 = 0xFFFF_0002;

/// Largest buffer `sys_kprint` copies in one call; bounds the on-stack kernel buffer.
const KPRINT_MAX: usize = 4096;

/// Route a decoded syscall to its handler: `nr` is the number (RAX), `a0..a5` the argument
/// registers (RDI, RSI, RDX, R10, R8, R9); returns the `isize` handed back in RAX.
pub fn dispatch(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> isize {
    // Handle syscalls drop a read guard that marks the context quiescent, so deferred closes
    // are reclaimed on the next allocate/close.
    // TODO(smp): a non-lookup syscall, or SMP, may need `global::get().quiesce(current_ctx_id())`.
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
        SYS_PROCESS_TERMINATE => encode(sys_process_terminate(a0)),
        SYS_NS_CREATE => encode(sys_ns_create()),
        SYS_NS_DERIVE => encode(sys_ns_derive(a0)),
        SYS_NS_SYNC => encode(sys_ns_sync(a0, a1, a2 as usize)),
        SYS_NS_HELD => encode(sys_ns_held(a0, a1, a2 as usize)),
        SYS_POWER => encode(sys_power(a0, a1)),
        SYS_CLOCK_SET => encode(sys_clock_set(a0, a1)),
        SYS_NS_LOOKUP => encode(sys_ns_lookup(a0, a1, a2 as usize, a3, ResolveOp::Plain)),
        SYS_FILE_GROW => encode(sys_ns_lookup(a0, a1, a2 as usize, a3, ResolveOp::Size(a4 as u32, SizeChange::Grow))),
        SYS_FILE_CREATE => {
            encode(sys_ns_lookup(a0, a1, a2 as usize, a3, ResolveOp::Size(a4 as u32, SizeChange::Create)))
        }
        SYS_FILE_RENAME => encode(sys_ns_lookup(
            a0,
            a1,
            a2 as usize,
            crate::libkern::handle::Rights::MAP_READ.bits(),
            ResolveOp::Rename { dest_ptr: a3, dest_len: a4 as usize, flags: a5 as u16 },
        )),
        SYS_FILE_TRUNCATE => {
            encode(sys_ns_lookup(a0, a1, a2 as usize, a3, ResolveOp::Size(a4 as u32, SizeChange::Truncate)))
        }
        SYS_NS_BIND => encode(sys_ns_bind(a0, a1, a2 as usize, a3, a4, a5 as usize)),
        SYS_NS_UNBIND => encode(sys_ns_unbind(a0, a1, a2 as usize)),
        SYS_NS_ENUMERATE => encode(sys_ns_enumerate(a0, a1, a2)),
        SYS_ENTROPY_CREATE => encode(sys_entropy_create()),
        SYS_ENTROPY_READ => encode(sys_entropy_read(a0, a1, a2 as usize)),
        SYS_IO_SUBMIT => encode(sys_io_submit(a0, a1)),
        SYS_IO_CANCEL => encode(sys_io_cancel(a0)),
        SYS_FILE_SYNC => encode(sys_file_sync(a0)),
        SYS_DEBUG_KPRINT => encode(sys_kprint(a0, a1 as usize)),
        // Test build only: end the QEMU run with the caller's verdict. Without `isa-debug-exit`
        // the write is ignored and this **returns `Unsupported`**; parking the CPU instead
        // deadlocked every later TLB shootdown.
        #[cfg(feature = "test-harness")]
        SYS_TEST_EXIT => {
            report_fill_stats();
            report_stack_watermark();
            crate::arch::debug_exit(a0 as u32);
            KError::Unsupported.as_isize()
        }
        SYS_PROCESS_EXIT => sys_process_exit(a0 as i32),
        SYS_THREAD_EXIT => sys_thread_exit(a0 as i32),
        _ => KError::Unsupported.as_isize(),
    }
}

/// `sys_process_exit(status)` — terminate the calling **process** with `status`: every sibling
/// thread is torn down, `ChildExited { pid, Normal(status) }` goes to the parent's notification
/// channel (if any), and this thread is reaped. Never returns.
fn sys_process_exit(status: i32) -> ! {
    // **Close our own handle table first, in syscall context** (no locks held, free to allocate
    // and drop): `exit_process` takes `SCHED` and never returns, so dropping there is forbidden.
    // An IPC endpoint's last reference nulls its peer, so this makes `PeerClosed` prompt. The
    // reaper still closes this pid afterwards (an empty table) for a process killed externally.
    if let Some((pid, _tid)) = crate::sched::current_pid_tid() {
        let n = crate::handle::global::close_all_owned_by(pid);
        crate::sched::EXIT_CLOSED.fetch_add(n as u64, core::sync::atomic::Ordering::Relaxed);
    }
    crate::sched::exit_process(ExitStatus { kind: ExitKind::Normal as u32, code: status })
}

/// `sys_thread_exit(status)` — terminate the **calling thread**; `ChildExited` fires only for
/// the last. Never returns.
fn sys_thread_exit(status: i32) -> ! {
    crate::sched::exit_thread(ExitStatus { kind: ExitKind::Normal as u32, code: status })
}

/// Close every child-side handle a partly-built spawn allocated; `ns_h` is `NULL` if none.
fn spawn_rollback_child_handles(child_pid: u32, notif_h: RawHandle, ns_h: RawHandle, installed: &[RawHandle]) {
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

/// `sys_process_spawn(args)` — create a child process from an image object and start it.
/// Returns a handle to the child `Process` (`SIGNAL | TERMINATE`). The child's handle table is
/// populated from `args.handles` (each installed with `source_rights & args.rights[i]`; bit `i`
/// of `args.move_mask` chooses move vs. duplicate), plus a fresh notification channel. The
/// bootstrap registers are `rdi` = its notification-channel handle, `rsi` = its namespace
/// handle, `rdx` = its first installed handle, `rcx` = `args.arg0`.
///
/// Atomic-or-fail: a failure before the child thread is enqueued rolls back every child-side
/// allocation and leaves the parent's handles untouched (moves close the source handle only
/// after the spawn commits).
pub fn sys_process_spawn(args_ptr: u64) -> SysResult {
    let spawn_started = Timer::read_ns();
    let r = sys_process_spawn_inner(args_ptr, spawn_started);
    // Even a failed spawn: one that fails after materialising the image has paid for it.
    spawn_stats::SPAWN_NS
        .fetch_add(Timer::read_ns().saturating_sub(spawn_started), core::sync::atomic::Ordering::Relaxed);
    r
}

fn sys_process_spawn_inner(args_ptr: u64, _spawn_started: u64) -> SysResult {
    let aptr = UserPtr::<SpawnArgs>::new(args_ptr).map_err(from_user_access)?;
    let args = copy_from_user(aptr).map_err(from_user_access)?;
    let count = args.handle_count as usize;
    if count > SPAWN_MAX_HANDLES {
        return Err(KError::TooLarge);
    }
    let parent_pid = crate::sched::current_owner_pid();

    let img =
        global::get().lookup(RawHandle(args.image.bits()), parent_pid, Rights::MAP_READ).map_err(map_handle_err)?;
    let image_started = Timer::read_ns();
    let img_bytes = match img.object.object_type() {
        KObjectType::MemoryObject => {
            // SAFETY: type checked above; `img.object` pins it live.
            unsafe { &*(img.object.as_ptr() as *const MemoryObject) }.copy_to_kvec().map_err(|_| KError::OutOfMemory)?
        }
        // Blocks on each fill — sound on the syscall thread, which holds no AS/cache lock.
        KObjectType::FileObject => FileObject::read_to_kvec(&img.object).map_err(|_| KError::OutOfMemory)?,
        _ => return Err(KError::InvalidArgument),
    };
    spawn_stats::record(
        0, // the whole-call duration is added by the wrapper
        Timer::read_ns().saturating_sub(image_started),
        img_bytes.len() as u64,
    );

    let asp = AddressSpace::new().map_err(|_| KError::OutOfMemory)?;
    // A spawner's ELF is untrusted input: the loader bounds-checks every offset.
    let info = load_elf(&asp, &img_bytes).map_err(|_| KError::InvalidArgument)?;
    drop(img);
    let child_pid = crate::sched::alloc_pid();

    // `child = parent & requested` syscaps (`docs/architecture/syscaps.md`); no owning Process
    // yields an unprivileged child.
    let child_syscaps = {
        let requested = crate::libkern::SysCaps::from_bits_truncate(args.syscaps);
        match crate::sched::current_process() {
            // SAFETY: pins a live Process; read its immutable syscap set.
            Some(pp) => unsafe { &*(pp.as_ptr() as *const Process) }.syscaps() & requested,
            None => crate::libkern::SysCaps::empty(),
        }
    };

    let mut proc_box = Process::try_new_user(child_pid, asp, child_syscaps).map_err(|_| KError::OutOfMemory)?;
    if let Some(parent_proc) = crate::sched::current_process() {
        // SAFETY: `parent_proc` pins a live Process; clone its channel ref.
        if let Some(c) = unsafe { &*(parent_proc.as_ptr() as *const Process) }.notification_channel_ref() {
            proc_box.set_parent_notif(c);
        }
    }

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

    // The child's root: `args.namespace` if the parent holds it with `LOOKUP`, else the parent's
    // own; always a **LOOKUP-only** handle, so the child cannot rebind its root.
    let ns_source: Option<ObjectRef> = if !args.namespace.is_null() {
        match lookup_typed(args.namespace.bits(), parent_pid, Rights::LOOKUP, KObjectType::Namespace) {
            Ok(ok) => Some(ok.object.clone()),
            Err(e) => {
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
    }

    let proc_obj = {
        let p = KBox::into_raw(proc_box).as_ptr() as *mut ();
        // SAFETY: `into_raw` yielded the single creation reference; adopt it.
        unsafe { ObjectRef::from_raw(p, KObjectType::Process) }
    };
    let proc_for_handle = proc_obj.clone();

    let endpoint_h = if count > 0 { installed[0].bits() } else { 0 };
    let boot = [notif_h.bits(), child_ns_h.bits(), endpoint_h, args.arg0];
    if crate::sched::spawn_user(proc_obj, info.entry_point.as_u64(), info.stack_top.as_u64(), boot).is_err() {
        drop(proc_for_handle);
        spawn_rollback_child_handles(child_pid, notif_h, child_ns_h, &installed[..count]);
        return Err(KError::OutOfMemory);
    }

    let (pp, pt) = proc_for_handle.into_raw();
    let parent_handle = match global::get().allocate(parent_pid, pp, pt, Rights::SIGNAL | Rights::TERMINATE) {
        Ok(h) => h,
        Err(e) => {
            // The child is already running; reclaim only the process-handle reference.
            // SAFETY: adopt + drop the reference yielded by `into_raw`.
            drop(unsafe { ObjectRef::from_raw(pp, pt) });
            return Err(map_handle_err(e));
        }
    };

    for i in 0..count {
        if (args.move_mask >> i) & 1 == 1 {
            close_and_release(args.handles[i], parent_pid);
        }
    }

    Ok(parent_handle.bits() as isize)
}

/// `sys_thread_set_affinity(thread, cpu_mask)` — restrict a thread's CPUs (bit `c` ⇒ CPU `c`).
/// Requires `SIGNAL`. Bits above `MAX_CPUS` are ignored; an empty mask is `InvalidArgument`.
pub fn sys_thread_set_affinity(thread_h: u64, cpu_mask: u64) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    let ok = lookup_typed(thread_h, pid, Rights::SIGNAL, KObjectType::Thread)?;
    let mask = (cpu_mask & ((1u64 << crate::arch::MAX_CPUS) - 1)) as u8;
    if mask == 0 {
        return Err(KError::InvalidArgument);
    }
    // SAFETY: `ok.object` is the live `Thread` its lookup type-checked, held across the call.
    unsafe { crate::sched::set_thread_affinity(ok.object.as_ptr(), mask) };
    Ok(0)
}

/// `sys_thread_create(args)` — start another thread in the **calling process** and return a
/// `Thread` handle (`SIGNAL | TERMINATE | INSPECT | DUPLICATE`) to it. The new thread begins at
/// `args.entry` (ring 3) with `rsp = args.user_sp` and `rcx = args.arg0`; the caller owns the
/// user stack. `entry` and `user_sp` must be non-null user-half addresses; that they are
/// *mapped* is the caller's responsibility (an unmapped one faults the new thread, which is
/// then contained by suspend/terminate).
///
/// The supervisor primitive behind exception handling.
pub fn sys_thread_create(args_ptr: u64) -> SysResult {
    let aptr = UserPtr::<ThreadArgs>::new(args_ptr).map_err(from_user_access)?;
    let args = copy_from_user(aptr).map_err(from_user_access)?;
    if args._reserved != [0u8; 36] {
        return Err(KError::InvalidArgument);
    }
    if args.entry == 0 || args.user_sp == 0 {
        return Err(KError::InvalidArgument);
    }
    UserPtr::<u8>::new(args.entry).map_err(|_| KError::InvalidArgument)?;
    UserPtr::<u8>::new(args.user_sp).map_err(|_| KError::InvalidArgument)?;

    use crate::libkern::thread::{THREAD_CLASS_REALTIME, THREAD_CLASS_TIMESHARED};
    use crate::object::thread::SchedClass;
    // Only `RealTime` is gated: renicing or pinning your own thread is not privileged.
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

    let proc = crate::sched::current_process().ok_or(KError::InvalidArgument)?;
    let pid = crate::sched::current_owner_pid();

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

    let (tp, tt) = th.into_raw();
    let rights = Rights::SIGNAL | Rights::TERMINATE | Rights::INSPECT | Rights::DUPLICATE;
    match global::get().allocate(pid, tp, tt, rights) {
        Ok(h) => Ok(h.bits() as isize),
        Err(e) => {
            // Already running; reclaim only this surplus reference, off the rank-3 table lock.
            // SAFETY: adopt + drop the single reference `into_raw` yielded.
            drop(unsafe { ObjectRef::from_raw(tp, tt) });
            Err(map_handle_err(e))
        }
    }
}

/// `sys_thread_get_registers(thread, out)` — write the user registers of a **suspended**
/// (faulted) thread into the [`RegisterValues`] at `out`. Needs `SIGNAL`; a thread not
/// suspended on a fault is `InvalidArgument`. The frame lives on the parked thread's kernel
/// stack, so it is stable across the lock-free copy-out.
pub fn sys_thread_get_registers(thread_h: u64, out_ptr: u64) -> SysResult {
    let uptr = UserMutPtr::<RegisterValues>::new(out_ptr).map_err(from_user_access)?;

    let pid = crate::sched::current_owner_pid();
    let ok = lookup_typed(thread_h, pid, Rights::SIGNAL, KObjectType::Thread)?;

    // SAFETY: `ok.object` is the live `Thread` its lookup type-checked, held across the call.
    let frame_ptr =
        unsafe { crate::sched::thread_exception_frame(ok.object.as_ptr()) }.ok_or(KError::InvalidArgument)?;
    let regs: RegisterValues = crate::arch::Registers::read_from_exception_frame(frame_ptr);
    copy_to_user(uptr, &regs).map_err(from_user_access)?;
    Ok(0)
}

/// `sys_exception_resume(thread, disposition, code)` — act on a thread suspended on a fault.
/// `disposition` is `0` = **Resume** (re-enter the faulting instruction; without fixing the
/// cause it re-faults) or `2` = **Terminate** (exit the thread with `code`); other values are
/// `Unsupported`. Needs `SIGNAL` on a `Thread` handle; a thread not suspended is
/// `InvalidArgument`. Returns `0`.
pub fn sys_exception_resume(thread_h: u64, disposition: u64, code: u64) -> SysResult {
    let tag = match disposition {
        0 => 0u8,
        2 => 2u8,
        _ => return Err(KError::Unsupported),
    };

    let pid = crate::sched::current_owner_pid();
    let ok = lookup_typed(thread_h, pid, Rights::SIGNAL, KObjectType::Thread)?;
    // SAFETY: `ok.object` is the live `Thread` its lookup type-checked, held across the call.
    if !unsafe { crate::sched::resume_suspended(ok.object.as_ptr(), tag, code as i32) } {
        return Err(KError::InvalidArgument);
    }
    Ok(0)
}

/// `sys_process_terminate(process)` — **ask** a process to exit (§11h).
///
/// Gated on `SIGNAL`, not `TERMINATE`: this is a *request*, and `TERMINATE` stays free to gate
/// a forcible kill later. The authority is the handle: no pid is nameable anywhere in the
/// syscall surface, so "you may terminate your own children" needs no check. `Unsupported` if
/// the target has no notification channel (it cannot be asked).
pub fn sys_process_terminate(process_h: u64) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    let ok = lookup_typed(process_h, pid, Rights::SIGNAL, KObjectType::Process)?;
    // SAFETY: `ok.object` pins the `Process` its lookup type-checked.
    if !unsafe { crate::sched::request_terminate(ok.object.as_ptr()) } {
        return Err(KError::Unsupported);
    }
    Ok(0)
}

/// Print the deepest kernel-stack usage over the run, in bytes and as a percentage of one
/// stack. Interrupts have no separate stack (`TODO(irq-stack)`), so their frames are included.
#[cfg(feature = "test-harness")]
fn report_stack_watermark() {
    let (prefix, ring, elided) = crate::klog::stats();
    crate::kprintln!(
        "klog: boot prefix {} B of {}, ring {} B of {}, elided {} B",
        prefix,
        crate::klog::PREFIX_CAP,
        ring,
        crate::klog::RING_CAP,
        elided
    );
    let (used, pct, pid) = crate::mm::kstack::watermark::deepest();
    crate::kprintln!(
        "kstack: deepest {} B of {} ({}%), by pid {}",
        used,
        crate::mm::kstack::KERNEL_STACK_BYTES,
        pct,
        pid
    );
}

/// `sys_kprint(ptr, len)` — copy `len` bytes from `ptr` to the serial console. Debug-only.
/// Returns the bytes written; `len == 0` and `len > KPRINT_MAX` (`TooLarge`) are checked first.
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
    copy_slice_from_user(dst, uptr).map_err(from_user_access)?;

    // SERIAL is rank 7 (lowest); no other lock is held. `\n` becomes `\r\n`.
    let serial = crate::arch::serial::SERIAL.lock();
    // The screen shows what COM1 receives (no serial port: a failed mount would say nothing).
    crate::fbcon::push(dst);
    for &b in dst.iter() {
        if b == b'\n' {
            serial.write_byte(b'\r');
        }
        serial.write_byte(b);
    }
    Ok(len as isize)
}

// --- Handle operation syscalls: wrappers resolve the pid (rank-1 lock) **before** entering the
// table (rank-3 lock), never nested, then defer to a pure `*_on(&HandleTable, …)` core. ---

/// Map a handle-table error into the syscall error space. `NotOwner` collapses to
/// `InvalidHandle`, not `NoAccess`: another process's handle must be indistinguishable from
/// one that never existed.
fn map_handle_err(e: HandleError) -> KError {
    match e {
        HandleError::NullHandle | HandleError::InvalidHandle | HandleError::NotOwner => KError::InvalidHandle,
        HandleError::NoAccess => KError::NoAccess,
        HandleError::OutOfHandles => KError::OutOfHandles,
        HandleError::OutOfMemory => KError::OutOfMemory,
        HandleError::BadRights => KError::InvalidArgument,
    }
}

/// Look up `h` for `pid`, require `required` rights, confirm the object is of type `expected`
/// (else `InvalidArgument`); the [`LookupOk`] pins the object.
fn lookup_typed(h: u64, pid: u32, required: Rights, expected: KObjectType) -> Result<LookupOk, KError> {
    let ok = global::get().lookup(RawHandle(h), pid, required).map_err(map_handle_err)?;
    if ok.object.object_type() != expected {
        return Err(KError::InvalidArgument);
    }
    Ok(ok)
}

fn close_on(t: &HandleTable, h: RawHandle, pid: u32) -> SysResult {
    let co = t.close(h, pid).map_err(map_handle_err)?;
    // Dropped outside the table call so teardown (rank-6 allocator) doesn't nest under rank 3.
    // SAFETY: `co` carries exactly the handle's one outstanding reference;
    // we account for it once.
    drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
    Ok(0)
}

fn duplicate_on(t: &HandleTable, h: RawHandle, pid: u32, new_rights: Rights) -> SysResult {
    let dup = t.duplicate(h, pid, new_rights).map_err(map_handle_err)?;
    // A handle's bits are non-negative as an `isize`: the generation never reaches its top bit.
    Ok(dup.bits() as isize)
}

fn restrict_on(t: &HandleTable, h: RawHandle, pid: u32, new_rights: Rights) -> SysResult {
    t.restrict(h, pid, new_rights).map_err(map_handle_err)?;
    Ok(0)
}

/// Core of `sys_handle_stat`: build the `HandleInfo` for `h`, apart from the copy-out.
fn stat_on(t: &HandleTable, h: RawHandle, pid: u32) -> Result<HandleInfo, KError> {
    let ok = t.lookup(h, pid, Rights::INSPECT).map_err(map_handle_err)?;
    let object_type = ok.object.object_type();
    let (_, _, generation) = h.decode();
    let size = object_byte_size(object_type, &ok.object);
    Ok(HandleInfo::from_stat(object_type, ok.rights, generation, size))
}

/// The byte size in `HandleInfo.size`, else `0`: a `MemoryObject`'s page-rounded size, a
/// `FileObject`'s exact size, a block `DeviceNode`'s capacity (an installer needs it).
fn object_byte_size(ty: KObjectType, obj: &ObjectRef) -> u64 {
    match ty {
        // SAFETY: `obj` pins a live object of the matched type.
        KObjectType::MemoryObject => unsafe { &*(obj.as_ptr() as *const MemoryObject) }.size() as u64,
        // SAFETY: as above.
        KObjectType::FileObject => unsafe { &*(obj.as_ptr() as *const FileObject) }.size() as u64,
        // SAFETY: as above.
        KObjectType::DeviceNode => unsafe { &*(obj.as_ptr() as *const DeviceNode) }.block_info().byte_capacity(),
        _ => 0,
    }
}

/// `sys_handle_close(h)` — release the caller's reference; the value is invalid afterwards.
/// Requires no right (authorisation is the ownership check). Returns 0.
pub fn sys_handle_close(h: u64) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    close_on(global::get(), RawHandle(h), pid)
}

/// `sys_handle_duplicate(h, new_rights)` — a new handle to the same object with rights
/// `h.rights & new_rights`; `h` stays valid. Requires `DUPLICATE`. Returns the new handle.
pub fn sys_handle_duplicate(h: u64, new_rights: u64) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    duplicate_on(global::get(), RawHandle(h), pid, Rights::from_bits_truncate(new_rights))
}

/// `sys_handle_restrict(h, new_rights)` — attenuate `h`'s rights in place to
/// `h.rights & new_rights`; cannot amplify. Requires no right. Returns 0.
pub fn sys_handle_restrict(h: u64, new_rights: u64) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    restrict_on(global::get(), RawHandle(h), pid, Rights::from_bits_truncate(new_rights))
}

/// `sys_handle_stat(h, out)` — write `HandleInfo` for `h` to `out`. Needs `INSPECT`. Returns 0.
pub fn sys_handle_stat(h: u64, out: u64) -> SysResult {
    let uptr = UserMutPtr::<HandleInfo>::new(out).map_err(from_user_access)?;
    let pid = crate::sched::current_owner_pid();
    let info = stat_on(global::get(), RawHandle(h), pid)?;
    copy_to_user(uptr, &info).map_err(from_user_access)?;
    Ok(0)
}

/// Rights minted on a fresh `MemoryObject` handle: the `MAP_*` band only (`handle/type_rights.rs`);
/// `READ`/`WRITE`/`EXECUTE` would be rejected as `BadRights`.
fn full_mem_rights() -> Rights {
    Rights::MAP_READ | Rights::MAP_WRITE | Rights::MAP_EXEC | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER
}

fn map_mem_map_err(e: MapError) -> KError {
    match e {
        MapError::Overlap | MapError::NotCanonical | MapError::NotUserHalf => KError::InvalidArgument,
        MapError::OutOfMemory => KError::OutOfMemory,
    }
}

fn round_up_page(n: u64) -> u64 {
    n.saturating_add(PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1)
}

fn current_address_space(proc_ref: &ObjectRef) -> Option<&AddressSpace> {
    debug_assert_eq!(proc_ref.object_type(), KObjectType::Process);
    // SAFETY: `proc_ref` references a live `Process` (its `KObjectHeader` is at
    // offset 0), pinned by the current user thread for the syscall's duration.
    // The returned borrow is tied to `proc_ref`'s lifetime.
    let proc: &Process = unsafe { &*(proc_ref.as_ptr() as *const Process) };
    proc.address_space()
}

/// `sys_memory_create(size, flags)` — a zero-filled `MemoryObject` of `size` bytes (page-rounded)
/// with a full-rights handle. `flags` must be 0.
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
            // `allocate` did not adopt the creation reference; drop it outside the table call.
            // SAFETY: `ptr` carries the single outstanding creation reference.
            drop(unsafe { ObjectRef::from_raw(ptr, KObjectType::MemoryObject) });
            Err(map_handle_err(e))
        }
    }
}

/// `sys_memory_map(obj, hint, size, rights)` — map `obj`'s frames into the caller's address
/// space with `rights` (a subset of the `MAP_*` band). `hint == 0` picks any free range;
/// otherwise `hint` is the requested page-aligned base. Returns the mapped base address.
pub fn sys_memory_map(obj_h: u64, hint: u64, size: u64, rights: u64) -> SysResult {
    let req = Rights::from_bits_truncate(rights);
    // The handle must carry the `MAP_*` bits being installed; amplification is `NoAccess`.
    let required = req & (Rights::MAP_READ | Rights::MAP_WRITE | Rights::MAP_EXEC);

    let pid = crate::sched::current_owner_pid();
    let proc_ref = crate::sched::current_process().ok_or(KError::KernelError)?;
    let asp = current_address_space(&proc_ref).ok_or(KError::KernelError)?;

    // A `MemoryObject` (eager) or demand-paged `FileObject`; others lack the `MAP_*` right.
    let ok = global::get().lookup(RawHandle(obj_h), pid, required).map_err(map_handle_err)?;
    let obj_ty = ok.object.object_type();
    let max_bytes = match obj_ty {
        // SAFETY: type-confirmed live object pinned by `lookup`; read its size.
        KObjectType::MemoryObject => unsafe { &*(ok.object.as_ptr() as *const MemoryObject) }.size(),
        // SAFETY: as above.
        KObjectType::FileObject => unsafe { &*(ok.object.as_ptr() as *const FileObject) }.npages() * PAGE_SIZE,
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
        // The stack guard gap must bind hinted mappings too: `MMAP_MAX` bounds only the
        // `hint == 0` search, so an overrun into a hinted mapping would land in mapped memory
        // silently. `in_stack_guard` is host-tested; this call is not covered end to end.
        if AddressSpace::in_stack_guard(hint, end) {
            return Err(KError::InvalidArgument);
        }
        VAddrRange::new(start, VirtAddr::new(end)).ok_or(KError::InvalidArgument)?
    };

    let mut prot = Protection::USER;
    if req.contains(Rights::MAP_WRITE) {
        prot = prot | Protection::WRITE;
    }
    if req.contains(Rights::MAP_EXEC) {
        prot = prot | Protection::EXEC;
    }

    // `MemoryObject` maps eagerly; `FileObject` **lazily** (`fault_in` pages in on first touch).
    let result = match obj_ty {
        KObjectType::FileObject => asp.map_file(range, prot, ok.object),
        _ => asp.map_object(range, prot, ok.object),
    };
    match result {
        Ok(()) => {
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

/// `sys_memory_unmap(addr, size)` — unmap the region at `addr` from the caller's address
/// space. Unmaps the **whole** VMA covering `addr`; `size` is not yet honored for partial
/// unmaps (TODO(mm)). Returns 0, or `InvalidArgument` if nothing is mapped at `addr`.
pub fn sys_memory_unmap(addr: u64, _size: u64) -> SysResult {
    let va = VirtAddr::new(addr);
    if !va.is_page_aligned() || addr >= USER_VIRT_END {
        return Err(KError::InvalidArgument);
    }
    let proc_ref = crate::sched::current_process().ok_or(KError::KernelError)?;
    let asp = current_address_space(&proc_ref).ok_or(KError::KernelError)?;
    let mut frames: crate::libkern::KVec<crate::mm::PhysAddr> = crate::libkern::KVec::new();
    let vma = loop {
        match asp.unmap_covering_deferred(va, &mut frames) {
            Ok(Some(vma)) => break vma,
            Ok(None) => return Err(KError::InvalidArgument),
            Err(pages) => frames.try_reserve(pages as usize).map_err(|_| KError::OutOfMemory)?,
        }
    };
    // Another CPU may still hold translations: invalidate everywhere BEFORE any frame becomes
    // reusable, then free the frames and drop the VMA. IF-masked; `shootdown_all` is IF-robust.
    crate::tlb::shootdown_all();
    for &f in frames.iter() {
        crate::mm::heap::buddy_free(f, 0);
    }
    drop(vma);
    Ok(0)
}

/// `sys_clock_read(clock, out)` — write the selected clock's value, in nanoseconds, to the user
/// `u64` at `out`. Returns 0.
///
/// `Realtime` is serviced once anchored from the RTC at boot ([`crate::clock`]) or set, else
/// `Unsupported`. Reading time-of-day is deliberately **ambient**; *setting* it needs
/// `SYSTEM_CLOCK`.
///
/// TODO(sched-acct): `ProcessCpu`/`ThreadCpu` need per-thread CPU accounting from the
/// scheduler tick (none yet). See docs/planning/implementation-plan.md.
pub fn sys_clock_read(clock: u64, out: u64) -> SysResult {
    let id = u32::try_from(clock).ok().and_then(ClockId::from_u32).ok_or(KError::InvalidArgument)?;
    let uptr = UserMutPtr::<u64>::new(out).map_err(from_user_access)?;
    let ns = match id {
        ClockId::Monotonic => Timer::read_ns(),
        // Never negative: the RTC anchor is validated as a 21st-century date.
        ClockId::Realtime => crate::clock::realtime_ns().ok_or(KError::Unsupported)? as u64,
        ClockId::ProcessCpu | ClockId::ThreadCpu => {
            return Err(KError::Unsupported);
        }
    };
    copy_to_user(uptr, &ns).map_err(from_user_access)?;
    Ok(0)
}

/// `sys_clock_set(clock, ns)` — **set the wall clock** to `ns` nanoseconds since the Unix epoch,
/// and write the machine's RTC so the next boot anchors to it.
///
/// - `clock` must be [`ClockId::Realtime`], and `ns` in 2000–2099 ([`crate::clock::settable`]),
///   else `InvalidArgument`.
/// - The caller must hold `SYSTEM_CLOCK`, or `NoAccess`; checked after the arguments, so a
///   refusal of either is reachable without the authority.
///
/// Returns `CLOCK_SET_KEPT` when the RTC read back what was written, or `CLOCK_SET_THIS_BOOT`
/// when it did not (the clock is set either way). **It steps the clock**, backwards as readily
/// as forwards. Every set is logged.
pub fn sys_clock_set(clock: u64, ns: u64) -> SysResult {
    let id = u32::try_from(clock).ok().and_then(ClockId::from_u32);
    if id != Some(ClockId::Realtime) {
        return Err(KError::InvalidArgument);
    }
    let secs = crate::clock::settable(ns).ok_or(KError::InvalidArgument)?;
    require_syscap(crate::libkern::SysCaps::SYSTEM_CLOCK)?;
    let kept = crate::clock::set(ns);
    if kept {
        crate::kprintln!("wall clock: set to {} (Unix epoch seconds, UTC); the RTC holds it", secs);
        Ok(crate::libkern::clock::CLOCK_SET_KEPT as isize)
    } else {
        crate::kprintln!(
            "wall clock: set to {} (Unix epoch seconds, UTC); the RTC did not take it, so it lasts \
             until the next boot",
            secs
        );
        Ok(crate::libkern::clock::CLOCK_SET_THIS_BOOT as isize)
    }
}

fn timer_rights() -> Rights {
    Rights::WAIT | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER
}

fn pending_op_rights() -> Rights {
    Rights::WAIT | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER
}

/// Rights a fresh `Namespace` handle carries: `LOOKUP | BIND | UNBIND` plus the management band.
fn namespace_rights() -> Rights {
    Rights::LOOKUP | Rights::BIND | Rights::UNBIND | Rights::DUPLICATE | Rights::TRANSFER | Rights::INSPECT
}

/// Map a [`NsError`] to the user-facing [`KError`]: `InvalidPath` is `InvalidArgument`,
/// `NotBound` is `NotFound`, `AlreadyBound` is `AlreadyExists` (distinct from `InvalidPath`, so
/// a supervisor can tell "that path is taken" from "that path is malformed").
fn map_ns_err(e: NsError) -> KError {
    match e {
        NsError::InvalidPath => KError::InvalidArgument,
        NsError::AlreadyBound => KError::AlreadyExists,
        NsError::NotBound => KError::NotFound,
        NsError::OutOfMemory => KError::OutOfMemory,
    }
}

/// Copy a namespace path from user memory into `buf`, returning the populated slice. `path_len`
/// must be in `1..=NS_PATH_MAX` (`InvalidArgument` / `TooLarge` otherwise); the path *grammar*
/// is the caller's to validate ([`validate_path`] / `Namespace::bind`).
fn copy_ns_path(path_ptr: u64, path_len: usize, buf: &mut [u8; NS_PATH_MAX]) -> Result<&[u8], KError> {
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

/// `sys_timer_create(flags)` — create an unarmed `Timer`; `flags` must be 0.
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
            // SAFETY: `ptr` carries the single outstanding creation reference.
            drop(unsafe { ObjectRef::from_raw(ptr, KObjectType::Timer) });
            Err(map_handle_err(e))
        }
    }
}

/// `sys_timer_set(timer, deadline_ns, interval_ns)` — arm `timer` to fire at the absolute
/// monotonic deadline `deadline_ns` (`0` disarms), re-arming every `interval_ns` (`0` =
/// one-shot). Returns 0. Needs only handle ownership: `WAIT` gates `sys_wait`, not arming.
pub fn sys_timer_set(timer_h: u64, deadline_ns: u64, interval_ns: u64) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    let ok = lookup_typed(timer_h, pid, Rights::empty(), KObjectType::Timer)?;
    // SAFETY: `ok.object` is the live `Timer` its lookup type-checked, held across the call.
    unsafe { crate::sched::timer_arm(ok.object.as_ptr(), deadline_ns, interval_ns) }
        .map_err(|()| KError::OutOfMemory)?;
    Ok(0)
}

/// `sys_ns_create()` — create an empty [`Namespace`]; the handle carries [`namespace_rights`].
pub fn sys_ns_create() -> SysResult {
    let obj = Namespace::try_new().map_err(|_| KError::OutOfMemory)?;
    let ptr = KBox::into_raw(obj).as_ptr() as *mut ();
    let pid = crate::sched::current_owner_pid();
    match global::get().allocate(pid, ptr, KObjectType::Namespace, namespace_rights()) {
        Ok(h) => Ok(h.bits() as isize),
        Err(e) => {
            // SAFETY: `ptr` carries the single outstanding creation reference.
            drop(unsafe { ObjectRef::from_raw(ptr, KObjectType::Namespace) });
            Err(map_handle_err(e))
        }
    }
}

/// `sys_ns_derive(ns)` — create a new [`Namespace`] holding a **copy of `ns`'s bindings** and
/// return a full-rights handle to it. Requires `LOOKUP` on `ns`.
///
/// **`LOOKUP` is enough because a copy binds nothing a caller could not already resolve**;
/// binding into it still needs `BIND_NAMESPACE`. A process's own root arrives `LOOKUP`-only
/// and cannot be sent, so this is how a program hands someone its namespace. **`UNBIND` on the
/// copy can widen what it reaches**: resolution is longest-prefix, so removing a narrower
/// binding exposes the broader one beneath it
/// (`docs/architecture/namespace-and-resource-servers.md`). The copy is a snapshot
/// ([`Namespace::try_derive`]).
pub fn sys_ns_derive(ns_h: u64) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    let ns_ok = lookup_typed(ns_h, pid, Rights::LOOKUP, KObjectType::Namespace)?;
    // SAFETY: live `Namespace` (type verified by `lookup_typed`), pinned by `ns_ok`.
    let ns: &Namespace = unsafe { &*(ns_ok.object.as_ptr() as *const Namespace) };
    let obj = ns.try_derive().map_err(|_| KError::OutOfMemory)?;
    let ptr = KBox::into_raw(obj).as_ptr() as *mut ();
    match global::get().allocate(pid, ptr, KObjectType::Namespace, namespace_rights()) {
        Ok(h) => Ok(h.bits() as isize),
        Err(e) => {
            // SAFETY: `ptr` carries the single outstanding creation reference.
            drop(unsafe { ObjectRef::from_raw(ptr, KObjectType::Namespace) });
            Err(map_handle_err(e))
        }
    }
}

/// Require the calling process to hold system capability `cap` (`docs/architecture/syscaps.md`),
/// else [`KError::NoAccess`]. Checked alongside, not instead of, per-handle `Rights`.
fn require_syscap(cap: crate::libkern::SysCaps) -> Result<(), KError> {
    let proc = crate::sched::current_process().ok_or(KError::KernelError)?;
    // SAFETY: `proc` pins a live `Process`; read its immutable syscap set.
    let held = unsafe { &*(proc.as_ptr() as *const Process) }.syscaps();
    if held.contains(cap) { Ok(()) } else { Err(KError::NoAccess) }
}

/// Whether a handle carrying `rights` may be bound as a **direct handle**: only one that may
/// leave its process, so `NoAccess` without `TRANSFER`. A direct bind gives the object away
/// (every resolve mints a handle to it), as an IPC send and a spawn grant do; `init`'s
/// system-control object carries no `TRANSFER` so it cannot leave `init`. A **userspace-server**
/// bind needs no right: the kernel keeps the endpoint.
fn direct_bindable(rights: Rights) -> Result<(), KError> {
    if rights.contains(Rights::TRANSFER) { Ok(()) } else { Err(KError::NoAccess) }
}

/// `sys_ns_bind(ns, path, path_len, resource, base, base_len)` — bind `resource` at `path` in
/// namespace `ns`: an `IpcChannel` as a userspace server, anything else as a direct handle.
/// Requires `BIND` on `ns`, the `BIND_NAMESPACE` syscap, and, for a direct handle, `TRANSFER`
/// ([`direct_bindable`]). The binding takes a **clone** of the resource's reference (the caller's
/// handle stays valid) and caps its rights at the handle's current rights. Returns 0.
pub fn sys_ns_bind(
    ns_h: u64,
    path_ptr: u64,
    path_len: usize,
    resource_h: u64,
    base_ptr: u64,
    base_len: usize,
) -> SysResult {
    // An additional gate atop the `BIND` right (`docs/architecture/syscaps.md`).
    require_syscap(crate::libkern::SysCaps::BIND_NAMESPACE)?;
    let pid = crate::sched::current_owner_pid();
    let ns_ok = lookup_typed(ns_h, pid, Rights::BIND, KObjectType::Namespace)?;
    let mut buf = [0u8; NS_PATH_MAX];
    let path = copy_ns_path(path_ptr, path_len, &mut buf)?;

    // Optional subtree scoping (a4/a5 = base ptr/len), userspace-server binds only: a lookup's
    // suffix is prepended with `base` before forwarding (docs/architecture/session-and-auth.md).
    let mut base_buf = [0u8; NS_PATH_MAX];
    let base = if base_len > 0 {
        let bpath = copy_ns_path(base_ptr, base_len, &mut base_buf)?;
        SubtreeBase::from_path(bpath).map_err(|_| KError::InvalidArgument)?
    } else {
        SubtreeBase::empty()
    };

    let res_ok = global::get().lookup(RawHandle(resource_h), pid, Rights::empty()).map_err(map_handle_err)?;
    let rights = res_ok.rights;
    // SAFETY: `ns_ok.object` addresses a live `Namespace` — `lookup_typed`
    // verified the type tag and the refcount pins it for this call.
    let ns: &Namespace = unsafe { &*(ns_ok.object.as_ptr() as *const Namespace) };

    // An `IpcChannel` binds as a **Userspace Server** (the kernel adopts the endpoint); any
    // other type binds as a direct handle (`docs/architecture/namespace-and-resource-servers.md`).
    if res_ok.object.object_type() == KObjectType::IpcChannel {
        // Bind-mount semantics: an endpoint that already backs a registration is *shared*; a
        // rival would clobber the endpoint→reg reply-routing back-pointer and hijack replies.
        // SAFETY: `res_ok.object` is the live `IpcChannel` its lookup type-checked, held across the call.
        if let Some(existing) = unsafe { crate::sched::us_forward_existing_reg(res_ok.object.as_ptr()) } {
            return match ns.bind_userspace_server(path, existing, rights, base) {
                Ok(()) => Ok(0),
                Err((returned, e)) => {
                    drop(returned);
                    Err(map_ns_err(e))
                }
            };
        }
        let reg_box = UserspaceServerReg::try_new(res_ok.object.clone()).map_err(|_| KError::OutOfMemory)?;
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        let reg_ref = unsafe {
            ObjectRef::from_raw(KBox::into_raw(reg_box).as_ptr() as *mut (), KObjectType::UserspaceServerReg)
        };
        // Held across the attach: an unbind on another thread can drop the binding's reference.
        let keep = reg_ref.clone();
        return match ns.bind_userspace_server(path, reg_ref, rights, base) {
            Ok(()) => {
                // Record the endpoint → registration back-pointer (only on success).
                // SAFETY: `res_ok.object` is the live `IpcChannel` its lookup type-checked, held across
                // the call.
                unsafe { crate::sched::us_server_attach(res_ok.object.as_ptr(), &keep) };
                Ok(0)
            }
            Err((returned, e)) => {
                drop(returned);
                Err(map_ns_err(e))
            }
        };
    }

    if !base.is_empty() {
        return Err(KError::InvalidArgument);
    }
    direct_bindable(rights)?;
    let target = res_ok.object.clone();
    match ns.bind(path, target, rights) {
        Ok(()) => Ok(0),
        Err((returned, e)) => {
            drop(returned);
            Err(map_ns_err(e))
        }
    }
}

/// `sys_ns_unbind(ns, path, path_len)` — remove the binding at `path`. Requires `UNBIND` on
/// `ns`; `NotFound` if nothing is bound there. Returns 0.
pub fn sys_ns_unbind(ns_h: u64, path_ptr: u64, path_len: usize) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    let ns_ok = lookup_typed(ns_h, pid, Rights::UNBIND, KObjectType::Namespace)?;
    let mut buf = [0u8; NS_PATH_MAX];
    let path = copy_ns_path(path_ptr, path_len, &mut buf)?;
    // SAFETY: live `Namespace` (type verified by `lookup_typed`).
    let ns: &Namespace = unsafe { &*(ns_ok.object.as_ptr() as *const Namespace) };
    match ns.unbind(path) {
        Some(target) => {
            drop(target);
            Ok(0)
        }
        None => Err(KError::NotFound),
    }
}

/// `sys_ns_enumerate(ns, index, out)` — write the `index`-th binding of `ns` (insertion order)
/// to the user [`NsEntry`] at `out`: path, target kind (`NS_KIND_*`) and rights. Requires
/// `LOOKUP`. Returns `0`, or `NotFound` when `index` is past the binding count (the iteration
/// terminator). Lists **bindings**, not filesystem `readdir`.
pub fn sys_ns_enumerate(ns_h: u64, index: u64, out: u64) -> SysResult {
    let uptr = UserMutPtr::<NsEntry>::new(out).map_err(from_user_access)?;
    let pid = crate::sched::current_owner_pid();
    let ns_ok = lookup_typed(ns_h, pid, Rights::LOOKUP, KObjectType::Namespace)?;
    // SAFETY: live `Namespace` (type verified by `lookup_typed`).
    let ns: &Namespace = unsafe { &*(ns_ok.object.as_ptr() as *const Namespace) };
    let mut entry = NsEntry::zeroed();
    if !ns.enumerate(index as usize, &mut entry) {
        return Err(KError::NotFound);
    }
    copy_to_user(uptr, &entry).map_err(from_user_access)?;
    Ok(0)
}

/// Per-call cap on `sys_entropy_read`; bounds the on-stack bounce buffer.
const ENTROPY_READ_MAX: usize = 256;

fn entropy_rights() -> Rights {
    Rights::READ | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER
}

/// `sys_entropy_create()` — create an [`EntropyObject`] token onto the kernel CSPRNG.
pub fn sys_entropy_create() -> SysResult {
    let obj = EntropyObject::try_new().map_err(|_| KError::OutOfMemory)?;
    let ptr = KBox::into_raw(obj).as_ptr() as *mut ();
    let pid = crate::sched::current_owner_pid();
    match global::get().allocate(pid, ptr, KObjectType::EntropyObject, entropy_rights()) {
        Ok(h) => Ok(h.bits() as isize),
        Err(e) => {
            // SAFETY: `ptr` carries the single outstanding creation reference.
            drop(unsafe { ObjectRef::from_raw(ptr, KObjectType::EntropyObject) });
            Err(map_handle_err(e))
        }
    }
}

/// `sys_entropy_read(ent, buf, len)` — fill `buf[0..len]` with CSPRNG output. Requires `READ`
/// on `ent`. `len` is capped at [`ENTROPY_READ_MAX`].
///
/// Returns `0` when filled synchronously (the common case: the pool seeds at boot). A
/// **positive** value is a [`PendingOperation`] handle: the pool is not yet seeded (hardware
/// without `RDSEED`/`RDRAND`), so the caller `sys_wait`s on it and **re-reads**. A negative
/// value is a [`KError`]; handles are ≥ 1, so the cases are unambiguous.
pub fn sys_entropy_read(ent_h: u64, buf_ptr: u64, len: usize) -> SysResult {
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
        // Wipe the bounce buffer so no entropy lingers on the kernel stack.
        let mut buf = [0u8; ENTROPY_READ_MAX];
        crate::entropy::fill(&mut buf[..len]);
        let r = copy_slice_to_user(uptr, &buf[..len]).map_err(from_user_access);
        buf.iter_mut().for_each(|b| *b = 0);
        r?;
        return Ok(0);
    }

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
            // SAFETY: `r` is the seed waiter's `PendingOperation` reference.
            unsafe { crate::sched::complete_pending_op(r.as_ptr(), 0, 0) };
            drop(r);
        }
        crate::entropy::SeedWaitReg::Full(r) => {
            // Too many concurrent unseeded readers: `WouldBlock` is the status the caller reads.
            // SAFETY: `r` is the seed waiter's `PendingOperation` reference.
            unsafe { crate::sched::complete_pending_op(r.as_ptr(), KError::WouldBlock as i32, 0) };
            drop(r);
        }
    }
    Ok(po_h.bits() as isize)
}

/// `sys_ns_lookup(ns, path, path_len, rights)` — resolve `path` in `ns` (longest-prefix match),
/// requesting at most `rights`. Requires `LOOKUP` on `ns`. Returns a [`PendingOperation`]
/// handle; the completion delivers the resolved resource handle in `IoResult.result` (rights =
/// `requested ∩ binding.rights`) with `status == 0`, or a `NotFound` `status`.
///
/// Argument / permission / allocation failures return **synchronously** (no PO); resolution
/// failures (no covering binding, or a non-empty suffix on a direct-handle leaf) arrive
/// **through** the PO.
pub fn sys_ns_lookup(ns_h: u64, path_ptr: u64, path_len: usize, rights_bits: u64, op: ResolveOp) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    let requested = Rights::from_bits_truncate(rights_bits);
    let ns_ok = lookup_typed(ns_h, pid, Rights::LOOKUP, KObjectType::Namespace)?;
    let mut buf = [0u8; NS_PATH_MAX];
    let path = copy_ns_path(path_ptr, path_len, &mut buf)?;
    validate_path(path).map_err(|_| KError::InvalidArgument)?;

    let mut dest_buf = [0u8; NS_PATH_MAX];
    let dest: Option<&[u8]> = match op {
        ResolveOp::Rename { dest_ptr, dest_len, .. } => {
            let d = copy_ns_path(dest_ptr, dest_len, &mut dest_buf)?;
            validate_path(d).map_err(|_| KError::InvalidArgument)?;
            Some(d)
        }
        _ => None,
    };

    let po = PendingOperation::try_new().map_err(|_| KError::OutOfMemory)?;
    // SAFETY: `into_raw` yields the single creation reference; adopt it.
    let po_ref = unsafe { ObjectRef::from_raw(KBox::into_raw(po).as_ptr() as *mut (), KObjectType::PendingOperation) };
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

    // SAFETY: live `Namespace` (type verified by `lookup_typed`).
    let ns: &Namespace = unsafe { &*(ns_ok.object.as_ptr() as *const Namespace) };
    if let Some((status, result)) = resolve_and_start(ns, path, dest, op.forward_op(), requested, pid, &po_ref, 0) {
        // SAFETY: `po_ref` pins the `PendingOperation` at `po_ptr`.
        unsafe { crate::sched::complete_pending_op(po_ptr, status, result) };
    }
    Ok(po_h.bits() as isize)
}

impl ResolveOp {
    fn forward_op(self) -> ForwardOp {
        match self {
            ResolveOp::Plain => ForwardOp::Plain,
            ResolveOp::Size(size, change) => ForwardOp::Size(size, change),
            ResolveOp::Rename { flags, .. } => ForwardOp::Rename { flags },
        }
    }
}

fn install_for(pid: u32, requested: Rights, obj: ObjectRef, binding_rights: Rights) -> (i32, u64) {
    let (tptr, tty) = obj.into_raw();
    match global::get().allocate(pid, tptr, tty, requested & binding_rights) {
        Ok(h) => (0, h.bits()),
        Err(e) => {
            // SAFETY: `allocate` did not adopt the reference; reclaim it.
            drop(unsafe { ObjectRef::from_raw(tptr, tty) });
            (map_handle_err(e) as i32, 0)
        }
    }
}

/// **Resolve `path` in `ns` and start the operation there**: the half of a lookup that a
/// `SUBNAMESPACE` reply repeats in another namespace. A direct binding installs its object, a
/// kernel server answers, a userspace server is sent the request. `Some((status, result))`
/// completes the lookup's PO now; `None` leaves it pending on the server's reply.
///
/// `depth` counts the `SUBNAMESPACE` replies followed. Past `0` this runs in the replying
/// server's syscall, so **a kernel server is refused there**: `/proc/self` answers for whoever
/// is calling, which would hand the caller the server's process, thread or namespace.
///
/// A rename (`dest` is a path in `ns`) must land on the same userspace server and subtree base
/// as its source, else `Unsupported` (the cue to fall back to copy + unlink). No *write*
/// authority is checked here: a userspace-server binding's rights describe the endpoint, not
/// the files behind it (`TODO(mount-write-authority)`).
#[allow(clippy::too_many_arguments, reason = "a resolve's request, its caller and the operation it completes")]
fn resolve_and_start(
    ns: &Namespace,
    path: &[u8],
    dest: Option<&[u8]>,
    op: ForwardOp,
    requested: Rights,
    pid: u32,
    po_ref: &ObjectRef,
    depth: u8,
) -> Option<(i32, u64)> {
    let mut dest_buf = [0u8; NS_PATH_MAX];
    let mut dest_len = 0usize;
    if let ForwardOp::Rename { .. } = op {
        let Some(d) = dest else {
            return Some((KError::InvalidArgument as i32, 0));
        };
        let verdict = match (ns.resolve(path), ns.resolve(d)) {
            (
                Some((ResolvedTarget::UserspaceServer(sreg, sbase), _, _)),
                Some((ResolvedTarget::UserspaceServer(dreg, dbase), _, dsuffix)),
            ) => {
                let same = sreg.as_ptr() == dreg.as_ptr() && sbase.as_path() == dbase.as_path();
                let joined = if dbase.as_path().is_empty() {
                    dest_buf.get_mut(..dsuffix.len()).map(|b| {
                        b.copy_from_slice(dsuffix);
                        dsuffix.len()
                    })
                } else {
                    let mut jb = [0u8; NS_PATH_MAX];
                    join_subtree(dbase.as_path(), dsuffix, &mut jb).map(|j| {
                        let n = j.len();
                        dest_buf[..n].copy_from_slice(j);
                        n
                    })
                };
                drop(sreg);
                drop(dreg);
                match (same, joined) {
                    (false, _) => Some(KError::Unsupported),
                    (_, None) => Some(KError::TooLarge),
                    (true, Some(n)) => {
                        dest_len = n;
                        None
                    }
                }
            }
            _ => Some(KError::Unsupported),
        };
        // A refused rename short-circuits the resolve, which would hand back the source.
        if let Some(e) = verdict {
            return Some((e as i32, 0));
        }
    }

    match ns.resolve(path) {
        None => Some((KError::NotFound as i32, 0)),
        Some((ResolvedTarget::DirectHandle(target), binding_rights, suffix)) => {
            if suffix.is_empty() {
                Some(install_for(pid, requested, target, binding_rights))
            } else {
                drop(target);
                Some((KError::NotFound as i32, 0))
            }
        }
        // Not from a server's syscall: `/proc/self` would answer for the server.
        Some((ResolvedTarget::KernelServer(_), _, _)) if depth > 0 => Some((KError::Unsupported as i32, 0)),
        Some((ResolvedTarget::KernelServer(id), binding_rights, suffix)) => {
            match kernel_server::dispatch(id, suffix, requested) {
                OpStatus::Completed(obj) => Some(install_for(pid, requested, obj, binding_rights)),
                OpStatus::Rejected(err) => Some((err as i32, 0)),
                OpStatus::Pending => {
                    // A Kernel Server never forwards; an internal error, not a panic.
                    debug_assert!(false, "kernel-server dispatch returned Pending");
                    Some((KError::KernelError as i32, 0))
                }
            }
        }
        Some((ResolvedTarget::UserspaceServer(reg, base), _binding_rights, suffix)) => {
            // Forward over IPC and leave the PO pending (the reply completes it inline); a
            // synchronous failure completes it now. A subtree binding prepends its `base`; both
            // `base` and `suffix` are validated (no `.`/`..`), so the join cannot escape.
            let fwd = Forward { op, dest: &dest_buf[..dest_len], depth };
            if base.is_empty() {
                forward_userspace_lookup(reg, po_ref, pid, requested, suffix, fwd)
            } else {
                let mut jbuf = [0u8; NS_PATH_MAX];
                match join_subtree(base.as_path(), suffix, &mut jbuf) {
                    Some(joined) => forward_userspace_lookup(reg, po_ref, pid, requested, joined, fwd),
                    None => {
                        // Overflow: drop the forwarding reference outside the namespace lock.
                        drop(reg);
                        Some((KError::TooLarge as i32, 0))
                    }
                }
            }
        }
    }
}

/// Join a subtree binding's `base` (an absolute path) with a resolved server-relative `suffix`,
/// producing the path to forward (`/home/alice` + `notes.txt` → `home/alice/notes.txt`). `None`
/// if it would exceed `out`. Callers guarantee both are validated (no `.`/`..`).
fn join_subtree<'o>(base: &[u8], suffix: &[u8], out: &'o mut [u8]) -> Option<&'o [u8]> {
    let base_rel = &base[1..];
    let joined_len = if suffix.is_empty() { base_rel.len() } else { base_rel.len() + 1 + suffix.len() };
    if joined_len > out.len() {
        return None;
    }
    out[..base_rel.len()].copy_from_slice(base_rel);
    if !suffix.is_empty() {
        out[base_rel.len()] = b'/';
        out[base_rel.len() + 1..joined_len].copy_from_slice(suffix);
    }
    Some(&out[..joined_len])
}

/// Forward a namespace lookup that resolved to a userspace-server binding to its server over
/// IPC: build the `Namespace::Resolve` request and originate it (recording the lookup with
/// `fwd` so a `SUBNAMESPACE` reply can carry it on). `None` leaves the lookup PO **pending**;
/// `Some((status, 0))` completes it now with an error. `reg` is dropped here, outside the
/// namespace lock; `suffix` is the path past the mount prefix.
fn forward_userspace_lookup(
    reg: ObjectRef,
    po_ref: &ObjectRef,
    pid: u32,
    requested: Rights,
    suffix: &[u8],
    fwd: Forward<'_>,
) -> Option<(i32, u64)> {
    // Heap-bounced (4 KiB, never on the stack); a grow or create request also carries the size.
    // SAFETY: `StoredMsg` is plain `#[repr(C)]` data; all-zero is a valid value. Zeroed
    // in place so the 4 KiB message never lands in this frame (see `try_new_zeroed`).
    let mut msg = match unsafe { KBox::<StoredMsg>::try_new_zeroed() } {
        Ok(m) => m,
        Err(_) => return Some((KError::OutOfMemory as i32, 0)),
    };
    let built = match fwd.op {
        ForwardOp::Size(new_size, change) => {
            crate::rsproto::build_resolve_request_sized(&mut msg.payload, requested.bits(), suffix, new_size, change)
        }
        ForwardOp::Rename { flags } => {
            crate::rsproto::build_resolve_request_rename(&mut msg.payload, requested.bits(), suffix, fwd.dest, flags)
        }
        ForwardOp::Plain => crate::rsproto::build_resolve_request(&mut msg.payload, requested.bits(), suffix),
    };
    let body_len = match built {
        Some(n) => n,
        None => return Some((KError::TooLarge as i32, 0)),
    };
    msg.header.payload_len = body_len as u32;
    msg.header.handle_count = 0;

    // The suffix is stored so a lazy `FILE` reply can name the file, and a `SUBNAMESPACE` reply
    // can continue the lookup.
    // SAFETY: `reg` is a live `UserspaceServerReg` reference held across the call.
    match unsafe { crate::sched::us_forward_originate(reg.as_ptr(), &mut msg, po_ref, pid, requested, suffix, fwd) } {
        crate::sched::ForwardOutcome::Pending => None,
        crate::sched::ForwardOutcome::Busy | crate::sched::ForwardOutcome::Full => Some((KError::WouldBlock as i32, 0)),
        crate::sched::ForwardOutcome::PeerClosed => Some((KError::PeerClosed as i32, 0)),
    }
}

/// `sys_io_submit(resource, op)` — initiate the [`IoOp`] `*op` against `resource` (a block
/// [`DeviceNode`](crate::object::DeviceNode) or char device) and return a `PendingOperation`
/// handle. Never blocks: the outcome (status + bytes transferred) arrives through the PO.
/// Argument / permission / allocation failures return a negative `KError` synchronously with no
/// PO; device/medium failures arrive through the PO (`docs/spec/io-operation.md`).
pub fn sys_io_submit(resource_h: u64, op_ptr: u64) -> SysResult {
    let pid = crate::sched::current_owner_pid();

    let optr = UserPtr::<IoOp>::new(op_ptr).map_err(from_user_access)?;
    let op = copy_from_user(optr).map_err(from_user_access)?;
    if op.flags != 0 {
        return Err(KError::InvalidArgument);
    }
    let opcode = IoOpcode::from_u32(op.opcode).ok_or(KError::InvalidArgument)?;

    let (dev_right, buf_right) = match opcode {
        IoOpcode::Read => (Rights::READ, Rights::MAP_WRITE),
        IoOpcode::Write => (Rights::WRITE, Rights::MAP_READ),
        IoOpcode::Flush => return submit_control(resource_h, &op, pid, crate::io::irp::IrpOp::Flush),
        IoOpcode::Rescan => return submit_control(resource_h, &op, pid, crate::io::irp::IrpOp::Rescan),
        IoOpcode::Eject => return submit_control(resource_h, &op, pid, crate::io::irp::IrpOp::Eject),
    };
    let dev_ok = lookup_typed(resource_h, pid, dev_right, KObjectType::DeviceNode)?;
    let buf_ok = lookup_typed(op.buffer, pid, buf_right, KObjectType::MemoryObject)?;

    // SAFETY: `buf_ok` pins a live `MemoryObject`.
    let buffer_size = unsafe { &*(buf_ok.object.as_ptr() as *const MemoryObject) }.size() as u64;
    let buf_end = op.buf_offset.checked_add(op.length).ok_or(KError::InvalidArgument)?;
    if buf_end > buffer_size {
        return Err(KError::InvalidArgument);
    }

    let po_box = PendingOperation::try_new().map_err(|_| KError::OutOfMemory)?;
    // SAFETY: adopt the single creation reference.
    let po_ref =
        unsafe { ObjectRef::from_raw(KBox::into_raw(po_box).as_ptr() as *mut (), KObjectType::PendingOperation) };
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

    if op.length == 0 {
        // SAFETY: `po_ref` pins the `PendingOperation`.
        unsafe { crate::sched::complete_pending_op(po_ref.as_ptr(), 0, 0) };
        return Ok(po_h.bits() as isize);
    }

    // A block device goes through the IRP path; a char device through its backend. On failure
    // the PO handle is rolled back.
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
        DeviceClass::Char if opcode == IoOpcode::Write => {
            match dn.char_backend().and_then(|b| Some((b.submit_write?, b.ctx))) {
                Some((write, ctx)) => write(&buf_ok.object, &po_ref, op.buf_offset, op.length, ctx),
                None => Err(KError::Unsupported),
            }
        }
        DeviceClass::Char if opcode == IoOpcode::Read => match dn.char_backend() {
            Some(backend) => {
                (backend.submit_read)(&buf_ok.object, &po_ref, op.buf_offset, op.offset, op.length, backend.ctx)
            }
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

/// `sys_io_submit`'s **flush**: make what has been written to the block device `resource_h`
/// durable, completing the returned `PendingOperation` once the device has written its cache.
/// Needs `WRITE` on the device and names no buffer and no range, so each of those fields must
/// be `0`. Not a zero-length transfer, so it never takes that path's pre-signalled no-op.
///
/// **And its rescan and its eject**, `irp_op` saying which, with the same rules (`WRITE`). A
/// partition and every disk but a USB one answer `Unsupported`, through the IRP.
fn submit_control(resource_h: u64, op: &IoOp, pid: u32, irp_op: crate::io::irp::IrpOp) -> SysResult {
    if op.buffer != 0 || op.buf_offset != 0 || op.offset != 0 || op.length != 0 {
        return Err(KError::InvalidArgument);
    }
    let dev_ok = lookup_typed(resource_h, pid, Rights::WRITE, KObjectType::DeviceNode)?;
    // SAFETY: `dev_ok.object` pins a live `DeviceNode` (type-checked above).
    if unsafe { &*(dev_ok.object.as_ptr() as *const DeviceNode) }.class() != DeviceClass::Block {
        return Err(KError::Unsupported);
    }
    let po_box = PendingOperation::try_new().map_err(|_| KError::OutOfMemory)?;
    // SAFETY: adopt the single creation reference.
    let po_ref =
        unsafe { ObjectRef::from_raw(KBox::into_raw(po_box).as_ptr() as *mut (), KObjectType::PendingOperation) };
    let (tptr, tty) = po_ref.clone().into_raw();
    let po_h = match global::get().allocate(pid, tptr, tty, pending_op_rights()) {
        Ok(h) => h,
        Err(e) => {
            // SAFETY: `allocate` did not adopt this reference; reclaim it.
            drop(unsafe { ObjectRef::from_raw(tptr, tty) });
            return Err(map_handle_err(e));
        }
    };
    match crate::io::block::dispatch_block_control(&dev_ok.object, &po_ref, irp_op) {
        Ok(()) => Ok(po_h.bits() as isize),
        Err(e) => {
            close_and_release(po_h, pid);
            Err(e)
        }
    }
}

/// `sys_io_cancel(pending)` — request cancellation of an in-flight operation. **Always
/// `Unsupported`**; the number is reserved.
pub fn sys_io_cancel(_pending_h: u64) -> SysResult {
    Err(KError::Unsupported)
}

/// `sys_wait(handles, count, results, deadline)` — block until ≥1 of `handles[0..count]`
/// signals or `deadline` (absolute monotonic ns; `0` = poll, `u64::MAX` = no timeout) elapses.
/// Each handle needs `WAIT` and must be a `Timer`, `NotificationChannel`, `IpcChannel`,
/// `PendingOperation` or `InterruptObject` (else `Unsupported`). Writes one [`IoResult`] per
/// signaled handle to `results` and returns the count signaled (≥1), or `TimedOut` /
/// `WouldBlock`.
pub fn sys_wait(handles_ptr: u64, count: usize, results_ptr: u64, deadline_ns: u64) -> SysResult {
    if count == 0 {
        return Err(KError::InvalidArgument);
    }
    if count > MAX_WAIT_HANDLES {
        return Err(KError::TooLarge);
    }
    let hptr = UserPtr::<u8>::new(handles_ptr).map_err(from_user_access)?;
    let rptr = UserMutPtr::<u8>::new(results_ptr).map_err(from_user_access)?;

    let mut hbytes = [0u8; MAX_WAIT_HANDLES * 8];
    copy_slice_from_user(&mut hbytes[..count * 8], hptr).map_err(from_user_access)?;
    let mut handles = [0u64; MAX_WAIT_HANDLES];
    for i in 0..count {
        handles[i] = u64::from_ne_bytes(hbytes[i * 8..i * 8 + 8].try_into().unwrap());
    }

    let pid = crate::sched::current_owner_pid();
    let mut refs: [Option<ObjectRef>; MAX_WAIT_HANDLES] = core::array::from_fn(|_| None);
    let mut objs = [0usize; MAX_WAIT_HANDLES];
    let mut types = [KObjectType::Invalid; MAX_WAIT_HANDLES];
    for i in 0..count {
        let ok = global::get().lookup(RawHandle(handles[i]), pid, Rights::WAIT).map_err(map_handle_err)?;
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

    let now = Timer::read_ns();
    // SAFETY: `refs` pins each object in `objs`, a waitable its lookup type-checked.
    let result = unsafe { crate::sched::wait_on(&objs[..count], deadline_ns, now) };

    match result {
        crate::sched::WaitResult::Signaled(bits) => {
            let mut out = [IoResult::ready(0); MAX_WAIT_HANDLES];
            let mut k = 0usize;
            for i in 0..count {
                if bits[i]
                    && let Some(obj) = &refs[i]
                {
                    out[k] = if types[i] == KObjectType::PendingOperation {
                        // SAFETY: `obj` pins the `PendingOperation` its lookup type-checked.
                        let (status, result) = unsafe { crate::sched::pending_op_completion(obj.as_ptr()) };
                        IoResult::completed_with_result(handles[i], status, result)
                    } else {
                        if types[i] == KObjectType::InterruptObject {
                            // SAFETY: `obj` pins the `InterruptObject` its lookup type-checked.
                            unsafe { crate::sched::interrupt_consume(obj.as_ptr()) };
                        }
                        IoResult::ready(handles[i])
                    };
                    k += 1;
                }
            }
            // SAFETY: `IoResult` is `#[repr(C)]` with every byte initialised and
            // no padding (asserted in `libkern::io_result`); reinterpret the
            // first `k` records as a byte slice to copy out.
            let bytes =
                unsafe { core::slice::from_raw_parts(out.as_ptr() as *const u8, k * core::mem::size_of::<IoResult>()) };
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

/// `sys_notif_recv(channel, out)` — copy the oldest notification on `channel` to the 64-byte
/// [`Notification`] at `out`, remove it, return 0. `WouldBlock` if the queue is empty.
///
/// Gated by handle ownership, not a right: `WAIT` gates *blocking* via `sys_wait`.
pub fn sys_notif_recv(channel_h: u64, out: u64) -> SysResult {
    let uptr = UserMutPtr::<Notification>::new(out).map_err(from_user_access)?;
    let pid = crate::sched::current_owner_pid();
    let ok = lookup_typed(channel_h, pid, Rights::empty(), KObjectType::NotificationChannel)?;
    // Copied out after the lock is released (never hold the IrqSpinLock across a user copy).
    // SAFETY: `ok.object` is the live `NotificationChannel` its lookup type-checked, held across the call.
    let n = unsafe { crate::sched::notif_try_recv(ok.object.as_ptr()) }.ok_or(KError::WouldBlock)?;
    copy_to_user(uptr, &n).map_err(from_user_access)?;
    Ok(0)
}

/// The rights of a freshly created channel endpoint (`docs/spec/ipc-message-format.md`).
fn channel_endpoint_rights() -> Rights {
    Rights::SEND | Rights::RECV | Rights::DUPLICATE | Rights::TRANSFER | Rights::INSPECT | Rights::WAIT
}

fn close_and_release(h: RawHandle, pid: u32) {
    if let Ok(co) = global::get().close(h, pid) {
        // SAFETY: `close` transferred one reference into the token without
        // decrementing; adopt it into an `ObjectRef` and drop to release.
        drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
    }
}

/// `sys_channel_create(end0, end1, queue_depth)` — create a bidirectional IPC channel and write
/// its two endpoint handles to `*end0` / `*end1`. Each direction gets a `queue_depth`-slot ring
/// (`0` → default; `> IPC_MAX_QUEUE_DEPTH` is `InvalidArgument`).
pub fn sys_channel_create(end0: u64, end1: u64, queue_depth: u64) -> SysResult {
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

    let h0 = match global::get().allocate(pid, a_ptr, KObjectType::IpcChannel, rights) {
        Ok(h) => h,
        Err(e) => {
            // SAFETY: each pointer still owns its single creation reference.
            drop(unsafe { ObjectRef::from_raw(a_ptr, KObjectType::IpcChannel) });
            // SAFETY: as above.
            drop(unsafe { ObjectRef::from_raw(b_ptr, KObjectType::IpcChannel) });
            return Err(map_handle_err(e));
        }
    };
    // Endpoint 1; on failure, close `h0` (nulls b's peer) and reclaim b's creation reference.
    let h1 = match global::get().allocate(pid, b_ptr, KObjectType::IpcChannel, rights) {
        Ok(h) => h,
        Err(e) => {
            close_and_release(h0, pid);
            // SAFETY: b_ptr still owns its creation reference.
            drop(unsafe { ObjectRef::from_raw(b_ptr, KObjectType::IpcChannel) });
            return Err(map_handle_err(e));
        }
    };

    // A faulting copy rolls both back so no endpoint leaks into a process that cannot see it.
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

/// `sys_channel_send(ch, msg, handles, count, mode, deadline)` — enqueue `*msg` on the peer of
/// endpoint `ch`, **moving** `handles[0..count]` to the receiver. Each moved handle needs
/// `TRANSFER`; the sender's handles are closed only after the message is queued/held, so a
/// failed send loses no capability.
///
/// - `NoBlock`: returns `0`, `WouldBlock` if the peer ring is full, or `PeerClosed`.
/// - `Block` / `BlockBounded`: returns a `PendingOperation` handle that completes when the
///   message is delivered (held in the peer's pending-send queue if the ring is full).
///   `BlockBounded` completes the PO `TimedOut` (reclaiming the message) if undelivered by
///   `deadline` (absolute monotonic ns); `deadline` is ignored for `NoBlock`/`Block`.
pub fn sys_channel_send(ch: u64, msg: u64, handles: u64, count: u64, mode: u64, deadline: u64) -> SysResult {
    let send_mode = match SendMode::from_u32(mode as u32) {
        Some(m) => m,
        None => return Err(KError::InvalidArgument),
    };
    let count = count as usize;
    if count > IPC_HANDLE_MAX {
        return Err(KError::TooLarge);
    }
    let mptr = UserPtr::<u8>::new(msg).map_err(from_user_access)?;

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

    // Each transferred handle needs `TRANSFER`; a failure drops the collected refs (outside SCHED)
    // and leaves the sender's table unchanged: atomic-or-fail.
    let mut transfers: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    for i in 0..count {
        let h = RawHandle(h_raw[i]);
        let t = global::get().lookup(h, pid, Rights::TRANSFER).map_err(map_handle_err)?;
        transfers[i] = Some(TransferRef { obj: t.object, rights: t.rights });
    }

    // SAFETY: `StoredMsg` is plain `#[repr(C)]` data; all-zero is a valid value. Zeroed
    // in place — a stack temporary here would be 4 KiB on every send.
    let mut bounce = unsafe { KBox::<StoredMsg>::try_new_zeroed() }.map_err(|_| KError::OutOfMemory)?;
    copy_slice_from_user(bounce.as_bytes_mut(), mptr).map_err(from_user_access)?;
    if bounce.header.payload_len as usize > IPC_PAYLOAD_SIZE {
        return Err(KError::InvalidArgument);
    }
    bounce.header.sender_pid = pid;
    bounce.header.timestamp = Timer::read_ns();
    bounce.header.handle_count = count as u8;
    bounce.handles = [0u64; IPC_HANDLE_MAX];

    // A peer that is the kernel's end of a Userspace Server channel means a forwarded-lookup
    // **reply**: complete the waiting lookup inline (nothing receives that ring), whatever
    // `send_mode`. See `docs/spec/rsproto-namespace-ops.md`.
    // SAFETY: `ok.object` is the live `IpcChannel` its lookup type-checked, held across the call.
    if let Some(reg) = unsafe { crate::sched::us_forward_reg_for_send(ok.object.as_ptr()) } {
        let payload_len = (bounce.header.payload_len as usize).min(IPC_PAYLOAD_SIZE);
        if let Some(file_id) = crate::rsproto::forget_request(&bounce.payload[..payload_len]) {
            // Only `Block`: its result is a PO, and no deadline could apply.
            if send_mode != SendMode::Block || count != 0 {
                return Err(KError::InvalidArgument);
            }
            return answer_forget(&reg, file_id, pid);
        }
        return complete_forwarded_reply(&reg, &bounce, &mut transfers, &h_raw, count, pid);
    }

    match send_mode {
        // `Sent` commits the transfer move; a full ring / dead peer leaves the transfers untaken.
        SendMode::NoBlock => {
            // SAFETY: `ok.object` is the live `IpcChannel` its lookup type-checked, held across the call.
            match unsafe { crate::sched::ipc_send_push(ok.object.as_ptr(), &bounce, &mut transfers) } {
                SendOutcome::Sent { .. } => {
                    for &h in &h_raw[..count] {
                        close_and_release(RawHandle(h), pid);
                    }
                    Ok(0)
                }
                SendOutcome::Full => Err(KError::WouldBlock),
                SendOutcome::PeerClosed => Err(KError::PeerClosed),
            }
        }
        // Returns a `PendingOperation` that completes on delivery; the message is committed
        // either way, so the transfer move is too. The syscall never parks (async-first).
        SendMode::Block | SendMode::BlockBounded => {
            let deadline_ns = if send_mode == SendMode::BlockBounded { deadline } else { u64::MAX };
            let po = PendingOperation::try_new().map_err(|_| KError::OutOfMemory)?;
            // SAFETY: `into_raw` yields the single creation reference; adopt it.
            let po_ref =
                unsafe { ObjectRef::from_raw(KBox::into_raw(po).as_ptr() as *mut (), KObjectType::PendingOperation) };
            // SAFETY: `ok.object` is the live `IpcChannel` its lookup type-checked, held across the call.
            match unsafe {
                crate::sched::ipc_send_push_blocking(ok.object.as_ptr(), &bounce, &mut transfers, &po_ref, deadline_ns)
            } {
                BlockSendOutcome::Sent { .. } | BlockSendOutcome::Queued => {
                    for &h in &h_raw[..count] {
                        close_and_release(RawHandle(h), pid);
                    }
                    let (op, ot) = po_ref.into_raw();
                    match global::get().allocate(pid, op, ot, pending_op_rights()) {
                        Ok(h) => Ok(h.bits() as isize),
                        // Table full: the message was still committed. A `Queued` PO lives on via
                        // the pending-send queue; a `Sent` one is reclaimed here.
                        Err(e) => {
                            // SAFETY: reclaims the reference `into_raw` gave up.
                            drop(unsafe { ObjectRef::from_raw(op, ot) });
                            Err(map_handle_err(e))
                        }
                    }
                }
                // Back-pressure / dead peer: nothing committed; the sender keeps its handles.
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

/// **Answer a server's `File::Forget`**: the file `file_id` is about to be freed, and the server
/// frees its blocks only once the kernel says nothing of it is in flight. The file's cached
/// object is forgotten ([`forget_file`](crate::object::UserspaceServerReg::forget_file)): it
/// starts no more device I/O, and no resolve or sync finds it again. The answer is a
/// `PendingOperation` installed in the server's table, complete already if nothing was in
/// flight, else when the last IRP ends; a second `Forget` meanwhile gets the same one. The
/// server `sys_wait`s on it, so a `Forget` must be sent `Block`.
fn answer_forget(reg: &ObjectRef, file_id: u64, pid: u32) -> SysResult {
    use crate::object::{Forgotten, UserspaceServerReg};
    let po = PendingOperation::try_new().map_err(|_| KError::OutOfMemory)?;
    // SAFETY: `into_raw` yields the single creation reference; adopt it.
    let answer = unsafe { ObjectRef::from_raw(KBox::into_raw(po).as_ptr() as *mut (), KObjectType::PendingOperation) };
    // SAFETY: `reg` pins the registration the send arrived through.
    let r: &UserspaceServerReg = unsafe { &*(reg.as_ptr() as *const UserspaceServerReg) };
    let wait_on = match r.forget_file(file_id, &answer) {
        Forgotten::Later(po) => po,
        Forgotten::Now => {
            // SAFETY: `answer` is the `PendingOperation` this created.
            unsafe { crate::sched::complete_pending_op(answer.as_ptr(), 0, 0) };
            answer.clone()
        }
    };
    drop(answer);
    let (op, ot) = wait_on.into_raw();
    match global::get().allocate(pid, op, ot, pending_op_rights()) {
        Ok(h) => Ok(h.bits() as isize),
        Err(e) => {
            // SAFETY: `allocate` did not adopt the reference; reclaim it.
            drop(unsafe { ObjectRef::from_raw(op, ot) });
            Err(map_handle_err(e))
        }
    }
}

/// Complete a forwarded **reply** inline in the server's `sys_channel_send`: a
/// `Namespace::Resolve` or `File::ReadRange` (page-cache fill) reply is routed by its op; one
/// that is neither is consumed (transfers dropped, sender's handles closed). `transfers[i]` ↔
/// the sender handle `h_raw[i]`. Runs in syscall context (no `SCHED` held), so the
/// cross-context `allocate` and the drops are outside the lock.
fn complete_forwarded_reply(
    reg: &ObjectRef,
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
        _ => {
            for &h in &h_raw[..count] {
                close_and_release(RawHandle(h), pid);
            }
            Ok(0)
        }
    }
}

/// Complete a forwarded `Namespace::Resolve` reply: correlate it to the registration's pending
/// lookup and deliver its outcome through that lookup's `PendingOperation`. An
/// `OBJECT_KIND_MEMOBJ` reply installs the transferred `MemoryObject` into the **original
/// caller's** table (rights = `requested ∩ the rights the server granted`); an
/// `OBJECT_KIND_FILE` reply builds a lazy [`FileObject`] pointed back at this server; an error
/// completes the PO with a `KError`. An uncorrelated (duplicate / stale) reply is dropped.
fn complete_resolve_reply(
    reg: &ObjectRef,
    bounce: &StoredMsg,
    transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    h_raw: &[u64; IPC_HANDLE_MAX],
    count: usize,
    pid: u32,
    payload_len: usize,
) -> SysResult {
    use crate::rsproto::{
        OBJECT_KIND_CHANNEL, OBJECT_KIND_FILE, OBJECT_KIND_FILE_BLOCKS, OBJECT_KIND_MEMOBJ, OBJECT_KIND_NONE,
        OBJECT_KIND_SUBNAMESPACE, ReplyKind, parse_reply,
    };

    let reply = parse_reply(&bounce.payload[..payload_len]);
    let pl = match reply {
        // SAFETY: `reg` pins the registration the send arrived through.
        Some(r) => unsafe { crate::sched::us_forward_take_pending(reg.as_ptr(), r.request_id) },
        None => None,
    };
    let Some(pl) = pl else {
        for &h in &h_raw[..count] {
            close_and_release(RawHandle(h), pid);
        }
        return Ok(0);
    };
    let reply = reply.expect("pending taken ⇒ reply parsed");

    let subnamespace =
        matches!(reply.kind, ReplyKind::Success { object_kind, .. } if object_kind == OBJECT_KIND_SUBNAMESPACE);
    let outcome: Option<(i32, u64)> = if subnamespace {
        continue_resolve(&pl, &bounce.payload[..payload_len], transfers)
    } else {
        Some(match reply.kind {
            ReplyKind::Error { kerror } => (kerror, 0),
            ReplyKind::Malformed => (KError::KernelError as i32, 0),
            ReplyKind::Success { object_kind, content_len } if object_kind == OBJECT_KIND_FILE => {
                build_and_install_file(reg, &pl, content_len)
            }
            ReplyKind::Success { object_kind, content_len } if object_kind == OBJECT_KIND_FILE_BLOCKS => {
                build_and_install_file_blocks(reg, &pl, content_len, &bounce.payload[..payload_len], transfers)
            }
            ReplyKind::Success { object_kind, .. } if object_kind == OBJECT_KIND_NONE => (0, 0),
            ReplyKind::Success { object_kind, .. }
                if object_kind != OBJECT_KIND_MEMOBJ && object_kind != OBJECT_KIND_CHANNEL =>
            {
                (KError::Unsupported as i32, 0)
            }
            ReplyKind::Success { .. } => match transfers[0].take() {
                None => (KError::InvalidArgument as i32, 0), // success but no handle
                // The resolved object is a capability the server hands back: a `MemoryObject`, a
                // **re-exported** `FileObject` (an indirection server passes the store's through)
                // or an `IpcChannel` (a live **connection**). Other types are not valid results.
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
                    let attenuated = pl.requested & tr.rights;
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
        })
    };

    if let Some((status, result)) = outcome {
        // SAFETY: `pl` owns the lookup's `PendingOperation` reference.
        unsafe { crate::sched::complete_pending_op(pl.po.as_ptr(), status, result) };
    }
    drop(pl);
    for &h in &h_raw[..count] {
        close_and_release(RawHandle(h), pid);
    }
    Ok(0)
}

/// `sys_file_sync(handle)` — flush a writable file mapping's resident pages to the device.
/// Requires `MAP_WRITE` on the `FileObject` handle. Blocks on the write IRPs (async-first-exempt
/// on a syscall thread). `0` on success, else a `KError`.
fn sys_file_sync(handle: u64) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    let ok = lookup_typed(handle, pid, Rights::MAP_WRITE, KObjectType::FileObject)?;
    write_back(&ok.object)?;
    Ok(0)
}

/// Write one file's pages back, tell its server, and unpin it if no writable mapping appeared.
fn write_back(file_obj: &ObjectRef) -> Result<(), KError> {
    use crate::object::FileObject;
    // SAFETY: `file_obj` pins a live `FileObject` (both callers checked the type).
    let mark = unsafe { &*(file_obj.as_ptr() as *const FileObject) }.clean_mark();
    match FileObject::writeback(file_obj) {
        crate::object::file_object::WriteBack::Written => {}
        crate::object::file_object::WriteBack::Failed => return Err(KError::IoError),
        // **Its device has gone**: nothing will take these pages, so the file is let go.
        crate::object::file_object::WriteBack::Gone => {
            let pages = FileObject::let_go(file_obj);
            crate::kprintln!("file: its device has gone; a dirty file and its {pages} page(s) let go, unwritten");
            return Err(KError::PeerClosed);
        }
    }
    // Under Model A the server does not know the data landed: tell it (best-effort, unwaited).
    notify_file_touched(file_obj);
    if let Some(mark) = mark {
        FileObject::unpin_if_clean(file_obj, mark);
    }
    Ok(())
}

/// `sys_ns_sync(ns, path, path_len)` — **write back every dirty file** cached under the
/// registration `path` resolves through in `ns`, whether or not anything still holds it (a
/// writer that exits without syncing leaves its file dirty and pinned). Needs `LOOKUP` on `ns`:
/// it changes no file, only makes what was written durable;
/// `path` must resolve to a userspace server's binding, else `Unsupported`. Returns how many
/// files were written. **Blocks** on the write IRPs in the calling thread, like `sys_file_sync`.
fn sys_ns_sync(ns_h: u64, path_ptr: u64, path_len: usize) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    let ns_ok = lookup_typed(ns_h, pid, Rights::LOOKUP, KObjectType::Namespace)?;
    let mut buf = [0u8; NS_PATH_MAX];
    let path = copy_ns_path(path_ptr, path_len, &mut buf)?;
    validate_path(path).map_err(|_| KError::InvalidArgument)?;
    // SAFETY: `lookup_typed` verified the type; the refcount pins it for this call.
    let ns: &Namespace = unsafe { &*(ns_ok.object.as_ptr() as *const Namespace) };
    let reg = match ns.resolve(path) {
        Some((ResolvedTarget::UserspaceServer(reg, _), _, _)) => reg,
        Some(_) => return Err(KError::Unsupported),
        None => return Err(KError::NotFound),
    };
    // SAFETY: `reg` pins a live `UserspaceServerReg`.
    let r: &crate::object::UserspaceServerReg = unsafe { &*(reg.as_ptr() as *const crate::object::UserspaceServerReg) };
    let files = r.cache_objects().map_err(|_| KError::OutOfMemory)?;
    let mut written = 0isize;
    let mut result = Ok(());
    for f in files.iter() {
        // SAFETY: `f` pins a live `FileObject`.
        let fo: &crate::object::FileObject = unsafe { &*(f.as_ptr() as *const crate::object::FileObject) };
        if !fo.is_dirty() {
            continue;
        }
        match write_back(f) {
            Ok(()) => written += 1,
            Err(e) => result = Err(e),
        }
    }
    drop(files);
    drop(reg);
    result.map(|()| written)
}

/// `sys_ns_held(ns, path, path_len)` — **how many of a mount's files something still holds**
/// (a handle, mapping, IRP in flight or dirty pin; the cache holds its objects weakly): the
/// live page-cache objects of the registration `path` resolves through in `ns`, a forgotten
/// file's aside. **Asked after [`sys_ns_sync`]**, what is left is held by someone; the storage
/// service refuses an unmount while it is not zero.
///
/// **A finished IRP's reference is released first** (its box is freed only in thread context,
/// [`crate::io::block::reclaim_completed`]), or the first unmount after a write was refused.
/// Exact except for an IRP whose completion is still running on another CPU: that file is
/// counted ([`storage.md`](../../docs/architecture/storage.md) §6).
///
/// Needs `LOOKUP`. `Unsupported` if `path` resolves to anything but a userspace server,
/// `NotFound` if to nothing. It does not block.
fn sys_ns_held(ns_h: u64, path_ptr: u64, path_len: usize) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    let ns_ok = lookup_typed(ns_h, pid, Rights::LOOKUP, KObjectType::Namespace)?;
    let mut buf = [0u8; NS_PATH_MAX];
    let path = copy_ns_path(path_ptr, path_len, &mut buf)?;
    validate_path(path).map_err(|_| KError::InvalidArgument)?;
    // SAFETY: `lookup_typed` verified the type; the refcount pins it for this call.
    let ns: &Namespace = unsafe { &*(ns_ok.object.as_ptr() as *const Namespace) };
    let reg = match ns.resolve(path) {
        Some((ResolvedTarget::UserspaceServer(reg, _), _, _)) => reg,
        Some(_) => return Err(KError::Unsupported),
        None => return Err(KError::NotFound),
    };
    // SAFETY: `reg` pins a live `UserspaceServerReg`.
    let r: &crate::object::UserspaceServerReg = unsafe { &*(reg.as_ptr() as *const crate::object::UserspaceServerReg) };
    crate::io::block::reclaim_completed();
    // Taken under the cache's lock, dropped after: each may be the last and `Drop` takes it.
    let files = r.cache_objects().map_err(|_| KError::OutOfMemory)?;
    let held = files.len() as isize;
    drop(files);
    drop(reg);
    Ok(held)
}

/// `sys_power(system_control, op)` — flush every disk, stop every processor, then halt or reset
/// the machine ([`crate::power`]). Needs `WRITE` on a
/// [`SystemControl`](crate::object::SystemControl), which only `init` holds; `op` is
/// `POWER_HALT` or `POWER_REBOOT`, else `InvalidArgument`.
///
/// **It returns only to refuse.** The handle is checked before the op, so a process without the
/// object learns nothing from either. The flush waits in the kernel, bounded.
fn sys_power(sc_h: u64, op: u64) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    let _system_control = lookup_typed(sc_h, pid, Rights::WRITE, KObjectType::SystemControl)?;
    use crate::libkern::power::PowerOp;
    let op = PowerOp::from_u64(op).ok_or(KError::InvalidArgument)?;
    let what = if op == PowerOp::Halt { "halt" } else { "reboot" };
    crate::kprintln!("power: pid {pid} asks to {what}");
    crate::power::power(op)
}

/// Send `File::Touch` for a just-flushed Model A file so its server can stamp `mtime`. Stamps
/// on **sync**, not on the write (no per-page dirty bit, `TODO(page-dirty-tracking)`). Silent
/// on failure: the data is durable, and a dropped touch costs only a stale `mtime`.
fn notify_file_touched(file_obj: &ObjectRef) {
    use crate::libkern::KBox;
    use crate::object::FileObject;

    let Some((reg, file_id)) = FileObject::touch_target(file_obj) else {
        return; // not a cached Model A file — nothing to notify
    };
    // Heap-bounced: a 4 KiB `StoredMsg` does not belong on a measured kernel stack.
    // SAFETY: `StoredMsg` is plain `#[repr(C)]` data; all-zero is a valid value. Zeroed in
    // place — the by-value form would put 4 KiB in this frame, on a sync path that is
    // already among the deepest in the kernel.
    let Ok(mut msg) = (unsafe { KBox::<crate::object::ipc_channel::StoredMsg>::try_new_zeroed() }) else {
        return;
    };
    let Some(body_len) = crate::rsproto::build_touch_request(&mut msg.payload, file_id) else {
        return;
    };
    msg.header.payload_len = body_len as u32;
    msg.header.handle_count = 0;
    // SAFETY: `reg` is a live `UserspaceServerReg` reference held across the call.
    unsafe { crate::sched::us_forward_notify(reg.as_ptr(), &mut msg) };
}

/// Build a lazy [`FileObject`] for an `OBJECT_KIND_FILE` resolve reply and install it into the
/// caller's table; returns the `(status, handle)` the lookup PO completes with. Its producer
/// names the file by the lookup's stored suffix, so a page fault fills it via `File::ReadRange`.
/// `TooLarge` if the suffix overran the inline buffer, `OutOfMemory` on allocation failure.
fn build_and_install_file(
    reg: &ObjectRef,
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
    let fobj = match FileObject::try_new(content_len as usize, Producer::FsServer { reg: reg.clone(), suffix: kstr }) {
        Ok(f) => f,
        Err(_) => return (KError::OutOfMemory as i32, 0),
    };
    // SAFETY: `into_raw` yields the single creation reference; adopt it.
    let fref = unsafe { ObjectRef::from_raw(KBox::into_raw(fobj).as_ptr() as *mut (), KObjectType::FileObject) };
    let (op, ot) = fref.into_raw();
    // `INSPECT` too, so a client can `sys_handle_stat` the lazy file for its size before mapping.
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

/// Build + install a **Model A** (block-fs) lazy file from a resolve reply: adopt the
/// transferred block `DeviceNode` (`handles[0]`), parse the block size + `BlockRun` map from
/// the reply body, and construct a `FileObject` whose producer fills each page zero-copy from
/// the device. Returns `(0, handle)` or `(kerror, 0)`.
fn build_and_install_file_blocks(
    reg: &ObjectRef,
    pl: &crate::object::userspace_server::PendingLookup,
    content_len: u32,
    reply_msg: &[u8],
    transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
) -> (i32, u64) {
    use crate::object::FileObject;
    use crate::rsproto::{FILE_BLOCKS_READ_ONLY, reply_body};

    let device = match transfers[0].take() {
        Some(tr) if tr.obj.object_type() == KObjectType::DeviceNode => tr.obj,
        Some(tr) => {
            drop(tr.obj);
            return (KError::Unsupported as i32, 0);
        }
        None => return (KError::InvalidArgument as i32, 0), // success but no device
    };
    let Some(body) = reply_body(reply_msg) else {
        return (KError::KernelError as i32, 0);
    };
    let (header, runs) = match block_map(body) {
        Ok(m) => m,
        Err(e) => return (e as i32, 0),
    };

    let reg_ref = reg.clone();
    // **One object per file**: a file this registration already caches is resized in place and
    // shared by every resolve. A file id of `0` names nothing and gets an uncached object.
    let candidate = match new_block_file(content_len, device, header.block_size, reg_ref.clone(), header.file_id, runs)
    {
        Ok(f) => f,
        Err(e) => return (e as i32, 0),
    };
    let fref = if header.file_id == 0 {
        candidate
    } else {
        match FileObject::cache_in(&reg_ref, candidate) {
            Ok(f) => f,
            Err(crate::object::file_object::CacheInError::OutOfMemory) => return (KError::OutOfMemory as i32, 0),
            Err(crate::object::file_object::CacheInError::BlockSizeChanged) => {
                return (KError::KernelError as i32, 0);
            }
        }
    };
    drop(reg_ref);

    let (op, ot) = fref.into_raw();
    // `INSPECT` as above. A read-only mount's file is never writable, whatever was asked for.
    let mut rights = pl.requested | Rights::INSPECT;
    if header.flags & FILE_BLOCKS_READ_ONLY != 0 {
        rights = rights.difference(Rights::MAP_WRITE);
    }
    match global::get().allocate(pl.owner_pid, op, ot, rights) {
        Ok(h) => (0, h.bits()),
        Err(e) => {
            // SAFETY: `allocate` did not adopt the reference; reclaim it.
            drop(unsafe { ObjectRef::from_raw(op, ot) });
            (map_handle_err(e) as i32, 0)
        }
    }
}

/// The deepest a lookup follows `SUBNAMESPACE` replies; the next one is `TooLarge`.
const SUBNAMESPACE_DEPTH_MAX: u8 = 4;

/// **Carry a lookup on into the namespace a server replied with** (`OBJECT_KIND_SUBNAMESPACE`).
/// `handles[0]` is the namespace, on which the server must hold `LOOKUP`; the first `consumed`
/// bytes of the suffix it was sent stand for `base` there. The path, and a rename's destination
/// (which must share those bytes), continue from `base` with the same operation, rights, caller
/// and PO, one level deeper ([`resolve_and_start`]). `None` if it went on to another server.
/// **This widens no server's power**: one that can reply with a namespace could have resolved
/// the path in it and handed back what it found.
fn continue_resolve(
    pl: &crate::object::userspace_server::PendingLookup,
    reply_msg: &[u8],
    transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
) -> Option<(i32, u64)> {
    let ns_ref = match transfers[0].take() {
        Some(tr) if tr.obj.object_type() == KObjectType::Namespace && tr.rights.contains(Rights::LOOKUP) => tr.obj,
        Some(tr) => {
            drop(tr.obj);
            return Some((KError::Unsupported as i32, 0));
        }
        None => return Some((KError::InvalidArgument as i32, 0)),
    };
    if pl.depth >= SUBNAMESPACE_DEPTH_MAX {
        return Some((KError::TooLarge as i32, 0));
    }
    let Some((consumed, base)) = crate::rsproto::reply_body(reply_msg).and_then(crate::rsproto::subnamespace_reply)
    else {
        return Some((KError::KernelError as i32, 0));
    };
    let Some(suffix) = pl.suffix() else {
        return Some((KError::TooLarge as i32, 0));
    };
    let Ok(mut bufs) = KBox::try_new([[0u8; NS_PATH_MAX]; 2]) else {
        return Some((KError::OutOfMemory as i32, 0));
    };
    let [pbuf, dbuf] = &mut *bufs;
    let path = match continued_path(suffix, consumed as usize, base, pbuf) {
        Ok(p) => p,
        Err(e) => return Some((e as i32, 0)),
    };
    let dest = match pl.op {
        ForwardOp::Rename { .. } => {
            let Some(d) = pl.dest() else {
                return Some((KError::TooLarge as i32, 0));
            };
            match continued_dest(suffix, d, consumed as usize, base, dbuf) {
                Ok(p) => Some(p),
                Err(e) => return Some((e as i32, 0)),
            }
        }
        _ => None,
    };
    // SAFETY: `ns_ref` pins a live `Namespace` (type checked above) across this call.
    let ns: &Namespace = unsafe { &*(ns_ref.as_ptr() as *const Namespace) };
    let outcome = resolve_and_start(ns, path, dest, pl.op, pl.requested, pl.owner_pid, &pl.po, pl.depth + 1);
    drop(ns_ref);
    outcome
}

/// **Where a `SUBNAMESPACE` reply sends a path.** The server answered for the first `consumed`
/// bytes of `suffix`; they stand for `base`, an absolute path in the namespace it replied with,
/// and the rest is joined onto `base`. `consumed` must end at a component boundary and `base`
/// must be a valid absolute path, else `InvalidArgument` (a server's protocol error, not the
/// caller's). A join longer than `out` is `TooLarge`. `base` is judged by the path it makes: the
/// joined path is validated, so a relative base, a trailing slash or a `..` fails there; the
/// empty base would join validly, so it is refused up front.
fn continued_path<'o>(suffix: &[u8], consumed: usize, base: &[u8], out: &'o mut [u8]) -> Result<&'o [u8], KError> {
    if consumed > suffix.len() || base.is_empty() {
        return Err(KError::InvalidArgument);
    }
    let rest = match (consumed, &suffix[consumed..]) {
        (0, rest) => rest,
        (_, []) => &[][..],
        (_, [b'/', rest @ ..]) => rest,
        _ => return Err(KError::InvalidArgument),
    };
    let root = base == b"/";
    let len = match (rest.is_empty(), root) {
        (true, _) => base.len(),
        (false, true) => 1 + rest.len(),
        (false, false) => base.len() + 1 + rest.len(),
    };
    if len > out.len() {
        return Err(KError::TooLarge);
    }
    let mut at = 0;
    if !(root && !rest.is_empty()) {
        out[..base.len()].copy_from_slice(base);
        at = base.len();
    }
    if !rest.is_empty() {
        out[at] = b'/';
        out[at + 1..len].copy_from_slice(rest);
    }
    let path = &out[..len];
    validate_path(path).map_err(|_| KError::InvalidArgument)?;
    Ok(path)
}

/// **Where a rename's destination goes**: it must begin with the same `consumed` bytes the
/// server answered for in the source and end a component there (the same mount), else
/// `Unsupported`, the cue to fall back to copy + unlink. Then as [`continued_path`].
fn continued_dest<'o>(
    suffix: &[u8],
    dest: &[u8],
    consumed: usize,
    base: &[u8],
    out: &'o mut [u8],
) -> Result<&'o [u8], KError> {
    if consumed > suffix.len() {
        return Err(KError::InvalidArgument);
    }
    let same_prefix = dest.len() >= consumed
        && dest[..consumed] == suffix[..consumed]
        && (consumed == 0 || dest.len() == consumed || dest[consumed] == b'/');
    if !same_prefix {
        return Err(KError::Unsupported);
    }
    continued_path(dest, consumed, base, out)
}

/// **A block-file reply's map**: its header and runs, or `KernelError` for one the page cache
/// cannot use: a body too short for its runs, and
///
/// - **a block size that is not a power of two from a sector to a page** (the fill divides by
///   it; `0` would divide by zero);
/// - **a run whose ends do not fit a `u64`**: its file blocks' end, its device blocks' end, and
///   that end in bytes. The fill and write-back add within a run and multiply by the block
///   size, and a kernel built with overflow checks panics on either.
///
/// `OutOfMemory` if the runs cannot be held.
fn block_map(
    body: &[u8],
) -> Result<(crate::rsproto::FileBlocksHeader, crate::libkern::KVec<crate::object::BlockRun>), KError> {
    use crate::object::BlockRun;
    use crate::rsproto::{file_blocks_reply_header, file_blocks_run};
    let header = file_blocks_reply_header(body).ok_or(KError::KernelError)?;
    let bs = header.block_size;
    if !bs.is_power_of_two() || !(512..=PAGE_SIZE as u32).contains(&bs) {
        return Err(KError::KernelError);
    }
    let mut runs = crate::libkern::KVec::new();
    runs.try_reserve(header.run_count as usize).map_err(|_| KError::OutOfMemory)?;
    for i in 0..header.run_count as usize {
        let (file_block, device_lba, length, flags) = file_blocks_run(body, i).ok_or(KError::KernelError)?;
        let fits = file_block.checked_add(length as u64).is_some()
            && device_lba.checked_add(length as u64).and_then(|end| end.checked_mul(bs as u64)).is_some();
        if !fits {
            return Err(KError::KernelError);
        }
        let _ = runs.try_push(BlockRun { file_block, device_lba, length, flags });
    }
    Ok((header, runs))
}

/// A new Model A `FileObject` over `device`, adopted into an `ObjectRef`: size, block size, run
/// map, and identity (registration and file id; non-zero means cached under it).
fn new_block_file(
    size: u32,
    device: ObjectRef,
    block_size: u32,
    reg: ObjectRef,
    file_id: u64,
    runs: crate::libkern::KVec<crate::object::BlockRun>,
) -> Result<ObjectRef, KError> {
    use crate::object::{FileObject, Producer};
    let fobj = FileObject::try_new_with_runs(
        size as usize,
        Producer::FsServerBlocks { device, block_size, reg, file_id },
        runs,
    )
    .map_err(|_| KError::OutOfMemory)?;
    // SAFETY: `into_raw` yields the single creation reference; adopt it.
    Ok(unsafe { ObjectRef::from_raw(KBox::into_raw(fobj).as_ptr() as *mut (), KObjectType::FileObject) })
}

/// Complete a forwarded `File::ReadRange` reply: correlate it to the registration's pending
/// fill, copy the replied bytes (a transferred ≤1-page `MemoryObject`) into the cache frame,
/// mark the page ready, and complete the fill PO (waking the parked faulter). An error or
/// uncorrelated reply fails the PO / is dropped (transfers released, handles closed).
fn complete_read_range_reply(
    reg: &ObjectRef,
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
        // SAFETY: `reg` pins the registration the send arrived through.
        Some(r) => unsafe { crate::sched::us_forward_take_pending_fill(reg.as_ptr(), r.request_id) },
        None => None,
    };
    let Some(pf) = pf else {
        for &h in &h_raw[..count] {
            close_and_release(RawHandle(h), pid);
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

    // Complete the fill PO (one-shot); release the kernel's clones outside `SCHED`.
    // SAFETY: `pf` owns the fill's `PendingOperation` reference.
    unsafe { crate::sched::complete_pending_op(pf.po.as_ptr(), status, 0) };
    drop(pf);
    for &h in &h_raw[..count] {
        close_and_release(RawHandle(h), pid);
    }
    Ok(0)
}

/// Copy `n` bytes (clamped to one page) from the first frame of the transferred fill
/// `MemoryObject` `src` into the cache frame `dst`, both via the HHDM; a no-op if `src` has no
/// frames or `n == 0`.
fn copy_fill_into_frame(src: &ObjectRef, dst: crate::mm::PhysAddr, n: usize) {
    use crate::mm::{PAGE_SIZE, heap};
    // SAFETY: `src` pins the live `MemoryObject` (header at offset 0).
    let mo: &crate::object::MemoryObject = unsafe { &*(src.as_ptr() as *const crate::object::MemoryObject) };
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

/// Close every handle a partial `sys_channel_recv` installed, so a copy-out fault or
/// mid-install error after the dequeue doesn't leak them.
fn recv_rollback_installed(installed: &[RawHandle], pid: u32) {
    for &h in installed {
        if !h.is_null() {
            close_and_release(h, pid);
        }
    }
}

/// `sys_channel_recv(ch, msg, handles, count)` — dequeue the oldest message on endpoint `ch`
/// into `*msg` (4096 bytes), **install** any transferred handles into the caller's table
/// (writing their values to `handles[0..*count]` and the in-message `handles[]`), and write
/// the count to `*count`. `WouldBlock` if the inbox is empty (`sys_wait` on `ch`); `PeerClosed`
/// if empty and the peer has gone.
pub fn sys_channel_recv(ch: u64, msg: u64, handles: u64, count: u64) -> SysResult {
    let mptr = UserMutPtr::<u8>::new(msg).map_err(from_user_access)?;
    let cptr = UserMutPtr::<usize>::new(count).map_err(from_user_access)?;

    let pid = crate::sched::current_owner_pid();
    let ok = lookup_typed(ch, pid, Rights::RECV, KObjectType::IpcChannel)?;

    // SAFETY: `ok.object` is the live `IpcChannel` its lookup type-checked, held across the call.
    match unsafe { crate::sched::ipc_recv_peek(ok.object.as_ptr()) } {
        RecvState::Empty => return Err(KError::WouldBlock),
        RecvState::PeerClosed => return Err(KError::PeerClosed),
        RecvState::HasMsg => {}
    }
    // SAFETY: `StoredMsg` is plain `#[repr(C)]` data; all-zero is a valid value. Zeroed in
    // place — a stack temporary here would be 4 KiB on every recv.
    let mut bounce = unsafe { KBox::<StoredMsg>::try_new_zeroed() }.map_err(|_| KError::OutOfMemory)?;
    let mut transfers: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    // Popping frees a ring slot, so a held blocking sender may be promoted into it; its PO and
    // any timed-out (`BlockBounded`) sends drop at scope exit, **outside** `SCHED`.
    let mut _reclaimed: [Option<ReclaimedSend>; IpcChannel::MAX_PENDING_SENDS] = core::array::from_fn(|_| None);
    let (popped, _promoted_po) =
        // SAFETY: `ok.object` is the live `IpcChannel` its lookup type-checked, held across the call.
        unsafe { crate::sched::ipc_recv_pop_into(ok.object.as_ptr(), &mut bounce, &mut transfers, &mut _reclaimed) };
    if !popped {
        return Err(KError::WouldBlock);
    }
    // From here any error must reclaim what we've installed; `transfers` drops outside SCHED.
    let n = bounce.header.handle_count as usize;
    // `handle_count` is bounded at send, so a larger value is a corrupted stored message:
    // refuse it, since the loops below index `[_; IPC_HANDLE_MAX]` buffers and would panic.
    if n > IPC_HANDLE_MAX {
        return Err(KError::KernelError);
    }
    let mut installed = [RawHandle::NULL; IPC_HANDLE_MAX];
    for i in 0..n {
        if let Some(tr) = transfers[i].take() {
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
mod tests;
