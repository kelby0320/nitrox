use super::*;
use crate::arch::fail_page_tables_after;
use crate::mm::heap;
use crate::mm::kvmap::{VMAP_WINDOW_END, VmapFrames};
use crate::mm::test_support::init_global_heap;

const USER: u64 = 0x40_0000;

fn rw() -> PageFlags {
    PageFlags::WRITABLE | PageFlags::USER
}

#[test]
fn a_user_half_owns_what_it_maps_and_gives_back_what_it_unmaps() {
    init_global_heap();
    let mut t = UserTables::new().unwrap();
    let frame = Frame::alloc_zeroed().unwrap();
    let phys = frame.phys();
    assert!(t.map_owned(VirtAddr::new(USER), frame, rw()).is_ok());
    assert_eq!(t.translate(VirtAddr::new(USER + 8)), Some(PhysAddr::new(phys.as_u64() + 8)));
    let freed = heap::frames_freed_by_this_thread();
    assert_eq!(t.unmap(VirtAddr::new(USER)), Ok(phys), "the frame, not freed");
    assert_eq!(heap::frames_freed_by_this_thread(), freed);
    assert_eq!(t.translate(VirtAddr::new(USER)), None);
    // SAFETY: the frame `map_owned` handed over, unmapped in tables no CPU has loaded.
    drop(unsafe { Frame::from_pte(phys) });
    drop(t);
    assert_eq!(heap::frames_freed_by_this_thread() - freed, 2, "the frame, then the root");
}

/// **A user half's tables edit no kernel mapping, and map no global or uncached frame of theirs.**
#[test]
fn a_user_half_refuses_the_kernel_half_global_mappings_and_its_own_frames_uncached() {
    init_global_heap();
    let mut t = UserTables::new().unwrap();
    let stack = VmapFrames::<1>::new(PageFlags::WRITABLE, 1).unwrap();
    let (mapped, guard) = (stack.base(), stack.guard_base());
    let top = VirtAddr::new(USER_MAP_END);
    assert_eq!((t.unmap(mapped), t.unmap(top)), (Err(UnmapError::NotUser), Err(UnmapError::NotUser)));
    assert!(t.translate(mapped).is_some(), "the kernel's page, still mapped");
    assert_eq!(KernelTables::get().translate(mapped), t.translate(mapped), "in the kernel's tables too");
    assert_eq!(KernelTables::get().attribute_index(mapped), Some(0), "and write-back, in the kernel's tables");
    let frame = Frame::alloc_zeroed().unwrap();
    let phys = frame.phys();
    let refused = [
        (guard, rw()),
        (top, rw()),
        (VirtAddr::new(USER), rw() | PageFlags::GLOBAL),
        (VirtAddr::new(USER), rw() | PageFlags::WRITE_COMBINING),
        (VirtAddr::new(USER), rw() | PageFlags::NO_CACHE),
        (VirtAddr::new(USER), rw() | PageFlags::WRITE_THROUGH),
    ];
    let mut frame = Some(frame);
    for (va, flags) in refused {
        let (back, e) = t.map_owned(va, frame.take().unwrap(), flags).unwrap_err();
        assert_eq!((back.phys(), e), (phys, MapError::NotUser), "{va:?} {flags:?}");
        frame = Some(back);
    }
    assert_eq!(t.translate(guard), None, "the guard page, still absent");
    for (va, flags) in [(guard, rw()), (top, rw()), (VirtAddr::new(USER), rw() | PageFlags::GLOBAL)] {
        // SAFETY: the frame this test holds, mapped nowhere here, as each map is refused before it.
        assert_eq!(unsafe { t.map(va, phys, flags) }, Err(MapError::NotUser), "{va:?} {flags:?}");
    }
    assert_eq!(KernelTables::get().translate(guard), None);
    // SAFETY: as above; an object's frame may be write-combining, as an aperture's is.
    assert_eq!(unsafe { t.map(VirtAddr::new(USER), phys, rw() | PageFlags::WRITE_COMBINING) }, Ok(()));
    assert_eq!(t.unmap(VirtAddr::new(USER)), Ok(phys));
}

/// **A user half hands an owned frame back on every refusal**, freeing nothing of the caller's.
#[test]
fn a_user_half_hands_back_an_owned_frame_it_could_not_map() {
    init_global_heap();
    let mut t = UserTables::new().unwrap();
    assert!(t.map_owned(VirtAddr::new(USER), Frame::alloc_zeroed().unwrap(), rw()).is_ok());
    let frame = Frame::alloc_zeroed().unwrap();
    let phys = frame.phys();
    let freed = heap::frames_freed_by_this_thread();
    let (frame, e) = t.map_owned(VirtAddr::new(USER), frame, rw()).unwrap_err();
    assert_eq!((frame.phys(), e), (phys, MapError::AlreadyMapped));
    fail_page_tables_after(Some(0));
    let refused = t.map_owned(VirtAddr::new(1 << 39), frame, rw());
    fail_page_tables_after(None);
    let (frame, e) = refused.unwrap_err();
    assert_eq!((frame.phys(), e), (phys, MapError::OutOfMemory));
    assert_eq!(heap::frames_freed_by_this_thread(), freed, "nothing freed but by the caller");
}

/// **The kernel's tables link no top-level table after boot**, or the boot root's alone would map.
#[test]
fn a_kernel_map_past_the_shared_top_level_is_refused() {
    init_global_heap();
    let va = VirtAddr::new(VMAP_WINDOW_END);
    let frame = Frame::alloc_zeroed().unwrap();
    // SAFETY: a kernel page past the vmap's window, which nothing uses; refused before any write.
    assert_eq!(unsafe { KernelTables::get().map(va, frame.phys(), PageFlags::WRITABLE) }, Err(MapError::Unshared));
    assert_eq!(KernelTables::get().translate(va), None);
}

#[test]
#[should_panic(expected = "adopted twice")]
fn the_kernels_tables_are_adopted_once() {
    init_global_heap();
    // SAFETY: refused before it reaches any table.
    unsafe { KernelTables::adopt(KernelTables::get().root(), core::iter::empty()) };
}
