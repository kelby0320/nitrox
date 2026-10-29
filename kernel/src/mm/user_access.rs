//! User memory access: the arch-neutral half.
//!
//! The single sanctioned interface for reading or writing user memory from kernel code: the
//! opaque [`UserPtr<T>`] / [`UserMutPtr<T>`], [`UserAccessError`], the five copy primitives and
//! the exception-table lookup the `#PF` handler walks. The raw byte copy under SMAP/PAN lives in
//! `kernel/src/arch/<arch>/user_access.rs` ([`crate::arch::user_access`]).
//!
//! ## Discipline rules
//!
//! 1. **Opaque pointer types.** `UserPtr<T>` / `UserMutPtr<T>` have no `Deref`, no `read` /
//!    `write`, and no way to recover the raw `u64` from outside this module; the kernel may not
//!    access user memory by any other path (`kernel/CLAUDE.md`).
//! 2. **SMAP/PAN discipline.** The protection is enabled at boot
//!    ([`crate::arch::ensure_smap_smep`] on x86_64). The instructions bracketing each user access
//!    live only inside the arch primitives, never as Rust-visible wrappers.
//! 3. **Fault recovery.** Each raw arch primitive registers a `(fault_pc, recovery_pc)` pair in
//!    `.user_access_table`. On a `#PF` whose RIP matches a fault site, the arch `#PF` handler
//!    patches RIP to the recovery PC, which closes the protection window and signals failure: a
//!    user-side fault becomes [`UserAccessError::Fault`], not a kernel halt.
//!
//! ## Why absolute u64 entries
//!
//! Linux uses 32-bit relative offsets to suit KASLR. Nitrox has none, so absolute 64-bit PCs are
//! simpler and the per-entry cost (16 vs 8 bytes) is negligible.

use core::marker::PhantomData;
use core::mem::{align_of, size_of};

use crate::arch::UserAccess;
use crate::arch::abi::USER_VIRT_END;
use crate::arch::user_access::{ArchUserAccess, CstrCopyOutcome};
use crate::libkern::pod::{Pod, bytes_of, bytes_of_mut};

/// One entry in `.user_access_table`: a (fault_pc, recovery_pc) pair. The copy primitives' inline
/// asm emits entries with `.quad`, so Rust and asm must agree on the layout: `#[repr(C)]`.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) struct ExtableEntry {
    /// Instruction pointer on a fault at the registered site.
    pub fault_pc: u64,
    /// Instruction pointer to resume at; the recovery code does the `clac` and signals failure.
    pub recovery_pc: u64,
}

const _: () = assert!(size_of::<ExtableEntry>() == 16);
const _: () = assert!(align_of::<ExtableEntry>() == 8);

/// Search the exception table for `fault_pc`; the recovery PC on a match.
///
/// A linear scan: one entry per copy primitive, and it runs only on the rare faulting path.
pub fn lookup_recovery(fault_pc: u64) -> Option<u64> {
    // SAFETY: the linker provides matching `__start_user_access_table`
    // and `__stop_user_access_table` symbols, both within the kernel
    // image; the slice is rodata for the lifetime of the kernel.
    // Under `cfg(test)` the function returns an empty slice (no
    // linker symbols in host builds).
    let table = unsafe { exception_table() };
    lookup_recovery_in(table, fault_pc)
}

/// Lookup against an explicit table, so host tests can inject entries without linker symbols.
fn lookup_recovery_in(table: &[ExtableEntry], fault_pc: u64) -> Option<u64> {
    for entry in table {
        if entry.fault_pc == fault_pc {
            return Some(entry.recovery_pc);
        }
    }
    None
}

/// The exception-table slice between the linker's start/stop symbols; empty under `cfg(test)`.
///
/// # Safety
/// Only sound when called in a kernel build where the linker has
/// emitted matching `__start_user_access_table` / `__stop_user_access_table`
/// symbols bracketing a contiguous run of [`ExtableEntry`] values.
#[cfg(not(test))]
unsafe fn exception_table() -> &'static [ExtableEntry] {
    unsafe extern "C" {
        static __start_user_access_table: ExtableEntry;
        static __stop_user_access_table: ExtableEntry;
    }
    let start: *const ExtableEntry = &raw const __start_user_access_table;
    let stop: *const ExtableEntry = &raw const __stop_user_access_table;
    // SAFETY: the linker emits stop >= start with both pointers
    // within the same rodata section. The byte length is divisible
    // by `size_of::<ExtableEntry>()` because every contributor to
    // `.user_access_table` writes whole entries.
    let len = unsafe { stop.offset_from(start) } as usize;
    // SAFETY: forwarded from this function's contract.
    unsafe { core::slice::from_raw_parts(start, len) }
}

