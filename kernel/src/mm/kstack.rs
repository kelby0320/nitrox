//! Per-thread kernel stack with guard page.
//!
//! Each kernel thread that will ever run in ring 0 (via syscall,
//! interrupt, or exception entry) needs its own kernel stack —
//! switching between threads switches between stacks. Today nothing
//! creates threads, so [`KernelStack`] has no production consumer; it
//! lands now because the underlying infrastructure (the kernel-vmap
//! allocator + guard-page discipline) belongs with the memory
//! subsystem rather than with the future threading slice.
//!
//! Each stack occupies `KERNEL_STACK_PAGES + 1` virtual pages in the
//! kernel vmap region: a guard page at the bottom (deliberately
//! unmapped) plus `KERNEL_STACK_PAGES` writable / NX / kernel-only
//! pages above. Stack overflow into the guard page faults loudly via
//! the page-fault handler instead of silently corrupting whatever
//! sits below.
//!
//! ## Drop
//!
//! `Drop` unmaps the stack PTEs and returns the frames to the buddy.
//! The vmap region itself is **not** released — the bump allocator
//! has no freelist. That's fine for Phase 1; if kernel stacks ever
//! churn heavily (they shouldn't — a stack lives as long as its
//! thread), a vmap freelist is a local addition.

use crate::arch::paging::{ArchPaging, PageFlags};
use crate::arch::Paging;
use crate::libkern::AllocError;
use crate::mm::kvmap;
use crate::mm::{PAGE_SIZE, PhysAddr, VirtAddr, heap};

/// Pages of usable stack space per kernel stack (16 KiB total).
pub const KERNEL_STACK_PAGES: usize = 4;
/// Bytes of usable stack space.
pub const KERNEL_STACK_BYTES: u64 = (KERNEL_STACK_PAGES as u64) * (PAGE_SIZE as u64);

/// A per-thread kernel stack: `KERNEL_STACK_PAGES` of mapped,
/// writable, non-executable, kernel-only memory in the kernel vmap
/// region, preceded by one unmapped guard page.
///
/// Construction takes the page-table `root` the stack should be
/// installed into. The shared kernel-vmap PDPT means the resulting
/// PTEs are visible to every address space, regardless of which
/// `root` was passed — `root` is stored so `Drop` can clear the
/// PTEs symmetrically.
pub struct KernelStack {
    /// Exclusive top of the stack — the value to load into the
    /// initial RSP. Stack grows down from here.
    top: VirtAddr,
    /// Inclusive base of the mapped stack region.
    base: VirtAddr,
    /// Physical frames backing the stack pages, low-address-first.
    frames: [PhysAddr; KERNEL_STACK_PAGES],
    /// Page-table root the stack was installed into. Drop uses it.
    root: PhysAddr,
}

impl KernelStack {
    /// Allocate a kernel stack: reserve `KERNEL_STACK_PAGES + 1`
    /// virtual pages in the kernel vmap, allocate frames for the top
    /// `KERNEL_STACK_PAGES`, install them writable / NX / kernel-only.
    /// The bottom page is the guard — left unmapped.
    ///
    /// Returns [`AllocError`] if the vmap allocator, the buddy, or
    /// page-table-frame allocation fails. On a partial failure the
    /// already-allocated frames and installed PTEs are rolled back so
    /// the caller sees an all-or-nothing outcome.
    pub fn new(root: PhysAddr) -> Result<Self, AllocError> {
        // Reserve N+1 virtual pages: 1 guard + N stack.
        let guard_base = kvmap::vmap_alloc_pages(KERNEL_STACK_PAGES as u64 + 1)?;
        let base = VirtAddr::new(guard_base.as_u64() + PAGE_SIZE as u64);
        let top = VirtAddr::new(base.as_u64() + KERNEL_STACK_BYTES);

        // Allocate one frame per stack page. Roll back on any failure.
        let mut frames = [PhysAddr::new(0); KERNEL_STACK_PAGES];
        let mut allocated_frames = 0usize;
        for slot in &mut frames {
            match heap::buddy_alloc(0) {
                Some(phys) => {
                    *slot = phys;
                    allocated_frames += 1;
                }
                None => {
                    for j in 0..allocated_frames {
                        heap::buddy_free(frames[j], 0);
                    }
                    return Err(AllocError);
                }
            }
        }

        // Install PTEs. Roll back installed PTEs + free all frames on
        // any failure (out of intermediate page-table frames).
        //
        // **No shootdown here, and the unmap path below has one — `TODO(kstack-vmap-coherence)`.**
        // These PTEs, and any PD/PT frames the walk allocates, hang off the *shared* kernel-vmap
        // PDPT and are therefore visible to every address space. x86 caches paging-structure
        // entries as well as final translations, so a CPU that has walked this region before can
        // hold a stale intermediate entry and miss a newly installed stack. The decision log
        // (2026-05-29) named this as the residual cause of an intermittent `#DF` — a thread's
        // first exception delivered onto a kernel stack whose translation is stale on the
        // running CPU, so the `RSP0` push itself faults — and it is still open.
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
                        let v = VirtAddr::new(
                            base.as_u64() + (j as u64) * (PAGE_SIZE as u64),
                        );
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

        // No shootdown on *install*: adding a mapping (not-present → present) needs
        // no cross-CPU invalidation on x86 — the vmap VA is freshly allocated (no
        // CPU has a cached translation for it), and the PD/PT are installed into
        // the shared-by-reference vmap hierarchy, so every address space sees them.
        // (Matches Linux, which shoots down only on unmap / permission-restrict.)

        // Paint the fresh stack so its high-water mark is measurable later (see
        // `watermark`). Instrumentation only — a production stack is left as the buddy
        // handed it over.
        #[cfg(feature = "test-harness")]
        // SAFETY: the whole `[base, top)` range was just mapped writable above, and no
        // thread runs on this stack until the caller installs it.
        unsafe {
            watermark::paint(base, KERNEL_STACK_BYTES);
        }

        Ok(KernelStack {
            top,
            base,
            frames,
            root,
        })
    }

