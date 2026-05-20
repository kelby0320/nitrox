//! Memory management.
//!
//! Three-layer design (per `docs/architecture/overview.md` §"Memory
//! management"): the buddy allocator manages physical page frames; a
//! SLUB-inspired slab allocator handles kernel object allocation on top
//! of the buddy; the VMM owns per-process address spaces. This module
//! holds the buddy allocator and the common types both upper layers
//! consume.

pub mod buddy;
pub mod heap;
pub mod slab;

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
