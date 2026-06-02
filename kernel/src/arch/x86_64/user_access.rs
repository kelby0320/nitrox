//! x86_64 raw user-memory copy primitives.
//!
//! The arch-specific half of the user-memory-access subsystem: two
//! `unsafe` functions that copy bytes between kernel and user memory
//! under the SMAP `stac`/`clac` window, with `#PF`-during-copy
//! converted to a fault flag via the kernel-wide exception table
//! (`.user_access_table` section, walked by
//! [`crate::mm::user_access::lookup_recovery`]).
//!
//! The arch-neutral half — [`UserPtr<T>`](crate::mm::user_access::UserPtr),
//! [`UserMutPtr<T>`](crate::mm::user_access::UserMutPtr), validation,
//! and the public `copy_*_user` API — lives in
//! [`crate::mm::user_access`]. That module calls into the functions
//! here for the actual asm.
//!
//! When aarch64 is implemented its raw primitives live in
//! `kernel/src/arch/aarch64/user_access.rs` and use PAN (Privileged
//! Access Never) in place of SMAP, with the same return shape so the
//! mm-layer wrapper does not need to change.

/// Copy `len` bytes from `src` to `dst` under SMAP discipline.
/// Returns `true` if a `#PF` occurred inside the copy window
/// (the kernel-side bytes are partially copied; the caller treats
/// the operation as failed), `false` on success.
///
/// `src` and `dst` must not overlap. One of them is a user pointer;
/// the other is a kernel pointer. SMAP must be enabled
/// ([`crate::arch::ensure_smap_smep`] has run).
///
/// # Safety
/// - At least one of `src` / `dst` points into user memory; the
///   other must be a valid kernel pointer for the corresponding
///   direction.
/// - `len` bytes from `src` and to `dst` must be reachable for the
///   direction in question — the user side may fault, in which
///   case the recovery path converts the fault into the `true`
///   return.
#[cfg(not(test))]
pub(crate) unsafe fn copy_bytes_raw(
    dst: *mut u8,
    src: *const u8,
    len: usize,
) -> bool {
    if len == 0 {
        return false;
    }
    let faulted: u64;
    // SAFETY: forwarded from this function's contract. The asm opens
    // the SMAP window with `stac`, runs `rep movsb` once (advancing
    // rsi/rdi/rcx and possibly faulting on a user-side access),
    // then closes the window with `clac` on either path. The
    // exception-table entry (`.user_access_table`) registers the
    // `rep movsb` PC as the fault site and the recovery label as
    // the resume PC; the #PF handler patches `frame.rip` to that
    // label on match.
    unsafe {
        core::arch::asm!(
            "stac",
            // Fault PC: the `rep movsb` instruction itself. On a
            // user-side #PF the CPU leaves RIP pointing here so the
            // exception-table match below can find it.
            "2:",
            "rep movsb",
            // Success: close SMAP, return 0.
            "clac",
            "xor eax, eax",
            "jmp 3f",
            // Recovery PC: the #PF handler jumped here. The saved
            // RFLAGS still has AC=1 (we faulted inside the stac
            // window), so the first instruction is `clac` to put
            // SMAP enforcement back. Then signal failure with rax=1.
            "4:",
            "clac",
            "mov eax, 1",
            "3:",
            // Register the recovery pair in the exception table.
            ".pushsection .user_access_table, \"a\"",
            ".balign 8",
            ".quad 2b",
            ".quad 4b",
            ".popsection",
            inout("rsi") src => _,
            inout("rdi") dst => _,
            inout("rcx") len => _,
            lateout("rax") faulted,
            options(nostack),
        );
    }
    faulted != 0
}

#[cfg(test)]
pub(crate) unsafe fn copy_bytes_raw(
    dst: *mut u8,
    src: *const u8,
    len: usize,
) -> bool {
    // Host stub. No SMAP, no exception table; the asm body is what
    // the integration test will eventually exercise. The mm-layer
    // wrapper tests use this to verify validation plumbing without
    // privileged instructions.
    // SAFETY: forwarded from this function's contract; the host
    // never has user memory, so this only runs against ordinary
    // in-process buffers.
    unsafe { core::ptr::copy_nonoverlapping(src, dst, len) };
    false
}

