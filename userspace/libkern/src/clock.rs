//! **Reading a clock.**

use crate::error::{KError, from_raw};
use crate::syscall::{SYS_CLOCK_READ, syscall2};

/// Read clock `clock` (`CLOCK_MONOTONIC` and the like), in nanoseconds. `Err` with the kernel's
/// refusal; nothing is read then.
pub fn clock_read(clock: u64) -> Result<u64, KError> {
    let mut ns = 0u64;
    // SAFETY: `ns` is a writable u64 out-param on this frame.
    let r = unsafe { syscall2(SYS_CLOCK_READ, clock, (&raw mut ns) as u64) };
    from_raw(r).map(|_| ns)
}
