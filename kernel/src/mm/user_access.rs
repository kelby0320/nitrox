//! User memory access — arch-neutral half.
//!
//! The single sanctioned interface for reading or writing user memory
//! from kernel code. This file owns:
//!
//! - The opaque [`UserPtr<T>`] / [`UserMutPtr<T>`] pointer types and
//!   their validating constructors.
//! - The [`UserAccessError`] enum.
//! - The five public copy primitives (`copy_from_user`,
//!   `copy_to_user`, `copy_slice_from_user`, `copy_slice_to_user`,
//!   `copy_cstr_from_user`).
//! - The exception-table lookup walked by the `#PF` handler.
//!
//! The arch-specific half — the inline-asm raw byte copy under
//! SMAP/PAN and the `.user_access_table` entry emission — lives in
//! `kernel/src/arch/<arch>/user_access.rs` and is reached through
//! [`crate::arch::user_access`].
//!
//! ## Discipline rules
//!
//! 1. **Opaque pointer types.** `UserPtr<T>` / `UserMutPtr<T>` have
//!    no `Deref`, no `read` / `write` methods, and no way to recover
//!    the raw `u64` from outside this module. The kernel may not
//!    access user memory by any other path; see the project-level
//!    rule in `kernel/CLAUDE.md`.
//! 2. **SMAP/PAN discipline.** The protection is enabled at boot
//!    ([`crate::arch::ensure_smap_smep`] on x86_64). The instructions
//!    that bracket each user access live only inside the arch
//!    primitives in `arch::user_access`, never as Rust-visible
//!    wrappers.
//! 3. **Fault recovery.** Each raw arch primitive registers a
//!    `(fault_pc, recovery_pc)` pair in the `.user_access_table`
//!    section. On a `#PF` whose RIP matches a registered fault site,
//!    the [`#PF` handler](crate::arch::idt) patches RIP to the
//!    recovery PC, which closes the protection window and signals
//!    failure. A user-side fault turns into
//!    [`UserAccessError::Fault`], not a kernel halt.
//!
//! ## Why absolute u64 entries
//!
//! Linux uses 32-bit relative offsets to be KASLR-friendly. Nitrox
//! has no KASLR (and isn't planning any in Phase 1), so absolute
//! 64-bit PCs are simpler and the per-entry cost (16 vs 8 bytes) is
//! negligible at the entry counts we expect. Decision recorded in
//! the slice-1 decision log entry.

use core::marker::PhantomData;
use core::mem::{MaybeUninit, align_of, size_of};

use crate::arch::abi::USER_VIRT_END;
use crate::arch::user_access::{CstrCopyOutcome, copy_bytes_raw, copy_cstr_raw};

/// One entry in the `.user_access_table` section: a (fault_pc,
/// recovery_pc) pair. Inline asm in the copy primitives emits the
/// entries with `.quad` directives — Rust and asm have to agree on
/// the exact byte layout, hence `#[repr(C)]`.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) struct ExtableEntry {
    /// Instruction-pointer value the CPU pushes when a fault occurs
    /// at the registered site. The handler matches against this.
    pub fault_pc: u64,
    /// Instruction-pointer value to resume at when the fault matches.
    /// The recovery code is responsible for `clac` and for signalling
    /// failure back to the copy primitive's caller.
    pub recovery_pc: u64,
}

const _: () = assert!(size_of::<ExtableEntry>() == 16);
const _: () = assert!(align_of::<ExtableEntry>() == 8);

/// Search the exception table for an entry matching `fault_pc`.
/// Returns the recovery PC if a match exists, otherwise `None`.
///
/// Linear scan; the table holds one entry per copy primitive (five
/// today), easily fits in a single cacheline, and the lookup happens
/// only on the rare (faulting) path. Revisit if Phase 2 ever grows
/// the entry count past a few dozen.
pub fn lookup_recovery(fault_pc: u64) -> Option<u64> {
    // SAFETY: the linker provides matching `__start_user_access_table`
    // and `__stop_user_access_table` symbols, both within the kernel
    // image; the slice is rodata for the lifetime of the kernel.
    // Under `cfg(test)` the function returns an empty slice (no
    // linker symbols in host builds).
    let table = unsafe { exception_table() };
    lookup_recovery_in(table, fault_pc)
}

