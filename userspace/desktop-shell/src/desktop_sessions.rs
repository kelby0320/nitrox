//! `/dev/desktop`: its sessions served, and each told which desktop is current.

use super::*;

/// How many clients may hold a `/dev/desktop` session at once; small, as every slot costs a
/// waiter in the main wait set.
pub(super) const MAX_DESKTOP_SESSIONS: usize = 4;

/// What opens a path: one program, a constant rather than a table, until a second can open
/// something.
const OPENER: &str = "nxedit";

/// The desktop endpoint's serve end, and its open sessions.
pub(super) struct DesktopServer {
    pub(super) serve: Option<Endpoint>,
    sessions: [Option<Endpoint>; MAX_DESKTOP_SESSIONS],
}

/// Answer `op` on `ch` with `body`, no handles.
fn reply(ch: &Endpoint, op: u16, request_id: u64, body: &[u8]) {
    let _ = librsproto::server::send(ch, op, request_id, librsproto::RS_FLAG_REPLY, body, &mut Carried::new());
}

impl DesktopServer {
    pub(super) const fn new() -> Self {
        Self { serve: None, sessions: [const { None }; MAX_DESKTOP_SESSIONS] }
    }

    /// Add the serve end and every open session to `set`, in that order.
    pub(super) fn watch<'a, const N: usize>(&'a self, set: &mut WaitSet<'a, N>) {
        for ch in self.serve.iter().chain(self.sessions.iter().flatten()) {
            let _ = set.add(ch);
        }
    }

    /// Serve one request on session slot `slot`, if it is open. Returns whether the shell must
    /// redraw.
    ///
    /// The one place another process changes the desktop model, through the same operations the
    /// chords use (`switch_desktop`, the name field), so the lifecycle rule has one path.
    #[allow(clippy::too_many_arguments, reason = "the session's state lives in separate locals of the caller")]
    pub(super) fn serve_session(
        &mut self,
        slot: usize,
        mgr: Option<&mut ChannelTransport>,
        desktops: &mut alloc::vec::Vec<Desktop>,
        entries: &[WinEntry],
        current: &mut u32,
        told: &mut u32,
        next_id: &mut u32,
        launcher: &Launcher<'_>,
    ) -> bool {
        use librsproto::decode;
        use librsproto::desktop::{
            DesktopEntry, DesktopIndex, MAX_DESKTOP_NAME, MAX_LISTED, MAX_OPEN_PATH, OP_DESKTOP_LIST, OP_DESKTOP_NAME,
            OP_DESKTOP_OPEN, OP_DESKTOP_SWITCH, write_list,
        };
        let Some(ch) = self.sessions[slot].as_ref() else { return false };
        let mut msg = Msg::new();
        let _sent = match ch.recv(&mut msg) {
            Ok(sent) => sent,
            Err(e) => {
                if e.code() == KError::PeerClosed {
                    // The peer is gone: free the slot, which closes our end.
                    self.sessions[slot] = None;
                }
                return false;
            }
        };
        let (op, request_id, body) = match decode(msg.payload()) {
            Ok(m) => (m.op, m.request_id, m.body.to_vec()),
            Err(_) => return false,
        };
        let bad = |code: KError| {
            librsproto::server::reply_error(ch, op, request_id, code, b"");
            false
        };
        match op {
            OP_DESKTOP_LIST => {
                let cur = desktops.iter().position(|d| d.id == *current).map(|i| i + 1).unwrap_or(0);
                let listed: alloc::vec::Vec<DesktopEntry<'_>> = desktops
                    .iter()
                    .take(MAX_LISTED)
                    .map(|d| DesktopEntry { id: d.id, name: d.name.as_str() })
                    .collect();
                let mut out = [0u8; 1024];
                let Some(n) = write_list(&mut out, cur as u32, &listed, desktops.len() > MAX_LISTED) else {
                    // Server-side data, not a kernel fault, which `KernelError` would claim.
                    log::write(b"desktop-shell: a desktop list would not serialise\n");
                    return bad(KError::InvalidArgument);
                };
                reply(ch, op, request_id, &out[..n]);
                Line::new().s(b"desktop-shell: served List of ").u(desktops.len() as u64).end();
                false
            }
            OP_DESKTOP_SWITCH => {
                let Some(req) = DesktopIndex::read(&body) else { return bad(KError::InvalidArgument) };
                let Some(d) = (req.index as usize).checked_sub(1).and_then(|i| desktops.get(i)) else {
                    return bad(KError::NotFound);
                };
                let to = d.id;
                let Some(m) = mgr else { return bad(KError::Unsupported) };
                if !switch_desktop(m, desktops, current, told, to) {
                    return bad(KError::InvalidArgument);
                }
                normalize_desktops(desktops, entries, current, next_id);
                reply(ch, op, request_id, &[]);
                log::write(b"desktop-shell: served Switch\n");
                true
            }
            OP_DESKTOP_NAME => {
                let Some(req) = DesktopIndex::read(&body) else { return bad(KError::InvalidArgument) };
                let Ok(name) = core::str::from_utf8(&body[4..]) else {
                    return bad(KError::InvalidArgument);
                };
                if name.len() > MAX_DESKTOP_NAME {
                    return bad(KError::InvalidArgument);
                }
                let Some(d) = (req.index as usize).checked_sub(1).and_then(|i| desktops.get_mut(i)) else {
                    return bad(KError::NotFound);
                };
                d.name.clear();
                d.name.push_str(name);
                normalize_desktops(desktops, entries, current, next_id);
                reply(ch, op, request_id, &[]);
                Line::new().s(b"desktop-shell: served Name ").untrusted(name.as_bytes()).end();
                true
            }
            OP_DESKTOP_OPEN => {
                // The client names a path; the shell decides what runs. An application holds no
                // authority to spawn, so "open this" is a question, and a request naming the program
                // would be ambient authority wearing a protocol.
                let Ok(path) = core::str::from_utf8(&body) else {
                    return bad(KError::InvalidArgument);
                };
                if path.is_empty() || path.len() > MAX_OPEN_PATH || !path.starts_with('/') {
                    // Absolute only: a relative path needs a working directory this shell does not
                    // have.
                    return bad(KError::InvalidArgument);
                }
                // Nothing bounds how many times this may be asked: `Open` is the first spawn path a
                // program can drive, `MAX_DESKTOP_SESSIONS` is no bound, and the shell does not reap
                // what it launches.
                // TODO(open-amplification): bound this once the shell has that record.
                //
                // Not stat'ed here, deliberately: the answer would be about the shell's namespace,
                // not the caller's or the editor's.
                if !launcher.launch(OPENER, &[path]) {
                    return bad(KError::Unsupported);
                }
                reply(ch, op, request_id, &[]);
                Line::new().s(b"desktop-shell: served Open ").untrusted(path.as_bytes()).end();
                false
            }
            _ => bad(KError::Unsupported),
        }
    }

    /// Answer one message on the desktop endpoint's serve end: a resolve, minting a session.
    ///
    /// The shell is a resource server here but does not register itself: it binds this endpoint
    /// into the namespaces it constructs.
    pub(super) fn serve_endpoint(&mut self) {
        use librsproto::namespace::parse_resolve_request;
        use librsproto::{OP_NS_RESOLVE, decode};
        let Some(serve) = self.serve.as_ref() else { return };
        let mut msg = Msg::new();
        let Ok(_sent) = serve.recv(&mut msg) else {
            return;
        };
        let (op, request_id, bare) = match decode(msg.payload()) {
            // An empty suffix only: `/dev/desktop` is the resource itself, with no per-object
            // paths beneath.
            Ok(m) if m.op == OP_NS_RESOLVE => match parse_resolve_request(m.body) {
                Some(r) if r.suffix.is_empty() => (m.op, m.request_id, true),
                _ => (m.op, m.request_id, false),
            },
            Ok(m) => (m.op, m.request_id, false),
            Err(_) => return,
        };
        if !bare {
            librsproto::server::reply_error(serve, op, request_id, KError::NotFound, b"");
            return;
        }
        let Some(slot) = self.sessions.iter().position(Option::is_none) else {
            librsproto::server::reply_error(serve, op, request_id, KError::WouldBlock, b"");
            return;
        };
        let Ok((client_end, session_end)) = Endpoint::pair(4) else {
            librsproto::server::reply_error(serve, op, request_id, KError::KernelError, b"");
            return;
        };
        // Bound before replying, so a fast client's first request finds the slot live.
        self.sessions[slot] = Some(session_end);
        if !librsproto::server::reply_channel(serve, request_id, client_end) {
            self.sessions[slot] = None;
            return;
        }
        log::write(b"desktop-shell: /dev/desktop session opened\n");
    }
}

