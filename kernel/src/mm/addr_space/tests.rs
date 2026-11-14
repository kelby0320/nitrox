use super::*;
use crate::arch::abi::USER_VIRT_END;
use crate::mm::frame::fail_frames_after;
use crate::mm::{frame, heap};

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
    KBox::try_new(Vma::new(r, prot)).expect("test heap exhausted")
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
    let freed = heap::frames_freed_by_this_thread();
    drop(asp);
    assert_eq!(heap::frames_freed_by_this_thread() - freed, 1, "its root, once");
}

#[test]
fn map_single_page_installs_pte_translate_finds_it() {
    init_global_heap();
    let mut asp = AddressSpace::new().expect("new must succeed");
    let r = range(PAGE * 4, PAGE * 5);
    asp.map_vma(anon_box(r, Protection::WRITE | Protection::USER)).expect("map must succeed");

    let phys = asp.translate(va(PAGE * 4));
    assert!(phys.is_some());
    let beyond = asp.translate(va(PAGE * 5));
    assert!(beyond.is_none());
    assert_eq!(asp.len(), 1);
}

#[test]
fn map_multi_page_installs_every_pte() {
    init_global_heap();
    let mut asp = AddressSpace::new().expect("new must succeed");
    let r = range(PAGE * 8, PAGE * 16);
    asp.map_vma(anon_box(r, Protection::WRITE | Protection::USER)).expect("map must succeed");

    for i in 8..16 {
        let p = asp.translate(va(PAGE * i));
        assert!(p.is_some(), "page {i} not mapped");
    }
    let before = asp.translate(va(PAGE * 7));
    let after = asp.translate(va(PAGE * 16));
    assert!(before.is_none());
    assert!(after.is_none());
}

#[test]
fn map_then_unmap_removes_the_ptes() {
    init_global_heap();
    let mut asp = AddressSpace::new().expect("new must succeed");
    let r = range(PAGE * 4, PAGE * 8);
    asp.map_vma(anon_box(r, Protection::WRITE | Protection::USER)).expect("map must succeed");

    let removed = asp.unmap_at(va(PAGE * 5)).expect("unmap must find covering vma");
    assert_eq!(removed.vma().unwrap().range, r);
    assert!(asp.is_empty());

    for i in 4..8 {
        let p = asp.translate(va(PAGE * i));
        assert!(p.is_none(), "page {i} still mapped after unmap");
    }
}

#[test]
fn map_rejects_overlap_with_existing_vma() {
    init_global_heap();
    let mut asp = AddressSpace::new().expect("new must succeed");
    asp.map_vma(anon_box(range(PAGE * 4, PAGE * 8), Protection::WRITE | Protection::USER))
        .expect("first map must succeed");

    let err = asp
        .map_vma(anon_box(range(PAGE * 6, PAGE * 10), Protection::WRITE | Protection::USER))
        .expect_err("overlapping map must fail");
    assert_eq!(err.1, MapError::Overlap);
    assert_eq!(err.0.range, range(PAGE * 6, PAGE * 10));

    assert!(asp.translate(va(PAGE * 4)).is_some());
    assert!(asp.translate(va(PAGE * 9)).is_none());
    assert_eq!(asp.len(), 1);
}

#[test]
fn no_way_of_mapping_reaches_the_top_page_of_the_user_half() {
    init_global_heap();
    let top = USER_VIRT_END - PAGE;
    let below = range(top - PAGE, top);
    let last = range(top, USER_VIRT_END);
    let mut asp = AddressSpace::new().unwrap();
    assert!(matches!(asp.map_vma(anon_box(last, uprot())), Err((_, MapError::NotUserHalf))));
    assert!(matches!(asp.map_vma_lazy(anon_box(last, uprot())), Err((_, MapError::NotUserHalf))));
    let obj = into_obj(MemoryObject::try_new(PAGE as usize).unwrap());
    assert!(matches!(asp.map_object(last, uprot(), obj.clone(), 0), Err((_, MapError::NotUserHalf))));
    let file = into_file(FileObject::try_new(PAGE as usize, Producer::Stub { base: 0 }).unwrap());
    assert!(matches!(asp.map_file(last, uprot(), file, 0), Err((_, MapError::NotUserHalf))));
    assert!(asp.map_object(below, uprot(), obj, 0).is_ok());
}

