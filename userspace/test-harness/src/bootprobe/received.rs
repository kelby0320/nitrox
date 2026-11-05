//! Handles a receiver was sent and did not ask for: it closes them, or anyone who can reach it fills
//! its handle table, and what they pin outlives their sender.

use super::*;
use libstream::channel::{IpcPort, MsgPort};

/// Send on `ch` a message of `op` carrying one end of a fresh channel, and answer whether that end
/// is closed within 2 s: whether the receiver closed what it was sent. `None` if it would not send.
fn sent_end_closes<'a>(ch: impl Into<Ref<'a, Channel, Only>>, op: u16) -> Option<bool> {
    let (end, peer) = channel()?;
    if !rs_send(ch, op, 1, b"", carry(end)) {
        return None;
    }
    Some(peer_gone_by(&peer, clock_ns() + 2_000 * MS))
}

/// **`auth-service` closes the handles a session sends it**, which every process can reach.
pub(super) fn auth_closes_what_it_was_sent_test(root: Root<'_>) -> bool {
    let oracle = lookup::<Channel, Only, _>(root, "/svc/auth", Rights::SEND | Rights::RECV);
    let closed = oracle.ok().and_then(|oracle| sent_end_closes(&oracle, 0x7fff));
    if closed != Some(true) {
        log::write(b"boot-probe: auth sent a handle FAIL: it kept what an oracle session sent\n");
        return false;
    }
    log::write(b"boot-probe: auth-service closes the handles a session sends it ok\n");
    true
}

/// **The logging service closes the handles a source sends it.**
pub(super) fn log_closes_what_it_was_sent_test(root: Root<'_>) -> bool {
    let source = lookup::<Channel, Only, _>(root, "/log/system/boot-probe/handles", Rights::SEND);
    let closed = source.ok().and_then(|source| sent_end_closes(&source, 1));
    if closed != Some(true) {
        log::write(b"boot-probe: log sent a handle FAIL: it kept what a source sent\n");
        return false;
    }
    log::write(b"boot-probe: the logging service closes the handles a source sends it ok\n");
    true
}

/// `N` fresh channels, or none: those made before one failed are closed.
fn channels<const N: usize>() -> Option<[Pipe; N]> {
    let mut made = Vec::with_capacity(N);
    for _ in 0..N {
        made.push(channel()?);
    }
    made.try_into().ok()
}

/// **`libstream`'s `IpcPort` closes the handles a stream's message carries**: nxsh reads a stage's
/// output through it, and the stage chooses what comes.
pub(super) fn ipc_port_closes_what_it_was_sent_test() -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: IpcPort sent a handle FAIL: ").s(why).end();
        false
    };
    let Some([(ours, theirs), (end, peer)]) = channels() else {
        return fail(b"no channels");
    };
    if !rs_send(&theirs, 1, 1, b"x", carry(end)) {
        return fail(b"no send");
    }
    let mut out = Vec::new();
    let mut port = IpcPort::owning(ours);
    let received = port.recv(&mut out).is_ok();
    let closed = peer_gone_by(&peer, clock_ns() + 2_000 * MS);
    drop(port);
    if !received {
        return fail(b"nothing received");
    }
    if !closed {
        return fail(b"it kept the handle the message carried");
    }
    log::write(b"boot-probe: libstream's IpcPort closes the handles a message carries ok\n");
    true
}

/// **`libos::Carried` closes, as it drops, each handle not taken from it**, and only those.
pub(super) fn received_closes_what_was_not_taken_test() -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: Received FAIL: ").s(why).end();
        false
    };
    let Some([(ours, theirs), (e0, p0), (e1, p1)]) = channels() else {
        return fail(b"no channels");
    };
    let mut carried = Carried::new();
    let _ = carried.push(e0);
    let _ = carried.push(e1);
    if !rs_send(&theirs, 1, 1, b"", carried) {
        return fail(b"no send");
    }
    let mut msg = Msg::new();
    let (count, kept, again, past) = match ours.recv(&mut msg) {
        Ok(mut sent) => {
            let kept = sent.take::<Channel, Only>(0);
            (sent.len(), kept, sent.take::<Channel, Only>(0), sent.take::<Channel, Only>(2))
        }
        Err(_) => (0, None, None, None),
    };
    let untaken_closed = peer_gone_by(&p1, clock_ns() + 2_000 * MS);
    let taken_open = !peer_gone_by(&p0, clock_ns());
    let kept_present = kept.is_some();
    let kept_was_e0 = kept.is_some_and(|h| {
        drop(h);
        peer_gone_by(&p0, clock_ns() + 2_000 * MS)
    });
    if count != 2 || !kept_present || again.is_some() || past.is_some() {
        return fail(b"count, or a take, answered wrong");
    }
    if !untaken_closed {
        return fail(b"the handle not taken stayed open");
    }
    if !taken_open || !kept_was_e0 {
        return fail(b"the taken handle was closed, or was not the one sent first");
    }
    log::write(b"boot-probe: Received closes each handle not taken from it ok\n");
    true
}