/// Outcome of a byte-by-byte NUL-terminated user copy.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum CstrCopyOutcome {
    /// NUL terminator was found within `max_len` bytes. The `usize`
    /// is the number of bytes written to `dst`, including the NUL.
    Ok(usize),
    /// A `#PF` occurred on a user-side load. The kernel-side `dst`
    /// may hold a partial copy; the caller treats it as failed.
    Fault,
    /// `dst` filled to `max_len` without encountering a NUL byte.
    /// The caller can grow the buffer and retry.
    NoTerminator,
}

/// Byte-by-byte copy of a NUL-terminated user string. Stops at the
/// first NUL byte or when `max_len` bytes have been read.
///
/// # Safety
/// Same contract as [`copy_bytes_raw`]: `src` points into user
/// memory, `dst` provides `max_len` writable kernel bytes, SMAP is
/// enabled.
#[cfg(not(test))]
pub(crate) unsafe fn copy_cstr_raw(
    dst: *mut u8,
    src: *const u8,
    max_len: usize,
) -> CstrCopyOutcome {
    let result_code: u64;
    let count: u64;
    // SAFETY: forwarded from this function's contract. The asm uses
    // `lodsb` for the user-side load (the registered fault site) and
    // `stosb` for the kernel-side store; rax/al holds each loaded
    // byte; rdx counts bytes written.
    unsafe {
        core::arch::asm!(
            "stac",
            "xor rdx, rdx",
            // Fault PC. Only `lodsb` reads from user memory; the
            // subsequent `stosb`, `inc`, `test`, `cmp`, `jb` touch
            // only kernel memory (`stosb` into `dst`) or no memory
            // at all, so a #PF inside this block can only originate
            // from `lodsb`.
            "20:",
            "lodsb",
            "stosb",
            "inc rdx",
            "test al, al",
            "jz 22f",                  // found NUL
            "cmp rdx, rcx",
            "jb 20b",                  // continue (rdx < max_len)
            // Filled without finding NUL.
            "clac",
            "mov eax, 2",
            "jmp 23f",
            // Found NUL.
            "22:",
            "clac",
            "xor eax, eax",
            "jmp 23f",
            // Recovery PC.
            "24:",
            "clac",
            "mov eax, 1",
            "23:",
            ".pushsection .user_access_table, \"a\"",
            ".balign 8",
            ".quad 20b",
            ".quad 24b",
            ".popsection",
            inout("rsi") src => _,
            inout("rdi") dst => _,
            inout("rcx") max_len => _,
            lateout("rax") result_code,
            lateout("rdx") count,
            options(nostack),
        );
    }
    match result_code {
        0 => CstrCopyOutcome::Ok(count as usize),
        1 => CstrCopyOutcome::Fault,
        2 => CstrCopyOutcome::NoTerminator,
        // SAFETY: the asm writes exactly one of {0, 1, 2} into eax
        // on every exit path.
        _ => unsafe { core::hint::unreachable_unchecked() },
    }
}

#[cfg(test)]
std::thread_local! {
    /// One-shot **per-thread** flag the suite can set to force the next
    /// [`copy_cstr_raw`] host-stub call on the same thread to report a
    /// [`CstrCopyOutcome::Fault`], exercising the `#PF` mapping path that
    /// the real asm body produces only on a genuine user-side fault.
    ///
    /// Per-thread (rather than process-global) so concurrent tests do
    /// not consume each other's flag — cargo runs unit tests in parallel.
    pub(crate) static FAIL_NEXT_CSTR_COPY: core::cell::Cell<bool> =
        const { core::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) unsafe fn copy_cstr_raw(
    dst: *mut u8,
    src: *const u8,
    max_len: usize,
) -> CstrCopyOutcome {
    if FAIL_NEXT_CSTR_COPY.with(|f| f.replace(false)) {
        return CstrCopyOutcome::Fault;
    }
    // Host stub: byte-by-byte copy until NUL or max_len.
    // SAFETY: forwarded; host tests use ordinary in-process buffers.
    let src_slice = unsafe { core::slice::from_raw_parts(src, max_len) };
    let dst_slice = unsafe { core::slice::from_raw_parts_mut(dst, max_len) };
    for i in 0..max_len {
        dst_slice[i] = src_slice[i];
        if src_slice[i] == 0 {
            return CstrCopyOutcome::Ok(i + 1);
        }
    }
    CstrCopyOutcome::NoTerminator
}
