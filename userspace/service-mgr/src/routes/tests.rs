use super::*;
use libkern::{KIND_CHILD_EXITED, Notification, RawHandle};
use libos::{Carried, mock, time};
use librsproto::{OP_NS_RESOLVE, RS_FLAG_ERROR};

/// Queue a resolve on `client`, as the kernel forwards one, with request id `id`.
fn resolve(client: &Endpoint, id: u64) {
    let mut body = [0u8; 64];
    let n = librsproto::namespace::resolve_request(&mut body, 0, 0, b"x").unwrap();
    let mut msg = Msg::new();
    let len = librsproto::encode(msg.payload_mut(), OP_NS_RESOLVE, id, 0, &body[..n], 0).unwrap();
    msg.set_len(len).unwrap();
    client.send(&msg, &mut Carried::new()).unwrap();
}

/// Each reply queued on `client`, as `(request id, whether it is an error)`.
fn replies(client: &Endpoint) -> Vec<(u64, bool)> {
    let mut msg = Msg::new();
    let mut got = Vec::new();
    while client.recv(&mut msg).is_ok() {
        let m = librsproto::decode(msg.payload()).unwrap();
        got.push((m.request_id, m.flags & RS_FLAG_ERROR != 0));
    }
    got
}

fn junk(client: &Endpoint, n: usize) {
    let mut msg = Msg::new();
    msg.payload_mut()[..4].copy_from_slice(b"junk");
    msg.set_len(4).unwrap();
    for _ in 0..n {
        client.send(&msg, &mut Carried::new()).unwrap();
    }
}

/// A `ChildExited` with `code`.
fn exited(code: i32) -> Notification {
    let mut n = Notification { kind: KIND_CHILD_EXITED, body: [0; 60] };
    n.body[8..12].copy_from_slice(&code.to_le_bytes());
    n
}

/// **The grace wait for an exit code lasts its length though a route wakes it**: a resolve before a
/// `ChildExited` ended it, and the death was a failure. A code that comes after that wake is taken.
#[test]
fn the_grace_wait_lasts_its_length_though_a_route_wakes_it() {
    for late in [false, true] {
        mock::reset();
        let mut routes = Routes::default();
        let r = routes.route(0, "tty-server", None).unwrap();
        let raw = mock::notify(&[]);
        // SAFETY: a queue the mock has just made, which nothing else holds.
        let notif = unsafe { Handle::<Notify, Only>::from_raw(RawHandle(raw), Rights::empty()) };
        resolve(&routes[r].client, 7);
        if late {
            mock::notify_after_wait(raw, exited(0));
        }
        let until = time::now().unwrap() + 1_000_000_000;
        assert_eq!(wait_codes(&notif, &routes, None, until), late, "late {late}: the code");
        assert_eq!(replies(&routes[r].client), [(7, true)], "late {late}: the resolve answered meanwhile");
        assert_eq!(mock::sleeps(), !late as u32, "late {late}: the grace waited out");
    }
}

/// **A pass answers at most [`PER_PASS`] resolves a route**, the rest left for the next: a client
/// that kept a ring full held this manager in that route's loop, reaping and restarts with it.
#[test]
fn a_pass_answers_a_bounded_number_of_resolves_a_route() {
    mock::reset();
    let mut routes = Routes::default();
    let r = routes.route(0, "tty-server", None).unwrap();
    for id in 0..PER_PASS as u64 + 3 {
        resolve(&routes[r].client, id);
    }
    routes.serve(None, |_| true);
    assert_eq!(replies(&routes[r].client).len(), PER_PASS, "one pass");
    routes.serve(None, |_| true);
    assert_eq!(replies(&routes[r].client).len(), 3, "the next");
}

/// **A pass takes at most [`PER_PASS`] requests from a channel**, and says it closed only once
/// none is left.
#[test]
fn a_pass_takes_a_bounded_number_of_requests_from_a_channel() {
    mock::reset();
    let (ours, theirs) = Endpoint::pair(64).unwrap();
    for id in 0..PER_PASS as u64 + 3 {
        resolve(&theirs, id);
    }
    drop(theirs);
    assert_eq!(take(&ours).0.len(), PER_PASS);
    let (rest, closed) = take(&ours);
    assert_eq!((rest.len(), closed), (3, true));
}

/// **A route that keeps signalling does not hold the grace wait past its deadline.**
#[test]
fn a_route_that_keeps_signalling_ends_the_grace_wait_at_its_deadline() {
    mock::reset();
    let mut routes = Routes::default();
    let r = routes.route(0, "tty-server", None).unwrap();
    // SAFETY: a queue the mock has just made, which nothing else holds.
    let notif = unsafe { Handle::<Notify, Only>::from_raw(RawHandle(mock::notify(&[])), Rights::empty()) };
    for id in 0..SERVE_DEPTH as u64 {
        resolve(&routes[r].client, id);
    }
    assert!(!wait_codes(&notif, &routes, None, time::now().unwrap() + 2_000_000));
    assert!(replies(&routes[r].client).len() < SERVE_DEPTH as usize, "ended by the clock, not by the ring");
}

