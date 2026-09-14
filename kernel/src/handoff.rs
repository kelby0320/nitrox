//! What the bootloader handed over, summarised for the log (Phase 5 Part D.1).
//!
//! Every boot says which bootloader loaded it, on what firmware, and what the memory map
//! holds, so that every transcript is a hardware report of the machine it ran on. `main.rs`
//! owns the Limine request statics and prints the lines; the pieces here are the ones worth
//! host-testing — the memory map's tally and the firmware type's name.

use core::fmt;

use crate::limine::{
    FIRMWARE_EFI32, FIRMWARE_EFI64, FIRMWARE_SBI, FIRMWARE_X86_BIOS, MEMMAP_ACPI_NVS,
    MEMMAP_ACPI_RECLAIMABLE, MEMMAP_BAD_MEMORY, MEMMAP_BOOTLOADER_RECLAIMABLE,
    MEMMAP_FRAMEBUFFER, MEMMAP_KERNEL_AND_MODULES, MEMMAP_RESERVED, MEMMAP_RESERVED_MAPPED,
    MEMMAP_USABLE,
};

/// The name of a Limine firmware type.
pub fn firmware_name(kind: u64) -> &'static str {
    match kind {
        FIRMWARE_X86_BIOS => "BIOS",
        FIRMWARE_EFI32 => "UEFI (32-bit)",
        FIRMWARE_EFI64 => "UEFI (64-bit)",
        FIRMWARE_SBI => "SBI",
        _ => "unknown firmware",
    }
}

/// The memory map summarised by kind: how many entries, and how many KiB of each.
///
/// Bytes rather than a count of regions because the question a report answers is "how much",
/// and KiB rather than MiB because the kinds that matter when something is wrong — ACPI and
/// NVS — are often a few hundred KiB, which a MiB column would print as zero.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryTally {
    pub entries: u64,
    pub usable: u64,
    pub bootloader: u64,
    pub kernel_and_modules: u64,
    pub framebuffer: u64,
    pub acpi: u64,
    pub acpi_nvs: u64,
    /// Reserved, and reserved-but-mapped: the same answer to "can the kernel use it".
    pub reserved: u64,
    pub bad: u64,
    /// A kind this kernel has no name for — a newer protocol revision's. Printed only when
    /// non-zero, so it cannot hide.
    pub other: u64,
}

impl MemoryTally {
    /// Count one memory-map entry of `kind` and `length` bytes.
    pub fn add(&mut self, kind: u64, length: u64) {
        self.entries += 1;
        let slot = match kind {
            MEMMAP_USABLE => &mut self.usable,
            MEMMAP_BOOTLOADER_RECLAIMABLE => &mut self.bootloader,
            MEMMAP_KERNEL_AND_MODULES => &mut self.kernel_and_modules,
            MEMMAP_FRAMEBUFFER => &mut self.framebuffer,
            MEMMAP_ACPI_RECLAIMABLE => &mut self.acpi,
            MEMMAP_ACPI_NVS => &mut self.acpi_nvs,
            MEMMAP_RESERVED | MEMMAP_RESERVED_MAPPED => &mut self.reserved,
            MEMMAP_BAD_MEMORY => &mut self.bad,
            _ => &mut self.other,
        };
        *slot = slot.saturating_add(length);
    }
}

impl fmt::Display for MemoryTally {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let k = |bytes: u64| bytes / 1024;
        write!(
            f,
            "{} entries; usable {}K, bootloader {}K, kernel+modules {}K, framebuffer {}K, \
             ACPI {}K, NVS {}K, reserved {}K, bad {}K",
            self.entries,
            k(self.usable),
            k(self.bootloader),
            k(self.kernel_and_modules),
            k(self.framebuffer),
            k(self.acpi),
            k(self.acpi_nvs),
            k(self.reserved),
            k(self.bad),
        )?;
        if self.other != 0 {
            write!(f, ", unknown kinds {}K", k(self.other))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_kind_lands_in_its_own_column() {
        let mut t = MemoryTally::default();
        let kinds = [
            (MEMMAP_USABLE, 1),
            (MEMMAP_RESERVED, 2),
            (MEMMAP_ACPI_RECLAIMABLE, 3),
            (MEMMAP_ACPI_NVS, 4),
            (MEMMAP_BAD_MEMORY, 5),
            (MEMMAP_BOOTLOADER_RECLAIMABLE, 6),
            (MEMMAP_KERNEL_AND_MODULES, 7),
            (MEMMAP_FRAMEBUFFER, 8),
            (MEMMAP_RESERVED_MAPPED, 9),
        ];
        for (kind, kib) in kinds {
            t.add(kind, kib * 1024);
        }
        assert_eq!(
            format!("{t}"),
            "9 entries; usable 1K, bootloader 6K, kernel+modules 7K, framebuffer 8K, \
             ACPI 3K, NVS 4K, reserved 11K, bad 5K"
        );
    }

    #[test]
    fn an_unknown_kind_is_counted_and_shown_rather_than_dropped() {
        let mut t = MemoryTally::default();
        t.add(MEMMAP_USABLE, 4096);
        let quiet = format!("{t}");
        assert!(!quiet.contains("unknown"), "a map with no unknown kind says nothing: {quiet}");
        t.add(0x99, 8192);
        let loud = format!("{t}");
        assert!(loud.starts_with("2 entries;"), "{loud}");
        assert!(loud.ends_with(", unknown kinds 8K"), "{loud}");
    }

    #[test]
    fn same_kind_entries_accumulate() {
        let mut t = MemoryTally::default();
        t.add(MEMMAP_USABLE, 0x9f000);
        t.add(MEMMAP_USABLE, 0x7ee0_0000);
        assert_eq!(t.entries, 2);
        assert_eq!(t.usable, 0x9f000 + 0x7ee0_0000);
    }

    #[test]
    fn firmware_types_have_names_and_an_unknown_one_says_so() {
        assert_eq!(firmware_name(FIRMWARE_EFI64), "UEFI (64-bit)");
        assert_eq!(firmware_name(FIRMWARE_X86_BIOS), "BIOS");
        assert_eq!(firmware_name(42), "unknown firmware");
    }
}
