//! `/dev/desktop`: its sessions served, and each told which desktop is current.

use super::*;

/// Serve one request on an open `/dev/desktop` session. Returns whether the shell must redraw.
///
/// The one place another process changes the desktop model, through the same operations the
/// chords use (`switch_desktop`, the name field), so the lifecycle rule has one path.
#[allow(clippy::too_many_arguments, reason = "the session's state lives in separate locals of the caller")]
pub(super) fn serve_desktop_session(
    ch: u64,
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
    // SAFETY: valid recv out-params, sized from `IPC_HANDLE_MAX`.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ch,
            (&raw mut DS_MSG) as u64,
            (&raw mut DS_HANDLES) as u64,
            (&raw mut DS_COUNT) as u64,
        )
    };
    // Close whatever came with it: the kernel installs attached handles whether or not anything
    // looks at them, and they would pin slots in the global handle table.
    // SAFETY: closing handles the kernel just installed for us.
    unsafe {
        let n = ((&raw const DS_COUNT).read()).min(libkern::abi::IPC_HANDLE_MAX);
        #[allow(clippy::needless_range_loop, reason = "an iterator would borrow a `static mut`")]
        for i in 0..n {
            syscall1(SYS_HANDLE_CLOSE, DS_HANDLES[i]);
        }
    }
    if rr != 0 {
        if rr == KError::PeerClosed.as_i32() as i64 {
            // SAFETY: the peer is gone; free the slot and close our end.
            unsafe {
                #[allow(clippy::needless_range_loop, reason = "an iterator would borrow a `static mut`")]
                for i in 0..MAX_DESKTOP_SESSIONS {
                    if DESKTOP_SESSIONS[i] == ch {
                        DESKTOP_SESSIONS[i] = 0;
                    }
                }
                syscall1(SYS_HANDLE_CLOSE, ch);
            }
        }
        return false;
    }
    // SAFETY: bounded read-only slice over the message just received.
    let (op, request_id, body) = unsafe {
        let payload_len = u32::from_le_bytes([DS_MSG[4], DS_MSG[5], DS_MSG[6], DS_MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(((&raw const DS_MSG) as *const u8).add(24), payload_len.min(4096 - 24));
        match decode(req) {
            Ok(m) => (m.op, m.request_id, m.body.to_vec()),
            Err(_) => return false,
        }
    };
    let bad = |code: KError| {
        let mut b = [0u8; 4];
        b.copy_from_slice(&code.as_i32().to_le_bytes());
        ds_reply(ch, op, request_id, &b, 0, true);
        false
    };
    match op {
        OP_DESKTOP_LIST => {
            let cur = desktops.iter().position(|d| d.id == *current).map(|i| i + 1).unwrap_or(0);
            let listed: alloc::vec::Vec<DesktopEntry<'_>> =
                desktops.iter().take(MAX_LISTED).map(|d| DesktopEntry { id: d.id, name: d.name.as_str() }).collect();
            let mut out = [0u8; 1024];
            let Some(n) = write_list(&mut out, cur as u32, &listed, desktops.len() > MAX_LISTED) else {
                // Server-side data, not a kernel fault, which `KernelError` would claim.
                kprint(b"desktop-shell: a desktop list would not serialise\n");
                return bad(KError::InvalidArgument);
            };
            ds_reply(ch, op, request_id, &out[..n], 0, false);
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
            ds_reply(ch, op, request_id, &[], 0, false);
            kprint(b"desktop-shell: served Switch\n");
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
            ds_reply(ch, op, request_id, &[], 0, false);
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
            ds_reply(ch, op, request_id, &[], 0, false);
            Line::new().s(b"desktop-shell: served Open ").untrusted(path.as_bytes()).end();
            false
        }
        _ => bad(KError::Unsupported),
    }
}

/// What opens a path: one program, a constant rather than a table, until a second can open
/// something.
const OPENER: &str = "nxedit";

/// Answer one message on the desktop endpoint's serve end: a resolve, minting a session.
///
/// The shell is a resource server here but does not register itself: it binds this endpoint into
/// the namespaces it constructs.
pub(super) fn serve_desktop_endpoint() {
    use librsproto::namespace::{OBJECT_KIND_CHANNEL, parse_resolve_request, resolve_reply};
    use librsproto::{OP_NS_RESOLVE, decode};
    // SAFETY: reading our own serve end into valid out-params.
    let serve = unsafe { DESKTOP_SERVE };
    // SAFETY: DS_MSG, DS_HANDLES and DS_COUNT are this process's recv buffers, valid for writes and
    // touched by one thread
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            serve,
            (&raw mut DS_MSG) as u64,
            (&raw mut DS_HANDLES) as u64,
            (&raw mut DS_COUNT) as u64,
        )
    };
    if rr != 0 {
        return;
    }
    // SAFETY: bounded read-only slice over the message just received.
    let (op, request_id, bare) = unsafe {
        let payload_len = u32::from_le_bytes([DS_MSG[4], DS_MSG[5], DS_MSG[6], DS_MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(((&raw const DS_MSG) as *const u8).add(24), payload_len.min(4096 - 24));
        match decode(req) {
            // An empty suffix only: `/dev/desktop` is the resource itself, with no per-object
            // paths beneath.
            Ok(m) if m.op == OP_NS_RESOLVE => match parse_resolve_request(m.body) {
                Some(r) if r.suffix.is_empty() => (m.op, m.request_id, true),
                _ => (m.op, m.request_id, false),
            },
            Ok(m) => (m.op, m.request_id, false),
            Err(_) => return,
        }
    };
    if !bare {
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&KError::NotFound.as_i32().to_le_bytes());
        ds_reply(serve, op, request_id, &b[..4], 0, true);
        return;
    }
    // SAFETY: single-threaded scan of our own session table.
    let slot = unsafe { (0..MAX_DESKTOP_SESSIONS).find(|&i| DESKTOP_SESSIONS[i] == 0) };
    let Some(slot) = slot else {
        let mut b = [0u8; 4];
        b.copy_from_slice(&KError::WouldBlock.as_i32().to_le_bytes());
        ds_reply(serve, op, request_id, &b, 0, true);
        return;
    };
    let Some((client_end, session_end)) = make_channel() else {
        let mut b = [0u8; 4];
        b.copy_from_slice(&KError::KernelError.as_i32().to_le_bytes());
        ds_reply(serve, op, request_id, &b, 0, true);
        return;
    };
    // Bound before replying, so a fast client's first request finds the slot live.
    // SAFETY: `slot` is free.
    unsafe { DESKTOP_SESSIONS[slot] = session_end };
    let mut body = [0u8; 32];
    let n = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0).unwrap_or(0);
    if !ds_reply(serve, op, request_id, &body[..n], client_end, false) {
        // SAFETY: the transfer failed, so both ends are still ours.
        unsafe {
            DESKTOP_SESSIONS[slot] = 0;
            syscall1(SYS_HANDLE_CLOSE, session_end);
            syscall1(SYS_HANDLE_CLOSE, client_end);
        }
        return;
    }
    kprint(b"desktop-shell: /dev/desktop session opened\n");
}

