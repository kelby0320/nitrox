//! x86_64 user-space ABI constants.
//!
//! The handful of arch-specific values that arch-neutral code (the
//! VMM, the ELF loader) needs to know in order to talk about user
//! space without baking x86_64 assumptions into itself. Re-exported
//! from [`crate::arch::abi`] under whichever architecture this build
//! targets; consumers should read them through that path rather than
//! reaching directly into the `x86_64` module.

use crate::mm::PAGE_SIZE;

/// ELF `e_machine` value for this architecture. Used by the ELF
/// loader to reject binaries built for a different machine.
///
/// `EM_X86_64 = 62` per the ELF specification.
pub const E_MACHINE: u16 = 62;

/// Exclusive upper bound of the user half on 4-level paging: the
/// first non-canonical address past the user half. Any VMA whose
/// range reaches or crosses this is in the canonical hole or the
/// kernel half and must be rejected at the user-facing layer.
pub const USER_VIRT_END: u64 = 0x0000_8000_0000_0000;

/// Top-of-user-space address chosen as the default initial-stack
/// top for a freshly-loaded process. Page-aligned, canonical, and
/// well below [`USER_VIRT_END`].
pub const DEFAULT_USER_STACK_TOP: u64 = 0x0000_7FFF_FFFF_0000;

/// Default initial user stack size (8 pages = 32 KiB). Bumped from 4 pages for the
/// read-write fs-server, whose metadata mutation legitimately nests several 4 KiB block
/// buffers (bitmap, superblock, directory, extent scratch) on the stack. A process can
/// grow it later via an explicit `sys_memory_map`; today this is the only stack it gets.
pub const DEFAULT_USER_STACK_SIZE: u64 = 8 * (PAGE_SIZE as u64);
