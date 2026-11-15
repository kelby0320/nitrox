use super::*;
use libkern::RawHandle;
use libos::{Carried, mock};

fn owned<T, M>(raw: u64) -> Handle<T, M> {
    // SAFETY: a handle the mock has just made, which nothing else holds.
    unsafe { Handle::from_raw(RawHandle(raw), Rights::empty()) }
}

fn control(sent: Option<Option<u64>>) -> (Handle<Channel, Only>, Handle<Channel, Only>) {
    let (smgr, ours) = Handle::<Channel, Only>::pair(4).unwrap();
    if let Some(handle) = sent {
        let mut carried = Carried::new();
        if let Some(raw) = handle {
            let h: Handle<Resource, WriteOnly> = owned(raw);
            carried.push(h).ok().unwrap();
        }
        smgr.send(&Msg::new(), &mut carried).unwrap();
    }
    (smgr, ours)
}

#[test]
fn the_clock_is_taken_from_the_first_message_and_nothing_else_is_taken_for_it() {
    mock::reset();
    let (_s, ours) = control(Some(Some(mock::clock_node())));
    assert!(receive(Some(&ours)).is_some(), "the clock node");
    let (_s, ours) = control(Some(None));
    assert!(receive(Some(&ours)).is_none(), "an empty message: none");
    let (_s, ours) = control(Some(Some(mock::with_rights(mock::forwarding(), Rights::SEND | Rights::TRANSFER))));
    assert!(receive(Some(&ours)).is_none(), "a channel, as a log endpoint is: not the clock");
    let unwritable = mock::with_rights(mock::clock_node(), Rights::INSPECT | Rights::TRANSFER);
    let (_s, ours) = control(Some(Some(unwritable)));
    assert!(receive(Some(&ours)).is_none(), "a node it may not write");
    let (_s, ours) = control(None);
    assert!(receive(Some(&ours)).is_none(), "nothing within the wait");
    assert!(receive(None).is_none(), "no control channel");
}

#[test]
fn a_view_gets_the_clock_at_its_path_keeps_its_callers_and_is_refused_without_one() {
    mock::reset();
    let clock: Clock = owned(mock::clock_node());
    let view = Handle::<Namespace, NsMutable>::create().unwrap();
    assert_eq!(bind(None, &view), Err("the clock is not here to grant"));
    assert_eq!(bind(Some(&clock), &view), Ok(true), "bound, this broker's");
    let nested = view.copy().unwrap();
    assert_eq!(bind(Some(&clock), &nested), Ok(false), "a view inside one keeps its caller's");
    let node: Handle<Resource, WriteOnly> =
        libos::block_on(view.lookup(CLOCK_PATH, Rights::WRITE | Rights::INSPECT)).unwrap();
    assert!(node.stat().is_ok_and(|info| info.object_type == KOBJ_DEVICE_NODE));
}
