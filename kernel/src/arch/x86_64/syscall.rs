//! x86_64 `syscall`/`sysretq` fast path and a throwaway ring-3 bootstrap.
//!
//! `syscall` (unlike an interrupt) does not switch RSP, so the entry stub
//! obtains the kernel stack itself: it `swapgs` to a per-CPU block
//! ([`CpuLocal`], reachable via `IA32_KERNEL_GS_BASE`), stashes the user
//! RSP there, and loads the kernel syscall stack. It builds a
//! [`SyscallFrame`](crate::syscall::SyscallFrame) and calls the
//! architecture-neutral dispatcher, then restores and `sysretq`s. See
//! `docs/spec/syscall-abi.md` and the decision log (2026-05-29).
//!
//! ## GS model (Phase 1, single CPU)
//!
//! Ring 0 normally runs with `GS_BASE = 0` and `KERNEL_GS_BASE = &CPU0`.
//! The only `gs:`-relative code is the entry stub, bracketed by a `swapgs`
//! on entry and a matching `swapgs` before `sysretq` (or a manual one on
//! the debug-exit path). Nothing else in the kernel uses `gs:`.
//!
//! ## Throwaway ring-3 harness
//!
//! [`enter_user`] + [`syscall_debug_exit`] form a one-shot
//! enter-ring-3 / return-to-kernel scaffold used to exercise the syscall
//! path before a real process exists. It is replaced next slice by the
//! ELF-loaded `Process` + scheduler-driven user thread.

use core::arch::naked_asm;
use core::mem::offset_of;

use super::{gdt, regs};
use crate::syscall::table;

// --- MSRs -----------------------------------------------------------------

const MSR_EFER: u32 = 0xC000_0080;
const EFER_SCE: u64 = 1 << 0; // syscall enable
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_SFMASK: u32 = 0xC000_0084;
const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;

const RFLAGS_IF: u64 = 1 << 9;
const RFLAGS_DF: u64 = 1 << 10;
const RFLAGS_AC: u64 = 1 << 18;
/// RFLAGS bits cleared by the CPU on `syscall` entry: interrupts (already
/// masked in Phase 1, kept clear), direction (so `rep`/string ops in the
/// dispatch path are well-defined), and SMAP AC (closes any stray access
/// window; the copy primitives reopen it with `stac`).
const SFMASK_VALUE: u64 = RFLAGS_IF | RFLAGS_DF | RFLAGS_AC;

// --- Per-CPU block --------------------------------------------------------

/// Per-CPU kernel data reached via `IA32_KERNEL_GS_BASE` + `swapgs`. Single
/// instance for Phase 1's single CPU; becomes one-per-CPU under SMP.
#[repr(C)]
pub struct CpuLocal {
    /// `gs:[0]` — scratch the entry stub stashes the user RSP into.
    rsp_scratch: u64,
    /// `gs:[8]` — top of the kernel syscall stack, loaded into RSP on entry.
    kstack_top: u64,
    /// `gs:[16]` — kernel resume RSP for the [`enter_user`] round trip.
    resume_rsp: u64,
}

static mut CPU0: CpuLocal = CpuLocal {
    rsp_scratch: 0,
    kstack_top: 0,
    resume_rsp: 0,
};

const OFF_SCRATCH: usize = offset_of!(CpuLocal, rsp_scratch);
const OFF_KSTACK: usize = offset_of!(CpuLocal, kstack_top);
const OFF_RESUME: usize = offset_of!(CpuLocal, resume_rsp);
const _: () = assert!(OFF_SCRATCH == 0 && OFF_KSTACK == 8 && OFF_RESUME == 16);

// --- Initialisation -------------------------------------------------------

/// Program the syscall MSRs and the per-CPU syscall stack. Must run after
/// `gdt::init` (STAR's selectors must already be in the loaded GDT before
/// the first `syscall`) and after paging (EFER fully formed), and before
/// any ring-3 entry. `kstack_top` is the (16-aligned) top of the kernel
/// stack the entry stub switches to.
pub fn init(kstack_top: u64) {
    // SAFETY: single-CPU boot; `CPU0` is exclusively owned with no
    // outstanding reference. `kstack_top` is written before it can be read
    // by any `syscall`.
    unsafe {
        (&raw mut (*(&raw mut CPU0)).kstack_top).write(kstack_top);
    }
    let cpu0_addr = &raw const CPU0 as u64;
    let star = gdt::STAR_VALUE;
    let lstar = syscall_entry as *const () as u64;
    // SAFETY: all four MSRs are architectural on every long-mode CPU. We
    // OR `SCE` into EFER without disturbing LME/LMA/NXE. STAR encodes the
    // GDT selectors loaded by `gdt::init`; LSTAR is the entry stub.
    unsafe {
        regs::wrmsr(MSR_KERNEL_GS_BASE, cpu0_addr);
        regs::wrmsr(MSR_STAR, star);
        regs::wrmsr(MSR_LSTAR, lstar);
        regs::wrmsr(MSR_SFMASK, SFMASK_VALUE);
        let efer = regs::rdmsr(MSR_EFER);
        regs::wrmsr(MSR_EFER, efer | EFER_SCE);
    }
}

