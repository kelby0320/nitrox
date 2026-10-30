//! `sys_process_spawn`: a child process from an image, and the handles it starts with.

use super::*;
use crate::libkern::KVec;

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
    let mut claims: [Option<Claim<'_>>; SPAWN_MAX_HANDLES] = core::array::from_fn(|_| None);
    for i in 0..count {
        // A grant not moved is a copy, and a copy needs DUPLICATE, as `sys_handle_duplicate` does.
        // A move is claimed until the spawn commits, so a second move of it at once fails.
        let found = if args.move_mask & (1 << i) == 0 {
            global::get().lookup(args.handles[i], parent_pid, Rights::TRANSFER | Rights::DUPLICATE)
        } else {
            global::get().claim(args.handles[i], parent_pid, Rights::TRANSFER).map(|(ok, c)| {
                claims[i] = Some(c);
                ok
            })
        };
        let ok = match found {
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

    // The parent's handle to the child before the child runs: nothing may fail once it does.
    let parent_handle = match install(parent_pid, proc_obj.clone(), Rights::SIGNAL | Rights::TERMINATE) {
        Ok(h) => h,
        Err(e) => {
            drop(proc_obj);
            spawn_rollback_child_handles(child_pid, notif_h, child_ns_h, &installed[..count]);
            return Err(e);
        }
    };

    let endpoint_h = if count > 0 { installed[0].bits() } else { 0 };
    let boot = [notif_h.bits(), child_ns_h.bits(), endpoint_h, args.arg0];
    if crate::sched::spawn_user(proc_obj, info.entry_point.as_u64(), info.stack_top.as_u64(), boot).is_err() {
        close_and_release(parent_handle, parent_pid);
        spawn_rollback_child_handles(child_pid, notif_h, child_ns_h, &installed[..count]);
        return Err(KError::OutOfMemory);
    }
    commit_all(&mut claims);

    Ok(parent_handle.bits() as isize)
}

/// A spawn's image as one buffer for the loader, read through the HHDM: a borrowed one is refused.
pub(super) fn image_bytes(img: &ObjectRef) -> Result<KVec<u8>, KError> {
    match img.object_type() {
        KObjectType::MemoryObject => {
            // SAFETY: type checked above; `img` pins it live.
            let mo = unsafe { &*(img.as_ptr() as *const MemoryObject) };
            if !mo.kernel_reachable() {
                return Err(KError::InvalidArgument);
            }
            image_fits(mo.size())?;
            mo.copy_to_kvec().map_err(|_| KError::OutOfMemory)
        }
        // Blocks on each fill — sound on the syscall thread, which holds no AS/cache lock.
        KObjectType::FileObject => {
            // SAFETY: type checked above; `img` pins it live.
            image_fits(unsafe { &*(img.as_ptr() as *const FileObject) }.size())?;
            FileObject::read_to_kvec(img).map_err(|_| KError::OutOfMemory)
        }
        _ => Err(KError::InvalidArgument),
    }
}