#[cfg(test)]
unsafe fn exception_table() -> &'static [ExtableEntry] {
    &[]
}

/// Why a user-memory access could not be completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserAccessError {
    /// The address (or address + length) is outside `[0, USER_VIRT_END)`, or the length overflows.
    BadAddress,
    /// The address is not aligned for the access type.
    Misaligned,
    /// A page fault during the copy: the user page may be unmapped, write-protected (on a
    /// `to_user` copy) or otherwise inaccessible.
    Fault,
    /// [`copy_cstr_from_user`] filled the buffer without finding a NUL; the caller may grow it.
    NoTerminator,
}

/// A read-only pointer into user memory. Opaque: the address is invisible outside this module.
/// [`UserPtr::new`] validates that it is in the user half and aligned for `T`. `T` is only a
/// tag: nothing but the copy primitives reads a `T` out of it.
#[repr(transparent)]
#[derive(Copy, Clone, Debug)]
pub struct UserPtr<T> {
    addr: u64,
    _phantom: PhantomData<*const T>,
}

/// A read-write pointer into user memory: the writer-side twin of [`UserPtr<T>`], same validation.
#[repr(transparent)]
#[derive(Copy, Clone, Debug)]
pub struct UserMutPtr<T> {
    addr: u64,
    _phantom: PhantomData<*mut T>,
}

impl<T> UserPtr<T> {
    /// Construct from a raw `u64`; `Err` if outside the user half or misaligned for `T`.
    pub fn new(addr: u64) -> Result<Self, UserAccessError> {
        validate_user_addr::<T>(addr)?;
        Ok(Self { addr, _phantom: PhantomData })
    }

    /// The raw address, crate-internal.
    pub(crate) fn as_u64(&self) -> u64 {
        self.addr
    }
}

impl<T> UserMutPtr<T> {
    /// Construct from a raw `u64`; `Err` if outside the user half or misaligned for `T`.
    pub fn new(addr: u64) -> Result<Self, UserAccessError> {
        validate_user_addr::<T>(addr)?;
        Ok(Self { addr, _phantom: PhantomData })
    }

    /// The raw address, crate-internal.
    pub(crate) fn as_u64(&self) -> u64 {
        self.addr
    }
}

fn validate_user_addr<T>(addr: u64) -> Result<(), UserAccessError> {
    if addr >= USER_VIRT_END {
        return Err(UserAccessError::BadAddress);
    }
    let align = align_of::<T>() as u64;
    if align > 1 && !addr.is_multiple_of(align) {
        return Err(UserAccessError::Misaligned);
    }
    Ok(())
}

fn validate_user_range(addr: u64, len: u64) -> Result<(), UserAccessError> {
    let end = addr.checked_add(len).ok_or(UserAccessError::BadAddress)?;
    if end > USER_VIRT_END {
        return Err(UserAccessError::BadAddress);
    }
    Ok(())
}

/// Copy a `T` out of user memory; [`UserAccessError::Fault`] commits no kernel-side state. A
/// zero-sized `T` yields its only value without touching the pointer. `T` is plain data, so the
/// user's bytes, whatever they are, are a `T`.
pub fn copy_from_user<T: Pod>(src: UserPtr<T>) -> Result<T, UserAccessError> {
    let addr = src.as_u64();
    let mut out = T::zeroed();
    let n = size_of::<T>();
    if n == 0 {
        return Ok(out);
    }
    validate_user_range(addr, n as u64)?;
    // SAFETY: `out` is `n` writable bytes; `addr` was validated user-half and aligned-for-T; the
    // copy goes through `arch::user_access::copy_bytes_raw`, which opens the SMAP window and
    // registers an exception-table entry for the read.
    let faulted = unsafe { UserAccess::copy_bytes(bytes_of_mut(&mut out).as_mut_ptr(), addr as *const u8, n) };
    if faulted { Err(UserAccessError::Fault) } else { Ok(out) }
}

