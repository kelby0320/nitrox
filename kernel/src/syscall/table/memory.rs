//! The memory syscalls: an object made, mapped and unmapped.

use super::*;

/// Rights minted on a fresh `MemoryObject` handle: the `MAP_*` band only (`handle/type_rights.rs`);
/// `READ`/`WRITE`/`EXECUTE` would be rejected as `BadRights`.
pub(super) fn full_mem_rights() -> Rights {
    Rights::MAP_READ | Rights::MAP_WRITE | Rights::MAP_EXEC | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER
}

pub(super) fn map_mem_map_err(e: MapError) -> KError {
    match e {
        MapError::Overlap | MapError::NotCanonical | MapError::NotUserHalf => KError::InvalidArgument,
        MapError::OutOfMemory => KError::OutOfMemory,
    }
}

pub(super) fn round_up_page(n: u64) -> u64 {
    n.saturating_add(PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1)
}

fn current_address_space(proc_ref: &Ref<Process>) -> Option<&AddressSpace> {
    proc_ref.address_space()
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
    let pid = crate::sched::current_owner_pid();
    Ok(install(pid, ObjectRef::new(obj), full_mem_rights())?.bits() as isize)
}

/// `sys_memory_map(obj, hint, size, rights)` — map `obj`'s frames into the caller's address
/// space with `rights` (a subset of the `MAP_*` band). `hint == 0` picks any free range;
/// otherwise `hint` is the requested page-aligned base. Returns the mapped base address.
pub fn sys_memory_map(obj_h: u64, hint: u64, size: u64, rights: u64) -> SysResult {
    let req = Rights::from_bits_truncate(rights);
    // The handle must carry the `MAP_*` bits installed, and `MAP_READ` always: every mapping reads.
    let required = (req & (Rights::MAP_READ | Rights::MAP_WRITE | Rights::MAP_EXEC)) | Rights::MAP_READ;

    let pid = crate::sched::current_owner_pid();
    let proc_ref = crate::sched::current_process().ok_or(KError::KernelError)?;
    let asp = current_address_space(&proc_ref).ok_or(KError::KernelError)?;

    // A `MemoryObject` (eager) or demand-paged `FileObject`; others lack the `MAP_*` right.
    let ok = global::get().lookup(RawHandle(obj_h), pid, required).map_err(map_handle_err)?;
    let object = Mappable::of(ok.object)?;
    let max_bytes = match &object {
        Mappable::Memory(memory) => memory.size(),
        Mappable::File(file) => file.npages() * PAGE_SIZE,
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
    let result = match object {
        Mappable::Memory(memory) => asp.map_object(range, prot, memory).map_err(|(_, e)| e),
        Mappable::File(file) => asp.map_file(range, prot, file).map_err(|(_, e)| e),
    };
    result.map_err(map_mem_map_err)?;
    Paging::flush_tlb_all();
    Ok(range.start().as_u64() as isize)
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
    let mut frames = crate::libkern::KVec::new();
    let unmapped = loop {
        match asp.unmap_covering(va, frames) {
            Ok(Some(unmapped)) => break unmapped,
            Ok(None) => return Err(KError::InvalidArgument),
            Err((mut f, pages)) => {
                f.try_reserve(pages as usize).map_err(|_| KError::OutOfMemory)?;
                frames = f;
            }
        }
    };
    // Shot down on every CPU, then freed. IF-masked; `shootdown_all` is IF-robust.
    drop(unmapped);
    Ok(0)
}
