//! The [`MemoryObject`] kernel object — mappable memory.
//!
//! **Owns** physical frames, allocated and zeroed at creation and freed with the last
//! reference (or **borrows** a device range; see [`FrameOwnership`]). A mapping
//! ([`AddressSpace::map_object`]) holds an [`ObjectRef`], so the frames outlive every mapping
//! and `unmap` never frees them; mapping one object twice aliases the same memory. Allocation
//! is eager and anonymous only (`docs/architecture/memory-management.md`).
//!
//! [`AddressSpace::map_object`]: crate::mm::addr_space::AddressSpace::map_object
//! [`ObjectRef`]: crate::object::ObjectRef

use core::ptr;

use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox, KVec};
use crate::mm::{Caching, PAGE_SHIFT, PAGE_SIZE, PhysAddr, heap};
use crate::object::header::KObjectHeader;

/// Whether a [`MemoryObject`]'s frames belong to it.
///
/// Load-bearing: [`Drop`] hands every frame to the buddy allocator, which is **catastrophic**
/// for a device aperture, whose MMIO addresses would land on the free list and be handed out
/// as RAM.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameOwnership {
    /// Frames came from the buddy allocator and are freed on drop.
    Owned,
    /// A fixed physical range this object does not own (a device aperture); never freed.
    Borrowed,
}

/// An anonymous memory kernel object (`#[repr(C)]`, [`KObjectHeader`] first).
#[repr(C)]
pub struct MemoryObject {
    header: KObjectHeader,
    magic: u64,
    size: usize,
    /// One frame per page; freed in [`Drop`] only when `ownership` is `Owned`.
    frames: KVec<PhysAddr>,
    ownership: FrameOwnership,
    /// How every mapping of these frames is cached; ordinary unless an aperture says otherwise.
    caching: Caching,
}

impl MemoryObject {
    /// Sentinel written into [`MemoryObject::magic`] at construction.
    pub const MAGIC: u64 = 0x4d65_6d4f_626a_2121; // "MemObj!!"

    /// Largest object `sys_memory_create` will build (4096 frames); larger is `TooLarge`.
    ///
    /// A **denial-of-service guard tied to eager allocation, not a designed ceiling**: one create
    /// would pin that much RAM in an unpreemptable zeroing loop. See
    /// `docs/rationale/deferred-decisions.md` § "Lazy (demand-paged) MemoryObject backing".
    pub const MAX_SIZE: usize = 16 * 1024 * 1024;

