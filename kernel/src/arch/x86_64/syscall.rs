//! x86_64 `syscall`/`sysretq` fast path and the ring-3 descent.
//!
//! `syscall` does not switch RSP, so the entry stub `swapgs`es to a per-CPU block ([`CpuLocal`]),
//! stashes the user RSP there, loads the kernel syscall stack, builds a [`SyscallFrame`] and
//! calls the neutral dispatcher, then restores and `sysretq`s. See `docs/spec/syscall-abi.md`.
//!
//! ## GS model
//!
//! On each CPU, `KERNEL_GS_BASE` is held at **that CPU's [`CpuLocal`] block** **at all times**
//! (in userspace, in the kernel body, and while a thread is blocked mid-syscall), and
//! `GS_BASE = 0` everywhere except the entry-stub window. The stub `swapgs`es in, grabs the
//! kernel stack, then `swapgs`es **back** before the body; there is no `swapgs` before
//! `sysretq`. The only `gs:`-relative code is that stub.
//!
//! This is what keeps the entry correct across threads: a thread that blocks mid-body is
//! switched away with `KERNEL_GS_BASE` still at this CPU's block, so a *sibling's* `swapgs`
//! still lands on it rather than on a stale `0` (a fault on `gs:[0]` is a `#DF`). The IDT trap
//! entries never touch `gs:` or `swapgs`, so they are consistent with `GS_BASE = 0` in both
//! rings.

use core::arch::naked_asm;
use core::mem::offset_of;

use super::{gdt, regs};
use crate::syscall::table;

const MSR_EFER: u32 = 0xC000_0080;
const EFER_SCE: u64 = 1 << 0; // syscall enable
const MSR_STAR: u32 = 0xC000_0081;
pub(super) const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_SFMASK: u32 = 0xC000_0084;
const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;

const RFLAGS_IF: u64 = 1 << 9;
const RFLAGS_DF: u64 = 1 << 10;
const RFLAGS_AC: u64 = 1 << 18;
/// RFLAGS bits cleared on `syscall` entry: interrupts, direction (so string ops in the dispatch
/// path are well-defined), and SMAP AC (the copy primitives reopen it with `stac`).
const SFMASK_VALUE: u64 = RFLAGS_IF | RFLAGS_DF | RFLAGS_AC;

/// Per-CPU kernel data reached via `IA32_KERNEL_GS_BASE` + `swapgs`, one per CPU in [`CPUS`].
/// The entry stub reads `gs:[0]` and `gs:[8]`; the `offset_of!` assertion below pins them.
#[repr(C)]
pub struct CpuLocal {
    /// `gs:[0]`: where the entry stub stashes the user RSP.
    rsp_scratch: u64,
    /// `gs:[8]`: top of the kernel syscall stack, loaded into RSP on entry. Set per-thread by
    /// [`set_syscall_kernel_stack`].
    kstack_top: u64,
}

const CPU_LOCAL_INIT: CpuLocal = CpuLocal { rsp_scratch: 0, kstack_top: 0 };

/// One [`CpuLocal`] per CPU, indexed by the dense `current_cpu()` id.
static mut CPUS: [CpuLocal; crate::arch::smp::MAX_CPUS] = [CPU_LOCAL_INIT; crate::arch::smp::MAX_CPUS];

const OFF_SCRATCH: usize = offset_of!(CpuLocal, rsp_scratch);
const OFF_KSTACK: usize = offset_of!(CpuLocal, kstack_top);
const _: () = assert!(OFF_SCRATCH == 0 && OFF_KSTACK == 8);

/// Raw pointer to the running CPU's [`CpuLocal`] slot, indexed by the dense CPU id from `RDTSCP`.
fn this_cpu_block() -> *mut CpuLocal {
    let idx = regs::rdtscp_aux() as usize;
    debug_assert!(idx < crate::arch::smp::MAX_CPUS, "cpu index out of range");
    // SAFETY: `idx < MAX_CPUS`: `IA32_TSC_AUX` is written only with 0 (the BSP) or a slot
    // of the `MAX_CPUS`-entry identity map (`adopt_dense_index`). Pointer arithmetic within the
    // static array (no bounds-check panic in this entry path, no reference formed);
    // each CPU addresses only its own slot.
    unsafe { (&raw mut CPUS).cast::<CpuLocal>().add(idx) }
}

