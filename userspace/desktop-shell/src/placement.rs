//! Placing, snapping, raising and minimizing windows.

use super::*;

static mut GEOMETRY_LOGGED: u32 = 0;

/// How many window-geometry lines to print before going quiet: the event is client-driven.
const MAX_LOGGED_GEOMETRY: u32 = 16;

/// Where a placed window's top-left goes: below the top bar, cascading so two launches do not
/// land on top of each other.
pub(super) const CASCADE_STEP: i32 = 24;

/// How wide the band along each screen edge that triggers a snap is, in pixels: wide enough to
/// reach by throwing a window at the edge, narrow enough not to fire on a drag that ends near
/// one. Policy kept here; the compositor knows nothing of edges.
const SNAP_BAND: u32 = 24;

/// Register the eight snap zones — four edges and four corners — for `work`.
///
/// Wholesale each time (registering an existing id replaces it): the zones are the work area, so
/// a bar appearing or going makes all of them wrong. The targets are the policy: half the work
/// area for an edge, a quarter for a corner.
pub(super) fn register_snap_zones(mgr: &mut ChannelTransport, work: &MgrLayout) -> bool {
    use librsproto::surface::{MgrSnapZone, OP_MGR_REGISTER_SNAP_ZONE};
    let (x, y, w, h) = (work.work_x, work.work_y, work.work_w, work.work_h);
    let (hw, hh) = (w / 2, h / 2);
    let band = SNAP_BAND.min(w.max(1)).min(h.max(1));
    let b = band as i32;
    // `(id, trigger, target)`. Corners first: they overlap the edges, and the compositor takes
    // the first match.
    let zones = [
        (1u32, (x, y, band, band), (x, y, hw, hh)),
        (2, (x + w as i32 - b, y, band, band), (x + hw as i32, y, w - hw, hh)),
        (3, (x, y + h as i32 - b, band, band), (x, y + hh as i32, hw, h - hh)),
        (4, (x + w as i32 - b, y + h as i32 - b, band, band), (x + hw as i32, y + hh as i32, w - hw, h - hh)),
        (5, (x, y, band, h), (x, y, hw, h)),
        (6, (x + w as i32 - b, y, band, h), (x + hw as i32, y, w - hw, h)),
        (7, (x, y, w, band), (x, y, w, hh)),
        (8, (x, y + h as i32 - b, w, band), (x, y + hh as i32, w, h - hh)),
    ];
    let mut all = true;
    for (id, t, g) in zones {
        let z = MgrSnapZone {
            id,
            trigger_x: t.0,
            trigger_y: t.1,
            trigger_w: t.2,
            trigger_h: t.3,
            target_x: g.0,
            target_y: g.1,
            target_w: g.2,
            target_h: g.3,
        };
        let mut body = [0u8; 36];
        let mut reply = [0u8; 8];
        let ok =
            z.write(&mut body).is_some() && mgr.request(OP_MGR_REGISTER_SNAP_ZONE, &body, None, &mut reply).is_ok();
        if !ok {
            Line::new().s(b"desktop-shell: snap zone ").u(id as u64).s(b" was refused").end();
            all = false;
        }
    }
    if all {
        Line::new()
            .s(b"desktop-shell: snap zones registered for work area ")
            .i(x as i64)
            .s(b",")
            .i(y as i64)
            .s(b" ")
            .u(w as u64)
            .s(b"x")
            .u(h as u64)
            .end();
    }
    all
}