/// Tell the compositor which scheme the session's theme named. Sent whatever the scheme is: every
/// gate boots the light staged theme, and the compositor's `scheme light` line is the evidence it
/// arrived.
pub(super) fn tell_scheme(mgr: &mut ChannelTransport, scheme: libdraw::theme::Scheme) {
    use librsproto::surface::{MgrScheme, OP_MGR_SET_SCHEME, SCHEME_DARK, SCHEME_LIGHT};
    let value = match scheme {
        libdraw::theme::Scheme::Light => SCHEME_LIGHT,
        libdraw::theme::Scheme::Dark => SCHEME_DARK,
    };
    let mut body = [0u8; core::mem::size_of::<MgrScheme>()];
    if (MgrScheme { scheme: value }).write(&mut body).is_none() {
        log::write(b"desktop-shell: a SetScheme body would not serialise\n");
        return;
    }
    let mut reply = [0u8; 64];
    if mgr.request(OP_MGR_SET_SCHEME, &body, None, &mut reply).is_err() {
        // Not fatal: the windows are drawn, with the light scheme's shadows.
        log::write(b"desktop-shell: SetScheme was refused; shadows stay the light scheme's\n");
    }
}

/// Tell the compositor which desktop to composite, and say so: the only thing it is told about
/// desktops. `told` is the desktop it was last told, since the current desktop also moves by a
/// route that is not a switch: see [`sync_current`].
pub(super) fn switch_desktop(
    mgr: &mut ChannelTransport,
    desktops: &[Desktop],
    current: &mut u32,
    told: &mut u32,
    to: u32,
) -> bool {
    if to == *current || !desktops.iter().any(|d| d.id == to) {
        return false;
    }
    if !tell_current(mgr, to) {
        return false;
    }
    *current = to;
    *told = to;
    Line::new().s(b"desktop-shell: switched to ").untrusted(desktop_label(desktops, to).as_bytes()).end();
    true
}

