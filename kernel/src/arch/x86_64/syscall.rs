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
//! ## GS model
//!
//! On each CPU, `KERNEL_GS_BASE` is held at **that CPU's [`CpuLocal`] block** (its
//! [`CPUS`] slot) **at all times** — in userspace, in the kernel body, and while a
//! thread is blocked mid-syscall — and `GS_BASE = 0` (the userspace value)
//! everywhere except the brief entry-stub window. The entry stub `swapgs`es in (so
//! `gs:` reaches this CPU's block), grabs the kernel stack, then `swapgs`es **back**
//! before running the body; there is no `swapgs` before `sysretq`. The only
//! `gs:`-relative code is that stub.
//!
//! This always-points-at-this-CPU's-block invariant is what makes the entry robust
//! across multiple threads on a CPU: a thread that blocks mid-body (e.g. `sys_wait`)
//! is switched away with `KERNEL_GS_BASE` still at this CPU's block, so a *sibling's*
//! `syscall` `swapgs` still lands `GS_BASE` on it rather than the parked user GS. (An
//! earlier model held `GS_BASE` at the block across the body and parked the user GS
//! in `KERNEL_GS_BASE`; a blocked thread then left `KERNEL_GS_BASE = 0`, and a
//! sibling's entry `swapgs` faulted on `gs:[0]` — an intermittent `#DF`. Fixed
//! 2026-06-23; see the decision log.) The IDT trap entries never touch `gs:` and
//! never `swapgs`, so they are consistent with `GS_BASE = 0` in both rings.
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

/// Per-CPU kernel data reached via `IA32_KERNEL_GS_BASE` + `swapgs`. One per CPU
/// (the [`CPUS`] array); each CPU's `KERNEL_GS_BASE` points at its own slot.
#[repr(C)]
pub struct CpuLocal {
    /// `gs:[0]` — scratch the entry stub stashes the user RSP into.
    rsp_scratch: u64,
    /// `gs:[8]` — top of the kernel syscall stack, loaded into RSP on entry.
    /// Set per-thread by [`set_syscall_kernel_stack`] when a user thread is
    /// about to run, so syscalls from ring 3 land on that thread's stack.
    kstack_top: u64,
}

/// Zeroed initial block. Named so the `[CONST; N]` array initialiser below works
/// without requiring `CpuLocal: Copy`.
const CPU_LOCAL_INIT: CpuLocal = CpuLocal {
    rsp_scratch: 0,
    kstack_top: 0,
};

/// One [`CpuLocal`] per CPU, indexed by the dense `current_cpu()` id; each CPU
/// programs its `KERNEL_GS_BASE` to its own slot (see [`this_cpu_block`]). Only
/// slot 0 is live until APs start (slice 1).
static mut CPUS: [CpuLocal; crate::arch::smp::MAX_CPUS] =
    [CPU_LOCAL_INIT; crate::arch::smp::MAX_CPUS];

const OFF_SCRATCH: usize = offset_of!(CpuLocal, rsp_scratch);
const OFF_KSTACK: usize = offset_of!(CpuLocal, kstack_top);
const _: () = assert!(OFF_SCRATCH == 0 && OFF_KSTACK == 8);

/// Raw pointer to the running CPU's [`CpuLocal`] slot — its [`CPUS`] entry, indexed
/// by the dense CPU id read via `RDTSCP` (the same source as
/// [`X86Smp::current_cpu`](super::smp::X86Smp), reached one layer down to avoid a
/// trait import in the entry path). Used to program `KERNEL_GS_BASE` and to set the
/// per-thread syscall kernel stack, always on the CPU whose slot it is.
fn this_cpu_block() -> *mut CpuLocal {
    let idx = regs::rdtscp_aux() as usize;
    debug_assert!(idx < crate::arch::smp::MAX_CPUS, "cpu index out of range");
    // SAFETY: `idx < MAX_CPUS` — `current_cpu()` returns a dense id in
    // `0..cpu_count()` and `cpu_count() <= MAX_CPUS`. Pointer arithmetic within the
    // static array (no bounds-check panic in this entry path, no reference formed);
    // each CPU addresses only its own slot.
    unsafe { (&raw mut CPUS).cast::<CpuLocal>().add(idx) }
}

