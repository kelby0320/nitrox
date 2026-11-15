use super::*;
use libkern::{KOBJ_DEVICE_NODE, KOBJ_IPC_CHANNEL, RawHandle, Rights};
use libos::mock;

fn owned<T, M>(raw: u64) -> Handle<T, M> {
    // SAFETY: a handle the mock has just made, which nothing else holds.
    unsafe { Handle::from_raw(RawHandle(raw), Rights::empty()) }
}

fn arrived(theirs: &Endpoint) -> std::vec::Vec<Option<u32>> {
    let mut out = std::vec::Vec::new();
    while let Ok(mut got) = theirs.recv(&mut Msg::new()) {
        let h: Option<Handle<Resource, Only>> = got.take(0);
        out.push(h.map(|h| h.stat().unwrap().object_type));
    }
    out
}

#[test]
fn a_service_gets_its_log_endpoint_then_the_clock_and_the_clock_moves_once() {
    mock::reset();
    let (ours, theirs) = Endpoint::pair(4).unwrap();
    let (log, _peer) = Endpoint::pair(1).unwrap();
    let mut clock: Option<Clock> = Some(owned(mock::clock_node()));
    hand_on(&ours, false, Some(log), true, &mut clock);
    assert_eq!(arrived(&theirs), [Some(KOBJ_IPC_CHANNEL), Some(KOBJ_DEVICE_NODE)], "the log endpoint first");
    assert!(clock.is_none(), "moved");
    hand_on(&ours, true, None, true, &mut clock);
    assert_eq!(arrived(&theirs), [None], "a later declarer: an empty message, read positionally");
}

#[test]
fn a_server_gets_the_clock_alone_and_one_that_does_not_declare_it_gets_none() {
    mock::reset();
    let (ours, theirs) = Endpoint::pair(4).unwrap();
    let mut clock: Option<Clock> = Some(owned(mock::clock_node()));
    hand_on(&ours, true, None, false, &mut clock);
    assert!(arrived(&theirs).is_empty() && clock.is_some(), "nothing sent, and the clock kept");
    hand_on(&ours, false, None, false, &mut clock);
    assert_eq!(arrived(&theirs), [None], "a non-server's log endpoint, here none");
    hand_on(&ours, true, None, true, &mut clock);
    assert_eq!(arrived(&theirs), [Some(KOBJ_DEVICE_NODE)]);
}
