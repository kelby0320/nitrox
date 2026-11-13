//! The [`MemoryObject`] kernel object — mappable memory.
//!
//! **Owns** physical frames, allocated and zeroed at creation and freed with the last
//! reference (or **borrows** a device range; see [`MemoryObject::try_new_borrowed`]). A mapping
//! ([`AddressSpace::map_object`]) holds an [`ObjectRef`], so the frames outlive every mapping
//! and `unmap` never frees them; mapping one object twice aliases the same memory. Allocation
//! is eager and anonymous only (`docs/architecture/memory-management.md`).
//!
//! [`AddressSpace::map_object`]: crate::mm::addr_space::AddressSpace::map_object
//! [`ObjectRef`]: crate::object::ObjectRef

use core::ops::Range;

use crate::libkern::{AllocError, KBox, KVec};
use crate::mm::frame::{Frame, FrameView};
use crate::mm::{Caching, PAGE_SHIFT, PAGE_SIZE, PhysAddr};
use crate::object::header::{Header, Shared, kobject};

/// A [`MemoryObject`]'s pages, and whether they are its own.
enum Frames {
    /// Frames from the buddy, one a page, freed as the object drops.
    Owned(KVec<Frame>),
    /// A device's range from `base`, never freed: an aperture freed to the buddy would be RAM handed out.
    Borrowed { base: PhysAddr, npages: usize },
}

/// Why the kernel refuses to reach a borrowed object's frames itself.
const BORROWED_ONLY_MAPPED: &str = "a borrowed object's frames are only mapped";

/// An anonymous memory kernel object (`#[repr(C)]`, [`KObjectHeader`] first).
#[repr(C)]
pub struct MemoryObject {
    header: Header<Self>,
    magic: u64,
    size: usize,
    frames: Frames,
    /// How every mapping of these frames is cached; ordinary unless an aperture says otherwise.
    caching: Caching,
}

