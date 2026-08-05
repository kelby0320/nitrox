//! The [`MemoryObject`] kernel object — anonymous, mappable memory.
//!
//! A `MemoryObject` **owns** a set of physical frames, allocated and zeroed
//! at creation and freed when the object's last reference goes away.
//! `sys_memory_map` installs page-table entries pointing at *these* frames
//! into a process's address space (see [`AddressSpace::map_object`]); a
//! mapping records an [`ObjectRef`] back to the object, so the frames outlive
//! every mapping and `unmap` never frees them. Mapping the same object twice —
//! or, once a second process exists, in two address spaces — therefore aliases
//! the same physical memory. This is the property that makes a `MemoryObject`
//! a first-class, shareable thing rather than just "anonymous mmap".
//!
//! Phase 1 scope: eager allocation (every frame up front), anonymous (zero-
//! filled) backing only. Lazy on-fault allocation, copy-on-write, and
//! file-backed objects are deferred (see `docs/architecture/memory-management.md`).
//!
//! [`AddressSpace::map_object`]: crate::mm::addr_space::AddressSpace::map_object
//! [`ObjectRef`]: crate::object::ObjectRef

use core::ptr;

use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox, KVec};
use crate::mm::{PAGE_SHIFT, PAGE_SIZE, PhysAddr, heap};
use crate::object::header::KObjectHeader;

/// Whether a [`MemoryObject`]'s frames belong to it.
///
/// The distinction is load-bearing rather than descriptive. [`Drop`] hands every
/// frame back to the buddy allocator, which is correct for ordinary anonymous memory
/// and **catastrophic** for a device aperture: closing the last handle to a framebuffer
/// would put MMIO physical addresses on the free list, to be handed out later as if
/// they were RAM. The failure is silent at the point of the mistake and appears much
/// later as unrelated corruption.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameOwnership {
    /// Frames came from the buddy allocator and are freed on drop.
    Owned,
    /// Frames are a fixed physical range this object does not own — a device
    /// aperture reported by firmware. Never freed.
    Borrowed,
}

/// An anonymous memory kernel object.
///
/// `#[repr(C)]` with [`KObjectHeader`] first so the type-erased object
/// pointer in a handle entry can be read as `*const KObjectHeader` at offset
/// 0 — see [`crate::object::header`].
#[repr(C)]
pub struct MemoryObject {
    header: KObjectHeader,
    /// Self-check sentinel; a live object always reads [`MemoryObject::MAGIC`].
    magic: u64,
    /// Page-rounded byte size of the object.
    size: usize,
    /// One physical frame per page; `frames[i]` backs page `i`. Freed in [`Drop`]
    /// only when [`MemoryObject::ownership`] is [`FrameOwnership::Owned`].
    frames: KVec<PhysAddr>,
    /// Whether [`Drop`] frees `frames`.
    ownership: FrameOwnership,
}

impl MemoryObject {
    /// Sentinel written into [`MemoryObject::magic`] at construction.
    pub const MAGIC: u64 = 0x4d65_6d4f_626a_2121; // "MemObj!!"

    /// Largest object `sys_memory_create` will build, in bytes (4096 frames).
    /// Larger requests are rejected as `TooLarge`.
    ///
    /// This is a **denial-of-service guard tied to eager allocation, not a
    /// designed ceiling.** [`try_new`](Self::try_new) commits every frame up
    /// front (one `buddy_alloc` + zero per page), so a single large create
    /// would pin that much physical RAM at once and run an unpreemptable
    /// allocate-and-zero loop — dangerous on a small VM with a cooperative
    /// scheduler. Real systems (Linux anonymous `mmap`/`memfd`, Windows
    /// pagefile-backed sections) have no per-allocation byte cap because they
    /// are lazy (demand-zero on first fault) and bound memory with system-wide
    /// accounting instead. The cap disappears when `MemoryObject` backing
    /// becomes demand-paged (gated on a real `#PF` handler) and per-process
    /// memory quotas land. Until then, raising it only moves the threshold —
    /// see `docs/rationale/deferred-decisions.md` § "Lazy (demand-paged)
    /// MemoryObject backing".
    pub const MAX_SIZE: usize = 16 * 1024 * 1024;