// --- Initialisation -------------------------------------------------------

/// Arm the architecture's syscall fast-path entry, once at boot. Must run
/// after `gdt::init` (STAR's selectors must already be in the loaded GDT
/// before the first `syscall`) and after paging (EFER fully formed), and
/// before any ring-3 entry.
///
/// The per-CPU kernel stack the entry stub switches to is **not** set here —
/// it is per-thread (see [`set_syscall_kernel_stack`]). (On x86_64 this
/// programs the `EFER.SCE`/`STAR`/`LSTAR`/`SFMASK`/`KERNEL_GS_BASE` MSRs;
/// that is the architecture's implementation detail.)
pub fn init_syscall_entry() {
    let gs_base = this_cpu_block() as u64;
    let star = gdt::STAR_VALUE;
    let lstar = syscall_entry as *const () as u64;
    // SAFETY: all four MSRs are architectural on every long-mode CPU. We
    // OR `SCE` into EFER without disturbing LME/LMA/NXE. STAR encodes the
    // GDT selectors loaded by `gdt::init`; LSTAR is the entry stub.
    unsafe {
        regs::wrmsr(MSR_KERNEL_GS_BASE, gs_base);
        regs::wrmsr(MSR_STAR, star);
        regs::wrmsr(MSR_LSTAR, lstar);
        regs::wrmsr(MSR_SFMASK, SFMASK_VALUE);
        let efer = regs::rdmsr(MSR_EFER);
        regs::wrmsr(MSR_EFER, efer | EFER_SCE);
    }
}

/// Re-assert `KERNEL_GS_BASE =` this CPU's block before a thread's **first** ring-3
/// descent.
///
/// `enter_user` does no `swapgs` — it assumes the kernel reached it in the boot
/// GS state (`GS_BASE = 0`, `KERNEL_GS_BASE =` this CPU's block). That holds only if
/// the path here never crossed a `swapgs`. But a thread that **blocks mid-syscall**
/// (e.g. `sys_wait`) is switched away with the entry `swapgs` still in effect:
/// `GS_BASE =` this CPU's block, `KERNEL_GS_BASE =` the blocked thread's (user) GS.
/// If the scheduler then descends a *different* thread to ring 3 for the first time
/// via `enter_user`, that thread's first `syscall`'s `swapgs` would load the stale
/// `KERNEL_GS_BASE` (often `0`) into `GS_BASE`, and the entry stub's `gs:` access
/// faults (→ `#PF` → `#DF`). Writing this CPU's block here makes the first
/// `syscall`'s `swapgs` correct regardless of the incoming GS state; the entry/exit
/// `swapgs` pair keeps it correct thereafter.
pub fn arm_user_entry_cpu_base() {
    let gs_base = this_cpu_block() as u64;
    // SAFETY: architectural MSR; this CPU's `CpuLocal` slot is the block the entry
    // stub reaches through `gs:` after `swapgs`.
    unsafe { regs::wrmsr(MSR_KERNEL_GS_BASE, gs_base) };

    // Guarantee this CPU's `syscall` fast-path MSRs are armed before it descends
    // to ring 3. `init_syscall_entry` normally arms them once during bring-up, but
    // under SMP a CPU could reach a user descent before its bring-up
    // `init_syscall_entry` has taken effect (an AP running a user thread while not
    // yet fully initialised); its first `syscall` would then `#UD` (`EFER.SCE=0`).
    // The `rdmsr` is cheap and runs every descent; the full re-arm runs only in the
    // (should-never-happen) unarmed case, so there is no steady-state cost.
    // SAFETY: `rdmsr(EFER)` is architectural in ring 0.
    if unsafe { regs::rdmsr(MSR_EFER) } & EFER_SCE == 0 {
        init_syscall_entry();
    }
}

