//! Freestanding `mem*` intrinsics for the bare userspace target.
//!
//! On the non-PIE, `/DISCARD/`-`.plt` `x86_64-unknown-nitrox` target the `compiler_builtins`
//! `mem*` symbols resolve incorrectly (`memcpy` lands in the middle of an unrelated function),
//! so these **strong** definitions override its weak ones.
//!
//! The loops use `read_volatile`/`write_volatile` so LLVM's loop-idiom recogniser cannot fold
//! them back into a `memcpy`/`memset` call (which would recurse). Built only for the bare target.
//!
//! The signatures use [`c_void`] because rustc checks these runtime symbols against the shapes
//! the standard library expects. `-Z build-std-features=compiler-builtins-mem` must stay **off**
//! in the userspace build, or the same symbols would be defined twice.

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
