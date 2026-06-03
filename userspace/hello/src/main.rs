//! `hello` — the first Nitrox userspace program (a throwaway Phase-1 proof).
//!
//! A freestanding ring-3 program: it prints one line via the debug
//! `sys_kprint` syscall, then exits via `sys_process_exit`. It exists only to
//! demonstrate the kernel can load an ELF, build a process + address space,
//! schedule a thread into ring 3, service a syscall, and tear the process
//! down. It is replaced by the real `init` (PID 1) once an initramfs exists.
//!
//! Built as a **static, non-PIE `ET_EXEC`** at a low user virtual address —
//! see `user.ld` and `.cargo/config.toml`. The kernel's ELF loader
//! (`kernel/src/mm/elf.rs`) rejects PIE/`ET_DYN`, dynamic interpreters, and
//! misaligned segments, so the build flags matter.

#![no_std]
#![no_main]

use core::arch::asm;

/// Debug syscall numbers — must match `kernel/src/syscall/table.rs`. These
/// live in a high, non-ABI-stable range and exist only for bootstrap.
const SYS_DEBUG_KPRINT: u64 = 0xFFFF_0000;
const SYS_PROCESS_EXIT: u64 = 0xFFFF_0001;

/// The message printed from ring 3. A `const &[u8]` so the bytes are a
/// link-time rodata constant whose address resolves without a dynamic
/// relocation (the binary is non-PIE).
const MSG: &[u8] = b"hello from ring 3 (pid 1)\n";

/// ELF entry point. The kernel sets RSP to a freshly mapped user stack via
/// the `iretq` frame before transferring control here, but this code is
/// stack-light (no `call`/`push`) and does not rely on stack contents —
/// there is no argv/auxv setup yet.
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // SAFETY: the ring-3 syscall ABI is rax = number, rdi/rsi = args 1-2;
    // the `syscall` instruction clobbers rcx (saved RIP) and r11 (saved
    // RFLAGS), and returns in rax. We touch no memory the kernel hasn't
    // mapped and use no stack.
    unsafe {
        // sys_debug_kprint(MSG.as_ptr(), MSG.len())
        asm!(
            "syscall",
            in("rax") SYS_DEBUG_KPRINT,
            in("rdi") MSG.as_ptr(),
            in("rsi") MSG.len(),
            out("rcx") _,
            out("r11") _,
            lateout("rax") _,
            options(nostack),
        );
        // sys_process_exit(0) — never returns to ring 3.
        asm!(
            "syscall",
            in("rax") SYS_PROCESS_EXIT,
            in("rdi") 0usize,
            options(noreturn, nostack),
        );
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // The program has no fallible operations, so this is unreachable in
    // practice; spin defensively rather than risk a further fault.
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}
