//! Making what was written durable: a file's pages, and a mount's.

use super::*;

/// `sys_file_sync(handle)` — flush a writable file mapping's resident pages to the device.
/// Requires `MAP_WRITE` on the `FileObject` handle. Blocks on the write IRPs (async-first-exempt
/// on a syscall thread). `0` on success, else a `KError`.
pub(super) fn sys_file_sync(handle: u64) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    let file = lookup_as::<FileObject>(handle, pid, Rights::MAP_WRITE)?;
    write_back(&file)?;
    Ok(0)
}

/// Write one file's pages back, tell its server, and unpin it if no writable mapping appeared.
fn write_back(file_obj: &Ref<FileObject>) -> Result<(), KError> {
    let mark = file_obj.clean_mark();
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
pub(super) fn sys_ns_sync(ns_h: u64, path_ptr: u64, path_len: usize) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    let ns = lookup_as::<Namespace>(ns_h, pid, Rights::LOOKUP)?;
    let mut buf = [0u8; NS_PATH_MAX];
    let path = copy_ns_path(path_ptr, path_len, &mut buf)?;
    validate_path(path).map_err(|_| KError::InvalidArgument)?;
    let reg = match ns.resolve(path) {
        Some((ResolvedTarget::UserspaceServer(reg, _), _, _)) => reg,
        Some(_) => return Err(KError::Unsupported),
        None => return Err(KError::NotFound),
    };
    let files = reg.cache_objects().map_err(|_| KError::OutOfMemory)?;
    let mut written = 0isize;
    let mut result = Ok(());
    for f in files.iter() {
        if !f.is_dirty() {
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
pub(super) fn sys_ns_held(ns_h: u64, path_ptr: u64, path_len: usize) -> SysResult {
    let pid = crate::sched::current_owner_pid();
    let ns = lookup_as::<Namespace>(ns_h, pid, Rights::LOOKUP)?;
    let mut buf = [0u8; NS_PATH_MAX];
    let path = copy_ns_path(path_ptr, path_len, &mut buf)?;
    validate_path(path).map_err(|_| KError::InvalidArgument)?;
    let reg = match ns.resolve(path) {
        Some((ResolvedTarget::UserspaceServer(reg, _), _, _)) => reg,
        Some(_) => return Err(KError::Unsupported),
        None => return Err(KError::NotFound),
    };
    crate::io::block::reclaim_completed();
    // Taken under the cache's lock, dropped after: each may be the last and `Drop` takes it.
    let files = reg.cache_objects().map_err(|_| KError::OutOfMemory)?;
    let held = files.len() as isize;
    drop(files);
    drop(reg);
    Ok(held)
}

/// Send `File::Touch` for a just-flushed Model A file so its server can stamp `mtime`. Stamps
/// on **sync**, not on the write (no per-page dirty bit, `TODO(page-dirty-tracking)`). Silent
/// on failure: the data is durable, and a dropped touch costs only a stale `mtime`.
fn notify_file_touched(file_obj: &Ref<FileObject>) {
    use crate::libkern::KBox;
    use crate::object::FileObject;

    let Some((reg, file_id)) = FileObject::touch_target(file_obj) else {
        return; // not a cached Model A file — nothing to notify
    };
    // Heap-bounced: a 4 KiB `StoredMsg` does not belong on a measured kernel stack.
    // Zeroed in place: the by-value form would put 4 KiB in this frame, on a sync path that is
    // already among the deepest in the kernel.
    let Ok(mut msg) = KBox::<crate::object::ipc_channel::StoredMsg>::try_new_zeroed() else {
        return;
    };
    let Some(body_len) = crate::rsproto::build_touch_request(&mut msg.payload, file_id) else {
        return;
    };
    msg.header.payload_len = body_len as u32;
    msg.header.handle_count = 0;
    crate::sched::us_forward_notify(&reg, &mut msg);
}