fn tell_current(mgr: &mut ChannelTransport, to: u32) -> bool {
    use librsproto::surface::{MgrDesktop, OP_MGR_SET_CURRENT_DESKTOP};
    let mut body = [0u8; core::mem::size_of::<MgrDesktop>()];
    if (MgrDesktop { desktop: to }).write(&mut body).is_none() {
        log::write(b"desktop-shell: a SetCurrentDesktop body would not serialise\n");
        return false;
    }
    let mut reply = [0u8; 64];
    if mgr.request(OP_MGR_SET_CURRENT_DESKTOP, &body, None, &mut reply).is_err() {
        log::write(b"desktop-shell: SetCurrentDesktop was refused\n");
        return false;
    }
    true
}

/// Bring the compositor back into step with the shell's current desktop when something other than
/// a switch moved it.
///
/// `normalize_desktops` removes an emptied, unnamed current desktop and the shell lands on its
/// successor; left untold, the compositor stamps new windows with the removed desktop's number.
/// Called before the manager drain, after it and after the event drain: the compositor must be
/// told before a window is drained, and the desktop endpoint can normalise ahead of the drain.
pub(super) fn sync_current(mgr: &mut ChannelTransport, desktops: &[Desktop], current: u32, told: &mut u32) {
    if current == *told {
        return;
    }
    if tell_current(mgr, current) {
        *told = current;
        Line::new()
            .s(b"desktop-shell: the compositor follows to ")
            .untrusted(desktop_label(desktops, current).as_bytes())
            .end();
    }
}