/// Arm the syscall entry on this CPU: `EFER.SCE`, `STAR`, `LSTAR`, `SFMASK`, `KERNEL_GS_BASE`.
///
/// Must run after `gdt::init` (STAR's selectors must be in the loaded GDT) and before any ring-3
/// entry. The kernel stack is per-thread ([`set_syscall_kernel_stack`]).
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

/// Re-assert `KERNEL_GS_BASE =` this CPU's block before a thread's **first** ring-3 descent.
///
/// `enter_user` does no `swapgs`; a stale `KERNEL_GS_BASE` would fault on `gs:` in the first
/// `syscall`'s entry.
pub fn arm_user_entry_cpu_base() {
    let gs_base = this_cpu_block() as u64;
    // SAFETY: architectural MSR; this CPU's `CpuLocal` slot is the block the entry
    // stub reaches through `gs:` after `swapgs`.
    unsafe { regs::wrmsr(MSR_KERNEL_GS_BASE, gs_base) };

    // An AP could reach a user descent before its `init_syscall_entry` took effect; its first
    // `syscall` would then `#UD` (`EFER.SCE=0`). The re-arm runs only in that unarmed case.
    // SAFETY: `rdmsr(EFER)` is architectural in ring 0.
    if unsafe { regs::rdmsr(MSR_EFER) } & EFER_SCE == 0 {
        init_syscall_entry();
    }
}

/// Set the kernel stack the `syscall` entry stub switches to on this CPU.
///
/// Called with the user thread's kernel stack top before it descends to ring 3.
pub fn set_syscall_kernel_stack(top: u64) {
    // SAFETY: writes only the running CPU's slot. We're in ring 0 about to descend
    // to ring 3 on this CPU, so no `syscall` on this CPU can be reading `kstack_top`
    // concurrently, and other CPUs touch only their own slots.
    unsafe {
        (&raw mut (*this_cpu_block()).kstack_top).write(top);
    }
}

/// The register snapshot the entry stub builds on the kernel stack for [`syscall_dispatch`].
///
/// Fields are **lowest address first** and mirror the stub's push order (`r15` is pushed last,
/// so RSP points at it at the call); the `offset_of!` assertions pin the layout the stub's
/// `mov [rsp + …]` depends on. `rcx` holds the user RIP and `r11` the user RFLAGS, which
/// `sysretq` consumes.
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

/// Reached from [`syscall_entry`] with `rdi = &mut frame`. Routes the frame through
/// [`table::dispatch`]; the stub stores the returned `isize` into the frame's `rax` slot.
///
/// # Safety
/// `frame` must point at a fully-initialised [`SyscallFrame`] on the current
/// kernel stack — exactly what the entry stub builds.
unsafe extern "C" fn syscall_dispatch(frame: *mut SyscallFrame) -> isize {
    // A ring-3 entry opens no lock-ordering scope: the thread was in user mode, so it holds no
    // kernel lock and the tracker's held set must be empty. `cargo xtask check-irq-scope` knows
    // this entry is exempt for that reason.
    crate::libkern::lockrank::assert_user_entry_safe();
    // Likewise a thread in user mode cannot have kernel preemption disabled.
    crate::sched::assert_user_entry_preempt_safe();
    // SAFETY: the stub built a complete frame at the kernel stack top and
    // passed its address; valid, aligned, and unaliased for this call.
    let f = unsafe { &mut *frame };
    let ret = table::dispatch(f.rax, f.rdi, f.rsi, f.rdx, f.r10, f.r8, f.r9);
    crate::sched::end_if_ending();
    // **A syscall body must not return with interrupts enabled**: the stub's `cli` makes the
    // `pop rsp` / `sysretq` window safe regardless, and this catches the *cause*.
    debug_assert!(
        !<crate::arch::Cpu as crate::arch::cpu::ArchCpu>::interrupts_enabled(),
        "syscall {} returned with interrupts enabled",
        f.rax
    );
    ret
}

