//! Memory management.
//!
//! Three layers (`docs/architecture/overview.md` §"Memory management"): the buddy allocator
//! manages physical frames; a SLUB-inspired slab allocator serves kernel objects on top of it; the
//! VMM owns per-process address spaces. This module holds the types they share.

pub mod addr_space;
pub mod buddy;
pub mod dma;
pub mod elf;
pub mod heap;
pub mod kstack;
pub mod kvmap;
pub mod slab;
pub mod user_access;
pub mod vmm;

pub use dma::DmaBuffer;

#[cfg(test)]
pub(crate) mod test_support;

use core::sync::atomic::{AtomicU64, Ordering};

/// Anonymous pages faulted in on demand since boot. Diagnostic only (relaxed, lock-free).
static DEMAND_FAULTS: AtomicU64 = AtomicU64::new(0);

/// Record one successful anonymous demand fault-in.
pub fn record_demand_fault() {
    DEMAND_FAULTS.fetch_add(1, Ordering::Relaxed);
}

/// The number of anonymous pages faulted in on demand since boot.
pub fn demand_fault_count() -> u64 {
    DEMAND_FAULTS.load(Ordering::Relaxed)
}

/// Page size in bytes: 4 KiB on x86_64; large pages are an optimisation inside the VMM.
pub const PAGE_SIZE: usize = 4096;

/// How a mapping of some memory should be cached.
///
/// A property of the memory, carried by every mapping of it: two mappings of one device aperture
/// that disagree on caching are an aliasing hazard (a write-back mapping of a framebuffer is
/// overridden by the firmware's uncacheable range, and is far slower than a write-combining one).
///
/// Only the two cases the system has.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Caching {
    /// Ordinary memory: cached, and the page table says nothing about it. Every anonymous and
    /// file-backed mapping.
    #[default]
    Normal,
    /// Writes gathered into bursts, reads uncached — what a framebuffer wants, and the one case
    /// where a page table can *raise* a range the firmware calls uncacheable.
    WriteCombining,
}

impl Caching {
    /// A name for a log line.
    pub fn name(self) -> &'static str {
        match self {
            Caching::Normal => "ordinary",
            Caching::WriteCombining => "write-combining",
        }
    }
}

/// `log2(PAGE_SIZE)`. Pre-shifted so frame arithmetic is `addr >> PAGE_SHIFT`.
pub const PAGE_SHIFT: u32 = 12;

/// A physical address. A newtype so mixing physical and virtual addresses is a type error.
///
/// Not `#[repr(C)]`: it never crosses the syscall ABI.
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

/// A virtual (linear) address; a newtype for the same reason as [`PhysAddr`].
///
/// Not `#[repr(C)]`: it never crosses the syscall ABI.
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

    /// `true` if this address is canonical for 4-level paging: bits 63:48 replicate bit 47. The CPU
    /// `#GP`s on a non-canonical address, so the paging layer rejects them before walking a table.
    pub const fn is_canonical(self) -> bool {
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
        assert!(VirtAddr::new(0).is_canonical());
        assert!(VirtAddr::new(0x0000_7FFF_FFFF_F000).is_canonical());
        assert!(!VirtAddr::new(0x0001_0000_0000_0000).is_canonical());
        assert!(!VirtAddr::new(0x0000_8000_0000_0000).is_canonical());
    }

    #[test]
    fn virt_canonical_high_half() {
        assert!(VirtAddr::new(0xFFFF_8000_0000_0000).is_canonical());
        assert!(VirtAddr::new(0xFFFF_FFFF_8000_0000).is_canonical());
        assert!(!VirtAddr::new(0xFFFE_8000_0000_0000).is_canonical());
    }
}
