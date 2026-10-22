use super::*;

#[test]
fn the_mapping_window_stops_below_the_guard_gap() {
    const { assert!(USER_STACK_GUARD_SIZE > 0, "a zero-width gap guards nothing") };
    assert_eq!(USER_STACK_GUARD_BOTTOM + USER_STACK_GUARD_SIZE, USER_STACK_BOTTOM);
    assert_eq!(MMAP_MAX, USER_STACK_GUARD_BOTTOM);
    const { assert!(MMAP_MAX + USER_STACK_GUARD_SIZE <= USER_STACK_BOTTOM) };
    const { assert!(MMAP_BASE < MMAP_MAX, "the window must not be empty") };
}

#[test]
fn in_stack_guard_catches_overlap_not_adjacency() {
    let page = PAGE_SIZE as u64;
    // Inside, straddling either edge, and spanning the gap with neither endpoint in it (what an
    // endpoint test misses); then ending at its first byte or starting at the stack's is clear.
    assert!(AddressSpace::in_stack_guard(USER_STACK_GUARD_BOTTOM + page, USER_STACK_GUARD_BOTTOM + 2 * page));
    assert!(AddressSpace::in_stack_guard(USER_STACK_GUARD_BOTTOM - page, USER_STACK_BOTTOM));
    assert!(AddressSpace::in_stack_guard(USER_STACK_BOTTOM - page, USER_STACK_BOTTOM + page));
    assert!(AddressSpace::in_stack_guard(USER_STACK_GUARD_BOTTOM - page, USER_STACK_BOTTOM + page));

    assert!(!AddressSpace::in_stack_guard(USER_STACK_GUARD_BOTTOM - page, USER_STACK_GUARD_BOTTOM));
    assert!(!AddressSpace::in_stack_guard(USER_STACK_BOTTOM, USER_STACK_BOTTOM + page));
    assert!(!AddressSpace::in_stack_guard(MMAP_BASE, MMAP_BASE + page));
}

#[test]
fn the_stack_and_guard_are_the_sizes_the_rationale_claims() {
    assert_eq!(DEFAULT_USER_STACK_SIZE, 8 * 1024 * 1024);
    assert_eq!(USER_STACK_GUARD_SIZE, 256 * (PAGE_SIZE as u64));
    assert_eq!(DEFAULT_USER_STACK_SIZE % PAGE_SIZE as u64, 0);
    assert_eq!(USER_STACK_GUARD_SIZE % PAGE_SIZE as u64, 0);
    assert_eq!(USER_STACK_GUARD_BOTTOM % PAGE_SIZE as u64, 0);
}
use crate::arch::Paging;
use crate::arch::paging::ArchPaging;
use crate::mm::test_support::init_global_heap;
use crate::mm::vmm::VAddrRange;

const PAGE: u64 = PAGE_SIZE as u64;

fn va(v: u64) -> VirtAddr {
    VirtAddr::new(v)
}

fn range(start: u64, end: u64) -> VAddrRange {
    VAddrRange::new(va(start), va(end)).expect("test range must be valid")
}

fn anon_box(r: VAddrRange, prot: Protection) -> KBox<Vma> {
    KBox::try_new(Vma::new(r, prot, MappingKind::Anonymous)).expect("test heap exhausted")
}

#[test]
fn new_yields_empty_address_space_with_real_root() {
    init_global_heap();
    let asp = AddressSpace::new().expect("new must succeed");
    assert!(asp.is_empty());
    assert_eq!(asp.len(), 0);
    let root = asp.root();
    assert!(root.is_page_aligned());
    assert_ne!(root.as_u64(), 0);
}

#[test]
fn map_single_page_installs_pte_translate_finds_it() {
    init_global_heap();
    let asp = AddressSpace::new().expect("new must succeed");
    let r = range(PAGE * 4, PAGE * 5);
    asp.map_vma(anon_box(r, Protection::WRITE | Protection::USER)).expect("map must succeed");

    // SAFETY: translate is read-only against the live tables we
    // just populated. The root is owned by `asp`.
    let phys = unsafe { Paging::translate(asp.root(), va(PAGE * 4)) };
    assert!(phys.is_some());
    // SAFETY: `asp`'s root is the live page table this test built.
    let beyond = unsafe { Paging::translate(asp.root(), va(PAGE * 5)) };
    assert!(beyond.is_none());
    assert_eq!(asp.len(), 1);
}

