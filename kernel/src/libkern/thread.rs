//! The [`ThreadArgs`] thread-creation ABI block.
//!
//! `ThreadArgs` crosses the kernel/userspace boundary: `sys_thread_create` reads
//! one. Its layout is a kernel-ABI-hash input (like
//! [`SpawnArgs`](crate::libkern::SpawnArgs) / [`IpcMsg`](crate::libkern::IpcMsg));
//! the hash is not yet computed in code, so the compile-time asserts pin it.
//!
//! `ThreadArgs` is arch-**neutral** (its `entry`/`user_sp` are just user VAs and
//! `arg0` is opaque), so it lives here. The faulted-register snapshot a
//! supervisor reads back — `RegisterValues` — *is* arch-specific (the register
//! set differs per architecture), so it lives behind the arch boundary as
//! `crate::arch::RegisterValues` (see `crate::arch::registers::ArchRegisters`),
//! not here.

/// The argument block `sys_thread_create` reads from userspace to start a new
/// thread in the calling process.
///
/// The new thread begins executing at `entry` (ring 3) with `rsp = user_sp` and
/// `rdx = arg0` (the Phase-1 register bootstrap). The caller owns the user stack:
/// it allocates + maps a region (e.g. via `sys_memory_create` + `sys_memory_map`)
/// and passes the **top** (stacks grow down) as `user_sp`. Both `entry` and
/// `user_sp` must be canonical user addresses already mapped (X / W) in the
/// calling process's address space.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ThreadArgs {
    /// Ring-3 entry point VA (offset 0).
    pub entry: u64,
    /// Initial user stack pointer VA — the stack top (offset 8).
    pub user_sp: u64,
    /// Opaque bootstrap word, delivered to the thread in `rdx` (offset 16).
    pub arg0: u64,
    /// Reserved; must be zero (offset 24).
    pub _reserved: [u8; 40],
}

const _: () = assert!(core::mem::size_of::<ThreadArgs>() == 64);
const _: () = assert!(core::mem::align_of::<ThreadArgs>() == 8);
const _: () = assert!(core::mem::offset_of!(ThreadArgs, entry) == 0);
const _: () = assert!(core::mem::offset_of!(ThreadArgs, user_sp) == 8);
const _: () = assert!(core::mem::offset_of!(ThreadArgs, arg0) == 16);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_args_layout_is_stable() {
        assert_eq!(core::mem::size_of::<ThreadArgs>(), 64);
        assert_eq!(core::mem::offset_of!(ThreadArgs, entry), 0);
        assert_eq!(core::mem::offset_of!(ThreadArgs, user_sp), 8);
        assert_eq!(core::mem::offset_of!(ThreadArgs, arg0), 16);
    }
}
