//! Asking a window to close, and insisting.

use super::*;

/// How long a close request stays armed for the click that insists on it.
///
/// **Not the grace period coming back.** That timer *acted* when it expired — it destroyed a
/// window whose client had not answered. This one only forgets: when it runs out the next
/// middle-click asks again, which is the safe direction and the one a person can recover from
/// by clicking once more.
///
/// It is how long a second click is still part of the first, and the person is the clock it is
/// measured against: they click, watch for a moment, and click again when nothing happened.
/// Long enough for that; short enough that a click made after reading a question and answering
/// it is a fresh intention rather than a continuation. `check-login`'s two clicks are 63 ms
/// apart on the accelerator CI uses, so the gate has two orders of magnitude of room.
const INSIST_WINDOW_NS: u64 = 5_000_000_000;

/// Ask a window's client to close — or, if the ask is still in hand, insist.
///
/// **The second click is the insist**, which is the whole of M12 Part A's close-policy change:
/// the first middle-click asks, and if the window is still there when the person clicks again
/// they have said they meant it. Before this the answer was a two-second timer, which was safe
/// only while no client could decline — see `asked_to_close`'s declaration for why the editor's
/// confirmation dialog is exactly the client that makes a timer lose work, and for why the arm
/// expires rather than lasting the window's life.
///
/// **A clock that will not answer means nothing is ever armed**, so the taskbar can ask and
/// never insist. That is the shell's stance on `now_ns` everywhere: the failure that leaves the
/// machine working is the one where a window stays, and a window that cannot be forced shut from
/// the bar can still be closed by its own button.
///
/// A window that goes away on its own leaves the entry that named it, so the ordinary case never
/// reaches `Manage::Close` at all.
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
    // An expired entry for this window is replaced rather than joined: one window is being
    // asked about once, however many times the question has been put.
    asked.retain(|&(id, _)| id != window);
    if let Some(n) = now {
        asked.push((window, n.saturating_add(INSIST_WINDOW_NS)));
    }
}

/// **Ask a window's client to close it** — `Manage::RequestClose`, the polite half. `true` if the
/// compositor forwarded it. The taskbar's ask ([`ask_to_close`]) and a session end
/// (administration Part F.2) both come here.
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

/// Destroy a window whose client did not answer — `Manage::Close`.
///
/// Reached only from [`ask_to_close`]'s second click. `true` if the compositor took it.
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