#[test]
fn map_rejects_kernel_half_range() {
    init_global_heap();
    let mut asp = AddressSpace::new().expect("new must succeed");
    let r = VAddrRange::new(va(USER_VIRT_END - PAGE), va(USER_VIRT_END + PAGE)).unwrap();
    let err =
        asp.map_vma(anon_box(r, Protection::WRITE | Protection::USER)).expect_err("kernel-half range must be rejected");
    assert!(matches!(err.1, MapError::NotUserHalf | MapError::NotCanonical));
}

#[test]
fn unmap_on_address_with_no_vma_returns_none() {
    init_global_heap();
    let mut asp = AddressSpace::new().expect("new must succeed");
    assert!(asp.unmap_at(va(PAGE * 100)).is_none());
    asp.map_vma(anon_box(range(PAGE * 4, PAGE * 8), Protection::WRITE | Protection::USER)).unwrap();
    assert!(asp.unmap_at(va(PAGE * 20)).is_none());
    assert_eq!(asp.len(), 1);
}

#[test]
fn drop_tears_down_populated_address_space() {
    // Drop must free the leaf frames and the PML4: eight iterations of 16 pages would
    // otherwise exhaust the 16 MiB test heap.
    init_global_heap();
    for _ in 0..8 {
        let mut asp = AddressSpace::new().expect("new must succeed");
        for i in 0..4u64 {
            let start = (i * 8) * PAGE;
            asp.map_vma(anon_box(range(start, start + PAGE * 4), Protection::WRITE | Protection::USER))
                .expect("map must succeed");
        }
    }
}

use crate::object::header::{Ref, test_probe};

fn into_obj(m: KBox<MemoryObject>) -> Ref<MemoryObject> {
    Ref::new(m)
}

fn uprot() -> Protection {
    Protection::WRITE | Protection::USER
}

#[test]
fn unmapping_an_object_needs_no_room_for_frames_and_an_anonymous_mapping_does() {
    init_global_heap();
    let mut asp = AddressSpace::new().unwrap();
    let obj = into_obj(MemoryObject::try_new(PAGE as usize * 3).unwrap());
    asp.map_object(range(PAGE * 4, PAGE * 7), uprot(), obj, 0).unwrap();
    asp.map_vma(anon_box(range(PAGE * 16, PAGE * 18), uprot())).unwrap();
    assert!(matches!(asp.unmap_covering(va(PAGE * 5), KVec::new()), Ok(Some(_))));
    assert_eq!(asp.unmap_covering(va(PAGE * 16), KVec::new()).err().map(|(_, pages)| pages), Some(2));
}

#[test]
fn map_object_points_ptes_at_object_frames() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let m = MemoryObject::try_new(PAGE as usize * 3).unwrap();
    let frames = [m.frames()[0].phys(), m.frames()[1].phys(), m.frames()[2].phys()];
    let obj = into_obj(m);

    asp.map_object(range(PAGE * 4, PAGE * 7), uprot(), obj, 0).expect("map_object must succeed");

    for i in 0..3u64 {
        let phys = asp.translate(va(PAGE * (4 + i))).expect("page must be mapped");
        assert_eq!(phys, frames[i as usize], "page {i} not pointing at object frame");
    }
    assert_eq!(asp.len(), 1);
}

#[test]
fn map_object_twice_aliases_same_frame() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let m = MemoryObject::try_new(PAGE as usize).unwrap();
    let frame0 = m.frames()[0].phys();
    let obj = into_obj(m);
    let obj2 = obj.clone(); // second mapping needs its own reference

    asp.map_object(range(PAGE * 4, PAGE * 5), uprot(), obj, 0).unwrap();
    asp.map_object(range(PAGE * 8, PAGE * 9), uprot(), obj2, 0).unwrap();

    let p1 = asp.translate(va(PAGE * 4)).unwrap();
    let p2 = asp.translate(va(PAGE * 8)).unwrap();
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

    asp.map_object(range(PAGE * 4, PAGE * 5), uprot(), obj, 0).unwrap();
    let removed = asp.unmap_at(va(PAGE * 4)).expect("unmap must find the covering vma");
    assert!(asp.translate(va(PAGE * 4)).is_none());
    drop(removed);
    assert_eq!(test_probe::memory_object_destroys(), 0, "unmap freed shared frames");
    drop(keep);
    assert_eq!(test_probe::memory_object_destroys(), 1);
}

