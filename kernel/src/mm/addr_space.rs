//! Per-process virtual address space.
//!
//! [`AddressSpace`] pairs a [`VmaTree`] (per-AS catalogue of virtual
//! memory areas) with a top-level page-table root (the PML4 base on
//! x86_64) under a single [`SpinLock`]. It is the bridge between the
//! VMM's bookkeeping (the tree) and the hardware MMU's actual
//! translations (the page tables): `map_vma` updates both atomically;
//! `unmap_covering` does the inverse.
//!
//! ## What this layer is for
//!
//! Every process gets exactly one `AddressSpace`. Today nothing
//! *consumes* one — there is no scheduler, no first userspace process —
//! but the address-spaces-and-paging slice needs to deliver something
//! that can actually represent a process's memory. Without
//! `AddressSpace`, the [`VmaTree`] is structurally testable but
//! disconnected from any real translation; with it, a `Vma` going into
//! the tree is the same operation as PTEs going into the hardware.
//!
//! ## Phase 1 limitations
//!
//! - **No kernel-half mapping yet.** A fresh `AddressSpace` has an
//!   all-zero PML4: switching `CR3` to its root would triple-fault
//!   immediately because the kernel code currently executing wouldn't
//!   be mapped. The next sub-item in the slice (higher-half kernel
//!   mapping shared across all address spaces) closes that gap. Until
//!   then this type is "installable but not loadable."
//! - **No TLB flushing.** No `AddressSpace` is ever loaded onto a CPU
//!   today, so the TLB never holds entries for the new mappings. When
//!   the scheduler arrives it will gain a `set_active` entry point
//!   that takes responsibility for flushing.
//! - **Eager anonymous allocation.** `map_vma` allocates one frame per
//!   page up front. Lazy on-fault allocation is a real OS pattern but
//!   needs a working page-fault handler — the current one is dump-and-
//!   halt. Eager allocation works today; the switch to lazy will come
//!   with the upgraded `#PF` handler.
//! - **Intermediate page-table frames are not reclaimed on unmap.**
//!   This matches the deferred decision documented for
//!   `ArchPaging::unmap_page` (see [deferred-decisions.md]). `Drop`
//!   reclaims leaf frames and the top-level PML4; the PDPT / PD / PT
//!   levels in between leak. For a single Phase 1 address space this
//!   is negligible.
//!
//! [deferred-decisions.md]: ../../docs/rationale/deferred-decisions.md

use core::ptr;

use crate::arch::Paging;
use crate::arch::paging::{ArchPaging, MapError as ArchMapError, PageFlags};
use crate::libkern::{AllocError, KBox, SpinLock};
use crate::mm::vmm::{MappingKind, Protection, Vma, VmaTree};
use crate::mm::{PAGE_SIZE, PhysAddr, VirtAddr, heap};

/// First non-canonical address — and therefore the exclusive upper
/// bound of the user half on 4-level paging. Anything at or above this
/// either falls in the canonical hole or is in the kernel half; both
/// are off-limits to `map_vma`.
pub const USER_VIRT_END: u64 = 0x0000_8000_0000_0000;

/// Why [`AddressSpace::map_vma`] could not install a mapping.
///
/// On error the caller's [`KBox<Vma>`] is returned in the tuple
/// alongside the variant so it can be inspected, dropped, or retried.
#[derive(Debug, PartialEq, Eq)]
pub enum MapError {
    /// A range endpoint was not canonical for 4-level paging.
    NotCanonical,
    /// The range falls outside the user half (`[0, USER_VIRT_END)`).
    /// Kernel-half mappings are installed by a separate path.
    NotUserHalf,
    /// The range overlaps an existing VMA in this address space.
    Overlap,
    /// Physical-frame allocation (a leaf, or an intermediate page-table
    /// frame) failed. Any frames installed before the failure are
    /// rolled back before this is returned.
    OutOfMemory,
}