/// Ask a window's client to adopt a geometry: `Manage::Configure`. The reply says only that the
/// compositor forwarded it; a client may decline and go on committing what it has.
pub(super) fn configure_window(mgr: &mut ChannelTransport, window: u32, x: i32, y: i32, w: u32, h: u32, why: &[u8]) {
    use librsproto::surface::{ConfigureEvent, OP_MGR_CONFIGURE};
    let mut body = [0u8; core::mem::size_of::<ConfigureEvent>()];
    let ev = ConfigureEvent { window, width: w, height: h, x, y };
    if ev.write(&mut body).is_none() {
        kprint(b"desktop-shell: a Configure body would not serialise\n");
        return;
    }
    // Logged from the request's own arguments, inside the function that sends it: one value, read
    // once, by the code that puts it on the wire.
    Line::new()
        .s(b"desktop-shell: ")
        .s(why)
        .s(b" window ")
        .u(window as u64)
        .s(b" to ")
        .i(x as i64)
        .s(b",")
        .i(y as i64)
        .s(b" ")
        .u(w as u64)
        .s(b"x")
        .u(h as u64)
        .end();
    let mut reply = [0u8; 64];
    if mgr.request(OP_MGR_CONFIGURE, &body, None, &mut reply).is_err() {
        Line::new().s(b"desktop-shell: Configure refused for window ").u(window as u64).end();
    }
}

pub(super) fn query_layout(mgr: &mut ChannelTransport) -> Option<MgrLayout> {
    let mut reply = [0u8; 64];
    let n = mgr.request(OP_MGR_QUERY_LAYOUT, &[], None, &mut reply).ok()??;
    MgrLayout::read(&reply[..n])
}

/// What to assume when the compositor cannot be asked: a shell with no manager channel still
/// draws bars and launches things, so it needs the numbers it used before there was an op.
pub(super) fn default_layout(screen: Screen) -> MgrLayout {
    MgrLayout {
        screen_w: screen.width,
        screen_h: screen.height,
        work_x: 0,
        work_y: BAR_H as i32,
        work_w: screen.width,
        work_h: screen.height.saturating_sub(BAR_H * 2),
    }
}

pub(super) fn place_window(mgr: &mut ChannelTransport, window: u32, x: i32, y: i32) {
    use librsproto::surface::{MgrPlace, OP_MGR_PLACE};
    let mut body = [0u8; core::mem::size_of::<MgrPlace>()];
    if (MgrPlace { window, x, y }).write(&mut body).is_none() {
        kprint(b"desktop-shell: a Place body would not serialise\n");
        return;
    }
    let mut reply = [0u8; 64];
    if mgr.request(OP_MGR_PLACE, &body, None, &mut reply).is_err() {
        Line::new().s(b"desktop-shell: Place refused for window ").u(window as u64).end();
    }
}

pub(super) fn window_value(mgr: &mut ChannelTransport, op: u16, window: u32, value: u32) -> bool {
    use librsproto::surface::MgrWindowValue;
    let mut body = [0u8; core::mem::size_of::<MgrWindowValue>()];
    if (MgrWindowValue { window, value }).write(&mut body).is_none() {
        kprint(b"desktop-shell: a window-value body would not serialise\n");
        return false;
    }
    let mut reply = [0u8; 64];
    mgr.request(op, &body, None, &mut reply).is_ok()
}

/// Make one of the shell's own popups sticky, so a desktop switch does not strand it.
///
/// The compositor stamps new windows with the current desktop and `visible_on` governs
/// compositing, focus and hit-testing, so an overview opened on desktop 1 is invisible on desktop
/// 2 while the shell believes it open.
pub(super) fn stick(mgr: &mut ChannelTransport, id: u32, what: &[u8]) {
    if !window_value(mgr, OP_MGR_SET_WINDOW_DESKTOP, id, STICKY_DESKTOP) {
        Line::new()
            .s(b"desktop-shell: ")
            .s(what)
            .s(b" could not be made sticky; it will strand on a desktop switch")
            .end();
    }
}

/// Raise `window` and give it the keyboard, restoring it first if it was minimized.
///
/// `Raise` is the focus change: the focus candidate is the topmost focusable window.
pub(super) fn raise_window(mgr: &mut ChannelTransport, e: &mut WinEntry) -> bool {
    use librsproto::surface::{MgrWindowRef, OP_MGR_RAISE, OP_MGR_SET_MINIMIZED};
    if e.minimized {
        if !window_value(mgr, OP_MGR_SET_MINIMIZED, e.id, 0) {
            return false;
        }
        e.minimized = false;
    }
    // `MgrWindowRef` is what `dispatch` parses this op as; `MgrWindowValue` matches only while
    // the second field is zero.
    let mut body = [0u8; core::mem::size_of::<MgrWindowRef>()];
    if (MgrWindowRef { window: e.id, other: 0 }).write(&mut body).is_none() {
        return false;
    }
    let mut reply = [0u8; 64];
    mgr.request(OP_MGR_RAISE, &body, None, &mut reply).is_ok()
}

