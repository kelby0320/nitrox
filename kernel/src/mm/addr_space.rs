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

use crate::arch::Paging;
use crate::arch::abi::{DEFAULT_USER_STACK_SIZE, DEFAULT_USER_STACK_TOP, USER_MAP_END, USER_STACK_GUARD_SIZE};
use crate::arch::paging::{ArchPaging, MapError as ArchMapError, PageFlags};
use crate::libkern::lockrank::LockRank;
use crate::libkern::{AllocError, KBox, KVec, SpinLock, SpinLockGuard};
use crate::mm::frame::{Frame, FrameView};
use crate::mm::page_tables::UserTables;
use crate::mm::vmm::{Backing, FaultAccess, Protection, VAddrRange, Vma, VmaTree};
use crate::mm::{Caching, PAGE_SIZE, PhysAddr, VirtAddr};
use crate::object::header::Ref;
use crate::object::{FileObject, MemoryObject};

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

/// **The page an "anywhere" mapping keeps unmapped below itself and the mapping above**: a stack's.
const MMAP_GAP: u64 = PAGE_SIZE as u64;

/// **Where a mapping goes**, placed under the hold that inserts it, so two at once never meet.
#[derive(Copy, Clone, Debug)]
pub enum Placement {
    /// Exactly this range.
    At(VAddrRange),
    /// This many bytes, page-aligned, anywhere in the window, [`MMAP_GAP`] free on either side.
    Anywhere(u64),
}

impl From<VAddrRange> for Placement {
    fn from(range: VAddrRange) -> Placement {
        Placement::At(range)
    }
}

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
    /// No range of the size asked for is free in the "anywhere" window.
    NoRoom,
}

/// What [`AddressSpace::map_file_page`] did with a filled page.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FilePage {
    /// The page is mapped: installed here, or by a racing fault.
    Mapped,
    /// **The mapping there no longer puts that page there**: nothing installed; the access retries.
    Changed,
    /// A page-table allocation failed.
    NoMemory,
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
    /// A [`File`](Backing::File) not-present fault. `fault_in` only *signals*:
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
    /// Freed after the tree, as the fields drop, `Drop` having torn down the VMAs.
    tables: UserTables,
}

impl AddressSpace {
    /// Build an empty address space: a zeroed PML4 frame inheriting the kernel-half entries from
    /// the boot template, and an empty VMA tree. Panics before `KernelTables::adopt`.
    pub fn new() -> Result<Self, AllocError> {
        let tables = UserTables::new()?;
        Ok(AddressSpace { inner: SpinLock::new(LockRank::KernelObject, Inner { vma_tree: VmaTree::new(), tables }) })
    }

    /// **Copy `src` into the anonymous memory mapped from `at`**, as the loader writes an image;
    /// `false`, what came before written, at a page no anonymous mapping has present.
    pub fn copy_in(&mut self, at: VirtAddr, src: &[u8]) -> bool {
        let inner = self.inner.get_mut();
        let mut done = 0;
        while done < src.len() {
            let Some(va) = at.as_u64().checked_add(done as u64).map(VirtAddr::new) else { return false };
            let anonymous = inner.vma_tree.find_covering(va).is_some_and(|v| matches!(v.backing, Backing::Anonymous));
            let Some(phys) = anonymous.then(|| inner.tables.translate(va)).flatten() else {
                return false;
            };
            // SAFETY: an anonymous frame mapped here, which this space owns and frees only at an
            // unmap or its drop, both of which `&mut self` rules out for the copy.
            let page = unsafe { FrameView::new(PhysAddr::new(phys.as_u64() & !(PAGE_SIZE as u64 - 1))) };
            let at = phys.as_u64() as usize % PAGE_SIZE;
            let n = (PAGE_SIZE - at).min(src.len() - done);
            page.write(at, &src[done..done + n]);
            done += n;
        }
        true
    }

    /// Physical base of this address space's top-level page table.
    pub fn root(&self) -> PhysAddr {
        self.inner.lock().tables.root()
    }

    /// The address `va` maps to here, or `None`.
    pub fn translate(&self, va: VirtAddr) -> Option<PhysAddr> {
        self.inner.lock().tables.translate(va)
    }

