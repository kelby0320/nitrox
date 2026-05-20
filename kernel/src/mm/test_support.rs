//! Host-test helpers for the memory-management subsystem.
//!
//! Compiled only under `cfg(test)`. The key helper is
//! [`init_global_heap`], which boots the production `BUDDY` /
//! `SLAB_CACHES` statics against a leaked host allocation, so that
//! `kmalloc` / `kfree` — and the `libkern` containers layered on them —
//! can be exercised by `cargo test`.
//!
//! This deliberately drives the *global* allocator path. The buddy
//! module's own `FakeMem` and the slab module's copy of it build *local*
//! allocators instead, to keep those modules' tests hermetic; the
//! `libkern` containers have no allocator-injection seam, so they need
//! the real statics to be live.

use std::sync::Once;

use crate::limine::{MEMMAP_USABLE, MemoryMapEntry, MemoryMapResponse};
use crate::mm::{PAGE_SIZE, heap, slab};

static HEAP_INIT: Once = Once::new();

/// Boot the global buddy and slab allocators once, against a leaked
/// 16 MiB host buffer.
///
/// Idempotent and thread-safe: every test may call it unconditionally.
/// The heap is initialised exactly once and then shared — it is
/// internally locked, exactly as on the real kernel, so parallel tests
/// allocating against it is sound. The HHDM offset is `0` because the
/// fake "physical" addresses are real host addresses.
pub fn init_global_heap() {
    HEAP_INIT.call_once(|| {
        const HEAP_BYTES: usize = 16 * 1024 * 1024;

        // Leak a page-aligned backing region that outlives the test
        // process; the buddy hands frames out of it for the run's
        // lifetime. The extra two pages give slack for alignment.
        let backing: &'static mut [u8] = vec![0u8; HEAP_BYTES + 2 * PAGE_SIZE].leak();
        let base = backing.as_mut_ptr() as usize;
        let aligned = (base + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        assert!(
            aligned as u64 >= 0x10_0000,
            "host buffer sits below the buddy's 1 MiB cutoff"
        );

        let entries: &'static mut [MemoryMapEntry] = vec![MemoryMapEntry {
            base: aligned as u64,
            length: HEAP_BYTES as u64,
            kind: MEMMAP_USABLE,
        }]
        .leak();
        let entry_ptrs: &'static mut [*mut MemoryMapEntry] =
            vec![entries.as_mut_ptr()].leak();
        let response: &'static MemoryMapResponse = Box::leak(Box::new(MemoryMapResponse {
            revision: 0,
            entry_count: 1,
            entries: entry_ptrs.as_mut_ptr(),
        }));

        // SAFETY: `response` describes one usable region that is a live,
        // leaked, page-aligned host allocation; an HHDM offset of 0 is
        // correct because the fake physical addresses double as host
        // virtual addresses. This satisfies `init_buddy`'s contract.
        unsafe { heap::init_buddy(response, 0) };
        slab::slab_init();
    });
}