// --- Register frame + dispatch --------------------------------------------

/// The x86_64 register snapshot the entry stub builds on the kernel stack
/// and hands to [`syscall_dispatch`].
///
/// `#[repr(C)]`; the field order is **lowest address first** and must mirror
/// the stub's push order exactly (the `r15` field is what the stub pushes
/// last, so it lies at the lowest address — where RSP points when the
/// dispatcher is called). The `offset_of!` assertions pin the layout the
/// stub's `mov [rsp + …]` depends on.
///
/// `rcx` holds the user RIP and `r11` the user RFLAGS — the values `sysretq`
/// consumes — so the stub saves and restores them across the dispatch call
/// even though no handler reads them. This is x86-specific, so it lives in
/// the arch layer; the neutral syscall layer only sees `(number, args) ->
/// isize` via [`table::dispatch`].
#[repr(C)]
struct SyscallFrame {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    rbp: u64,
    rbx: u64,
    r11: u64, // user RFLAGS
    r10: u64, // arg4
    r9: u64,  // arg6
    r8: u64,  // arg5
    rdx: u64, // arg3
    rsi: u64, // arg2
    rdi: u64, // arg1
    rcx: u64, // user RIP
    rax: u64, // syscall number in / return value out
    user_rsp: u64,
}

const _: () = assert!(core::mem::size_of::<SyscallFrame>() == 16 * 8);
const _: () = assert!(offset_of!(SyscallFrame, r15) == 0);
const _: () = assert!(offset_of!(SyscallFrame, rax) == 14 * 8);
const _: () = assert!(offset_of!(SyscallFrame, user_rsp) == 15 * 8);

/// Reached from [`syscall_entry`] via `call` with `rdi = &mut frame`. Unpacks
/// the x86 register frame into the neutral `(number, args)` form and routes
/// it through [`table::dispatch`]; the returned `isize` goes back in RAX and
/// the stub stores it into the frame's `rax` slot before `sysretq`.
///
/// # Safety
/// `frame` must point at a fully-initialised [`SyscallFrame`] on the current
/// kernel stack — exactly what the entry stub builds.
unsafe extern "C" fn syscall_dispatch(frame: *mut SyscallFrame) -> isize {
    // SAFETY: the stub built a complete frame at the kernel stack top and
    // passed its address; valid, aligned, and unaliased for this call.
    let f = unsafe { &mut *frame };
    table::dispatch(f.rax, f.rdi, f.rsi, f.rdx, f.r10, f.r8, f.r9)
}

// --- Entry stub -----------------------------------------------------------

