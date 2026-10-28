use super::*;
use crate::mm::addr_space::AddressSpace;
use crate::mm::test_support::init_global_heap;

const PAGE: u64 = PAGE_SIZE as u64;
const BASE: u64 = 0x1000_0000;

fn spare(pages: u64) -> KBox<Slot> {
    KBox::try_new(Slot { start: 0, pages, next: None }).unwrap()
}

fn translate(root: PhysAddr, va: u64) -> Option<PhysAddr> {
    // SAFETY: a read-only walk of a root this test holds.
    unsafe { Paging::translate(root, VirtAddr::new(va)) }
}

#[test]
fn a_vmap_hands_out_va_up_to_its_end_and_no_further() {
    let mut v = Vmap::new(BASE, BASE + 8 * PAGE);
    assert_eq!(v.bump(3), Ok(BASE));
    assert_eq!(v.bump(5), Ok(BASE + 3 * PAGE), "the range that ends at the end");
    assert_eq!(v.bump(1), Err(AllocError));
    let mut v = Vmap::new(BASE, BASE + 8 * PAGE);
    assert_eq!(v.bump(9), Err(AllocError), "one page past it");
    assert_eq!(v.bump(u64::MAX), Err(AllocError));
}

#[test]
fn a_range_given_back_is_reused_by_the_next_of_its_size_only() {
    init_global_heap();
    let mut v = Vmap::new(BASE, BASE + 64 * PAGE);
    let (a, unused) = v.take(spare(5));
    assert!(unused.is_none(), "a fresh range keeps the spare");
    let a = a.unwrap();
    let b = v.take(spare(3)).0.unwrap();
    let (a_start, b_start) = (a.start, b.start);
    v.give_back(a);
    v.give_back(b);
    let c = v.take(spare(4)).0.unwrap();
    assert!(c.start != a_start && c.start != b_start, "no range of its size was given back");
    let (d, unused) = v.take(spare(5));
    assert_eq!(d.unwrap().start, a_start, "found past a range of another size");
    assert!(unused.is_some(), "the spare comes back unused");
    assert_eq!(v.take(spare(3)).0.unwrap().start, b_start);
}

/// Mapped after both spaces were made; past the window, a page is under one root alone.
#[test]
fn the_last_page_the_vmap_hands_out_is_mapped_under_every_root() {
    init_global_heap();
    let last = VMAP.lock().end - PAGE;
    let (a, b) = (AddressSpace::new().unwrap(), AddressSpace::new().unwrap());
    let frame = heap::buddy_alloc(0).unwrap();
    // SAFETY: the window's last page, far past the cursor, which no test hands out.
    unsafe { map_all(last, core::iter::once(frame), PageFlags::WRITABLE).unwrap() };
    for root in [a.root(), b.root()] {
        assert_eq!(translate(root, last), Some(frame));
    }
    // SAFETY: mapped just above, and used by nothing.
    unsafe { unmap_all(last, 1) };
    heap::buddy_free(frame, 0);
}

#[test]
fn a_dropped_range_is_shot_down_before_its_frames_are_freed() {
    init_global_heap();
    let other = AddressSpace::new().unwrap();
    let range = VmapFrames::<3>::new(PageFlags::WRITABLE, 1).unwrap();
    let base = range.base().as_u64();
    assert!((0..3).all(|i| translate(other.root(), base + i * PAGE).is_some()));
    assert_eq!(translate(other.root(), base - PAGE), None, "the guard page");
    assert_eq!(range.end().as_u64(), base + 3 * PAGE);
    let (shots, freed) = (crate::tlb::shootdowns_by_this_thread().0, heap::frames_freed_by_this_thread());
    drop(range);
    let (shots_after, freed_at_shot) = crate::tlb::shootdowns_by_this_thread();
    assert_eq!(shots_after, shots + 1);
    assert_eq!(freed_at_shot, freed, "no frame freed before the shootdown");
    assert_eq!(heap::frames_freed_by_this_thread() - freed, 3);
    assert!((0..3).all(|i| translate(other.root(), base + i * PAGE).is_none()));
}

#[test]
fn a_repointed_window_reads_the_new_frame_and_shoots_down_the_old() {
    init_global_heap();
    let other = AddressSpace::new().unwrap();
    let mut win = MmioWindow::new().unwrap();
    let (first, second) = (heap::buddy_alloc(0).unwrap(), heap::buddy_alloc(0).unwrap());
    let shots = crate::tlb::shootdowns_by_this_thread().0;
    // SAFETY: frames this test owns, read through the window by nothing.
    unsafe { win.repoint(first).unwrap() };
    assert_eq!(translate(other.root(), win.va().as_u64()), Some(first));
    assert_eq!(crate::tlb::shootdowns_by_this_thread().0, shots, "nothing to shoot down yet");
    // SAFETY: as above.
    unsafe { win.repoint(second).unwrap() };
    assert_eq!(translate(other.root(), win.va().as_u64()), Some(second));
    assert_eq!(crate::tlb::shootdowns_by_this_thread().0, shots + 1, "the first frame's page");
}

/// A size no other test asks for, so no other thread takes the range in between.
#[test]
fn a_dropped_range_gives_its_va_to_the_next_of_its_size() {
    init_global_heap();
    let first = VmapFrames::<7>::new(PageFlags::WRITABLE, 2).unwrap().base();
    let again = VmapFrames::<7>::new(PageFlags::WRITABLE, 2).unwrap();
    assert_eq!(again.base(), first);
}
