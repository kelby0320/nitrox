//! Per-thread kernel stack with guard page.
//!
//! Each stack is `KERNEL_STACK_PAGES + 1` vmap pages: a deliberately unmapped guard page at the
//! bottom plus `KERNEL_STACK_PAGES` writable / NX / kernel-only pages above, so an overflow faults
//! loudly instead of corrupting whatever sits below.
//!
//! `Drop` unmaps the PTEs and returns the frames to the buddy; the vmap range is **not**
//! released (the bump allocator has no freelist).

use crate::arch::Paging;
use crate::arch::paging::{ArchPaging, PageFlags};
use crate::libkern::AllocError;
use crate::mm::kvmap;
use crate::mm::{PAGE_SIZE, PhysAddr, VirtAddr, heap};

/// Pages of usable stack space per kernel stack (16 KiB total).
pub const KERNEL_STACK_PAGES: usize = 4;
/// Bytes of usable stack space.
pub const KERNEL_STACK_BYTES: u64 = (KERNEL_STACK_PAGES as u64) * (PAGE_SIZE as u64);

/// A per-thread kernel stack: `KERNEL_STACK_PAGES` of mapped, writable, non-executable,
/// kernel-only vmap memory above one unmapped guard page.
///
/// Construction takes the page-table `root` to install into; the shared vmap PDPT makes the PTEs
/// visible to every address space, and `root` is kept for `Drop`.
pub struct KernelStack {
    /// Exclusive top of the stack: the initial RSP. The stack grows down from here.
    top: VirtAddr,
    base: VirtAddr,
    frames: [PhysAddr; KERNEL_STACK_PAGES],
    root: PhysAddr,
}

impl KernelStack {
    /// Allocate a kernel stack: `KERNEL_STACK_PAGES + 1` vmap pages, frames for the top
    /// `KERNEL_STACK_PAGES`, the bottom page left unmapped as the guard. [`AllocError`] if the
    /// vmap, the buddy or page-table-frame allocation fails; a partial failure is rolled back.
    pub fn new(root: PhysAddr) -> Result<Self, AllocError> {
        let guard_base = kvmap::vmap_alloc_pages(KERNEL_STACK_PAGES as u64 + 1)?;
        let base = VirtAddr::new(guard_base.as_u64() + PAGE_SIZE as u64);
        let top = VirtAddr::new(base.as_u64() + KERNEL_STACK_BYTES);

        let mut frames = [PhysAddr::new(0); KERNEL_STACK_PAGES];
        let mut allocated_frames = 0usize;
        for slot in &mut frames {
            match heap::buddy_alloc(0) {
                Some(phys) => {
                    *slot = phys;
                    allocated_frames += 1;
                }
                None => {
                    for &f in &frames[..allocated_frames] {
                        heap::buddy_free(f, 0);
                    }
                    return Err(AllocError);
                }
            }
        }

        // Install PTEs; on failure (out of intermediate page-table frames) unmap and free all.
        let flags = PageFlags::WRITABLE | PageFlags::NO_EXECUTE;
        let mut installed = 0usize;
        for i in 0..KERNEL_STACK_PAGES {
            let virt = VirtAddr::new(base.as_u64() + (i as u64) * (PAGE_SIZE as u64));
            // SAFETY: `root` is a valid PML4 owned by the caller; the
            // vmap region's PDPT was pre-allocated at boot (see
            // `kvmap::init`), so the PTE walk will only allocate PD /
            // PT frames — those modifications hang off the shared
            // PDPT and are visible to every AS.
            let r = unsafe { Paging::map_page(root, virt, frames[i], flags) };
            match r {
                Ok(()) => installed += 1,
                Err(_) => {
                    for j in 0..installed {
                        let v = VirtAddr::new(base.as_u64() + (j as u64) * (PAGE_SIZE as u64));
                        // SAFETY: we just installed this PTE; unmap
                        // returns the frame we mapped.
                        let _ = unsafe { Paging::unmap_page(root, v) };
                    }
                    for f in &frames {
                        heap::buddy_free(*f, 0);
                    }
                    return Err(AllocError);
                }
            }
        }

        // No shootdown on *install*: a not-present to present change needs no cross-CPU
        // invalidation on x86. The vmap VA is freshly allocated (the bump allocator **never
        // reclaims VA**, so no CPU holds a cached translation for it), and the PD/PT hang off the
        // shared vmap hierarchy every address space sees.
        // Re-checked against the open intermittent `#DF` (`TODO(unexplained-df)`): it stands.

        // Paint the fresh stack so its high-water mark is measurable later (see `watermark`).
        #[cfg(feature = "test-harness")]
        // SAFETY: the whole `[base, top)` range was just mapped writable above, and no
        // thread runs on this stack until the caller installs it.
        unsafe {
            watermark::paint(base, KERNEL_STACK_BYTES);
        }

        Ok(KernelStack { top, base, frames, root })
    }

