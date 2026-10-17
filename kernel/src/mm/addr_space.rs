//! Per-process virtual address space.
//!
//! [`AddressSpace`] pairs a [`VmaTree`] with a top-level page-table root (the PML4 on x86_64)
//! under one [`SpinLock`]: `map_vma` updates both atomically, `unmap_covering` does the inverse.
//!
//! - **Eager vs lazy anonymous allocation.** `map_vma` allocates a frame per page up front;
//!   `map_vma_lazy` reserves the range and [`fault_in`](AddressSpace::fault_in) backs each page
//!   on first touch. Object- and file-backed mappings have their own backing sources.
//! - **Intermediate page-table frames are not reclaimed on unmap** (the deferred decision on
//!   `ArchPaging::unmap_page`, [deferred-decisions.md]): `Drop` reclaims leaf frames and the
//!   PML4, and the PDPT / PD / PT levels leak.
//!
//! [deferred-decisions.md]: ../../docs/rationale/deferred-decisions.md

use core::ptr;

use crate::arch::Paging;
use crate::arch::abi::{DEFAULT_USER_STACK_SIZE, DEFAULT_USER_STACK_TOP, USER_STACK_GUARD_SIZE, USER_VIRT_END};
use crate::arch::paging::{ArchPaging, MapError as ArchMapError, PageFlags};
use crate::libkern::handle::KObjectType;
use crate::libkern::lockrank::LockRank;
use crate::libkern::{AllocError, KBox, KVec, SpinLock};
use crate::mm::vmm::{FaultAccess, MappingKind, Protection, VAddrRange, Vma, VmaTree};
use crate::mm::{Caching, PAGE_SIZE, PhysAddr, VirtAddr, heap};
use crate::object::{FileObject, MemoryObject, ObjectRef};

/// Base of the `hint == 0` ("anywhere") window: above the ELF image, well below the user stack.
const MMAP_BASE: u64 = 0x1_0000_0000;

/// Lowest address of the initial thread's stack region.
pub const USER_STACK_BOTTOM: u64 = DEFAULT_USER_STACK_TOP - DEFAULT_USER_STACK_SIZE;

/// Lowest address of the guard gap below the stack; nothing may be mapped in it.
pub const USER_STACK_GUARD_BOTTOM: u64 = USER_STACK_BOTTOM - USER_STACK_GUARD_SIZE;

/// Exclusive upper bound of the "anywhere" window, below the **guard gap**. It bounds only the
/// `hint == 0` path; a *hinted* mapping is bounded by [`AddressSpace::in_stack_guard`], or it
/// could sit in the gap and restore the silent stack overrun the gap removes.
const MMAP_MAX: u64 = USER_STACK_GUARD_BOTTOM;

/// Why [`AddressSpace::map_vma`] could not install a mapping; the caller's [`KBox<Vma>`] comes
/// back alongside.
#[derive(Debug, PartialEq, Eq)]
pub enum MapError {
    /// A range endpoint was not canonical.
    NotCanonical,
    /// The range falls outside the user half (`[0, USER_VIRT_END)`).
    NotUserHalf,
    /// The range overlaps an existing VMA in this address space.
    Overlap,
    /// Frame allocation failed; frames installed before the failure are rolled back first.
    OutOfMemory,
}

/// Outcome of [`AddressSpace::fault_in`]. Only [`Mapped`](FaultIn::Mapped) lets the instruction
/// retry; every other variant is fatal to the thread.
#[derive(Debug, PartialEq, Eq)]
pub enum FaultIn {
    /// A frame was faulted in (or was already present): retry the instruction.
    Mapped,
    /// No VMA covers the faulting address.
    NoVma,
    /// A VMA covers the address but forbids the access.
    Protection,
    /// A VMA permits the access but no backing frame could be allocated.
    Oom,
    /// A [`FileBacked`](MappingKind::FileBacked) not-present fault. `fault_in` only *signals*:
    /// the caller fetches the backing ([`file_backing`](AddressSpace::file_backing)), makes the
    /// page resident and installs the PTE ([`map_file_page`](AddressSpace::map_file_page)) outside
    /// the AS lock, since the file's page-cache lock is rank 4 like this one and they never nest.
    FileBacked,
}

/// A per-process virtual address space: all state behind one rank-4 `SpinLock<Inner>`
/// ([kernel/docs/lock-ordering.md]), taken and released inside the public methods.
pub struct AddressSpace {
    inner: SpinLock<Inner>,
}

struct Inner {
    vma_tree: VmaTree,
    /// Physical base of the top-level page-table frame; freed in `Drop` after the tree.
    root: PhysAddr,
}

impl AddressSpace {
    /// Build an empty address space: a zeroed PML4 frame inheriting the kernel-half entries from
    /// the boot template, and an empty VMA tree. Panics before `arch::init_kernel_template`.
    pub fn new() -> Result<Self, AllocError> {
        let root = heap::buddy_alloc(0).ok_or(AllocError)?;
        // SAFETY: the frame was just returned by the buddy and is not
        // aliased; HHDM access is the standard way to reach a fresh
        // physical frame. Zeroing it makes every PML4 entry absent;
        // `inherit_kernel_mappings` then refills the kernel half so
        // the AS is loadable without losing kernel code, stack, or HHDM.
        unsafe {
            let virt = (root.as_u64() + heap::hhdm_offset()) as *mut u8;
            ptr::write_bytes(virt, 0, PAGE_SIZE);
            Paging::inherit_kernel_mappings(root);
        }
        Ok(AddressSpace { inner: SpinLock::new(LockRank::KernelObject, Inner { vma_tree: VmaTree::new(), root }) })
    }

    /// Physical base of this address space's top-level page table.
    pub fn root(&self) -> PhysAddr {
        self.inner.lock().root
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().vma_tree.is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().vma_tree.len()
    }