    /// `f` of this space's page tables, under its lock. **Host tests only.**
    #[cfg(test)]
    pub(crate) fn with_tables<R>(&self, f: impl FnOnce(&UserTables) -> R) -> R {
        f(&self.inner.lock().tables)
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
        if let Err(e) = user_placement(Placement::At(range)) {
            return Err((boxed, e));
        }

        let mut guard = self.inner.lock();

        // Pre-check overlap so a doomed insert runs no frame allocations to roll back.
        if guard.vma_tree.find_first_overlapping(range).is_some() {
            return Err((boxed, MapError::Overlap));
        }

        let flags = page_flags_for(boxed.prot, Caching::Normal);
        let total_pages = range.pages();
        let mut installed: u64 = 0;

        for i in 0..total_pages {
            let virt = VirtAddr::new(range.start().as_u64() + i * (PAGE_SIZE as u64));

            let Some(frame) = Frame::alloc_zeroed() else {
                rollback_partial_map(&mut guard.tables, range.start(), installed);
                return Err((boxed, MapError::OutOfMemory));
            };

            match guard.tables.map_owned(virt, frame, flags) {
                Ok(()) => installed += 1,
                Err((_, ArchMapError::OutOfMemory)) => {
                    rollback_partial_map(&mut guard.tables, range.start(), installed);
                    return Err((boxed, MapError::OutOfMemory));
                }
                Err((_, ArchMapError::AlreadyMapped)) => {
                    // Impossible: AS lock held and overlap pre-checked.
                    unreachable!("ArchPaging::map_page returned AlreadyMapped after VmaTree overlap pre-check");
                }
                Err((_, e @ (ArchMapError::Misaligned | ArchMapError::NotUser | ArchMapError::Unshared))) => {
                    // Impossible: `VAddrRange` enforces page alignment.
                    unreachable!("map_vma: {e:?} for a page-aligned user-half address");
                }
            }
        }

        // Commit. The insert cannot reject: the lock is held and overlap was checked above.
        match guard.vma_tree.insert(boxed) {
            Ok(()) => Ok(()),
            Err(b) => {
                rollback_partial_map(&mut guard.tables, range.start(), installed);
                Err((b, MapError::Overlap))
            }
        }
    }

