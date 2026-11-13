use super::*;
use crate::mm::heap;
use crate::mm::test_support::init_global_heap;
use crate::object::ObjectRef;
use crate::object::header::test_probe;

fn fobj(size: usize) -> KBox<FileObject> {
    FileObject::try_new(size, Producer::Stub { base: 0 }).unwrap()
}

fn frame_byte(frame: PhysAddr, off: usize) -> u8 {
    // SAFETY: `frame` is a live, HHDM-reachable cache frame; read-only.
    unsafe { *((frame.as_u64() + heap::hhdm_offset()) as *const u8).add(off) }
}
fn write_frame_byte(frame: PhysAddr, off: usize, v: u8) {
    // SAFETY: `frame` is a live, HHDM-reachable, unaliased cache frame.
    unsafe { *((frame.as_u64() + heap::hhdm_offset()) as *mut u8).add(off) = v };
}

#[test]
fn size_and_npages_round_up() {
    init_global_heap();
    assert_eq!(fobj(0).npages(), 0);
    assert_eq!(fobj(1).npages(), 1);
    let f = fobj(2 * PAGE_SIZE + 1);
    assert_eq!(f.size(), 2 * PAGE_SIZE + 1);
    assert_eq!(f.npages(), 3);
    assert!(f.magic_ok());
}

#[test]
fn reserve_then_mark_ready_lifecycle() {
    init_global_heap();
    let f = fobj(4 * PAGE_SIZE);

    let frame = match f.reserve(2) {
        Reserve::New(frame) => frame,
        other => panic!("expected New, got {other:?}"),
    };
    assert_eq!(frame_byte(frame, 0), 0, "fresh frame must be zeroed");
    assert_eq!(frame_byte(frame, PAGE_SIZE - 1), 0);
    assert_eq!(f.resident_pages(), 1);
    assert_eq!(f.lookup(2), Some((frame, PageState::Loading)));

    assert_eq!(f.reserve(2), Reserve::Loading(frame));
    assert_eq!(f.resident_pages(), 1);

    write_frame_byte(frame, 0, 0xAB);
    f.mark_ready(2);
    assert_eq!(f.lookup(2), Some((frame, PageState::Ready)));
    assert_eq!(f.reserve(2), Reserve::Ready(frame));
    assert_eq!(frame_byte(frame, 0), 0xAB, "ready frame keeps its bytes");
}

#[test]
fn distinct_pages_get_distinct_frames() {
    init_global_heap();
    let f = fobj(8 * PAGE_SIZE);
    let a = match f.reserve(0) {
        Reserve::New(fr) => fr,
        o => panic!("{o:?}"),
    };
    let b = match f.reserve(5) {
        Reserve::New(fr) => fr,
        o => panic!("{o:?}"),
    };
    let c = match f.reserve(3) {
        Reserve::New(fr) => fr,
        o => panic!("{o:?}"),
    };
    assert_ne!(a, b);
    assert_ne!(a, c);
    assert_ne!(b, c);
    assert_eq!(f.resident_pages(), 3);
    assert_eq!(f.lookup(0), Some((a, PageState::Loading)));
    assert_eq!(f.lookup(5), Some((b, PageState::Loading)));
    assert_eq!(f.lookup(3), Some((c, PageState::Loading)));
    assert_eq!(f.lookup(7), None);
}

#[test]
fn mark_ready_absent_is_noop() {
    init_global_heap();
    let f = fobj(PAGE_SIZE);
    f.mark_ready(0); // not present — no panic
    assert_eq!(f.lookup(0), None);
}

use crate::object::device_node::{BlockGeometry, DeviceClass, ResourceDescriptor};
use crate::object::{DeviceNode, IpcChannel, UserspaceServerReg};

fn registration() -> Ref<UserspaceServerReg> {
    let (ep, b) = IpcChannel::try_new_pair(4).unwrap();
    drop(b);
    UserspaceServerReg::try_new(ep).unwrap()
}

fn candidate(reg: &Ref<UserspaceServerReg>, id: u64, size: usize) -> Ref<FileObject> {
    candidate_in(reg, id, size, 4096)
}

