//! x86_64 raw user-memory copy primitives.
//!
//! Two `unsafe` functions copy bytes between kernel and user memory under the SMAP `stac`/`clac`
//! window, turning a `#PF` during the copy into a fault flag via the exception table
//! (`.user_access_table`, walked by [`crate::mm::user_access::lookup_recovery`]).
//!
//! Validation and the public `copy_*_user` API live in [`crate::mm::user_access`]. See
//! `docs/spec/user-memory-access.md`.

/// Copy `len` bytes from `src` to `dst` under SMAP discipline.
///
/// Returns `true` if a `#PF` occurred inside the copy window (the kernel-side bytes are
/// partially copied), `false` on success. `src` and `dst` must not overlap; one is a user
/// pointer and the other a kernel pointer. SMAP must be enabled
/// ([`crate::arch::ensure_smap_smep`] has run).
///
/// # Safety
/// One of `src` / `dst` points into user memory, the other must be a valid kernel pointer. `len`
/// bytes must be reachable on both sides; the user side may fault, which the recovery path turns
/// into the `true` return.
#[cfg(not(test))]
pub(crate) unsafe fn copy_bytes_raw(dst: *mut u8, src: *const u8, len: usize) -> bool {
    if len == 0 {
        return false;
    }
    let faulted: u64;
    // SAFETY: forwarded from this function's contract. `stac` opens the SMAP window and `clac`
    // closes it on either path. The exception-table entry registers the `rep movsb` PC as the
    // fault site and the recovery label as the resume PC; the #PF handler patches `frame.rip`
    // to that label on match.
    unsafe {
        core::arch::asm!(
            "stac",
            // `rep movsb` runs forward only with DF=0. SFMASK clears DF on syscall entry, but
            // other contexts reaching this primitive may not, and a backwards copy corrupts memory.
            "cld",
            // Fault PC: the CPU leaves RIP here on a user-side #PF.
            "2:",
            "rep movsb",
            "clac",
            "xor eax, eax",
            "jmp 3f",
            // Recovery PC: the saved RFLAGS still has AC=1 (the fault was inside the `stac`
            // window), so `clac` first, then signal failure with rax=1.
            "4:",
            "clac",
            "mov eax, 1",
            "3:",
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

/// Host-test stand-in for the asm [`copy_bytes_raw`] (same contract, never faults).
#[cfg(test)]
pub(crate) unsafe fn copy_bytes_raw(dst: *mut u8, src: *const u8, len: usize) -> bool {
    // SAFETY: forwarded from this function's contract; the host
    // never has user memory, so this only runs against ordinary
    // in-process buffers.
    unsafe { core::ptr::copy_nonoverlapping(src, dst, len) };
    false
}

/// Outcome of a byte-by-byte NUL-terminated user copy.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CstrCopyOutcome {
    /// NUL terminator found within `max_len` bytes; the `usize` is the bytes written to `dst`,
    /// including the NUL.
    Ok(usize),
    /// A `#PF` occurred on a user-side load; `dst` may hold a partial copy.
    Fault,
    /// `dst` filled to `max_len` without a NUL byte.
    NoTerminator,
}

/// Byte-by-byte copy of a NUL-terminated user string, stopping at the first NUL or after
/// `max_len` bytes.
///
/// # Safety
/// Same contract as [`copy_bytes_raw`]: `src` points into user
/// memory, `dst` provides `max_len` writable kernel bytes, SMAP is
/// enabled.
#[cfg(not(test))]
pub(crate) unsafe fn copy_cstr_raw(dst: *mut u8, src: *const u8, max_len: usize) -> CstrCopyOutcome {
    let result_code: u64;
    let count: u64;
    // SAFETY: forwarded from this function's contract. The asm uses
    // `lodsb` for the user-side load (the registered fault site) and
    // `stosb` for the kernel-side store; rax/al holds each loaded
    // byte; rdx counts bytes written.
    unsafe {
        core::arch::asm!(
            "stac",
            // `lodsb`/`stosb` run forward only with DF=0 (see `copy_bytes_raw`).
            "cld",
            "xor rdx, rdx",
            // Fault PC: only `lodsb` reads user memory, so a #PF in this block can only come
            // from it.
            "20:",
            "lodsb",
            "stosb",
            "inc rdx",
            "test al, al",
            "jz 22f",                  // found NUL
            "cmp rdx, rcx",
            "jb 20b",                  // continue (rdx < max_len)
            "clac",
            "mov eax, 2",
            "jmp 23f",
            "22:",
            "clac",
            "xor eax, eax",
            "jmp 23f",
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
    /// One-shot **per-thread** flag forcing the next [`copy_cstr_raw`] host-stub call on the same
    /// thread to report [`CstrCopyOutcome::Fault`]. Per-thread so parallel tests do not consume
    /// each other's flag.
    pub(crate) static FAIL_NEXT_CSTR_COPY: core::cell::Cell<bool> =
        const { core::cell::Cell::new(false) };
}

/// Host-test stand-in for the asm [`copy_cstr_raw`], with a forced-fault hook.
#[cfg(test)]
pub(crate) unsafe fn copy_cstr_raw(dst: *mut u8, src: *const u8, max_len: usize) -> CstrCopyOutcome {
    if FAIL_NEXT_CSTR_COPY.with(|f| f.replace(false)) {
        return CstrCopyOutcome::Fault;
    }
    // SAFETY: forwarded; host tests use ordinary in-process buffers.
    let src_slice = unsafe { core::slice::from_raw_parts(src, max_len) };
    // SAFETY: as above.
    let dst_slice = unsafe { core::slice::from_raw_parts_mut(dst, max_len) };
    for i in 0..max_len {
        dst_slice[i] = src_slice[i];
        if src_slice[i] == 0 {
            return CstrCopyOutcome::Ok(i + 1);
        }
    }
    CstrCopyOutcome::NoTerminator
}

/// The x86_64 [`ArchUserAccess`](crate::arch::user_access::ArchUserAccess) implementation,
/// forwarding to the copy primitives above.
pub struct X86UserAccess;

impl crate::arch::user_access::ArchUserAccess for X86UserAccess {
    unsafe fn copy_bytes(dst: *mut u8, src: *const u8, len: usize) -> bool {
        // SAFETY: forwarded verbatim to `copy_bytes_raw`'s contract.
        unsafe { copy_bytes_raw(dst, src, len) }
    }

    unsafe fn copy_cstr(dst: *mut u8, src: *const u8, max_len: usize) -> CstrCopyOutcome {
        // SAFETY: forwarded verbatim to `copy_cstr_raw`'s contract.
        unsafe { copy_cstr_raw(dst, src, max_len) }
    }
}
