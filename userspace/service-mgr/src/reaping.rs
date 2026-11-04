//! What a pass reads from the control channels and the notification queue: a server's `Ready`, a
//! channel's close, and the exit codes, with the grace wait for a code the close came before.

use alloc::string::String;
use alloc::vec::Vec;

use libkern::{KError, KIND_CHILD_EXITED};
use libos::log::Line;
use libos::{Handle, Msg, Notify, Only};
use service_mgr::wake;

use crate::{Endpoint, Mgr, now_ns};

pub(crate) enum Ready {
    /// A `Meta::Ready`, and the endpoint it moved.
    Endpoint(Endpoint),
    /// A refusal, with its reason; or a message that was no `Ready`.
    Refused(String),
    Nothing,
    Closed,
}

pub(crate) fn take_ready(ctrl: &Endpoint) -> Ready {
    let mut msg = Msg::new();
    let mut sent = match ctrl.recv(&mut msg) {
        Ok(sent) => sent,
        Err(e) if e.code() == KError::PeerClosed => return Ready::Closed,
        Err(_) => return Ready::Nothing,
    };
    let decoded = librsproto::decode(msg.payload()).ok().map(|m| (m.op, m.flags, m.body.to_vec()));
    let endpoint = decoded.as_ref().and_then(|d| service_mgr::ready::kept(d.0, d.1)).and_then(|i| sent.take(i));
    let refused = |why: &[u8]| Ready::Refused(String::from_utf8_lossy(why).into_owned());
    match decoded {
        Some((op, flags, body)) if op == librsproto::OP_READY && flags & librsproto::RS_FLAG_ERROR != 0 => {
            let why = librsproto::error::parse_error(&body).map(|e| e.msg).unwrap_or(b"");
            let why: &[u8] = if why.is_empty() { b"it refused, and gave no reason" } else { why };
            refused(why)
        }
        Some((op, _, _)) if op == librsproto::OP_READY => match endpoint {
            Some(endpoint) => Ready::Endpoint(endpoint),
            None => refused(b"a Ready with no endpoint in it"),
        },
        _ => refused(b"its first message was not a Ready"),
    }
}

/// A pass's reading of the control channels, and its grace wait for exit codes, which starts when
/// the first death is found without one and lasts [`CODE_GRACE_NS`].
pub(crate) struct Reaping<'m> {
    mgr: &'m mut Mgr,
    until: Option<u64>,
}

impl<'m> Reaping<'m> {
    /// A pass over `mgr`'s channels, its grace wait not yet begun.
    pub(crate) fn new(mgr: &'m mut Mgr) -> Self {
        Reaping { mgr, until: None }
    }
}

impl wake::Pass for Reaping<'_> {
    fn look(&mut self, i: usize) -> bool {
        self.mgr.look(i)
    }

    fn more_codes(&mut self, codes: &mut Vec<i32>) -> bool {
        let until = *self.until.get_or_insert_with(|| now_ns().saturating_add(CODE_GRACE_NS));
        let more = self.mgr.wait_codes(until);
        if more {
            drain_codes(self.mgr.notif.as_ref(), codes);
        }
        more
    }
}

/// How long a service found dead waits for its exit code. **The close can come first**:
/// `sys_process_exit` closes the child's handles before it queues `ChildExited`, so a wake on
/// another CPU can find it dead with the code still to come, and log `code=unknown`.
pub(crate) const CODE_GRACE_NS: u64 = 1_000_000_000;

/// Take every `ChildExited` queued on `notif`, logging each, and append its code to `codes`.
pub(crate) fn drain_codes(notif: Option<&Handle<Notify, Only>>, codes: &mut Vec<i32>) {
    let Some(notif) = notif else { return };
    // Drained, until it answers nothing.
    while let Ok(Some(n)) = notif.recv() {
        let (kind, body) = (n.kind, n.body);
        if kind != KIND_CHILD_EXITED {
            continue;
        }
        let cpid = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        let code = i32::from_le_bytes([body[8], body[9], body[10], body[11]]);
        Line::new()
            .s(b"service-mgr: reaped pid=")
            .u(cpid as u64)
            // `.i`, not `.u`: an exit code is signed.
            .s(b" code=")
            .i(code as i64)
            .end();
        codes.push(code);
    }
}

/// Whether `ch`'s peer has gone: drain the endpoint until it answers.
///
/// `sys_channel_recv` answers `WouldBlock` when the ring is merely empty and the peer is alive,
/// `PeerClosed` when it is empty and the peer is gone; that is what makes a control channel an exit
/// discriminator (see [`Mgr::poll`]). **A drain, not a single receive**, because a receive that
/// returns `0` has *consumed* a message: a queued message would otherwise mask the close behind it.
/// A service that is not a server cannot send here — its control end is granted `RECV | WAIT` and
/// no `SEND` — but **a server can**, and this is where a `Ready` lands that came after its
/// deadline.
///
/// A message found here is reported, and **every handle it carried is closed**: a late `Ready`
/// carries the server's endpoint, which nothing binds now, and a server whose endpoint has
/// closed ends itself — its exit is then attributed here, as any other's.
pub(crate) fn channel_peer_closed(ch: &Endpoint) -> bool {
    let mut msg = Msg::new();
    loop {
        // What came closes as `sent` drops, at the end of this pass.
        let sent = match ch.recv(&mut msg) {
            Ok(sent) => sent,
            Err(e) if e.code() == KError::PeerClosed => return true,
            Err(_) => return false, // WouldBlock (alive and quiet), or an error we cannot act on
        };
        let count = sent.len();
        Line::new()
            .s(b"service-mgr: unexpected message on a control channel (dropped, and ")
            .u(count as u64)
            .s(b" handle(s) it carried closed)")
            .end();
    }
}
