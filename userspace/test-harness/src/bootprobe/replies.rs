//! **A reply is its own request's**, on real channels and with the real clients. No server: each
//! message is queued before the call that reads it, which is what a failed receive leaves behind.

use super::*;
use librsproto::auth::{AUTH_RESULT_AUTHENTICATED, OP_AUTH_LIST, build_authenticate_reply, build_denied_reply};
use librsproto::session::{WireError, exchange};
use librsproto::views::OP_VIEWS_OPEN_SESSION;
use librsproto::{OP_AUTHENTICATE, OP_FILE_RMDIR, decode};

/// The rsproto message `exchange` left in `msg`, `n` bytes of it.
fn answer(msg: &Msg, n: usize) -> Option<librsproto::Message<'_>> {
    decode(msg.payload().get(..n)?).ok()
}

pub(super) fn a_reply_is_its_requests_test(root: Root<'_>) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: replies FAIL: ").s(why).end();
        false
    };
    let chan = Rights::SEND | Rights::RECV | Rights::WAIT;
    let mut msg = Msg::new();

    let Ok(oracle) = lookup::<Channel, Only, _>(root, "/svc/auth", chan) else {
        return fail(b"no oracle session");
    };
    let refused = exchange(&oracle, &mut msg, 9, OP_AUTH_LIST, &[]);
    drop(oracle);
    if !matches!(refused, Ok(n) if answer(&msg, n).is_some_and(|m| m.is_error())) {
        return fail(b"an op an oracle session does not serve was not answered with its op");
    }

    let (Some((client, server)), Some((sessions, broker))) = (channel(), channel()) else {
        return fail(b"no channels");
    };
    let queued = rs_send(&server, OP_FILE_RMDIR, 5, b"left", Carried::new())
        && rs_send(&server, OP_FILE_RMDIR, 6, b"mine", Carried::new());
    let skipped = exchange(&client, &mut msg, 6, OP_FILE_RMDIR, &[]);
    let skipped = matches!(skipped, Ok(n) if answer(&msg, n).is_some_and(|m| m.body == b"mine"));
    let other_op = rs_send(&server, OP_FILE_RMDIR + 1, 7, b"", Carried::new())
        && exchange(&client, &mut msg, 7, OP_FILE_RMDIR, &[]) == Err(WireError::Protocol);

    let (mut yes, mut no) = ([0u8; 64], [0u8; 64]);
    let yes_len = build_authenticate_reply(&mut yes, AUTH_RESULT_AUTHENTICATED, DEMO_USER, b"/home/a").unwrap_or(0);
    let no_len = build_denied_reply(&mut no).unwrap_or(0);
    let mut home = [0u8; 16];
    // The first `libsession` call in this process is id 1, and the next is 2. A first reply left
    // queued must not answer the second, as an earlier AUTHENTICATED answered a wrong password.
    let first = rs_send(&server, OP_AUTHENTICATE, 1, &yes[..yes_len], Carried::new())
        && libsession::authenticate_on(&client, DEMO_USER, b"x", &mut home).is_some();
    let second = rs_send(&server, OP_AUTHENTICATE, 1, &yes[..yes_len], Carried::new())
        && rs_send(&server, OP_AUTHENTICATE, 2, &no[..no_len], Carried::new())
        && libsession::authenticate_on(&client, DEMO_USER, b"x", &mut home).is_none();

    if !first || !second {
        return fail(b"a second authenticate took the first's reply");
    }

    let mut base = [0u8; 24];
    let long = rs_send(&broker, OP_VIEWS_OPEN_SESSION, 3, &[1; 9], Carried::new())
        && libsession::open_views_session(&sessions, DEMO_USER, &mut base).is_none();
    let whole = rs_send(&broker, OP_VIEWS_OPEN_SESSION, 4, &[1; 8], Carried::new())
        && libsession::open_views_session(&sessions, DEMO_USER, &mut base).is_some();
    drop((client, server, sessions, broker));

    if !queued || !skipped {
        return fail(b"a message that was not this request's was taken for its reply");
    }
    if !other_op {
        return fail(b"a reply with this request's id and another op was not refused");
    }
    if !long || !whole {
        return fail(b"a broker's answer past its buffer was not refused, or one that fit was");
    }
    log::write(b"boot-probe: a reply is its own request's ok\n");
    true
}
