//! The [`EntropyObject`] kernel object — a capability token onto the kernel CSPRNG.
//!
//! The random source is a singleton (the one global generator in
//! [`crate::entropy`]); an `EntropyObject` is a handle-bearing **token** that
//! authorizes reading from it, exactly the way many handles can refer to one
//! object. It therefore carries no per-handle state — every `EntropyObject` draws
//! from the same pool. `sys_entropy_read` looks one up (requiring `READ`) and
//! draws via [`crate::entropy::fill`]. See `docs/architecture/entropy.md`.

use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox};
use crate::object::header::KObjectHeader;

/// An entropy-source capability token.
///
/// `#[repr(C)]` with [`KObjectHeader`] first — see [`crate::object::header`].
#[repr(C)]
pub struct EntropyObject {
    header: KObjectHeader,
    /// Self-check sentinel; a live object always reads [`EntropyObject::MAGIC`].
    magic: u64,
}

impl EntropyObject {
    /// Sentinel written into [`EntropyObject::magic`] at construction.
    pub const MAGIC: u64 = 0x456e_7472_6f70_7921; // "Entropy!"

    /// Allocate an entropy-source token with a refcount of one.
    pub fn try_new() -> Result<KBox<Self>, AllocError> {
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::EntropyObject),
            magic: Self::MAGIC,
        })
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }
}

// No `Drop`: the object owns nothing (the CSPRNG is the global singleton), so the
// `KBox` drop run by `dispatch_destroy` suffices.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;
    use crate::object::ObjectRef;
    use crate::object::header::test_probe;

    #[test]
    fn try_new_has_magic() {
        init_global_heap();
        let e = EntropyObject::try_new().unwrap();
        assert!(e.magic_ok());
    }

    #[test]
    fn dropping_last_objectref_routes_through_dispatch_destroy() {
        init_global_heap();
        test_probe::reset();
        // Adopt the object into an `ObjectRef` — the path a real handle release
        // takes — and drop it; the last reference runs `dispatch_destroy`'s arm.
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        let r = unsafe {
            ObjectRef::from_raw(
                KBox::into_raw(EntropyObject::try_new().unwrap()).as_ptr() as *mut (),
                KObjectType::EntropyObject,
            )
        };
        assert_eq!(test_probe::entropy_object_destroys(), 0);
        drop(r);
        assert_eq!(test_probe::entropy_object_destroys(), 1);
    }
}
