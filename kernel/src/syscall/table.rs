//! The syscall number → handler table and the handlers themselves.
//!
//! Dispatch is a `match` on the number (per `kernel/CLAUDE.md`: match, not
//! `dyn`), keyed by the constants below. The stable ABI numbers
//! (`docs/spec/syscall-abi.md`) are allocated sequentially from `0`; the
//! **debug** syscalls this slice adds live in a high, deliberately
//! non-stable range so they can never shadow a future stable number. They
//! are excluded from the v1.0 ABI freeze and exist only to bootstrap and
//! exercise the entry/exit path before real syscalls land.

use super::error::{KError, SysResult, encode, from_user_access};
use crate::mm::user_access::{UserPtr, copy_slice_from_user};

/// Debug: write a user byte buffer to the kernel serial log. Not ABI-stable.
pub const SYS_DEBUG_KPRINT: u64 = 0xFFFF_0000;
/// Debug: leave ring 3 and return control to the kernel bootstrap. Not
/// ABI-stable; see [`crate::arch`]'s throwaway ring-3 harness.
pub const SYS_DEBUG_EXIT: u64 = 0xFFFF_0001;

/// Largest buffer `sys_kprint` will copy in one call. Bounds the on-stack
/// kernel buffer; well under `MAX_USER_COPY_SIZE`.
const KPRINT_MAX: usize = 4096;

/// Route a decoded syscall to its handler. `nr` is the number (from RAX);
/// `a0..a5` are the six argument registers (RDI, RSI, RDX, R10, R8, R9).
/// Returns the `isize` the ABI hands back in RAX.
pub fn dispatch(nr: u64, a0: u64, a1: u64, _a2: u64, _a3: u64, _a4: u64, _a5: u64) -> isize {
    match nr {
        SYS_DEBUG_KPRINT => encode(sys_kprint(a0, a1 as usize)),
        // Diverges: returns to the kernel bootstrap, never back to dispatch.
        SYS_DEBUG_EXIT => crate::arch::syscall_debug_exit(a0 as i32),
        _ => KError::Unsupported.as_isize(),
    }
}

/// `sys_kprint(ptr, len)` — copy `len` bytes from the user buffer at `ptr`
/// and write them to the serial console. Debug-only. Returns the number of
/// bytes written.
///
/// The validation/bounds checks are ordered so the `len == 0` and
/// `len > KPRINT_MAX` paths are reachable without touching user memory or
/// the serial port (host-testable); the copy + serial write are exercised
/// only under QEMU.
pub fn sys_kprint(ptr: u64, len: usize) -> SysResult {
    if len == 0 {
        return Ok(0);
    }
    if len > KPRINT_MAX {
        return Err(KError::TooLarge);
    }
    let uptr = UserPtr::<u8>::new(ptr).map_err(from_user_access)?;

    let mut buf = [0u8; KPRINT_MAX];
    let dst = &mut buf[..len];
    // SMAP-safe, fault-recovering copy: a bad user buffer yields
    // `UserAccessError::Fault` (→ `FaultFromUser`), never a kernel halt.
    copy_slice_from_user(dst, uptr).map_err(from_user_access)?;

    // SERIAL is rank 7 (lowest); no other lock is held here.
    let serial = crate::arch::serial::SERIAL.lock();
    for &b in dst.iter() {
        serial.write_byte(b);
    }
    Ok(len as isize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_number_is_unsupported() {
        assert_eq!(dispatch(0xDEAD, 0, 0, 0, 0, 0, 0), KError::Unsupported.as_isize());
    }

    #[test]
    fn kprint_zero_len_is_ok_without_touching_memory() {
        // len == 0 returns before building a UserPtr or touching serial.
        assert_eq!(dispatch(SYS_DEBUG_KPRINT, 0xDEAD_BEEF, 0, 0, 0, 0, 0), 0);
        assert_eq!(sys_kprint(0xDEAD_BEEF, 0), Ok(0));
    }

    #[test]
    fn kprint_oversize_is_too_large_without_touching_memory() {
        let too_big = (KPRINT_MAX + 1) as u64;
        assert_eq!(
            dispatch(SYS_DEBUG_KPRINT, 0xDEAD_BEEF, too_big, 0, 0, 0, 0),
            KError::TooLarge.as_isize(),
        );
        assert_eq!(sys_kprint(0xDEAD_BEEF, KPRINT_MAX + 1), Err(KError::TooLarge));
    }
}
