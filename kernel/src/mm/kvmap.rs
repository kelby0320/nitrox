//! Kernel-half virtual address allocator (the "kernel vmap").
//!
//! Hands out virtual address ranges in the kernel-vmap region for
//! kernel-half mappings the boot path didn't pre-establish: kernel
//! stacks, future per-CPU data, future driver MMIO. The allocator
//! itself is just a bump pointer — Phase 1 has 16 TiB to spend and no
//! workload that approaches it, so a freelist isn't worth the
//! complexity. The vmap region is laid out by
//! `docs/architecture/overview.md`.
//!
//! ## Sharing across address spaces
//!
//! Vmap mappings must be visible to every address space — a kernel
//! stack pre-allocated by one AS has to be reachable when the CPU is
//! running under another. The mechanism is the
//! [Kernel-half PML4 sharing](../../docs/architecture/memory-management.md#kernel-half-pml4-sharing)
//! captured in the previous slice item:
//!
//! 1. [`init`] runs at boot **before** `init_kernel_template` and
//!    pre-allocates the top-level intermediate page tables for the
//!    vmap region via `ArchPaging::ensure_kernel_intermediate`. The
//!    live PML4 now points at real PDPTs covering the vmap.
//! 2. `init_kernel_template` snapshots the (now-populated) kernel-half
//!    PML4 entries into the boot template.
//! 3. Every `AddressSpace::new` inherits the same PML4 entries, which
//!    point at the same shared PDPTs. Future `map_page` calls in the
//!    vmap modify a PD/PT chain that hangs off those shared PDPTs and
//!    is visible to every AS — no shootdown step, no per-AS
//!    coordination.

use crate::arch::paging::ArchPaging;
use crate::arch::{Paging, active_root};
use crate::libkern::{AllocError, SpinLock};
use crate::mm::{PAGE_SIZE, VirtAddr};

/// Inclusive lower bound of the kernel vmap region per
/// `docs/architecture/overview.md`.
pub const KERNEL_VMAP_START: u64 = 0xFFFF_C000_0000_0000;
/// Exclusive upper bound of the kernel vmap region (16 TiB).
pub const KERNEL_VMAP_END: u64 = 0xFFFF_D000_0000_0000;

/// Bump-pointer cursor. Grows upward through the vmap region. Holds
/// the next virtual byte to hand out. Acquired briefly per allocation
/// and never nested with other locks — sits at lock rank 6d alongside
/// the allocator leaves.
static VMAP_NEXT: SpinLock<u64> = SpinLock::new(KERNEL_VMAP_START);

/// Boot-time setup: pre-allocate kernel-vmap intermediate page tables
/// in the live PML4 so the kernel template captures their pointers.
/// After the template snapshot, the same PDPTs are reached from every
/// AS's PML4 — post-boot leaf installs into vmap propagate without
/// further coordination.
///
/// For Phase 1 this pre-allocates a single PDPT covering the first
/// 512 GiB of vmap. That's more than the slice needs (a handful of
/// 16 KiB kernel stacks); if a future allocation crosses the 512 GiB
/// boundary it must add its PDPT to the live PML4 here, before any AS
/// exists, to keep the immutable-post-boot rule intact.
///
/// # Safety
/// Must be called after `init_memory` (HHDM required to reach the
/// PML4 frame) and **before** [`crate::arch::init_kernel_template`]
/// (must influence the snapshot). The single CPU running boot code
/// is the only mutator of the live PML4 at this point.
pub unsafe fn init() {
    let root = active_root();
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

/// Reserve `n_pages` consecutive virtual pages in the kernel vmap
/// region. Returns the starting virtual address (page-aligned).
///
/// The returned range is reserved but **not mapped** — the caller is
/// responsible for installing PTEs into it (or deliberately leaving
/// pages absent, e.g. for guard pages). Returns [`AllocError`] if the
/// vmap region cannot satisfy the request (Phase 1: only if the bump
/// pointer would overflow `KERNEL_VMAP_END`, which won't happen at
/// realistic workloads).
pub fn vmap_alloc_pages(n_pages: u64) -> Result<VirtAddr, AllocError> {
    let bytes = n_pages
        .checked_mul(PAGE_SIZE as u64)
        .ok_or(AllocError)?;
    let mut next = VMAP_NEXT.lock();
    let start = *next;
    let end = start.checked_add(bytes).ok_or(AllocError)?;
    if end > KERNEL_VMAP_END {
        return Err(AllocError);
    }
    *next = end;
    Ok(VirtAddr::new(start))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocations_are_page_aligned_and_in_vmap_region() {
        let v = vmap_alloc_pages(1).unwrap();
        assert!(v.is_page_aligned(), "vmap allocation not page-aligned");
        assert!(
            v.as_u64() >= KERNEL_VMAP_START,
            "vmap allocation below region: {:#x}",
            v.as_u64()
        );
        assert!(
            v.as_u64() < KERNEL_VMAP_END,
            "vmap allocation past region: {:#x}",
            v.as_u64()
        );
    }

    #[test]
    fn back_to_back_allocations_advance_by_at_least_request_size() {
        // The global cursor is shared across tests, so `b` could be
        // further along than `a + n*PAGE` if a parallel test slipped
        // in between — but never closer.
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