std::thread_local! {
    /// The test's server's device: one, as a server has, since a cached file refuses another.
    static DEVICE: Ref<DeviceNode> = {
        let dev = DeviceNode::try_new(DeviceClass::Other, ResourceDescriptor::ZERO, BlockGeometry::ZERO).unwrap();
        Ref::new(dev)
    };
}

fn candidate_in(reg: &Ref<UserspaceServerReg>, id: u64, size: usize, block_size: u32) -> Ref<FileObject> {
    let device = DEVICE.with(Ref::clone);
    let f = FileObject::try_new(size, Producer::FsServerBlocks { device, block_size, reg: reg.clone(), file_id: id })
        .unwrap();
    Ref::new(f)
}

fn resolve(reg: &Ref<UserspaceServerReg>, id: u64, size: usize) -> Ref<FileObject> {
    FileObject::cache_in(reg, candidate(reg, id, size)).unwrap()
}

fn fill(f: &FileObject, index: usize, byte: u8) -> PhysAddr {
    let frame = match f.reserve(index) {
        Reserve::New(fr) => fr,
        o => panic!("{o:?}"),
    };
    let page = (frame.as_u64() + heap::hhdm_offset()) as *mut u8;
    // SAFETY: `frame` is a live, HHDM-reachable, unaliased cache frame, a page long.
    unsafe { page.write_bytes(byte, PAGE_SIZE) };
    f.mark_ready(index);
    frame
}

/// **A reply that changes a cached file's block size is refused**: the kernel's map check
/// reads each reply's runs against that reply's block size, and the object keeps its own.
#[test]
fn a_reply_changing_a_cached_files_block_size_is_refused() {
    init_global_heap();
    let reg = registration();
    let first = FileObject::cache_in(&reg, candidate_in(&reg, 7, 2 * PAGE_SIZE, 4096)).unwrap();
    let changed = FileObject::cache_in(&reg, candidate_in(&reg, 7, PAGE_SIZE, 512));
    assert_eq!(changed.err(), Some(CacheInError::BlockSizeChanged));
    assert_eq!(first.size(), 2 * PAGE_SIZE, "nothing resized");
    let same = FileObject::cache_in(&reg, candidate_in(&reg, 7, PAGE_SIZE, 4096)).unwrap();
    assert_eq!(same.as_ptr(), first.as_ptr());
    assert_eq!(first.size(), PAGE_SIZE, "a reply in its own block size resizes it");
}

#[test]
fn two_resolves_of_one_file_share_one_object() {
    init_global_heap();
    let reg = registration();
    let a = resolve(&reg, 7, PAGE_SIZE);
    let b = resolve(&reg, 7, PAGE_SIZE);
    assert_eq!(a.as_ptr(), b.as_ptr(), "one file, one object");
    let other = resolve(&reg, 8, PAGE_SIZE);
    assert_ne!(other.as_ptr(), a.as_ptr(), "another file, another object");
    assert_eq!(reg.cached_files(), 2);
    let frame = fill(&a, 0, 0x5A);
    assert_eq!(b.lookup(0), Some((frame, PageState::Ready)));
}

#[test]
fn a_later_resolve_resizes_the_shared_object() {
    init_global_heap();
    let reg = registration();
    let a = resolve(&reg, 7, PAGE_SIZE);
    let b = resolve(&reg, 7, 3 * PAGE_SIZE);
    assert_eq!(a.as_ptr(), b.as_ptr());
    assert_eq!(a.size(), 3 * PAGE_SIZE, "the grow reached the first holder too");
}

#[test]
fn a_clean_object_leaves_the_cache_with_its_last_user() {
    init_global_heap();
    let reg = registration();
    let a = resolve(&reg, 7, PAGE_SIZE);
    let first = a.as_ptr();
    FileObject::writable_mapped(&a);
    a.writable_unmapped();
    let mark = a.clean_mark().expect("no writable mapping left");
    FileObject::unpin_if_clean(&a, mark);
    assert!(!a.is_dirty());
    test_probe::reset();
    drop(a);
    assert_eq!(test_probe::file_object_destroys(), 1, "clean, and nothing holds it");
    assert_eq!(reg.cached_files(), 0, "and its entry went with it");
    let again = resolve(&reg, 7, PAGE_SIZE);
    assert_eq!(reg.cached_files(), 1);
    drop((first, again));
}

