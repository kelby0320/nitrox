//! Architecture abstraction. Phase 0 implements only x86_64.
//!
//! When aarch64 is brought up, this module re-exports the active
//! architecture's primitives under a stable interface (see
//! `docs/architecture/overview.md`).

#[cfg(target_arch = "x86_64")]
pub mod x86_64;

#[cfg(target_arch = "x86_64")]
pub use x86_64::halt_loop;
