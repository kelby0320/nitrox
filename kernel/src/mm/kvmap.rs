//! Kernel-half virtual address allocator (the "kernel vmap").
//!
//! A bump pointer, never reclaimed, over the vmap region (16 TiB; layout in
//! `docs/architecture/overview.md`), for kernel-half mappings the boot path did not pre-establish.
//!
//! A vmap mapping must be visible under every address space (a stack allocated under one AS is
//! used while another is loaded), via
//! [Kernel-half PML4 sharing](../../docs/architecture/memory-management.md#kernel-half-pml4-sharing):
//! [`init`] runs at boot **before** `init_kernel_template` and pre-allocates the vmap's top-level
//! intermediate page tables, the template snapshots those PML4 entries, and every
//! `AddressSpace::new` inherits them. Later `map_page` calls modify a PD/PT chain under the shared
//! PDPTs, visible to every AS with no shootdown.

use crate::arch::Paging;
use crate::arch::paging::{ArchPaging, PageFlags};
use crate::libkern::lockrank::LockRank;
use crate::libkern::{AllocError, SpinLock};
use crate::mm::{PAGE_SIZE, PhysAddr, VirtAddr};

/// Inclusive lower bound of the kernel vmap region (`docs/architecture/overview.md`).
pub const KERNEL_VMAP_START: u64 = 0xFFFF_C000_0000_0000;
/// Exclusive upper bound of the kernel vmap region (16 TiB).
pub const KERNEL_VMAP_END: u64 = 0xFFFF_D000_0000_0000;

/// Bump-pointer cursor: the next virtual byte to hand out. Never nested; lock rank 6d.
static VMAP_NEXT: SpinLock<u64> = SpinLock::new(LockRank::KernelVmap, KERNEL_VMAP_START);

/// Boot-time setup: pre-allocate the vmap's intermediate page tables in the live PML4 so the
/// kernel template captures them. One PDPT covers the first 512 GiB; crossing that boundary needs
/// another added here, before any AS exists.
///
/// # Safety
/// Must be called after `init_memory` (HHDM required to reach the
/// PML4 frame) and **before** [`crate::arch::init_kernel_template`]
/// (must influence the snapshot). The single CPU running boot code
/// is the only mutator of the live PML4 at this point.
pub unsafe fn init() {
    let root = Paging::active_root();
    // SAFETY: forwarded from this function's contract. Allocating a
    // PDPT under the PML4 entry covering KERNEL_VMAP_START is sound
    // because (a) no AS has been built yet, so there is no captured
    // template to disagree with, and (b) the buddy allocator is up
    // (precondition of this function).
    unsafe {
        Paging::ensure_kernel_intermediate(root, VirtAddr::new(KERNEL_VMAP_START))
            .expect("kvmap PDPT pre-allocation failed at boot");
    }
}

/// Reserve `n_pages` consecutive vmap pages; returns the page-aligned start. The range is
/// **not mapped**: the caller installs PTEs or leaves guard pages absent. [`AllocError`] only if
/// the bump pointer would overflow `KERNEL_VMAP_END`.
pub fn vmap_alloc_pages(n_pages: u64) -> Result<VirtAddr, AllocError> {
    let bytes = n_pages.checked_mul(PAGE_SIZE as u64).ok_or(AllocError)?;
    let mut next = VMAP_NEXT.lock();
    let start = *next;
    let end = start.checked_add(bytes).ok_or(AllocError)?;
    if end > KERNEL_VMAP_END {
        return Err(AllocError);
    }
    *next = end;
    Ok(VirtAddr::new(start))
}

/// Map `n_pages` of physical MMIO from `phys` into a fresh vmap range, **uncached** and writable.
/// **Permanent** (VA is never reclaimed). `phys` should be page-aligned.
///
/// # Safety
/// - `phys .. phys + n_pages*PAGE` must be a real device MMIO range safe to map
///   uncached, owned by the caller for the kernel's lifetime.
/// - Ring-0; mutates the active page tables. The vmap intermediate tables are
///   shared kernel-half (see the module docs), so the mapping is visible to every
///   address space.
pub unsafe fn map_mmio(phys: PhysAddr, n_pages: u64) -> Result<VirtAddr, AllocError> {
    let base = vmap_alloc_pages(n_pages)?;
    let root = Paging::active_root();
    let flags = PageFlags::WRITABLE | PageFlags::NO_CACHE;
    for i in 0..n_pages {
        let v = VirtAddr::new(base.as_u64() + i * PAGE_SIZE as u64);
        let p = PhysAddr(phys.as_u64() + i * PAGE_SIZE as u64);
        // SAFETY: `v` is a fresh vmap page; `p` is a caller-owned device frame
        // mapped uncached; intermediates are the pre-allocated shared kernel PDPTs.
        unsafe {
            Paging::map_page(root, v, p, flags).map_err(|_| AllocError)?;
            Paging::flush_tlb_page(v);
        }
    }
    Ok(base)
}

/// Point one already-reserved vmap page at the MMIO frame `phys`, **uncached** and writable,
/// flushing the stale TLB entry: for a reusable scan window reserved once with
/// [`vmap_alloc_pages`]. `phys` must be page-aligned; the previous mapping of `virt` is dropped.
///
/// # Safety
/// - `virt` must be a page handed out by [`vmap_alloc_pages`] that the caller
///   owns and is not concurrently using under another physical frame.
/// - `phys` must be a real MMIO frame safe to map uncached (a device window).
/// - Ring-0; mutates the active page tables. The vmap intermediate tables are
///   shared kernel-half (see the module docs), so the change is visible to every
///   address space.
pub unsafe fn remap_mmio_page(virt: VirtAddr, phys: PhysAddr) -> Result<(), AllocError> {
    let root = Paging::active_root();
    // SAFETY: `virt` is a caller-owned vmap page in the shared kernel half.
    let _ = unsafe { Paging::unmap_page(root, virt) };
    let flags = PageFlags::WRITABLE | PageFlags::NO_CACHE;
    // SAFETY: fresh leaf for a caller-owned vmap page; `phys` is a device frame
    // mapped uncached; intermediates are the pre-allocated shared kernel PDPTs.
    unsafe {
        Paging::map_page(root, virt, phys, flags).map_err(|_| AllocError)?;
        Paging::flush_tlb_page(virt);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocations_are_page_aligned_and_in_vmap_region() {
        let v = vmap_alloc_pages(1).unwrap();
        assert!(v.is_page_aligned(), "vmap allocation not page-aligned");
        assert!(v.as_u64() >= KERNEL_VMAP_START, "vmap allocation below region: {:#x}", v.as_u64());
        assert!(v.as_u64() < KERNEL_VMAP_END, "vmap allocation past region: {:#x}", v.as_u64());
    }

    #[test]
    fn back_to_back_allocations_advance_by_at_least_request_size() {
        let pages = 4u64;
        let a = vmap_alloc_pages(pages).unwrap();
        let b = vmap_alloc_pages(1).unwrap();
        assert!(
            b.as_u64() >= a.as_u64() + pages * PAGE_SIZE as u64,
            "second allocation overlaps first: a={:#x}, b={:#x}",
            a.as_u64(),
            b.as_u64()
        );
    }

    #[test]
    fn distinct_calls_return_distinct_addresses() {
        let a = vmap_alloc_pages(1).unwrap();
        let b = vmap_alloc_pages(1).unwrap();
        assert_ne!(a, b);
    }
}
