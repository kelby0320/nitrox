use super::*;
use crate::mm::test_support::init_global_heap;

#[test]
fn a_listing_is_refused_a_count_past_its_cap_or_an_array_past_the_user_half_before_any_lookup() {
    let size = core::mem::size_of::<NsEntry>() as u64;
    let fits = USER_VIRT_END - 2 * size;
    let refused = |out, count| sys_ns_enumerate(0xDEAD, 0, out, count);
    assert_eq!(refused(0x1000, 0), Err(KError::InvalidArgument));
    assert_eq!(refused(0x1000, NS_ENUMERATE_MAX + 1), Err(KError::InvalidArgument));
    assert_eq!(refused(0x1004, 1), Err(KError::InvalidArgument), "misaligned");
    assert_eq!(refused(fits, 3), Err(KError::FaultFromUser), "its last entry past the end");
    assert_eq!(refused(USER_VIRT_END - 8, 1), Err(KError::FaultFromUser), "one straddling the end");
    assert_eq!(refused((u64::MAX - size) & !7, 3), Err(KError::FaultFromUser), "an array that wraps");
    assert_eq!(entries(0x1000, NS_ENUMERATE_MAX), Ok(size), "the cap itself is taken");
    assert_eq!(entries(fits, 2), Ok(size), "two entries end at the user half's end");
    assert_eq!(entries(USER_VIRT_END - size, 1), Ok(size), "and one");
}

#[test]
fn a_listing_puts_each_binding_fresh_at_its_place_from_its_index_and_stops_at_the_end() {
    use crate::libkern::handle::NS_ENTRY_PATH_MAX;
    init_global_heap();
    let ns = Namespace::try_new().unwrap();
    for path in [&b"/a/very/long/binding"[..], b"/b", b"/c"] {
        ns.bind(path, ObjectRef::new(MemoryObject::try_new(4096).unwrap()), Rights::READ).ok().unwrap();
    }
    let size = core::mem::size_of::<NsEntry>() as u64;
    let listed = |index, count| {
        let mut got = std::vec::Vec::new();
        let n = fill(&ns, index, count, 0x1000, size, |at, e: &NsEntry| {
            got.push((at, e.path_len, e.path));
            Ok(())
        });
        (n, got)
    };
    let (n, got) = listed(0, 2);
    assert_eq!(n, Ok(2));
    assert_eq!(got.iter().map(|g| g.0).collect::<std::vec::Vec<_>>(), [0x1000, 0x1000 + size], "each at its place");
    assert_eq!((got[1].1, &got[1].2[..2]), (2, &b"/b"[..]));
    assert_eq!(got[1].2[2..], [0u8; NS_ENTRY_PATH_MAX - 2], "no byte of the longer path before it");
    let (n, got) = listed(1, NS_ENUMERATE_MAX);
    assert_eq!(n, Ok(2), "from its index, and short at the end");
    assert_eq!((&got[0].2[..2], &got[1].2[..2]), (&b"/b"[..], &b"/c"[..]));
    assert_eq!(listed(3, 4), (Ok(0), std::vec::Vec::new()), "past the end");
    let mut puts = 0;
    let failed = fill(&ns, 0, 3, 0x1000, size, |_, _| {
        puts += 1;
        if puts == 2 { Err(KError::FaultFromUser) } else { Ok(()) }
    });
    assert_eq!((failed, puts), (Err(KError::FaultFromUser), 2), "a failed put ends it");
}