/// **A route copied before its server is up reaches it once it is**, as a login supervisor's must.
#[test]
fn a_route_copied_before_its_server_is_up_reaches_it_once_it_is() {
    mock::reset();
    let registry = BindNs::create().unwrap();
    let mut routes = Routes::default();
    let copy = routes.copy(3, "storage-service", None).expect("a copy before the server is up");
    // Its registration, as `register` makes it: the route found, and live.
    let r = routes.route(3, "storage-service", None).unwrap();
    assert!(mock::same_object(copy.raw().0, routes[r].client.raw().0), "the copy is that route");
    resolve(&routes[r].client, 1);
    routes.serve(Some(&registry), |_| true);
    assert_eq!(replies(&routes[r].client), [(1, true)], "NotFound until it is up");
    routes[r].live = true;
    resolve(&routes[r].client, 2);
    routes.serve(Some(&registry), |_| true);
    assert_eq!(replies(&routes[r].client), [(2, false)], "answered once it is up");
}

/// **A pass counts the messages that do not decode**, or junk queued as fast as read held it.
#[test]
fn a_pass_counts_the_messages_that_do_not_decode() {
    mock::reset();
    let (ours, theirs) = Endpoint::pair(64).unwrap();
    junk(&theirs, PER_PASS + 4);
    resolve(&theirs, 9);
    assert_eq!(take(&ours), (Vec::new(), false), "junk alone");
    let (asked, closed) = take(&ours);
    assert_eq!((asked.iter().map(|r| r.1).collect::<Vec<_>>(), closed), (vec![9], false), "the next pass");
}

/// **A look at a control channel is as bounded**, and finds a peer gone behind a full ring.
#[test]
fn a_look_at_a_control_channel_is_bounded_and_sees_past_a_full_ring() {
    mock::reset();
    let (ours, theirs) = Endpoint::pair(64).unwrap();
    junk(&theirs, PER_PASS);
    resolve(&theirs, 9);
    assert!(!channel_peer_closed(&ours), "alive");
    assert_eq!(take(&ours).0.len(), 1, "what the look left");
    let (ours, theirs) = Endpoint::pair(CONTROL_DEPTH).unwrap();
    junk(&theirs, CONTROL_DEPTH as usize);
    drop(theirs);
    assert!(channel_peer_closed(&ours), "gone");
}

/// **A channel whose peer is open is not closed**, with nothing queued or with less than a pass.
#[test]
fn an_open_channel_is_not_taken_for_closed() {
    mock::reset();
    let (ours, theirs) = Endpoint::pair(64).unwrap();
    assert_eq!(take(&ours), (Vec::new(), false), "nothing queued");
    resolve(&theirs, 1);
    resolve(&theirs, 2);
    assert_eq!((take(&ours).0.len(), take(&ours)), (2, (Vec::new(), false)), "two, then none");
}

/// **A derived endpoint has a route of its own**, at its own base, beside its server's.
#[test]
fn a_derived_endpoint_has_a_route_of_its_own() {
    mock::reset();
    let mut routes = Routes::default();
    let own = routes.route(4, "storage-service", None).unwrap();
    let session = routes.route(4, "storage-service", Some("session-endpoint")).unwrap();
    assert_ne!(own, session);
    assert_eq!(routes.route(4, "storage-service", Some("session-endpoint")), Some(session), "found again");
    assert_eq!(routes.route(4, "storage-service", None), Some(own), "found again");
    assert_eq!(
        (routes[own].base.as_str(), routes[session].base.as_str()),
        ("/storage-service", "/storage-service.session-endpoint")
    );
}

/// **A pass serves each channel it picks, and takes out those whose peer has gone**, in order.
#[test]
fn a_pass_serves_each_channel_it_picks_and_takes_out_those_gone() {
    struct M {
        list: Vec<Endpoint>,
        asked: Vec<(u64, u64)>,
        gone: Vec<u64>,
    }
    mock::reset();
    let mut m = M { list: Vec::new(), asked: Vec::new(), gone: Vec::new() };
    let mut peers = Vec::new();
    for _ in 0..5 {
        let (ours, theirs) = Endpoint::pair(64).unwrap();
        m.list.push(ours);
        peers.push(Some(theirs));
    }
    let ids: Vec<u64> = m.list.iter().map(|h| h.raw().0).collect();
    // 0 asks; 1 has gone; 2 asks twice and goes; 3 has gone, not picked; 4 asks.
    for (at, id) in [(0, 10), (2, 20), (2, 21), (4, 40)] {
        resolve(peers[at].as_ref().unwrap(), id);
    }
    for at in [1, 2, 3] {
        peers[at] = None;
    }
    let each = |m: &mut M, k: usize, r: (u16, u64, Vec<u8>)| {
        let id = m.list[k].raw().0;
        m.asked.push((id, r.1));
    };
    serve_each(&mut m, |m| &mut m.list, |id| id != ids[3], each, |m, id| m.gone.push(id));
    assert_eq!(m.asked, [(ids[0], 10), (ids[2], 20), (ids[2], 21), (ids[4], 40)]);
    assert_eq!(m.gone, [ids[1], ids[2]]);
    assert_eq!(m.list.iter().map(|h| h.raw().0).collect::<Vec<_>>(), [ids[0], ids[3], ids[4]]);
}
