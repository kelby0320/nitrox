//! The `isa-debug-exit` shutdown primitive, compiled only under `test-harness`.
//!
//! A write to the device's port makes QEMU exit with `(value << 1) | 1`, so exit `0` is
//! unreachable; the xtask runner maps `0x10` (exit 33) to pass and `0x11` (exit 35) to fail.
//! See `docs/conventions/qemu-integration-tests.md`.

use super::regs;

/// The `isa-debug-exit` I/O port; must match the runner's `iobase=` (`tools/xtask`).
const DEBUG_EXIT_PORT: u16 = 0xf4;

/// Report the harness verdict `code` to QEMU's `isa-debug-exit` device.
///
/// Returns if the device is absent: only `xtask test-qemu` attaches it, while the gates that
/// boot this image to keep the guest alive after the verdict do not. It does not park the CPU,
/// which would leave it in `sched::online_mask` with interrupts off and deadlock the next TLB
/// shootdown (see [`crate::sched::leave_online`]); the panic path halts itself.
pub fn debug_exit(code: u32) {
    // SAFETY: writing the harness verdict to the `isa-debug-exit` port. Under
    // the `test-harness` feature the runner always attaches the device at this
    // `iobase`; the write's only effect is to terminate QEMU. Ring 0, and by
    // this point no other CPU is doing meaningful work (a verdict has been
    // reached), so racing port writers are not a concern.
    unsafe {
        regs::outb(DEBUG_EXIT_PORT, code as u8);
    }
}