#[test]
fn an_anywhere_mapping_starts_at_the_window_base() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let r =
        asp.map_object(Placement::Anywhere(PAGE), uprot(), into_obj(MemoryObject::try_new(PAGE as usize).unwrap()), 0);
    let r = r.expect("an empty space has room");
    assert_eq!((r.start().as_u64(), r.len()), (MMAP_BASE, PAGE));
}

use crate::object::{FileObject, Producer, Reserve};

fn into_file(f: KBox<FileObject>) -> Ref<FileObject> {
    Ref::new(f)
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
    asp.map_file(range(PAGE * 4, PAGE * 7), uprot(), obj, 0).expect("map_file must succeed");
    assert_eq!(test_probe::file_object_destroys(), 0, "the mapping holds the file alive");
    drop(asp);
    assert_eq!(test_probe::file_object_destroys(), 1, "dropping the AS released the file");
}

#[test]
fn map_file_reserves_lazily_with_no_ptes() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let obj = into_file(FileObject::try_new(3 * PAGE as usize, Producer::Stub { base: 0 }).unwrap());
    asp.map_file(range(PAGE * 4, PAGE * 7), uprot(), obj, 0).expect("map_file must succeed");
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
    asp.map_file(range(PAGE * 4, PAGE * 8), uprot(), obj, 0).unwrap();

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
    asp.map_file(range(PAGE * 4, PAGE * 8), uprot(), obj.clone(), 0).unwrap();

    assert_eq!(asp.map_file_page(va(PAGE * 5), &obj, 1, frame, &obj.running()), FilePage::Mapped);
    let phys = asp.translate(va(PAGE * 5)).expect("page must now be mapped");
    assert_eq!(phys, frame, "PTE points at the cache frame");
    assert_eq!(asp.map_file_page(va(PAGE * 5), &obj, 1, frame, &obj.running()), FilePage::Mapped);
}

#[test]
fn a_faults_running_keeps_its_frame_from_being_let_go_before_its_pte_is_in() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let f = FileObject::try_new(4 * PAGE as usize, Producer::Stub { base: 0 }).unwrap();
    let Reserve::New(_) = f.reserve(1) else { panic!("a new page") };
    f.mark_ready(1);
    let obj = into_file(f);
    asp.map_file(range(PAGE * 4, PAGE * 8), uprot(), obj.clone(), 0).unwrap();
    let running = obj.running();
    let frame = obj.resident_page(1).unwrap();
    drop(asp.unmap_at(va(PAGE * 4)));
    assert_eq!(obj.let_window_go(0..4, obj.clean_mark().unwrap()), 0, "a fault between fetch and install");
    asp.map_file(range(PAGE * 4, PAGE * 8), uprot(), obj.clone(), 0).unwrap();
    assert_eq!(asp.map_file_page(va(PAGE * 5), &obj, 1, frame, &running), FilePage::Mapped);
    assert_eq!(obj.resident_page(1), Some(frame), "the PTE's frame still the file's");
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
    asp.map_file(range(PAGE * 4, PAGE * 5), uprot(), obj.clone(), 0).unwrap();

    let other = into_file(FileObject::try_new(PAGE as usize, Producer::Stub { base: 0 }).unwrap());
    assert_eq!(asp.map_file_page(va(PAGE * 4), &other, 0, frame, &other.running()), FilePage::Changed);
    assert_eq!(asp.map_file_page(va(PAGE * 99), &obj, 0, frame, &obj.running()), FilePage::Changed);
    assert!(!is_mapped(&asp, va(PAGE * 4)), "neither installed");
    assert_eq!(asp.map_file_page(va(PAGE * 4), &obj, 0, frame, &obj.running()), FilePage::Mapped);
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
    asp.map_file(range(PAGE * 4, PAGE * 5), uprot(), obj, 0).unwrap();
    asp.map_file_page(va(PAGE * 4), &keep, 0, frame, &keep.running());
    assert!(is_mapped(&asp, va(PAGE * 4)));

    let removed = asp.unmap_at(va(PAGE * 4)).expect("covering vma");
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
    let fo = || -> &FileObject { &obj };
    asp.map_file(range(PAGE * 4, PAGE * 5), uprot(), obj.clone(), 0).unwrap();
    assert_eq!(fo().writable_maps(), 1);
    let (back, _) = asp.map_file(range(PAGE * 4, PAGE * 5), uprot(), obj.clone(), 0).unwrap_err();
    drop(back);
    assert_eq!(fo().writable_maps(), 1, "a refused map leaves no count behind");
    asp.map_file(range(PAGE * 8, PAGE * 9), Protection::USER, obj.clone(), 0).unwrap();
    assert_eq!(fo().writable_maps(), 1);
    drop(asp.unmap_at(va(PAGE * 4)).unwrap());
    assert_eq!(fo().writable_maps(), 0);
    let asp2 = AddressSpace::new().unwrap();
    asp2.map_file(range(PAGE * 4, PAGE * 5), uprot(), obj.clone(), 0).unwrap();
    assert_eq!(fo().writable_maps(), 1);
    drop(asp2);
    assert_eq!(fo().writable_maps(), 0);
    drop(asp);
}