    /// Allocate a memory object of `size` bytes (rounded up to whole pages), every frame zeroed,
    /// refcount one. On any frame-allocation failure the frames taken so far are freed before
    /// returning [`AllocError`]. (`size == 0` is one page; the syscall layer rejects 0.)
    pub fn try_new(size: usize) -> Result<KBox<Self>, AllocError> {
        let size = size.max(1);
        let size = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let npages = size >> PAGE_SHIFT;

        // Reserve the frame vector up front, the only fallible growth: the pushes cannot fail.
        let mut frames: KVec<PhysAddr> = KVec::new();
        frames.try_reserve(npages)?;

        for _ in 0..npages {
            let Some(f) = heap::buddy_alloc(0) else {
                // `frames` is a bare `KVec` here: its `Drop` frees only storage, not these frames.
                for &done in frames.iter() {
                    heap::buddy_free(done, 0);
                }
                return Err(AllocError);
            };
            // SAFETY: `f` was just returned by the buddy, is not aliased, and is
            // HHDM-reachable. Zeroing prevents leaking stale memory to userspace.
            unsafe {
                ptr::write_bytes((f.as_u64() + heap::hhdm_offset()) as *mut u8, 0, PAGE_SIZE);
            }
            frames.try_push(f).expect("within reserved capacity");
        }

        // A `KBox::try_new` failure drops the value, and `Drop` below frees every frame.
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::MemoryObject),
            magic: Self::MAGIC,
            size,
            frames,
            ownership: FrameOwnership::Owned,
            caching: Caching::Normal,
        })
    }

    /// Build a memory object over a **borrowed**, contiguous range the kernel does not own (a
    /// device aperture): nothing is allocated and [`Drop`] frees nothing. The compositor maps the
    /// framebuffer this way (`docs/design/display-substrate.md` §3). [`MemoryObject::MAX_SIZE`]
    /// does **not** apply: it guards eager allocation, and a 4K aperture is ≈33 MiB.
    ///
    /// `base` must be page-aligned; `size` rounds up. `AllocError` only if the frame vector fails.
    ///
    /// # Safety
    /// `base..base + size` must be physical memory safe to map into a user address space for the
    /// lifetime of every handle derived from this object, and must not overlap any frame the
    /// buddy allocator manages (ordinary RAM would alias allocatable memory and never be freed).
    pub unsafe fn try_new_borrowed(base: PhysAddr, size: usize, caching: Caching) -> Result<KBox<Self>, AllocError> {
        let size = (size.max(1) + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let npages = size >> PAGE_SHIFT;

        let mut frames: KVec<PhysAddr> = KVec::new();
        frames.try_reserve(npages)?;
        for i in 0..npages {
            frames.try_push(PhysAddr::new(base.as_u64() + (i << PAGE_SHIFT) as u64)).expect("within reserved capacity");
        }

        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::MemoryObject),
            magic: Self::MAGIC,
            size,
            frames,
            ownership: FrameOwnership::Borrowed,
            caching,
        })
    }

    /// Whether this object's frames are freed when it drops.
    pub fn ownership(&self) -> FrameOwnership {
        self.ownership
    }

    /// How every mapping of this object is cached.
    pub fn caching(&self) -> Caching {
        self.caching
    }

    /// Allocate a memory object holding a copy of `bytes` (tail zero), refcount one: how the
    /// `/initramfs` server hands userspace a mappable copy of a file. (See
    /// `docs/rationale/deferred-decisions.md` § "Resource servers" / `/proc/self/status`.)
    pub fn try_new_filled(bytes: &[u8]) -> Result<KBox<Self>, AllocError> {
        let obj = Self::try_new(bytes.len().max(1))?;
        let mut copied = 0usize;
        for &f in obj.frames.iter() {
            if copied >= bytes.len() {
                break;
            }
            let n = core::cmp::min(PAGE_SIZE, bytes.len() - copied);
            // SAFETY: `f` is a live, HHDM-reachable frame owned by `obj`, not
            // aliased; we copy `n ≤ PAGE_SIZE` bytes into it from a valid source.
            unsafe {
                ptr::copy_nonoverlapping(bytes.as_ptr().add(copied), (f.as_u64() + heap::hhdm_offset()) as *mut u8, n);
            }
            copied += n;
        }
        Ok(obj)
    }

    /// Page-rounded byte size.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Number of frames (pages) backing the object.
    pub fn npages(&self) -> usize {
        self.frames.len()
    }

    /// The object's backing frames; `frames()[i]` backs page `i`.
    pub fn frames(&self) -> &[PhysAddr] {
        &self.frames
    }

    /// **Copy `src` into the object at byte `offset`** through the HHDM; a range past the last
    /// frame is cut short, as the caller (`sys_io_submit`) has bounds-checked it.
    pub fn copy_in(&self, offset: usize, src: &[u8]) {
        let hhdm = heap::hhdm_offset();
        for (i, &b) in src.iter().enumerate() {
            let pos = offset + i;
            let Some(frame) = self.frames.get(pos / PAGE_SIZE) else {
                break;
            };
            let dst = (frame.as_u64() + hhdm) as *mut u8;
            // SAFETY: within one of this object's frames, which the HHDM maps and the object
            // owns or borrows for as long as `self` is alive.
            unsafe { *dst.add(pos % PAGE_SIZE) = b };
        }
    }

    /// Copy the object into a fresh contiguous heap buffer (page-rounded [`size`](Self::size)
    /// bytes): `sys_process_spawn` needs one slice for the ELF loader, so the largest is
    /// [`MAX_IMAGE_BYTES`](crate::mm::elf::MAX_IMAGE_BYTES), 4 MiB less a page. Mapping the frames
    /// into a temporary kernel VMA would avoid the copy; see `docs/rationale/deferred-decisions.md`.
    pub fn copy_to_kvec(&self) -> Result<KVec<u8>, AllocError> {
        let mut buf = KVec::new();
        buf.try_reserve(self.size)?;
        let mut remaining = self.size;
        for &f in self.frames.iter() {
            if remaining == 0 {
                break;
            }
            let n = core::cmp::min(PAGE_SIZE, remaining);
            // SAFETY: `f` is a live, HHDM-reachable frame owned by `self`, not aliased;
            // reading `n <= PAGE_SIZE` bytes from its HHDM mapping is sound.
            let page = unsafe { core::slice::from_raw_parts((f.as_u64() + heap::hhdm_offset()) as *const u8, n) };
            buf.try_extend_from_slice(page)?;
            remaining -= n;
        }
        Ok(buf)
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }
}

