//! Kernel heap facade: the single owner of the buddy allocator.
//!
//! The buddy allocator (`mm/buddy.rs`) is `!Sync`; this wraps it in a
//! `SpinLock<Option<BuddyAllocator>>`. The slab reaches it through [`BuddyPager`]. Order:
//! [`init_buddy`] once from `kernel_main`, then [`super::slab::slab_init`]; the buddy functions
//! panic before [`init_buddy`].

use core::sync::atomic::{AtomicU64, Ordering};

use crate::libkern::SpinLock;
use crate::libkern::lockrank::LockRank;
use crate::limine::MemoryMapResponse;
use crate::mm::PhysAddr;
use crate::mm::buddy::BuddyAllocator;

/// The single global buddy allocator. `None` until [`init_buddy`].
static BUDDY: SpinLock<Option<BuddyAllocator>> = SpinLock::new(LockRank::Buddy, None);

/// HHDM offset captured at init, read-only after; separate from `BUDDY` so it needs no lock.
static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Initialise the buddy allocator. Must be called exactly once, early in `kernel_main`.
///
/// # Safety
///
/// All the requirements of [`BuddyAllocator::new`] apply: `memory_map`
/// must be a valid Limine response and `hhdm_offset` must be the
/// bootloader's HHDM base.
pub unsafe fn init_buddy(memory_map: &MemoryMapResponse, hhdm_offset: u64) {
    let mut slot = BUDDY.lock();
    assert!(slot.is_none(), "heap::init_buddy called twice; the buddy allocator must be initialised exactly once");
    // SAFETY: forwarded from the caller per the function-level contract.
    let allocator = unsafe { BuddyAllocator::new(memory_map, hhdm_offset) };
    *slot = Some(allocator);
    HHDM_OFFSET.store(hhdm_offset, Ordering::Release);
}

/// Allocate `2^order` contiguous physical frames; `None` on OOM. Panics before [`init_buddy`].
pub fn buddy_alloc(order: usize) -> Option<PhysAddr> {
    let mut slot = BUDDY.lock();
    slot.as_mut().expect("heap::buddy_alloc called before init_buddy").alloc(order)
}

/// Return frames from [`buddy_alloc`]; `order` must match. Panics before [`init_buddy`].
pub fn buddy_free(addr: PhysAddr, order: usize) {
    let mut slot = BUDDY.lock();
    slot.as_mut().expect("heap::buddy_free called before init_buddy").free(addr, order);
}

/// HHDM offset captured at [`init_buddy`] time; `0` before init (no panic: the slab allocator
/// panics on its own `SLAB_INITIALISED`).
pub fn hhdm_offset() -> u64 {
    HHDM_OFFSET.load(Ordering::Acquire)
}

/// The buddy allocator as the slab sees it: [`HeapBuddy`] in production, a `LocalBuddy` over a
/// per-test `BuddyAllocator` in tests.
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

    #[test]
    fn hhdm_offset_is_zero_before_init() {
        // `HHDM_OFFSET` is process-global and `init_global_heap` sets it, to 0: this holds only
        // while every host caller of `init_buddy` passes 0.
        let _ = PAGE_SIZE;
        assert_eq!(hhdm_offset(), 0);
    }
}
