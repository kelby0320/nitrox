//! `sys_ns_enumerate`: a namespace's bindings, a batch a call.

use super::*;

/// `sys_ns_enumerate(ns, index, out, count)` — write the bindings of `ns` from the `index`-th
/// (insertion order) to the user [`NsEntry`] array at `out`, at most `count` of them, `1` to
/// [`NS_ENUMERATE_MAX`]: path, target kind (`NS_KIND_*`) and rights. Requires `LOOKUP`. Returns
/// how many it wrote, fewer than `count` only at the end, `0` past it. Each entry is read under the
/// namespace's lock in turn, so a bind or unbind beside a listing may shift what follows. Lists
/// **bindings**, not filesystem `readdir`.
pub fn sys_ns_enumerate(ns_h: u64, index: u64, out: u64, count: u64) -> SysResult {
    let size = entries(out, count)?;
    let pid = crate::sched::current_owner_pid();
    let ns = lookup_as::<Namespace>(ns_h, pid, Rights::LOOKUP)?;
    let (mut written, mut entry) = (0, NsEntry::zeroed());
    while written < count && ns.enumerate(index.saturating_add(written) as usize, &mut entry) {
        let uptr = UserMutPtr::<NsEntry>::new(out + written * size).map_err(from_user_access)?;
        copy_to_user(uptr, &entry).map_err(from_user_access)?;
        written += 1;
    }
    Ok(written as isize)
}

/// A listing's array of `count` entries at `out`, refused unless it is `1` to `NS_ENUMERATE_MAX`
/// entries wholly in the user half: an entry's size.
pub(super) fn entries(out: u64, count: u64) -> Result<u64, KError> {
    if count == 0 || count > NS_ENUMERATE_MAX {
        return Err(KError::InvalidArgument);
    }
    let size = core::mem::size_of::<NsEntry>() as u64;
    let last = out.checked_add((count - 1) * size).ok_or(KError::InvalidArgument)?;
    UserMutPtr::<NsEntry>::new(last).map_err(from_user_access)?;
    Ok(size)
}
