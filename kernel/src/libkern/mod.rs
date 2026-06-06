//! Kernel-internal data structures and primitives.
//!
//! This module is the kernel's own `libkern`: in-kernel synchronisation
//! primitives, heap-backed containers, and small utilities the rest of
//! the kernel builds on. It is distinct from `userspace/libkern/`, which
//! is the raw syscall layer for user-mode code. The two share a name
//! because the kernel CLAUDE.md describes a single "kernel/src/libkern/
//! or equivalent" home for hand-rolled primitives; consult
//! `docs/architecture/memory-management.md` for how this module fits
//! with the buddy and slab allocators.
//!
//! ## Fallible allocation
//!
//! The kernel registers no `#[global_allocator]` and does not use the
//! `alloc` crate: every type in `alloc` aborts on allocation failure,
//! which a kernel cannot tolerate. Instead [`KBox`], [`KVec`], and
//! [`KString`] call the slab allocator directly and report exhaustion as
//! [`AllocError`]. See the decision log entry of 2026-05-20.

pub mod handle;
pub mod kbox;
pub mod kstring;
pub mod kvec;
pub mod memory;
pub mod spinlock;

pub use handle::{KObjectType, RawHandle, Rights};
pub use kbox::KBox;
pub use memory::MemFlags;
pub use kstring::KString;
pub use kvec::KVec;
pub use spinlock::{SpinLock, SpinLockGuard};

/// Error returned by the fallible `libkern` allocators ([`KBox`],
/// [`KVec`], [`KString`]) when the kernel heap cannot satisfy a request.
///
/// Allocation failure in the kernel is a recoverable condition that must
/// propagate as a `Result` — never a panic. That requirement is the
/// reason the kernel forgoes the `alloc` crate entirely; see the
/// decision log entry of 2026-05-20.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocError;
