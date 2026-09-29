//! The [`SystemControl`] kernel object — the capability to stop the machine (administration
//! Part E.3).
//!
//! **One exists**, made at boot and handed to `init` in `rdx`, with `WRITE` and `INSPECT` and
//! neither `DUPLICATE` nor `TRANSFER`: `init` cannot duplicate it, send it, grant it at spawn or
//! bind it in a namespace, so the process that stops the machine is the one the kernel started
//! first. Like an [`EntropyObject`](super::EntropyObject)
//! it is a **token** and carries no state — the machine it controls is the one there is.
//! `sys_power` looks it up, requiring `WRITE`, and flushes, stops and halts or resets through
//! [`crate::power`]. See `docs/architecture/power.md`.

use crate::libkern::handle::{KObjectType, Rights};
use crate::libkern::{AllocError, KBox};
use crate::object::header::KObjectHeader;

/// The rights `init`'s handle carries: `WRITE`, which `sys_power` requires, and `INSPECT`, so it
/// can check what it was handed — and neither `DUPLICATE` nor `TRANSFER`, so it can give the
/// handle to no one.
pub const INIT_RIGHTS: Rights = Rights::WRITE.union(Rights::INSPECT);

/// The capability to stop or reset the machine.
///
/// `#[repr(C)]` with [`KObjectHeader`] first — see [`crate::object::header`].
#[repr(C)]
pub struct SystemControl {
    header: KObjectHeader,
    /// Self-check sentinel; a live object always reads [`SystemControl::MAGIC`].
    magic: u64,
}

impl SystemControl {
    /// Sentinel written into [`SystemControl::magic`] at construction.
    pub const MAGIC: u64 = 0x5379_7343_746c_2121; // "SysCtl!!"

    /// Allocate the token with a refcount of one.
    pub fn try_new() -> Result<KBox<Self>, AllocError> {
        let header = KObjectHeader::new(KObjectType::SystemControl);
        KBox::try_new(Self { header, magic: Self::MAGIC })
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }
}

// No `Drop`: the object owns nothing, so the `KBox` drop run by `dispatch_destroy` suffices.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;
    use crate::object::ObjectRef;
    use crate::object::header::test_probe;

    #[test]
    fn try_new_has_magic() {
        init_global_heap();
        let s = SystemControl::try_new().unwrap();
        assert!(s.magic_ok());
    }

    #[test]
    fn dropping_last_objectref_routes_through_dispatch_destroy() {
        init_global_heap();
        test_probe::reset();
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        let r = unsafe {
            ObjectRef::from_raw(
                KBox::into_raw(SystemControl::try_new().unwrap()).as_ptr() as *mut (),
                KObjectType::SystemControl,
            )
        };
        assert_eq!(test_probe::system_control_destroys(), 0);
        drop(r);
        assert_eq!(test_probe::system_control_destroys(), 1);
    }
}
