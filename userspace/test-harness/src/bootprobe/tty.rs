//! The terminal server's backends: which it takes, and which it lets go.

use super::*;
use libkern::{RIGHT_RECV, RIGHT_SEND, RIGHT_WAIT, SYS_HANDLE_RESTRICT};
use librsproto::{OP_TTY_ATTACH_BACKEND, OP_TTY_WRITE};

const TERMINAL: u64 = RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT;

/// Send `op` on terminal `tty`, moving `handles`, and answer its reply's error, `0` for none.
/// `None` if it would not send or no reply came within 5 s; a handle not sent is closed.
fn request(tty: u64, op: u16, request_id: u64, handles: &[u64]) -> Option<i32> {
    if !rs_send(tty, op, request_id, &[], handles) {
        handles.iter().for_each(|&h| close(h));
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
fn attach_restricted(tty: u64, rights: u64, request_id: u64) -> Option<(i32, bool)> {
    let (ours, theirs) = channel()?;
    // SAFETY: register-only syscall on a handle this process holds.
    if unsafe { syscall2(SYS_HANDLE_RESTRICT, theirs, rights) } != 0 {
        [ours, theirs].iter().for_each(|&h| close(h));
        return None;
    }
    let answer = request(tty, OP_TTY_ATTACH_BACKEND, request_id, &[theirs]);
    let closed = answer.is_some_and(|e| e != 0) && peer_gone_by(ours, clock_ns() + 2_000 * MS);
    close(ours);
    answer.map(|e| (e, closed))
}

/// **A terminal refuses a backend it could not wait on, and closes it**: a memory object, or a
/// channel end without `WAIT`, fails every wait of the server's loop, so it would serve no
/// terminal again. One without `INSPECT` cannot be told from either, so it is refused too.
pub(super) fn tty_refuses_a_backend_it_cannot_wait_on_test(root_ns: u64) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: tty backend refused FAIL: ").s(why).end();
        false
    };
    let (st, tty) = ns_lookup(root_ns, b"/dev/tty", TERMINAL);
    if st != 0 {
        return fail(b"no terminal");
    }
    use libkern::{KError, RIGHT_INSPECT};
    // SAFETY: register-only syscall.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    let memory = if mem < 0 { None } else { request(tty, OP_TTY_ATTACH_BACKEND, 0x6176, &[mem as u64]) };
    let unwaitable = attach_restricted(tty, RIGHT_SEND | RIGHT_RECV | RIGHT_TRANSFER | RIGHT_INSPECT, 0x6177);
    let uninspectable = attach_restricted(tty, RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT | RIGHT_TRANSFER, 0x6179);
    let serving = request(tty, OP_TTY_WRITE, 0x6178, &[]) == Some(0);
    close(tty);
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
    kprint(b"boot-probe: tty-server refuses a backend it cannot wait on ok\n");
    true
}

/// **A terminal pointed at a new backend lets go of the old one**: closing it is how the old
/// backend's emulator learns its terminal has gone.
pub(super) fn tty_reattach_closes_the_old_backend_test(root_ns: u64) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: tty re-attach FAIL: ").s(why).end();
        false
    };
    let (st, tty) = ns_lookup(root_ns, b"/dev/tty", TERMINAL);
    let (Some((a, a_far)), Some((b, b_far))) = (channel(), channel()) else {
        close(tty);
        return fail(b"no channels");
    };
    let first = st == 0 && request(tty, OP_TTY_ATTACH_BACKEND, 0x617b, &[a_far]) == Some(0);
    let second = first && request(tty, OP_TTY_ATTACH_BACKEND, 0x617a, &[b_far]) == Some(0);
    if !first {
        close(b_far);
    }
    let old_closed = second && peer_gone_by(a, clock_ns() + 2_000 * MS);
    let new_open = second && !peer_gone_by(b, clock_ns());
    [tty, a, b].iter().for_each(|&h| close(h));
    if !second {
        return fail(b"the terminal would not take two backends");
    }
    if !old_closed {
        return fail(b"it kept the backend it was re-pointed from");
    }
    if !new_open {
        return fail(b"it closed the backend it was re-pointed to");
    }
    kprint(b"boot-probe: tty-server closes the backend a terminal leaves ok\n");
    true
}
