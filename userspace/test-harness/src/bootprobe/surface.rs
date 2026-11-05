//! **What a compositor session takes from a client**: an attach is memory or it is refused, and a
//! request's body is read whole. Through the channel a client holds, so the refusals are the real
//! ones.

use super::*;
use libkern::KError;
use libos::{MapReadWrite, Memory};
use librsproto::error::parse_error;
use librsproto::surface::{
    AttachBufferRequest, CreateWindowRequest, DROP_KIND_FILE, MAX_DROP_NAME, MAX_DROP_PATH, OP_ATTACH_BUFFER,
    OP_CREATE_WINDOW, OP_START_DRAG, OP_START_MOVE, Role, SURFACE_FORMAT_XRGB8888, StartDrag, StartMove,
    build_attach_buffer_request, build_create_window_request, parse_create_window_reply,
};

/// The answer to request `id`, past whatever events came first; two seconds at most.
fn answer_to<'a>(ch: impl Into<Ref<'a, Channel, Only>>, id: u64) -> Option<Received> {
    let ch = ch.into();
    let deadline = clock_ns() + 2_000 * MS;
    loop {
        let m = receive(ch, deadline)?;
        if m.request_id == id {
            return Some(m);
        }
    }
}

/// The `KError` the compositor refused an attach with, sent as request `id` with `handles`, or
/// `0` if it took it. An attach that holds is not answered, so a `StartMove` of no window follows
/// it: every session request is answered in order, and that answer arriving first says it held.
fn attach_refused(ch: &Handle<Channel, Only>, id: u64, req: &AttachBufferRequest, handles: Carried) -> Option<i32> {
    let mut attach = [0u8; 32];
    let n = build_attach_buffer_request(&mut attach, req)?;
    let mut probe = [0u8; 8];
    let p = StartMove { window: u32::MAX }.write(&mut probe)?;
    if !rs_send(ch, OP_ATTACH_BUFFER, id, &attach[..n], handles)
        || !rs_send(ch, OP_START_MOVE, id + 1, &probe[..p], Carried::new())
    {
        return None;
    }
    let deadline = clock_ns() + 2_000 * MS;
    loop {
        let m = receive(ch, deadline)?;
        if m.request_id == id {
            return Some(parse_error(&m.body).map_or(-1, |e| e.kerror));
        }
        if m.request_id == id + 1 {
            return Some(0);
        }
    }
}

pub(super) fn a_session_takes_memory_and_whole_bodies_test(root: Root<'_>) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: surface FAIL: ").s(why).end();
        false
    };
    let chan = Rights::SEND | Rights::RECV | Rights::WAIT;
    let Ok(session) = lookup::<Channel, Only, _>(root, "/dev/draw/new", chan) else {
        log::write(b"boot-probe: no compositor on this machine, so no session to ask ok\n");
        return true;
    };
    let mut body = [0u8; 64];
    let created = build_create_window_request(&mut body, &CreateWindowRequest::new(8, 8, Role::Normal))
        .filter(|&n| rs_send(&session, OP_CREATE_WINDOW, 1, &body[..n], Carried::new()))
        .and_then(|_| answer_to(&session, 1))
        .and_then(|m| parse_create_window_reply(&m.body));
    let Some(window) = created else {
        return fail(b"no window");
    };
    let at = |width, height, buffer| AttachBufferRequest {
        window,
        buffer,
        width,
        height,
        pitch: width * 4,
        format: SURFACE_FORMAT_XRGB8888,
    };

    let page = || Handle::<Memory, MapReadWrite>::create(PAGE as usize);
    let Ok(memory) = page() else {
        return fail(b"no memory object");
    };
    let invalid = Some(KError::InvalidArgument.as_i32());
    let held = attach_refused(&session, 10, &at(8, 8, 0), carry(memory)) == Some(0);

    let none = attach_refused(&session, 20, &at(1024, 1024, 0), Carried::new()) == invalid;
    let foreign = AttachBufferRequest { window: u32::MAX, ..at(8, 8, 0) };
    let not_found = KError::NotFound.as_i32();
    let not_owned = attach_refused(&session, 22, &foreign, Carried::new()) == Some(not_found);
    let too_short = page().is_ok_and(|short| attach_refused(&session, 24, &at(1024, 1024, 3), carry(short)) == invalid);

    let file = lookup::<File, MapRead, _>(
        root,
        "/system/current-generation",
        Rights::MAP_READ | Rights::TRANSFER | Rights::INSPECT,
    );
    let file_refused = file.is_ok_and(|file| attach_refused(&session, 30, &at(1, 1, 1), carry(file)) == invalid);

    let blind = page().ok().filter(|blind| refused::restrict::narrow(blind, Rights::MAP_READ | Rights::TRANSFER));
    let blind_refused = blind.is_some_and(|blind| attach_refused(&session, 40, &at(8, 8, 2), carry(blind)) == invalid);

    // A body past `SetTitle`'s length, read whole: no grab is `NotFound`, and cut it is malformed.
    let mut drag = [0u8; StartDrag::HEAD + MAX_DROP_PATH + MAX_DROP_NAME];
    let whole = StartDrag { window, kind: DROP_KIND_FILE, path_len: 0 }
        .write(&mut drag, &[b'p'; 300], b"name")
        .filter(|&n| rs_send(&session, OP_START_DRAG, 50, &drag[..n], Carried::new()))
        .and_then(|_| answer_to(&session, 50))
        .is_some_and(|m| parse_error(&m.body).is_some_and(|e| e.kerror == not_found));
    drop(session);

    let mut ok = true;
    for (held, why) in [
        (held, &b"an attach of memory was refused"[..]),
        (none, b"an attach with no memory was taken"),
        (not_owned, b"a refused attach on no window of this client's was not NotFound"),
        (too_short, b"memory short of its geometry was taken"),
        (file_refused, b"a file was taken for a buffer"),
        (blind_refused, b"a handle that cannot say what it is was taken for a buffer"),
        (whole, b"a long request's body was cut"),
    ] {
        if !held {
            ok = fail(why);
        }
    }
    if !ok {
        return false;
    }
    log::write(b"boot-probe: a session takes memory and whole bodies ok\n");
    true
}
