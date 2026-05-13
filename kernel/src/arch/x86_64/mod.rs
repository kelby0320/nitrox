//! x86_64-specific primitives for Phase 0.

use core::arch::asm;

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
