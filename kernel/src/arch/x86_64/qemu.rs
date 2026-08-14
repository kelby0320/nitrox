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

/// Report the harness verdict `code` to QEMU's `isa-debug-exit` device (the low byte
/// is what QEMU reads; it exits with `(code << 1) | 1`).
///
/// **Returns if — and only if — the device is absent**, which is not the exotic case the
/// original version of this assumed. Only `xtask test-qemu` attaches
/// `-device isa-debug-exit`; `check-input`, `check-display`, `check-terminal` and
/// `test-interactive` boot this same `test-harness` image *without* it, because they need
/// the guest alive after the boot self-test reports its verdict. For them the write is
/// ignored and this returns.
///
/// It used to park the CPU in a bespoke `hlt` loop instead, "so we don't execute past a
/// 'we're done' point". That cost two things and bought nothing: the CPU halted with
/// interrupts disabled while still counted in `sched::online_mask`, deadlocking the
/// machine's next TLB shootdown (see [`crate::sched::leave_online`]), and it stranded the
/// calling thread forever on a CPU that would never run again. Returning lets the caller
/// decide; the one caller that must not continue — the kernel panic path — halts itself.
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