/// Copy a `T` from kernel memory into user memory. [`UserAccessError::Fault`] on a fault, after
/// which a prefix of the bytes may already be visible to the user; `src` is untouched. A no-op
/// for zero-sized `T`. `T` is plain data, so no padding carries kernel bytes out.
pub fn copy_to_user<T: Pod>(dst: UserMutPtr<T>, src: &T) -> Result<(), UserAccessError> {
    let addr = dst.as_u64();
    let n = size_of::<T>();
    if n == 0 {
        return Ok(());
    }
    validate_user_range(addr, n as u64)?;
    // SAFETY: `src`'s bytes are a valid `n`-byte read source; `addr` was validated user-half +
    // aligned-for-T. `copy_bytes_raw` handles SMAP/exception-table.
    let faulted = unsafe { UserAccess::copy_bytes(addr as *mut u8, bytes_of(src).as_ptr(), n) };
    if faulted { Err(UserAccessError::Fault) } else { Ok(()) }
}

/// Copy `dst.len()` bytes from user memory into `dst`. Byte-granular; an empty `dst` is a no-op.
pub fn copy_slice_from_user(dst: &mut [u8], src: UserPtr<u8>) -> Result<(), UserAccessError> {
    let addr = src.as_u64();
    let n = dst.len();
    validate_user_range(addr, n as u64)?;
    if n == 0 {
        return Ok(());
    }
    // SAFETY: `addr` was validated user-half + `n` bytes; `dst` is
    // exactly `n` writable bytes of caller storage.
    let faulted = unsafe { UserAccess::copy_bytes(dst.as_mut_ptr(), addr as *const u8, n) };
    if faulted { Err(UserAccessError::Fault) } else { Ok(()) }
}

/// Copy `src.len()` bytes from `src` into user memory at `dst`.
pub fn copy_slice_to_user(dst: UserMutPtr<u8>, src: &[u8]) -> Result<(), UserAccessError> {
    let addr = dst.as_u64();
    let n = src.len();
    validate_user_range(addr, n as u64)?;
    if n == 0 {
        return Ok(());
    }
    // SAFETY: `addr` was validated user-half + `n` bytes; `src` is
    // exactly `n` readable bytes of caller storage.
    let faulted = unsafe { UserAccess::copy_bytes(addr as *mut u8, src.as_ptr(), n) };
    if faulted { Err(UserAccessError::Fault) } else { Ok(()) }
}

