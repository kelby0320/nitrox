//! In-kernel resource servers and their dispatch registry.
//!
//! A **Kernel Server** is one of the two kinds of resource server (the other,
//! a Userspace Server, lands in slice 7): an in-kernel function the kernel calls
//! *during a namespace lookup* to produce a handle. See
//! `docs/architecture/namespace-and-resource-servers.md` § "Kernel Servers".
//!
//! A [`BindingTarget::KernelServer`](crate::object::namespace::BindingTarget)
//! binding holds a [`KernelServerId`] — a small dispatch id into the registry
//! below. `sys_ns_lookup` resolves the path to the binding, then calls
//! [`dispatch`] **in the caller's syscall context** with the lookup *suffix* and
//! the requested rights; the server answers with an [`OpStatus`]. The syscall
//! installs the rights-attenuated handle and pre-signals the lookup's
//! `PendingOperation` — reusing the slice-1 direct-handle delivery path, so an
//! in-kernel lookup is synchronous (no IPC, no cross-context install, no new ABI).
//!
//! The contract mirrors the umbrella resource-server contract exactly
//! (`lookup(suffix, rights) -> OpStatus`), which is what makes a Kernel Server
//! substitutable for a Userspace Server at the same path with zero client change.

use crate::libkern::KBox;
use crate::libkern::handle::{KObjectType, Rights};
use crate::object::EntropyObject;
use crate::object::MemoryObject;
use crate::object::ObjectRef;
use crate::object::Process;
use crate::syscall::error::KError;

/// Identifies one in-kernel resource server in the dispatch registry. A
/// [`BindingTarget::KernelServer`](crate::object::namespace::BindingTarget)
/// binding holds one of these; [`dispatch`] fans out on it.
///
/// `Copy` (no backing allocation) — a `KernelServer` binding therefore needs no
/// outside-the-lock drop, unlike a direct-handle binding.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum KernelServerId {
    /// `/dev/entropy` — the kernel CSPRNG (see [`entropy_server`]).
    Entropy,
    /// `/proc/self/process` — the caller's own [`Process`] (see [`proc_self_process`]).
    ProcSelfProcess,
    /// `/proc/self/thread` — the calling [`Thread`](crate::object::Thread)
    /// (see [`proc_self_thread`]).
    ProcSelfThread,
    /// `/proc/self/namespace` — the caller's root
    /// [`Namespace`](crate::object::Namespace) (see [`proc_self_namespace`]).
    ProcSelfNamespace,
    /// `/initramfs/<path>` — a file from the boot CPIO blob, served as a
    /// read-only [`MemoryObject`] copy (see [`initramfs_server`]).
    Initramfs,
    /// `/dev/blk/<n>` — the `n`-th discovered block device, served as a
    /// [`DeviceNode`](crate::object::DeviceNode) handle (see [`block_device_server`]).
    /// One binding (at `/dev/blk`) owns the whole subtree; the suffix indexes the
    /// device-table registry.
    BlockDevice,
    // `/proc/self/status` (numeric pid/tid) and the `/dev` directory listing are
    // deferred — see `docs/rationale/deferred-decisions.md`.
}

/// The outcome of a resource-server lookup — the umbrella RS contract's return.
///
/// A **Kernel Server** answers synchronously, so it returns only [`Completed`] or
/// [`Rejected`]. The third state, [`Pending`], belongs to the **userspace** path
/// (slice 7): the kernel forwarded the lookup over IPC and the lookup's
/// `PendingOperation` will be completed later, when the server replies. An
/// in-kernel server never returns it (it never blocks).
///
/// [`Completed`]: OpStatus::Completed
/// [`Rejected`]: OpStatus::Rejected
/// [`Pending`]: OpStatus::Pending
pub enum OpStatus {
    /// The server produced a handle to a kernel object. The caller installs it
    /// (rights-attenuated) and pre-signals the lookup PO with status `0`.
    Completed(ObjectRef),
    /// The lookup failed; the caller delivers `err` through the lookup PO.
    Rejected(KError),
    /// The lookup was forwarded to a Userspace Server; its `PendingOperation` is
    /// left **uncompleted** and will be signalled when the server's reply arrives
    /// (the `sys_ns_lookup` forwarding arm). Only the userspace path produces this.
    Pending,
}