fn is_mapped(asp: &AddressSpace, v: VirtAddr) -> bool {
    asp.translate(v).is_some()
}

/// Two CPUs can fault on one fresh page: the second, taking the lock after the first mapped it,
/// finds the page there.
#[test]
fn a_fault_on_a_page_another_cpu_just_mapped_is_a_retry_and_keeps_no_frame() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    asp.map_vma_lazy(anon_box(range(PAGE * 4, PAGE * 6), uprot())).unwrap();
    assert_eq!(asp.fault_in(va(PAGE * 5), FaultAccess::Write), FaultIn::Mapped);
    let first = asp.translate(va(PAGE * 5));
    let freed = heap::frames_freed_by_this_thread();
    assert_eq!(asp.fault_in(va(PAGE * 5 + 8), FaultAccess::Write), FaultIn::Mapped);
    assert_eq!(heap::frames_freed_by_this_thread() - freed, 1, "the second fault's frame goes back");
    assert_eq!(asp.translate(va(PAGE * 5)), first);
}

#[test]
fn an_unmap_frees_an_anonymous_mappings_frames_and_never_an_objects() {
    init_global_heap();
    let mut asp = AddressSpace::new().unwrap();
    asp.map_vma(anon_box(range(PAGE * 16, PAGE * 18), uprot())).unwrap();
    let obj = into_obj(MemoryObject::try_new(PAGE as usize * 3).unwrap());
    let keep = obj.clone();
    asp.map_object(range(PAGE * 4, PAGE * 7), uprot(), obj, 0).unwrap();
    let freed = heap::frames_freed_by_this_thread();
    drop(asp.unmap_at(va(PAGE * 5)));
    assert_eq!(heap::frames_freed_by_this_thread(), freed, "an object's frames are the object's to free");
    drop(keep);
    let freed = heap::frames_freed_by_this_thread();
    drop(asp.unmap_at(va(PAGE * 16)));
    assert_eq!(heap::frames_freed_by_this_thread() - freed, 2);

    let mut held = AddressSpace::new().unwrap();
    held.map_vma(anon_box(range(PAGE * 16, PAGE * 19), uprot())).unwrap();
    let freed = heap::frames_freed_by_this_thread();
    drop(held);
    assert!(heap::frames_freed_by_this_thread() - freed >= 3, "a dropped address space frees its anonymous frames");
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

    let removed = asp.unmap_at(va(PAGE * 5)).expect("covering vma");
    assert_eq!(removed.vma().unwrap().range, r);
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
    asp.map_object(range(PAGE * 4, PAGE * 5), uprot(), into_obj(device), 0).expect("map_object must succeed");
    let ordinary = MemoryObject::try_new(PAGE as usize).unwrap();
    asp.map_object(range(PAGE * 8, PAGE * 9), uprot(), into_obj(ordinary), 0).expect("map_object must succeed");

    let index = |v: u64| asp.with_tables(|t| t.attribute_index(va(v)));
    // Entry 5 is write-combining and entry 0 write-back in the table the kernel programs.
    assert_eq!(index(PAGE * 4), Some(5), "the aperture's mapping");
    assert_eq!(index(PAGE * 8), Some(0), "ordinary memory's");
    assert_eq!(index(PAGE * 16), None, "nothing is mapped there");
}

