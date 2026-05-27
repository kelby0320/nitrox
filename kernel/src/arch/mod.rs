//! Architecture abstraction. Phase 0 implements only x86_64.
//!
//! When aarch64 is brought up, this module re-exports the active
//! architecture's primitives under a stable interface (see
//! `docs/architecture/overview.md`).
//!
//! [`paging`] is architecture-neutral — it holds the [`ArchPaging`]
//! trait and its supporting types. The active architecture's
//! implementation is re-exported here as [`Paging`].
//!
//! [`ArchPaging`]: paging::ArchPaging

pub mod paging;

#[cfg(target_arch = "x86_64")]
pub mod x86_64;

#[cfg(target_arch = "x86_64")]
pub use x86_64::{abi, gdt, halt_loop, idt, serial, user_access};

#[cfg(target_arch = "x86_64")]
pub use x86_64::paging::{
    X86Paging as Paging, active_root, init_kernel_template, init_protections, translate,
};
