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
    let written = fill(&ns, index, count, out, size, |at, entry| {
        let uptr = UserMutPtr::<NsEntry>::new(at).map_err(from_user_access)?;
        copy_to_user(uptr, entry).map_err(from_user_access)
    })?;
    Ok(written as isize)
}

/// A listing's array of `count` entries at `out`, refused before any lookup unless it is `1` to
/// `NS_ENUMERATE_MAX` entries (`InvalidArgument`), aligned (`InvalidArgument`) and wholly in the
/// user half (`FaultFromUser`): an entry's size.
pub(super) fn entries(out: u64, count: u64) -> Result<u64, KError> {
    if count == 0 || count > NS_ENUMERATE_MAX {
        return Err(KError::InvalidArgument);
    }
    let size = core::mem::size_of::<NsEntry>() as u64;
    UserMutPtr::<NsEntry>::new(out).map_err(from_user_access)?;
    if out.checked_add(count * size).is_none_or(|end| end > USER_VIRT_END) {
        return Err(KError::FaultFromUser);
    }
    Ok(size)
}

/// Up to `count` bindings of `ns` from the `index`-th, each a fresh entry `put` at its place: how many.
pub(super) fn fill(
    ns: &Namespace,
    index: u64,
    count: u64,
    out: u64,
    size: u64,
    mut put: impl FnMut(u64, &NsEntry) -> Result<(), KError>,
) -> Result<u64, KError> {
    let mut written = 0;
    while written < count {
        let mut entry = NsEntry::zeroed();
        if !ns.enumerate(index.saturating_add(written) as usize, &mut entry) {
            break;
        }
        put(out + written * size, &entry)?;
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests;