    /// Exclusive top of the stack — the value to load into RSP when
    /// switching to a thread that owns this stack. Pre-decrement
    /// convention: pushes write at `top - 8` first.
    pub fn top(&self) -> VirtAddr {
        self.top
    }

    /// Inclusive base of the mapped stack region. The guard page sits
    /// at `base - PAGE_SIZE` and is unmapped — a stack-overflow write
    /// to it page-faults.
    pub fn base(&self) -> VirtAddr {
        self.base
    }

    /// Base of the guard page. Reading from or writing to this address
    /// page-faults — that's the overflow detector.
    pub fn guard_page(&self) -> VirtAddr {
        VirtAddr::new(self.base.as_u64() - PAGE_SIZE as u64)
    }
}

/// Kernel-stack high-water measurement (`test-harness` builds only).
///
/// Exists because the kernel stack budget was being reasoned about from *estimates* — the
/// `MAX_WAIT_HANDLES` sizing argument (Slice C3) summed array widths on paper. This
/// replaces that with a number.
///
/// Fresh stacks are painted with [`POISON`]; the deepest point any thread has ever reached
/// is then just the lowest address whose word is no longer poison. Sampling that at
/// context-switch-out covers **every** thread that actually runs, including servers that
/// spend their lives blocked and so never appear in a run queue.
///
/// The sample is **O(1) in the common case**: it only reads the word immediately below the
/// standing record, and walks further down only when that word has been overwritten — i.e.
/// only when a new record is genuinely being set, which happens a handful of times per
/// boot. The mark is monotonic, so a thread that ran deep and returned still counts.
///
/// It records how deep and whose thread, but not *where* — that needs a return-address
/// capture at the moment the record is set.
/// TODO(stack-attribution): `docs/rationale/deferred-decisions.md`.
#[cfg(feature = "test-harness")]
pub mod watermark {
    use super::{KERNEL_STACK_BYTES, VirtAddr};
    use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    /// Fill word for an untouched stack. Chosen to be implausible as a real value: not a
    /// canonical address, not a small integer.
    pub const POISON: u64 = 0x5041_494e_5445_4421; // "PAINTED!"

    /// Deepest number of bytes any kernel stack has been observed to use.
    static DEEPEST: AtomicU64 = AtomicU64::new(0);
    /// Owning pid of the thread that set the standing record (`0` = a kernel thread).
    /// A depth on its own says "something went deep"; this says *who*, which is the
    /// difference between a number and a lead.
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