/// Raise the window `id` and give it the keyboard — [`raise_window`] for a window the list does
/// not hold, such as an application's question.
pub(super) fn raise_id(mgr: &mut ChannelTransport, id: u32) -> bool {
    use librsproto::surface::{MgrWindowRef, OP_MGR_RAISE};
    let mut body = [0u8; core::mem::size_of::<MgrWindowRef>()];
    if (MgrWindowRef { window: id, other: 0 }).write(&mut body).is_none() {
        return false;
    }
    let mut reply = [0u8; 64];
    mgr.request(OP_MGR_RAISE, &body, None, &mut reply).is_ok()
}

pub(super) fn minimize_window(mgr: &mut ChannelTransport, e: &mut WinEntry) -> bool {
    use librsproto::surface::OP_MGR_SET_MINIMIZED;
    if window_value(mgr, OP_MGR_SET_MINIMIZED, e.id, 1) {
        e.minimized = true;
        e.focused = false;
        return true;
    }
    false
}

/// Drain the manager channel and place every window it announces, which releases the window the
/// compositor holds until the manager acts.
#[allow(clippy::too_many_arguments, reason = "the session's state lives in separate locals of the caller")]
pub(super) fn place_new_windows(
    mgr: &mut ChannelTransport,
    next_origin: &mut i32,
    entries: &mut alloc::vec::Vec<WinEntry>,
    last_focus: &mut u32,
    ours: &[u32],
    fired: &mut alloc::vec::Vec<u32>,
    layout: &mut MgrLayout,
    states: &mut alloc::vec::Vec<librsproto::surface::WindowState>,
    dropped: &mut alloc::vec::Vec<librsproto::surface::ConfigureEvent>,
    restore: &mut RestoreList,
    current: u32,
    screen: Screen,
) -> bool {
    use librsproto::surface::{
        FocusEvent, MgrHotkey, MgrPlace, MgrWindowCreated, MgrWindowRef, OP_MGR_DRAG_ENDED, OP_MGR_HOTKEY,
        OP_MGR_LAYOUT_CHANGED, OP_MGR_PLACE, OP_MGR_WINDOW_CREATED, OP_MGR_WINDOW_DESTROYED, OP_MGR_WINDOW_FOCUS,
        OP_MGR_WINDOW_GEOMETRY, OP_MGR_WINDOW_STATE_REQUEST, OP_MGR_WINDOW_TITLE, ROLE_DIALOG, ROLE_NORMAL,
    };
    let mut dirty = false;
    // Four bytes more than `MAX_TITLE`, as a `WindowTitle` body is `4 + title.len()`: a shorter
    // buffer truncates the longest titles and leaves the label stale.
    let mut buf = [0u8; 4 + librsproto::surface::MAX_TITLE];
    while let Ok(Some((op, n))) = mgr.wait_event_timeout(&mut buf, 0) {
        match op {
            OP_MGR_WINDOW_DESTROYED => {
                // The restore rectangle goes with the window, or a create/maximise/destroy loop
                // grows this vector.
                if let Some(r) = MgrWindowRef::read(&buf[..n]) {
                    restore.retain(|(id, _)| *id != r.window);
                    let before = entries.len();
                    entries.retain(|e| e.id != r.window);
                    dirty |= entries.len() != before;
                }
                continue;
            }
            OP_MGR_LAYOUT_CHANGED => {
                if let Some(l) = MgrLayout::read(&buf[..n]) {
                    *layout = l;
                    register_snap_zones(mgr, &l);
                    Line::new()
                        .s(b"desktop-shell: work area now ")
                        .i(l.work_x as i64)
                        .s(b",")
                        .i(l.work_y as i64)
                        .s(b" ")
                        .u(l.work_w as u64)
                        .s(b"x")
                        .u(l.work_h as u64)
                        .end();
                }
                continue;
            }
            OP_MGR_WINDOW_STATE_REQUEST => {
                if let Some(s) = librsproto::surface::WindowState::read(&buf[..n]) {
                    states.push(s);
                }
                continue;
            }
            OP_MGR_DRAG_ENDED => {
                // The one event a whole gesture produces: a resize, or a move released in a snap
                // zone, both a rectangle somebody asked for, answered with a `Configure` and
                // collected as a state request is.
                if let Some(c) = librsproto::surface::ConfigureEvent::read(&buf[..n]) {
                    dropped.push(c);
                }
                continue;
            }
            OP_MGR_WINDOW_FOCUS => {
                if let Some(f) = FocusEvent::read(&buf[..n]) {
                    let has = f.focused != 0;
                    for e in entries.iter_mut() {
                        let was = e.focused;
                        e.focused = has && e.id == f.window;
                        dirty |= was != e.focused;
                    }
                    if has {
                        *last_focus = f.window;
                    }
                }
                continue;
            }
            OP_MGR_WINDOW_GEOMETRY => {
                // A window's rectangle changes for more than one reason; this event reports all,
                // and the overview needs the size to clamp a capture.
                if let Some(g) = librsproto::surface::ConfigureEvent::read(&buf[..n])
                    && let Some(e) = entries.iter_mut().find(|e| e.id == g.window)
                {
                    e.size = (g.width, g.height);
                    e.origin = (g.x, g.y);
                    // Once per move and no more than sixteen times (a drag reports one event):
                    // the only place a gate can read where a drag put a window. Bounded because
                    // the event is client-driven.
                    // SAFETY: single-threaded process; this counter is touched only here.
                    let seen = unsafe {
                        GEOMETRY_LOGGED += 1;
                        GEOMETRY_LOGGED
                    };
                    if seen > MAX_LOGGED_GEOMETRY {
                        continue;
                    }
                    Line::new()
                        .s(b"desktop-shell: window ")
                        .u(g.window as u64)
                        .s(b" geometry ")
                        .i(g.x as i64)
                        .s(b",")
                        .i(g.y as i64)
                        .s(b" ")
                        .u(g.width as u64)
                        .s(b"x")
                        .u(g.height as u64)
                        .end();
                }
                continue;
            }
            OP_MGR_WINDOW_TITLE => {
                if let Some((id, title)) = librsproto::surface::title::read(&buf[..n])
                    && let Some(e) = entries.iter_mut().find(|e| e.id == id)
                    && e.title != title
                {
                    e.title.clear();
                    e.title.push_str(title);
                    dirty = true;
                }
                continue;
            }
            OP_MGR_HOTKEY => {
                if let Some(hk) = MgrHotkey::read(&buf[..n]) {
                    fired.push(hk.id);
                }
                continue;
            }
            OP_MGR_WINDOW_CREATED => {}
            _ => continue,
        }
        let Some(created) = MgrWindowCreated::read(&buf[..n]) else {
            continue;
        };
        // A dialog is placed and not listed. It is held for a manager as a `normal` is, so
        // ignoring it would leave it waiting out the compositor's 200 ms deadline, then at the
        // top-left. Centred on its parent, or on the work area when the parent is not known (its
        // own window, or a `popup`). It does not join `entries`: a taskbar slot would offer to
        // close or minimise it on its own, and a minimised question cannot be closed; its
        // parent's slot stands for both.
        if created.role == ROLE_DIALOG && !ours.contains(&created.window) {
            let base = entries
                .iter()
                .find(|e| e.id == created.aux32)
                .map(|e| (e.origin.0, e.origin.1, e.size.0, e.size.1))
                .unwrap_or((layout.work_x, layout.work_y, layout.work_w, layout.work_h));
            let (x, y) = centre_dialog(base, (created.width, created.height), layout);
            let place = MgrPlace { window: created.window, x, y };
            let mut body = [0u8; core::mem::size_of::<MgrPlace>()];
            if place.write(&mut body).is_none() {
                continue;
            }
            let mut reply = [0u8; 64];
            if mgr.request(OP_MGR_PLACE, &body, None, &mut reply).is_err() {
                Line::new().s(b"desktop-shell: Place refused for dialog ").u(created.window as u64).end();
                continue;
            }
            // Where it went, because `check-login` presses a confirmation's buttons aiming from
            // this origin, rather than re-deriving a centring the shell may change.
            Line::new()
                .s(b"desktop-shell: placed dialog ")
                .u(created.window as u64)
                .s(b" of window ")
                .u(created.aux32 as u64)
                .s(b" at ")
                .i(x as i64)
                .s(b",")
                .i(y as i64)
                .s(b" ")
                .u(created.width as u64)
                .s(b"x")
                .u(created.height as u64)
                .end();
            continue;
        }
        // Only `normal` windows, and none of the shell's own: an application's popup is a `popup`
        // too and would put a menu in the taskbar.
        if created.role != ROLE_NORMAL || ours.contains(&created.window) {
            // Nor placed: every window is announced to the manager, the shell's own included, and
            // cascading the bottom bar moved it under the top bar.
            continue;
        }
        entries.push(WinEntry {
            id: created.window,
            title: alloc::string::String::new(),
            focused: false,
            minimized: false,
            origin: (0, 0),
            size: (created.width, created.height),
            desktop: current,
        });
        dirty = true;
        let (x, y) = (CASCADE_STEP, *next_origin);
        *next_origin += CASCADE_STEP;
        if *next_origin > screen.height as i32 - CASCADE_STEP {
            *next_origin = BAR_H as i32 + CASCADE_STEP;
        }
        let place = MgrPlace { window: created.window, x, y };
        let mut body = [0u8; 12];
        if place.write(&mut body).is_none() {
            continue;
        }
        let mut reply = [0u8; 64];
        if mgr.request(OP_MGR_PLACE, &body, None, &mut reply).is_err() {
            Line::new().s(b"desktop-shell: Place refused for window ").u(created.window as u64).end();
            continue;
        }
        // `x`, not a literal zero: `check-login` parses this line to find the window it clicks,
        // so it must report the origin actually placed.
        Line::new()
            .s(b"desktop-shell: placed window ")
            .u(created.window as u64)
            .s(b" at ")
            .i(x as i64)
            .s(b",")
            .i(y as i64)
            .end();
    }
    dirty
}

