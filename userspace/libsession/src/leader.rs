//! Spawning a session's leader and supervising it until it exits.

use crate::{LEADER_STOP_NS, STOP_ASKED, session_env};
use core::sync::atomic::Ordering;
use libkern::{KIND_CHILD_EXITED, KIND_TERMINATE_REQUESTED, Notification, Rights, SysCaps};
use libos::{
    CanLookup, Carried, Channel, Deadline, Handle, Msg, Namespace, Notify, Only, Ref, Spawn, WaitSet, block_on, log,
};
use libstream::setup::{StageStreams, bootstrap_arg0, send_stage_setup};

/// The next notification queued on `notif`, if there is one.
fn next_notification(notif: Ref<'_, Notify, Only>) -> Option<Notification> {
    notif.recv().ok().flatten()
}

fn now_ns() -> u64 {
    libos::time::now().unwrap_or(0)
}

/// Spawn the leader `program` into `session_ns`, then block on `notif` for its `ChildExited` and
/// return its exit code. `-1` if it could not be spawned or set up.
///
/// **`syscaps` is the caller's to choose.** The serial leader gets none, **and none is what every
/// command in the session gets**: `nxsh` passes each stage whatever it holds. The graphical
/// leader gets `BIND_NAMESPACE`, because `desktop-shell` constructs a namespace per
/// application (`ui-composition-model.md` §5a, `graphical-session.md` §3).
///
/// Each of `extras` is duplicated to the leader in its own message after the setup message; `None`
/// sends an empty one, so the leader's positions hold.
pub fn run_leader<'a, M: CanLookup + 'a, M2: CanLookup + 'a>(
    root_ns: impl Into<Ref<'a, Namespace, M>>,
    session_ns: impl Into<Ref<'a, Namespace, M2>>,
    notif: impl Into<Ref<'a, Notify, Only>>,
    program: &str,
    argv_rest: &[&str],
    syscaps: SysCaps,
    extras: &[Option<Ref<'a, Channel, Only>>],
) -> i32 {
    let notif = notif.into();
    // Resolved from the *supervisor's* namespace: the session's exists to be handed to the leader.
    let mut path = alloc::string::String::from("/bin/");
    path.push_str(program);
    let Ok(mut spawn) = block_on(Spawn::path(root_ns, &path)) else {
        log::Line::new().s(b"libsession: ").s(program.as_bytes()).s(b" image not found").end();
        return -1;
    };
    // **Sized from what is sent**: sends are `NOBLOCK`, so a queue one short *drops the last
    // handle silently*.
    let depth = (1 + extras.len()) as u32;
    let Ok((setup_mgr, setup_shell)) = Handle::<Channel, Only>::pair(depth) else {
        log::write(b"libsession: setup channel FAIL\n");
        return -1;
    };
    spawn.namespace(session_ns).arg0(bootstrap_arg0(true)).syscaps(syscaps);
    // The first of four grants cannot be handed back.
    if spawn.grant(setup_shell, Rights::from_bits(u64::MAX)).is_err() {
        return -1;
    }
    // One handle: the setup channel, through which a Tier-1 stage gets `argv` and environment.
    // A refused spawn hands back the setup channel's end, and it closes with `setup_mgr`: a login
    // is retried, so a leak would repeat.
    let Ok(child) = spawn.spawn() else {
        log::Line::new().s(b"libsession: ").s(program.as_bytes()).s(b" spawn FAIL").end();
        return -1;
    };

    // **`argv_rest` tells a leader what its namespace cannot show it**: `desktop-shell` needs the
    // user's real home to scope the `/home` it binds into applications, and a binding never
    // resolves back to its base. It states a fact; the fs endpoint carries the authority.
    let mut argv_buf: alloc::vec::Vec<&str> = alloc::vec::Vec::with_capacity(1 + argv_rest.len());
    argv_buf.push(program);
    argv_buf.extend_from_slice(argv_rest);

    // No streams: a leader's I/O is what its namespace gives it, not a stream it was handed.
    let sent =
        send_stage_setup(setup_mgr.as_ref(), StageStreams::default(), None, &argv_buf, &session_env(), Deadline::NEVER);
    // **One message per extra, in order, after the setup message.** Only `handles[0]` reaches a
    // child, so the rest arrive *over* the setup channel; a leader binding resources needs the
    // endpoints themselves, as a *binding* never resolves back to one.
    for extra in extras {
        if sent.is_err() {
            continue;
        }
        send_extra(setup_mgr.as_ref(), *extra);
    }
    // The leader holds its own end.
    drop(setup_mgr);
    // **A terminate request is passed on**: a shutdown asks the supervisor, and the session is the
    // leader's to end. It has [`LEADER_STOP_NS`] to exit, and past that the supervisor goes on
    // without it — a stop is a request, and there is no forcible kill.
    let mut until = u64::MAX;
    if sent.is_err() {
        // **Stopped and waited for**: its exit, arriving later, would end the next session
        // (past the wait, `TODO(child-exit-attribution)`).
        log::write(b"libsession: setup message FAIL; asking the leader to stop\n");
        let _ = child.terminate();
        until = now_ns().saturating_add(LEADER_STOP_NS);
    } else {
        log::Line::new()
            .s(b"libsession: ")
            .s(program.as_bytes())
            .s(b" spawned into the session namespace with its environment")
            .end();
    }
    let mut set = WaitSet::<1>::new();
    // A first add cannot be refused.
    let _ = set.add(notif);
    loop {
        if !set.wait(Deadline::at(until)).is_ok_and(|s| !s.is_empty()) {
            if until != u64::MAX && now_ns() >= until {
                log::Line::new()
                    .s(b"libsession: ")
                    .s(program.as_bytes())
                    .s(b" is still running after it was asked to stop")
                    .end();
                return -1;
            }
            continue;
        }
        while let Some(Notification { kind, body }) = next_notification(notif) {
            if kind == KIND_CHILD_EXITED {
                let code = i32::from_le_bytes([body[8], body[9], body[10], body[11]]);
                return if sent.is_ok() { code } else { -1 };
            }
            if kind == KIND_TERMINATE_REQUESTED {
                STOP_ASKED.store(true, Ordering::Relaxed);
            }
            if kind == KIND_TERMINATE_REQUESTED && until == u64::MAX {
                log::Line::new().s(b"libsession: asked to stop; passing it on to ").s(program.as_bytes()).end();
                let _ = child.terminate();
                until = now_ns().saturating_add(LEADER_STOP_NS);
            }
        }
    }
}