#[test]
fn a_page_is_writable_executable_and_user_only_as_its_protection_says() {
    use crate::arch::paging::PageFlags;
    let f = |p| page_flags_for(p, Caching::Normal);
    let user = Protection::USER;
    for (prot, write, exec) in
        [(user, false, false), (user | Protection::WRITE, true, false), (user | Protection::EXEC, false, true)]
    {
        assert_eq!(f(prot).contains(PageFlags::WRITABLE), write, "{prot:?}");
        assert_eq!(!f(prot).contains(PageFlags::NO_EXECUTE), exec, "{prot:?}");
        assert!(f(prot).contains(PageFlags::USER));
    }
    assert!(!f(Protection::WRITE).contains(PageFlags::USER), "a kernel mapping");
}

/// **A map a frame short frees the pages it took**: a first page, its three tables, then two more.
#[test]
fn a_map_a_frame_short_at_any_take_frees_the_pages_it_took() {
    init_global_heap();
    for (short, pages) in [(0, 0), (1, 1), (2, 1), (3, 1), (4, 1), (5, 2)] {
        let mut asp = AddressSpace::new().unwrap();
        fail_frames_after(Some(short));
        let freed = heap::frames_freed_by_this_thread();
        let made = asp.map_vma(anon_box(range(PAGE * 16, PAGE * 19), uprot()));
        fail_frames_after(None);
        assert_eq!(made.err().map(|(_, e)| e), Some(MapError::OutOfMemory), "short at {short}");
        assert_eq!(heap::frames_freed_by_this_thread() - freed, pages, "short at {short}");
        assert!((16..19).all(|p| !is_mapped(&asp, va(PAGE * p))), "short at {short}: rolled back");
    }
}

/// **A fault a frame short frees the frame it took**: its page, then its three tables.
#[test]
fn a_fault_a_frame_short_at_any_take_frees_the_frame_it_took() {
    init_global_heap();
    for (short, pages) in [(0, 0), (1, 1), (2, 1), (3, 1)] {
        let asp = AddressSpace::new().unwrap();
        asp.map_vma_lazy(anon_box(range(PAGE * 16, PAGE * 18), uprot())).unwrap();
        fail_frames_after(Some(short));
        let freed = heap::frames_freed_by_this_thread();
        let fault = asp.fault_in(va(PAGE * 17), FaultAccess::Write);
        fail_frames_after(None);
        assert_eq!(fault, FaultIn::Oom, "short at {short}");
        assert_eq!(heap::frames_freed_by_this_thread() - freed, pages, "short at {short}");
        assert!(!is_mapped(&asp, va(PAGE * 17)), "short at {short}");
    }
}

/// **A failed object map is shot down before its object comes back**: the caller may drop the
/// object, and with it the frames another CPU may have cached through the pages mapped.
#[test]
fn a_failed_object_map_is_shot_down_before_its_object_comes_back() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let obj = into_obj(MemoryObject::try_new(PAGE as usize * 4).unwrap());
    // Two pages each side of 2 MiB: a fresh space's PDPT, PD and first PT, then a PT of its own.
    let start = 0x20_0000 - 2 * PAGE;
    let mut obj = obj;
    for (tables, mapped) in [(0, 0), (3, 2)] {
        crate::arch::fail_page_tables_after(Some(tables));
        let shots = crate::tlb::shootdowns_by_this_thread().0;
        let result = asp.map_object(range(start, start + 4 * PAGE), uprot(), obj, 0);
        crate::arch::fail_page_tables_after(None);
        let (back, e) = result.unwrap_err();
        assert_eq!(e, MapError::OutOfMemory);
        let shot = crate::tlb::shootdowns_by_this_thread().0 - shots;
        assert_eq!(shot, usize::from(mapped > 0), "{mapped} pages mapped first");
        assert!(!is_mapped(&asp, va(start)) && !is_mapped(&asp, va(start + PAGE)), "rolled back");
        obj = back;
    }
}

