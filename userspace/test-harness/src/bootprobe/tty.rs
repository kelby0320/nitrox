//! The terminal server's backends: which it takes, and which it lets go.

use super::*;
use libos::{MapReadWrite, Memory};
use librsproto::{OP_TTY_ATTACH_BACKEND, OP_TTY_WRITE};

const TERMINAL: Rights = Rights::from_bits(libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT);

/// Send `op` on terminal `tty`, moving `handles`, and answer its reply's error, `0` for none.
/// `None` if it would not send or no reply came within 5 s; a handle not sent is closed.
fn request<'a>(tty: impl Into<Ref<'a, Channel, Only>>, op: u16, request_id: u64, handles: Carried) -> Option<i32> {
    let tty = tty.into();
    if !rs_send(tty, op, request_id, &[], handles) {
        return None;
    }
    let deadline = clock_ns() + 5_000 * MS;
    loop {
        match receive(tty, deadline) {
            Some(m) if m.request_id == request_id && !m.error => return Some(0),
            Some(m) if m.request_id == request_id => {
                return Some(librsproto::error::parse_error(&m.body).map_or(i32::MIN, |e| e.kerror));
            }
            Some(_) => continue,
            None => return None,
        }
    }
}

/// Attach a fresh channel end restricted to `rights` to terminal `tty`: the reply's error, and
/// whether the end was closed by the server. `None` if no reply came.
fn attach_restricted(tty: &Handle<Channel, Only>, rights: Rights, request_id: u64) -> Option<(i32, bool)> {
    let (ours, theirs) = channel()?;
    if !refused::restrict::narrow(&theirs, rights) {
        return None;
    }
    let answer = request(tty, OP_TTY_ATTACH_BACKEND, request_id, carry(theirs));
    let closed = answer.is_some_and(|e| e != 0) && peer_gone_by(&ours, clock_ns() + 2_000 * MS);
    answer.map(|e| (e, closed))
}

/// **A terminal refuses a backend it could not wait on, and closes it**: a memory object, or a
/// channel end without `WAIT`, fails every wait of the server's loop, so it would serve no
/// terminal again. One without `INSPECT` cannot be told from either, so it is refused too.
pub(super) fn tty_refuses_a_backend_it_cannot_wait_on_test(root: Root<'_>) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: tty backend refused FAIL: ").s(why).end();
        false
    };
    let Ok(tty) = lookup::<Channel, Only, _>(root, "/dev/tty", TERMINAL) else {
        return fail(b"no terminal");
    };
    use libkern::KError;
    let memory = Handle::<Memory, MapReadWrite>::create(PAGE as usize)
        .ok()
        .and_then(|mem| request(&tty, OP_TTY_ATTACH_BACKEND, 0x6176, carry(mem)));
    let unwaitable = attach_restricted(&tty, Rights::SEND | Rights::RECV | Rights::TRANSFER | Rights::INSPECT, 0x6177);
    let uninspectable = attach_restricted(&tty, Rights::SEND | Rights::RECV | Rights::WAIT | Rights::TRANSFER, 0x6179);
    let serving = request(&tty, OP_TTY_WRITE, 0x6178, Carried::new()) == Some(0);
    drop(tty);
    if memory != Some(KError::InvalidArgument.as_i32()) {
        return fail(b"a memory object was not refused InvalidArgument");
    }
    if unwaitable.map(|r| r.0) != Some(KError::NoAccess.as_i32()) {
        return fail(b"a channel end without WAIT was not refused NoAccess");
    }
    if uninspectable.map(|r| r.0) != Some(KError::NoAccess.as_i32()) {
        return fail(b"a channel end without INSPECT was not refused NoAccess");
    }
    if !unwaitable.is_some_and(|r| r.1) || !uninspectable.is_some_and(|r| r.1) {
        return fail(b"it kept a backend it refused");
    }
    if !serving {
        return fail(b"tty-server stopped serving");
    }
    log::write(b"boot-probe: tty-server refuses a backend it cannot wait on ok\n");
    true
}

/// **A terminal pointed at a new backend lets go of the old one**: closing it is how the old
/// backend's emulator learns its terminal has gone.
pub(super) fn tty_reattach_closes_the_old_backend_test(root: Root<'_>) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: tty re-attach FAIL: ").s(why).end();
        false
    };
    let tty = lookup::<Channel, Only, _>(root, "/dev/tty", TERMINAL);
    let (Some((a, a_far)), Some((b, b_far))) = (channel(), channel()) else {
        return fail(b"no channels");
    };
    let first = tty.as_ref().is_ok_and(|tty| request(tty, OP_TTY_ATTACH_BACKEND, 0x617b, carry(a_far)) == Some(0));
    let second =
        first && tty.as_ref().is_ok_and(|tty| request(tty, OP_TTY_ATTACH_BACKEND, 0x617a, carry(b_far)) == Some(0));
    let old_closed = second && peer_gone_by(&a, clock_ns() + 2_000 * MS);
    let new_open = second && !peer_gone_by(&b, clock_ns());
    if !second {
        return fail(b"the terminal would not take two backends");
    }
    if !old_closed {
        return fail(b"it kept the backend it was re-pointed from");
    }
    if !new_open {
        return fail(b"it closed the backend it was re-pointed to");
    }
    log::write(b"boot-probe: tty-server closes the backend a terminal leaves ok\n");
    true
}
