//! The [`Process`] kernel object.
//!
//! Minimal this slice: a [`KObjectHeader`] plus the process identifier.
//! A process is conceptually an address space, a namespace handle, a
//! current-working-directory handle, a list of owned handles, a syscap
//! bitmask, and a set of threads (see `docs/architecture/overview.md`),
//! but those fields arrive with the process-management and threading
//! slices — see `docs/planning/implementation-plan.md`. Keeping the type
//! minimal here avoids referencing subsystems that do not yet exist.

use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox};
use crate::object::header::KObjectHeader;

/// A process kernel object.
///
/// `#[repr(C)]` with [`KObjectHeader`] first so the type-erased object
/// pointer in a handle entry can be read as `*const KObjectHeader` at
/// offset 0 — see [`crate::object::header`].
#[repr(C)]
pub struct Process {
    header: KObjectHeader,
    pid: u32,
    /// Self-check sentinel. A live `Process` always reads
    /// [`Process::MAGIC`] here; a use-after-free reads freed or reused
    /// memory. Used by the concurrency torture tests; cheap enough to
    /// keep unconditionally as a defensive tripwire.
    magic: u64,
}

impl Process {
    /// Sentinel written into [`Process::magic`] at construction.
    pub const MAGIC: u64 = 0x5072_6f63_4f62_6a21; // "ProcObj!"

    /// Allocate a process object on the kernel heap with a refcount of
    /// one (owned by the caller, who transfers it to a handle via
    /// `KBox::into_raw` + `HandleTable::allocate`).
    pub fn try_new(pid: u32) -> Result<KBox<Self>, AllocError> {
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::Process),
            pid,
            magic: Self::MAGIC,
        })
    }

    /// The process identifier.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }
}