/// Set the per-CPU kernel stack the `syscall` entry stub switches to. The
/// scheduler/`thread_enter` calls this with the running user thread's kernel
/// stack top before descending to ring 3, so a syscall lands on the right
/// stack.
pub fn set_syscall_kernel_stack(top: u64) {
    // SAFETY: writes only the running CPU's slot. We're in ring 0 about to descend
    // to ring 3 on this CPU, so no `syscall` on this CPU can be reading `kstack_top`
    // concurrently, and other CPUs touch only their own slots.
    unsafe {
        (&raw mut (*this_cpu_block()).kstack_top).write(top);
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
        "push gs:[{scratch}]",      // user_rsp (last `gs:` use — read before swap-back)
        // Swap GS back so the *body* runs in the userspace GS state
        // (`GS_BASE = 0`, `KERNEL_GS_BASE = &CPU0`). This is the invariant
        // `enter_user` assumes and that every ring-3 entry (`syscall` here,
        // traps via the IDT) needs: a thread that blocks mid-body is switched
        // away with `KERNEL_GS_BASE = &CPU0` (not the parked user GS), so a
        // sibling's `syscall` `swapgs` still lands `GS_BASE = &CPU0`. The body
        // never touches `gs:` (only this stub does), so running it with
        // `GS_BASE = 0` is fine. Matching swap is the one at entry above — there
        // is deliberately **no** swap before `sysretq` (the body already holds
        // the userspace GS state). See the module docs.
        "swapgs",
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
        // Every GPR has now been restored to the user's own saved value
        // (the pops above), except RAX (return value), RCX (user RIP), and
        // R11 (user RFLAGS) which `sysretq` consumes. Nothing kernel-private
        // is left in any register, so no scrubbing is needed — and the ABI
        // contract that all GPRs but RCX/R11/RAX are preserved across a
        // syscall (`docs/spec/syscall-abi.md`) holds. (An earlier version
        // zeroed RDX/RSI/RDI/R8/R9/R10 here; that destroyed the user's own
        // restored argument registers and broke multi-syscall callers, while
        // leaking nothing — the popped values are the user's, not kernel
        // scratch. Removed; see the decision log.)
        // No `swapgs` here: the body already ran in the userspace GS state
        // (`GS_BASE = 0`, `KERNEL_GS_BASE = &CPU0`) — the entry stub swapped
        // back right after grabbing the kernel stack — so `sysretq` returns
        // with GS already correct.
        "sysretq",
        scratch  = const OFF_SCRATCH,
        kstack   = const OFF_KSTACK,
        rax_off  = const offset_of!(SyscallFrame, rax),
        dispatch = sym syscall_dispatch,
    );
}

// --- Ring-3 descent -------------------------------------------------------