    /// Exclusive top of the stack: the RSP to load for its thread.
    pub fn top(&self) -> VirtAddr {
        self.top
    }

    /// Inclusive base of the mapped region; the guard page below it is unmapped.
    pub fn base(&self) -> VirtAddr {
        self.base
    }

    /// Base of the guard page; any access to it page-faults.
    pub fn guard_page(&self) -> VirtAddr {
        VirtAddr::new(self.base.as_u64() - PAGE_SIZE as u64)
    }
}

/// Kernel-stack high-water measurement (`test-harness` builds only).
///
/// Fresh stacks are painted with [`POISON`]; the deepest point a thread has reached is the lowest
/// address whose word is no longer poison. Sampling at context-switch-out covers every thread
/// that runs. The sample is O(1) unless a new record is being set; the mark is monotonic.
///
/// It records how deep and whose thread, not *where*.
/// TODO(stack-attribution): `docs/rationale/deferred-decisions.md`.
#[cfg(feature = "test-harness")]
pub mod watermark {
    use super::{KERNEL_STACK_BYTES, VirtAddr};
    use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    /// Fill word for an untouched stack: not a canonical address, not a small integer.
    pub const POISON: u64 = 0x5041_494e_5445_4421; // "PAINTED!"

    static DEEPEST: AtomicU64 = AtomicU64::new(0);
    static DEEPEST_PID: AtomicU32 = AtomicU32::new(0);

    /// Paint `bytes` of freshly-mapped stack starting at `base`.
    ///
    /// # Safety
    /// `[base, base + bytes)` must be mapped writable and not in use by any thread.
    pub unsafe fn paint(base: VirtAddr, bytes: u64) {
        let p = base.as_u64() as *mut u64;
        for i in 0..(bytes / 8) {
            // SAFETY: within the caller-guaranteed mapped, unused range.
            unsafe { p.add(i as usize).write(POISON) };
        }
    }

    /// Update the mark from a stack whose exclusive top is `top`; called with the outgoing
    /// thread's stack at a context switch (one load unless the record moves).
    ///
    /// # Safety
    /// `top` must be the exclusive top of a live, painted kernel stack — so
    /// `[top - KERNEL_STACK_BYTES, top)` is mapped and readable.
    pub unsafe fn sample(top: u64, pid: u32) {
        let base = top - KERNEL_STACK_BYTES;
        let record = DEEPEST.load(Ordering::Relaxed);
        let probe = top - record - 8;
        if probe < base {
            return; // the record already reaches the guard page
        }
        // SAFETY: `probe` is inside the mapped stack (checked against `base`).
        if unsafe { (probe as *const u64).read_volatile() } == POISON {
            return;
        }
        let mut addr = probe;
        while addr > base {
            // SAFETY: `addr` stays within `[base, top)` by the loop condition.
            if unsafe { (addr as *const u64).read_volatile() } == POISON {
                break;
            }
            addr -= 8;
        }
        let used = top - addr - 8;
        if DEEPEST.fetch_max(used, Ordering::Relaxed) < used {
            DEEPEST_PID.store(pid, Ordering::Relaxed);
        }
    }

