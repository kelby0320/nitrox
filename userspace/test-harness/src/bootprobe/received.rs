//! Handles a receiver was sent and did not ask for: it closes them, or anyone who can reach it fills
//! its handle table, and what they pin outlives their sender.

use super::*;
use libstream::channel::{IpcPort, MsgPort};

/// Send on `ch` a message of `op` carrying one end of a fresh channel, and answer whether that end
/// is closed within 2 s: whether the receiver closed what it was sent. `None` if it would not send.
fn sent_end_closes(ch: u64, op: u16) -> Option<bool> {
    let (end, peer) = channel()?;
    if !rs_send(ch, op, 1, b"", &[end]) {
        close(end);
        close(peer);
        return None;
    }
    let closed = peer_gone_by(peer, clock_ns() + 2_000 * MS);
    close(peer);
    Some(closed)
}

/// **`auth-service` closes the handles a session sends it**, which every process can reach.
pub(super) fn auth_closes_what_it_was_sent_test(root_ns: u64) -> bool {
    let (st, oracle) = ns_lookup(root_ns, b"/svc/auth", libkern::RIGHT_SEND | libkern::RIGHT_RECV);
    let closed = (st == 0 && oracle != 0).then(|| sent_end_closes(oracle, 0x7fff)).flatten();
    close(oracle);
    if closed != Some(true) {
        kprint(b"boot-probe: auth sent a handle FAIL: it kept what an oracle session sent\n");
        return false;
    }
    kprint(b"boot-probe: auth-service closes the handles a session sends it ok\n");
    true
}

/// **The logging service closes the handles a source sends it.**
pub(super) fn log_closes_what_it_was_sent_test(root_ns: u64) -> bool {
    let (st, source) = ns_lookup(root_ns, b"/log/system/boot-probe/handles", libkern::RIGHT_SEND);
    let closed = (st == 0 && source != 0).then(|| sent_end_closes(source, 1)).flatten();
    close(source);
    if closed != Some(true) {
        kprint(b"boot-probe: log sent a handle FAIL: it kept what a source sent\n");
        return false;
    }
    kprint(b"boot-probe: the logging service closes the handles a source sends it ok\n");
    true
}

/// **`libstream`'s `IpcPort` closes the handles a stream's message carries**: nxsh reads a stage's
/// output through it, and the stage chooses what comes.
pub(super) fn ipc_port_closes_what_it_was_sent_test() -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: IpcPort sent a handle FAIL: ").s(why).end();
        false
    };
    let (Some((ours, theirs)), Some((end, peer))) = (channel(), channel()) else {
        return fail(b"no channels");
    };
    if !rs_send(theirs, 1, 1, b"x", &[end]) {
        [ours, theirs, end, peer].iter().for_each(|&h| close(h));
        return fail(b"no send");
    }
    let mut out = alloc::vec::Vec::new();
    let received = IpcPort::new(ours).recv(&mut out).is_ok();
    let closed = peer_gone_by(peer, clock_ns() + 2_000 * MS);
    [ours, theirs, peer].iter().for_each(|&h| close(h));
    if !received {
        return fail(b"nothing received");
    }
    if !closed {
        return fail(b"it kept the handle the message carried");
    }
    kprint(b"boot-probe: libstream's IpcPort closes the handles a message carries ok\n");
    true
}