#[test]
fn map_multi_page_installs_every_pte() {
    init_global_heap();
    let asp = AddressSpace::new().expect("new must succeed");
    let r = range(PAGE * 8, PAGE * 16);
    asp.map_vma(anon_box(r, Protection::WRITE | Protection::USER)).expect("map must succeed");

    for i in 8..16 {
        // SAFETY: `asp`'s root is the live page table this test built.
        let p = unsafe { Paging::translate(asp.root(), va(PAGE * i)) };
        assert!(p.is_some(), "page {i} not mapped");
    }
    // SAFETY: `asp`'s root is the live page table this test built.
    let before = unsafe { Paging::translate(asp.root(), va(PAGE * 7)) };
    // SAFETY: `asp`'s root is the live page table this test built.
    let after = unsafe { Paging::translate(asp.root(), va(PAGE * 16)) };
    assert!(before.is_none());
    assert!(after.is_none());
}

#[test]
fn map_then_unmap_removes_the_ptes() {
    init_global_heap();
    let asp = AddressSpace::new().expect("new must succeed");
    let r = range(PAGE * 4, PAGE * 8);
    asp.map_vma(anon_box(r, Protection::WRITE | Protection::USER)).expect("map must succeed");

    let removed = asp.unmap_covering(va(PAGE * 5)).expect("unmap must find covering vma");
    assert_eq!(removed.range, r);
    assert!(asp.is_empty());

    for i in 4..8 {
        // SAFETY: `asp`'s root is the live page table this test built.
        let p = unsafe { Paging::translate(asp.root(), va(PAGE * i)) };
        assert!(p.is_none(), "page {i} still mapped after unmap");
    }
}

#[test]
fn map_rejects_overlap_with_existing_vma() {
    init_global_heap();
    let asp = AddressSpace::new().expect("new must succeed");
    asp.map_vma(anon_box(range(PAGE * 4, PAGE * 8), Protection::WRITE | Protection::USER))
        .expect("first map must succeed");

    let err = asp
        .map_vma(anon_box(range(PAGE * 6, PAGE * 10), Protection::WRITE | Protection::USER))
        .expect_err("overlapping map must fail");
    assert_eq!(err.1, MapError::Overlap);
    assert_eq!(err.0.range, range(PAGE * 6, PAGE * 10));

    // SAFETY: `asp`'s root is the live page table this test built.
    assert!(unsafe { Paging::translate(asp.root(), va(PAGE * 4)) }.is_some());
    // SAFETY: `asp`'s root is the live page table this test built.
    assert!(unsafe { Paging::translate(asp.root(), va(PAGE * 9)) }.is_none());
    assert_eq!(asp.len(), 1);
}

#[test]
fn map_rejects_kernel_half_range() {
    init_global_heap();
    let asp = AddressSpace::new().expect("new must succeed");
    let r = VAddrRange::new(va(USER_VIRT_END - PAGE), va(USER_VIRT_END + PAGE)).unwrap();
    let err = asp
        .map_vma(anon_box(r, Protection::WRITE | Protection::USER))
        .expect_err("kernel-half range must be rejected");
    assert!(matches!(err.1, MapError::NotUserHalf | MapError::NotCanonical));
}

#[test]
fn unmap_on_address_with_no_vma_returns_none() {
    init_global_heap();
    let asp = AddressSpace::new().expect("new must succeed");
    assert!(asp.unmap_covering(va(PAGE * 100)).is_none());
    asp.map_vma(anon_box(range(PAGE * 4, PAGE * 8), Protection::WRITE | Protection::USER)).unwrap();
    assert!(asp.unmap_covering(va(PAGE * 20)).is_none());
    assert_eq!(asp.len(), 1);
}

#[test]
fn drop_tears_down_populated_address_space() {
    // Drop must free the leaf frames and the PML4: eight iterations of 16 pages would
    // otherwise exhaust the 16 MiB test heap.
    init_global_heap();
    for _ in 0..8 {
        let asp = AddressSpace::new().expect("new must succeed");
        for i in 0..4u64 {
            let start = (i * 8) * PAGE;
            asp.map_vma(anon_box(range(start, start + PAGE * 4), Protection::WRITE | Protection::USER))
                .expect("map must succeed");
        }
    }
}

