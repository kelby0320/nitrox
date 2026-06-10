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
use crate::handle::table::{HandleError, HandleTable};
use crate::libkern::clock::ClockId;
use crate::libkern::handle::{HandleInfo, KObjectType, RawHandle, Rights};
use crate::libkern::ipc::{IPC_DEFAULT_QUEUE_DEPTH, IPC_HANDLE_MAX, IPC_MAX_QUEUE_DEPTH, IPC_PAYLOAD_SIZE};
use crate::libkern::{IoResult, KBox, MemFlags, Notification, SendMode, TimerFlags};
use crate::mm::addr_space::{AddressSpace, MapError};
use crate::mm::user_access::{
    UserMutPtr, UserPtr, copy_slice_from_user, copy_slice_to_user, copy_to_user,
};
use crate::mm::vmm::{Protection, VAddrRange};
use crate::mm::{PAGE_SIZE, VirtAddr};
// `Timer` (the arch hardware-clock alias) is imported above; the Timer kernel
// object is referenced as `TimerObject` to avoid the name clash.
use crate::object::{
    IpcChannel, MAX_WAIT_HANDLES, MemoryObject, ObjectRef, Process, RecvState, SendOutcome,
    StoredMsg, Timer as TimerObject,
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

/// Debug: write a user byte buffer to the kernel serial log. Not ABI-stable.
pub const SYS_DEBUG_KPRINT: u64 = 0xFFFF_0000;
/// Terminate the calling (single-threaded) process. Routes to the
/// scheduler's thread exit; the dying thread's last `Process` reference is
/// released on reap, freeing its address space. Debug number for now
/// (status plumbing / multi-thread teardown land with later slices).
pub const SYS_PROCESS_EXIT: u64 = 0xFFFF_0001;

/// Largest buffer `sys_kprint` will copy in one call. Bounds the on-stack
/// kernel buffer; well under `MAX_USER_COPY_SIZE`.
const KPRINT_MAX: usize = 4096;

/// Route a decoded syscall to its handler. `nr` is the number (from RAX);
/// `a0..a5` are the six argument registers (RDI, RSI, RDX, R10, R8, R9).
/// Returns the `isize` the ABI hands back in RAX.
pub fn dispatch(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, _a5: u64) -> isize {
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
        SYS_CHANNEL_SEND => encode(sys_channel_send(a0, a1, a2, a3, a4)),
        SYS_CHANNEL_RECV => encode(sys_channel_recv(a0, a1, a2, a3)),
        SYS_DEBUG_KPRINT => encode(sys_kprint(a0, a1 as usize)),
        // Diverges into the scheduler; never returns to dispatch/sysret.
        SYS_PROCESS_EXIT => sys_process_exit(a0 as i32),
        _ => KError::Unsupported.as_isize(),
    }
}