    /// Install a VMA: allocate and zero a frame per page, install each PTE, then insert the VMA,
    /// all under the AS lock so tree and page tables never disagree. The box is returned on any
    /// rejection.
    pub fn map_vma(&self, boxed: KBox<Vma>) -> Result<(), (KBox<Vma>, MapError)> {
        let range = boxed.range;

        // Both endpoints must be canonical; test `end - 1` since the end is exclusive.
        let last_byte = VirtAddr::new(range.end().as_u64() - 1);
        if !range.start().is_canonical() || !last_byte.is_canonical() {
            return Err((boxed, MapError::NotCanonical));
        }
        if range.end().as_u64() > USER_VIRT_END {
            return Err((boxed, MapError::NotUserHalf));
        }

        let mut guard = self.inner.lock();

        // Pre-check overlap so a doomed insert runs no frame allocations to roll back.
        if guard.vma_tree.find_first_overlapping(range).is_some() {
            return Err((boxed, MapError::Overlap));
        }

        let flags = page_flags_for(boxed.prot, boxed.caching);
        let root = guard.root;
        let total_pages = range.pages();
        let mut installed: u64 = 0;

        for i in 0..total_pages {
            let virt = VirtAddr::new(range.start().as_u64() + i * (PAGE_SIZE as u64));

            let Some(phys) = heap::buddy_alloc(0) else {
                rollback_partial_map(root, range.start(), installed);
                return Err((boxed, MapError::OutOfMemory));
            };

            // SAFETY: `phys` was just allocated, is not aliased, and is
            // HHDM-reachable.
            unsafe {
                let v = (phys.as_u64() + heap::hhdm_offset()) as *mut u8;
                ptr::write_bytes(v, 0, PAGE_SIZE);
            }

            // SAFETY: `root` is the PML4 owned by this AS; `phys` is a
            // fresh frame we just allocated and zeroed; `virt` is in
            // the validated, canonical, user-half range. No TLB flush
            // is needed today — no CPU has this address space loaded.
            let r = unsafe { Paging::map_page(root, virt, phys, flags) };
            match r {
                Ok(()) => installed += 1,
                Err(ArchMapError::OutOfMemory) => {
                    heap::buddy_free(phys, 0);
                    rollback_partial_map(root, range.start(), installed);
                    return Err((boxed, MapError::OutOfMemory));
                }
                Err(ArchMapError::AlreadyMapped) => {
                    // Impossible: AS lock held and overlap pre-checked.
                    unreachable!("ArchPaging::map_page returned AlreadyMapped after VmaTree overlap pre-check");
                }
                Err(ArchMapError::Misaligned) => {
                    // Impossible: `VAddrRange` enforces page alignment.
                    unreachable!("ArchPaging::map_page returned Misaligned for a page-aligned per-page address");
                }
            }
        }

        // Commit. The insert cannot reject: the lock is held and overlap was checked above.
        match guard.vma_tree.insert(boxed) {
            Ok(()) => Ok(()),
            Err(b) => {
                rollback_partial_map(root, range.start(), installed);
                Err((b, MapError::Overlap))
            }
        }
    }

    /// Reserve an **anonymous** VMA with no frames and no PTEs; [`fault_in`](Self::fault_in)
    /// zeroes and maps each page on first touch. `boxed.mapping` must be
    /// [`MappingKind::Anonymous`]. Same rejections as `map_vma` (box returned), but it allocates
    /// nothing, so never [`OutOfMemory`](MapError::OutOfMemory).
    pub fn map_vma_lazy(&self, boxed: KBox<Vma>) -> Result<(), (KBox<Vma>, MapError)> {
        debug_assert_eq!(boxed.mapping, MappingKind::Anonymous, "map_vma_lazy is for anonymous mappings only");
        let range = boxed.range;

        let last_byte = VirtAddr::new(range.end().as_u64() - 1);
        if !range.start().is_canonical() || !last_byte.is_canonical() {
            return Err((boxed, MapError::NotCanonical));
        }
        if range.end().as_u64() > USER_VIRT_END {
            return Err((boxed, MapError::NotUserHalf));
        }

        let mut guard = self.inner.lock();
        if guard.vma_tree.find_first_overlapping(range).is_some() {
            return Err((boxed, MapError::Overlap));
        }

        match guard.vma_tree.insert(boxed) {
            Ok(()) => Ok(()),
            // Unreachable: overlap was pre-checked under the same lock.
            Err(b) => Err((b, MapError::Overlap)),
        }
    }

    /// Resolve a page fault at `addr` for `access` against this (currently loaded) address space.
    /// Called from the arch `#PF` handler for a **not-present** ring-3 fault.
    ///
    /// For an [`Anonymous`](MappingKind::Anonymous) VMA permitting the access, allocates and zeroes
    /// a frame, installs the leaf PTE and flushes that page's stale not-present TLB entry. The
    /// address space **is live in the MMU** (unlike eager `map_vma`), so the flush is mandatory.
    /// Only [`FaultIn::Mapped`] means retry.
    pub fn fault_in(&self, addr: VirtAddr, access: FaultAccess) -> FaultIn {
        let guard = self.inner.lock();

        let Some(vma) = guard.vma_tree.find_covering(addr) else {
            return FaultIn::NoVma;
        };

        match access {
            FaultAccess::Write if !vma.prot.contains(Protection::WRITE) => {
                return FaultIn::Protection;
            }
            FaultAccess::Execute if !vma.prot.contains(Protection::EXEC) => {
                return FaultIn::Protection;
            }
            _ => {}
        }

        let flags = page_flags_for(vma.prot, vma.caching);
        let kind = vma.mapping;
        let root = guard.root;
        let page_base = VirtAddr::new(addr.as_u64() & !(PAGE_SIZE as u64 - 1));

        match kind {
            MappingKind::Anonymous => {
                let Some(phys) = heap::buddy_alloc(0) else {
                    return FaultIn::Oom;
                };
                // SAFETY: `phys` was just allocated, is not aliased, and is
                // HHDM-reachable; we zero the whole frame before it is mapped.
                unsafe {
                    let v = (phys.as_u64() + heap::hhdm_offset()) as *mut u8;
                    ptr::write_bytes(v, 0, PAGE_SIZE);
                }
                // SAFETY: `root` is this AS's PML4; `phys` is the fresh,
                // zeroed frame; `page_base` is the page-aligned faulting
                // address, in the validated user-half VMA range. The PTE is
                // absent (this was a not-present fault), so `map_page` installs
                // a new leaf rather than aliasing.
                match unsafe { Paging::map_page(root, page_base, phys, flags) } {
                    Ok(()) => {}
                    Err(ArchMapError::OutOfMemory) => {
                        heap::buddy_free(phys, 0);
                        return FaultIn::Oom;
                    }
                    // Impossible: a not-present fault means no PTE, and `page_base` is aligned.
                    Err(ArchMapError::AlreadyMapped | ArchMapError::Misaligned) => {
                        heap::buddy_free(phys, 0);
                        unreachable!("fault_in: map_page rejected a fresh page-aligned not-present mapping");
                    }
                }
                // SAFETY: ring-0 `invlpg` evicting the not-present entry the
                // CPU cached for this page on the fault we are servicing.
                unsafe {
                    Paging::flush_tlb_page(page_base);
                }
                crate::mm::record_demand_fault();
                FaultIn::Mapped
            }
            // Installed eagerly by `map_object`; a fault here is a bug or teardown race.
            MappingKind::Object => FaultIn::NoVma,
            // Signal only: the file cache's lock is rank 4 like this one and they never nest
            // (see `FaultIn::FileBacked`).
            MappingKind::FileBacked => FaultIn::FileBacked,
        }
    }

