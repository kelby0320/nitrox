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
