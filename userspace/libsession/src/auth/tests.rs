use super::*;
use libkern::IPC_PAYLOAD_SIZE;
use libos::{Carried, Handle, mock};
use librsproto::auth::{
    AUTH_RESULT_AUTHENTICATED, build_authenticate_reply, build_denied_reply, parse_authenticate_request,
};
use librsproto::views::{OP_VIEWS_CLOSE_SESSION, OP_VIEWS_OPEN_SESSION, build_session_id, parse_session_id};
use librsproto::{RS_FLAG_ERROR, RS_FLAG_REPLY};
use std::vec::Vec;

type End = Handle<Channel, Only>;

/// Replies to `op` carrying `body`, for each request id a call here could have drawn: the id is one
/// counter for the process, so a reply is queued for every id there is, and a call skips the rest.
fn answer_every_id(server: &End, op: u16, flags: u32, body: &[u8]) {
    for id in 1..=400u64 {
        let mut m = Msg::new();
        let n = librsproto::encode(m.payload_mut(), op, id, flags, body, 0).unwrap();
        m.set_len(n).unwrap();
        server.send(&m, &mut Carried::new()).unwrap();
    }
}

fn pair() -> (End, End) {
    mock::reset();
    End::pair(512).unwrap()
}

/// What the server end was sent: the rsproto op and body.
fn request(server: &End) -> (u16, Vec<u8>) {
    let mut m = Msg::new();
    server.recv(&mut m).unwrap();
    let m = decode(m.payload()).unwrap();
    (m.op, m.body.to_vec())
}

fn authenticated(home: &[u8]) -> Vec<u8> {
    let mut body = [0u8; 512];
    let n = build_authenticate_reply(&mut body, AUTH_RESULT_AUTHENTICATED, b"alice", home).unwrap();
    body[..n].to_vec()
}

/// A home is copied whole or refused: one past the caller's buffer is never cut short.
#[test]
fn a_home_that_does_not_fit_is_refused() {
    let reply = |body: &[u8]| {
        let mut out = std::vec![0u8; 600];
        let len = librsproto::encode(&mut out, OP_AUTHENTICATE, 1, RS_FLAG_REPLY, body, 0).unwrap();
        out.truncate(len);
        out
    };
    let home = [b'h'; 300];
    let ok = reply(&authenticated(&home));
    let (mut short, mut fits) = ([0u8; 299], [0u8; 300]);
    assert_eq!(home_of(&ok, &mut short), None);
    assert_eq!(home_of(&ok, &mut fits), Some(300));
    assert_eq!(fits, home);
    let mut denied = [0u8; 64];
    let n = build_denied_reply(&mut denied).unwrap();
    assert_eq!(home_of(&reply(&denied[..n]), &mut fits), None);
}

/// A login asks auth-service with the user and password, and gets the home it names.
#[test]
fn a_login_sends_its_credentials_and_reads_the_home() {
    let (client, server) = pair();
    answer_every_id(&server, OP_AUTHENTICATE, RS_FLAG_REPLY, &authenticated(b"/home/alice"));
    let mut home = [0u8; 64];
    assert_eq!(authenticate_on(&client, b"alice", b"hunter2", &mut home), Some(11));
    assert_eq!(&home[..11], b"/home/alice");
    let (op, body) = request(&server);
    let asked = parse_authenticate_request(&body).unwrap();
    assert_eq!((op, asked.username, asked.password), (OP_AUTHENTICATE, &b"alice"[..], &b"hunter2"[..]));
}

/// A denial, an error reply, a peer that has gone and a home that will not fit all read as `None`.
#[test]
fn a_login_that_is_not_authenticated_is_none() {
    let (client, server) = pair();
    let mut denied = [0u8; 64];
    let n = build_denied_reply(&mut denied).unwrap();
    answer_every_id(&server, OP_AUTHENTICATE, RS_FLAG_REPLY, &denied[..n]);
    let mut home = [0u8; 64];
    assert_eq!(authenticate_on(&client, b"alice", b"wrong", &mut home), None);
    let (client, server) = pair();
    answer_every_id(&server, OP_AUTHENTICATE, RS_FLAG_REPLY, &authenticated(&[b'h'; 100]));
    assert_eq!(authenticate_on(&client, b"alice", b"pw", &mut home), None, "a home longer than the buffer");
    let (client, server) = pair();
    drop(server);
    assert_eq!(authenticate_on(&client, b"alice", b"pw", &mut home), None);
    let (client, _server) = pair();
    assert_eq!(authenticate_on(&client, &[b'u'; 600], b"pw", &mut home), None, "a request too large to build");
}

/// A password's message is zeroed where it lies, whichever way the scope ends.
#[test]
fn a_scrubbed_message_is_zero_once_its_scope_ends() {
    let mut slot = core::mem::MaybeUninit::new(Scrubbed(Msg::new()));
    // SAFETY: initialised above and dropped once, in place.
    let guard = unsafe { &mut *slot.as_mut_ptr() };
    guard.0.payload_mut().fill(0xAA);
    // SAFETY: as above; the bytes are plain data, read after the drop where they lay.
    let after = unsafe {
        core::ptr::drop_in_place(slot.as_mut_ptr());
        core::slice::from_raw_parts((slot.as_ptr() as *const u8).add(libkern::IPC_HEADER_SIZE), IPC_PAYLOAD_SIZE)
    };
    assert!(after.iter().all(|&b| b == 0));
}

/// The view broker's session id comes back as the base a binding is made at.
#[test]
fn a_views_session_opens_at_its_base_and_closes() {
    let (client, server) = pair();
    let mut body = [0u8; 8];
    let n = build_session_id(&mut body, 4242).unwrap();
    answer_every_id(&server, OP_VIEWS_OPEN_SESSION, RS_FLAG_REPLY, &body[..n]);
    let mut base = [0u8; 24];
    assert_eq!(open_views_session(&client, b"alice", &mut base), Some((4242, 7)));
    assert_eq!(&base[..7], b"/s/4242");
    assert_eq!(request(&server), (OP_VIEWS_OPEN_SESSION, b"alice".to_vec()));

    let (client, server) = pair();
    answer_every_id(&server, OP_VIEWS_CLOSE_SESSION, RS_FLAG_REPLY, b"");
    close_views_session(&client, 4242);
    let (op, body) = request(&server);
    assert_eq!((op, parse_session_id(&body)), (OP_VIEWS_CLOSE_SESSION, Some(4242)));
}

/// A refusal, a reply that is not an id and a broker that has gone are all no session.
#[test]
fn a_broker_that_refuses_opens_no_session() {
    let (client, server) = pair();
    let mut id = [0u8; 8];
    let n = build_session_id(&mut id, 9).unwrap();
    answer_every_id(&server, OP_VIEWS_OPEN_SESSION, RS_FLAG_REPLY | RS_FLAG_ERROR, &id[..n]);
    let mut base = [0u8; 24];
    assert_eq!(open_views_session(&client, b"alice", &mut base), None, "an error reply is no session");
    let (client, server) = pair();
    answer_every_id(&server, OP_VIEWS_OPEN_SESSION, RS_FLAG_REPLY, b"x");
    assert_eq!(open_views_session(&client, b"alice", &mut base), None, "a body that is no id");
    let (client, server) = pair();
    drop(server);
    assert_eq!(open_views_session(&client, b"alice", &mut base), None);
    close_views_session(&client, 1);
}