    /// Allocate a memory object of `size` bytes (rounded up to a whole number
    /// of pages), with every frame zeroed. Refcount one.
    ///
    /// On any frame-allocation failure, the frames allocated so far are freed
    /// before returning [`AllocError`]. (`size == 0` is treated as one page
    /// defensively; the syscall layer rejects 0 before reaching here.)
    pub fn try_new(size: usize) -> Result<KBox<Self>, AllocError> {
        let size = size.max(1);
        let size = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let npages = size >> PAGE_SHIFT;

        // Reserve the whole frame vector up front — the only fallible growth,
        // so the per-frame pushes below cannot fail.
        let mut frames: KVec<PhysAddr> = KVec::new();
        frames.try_reserve(npages)?;

        for _ in 0..npages {
            let Some(f) = heap::buddy_alloc(0) else {
                // Out of frames mid-build: free the ones already taken. `frames`
                // here is a bare KVec (not yet a MemoryObject), so its own Drop
                // would free only its storage, not these buddy frames.
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

        // On `KBox::try_new` failure the moved-in value is dropped, running
        // `Drop` below, which frees every frame — no manual cleanup needed here.
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::MemoryObject),
            magic: Self::MAGIC,
            size,
            frames,
            ownership: FrameOwnership::Owned,
        })
    }

    /// Build a memory object over a **borrowed**, physically contiguous range that the
    /// kernel does not own — a device aperture reported by firmware.
    ///
    /// No frames are allocated: `base` and `size` describe memory that already exists.
    /// [`Drop`] therefore never frees them ([`FrameOwnership::Borrowed`]), which is the
    /// entire point. The framebuffer is the first user: Limine hands the kernel a linear
    /// aperture before userspace exists, and the compositor maps it through an ordinary
    /// `MemoryObject` handle rather than through a bespoke protocol
    /// (`docs/design/display-substrate.md` §3, "the kernel already has `MemoryObject`
    /// for 'memory you map'").
    ///
    /// [`MemoryObject::MAX_SIZE`] deliberately does **not** apply. That cap is a
    /// denial-of-service guard on *eager allocation* — it bounds how much RAM one
    /// create can pin and zero. Nothing is allocated or zeroed here, and a 4K display's
    /// aperture (≈33 MiB) legitimately exceeds it.
    ///
    /// `base` must be page-aligned; `size` is rounded up to whole pages. Returns
    /// `AllocError` only if the frame vector itself cannot be allocated.
    ///
    /// # Safety
    ///
    /// `base..base + size` must be a physical range that is safe to map into a user
    /// address space for the lifetime of every handle derived from this object, and
    /// must not overlap any frame the buddy allocator manages. Passing ordinary RAM
    /// here would create an object that aliases allocatable memory and never frees it.
    pub unsafe fn try_new_borrowed(
        base: PhysAddr,
        size: usize,
    ) -> Result<KBox<Self>, AllocError> {
        let size = (size.max(1) + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let npages = size >> PAGE_SHIFT;

        let mut frames: KVec<PhysAddr> = KVec::new();
        frames.try_reserve(npages)?;
        for i in 0..npages {
            frames
                .try_push(PhysAddr::new(base.as_u64() + (i << PAGE_SHIFT) as u64))
                .expect("within reserved capacity");
        }

        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::MemoryObject),
            magic: Self::MAGIC,
            size,
            frames,
            ownership: FrameOwnership::Borrowed,
        })
    }

    /// Whether this object's frames are freed when it drops.
    pub fn ownership(&self) -> FrameOwnership {
        self.ownership
    }

    /// Allocate a memory object holding a copy of `bytes` (size rounded up to a
    /// whole number of pages; any tail past `bytes.len()` stays zero). Refcount
    /// one. The first **synthesised read-only `MemoryObject`** primitive: the
    /// in-kernel `/initramfs` server uses it to hand userspace a readable,
    /// mappable copy of a file's content. (See `docs/rationale/deferred-decisions.md`
    /// § "Resource servers" / `/proc/self/status`.)
    pub fn try_new_filled(bytes: &[u8]) -> Result<KBox<Self>, AllocError> {
        let obj = Self::try_new(bytes.len().max(1))?;
        // Copy `bytes` into the already-zeroed frames, page by page, via the HHDM.
        let mut copied = 0usize;
        for &f in obj.frames.iter() {
            if copied >= bytes.len() {
                break;
            }
            let n = core::cmp::min(PAGE_SIZE, bytes.len() - copied);
            // SAFETY: `f` is a live, HHDM-reachable frame owned by `obj`, not
            // aliased; we copy `n ≤ PAGE_SIZE` bytes into it from a valid source.
            unsafe {
                ptr::copy_nonoverlapping(
                    bytes.as_ptr().add(copied),
                    (f.as_u64() + heap::hhdm_offset()) as *mut u8,
                    n,
                );
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

    /// Copy the object's contents into a fresh contiguous heap buffer (page-rounded
    /// [`size`](Self::size) bytes; the tail past the real data stays zero). The reverse
    /// of [`try_new_filled`](Self::try_new_filled): `sys_process_spawn` uses it to hand a
    /// spawner-supplied ELF image to the ELF loader, which needs one contiguous slice
    /// (this object's frames are one-per-page and physically discontiguous).
    ///
    /// Deferred optimization (the preferred long-term approach): map the frames into a
    /// temporary contiguous kernel VMA and load from that, avoiding the copy — see
    /// `docs/rationale/deferred-decisions.md`.
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
            let page = unsafe {
                core::slice::from_raw_parts((f.as_u64() + heap::hhdm_offset()) as *const u8, n)
            };
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
    /// Free every backing frame. Runs when the last reference releases (via
    /// `dispatch_destroy` dropping the owning `KBox`). Unlike `Process` — whose
    /// owned `AddressSpace` carries its own `Drop` — a `MemoryObject` holds raw
    /// `PhysAddr`s with no owning wrapper, so it must free them itself.
    fn drop(&mut self) {
        // Borrowed frames are a device aperture the kernel does not own. Freeing them
        // would put MMIO addresses on the buddy free list — see [`FrameOwnership`].
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
        // 1 byte rounds up to one page.
        let m = MemoryObject::try_new(1).unwrap();
        assert_eq!(m.size(), PAGE_SIZE);
        assert_eq!(m.npages(), 1);
        assert!(m.magic_ok());

        // A 3-page request: every byte of every frame reads zero (via HHDM).
        let m = MemoryObject::try_new(2 * PAGE_SIZE + 1).unwrap();
        assert_eq!(m.size(), 3 * PAGE_SIZE);
        assert_eq!(m.npages(), 3);
        for &f in m.frames() {
            // SAFETY: a live MemoryObject's frames are allocated and
            // HHDM-reachable; read-only check.
            let base = (f.as_u64() + heap::hhdm_offset()) as *const u8;
            for i in 0..PAGE_SIZE {
                assert_eq!(unsafe { *base.add(i) }, 0, "frame byte {i} not zeroed");
            }
        }
    }

    #[test]
    fn a_borrowed_object_describes_the_range_without_allocating() {
        init_global_heap();
        // A plausible aperture base, page-aligned and well clear of anything the
        // buddy hands out in tests.
        let base = PhysAddr::new(0xF000_0000);
        // SAFETY: test-only. Nothing maps or dereferences these frames — the test
        // inspects the object's bookkeeping, never the memory itself.
        let m = unsafe { MemoryObject::try_new_borrowed(base, 3 * PAGE_SIZE + 1).unwrap() };

        assert_eq!(m.ownership(), FrameOwnership::Borrowed);
        assert_eq!(m.size(), 4 * PAGE_SIZE, "size rounds up to whole pages");
        assert_eq!(m.npages(), 4);
        // Frames describe the aperture, contiguously, in order.
        for (i, &f) in m.frames().iter().enumerate() {
            assert_eq!(f.as_u64(), base.as_u64() + (i * PAGE_SIZE) as u64, "frame {i}");
        }
    }

    #[test]
    fn dropping_a_borrowed_object_never_frees_its_frames_into_the_buddy() {
        // The property the whole `FrameOwnership` distinction exists for. If it
        // regresses, closing the last framebuffer handle puts MMIO addresses on the
        // buddy free list, and the damage surfaces much later, somewhere unrelated.
        //
        // Deterministic by construction: the fake aperture is a high physical range
        // the test heap never contains, so a leaked frame is identifiable by address
        // rather than by hoping the allocator hands the same one straight back. An
        // earlier version of this test allocated a real frame and asserted the next
        // allocation differed — which was flaky under the parallel test runner, and
        // was caught only because two consecutive runs disagreed.
        init_global_heap();
        const APERTURE: u64 = 0xF000_0000;
        const PAGES: usize = 64;

        // SAFETY: test-only. Nothing maps or dereferences these frames; the object
        // only records their addresses, and Borrowed drop must not touch them.
        let m = unsafe {
            MemoryObject::try_new_borrowed(PhysAddr::new(APERTURE), PAGES * PAGE_SIZE).unwrap()
        };
        drop(m);

        // If Drop had freed them, those bogus addresses are now on the free list.
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

    // The control for the test above lives in `drop_frees_frames_no_leak`: it builds
    // and drops 64 eight-page *owned* objects against a 16 MiB heap, so if `Drop`
    // ever stopped freeing anything at all, that test exhausts the heap and fails.
    // Without it, "borrowed frames are not freed" would also pass on a `Drop` that
    // had become a no-op.

    #[test]
    fn try_new_filled_copies_bytes_and_zeroes_tail() {
        init_global_heap();
        // A payload spanning into a second page; the tail past it must stay zero.
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
            for i in 0..PAGE_SIZE {
                assert_eq!(*base0.add(i), data[i], "page0 byte {i}");
            }
            for i in 0..10 {
                assert_eq!(*base1.add(i), data[PAGE_SIZE + i], "page1 byte {i}");
            }
            // Tail of page 1 past the payload is zero.
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
        // Repeatedly build + drop a multi-page object. A leak of the backing
        // frames would exhaust the 16 MiB test heap over these rounds.
        init_global_heap();
        for _ in 0..64 {
            let m = MemoryObject::try_new(8 * PAGE_SIZE).unwrap();
            assert_eq!(m.npages(), 8);
            // Dropped at end of iteration.
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
}
