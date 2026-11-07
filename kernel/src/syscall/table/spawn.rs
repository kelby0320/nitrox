//! `sys_process_spawn`: a child process from an image, and the handles it starts with.

use super::*;
use crate::libkern::KVec;

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
    if (1..count).any(|i| args.handles[..i].contains(&args.handles[i])) {
        return Err(KError::InvalidArgument);
    }
    let parent_pid = crate::sched::current_owner_pid();

    let img =
        global::get().lookup(RawHandle(args.image.bits()), parent_pid, Rights::MAP_READ).map_err(map_handle_err)?;
    let image_started = Timer::read_ns();
    let img_bytes = image_bytes(&img.object)?;
    spawn_stats::record(
        0, // the whole-call duration is added by the wrapper
        Timer::read_ns().saturating_sub(image_started),
        img_bytes.len() as u64,
    );

    let mut asp = AddressSpace::new().map_err(|_| KError::OutOfMemory)?;
    // A spawner's ELF is untrusted input: the loader bounds-checks every offset.
    let info = load_elf(&mut asp, &img_bytes).map_err(from_elf_load)?;
    drop(img);
    let child_pid = crate::sched::alloc_pid().ok_or(KError::OutOfMemory)?;

    // `child = parent & requested` syscaps (`docs/architecture/syscaps.md`); no owning Process
    // yields an unprivileged child.
    let child_syscaps = {
        let requested = crate::libkern::SysCaps::from_bits_truncate(args.syscaps);
        match crate::sched::current_process() {
            Some(pp) => pp.syscaps() & requested,
            None => crate::libkern::SysCaps::empty(),
        }
    };

    let mut proc_box = Process::try_new_user(child_pid, asp, child_syscaps).map_err(|_| KError::OutOfMemory)?;
    if let Some(c) = crate::sched::current_process().and_then(|p| p.notification_channel_ref()) {
        proc_box.set_parent_notif(c);
    }

    let chan = NotificationChannel::try_new().map_err(|_| KError::OutOfMemory)?;
    let chan_ref = Ref::new(chan);
    proc_box.set_notification_channel(chan_ref.clone()); // refcount 2
    // Every handle the child is given is closed again, as `proc_box` drops, unless it starts.
    let mut child = installing::<{ SPAWN_MAX_HANDLES + 2 }>(child_pid);
    let notif_h =
        child.install(chan_ref.erase(), Rights::WAIT | Rights::DUPLICATE | Rights::INSPECT).map_err(refused)?;

    // The child's root: `args.namespace` if the parent holds it with `LOOKUP`, else the parent's
    // own; always a **LOOKUP-only** handle, so the child cannot rebind its root.
    let ns_source: Option<ObjectRef> = if !args.namespace.is_null() {
        Some(lookup_as::<Namespace>(args.namespace.bits(), parent_pid, Rights::LOOKUP)?.erase())
    } else {
        crate::sched::current_process().and_then(|p| p.namespace_ref())
    };
    let child_ns_h = match ns_source {
        Some(ns_ref) => {
            proc_box.set_namespace(ns_ref.clone()); // the child owns a reference
            child.install(ns_ref, Rights::LOOKUP).map_err(refused)?
        }
        None => RawHandle::NULL,
    };

    let mut endpoint_h = RawHandle::NULL;
    let mut claims: [Option<Claim<'_>>; SPAWN_MAX_HANDLES] = core::array::from_fn(|_| None);
    for (i, claim) in claims.iter_mut().enumerate().take(count) {
        // A grant not moved is a copy, and a copy needs DUPLICATE, as `sys_handle_duplicate` does.
        // A move is claimed until the spawn commits, so a second move of it at once fails.
        let found = if args.move_mask & (1 << i) == 0 {
            global::get().lookup(args.handles[i], parent_pid, Rights::TRANSFER | Rights::DUPLICATE)
        } else {
            global::get().claim(args.handles[i], parent_pid, Rights::TRANSFER).map(|(ok, c)| {
                *claim = Some(c);
                ok
            })
        };
        let ok = found.map_err(map_handle_err)?;
        let child_rights = ok.rights & Rights::from_bits_truncate(args.rights[i]);
        let h = child.install(ok.object, child_rights).map_err(refused)?;
        if i == 0 {
            endpoint_h = h;
        }
    }

    let proc_obj = Ref::new(proc_box);

    // The parent's handle to the child before the child runs: nothing may fail once it does.
    let mut parent = installing::<1>(parent_pid);
    let parent_handle =
        parent.install(proc_obj.clone().erase(), Rights::SIGNAL | Rights::TERMINATE).map_err(refused)?;

    let boot = [notif_h.bits(), child_ns_h.bits(), endpoint_h.bits(), args.arg0];
    crate::sched::spawn_user(proc_obj, info.entry_point.as_u64(), info.stack_top.as_u64(), boot)
        .map_err(|_| KError::OutOfMemory)?;
    commit_all(&mut claims);
    child.commit();
    parent.commit();
    Ok(parent_handle.bits() as isize)
}

/// A spawn's image as one buffer for the loader, read through the HHDM: a borrowed one is refused.
pub(super) fn image_bytes(img: &ObjectRef) -> Result<KVec<u8>, KError> {
    if let Some(mo) = img.downcast_ref::<MemoryObject>() {
        if !mo.kernel_reachable() {
            return Err(KError::InvalidArgument);
        }
        image_fits(mo.size())?;
        return mo.copy_to_kvec().map_err(|_| KError::OutOfMemory);
    }
    // Blocks on each fill — sound on the syscall thread, which holds no AS/cache lock.
    let file = img.clone().downcast::<FileObject>().map_err(|_| KError::InvalidArgument)?;
    image_fits(file.size())?;
    FileObject::read_to_kvec(&file).map_err(|_| KError::OutOfMemory)
}