/// Call the in-kernel server identified by `id` with the lookup `suffix` (the
/// part of the path past the binding prefix, leading `/` stripped — empty on an
/// exact match) and the `requested` rights. Runs in the caller's syscall
/// context, so a server may read the calling process/thread directly (the
/// `/proc/self` servers will, in Part C).
///
/// Rights *attenuation* is the lookup syscall's job (`requested ∩ binding.rights`
/// is applied to whatever object the server returns), so a server hands back a
/// full-rights object and need not attenuate itself.
pub fn dispatch(id: KernelServerId, suffix: &[u8], requested: Rights) -> OpStatus {
    match id {
        KernelServerId::Entropy => entropy_server(suffix, requested),
        KernelServerId::ProcSelfProcess => proc_self_process(suffix, requested),
        KernelServerId::ProcSelfThread => proc_self_thread(suffix, requested),
        KernelServerId::ProcSelfNamespace => proc_self_namespace(suffix, requested),
        KernelServerId::Initramfs => initramfs_server(suffix, requested),
        KernelServerId::BlockDevice => block_device_server(suffix, requested),
    }
}

/// `/dev/entropy` — a **leaf** server: it owns exactly the bound path and has no
/// sub-resources, so any non-empty `suffix` is *not found*. An exact match mints
/// a fresh [`EntropyObject`] token onto the global CSPRNG (the same object
/// `sys_entropy_create` returns); the caller then `sys_entropy_read`s it.
///
/// `requested` is accepted to match the RS contract but ignored — the binding's
/// rights cap what the caller obtains, applied by the lookup syscall.
fn entropy_server(suffix: &[u8], _requested: Rights) -> OpStatus {
    if !suffix.is_empty() {
        return OpStatus::Rejected(KError::NotFound);
    }
    match EntropyObject::try_new() {
        // Adopt the single creation reference into an `ObjectRef` for the caller
        // to install.
        // SAFETY: `into_raw` yields the one outstanding creation reference.
        Ok(obj) => OpStatus::Completed(unsafe {
            ObjectRef::from_raw(KBox::into_raw(obj).as_ptr() as *mut (), KObjectType::EntropyObject)
        }),
        Err(_) => OpStatus::Rejected(KError::OutOfMemory),
    }
}

// --- /proc/self — self-reference servers (no ambient authority) -----------
//
// Each is a **leaf** (non-empty suffix → `NotFound`) that returns the **caller's
// own** object, derived from the running syscall context — there is no pid
// parameter to forge, and the facility is reachable only if a supervisor bound it
// into the caller's namespace. See `docs/architecture/namespace-and-resource-servers.md`
// § "`/proc/self`". The returned `ObjectRef` is a clone (an atomic refcount bump),
// owned by the caller; rights attenuation to the binding's cap is the lookup
// syscall's job. `None` (a kernel/boot thread with no process) → `NotFound`.

/// `/proc/self/process` — the caller's own [`Process`] handle.
fn proc_self_process(suffix: &[u8], _requested: Rights) -> OpStatus {
    if !suffix.is_empty() {
        return OpStatus::Rejected(KError::NotFound);
    }
    match crate::sched::current_process() {
        Some(obj) => OpStatus::Completed(obj),
        None => OpStatus::Rejected(KError::NotFound),
    }
}

/// `/proc/self/thread` — the calling [`Thread`](crate::object::Thread) handle.
fn proc_self_thread(suffix: &[u8], _requested: Rights) -> OpStatus {
    if !suffix.is_empty() {
        return OpStatus::Rejected(KError::NotFound);
    }
    match crate::sched::current_thread() {
        Some(obj) => OpStatus::Completed(obj),
        None => OpStatus::Rejected(KError::NotFound),
    }
}

/// `/proc/self/namespace` — the caller's root [`Namespace`](crate::object::Namespace)
/// handle (the same object `Process::namespace` resolves names against).
fn proc_self_namespace(suffix: &[u8], _requested: Rights) -> OpStatus {
    if !suffix.is_empty() {
        return OpStatus::Rejected(KError::NotFound);
    }
    let ns = crate::sched::current_process().and_then(|p| {
        // SAFETY: `current_process` returns a live `Process` ObjectRef; `p` pins it
        // for this borrow. `namespace_ref` clones the stored namespace ObjectRef.
        unsafe { &*(p.as_ptr() as *const Process) }.namespace_ref()
    });
    match ns {
        Some(obj) => OpStatus::Completed(obj),
        None => OpStatus::Rejected(KError::NotFound),
    }
}

