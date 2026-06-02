//! Architecture-neutral syscall dispatch.
//!
//! The arch layer ([`crate::arch::x86_64::syscall`]) owns the privilege
//! transition (the `syscall` entry stub, `swapgs`, the per-CPU kernel
//! stack) and builds a [`SyscallFrame`] on the kernel stack, then calls
//! [`syscall_dispatch`]. This module turns that frame into a numbered call
//! and routes it through [`table::dispatch`]. See `docs/spec/syscall-abi.md`.

pub mod error;
pub mod table;

pub use error::{KError, SysResult};

/// The register snapshot the arch syscall entry stub builds on the kernel
/// stack and hands to [`syscall_dispatch`].
///
/// `#[repr(C)]`; the field order is **lowest address first** and must
/// mirror the stub's push order exactly (the `r15` field is what the stub
/// pushes last, so it lies at the lowest address — where RSP points when
/// the dispatcher is called). The `offset_of!` assertions below pin the
/// layout the stub's `mov [rsp + …]` depends on.
///
/// `rcx` holds the user RIP and `r11` the user RFLAGS — the values
/// `sysretq` consumes — so the stub saves and restores them across the
/// dispatch call even though no handler reads them.
#[repr(C)]
pub struct SyscallFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub r11: u64, // user RFLAGS
    pub r10: u64, // arg4
    pub r9: u64,  // arg6
    pub r8: u64,  // arg5
    pub rdx: u64, // arg3
    pub rsi: u64, // arg2
    pub rdi: u64, // arg1
    pub rcx: u64, // user RIP
    pub rax: u64, // syscall number in / return value out
    pub user_rsp: u64,
}

const _: () = assert!(core::mem::size_of::<SyscallFrame>() == 16 * 8);
const _: () = assert!(core::mem::offset_of!(SyscallFrame, r15) == 0);
const _: () = assert!(core::mem::offset_of!(SyscallFrame, r11) == 6 * 8);
const _: () = assert!(core::mem::offset_of!(SyscallFrame, r10) == 7 * 8);
const _: () = assert!(core::mem::offset_of!(SyscallFrame, rdi) == 12 * 8);
const _: () = assert!(core::mem::offset_of!(SyscallFrame, rcx) == 13 * 8);
const _: () = assert!(core::mem::offset_of!(SyscallFrame, rax) == 14 * 8);
const _: () = assert!(core::mem::offset_of!(SyscallFrame, user_rsp) == 15 * 8);

/// Entry point the arch syscall stub `call`s with `rdi = &mut frame`.
/// Returns the `isize` result in RAX; the stub stores it back into the
/// frame's `rax` slot before `sysretq`.
///
/// The `&mut SyscallFrame` is the seam for reaching per-thread/per-process
/// context: a future handler will fetch the current thread (and its
/// process's handle table / syscaps) here before dispatching. `sys_kprint`
/// needs none of that, so this slice only reads the argument fields.
///
/// # Safety
/// `frame` must point at a fully-initialised [`SyscallFrame`] on the
/// current kernel stack — exactly what the entry stub builds.
pub unsafe extern "C" fn syscall_dispatch(frame: *mut SyscallFrame) -> isize {
    // SAFETY: the stub built a complete `SyscallFrame` at the kernel stack
    // top and passed its address; it is valid, aligned, and unaliased for
    // the duration of this call.
    let f = unsafe { &mut *frame };
    table::dispatch(f.rax, f.rdi, f.rsi, f.rdx, f.r10, f.r8, f.r9)
}
