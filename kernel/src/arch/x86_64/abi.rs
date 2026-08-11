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

/// Default initial user stack size: **8 MiB**, matching Linux's `RLIMIT_STACK` default.
///
/// **The size costs address space and nothing else**, because the stack is mapped lazily
/// (`mm::elf` uses `map_vma_lazy`, and a test there asserts it is not eagerly backed). Pages
/// materialise on first touch, so a process that uses 12 KiB of this backs three pages. That
/// is why it can be generous: 8 MiB out of a 128 TiB user half, once per process.
///
/// Once per process, not per thread — `sys_thread_create` requires the caller to allocate
/// and map its own stack and pass the top, so this is the initial thread's only.
///
/// It was 32 KiB (8 pages, itself raised from 4 for the read-write fs-server's nested block
/// buffers), which is small enough that a single ~9 KiB struct held by value could run a
/// client off the end of it — `libsurface` documents exactly that, presenting as "a process
/// that dies in its prologue and prints nothing at all". See
/// `docs/rationale/deferred-decisions.md`.
pub const DEFAULT_USER_STACK_SIZE: u64 = 8 * 1024 * 1024;

/// Unmapped gap kept below the stack so an overrun **faults instead of landing in mapped
/// memory**, 1 MiB.
///
/// This is the part that matters more than the size. Without it the mmap window ends at
/// exactly the stack's lowest address, so running off the bottom of the stack writes into
/// whatever was mapped there — silently, which is the property that makes a stack overrun
/// expensive to diagnose rather than merely a crash.
///
/// 256 pages rather than one because Linux ran the experiment: its guard was a single page
/// until Stack Clash (CVE-2017-1000364) demonstrated that a large stack frame can step over
/// one page into the mapping below, and the default has been 256 pages since. A gap costs
/// address space only — nothing is mapped in it by construction.
pub const USER_STACK_GUARD_SIZE: u64 = 256 * (PAGE_SIZE as u64);
