use super::*;
use crate::mm::heap;
use crate::mm::kvmap::VmapFrames;
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
    assert_eq!(t.unmap(mapped), Err(UnmapError::NotUser));
    assert!(t.translate(mapped).is_some(), "the kernel's page, still mapped");
    let frame = Frame::alloc_zeroed().unwrap();
    let phys = frame.phys();
    let refused = [
        (guard, rw()),
        (VirtAddr::new(USER), rw() | PageFlags::GLOBAL),
        (VirtAddr::new(USER), rw() | PageFlags::WRITE_COMBINING),
        (VirtAddr::new(USER), rw() | PageFlags::NO_CACHE),
    ];
    let mut frame = Some(frame);
    for (va, flags) in refused {
        let (back, e) = t.map_owned(va, frame.take().unwrap(), flags).unwrap_err();
        assert_eq!((back.phys(), e), (phys, MapError::NotUser), "{va:?} {flags:?}");
        frame = Some(back);
    }
    assert_eq!(t.translate(guard), None, "the guard page, still absent");
    // SAFETY: the frame this test holds, mapped nowhere here, as each map is refused before it.
    let global = unsafe { t.map(VirtAddr::new(USER), phys, rw() | PageFlags::GLOBAL) };
    assert_eq!(global, Err(MapError::NotUser));
    // SAFETY: as above; an object's frame may be write-combining, as an aperture's is.
    assert_eq!(unsafe { t.map(VirtAddr::new(USER), phys, rw() | PageFlags::WRITE_COMBINING) }, Ok(()));
    assert_eq!(t.unmap(VirtAddr::new(USER)), Ok(phys));
}
