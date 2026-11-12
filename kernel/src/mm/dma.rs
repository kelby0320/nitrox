//! [`DmaBuffer`]: a physically contiguous, zeroed DMA buffer for bus-mastering devices.
//!
//! A device DMAs to a **physical** address and needs its buffer **physically contiguous** and
//! aligned, which `KBox`/`kmalloc` cannot give. `DmaBuffer` takes a power-of-two block of frames
//! from the buddy, zeroes it, and exposes the physical address and **bounded volatile accessors**,
//! never a reference: a device may write the bytes meanwhile
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
use core::ops::Deref;
use core::ptr::NonNull;

use crate::libkern::AllocError;
use crate::libkern::Pod;
use crate::mm::buddy::MAX_ORDER;
use crate::mm::frame::FrameBlock;
use crate::mm::{PAGE_SIZE, PhysAddr, VirtAddr};

/// An owned, physically contiguous, zeroed block of DMA-able memory; freed to the buddy on drop.
#[derive(Debug)]
pub struct DmaBuffer {
    virt: NonNull<u8>,
    block: FrameBlock,
}

// SAFETY: a `DmaBuffer` owns its physical frames outright; the `virt` pointer
// addresses the kernel-global HHDM mapping of those frames, valid for the
// buffer's lifetime regardless of which CPU/thread holds it. Moving the owner
// across threads is therefore sound. (Not `Sync`: its accessors write through
// `&self`, so a buffer shared between CPUs is shared behind the lock that
// serialises them.)
unsafe impl Send for DmaBuffer {}

impl DmaBuffer {
    /// Allocate a zeroed, physically contiguous buffer of **at least** `size` bytes, rounded up to a
    /// power-of-two number of pages. [`AllocError`] if `size` exceeds the largest buddy block
    /// (`2^MAX_ORDER × PAGE_SIZE`) or physical memory is exhausted.
    pub fn alloc(size: usize) -> Result<Self, AllocError> {
        let block = FrameBlock::alloc_zeroed(order_for(size).ok_or(AllocError)?).ok_or(AllocError)?;
        let virt = crate::mm::frame::hhdm::<u8>(block.phys());
        Ok(Self {
            // SAFETY: a live HHDM mapping of a real frame is never null.
            virt: unsafe { NonNull::new_unchecked(virt) },
            block,
        })
    }

    /// The physical base address: what a device DMAs to.
    pub fn phys(&self) -> PhysAddr {
        self.block.phys()
    }

    /// Where the kernel maps it: an address to check a translation against, not a way in.
    pub fn mapped_at(&self) -> VirtAddr {
        VirtAddr::new(self.virt.as_ptr() as u64)
    }

    /// The buffer length in bytes (`2^order × PAGE_SIZE`).
    #[allow(clippy::len_without_is_empty, reason = "never empty: at least a page")]
    pub fn len(&self) -> usize {
        PAGE_SIZE << self.block.order()
    }

    /// The `T` at byte `off`, as the device left it. Panics past the end, or unaligned for `T`.
    pub fn read<T: Pod>(&self, off: usize) -> T {
        // SAFETY: `at` holds an aligned `T` inside the buffer, which this borrow keeps; volatile,
        // as a device may write it, and any bytes are a `T`.
        unsafe { self.at::<T>(off, 1).read_volatile() }
    }

    /// Write `v` at byte `off`, for the device to read. Panics as [`read`](Self::read) does.
    pub fn write<T: Pod>(&self, off: usize, v: T) {
        // SAFETY: as `read`.
        unsafe { self.at::<T>(off, 1).write_volatile(v) }
    }

    /// Copy `src` into the buffer from `off`. Panics past the end, before a byte is written.
    pub fn copy_in(&self, off: usize, src: &[u8]) {
        self.at::<u8>(off, src.len());
        src.iter().enumerate().for_each(|(i, &b)| self.write(off + i, b));
    }

    /// Copy the buffer's bytes from `off` into `dst`. Panics past the end.
    pub fn copy_out(&self, off: usize, dst: &mut [u8]) {
        self.at::<u8>(off, dst.len());
        dst.iter_mut().enumerate().for_each(|(i, b)| *b = self.read(off + i));
    }

    /// Set `len` bytes from `off` to `byte`. Panics past the end, before a byte is written.
    pub fn fill(&self, off: usize, len: usize, byte: u8) {
        self.at::<u8>(off, len);
        (off..off + len).for_each(|i| self.write(i, byte));
    }

    fn at<T>(&self, off: usize, n: usize) -> *mut T {
        let bytes = size_of::<T>().saturating_mul(n);
        assert!(
            off.checked_add(bytes).is_some_and(|end| end <= self.len()),
            "{bytes} bytes at {off} run past a DMA buffer"
        );
        assert!(off.is_multiple_of(align_of::<T>()), "{off} is not aligned for {}", core::any::type_name::<T>());
        self.virt.as_ptr().wrapping_add(off).cast()
    }
}

/// **Proof a device stopped using the memory it was given**: minted only where its stop, or the
/// retiring of the one command holding the memory, was seen.
#[derive(Debug)]
pub struct Quiesced(());

impl Quiesced {
    /// # Safety
    /// The device given the memory was seen to stop with it, and is not given it again.
    pub unsafe fn assume() -> Quiesced {
        Quiesced(())
    }
}

/// **A DMA buffer whose address a device was given.** Dropped, it is leaked, as the device may still
/// write it, unless [`release`](Self::release)d first with the proof the device stopped. No
/// `&mut DmaBuffer` is to be had through it, so nothing replaces it but a drop.
#[derive(Debug)]
pub struct Programmed {
    buffer: ManuallyDrop<DmaBuffer>,
    released: bool,
}

