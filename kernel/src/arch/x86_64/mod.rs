//! x86_64-specific primitives.

pub mod abi;
pub mod context;
pub mod gdt;
pub mod idt;
pub mod paging;
pub mod regs;
pub mod serial;
pub mod syscall;
pub mod user_access;

use core::arch::asm;

/// Install the architecture's CPU control tables, early in boot.
///
/// On x86_64 this is the GDT (with its TSS) followed by the IDT. The order
/// is fixed here, not exposed to the caller: the IDT's gates reference the
/// kernel code selector the GDT installs, and the double-fault gate needs
/// the TSS's IST stack. The arch-neutral entry point — `main` calls this
/// rather than the per-table `gdt`/`idt` initialisers.
pub fn init_cpu_tables() {
    gdt::init();
    idt::init();
}

/// Park the CPU forever. Disables interrupts and `hlt`s in a loop so a
/// spurious wake-up cannot restart execution. This is the only sanctioned
/// way to exit the kernel's top-level entry point in Phase 0.
pub fn halt_loop() -> ! {
    loop {
        // SAFETY: `cli` and `hlt` are always valid in ring 0. Neither
        // touches memory; both are allowed under the kernel's lock
        // ordering since no locks are held at the call site (we're at
        // the top of the boot path).
        unsafe {
            asm!("cli", "hlt", options(nomem, nostack, preserves_flags));
        }
    }
}
