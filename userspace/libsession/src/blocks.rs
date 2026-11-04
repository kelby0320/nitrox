//! Handing a session's view broker block devices, and taking them back.

use crate::resolve;
use alloc::vec::Vec;
use libkern::abi::NS_ENTRY_PATH_MAX;
use libkern::{RawHandle, Rights};
use libos::{CanLookup, MapRead, Memory, Namespace, NsMutable, NsReadOnly, ReadWrite, Ref, Resource, log};

/// **Hand every block device `from`'s registry lists to `to`, except those whose registry ids are
/// in `withheld`**, each with its `info` snapshot; returns how many were bound. The view broker's
/// `disks` grant, the only way a disk reaches a program; it withholds what is in use, since a raw
/// write would land underneath a live server. `sys_ns_bind` cannot re-bind the kernel-server
/// `/dev/blk`, so each device is bound singly. **Only a source with a registry hands anything
/// on**; the broker's is the root namespace.
pub fn share_block_devices<'a, 'b, M: CanLookup + 'a>(
    from: impl Into<Ref<'a, Namespace, M>>,
    to: impl Into<Ref<'b, Namespace, NsMutable>>,
    withheld: &[u32],
) -> usize {
    let (from, to) = (from.into(), to.into());
    let mut bound = 0;
    for n in block_indices(from, withheld) {
        let path = blk_path(n, false);
        let Ok(dev) = resolve::<Resource, ReadWrite, _>(
            from,
            &path,
            Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER,
        ) else {
            // Not the end of the list: a source granted one disk has a gap before it.
            continue;
        };
        // A direct-handle bind, so no subtree base; it takes its own reference.
        if to.bind(&path, &dev).is_err() {
            continue;
        }
        bound += 1;
        // A snapshot, resolved once: device facts do not change while a machine runs.
        let info_path = blk_path(n, true);
        if let Ok(info) = resolve::<Memory, MapRead, _>(from, &info_path, Rights::MAP_READ | Rights::TRANSFER)
            && to.bind(&info_path, &info).is_err()
        {
            log::write(b"libsession: a device's info leaf would not bind\n");
        }
    }
    if bound > 0 {
        log::Line::new().s(b"libsession: ").u(bound as u64).s(b" block device(s) handed to a namespace").end();
    }
    bound
}

/// [`share_block_devices`] between raw namespaces the caller keeps open. For callers not yet on
/// libos; the typed form is [`share_block_devices`].
// TODO(raw-handle-shims): goes when its last caller is on libos.
pub fn rebind_block_devices_except(from_ns: u64, to_ns: u64, withheld: &[u32]) -> usize {
    share_block_devices(namespace::<NsReadOnly>(from_ns), namespace::<NsMutable>(to_ns), withheld)
}

/// Take back what [`share_block_devices`] bound into `ns`; returns how many devices were unbound.
/// A handle a program already holds is not taken back — the kernel has no revocation. **What is
/// bound is read from `ns` itself**, not a fixed range, which could miss a disk.
pub fn withdraw_block_devices<'a>(ns: impl Into<Ref<'a, Namespace, NsMutable>>) -> usize {
    let ns = ns.into();
    let mut unbound = 0;
    for n in bound_block_indices(ns) {
        // The leaf first, so a device is never left without its `info`; `NotFound` for a leaf
        // never bound is no error.
        let _ = ns.unbind(&blk_path(n, true));
        if ns.unbind(&blk_path(n, false)).is_ok() {
            unbound += 1;
        }
    }
    unbound
}

/// [`withdraw_block_devices`] in a raw `ns` the caller keeps open. For callers not yet on libos; the
/// typed form is [`withdraw_block_devices`].
// TODO(raw-handle-shims): goes when its last caller is on libos.
pub fn unbind_block_devices(ns: u64) -> usize {
    withdraw_block_devices(namespace::<NsMutable>(ns))
}

fn namespace<'a, M>(raw: u64) -> Ref<'a, Namespace, M> {
    // SAFETY: the caller's own namespace, open for the call, as the function always asked.
    unsafe { Ref::from_raw(RawHandle(raw), Rights::empty()) }
}

/// The `/dev/blk/<n>` indices `ns` can reach, less `withheld` registry ids. **From the registry**:
/// `/dev/blk` is one kernel-server binding whose children no enumeration sees.
fn block_indices<M: CanLookup>(ns: Ref<'_, Namespace, M>, withheld: &[u32]) -> Vec<u32> {
    let Some(blocks) = registry_blocks(ns) else { return Vec::new() };
    let mut out: Vec<u32> = blocks.iter().filter(|(id, _)| !withheld.contains(id)).map(|&(_, n)| n).collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn registry_blocks<M: CanLookup>(ns: Ref<'_, Namespace, M>) -> Option<Vec<(u32, u32)>> {
    let snap = resolve::<Memory, MapRead, _>(ns, "/dev/registry", Rights::MAP_READ | Rights::INSPECT).ok()?;
    let size = snap.stat().ok()?.size as usize;
    let map = snap.map(size).ok()?;
    let mut bytes = alloc::vec![0u8; size];
    map.read_into(0, &mut bytes).ok()?;
    let out = match libkern::device::records(&bytes) {
        Ok(records) => records.filter_map(|r| r.block_index().map(|n| (r.id, n))).collect(),
        Err(_) => Vec::new(),
    };
    Some(out)
}

fn bound_block_indices<M: CanLookup>(ns: Ref<'_, Namespace, M>) -> Vec<u32> {
    // NotFound ends the walk, and so does any other refusal.
    let mut out: Vec<u32> = ns
        .bindings()
        .map_while(Result::ok)
        .filter_map(|entry| blk_binding_index(&entry.path[..(entry.path_len as usize).min(NS_ENTRY_PATH_MAX)]))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The `<n>` of a binding at exactly `/dev/blk/<n>`; decimal with no leading zero, so a device
/// has one spelling.
pub(crate) fn blk_binding_index(path: &[u8]) -> Option<u32> {
    let digits = path.strip_prefix(b"/dev/blk/")?;
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) || (digits.len() > 1 && digits[0] == b'0') {
        return None;
    }
    core::str::from_utf8(digits).ok()?.parse().ok()
}

fn blk_path(n: u32, info: bool) -> alloc::string::String {
    if info { alloc::format!("/dev/blk/{n}/info") } else { alloc::format!("/dev/blk/{n}") }
}

#[cfg(test)]
mod tests;
