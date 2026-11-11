use super::*;
use crate::mm::heap;
use crate::mm::test_support::init_global_heap;

/// A zeroed frame, the test's to leak or take back: a table, or a page to map.
fn alloc_page_table() -> Option<PhysAddr> {
    new_table().map(Frame::into_pte)
}

std::thread_local! {
    static OTHER_CPUS_TABLE: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// Another CPU links its own table into the slot this one has just read empty.
fn other_cpu_links_first(slot: &core::sync::atomic::AtomicU64) {
    let table = alloc_page_table().unwrap();
    OTHER_CPUS_TABLE.set(table.as_u64());
    slot.store(Pte::new_table(table).0, core::sync::atomic::Ordering::Release);
}

#[test]
fn a_cpu_beaten_to_an_empty_slot_takes_the_table_linked_first() {
    init_global_heap();
    let parent = alloc_page_table().unwrap();
    BEFORE_LINK.set(Some(other_cpu_links_first));
    let freed = heap::frames_freed_by_this_thread();
    // SAFETY: `parent` is a fresh, zeroed table only this test touches.
    let parent_table = unsafe { TableRef::root(parent) };
    let got = ensure_table(parent_table, 7).unwrap();
    let first = PhysAddr::new(OTHER_CPUS_TABLE.get());
    assert_eq!(parent_table.entry(7).phys(), first, "the first table was unlinked");
    assert_eq!(got.entries.as_ptr(), table_ptr(first).cast(), "the first table is the one taken");
    assert_eq!(heap::frames_freed_by_this_thread() - freed, 1, "the second table goes back");
    // SAFETY: the tables this test made, which nothing walks from here.
    drop(unsafe { (Frame::from_pte(first), Frame::from_pte(parent)) });
}

#[test]
fn index_split_isolates_each_level() {
    let v = VirtAddr::new((1 << 39) | (2 << 30) | (3 << 21) | (4 << 12) | 0x5);
    assert_eq!(pml4_index(v), 1);
    assert_eq!(pdpt_index(v), 2);
    assert_eq!(pd_index(v), 3);
    assert_eq!(pt_index(v), 4);
    assert_eq!(leaf_offset(1, v), 0x5);
    assert_eq!(leaf_offset(2, v), (4 << 12) | 0x5);
    assert_eq!(leaf_offset(3, v), (3 << 21) | (4 << 12) | 0x5);
}

#[test]
fn index_helpers_mask_to_nine_bits() {
    let v = VirtAddr::new(u64::MAX);
    assert_eq!(pml4_index(v), 0x1FF);
    assert_eq!(pdpt_index(v), 0x1FF);
    assert_eq!(pd_index(v), 0x1FF);
    assert_eq!(pt_index(v), 0x1FF);
    assert_eq!((leaf_offset(1, v), leaf_offset(2, v), leaf_offset(3, v)), (0xFFF, 0x1F_FFFF, 0x3FFF_FFFF));
}

#[test]
fn flags_translate_to_expected_bits() {
    assert_eq!(flags_to_pte_bits(PageFlags::empty()), 0);
    assert_eq!(flags_to_pte_bits(PageFlags::WRITABLE), PTE_WRITABLE);
    assert_eq!(flags_to_pte_bits(PageFlags::USER), PTE_USER);
    assert_eq!(flags_to_pte_bits(PageFlags::NO_EXECUTE), PTE_NX);
    assert_eq!(flags_to_pte_bits(PageFlags::GLOBAL), PTE_GLOBAL);
    assert_eq!(flags_to_pte_bits(PageFlags::NO_CACHE), PTE_PCD);
    assert_eq!(flags_to_pte_bits(PageFlags::WRITE_THROUGH), PTE_PWT);
    let combo = PageFlags::WRITABLE | PageFlags::USER | PageFlags::NO_EXECUTE;
    assert_eq!(flags_to_pte_bits(combo), PTE_WRITABLE | PTE_USER | PTE_NX);
}

#[test]
fn pte_leaf_round_trip() {
    let phys = PhysAddr::new(0x1234_5000);
    let pte = Pte::new_leaf(phys, PageFlags::WRITABLE);
    assert!(pte.is_present());
    assert!(!pte.is_huge());
    assert_eq!(pte.phys(), phys);
    assert_eq!(pte.0 & PTE_WRITABLE, PTE_WRITABLE);
    assert!(!Pte::empty().is_present());
}

#[test]
fn pte_table_entry_is_present_writable_user() {
    let child = PhysAddr::new(0x9_A000);
    let pte = Pte::new_table(child);
    assert!(pte.is_present());
    assert_eq!(pte.phys(), child);
    assert_eq!(pte.0 & PTE_WRITABLE, PTE_WRITABLE);
    assert_eq!(pte.0 & PTE_USER, PTE_USER);
}

#[test]
fn map_translate_unmap_round_trip() {
    init_global_heap();
    let root = alloc_page_table().expect("root frame");
    let phys = alloc_page_table().expect("target frame");
    let virt = VirtAddr::new(0x4000_0000);

    // SAFETY: `root` is a freshly zeroed table; `virt`/`phys` are
    // page-aligned and `virt` is canonical; the host heap hands out
    // HHDM-reachable frames (HHDM offset 0 under `init_global_heap`).
    unsafe {
        X86Paging::map_page(root, virt, phys, PageFlags::WRITABLE).unwrap();
        assert_eq!(X86Paging::translate(root, virt), Some(phys));
    }
}

#[test]
fn double_map_is_rejected() {
    init_global_heap();
    let root = alloc_page_table().unwrap();
    let phys = alloc_page_table().unwrap();
    let virt = VirtAddr::new(0x8000_0000);
    // SAFETY: see `map_translate_unmap_round_trip`.
    unsafe {
        X86Paging::map_page(root, virt, phys, PageFlags::empty()).unwrap();
        assert_eq!(X86Paging::map_page(root, virt, phys, PageFlags::empty()), Err(MapError::AlreadyMapped),);
    }
}

#[test]
fn unmap_returns_frame_then_reports_not_mapped() {
    init_global_heap();
    let root = alloc_page_table().unwrap();
    let phys = alloc_page_table().unwrap();
    let virt = VirtAddr::new(0xC000_0000);
    // SAFETY: see `map_translate_unmap_round_trip`.
    unsafe {
        X86Paging::map_page(root, virt, phys, PageFlags::WRITABLE).unwrap();
        assert_eq!(X86Paging::unmap_page(root, virt), Ok(phys));
        assert_eq!(X86Paging::translate(root, virt), None);
        assert_eq!(X86Paging::unmap_page(root, virt), Err(UnmapError::NotMapped));
    }
}

#[test]
fn unmap_of_never_mapped_address_reports_not_mapped() {
    init_global_heap();
    let root = alloc_page_table().unwrap();
    let virt = VirtAddr::new(0x1_0000_0000);
    // SAFETY: `root` is a valid, empty table; nothing is mapped.
    unsafe {
        assert_eq!(X86Paging::unmap_page(root, virt), Err(UnmapError::NotMapped));
    }
}

#[test]
fn misaligned_and_noncanonical_inputs_are_rejected() {
    init_global_heap();
    let root = alloc_page_table().unwrap();
    let phys = alloc_page_table().unwrap();
    // SAFETY: `root` is a valid, empty table; each call fails an
    // alignment or canonical check before any table walk.
    unsafe {
        assert_eq!(
            X86Paging::map_page(root, VirtAddr::new(0x1234), phys, PageFlags::empty()),
            Err(MapError::Misaligned),
            "unaligned virtual address",
        );
        assert_eq!(
            X86Paging::map_page(root, VirtAddr::new(0x0001_0000_0000_0000), phys, PageFlags::empty(),),
            Err(MapError::Misaligned),
            "non-canonical virtual address",
        );
        assert_eq!(
            X86Paging::map_page(root, VirtAddr::new(0x1000), PhysAddr::new(0x800), PageFlags::empty(),),
            Err(MapError::Misaligned),
            "unaligned physical address",
        );
    }
}

#[test]
fn maps_spanning_distinct_top_level_entries() {
    init_global_heap();
    let root = alloc_page_table().unwrap();
    // Three addresses in distinct PML4 slots force intermediate table allocation.
    let cases = [
        VirtAddr::new(0x0000_0000_0000_1000), // PML4 slot 0
        VirtAddr::new(0x0000_0080_0000_1000), // PML4 slot 1 (512 GiB)
        VirtAddr::new(0x0000_0100_0000_1000), // PML4 slot 2 (1 TiB)
    ];
    for v in cases {
        let phys = alloc_page_table().unwrap();
        // SAFETY: see `map_translate_unmap_round_trip`.
        unsafe {
            X86Paging::map_page(root, v, phys, PageFlags::WRITABLE).unwrap();
            assert_eq!(X86Paging::translate(root, v), Some(phys));
        }
    }
}

// ----- Kernel-half PML4 template -----

/// Fill a PML4 frame with `value` in every entry, returning its physical base.
fn fill_pml4(value: u64) -> PhysAddr {
    let frame = alloc_page_table().unwrap();
    // SAFETY: just allocated, page-aligned, HHDM-reachable; we write
    // exactly the 512 entries the frame holds.
    unsafe {
        let pml4 = (frame.as_u64() + heap::hhdm_offset()) as *mut u64;
        for i in 0..PAGE_TABLE_ENTRIES {
            *pml4.add(i) = value.wrapping_add(i as u64);
        }
    }
    frame
}

fn read_all_entries(frame: PhysAddr) -> [u64; PAGE_TABLE_ENTRIES] {
    let mut out = [0u64; PAGE_TABLE_ENTRIES];
    // SAFETY: PML4 is page-aligned and HHDM-reachable.
    unsafe {
        let pml4 = (frame.as_u64() + heap::hhdm_offset()) as *const u64;
        for (i, e) in out.iter_mut().enumerate() {
            *e = *pml4.add(i);
        }
    }
    out
}

#[test]
fn read_kernel_half_entries_captures_only_kernel_half() {
    init_global_heap();
    // 0xDEAD_0000_0000_0000 + index gives a distinct value per entry across both halves.
    let source = fill_pml4(0xDEAD_0000_0000_0000);

    // SAFETY: `source` was just constructed and is HHDM-reachable.
    let captured = unsafe { read_kernel_half_entries(source) };

    #[allow(clippy::needless_range_loop, reason = "the index names the entry in each assertion's message")]
    for i in 0..KERNEL_PML4_COUNT {
        let expected = 0xDEAD_0000_0000_0000u64.wrapping_add((KERNEL_PML4_BASE + i) as u64);
        assert_eq!(
            captured[i],
            expected,
            "kernel entry {i} (PML4 slot {}): expected {expected:#x}, got {:#x}",
            KERNEL_PML4_BASE + i,
            captured[i]
        );
    }
}

#[test]
fn write_kernel_half_entries_preserves_user_half() {
    init_global_heap();
    // Prepopulated with a user-half marker to verify the write leaves entries 0..256 alone.
    let target = fill_pml4(0xC0DE_0000_0000_0000);
    let entries: [u64; KERNEL_PML4_COUNT] = core::array::from_fn(|i| 0xBEEF_0000_0000_0000u64.wrapping_add(i as u64));

    // SAFETY: `target` is page-aligned and HHDM-reachable.
    unsafe { write_kernel_half_entries(target, &entries) };

    let after = read_all_entries(target);
    #[allow(clippy::needless_range_loop, reason = "the index names the entry in each assertion's message")]
    for i in 0..KERNEL_PML4_BASE {
        // User-half entries untouched: they still match the original
        // `fill_pml4` pattern.
        let expected = 0xC0DE_0000_0000_0000u64.wrapping_add(i as u64);
        assert_eq!(after[i], expected, "user-half entry {i} was modified");
    }
    for i in 0..KERNEL_PML4_COUNT {
        assert_eq!(after[KERNEL_PML4_BASE + i], entries[i], "kernel-half entry {i} was not written");
    }
}

#[test]
fn read_then_write_round_trips_the_kernel_half() {
    init_global_heap();
    let source = fill_pml4(0xFEED_0000_0000_0000);
    // SAFETY: see fill_pml4.
    let captured = unsafe { read_kernel_half_entries(source) };

    let target = alloc_page_table().unwrap();
    // Zeroed so any leak through from `alloc_page_table` is a clear bug.
    // SAFETY: just allocated, page-aligned, HHDM-reachable.
    unsafe {
        let pml4 = (target.as_u64() + heap::hhdm_offset()) as *mut u8;
        core::ptr::write_bytes(pml4, 0, PAGE_SIZE);
        write_kernel_half_entries(target, &captured);
    }

    let after = read_all_entries(target);
    #[allow(clippy::needless_range_loop, reason = "the index names the entry in each assertion's message")]
    for i in 0..KERNEL_PML4_BASE {
        assert_eq!(after[i], 0, "user half not zero at entry {i}");
    }
    for i in 0..KERNEL_PML4_COUNT {
        let expected = 0xFEED_0000_0000_0000u64.wrapping_add((KERNEL_PML4_BASE + i) as u64);
        assert_eq!(after[KERNEL_PML4_BASE + i], expected, "round-trip mismatch at kernel entry {i}");
    }
}

// ---- what a leaf's bits mean ----

/// The console's own mapping on the laptop: a 2 MiB page at `0x80000000` selecting
/// attribute 5, write-combining.
const HUGE_WC: u64 = 0x8000_10EB;

#[test]
fn a_huge_leafs_frame_drops_the_attribute_bit_that_a_small_one_keeps() {
    // Bit 12 set: the frame on a 4 KiB entry, the `PAT` bit on a huge one.
    assert_eq!(frame_base(HUGE_WC, true), 0x8000_0000, "a huge page starts where it says");
    assert_eq!(frame_base(HUGE_WC, false), 0x8000_1000, "the same bit is frame on a small one");
    // And a huge entry without the bit is unchanged by the masking.
    assert_eq!(frame_base(0x8000_00EB, true), 0x8000_0000);
}

/// The one huge mapping this kernel reads selects entry 5 whichever bit is read, because it
/// has both set; these are the entries that differ.
#[test]
fn a_huge_leafs_attribute_comes_from_bit_twelve_and_a_small_ones_from_bit_seven() {
    assert_eq!(attribute_of(HUGE_WC, true), 5, "PAT | PWT: write-combining in Limine's table");
    // Bit 7 is "huge" here, so reading it would report 5 for an entry that selects 1.
    assert_eq!(attribute_of(0x8000_008B, true), 1, "PWT alone: entry 1");
    assert_eq!(attribute_of(0x8000_008B, false), 5, "the same bits in a 4 KiB entry");
    assert_eq!(attribute_of(0x8000_0083, true), 0, "a huge page with no attribute bits");
    assert_eq!(attribute_of(0x8000_0003, false), 0);
    assert_eq!(attribute_of(PTE_PWT, false), 1);
    assert_eq!(attribute_of(PTE_PCD, false), 2);
    assert_eq!(attribute_of(PTE_PAT, false), 4);
    assert_eq!(attribute_of(PTE_PCD | PTE_PWT, false), 3);
}

/// The mapping `kvmap` makes for device registers selects entry 2 (`UC-` in both tables seen).
#[test]
fn a_no_cache_mapping_selects_the_entry_pcd_alone_names() {
    let bits = flags_to_pte_bits(PageFlags::NO_CACHE);
    assert_eq!(bits, PTE_PCD);
    assert_eq!(attribute_of(bits, false), 2);
}

/// **A walk ends at a huge leaf, at either level, and a 4 KiB map inside one is refused.**
#[test]
fn a_walk_ends_at_a_huge_page_and_a_map_inside_one_is_refused() {
    init_global_heap();
    let (root, pdpt, pd) = (alloc_page_table().unwrap(), alloc_page_table().unwrap(), alloc_page_table().unwrap());
    // SAFETY: fresh tables only this test touches, linked into one tree here.
    let (l4, l3, l2) = unsafe { (TableRef::root(root), TableRef::at(pdpt, 3), TableRef::at(pd, 2)) };
    l4.set(0, Pte::new_table(pdpt));
    l3.set(1, Pte(0x4000_0000 | PTE_HUGE | PTE_PRESENT));
    l3.set(2, Pte::new_table(pd));
    l2.set(3, Pte(HUGE_WC));
    let (gib, mib) = (VirtAddr::new((1 << 30) | 0x1234_5678), VirtAddr::new((2 << 30) | (3 << 21) | 0x1_2345));
    // SAFETY: `root` is the live tree built above.
    unsafe {
        assert_eq!(X86Paging::translate(root, gib), Some(PhysAddr::new(0x5234_5678)));
        assert_eq!(X86Paging::translate(root, mib), Some(PhysAddr::new(0x8001_2345)));
        assert_eq!((attribute_index(root, gib), attribute_index(root, mib)), (Some(0), Some(5)));
        let inside = VirtAddr::new((1 << 30) | 0x1000);
        let target = PhysAddr::new(0x10_0000);
        assert_eq!(X86Paging::map_page(root, inside, target, PageFlags::WRITABLE), Err(MapError::AlreadyMapped));
    }
    // SAFETY: the tables this test made, which nothing walks from here.
    drop(unsafe { [pd, pdpt, root].map(|t| Frame::from_pte(t)) });
}

/// **A walk beside a mapper in one tree reads each slot atomically**: a race Miri reports.
#[test]
fn a_walk_beside_a_mapper_in_one_tree_reads_what_is_linked() {
    init_global_heap();
    let root = alloc_page_table().unwrap();
    let frames: std::vec::Vec<PhysAddr> = (0..4).map(|_| alloc_page_table().unwrap()).collect();
    let virt = |i: usize| VirtAddr::new(0xFFFF_C000_0000_0000 + (i as u64) * 0x20_0000);
    let mapper = std::thread::spawn({
        let frames = frames.clone();
        move || {
            for (i, &f) in frames.iter().enumerate() {
                // SAFETY: `root` is a live table this test owns, and each `virt(i)` is fresh.
                unsafe { X86Paging::map_page(root, virt(i), f, PageFlags::WRITABLE) }.unwrap();
            }
        }
    });
    for _ in 0..16 {
        for (i, &f) in frames.iter().enumerate() {
            // SAFETY: as above; a walk reads the tree as the mapper builds it.
            let at = unsafe { X86Paging::translate(root, virt(i)) };
            assert!(at.is_none() || at == Some(f));
        }
    }
    mapper.join().unwrap();
    for (i, &f) in frames.iter().enumerate() {
        // SAFETY: as above.
        assert_eq!(unsafe { X86Paging::translate(root, virt(i)) }, Some(f));
    }
}