kobject!(MemoryObject);
// SAFETY: nothing writes a `MemoryObject`'s fields once it is made; its frames' bytes are not
// its fields.
unsafe impl Shared for MemoryObject {}

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
        let mut frames = KVec::new();
        frames.try_reserve(npages)?;
        for _ in 0..npages {
            frames.try_push(Frame::alloc_zeroed().ok_or(AllocError)?).expect("within reserved capacity");
        }
        KBox::try_new(Self {
            header: Header::new(),
            magic: Self::MAGIC,
            size,
            frames: Frames::Owned(frames),
            caching: Caching::Normal,
        })
    }

    /// Build a memory object over a **borrowed**, contiguous range the kernel does not own (a
    /// device aperture): nothing is allocated and [`Drop`] frees nothing. The compositor maps the
    /// framebuffer this way (`docs/design/display-substrate.md` §3). [`MemoryObject::MAX_SIZE`]
    /// does **not** apply: it guards eager allocation, and a 4K aperture is ≈33 MiB.
    ///
    /// `base` must be page-aligned; `size` rounds up. `AllocError` only if the box fails.
    ///
    /// # Safety
    /// `base..base + size` must be physical memory safe to map into a user address space for the
    /// lifetime of every handle derived from this object, and must not overlap any frame the
    /// buddy allocator manages (ordinary RAM would alias allocatable memory and never be freed).
    pub unsafe fn try_new_borrowed(base: PhysAddr, size: usize, caching: Caching) -> Result<KBox<Self>, AllocError> {
        let size = (size.max(1) + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        KBox::try_new(Self {
            header: Header::new(),
            magic: Self::MAGIC,
            size,
            frames: Frames::Borrowed { base, npages: size >> PAGE_SHIFT },
            caching,
        })
    }

    /// Whether the kernel may reach these frames itself, through the HHDM or a device: only those
    /// it owns. A borrowed range need not be in the HHDM, nor be memory, so it is for mapping.
    pub fn kernel_reachable(&self) -> bool {
        matches!(self.frames, Frames::Owned(_))
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
        obj.copy_in(0, bytes);
        Ok(obj)
    }

    /// Page-rounded byte size.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Number of frames (pages) backing the object.
    pub fn npages(&self) -> usize {
        match &self.frames {
            Frames::Owned(frames) => frames.len(),
            Frames::Borrowed { npages, .. } => *npages,
        }
    }

    /// The object's backing frames, for the kernel to reach itself; `frames()[i]` backs page `i`.
    /// A borrowed object's stop here, being only mapped: see [`Self::frame_to_map`].
    pub fn frames(&self) -> &[Frame] {
        match &self.frames {
            Frames::Owned(frames) => frames,
            Frames::Borrowed { .. } => panic!("{BORROWED_ONLY_MAPPED}"),
        }
    }

    /// Page `i`'s frame for a mapping into a user's address space, a borrowed object's too.
    pub fn frame_to_map(&self, i: usize) -> Option<PhysAddr> {
        match &self.frames {
            Frames::Owned(frames) => frames.get(i).map(Frame::phys),
            Frames::Borrowed { base, npages } => {
                (i < *npages).then(|| PhysAddr::new(base.as_u64() + (i << PAGE_SHIFT) as u64))
            }
        }
    }

    /// Page `i`'s frame, for the kernel to copy in or out of; `None` past the last.
    pub fn page(&self, i: usize) -> Option<FrameView<'_>> {
        self.frames().get(i).map(Frame::view)
    }

    /// **Copy `src` into the object from byte `offset`**; a range past the last frame is cut
    /// short, as the caller (`sys_io_submit`) has bounds-checked it.
    pub fn copy_in(&self, offset: usize, src: &[u8]) {
        self.pieces(offset, src.len(), |page, at, piece| page.write(at, &src[piece]));
    }

    /// **Copy the object's bytes from `offset` into `dst`**, cut short at the last frame; the count.
    pub fn copy_out(&self, offset: usize, dst: &mut [u8]) -> usize {
        self.pieces(offset, dst.len(), |page, at, piece| page.read(at, &mut dst[piece]))
    }

    /// Hand `step` each frame's piece of `len` bytes from `offset`: page, offset in it, buffer range.
    fn pieces(&self, offset: usize, len: usize, mut step: impl FnMut(FrameView<'_>, usize, Range<usize>)) -> usize {
        let mut done = 0;
        while let Some(pos) = offset.checked_add(done).filter(|_| done < len) {
            let Some(page) = self.page(pos / PAGE_SIZE) else { break };
            let at = pos % PAGE_SIZE;
            let n = (PAGE_SIZE - at).min(len - done);
            step(page, at, done..done + n);
            done += n;
        }
        done
    }

    /// Copy the object into a fresh contiguous heap buffer (page-rounded [`size`](Self::size)
    /// bytes): `sys_process_spawn` needs one slice for the ELF loader, so the largest is
    /// [`MAX_IMAGE_BYTES`](crate::mm::elf::MAX_IMAGE_BYTES), 4 MiB less a page. A loader reading
    /// the frames would avoid the copy (`spawn-image-kvec` in `docs/rationale/deferred-decisions.md`).
    pub fn copy_to_kvec(&self) -> Result<KVec<u8>, AllocError> {
        assert!(self.kernel_reachable(), "{BORROWED_ONLY_MAPPED}");
        let mut buf = KVec::try_filled(0, self.size)?;
        self.copy_out(0, &mut buf);
        Ok(buf)
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::frame::fail_frames_after;
    use crate::mm::heap;
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
        for f in m.frames() {
            let mut bytes = [1u8; PAGE_SIZE];
            f.view().read(0, &mut bytes);
            assert!(bytes.iter().all(|&b| b == 0), "a frame not zeroed");
        }
    }

    /// **An object frees each frame it owns once as it drops; one a frame short, those it took.**
    #[test]
    fn an_object_frees_its_frames_once_and_a_short_one_those_it_took() {
        init_global_heap();
        let freed = heap::frames_freed_by_this_thread();
        drop(MemoryObject::try_new(3 * PAGE_SIZE).unwrap());
        assert_eq!(heap::frames_freed_by_this_thread() - freed, 3);
        for short in 0..3 {
            fail_frames_after(Some(short));
            let freed = heap::frames_freed_by_this_thread();
            let made = MemoryObject::try_new(3 * PAGE_SIZE);
            fail_frames_after(None);
            assert!(made.is_err());
            assert_eq!(heap::frames_freed_by_this_thread() - freed, short, "a frame short at {short}");
        }
    }

    #[test]
    fn a_borrowed_object_describes_the_range_without_allocating() {
        init_global_heap();
        let base = PhysAddr::new(0xF000_0000);
        // SAFETY: test-only. Nothing maps or dereferences these frames — the test
        // inspects the object's bookkeeping, never the memory itself.
        let m = unsafe { MemoryObject::try_new_borrowed(base, 3 * PAGE_SIZE + 1, Caching::Normal).unwrap() };

        assert!(!m.kernel_reachable());
        assert_eq!(m.size(), 4 * PAGE_SIZE, "size rounds up to whole pages");
        assert_eq!(m.npages(), 4);
        for i in 0..4 {
            assert_eq!(m.frame_to_map(i), Some(PhysAddr::new(base.as_u64() + (i * PAGE_SIZE) as u64)), "frame {i}");
        }
        assert_eq!(m.frame_to_map(4), None);
    }

    /// **The kernel does not reach a borrowed object's frames itself**: copying in or out, or
    /// taking its frames for the HHDM or a device, stops; only a mapping takes them.
    #[test]
    fn the_kernel_does_not_reach_a_borrowed_objects_frames_itself() {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        init_global_heap();
        let base = PhysAddr::new(0xF000_0000);
        // SAFETY: a range the test heap never holds; nothing below reads or writes it.
        let m = unsafe { MemoryObject::try_new_borrowed(base, PAGE_SIZE, Caching::Normal).unwrap() };
        let stops = |f: &dyn Fn()| {
            let stopped = catch_unwind(AssertUnwindSafe(f));
            stopped.is_err_and(|e| e.downcast_ref::<String>().is_some_and(|s| s.contains(BORROWED_ONLY_MAPPED)))
        };
        assert!(stops(&|| assert!(!m.frames().is_empty())), "frames");
        assert!(stops(&|| m.copy_in(0, b"x")), "copy_in");
        assert!(stops(&|| drop(m.copy_to_kvec())), "copy_to_kvec");
        assert_eq!((m.frame_to_map(0), m.frame_to_map(1)), (Some(base), None));
    }

    #[test]
    fn dropping_a_borrowed_object_never_frees_its_frames_into_the_buddy() {
        init_global_heap();
        // SAFETY: test-only. Nothing maps or dereferences these frames; the object
        // only records their addresses, and Borrowed drop must not touch them.
        let m = unsafe {
            MemoryObject::try_new_borrowed(PhysAddr::new(0xF000_0000), 64 * PAGE_SIZE, Caching::WriteCombining).unwrap()
        };
        let freed = heap::frames_freed_by_this_thread();
        drop(m);
        assert_eq!(heap::frames_freed_by_this_thread(), freed);
    }

    #[test]
    fn try_new_filled_copies_bytes_and_zeroes_tail() {
        init_global_heap();
        let mut data = [0u8; PAGE_SIZE + 10];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let m = MemoryObject::try_new_filled(&data).unwrap();
        assert_eq!(m.npages(), 2);
        let base0 = (m.frames()[0].phys().as_u64() + heap::hhdm_offset()) as *const u8;
        let base1 = (m.frames()[1].phys().as_u64() + heap::hhdm_offset()) as *const u8;
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
                assert_ne!(fs[i].phys(), fs[j].phys(), "duplicate frame at {i},{j}");
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
        let r = ObjectRef::new(m);
        assert_eq!(test_probe::memory_object_destroys(), 0);
        drop(r);
        assert_eq!(test_probe::memory_object_destroys(), 1);
    }

    #[test]
    #[cfg_attr(miri, ignore = "`kmalloc` is the host's under Miri, with no largest block")]
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

    /// **An object copied out while a user mapping writes it is no race**, which Miri tells: natively,
    /// any copy passes. Both move aligned atomic words.
    #[test]
    fn copying_an_object_out_while_another_writes_it_is_no_race() {
        use core::sync::atomic::{AtomicU64, Ordering};
        init_global_heap();
        let m = MemoryObject::try_new(PAGE_SIZE).unwrap();
        let words = (m.frames()[0].phys().as_u64() + heap::hhdm_offset()) as usize;
        std::thread::scope(|s| {
            s.spawn(|| {
                for i in 0..PAGE_SIZE / 8 {
                    // SAFETY: a word of the object's frame, which outlives the scope; stored
                    // atomically, as a user's store is to the kernel's reads.
                    unsafe { AtomicU64::from_ptr((words as *mut u64).add(i)) }.store(u64::MAX, Ordering::Relaxed);
                }
            });
            let copy = m.copy_to_kvec().unwrap();
            assert!(copy.iter().all(|&b| b == 0 || b == 0xFF));
        });
    }

    /// **Copies at different alignments are no race**, which Miri tells: one in from an odd offset
    /// and one out whole reach the frame in the same aligned words.
    #[test]
    fn copies_at_different_alignments_are_no_race() {
        init_global_heap();
        let m = MemoryObject::try_new(PAGE_SIZE).unwrap();
        std::thread::scope(|s| {
            s.spawn(|| m.copy_in(1, &[0xFF; 15]));
            let copy = m.copy_to_kvec().unwrap();
            assert!(copy[1..16].iter().all(|&b| b == 0 || b == 0xFF) && copy[0] == 0 && copy[16] == 0);
        });
    }
}