/// Pure lookup against an explicit table — extracted so host tests can
/// inject their own entries without needing the linker symbols.
fn lookup_recovery_in(table: &[ExtableEntry], fault_pc: u64) -> Option<u64> {
    for entry in table {
        if entry.fault_pc == fault_pc {
            return Some(entry.recovery_pc);
        }
    }
    None
}

/// The exception-table slice between the linker-provided start/stop
/// symbols. Under `cfg(test)` returns an empty slice — host builds
/// have no linker section for `.user_access_table`.
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

// --- Public types -------------------------------------------------------

/// Why a user-memory access could not be completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserAccessError {
    /// The address (or address + length) falls outside the user half
    /// `[0, USER_VIRT_END)`, or the length arithmetic overflows.
    BadAddress,
    /// The address is not aligned for the access type.
    Misaligned,
    /// A page fault occurred during the copy. The user's page may be
    /// unmapped, write-protected (on a `to_user` copy), or otherwise
    /// inaccessible. The copy is undone for the caller (no kernel-side
    /// bytes were committed past the point of partial completion).
    Fault,
    /// [`copy_cstr_from_user`] filled the destination buffer without
    /// finding a NUL byte. The caller can grow the buffer and retry.
    NoTerminator,
}

/// A read-only pointer into user memory. Opaque: the held address is
/// invisible outside this module. Constructed from a `u64` (typically
/// a syscall argument) via [`UserPtr::new`], which validates that the
/// address is in the user half and aligned for `T`.
///
/// The type parameter `T` is a tag; nothing reads a `T` out of a
/// `UserPtr<T>` except the copy primitives below.
#[repr(transparent)]
#[derive(Copy, Clone, Debug)]
pub struct UserPtr<T> {
    addr: u64,
    _phantom: PhantomData<*const T>,
}

/// A read-write pointer into user memory. Twin of [`UserPtr<T>`]
/// tagged for the writer side; identical validation rules.
#[repr(transparent)]
#[derive(Copy, Clone, Debug)]
pub struct UserMutPtr<T> {
    addr: u64,
    _phantom: PhantomData<*mut T>,
}

impl<T> UserPtr<T> {
    /// Construct a `UserPtr<T>` from a raw `u64` address. Returns
    /// `Err` if the address is outside the user half, or not aligned
    /// for `T`.
    pub fn new(addr: u64) -> Result<Self, UserAccessError> {
        validate_user_addr::<T>(addr)?;
        Ok(Self { addr, _phantom: PhantomData })
    }

    /// The raw u64 address. Crate-internal only — nothing outside
    /// this module should reach for a user address.
    pub(crate) fn as_u64(self) -> u64 {
        self.addr
    }
}

impl<T> UserMutPtr<T> {
    /// Construct a `UserMutPtr<T>` from a raw `u64` address. Returns
    /// `Err` if the address is outside the user half, or not aligned
    /// for `T`.
    pub fn new(addr: u64) -> Result<Self, UserAccessError> {
        validate_user_addr::<T>(addr)?;
        Ok(Self { addr, _phantom: PhantomData })
    }

    /// The raw u64 address. Crate-internal only.
    pub(crate) fn as_u64(self) -> u64 {
        self.addr
    }
}

