//! QEMU integration-test support: the `isa-debug-exit` shutdown primitive.
//!
//! Compiled **only** under the kernel's `test-harness` feature (`cargo xtask
//! test-qemu`); it does not exist in production builds, so there is no
//! emulator-exit backdoor in a shipping kernel. See
//! `docs/conventions/qemu-integration-tests.md`.
//!
//! QEMU's `isa-debug-exit` device (`-device isa-debug-exit,iobase=0xf4,
//! iosize=0x04`) turns a guest write to its I/O port into a host process exit:
//! QEMU terminates with status `(value << 1) | 1`. The value is arbitrary — the
//! xtask runner owns the pass/fail mapping (`0x10` → exit 33 = pass, `0x11` →
//! exit 35 = fail). Because the low bit is always set, exit `0` is unreachable,
//! so "pass" is a chosen odd code, not zero.

use super::regs;

/// The `isa-debug-exit` I/O port. Must match the `iobase=` the runner passes to
/// QEMU (`tools/xtask`).
const DEBUG_EXIT_PORT: u16 = 0xf4;

/// Terminate the emulator with the harness verdict `code` (the low byte is what
/// QEMU reads; it exits with `(code << 1) | 1`). Never returns — on the
/// vanishingly unlikely chance the device is absent (a non-QEMU host, or a
/// mistyped `-device`), fall through to a halt so we don't execute past a
/// "we're done" point.
pub fn debug_exit(code: u32) -> ! {
    // SAFETY: writing the harness verdict to the `isa-debug-exit` port. Under
    // the `test-harness` feature the runner always attaches the device at this
    // `iobase`; the write's only effect is to terminate QEMU. Ring 0, and by
    // this point no other CPU is doing meaningful work (a verdict has been
    // reached), so racing port writers are not a concern.
    unsafe {
        regs::outb(DEBUG_EXIT_PORT, code as u8);
    }
    // Device absent / write ignored: park this CPU. `hlt` in a loop with
    // interrupts in whatever state the caller left them; we only reach here on a
    // misconfigured host.
    loop {
        // SAFETY: `hlt` merely halts until the next interrupt; sound in ring 0.
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}
