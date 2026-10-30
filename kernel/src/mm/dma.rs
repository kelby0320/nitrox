//! [`DmaBuffer`]: a physically contiguous, zeroed DMA buffer for bus-mastering devices.
//!
//! A device DMAs to a **physical** address and needs its buffer **physically contiguous** and
//! aligned, which `KBox`/`kmalloc` cannot give. `DmaBuffer` takes a power-of-two block of frames
//! from the buddy, zeroes it, and exposes the HHDM pointer and the physical address
//! (`docs/architecture/drivers-and-irps.md` § DMA).
//!
//! ## Alignment
//!
//! The base is page-aligned; an `order > 0` buffer is aligned to its whole block size by the
//! buddy invariant, so a larger alignment comes from a larger block.
//!
//! ## Coherency
//!
//! x86_64 DMA to write-back HHDM memory is hardware-coherent. A non-coherent architecture will
//! need clean/invalidate around device access (a future `ArchDma` hook).

use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};
use core::ptr::{self, NonNull};

use crate::libkern::AllocError;
use crate::mm::buddy::MAX_ORDER;
use crate::mm::{PAGE_SIZE, PhysAddr, heap};

/// An owned, physically contiguous, zeroed block of DMA-able memory; freed to the buddy on drop.
pub struct DmaBuffer {
    virt: NonNull<u8>,
    phys: PhysAddr,
    order: usize,
}

// SAFETY: a `DmaBuffer` owns its physical frames outright; the `virt` pointer
// addresses the kernel-global HHDM mapping of those frames, valid for the
// buffer's lifetime regardless of which CPU/thread holds it. Moving the owner
// across threads is therefore sound. (Not `Sync`: shared concurrent access to
// the bytes is the holder's responsibility, like `&mut`-vs-`&` on any buffer.)
unsafe impl Send for DmaBuffer {}

impl DmaBuffer {
    /// Allocate a zeroed, physically contiguous buffer of **at least** `size` bytes, rounded up to a
    /// power-of-two number of pages. [`AllocError`] if `size` exceeds the largest buddy block
    /// (`2^MAX_ORDER × PAGE_SIZE`) or physical memory is exhausted.
    pub fn alloc(size: usize) -> Result<Self, AllocError> {
        let order = order_for(size).ok_or(AllocError)?;
        let phys = heap::buddy_alloc(order).ok_or(AllocError)?;
        let len = PAGE_SIZE << order;
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

    /// The physical base address: what a device DMAs to.
    pub fn phys(&self) -> PhysAddr {
        self.phys
    }

    /// The CPU-side virtual base (HHDM).
    pub fn virt(&self) -> *mut u8 {
        self.virt.as_ptr()
    }

    /// The buffer length in bytes (`2^order × PAGE_SIZE`).
    #[allow(clippy::len_without_is_empty, reason = "never empty: at least a page")]
    pub fn len(&self) -> usize {
        PAGE_SIZE << self.order
    }

    /// The buffer as a byte slice.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `virt` addresses `len()` valid, initialised (zeroed) bytes the
        // buffer owns for its lifetime; the shared borrow ties the slice to it.
        unsafe { core::slice::from_raw_parts(self.virt.as_ptr(), self.len()) }
    }

    /// The buffer as a mutable byte slice.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as `as_slice`; `&mut self` proves exclusive access.
        unsafe { core::slice::from_raw_parts_mut(self.virt.as_ptr(), self.len()) }
    }
}

impl Drop for DmaBuffer {
    /// Return the block to the buddy allocator. The bytes are raw, so the free is the only cleanup.
    fn drop(&mut self) {
        heap::buddy_free(self.phys, self.order);
    }
}

/// **Proof a device has stopped using the memory it was given**, which [`Programmed::release`]
/// takes: minted only where a stop was seen to take, or the one command that held the memory's
/// address was seen to retire.
pub struct Quiesced(());

impl Quiesced {
    /// The proof, for a device seen to stop.
    ///
    /// # Safety
    /// The device given the memory this releases was seen to stop with it — halted, its port or
    /// slot stopped, or the command that held its address retired — and is not given it again.
    pub unsafe fn assume() -> Quiesced {
        Quiesced(())
    }
}

/// **Memory whose address a device was given.** Dropped, it is leaked, since the device may still
/// write it; it is freed only through [`release`](Self::release), with the proof the device
/// stopped. Wrapped before the address is written to the device.
pub struct Programmed<T>(ManuallyDrop<T>);

impl<T> Programmed<T> {
    /// `value`, whose address a device is about to be given.
    pub fn new(value: T) -> Self {
        Programmed(ManuallyDrop::new(value))
    }

    /// The value back, the device having stopped with it: freed as any value is, when dropped.
    pub fn release(self, _stopped: Quiesced) -> T {
        ManuallyDrop::into_inner(self.0)
    }

    /// The value back, for an owner that never frees it: one publishing it for the kernel's life.
    ///
    /// # Safety
    /// The value is not dropped while the device may use the memory.
    pub unsafe fn keep(self) -> T {
        ManuallyDrop::into_inner(self.0)
    }
}

impl<T> Deref for Programmed<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for Programmed<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

/// The smallest buddy `order` whose block holds `size` bytes, or `None` if that exceeds
/// [`MAX_ORDER`]. A request of at most one page is order 0.
fn order_for(size: usize) -> Option<usize> {
    let pages = size.div_ceil(PAGE_SIZE).max(1);
    let order = pages.next_power_of_two().trailing_zeros() as usize;
    (order <= MAX_ORDER).then_some(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;

    /// **Memory a device was given is freed only with the proof it stopped**: dropped, it leaks.
    #[test]
    fn programmed_memory_is_freed_only_once_its_device_stopped() {
        init_global_heap();
        let freed = heap::frames_freed_by_this_thread();
        let _ = Programmed::new(DmaBuffer::alloc(PAGE_SIZE).unwrap());
        assert_eq!(heap::frames_freed_by_this_thread(), freed, "dropped while the device may write it");
        let mut kept = Programmed::new(DmaBuffer::alloc(2 * PAGE_SIZE).unwrap());
        kept.as_mut_slice()[0] = 1;
        // SAFETY: no device was given it.
        drop(kept.release(unsafe { Quiesced::assume() }));
        assert_eq!(heap::frames_freed_by_this_thread(), freed + 2, "released, then freed");
    }

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
        let buf = DmaBuffer::alloc(3 * PAGE_SIZE).unwrap();
        assert_eq!(buf.len(), 4 * PAGE_SIZE);
        assert_eq!(buf.phys().as_u64() % (buf.len() as u64), 0, "block-size aligned");
        assert!(!buf.as_slice().is_empty());
        assert_eq!(buf.as_slice()[buf.len() - 1], 0);
    }

    #[test]
    fn write_through_virt_is_visible_at_the_phys_mapping() {
        init_global_heap();
        let mut buf = DmaBuffer::alloc(PAGE_SIZE).unwrap();
        buf.as_mut_slice()[0] = 0xAB;
        buf.as_mut_slice()[PAGE_SIZE - 1] = 0xCD;
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
        init_global_heap();
        for _ in 0..64 {
            let buf = DmaBuffer::alloc(4 * PAGE_SIZE).unwrap();
            assert_eq!(buf.len(), 4 * PAGE_SIZE);
        }
    }
}
