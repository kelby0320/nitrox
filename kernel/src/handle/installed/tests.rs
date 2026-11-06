use super::*;
use crate::handle::table::{fail_installs_after, net_installed};
use crate::mm::test_support::init_global_heap;
use crate::object::Process;
use crate::object::header::test_probe;

fn table() -> HandleTable {
    init_global_heap();
    HandleTable::try_new(0x5EED).unwrap()
}

fn process(pid: u32) -> ObjectRef {
    ObjectRef::new(Process::try_new(pid).unwrap())
}

/// **Every return before the commit closes what was installed**, however far the syscall got:
/// the handles are gone from the table and their objects released.
#[test]
fn an_uncommitted_set_closes_every_handle_it_installed() {
    let t = table();
    test_probe::reset();
    let before = net_installed();
    let (h0, h1) = {
        let mut installed = Installed::<3>::new(&t, 7);
        let h0 = installed.install(process(1), Rights::SIGNAL).unwrap();
        let h1 = installed.install(process(2), Rights::SIGNAL).unwrap();
        assert!(t.lookup(h0, 7, Rights::SIGNAL).is_ok() && t.lookup(h1, 7, Rights::SIGNAL).is_ok());
        (h0, h1)
    };
    assert_eq!(t.lookup(h0, 7, Rights::empty()).unwrap_err(), HandleError::InvalidHandle);
    assert_eq!(t.lookup(h1, 7, Rights::empty()).unwrap_err(), HandleError::InvalidHandle);
    assert_eq!(test_probe::process_destroys(), 2);
    assert_eq!(net_installed(), before);
}

#[test]
fn a_committed_set_keeps_its_handles() {
    let t = table();
    test_probe::reset();
    let mut installed = Installed::<1>::new(&t, 7);
    let h = installed.install(process(1), Rights::SIGNAL).unwrap();
    installed.commit();
    assert!(t.lookup(h, 7, Rights::SIGNAL).is_ok());
    assert_eq!(test_probe::process_destroys(), 0);
    drop(t.close(h, 7).unwrap());
    assert_eq!(test_probe::process_destroys(), 1);
}

/// **A refused install hands its reference back**, and the set still closes what it holds: the
/// refused object is destroyed only when its reference drops, the installed one when the set does.
#[test]
fn a_refused_install_hands_back_its_reference_and_the_rest_still_close() {
    let t = table();
    test_probe::reset();
    let before = net_installed();
    let mut installed = Installed::<2>::new(&t, 7);
    installed.install(process(1), Rights::SIGNAL).unwrap();
    fail_installs_after(Some(0));
    let (e, back) = installed.install(process(2), Rights::SIGNAL).unwrap_err();
    fail_installs_after(None);
    assert_eq!(e, HandleError::OutOfMemory);
    assert_eq!(test_probe::process_destroys(), 0, "the refused one is still held, and the first");
    drop(back);
    assert_eq!(test_probe::process_destroys(), 1);
    drop(installed);
    assert_eq!(test_probe::process_destroys(), 2);
    assert_eq!(net_installed(), before);
}

/// A set holds no more than it was sized for: the extra is handed back, not installed unseen.
#[test]
fn a_full_set_refuses_another() {
    let t = table();
    let before = net_installed();
    let mut installed = Installed::<1>::new(&t, 7);
    installed.install(process(1), Rights::SIGNAL).unwrap();
    let (e, back) = installed.install(process(2), Rights::SIGNAL).unwrap_err();
    assert_eq!(e, HandleError::OutOfHandles);
    assert!(back.is::<Process>());
    drop(installed);
    assert_eq!(net_installed(), before);
}
