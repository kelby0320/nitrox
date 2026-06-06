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
    /// One physical frame per page; `frames[i]` backs page `i`. Owned: freed
    /// in [`Drop`] when the last reference releases.
    frames: KVec<PhysAddr>,
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
        })
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
