//! **Random bytes from the kernel's entropy source**: `account`'s salts, and since Phase 6 Part G.4
//! `disk`'s volume IDs, UUIDs and partition GUIDs. It was `account`'s own until `disk` needed it
//! too.

use libkern::syscall::{SYS_ENTROPY_CREATE, SYS_ENTROPY_READ, SYS_HANDLE_CLOSE, SYS_WAIT, syscall0, syscall1, syscall3, syscall4};

/// **Fill `buf`** from the kernel's entropy source — which may answer with a pending operation to
/// wait on before it has been seeded, and is read again after it. `false` if it would not answer;
/// **`buf` is then not to be used**, since a constant in place of a random one is a collision with
/// every other machine's.
pub fn fill(buf: &mut [u8]) -> bool {
    // SAFETY: register-only syscall; returns a fresh entropy handle.
    let ent = unsafe { syscall0(SYS_ENTROPY_CREATE) };
    if ent <= 0 {
        return false;
    }
    let ent = ent as u64;
    let got = loop {
        // SAFETY: a valid out-buffer of `buf.len()` bytes, and an entropy handle with READ.
        let r = unsafe { syscall3(SYS_ENTROPY_READ, ent, buf.as_mut_ptr() as u64, buf.len() as u64) };
        if r == 0 {
            break true;
        }
        if r < 0 {
            break false;
        }
        // Not yet seeded: wait for the pending operation, then read again.
        let handles = [r as u64];
        let mut results = [0u8; 24];
        // SAFETY: a valid one-entry handle array and result buffer on this frame.
        let waited = unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, results.as_mut_ptr() as u64, u64::MAX) };
        // SAFETY: closing the pending operation this process owns.
        unsafe { syscall1(SYS_HANDLE_CLOSE, r as u64) };
        let status = i32::from_le_bytes([results[8], results[9], results[10], results[11]]);
        if waited != 1 || status != 0 {
            break false;
        }
    };
    // SAFETY: closing the entropy handle this process owns.
    unsafe { syscall1(SYS_HANDLE_CLOSE, ent) };
    got
}
