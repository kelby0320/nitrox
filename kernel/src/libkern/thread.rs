//! The [`ThreadArgs`] thread-creation ABI block.
//!
//! `ThreadArgs` crosses the kernel/userspace boundary: `sys_thread_create` reads
//! one. It is **syscall-ABI** — passed by `UserPtr` to a syscall, self-pinned by the
//! compile-time `size_of`/`offset_of` asserts below and `docs/spec/thread-args.md` —
//! not a Tier-2 module-boundary layout, so it is *not* in the module ABI-version hash
//! (`docs/spec/abi-version-hash.md`). Growing it (as the SysCaps slice did, filling
//! the reserved block with the scheduling fields) is a pre-v1 syscall-ABI change.
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
    /// Scheduling class: [`THREAD_CLASS_TIMESHARED`] (`0`, the default so a zeroed
    /// block means TimeShared) or [`THREAD_CLASS_REALTIME`] (`1`). The RealTime class
    /// requires the `REAL_TIME` syscap (offset 24).
    pub class: u8,
    /// RealTime fixed priority (`0..=99`); ignored for TimeShared (offset 25).
    pub rt_priority: u8,
    /// TimeShared `nice` (`-20..=19`); ignored for RealTime. Ungated (offset 26).
    pub nice: i8,
    /// CPU affinity mask (bit `c` ⇒ may run on CPU `c`); `0` ⇒ no restriction.
    /// Ungated (offset 27).
    pub cpu_affinity: u8,
    /// Reserved; must be zero (offset 28).
    pub _reserved: [u8; 36],
}

/// `ThreadArgs::class` — the default cooperative/fair class (a zeroed block).
pub const THREAD_CLASS_TIMESHARED: u8 = 0;
/// `ThreadArgs::class` — fixed-priority real-time; requires the `REAL_TIME` syscap.
pub const THREAD_CLASS_REALTIME: u8 = 1;

const _: () = assert!(core::mem::size_of::<ThreadArgs>() == 64);
const _: () = assert!(core::mem::align_of::<ThreadArgs>() == 8);
const _: () = assert!(core::mem::offset_of!(ThreadArgs, entry) == 0);
const _: () = assert!(core::mem::offset_of!(ThreadArgs, user_sp) == 8);
const _: () = assert!(core::mem::offset_of!(ThreadArgs, arg0) == 16);
const _: () = assert!(core::mem::offset_of!(ThreadArgs, class) == 24);
const _: () = assert!(core::mem::offset_of!(ThreadArgs, rt_priority) == 25);
const _: () = assert!(core::mem::offset_of!(ThreadArgs, nice) == 26);
const _: () = assert!(core::mem::offset_of!(ThreadArgs, cpu_affinity) == 27);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_args_layout_is_stable() {
        assert_eq!(core::mem::size_of::<ThreadArgs>(), 64);
        assert_eq!(core::mem::offset_of!(ThreadArgs, entry), 0);
        assert_eq!(core::mem::offset_of!(ThreadArgs, user_sp), 8);
        assert_eq!(core::mem::offset_of!(ThreadArgs, arg0), 16);
        assert_eq!(core::mem::offset_of!(ThreadArgs, class), 24);
        assert_eq!(core::mem::offset_of!(ThreadArgs, rt_priority), 25);
        assert_eq!(core::mem::offset_of!(ThreadArgs, nice), 26);
        assert_eq!(core::mem::offset_of!(ThreadArgs, cpu_affinity), 27);
    }
}