/// The `syscall` entry point (the `IA32_LSTAR` target). Builds a
/// [`SyscallFrame`] and calls [`syscall_dispatch`], then `sysretq`s.
#[unsafe(naked)]
extern "C" fn syscall_entry() -> ! {
    // SAFETY: naked — every register/stack effect is explicit. On entry
    // `syscall` has put the user RIP in RCX and user RFLAGS in R11 (both
    // preserved across the dispatch for `sysretq`); RAX holds the number,
    // RDI/RSI/RDX/R10/R8/R9 the args. `swapgs` makes `gs:` reach CPU0.
    // The push order builds `SyscallFrame` (lowest field = last pushed).
    naked_asm!(
        "swapgs",
        "mov gs:[{scratch}], rsp",  // stash user RSP
        "mov rsp, gs:[{kstack}]",   // switch to the kernel syscall stack
        // Build SyscallFrame: highest field (user_rsp) pushed first.
        "push gs:[{scratch}]",      // user_rsp
        "push rax",                 // rax (number / return)
        "push rcx",                 // rcx = user RIP
        "push rdi",
        "push rsi",
        "push rdx",
        "push r8",
        "push r9",
        "push r10",
        "push r11",                 // r11 = user RFLAGS
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov rdi, rsp",             // &mut SyscallFrame
        "call {dispatch}",
        "mov [rsp + {rax_off}], rax", // store return value into the frame
        // Restore (mirror of the pushes).
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "pop r11",                  // user RFLAGS for sysretq
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop rcx",                  // user RIP for sysretq
        "pop rax",                  // return value
        "pop rsp",                  // restore user RSP
        // Zero caller-saved scratch we don't deliberately return, so no
        // kernel value leaks to ring 3. RAX (return), RCX (RIP), R11
        // (RFLAGS) are intended; RBX/RBP/R12-R15 hold the user's values.
        "xor edx, edx",
        "xor esi, esi",
        "xor edi, edi",
        "xor r8d, r8d",
        "xor r9d, r9d",
        "xor r10d, r10d",
        "swapgs",
        "sysretq",
        scratch  = const OFF_SCRATCH,
        kstack   = const OFF_KSTACK,
        rax_off  = const offset_of!(SyscallFrame, rax),
        dispatch = sym syscall_dispatch,
    );
}

// --- Throwaway ring-3 bootstrap -------------------------------------------

/// Enter ring 3 at `entry` with user stack `user_sp`, in the address space
/// rooted at `cr3_phys`. Returns (with the user's exit status in the low 32
/// bits) when the ring-3 code invokes the `SYS_DEBUG_EXIT` syscall, which
/// routes to [`syscall_debug_exit`].
///
/// THROWAWAY test scaffold — replaced next slice by the ELF `Process` +
/// scheduler-driven user thread.
///
/// # Safety
/// `cr3_phys` must be a fully-formed PML4 (with the kernel half inherited,
/// so the current kernel stack stays mapped); `entry`/`user_sp` must be
/// canonical user addresses mapped executable / writable respectively;
/// [`init`] must have run.
#[unsafe(naked)]
pub unsafe extern "C" fn enter_user(entry: u64, user_sp: u64, cr3_phys: u64) -> i32 {
    // SAFETY: naked. SysV: rdi=entry, rsi=user_sp, rdx=cr3_phys. We save
    // callee-saved registers and the kernel resume RSP into CPU0 (by
    // absolute address — GS_BASE is 0 in ring 0 here), switch CR3, build an
    // `iretq` frame, and drop to ring 3. `syscall_debug_exit` restores the
    // resume RSP and returns into this function's caller.
    naked_asm!(
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "lea rax, [rip + {cpu0}]",
        "mov [rax + {resume}], rsp", // CPU0.resume_rsp = kernel RSP
        "mov cr3, rdx",              // switch to the user address space
        // Build the iretq frame (CPU pops RIP, CS, RFLAGS, RSP, SS).
        "push {user_ss}",           // SS
        "push rsi",                 // RSP = user_sp
        "push {rflags}",            // RFLAGS (IF=0; bit1 reserved=1)
        "push {user_cs}",           // CS
        "push rdi",                 // RIP = entry
        "iretq",
        cpu0    = sym CPU0,
        resume  = const OFF_RESUME,
        user_ss = const gdt::USER_DATA_SELECTOR as u64,
        user_cs = const gdt::USER_CODE_SELECTOR as u64,
        rflags  = const 0x2u64,
    );
}

/// Handle `SYS_DEBUG_EXIT`: leave ring 3 and return into [`enter_user`]'s
/// caller, carrying `status`. Reached from inside [`syscall_entry`] (so
/// `GS_BASE = &CPU0`); we abandon the `sysretq` path, so we `swapgs` by
/// hand to restore the kernel-normal GS before resuming. Never returns to
/// the dispatcher. THROWAWAY.
#[unsafe(naked)]
pub extern "C" fn syscall_debug_exit(status: i32) -> ! {
    // SAFETY: naked. `enter_user` stashed a valid resume RSP in CPU0; we
    // are on the syscall kernel stack inside the dispatch call chain. We
    // switch back to `enter_user`'s stack, rebalance GS, restore its
    // callee-saved registers, and `ret` to its caller with `status` in EAX.
    naked_asm!(
        "mov eax, edi",            // return status
        "mov rsp, gs:[{resume}]",  // restore enter_user's kernel RSP
        "swapgs",                  // back to GS_BASE = 0 (kernel-normal)
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
        resume = const OFF_RESUME,
    );
}