/// Send the leader `extra` over `setup`, duplicated, **because a handle on an IPC message is
/// always a *move***: the caller reuses `extra` for every later session.
fn send_extra(setup: Ref<'_, Channel, Only>, extra: Option<Ref<'_, Channel, Only>>) {
    // **A missing endpoint sends an empty message, not nothing.** The leader reads these
    // positionally, so skipping one shifts every later endpoint up a slot: `desktop-shell`
    // would bind the profile server at `/dev/tty` and no `/bin`. Reachable: `init`'s non-fatal
    // `bind_tty_server` failure propagates a zero. A failed duplicate or send does too.
    let mut carried = Carried::new();
    if let Some(extra) = extra {
        match extra.duplicate(Rights::TRANSFER | Rights::DUPLICATE) {
            Ok(dup) => {
                let _ = carried.push(dup);
            }
            Err(_) => log::write(b"libsession: could not duplicate the leader's extra handle\n"),
        }
    }
    let count = carried.len();
    if setup.send(&Msg::new(), &mut carried).is_err() {
        log::write(b"libsession: the leader's extra handle was not delivered\n");
        if count == 1 {
            // The send failed, so the duplicate is still ours; then the empty message a missing
            // endpoint sends, in its place.
            drop(carried);
            let _ = setup.send(&Msg::new(), &mut Carried::new());
        }
    } else if count == 0 {
        // The leader cannot tell this from an endpoint that was never wanted.
        log::write(b"libsession: an endpoint the leader expects is absent (placeholder sent)\n");
    }
}

#[cfg(test)]
mod tests;
