//! x86_64's answer to [`crate::arch::memory_types`]: the variable-range MTRRs, the default type,
//! and the page-attribute table (PAT), read and reported.
//!
//! **Read-only, for now.** Phase 5 Part G's first piece is the measurement — what the firmware
//! set, and what a full-screen write therefore costs — because the fix (mapping the framebuffer
//! write-combining) should be judged against a number rather than against an expectation.
//!
//! **The effective type is the stronger of the range registers and the page table**, which is why
//! the range registers decide the framebuffer's fate today: every user mapping this kernel makes
//! is write-back (`mm::addr_space::protection_to_page_flags`), and write-back under an uncacheable
//! range is uncacheable. The one way a page table can *raise* an uncacheable range is a PAT entry
//! of write-combining, which is what Part G is for.

use super::regs;
use crate::arch::memory_types::{ArchMemoryTypes, MemoryType};

/// `IA32_MTRRCAP`: how many variable ranges the CPU has, and whether fixed ranges exist.
const MSR_MTRRCAP: u32 = 0xFE;
/// `IA32_MTRR_DEF_TYPE`: the type of everything no range covers, plus the enable bits.
const MSR_MTRR_DEF_TYPE: u32 = 0x2FF;
/// `IA32_MTRR_PHYSBASE0`. Base and mask alternate from here, two MSRs per range.
const MSR_MTRR_PHYSBASE0: u32 = 0x200;
/// `IA32_PAT`: eight type entries, selected by a page's PAT/PCD/PWT bits.
const MSR_PAT: u32 = 0x277;

/// `CPUID.01H:EDX` bit 12 — the CPU has memory-type range registers.
const CPUID_EDX_MTRR: u32 = 1 << 12;
/// `CPUID.01H:EDX` bit 16 — the CPU has a page-attribute table.
const CPUID_EDX_PAT: u32 = 1 << 16;

/// `IA32_MTRR_DEF_TYPE` bit 11: the range registers are in effect at all.
const DEF_TYPE_ENABLED: u64 = 1 << 11;
/// `IA32_MTRR_DEF_TYPE` bit 10: the fixed ranges (below 1 MiB) are in effect.
const DEF_TYPE_FIXED_ENABLED: u64 = 1 << 10;
/// `IA32_MTRR_PHYSMASK` bit 11: this range is in use.
const PHYSMASK_VALID: u64 = 1 << 11;
/// The address bits of a base or mask register.
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
/// The fixed ranges cover the first mebibyte, and nothing this kernel asks about lives there.
const FIXED_RANGE_TOP: u64 = 0x10_0000;
/// Ranges are read into a fixed array; the architecture allows at most this many.
const MAX_RANGES: usize = 64;

/// `CR0.CD` — no-fill cache mode, which the vendor's procedure enters before the table changes.
const CR0_CACHE_DISABLE: u64 = 1 << 30;
/// `CR0.NW` — not-write-through. Cleared alongside `CD`, as the procedure requires.
const CR0_NOT_WRITE_THROUGH: u64 = 1 << 29;
/// `CR4.PGE` — global pages, cleared and restored around the change so global TLB entries go too.
const CR4_GLOBAL_PAGES: u64 = 1 << 7;

/// The attribute table this kernel programs, entry 0 first:
/// write-back, write-through, `UC-`, uncacheable, write-protected, **write-combining**,
/// uncacheable, uncacheable.
///
/// **These are the values Limine already leaves**, on the laptop and under QEMU alike, and that is
/// deliberate rather than lazy: mappings made before the kernel takes the table over chose their
/// entries out of it. Two such entries are live — the console's framebuffer mapping selects 5, and
/// every `kvmap` MMIO mapping selects 2 — so a layout that moved either would change what an
/// in-flight mapping means. What owning it buys is that the meanings stop depending on a
/// bootloader's choice; what keeping the values buys is that nothing in flight changes.
const KERNEL_PAT: u64 = 0x0000_0105_0007_0406;

