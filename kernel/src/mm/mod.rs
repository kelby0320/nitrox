//! Memory management.
//!
//! Three-layer design (per `docs/architecture/overview.md` §"Memory
//! management"): the buddy allocator manages physical page frames; a
//! SLUB-inspired slab allocator handles kernel object allocation on top
//! of the buddy; the VMM owns per-process address spaces. This module
//! holds the buddy allocator and the common types both upper layers
//! consume.

pub mod addr_space;
pub mod buddy;
pub mod elf;
pub mod heap;
pub mod kstack;
pub mod kvmap;
pub mod slab;
pub mod user_access;
pub mod vmm;

#[cfg(test)]
pub(crate) mod test_support;

use core::sync::atomic::{AtomicU64, Ordering};

/// Count of anonymous pages faulted in on demand by
/// [`AddressSpace::fault_in`](addr_space::AddressSpace::fault_in) since boot.
/// A diagnostic-only counter (relaxed, lock-free): it makes the otherwise-
/// invisible demand-paging path observable — the boot demo logs it to prove
/// lazily-reserved stacks are faulted in rather than eagerly allocated.
static DEMAND_FAULTS: AtomicU64 = AtomicU64::new(0);

/// Record one successful demand fault-in. Called from `fault_in` on each
/// [`FaultIn::Mapped`](addr_space::FaultIn::Mapped) for an anonymous page.
pub fn record_demand_fault() {
    DEMAND_FAULTS.fetch_add(1, Ordering::Relaxed);
}

/// The number of anonymous pages faulted in on demand since boot.
pub fn demand_fault_count() -> u64 {
    DEMAND_FAULTS.load(Ordering::Relaxed)
}

/// Page size in bytes. The kernel uses 4 KiB pages on x86_64; large pages
/// are an optimisation handled inside the VMM, not a different unit of
/// allocation.
pub const PAGE_SIZE: usize = 4096;

/// `log2(PAGE_SIZE)`. Pre-shifted so frame arithmetic is `addr >> PAGE_SHIFT`.
pub const PAGE_SHIFT: u32 = 12;

/// A physical address. Newtype rather than a bare `u64` so that mixing
/// physical and virtual addresses is a type error.
///
/// Not `#[repr(C)]`: `PhysAddr` is internal to the kernel and never
/// crosses the syscall ABI. The `#[repr(transparent)]` is for layout
/// parity with `u64` only.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct PhysAddr(pub u64);

impl PhysAddr {
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Frame number this address belongs to (i.e. `address >> PAGE_SHIFT`).
    pub const fn frame(self) -> usize {
        (self.0 >> PAGE_SHIFT) as usize
    }

    /// Construct a `PhysAddr` from a frame number.
    pub const fn from_frame(frame: usize) -> Self {
        Self((frame as u64) << PAGE_SHIFT)
    }

    /// `true` if this address sits on a 4 KiB boundary.
    pub const fn is_page_aligned(self) -> bool {
        (self.0 & (PAGE_SIZE as u64 - 1)) == 0
    }
}

/// A virtual (linear) address. Newtype over `u64` for the same reason as
/// [`PhysAddr`]: so a physical address can never be used where a virtual
/// one is meant, and vice versa.
///
/// Not `#[repr(C)]`: `VirtAddr` is internal to the kernel and never
/// crosses the syscall ABI. The `#[repr(transparent)]` is for layout
/// parity with `u64` only.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct VirtAddr(pub u64);

impl VirtAddr {
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// `true` if this address sits on a 4 KiB boundary.
    pub const fn is_page_aligned(self) -> bool {
        (self.0 & (PAGE_SIZE as u64 - 1)) == 0
    }

    /// `true` if this address is canonical for 4-level paging: bits 63:48
    /// must all replicate bit 47. The CPU `#GP`s on a non-canonical
    /// address, so the paging layer rejects them before walking a table.
    pub const fn is_canonical(self) -> bool {
        // Sign-extend bit 47 across the top 16 bits; a canonical address
        // is unchanged by the round trip.
        let sign_extended = ((self.0 << 16) as i64 >> 16) as u64;
        sign_extended == self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virt_page_alignment() {
        assert!(VirtAddr::new(0).is_page_aligned());
        assert!(VirtAddr::new(0x1000).is_page_aligned());
        assert!(!VirtAddr::new(0x1).is_page_aligned());
        assert!(!VirtAddr::new(0xFFF).is_page_aligned());
    }

    #[test]
    fn virt_canonical_low_half() {
        // Bit 47 clear: canonical iff bits 63:48 are also clear.
        assert!(VirtAddr::new(0).is_canonical());
        assert!(VirtAddr::new(0x0000_7FFF_FFFF_F000).is_canonical());
        assert!(!VirtAddr::new(0x0001_0000_0000_0000).is_canonical());
        assert!(!VirtAddr::new(0x0000_8000_0000_0000).is_canonical());
    }

    #[test]
    fn virt_canonical_high_half() {
        // Bit 47 set: canonical iff bits 63:48 are all set — the
        // higher-half kernel range.
        assert!(VirtAddr::new(0xFFFF_8000_0000_0000).is_canonical());
        assert!(VirtAddr::new(0xFFFF_FFFF_8000_0000).is_canonical());
        assert!(!VirtAddr::new(0xFFFE_8000_0000_0000).is_canonical());
    }
}
