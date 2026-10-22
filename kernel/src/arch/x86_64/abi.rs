//! x86_64 user-space ABI constants, read through `crate::arch::abi`.

use crate::mm::PAGE_SIZE;

/// ELF `e_machine` value for this architecture (`EM_X86_64 = 62`).
pub const E_MACHINE: u16 = 62;

/// Exclusive upper bound of the user half on 4-level paging: the first non-canonical address.
/// A VMA reaching or crossing it is in the canonical hole or the kernel half and must be rejected.
pub const USER_VIRT_END: u64 = 0x0000_8000_0000_0000;

/// Exclusive upper bound of what a user mapping may cover: the top page of the user half stays
/// unmapped, as Linux keeps it. A `syscall` in its last bytes would return to `USER_VIRT_END`,
/// non-canonical, and Intel's `sysretq` faults on that in ring 0 with the user stack loaded.
pub const USER_MAP_END: u64 = USER_VIRT_END - PAGE_SIZE as u64;

/// Default initial-stack top for a freshly-loaded process: page-aligned, canonical, below
/// [`USER_VIRT_END`].
pub const DEFAULT_USER_STACK_TOP: u64 = 0x0000_7FFF_FFFF_0000;

/// Default initial user stack size: **8 MiB**, matching Linux's `RLIMIT_STACK` default.
///
/// It costs address space only: the stack is mapped lazily (`mm::elf` uses `map_vma_lazy`), so
/// pages materialise on first touch. It is the initial thread's only; `sys_thread_create`
/// takes a caller-allocated stack.
pub const DEFAULT_USER_STACK_SIZE: u64 = 8 * 1024 * 1024;

/// Unmapped gap kept below the stack so an overrun **faults instead of landing in mapped
/// memory**, 1 MiB.
///
/// 256 pages rather than one because a large stack frame can step over a single guard page
/// into the mapping below (Stack Clash, CVE-2017-1000364). Nothing is mapped in the gap.
pub const USER_STACK_GUARD_SIZE: u64 = 256 * (PAGE_SIZE as u64);
