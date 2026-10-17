//! Asking a window to close, and insisting.

use super::*;

/// How long a close request stays armed for the click that insists on it. Expiry only forgets:
/// the next middle-click asks again, which a person can recover from. `check-login`'s two clicks
/// are 63 ms apart.
const INSIST_WINDOW_NS: u64 = 5_000_000_000;

/// Ask a window's client to close, or, if the ask is still in hand, insist.
///
/// The second click is the insist: a timer would lose work once a client can decline (the
/// editor's confirmation), so the arm expires rather than lasting the window's life. A clock that
/// will not answer arms nothing: the bar can ask and never insist, and the window's own button
/// still closes it.
pub(super) fn ask_to_close(mgr: &mut ChannelTransport, window: u32, asked: &mut alloc::vec::Vec<(u32, u64)>) {
    let now = now_ns();
    let armed = asked.iter().position(|&(id, until)| id == window && now.is_some_and(|n| n < until));
    if let Some(i) = armed {
        if insist_on_close(mgr, window) {
            asked.remove(i);
        }
        return;
    }
    if !request_close(mgr, window) {
        return;
    }
    asked.retain(|&(id, _)| id != window);
    if let Some(n) = now {
        asked.push((window, n.saturating_add(INSIST_WINDOW_NS)));
    }
}

/// Ask a window's client to close it: `Manage::RequestClose`, the polite half. `true` if the
/// compositor forwarded it.
pub(super) fn request_close(mgr: &mut ChannelTransport, window: u32) -> bool {
    use librsproto::surface::{MgrWindowRef, OP_MGR_REQUEST_CLOSE};
    let mut body = [0u8; core::mem::size_of::<MgrWindowRef>()];
    if (MgrWindowRef { window, other: 0 }).write(&mut body).is_none() {
        return false;
    }
    let mut reply = [0u8; 64];
    if mgr.request(OP_MGR_REQUEST_CLOSE, &body, None, &mut reply).is_err() {
        Line::new().s(b"desktop-shell: RequestClose refused for window ").u(window as u64).end();
        return false;
    }
    Line::new().s(b"desktop-shell: asked window ").u(window as u64).s(b" to close").end();
    true
}

/// Destroy a window whose client did not answer: `Manage::Close`, reached only from
/// [`ask_to_close`]'s second click. `true` if the compositor took it.
pub(super) fn insist_on_close(mgr: &mut ChannelTransport, window: u32) -> bool {
    use librsproto::surface::{MgrWindowRef, OP_MGR_CLOSE};
    let mut body = [0u8; core::mem::size_of::<MgrWindowRef>()];
    if (MgrWindowRef { window, other: 0 }).write(&mut body).is_none() {
        return false;
    }
    let mut reply = [0u8; 64];
    if mgr.request(OP_MGR_CLOSE, &body, None, &mut reply).is_err() {
        Line::new().s(b"desktop-shell: Close refused for window ").u(window as u64).end();
        return false;
    }
    Line::new().s(b"desktop-shell: window ").u(window as u64).s(b" did not answer; closed it").end();
    true
}
