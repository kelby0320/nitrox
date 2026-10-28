//! Host-test helpers (`cfg(test)`): [`init_global_heap`] boots the production `BUDDY` /
//! `SLAB_CACHES` statics for the `libkern` containers, which have no allocator-injection seam.

use std::sync::Once;

use crate::arch::Paging;
use crate::arch::paging::ArchPaging;
use crate::limine::{MEMMAP_USABLE, MemoryMapEntry, MemoryMapResponse};
use crate::mm::{PAGE_SIZE, heap, slab};

static HEAP_INIT: Once = Once::new();

/// Boot the global buddy and slab allocators once, against a leaked 16 MiB host buffer. Idempotent
/// and thread-safe; the HHDM offset is `0` because fake physical addresses are host addresses.
pub fn init_global_heap() {
    HEAP_INIT.call_once(|| {
        const HEAP_BYTES: usize = 16 * 1024 * 1024;

        // Leaked page-aligned backing region; two spare pages for alignment.
        let backing: &'static mut [u8] = vec![0u8; HEAP_BYTES + 2 * PAGE_SIZE].leak();
        let base = backing.as_mut_ptr() as usize;
        let aligned = (base + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        assert!(aligned as u64 >= 0x10_0000, "host buffer sits below the buddy's 1 MiB cutoff");

        let entries: &'static mut [MemoryMapEntry] =
            vec![MemoryMapEntry { base: aligned as u64, length: HEAP_BYTES as u64, kind: MEMMAP_USABLE }].leak();
        let entry_ptrs: &'static mut [*mut MemoryMapEntry] = vec![entries.as_mut_ptr()].leak();
        let response: &'static MemoryMapResponse =
            Box::leak(Box::new(MemoryMapResponse { revision: 0, entry_count: 1, entries: entry_ptrs.as_mut_ptr() }));

        // SAFETY: `response` describes one usable region that is a live,
        // leaked, page-aligned host allocation; an HHDM offset of 0 is
        // correct because the fake physical addresses double as host
        // virtual addresses. This satisfies `init_buddy`'s contract.
        unsafe { heap::init_buddy(response, 0) };
        slab::slab_init();

        // A boot root holding the vmap's window, as `paging_init` builds it, so every
        // `AddressSpace::new` shares the window as at boot.
        let boot_root = heap::buddy_alloc(0).expect("a frame for the test boot root");
        // SAFETY: a fresh, unaliased frame, reachable at its own address under the test HHDM
        // offset of 0. Zeroed, it is an empty PML4; `kvmap::init` links the window into it before
        // the template is captured, and it is never freed.
        unsafe {
            core::ptr::write_bytes(boot_root.as_u64() as *mut u8, 0, PAGE_SIZE);
            crate::mm::kvmap::init(boot_root);
            Paging::init_kernel_template(boot_root);
        }
    });
}
