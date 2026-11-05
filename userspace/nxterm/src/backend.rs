//! The tty side of the terminal: attaching as its backend, and hosting a shell on it.
//!
//! `nxterm` resolves `/dev/tty`, hands the tty server the far end of a channel (`AttachBackend`),
//! and gives the terminal itself to the shell it spawns; the backend it keeps carries output in and
//! keystrokes out. The terminal is handed down, not resolved, as `/dev/tty` is one binding per
//! session and would reach the session's console (`docs/spec/process-spawn-args.md`).

use alloc::boxed::Box;
use libkern::{KError, Rights};
use libos::log;
use libos::{Carried, Channel, Deadline, Handle, Msg, Namespace, NsReadOnly, Only, Ref, Spawn, WaitSet, block_on};
use librsproto::server::send;
use librsproto::{OP_TTY_ATTACH_BACKEND, OP_TTY_INPUT, OP_TTY_OUTPUT, RS_FLAG_REPLY, decode};
use libstream::setup::{StageStreams, send_stage_setup};

/// The most terminal input one message carries ([`librsproto::RS_MAX_BODY`]); a paste reaches it.
const MAX_BODY: usize = librsproto::RS_MAX_BODY;

/// The terminal half this process keeps: the backend channel.
///
/// Dropping it closes the channel, which is how the terminal ends: the tty server sees
/// `PeerClosed`, finishes the terminal, and the `nxsh` on it stops. A closed tab drops a
/// `Backend`, so the close needs no call site of its own.
pub struct Backend {
    /// The channel the tty server writes output to and reads input from.
    pub channel: Handle<Channel, Only>,
    /// The server has closed its end; learned only from [`output`](Backend::output)'s receive.
    gone: bool,
    /// What [`output`](Backend::output) lends out, so holding it holds this `Backend`.
    msg: Box<Msg>,
}

/// Obtain a terminal and become its backend.
///
/// Returns `(terminal, backend)`: the terminal channel to hand the shell, and the backend this
/// process keeps. `None` if `/dev/tty` does not resolve or the server refuses.
pub fn attach(root: Ref<'_, Namespace, NsReadOnly>) -> Option<(Handle<Channel, Only>, Backend)> {
    let rights = Rights::SEND | Rights::RECV | Rights::WAIT | Rights::TRANSFER;
    let Ok(tty) = block_on(root.lookup::<Channel, Only>("/dev/tty", rights)) else {
        log::write(b"nxterm: /dev/tty did not resolve\n");
        return None;
    };
    let (near, far) = Handle::<Channel, Only>::pair(8).ok()?;
    // `far` is moved: once the server owns it, its close is how this process learns the terminal
    // ended.
    let mut carried = Carried::new();
    if carried.push(far).is_err() || send(&tty, OP_TTY_ATTACH_BACKEND, 1, 0, &[], &mut carried).is_err() {
        log::write(b"nxterm: AttachBackend did not send\n");
        return None;
    }
    // The reply is awaited: a refusal would leave the shell on a terminal whose output goes to
    // the serial console.
    let waited = {
        let mut set = WaitSet::<1>::new();
        set.add(&tty).is_ok() && set.wait(Deadline::NEVER).is_ok_and(|s| !s.is_empty())
    };
    if !waited || !recv_is_ok(&tty) {
        log::write(b"nxterm: AttachBackend refused\n");
        return None;
    }
    Some((tty, Backend { channel: near, gone: false, msg: Box::new(Msg::new()) }))
}

/// Receive one message on `ch` and report whether it is a non-error reply.
fn recv_is_ok(ch: &Handle<Channel, Only>) -> bool {
    let mut msg = Msg::new();
    if ch.recv(&mut msg).is_err() {
        return false;
    }
    matches!(decode(msg.payload()), Ok(m) if m.flags & RS_FLAG_REPLY != 0 && m.flags & librsproto::RS_FLAG_ERROR == 0)
}

impl Backend {
    /// Tell the server the user typed `bytes`, exactly what a serial keyboard would produce.
    pub fn typed(&self, bytes: &[u8]) -> bool {
        bytes.chunks(MAX_BODY).all(|part| send(&self.channel, OP_TTY_INPUT, 0, 0, part, &mut Carried::new()).is_ok())
    }

    /// Take one queued message of program output, if there is one.
    ///
    /// `None` when empty; never `Some(&[])`. This is the only receive on the backend:
    /// `sys_channel_recv` has no peek, so a separate probe would dequeue a message and the channel,
    /// no longer signalled, would never wake to retry. `PeerClosed` is recorded here for
    /// [`is_gone`](Self::is_gone).
    pub fn output(&mut self) -> Option<&[u8]> {
        if let Err(e) = self.channel.recv(&mut self.msg) {
            self.gone = e.code() == KError::PeerClosed;
            return None;
        }
        match decode(self.msg.payload()) {
            Ok(m) if m.op == OP_TTY_OUTPUT && !m.body.is_empty() => Some(m.body),
            _ => None,
        }
    }

    /// Whether the server has closed its end; reads what [`output`](Self::output) last saw.
    pub fn is_gone(&self) -> bool {
        self.gone
    }
}

/// Spawn `/bin/nxsh` on `terminal`, handing the terminal down in its setup message.
///
/// The terminal handle is **moved**, on every path: keeping a second reference would stop the
/// server seeing `PeerClosed` when the shell exits. `false` if the shell did not start.
/// `env` is forwarded from this terminal's own setup message so the shell has the same
/// `$env.HOME` a serial login's does.
pub fn spawn_shell(
    root: Ref<'_, Namespace, NsReadOnly>,
    terminal: Handle<Channel, Only>,
    env: &libstream::wire::Record,
) -> bool {
    let Ok(mut spawn) = block_on(Spawn::path(root, "/bin/nxsh")) else {
        log::write(b"nxterm: /bin/nxsh not found\n");
        return false;
    };
    let Ok((setup_ours, setup_theirs)) = Handle::<Channel, Only>::pair(8) else {
        return false;
    };
    spawn.arg0(libstream::setup::bootstrap_arg0(true));
    // Without `namespace`, the shell inherits a LOOKUP-only view of ours.
    if spawn.grant(setup_theirs, Rights::from_bits(u64::MAX)).is_err() {
        return false;
    }
    let Ok(child) = spawn.spawn() else {
        return false;
    };
    let sent =
        send_stage_setup(&setup_ours, StageStreams::default(), Some(terminal), &["nxsh"], env, Deadline::NEVER).is_ok();
    if sent {
        // Logged beside the send: this count is what the shell is handed, and the only link in
        // the chain a release image shows. `check-login`'s clipboard gate waits on this line.
        log::Line::new().s(b"nxterm: hosting a shell (env: ").u(env.values.len() as u64).s(b" fields)").end();
    }
    // Our end of the setup channel and our reference to the shell go: nothing here waits on it,
    // and its terminal closing is what ends it.
    drop((setup_ours, child));
    if !sent {
        // The terminal went with the attempt, sent or closed: it is not ours to close.
        log::write(b"nxterm: the shell's setup message did not send\n");
    }
    sent
}
