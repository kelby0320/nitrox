//! Per-thread kernel stack with guard page.
//!
//! Each stack is `KERNEL_STACK_PAGES + 1` vmap pages: a deliberately unmapped guard page at the
//! bottom plus `KERNEL_STACK_PAGES` writable / NX / kernel-only pages above, so an overflow faults
//! loudly instead of corrupting whatever sits below.
//!
//! The pages are a [`VmapFrames`]: dropping one shoots it down, frees it and reuses its VA.

use crate::arch::paging::PageFlags;
use crate::arch::{ArchThreadContext, fabricate_frame};
use crate::libkern::AllocError;
use crate::mm::kvmap::VmapFrames;
use crate::mm::{PAGE_SIZE, VirtAddr};

/// Pages of usable stack space per kernel stack (16 KiB total).
pub const KERNEL_STACK_PAGES: usize = 4;
/// Bytes of usable stack space.
pub const KERNEL_STACK_BYTES: u64 = (KERNEL_STACK_PAGES as u64) * (PAGE_SIZE as u64);

/// A kernel stack's top for a CPU's trap and syscall stacks, which only a [`KernelStack`] makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelStackTop(u64);

impl KernelStackTop {
    /// The address.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// A per-thread kernel stack: `KERNEL_STACK_PAGES` of mapped, writable, non-executable,
/// kernel-only vmap memory above one unmapped guard page.
pub struct KernelStack {
    pages: VmapFrames<KERNEL_STACK_PAGES>,
}

impl KernelStack {
    /// Allocate a kernel stack: `KERNEL_STACK_PAGES + 1` vmap pages, frames for the top
    /// `KERNEL_STACK_PAGES`, the bottom page left unmapped as the guard. [`AllocError`] if the
    /// vmap, the buddy or page-table-frame allocation fails; a partial failure is rolled back.
    pub fn new() -> Result<Self, AllocError> {
        let pages = VmapFrames::new(PageFlags::WRITABLE | PageFlags::NO_EXECUTE, 1)?;
        // No shootdown on *install*: a not-present to present change needs no cross-CPU
        // invalidation on x86, and the VA is fresh or was shot down when its last stack dropped.
        // Re-checked against the open intermittent `#DF` (`TODO(unexplained-df)`): it stands.

        // Paint the fresh stack so its high-water mark is measurable later (see `watermark`).
        #[cfg(feature = "test-harness")]
        // SAFETY: the whole `[base, top)` range was just mapped writable above, and no
        // thread runs on this stack until the caller installs it.
        unsafe {
            watermark::paint(pages.base(), KERNEL_STACK_BYTES);
        }

        Ok(KernelStack { pages })
    }

    /// Exclusive top of the stack: the RSP to load for its thread.
    pub fn top(&self) -> VirtAddr {
        self.pages.end()
    }

    /// The top, to set a CPU's trap and syscall stacks to while its thread runs there.
    pub fn stack_top(&self) -> KernelStackTop {
        KernelStackTop(self.top().as_u64())
    }

    /// Lay out the frame the first switch into this stack's thread returns through, to `trampoline`.
    pub fn fabricate(&mut self, trampoline: u64) -> ArchThreadContext {
        // SAFETY: this stack's own pages, mapped writable, which `&mut self` keeps any thread off.
        unsafe { fabricate_frame(self.top().as_u64(), trampoline) }
    }

    /// Inclusive base of the mapped region; the guard page below it is unmapped.
    pub fn base(&self) -> VirtAddr {
        self.pages.base()
    }

    /// Base of the guard page; any access to it page-faults.
    pub fn guard_page(&self) -> VirtAddr {
        self.pages.guard_base()
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

#[cfg(test)]
mod tests;