    /// Reserve an **anonymous** VMA with no frames and no PTEs; [`fault_in`](Self::fault_in)
    /// zeroes and maps each page on first touch. `boxed.backing` must be
    /// [`Backing::Anonymous`]. Same rejections as `map_vma` (box returned), but it allocates
    /// nothing, so never [`OutOfMemory`](MapError::OutOfMemory).
    pub fn map_vma_lazy(&self, boxed: KBox<Vma>) -> Result<(), (KBox<Vma>, MapError)> {
        debug_assert!(matches!(boxed.backing, Backing::Anonymous), "map_vma_lazy is for anonymous mappings only");
        let range = boxed.range;
        if let Err(e) = user_placement(Placement::At(range)) {
            return Err((boxed, e));
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
    /// For an [`Anonymous`](Backing::Anonymous) VMA permitting the access, allocates and zeroes
    /// a frame, installs the leaf PTE and flushes that page's stale not-present TLB entry. The
    /// address space **is live in the MMU** (unlike eager `map_vma`), so the flush is mandatory.
    /// Only [`FaultIn::Mapped`] means retry.
    pub fn fault_in(&self, addr: VirtAddr, access: FaultAccess) -> FaultIn {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;

        let Some(vma) = inner.vma_tree.find_covering(addr) else {
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

        let flags = page_flags_for(vma.prot, Caching::Normal);
        let page_base = VirtAddr::new(addr.as_u64() & !(PAGE_SIZE as u64 - 1));

        match vma.backing {
            Backing::Anonymous => {
                let Some(frame) = Frame::alloc_zeroed() else {
                    return FaultIn::Oom;
                };
                match inner.tables.map_owned(page_base, frame, flags) {
                    Ok(()) => crate::mm::record_demand_fault(),
                    Err((_, ArchMapError::OutOfMemory)) => return FaultIn::Oom,
                    // Another CPU faulted on the same page and mapped it first, between this
                    // CPU's fault and the lock: the page is there, so the access can be retried.
                    Err((_, ArchMapError::AlreadyMapped)) => {}
                    Err((_, e @ (ArchMapError::Misaligned | ArchMapError::NotUser | ArchMapError::Unshared))) => {
                        unreachable!("fault_in: {e:?} for a page-aligned user-half address");
                    }
                }
                // Evict the not-present entry the CPU cached for this page on the fault we are
                // servicing.
                Paging::flush_tlb_page(page_base);
                FaultIn::Mapped
            }
            // Installed eagerly by `map_object`; a fault here is a bug or teardown race.
            Backing::Object(_) => FaultIn::NoVma,
            // Signal only: the file cache's lock is rank 4 like this one and they never nest
            // (see `FaultIn::FileBacked`).
            Backing::File { .. } => FaultIn::FileBacked,
        }
    }

    /// Map a [`MemoryObject`]'s **own** frames from its page `first` where `at` places them, the
    /// range they map, holding `object` alive. Allocates and
    /// zeroes **nothing**: mapping the object twice aliases the same memory. The object frees the
    /// frames on its last-ref drop, not unmap.
    ///
    /// `object` needs `first` and the range's pages (the syscall layer checks). On failure it
    /// is handed back in the error tuple.
    pub fn map_object(
        &self,
        at: impl Into<Placement>,
        prot: Protection,
        object: Ref<MemoryObject>,
        first: usize,
    ) -> Result<VAddrRange, (Ref<MemoryObject>, MapError)> {
        let at = at.into();
        if let Err(e) = user_placement(at) {
            return Err((object, e));
        }
        let caching = object.caching();

        // Build the VMA box before the lock (no allocation under a spinlock); the only fallible
        // step before `object` is consumed, so failure can return it.
        let mut boxed = match KBox::try_new(Vma::new(placeholder(at), prot)) {
            Ok(b) => b,
            Err(_) => return Err((object, MapError::OutOfMemory)),
        };

        let mut guard = self.inner.lock();
        let range = match place(&guard.vma_tree, at) {
            Ok(range) => range,
            Err(e) => return Err((object, e)),
        };
        boxed.range = range;
        let npages = range.pages();
        debug_assert!(object.npages() as u64 >= first as u64 + npages, "object too small for range");

        let flags = page_flags_for(prot, caching);
        let mut installed: u64 = 0;

        for i in 0..npages {
            let virt = VirtAddr::new(range.start().as_u64() + i * (PAGE_SIZE as u64));
            let frame = object.frame_to_map(first + i as usize).expect("an object as large as the range");
            // SAFETY: `frame` is owned by the `MemoryObject` the VMA will keep alive until an
            // unmap's shootdown (`Unmapped`), or a rollback's; aliasing a frame already
            // mapped elsewhere is intended (shared memory). No frame is
            // allocated or zeroed here — the object owns them.
            match unsafe { guard.tables.map(virt, frame, flags) } {
                Ok(()) => installed += 1,
                Err(ArchMapError::OutOfMemory) => {
                    rollback_object_map(guard, range.start(), installed);
                    return Err((object, MapError::OutOfMemory));
                }
                Err(ArchMapError::AlreadyMapped) => {
                    // Impossible: overlap was pre-checked under the lock.
                    unreachable!("ArchPaging::map_page returned AlreadyMapped after VmaTree overlap pre-check");
                }
                Err(e @ (ArchMapError::Misaligned | ArchMapError::NotUser | ArchMapError::Unshared)) => {
                    // Impossible: VAddrRange enforces page alignment.
                    unreachable!("map_object: {e:?} for a page-aligned user-half address");
                }
            }
        }

        boxed.backing = Backing::Object(object);
        match guard.vma_tree.insert(boxed) {
            Ok(()) => Ok(range),
            Err(mut b) => {
                rollback_object_map(guard, range.start(), installed);
                let Backing::Object(object) = b.take_backing() else { unreachable!("an object-backed VMA") };
                Err((object, MapError::Overlap))
            }
        }
    }

    /// Reserve where `at` places it a **lazy file-backed** mapping of `object` from page `first` (a
    /// [`FileObject`](crate::object::FileObject)) with **no PTEs**; pages come in through
    /// `fault_in`, [`FaultIn::FileBacked`], fill, [`map_file_page`]. The VMA keeps `object` alive
    /// so its cache frames outlive the mapping; `unmap` removes the PTEs but never frees the
    /// frames. On rejection `object` is handed back.
    ///
    /// [`map_file_page`]: AddressSpace::map_file_page
    pub fn map_file(
        &self,
        at: impl Into<Placement>,
        prot: Protection,
        object: Ref<FileObject>,
        first: usize,
    ) -> Result<VAddrRange, (Ref<FileObject>, MapError)> {
        let at = at.into();
        if let Err(e) = user_placement(at) {
            return Err((object, e));
        }
        let mut boxed = match KBox::try_new(Vma::new(placeholder(at), prot)) {
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
        let refused = |object: Ref<FileObject>, e: MapError| {
            if writable {
                object.writable_unmapped();
            }
            Err((object, e))
        };
        let mut guard = self.inner.lock();
        let range = match place(&guard.vma_tree, at) {
            Ok(range) => range,
            Err(e) => {
                drop(guard);
                return refused(object, e);
            }
        };
        boxed.range = range;
        boxed.backing = Backing::File { file: object, first };
        match guard.vma_tree.insert(boxed) {
            Ok(()) => Ok(range),
            Err(mut b) => {
                let Backing::File { file: object, .. } = b.take_backing() else { unreachable!("a file-backed VMA") };
                drop(guard);
                refused(object, MapError::Overlap)
            }
        }
    }

    /// For a `FileBacked` fault at `addr`: the backing [`FileObject`](crate::object::FileObject)
    /// (a cloned reference) and the page index within the file; `None` if no file-backed VMA
    /// covers `addr` (a teardown race). A fresh lock acquisition, so the page cache is reached
    /// only **outside** this AS lock.
    pub fn file_backing(&self, addr: VirtAddr) -> Option<(Ref<FileObject>, usize)> {
        let guard = self.inner.lock();
        let vma = guard.vma_tree.find_covering(addr)?;
        let (file, index) = file_page(vma, addr)?;
        Some((file.clone(), index))
    }

    /// **Install the leaf PTE for page `index` of `object` at `addr`**, pointing at `frame`, if the
    /// mapping there still puts that page there, as an unmap and a map may have moved it since the
    /// fault read it. The object owns the frame, so `unmap` removes the PTE without freeing it.
    pub fn map_file_page(&self, addr: VirtAddr, object: &Ref<FileObject>, index: usize, frame: PhysAddr) -> FilePage {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        let Some(vma) = inner.vma_tree.find_covering(addr) else {
            return FilePage::Changed;
        };
        if file_page(vma, addr).is_none_or(|(file, at)| file.as_ptr() != object.as_ptr() || at != index) {
            return FilePage::Changed;
        }
        let flags = page_flags_for(vma.prot, Caching::Normal);
        let page_base = VirtAddr::new(addr.as_u64() & !(PAGE_SIZE as u64 - 1));
        // SAFETY: `frame` is a live, resident cache frame
        // the file object owns and keeps alive (the VMA holds its reference, until an unmap's
        // shootdown); `page_base` is page-aligned and in the validated user-half VMA range.
        match unsafe { inner.tables.map(page_base, frame, flags) } {
            Ok(()) | Err(ArchMapError::AlreadyMapped) => {
                Paging::flush_tlb_page(page_base);
                FilePage::Mapped
            }
            Err(ArchMapError::OutOfMemory) => FilePage::NoMemory,
            Err(e @ (ArchMapError::Misaligned | ArchMapError::NotUser | ArchMapError::Unshared)) => {
                unreachable!("map_file_page: {e:?} for a page-aligned user-half address")
            }
        }
    }

    /// Whether `[start, end)` overlaps the stack guard gap. `MMAP_MAX` bounds only
    /// `find_free_range` (`hint = 0`); a hinted mapping otherwise validates just alignment and the
    /// user-half bound, so without this check a stack overrun could land in mapped memory.
    pub fn in_stack_guard(start: u64, end: u64) -> bool {
        start < USER_STACK_BOTTOM && end > USER_STACK_GUARD_BOTTOM
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
        mut frames: KVec<Frame>,
    ) -> Result<Option<Unmapped>, (KVec<Frame>, u64)> {
        let mut guard = self.inner.lock();
        // Only an anonymous mapping's frames come back; reserving for a mapped 2 GiB file would
        // ask for more than the largest allocation and refuse the unmap.
        let pages = match guard.vma_tree.find_covering(addr) {
            Some(v) if matches!(v.backing, Backing::Anonymous) => v.range.pages(),
            Some(_) => 0,
            None => return Ok(None),
        };
        if ((frames.capacity() - frames.len()) as u64) < pages {
            return Err((frames, pages));
        }
        let boxed = guard.vma_tree.remove_covering(addr).expect("find_covering matched under the same hold");
        for i in 0..boxed.range.pages() {
            let virt = VirtAddr::new(boxed.range.start().as_u64() + i * (PAGE_SIZE as u64));
            let r = guard.tables.unmap(virt);
            if let Ok(phys) = r
                && matches!(boxed.backing, Backing::Anonymous)
            {
                // SAFETY: an anonymous page's frame, its PTE just cleared; freed after `Unmapped`'s shootdown.
                let frame = unsafe { Frame::from_pte(phys) };
                frames.try_push(frame).expect("within the spare capacity checked above");
            }
        }
        Ok(Some(Unmapped { _frames: frames, vma: Some(boxed) }))
    }
}

/// What an unmap took out of a live address space's page tables: anonymous frames, and a VMA
/// whose drop can free its object's. Dropping it shoots them down on every CPU, then releases
/// them, so it must be dropped outside every lock, as a shootdown requires.
pub struct Unmapped {
    _frames: KVec<Frame>,
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
            free_vma_pages(&mut guard.tables, &boxed);
        }
    }
}

/// Uninstall every PTE of a VMA's range and free each anonymous leaf frame.
fn free_vma_pages(tables: &mut UserTables, vma: &Vma) {
    let range = vma.range;
    for i in 0..range.pages() {
        let virt = VirtAddr::new(range.start().as_u64() + i * (PAGE_SIZE as u64));
        if let Ok(phys) = tables.unmap(virt) {
            match vma.backing {
                // SAFETY: an anonymous page's frame, its PTE just cleared in a space no CPU has loaded.
                Backing::Anonymous => drop(unsafe { Frame::from_pte(phys) }),
                // The `MemoryObject` owns the frames and frees them on its last-ref drop.
                Backing::Object(_) => {}
                // The file's page cache owns the frames, as an object owns its own.
                Backing::File { .. } => {}
            }
        }
    }
}

/// The range a VMA is boxed with before the lock; an "anywhere" one's is overwritten under it.
fn placeholder(at: Placement) -> VAddrRange {
    match at {
        Placement::At(range) => range,
        Placement::Anywhere(_) => {
            VAddrRange::new(VirtAddr::new(MMAP_BASE), VirtAddr::new(MMAP_BASE + PAGE_SIZE as u64))
                .expect("a page at the window's base")
        }
    }
}

/// A range named by its caller must be canonical and in the user half; the window's always are.
fn user_placement(at: Placement) -> Result<(), MapError> {
    let Placement::At(range) = at else { return Ok(()) };
    // Both endpoints must be canonical; test `end - 1` since the end is exclusive.
    let last_byte = VirtAddr::new(range.end().as_u64() - 1);
    if !range.start().is_canonical() || !last_byte.is_canonical() {
        return Err(MapError::NotCanonical);
    }
    if range.end().as_u64() > USER_MAP_END {
        return Err(MapError::NotUserHalf);
    }
    Ok(())
}

/// **The range `at` comes to in `tree`**, under the hold that will insert it.
fn place(tree: &VmaTree, at: Placement) -> Result<VAddrRange, MapError> {
    match at {
        Placement::At(range) if tree.find_first_overlapping(range).is_some() => Err(MapError::Overlap),
        Placement::At(range) => Ok(range),
        Placement::Anywhere(size) => tree
            .find_free_range(VirtAddr::new(MMAP_BASE), VirtAddr::new(MMAP_MAX), size, MMAP_GAP)
            .ok_or(MapError::NoRoom),
    }
}

/// The file `vma` maps and the page of it at `addr`, which `vma` covers; `None` for another backing.
fn file_page(vma: &Vma, addr: VirtAddr) -> Option<(&Ref<FileObject>, usize)> {
    let Backing::File { file, first } = &vma.backing else { return None };
    let page_base = addr.as_u64() & !(PAGE_SIZE as u64 - 1);
    Some((file, first + ((page_base - vma.range.start().as_u64()) / PAGE_SIZE as u64) as usize))
}

/// Roll back a partial `map_vma`: uninstall `installed` PTEs from `start`, freeing each frame, with
/// no shootdown, as `map_vma` fills only a space no CPU has loaded.
fn rollback_partial_map(tables: &mut UserTables, start: VirtAddr, installed: u64) {
    for i in 0..installed {
        let virt = VirtAddr::new(start.as_u64() + i * (PAGE_SIZE as u64));
        if let Ok(phys) = tables.unmap(virt) {
            // SAFETY: the frame `map_vma` handed this PTE, now cleared in a space no CPU has loaded.
            drop(unsafe { Frame::from_pte(phys) });
        }
    }
}

/// Roll back a partial `map_object`: uninstall `installed` PTEs from `start` but **do not free
/// the frames** (the `MemoryObject` owns them), then release the lock and shoot them down, if any
/// page was mapped. The space is live, so the object may be released only once this returns.
fn rollback_object_map(mut guard: SpinLockGuard<'_, Inner>, start: VirtAddr, installed: u64) {
    for i in 0..installed {
        let virt = VirtAddr::new(start.as_u64() + i * (PAGE_SIZE as u64));
        let _ = guard.tables.unmap(virt);
    }
    let unmapped = (installed > 0).then(|| Unmapped { _frames: KVec::new(), vma: None });
    drop(guard);
    drop(unmapped);
}

/// Translate the VMA-level [`Protection`] and [`Caching`] into hardware [`PageFlags`].
/// `NO_EXECUTE` is inverted: hardware is executable by default, [`Protection`] non-executable.
///
/// The caching comes from the object, so every mapping of one aperture asks for the
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
