//! Kernel-internal data structures and primitives.
//!
//! This module is the kernel's own `libkern`: it holds in-kernel
//! synchronisation primitives, intrusive containers, and small utilities
//! that the rest of the kernel builds on. It is distinct from
//! `userspace/libkern/`, which is the raw syscall layer for user-mode
//! code. The two share a name because the kernel CLAUDE.md describes a
//! single "kernel/src/libkern/ or equivalent" home for hand-rolled
//! primitives; consult `docs/architecture/memory-management.md` for how
//! this module fits with the buddy and slab allocators.

pub mod spinlock;

pub use spinlock::{SpinLock, SpinLockGuard};