/// A per-process virtual address space.
///
/// All state lives behind a single `SpinLock<Inner>` (rank 4 per
/// [kernel/docs/lock-ordering.md]). Public methods acquire and release
/// the lock internally; callers cannot reach the inner state otherwise.
pub struct AddressSpace {
    inner: SpinLock<Inner>,
}

struct Inner {
    vma_tree: VmaTree,
    /// Physical base of the top-level page-table frame (the PML4 on
    /// x86_64). Allocated at construction, freed in `Drop` after the
    /// tree is torn down.
    root: PhysAddr,
}

impl AddressSpace {
    /// Build an empty address space: allocate a fresh PML4 frame, zero
    /// it (all entries absent), and pair it with an empty VMA tree.
    /// Returns [`AllocError`] if the PML4 frame allocation fails.
    pub fn new() -> Result<Self, AllocError> {
        let root = heap::buddy_alloc(0).ok_or(AllocError)?;
        // SAFETY: the frame was just returned by the buddy and is not
        // aliased; HHDM access is the standard way to reach a fresh
        // physical frame. Zeroing it makes every PML4 entry absent.
        unsafe {
            let virt = (root.as_u64() + heap::hhdm_offset()) as *mut u8;
            ptr::write_bytes(virt, 0, PAGE_SIZE);
        }
        Ok(AddressSpace {
            inner: SpinLock::new(Inner {
                vma_tree: VmaTree::new(),
                root,
            }),
        })
    }

    /// Physical base of this address space's top-level page table. The
    /// future scheduler will hand this to
    /// [`ArchPaging::set_page_table`](crate::arch::paging::ArchPaging::set_page_table)
    /// to make the address space active.
    pub fn root(&self) -> PhysAddr {
        self.inner.lock().root
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().vma_tree.is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().vma_tree.len()
    }

    /// Install a VMA: allocate and zero a frame for every page, install
    /// each PTE, then insert the VMA into the tree. The whole sequence
    /// runs under the AS lock so the tree and the page tables can never
    /// disagree about what is mapped.
    ///
    /// Returns the box back to the caller on any rejection.
    pub fn map_vma(&self, boxed: KBox<Vma>) -> Result<(), (KBox<Vma>, MapError)> {
        let range = boxed.range;

        // Canonicality: both endpoints of the half-open range must be
        // canonical. The end is exclusive, so we test `end - 1` (the
        // last byte covered); that handles the edge case where `end ==
        // USER_VIRT_END` (a non-canonical address in itself, but the
        // covered bytes stop one short of it).
        let last_byte = VirtAddr::new(range.end().as_u64() - 1);
        if !range.start().is_canonical() || !last_byte.is_canonical() {
            return Err((boxed, MapError::NotCanonical));
        }
        if range.end().as_u64() > USER_VIRT_END {
            return Err((boxed, MapError::NotUserHalf));
        }

        let mut guard = self.inner.lock();

        // Pre-check overlap so a doomed insert doesn't run frame
        // allocations that we'd just have to roll back.
        if guard.vma_tree.find_first_overlapping(range).is_some() {
            return Err((boxed, MapError::Overlap));
        }

        let flags = protection_to_page_flags(boxed.prot);
        let root = guard.root;
        let total_pages = range.pages();
        let mut installed: u64 = 0;

        for i in 0..total_pages {
            let virt = VirtAddr::new(range.start().as_u64() + i * (PAGE_SIZE as u64));

            let Some(phys) = heap::buddy_alloc(0) else {
                rollback_partial_map(root, range.start(), installed);
                return Err((boxed, MapError::OutOfMemory));
            };

            // Anonymous mapping: zero the freshly allocated frame.
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
                    // Impossible: we hold the AS lock and pre-checked
                    // overlap; a PTE for this virt can't already exist.
                    unreachable!(
                        "ArchPaging::map_page returned AlreadyMapped after VmaTree overlap pre-check"
                    );
                }
                Err(ArchMapError::Misaligned) => {
                    // Impossible: VAddrRange enforces page alignment,
                    // and the per-page virt is start + i*PAGE_SIZE.
                    unreachable!(
                        "ArchPaging::map_page returned Misaligned for a page-aligned per-page address"
                    );
                }
            }
        }

        // Commit. The tree's insert can't reject — we still hold the
        // lock and re-checked overlap above.
        match guard.vma_tree.insert(boxed) {
            Ok(()) => Ok(()),
            Err(b) => {
                rollback_partial_map(root, range.start(), installed);
                Err((b, MapError::Overlap))
            }
        }
    }

    /// Remove the VMA covering `addr`: drop it from the tree, then walk
    /// its range uninstalling every PTE and freeing the backing frame
    /// (for anonymous mappings). Returns the VMA box, or `None` if no
    /// VMA covers `addr`.
    pub fn unmap_covering(&self, addr: VirtAddr) -> Option<KBox<Vma>> {
        let mut guard = self.inner.lock();
        let boxed = guard.vma_tree.remove_covering(addr)?;
        free_vma_pages(guard.root, &boxed);
        Some(boxed)
    }
}