use crate::object::header::test_probe;

fn into_obj(m: KBox<MemoryObject>) -> ObjectRef {
    // SAFETY: `into_raw` yields the single creation reference; adopt it.
    unsafe { ObjectRef::from_raw(KBox::into_raw(m).as_ptr() as *mut (), KObjectType::MemoryObject) }
}

fn uprot() -> Protection {
    Protection::WRITE | Protection::USER
}

#[test]
fn map_object_points_ptes_at_object_frames() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let m = MemoryObject::try_new(PAGE as usize * 3).unwrap();
    let frames = [m.frames()[0], m.frames()[1], m.frames()[2]];
    let obj = into_obj(m);

    asp.map_object(range(PAGE * 4, PAGE * 7), uprot(), obj).expect("map_object must succeed");

    for i in 0..3u64 {
        // SAFETY: `asp`'s root is the live page table this test built.
        let phys = unsafe { Paging::translate(asp.root(), va(PAGE * (4 + i))) }.expect("page must be mapped");
        assert_eq!(phys, frames[i as usize], "page {i} not pointing at object frame");
    }
    assert_eq!(asp.len(), 1);
}

#[test]
fn map_object_twice_aliases_same_frame() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let m = MemoryObject::try_new(PAGE as usize).unwrap();
    let frame0 = m.frames()[0];
    let obj = into_obj(m);
    let obj2 = obj.clone(); // second mapping needs its own reference

    asp.map_object(range(PAGE * 4, PAGE * 5), uprot(), obj).unwrap();
    asp.map_object(range(PAGE * 8, PAGE * 9), uprot(), obj2).unwrap();

    // SAFETY: `asp`'s root is the live page table this test built.
    let p1 = unsafe { Paging::translate(asp.root(), va(PAGE * 4)) }.unwrap();
    // SAFETY: `asp`'s root is the live page table this test built.
    let p2 = unsafe { Paging::translate(asp.root(), va(PAGE * 8)) }.unwrap();
    assert_eq!(p1, frame0);
    assert_eq!(p2, frame0);
    assert_eq!(p1, p2, "two mappings of one object must alias the same frame");
}

#[test]
fn unmap_object_mapping_does_not_free_the_objects_frames() {
    init_global_heap();
    test_probe::reset();
    let asp = AddressSpace::new().unwrap();
    let m = MemoryObject::try_new(PAGE as usize).unwrap();
    let obj = into_obj(m);
    let keep = obj.clone();

    asp.map_object(range(PAGE * 4, PAGE * 5), uprot(), obj).unwrap();
    let removed = asp.unmap_covering(va(PAGE * 4)).expect("unmap must find the covering vma");
    // SAFETY: `asp`'s root is the live page table this test built.
    assert!(unsafe { Paging::translate(asp.root(), va(PAGE * 4)) }.is_none());
    drop(removed);
    assert_eq!(test_probe::memory_object_destroys(), 0, "unmap freed shared frames");
    drop(keep);
    assert_eq!(test_probe::memory_object_destroys(), 1);
}

#[test]
fn find_free_range_picks_a_gap_in_the_mmap_window() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let r = asp.find_free_range(PAGE).expect("empty AS has room");
    assert_eq!(r.start().as_u64(), MMAP_BASE);
    assert_eq!(r.len(), PAGE);
}

use crate::object::{FileObject, Producer, Reserve};

fn into_file(f: KBox<FileObject>) -> ObjectRef {
    // SAFETY: `into_raw` yields the single creation reference; adopt it.
    unsafe { ObjectRef::from_raw(KBox::into_raw(f).as_ptr() as *mut (), KObjectType::FileObject) }
}

