//! Virtual memory manager — per-process address-space layer.
//!
//! The third layer of the kernel's memory design (see the table at the
//! top of `docs/architecture/memory-management.md`). The VMM owns the
//! address-space view of memory: virtual address ranges, their
//! protection, what backs them, and the red-black tree that stores them
//! per process. It does not touch hardware page tables directly; that
//! goes through [`arch::Paging`](crate::arch::Paging).
//!
//! This file lands the address-spaces-and-paging slice incrementally.
//! Today it holds the leaf data types ([`VAddrRange`], [`Protection`],
//! [`MappingKind`], [`Vma`]). The intrusive red-black-tree node, the
//! tree operations, and the `AddressSpace` owner land in the following
//! sub-items.

use crate::mm::{PAGE_SIZE, VirtAddr};

/// A half-open range of virtual addresses, `[start, end)`.
///
/// Both endpoints are 4 KiB aligned and `end > start`. The range is the
/// unit a [`Vma`] covers, but the type is dumber than a `Vma`: it carries
/// no protection or backing information, so the VMM can pass a
/// `VAddrRange` to the tree's overlap queries without manufacturing a
/// fake `Vma`.
///
/// Half-open intervals were chosen for the same reason most Unix VMM
/// code uses them: `len()` is `end - start` with no off-by-one, and two
/// "adjacent" ranges (one ends where the next begins) compose by
/// endpoint equality rather than `+ 1`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct VAddrRange {
    start: VirtAddr,
    end: VirtAddr,
}

impl VAddrRange {
    /// Construct a range covering `[start, end)`.
    ///
    /// Returns `None` if either endpoint is not 4 KiB aligned, or if
    /// `end <= start`. An empty range has no meaning at the VMA layer:
    /// every `Vma` covers at least one page.
    pub const fn new(start: VirtAddr, end: VirtAddr) -> Option<Self> {
        if !start.is_page_aligned() || !end.is_page_aligned() {
            return None;
        }
        if end.as_u64() <= start.as_u64() {
            return None;
        }
        Some(Self { start, end })
    }

    pub const fn start(self) -> VirtAddr {
        self.start
    }

    pub const fn end(self) -> VirtAddr {
        self.end
    }

    /// Length of the range in bytes; always a non-zero multiple of
    /// [`PAGE_SIZE`].
    pub const fn len(self) -> u64 {
        self.end.as_u64() - self.start.as_u64()
    }

    /// Number of 4 KiB pages the range covers.
    pub const fn pages(self) -> u64 {
        self.len() / (PAGE_SIZE as u64)
    }

    /// `true` if `addr` lies within `[start, end)`.
    pub const fn contains(self, addr: VirtAddr) -> bool {
        addr.as_u64() >= self.start.as_u64() && addr.as_u64() < self.end.as_u64()
    }

    /// `true` if `self` and `other` share at least one byte. Adjacent
    /// (touching) ranges do **not** overlap under the half-open
    /// convention.
    pub const fn overlaps(self, other: VAddrRange) -> bool {
        self.start.as_u64() < other.end.as_u64() && other.start.as_u64() < self.end.as_u64()
    }

    /// The intersection of `self` and `other`, or `None` if they are
    /// disjoint — including the merely adjacent case.
    pub fn intersect(self, other: VAddrRange) -> Option<VAddrRange> {
        let start = core::cmp::max(self.start, other.start);
        let end = core::cmp::min(self.end, other.end);
        if end <= start {
            None
        } else {
            Some(VAddrRange { start, end })
        }
    }
}

/// VMA-level access policy: who may reach the mapping and what may they
/// do with it.
///
/// `Protection` is a narrower abstraction than
/// [`PageFlags`](crate::arch::paging::PageFlags). Not every PTE flag is
/// meaningful at the VMA layer: a VMA never carries `GLOBAL` (a user
/// mapping cannot be global; a kernel-image mapping is global by
/// construction at install time, not via a per-VMA decision), and
/// cache-attribute bits are per-mapping policy decided by the code that
/// installs the PTE (driver MMIO, framebuffer), not a property of the
/// address range. The VMM translates `Protection` to `PageFlags` when
/// populating a leaf.
///
/// "Readable" is not a separate flag: a `Vma` existing in the tree
/// implies the range is readable. x86_64 has no separate read bit
/// (present implies readable); we surface that uniformly. The
/// `mprotect`-style distinction between "no access" and "read-only" is
/// expressed by removing the VMA entirely, not by clearing a flag.
///
/// Hand-rolled bitflags: the kernel uses no `bitflags` crate (see
/// `kernel/CLAUDE.md`).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Protection(u8);

impl Protection {
    /// The mapping may be written. Without it, the mapping is read-only.
    pub const WRITE: Protection = Protection(1 << 0);
    /// The mapping may be executed. Without it, instruction fetches fault.
    pub const EXEC: Protection = Protection(1 << 1);
    /// The mapping is reachable from ring 3. Without it, kernel-only.
    pub const USER: Protection = Protection(1 << 2);

    /// No flags: kernel-only, read-only, non-executable — the safe
    /// default. Contrast
    /// [`PageFlags::empty`](crate::arch::paging::PageFlags::empty), which
    /// is *executable* by default because `NO_EXECUTE` is opt-in at the
    /// hardware level. The VMM presents the safer logical default and
    /// translates to the inverted PTE encoding at install time.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// `true` if every flag set in `other` is also set in `self`.
    pub const fn contains(self, other: Protection) -> bool {
        (self.0 & other.0) == other.0
    }

    /// The union of two flag sets.
    pub const fn union(self, other: Protection) -> Self {
        Protection(self.0 | other.0)
    }

