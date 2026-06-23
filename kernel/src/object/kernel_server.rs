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
use crate::object::ObjectRef;
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
    // `/proc/self`, `/dev` — slice 3 Part C.
}

/// The outcome of a resource-server lookup — the umbrella RS contract's return.
///
/// A Kernel Server answers synchronously, so it returns only [`Completed`] or
/// [`Rejected`]. The third state of the full contract, `Pending` (the lookup
/// will complete later, via the PO), belongs to the **userspace** path and is
/// introduced with slice 7; an in-kernel server never blocks, so it is not
/// represented here yet.
///
/// [`Completed`]: OpStatus::Completed
/// [`Rejected`]: OpStatus::Rejected
pub enum OpStatus {
    /// The server produced a handle to a kernel object. The caller installs it
    /// (rights-attenuated) and pre-signals the lookup PO with status `0`.
    Completed(ObjectRef),
    /// The lookup failed; the caller delivers `err` through the lookup PO.
    Rejected(KError),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;

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
        }
    }

    #[test]
    fn entropy_rejects_non_empty_suffix() {
        init_global_heap();
        match dispatch(KernelServerId::Entropy, b"sub", Rights::READ) {
            OpStatus::Rejected(KError::NotFound) => {}
            OpStatus::Rejected(e) => panic!("expected NotFound, got {e:?}"),
            OpStatus::Completed(_) => panic!("a non-empty suffix must not resolve on a leaf"),
        }
    }
}