/// **Dropping an address space must release its file-backed VMAs without a lock-order
/// violation** (`Drop` once held its own `KernelObject` lock across the release). This test
/// does **not** catch that: the rank tracker is inert under `cfg(test)`, so no host test can
/// catch a lock-order violation; the reproduction is `cargo xtask check-terminal`. It pins
/// that the last holder of a file mapping releases the file when it drops.
#[test]
fn dropping_an_address_space_releases_a_file_mapping_it_last_holds() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    test_probe::reset();
    let obj = into_file(FileObject::try_new(3 * PAGE as usize, Producer::Stub { base: 0 }).unwrap());
    asp.map_file(range(PAGE * 4, PAGE * 7), uprot(), obj).expect("map_file must succeed");
    assert_eq!(test_probe::file_object_destroys(), 0, "the mapping holds the file alive");
    drop(asp);
    assert_eq!(test_probe::file_object_destroys(), 1, "dropping the AS released the file");
}

#[test]
fn map_file_reserves_lazily_with_no_ptes() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let obj = into_file(FileObject::try_new(3 * PAGE as usize, Producer::Stub { base: 0 }).unwrap());
    asp.map_file(range(PAGE * 4, PAGE * 7), uprot(), obj).expect("map_file must succeed");
    assert_eq!(asp.len(), 1, "the VMA is reserved");
    for i in 0..3u64 {
        assert!(!is_mapped(&asp, va(PAGE * (4 + i))), "page {i} must be lazy");
    }
}

#[test]
fn fault_in_file_backed_signals_and_file_backing_resolves() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let obj = into_file(FileObject::try_new(4 * PAGE as usize, Producer::Stub { base: 0 }).unwrap());
    let obj_ptr = obj.as_ptr();
    asp.map_file(range(PAGE * 4, PAGE * 8), uprot(), obj).unwrap();

    assert_eq!(asp.fault_in(va(PAGE * 5), FaultAccess::Read), FaultIn::FileBacked);

    for (vma_page, want_index) in [(4u64, 0usize), (5, 1), (7, 3)] {
        let (got_obj, index) = asp.file_backing(va(PAGE * vma_page)).expect("covered by the file mapping");
        assert_eq!(got_obj.as_ptr(), obj_ptr, "same backing object");
        assert_eq!(index, want_index, "page index for vma page {vma_page}");
    }
    assert!(asp.file_backing(va(PAGE * 2)).is_none());
}

#[test]
fn map_file_page_installs_pte_for_a_ready_frame() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let f = FileObject::try_new(4 * PAGE as usize, Producer::Stub { base: 0 }).unwrap();
    let frame = match f.reserve(1) {
        Reserve::New(frame) => frame,
        other => panic!("expected New, got {other:?}"),
    };
    f.mark_ready(1);
    let obj = into_file(f);
    asp.map_file(range(PAGE * 4, PAGE * 8), uprot(), obj.clone()).unwrap();

    assert!(asp.map_file_page(va(PAGE * 5), &obj, frame));
    // SAFETY: `asp`'s root is the live page table this test built.
    let phys = unsafe { Paging::translate(asp.root(), va(PAGE * 5)) }.expect("page must now be mapped");
    assert_eq!(phys, frame, "PTE points at the cache frame");
    assert!(asp.map_file_page(va(PAGE * 5), &obj, frame));
}

#[test]
fn map_file_page_rejects_wrong_object_or_unmapped() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let f = FileObject::try_new(PAGE as usize, Producer::Stub { base: 0 }).unwrap();
    let frame = match f.reserve(0) {
        Reserve::New(frame) => frame,
        other => panic!("{other:?}"),
    };
    f.mark_ready(0);
    let obj = into_file(f);
    asp.map_file(range(PAGE * 4, PAGE * 5), uprot(), obj.clone()).unwrap();

    let other = into_file(FileObject::try_new(PAGE as usize, Producer::Stub { base: 0 }).unwrap());
    assert!(!asp.map_file_page(va(PAGE * 4), &other, frame));
    assert!(!asp.map_file_page(va(PAGE * 99), &obj, frame));
    assert!(asp.map_file_page(va(PAGE * 4), &obj, frame));
}