/// Copy a NUL-terminated string from user memory into `dst`, stopping after the first NUL or when
/// `dst` is full.
///
/// - `Ok(&dst[..k])`: the `k`th byte is the NUL;
/// - `Err(NoTerminator)`: `dst` filled without a NUL, so the caller may grow it and retry;
/// - `Err(Fault)` / `Err(BadAddress)` per the normal rules.
pub fn copy_cstr_from_user(dst: &mut [u8], src: UserPtr<u8>) -> Result<&[u8], UserAccessError> {
    let addr = src.as_u64();
    let max = dst.len();
    if max == 0 {
        return Err(UserAccessError::NoTerminator);
    }
    validate_user_range(addr, max as u64)?;
    // SAFETY: `addr` is in `[0, USER_VIRT_END - max)`; `dst` provides
    // `max` writable bytes. The arch raw primitive walks user memory
    // one byte at a time, registering only the user-side load as a
    // fault site.
    let outcome = unsafe { UserAccess::copy_cstr(dst, addr as *const u8) };
    match outcome {
        CstrCopyOutcome::Ok(count) => Ok(&dst[..count]),
        CstrCopyOutcome::Fault => Err(UserAccessError::Fault),
        CstrCopyOutcome::NoTerminator => Err(UserAccessError::NoTerminator),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_table_returns_none() {
        let table: [ExtableEntry; 0] = [];
        assert_eq!(lookup_recovery_in(&table, 0), None);
        assert_eq!(lookup_recovery_in(&table, 0xDEAD_BEEF), None);
    }

    #[test]
    fn single_entry_hit_and_miss() {
        let table = [ExtableEntry { fault_pc: 0x1000, recovery_pc: 0x2000 }];
        assert_eq!(lookup_recovery_in(&table, 0x1000), Some(0x2000));
        assert_eq!(lookup_recovery_in(&table, 0x0FFF), None);
        assert_eq!(lookup_recovery_in(&table, 0x1001), None);
    }

    #[test]
    fn multiple_entries_match_correct_recovery() {
        let table = [
            ExtableEntry { fault_pc: 0x1000, recovery_pc: 0xA000 },
            ExtableEntry { fault_pc: 0x2000, recovery_pc: 0xB000 },
            ExtableEntry { fault_pc: 0x3000, recovery_pc: 0xC000 },
        ];
        assert_eq!(lookup_recovery_in(&table, 0x1000), Some(0xA000));
        assert_eq!(lookup_recovery_in(&table, 0x2000), Some(0xB000));
        assert_eq!(lookup_recovery_in(&table, 0x3000), Some(0xC000));
        assert_eq!(lookup_recovery_in(&table, 0x4000), None);
    }

    #[test]
    fn first_match_wins_on_duplicate_fault_pc() {
        let table = [
            ExtableEntry { fault_pc: 0x1000, recovery_pc: 0xAAAA },
            ExtableEntry { fault_pc: 0x1000, recovery_pc: 0xBBBB },
        ];
        assert_eq!(lookup_recovery_in(&table, 0x1000), Some(0xAAAA));
    }

    #[test]
    fn host_build_sees_empty_table() {
        assert_eq!(lookup_recovery(0), None);
        assert_eq!(lookup_recovery(0xDEAD_BEEF), None);
    }

    #[test]
    fn entry_layout_matches_assembly_contract() {
        assert_eq!(size_of::<ExtableEntry>(), 16);
        assert_eq!(align_of::<ExtableEntry>(), 8);
        assert_eq!(core::mem::offset_of!(ExtableEntry, fault_pc), 0);
        assert_eq!(core::mem::offset_of!(ExtableEntry, recovery_pc), 8);
    }

    #[test]
    fn user_ptr_rejects_kernel_half_addr() {
        assert!(matches!(UserPtr::<u8>::new(USER_VIRT_END), Err(UserAccessError::BadAddress),));
        assert!(matches!(UserPtr::<u8>::new(0xFFFF_8000_0000_0000), Err(UserAccessError::BadAddress),));
        assert!(matches!(UserMutPtr::<u8>::new(USER_VIRT_END), Err(UserAccessError::BadAddress),));
    }

    #[test]
    fn user_ptr_accepts_user_half_addr() {
        assert!(UserPtr::<u8>::new(0).is_ok());
        assert!(UserPtr::<u8>::new(0x1234).is_ok());
        assert!(UserPtr::<u8>::new(USER_VIRT_END - 1).is_ok());
        assert!(UserMutPtr::<u8>::new(0).is_ok());
    }

    #[test]
    fn user_ptr_rejects_misaligned_addr_for_t() {
        assert!(matches!(UserPtr::<u32>::new(1), Err(UserAccessError::Misaligned),));
        assert!(matches!(UserPtr::<u32>::new(2), Err(UserAccessError::Misaligned),));
        assert!(UserPtr::<u32>::new(0).is_ok());
        assert!(UserPtr::<u32>::new(4).is_ok());
        assert!(UserPtr::<u32>::new(0x1000).is_ok());
    }

    #[test]
    fn user_ptr_u8_accepts_any_byte_aligned_addr() {
        for addr in [0u64, 1, 2, 3, 0x1234, 0x1235, USER_VIRT_END - 1] {
            assert!(UserPtr::<u8>::new(addr).is_ok(), "addr {:#x}", addr);
        }
    }

    #[repr(C, align(16))]
    struct Aligned16(#[allow(dead_code, reason = "there for its size and alignment")] u128);

    #[test]
    fn user_ptr_respects_larger_alignment() {
        assert!(matches!(UserPtr::<Aligned16>::new(8), Err(UserAccessError::Misaligned),));
        assert!(UserPtr::<Aligned16>::new(16).is_ok());
        assert!(UserPtr::<Aligned16>::new(0x1000).is_ok());
    }

    #[test]
    fn range_within_user_half_ok() {
        assert!(validate_user_range(0, 1).is_ok());
        assert!(validate_user_range(0, USER_VIRT_END).is_ok());
        assert!(validate_user_range(USER_VIRT_END - 8, 8).is_ok());
    }

    #[test]
    fn range_crossing_user_boundary_rejected() {
        assert_eq!(validate_user_range(USER_VIRT_END - 8, 9), Err(UserAccessError::BadAddress),);
        assert_eq!(validate_user_range(0, USER_VIRT_END + 1), Err(UserAccessError::BadAddress),);
    }

    #[test]
    fn range_overflow_rejected() {
        assert_eq!(validate_user_range(u64::MAX - 4, 8), Err(UserAccessError::BadAddress),);
    }

    #[test]
    fn zero_length_range_is_vacuously_ok() {
        // A zero-length range accesses no byte, so it passes vacuously.
        assert!(validate_user_range(0, 0).is_ok());
        assert!(validate_user_range(USER_VIRT_END, 0).is_ok());
    }

    #[test]
    fn copy_slice_from_user_rejects_kernel_half() {
        let mut buf = [0u8; 8];
        let src = UserPtr::<u8>::new(USER_VIRT_END - 4).expect("low addr ok");
        assert_eq!(copy_slice_from_user(&mut buf, src), Err(UserAccessError::BadAddress),);
    }

    #[test]
    fn copy_slice_from_user_zero_length_is_noop() {
        let mut buf: [u8; 0] = [];
        let src = UserPtr::<u8>::new(0).unwrap();
        assert_eq!(copy_slice_from_user(&mut buf, src), Ok(()));
    }

    #[test]
    fn copy_slice_to_user_zero_length_is_noop() {
        let src: [u8; 0] = [];
        let dst = UserMutPtr::<u8>::new(0).unwrap();
        assert_eq!(copy_slice_to_user(dst, &src), Ok(()));
    }

    #[test]
    fn copy_cstr_rejects_empty_dst() {
        let mut buf: [u8; 0] = [];
        let src = UserPtr::<u8>::new(0).unwrap();
        assert_eq!(copy_cstr_from_user(&mut buf, src), Err(UserAccessError::NoTerminator),);
    }

    #[test]
    fn copy_slice_host_stub_copies_bytes() {
        let src_data: [u8; 5] = [1, 2, 3, 4, 5];
        // Treat the src address as a kernel pointer: `UserPtr::new` would reject it.
        let src = UserPtr::<u8> { addr: src_data.as_ptr() as u64, _phantom: PhantomData };
        let mut dst = [0u8; 5];
        if src.addr < USER_VIRT_END && src.addr.checked_add(dst.len() as u64).is_none_or(|e| e <= USER_VIRT_END) {
            assert_eq!(copy_slice_from_user(&mut dst, src), Ok(()));
            assert_eq!(dst, src_data);
        }
    }

    #[test]
    fn copy_cstr_host_stub_stops_at_nul() {
        let src_data: [u8; 8] = *b"hello\0XX";
        let src = UserPtr::<u8> { addr: src_data.as_ptr() as u64, _phantom: PhantomData };
        let mut dst = [0u8; 16];
        if src.addr < USER_VIRT_END && src.addr.checked_add(dst.len() as u64).is_none_or(|e| e <= USER_VIRT_END) {
            let out = copy_cstr_from_user(&mut dst, src).expect("cstr ok");
            assert_eq!(out, b"hello\0");
            assert_eq!(out.len(), 6);
        }
    }

    /// **A string copy into an empty destination writes nothing**: the routine copies a byte
    /// before it compares the count, so an empty destination is refused before it.
    #[test]
    fn a_string_copy_into_an_empty_destination_writes_nothing() {
        let src = *b"x\0";
        let mut buf = [0xAAu8; 2];
        // SAFETY: host buffers stand in for user memory.
        let outcome = unsafe { UserAccess::copy_cstr(&mut buf[..0], src.as_ptr()) };
        assert_eq!((outcome, buf), (CstrCopyOutcome::NoTerminator, [0xAA, 0xAA]));
    }

    #[test]
    fn copy_cstr_maps_fault_outcome_to_fault_error() {
        // Force the user-side fault outcome that the host stub never produces.
        let src_data: [u8; 8] = *b"hello\0XX";
        let src = UserPtr::<u8> { addr: src_data.as_ptr() as u64, _phantom: PhantomData };
        let mut dst = [0u8; 16];
        if src.addr < USER_VIRT_END && src.addr.checked_add(dst.len() as u64).is_none_or(|e| e <= USER_VIRT_END) {
            crate::arch::user_access::FAIL_NEXT_CSTR_COPY.with(|f| f.set(true));
            assert_eq!(copy_cstr_from_user(&mut dst, src), Err(UserAccessError::Fault),);
        }
    }

    #[test]
    fn copy_cstr_host_stub_no_terminator() {
        let src_data: [u8; 8] = *b"abcdefgh"; // no NUL
        let src = UserPtr::<u8> { addr: src_data.as_ptr() as u64, _phantom: PhantomData };
        let mut dst = [0u8; 4];
        if src.addr < USER_VIRT_END && src.addr.checked_add(dst.len() as u64).is_none_or(|e| e <= USER_VIRT_END) {
            assert_eq!(copy_cstr_from_user(&mut dst, src), Err(UserAccessError::NoTerminator),);
        }
    }
}
