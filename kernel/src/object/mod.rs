//! Kernel object substrate.
//!
//! A *kernel object* is anything a handle can refer to: it lives in
//! kernel memory, is reference-counted with a kernel-managed lifetime,
//! and has a type with associated operations (see
//! `docs/architecture/overview.md` § "Kernel objects"). The handle table
//! in [`crate::handle`] is the capability lookup layer that maps handles
//! to the type-erased pointers this module's objects live behind; the
//! two are deliberately separate modules (decision log, 2026-05-28).
//!
//! Every object begins with a [`KObjectHeader`] (refcount + type tag) as
//! its first `#[repr(C)]` field, and type-specific operations dispatch
//! through a `match` on [`KObjectType`](crate::libkern::handle::KObjectType)
//! rather than `dyn` (per `kernel/CLAUDE.md`). [`ObjectRef`] is the RAII
//! refcount holder that `HandleTable::lookup` hands back; dropping it
//! releases the reference and, on the last one, runs the concrete
//! destructor.
//!
//! This slice implements the substrate plus the first two concrete
//! objects, [`Process`] and [`Thread`]. The remaining object types land
//! behind their respective Phase 1 slices.

pub mod header;
pub mod process;
pub mod thread;

pub use header::{KObjectHeader, ObjectRef};
pub use process::Process;
pub use thread::Thread;