#[test]
fn a_dirty_object_outlives_its_last_user_and_is_found_again() {
    init_global_heap();
    let reg = registration();
    let a = resolve(&reg, 7, PAGE_SIZE);
    let first = a.as_ptr();
    let frame = fill(&a, 0, 0xC3);
    FileObject::writable_mapped(&a);
    a.writable_unmapped();
    assert!(a.is_dirty());
    test_probe::reset();
    drop(a);
    assert_eq!(test_probe::file_object_destroys(), 0, "dirty: it holds itself");
    assert_eq!(reg.cached_files(), 1);
    let found = resolve(&reg, 7, PAGE_SIZE);
    assert_eq!(found.as_ptr(), first, "the next resolve finds it");
    test_probe::reset(); // the resolve's own candidate was dropped for it
    assert_eq!(found.lookup(0), Some((frame, PageState::Ready)), "with its written page");
    assert_eq!(reg.cache_objects().unwrap().len(), 1, "and a sync can enumerate it");
    let mark = found.clean_mark().unwrap();
    FileObject::unpin_if_clean(&found, mark);
    drop(found);
    assert_eq!(test_probe::file_object_destroys(), 1);
    assert_eq!(reg.cached_files(), 0);
}

#[test]
fn a_write_back_cleans_only_if_no_writable_mapping_existed_throughout() {
    init_global_heap();
    let reg = registration();
    let a = resolve(&reg, 7, PAGE_SIZE);
    let fo = &a;

    FileObject::writable_mapped(&a);
    assert_eq!(fo.clean_mark(), None);
    a.writable_unmapped();
    assert!(fo.is_dirty(), "the sync that began mapped does not clean");

    let mark = fo.clean_mark().unwrap();
    FileObject::writable_mapped(&a);
    a.writable_unmapped();
    FileObject::unpin_if_clean(&a, mark);
    assert!(fo.is_dirty(), "a mapping during the write-back keeps it dirty");

    let mark = fo.clean_mark().unwrap();
    FileObject::unpin_if_clean(&a, mark);
    assert!(!fo.is_dirty());
    assert_eq!(fo.writable_maps(), 0);
}

/// An object with no id is in no cache a sync could search, so a pin on it would be a leak.
#[test]
fn an_uncached_object_never_pins_itself() {
    init_global_heap();
    let reg = registration();
    let a = candidate(&reg, 0, PAGE_SIZE);
    FileObject::writable_mapped(&a);
    assert_eq!(a.writable_maps(), 1);
    assert!(!a.is_dirty());
    a.writable_unmapped();
    assert_eq!(reg.cached_files(), 0);
    a.writable_unmapped();
    assert_eq!(a.writable_maps(), 0);
}

/// **A truncate and then a grow read zero over the regrown range.** The test holds the retired
/// frames to their old bytes: "a mapping's pages stay valid" alone passes for a design that
/// kept the pages.
#[test]
fn a_truncate_then_a_grow_reads_zero_over_the_regrown_range() {
    init_global_heap();
    let f = fobj(3 * PAGE_SIZE);
    let p0 = fill(&f, 0, 0xAB);
    let p1 = fill(&f, 1, 0xAB);
    let p2 = fill(&f, 2, 0xAB);

    f.resize(10, KVec::new()).unwrap();
    assert_eq!(f.lookup(1), None, "a whole page past the end leaves the index");
    assert_eq!(f.lookup(2), None);
    assert_eq!(f.retired_frames(), 2);
    assert_eq!((frame_byte(p1, 0), frame_byte(p2, 0)), (0xAB, 0xAB), "a mapping of one stays valid");
    assert_eq!(frame_byte(p0, 9), 0xAB, "the kept bytes");
    assert_eq!(frame_byte(p0, 10), 0, "the partial tail is zeroed");

    write_frame_byte(p0, 100, 0xCD);
    let phantom = fill(&f, 1, 0xCD);

    f.resize(3 * PAGE_SIZE, KVec::new()).unwrap();
    assert_eq!(f.size(), 3 * PAGE_SIZE);
    assert_eq!(frame_byte(p0, 9), 0xAB);
    assert_eq!(frame_byte(p0, 100), 0, "the partial tail reads zero after the grow");
    assert_eq!(f.lookup(1), None, "the page faulted in past the end is not served");
    assert_eq!(frame_byte(phantom, 0), 0xCD, "its frame is retired, not reused");
    // A page not resident is filled afresh: zeroed, then from the map.
    match f.reserve(1) {
        Reserve::New(fr) => assert_eq!(frame_byte(fr, 0), 0),
        o => panic!("{o:?}"),
    }
}