    /// Update the mark from a stack whose exclusive top is `top`.
    ///
    /// Called with the *outgoing* thread's stack at a context switch. Cheap enough for that
    /// path: one load unless the record actually moves.
    ///
    /// # Safety
    /// `top` must be the exclusive top of a live, painted kernel stack — so
    /// `[top - KERNEL_STACK_BYTES, top)` is mapped and readable.
    pub unsafe fn sample(top: u64, pid: u32) {
        let base = top - KERNEL_STACK_BYTES;
        let record = DEEPEST.load(Ordering::Relaxed);
        // The word just below the standing record. Still poison ⇒ nothing new; this is the
        // whole cost on all but a few switches per boot.
        let probe = top - record - 8;
        if probe < base {
            return; // the record already reaches the guard page
        }
        // SAFETY: `probe` is inside the mapped stack (checked against `base`).
        if unsafe { (probe as *const u64).read_volatile() } == POISON {
            return;
        }
        // A new record: walk down to the first still-poison word.
        let mut addr = probe;
        while addr > base {
            // SAFETY: `addr` stays within `[base, top)` by the loop condition.
            if unsafe { (addr as *const u64).read_volatile() } == POISON {
                break;
            }
            addr -= 8;
        }
        let used = top - addr - 8;
        // Record the pid only if this sample actually won, so the two stay consistent.
        if DEEPEST.fetch_max(used, Ordering::Relaxed) < used {
            DEEPEST_PID.store(pid, Ordering::Relaxed);
        }
    }

    /// Deepest kernel-stack usage observed so far: `(bytes, percent of one stack, pid of
    /// the thread that reached it)`. All zero before any sample.
    pub fn deepest() -> (u64, u64, u32) {
        let used = DEEPEST.load(Ordering::Relaxed);
        (
            used,
            used * 100 / KERNEL_STACK_BYTES,
            DEEPEST_PID.load(Ordering::Relaxed),
        )
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        // Clear the PTEs first (do **not** free the frames yet).
        for i in 0..KERNEL_STACK_PAGES {
            let virt = VirtAddr::new(self.base.as_u64() + (i as u64) * (PAGE_SIZE as u64));
            // SAFETY: every page was mapped by `new` and lives in the
            // shared kernel vmap; unmap reverses the install.
            let _ = unsafe { Paging::unmap_page(self.root, virt) };
        }
        // Publish the cleared mappings to every CPU before the backing frames
        // return to the allocator: these pages live in the **shared** kernel vmap,
        // so another CPU could still hold a cached translation for them — it must
        // drop it before the frame is recycled (else it would read/write, or
        // deliver an exception onto, memory that now belongs to someone else).
        crate::tlb::shootdown_all();
        for i in 0..KERNEL_STACK_PAGES {
            heap::buddy_free(self.frames[i], 0);
        }
        // The vmap region (KERNEL_STACK_PAGES + 1 pages) is not
        // reclaimed — the bump allocator has no freelist. Phase 1
        // kernel stacks are constructed once per thread and freed
        // when the thread ends; churn is negligible.
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

        // Every stack page must be mapped.
        for i in 0..KERNEL_STACK_PAGES as u64 {
            let v = VirtAddr::new(stack.base.as_u64() + i * PAGE_SIZE as u64);
            // SAFETY: read-only walk against the AS we just mapped into.
            let p = unsafe { Paging::translate(asp.root(), v) };
            assert!(p.is_some(), "stack page {i} not mapped");
        }
        // The guard page must NOT be mapped.
        // SAFETY: read-only walk.
        let guard = unsafe { Paging::translate(asp.root(), stack.guard_page()) };
        assert!(guard.is_none(), "guard page must be unmapped");
        // The page at `top` is past the stack and also unmapped.
        // SAFETY: read-only walk.
        let above = unsafe { Paging::translate(asp.root(), stack.top) };
        assert!(above.is_none(), "page at top should be unmapped");
    }

    #[test]
    fn top_is_base_plus_stack_bytes() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let stack = KernelStack::new(asp.root()).unwrap();
        assert_eq!(
            stack.top.as_u64() - stack.base.as_u64(),
            KERNEL_STACK_BYTES
        );
    }

    #[test]
    fn guard_page_is_one_page_below_base() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let stack = KernelStack::new(asp.root()).unwrap();
        assert_eq!(
            stack.base.as_u64() - stack.guard_page().as_u64(),
            PAGE_SIZE as u64
        );
    }

    #[test]
    fn multiple_stacks_have_disjoint_ranges() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let s1 = KernelStack::new(asp.root()).unwrap();
        let s2 = KernelStack::new(asp.root()).unwrap();
        // The vmap bump allocator hands out monotonically-increasing
        // ranges, so whichever was allocated first must end before
        // the other begins (no overlap).
        let (lo, hi) = if s1.base.as_u64() < s2.base.as_u64() {
            (&s1, &s2)
        } else {
            (&s2, &s1)
        };
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
                unsafe { Paging::translate(asp.root(), stack.base) }.is_some(),
                "stack base must be mapped before drop"
            );
            stack.base
        }; // stack dropped here
        // SAFETY: read-only walk against the now-cleared mapping.
        assert!(
            unsafe { Paging::translate(asp.root(), base) }.is_none(),
            "stack base must be unmapped after drop"
        );
    }
}