/// **`libkern::Received` closes, as it drops, each handle not taken from it**, and only those.
pub(super) fn received_closes_what_was_not_taken_test() -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: Received FAIL: ").s(why).end();
        false
    };
    let (Some((ours, theirs)), Some((e0, p0)), Some((e1, p1))) = (channel(), channel(), channel()) else {
        return fail(b"no channels");
    };
    if !rs_send(theirs, 1, 1, b"", &[e0, e1]) {
        [ours, theirs, e0, p0, e1, p1].iter().for_each(|&h| close(h));
        return fail(b"no send");
    }
    let mut buf = [0u8; IPC_MSG_SIZE];
    let (count, kept, again, past) = match libkern::channel_recv(ours, &mut buf) {
        Ok(mut sent) => (sent.count(), sent.take(0), sent.take(0), sent.take(2)),
        Err(_) => (0, None, None, None),
    };
    let untaken_closed = peer_gone_by(p1, clock_ns() + 2_000 * MS);
    let taken_open = !peer_gone_by(p0, clock_ns());
    let kept_was_e0 = kept.is_some_and(|h| {
        close(h);
        peer_gone_by(p0, clock_ns() + 2_000 * MS)
    });
    [ours, theirs, p0, p1].iter().for_each(|&h| close(h));
    if count != 2 || kept.is_none() || again.is_some() || past.is_some() {
        return fail(b"count, or a take, answered wrong");
    }
    if !untaken_closed {
        return fail(b"the handle not taken stayed open");
    }
    if !taken_open || !kept_was_e0 {
        return fail(b"the taken handle was closed, or was not the one sent first");
    }
    kprint(b"boot-probe: Received closes each handle not taken from it ok\n");
    true
}

/// **Every server a client opens a session on closes what the client sends it on the session**:
/// the clipboard, a terminal, the input server's consumers, `/bin`'s projection and a filesystem.
pub(super) fn sessions_close_what_they_were_sent_test(root_ns: u64) -> bool {
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT;
    let mut ok = true;
    let mut check = |what: &[u8], closed: Option<bool>| {
        if closed != Some(true) {
            Line::new().s(b"boot-probe: session sent a handle FAIL: ").s(what).s(b" kept it").end();
            ok = false;
        }
    };
    for (what, path) in [
        (&b"clipboard-server"[..], &b"/dev/clipboard"[..]),
        (b"tty-server", b"/dev/tty"),
        (b"input-server", b"/dev/input/new"),
    ] {
        let (st, session) = ns_lookup(root_ns, path, chan);
        check(what, (st == 0 && session != 0).then(|| sent_end_closes(session, 0x7fff)).flatten());
        close(session);
    }
    for (what, path) in [(&b"profile-server"[..], &b"/bin"[..]), (b"fs-server-ext4", b"/system")] {
        let mut buf = [0u8; IPC_MSG_SIZE];
        match librsproto::session::Dir::open(root_ns, path, &mut buf) {
            Ok(dir) => {
                check(what, sent_end_closes(dir.endpoint(), 0x7fff));
                dir.close();
            }
            Err(_) => check(what, None),
        }
    }
    if ok {
        kprint(b"boot-probe: every session closes the handles its client sends ok\n");
    }
    ok
}

/// **A terminal's backend: the handles an attach brings past the backend, and what the backend
/// sends, are closed.** Any session can attach a backend, so both are a client's to send.
pub(super) fn tty_backend_closes_what_it_was_sent_test(root_ns: u64) -> bool {
    use librsproto::OP_TTY_ATTACH_BACKEND;
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: tty backend sent a handle FAIL: ").s(why).end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT;
    let (st, tty) = ns_lookup(root_ns, b"/dev/tty", chan);
    let (Some((ours, theirs)), Some((extra, extra_peer))) = (channel(), channel()) else {
        close(tty);
        return fail(b"no channels");
    };
    let attached = st == 0
        && rs_send(tty, OP_TTY_ATTACH_BACKEND, 0x6175, &[], &[theirs, extra])
        && loop {
            match receive(tty, clock_ns() + 5_000 * MS) {
                Some(m) if m.request_id == 0x6175 => break !m.error,
                Some(_) => continue,
                None => break false,
            }
        };
    let extra_closed = attached && peer_gone_by(extra_peer, clock_ns() + 2_000 * MS);
    let sent_closed = attached && sent_end_closes(ours, 0x7fff) == Some(true);
    [tty, ours, extra_peer].iter().for_each(|&h| close(h));
    if !attached {
        return fail(b"the terminal would not take a backend");
    }
    if !extra_closed {
        return fail(b"it kept a handle the attach brought past the backend");
    }
    if !sent_closed {
        return fail(b"it kept what the backend sent");
    }
    kprint(b"boot-probe: tty-server closes what a backend and its attach send ok\n");
    true
}