/// Descend to ring 3 at `entry` with user stack `user_sp`, seeding the four
/// bootstrap argument registers `rdi = a0`, `rsi = a1`, `rdx = a2`, `rcx = a3`
/// (the spawn hand-off by which a process learns its initial handle values —
/// notification channel, root namespace, first installed handle, `arg0`; all `0`
/// for the boot/`hello` path). Never returns: the only way back to ring 0 is via
/// the `syscall` entry stub (or a trap).
///
/// Called from `thread_enter` on a user thread's first run. The page-table
/// root (CR3) is already the process address space (loaded by the scheduler
/// on switch-in); `TSS.RSP0` and the per-CPU syscall stack already point at
/// this thread's kernel stack (set by `thread_enter`). We build an `iretq`
/// frame and go. No `swapgs`: ring 0 runs with `GS_BASE = 0` and
/// `KERNEL_GS_BASE = &CPU0`, and the user's first `syscall` swaps it in.
///
/// ## Stack alignment at ring-3 entry
///
/// The entry point is reached by `iretq`, not by `call`, but userspace entry points
/// are ordinary `extern "C"` functions — and the SysV AMD64 ABI says a function
/// body may assume `RSP ≡ 8 (mod 16)` on entry, because a `call` has just pushed
/// an 8-byte return address onto a 16-aligned stack. So this routine rounds
/// `user_sp` **down** to a 16-byte boundary and then subtracts 8, synthesising the
/// state a `call` would have left. It is the exact ring-3 analogue of
/// [`thread_trampoline`](crate::arch::thread_trampoline)'s `and rsp, -16` before
/// its `call`.
///
/// This is not cosmetic. With a soft-float userspace nothing on the stack needed
/// more than 8-byte alignment, so a misaligned `RSP` was invisible; the moment
/// userspace moved to a hard-float target (Phase 4, `x86_64-unknown-nitrox`) LLVM
/// began spilling `xmm` registers with `movaps`, which `#GP`s on a 16-byte
/// misalignment — `init` faulted on its first such spill. Adjusting here rather
/// than at load time makes the guarantee unconditional: it covers a user-supplied
/// stack pointer for a future `sys_thread_create` just as it covers the initial
/// stack the ELF loader picks. Rounding *down* only ever moves `RSP` further into
/// the mapped stack, never past its top.
///
/// # Safety
/// `entry`/`user_sp` must be canonical user addresses mapped executable /
/// writable in the active address space; [`init_syscall_entry`] must have
/// run; the active CR3 must be the process address space.
#[unsafe(naked)]
pub unsafe extern "C" fn enter_user(
    entry: u64,
    user_sp: u64,
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
) -> ! {
    // SAFETY: naked. SysV: rdi=entry, rsi=user_sp, rdx=a0, rcx=a1, r8=a2, r9=a3.
    // Build the iretq frame (consuming rdi/rsi), then move the bootstrap args
    // into the user's rdi/rsi/rdx/rcx and zero every other GPR so no kernel value
    // leaks to userspace.
    naked_asm!(
        // Synthesise the post-`call` stack shape the SysV ABI promises a function
        // body: 16-byte align, then bias by 8 (see the doc comment above). Without
        // this, the first `movaps` spill in the entry point `#GP`s.
        "and rsi, -16",
        "sub rsi, 8",
        "push {user_ss}",           // SS
        "push rsi",                 // RSP = user_sp (16-aligned, minus the ABI bias)
        "push {rflags}",            // RFLAGS (IF=1; bit1 reserved=1) — ring 3 runs preemptible
        "push {user_cs}",           // CS
        "push rdi",                 // RIP = entry
        // Seed the user's bootstrap argument registers BEFORE zeroing the rest.
        // `rsi = rcx` must read `a1` from rcx before `rcx` is overwritten with a3.
        "mov rdi, rdx",             // user rdi = a0
        "mov rsi, rcx",             // user rsi = a1
        "mov rdx, r8",              // user rdx = a2
        "mov rcx, r9",              // user rcx = a3
        "xor eax, eax",
        "xor ebx, ebx",
        "xor ebp, ebp",
        "xor r8d, r8d",
        "xor r9d, r9d",
        "xor r10d, r10d",
        "xor r11d, r11d",
        "xor r12d, r12d",
        "xor r13d, r13d",
        "xor r14d, r14d",
        "xor r15d, r15d",
        "iretq",
        user_ss = const gdt::USER_DATA_SELECTOR as u64,
        user_cs = const gdt::USER_CODE_SELECTOR as u64,
        // bit 1 reserved (always 1) + IF (bit 9): ring 3 runs with interrupts
        // enabled so the periodic timer preempts user threads. A `syscall` back
        // into the kernel re-masks IF via SFMASK.
        rflags  = const 0x202u64,
    );
}
