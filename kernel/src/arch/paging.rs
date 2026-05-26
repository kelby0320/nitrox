//! Architecture-neutral paging contract.
//!
//! [`ArchPaging`] is the first cross-architecture *trait* in the kernel's
//! arch layer — `gdt`, `idt`, `regs`, and `serial` are cfg-gated modules
//! of free functions, not traits. Paging earns a trait because aarch64's
//! translation-table format genuinely differs from x86_64's, and the
//! virtual-memory manager (a later item of this slice) is written
//! against this trait rather than against x86 page-table entries.
//!
//! The active architecture's implementation is re-exported from
//! `crate::arch` as `Paging` (see `kernel/src/arch/mod.rs`).

use crate::mm::{PhysAddr, VirtAddr};

/// Permission and caching attributes requested for a mapping.
///
/// Architecture-neutral: the x86_64 implementation translates these to
/// page-table-entry bits, and a future aarch64 implementation would
/// translate them to its AP/UXN/PXN/attr-index encoding. The default —
/// [`PageFlags::empty`] — is a read-only, kernel-only, executable
/// mapping; each flag relaxes or restricts one axis from there.
///
/// Hand-rolled bitflags: the kernel uses no `bitflags` crate (see
/// `kernel/CLAUDE.md`).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct PageFlags(u32);

impl PageFlags {
    /// The page may be written. Without it the mapping is read-only.
    pub const WRITABLE: PageFlags = PageFlags(1 << 0);
    /// The page is reachable from ring 3. Without it it is kernel-only.
    pub const USER: PageFlags = PageFlags(1 << 1);
    /// Instruction fetches from the page fault — set this for data pages.
    pub const NO_EXECUTE: PageFlags = PageFlags(1 << 2);
    /// The mapping survives a page-table-root reload (a global page).
    pub const GLOBAL: PageFlags = PageFlags(1 << 3);
    /// Caching is disabled for the page — for MMIO and the framebuffer.
    pub const NO_CACHE: PageFlags = PageFlags(1 << 4);
    /// Writes go straight through the cache instead of being written back.
    pub const WRITE_THROUGH: PageFlags = PageFlags(1 << 5);

    /// No flags: a read-only, kernel-only, executable mapping.
    pub const fn empty() -> Self {
        PageFlags(0)
    }

    /// `true` if every flag set in `other` is also set in `self`.
    pub const fn contains(self, other: PageFlags) -> bool {
        (self.0 & other.0) == other.0
    }

    /// The union of two flag sets.
    pub const fn union(self, other: PageFlags) -> Self {
        PageFlags(self.0 | other.0)
    }

    /// The raw bit pattern, for tests and debugging.
    pub const fn bits(self) -> u32 {
        self.0
    }
}

impl core::ops::BitOr for PageFlags {
    type Output = PageFlags;

    fn bitor(self, rhs: PageFlags) -> PageFlags {
        self.union(rhs)
    }
}

/// Why [`ArchPaging::map_page`] could not install a mapping.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MapError {
    /// A leaf entry already maps the virtual address. This layer never
    /// silently replaces a mapping — the caller must unmap it first.
    AlreadyMapped,
    /// An intermediate page table had to be allocated and the physical
    /// frame allocator was out of memory.
    OutOfMemory,
    /// The virtual or physical address was not 4 KiB aligned, or the
    /// virtual address was not canonical.
    Misaligned,
}

/// Why [`ArchPaging::unmap_page`] could not remove a mapping.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum UnmapError {
    /// No leaf entry — or a missing intermediate table — covers the
    /// virtual address.
    NotMapped,
    /// The virtual address was not 4 KiB aligned, or not canonical.
    Misaligned,
}

/// Architecture page-table operations.
///
/// Every method is `unsafe`: they install, remove, or switch hardware
/// address translations and mutate live MMU state the running kernel
/// depends on. The implementation for the active architecture is
/// re-exported as `crate::arch::Paging`.
///
/// Neither [`map_page`](ArchPaging::map_page) nor
/// [`unmap_page`](ArchPaging::unmap_page) flushes the TLB; the caller
/// issues [`flush_tlb_page`](ArchPaging::flush_tlb_page) — or
/// [`flush_tlb_all`](ArchPaging::flush_tlb_all) — once it has finished a
/// batch of changes. This keeps the map/unmap paths free of privileged
/// instructions (so they are host-testable) and lets a bulk mapper
/// amortise one flush over many entries.
pub trait ArchPaging {
    /// Map the 4 KiB page at `virt` to the physical frame at `phys`, with
    /// `flags`, in the page-table tree rooted at `root`. Intermediate
    /// tables are allocated from the physical frame allocator as needed.
    ///
    /// Installs the entry but does **not** flush the TLB. Returns
    /// [`MapError::AlreadyMapped`] rather than replacing an existing leaf.
    ///
    /// # Safety
    /// - `root` must be the physical base of a valid top-level page
    ///   table, reachable through the higher-half direct map, and owned
    ///   by the caller for the duration of the call.
    /// - `phys` must be a real frame the caller owns. Installing a second
    ///   writable mapping of an already-mapped frame aliases it; the
    ///   caller is responsible for the consequences.
    /// - Mapping an address in the kernel range with the wrong `flags`,
    ///   or over live kernel state, can corrupt the running kernel.
    unsafe fn map_page(
        root: PhysAddr,
        virt: VirtAddr,
        phys: PhysAddr,
        flags: PageFlags,
    ) -> Result<(), MapError>;

    /// Remove the 4 KiB leaf mapping for `virt` from the tree rooted at
    /// `root`, returning the physical frame it referenced so the caller
    /// can reclaim it.
    ///
    /// Does **not** free intermediate page tables that become empty, and
    /// does **not** flush the TLB.
    ///
    /// # Safety
    /// `root` must be the physical base of a valid top-level page table,
    /// reachable through the higher-half direct map and owned by the
    /// caller. Unmapping a page the kernel is still using faults later.
    unsafe fn unmap_page(root: PhysAddr, virt: VirtAddr) -> Result<PhysAddr, UnmapError>;

    /// Invalidate the TLB entry for the page containing `virt` on the
    /// current CPU.
    ///
    /// # Safety
    /// Issues a ring-0-only instruction. The caller should already have
    /// updated the page tables so the invalidation reflects a real change.
    unsafe fn flush_tlb_page(virt: VirtAddr);

    /// Invalidate every non-global TLB entry on the current CPU by
    /// reloading the page-table root with its current value. Entries for
    /// pages marked [`PageFlags::GLOBAL`] survive.
    ///
    /// # Safety
    /// Ring-0 only. Reloads the active root with the value it already
    /// holds, so the active address space is unchanged.
    unsafe fn flush_tlb_all();

    /// Switch the active address space by loading `root` as the
    /// page-table root of the current CPU.
    ///
    /// # Safety
    /// `root` must be the physical base of a fully-formed top-level page
    /// table that maps, at minimum, all currently-executing kernel code,
    /// the current stack, and the higher-half direct map. Loading an
    /// incomplete table triple-faults the CPU instantly.
    unsafe fn set_page_table(root: PhysAddr);
}
