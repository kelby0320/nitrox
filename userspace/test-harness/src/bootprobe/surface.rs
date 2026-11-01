//! **What a compositor session takes from a client**: an attach is memory or it is refused, and a
//! request's body is read whole. Through the channel a client holds, so the refusals are the real
//! ones.

use super::*;
use libkern::{KError, RIGHT_INSPECT, SYS_HANDLE_RESTRICT};
use librsproto::error::parse_error;
use librsproto::surface::{
    AttachBufferRequest, CreateWindowRequest, DROP_KIND_FILE, MAX_DROP_NAME, MAX_DROP_PATH, OP_ATTACH_BUFFER,
    OP_CREATE_WINDOW, OP_START_DRAG, OP_START_MOVE, Role, SURFACE_FORMAT_XRGB8888, StartDrag, StartMove,
    build_attach_buffer_request, build_create_window_request, parse_create_window_reply,
};

/// The answer to request `id`, past whatever events came first; two seconds at most.
fn answer_to(ch: u64, id: u64) -> Option<Received> {
    let deadline = clock_ns() + 2_000 * MS;
    loop {
        let m = receive(ch, deadline)?;
        if m.request_id == id {
            return Some(m);
        }
    }
}

/// Whether the compositor refused an attach, sent as request `id` with `handles`. An attach that
/// holds is not answered, so a `StartMove` of no window follows it: every session request is
/// answered in order, and that answer arriving first says this one was taken.
fn attach_refused(ch: u64, id: u64, req: &AttachBufferRequest, handles: &[u64]) -> Option<bool> {
    let mut attach = [0u8; 32];
    let n = build_attach_buffer_request(&mut attach, req)?;
    let mut probe = [0u8; 8];
    let p = StartMove { window: u32::MAX }.write(&mut probe)?;
    if !rs_send(ch, OP_ATTACH_BUFFER, id, &attach[..n], handles)
        || !rs_send(ch, OP_START_MOVE, id + 1, &probe[..p], &[])
    {
        return None;
    }
    let deadline = clock_ns() + 2_000 * MS;
    loop {
        let m = receive(ch, deadline)?;
        if m.request_id == id {
            return Some(m.error);
        }
        if m.request_id == id + 1 {
            return Some(false);
        }
    }
}

pub(super) fn a_session_takes_memory_and_whole_bodies_test(root_ns: u64) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: surface FAIL: ").s(why).end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT;
    let (st, session) = ns_lookup(root_ns, b"/dev/draw/new", chan);
    if st != 0 || session == 0 {
        kprint(b"boot-probe: no compositor on this machine, so no session to ask ok\n");
        return true;
    }
    let mut body = [0u8; 64];
    let created = build_create_window_request(&mut body, &CreateWindowRequest::new(8, 8, Role::Normal))
        .filter(|&n| rs_send(session, OP_CREATE_WINDOW, 1, &body[..n], &[]))
        .and_then(|_| answer_to(session, 1))
        .and_then(|m| parse_create_window_reply(&m.body));
    let Some(window) = created else {
        close(session);
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

    // SAFETY: a plain anonymous object of one page.
    let memory = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if memory <= 0 {
        close(session);
        return fail(b"no memory object");
    }
    let memory = memory as u64;
    let held = attach_refused(session, 10, &at(8, 8, 0), &[memory]) == Some(false);

    let none = attach_refused(session, 20, &at(1024, 1024, 0), &[]) == Some(true);

    let (st, file) = ns_lookup(root_ns, b"/system/current-generation", RIGHT_MAP_READ | RIGHT_TRANSFER | RIGHT_INSPECT);
    let file_refused = st == 0 && file != 0 && attach_refused(session, 30, &at(1, 1, 1), &[file]) == Some(true);

    // SAFETY: a plain anonymous object of one page.
    let blind = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    // SAFETY: attenuating a handle this process holds; it cannot amplify.
    let narrowed =
        blind > 0 && unsafe { syscall2(SYS_HANDLE_RESTRICT, blind as u64, RIGHT_MAP_READ | RIGHT_TRANSFER) } == 0;
    let blind_refused = narrowed && attach_refused(session, 40, &at(8, 8, 2), &[blind as u64]) == Some(true);

    // A body past `SetTitle`'s length, read whole: no grab is `NotFound`, and cut it is malformed.
    let mut drag = [0u8; StartDrag::HEAD + MAX_DROP_PATH + MAX_DROP_NAME];
    let not_found = KError::NotFound.as_i32();
    let whole = StartDrag { window, kind: DROP_KIND_FILE, path_len: 0 }
        .write(&mut drag, &[b'p'; 300], b"name")
        .filter(|&n| rs_send(session, OP_START_DRAG, 50, &drag[..n], &[]))
        .and_then(|_| answer_to(session, 50))
        .is_some_and(|m| parse_error(&m.body).is_some_and(|e| e.kerror == not_found));
    close(session);

    let mut ok = true;
    for (held, why) in [
        (held, &b"an attach of memory was refused"[..]),
        (none, b"an attach with no memory was taken"),
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
    kprint(b"boot-probe: a session takes memory and whole bodies ok\n");
    true
}
