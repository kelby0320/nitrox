use super::*;
use core::cell::Cell;

/// **A device that connects between a change's read and its clear is seen**, before the clear's
/// own read or the one after it; decided by the read before, it was lost.
#[test]
fn a_device_connecting_as_a_stale_change_is_cleared_is_seen() {
    for at in [1, 2] {
        let (reg, reads) = (Cell::new(PORT_CSC), Cell::new(0));
        let read = || {
            if reads.replace(reads.get() + 1) == at {
                reg.set(reg.get() | PORT_CCS | PORT_CSC);
            }
            reg.get()
        };
        let write = |v: u32| reg.set(reg.get() & !(v & PORT_CHANGES));
        assert_eq!(connect_change(read, write), Some(true), "connected before read {at}");
    }
    let reg = Cell::new(PORT_CSC);
    let write = |v: u32| reg.set(reg.get() & !(v & PORT_CHANGES));
    assert_eq!(connect_change(|| reg.get(), write), Some(false), "a stale change on an empty port");
    assert_eq!(reg.get() & PORT_CSC, 0, "and it is cleared");
    assert_eq!(connect_change(|| reg.get(), |_| unreachable!()), None, "no change, no write");
}

/// **A read of the data page stops at the page**, whatever length the device gave.
#[test]
fn a_read_of_the_data_page_stops_at_the_page() {
    crate::mm::test_support::init_global_heap();
    let mem = device_mem().unwrap();
    assert_eq!(data_bytes(&mem, 3 * 4096).len(), crate::mm::PAGE_SIZE);
    assert_eq!(data_bytes(&mem, 18).len(), 18);
}

/// **A control transfer's page is cleared, and asked for, no further than the page**, whatever
/// length the device gave.
#[test]
fn a_control_transfer_is_cut_to_the_data_page() {
    crate::mm::test_support::init_global_heap();
    let mut mem = device_mem().unwrap();
    mem.data.as_mut_slice().fill(0xAB);
    assert_eq!(clear_for(&mut mem, 3 * 4096), crate::mm::PAGE_SIZE as u16);
    assert!(mem.data.as_slice().iter().all(|&b| b == 0), "the whole page cleared");
    assert_eq!(clear_for(&mut mem, 18), 18);
}

/// **A default endpoint left in doubt is asked nothing more**: a request may still be on its ring,
/// to complete into the data page. HID's in doubt stopped only HID, so storage's `GET MAX LUN`
/// went on the ring behind it, and storage's in doubt left the keyboard its lights.
#[test]
fn a_default_endpoint_in_doubt_is_asked_nothing_more() {
    crate::mm::test_support::init_global_heap();
    let mut regs = std::boxed::Box::new([0u32; 64]);
    let x = super::super::fake(regs.as_mut_ptr() as u64);
    for (failure, doubt) in [(Failed::Code(code::STALL), false), (Failed::Code(4), true), (Failed::Timeout, true)] {
        let mut mem = device_mem().unwrap();
        assert!(mem.settled::<()>(Err(failure)).is_err());
        assert_eq!(mem.in_doubt, doubt, "{failure}");
    }
    let mut mem = device_mem().unwrap();
    let _ = mem.settled::<()>(Err(Failed::Timeout));
    mem.data.as_mut_slice().fill(0xAB);
    let next = mem.ring.next_slot();
    let refused = |r: Result<(), Failed>| matches!(r, Err(Failed::Device(why)) if why.contains("in doubt"));
    assert!(refused(control_in(&x, 1, &mut mem, [0; 8], 18).map(|_| ())), "a read");
    assert!(refused(control_out(&x, 1, &mut mem, [0; 8])), "a request with no data");
    assert!(refused(control_out_data(&x, 1, &mut mem, [0; 8], &[1])), "a keyboard's lights");
    assert_eq!(mem.ring.next_slot(), next, "nothing on the ring");
    assert!(mem.data.as_slice().iter().all(|&b| b == 0xAB), "and the page untouched");
}
