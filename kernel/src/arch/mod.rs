//! Architecture abstraction. Phase 0 implements only x86_64.
//!
//! ## The arch boundary (read before adding to this module)
//!
//! This module is the kernel's **only** architecture-neutral interface to
//! CPU- and platform-specific machinery. The architecture implementation
//! lives under a **private** submodule (`x86_64`, and eventually
//! `aarch64`); the rest of the kernel — everything outside
//! `kernel/src/arch/` — must reach it solely through the neutral names
//! re-exported here. Because the arch submodule is private,
//! `crate::arch::x86_64::…` does not compile outside `arch/`; the
//! `cargo xtask check-arch` lint additionally rejects any such reference
//! in comments or new code. See `docs/conventions/arch-boundary.md`.
//!
//! To expose a new arch operation: add a neutral function (or re-export a
//! neutral-named item) here, backed by an implementation in the active
//! architecture's private submodule. Do not surface x86 jargon
//! (`gdt`, `idt`, `cr3`, `rsp`, MSR names, …) in the names re-exported
//! here — wrap it in a neutral name (e.g. `set_kernel_stack`,
//! `init_syscalls`).
//!
//! [`paging`] is the one architecture-neutral *trait* module — it holds
//! the [`ArchPaging`](paging::ArchPaging) trait and its supporting types;
//! the active architecture's implementation is re-exported here as
//! [`Paging`].

pub mod paging;

#[cfg(target_arch = "x86_64")]
mod x86_64;

// Neutral modules and free functions (defined at the x86_64 root). The
// `set_kernel_stack` gives a neutral name to the GDT/TSS RSP0 setter.
#[cfg(target_arch = "x86_64")]
pub use x86_64::{
    abi, halt_loop, init_cpu_tables, serial, set_kernel_stack, user_access,
};

#[cfg(target_arch = "x86_64")]
pub use x86_64::context::{ArchThreadContext, context_switch, fabricate_frame, thread_trampoline};

// Syscall fast-path: arm it once at boot, set the per-thread kernel stack on
// switch-in, and descend to ring 3 from a scheduled user thread.
#[cfg(target_arch = "x86_64")]
pub use x86_64::syscall::{enter_user, init_syscall_entry, set_syscall_kernel_stack};

#[cfg(target_arch = "x86_64")]
pub use x86_64::paging::{
    X86Paging as Paging, active_root, init_kernel_template, init_protections, translate,
};