/// x86_64's memory-type configuration.
pub struct X86MemoryTypes;

/// One variable range, as the registers describe it.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
struct Range {
    base: u64,
    mask: u64,
    kind: MemoryType,
}

/// Decode an architectural memory-type byte. `2` and `3` are reserved; `7` is PAT-only (`UC-`,
/// which a page table may weaken and a range register may not name).
fn decode(kind: u8) -> MemoryType {
    match kind {
        0 => MemoryType::Uncacheable,
        1 => MemoryType::WriteCombining,
        4 => MemoryType::WriteThrough,
        5 => MemoryType::WriteProtected,
        6 => MemoryType::WriteBack,
        // `UC-`: a page-attribute entry only. A range register never names it, so `at` can never
        // return it — but `log_configuration` prints the table, where it is the default for two
        // of the eight entries.
        7 => MemoryType::UncacheableWeak,
        other => MemoryType::Other(other),
    }
}

/// The type a set of ranges gives `phys`.
///
/// **The rules are the vendor's, not a simplification of them.** Disabled range registers make
/// every address uncacheable — not "whatever the page table says", which is the reading that
/// would quietly excuse a machine that had turned them off. Where ranges overlap, uncacheable
/// wins over everything, and write-through wins over write-back; any other overlap is undefined
/// by the architecture, so the first match is taken and the caller can see the overlap in the log.
fn type_for(phys: u64, ranges: &[Range], default: MemoryType, enabled: bool) -> MemoryType {
    if !enabled {
        return MemoryType::Uncacheable;
    }
    let mut found: Option<MemoryType> = None;
    for r in ranges {
        if (phys & r.mask) != (r.base & r.mask) {
            continue;
        }
        found = Some(match (found, r.kind) {
            (None, k) => k,
            (Some(MemoryType::Uncacheable), _) | (_, MemoryType::Uncacheable) => {
                MemoryType::Uncacheable
            }
            (Some(MemoryType::WriteBack), MemoryType::WriteThrough)
            | (Some(MemoryType::WriteThrough), MemoryType::WriteBack) => MemoryType::WriteThrough,
            (Some(first), _) => first,
        });
    }
    found.unwrap_or(default)
}

/// Read the variable ranges into `out`, returning the ones in use.
///
/// # Safety
/// Ring 0, on a CPU whose `CPUID` claims range registers.
unsafe fn read_ranges(out: &mut [Range; MAX_RANGES]) -> usize {
    // SAFETY: `IA32_MTRRCAP` is implemented wherever CPUID advertises MTRR, which the caller
    // has checked.
    let cap = unsafe { regs::rdmsr(MSR_MTRRCAP) };
    let count = ((cap & 0xFF) as usize).min(MAX_RANGES);
    let mut used = 0;
    for i in 0..count {
        // SAFETY: the first `VCNT` pairs from `IA32_MTRR_PHYSBASE0` are implemented, and `cap`
        // said how many there are.
        let (base, mask) = unsafe {
            (
                regs::rdmsr(MSR_MTRR_PHYSBASE0 + 2 * i as u32),
                regs::rdmsr(MSR_MTRR_PHYSBASE0 + 2 * i as u32 + 1),
            )
        };
        if mask & PHYSMASK_VALID == 0 {
            continue;
        }
        out[used] =
            Range { base: base & ADDR_MASK, mask: mask & ADDR_MASK, kind: decode((base & 0xFF) as u8) };
        used += 1;
    }
    used
}

/// Print an attribute table, entry by entry.
fn log_table(what: &str, pat: u64) {
    crate::kprintln!(
        "cache policy: {what} 0:{} 1:{} 2:{} 3:{} 4:{} 5:{} 6:{} 7:{}",
        decode((pat & 0xFF) as u8).name(),
        decode((pat >> 8 & 0xFF) as u8).name(),
        decode((pat >> 16 & 0xFF) as u8).name(),
        decode((pat >> 24 & 0xFF) as u8).name(),
        decode((pat >> 32 & 0xFF) as u8).name(),
        decode((pat >> 40 & 0xFF) as u8).name(),
        decode((pat >> 48 & 0xFF) as u8).name(),
        decode((pat >> 56 & 0xFF) as u8).name()
    );
}

