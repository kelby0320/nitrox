//! User memory access primitives — slice 1: the exception table.
//!
//! User-memory copies (slice 2) bracket each potentially-faulting
//! instruction with a registered `(fault_pc, recovery_pc)` pair so the
//! page-fault handler can resume the copy at a recovery PC instead of
//! tearing down the kernel. Each pair is emitted as a 16-byte entry into
//! the `.user_access_table` section by the copy primitive's inline asm;
//! the linker brackets the section with `__start_user_access_table` and
//! `__stop_user_access_table`. [`lookup_recovery`] walks the bracketed
//! slice and returns the matching recovery PC, if any.
//!
//! ## Why absolute u64 entries
//!
//! Linux uses 32-bit relative offsets to be KASLR-friendly. Nitrox has
//! no KASLR (and isn't planning any in Phase 1), so absolute 64-bit PCs
//! are simpler and the per-entry cost (16 vs 8 bytes) is negligible at
//! the entry counts we expect. The decision is recorded in the
//! decision log entry for this slice.
//!
//! ## What slice 1 does not include
//!
//! `UserPtr<T>` / `UserMutPtr<T>`, the actual copy primitives, and the
//! SMAP/SMEP discipline arrive in slice 2. This file's only export
//! today is [`lookup_recovery`]; the section is empty until slice 2
//! registers the first entry.

/// One entry in the `.user_access_table` section: a (fault_pc, recovery_pc)
/// pair. The layout is `#[repr(C)]` because slice 2's inline asm emits
/// raw bytes with `.quad` directives — the Rust and asm sides have to
/// agree on the exact byte layout.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) struct ExtableEntry {
    /// Instruction-pointer value the CPU pushes when a fault occurs at
    /// the registered site. The handler matches against this.
    pub fault_pc: u64,
    /// Instruction-pointer value to resume at when the fault matches.
    /// The recovery code is responsible for `clac` (slice 2) and for
    /// signalling failure back to the copy primitive's caller.
    pub recovery_pc: u64,
}

const _: () = assert!(size_of::<ExtableEntry>() == 16);
const _: () = assert!(align_of::<ExtableEntry>() == 8);

/// Search the exception table for an entry matching `fault_pc`. Returns
/// the recovery PC if a match exists, otherwise `None`.
///
/// Linear scan. The table is empty in slice 1; in slice 2 the entry
/// count is one per copy primitive, easily fits in a single cacheline,
/// and the lookup happens only on the rare (faulting) path — a sorted
/// table + binary search would be theoretical work. Revisit if Phase 2
/// ever grows the entry count past a few dozen.
pub fn lookup_recovery(fault_pc: u64) -> Option<u64> {
    // SAFETY: the linker provides matching `__start_user_access_table`
    // and `__stop_user_access_table` symbols, both within the kernel
    // image; the slice is rodata for the lifetime of the kernel. Under
    // `cfg(test)` the function returns an empty slice (no linker
    // symbols in host builds).
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
/// symbols. Under `cfg(test)` returns an empty slice — host builds have
/// no linker section for `.user_access_table`.
///
/// # Safety
/// Only sound when called in a kernel build where the linker has
/// emitted matching `__start_user_access_table` / `__stop_user_access_table`
/// symbols bracketing a contiguous run of [`ExtableEntry`] values. The
/// `cfg(not(test))` path relies on the project's linker script (see
/// `kernel/linker.ld`).
#[cfg(not(test))]
unsafe fn exception_table() -> &'static [ExtableEntry] {
    unsafe extern "C" {
        static __start_user_access_table: ExtableEntry;
        static __stop_user_access_table: ExtableEntry;
    }
    let start: *const ExtableEntry = &raw const __start_user_access_table;
    let stop: *const ExtableEntry = &raw const __stop_user_access_table;
    // SAFETY: the linker emits stop >= start with both pointers within
    // the same rodata section. The byte length is divisible by
    // `size_of::<ExtableEntry>()` because every contributor to
    // `.user_access_table` writes whole entries.
    let len = unsafe { stop.offset_from(start) } as usize;
    // SAFETY: forwarded from this function's contract.
    unsafe { core::slice::from_raw_parts(start, len) }
}

#[cfg(test)]
unsafe fn exception_table() -> &'static [ExtableEntry] {
    &[]
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
        // Should never occur in practice — every emitted entry has a
        // distinct fault_pc — but a defensive check that the lookup is
        // deterministic if it ever does.
        let table = [
            ExtableEntry { fault_pc: 0x1000, recovery_pc: 0xAAAA },
            ExtableEntry { fault_pc: 0x1000, recovery_pc: 0xBBBB },
        ];
        assert_eq!(lookup_recovery_in(&table, 0x1000), Some(0xAAAA));
    }

    #[test]
    fn host_build_sees_empty_table() {
        // The `cfg(test)` path of `exception_table()` returns `&[]`, so
        // the public `lookup_recovery` always misses in host tests.
        assert_eq!(lookup_recovery(0), None);
        assert_eq!(lookup_recovery(0xDEAD_BEEF), None);
    }

    #[test]
    fn entry_layout_matches_assembly_contract() {
        // Slice 2 will emit entries via inline `asm!` with
        // `.quad fault_pc; .quad recovery_pc`. Lock the layout in so
        // those `.quad` writes can't drift away from the Rust struct.
        assert_eq!(size_of::<ExtableEntry>(), 16);
        assert_eq!(align_of::<ExtableEntry>(), 8);
        assert_eq!(core::mem::offset_of!(ExtableEntry, fault_pc), 0);
        assert_eq!(core::mem::offset_of!(ExtableEntry, recovery_pc), 8);
    }
}
