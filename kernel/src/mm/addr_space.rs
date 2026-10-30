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
use crate::arch::abi::{DEFAULT_USER_STACK_SIZE, DEFAULT_USER_STACK_TOP, USER_MAP_END, USER_STACK_GUARD_SIZE};
use crate::arch::paging::{ArchPaging, MapError as ArchMapError, PageFlags};
use crate::libkern::handle::KObjectType;
use crate::libkern::lockrank::LockRank;
use crate::libkern::{AllocError, KBox, KVec, SpinLock, SpinLockGuard};
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
    /// The range falls outside what a user mapping may cover, `[0, USER_MAP_END)`.
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
    /// rejection. `&mut`, as only a space no CPU has loaded may be filled this way: a failure
    /// frees the frames it mapped with no shootdown.
    pub fn map_vma(&mut self, boxed: KBox<Vma>) -> Result<(), (KBox<Vma>, MapError)> {
        let range = boxed.range;

        // Both endpoints must be canonical; test `end - 1` since the end is exclusive.
        let last_byte = VirtAddr::new(range.end().as_u64() - 1);
        if !range.start().is_canonical() || !last_byte.is_canonical() {
            return Err((boxed, MapError::NotCanonical));
        }
        if range.end().as_u64() > USER_MAP_END {
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
        if range.end().as_u64() > USER_MAP_END {
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
                // address, in the validated user-half VMA range. A present PTE
                // is refused, not aliased.
                match unsafe { Paging::map_page(root, page_base, phys, flags) } {
                    Ok(()) => crate::mm::record_demand_fault(),
                    Err(ArchMapError::OutOfMemory) => {
                        heap::buddy_free(phys, 0);
                        return FaultIn::Oom;
                    }
                    // Another CPU faulted on the same page and mapped it first, between this
                    // CPU's fault and the lock: the page is there, so the access can be retried.
                    Err(ArchMapError::AlreadyMapped) => heap::buddy_free(phys, 0),
                    Err(ArchMapError::Misaligned) => {
                        heap::buddy_free(phys, 0);
                        unreachable!("fault_in: map_page called Misaligned a page-aligned address");
                    }
                }
                // SAFETY: ring-0 `invlpg` evicting the not-present entry the
                // CPU cached for this page on the fault we are servicing.
                unsafe {
                    Paging::flush_tlb_page(page_base);
                }
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
        if range.end().as_u64() > USER_MAP_END {
            return Err((object, MapError::NotUserHalf));
        }

        assert_eq!(object.object_type(), KObjectType::MemoryObject, "map_object");
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
                    rollback_object_map(guard, range.start(), installed);
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
                rollback_object_map(guard, range.start(), installed);
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
        assert_eq!(object.object_type(), KObjectType::FileObject, "map_file");
        let last_byte = VirtAddr::new(range.end().as_u64() - 1);
        if !range.start().is_canonical() || !last_byte.is_canonical() {
            return Err((object, MapError::NotCanonical));
        }
        if range.end().as_u64() > USER_MAP_END {
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

    /// [`unmap_covering`](Self::unmap_covering), reserving as it asks. **Host tests only.**
    #[cfg(test)]
    pub fn unmap_at(&self, addr: VirtAddr) -> Option<Unmapped> {
        let mut frames = KVec::new();
        loop {
            match self.unmap_covering(addr, frames) {
                Ok(unmapped) => return unmapped,
                Err((mut f, pages)) => {
                    f.try_reserve(pages as usize).expect("a host test's reserve");
                    frames = f;
                }
            }
        }
    }

    /// Remove the VMA covering `addr` and uninstall its PTEs. The anonymous frames, collected in
    /// `frames`, and the VMA, keeping its backing object, come back as an [`Unmapped`], released
    /// only after a shootdown. The production unmap (`sys_memory_unmap`).
    ///
    /// `frames`' spare capacity must hold the frames, as nothing grows under the lock: if it is
    /// too small, `frames` comes back with the pages needed, tree and PTEs untouched, to reserve
    /// outside the lock and retry.
    pub fn unmap_covering(
        &self,
        addr: VirtAddr,
        mut frames: KVec<PhysAddr>,
    ) -> Result<Option<Unmapped>, (KVec<PhysAddr>, u64)> {
        let mut guard = self.inner.lock();
        // Only an anonymous mapping's frames come back; reserving for a mapped 2 GiB file would
        // ask for more than the largest allocation and refuse the unmap.
        let pages = match guard.vma_tree.find_covering(addr) {
            Some(v) if matches!(v.mapping, MappingKind::Anonymous) => v.range.pages(),
            Some(_) => 0,
            None => return Ok(None),
        };
        if ((frames.capacity() - frames.len()) as u64) < pages {
            return Err((frames, pages));
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
        Ok(Some(Unmapped { frames, vma: Some(boxed) }))
    }
}

/// What an unmap took out of a live address space's page tables: anonymous frames, and a VMA
/// whose drop can free its object's. Dropping it shoots them down on every CPU, then releases
/// them, so it must be dropped outside every lock, as a shootdown requires.
pub struct Unmapped {
    frames: KVec<PhysAddr>,
    vma: Option<KBox<Vma>>,
}

impl Unmapped {
    /// The VMA unmapped, if a VMA was.
    pub fn vma(&self) -> Option<&Vma> {
        self.vma.as_deref()
    }
}

impl Drop for Unmapped {
    fn drop(&mut self) {
        crate::tlb::shootdown_all();
        self.frames.iter().for_each(|&f| heap::buddy_free(f, 0));
        drop(self.vma.take());
    }
}

impl Drop for AddressSpace {
    /// Tear down every VMA, drop the empty tree, then free the PML4 frame. Intermediate
    /// page-table frames leak per the deferred decision. No shootdown: no CPU has the space
    /// loaded, as a switch loads the next thread's root before the last one here is reaped.
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

/// Roll back a partial `map_vma`: uninstall `installed` PTEs from `start`, freeing each frame, with
/// no shootdown, as `map_vma` fills only a space no CPU has loaded.
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
/// the frames** (the `MemoryObject` owns them), then release the lock and shoot them down, if any
/// page was mapped. The space is live, so the object may be released only once this returns.
fn rollback_object_map(guard: SpinLockGuard<'_, Inner>, start: VirtAddr, installed: u64) {
    let root = guard.root;
    for i in 0..installed {
        let virt = VirtAddr::new(start.as_u64() + i * (PAGE_SIZE as u64));
        // SAFETY: we successfully installed exactly these `installed` pages in
        // `map_object` immediately before this call. The returned phys is the
        // object's frame, which the object still owns — discard it.
        let _ = unsafe { Paging::unmap_page(root, virt) };
    }
    let unmapped = (installed > 0).then(|| Unmapped { frames: KVec::new(), vma: None });
    drop(guard);
    drop(unmapped);
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
mod tests;
