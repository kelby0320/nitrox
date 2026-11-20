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
