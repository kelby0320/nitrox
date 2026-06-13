//! [`DmaBuffer`] — a physically-contiguous, zeroed DMA buffer for bus-mastering
//! devices.
//!
//! A device DMAs to a **physical** address and needs its buffer **physically
//! contiguous** and suitably aligned — neither of which the `KBox`/`kmalloc`
//! path exposes (it hands back a virtual pointer, and `slab` rejects alignments
//! above `SLAB_SIZE`). `DmaBuffer` fills that gap: it allocates a power-of-two
//! block of contiguous frames straight from the buddy allocator (whose order-`k`
//! blocks are aligned to `2^k × PAGE_SIZE`), zeroes it, and exposes **both** the
//! CPU-side virtual pointer (via the HHDM, for filling/reading the buffer) and
//! the physical address (to program into device registers / PRDTs). It is the
//! allocation path the AHCI command lists, received-FIS areas, and PRDTs in the
//! storage slice are built on (`docs/architecture/drivers-and-irps.md` § DMA).
//!
//! ## Alignment
//!
//! The base is page-aligned; a multi-page (`order > 0`) buffer is aligned to its
//! whole block size (`2^order × PAGE_SIZE`) by the buddy invariant. AHCI's
//! sub-structures (1 KiB command list, 256 B received-FIS, 128 B command tables)
//! all fit within a single page-aligned buffer, laid out at their required
//! offsets by the driver — so no explicit alignment parameter is needed; a larger
//! alignment is obtained by allocating a larger block.
//!
//! ## Coherency
//!
//! On x86_64 DMA to/from write-back HHDM memory is hardware-coherent (the CPU
//! snoops), so no cache maintenance is needed. A non-coherent architecture
//! (aarch64) will need clean/invalidate around device access; that is a future
//! `ArchDma` hook, not built here — `DmaBuffer` itself is arch-neutral (buddy +
//! HHDM only).

use core::ptr::{self, NonNull};

use crate::libkern::AllocError;
use crate::mm::buddy::MAX_ORDER;
use crate::mm::{PAGE_SIZE, PhysAddr, heap};

/// An owned, physically-contiguous, zeroed block of DMA-able memory. Frees its
/// frames back to the buddy allocator on drop.
pub struct DmaBuffer {
    /// HHDM-mapped virtual base (for CPU reads/writes). Non-null by construction.
    virt: NonNull<u8>,
    /// Physical base (for the device). Aligned to the block size.
    phys: PhysAddr,
    /// Block size as a buddy order: the buffer spans `2^order × PAGE_SIZE` bytes.
    order: usize,
}

// SAFETY: a `DmaBuffer` owns its physical frames outright; the `virt` pointer
// addresses the kernel-global HHDM mapping of those frames, valid for the
// buffer's lifetime regardless of which CPU/thread holds it. Moving the owner
// across threads is therefore sound. (Not `Sync`: shared concurrent access to
// the bytes is the holder's responsibility, like `&mut`-vs-`&` on any buffer.)
unsafe impl Send for DmaBuffer {}

impl DmaBuffer {
    /// Allocate a zeroed, physically-contiguous buffer of **at least** `size`
    /// bytes (rounded up to the buddy's power-of-two page granularity). Returns
    /// [`AllocError`] if `size` exceeds the largest buddy block
    /// (`2^MAX_ORDER × PAGE_SIZE`) or physical memory is exhausted.
    pub fn alloc(size: usize) -> Result<Self, AllocError> {
        let order = order_for(size).ok_or(AllocError)?;
        let phys = heap::buddy_alloc(order).ok_or(AllocError)?;
        let len = PAGE_SIZE << order;
        // The buddy returns an HHDM-reachable physical block; form its virtual
        // base and zero it before any device or CPU reads it.
        let virt = (phys.as_u64() + heap::hhdm_offset()) as *mut u8;
        // SAFETY: `phys` was just allocated (so not aliased), is HHDM-reachable,
        // and the block is exactly `len` bytes; zeroing it is sound.
        unsafe { ptr::write_bytes(virt, 0, len) };
        Ok(Self {
            // SAFETY: a live HHDM mapping of a real frame is never null.
            virt: unsafe { NonNull::new_unchecked(virt) },
            phys,
            order,
        })
    }

    /// The physical base address — what a device DMAs to (program it into the
    /// HBA's command-list / FIS-base registers, a PRDT entry, etc.).
    pub fn phys(&self) -> PhysAddr {
        self.phys
    }

    /// The CPU-side virtual base (HHDM), for filling/reading the buffer.
    pub fn virt(&self) -> *mut u8 {
        self.virt.as_ptr()
    }

    /// The buffer length in bytes (`2^order × PAGE_SIZE`; ≥ `PAGE_SIZE`).
    pub fn len(&self) -> usize {
        PAGE_SIZE << self.order
    }