#[test]
fn unmap_file_mapping_keeps_the_objects_frames() {
    init_global_heap();
    test_probe::reset();
    let asp = AddressSpace::new().unwrap();
    let f = FileObject::try_new(PAGE as usize, Producer::Stub { base: 0 }).unwrap();
    let frame = match f.reserve(0) {
        Reserve::New(frame) => frame,
        other => panic!("{other:?}"),
    };
    f.mark_ready(0);
    let obj = into_file(f);
    let keep = obj.clone();
    asp.map_file(range(PAGE * 4, PAGE * 5), uprot(), obj).unwrap();
    asp.map_file_page(va(PAGE * 4), &keep, frame);
    assert!(is_mapped(&asp, va(PAGE * 4)));

    let removed = asp.unmap_covering(va(PAGE * 4)).expect("covering vma");
    assert!(!is_mapped(&asp, va(PAGE * 4)), "PTE removed");
    drop(removed);
    assert_eq!(test_probe::file_object_destroys(), 0, "object still referenced");
    drop(keep);
    assert_eq!(test_probe::file_object_destroys(), 1);
}

/// A writable mapping is counted for as long as its VMA exists, however it goes; a refused
/// map counts nothing and a read-only mapping is never counted.
#[test]
fn a_writable_file_mapping_is_counted_while_its_vma_exists() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let obj = into_file(FileObject::try_new(PAGE as usize, Producer::Stub { base: 0 }).unwrap());
    let fo = || {
        // SAFETY: `obj` pins a live `FileObject` for the whole test.
        unsafe { &*(obj.as_ptr() as *const FileObject) }
    };
    asp.map_file(range(PAGE * 4, PAGE * 5), uprot(), obj.clone()).unwrap();
    assert_eq!(fo().writable_maps(), 1);
    let (back, _) = asp.map_file(range(PAGE * 4, PAGE * 5), uprot(), obj.clone()).unwrap_err();
    drop(back);
    assert_eq!(fo().writable_maps(), 1, "a refused map leaves no count behind");
    asp.map_file(range(PAGE * 8, PAGE * 9), Protection::USER, obj.clone()).unwrap();
    assert_eq!(fo().writable_maps(), 1);
    drop(asp.unmap_covering(va(PAGE * 4)).unwrap());
    assert_eq!(fo().writable_maps(), 0);
    let asp2 = AddressSpace::new().unwrap();
    asp2.map_file(range(PAGE * 4, PAGE * 5), uprot(), obj.clone()).unwrap();
    assert_eq!(fo().writable_maps(), 1);
    drop(asp2);
    assert_eq!(fo().writable_maps(), 0);
    drop(asp);
}

fn is_mapped(asp: &AddressSpace, v: VirtAddr) -> bool {
    // SAFETY: read-only walk of `asp`'s own page tables.
    unsafe { Paging::translate(asp.root(), v) }.is_some()
}

#[test]
fn map_vma_lazy_installs_no_ptes() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let r = range(PAGE * 4, PAGE * 8);
    asp.map_vma_lazy(anon_box(r, Protection::WRITE | Protection::USER)).expect("lazy reserve must succeed");
    assert_eq!(asp.len(), 1);
    for i in 4..8 {
        assert!(!is_mapped(&asp, va(PAGE * i)), "page {i} unexpectedly backed");
    }
}

#[test]
fn fault_in_backs_one_anonymous_page_on_demand() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let r = range(PAGE * 4, PAGE * 8);
    asp.map_vma_lazy(anon_box(r, Protection::WRITE | Protection::USER)).unwrap();

    let outcome = asp.fault_in(va(PAGE * 5 + 0x40), FaultAccess::Write);
    assert_eq!(outcome, FaultIn::Mapped);
    assert!(is_mapped(&asp, va(PAGE * 5)), "faulted page not backed");
    assert!(!is_mapped(&asp, va(PAGE * 4)));
    assert!(!is_mapped(&asp, va(PAGE * 6)));
}

#[test]
fn fault_in_with_no_covering_vma_is_no_vma() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    assert_eq!(asp.fault_in(va(PAGE * 9), FaultAccess::Read), FaultIn::NoVma);
}

#[test]
fn fault_in_write_to_readonly_is_protection() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    asp.map_vma_lazy(anon_box(range(PAGE * 4, PAGE * 5), Protection::USER)).unwrap();
    assert_eq!(asp.fault_in(va(PAGE * 4), FaultAccess::Write), FaultIn::Protection);
    assert!(!is_mapped(&asp, va(PAGE * 4)));
    assert_eq!(asp.fault_in(va(PAGE * 4), FaultAccess::Read), FaultIn::Mapped);
    assert!(is_mapped(&asp, va(PAGE * 4)));
}