    /// The raw bit pattern, for tests and debugging.
    pub const fn bits(self) -> u8 {
        self.0
    }
}

impl core::ops::BitOr for Protection {
    type Output = Protection;

    fn bitor(self, rhs: Protection) -> Protection {
        self.union(rhs)
    }
}

/// What backs a [`Vma`]'s pages.
///
/// Only `Anonymous` is defined today. `FileBacked(Handle)` lands with
/// the page-cache and fs-server integration in Phase 2; `Device(PhysAddr)`
/// lands with the driver MMIO mapper. The enum is open to extension:
/// adding a variant only touches the call sites that need to act on the
/// new backing kind.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MappingKind {
    /// Zero-initialised on first touch; backed by anonymous physical
    /// frames allocated lazily through the buddy.
    Anonymous,
}

/// A virtual memory area: a contiguous virtual address range with
/// uniform protection and a single backing kind.
///
/// The smallest unit the VMM tracks. An address space is a tree of
/// non-overlapping `Vma`s (the tree machinery lands in the next
/// sub-item). `mprotect`-style operations that change protection on
/// only a sub-range, and merges of adjacent compatible VMAs, are tree
/// operations rather than field mutations: a `Vma` is conceptually
/// immutable once installed.
#[derive(Clone, Debug)]
pub struct Vma {
    pub range: VAddrRange,
    pub prot: Protection,
    pub mapping: MappingKind,
}

impl Vma {
    pub const fn new(range: VAddrRange, prot: Protection, mapping: MappingKind) -> Self {
        Self {
            range,
            prot,
            mapping,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: u64 = PAGE_SIZE as u64;

    fn va(v: u64) -> VirtAddr {
        VirtAddr::new(v)
    }

    fn range(start: u64, end: u64) -> VAddrRange {
        VAddrRange::new(va(start), va(end)).expect("test range must be valid")
    }

    #[test]
    fn vrange_rejects_misaligned_endpoints() {
        assert!(VAddrRange::new(va(0x1), va(PAGE)).is_none());
        assert!(VAddrRange::new(va(0), va(PAGE + 1)).is_none());
        assert!(VAddrRange::new(va(0xFFF), va(PAGE * 2)).is_none());
    }

    #[test]
    fn vrange_rejects_empty_and_inverted() {
        assert!(VAddrRange::new(va(PAGE), va(PAGE)).is_none());
        assert!(VAddrRange::new(va(PAGE * 2), va(PAGE)).is_none());
    }

    #[test]
    fn vrange_len_and_pages() {
        let r = range(0, PAGE * 4);
        assert_eq!(r.len(), PAGE * 4);
        assert_eq!(r.pages(), 4);
    }

    #[test]
    fn vrange_contains_is_half_open() {
        let r = range(PAGE, PAGE * 3);
        assert!(r.contains(va(PAGE)));
        assert!(r.contains(va(PAGE * 2)));
        assert!(r.contains(va(PAGE * 3 - 1)));
        assert!(!r.contains(va(PAGE * 3)));
        assert!(!r.contains(va(PAGE - 1)));
    }

    #[test]
    fn vrange_overlaps_disjoint_and_adjacent() {
        let a = range(0, PAGE);
        let b = range(PAGE * 2, PAGE * 3);
        assert!(!a.overlaps(b));
        assert!(!b.overlaps(a));

        // Adjacent (touching at PAGE) — half-open, so no overlap.
        let c = range(0, PAGE);
        let d = range(PAGE, PAGE * 2);
        assert!(!c.overlaps(d));
        assert!(!d.overlaps(c));
    }

    #[test]
    fn vrange_overlaps_partial_and_nested() {
        let a = range(0, PAGE * 3);
        let b = range(PAGE * 2, PAGE * 4);
        assert!(a.overlaps(b));
        assert!(b.overlaps(a));

        let outer = range(0, PAGE * 4);
        let inner = range(PAGE, PAGE * 2);
        assert!(outer.overlaps(inner));
        assert!(inner.overlaps(outer));
    }

    #[test]
    fn vrange_intersect_disjoint_is_none() {
        let a = range(0, PAGE);
        let b = range(PAGE * 2, PAGE * 3);
        assert_eq!(a.intersect(b), None);

        // Adjacent counts as disjoint under half-open semantics.
        let c = range(0, PAGE);
        let d = range(PAGE, PAGE * 2);
        assert_eq!(c.intersect(d), None);
    }

    #[test]
    fn vrange_intersect_partial_and_nested() {
        let a = range(0, PAGE * 3);
        let b = range(PAGE * 2, PAGE * 4);
        assert_eq!(a.intersect(b), Some(range(PAGE * 2, PAGE * 3)));
        assert_eq!(b.intersect(a), Some(range(PAGE * 2, PAGE * 3)));

        let outer = range(0, PAGE * 4);
        let inner = range(PAGE, PAGE * 2);
        assert_eq!(outer.intersect(inner), Some(inner));
        assert_eq!(inner.intersect(outer), Some(inner));
    }

    #[test]
    fn protection_empty_is_zero() {
        assert_eq!(Protection::empty().bits(), 0);
        assert!(!Protection::empty().contains(Protection::WRITE));
        assert!(!Protection::empty().contains(Protection::EXEC));
        assert!(!Protection::empty().contains(Protection::USER));
    }

    #[test]
    fn protection_union_and_contains() {
        let rw_user = Protection::WRITE | Protection::USER;
        assert!(rw_user.contains(Protection::WRITE));
        assert!(rw_user.contains(Protection::USER));
        assert!(!rw_user.contains(Protection::EXEC));
        // Self-containment.
        assert!(rw_user.contains(rw_user));
        // Empty is contained in everything.
        assert!(rw_user.contains(Protection::empty()));
    }
}