/// **A file read whole while a user mapping writes a page is no race**, which Miri tells.
#[test]
fn a_file_read_whole_while_another_writes_a_page_is_no_race() {
    use core::sync::atomic::{AtomicU64, Ordering};
    init_global_heap();
    let f = Ref::new(fobj(PAGE_SIZE));
    let words = (fill(&f, 0, 0).as_u64() + heap::hhdm_offset()) as usize;
    std::thread::scope(|s| {
        s.spawn(|| {
            for i in 0..PAGE_SIZE / 8 {
                // SAFETY: a word of the file's ready page, which outlives the scope; stored
                // atomically, as a user's store is to the kernel's reads.
                unsafe { AtomicU64::from_ptr((words as *mut u64).add(i)) }.store(u64::MAX, Ordering::Relaxed);
            }
        });
        let copy = f.read_to_kvec(|i| FileObject::fault_in_page(&f, i)).unwrap();
        assert!(copy.iter().all(|&b| b == 0 || b == 0xFF));
    });
}

/// **A page a fault's fill is filling is neither lent nor made ready but by its settle**: a failed
/// fill frees it, so a view of it, or a ready page a settle could free, would outlive its frame.
#[test]
fn a_page_being_filled_is_neither_lent_nor_marked_ready() {
    init_global_heap();
    let f = fobj(PAGE_SIZE);
    let Fault::Fill(_, po) = f.reserve_fault(0) else { panic!("a miss") };
    assert!(f.ready_page(0).is_none(), "a loading page is not lent");
    f.mark_ready(0);
    assert_eq!(f.lookup(0).map(|(_, state)| state), Some(PageState::Loading), "nor marked ready");
    f.settle(0, &po, false);
    assert!(f.lookup(0).is_none(), "the failed fill freed it");
}

/// **A cache frees a failed fill's frame as it settles, a retired or cached page's with the object.**
#[test]
fn a_cache_frees_each_pages_frame_once() {
    init_global_heap();
    let f = fobj(3 * PAGE_SIZE);
    for i in 0..3 {
        fill(&f, i, 1);
    }
    let freed = heap::frames_freed_by_this_thread();
    f.resize(PAGE_SIZE, KVec::new()).unwrap();
    let Fault::Fill(_, po) = f.reserve_fault(1) else { panic!("a miss") };
    f.settle(1, &po, false);
    assert_eq!(heap::frames_freed_by_this_thread() - freed, 1, "only the failed fill's");
    drop(f);
    assert_eq!(heap::frames_freed_by_this_thread() - freed, 4, "the cached page and the two retired");
}

/// **A page a truncation takes between its fault and its copy fails the read**, rather than
/// lending a page that is no longer the file's.
#[test]
fn a_whole_read_fails_at_a_page_truncated_after_its_fault() {
    init_global_heap();
    let f = fobj(2 * PAGE_SIZE);
    let faults = core::cell::Cell::new(0);
    let read = f.read_to_kvec(|i| {
        faults.set(faults.get() + 1);
        let frame = fill(&f, i, 7);
        (i == 1).then(|| f.resize(PAGE_SIZE, KVec::new()).unwrap());
        Some(frame)
    });
    assert!(read.is_err() && faults.get() == 2, "the second page, truncated after its fault, fails it");
    let whole = fobj(2 * PAGE_SIZE);
    let read = whole.read_to_kvec(|i| Some(fill(&whole, i, 7)));
    assert!(read.is_ok_and(|b| b.len() == 2 * PAGE_SIZE && b.iter().all(|&x| x == 7)), "untruncated, it reads whole");
}

mod io;
mod rw;