impl Programmed {
    /// `buffer`, whose address a device is about to be given.
    pub fn new(buffer: DmaBuffer) -> Self {
        Programmed { buffer: ManuallyDrop::new(buffer), released: false }
    }

    /// The device stopped with it: it is freed when dropped, as any buffer is.
    pub fn release(&mut self, _stopped: &Quiesced) {
        self.released = true;
    }
}

impl Deref for Programmed {
    type Target = DmaBuffer;
    fn deref(&self) -> &DmaBuffer {
        &self.buffer
    }
}

impl Drop for Programmed {
    fn drop(&mut self) {
        if self.released {
            // SAFETY: dropped once, here, and never used again.
            unsafe { ManuallyDrop::drop(&mut self.buffer) };
        }
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
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;
    use crate::mm::heap;
    use crate::mm::test_support::init_global_heap;

    fn stops(why: &str, f: impl FnOnce()) -> bool {
        let payload = catch_unwind(AssertUnwindSafe(f)).err();
        payload.and_then(|p| p.downcast::<std::string::String>().ok()).is_some_and(|s| s.contains(why))
    }

    /// **Each access reaches the buffer's last byte and stops one past it**, or at an end that
    /// overflows, writing none of it; bytes, aligned anywhere, are held only to the end.
    #[test]
    fn an_access_reaches_the_last_byte_and_stops_one_past_it() {
        init_global_heap();
        let buf = DmaBuffer::alloc(PAGE_SIZE).unwrap();
        let len = buf.len();
        buf.write::<u32>(len - 4, 0x0403_0201);
        assert_eq!(buf.read::<[u8; 4]>(len - 4), [1, 2, 3, 4]);
        buf.write::<[u8; 3]>(len - 3, [5, 6, 7]);
        assert_eq!(buf.read::<u32>(len - 4), 0x0706_0501);
        assert!(stops("run past", || buf.write::<[u8; 4]>(len - 3, [9; 4])), "four bytes at len - 3");
        assert!(stops("run past", || buf.write::<u32>(len, 9)), "an aligned word at the end");
        assert!(stops("run past", || buf.copy_in(len - 2, &[9; 3])), "a copy in running one past");
        assert!(stops("run past", || buf.fill(len - 1, 2, 9)), "a fill running one past");
        assert!(stops("run past", || buf.copy_out(len - 1, &mut [0; 2])), "a copy out running one past");
        assert!(stops("run past", || buf.fill(1, usize::MAX, 9)), "an end that overflows");
        assert!(stops("run past", || _ = buf.read::<u8>(usize::MAX)), "an offset that overflows");
        assert_eq!(buf.read::<u32>(len - 4), 0x0706_0501, "none of it written");
        buf.copy_in(len, &[]);
        buf.fill(len, 0, 9);
        buf.copy_out(len, &mut []);
        let mut out = [0u8; 3];
        buf.copy_in(len - 3, &[7, 8, 9]);
        buf.copy_out(len - 3, &mut out);
        assert_eq!(out, [7, 8, 9]);
    }

    /// **An access unaligned for its type is refused**, inside the buffer, as `read_volatile` needs.
    #[test]
    fn an_unaligned_access_is_refused() {
        init_global_heap();
        let buf = DmaBuffer::alloc(PAGE_SIZE).unwrap();
        assert!(stops("not aligned", || _ = buf.read::<u32>(2)), "a word at 2");
        assert!(stops("not aligned", || buf.write::<u64>(4, 1)), "a double word at 4");
        assert_eq!(buf.read::<u64>(0), 0, "none of it written");
        assert_eq!((buf.read::<u16>(2), buf.read::<[u8; 4]>(1)), (0, [0; 4]));
    }

    /// **A buffer a device was given is freed only with the proof it stopped**: dropped, or cleared
    /// from a list, it leaks; released, it is freed when dropped.
    #[test]
    fn programmed_memory_is_freed_only_once_its_device_stopped() {
        init_global_heap();
        let freed = heap::frames_freed_by_this_thread();
        let mut list = crate::libkern::KVec::new();
        list.try_push(Programmed::new(DmaBuffer::alloc(PAGE_SIZE).unwrap())).unwrap();
        drop(list);
        drop(Programmed::new(DmaBuffer::alloc(PAGE_SIZE).unwrap()));
        assert_eq!(heap::frames_freed_by_this_thread(), freed, "dropped while the device may write it");
        let mut kept = Programmed::new(DmaBuffer::alloc(2 * PAGE_SIZE).unwrap());
        kept.write::<u8>(0, 1);
        // SAFETY: no device was given it.
        kept.release(&unsafe { Quiesced::assume() });
        drop(kept);
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
        assert!((0..buf.len()).all(|i| buf.read::<u8>(i) == 0), "freshly allocated → zeroed");
    }

    #[test]
    fn multi_page_alloc_is_contiguous_and_block_aligned() {
        init_global_heap();
        let buf = DmaBuffer::alloc(3 * PAGE_SIZE).unwrap();
        assert_eq!(buf.len(), 4 * PAGE_SIZE);
        assert_eq!(buf.phys().as_u64() % (buf.len() as u64), 0, "block-size aligned");
        assert_eq!(buf.read::<u8>(buf.len() - 1), 0);
    }

    #[test]
    fn write_through_virt_is_visible_at_the_phys_mapping() {
        init_global_heap();
        let buf = DmaBuffer::alloc(PAGE_SIZE).unwrap();
        buf.write::<u8>(0, 0xAB);
        buf.write::<u8>(PAGE_SIZE - 1, 0xCD);
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
