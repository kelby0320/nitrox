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
