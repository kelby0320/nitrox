//! Kernel heap facade: the single owner of the buddy allocator.
//!
//! The buddy allocator lives in `mm/buddy.rs` as a `!Sync` struct; this
//! module wraps it in a `SpinLock<Option<BuddyAllocator>>` so the rest of
//! the kernel can call into it from any module without juggling lifetimes
//! or locks directly. The slab allocator uses the [`BuddyPager`] trait
//! below so it can be tested against a local `BuddyAllocator` without
//! touching the production statics.
//!
//! Initialisation order:
//! 1. [`init_buddy`] (once, from `kernel_main`) — installs the buddy
//!    allocator and stores the HHDM offset.
//! 2. [`super::slab::slab_init`] — initialises the slab caches that sit
//!    on top of this facade.
//!
//! Calling [`buddy_alloc`] or [`buddy_free`] before [`init_buddy`] panics
//! with a loud message; that's intentional — there is no silent failure
//! mode for "allocator not ready."

use core::sync::atomic::{AtomicU64, Ordering};

use crate::libkern::SpinLock;
use crate::limine::MemoryMapResponse;
use crate::mm::PhysAddr;
use crate::mm::buddy::BuddyAllocator;

/// The single global buddy allocator. `None` until [`init_buddy`].
static BUDDY: SpinLock<Option<BuddyAllocator>> = SpinLock::new(None);

/// HHDM offset captured at init time. Read-only after [`init_buddy`].
///
/// Stored separately from `BUDDY` so consumers (notably the slab
/// allocator) can read it without taking the buddy lock.
static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Initialise the buddy allocator. Must be called exactly once, early in
/// `kernel_main`, before any other code in this module is invoked.
///
/// # Safety
///
/// All the requirements of [`BuddyAllocator::new`] apply: `memory_map`
/// must be a valid Limine response and `hhdm_offset` must be the
/// bootloader's HHDM base.
pub unsafe fn init_buddy(memory_map: &MemoryMapResponse, hhdm_offset: u64) {
    let mut slot = BUDDY.lock();
    assert!(
        slot.is_none(),
        "heap::init_buddy called twice; the buddy allocator must be initialised exactly once"
    );
    // SAFETY: forwarded from the caller per the function-level contract.
    let allocator = unsafe { BuddyAllocator::new(memory_map, hhdm_offset) };
    *slot = Some(allocator);
    HHDM_OFFSET.store(hhdm_offset, Ordering::Release);
}

/// Allocate `2^order` contiguous physical frames. Returns `None` on OOM.
///
/// Panics if [`init_buddy`] has not run.
pub fn buddy_alloc(order: usize) -> Option<PhysAddr> {
    let mut slot = BUDDY.lock();
    slot.as_mut()
        .expect("heap::buddy_alloc called before init_buddy")
        .alloc(order)
}

/// Return frames previously obtained from [`buddy_alloc`]. `order` must
/// match the original allocation.
///
/// Panics if [`init_buddy`] has not run.
pub fn buddy_free(addr: PhysAddr, order: usize) {
    let mut slot = BUDDY.lock();
    slot.as_mut()
        .expect("heap::buddy_free called before init_buddy")
        .free(addr, order);
}

/// HHDM offset captured at [`init_buddy`] time. Returns `0` if init has
/// not run.
///
/// Callers that need the value before init (there should be none) get a
/// useless `0` rather than a panic; the slab allocator routes its own
/// "not initialised" panic through `SLAB_INITIALISED` instead.
pub fn hhdm_offset() -> u64 {
    HHDM_OFFSET.load(Ordering::Acquire)
}

/// Trait abstracting the buddy allocator for the slab. The production
/// implementation is [`HeapBuddy`], which dispatches to the static buddy
/// allocator above; tests construct a `LocalBuddy` that wraps a per-test
/// `BuddyAllocator`. Keeping this surface small (alloc + free + HHDM
/// offset) is enough for the slab; introducing a full trait isn't a
/// reach for in-tree generics.
pub trait BuddyPager {
    fn alloc(&self, order: usize) -> Option<PhysAddr>;
    fn free(&self, addr: PhysAddr, order: usize);
    fn hhdm_offset(&self) -> u64;
}

/// Production [`BuddyPager`] that routes through the static buddy.
pub struct HeapBuddy;

impl BuddyPager for HeapBuddy {
    fn alloc(&self, order: usize) -> Option<PhysAddr> {
        buddy_alloc(order)
    }
    fn free(&self, addr: PhysAddr, order: usize) {
        buddy_free(addr, order);
    }
    fn hhdm_offset(&self) -> u64 {
        hhdm_offset()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::PAGE_SIZE;

    // We can't easily test `init_buddy` itself without manufacturing a
    // valid `MemoryMapResponse` here, and that helper lives in the buddy
    // module's `#[cfg(test)]` block. To avoid duplicating it, the slab
    // module's tests build the `BuddyAllocator` themselves and use
    // [`LocalBuddy`] (defined there). This module's only host-testable
    // invariant is "panics-before-init" — which we can't test through the
    // singleton without poisoning subsequent tests. Cross-referenced by
    // the lock-ordering doc; revisit if a test-only reset helper becomes
    // worthwhile.

    #[test]
    fn hhdm_offset_is_zero_before_init() {
        // We rely on this module running after a clean process start in
        // test mode; if other tests touch HHDM_OFFSET first, this would
        // become flaky. Today the slab tests use their own LocalBuddy
        // (see `slab::tests`) and never touch the global. Should that
        // change, add a `#[cfg(test)]` reset helper and serialise tests
        // through a `std::sync::Mutex<()>`.
        let _ = PAGE_SIZE;
        assert_eq!(hhdm_offset(), 0);
    }
}
