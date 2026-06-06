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
use crate::arch::abi::USER_VIRT_END;
use crate::arch::paging::ArchPaging;
use crate::handle::global;
use crate::handle::table::{HandleError, HandleTable};
use crate::libkern::handle::{HandleInfo, KObjectType, RawHandle, Rights};
use crate::libkern::{KBox, MemFlags};
use crate::mm::addr_space::{AddressSpace, MapError};
use crate::mm::user_access::{UserMutPtr, UserPtr, copy_slice_from_user, copy_to_user};
use crate::mm::vmm::{Protection, VAddrRange};
use crate::mm::{PAGE_SIZE, VirtAddr};
use crate::object::{MemoryObject, ObjectRef, Process};

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
pub fn dispatch(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, _a4: u64, _a5: u64) -> isize {
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