impl ArchMemoryTypes for X86MemoryTypes {
    unsafe fn at(phys: u64) -> Option<MemoryType> {
        if regs::cpuid(1, 0).3 & CPUID_EDX_MTRR == 0 {
            return None;
        }
        // SAFETY: CPUID says the range registers exist, so `IA32_MTRR_DEF_TYPE` is implemented.
        let def = unsafe { regs::rdmsr(MSR_MTRR_DEF_TYPE) };
        let enabled = def & DEF_TYPE_ENABLED != 0;
        if phys < FIXED_RANGE_TOP && enabled && def & DEF_TYPE_FIXED_ENABLED != 0 {
            // The fixed registers describe the first mebibyte in 88 sub-ranges. Nothing in this
            // kernel asks about an address down there, so they are not read rather than read
            // wrongly — and saying "unknown" is the honest answer to a question not implemented.
            return None;
        }
        let mut ranges = [Range { base: 0, mask: 0, kind: MemoryType::Uncacheable }; MAX_RANGES];
        // SAFETY: as above.
        let used = unsafe { read_ranges(&mut ranges) };
        Some(type_for(phys, &ranges[..used], decode((def & 0xFF) as u8), enabled))
    }

    unsafe fn of_mapping(virt: u64) -> Option<MemoryType> {
        if regs::cpuid(1, 0).3 & CPUID_EDX_PAT == 0 {
            return None;
        }
        let root = <super::paging::X86Paging as crate::arch::paging::ArchPaging>::active_root();
        // SAFETY: the active root is live and reachable through the HHDM.
        let index = unsafe { super::paging::attribute_index(root, crate::mm::VirtAddr::new(virt)) }?;
        // SAFETY: CPUID advertises the attribute table, so `IA32_PAT` is implemented.
        let pat = unsafe { regs::rdmsr(MSR_PAT) };
        Some(decode((pat >> (8 * index as u32) & 0xFF) as u8))
    }

    unsafe fn install_policy() -> bool {
        if regs::cpuid(1, 0).3 & CPUID_EDX_PAT == 0 {
            return false;
        }
        // SAFETY: CPUID advertises the attribute table, so `IA32_PAT` is implemented.
        let had = unsafe { regs::rdmsr(MSR_PAT) };
        // **The vendor's sequence** (SDM vol. 3, "Programming the PAT"), which exists because the
        // caches may hold lines whose memory type the old table described: enter no-fill mode,
        // write everything back, drop the TLB including its global entries, change the table,
        // write back again, and only then let the caches fill.
        //
        // SAFETY: ring 0 during this CPU's bring-up, per this function's contract. Interrupts are
        // off across the window, so nothing runs on this CPU while its caches are disabled — a
        // handler that touched memory would run at no-fill speed but stay correct; the reason to
        // hold them off is that the window must not be extended by one.
        unsafe {
            let was_on = <super::cpu::X86Cpu as crate::arch::cpu::ArchCpu>::interrupts_disable();
            let cr0 = regs::read_cr0();
            regs::write_cr0((cr0 | CR0_CACHE_DISABLE) & !CR0_NOT_WRITE_THROUGH);
            regs::wbinvd();
            let cr4 = regs::read_cr4();
            regs::write_cr4(cr4 & !CR4_GLOBAL_PAGES);
            regs::write_cr3(regs::read_cr3());
            regs::wrmsr(MSR_PAT, KERNEL_PAT);
            regs::wbinvd();
            regs::write_cr3(regs::read_cr3());
            regs::write_cr4(cr4);
            regs::write_cr0(cr0);
            if was_on {
                <super::cpu::X86Cpu as crate::arch::cpu::ArchCpu>::interrupts_enable();
            }
        }
        if had != KERNEL_PAT {
            // The "before" half of G.1's before-and-after: a table nobody expected is worth
            // printing once, because every mapping made before this call named an entry in it.
            log_table("the bootloader's table was", had);
        }
        had == KERNEL_PAT
    }