/// `sys_process_exit(status)` — terminate the calling process. This slice's
/// processes are single-threaded, so process exit is the current thread's
/// exit: hand off to the scheduler, which switches to another thread and
/// (on the next scheduler entry) reaps this one — releasing its last
/// `Process` reference and freeing the address space. Never returns.
fn sys_process_exit(_status: i32) -> ! {
    crate::sched::exit()
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

    // SERIAL is rank 7 (lowest); no other lock is held here.
    let serial = crate::arch::serial::SERIAL.lock();
    for &b in dst.iter() {
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
/// Separated from the user copy-out so the metadata logic is host-testable.
fn stat_on(t: &HandleTable, h: RawHandle, pid: u32) -> Result<HandleInfo, KError> {
    let s = t.stat(h, pid).map_err(map_handle_err)?;
    Ok(HandleInfo::from_stat(s.object_type, s.rights, s.generation))
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

    let ok = global::get()
        .lookup(RawHandle(obj_h), pid, required)
        .map_err(map_handle_err)?;
    if ok.object.object_type() != KObjectType::MemoryObject {
        return Err(KError::InvalidArgument);
    }
    // SAFETY: `object_type` confirms a live `MemoryObject`; `lookup` pinned it.
    // The borrow is from a raw pointer, so it does not block moving `ok.object`
    // into `map_object` below (it is unused after `size()` is read).
    let mobj = unsafe { &*(ok.object.as_ptr() as *const MemoryObject) };

    let size = round_up_page(size);
    if size == 0 || size as usize > mobj.size() {
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

    // Move the looked-up reference into the mapping. `map_object` installs only
    // `range.pages()` PTEs; `size <= obj.size()` guarantees enough frames.
    match asp.map_object(range, prot, ok.object) {
        Ok(()) => {
            // The calling process's AS is active; make the new PTEs visible.
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
    let ok = global::get()
        .lookup(RawHandle(timer_h), pid, Rights::empty())
        .map_err(map_handle_err)?;
    if ok.object.object_type() != KObjectType::Timer {
        return Err(KError::InvalidArgument);
    }
    // `ok.object` (an ObjectRef) is held across the arm, keeping the Timer alive.
    crate::sched::timer_arm(ok.object.as_ptr(), deadline_ns, interval_ns)
        .map_err(|()| KError::OutOfMemory)?;
    Ok(0)
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

    // Resolve each handle (requires `WAIT`) to an ObjectRef held for the call;
    // only `Timer` is waitable this slice.
    let pid = crate::sched::current_owner_pid();
    let mut refs: [Option<ObjectRef>; MAX_WAIT_HANDLES] = core::array::from_fn(|_| None);
    let mut objs = [0usize; MAX_WAIT_HANDLES];
    for i in 0..count {
        let ok = global::get()
            .lookup(RawHandle(handles[i]), pid, Rights::WAIT)
            .map_err(map_handle_err)?;
        match ok.object.object_type() {
            KObjectType::Timer | KObjectType::NotificationChannel | KObjectType::IpcChannel => {}
            _ => return Err(KError::Unsupported),
        }
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
                    out[k] = IoResult::ready(handles[i]);
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
    let ok = global::get()
        .lookup(RawHandle(channel_h), pid, Rights::empty())
        .map_err(map_handle_err)?;
    if ok.object.object_type() != KObjectType::NotificationChannel {
        return Err(KError::InvalidArgument);
    }
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

/// `sys_channel_send(ch, msg, handles, count, mode)` — enqueue `*msg` on the
/// peer of endpoint `ch`. This slice supports `NoBlock` only (`Block` /
/// `BlockBounded` → `Unsupported`, pending the async-I/O slice) and no handle
/// transfer (`count != 0` → `Unsupported`, pending process spawn). Returns `0`
/// on success, `WouldBlock` if the queue is full, `PeerClosed` if the peer has
/// gone.
pub fn sys_channel_send(
    ch: u64,
    msg: u64,
    _handles: u64,
    count: u64,
    mode: u64,
) -> SysResult {
    // Mode + transfer gating first (host-reachable, no pointer/lock use).
    match SendMode::from_u32(mode as u32) {
        Some(SendMode::NoBlock) => {}
        Some(SendMode::Block) | Some(SendMode::BlockBounded) => return Err(KError::Unsupported),
        None => return Err(KError::InvalidArgument),
    }
    if count != 0 {
        return Err(KError::Unsupported); // handle transfer lands with spawn
    }
    let mptr = UserPtr::<u8>::new(msg).map_err(from_user_access)?;

    let pid = crate::sched::current_owner_pid();
    let ok = global::get()
        .lookup(RawHandle(ch), pid, Rights::SEND)
        .map_err(map_handle_err)?;
    if ok.object.object_type() != KObjectType::IpcChannel {
        return Err(KError::InvalidArgument);
    }

    // Bounce the 4096-byte message in from user memory (outside any lock — a
    // user copy may fault). One heap slot per send; optimisation target later.
    let mut bounce = KBox::try_new(StoredMsg::zeroed()).map_err(|_| KError::OutOfMemory)?;
    copy_slice_from_user(bounce.as_bytes_mut(), mptr).map_err(from_user_access)?;
    if bounce.header.payload_len as usize > IPC_PAYLOAD_SIZE {
        return Err(KError::InvalidArgument);
    }
    // Stamp the kernel-controlled header fields; clear the (unused) handle region.
    bounce.header.sender_pid = pid;
    bounce.header.timestamp = Timer::read_ns();
    bounce.header.handle_count = 0;
    bounce.handles = [0u64; IPC_HANDLE_MAX];

    // `ok.object` (held here) pins the endpoint across the push.
    match crate::sched::ipc_send_push(ok.object.as_ptr(), &bounce) {
        SendOutcome::Sent { .. } => Ok(0),
        SendOutcome::Full => Err(KError::WouldBlock),
        SendOutcome::PeerClosed => Err(KError::PeerClosed),
    }
}

/// `sys_channel_recv(ch, msg, handles, count)` — dequeue the oldest message on
/// endpoint `ch` into `*msg` (4096 bytes) and write the transferred-handle count
/// (`0` this slice) to `*count`. `WouldBlock` if the inbox is empty (caller
/// `sys_wait`s on `ch`); `PeerClosed` if empty and the peer has gone.
pub fn sys_channel_recv(ch: u64, msg: u64, _handles: u64, count: u64) -> SysResult {
    let mptr = UserMutPtr::<u8>::new(msg).map_err(from_user_access)?;
    let cptr = UserMutPtr::<usize>::new(count).map_err(from_user_access)?;

    let pid = crate::sched::current_owner_pid();
    let ok = global::get()
        .lookup(RawHandle(ch), pid, Rights::RECV)
        .map_err(map_handle_err)?;
    if ok.object.object_type() != KObjectType::IpcChannel {
        return Err(KError::InvalidArgument);
    }

    // Peek under SCHED so the empty-poll path allocates no bounce buffer.
    match crate::sched::ipc_recv_peek(ok.object.as_ptr()) {
        RecvState::Empty => return Err(KError::WouldBlock),
        RecvState::PeerClosed => return Err(KError::PeerClosed),
        RecvState::HasMsg => {}
    }
    let mut bounce = KBox::try_new(StoredMsg::zeroed()).map_err(|_| KError::OutOfMemory)?;
    if !crate::sched::ipc_recv_pop_into(ok.object.as_ptr(), &mut bounce) {
        // Drained between the peek and the pop (only possible under future SMP).
        return Err(KError::WouldBlock);
    }
    // Copy out AFTER the lock is released (never hold SCHED across a user copy).
    copy_slice_to_user(mptr, bounce.as_bytes()).map_err(from_user_access)?;
    let handle_count: usize = 0;
    copy_to_user(cptr, &handle_count).map_err(from_user_access)?;
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
    fn channel_send_rejects_mode_and_transfer_before_pointers() {
        // Mode is checked first: Block/BlockBounded are deferred (Unsupported),
        // an unknown selector is InvalidArgument — all before any pointer/lock.
        assert_eq!(sys_channel_send(0xDEAD, 0xDEAD, 0, 0, 0), Err(KError::Unsupported)); // Block
        assert_eq!(sys_channel_send(0xDEAD, 0xDEAD, 0, 0, 2), Err(KError::Unsupported)); // BlockBounded
        assert_eq!(sys_channel_send(0xDEAD, 0xDEAD, 0, 0, 99), Err(KError::InvalidArgument));
        // NoBlock but a non-zero handle count → transfer not implemented yet.
        assert_eq!(sys_channel_send(0xDEAD, 0xDEAD, 0, 1, 1), Err(KError::Unsupported));
    }

    #[test]
    fn channel_send_rejects_bad_message_pointer() {
        // NoBlock + count 0, then a bad `msg` pointer fails before lookup/lock.
        assert_eq!(
            sys_channel_send(0xDEAD, USER_VIRT_END, 0, 0, 1),
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