/// The `syscall` entry point (the `IA32_LSTAR` target).
#[allow(clippy::unnecessary_safety_comment, reason = "the asm's safety argument; `naked_asm!` has no `unsafe` block")]
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
        "push gs:[{scratch}]",      // user_rsp (last `gs:` use — read before swap-back)
        // Swap GS back so the body runs in the userspace GS state (`GS_BASE = 0`,
        // `KERNEL_GS_BASE = &CPU0`); only this stub touches `gs:`. See the module docs.
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
        // **Mask interrupts before the user stack is loaded**; `sysretq` restores the user's IF
        // from R11. Between `pop rsp` and `sysretq` the CPU is in ring 0 on the *user* stack,
        // where an interrupt pushes onto that stack, faults under SMAP, and double-faults. The
        // stub must not depend on every kernel path having kept `IF` masked. On entry the same
        // window is covered by SFMASK (IF) and IST2 (NMI), see `idt.rs`.
        "cli",
        "pop rsp",                  // restore user RSP
        // Every GPR is back to the user's own value except RAX (return), RCX (user RIP) and R11
        // (user RFLAGS), which `sysretq` consumes: nothing kernel-private is left to scrub, and
        // `docs/spec/syscall-abi.md`'s "all but RCX/R11/RAX preserved" holds. No `swapgs`: GS is
        // already in the userspace state.
        "sysretq",
        scratch  = const OFF_SCRATCH,
        kstack   = const OFF_KSTACK,
        rax_off  = const offset_of!(SyscallFrame, rax),
        dispatch = sym syscall_dispatch,
    );
}

/// Descend to ring 3 at `entry` with user stack `user_sp`, seeding `rdi = a0`, `rsi = a1`,
/// `rdx = a2`, `rcx = a3` (the spawn hand-off of a process's initial handle values).
///
/// Never returns: the only way back is the `syscall` entry stub or a trap. CR3, `TSS.RSP0` and
/// the syscall stack must already be this thread's. No `swapgs`: ring 0 runs with `GS_BASE = 0`
/// and `KERNEL_GS_BASE = &CPU0`, and the user's first `syscall` swaps it in.
///
/// ## Stack alignment at ring-3 entry
///
/// The entry is reached by `iretq`, not `call`, but the SysV ABI lets a function assume
/// `RSP ≡ 8 (mod 16)` on entry. So `user_sp` is rounded **down** to 16 bytes and 8 is
/// subtracted, as `call` would leave it (the ring-3 analogue of
/// [`thread_trampoline`](crate::arch::thread_trampoline)'s `and rsp, -16`). It is not cosmetic:
/// hard-float code spills `xmm` registers with `movaps`, which `#GP`s on misalignment. Rounding
/// down never moves `RSP` past the stack's top, and it covers a user-supplied stack too.
///
/// # Safety
/// `entry`/`user_sp` must be canonical user addresses mapped executable /
/// writable in the active address space; [`init_syscall_entry`] must have
/// run; the active CR3 must be the process address space.
#[allow(clippy::unnecessary_safety_comment, reason = "the asm's safety argument; `naked_asm!` has no `unsafe` block")]
#[unsafe(naked)]
pub unsafe extern "C" fn enter_user(entry: u64, user_sp: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> ! {
    // SAFETY: naked. SysV: rdi=entry, rsi=user_sp, rdx=a0, rcx=a1, r8=a2, r9=a3.
    // Build the iretq frame (consuming rdi/rsi), then move the bootstrap args
    // into the user's rdi/rsi/rdx/rcx and zero every other GPR so no kernel value
    // leaks to userspace.
    naked_asm!(
        // The post-`call` stack shape: 16-byte align, then bias by 8 (see the doc comment).
        "and rsi, -16",
        "sub rsi, 8",
        "push {user_ss}",           // SS
        "push rsi",                 // RSP = user_sp (16-aligned, minus the ABI bias)
        "push {rflags}",            // RFLAGS (IF=1; bit1 reserved=1) — ring 3 runs preemptible
        "push {user_cs}",           // CS
        "push rdi",                 // RIP = entry
        // Seed the argument registers before zeroing the rest; `rcx` is read before a3 lands.
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
        // Bit 1 reserved (always 1) + IF: the timer preempts ring 3. `syscall` re-masks IF
        // via SFMASK.
        rflags  = const 0x202u64,
    );
}