    unsafe fn log_configuration() {
        let edx = regs::cpuid(1, 0).3;
        if edx & CPUID_EDX_MTRR == 0 {
            crate::kprintln!("cache policy: no range registers on this CPU");
        } else {
            // SAFETY: CPUID advertises the range registers.
            let (cap, def) =
                unsafe { (regs::rdmsr(MSR_MTRRCAP), regs::rdmsr(MSR_MTRR_DEF_TYPE)) };
            crate::kprintln!(
                "cache policy: {} variable range(s), default {}, ranges {}, fixed {}",
                cap & 0xFF,
                decode((def & 0xFF) as u8).name(),
                if def & DEF_TYPE_ENABLED != 0 { "enabled" } else { "DISABLED (all uncacheable)" },
                if def & DEF_TYPE_FIXED_ENABLED != 0 { "enabled" } else { "disabled" }
            );
            let mut ranges = [Range { base: 0, mask: 0, kind: MemoryType::Uncacheable }; MAX_RANGES];
            // SAFETY: as above.
            let used = unsafe { read_ranges(&mut ranges) };
            for r in &ranges[..used] {
                // The mask's set bits are the address bits that must match, so the range's size
                // is the lowest set bit — the span a `(phys & mask) == (base & mask)` test admits.
                let size = if r.mask == 0 { 0 } else { 1u64 << r.mask.trailing_zeros() };
                crate::kprintln!(
                    "cache policy: range {:#x}..{:#x} {}",
                    r.base,
                    r.base.saturating_add(size),
                    r.kind.name()
                );
            }
        }
        if edx & CPUID_EDX_PAT == 0 {
            crate::kprintln!("cache policy: no page-attribute table on this CPU");
            return;
        }
        // SAFETY: CPUID advertises the page-attribute table, so `IA32_PAT` is implemented.
        let pat = unsafe { regs::rdmsr(MSR_PAT) };
        // Entry `n` is byte `n`; a page selects one with its PAT, PCD and PWT bits. **Two
        // entries are in use today**: a mapping with no cache flags lands on entry 0, and
        // `PageFlags::NO_CACHE` sets `PCD` alone, which selects entry 2 — `UC-`, not plain
        // uncacheable. Every kernel MMIO mapping (`kvmap`, the interrupt router) is entry 2
        // (PR #305 review, finding 2).
        log_table("page attributes", pat);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(base: u64, size: u64, kind: MemoryType) -> Range {
        Range { base, mask: !(size - 1) & ADDR_MASK, kind }
    }

    /// The laptop's shape, as the first boot found it: RAM write-back, and the framebuffer in a
    /// graphics aperture no range covers, so it falls to the default type.
    #[test]
    fn an_address_outside_every_range_takes_the_default() {
        let ranges = [range(0, 0x8000_0000, MemoryType::WriteBack)];
        let fb = 0xa000_0000;
        assert_eq!(
            type_for(fb, &ranges, MemoryType::Uncacheable, true),
            MemoryType::Uncacheable,
            "an uncacheable default is what makes a framebuffer slow"
        );
        assert_eq!(type_for(0x1000, &ranges, MemoryType::Uncacheable, true), MemoryType::WriteBack);
    }

    #[test]
    fn a_range_that_covers_the_address_decides_it() {
        let ranges = [
            range(0, 0x8000_0000, MemoryType::WriteBack),
            range(0xa000_0000, 0x1000_0000, MemoryType::WriteCombining),
        ];
        assert_eq!(
            type_for(0xa000_0000, &ranges, MemoryType::Uncacheable, true),
            MemoryType::WriteCombining
        );
        // The last byte of the range, and the first byte past it.
        assert_eq!(
            type_for(0xafff_f000, &ranges, MemoryType::Uncacheable, true),
            MemoryType::WriteCombining
        );
        assert_eq!(
            type_for(0xb000_0000, &ranges, MemoryType::Uncacheable, true),
            MemoryType::Uncacheable
        );
    }

    /// The architecture's overlap rules, which are not "the last one wins".
    #[test]
    fn overlapping_ranges_follow_the_architectures_precedence() {
        let uc_over_wb = [
            range(0, 0x1_0000_0000, MemoryType::WriteBack),
            range(0xa000_0000, 0x1000_0000, MemoryType::Uncacheable),
        ];
        assert_eq!(
            type_for(0xa000_0000, &uc_over_wb, MemoryType::WriteBack, true),
            MemoryType::Uncacheable,
            "uncacheable wins over everything"
        );
        // And in the other order, since a table walked in a different order must not differ.
        let wb_over_uc = [uc_over_wb[1], uc_over_wb[0]];
        assert_eq!(
            type_for(0xa000_0000, &wb_over_uc, MemoryType::WriteBack, true),
            MemoryType::Uncacheable
        );
        let wt_and_wb = [
            range(0, 0x1_0000_0000, MemoryType::WriteBack),
            range(0xa000_0000, 0x1000_0000, MemoryType::WriteThrough),
        ];
        assert_eq!(
            type_for(0xa000_0000, &wt_and_wb, MemoryType::WriteBack, true),
            MemoryType::WriteThrough,
            "write-through wins over write-back"
        );
    }

    /// Range registers turned off make **everything** uncacheable, whatever the ranges say — the
    /// case a "no range matched, so use the default" reading would report as write-back.
    #[test]
    fn disabled_range_registers_make_everything_uncacheable() {
        let ranges = [range(0, 0x1_0000_0000, MemoryType::WriteBack)];
        assert_eq!(
            type_for(0x1000, &ranges, MemoryType::WriteBack, false),
            MemoryType::Uncacheable
        );
    }

    #[test]
    fn the_architectures_type_bytes_decode_to_their_behaviours() {
        assert_eq!(decode(0), MemoryType::Uncacheable);
        assert_eq!(decode(1), MemoryType::WriteCombining);
        assert_eq!(decode(4), MemoryType::WriteThrough);
        assert_eq!(decode(5), MemoryType::WriteProtected);
        assert_eq!(decode(6), MemoryType::WriteBack);
        // `7` is `UC-`, which only a page-attribute entry may name; `2` and `3` are reserved.
        assert_eq!(decode(7), MemoryType::UncacheableWeak);
        assert_eq!(decode(2), MemoryType::Other(2));
        assert_eq!(decode(3), MemoryType::Other(3));
    }

    /// **The table this kernel programs, spelled out.** A `u64` of packed type bytes is exactly
    /// the kind of constant that is wrong in a way nothing notices: the first version of it had
    /// the entries shifted, and only a boot — which compared it against the bootloader's and said
    /// they DIFFERED — caught that. This is the same check, in a second.
    #[test]
    fn the_kernels_table_is_the_layout_its_doc_claims() {
        let entry = |n: u32| decode((KERNEL_PAT >> (8 * n) & 0xFF) as u8);
        assert_eq!(entry(0), MemoryType::WriteBack, "a mapping with no cache bits");
        assert_eq!(entry(1), MemoryType::WriteThrough);
        assert_eq!(entry(2), MemoryType::UncacheableWeak, "what every kvmap MMIO mapping selects");
        assert_eq!(entry(3), MemoryType::Uncacheable);
        assert_eq!(entry(4), MemoryType::WriteProtected);
        assert_eq!(entry(5), MemoryType::WriteCombining, "what the framebuffer will select");
        assert_eq!(entry(6), MemoryType::Uncacheable);
        assert_eq!(entry(7), MemoryType::Uncacheable);
    }
}