/// **Boot-probe's `receive` closes the handles its caller does not take** from what it returns.
pub(super) fn receive_closes_what_its_caller_does_not_take_test() -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: receive FAIL: ").s(why).end();
        false
    };
    let Some([(ours, theirs), (end, peer)]) = channels() else {
        return fail(b"no channels");
    };
    if !rs_send(&theirs, 1, 1, b"x", carry(end)) {
        return fail(b"no send");
    }
    let got = receive(&ours, clock_ns() + 2_000 * MS).is_some_and(|m| m.op == 1 && m.request_id == 1);
    let closed = peer_gone_by(&peer, clock_ns() + 2_000 * MS);
    if !got {
        return fail(b"nothing received");
    }
    if !closed {
        return fail(b"the handle its caller did not take stayed open");
    }
    log::write(b"boot-probe: receive closes the handles its caller does not take ok\n");
    true
}

/// **Every server a client opens a session on closes what the client sends it on the session**:
/// the clipboard, a terminal, the input server's consumers, `/bin`'s projection and a filesystem.
pub(super) fn sessions_close_what_they_were_sent_test(root: Root<'_>) -> bool {
    let chan = Rights::SEND | Rights::RECV | Rights::WAIT;
    let mut ok = true;
    let mut check = |what: &[u8], closed: Option<bool>| {
        if closed != Some(true) {
            Line::new().s(b"boot-probe: session sent a handle FAIL: ").s(what).s(b" kept it").end();
            ok = false;
        }
    };
    for (what, path) in
        [(&b"clipboard-server"[..], "/dev/clipboard"), (b"tty-server", "/dev/tty"), (b"input-server", "/dev/input/new")]
    {
        let session = lookup::<Channel, Only, _>(root, path, chan);
        check(what, session.ok().and_then(|session| sent_end_closes(&session, 0x7fff)));
    }
    for (what, path) in [(&b"profile-server"[..], "/bin"), (b"fs-server-ext4", "/system")] {
        let mut msg = Msg::new();
        match librsproto::session::Dir::resolve(root, path, &mut msg) {
            Ok(dir) => {
                check(what, sent_end_closes(dir.channel(), 0x7fff));
                dir.close();
            }
            Err(_) => check(what, None),
        }
    }
    if ok {
        log::write(b"boot-probe: every session closes the handles its client sends ok\n");
    }
    ok
}

/// **A terminal's backend: the handles an attach brings past the backend, and what the backend
/// sends, are closed.** Any session can attach a backend, so both are a client's to send.
pub(super) fn tty_backend_closes_what_it_was_sent_test(root: Root<'_>) -> bool {
    use librsproto::OP_TTY_ATTACH_BACKEND;
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: tty backend sent a handle FAIL: ").s(why).end();
        false
    };
    let chan = Rights::SEND | Rights::RECV | Rights::WAIT;
    let tty = lookup::<Channel, Only, _>(root, "/dev/tty", chan);
    let Some([(ours, theirs), (extra, extra_peer)]) = channels() else {
        return fail(b"no channels");
    };
    let mut carried = Carried::new();
    let _ = carried.push(theirs);
    let _ = carried.push(extra);
    // A send not made, or refused, leaves both ends to close as `carried` drops.
    let sent = tty.as_ref().is_ok_and(|tty| rs_send(tty, OP_TTY_ATTACH_BACKEND, 0x6175, &[], carried));
    let attached = sent
        && tty.as_ref().is_ok_and(|tty| {
            loop {
                match receive(tty, clock_ns() + 5_000 * MS) {
                    Some(m) if m.request_id == 0x6175 => break !m.error,
                    Some(_) => continue,
                    None => break false,
                }
            }
        });
    let extra_closed = attached && peer_gone_by(&extra_peer, clock_ns() + 2_000 * MS);
    let sent_closed = attached && sent_end_closes(&ours, 0x7fff) == Some(true);
    if !attached {
        return fail(b"the terminal would not take a backend");
    }
    if !extra_closed {
        return fail(b"it kept a handle the attach brought past the backend");
    }
    if !sent_closed {
        return fail(b"it kept what the backend sent");
    }
    log::write(b"boot-probe: tty-server closes what a backend and its attach send ok\n");
    true
}

/// **A stage takes the handles its setup's fields name, and closes any past them while it runs.**
pub(super) fn setup_closes_what_its_fields_do_not_name_test(root: Root<'_>) -> bool {
    use libstream::setup::{STREAM_STDIN, SetupPayload, bootstrap_arg0};
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: setup surplus FAIL: ").s(why).end();
        false
    };
    let Some([(ctrl, ctrl_far), (stdin, stdin_far), (extra, extra_peer)]) = channels() else {
        return fail(b"no channels");
    };
    let payload =
        SetupPayload { streams: STREAM_STDIN, argv: alloc::vec!["hold".into()], ..Default::default() }.encode();
    let mut msg = Msg::new();
    let mut carried = Carried::new();
    let _ = carried.push(stdin_far);
    let _ = carried.push(extra);
    let sent = payload.is_ok_and(|p| msg.set(&p).is_ok() && ctrl.send(&msg, &mut carried).is_ok());
    if !sent {
        return fail(b"no setup sent");
    }
    if !spawn_stage(root, bootstrap_arg0(true), None, ctrl_far) {
        return fail(b"no stage");
    }
    let surplus_closed = peer_gone_by(&extra_peer, clock_ns() + 2_000 * MS);
    // The stage holds stdin until its other end closes: still running, its exit closed nothing.
    let running = !peer_gone_by(&stdin, clock_ns());
    drop((stdin, ctrl, extra_peer));
    if !running {
        return fail(b"the stage had exited before it was looked at");
    }
    if !surplus_closed {
        return fail(b"the stage kept a handle its setup did not name");
    }
    log::write(b"boot-probe: a stage closes the handles past those its setup names ok\n");
    true
}
