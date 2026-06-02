//! The [`Thread`] kernel object.
//!
//! Minimal this slice: a [`KObjectHeader`] plus the thread identifier and
//! its owning process id. A thread is conceptually a register state, an
//! FPU context, a kernel stack, scheduling parameters, and a TLS base
//! (see `docs/architecture/overview.md`), but those fields arrive with
//! the threading-and-context-switch slice — see
//! `docs/planning/implementation-plan.md`. The owning process is
//! referenced by id rather than by pointer to avoid a refcount cycle and
//! an object-graph this slice does not yet need.

use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox};
use crate::object::header::KObjectHeader;

/// A thread kernel object.
///
/// `#[repr(C)]` with [`KObjectHeader`] first — see
/// [`crate::object::header`].
#[repr(C)]
pub struct Thread {
    header: KObjectHeader,
    tid: u32,
    owner_pid: u32,
}

impl Thread {
    /// Allocate a thread object on the kernel heap with a refcount of one
    /// (owned by the caller, who transfers it to a handle via
    /// `KBox::into_raw` + `HandleTable::allocate`).
    pub fn try_new(tid: u32, owner_pid: u32) -> Result<KBox<Self>, AllocError> {
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::Thread),
            tid,
            owner_pid,
        })
    }

    /// The thread identifier.
    pub fn tid(&self) -> u32 {
        self.tid
    }

    /// The id of the process this thread belongs to.
    pub fn owner_pid(&self) -> u32 {
        self.owner_pid
    }
}