    /// The buffer as a byte slice (read-only CPU view).
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `virt` addresses `len()` valid, initialised (zeroed) bytes the
        // buffer owns for its lifetime; the shared borrow ties the slice to it.
        unsafe { core::slice::from_raw_parts(self.virt.as_ptr(), self.len()) }
    }

    /// The buffer as a mutable byte slice (CPU fills it before handing the
    /// `phys()` address to a device).
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as `as_slice`; `&mut self` proves exclusive access.
        unsafe { core::slice::from_raw_parts_mut(self.virt.as_ptr(), self.len()) }
    }
}

impl Drop for DmaBuffer {
    /// Return the block to the buddy allocator. The bytes are raw (no element
    /// destructors), so the free is the only cleanup.
    fn drop(&mut self) {
        heap::buddy_free(self.phys, self.order);
    }
}

/// The smallest buddy `order` whose block (`2^order × PAGE_SIZE`) holds `size`
/// bytes, or `None` if that exceeds [`MAX_ORDER`]. A zero/`size ≤ PAGE_SIZE`
/// request is one page (order 0).
fn order_for(size: usize) -> Option<usize> {
    let pages = size.div_ceil(PAGE_SIZE).max(1);
    let order = pages.next_power_of_two().trailing_zeros() as usize;
    (order <= MAX_ORDER).then_some(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;

    #[test]
    fn order_for_rounds_up_to_power_of_two_pages() {
        assert_eq!(order_for(0), Some(0));
        assert_eq!(order_for(1), Some(0));
        assert_eq!(order_for(PAGE_SIZE), Some(0));
        assert_eq!(order_for(PAGE_SIZE + 1), Some(1)); // 2 pages
        assert_eq!(order_for(3 * PAGE_SIZE), Some(2)); // rounds 3 → 4 pages
        assert_eq!(order_for(PAGE_SIZE << MAX_ORDER), Some(MAX_ORDER));
        assert_eq!(order_for((PAGE_SIZE << MAX_ORDER) + 1), None); // over MAX_ORDER
    }

    #[test]
    fn alloc_is_page_aligned_nonzero_phys_and_zeroed() {
        init_global_heap();
        let buf = DmaBuffer::alloc(100).unwrap();
        assert_eq!(buf.len(), PAGE_SIZE, "100 bytes rounds to one page");
        assert_ne!(buf.phys().as_u64(), 0);
        assert!(buf.phys().is_page_aligned());
        assert!(buf.as_slice().iter().all(|&b| b == 0), "freshly allocated → zeroed");
    }

    #[test]
    fn multi_page_alloc_is_contiguous_and_block_aligned() {
        init_global_heap();
        // 3 pages → order 2 (4-page block), aligned to its 16 KiB block size.
        let buf = DmaBuffer::alloc(3 * PAGE_SIZE).unwrap();
        assert_eq!(buf.len(), 4 * PAGE_SIZE);
        assert_eq!(buf.phys().as_u64() % (buf.len() as u64), 0, "block-size aligned");
        // Contiguous + fully addressable: touch the first and last byte.
        assert!(buf.as_slice().first().is_some());
        assert_eq!(buf.as_slice()[buf.len() - 1], 0);
    }

    #[test]
    fn write_through_virt_is_visible_at_the_phys_mapping() {
        init_global_heap();
        let mut buf = DmaBuffer::alloc(PAGE_SIZE).unwrap();
        buf.as_mut_slice()[0] = 0xAB;
        buf.as_mut_slice()[PAGE_SIZE - 1] = 0xCD;
        // Read back through the raw HHDM mapping of `phys()` — the same bytes a
        // device would see.
        let p = (buf.phys().as_u64() + heap::hhdm_offset()) as *const u8;
        // SAFETY: `p` is the live HHDM mapping of this buffer's frame.
        unsafe {
            assert_eq!(*p, 0xAB);
            assert_eq!(*p.add(PAGE_SIZE - 1), 0xCD);
        }
    }

    #[test]
    fn oversize_alloc_errors() {
        init_global_heap();
        assert!(DmaBuffer::alloc((PAGE_SIZE << MAX_ORDER) + 1).is_err());
    }

    #[test]
    fn repeated_alloc_drop_does_not_leak() {
        // 64 alloc/drop cycles of a 4-page block would exhaust a leaking heap;
        // each `DmaBuffer` must return its frames on drop.
        init_global_heap();
        for _ in 0..64 {
            let buf = DmaBuffer::alloc(4 * PAGE_SIZE).unwrap();
            assert_eq!(buf.len(), 4 * PAGE_SIZE);
            // drops here, freeing the block
        }
    }
}