impl Drop for AddressSpace {
    /// Tear down every VMA (uninstall PTEs, free leaf frames), drop
    /// the (now-empty) tree, then free the top-level PML4 frame.
    /// Intermediate page-table frames leak per the deferred decision.
    fn drop(&mut self) {
        let mut guard = self.inner.lock();

        // Drain the tree leftmost-first: peek via iter (an immutable
        // borrow that ends before the mutating remove), then remove.
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
            // `boxed` drops at the end of this iteration, returning
            // the Vma to the slab.
        }

        heap::buddy_free(guard.root, 0);
    }
}

/// Walk a VMA's range, uninstall every PTE, and free each leaf frame.
/// Used by both `unmap_covering` and `Drop`.
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
            }
        }
    }
}

/// Roll back a partial map_vma: walk `installed` pages starting at
/// `start`, uninstall each PTE, and free each leaf frame. Used only on
/// failure paths inside `map_vma`.
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

/// Translate the VMA-level [`Protection`] into hardware
/// [`PageFlags`]. `NO_EXECUTE` is inverted because the hardware default
/// is executable (NX is opt-in), but [`Protection`]'s default is
/// non-executable (W^X by default).
fn protection_to_page_flags(prot: Protection) -> PageFlags {
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
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::translate;
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
        KBox::try_new(Vma::new(r, prot, MappingKind::Anonymous))
            .expect("test heap exhausted")
    }

    #[test]
    fn new_yields_empty_address_space_with_real_root() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        assert!(asp.is_empty());
        assert_eq!(asp.len(), 0);
        // Root must be a real, page-aligned physical address.
        let root = asp.root();
        assert!(root.is_page_aligned());
        assert_ne!(root.as_u64(), 0);
    }

    #[test]
    fn map_single_page_installs_pte_translate_finds_it() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        let r = range(PAGE * 4, PAGE * 5);
        asp.map_vma(anon_box(r, Protection::WRITE | Protection::USER))
            .expect("map must succeed");

        // SAFETY: translate is read-only against the live tables we
        // just populated. The root is owned by `asp`.
        let phys = unsafe { translate(asp.root(), va(PAGE * 4)) };
        assert!(phys.is_some());
        // Address just past the page should be unmapped.
        let beyond = unsafe { translate(asp.root(), va(PAGE * 5)) };
        assert!(beyond.is_none());
        assert_eq!(asp.len(), 1);
    }

    #[test]
    fn map_multi_page_installs_every_pte() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        let r = range(PAGE * 8, PAGE * 16);
        asp.map_vma(anon_box(r, Protection::WRITE | Protection::USER))
            .expect("map must succeed");

        // Each page in the range is mapped; the addresses immediately
        // before and after are not.
        for i in 8..16 {
            let p = unsafe { translate(asp.root(), va(PAGE * i)) };
            assert!(p.is_some(), "page {i} not mapped");
        }
        let before = unsafe { translate(asp.root(), va(PAGE * 7)) };
        let after = unsafe { translate(asp.root(), va(PAGE * 16)) };
        assert!(before.is_none());
        assert!(after.is_none());
    }

    #[test]
    fn map_then_unmap_removes_the_ptes() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        let r = range(PAGE * 4, PAGE * 8);
        asp.map_vma(anon_box(r, Protection::WRITE | Protection::USER))
            .expect("map must succeed");

        let removed = asp
            .unmap_covering(va(PAGE * 5))
            .expect("unmap must find covering vma");
        assert_eq!(removed.range, r);
        assert!(asp.is_empty());

        for i in 4..8 {
            let p = unsafe { translate(asp.root(), va(PAGE * i)) };
            assert!(p.is_none(), "page {i} still mapped after unmap");
        }
    }

    #[test]
    fn map_rejects_overlap_with_existing_vma() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        asp.map_vma(anon_box(
            range(PAGE * 4, PAGE * 8),
            Protection::WRITE | Protection::USER,
        ))
        .expect("first map must succeed");

        let err = asp
            .map_vma(anon_box(
                range(PAGE * 6, PAGE * 10),
                Protection::WRITE | Protection::USER,
            ))
            .expect_err("overlapping map must fail");
        assert_eq!(err.1, MapError::Overlap);
        // The returned box should still carry the rejected range.
        assert_eq!(err.0.range, range(PAGE * 6, PAGE * 10));

        // The original mapping must be untouched.
        assert!(unsafe { translate(asp.root(), va(PAGE * 4)) }.is_some());
        // The rejected range's pages must NOT have been installed.
        assert!(unsafe { translate(asp.root(), va(PAGE * 9)) }.is_none());
        assert_eq!(asp.len(), 1);
    }

    #[test]
    fn map_rejects_kernel_half_range() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        // A range that ends past USER_VIRT_END is rejected.
        let r = VAddrRange::new(
            va(USER_VIRT_END - PAGE),
            va(USER_VIRT_END + PAGE),
        )
        .unwrap();
        let err = asp
            .map_vma(anon_box(r, Protection::WRITE | Protection::USER))
            .expect_err("kernel-half range must be rejected");
        // The error is NotUserHalf, or NotCanonical depending on which
        // check triggers first — both are acceptable rejections.
        assert!(matches!(
            err.1,
            MapError::NotUserHalf | MapError::NotCanonical
        ));
    }

    #[test]
    fn unmap_on_address_with_no_vma_returns_none() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        assert!(asp.unmap_covering(va(PAGE * 100)).is_none());
        // Even with one mapped VMA, an addr not covered returns None.
        asp.map_vma(anon_box(
            range(PAGE * 4, PAGE * 8),
            Protection::WRITE | Protection::USER,
        ))
        .unwrap();
        assert!(asp.unmap_covering(va(PAGE * 20)).is_none());
        assert_eq!(asp.len(), 1);
    }

    #[test]
    fn drop_tears_down_populated_address_space() {
        // Verify Drop is well-behaved: build a populated AS, drop it,
        // do it again. Across 8 iterations of 16 pages each this
        // would gradually exhaust the 16 MiB test heap if Drop leaked
        // leaf frames or the PML4.
        init_global_heap();
        for _ in 0..8 {
            let asp = AddressSpace::new().expect("new must succeed");
            for i in 0..4u64 {
                let start = (i * 8) * PAGE;
                asp.map_vma(anon_box(
                    range(start, start + PAGE * 4),
                    Protection::WRITE | Protection::USER,
                ))
                .expect("map must succeed");
            }
            // Drop runs at end of iteration.
        }
    }
}