    /// Map a [`MemoryObject`]'s **own** frames over `range`, holding `object` alive. Allocates and
    /// zeroes **nothing**: mapping the object twice aliases the same memory. The object frees the
    /// frames on its last-ref drop, not unmap.
    ///
    /// `object` needs at least `range.pages()` frames (the syscall layer checks). On failure it
    /// is handed back in the error tuple.
    pub fn map_object(
        &self,
        range: VAddrRange,
        prot: Protection,
        object: ObjectRef,
    ) -> Result<(), (ObjectRef, MapError)> {
        let last_byte = VirtAddr::new(range.end().as_u64() - 1);
        if !range.start().is_canonical() || !last_byte.is_canonical() {
            return Err((object, MapError::NotCanonical));
        }
        if range.end().as_u64() > USER_VIRT_END {
            return Err((object, MapError::NotUserHalf));
        }

        debug_assert_eq!(object.object_type(), KObjectType::MemoryObject);
        // SAFETY: `object` holds a reference to a live `MemoryObject` (its
        // header is at offset 0); we only read its frame list.
        let mobj = unsafe { &*(object.as_ptr() as *const MemoryObject) };
        // `frames` borrows through the raw pointer, not `object`, so `object` can move into the VMA
        // below while the install loop still reads it; the object stays alive throughout.
        let frames = mobj.frames();
        let caching = mobj.caching();
        let npages = range.pages();
        debug_assert!(frames.len() as u64 >= npages, "object too small for range");

        // Build the VMA box before the lock (no allocation under a spinlock); the only fallible
        // step before `object` is consumed, so failure can return it.
        let mut boxed = match KBox::try_new(Vma::new(range, prot, MappingKind::Object)) {
            Ok(b) => b,
            Err(_) => return Err((object, MapError::OutOfMemory)),
        };
        boxed.caching = caching;

        let mut guard = self.inner.lock();

        if guard.vma_tree.find_first_overlapping(range).is_some() {
            return Err((object, MapError::Overlap));
        }

        let flags = page_flags_for(prot, caching);
        let root = guard.root;
        let mut installed: u64 = 0;

        for i in 0..npages {
            let virt = VirtAddr::new(range.start().as_u64() + i * (PAGE_SIZE as u64));
            // SAFETY: `root` is this AS's PML4; `frames[i]` is owned by the
            // `MemoryObject` the VMA will keep alive; `virt` is in the
            // validated canonical user-half range. Aliasing a frame already
            // mapped elsewhere is intended (shared memory). No frame is
            // allocated or zeroed here — the object owns them.
            match unsafe { Paging::map_page(root, virt, frames[i as usize], flags) } {
                Ok(()) => installed += 1,
                Err(ArchMapError::OutOfMemory) => {
                    rollback_object_map(root, range.start(), installed);
                    return Err((object, MapError::OutOfMemory));
                }
                Err(ArchMapError::AlreadyMapped) => {
                    // Impossible: overlap was pre-checked under the lock.
                    unreachable!("ArchPaging::map_page returned AlreadyMapped after VmaTree overlap pre-check");
                }
                Err(ArchMapError::Misaligned) => {
                    // Impossible: VAddrRange enforces page alignment.
                    unreachable!("ArchPaging::map_page returned Misaligned for a page-aligned per-page address");
                }
            }
        }

        boxed.object = Some(object);
        match guard.vma_tree.insert(boxed) {
            Ok(()) => Ok(()),
            Err(mut b) => {
                rollback_object_map(root, range.start(), installed);
                let object = b.object.take().expect("object-backed VMA carries its ObjectRef");
                Err((object, MapError::Overlap))
            }
        }
    }

    /// Reserve `range` as a **lazy file-backed** mapping of `object` (a
    /// [`FileObject`](crate::object::FileObject)) with **no PTEs**; pages come in through
    /// `fault_in`, [`FaultIn::FileBacked`], fill, [`map_file_page`]. The VMA keeps `object` alive
    /// so its cache frames outlive the mapping; `unmap` removes the PTEs but never frees the
    /// frames. On rejection `object` is handed back.
    ///
    /// [`map_file_page`]: AddressSpace::map_file_page
    pub fn map_file(
        &self,
        range: VAddrRange,
        prot: Protection,
        object: ObjectRef,
    ) -> Result<(), (ObjectRef, MapError)> {
        debug_assert_eq!(object.object_type(), KObjectType::FileObject);
        let last_byte = VirtAddr::new(range.end().as_u64() - 1);
        if !range.start().is_canonical() || !last_byte.is_canonical() {
            return Err((object, MapError::NotCanonical));
        }
        if range.end().as_u64() > USER_VIRT_END {
            return Err((object, MapError::NotUserHalf));
        }
        let mut boxed = match KBox::try_new(Vma::new(range, prot, MappingKind::FileBacked)) {
            Ok(b) => b,
            Err(_) => return Err((object, MapError::OutOfMemory)),
        };
        // A writable mapping is counted, and dirties the file, before the lock (the file's lock
        // is never taken inside this one). The VMA's `Drop` lowers the count; a refusal below
        // undoes it here, since its VMA drops without its object.
        let writable = prot.contains(Protection::WRITE);
        if writable {
            FileObject::writable_mapped(&object);
        }
        let refused = |object: ObjectRef, e: MapError| {
            if writable {
                // SAFETY: `object` pins the `FileObject` the mapping counted.
                unsafe { FileObject::writable_unmapped(object.as_ptr()) };
            }
            Err((object, e))
        };
        let mut guard = self.inner.lock();
        if guard.vma_tree.find_first_overlapping(range).is_some() {
            drop(guard);
            return refused(object, MapError::Overlap);
        }
        boxed.object = Some(object);
        match guard.vma_tree.insert(boxed) {
            Ok(()) => Ok(()),
            Err(mut b) => {
                let object = b.object.take().expect("file-backed VMA carries its ObjectRef");
                drop(guard);
                refused(object, MapError::Overlap)
            }
        }
    }

    /// For a `FileBacked` fault at `addr`: the backing [`FileObject`](crate::object::FileObject)
    /// (a cloned `ObjectRef`) and the page index within the file; `None` if no file-backed VMA
    /// covers `addr` (a teardown race). A fresh lock acquisition, so the page cache is reached
    /// only **outside** this AS lock.
    pub fn file_backing(&self, addr: VirtAddr) -> Option<(ObjectRef, usize)> {
        let guard = self.inner.lock();
        let vma = guard.vma_tree.find_covering(addr)?;
        if vma.mapping != MappingKind::FileBacked {
            return None;
        }
        let object = vma.object.as_ref()?.clone();
        let page_base = addr.as_u64() & !(PAGE_SIZE as u64 - 1);
        let index = ((page_base - vma.range.start().as_u64()) / PAGE_SIZE as u64) as usize;
        Some((object, index))
    }