fn validate_user_addr<T>(addr: u64) -> Result<(), UserAccessError> {
    if addr >= USER_VIRT_END {
        return Err(UserAccessError::BadAddress);
    }
    let align = align_of::<T>() as u64;
    if align > 1 && addr % align != 0 {
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

// --- Copy primitives ----------------------------------------------------

/// Copy a `T` out of user memory by value.
///
/// On success, returns the freshly-read value. On fault — the user's
/// page is unmapped, the address is bad, etc. — returns
/// [`UserAccessError::Fault`] without committing kernel-side state.
///
/// For zero-sized `T` this is a no-op that yields the type's only
/// inhabitant without touching the user pointer.
pub fn copy_from_user<T: Copy>(src: UserPtr<T>) -> Result<T, UserAccessError> {
    let addr = src.as_u64();
    let n = size_of::<T>();
    if n == 0 {
        // SAFETY: `T` is zero-sized, so there is only one possible
        // value and no bytes need be initialised.
        return Ok(unsafe { MaybeUninit::<T>::uninit().assume_init() });
    }
    validate_user_range(addr, n as u64)?;
    let mut out = MaybeUninit::<T>::uninit();
    // SAFETY: `out` is properly-aligned local storage of exactly `n`
    // bytes; `addr` was validated user-half and aligned-for-T; the
    // copy goes through `arch::user_access::copy_bytes_raw`, which
    // opens the SMAP window and registers an exception-table entry
    // for the read.
    let faulted = unsafe {
        copy_bytes_raw(out.as_mut_ptr().cast::<u8>(), addr as *const u8, n)
    };
    if faulted {
        return Err(UserAccessError::Fault);
    }
    // SAFETY: `copy_bytes_raw` returning `false` means every byte
    // landed; `out` is fully initialised as a `T`.
    Ok(unsafe { out.assume_init() })
}

/// Copy a `T` from kernel memory into user memory by value.
///
/// On success, the bytes of `src` are at the user address. On fault —
/// the user page is unmapped, read-only, etc. — returns
/// [`UserAccessError::Fault`]. Partial writes may have completed
/// before the fault (the user observed some prefix of the bytes);
/// the kernel-side `src` is untouched.
///
/// For zero-sized `T` this is a no-op.
pub fn copy_to_user<T: Copy>(
    dst: UserMutPtr<T>,
    src: &T,
) -> Result<(), UserAccessError> {
    let addr = dst.as_u64();
    let n = size_of::<T>();
    if n == 0 {
        return Ok(());
    }
    validate_user_range(addr, n as u64)?;
    let src_ptr = (src as *const T).cast::<u8>();
    // SAFETY: `src_ptr` is a valid `n`-byte read source (it is
    // `src`'s own backing storage); `addr` was validated user-half +
    // aligned-for-T. `copy_bytes_raw` handles SMAP/exception-table.
    let faulted = unsafe { copy_bytes_raw(addr as *mut u8, src_ptr, n) };
    if faulted { Err(UserAccessError::Fault) } else { Ok(()) }
}

/// Copy `dst.len()` bytes from user memory into `dst`.
///
/// Byte-granular: `src` and `dst` do not need to be aligned beyond
/// the byte boundary they already satisfy. Empty `dst` is a no-op.
pub fn copy_slice_from_user(
    dst: &mut [u8],
    src: UserPtr<u8>,
) -> Result<(), UserAccessError> {
    let addr = src.as_u64();
    let n = dst.len();
    validate_user_range(addr, n as u64)?;
    if n == 0 {
        return Ok(());
    }
    // SAFETY: `addr` was validated user-half + `n` bytes; `dst` is
    // exactly `n` writable bytes of caller storage.
    let faulted = unsafe { copy_bytes_raw(dst.as_mut_ptr(), addr as *const u8, n) };
    if faulted { Err(UserAccessError::Fault) } else { Ok(()) }
}

/// Copy `src.len()` bytes from `src` into user memory at `dst`.
pub fn copy_slice_to_user(
    dst: UserMutPtr<u8>,
    src: &[u8],
) -> Result<(), UserAccessError> {
    let addr = dst.as_u64();
    let n = src.len();
    validate_user_range(addr, n as u64)?;
    if n == 0 {
        return Ok(());
    }
    // SAFETY: `addr` was validated user-half + `n` bytes; `src` is
    // exactly `n` readable bytes of caller storage.
    let faulted = unsafe { copy_bytes_raw(addr as *mut u8, src.as_ptr(), n) };
    if faulted { Err(UserAccessError::Fault) } else { Ok(()) }
}

/// Copy a NUL-terminated string from user memory into `dst`. Stops at
/// the first NUL byte (which is included in the returned slice) or
/// when `dst` is full.
///
/// Returns:
/// - `Ok(&dst[..k])` where the `k`th byte is the NUL terminator;
/// - `Err(NoTerminator)` if `dst` filled without finding a NUL —
///   the caller may grow the buffer and retry;
/// - `Err(Fault)` / `Err(BadAddress)` per the normal rules.
pub fn copy_cstr_from_user<'a>(
    dst: &'a mut [u8],
    src: UserPtr<u8>,
) -> Result<&'a [u8], UserAccessError> {
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
    let outcome = unsafe { copy_cstr_raw(dst.as_mut_ptr(), addr as *const u8, max) };
    match outcome {
        CstrCopyOutcome::Ok(count) => Ok(&dst[..count]),
        CstrCopyOutcome::Fault => Err(UserAccessError::Fault),
        CstrCopyOutcome::NoTerminator => Err(UserAccessError::NoTerminator),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Exception-table lookup (unchanged from slice 1) ---

    #[test]
    fn empty_table_returns_none() {
        let table: [ExtableEntry; 0] = [];
        assert_eq!(lookup_recovery_in(&table, 0), None);
        assert_eq!(lookup_recovery_in(&table, 0xDEAD_BEEF), None);
    }

    #[test]
    fn single_entry_hit_and_miss() {
        let table = [ExtableEntry {
            fault_pc: 0x1000,
            recovery_pc: 0x2000,
        }];
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
        // Inline asm emits entries with `.quad fault_pc; .quad
        // recovery_pc`. Lock down the layout so those writes can't
        // drift away from the struct.
        assert_eq!(size_of::<ExtableEntry>(), 16);
        assert_eq!(align_of::<ExtableEntry>(), 8);
        assert_eq!(core::mem::offset_of!(ExtableEntry, fault_pc), 0);
        assert_eq!(core::mem::offset_of!(ExtableEntry, recovery_pc), 8);
    }

    // --- UserPtr / UserMutPtr validation ---

    #[test]
    fn user_ptr_rejects_kernel_half_addr() {
        assert!(matches!(
            UserPtr::<u8>::new(USER_VIRT_END),
            Err(UserAccessError::BadAddress),
        ));
        assert!(matches!(
            UserPtr::<u8>::new(0xFFFF_8000_0000_0000),
            Err(UserAccessError::BadAddress),
        ));
        assert!(matches!(
            UserMutPtr::<u8>::new(USER_VIRT_END),
            Err(UserAccessError::BadAddress),
        ));
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
        // u32 has align 4; addresses with bits 0..1 set are misaligned.
        assert!(matches!(
            UserPtr::<u32>::new(1),
            Err(UserAccessError::Misaligned),
        ));
        assert!(matches!(
            UserPtr::<u32>::new(2),
            Err(UserAccessError::Misaligned),
        ));
        // Aligned addresses pass.
        assert!(UserPtr::<u32>::new(0).is_ok());
        assert!(UserPtr::<u32>::new(4).is_ok());
        assert!(UserPtr::<u32>::new(0x1000).is_ok());
    }

    #[test]
    fn user_ptr_u8_accepts_any_byte_aligned_addr() {
        // u8 has align 1; every user-half address is aligned.
        for addr in [0u64, 1, 2, 3, 0x1234, 0x1235, USER_VIRT_END - 1] {
            assert!(UserPtr::<u8>::new(addr).is_ok(), "addr {:#x}", addr);
        }
    }

    #[repr(C, align(16))]
    struct Aligned16(#[allow(dead_code)] u128);

    #[test]
    fn user_ptr_respects_larger_alignment() {
        // Aligned16 has align 16.
        assert!(matches!(
            UserPtr::<Aligned16>::new(8),
            Err(UserAccessError::Misaligned),
        ));
        assert!(UserPtr::<Aligned16>::new(16).is_ok());
        assert!(UserPtr::<Aligned16>::new(0x1000).is_ok());
    }

    // --- validate_user_range ---

    #[test]
    fn range_within_user_half_ok() {
        assert!(validate_user_range(0, 1).is_ok());
        assert!(validate_user_range(0, USER_VIRT_END).is_ok());
        assert!(validate_user_range(USER_VIRT_END - 8, 8).is_ok());
    }

    #[test]
    fn range_crossing_user_boundary_rejected() {
        // Even one byte past USER_VIRT_END is rejected.
        assert_eq!(
            validate_user_range(USER_VIRT_END - 8, 9),
            Err(UserAccessError::BadAddress),
        );
        assert_eq!(
            validate_user_range(0, USER_VIRT_END + 1),
            Err(UserAccessError::BadAddress),
        );
    }

    #[test]
    fn range_overflow_rejected() {
        assert_eq!(
            validate_user_range(u64::MAX - 4, 8),
            Err(UserAccessError::BadAddress),
        );
    }

    #[test]
    fn zero_length_range_is_vacuously_ok() {
        // A zero-length range accesses no bytes, so it satisfies the
        // "every byte is in the user half" property vacuously. The
        // public copy primitives layer per-pointer validation on top
        // via `UserPtr::new`, which does reject `addr >= USER_VIRT_END`.
        assert!(validate_user_range(0, 0).is_ok());
        assert!(validate_user_range(USER_VIRT_END, 0).is_ok());
    }

    // --- Copy primitives against the host memcpy stub ---

    #[test]
    fn copy_slice_from_user_rejects_kernel_half() {
        let mut buf = [0u8; 8];
        // Construct a UserPtr via direct addr; only the bounds check
        // can trip on a 1-byte read from `USER_VIRT_END - 4` + 8.
        let src = UserPtr::<u8>::new(USER_VIRT_END - 4).expect("low addr ok");
        assert_eq!(
            copy_slice_from_user(&mut buf, src),
            Err(UserAccessError::BadAddress),
        );
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
        assert_eq!(
            copy_cstr_from_user(&mut buf, src),
            Err(UserAccessError::NoTerminator),
        );
    }

    // The copy primitives' *success-path* host tests use the host
    // memcpy stub; they verify only the wrapper plumbing (range
    // check → raw → bytes land at dst). Their target-side equivalents
    // exercise SMAP and the exception table — those will land with
    // the first syscall consumer in a later slice.

    #[test]
    fn copy_slice_host_stub_copies_bytes() {
        let src_data: [u8; 5] = [1, 2, 3, 4, 5];
        // The host test treats the src address as an ordinary kernel
        // pointer. `UserPtr::new` would reject `src_data.as_ptr() as
        // u64` because that pointer is above USER_VIRT_END on real
        // 64-bit Linux. Bypass via the unsafe `pub(crate)` field by
        // constructing the struct directly within this module.
        let src = UserPtr::<u8> {
            addr: src_data.as_ptr() as u64,
            _phantom: PhantomData,
        };
        let mut dst = [0u8; 5];
        // Validation will reject this if the host address happens to
        // be above USER_VIRT_END. Skip the assertion in that case —
        // the host stub's job is only to verify wrapper plumbing,
        // and stack pointers on Linux are typically in the user
        // half (below 0x7FFF_FFFF_FFFF) which is also below our
        // USER_VIRT_END (0x8000_0000_0000).
        if src.addr < USER_VIRT_END
            && src.addr.checked_add(dst.len() as u64).map_or(true, |e| e <= USER_VIRT_END)
        {
            assert_eq!(copy_slice_from_user(&mut dst, src), Ok(()));
            assert_eq!(dst, src_data);
        }
    }

    #[test]
    fn copy_cstr_host_stub_stops_at_nul() {
        let src_data: [u8; 8] = *b"hello\0XX";
        let src = UserPtr::<u8> {
            addr: src_data.as_ptr() as u64,
            _phantom: PhantomData,
        };
        let mut dst = [0u8; 16];
        if src.addr < USER_VIRT_END
            && src.addr.checked_add(dst.len() as u64).map_or(true, |e| e <= USER_VIRT_END)
        {
            let out = copy_cstr_from_user(&mut dst, src).expect("cstr ok");
            assert_eq!(out, b"hello\0");
            assert_eq!(out.len(), 6);
        }
    }

    #[test]
    fn copy_cstr_maps_fault_outcome_to_fault_error() {
        // The host stub never #PFs on its own, so force the step that the
        // real asm body reaches on a user-side fault and confirm
        // `copy_cstr_from_user` maps `CstrCopyOutcome::Fault` to
        // `UserAccessError::Fault`.
        let src_data: [u8; 8] = *b"hello\0XX";
        let src = UserPtr::<u8> {
            addr: src_data.as_ptr() as u64,
            _phantom: PhantomData,
        };
        let mut dst = [0u8; 16];
        if src.addr < USER_VIRT_END
            && src.addr.checked_add(dst.len() as u64).map_or(true, |e| e <= USER_VIRT_END)
        {
            crate::arch::user_access::FAIL_NEXT_CSTR_COPY.with(|f| f.set(true));
            assert_eq!(
                copy_cstr_from_user(&mut dst, src),
                Err(UserAccessError::Fault),
            );
        }
    }

    #[test]
    fn copy_cstr_host_stub_no_terminator() {
        let src_data: [u8; 8] = *b"abcdefgh"; // no NUL
        let src = UserPtr::<u8> {
            addr: src_data.as_ptr() as u64,
            _phantom: PhantomData,
        };
        let mut dst = [0u8; 4];
        if src.addr < USER_VIRT_END
            && src.addr.checked_add(dst.len() as u64).map_or(true, |e| e <= USER_VIRT_END)
        {
            assert_eq!(
                copy_cstr_from_user(&mut dst, src),
                Err(UserAccessError::NoTerminator),
            );
        }
    }
}