#[test]
fn an_unmap_is_shot_down_before_its_frames_or_its_object_are_freed() {
    init_global_heap();
    let mut asp = AddressSpace::new().unwrap();
    asp.map_vma(anon_box(range(PAGE * 16, PAGE * 18), uprot())).unwrap();
    asp.map_object(range(PAGE * 4, PAGE * 7), uprot(), into_obj(MemoryObject::try_new(PAGE as usize * 3).unwrap()), 0)
        .unwrap();
    for (at, frames) in [(PAGE * 16, 2), (PAGE * 4, 3)] {
        let unmapped = asp.unmap_at(va(at)).unwrap();
        let freed = heap::frames_freed_by_this_thread();
        drop(unmapped);
        assert_eq!(crate::tlb::shootdowns_by_this_thread().1, freed, "{at:#x}: none freed before it");
        assert_eq!(heap::frames_freed_by_this_thread() - freed, frames, "{at:#x}");
    }
}

/// **`copy_in` writes only anonymous memory present where it writes**: it refuses anything else,
/// having written what came before.
#[test]
fn copy_in_writes_only_anonymous_memory_that_is_present() {
    init_global_heap();
    let mut asp = AddressSpace::new().unwrap();
    asp.map_vma(anon_box(range(PAGE * 4, PAGE * 6), uprot())).unwrap();
    asp.map_vma_lazy(anon_box(range(PAGE * 6, PAGE * 7), uprot())).unwrap();
    let m = MemoryObject::try_new(PAGE as usize).unwrap();
    let object_frame = m.frames()[0].phys();
    asp.map_object(range(PAGE * 8, PAGE * 9), uprot(), into_obj(m), 0).unwrap();
    let f = FileObject::try_new(PAGE as usize, Producer::Stub { base: 0 }).unwrap();
    let Reserve::New(file_frame) = f.reserve(0) else { panic!("a fresh page") };
    f.mark_ready(0);
    let file = into_file(f);
    asp.map_file(range(PAGE * 10, PAGE * 11), uprot(), file.clone(), 0).unwrap();
    assert_eq!(asp.map_file_page(va(PAGE * 10), &file, 0, file_frame, &file.running()), FilePage::Mapped);

    assert!(asp.copy_in(va(PAGE * 5 - 2), &[1, 2, 3, 4]), "across two anonymous pages");
    assert!(!asp.copy_in(va(PAGE * 6 - 1), &[5, 6]), "into a page not yet faulted in");
    assert!(!asp.copy_in(va(PAGE * 8), &[9]), "an object's page");
    assert!(!asp.copy_in(va(PAGE * 10), &[9]), "a file's page");
    assert!(!asp.copy_in(va(PAGE * 12), &[9]), "no mapping");
    // SAFETY: frames this test's mappings and objects keep live; nothing else writes them now.
    let byte = |phys: PhysAddr| unsafe { *frame::hhdm::<u8>(phys) };
    let at = |a: u64| byte(asp.translate(va(a)).expect("mapped"));
    assert_eq!([PAGE * 5 - 2, PAGE * 5 - 1, PAGE * 5, PAGE * 5 + 1].map(at), [1, 2, 3, 4]);
    assert_eq!(at(PAGE * 6 - 1), 5, "what came before the page not faulted in");
    assert_eq!((byte(object_frame), byte(file_frame)), (0, 0), "neither the object's page nor the file's");
}

fn anywhere(asp: &AddressSpace, pages: u64) -> VAddrRange {
    let obj = into_obj(MemoryObject::try_new((pages * PAGE) as usize).unwrap());
    asp.map_object(Placement::Anywhere(pages * PAGE), uprot(), obj, 0).expect("room in the window")
}

