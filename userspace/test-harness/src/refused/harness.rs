//! **The harness's raw calls**: each passes what a safe wrapper cannot express, so the call is the
//! test. `check-syscalls` counts this module on its own.

use libkern::{SYS_CLOCK_READ, abi, syscall2};

/// Read the monotonic clock into the eight bytes at `dest`, which libos never does: it passes its
/// own local. The test points it into a mapping this process may only read.
pub(crate) fn clock_read_into(dest: *const u8) -> i64 {
    // SAFETY: `dest` is a mapped user page, so the kernel's copy-out faults on the user side and
    // answers `FaultFromUser` rather than writing.
    unsafe { syscall2(SYS_CLOCK_READ, abi::CLOCK_MONOTONIC, dest as u64) }
}