    /// Install the leaf PTE for the file-backed page at `addr` pointing at `frame`. Re-validates
    /// under the AS lock that a `FileBacked` VMA for the **same** `object` still covers `addr`
    /// (the fault may have raced an unmap or exit). `true` if the page is now present (installed
    /// here or by a racing fault); `false` if the VMA is gone or changed, or a page-table
    /// allocation failed. The object owns the frame, so `unmap` removes the PTE without freeing it.
    pub fn map_file_page(&self, addr: VirtAddr, object: &ObjectRef, frame: PhysAddr) -> bool {
        let guard = self.inner.lock();
        let Some(vma) = guard.vma_tree.find_covering(addr) else {
            return false;
        };
        if vma.mapping != MappingKind::FileBacked || vma.object.as_ref().map(|o| o.as_ptr()) != Some(object.as_ptr()) {
            return false;
        }
        let flags = page_flags_for(vma.prot, vma.caching);
        let root = guard.root;
        let page_base = VirtAddr::new(addr.as_u64() & !(PAGE_SIZE as u64 - 1));
        // SAFETY: `root` is this AS's PML4; `frame` is a live, resident cache frame
        // the file object owns and keeps alive (the VMA holds its `ObjectRef`);
        // `page_base` is page-aligned and in the validated user-half VMA range.
        match unsafe { Paging::map_page(root, page_base, frame, flags) } {
            Ok(()) | Err(ArchMapError::AlreadyMapped) => {
                // SAFETY: ring-0 `invlpg` evicting the not-present entry cached on
                // the fault we are servicing.
                unsafe { Paging::flush_tlb_page(page_base) };
                true
            }
            Err(ArchMapError::OutOfMemory) => false,
            Err(ArchMapError::Misaligned) => {
                unreachable!("map_file_page: Misaligned for a page-aligned address")
            }
        }
    }

    /// Whether `[start, end)` overlaps the stack guard gap. `MMAP_MAX` bounds only
    /// `find_free_range` (`hint = 0`); a hinted mapping otherwise validates just alignment and the
    /// user-half bound, so without this check a stack overrun could land in mapped memory.
    pub fn in_stack_guard(start: u64, end: u64) -> bool {
        start < USER_STACK_BOTTOM && end > USER_STACK_GUARD_BOTTOM
    }

    /// Lowest free range of `size` bytes in the "anywhere" window, for `sys_memory_map(hint=0)`.
    pub fn find_free_range(&self, size: u64) -> Option<VAddrRange> {
        let guard = self.inner.lock();
        guard.vma_tree.find_free_range(VirtAddr::new(MMAP_BASE), VirtAddr::new(MMAP_MAX), size)
    }

    /// Remove the VMA covering `addr`, uninstall its PTEs and free its anonymous frames.
    ///
    /// **Host tests only.** Freeing in place is wrong on SMP, where another CPU may still hold
    /// TLB translations; production uses [`unmap_covering_deferred`](Self::unmap_covering_deferred).
    #[cfg(test)]
    pub fn unmap_covering(&self, addr: VirtAddr) -> Option<KBox<Vma>> {
        let mut guard = self.inner.lock();
        let boxed = guard.vma_tree.remove_covering(addr)?;
        free_vma_pages(guard.root, &boxed);
        Some(boxed)
    }

    /// Remove the VMA covering `addr` and uninstall its PTEs, **deferring every frame release to
    /// the caller**: anonymous leaf frames go into `frames` (whose spare capacity must already
    /// hold them: nothing grows under the lock) and the returned [`Vma`] keeps its backing-object
    /// reference.
    ///
    /// The production unmap (`sys_memory_unmap`). Another CPU running this address space may
    /// still hold TLB translations for the range, so the caller must broadcast a TLB shootdown
    /// after this returns, and only then free the collected frames and drop the `Vma` (whose drop
    /// can release a `MemoryObject`'s frames).
    ///
    /// `Err(pages)`, tree and PTEs untouched, if `frames` has too little spare capacity; reserve
    /// outside the lock and retry.
    pub fn unmap_covering_deferred(
        &self,
        addr: VirtAddr,
        frames: &mut KVec<PhysAddr>,
    ) -> Result<Option<KBox<Vma>>, u64> {
        let mut guard = self.inner.lock();
        let pages = match guard.vma_tree.find_covering(addr) {
            Some(v) => v.range.pages(),
            None => return Ok(None),
        };
        if ((frames.capacity() - frames.len()) as u64) < pages {
            return Err(pages);
        }
        let boxed = guard.vma_tree.remove_covering(addr).expect("find_covering matched under the same hold");
        for i in 0..boxed.range.pages() {
            let virt = VirtAddr::new(boxed.range.start().as_u64() + i * (PAGE_SIZE as u64));
            // SAFETY: every page in the range was mapped by a prior `map_vma`
            // under this same AS lock; `guard.root` is the top-level table they
            // were installed into.
            let r = unsafe { Paging::unmap_page(guard.root, virt) };
            if let Ok(phys) = r
                && matches!(boxed.mapping, MappingKind::Anonymous)
            {
                frames.try_push(phys).expect("within the spare capacity checked above");
            }
        }
        Ok(Some(boxed))
    }
}

impl Drop for AddressSpace {
    /// Tear down every VMA, drop the empty tree, then free the PML4 frame. Intermediate
    /// page-table frames leak per the deferred decision.
    ///
    /// **`get_mut`, not `lock`, and that is a bug fix.** Dropping a VMA can drop the last
    /// reference to a `FileObject`, taking the *file's* lock; both are `LockRank::KernelObject`,
    /// so releasing a VMA under this guard is a same-rank acquisition and panics. `&mut self`
    /// means there is nothing to exclude anyway.
    fn drop(&mut self) {
        let guard = self.inner.get_mut();

        loop {
            let leftmost_start = {
                let mut it = guard.vma_tree.iter();
                let Some(v) = it.next() else { break };
                v.range.start()
            };
            let Some(boxed) = guard.vma_tree.remove_covering(leftmost_start) else {
                break;
            };
            free_vma_pages(guard.root, &boxed);
        }

        heap::buddy_free(guard.root, 0);
    }
}

/// Uninstall every PTE of a VMA's range and free each anonymous leaf frame.
fn free_vma_pages(root: PhysAddr, vma: &Vma) {
    let range = vma.range;
    for i in 0..range.pages() {
        let virt = VirtAddr::new(range.start().as_u64() + i * (PAGE_SIZE as u64));
        // SAFETY: every page in `range` was mapped by a prior `map_vma`
        // under the same AS lock; the PTEs exist now and `root` is the
        // valid top-level table they were installed into.
        let r = unsafe { Paging::unmap_page(root, virt) };
        if let Ok(phys) = r {
            match vma.mapping {
                MappingKind::Anonymous => heap::buddy_free(phys, 0),
                // The `MemoryObject` owns the frames and frees them on its last-ref drop.
                MappingKind::Object => {}
                // TODO(phase-2 page cache): file pages are owned by the page
                // cache, not this mapping, so (like Object) the PTE is removed
                // but the frame is not freed here. No FileBacked VMA is ever
                // constructed yet, so this arm is currently unreachable.
                MappingKind::FileBacked => {}
            }
        }
    }
}