impl Drop for MemoryObject {
    /// Free every backing frame; the raw `PhysAddr`s have no owning wrapper to do it.
    fn drop(&mut self) {
        // Borrowed frames are a device aperture: freeing them would put MMIO addresses on the
        // buddy free list (see [`FrameOwnership`]).
        if self.ownership == FrameOwnership::Borrowed {
            return;
        }
        for &f in self.frames.iter() {
            heap::buddy_free(f, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;
    use crate::object::ObjectRef;
    use crate::object::header::test_probe;

    #[test]
    fn try_new_rounds_up_and_zeroes_every_frame() {
        init_global_heap();
        let m = MemoryObject::try_new(1).unwrap();
        assert_eq!(m.size(), PAGE_SIZE);
        assert_eq!(m.npages(), 1);
        assert!(m.magic_ok());

        let m = MemoryObject::try_new(2 * PAGE_SIZE + 1).unwrap();
        assert_eq!(m.size(), 3 * PAGE_SIZE);
        assert_eq!(m.npages(), 3);
        for &f in m.frames() {
            let base = (f.as_u64() + heap::hhdm_offset()) as *const u8;
            for i in 0..PAGE_SIZE {
                // SAFETY: a live MemoryObject's frames are allocated and HHDM-reachable.
                let byte = unsafe { *base.add(i) };
                assert_eq!(byte, 0, "frame byte {i} not zeroed");
            }
        }
    }

    #[test]
    fn a_borrowed_object_describes_the_range_without_allocating() {
        init_global_heap();
        let base = PhysAddr::new(0xF000_0000);
        // SAFETY: test-only. Nothing maps or dereferences these frames — the test
        // inspects the object's bookkeeping, never the memory itself.
        let m = unsafe { MemoryObject::try_new_borrowed(base, 3 * PAGE_SIZE + 1, Caching::Normal).unwrap() };

        assert_eq!(m.ownership(), FrameOwnership::Borrowed);
        assert_eq!(m.size(), 4 * PAGE_SIZE, "size rounds up to whole pages");
        assert_eq!(m.npages(), 4);
        for (i, &f) in m.frames().iter().enumerate() {
            assert_eq!(f.as_u64(), base.as_u64() + (i * PAGE_SIZE) as u64, "frame {i}");
        }
    }

    #[test]
    fn dropping_a_borrowed_object_never_frees_its_frames_into_the_buddy() {
        // The property `FrameOwnership` exists for. The fake aperture is a high range the test heap
        // never contains, so a leaked frame is identifiable by address.
        init_global_heap();
        const APERTURE: u64 = 0xF000_0000;
        const PAGES: usize = 64;

        // SAFETY: test-only. Nothing maps or dereferences these frames; the object
        // only records their addresses, and Borrowed drop must not touch them.
        let m = unsafe {
            MemoryObject::try_new_borrowed(PhysAddr::new(APERTURE), PAGES * PAGE_SIZE, Caching::WriteCombining).unwrap()
        };
        drop(m);

        let mut taken = KVec::new();
        taken.try_reserve(PAGES).unwrap();
        for _ in 0..PAGES {
            let f = heap::buddy_alloc(0).expect("a frame");
            assert!(
                f.as_u64() < APERTURE || f.as_u64() >= APERTURE + (PAGES * PAGE_SIZE) as u64,
                "buddy returned {:#x}, inside the borrowed aperture — Drop freed frames \
                 it does not own",
                f.as_u64()
            );
            taken.try_push(f).unwrap();
        }
        for &f in taken.iter() {
            heap::buddy_free(f, 0);
        }
    }

    // The control for the test above is `drop_frees_frames_no_leak`: if `Drop` stopped freeing
    // anything, that test exhausts the heap, whereas a no-op `Drop` would still pass this one.

    #[test]
    fn try_new_filled_copies_bytes_and_zeroes_tail() {
        init_global_heap();
        let mut data = [0u8; PAGE_SIZE + 10];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let m = MemoryObject::try_new_filled(&data).unwrap();
        assert_eq!(m.npages(), 2);
        let base0 = (m.frames()[0].as_u64() + heap::hhdm_offset()) as *const u8;
        let base1 = (m.frames()[1].as_u64() + heap::hhdm_offset()) as *const u8;
        // SAFETY: live, HHDM-reachable frames; read-only checks.
        unsafe {
            for (i, &d) in data[..PAGE_SIZE].iter().enumerate() {
                assert_eq!(*base0.add(i), d, "page0 byte {i}");
            }
            for i in 0..10 {
                assert_eq!(*base1.add(i), data[PAGE_SIZE + i], "page1 byte {i}");
            }
            for i in 10..PAGE_SIZE {
                assert_eq!(*base1.add(i), 0, "tail byte {i} not zero");
            }
        }
    }

    #[test]
    fn frames_are_distinct() {
        init_global_heap();
        let m = MemoryObject::try_new(4 * PAGE_SIZE).unwrap();
        let fs = m.frames();
        for i in 0..fs.len() {
            for j in (i + 1)..fs.len() {
                assert_ne!(fs[i], fs[j], "duplicate frame at {i},{j}");
            }
        }
    }

    #[test]
    fn drop_frees_frames_no_leak() {
        init_global_heap();
        for _ in 0..64 {
            let m = MemoryObject::try_new(8 * PAGE_SIZE).unwrap();
            assert_eq!(m.npages(), 8);
        }
    }

    #[test]
    fn dispatch_destroy_runs_memory_object_arm() {
        init_global_heap();
        test_probe::reset();
        let m = MemoryObject::try_new(PAGE_SIZE).unwrap();
        let ptr = KBox::into_raw(m).as_ptr() as *mut ();
        // SAFETY: `ptr` carries the single creation reference.
        let r = unsafe { ObjectRef::from_raw(ptr, KObjectType::MemoryObject) };
        assert_eq!(test_probe::memory_object_destroys(), 0);
        drop(r);
        assert_eq!(test_probe::memory_object_destroys(), 1);
    }

    #[test]
    fn copying_an_image_past_the_largest_block_is_refused() {
        init_global_heap();
        let big = MemoryObject::try_new((PAGE_SIZE << crate::mm::buddy::MAX_ORDER) + PAGE_SIZE).unwrap();
        assert!(big.copy_to_kvec().is_err());
    }

    #[test]
    fn an_aperture_carries_its_caching_and_ordinary_memory_does_not() {
        init_global_heap();
        const APERTURE: u64 = 0xF000_0000;
        // SAFETY: a made-up physical range no test maps or frees; nothing reads through it.
        let device = unsafe {
            MemoryObject::try_new_borrowed(PhysAddr::new(APERTURE), PAGE_SIZE, Caching::WriteCombining).unwrap()
        };
        assert_eq!(device.caching(), Caching::WriteCombining);
        assert_eq!(MemoryObject::try_new(PAGE_SIZE).unwrap().caching(), Caching::Normal);
    }
}