#[test]
fn fault_in_exec_from_nonexec_is_protection() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    asp.map_vma_lazy(anon_box(range(PAGE * 4, PAGE * 5), Protection::WRITE | Protection::USER)).unwrap();
    assert_eq!(asp.fault_in(va(PAGE * 4), FaultAccess::Execute), FaultIn::Protection);
    assert!(!is_mapped(&asp, va(PAGE * 4)));
}

#[test]
fn dropping_a_partially_faulted_lazy_as_frees_only_backed_frames() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let r = range(PAGE * 4, PAGE * 8);
    asp.map_vma_lazy(anon_box(r, Protection::WRITE | Protection::USER)).unwrap();
    assert_eq!(asp.fault_in(va(PAGE * 4), FaultAccess::Write), FaultIn::Mapped);
    assert_eq!(asp.fault_in(va(PAGE * 6), FaultAccess::Write), FaultIn::Mapped);

    let removed = asp.unmap_covering(va(PAGE * 5)).expect("covering vma");
    assert_eq!(removed.range, r);
    assert!(asp.is_empty());
    for i in 4..8 {
        assert!(!is_mapped(&asp, va(PAGE * i)), "page {i} still mapped");
    }
}

#[test]
fn fault_in_is_idempotent_friendly_across_a_lazy_drop() {
    init_global_heap();
    for _ in 0..8 {
        let asp = AddressSpace::new().unwrap();
        asp.map_vma_lazy(anon_box(range(PAGE * 4, PAGE * 12), Protection::WRITE | Protection::USER)).unwrap();
        for i in 4..12u64 {
            assert_eq!(asp.fault_in(va(PAGE * i), FaultAccess::Write), FaultIn::Mapped);
        }
    }
}

/// The write-combining branch of `page_flags_for` once went untested while every gate stayed
/// green.
#[test]
fn a_write_combining_mapping_asks_for_it_and_an_ordinary_one_asks_for_nothing() {
    let prot = Protection::WRITE | Protection::USER;
    let normal = page_flags_for(prot, Caching::Normal);
    let wc = page_flags_for(prot, Caching::WriteCombining);
    assert!(!normal.contains(PageFlags::WRITE_COMBINING), "ordinary memory asks for nothing");
    assert!(wc.contains(PageFlags::WRITE_COMBINING));
    assert!(wc.contains(PageFlags::WRITABLE) && wc.contains(PageFlags::USER));
    assert_eq!(
        normal.contains(PageFlags::NO_EXECUTE),
        wc.contains(PageFlags::NO_EXECUTE),
        "caching does not decide executability"
    );
}

/// End to end: an aperture object saying write-combining is mapped with the matching entry,
/// an ordinary one is not.
#[test]
fn mapping_an_aperture_object_selects_the_write_combining_entry() {
    init_global_heap();
    const APERTURE: u64 = 0xF000_0000;
    let asp = AddressSpace::new().unwrap();

    // SAFETY: a physical range no test reads or frees; only its page-table entries are read.
    let device = unsafe {
        MemoryObject::try_new_borrowed(PhysAddr::new(APERTURE), PAGE as usize, Caching::WriteCombining).unwrap()
    };
    asp.map_object(range(PAGE * 4, PAGE * 5), uprot(), into_obj(device)).expect("map_object must succeed");
    let ordinary = MemoryObject::try_new(PAGE as usize).unwrap();
    asp.map_object(range(PAGE * 8, PAGE * 9), uprot(), into_obj(ordinary)).expect("map_object must succeed");

    // SAFETY: `asp`'s root is the live page table this test built.
    let index = |v: u64| unsafe { Paging::attribute_index(asp.root(), va(v)) };
    // Entry 5 is write-combining and entry 0 write-back in the table the kernel programs.
    assert_eq!(index(PAGE * 4), Some(5), "the aperture's mapping");
    assert_eq!(index(PAGE * 8), Some(0), "ordinary memory's");
    assert_eq!(index(PAGE * 16), None, "nothing is mapped there");
}
