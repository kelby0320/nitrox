//! Freestanding `mem*` intrinsics for the bare userspace target.
//!
//! `core`/`alloc` (and any `[u8; N]` copy/compare the compiler lowers to a
//! libcall) reference `memcpy`/`memmove`/`memset`/`memcmp`. On this
//! `x86_64-unknown-nitrox`, non-PIE/`/DISCARD/`-`.plt` target the
//! `compiler_builtins` versions resolve incorrectly — the `memcpy` symbol lands in
//! the middle of an unrelated function, so a call jumps into garbage and faults
//! (the 2026-06-22 codegen hazard, now hit for real by init's `alloc` use). We
//! provide our own **strong** definitions, which override `compiler_builtins`'
//! weak ones, so every userspace binary that links `libkern` gets correct
//! intrinsics.
//!
//! The loops use `read_volatile`/`write_volatile` so LLVM's loop-idiom recogniser
//! cannot fold them back into a `memcpy`/`memset` call (which would recurse). The
//! copies here are small and not hot (init's `alloc`, the demos), so the lost
//! vectorisation does not matter.
//!
//! Built only for the bare target (`cfg(not(test))`): under `cargo test` libkern
//! is a host `std` crate and must not redefine libc's `mem*`.
//!
//! The signatures use [`c_void`] rather than `u8` because rustc checks these
//! well-known runtime symbols against the shapes the standard library expects, and
//! warns on a mismatch. (They also mean `-Z build-std-features=compiler-builtins-mem`
//! must stay **off** in the userspace build — these strong definitions are the ones
//! we want, and enabling that feature would define the same symbols twice.)

use core::ffi::c_void;

/// `memcpy(dest, src, n)` — copy `n` bytes; `dest`/`src` must not overlap.
///
/// # Safety
/// `dest` and `src` must be valid for `n` bytes and non-overlapping.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcpy(dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    let (d, s) = (dest.cast::<u8>(), src.cast::<u8>());
    let mut i = 0;
    while i < n {
        // SAFETY: caller guarantees `dest`/`src` valid for `n` bytes; `i < n`.
        unsafe { d.add(i).write_volatile(s.add(i).read_volatile()) };
        i += 1;
    }
    dest
}

/// `memmove(dest, src, n)` — copy `n` bytes, handling overlap.
///
/// # Safety
/// `dest` and `src` must be valid for `n` bytes (may overlap).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memmove(dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    let (d, s) = (dest.cast::<u8>(), src.cast::<u8>());
    if (d as usize) < (s as usize) {
        let mut i = 0;
        while i < n {
            // SAFETY: as `memcpy`; ascending copy is correct when dest < src.
            unsafe { d.add(i).write_volatile(s.add(i).read_volatile()) };
            i += 1;
        }
    } else {
        let mut i = n;
        while i > 0 {
            i -= 1;
            // SAFETY: as above; descending copy is correct when dest >= src.
            unsafe { d.add(i).write_volatile(s.add(i).read_volatile()) };
        }
    }
    dest
}

/// `memset(dest, c, n)` — fill `n` bytes with `c as u8`.
///
/// # Safety
/// `dest` must be valid for `n` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(dest: *mut c_void, c: i32, n: usize) -> *mut c_void {
    let d = dest.cast::<u8>();
    let byte = c as u8;
    let mut i = 0;
    while i < n {
        // SAFETY: caller guarantees `dest` valid for `n` bytes; `i < n`.
        unsafe { d.add(i).write_volatile(byte) };
        i += 1;
    }
    dest
}

/// `memcmp(a, b, n)` — compare `n` bytes; `0` if equal, else the signed
/// difference of the first differing byte pair (`a[i] - b[i]`).
///
/// # Safety
/// `a` and `b` must be valid for `n` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
    let (a, b) = (a.cast::<u8>(), b.cast::<u8>());
    let mut i = 0;
    while i < n {
        // SAFETY: caller guarantees `a`/`b` valid for `n` bytes; `i < n`.
        let (x, y) = unsafe { (a.add(i).read_volatile(), b.add(i).read_volatile()) };
        if x != y {
            return x as i32 - y as i32;
        }
        i += 1;
    }
    0
}