/// How many clients may hold a `/dev/desktop` session at once; small, as every slot costs a
/// waiter in the main wait set.
pub(super) const MAX_DESKTOP_SESSIONS: usize = 4;

/// The desktop endpoint's serve end, and its open sessions.
pub(super) static mut DESKTOP_SERVE: u64 = 0;
/// See [`DESKTOP_SERVE`].
pub(super) static mut DESKTOP_SESSIONS: [u64; MAX_DESKTOP_SESSIONS] = [0; MAX_DESKTOP_SESSIONS];
static mut DS_MSG: [u8; 4096] = [0; 4096];
/// Sized from the ABI, not from what this server expects.
///
/// `sys_channel_recv` copies out `n * 8` bytes for the sender's stamped count, up to
/// `IPC_HANDLE_MAX`, and `/dev/desktop` is bound into every application namespace, so a client
/// attaching eight handles would smash whatever `.bss` follows a smaller array.
static mut DS_HANDLES: [u64; libkern::abi::IPC_HANDLE_MAX] = [0; libkern::abi::IPC_HANDLE_MAX];
static mut DS_COUNT: usize = 0;
static mut DS_REPLY: [u8; 4096] = [0; 4096];
static mut DS_REPLY_HANDLES: [u64; 4] = [0; 4];
static mut CH_OUT0: u64 = 0;
static mut CH_OUT1: u64 = 0;

/// Create a channel pair. `(client_end, server_end)`.
pub(super) fn make_channel() -> Option<(u64, u64)> {
    // SAFETY: CH_OUT0/CH_OUT1 are valid writable out-params.
    let cr = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut CH_OUT0) as u64, (&raw mut CH_OUT1) as u64, 4, 0) };
    if cr != 0 {
        return None;
    }
    // SAFETY: on success the kernel wrote both endpoint handles.
    Some(unsafe { ((&raw const CH_OUT0).read(), (&raw const CH_OUT1).read()) })
}

fn ds_reply(ch: u64, op: u16, request_id: u64, body: &[u8], handle: u64, err: bool) -> bool {
    use librsproto::{RS_FLAG_ERROR, RS_FLAG_REPLY};
    let flags = if err { RS_FLAG_REPLY | RS_FLAG_ERROR } else { RS_FLAG_REPLY };
    let hcount = u64::from(handle != 0);
    // SAFETY: DS_REPLY/DS_REPLY_HANDLES are valid buffers owned by this process.
    unsafe {
        let Some(rs_len) = librsproto::encode(&mut DS_REPLY[24..], op, request_id, flags, body, hcount as u16) else {
            return false;
        };
        DS_REPLY[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        DS_REPLY[8] = hcount as u8;
        DS_REPLY_HANDLES[0] = handle;
        syscall5(
            SYS_CHANNEL_SEND,
            ch,
            (&raw const DS_REPLY) as u64,
            (&raw const DS_REPLY_HANDLES) as u64,
            hcount,
            SENDMODE_NOBLOCK,
        ) == 0
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
        kprint(b"desktop-shell: a SetScheme body would not serialise\n");
        return;
    }
    let mut reply = [0u8; 64];
    if mgr.request(OP_MGR_SET_SCHEME, &body, None, &mut reply).is_err() {
        // Not fatal: the windows are drawn, with the light scheme's shadows.
        kprint(b"desktop-shell: SetScheme was refused; shadows stay the light scheme's\n");
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
        kprint(b"desktop-shell: a SetCurrentDesktop body would not serialise\n");
        return false;
    }
    let mut reply = [0u8; 64];
    if mgr.request(OP_MGR_SET_CURRENT_DESKTOP, &body, None, &mut reply).is_err() {
        kprint(b"desktop-shell: SetCurrentDesktop was refused\n");
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