/// Show-desktop: put away every window on `current` the bar has a button for, or bring back what
/// the last press put away.
///
/// [`ShownDesktop`] decides what and whether the set holds; this half talks to the compositor.
/// One set at a time: showing a second desktop forgets the first's.
pub(super) fn show_desktop(
    mgr: &mut ChannelTransport,
    entries: &mut [WinEntry],
    current: u32,
    capacity: usize,
    shown: &mut Option<ShownDesktop>,
) {
    if let Some(s) = shown.take_if(|s| s.desktop == current) {
        let mut back = 0u64;
        for id in s.put_away {
            if let Some(e) = entries.iter_mut().find(|e| e.id == id && e.minimized)
                && raise_window(mgr, e)
            {
                back += 1;
            }
        }
        Line::new().s(b"desktop-shell: restored ").u(back).s(b" window(s)").end();
        return;
    }
    let Some(mut plan) = ShownDesktop::plan(&windows_of(entries), current, capacity) else {
        kprint(b"desktop-shell: showing the desktop, minimised 0 window(s)\n");
        return;
    };
    // **A refusal is a window left up**, like one past the bar's end: it was never put away, so
    // its being up is not something coming back.
    let mut refused = alloc::vec::Vec::new();
    plan.put_away.retain(|id| {
        let ok = entries.iter_mut().find(|e| e.id == *id).is_some_and(|e| minimize_window(mgr, e));
        if !ok {
            refused.push(*id);
        }
        ok
    });
    plan.left_up.extend(refused);
    Line::new()
        .s(b"desktop-shell: showing the desktop, minimised ")
        .u(plan.put_away.len() as u64)
        .s(b" window(s)")
        .end();
    if !plan.put_away.is_empty() {
        *shown = Some(plan);
    }
}