/// Roll back a partial `map_vma`: uninstall `installed` PTEs from `start`, freeing each frame.
fn rollback_partial_map(root: PhysAddr, start: VirtAddr, installed: u64) {
    for i in 0..installed {
        let virt = VirtAddr::new(start.as_u64() + i * (PAGE_SIZE as u64));
        // SAFETY: we successfully installed exactly these `installed`
        // pages in `map_vma` immediately before this call.
        let r = unsafe { Paging::unmap_page(root, virt) };
        if let Ok(phys) = r {
            heap::buddy_free(phys, 0);
        }
    }
}

/// Roll back a partial `map_object`: uninstall `installed` PTEs from `start` but **do not free
/// the frames** (the `MemoryObject` owns them).
fn rollback_object_map(root: PhysAddr, start: VirtAddr, installed: u64) {
    for i in 0..installed {
        let virt = VirtAddr::new(start.as_u64() + i * (PAGE_SIZE as u64));
        // SAFETY: we successfully installed exactly these `installed` pages in
        // `map_object` immediately before this call. The returned phys is the
        // object's frame, which the object still owns — discard it.
        let _ = unsafe { Paging::unmap_page(root, virt) };
    }
}

/// Translate the VMA-level [`Protection`] and [`Caching`] into hardware [`PageFlags`].
/// `NO_EXECUTE` is inverted: hardware is executable by default, [`Protection`] non-executable.
///
/// The caching comes from the object via the VMA, so every mapping of one aperture asks for the
/// same thing (a write-back request over firmware-uncacheable memory made `/dev/framebuffer`
/// mappings far slower). Public because the boot's `report_framebuffer_cost` measures through it,
/// and a measurement with its own copy would pass with this translation deleted.
pub fn page_flags_for(prot: Protection, caching: Caching) -> PageFlags {
    let mut f = PageFlags::empty();
    if prot.contains(Protection::WRITE) {
        f = f | PageFlags::WRITABLE;
    }
    if !prot.contains(Protection::EXEC) {
        f = f | PageFlags::NO_EXECUTE;
    }
    if prot.contains(Protection::USER) {
        f = f | PageFlags::USER;
    }
    if caching == Caching::WriteCombining {
        f = f | PageFlags::WRITE_COMBINING;
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mapping_window_stops_below_the_guard_gap() {
        const { assert!(USER_STACK_GUARD_SIZE > 0, "a zero-width gap guards nothing") };
        assert_eq!(USER_STACK_GUARD_BOTTOM + USER_STACK_GUARD_SIZE, USER_STACK_BOTTOM);
        assert_eq!(MMAP_MAX, USER_STACK_GUARD_BOTTOM);
        const { assert!(MMAP_MAX + USER_STACK_GUARD_SIZE <= USER_STACK_BOTTOM) };
        const { assert!(MMAP_BASE < MMAP_MAX, "the window must not be empty") };
    }

    #[test]
    fn in_stack_guard_catches_overlap_not_adjacency() {
        let page = PAGE_SIZE as u64;
        // Inside, straddling either edge, and spanning the gap with neither endpoint in it (what an
        // endpoint test misses); then ending at its first byte or starting at the stack's is clear.
        assert!(AddressSpace::in_stack_guard(USER_STACK_GUARD_BOTTOM + page, USER_STACK_GUARD_BOTTOM + 2 * page));
        assert!(AddressSpace::in_stack_guard(USER_STACK_GUARD_BOTTOM - page, USER_STACK_BOTTOM));
        assert!(AddressSpace::in_stack_guard(USER_STACK_BOTTOM - page, USER_STACK_BOTTOM + page));
        assert!(AddressSpace::in_stack_guard(USER_STACK_GUARD_BOTTOM - page, USER_STACK_BOTTOM + page));

        assert!(!AddressSpace::in_stack_guard(USER_STACK_GUARD_BOTTOM - page, USER_STACK_GUARD_BOTTOM));
        assert!(!AddressSpace::in_stack_guard(USER_STACK_BOTTOM, USER_STACK_BOTTOM + page));
        assert!(!AddressSpace::in_stack_guard(MMAP_BASE, MMAP_BASE + page));
    }

    #[test]
    fn the_stack_and_guard_are_the_sizes_the_rationale_claims() {
        assert_eq!(DEFAULT_USER_STACK_SIZE, 8 * 1024 * 1024);
        assert_eq!(USER_STACK_GUARD_SIZE, 256 * (PAGE_SIZE as u64));
        assert_eq!(DEFAULT_USER_STACK_SIZE % PAGE_SIZE as u64, 0);
        assert_eq!(USER_STACK_GUARD_SIZE % PAGE_SIZE as u64, 0);
        assert_eq!(USER_STACK_GUARD_BOTTOM % PAGE_SIZE as u64, 0);
    }
    use crate::arch::Paging;
    use crate::arch::paging::ArchPaging;
    use crate::mm::test_support::init_global_heap;
    use crate::mm::vmm::VAddrRange;

    const PAGE: u64 = PAGE_SIZE as u64;

    fn va(v: u64) -> VirtAddr {
        VirtAddr::new(v)
    }

    fn range(start: u64, end: u64) -> VAddrRange {
        VAddrRange::new(va(start), va(end)).expect("test range must be valid")
    }

    fn anon_box(r: VAddrRange, prot: Protection) -> KBox<Vma> {
        KBox::try_new(Vma::new(r, prot, MappingKind::Anonymous)).expect("test heap exhausted")
    }

    #[test]
    fn new_yields_empty_address_space_with_real_root() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        assert!(asp.is_empty());
        assert_eq!(asp.len(), 0);
        let root = asp.root();
        assert!(root.is_page_aligned());
        assert_ne!(root.as_u64(), 0);
    }

    #[test]
    fn map_single_page_installs_pte_translate_finds_it() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        let r = range(PAGE * 4, PAGE * 5);
        asp.map_vma(anon_box(r, Protection::WRITE | Protection::USER)).expect("map must succeed");

        // SAFETY: translate is read-only against the live tables we
        // just populated. The root is owned by `asp`.
        let phys = unsafe { Paging::translate(asp.root(), va(PAGE * 4)) };
        assert!(phys.is_some());
        // SAFETY: `asp`'s root is the live page table this test built.
        let beyond = unsafe { Paging::translate(asp.root(), va(PAGE * 5)) };
        assert!(beyond.is_none());
        assert_eq!(asp.len(), 1);
    }

    #[test]
    fn map_multi_page_installs_every_pte() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        let r = range(PAGE * 8, PAGE * 16);
        asp.map_vma(anon_box(r, Protection::WRITE | Protection::USER)).expect("map must succeed");

        for i in 8..16 {
            // SAFETY: `asp`'s root is the live page table this test built.
            let p = unsafe { Paging::translate(asp.root(), va(PAGE * i)) };
            assert!(p.is_some(), "page {i} not mapped");
        }
        // SAFETY: `asp`'s root is the live page table this test built.
        let before = unsafe { Paging::translate(asp.root(), va(PAGE * 7)) };
        // SAFETY: `asp`'s root is the live page table this test built.
        let after = unsafe { Paging::translate(asp.root(), va(PAGE * 16)) };
        assert!(before.is_none());
        assert!(after.is_none());
    }

    #[test]
    fn map_then_unmap_removes_the_ptes() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        let r = range(PAGE * 4, PAGE * 8);
        asp.map_vma(anon_box(r, Protection::WRITE | Protection::USER)).expect("map must succeed");

        let removed = asp.unmap_covering(va(PAGE * 5)).expect("unmap must find covering vma");
        assert_eq!(removed.range, r);
        assert!(asp.is_empty());

        for i in 4..8 {
            // SAFETY: `asp`'s root is the live page table this test built.
            let p = unsafe { Paging::translate(asp.root(), va(PAGE * i)) };
            assert!(p.is_none(), "page {i} still mapped after unmap");
        }
    }

    #[test]
    fn map_rejects_overlap_with_existing_vma() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        asp.map_vma(anon_box(range(PAGE * 4, PAGE * 8), Protection::WRITE | Protection::USER))
            .expect("first map must succeed");

        let err = asp
            .map_vma(anon_box(range(PAGE * 6, PAGE * 10), Protection::WRITE | Protection::USER))
            .expect_err("overlapping map must fail");
        assert_eq!(err.1, MapError::Overlap);
        assert_eq!(err.0.range, range(PAGE * 6, PAGE * 10));

        // SAFETY: `asp`'s root is the live page table this test built.
        assert!(unsafe { Paging::translate(asp.root(), va(PAGE * 4)) }.is_some());
        // SAFETY: `asp`'s root is the live page table this test built.
        assert!(unsafe { Paging::translate(asp.root(), va(PAGE * 9)) }.is_none());
        assert_eq!(asp.len(), 1);
    }

    #[test]
    fn map_rejects_kernel_half_range() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        let r = VAddrRange::new(va(USER_VIRT_END - PAGE), va(USER_VIRT_END + PAGE)).unwrap();
        let err = asp
            .map_vma(anon_box(r, Protection::WRITE | Protection::USER))
            .expect_err("kernel-half range must be rejected");
        assert!(matches!(err.1, MapError::NotUserHalf | MapError::NotCanonical));
    }

    #[test]
    fn unmap_on_address_with_no_vma_returns_none() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        assert!(asp.unmap_covering(va(PAGE * 100)).is_none());
        asp.map_vma(anon_box(range(PAGE * 4, PAGE * 8), Protection::WRITE | Protection::USER)).unwrap();
        assert!(asp.unmap_covering(va(PAGE * 20)).is_none());
        assert_eq!(asp.len(), 1);
    }

    #[test]
    fn drop_tears_down_populated_address_space() {
        // Drop must free the leaf frames and the PML4: eight iterations of 16 pages would
        // otherwise exhaust the 16 MiB test heap.
        init_global_heap();
        for _ in 0..8 {
            let asp = AddressSpace::new().expect("new must succeed");
            for i in 0..4u64 {
                let start = (i * 8) * PAGE;
                asp.map_vma(anon_box(range(start, start + PAGE * 4), Protection::WRITE | Protection::USER))
                    .expect("map must succeed");
            }
        }
    }

    use crate::object::header::test_probe;

    fn into_obj(m: KBox<MemoryObject>) -> ObjectRef {
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        unsafe { ObjectRef::from_raw(KBox::into_raw(m).as_ptr() as *mut (), KObjectType::MemoryObject) }
    }

    fn uprot() -> Protection {
        Protection::WRITE | Protection::USER
    }

    #[test]
    fn map_object_points_ptes_at_object_frames() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let m = MemoryObject::try_new(PAGE as usize * 3).unwrap();
        let frames = [m.frames()[0], m.frames()[1], m.frames()[2]];
        let obj = into_obj(m);

        asp.map_object(range(PAGE * 4, PAGE * 7), uprot(), obj).expect("map_object must succeed");

        for i in 0..3u64 {
            // SAFETY: `asp`'s root is the live page table this test built.
            let phys = unsafe { Paging::translate(asp.root(), va(PAGE * (4 + i))) }.expect("page must be mapped");
            assert_eq!(phys, frames[i as usize], "page {i} not pointing at object frame");
        }
        assert_eq!(asp.len(), 1);
    }

    #[test]
    fn map_object_twice_aliases_same_frame() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let m = MemoryObject::try_new(PAGE as usize).unwrap();
        let frame0 = m.frames()[0];
        let obj = into_obj(m);
        let obj2 = obj.clone(); // second mapping needs its own reference

        asp.map_object(range(PAGE * 4, PAGE * 5), uprot(), obj).unwrap();
        asp.map_object(range(PAGE * 8, PAGE * 9), uprot(), obj2).unwrap();

        // SAFETY: `asp`'s root is the live page table this test built.
        let p1 = unsafe { Paging::translate(asp.root(), va(PAGE * 4)) }.unwrap();
        // SAFETY: `asp`'s root is the live page table this test built.
        let p2 = unsafe { Paging::translate(asp.root(), va(PAGE * 8)) }.unwrap();
        assert_eq!(p1, frame0);
        assert_eq!(p2, frame0);
        assert_eq!(p1, p2, "two mappings of one object must alias the same frame");
    }

    #[test]
    fn unmap_object_mapping_does_not_free_the_objects_frames() {
        init_global_heap();
        test_probe::reset();
        let asp = AddressSpace::new().unwrap();
        let m = MemoryObject::try_new(PAGE as usize).unwrap();
        let obj = into_obj(m);
        let keep = obj.clone();

        asp.map_object(range(PAGE * 4, PAGE * 5), uprot(), obj).unwrap();
        let removed = asp.unmap_covering(va(PAGE * 4)).expect("unmap must find the covering vma");
        // SAFETY: `asp`'s root is the live page table this test built.
        assert!(unsafe { Paging::translate(asp.root(), va(PAGE * 4)) }.is_none());
        drop(removed);
        assert_eq!(test_probe::memory_object_destroys(), 0, "unmap freed shared frames");
        drop(keep);
        assert_eq!(test_probe::memory_object_destroys(), 1);
    }

    #[test]
    fn find_free_range_picks_a_gap_in_the_mmap_window() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let r = asp.find_free_range(PAGE).expect("empty AS has room");
        assert_eq!(r.start().as_u64(), MMAP_BASE);
        assert_eq!(r.len(), PAGE);
    }

    use crate::object::{FileObject, Producer, Reserve};

    fn into_file(f: KBox<FileObject>) -> ObjectRef {
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        unsafe { ObjectRef::from_raw(KBox::into_raw(f).as_ptr() as *mut (), KObjectType::FileObject) }
    }

    /// **Dropping an address space must release its file-backed VMAs without a lock-order
    /// violation** (`Drop` once held its own `KernelObject` lock across the release). This test
    /// does **not** catch that: the rank tracker is inert under `cfg(test)`, so no host test can
    /// catch a lock-order violation; the reproduction is `cargo xtask check-terminal`. It pins
    /// that the last holder of a file mapping releases the file when it drops.
    #[test]
    fn dropping_an_address_space_releases_a_file_mapping_it_last_holds() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        test_probe::reset();
        let obj = into_file(FileObject::try_new(3 * PAGE as usize, Producer::Stub { base: 0 }).unwrap());
        asp.map_file(range(PAGE * 4, PAGE * 7), uprot(), obj).expect("map_file must succeed");
        assert_eq!(test_probe::file_object_destroys(), 0, "the mapping holds the file alive");
        drop(asp);
        assert_eq!(test_probe::file_object_destroys(), 1, "dropping the AS released the file");
    }

    #[test]
    fn map_file_reserves_lazily_with_no_ptes() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let obj = into_file(FileObject::try_new(3 * PAGE as usize, Producer::Stub { base: 0 }).unwrap());
        asp.map_file(range(PAGE * 4, PAGE * 7), uprot(), obj).expect("map_file must succeed");
        assert_eq!(asp.len(), 1, "the VMA is reserved");
        for i in 0..3u64 {
            assert!(!is_mapped(&asp, va(PAGE * (4 + i))), "page {i} must be lazy");
        }
    }

    #[test]
    fn fault_in_file_backed_signals_and_file_backing_resolves() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let obj = into_file(FileObject::try_new(4 * PAGE as usize, Producer::Stub { base: 0 }).unwrap());
        let obj_ptr = obj.as_ptr();
        asp.map_file(range(PAGE * 4, PAGE * 8), uprot(), obj).unwrap();

        assert_eq!(asp.fault_in(va(PAGE * 5), FaultAccess::Read), FaultIn::FileBacked);

        for (vma_page, want_index) in [(4u64, 0usize), (5, 1), (7, 3)] {
            let (got_obj, index) = asp.file_backing(va(PAGE * vma_page)).expect("covered by the file mapping");
            assert_eq!(got_obj.as_ptr(), obj_ptr, "same backing object");
            assert_eq!(index, want_index, "page index for vma page {vma_page}");
        }
        assert!(asp.file_backing(va(PAGE * 2)).is_none());
    }

    #[test]
    fn map_file_page_installs_pte_for_a_ready_frame() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let f = FileObject::try_new(4 * PAGE as usize, Producer::Stub { base: 0 }).unwrap();
        let frame = match f.reserve(1) {
            Reserve::New(frame) => frame,
            other => panic!("expected New, got {other:?}"),
        };
        f.mark_ready(1);
        let obj = into_file(f);
        asp.map_file(range(PAGE * 4, PAGE * 8), uprot(), obj.clone()).unwrap();

        assert!(asp.map_file_page(va(PAGE * 5), &obj, frame));
        // SAFETY: `asp`'s root is the live page table this test built.
        let phys = unsafe { Paging::translate(asp.root(), va(PAGE * 5)) }.expect("page must now be mapped");
        assert_eq!(phys, frame, "PTE points at the cache frame");
        assert!(asp.map_file_page(va(PAGE * 5), &obj, frame));
    }

    #[test]
    fn map_file_page_rejects_wrong_object_or_unmapped() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let f = FileObject::try_new(PAGE as usize, Producer::Stub { base: 0 }).unwrap();
        let frame = match f.reserve(0) {
            Reserve::New(frame) => frame,
            other => panic!("{other:?}"),
        };
        f.mark_ready(0);
        let obj = into_file(f);
        asp.map_file(range(PAGE * 4, PAGE * 5), uprot(), obj.clone()).unwrap();

        let other = into_file(FileObject::try_new(PAGE as usize, Producer::Stub { base: 0 }).unwrap());
        assert!(!asp.map_file_page(va(PAGE * 4), &other, frame));
        assert!(!asp.map_file_page(va(PAGE * 99), &obj, frame));
        assert!(asp.map_file_page(va(PAGE * 4), &obj, frame));
    }

    #[test]
    fn unmap_file_mapping_keeps_the_objects_frames() {
        init_global_heap();
        test_probe::reset();
        let asp = AddressSpace::new().unwrap();
        let f = FileObject::try_new(PAGE as usize, Producer::Stub { base: 0 }).unwrap();
        let frame = match f.reserve(0) {
            Reserve::New(frame) => frame,
            other => panic!("{other:?}"),
        };
        f.mark_ready(0);
        let obj = into_file(f);
        let keep = obj.clone();
        asp.map_file(range(PAGE * 4, PAGE * 5), uprot(), obj).unwrap();
        asp.map_file_page(va(PAGE * 4), &keep, frame);
        assert!(is_mapped(&asp, va(PAGE * 4)));

        let removed = asp.unmap_covering(va(PAGE * 4)).expect("covering vma");
        assert!(!is_mapped(&asp, va(PAGE * 4)), "PTE removed");
        drop(removed);
        assert_eq!(test_probe::file_object_destroys(), 0, "object still referenced");
        drop(keep);
        assert_eq!(test_probe::file_object_destroys(), 1);
    }

    /// A writable mapping is counted for as long as its VMA exists, however it goes; a refused
    /// map counts nothing and a read-only mapping is never counted.
    #[test]
    fn a_writable_file_mapping_is_counted_while_its_vma_exists() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let obj = into_file(FileObject::try_new(PAGE as usize, Producer::Stub { base: 0 }).unwrap());
        let fo = || {
            // SAFETY: `obj` pins a live `FileObject` for the whole test.
            unsafe { &*(obj.as_ptr() as *const FileObject) }
        };
        asp.map_file(range(PAGE * 4, PAGE * 5), uprot(), obj.clone()).unwrap();
        assert_eq!(fo().writable_maps(), 1);
        let (back, _) = asp.map_file(range(PAGE * 4, PAGE * 5), uprot(), obj.clone()).unwrap_err();
        drop(back);
        assert_eq!(fo().writable_maps(), 1, "a refused map leaves no count behind");
        asp.map_file(range(PAGE * 8, PAGE * 9), Protection::USER, obj.clone()).unwrap();
        assert_eq!(fo().writable_maps(), 1);
        drop(asp.unmap_covering(va(PAGE * 4)).unwrap());
        assert_eq!(fo().writable_maps(), 0);
        let asp2 = AddressSpace::new().unwrap();
        asp2.map_file(range(PAGE * 4, PAGE * 5), uprot(), obj.clone()).unwrap();
        assert_eq!(fo().writable_maps(), 1);
        drop(asp2);
        assert_eq!(fo().writable_maps(), 0);
        drop(asp);
    }

    fn is_mapped(asp: &AddressSpace, v: VirtAddr) -> bool {
        // SAFETY: read-only walk of `asp`'s own page tables.
        unsafe { Paging::translate(asp.root(), v) }.is_some()
    }

    #[test]
    fn map_vma_lazy_installs_no_ptes() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let r = range(PAGE * 4, PAGE * 8);
        asp.map_vma_lazy(anon_box(r, Protection::WRITE | Protection::USER)).expect("lazy reserve must succeed");
        assert_eq!(asp.len(), 1);
        for i in 4..8 {
            assert!(!is_mapped(&asp, va(PAGE * i)), "page {i} unexpectedly backed");
        }
    }

    #[test]
    fn fault_in_backs_one_anonymous_page_on_demand() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let r = range(PAGE * 4, PAGE * 8);
        asp.map_vma_lazy(anon_box(r, Protection::WRITE | Protection::USER)).unwrap();

        let outcome = asp.fault_in(va(PAGE * 5 + 0x40), FaultAccess::Write);
        assert_eq!(outcome, FaultIn::Mapped);
        assert!(is_mapped(&asp, va(PAGE * 5)), "faulted page not backed");
        assert!(!is_mapped(&asp, va(PAGE * 4)));
        assert!(!is_mapped(&asp, va(PAGE * 6)));
    }

    #[test]
    fn fault_in_with_no_covering_vma_is_no_vma() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        assert_eq!(asp.fault_in(va(PAGE * 9), FaultAccess::Read), FaultIn::NoVma);
    }

    #[test]
    fn fault_in_write_to_readonly_is_protection() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        asp.map_vma_lazy(anon_box(range(PAGE * 4, PAGE * 5), Protection::USER)).unwrap();
        assert_eq!(asp.fault_in(va(PAGE * 4), FaultAccess::Write), FaultIn::Protection);
        assert!(!is_mapped(&asp, va(PAGE * 4)));
        assert_eq!(asp.fault_in(va(PAGE * 4), FaultAccess::Read), FaultIn::Mapped);
        assert!(is_mapped(&asp, va(PAGE * 4)));
    }

    #[test]
    fn fault_in_exec_from_nonexec_is_protection() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        asp.map_vma_lazy(anon_box(range(PAGE * 4, PAGE * 5), Protection::WRITE | Protection::USER)).unwrap();
        assert_eq!(asp.fault_in(va(PAGE * 4), FaultAccess::Execute), FaultIn::Protection);
        assert!(!is_mapped(&asp, va(PAGE * 4)));
    }

    #[test]
    fn dropping_a_partially_faulted_lazy_as_frees_only_backed_frames() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let r = range(PAGE * 4, PAGE * 8);
        asp.map_vma_lazy(anon_box(r, Protection::WRITE | Protection::USER)).unwrap();
        assert_eq!(asp.fault_in(va(PAGE * 4), FaultAccess::Write), FaultIn::Mapped);
        assert_eq!(asp.fault_in(va(PAGE * 6), FaultAccess::Write), FaultIn::Mapped);

        let removed = asp.unmap_covering(va(PAGE * 5)).expect("covering vma");
        assert_eq!(removed.range, r);
        assert!(asp.is_empty());
        for i in 4..8 {
            assert!(!is_mapped(&asp, va(PAGE * i)), "page {i} still mapped");
        }
    }

    #[test]
    fn fault_in_is_idempotent_friendly_across_a_lazy_drop() {
        init_global_heap();
        for _ in 0..8 {
            let asp = AddressSpace::new().unwrap();
            asp.map_vma_lazy(anon_box(range(PAGE * 4, PAGE * 12), Protection::WRITE | Protection::USER)).unwrap();
            for i in 4..12u64 {
                assert_eq!(asp.fault_in(va(PAGE * i), FaultAccess::Write), FaultIn::Mapped);
            }
        }
    }

    /// The write-combining branch of `page_flags_for` once went untested while every gate stayed
    /// green.
    #[test]
    fn a_write_combining_mapping_asks_for_it_and_an_ordinary_one_asks_for_nothing() {
        let prot = Protection::WRITE | Protection::USER;
        let normal = page_flags_for(prot, Caching::Normal);
        let wc = page_flags_for(prot, Caching::WriteCombining);
        assert!(!normal.contains(PageFlags::WRITE_COMBINING), "ordinary memory asks for nothing");
        assert!(wc.contains(PageFlags::WRITE_COMBINING));
        assert!(wc.contains(PageFlags::WRITABLE) && wc.contains(PageFlags::USER));
        assert_eq!(
            normal.contains(PageFlags::NO_EXECUTE),
            wc.contains(PageFlags::NO_EXECUTE),
            "caching does not decide executability"
        );
    }

    /// End to end: an aperture object saying write-combining is mapped with the matching entry,
    /// an ordinary one is not.
    #[test]
    fn mapping_an_aperture_object_selects_the_write_combining_entry() {
        init_global_heap();
        const APERTURE: u64 = 0xF000_0000;
        let asp = AddressSpace::new().unwrap();

        // SAFETY: a physical range no test reads or frees; only its page-table entries are read.
        let device = unsafe {
            MemoryObject::try_new_borrowed(PhysAddr::new(APERTURE), PAGE as usize, Caching::WriteCombining).unwrap()
        };
        asp.map_object(range(PAGE * 4, PAGE * 5), uprot(), into_obj(device)).expect("map_object must succeed");
        let ordinary = MemoryObject::try_new(PAGE as usize).unwrap();
        asp.map_object(range(PAGE * 8, PAGE * 9), uprot(), into_obj(ordinary)).expect("map_object must succeed");

        // SAFETY: `asp`'s root is the live page table this test built.
        let index = |v: u64| unsafe { Paging::attribute_index(asp.root(), va(v)) };
        // Entry 5 is write-combining and entry 0 write-back in the table the kernel programs.
        assert_eq!(index(PAGE * 4), Some(5), "the aperture's mapping");
        assert_eq!(index(PAGE * 8), Some(0), "ordinary memory's");
        assert_eq!(index(PAGE * 16), None, "nothing is mapped there");
    }
}