    /// Deepest usage so far: `(bytes, percent of one stack, pid)`; all zero before any sample.
    pub fn deepest() -> (u64, u64, u32) {
        let used = DEEPEST.load(Ordering::Relaxed);
        (used, used * 100 / KERNEL_STACK_BYTES, DEEPEST_PID.load(Ordering::Relaxed))
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        // Clear the PTEs first; the frames are freed only after the shootdown.
        for i in 0..KERNEL_STACK_PAGES {
            let virt = VirtAddr::new(self.base.as_u64() + (i as u64) * (PAGE_SIZE as u64));
            // SAFETY: every page was mapped by `new` and lives in the
            // shared kernel vmap; unmap reverses the install.
            let _ = unsafe { Paging::unmap_page(self.root, virt) };
        }
        // Publish the cleared mappings to every CPU before the frames return to the allocator:
        // the vmap is shared, so another CPU may still cache a translation, and would otherwise
        // touch memory that now belongs to someone else.
        crate::tlb::shootdown_all();
        for i in 0..KERNEL_STACK_PAGES {
            heap::buddy_free(self.frames[i], 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::Paging;
    use crate::arch::paging::ArchPaging;
    use crate::mm::addr_space::AddressSpace;
    use crate::mm::test_support::init_global_heap;

    #[test]
    fn new_installs_stack_pages_and_leaves_guard_unmapped() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let stack = KernelStack::new(asp.root()).unwrap();

        for i in 0..KERNEL_STACK_PAGES as u64 {
            let v = VirtAddr::new(stack.base.as_u64() + i * PAGE_SIZE as u64);
            // SAFETY: read-only walk against the AS we just mapped into.
            let p = unsafe { Paging::translate(asp.root(), v) };
            assert!(p.is_some(), "stack page {i} not mapped");
        }
        // SAFETY: read-only walk.
        let guard = unsafe { Paging::translate(asp.root(), stack.guard_page()) };
        assert!(guard.is_none(), "guard page must be unmapped");
        // SAFETY: read-only walk.
        let above = unsafe { Paging::translate(asp.root(), stack.top) };
        assert!(above.is_none(), "page at top should be unmapped");
    }

    #[test]
    fn top_is_base_plus_stack_bytes() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let stack = KernelStack::new(asp.root()).unwrap();
        assert_eq!(stack.top.as_u64() - stack.base.as_u64(), KERNEL_STACK_BYTES);
    }

    #[test]
    fn guard_page_is_one_page_below_base() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let stack = KernelStack::new(asp.root()).unwrap();
        assert_eq!(stack.base.as_u64() - stack.guard_page().as_u64(), PAGE_SIZE as u64);
    }

    #[test]
    fn multiple_stacks_have_disjoint_ranges() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let s1 = KernelStack::new(asp.root()).unwrap();
        let s2 = KernelStack::new(asp.root()).unwrap();
        let (lo, hi) = if s1.base.as_u64() < s2.base.as_u64() { (&s1, &s2) } else { (&s2, &s1) };
        assert!(
            lo.top.as_u64() <= hi.guard_page().as_u64(),
            "stacks overlap: lo.top={:#x}, hi.guard={:#x}",
            lo.top.as_u64(),
            hi.guard_page().as_u64()
        );
    }

    #[test]
    fn drop_unmaps_stack_pages() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let base = {
            let stack = KernelStack::new(asp.root()).unwrap();
            // SAFETY: read-only walk.
            assert!(
                // SAFETY: `asp`'s root is the live page table this test built.
                unsafe { Paging::translate(asp.root(), stack.base) }.is_some(),
                "stack base must be mapped before drop"
            );
            stack.base
        }; // stack dropped here
        // SAFETY: read-only walk against the now-cleared mapping.
        assert!(unsafe { Paging::translate(asp.root(), base) }.is_none(), "stack base must be unmapped after drop");
    }
}