/// `/initramfs/<path>` — serve a file from the boot CPIO blob as a fresh
/// read-only [`MemoryObject`] (a copy of the file's bytes; the caller maps it
/// `MAP_READ`). The `suffix` is the path under `/initramfs` (no leading `/`); an
/// empty suffix, no loaded initramfs, or an absent file is `NotFound`. Unlike the
/// other in-kernel servers this is a **subtree** server: it uses the suffix.
fn initramfs_server(suffix: &[u8], _requested: Rights) -> OpStatus {
    if suffix.is_empty() {
        return OpStatus::Rejected(KError::NotFound);
    }
    let Some(blob) = crate::initramfs::blob() else {
        return OpStatus::Rejected(KError::NotFound);
    };
    let Some(data) = crate::initramfs::lookup(blob, suffix) else {
        return OpStatus::Rejected(KError::NotFound);
    };
    match MemoryObject::try_new_filled(data) {
        // Adopt the creation reference into an `ObjectRef` for the caller.
        // SAFETY: `into_raw` yields the one outstanding creation reference.
        Ok(obj) => OpStatus::Completed(unsafe {
            ObjectRef::from_raw(KBox::into_raw(obj).as_ptr() as *mut (), KObjectType::MemoryObject)
        }),
        Err(_) => OpStatus::Rejected(KError::OutOfMemory),
    }
}

/// `/dev/blk/<n>` — a **subtree** server over the block-device registry. The
/// `suffix` is a decimal index (`/dev/blk/0` ⇒ `b"0"`); it resolves to the
/// `n`-th block [`DeviceNode`](crate::object::DeviceNode), on which the caller
/// issues `sys_io_submit` reads. An empty or non-numeric suffix, or an index
/// past the discovered disks, is *not found*. One binding (at `/dev/blk`) serves
/// every disk; the binding is read-only in Phase 2, so the lookup attenuates the
/// returned handle to `READ` (write IoOps are rejected at the rights gate).
///
/// `requested` is accepted to match the RS contract but ignored — the binding's
/// rights cap what the caller obtains, applied by the lookup syscall.
fn block_device_server(suffix: &[u8], _requested: Rights) -> OpStatus {
    let Some(index) = parse_index(suffix) else {
        return OpStatus::Rejected(KError::NotFound);
    };
    match crate::device::find_block_device(index) {
        Some(node) => OpStatus::Completed(node),
        None => OpStatus::Rejected(KError::NotFound),
    }
}

/// Parse `s` as a non-empty run of ASCII decimal digits into a `usize`, or
/// `None` (empty, non-digit, or overflow).
fn parse_index(s: &[u8]) -> Option<usize> {
    if s.is_empty() {
        return None;
    }
    let mut n: usize = 0;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as usize)?;
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;

    #[test]
    fn parse_index_accepts_decimal_rejects_junk() {
        assert_eq!(parse_index(b"0"), Some(0));
        assert_eq!(parse_index(b"7"), Some(7));
        assert_eq!(parse_index(b"42"), Some(42));
        assert_eq!(parse_index(b""), None); // empty (the /dev/blk exact match)
        assert_eq!(parse_index(b"1a"), None); // trailing junk
        assert_eq!(parse_index(b"sda"), None); // non-numeric
        assert_eq!(parse_index(b"0/sub"), None); // a deeper path
    }

    #[test]
    fn entropy_exact_match_yields_entropy_object() {
        init_global_heap();
        match dispatch(KernelServerId::Entropy, b"", Rights::READ) {
            OpStatus::Completed(obj) => {
                assert_eq!(obj.object_type(), KObjectType::EntropyObject);
                // Drop releases the creation reference (no handle installed here).
                drop(obj);
            }
            OpStatus::Rejected(e) => panic!("expected Completed, got Rejected({e:?})"),
            OpStatus::Pending => panic!("a kernel server never returns Pending"),
        }
    }

    #[test]
    fn entropy_rejects_non_empty_suffix() {
        init_global_heap();
        match dispatch(KernelServerId::Entropy, b"sub", Rights::READ) {
            OpStatus::Rejected(KError::NotFound) => {}
            OpStatus::Rejected(e) => panic!("expected NotFound, got {e:?}"),
            OpStatus::Completed(_) => panic!("a non-empty suffix must not resolve on a leaf"),
            OpStatus::Pending => panic!("a kernel server never returns Pending"),
        }
    }

    // The `/proc/self/*` leaves reject a non-empty suffix; that arm runs *before*
    // any scheduler access, so it is reachable host-side. Their success arms need a
    // running syscall context (`current_process`/`current_thread`) and are covered
    // by the QEMU `proc_self_demo`, not host tests.
    #[test]
    fn proc_self_leaves_reject_non_empty_suffix() {
        for id in [
            KernelServerId::ProcSelfProcess,
            KernelServerId::ProcSelfThread,
            KernelServerId::ProcSelfNamespace,
        ] {
            match dispatch(id, b"sub", Rights::empty()) {
                OpStatus::Rejected(KError::NotFound) => {}
                OpStatus::Rejected(e) => panic!("{id:?}: expected NotFound, got {e:?}"),
                OpStatus::Completed(_) => panic!("{id:?}: a non-empty suffix must not resolve"),
                OpStatus::Pending => panic!("{id:?}: a kernel server never returns Pending"),
            }
        }
    }
}