/// **Two maps placed anywhere at once never meet**: neither is refused `Overlap`.
#[test]
fn two_maps_placed_anywhere_at_once_never_meet() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let placed = || (0..64).map(|_| anywhere(&asp, 1).start().as_u64()).collect::<std::vec::Vec<_>>();
    let mut all = std::thread::scope(|s| {
        let (a, b) = (s.spawn(placed), s.spawn(placed));
        [a.join().unwrap(), b.join().unwrap()].concat()
    });
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), 128, "every map its own range");
}

/// **An anywhere mapping keeps a page free below itself, and below the mapping above it.**
#[test]
fn an_anywhere_mapping_keeps_a_page_free_on_either_side() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let base = MMAP_BASE;
    assert_eq!(anywhere(&asp, 1).start().as_u64(), base);
    assert_eq!(anywhere(&asp, 1).start().as_u64(), base + 2 * PAGE, "a page above the first");
    let named = into_obj(MemoryObject::try_new(PAGE as usize).unwrap());
    asp.map_object(range(base + 5 * PAGE, base + 6 * PAGE), uprot(), named, 0).expect("a named range is not held off");
    assert_eq!(
        anywhere(&asp, 1).start().as_u64(),
        base + 7 * PAGE,
        "not the hole that leaves none below the named one"
    );
    let above = into_obj(MemoryObject::try_new(PAGE as usize).unwrap());
    asp.map_object(range(base + 8 * PAGE, base + 9 * PAGE), uprot(), above, 0).expect("the page above it, named");
    assert_eq!(anywhere(&asp, 1).start().as_u64(), base + 10 * PAGE, "a page free below it, too");
}

/// **A fill installed after its mapping moved installs nothing**: the access retries for page 1.
#[test]
fn a_fill_whose_mapping_moved_under_it_installs_nothing() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let f = FileObject::try_new(4 * PAGE as usize, Producer::Stub { base: 0 }).unwrap();
    let Reserve::New(frame) = f.reserve(2) else { panic!("a fresh page") };
    f.mark_ready(2);
    let f = into_file(f);
    asp.map_file(range(PAGE * 4, PAGE * 8), uprot(), f.clone(), 0).unwrap();
    let (_, index) = asp.file_backing(va(PAGE * 6)).unwrap();
    drop(asp.unmap_at(va(PAGE * 4)));
    asp.map_file(range(PAGE * 5, PAGE * 9), uprot(), f.clone(), 0).unwrap();
    assert_eq!(asp.map_file_page(va(PAGE * 6), &f, index, frame, &f.running()), FilePage::Changed);
    assert!(!is_mapped(&asp, va(PAGE * 6)), "page 2 is not where page 1 belongs");
    assert_eq!(asp.file_backing(va(PAGE * 6)).map(|(_, i)| i), Some(1), "what the retry faults in");
}

/// **A mapping from an offset begins at that page of its object.**
/// **A page no user may reach is never faulted in**: a kernel copy, unlike the CPU, would reach it.
#[test]
fn a_page_no_user_may_reach_is_not_faulted_in() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    asp.map_vma_lazy(anon_box(range(PAGE * 4, PAGE * 5), Protection::WRITE)).unwrap();
    assert_eq!(asp.fault_in(va(PAGE * 4), FaultAccess::Read), FaultIn::Protection);
    assert!(!is_mapped(&asp, va(PAGE * 4)));
}

#[test]
fn a_mapping_from_an_offset_begins_at_that_page() {
    init_global_heap();
    let asp = AddressSpace::new().unwrap();
    let m = MemoryObject::try_new(4 * PAGE as usize).unwrap();
    let third = m.frames()[2].phys();
    asp.map_object(range(PAGE * 4, PAGE * 6), uprot(), into_obj(m), 2).unwrap();
    assert_eq!(asp.translate(va(PAGE * 4)), Some(third));
    let f = into_file(FileObject::try_new(4 * PAGE as usize, Producer::Stub { base: 0 }).unwrap());
    asp.map_file(range(PAGE * 8, PAGE * 10), uprot(), f, 2).unwrap();
    assert_eq!(asp.file_backing(va(PAGE * 9)).map(|(_, i)| i), Some(3));
}
