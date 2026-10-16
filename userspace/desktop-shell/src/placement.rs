//! Placing, snapping, raising and minimizing windows.

use super::*;

/// How many window-geometry lines have been printed. See [`MAX_LOGGED_GEOMETRY`].
static mut GEOMETRY_LOGGED: u32 = 0;

/// How many window-geometry lines to print before going quiet.
///
/// The event behind them is client-driven — a client committing buffers of changing size
/// produces one each — so this is bounded the way the compositor bounds its own input
/// diagnostics, and for the same reason.
const MAX_LOGGED_GEOMETRY: u32 = 16;

/// Where a placed window's top-left goes: below the top bar, cascading so two launches do not
/// land on top of each other.
///
/// **A policy, and the shell's to have.** M6 built placement, restacking and the
/// initial-configure hold for a manager and nothing but a test client has ever supplied one —
/// this is the first process with an opinion about where a window goes. The opinion is
/// deliberately dull: below the bar, stepped. A real one is `desktop-shell.md`'s to specify.
pub(super) const CASCADE_STEP: i32 = 24;

/// Ask a window's client to adopt a geometry — `Manage::Configure`.
///
/// **A request, and the reply says only that the compositor forwarded it.** Whether the client
/// adopts the size is the client's: declining is legal and stays legal, and a window that
/// declines simply goes on committing what it has.
/// How wide the band along each screen edge that triggers a snap is, in pixels.
///
/// **Policy, and it lives here rather than in the compositor** — which is the whole point of a
/// registered table: the compositor tests the pointer against rectangles and knows nothing about
/// edges, halves or how close counts. Wide enough to reach by throwing a window at the edge,
/// narrow enough not to fire while dragging a window that happens to end up near one.
const SNAP_BAND: u32 = 24;

/// Register the eight snap zones — four edges and four corners — for `work`.
///
/// **Recomputed and re-registered wholesale**, which is why the ids are fixed and registering an
/// existing id replaces it: the zones *are* the work area, so a bar appearing or going away
/// makes every one of them wrong at once. A shell that registered them once at startup would
/// snap windows over its own bars for the rest of the session.
///
/// The targets are the policy: half the work area for an edge, a quarter for a corner. The
/// compositor never learns that — it matches a pointer against a rectangle and reports the one
/// it matched.
pub(super) fn register_snap_zones(mgr: &mut ChannelTransport, work: &MgrLayout) -> bool {
    use librsproto::surface::{MgrSnapZone, OP_MGR_REGISTER_SNAP_ZONE};
    let (x, y, w, h) = (work.work_x, work.work_y, work.work_w, work.work_h);
    let (hw, hh) = (w / 2, h / 2);
    let band = SNAP_BAND.min(w.max(1)).min(h.max(1));
    let b = band as i32;
    // `(id, trigger, target)`. Corners first: they overlap the edges, and the compositor takes
    // the **first** match — so the more specific zone has to come first, and that ordering is
    // the manager's to get right because the manager wrote the table.
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

pub(super) fn configure_window(mgr: &mut ChannelTransport, window: u32, x: i32, y: i32, w: u32, h: u32, why: &[u8]) {
    use librsproto::surface::{ConfigureEvent, OP_MGR_CONFIGURE};
    let mut body = [0u8; core::mem::size_of::<ConfigureEvent>()];
    let ev = ConfigureEvent { window, width: w, height: h, x, y };
    if ev.write(&mut body).is_none() {
        kprint(b"desktop-shell: a Configure body would not serialise\n");
        return;
    }
    // **Logged from the request's own arguments, inside the function that sends it.** The first
    // version logged the rectangle the *caller* had computed, beside the call — and a control
    // that changed what was sent while leaving the log alone passed the gate, which is the
    // defect PR #238's review found in a different assertion. One value, read once, by the code
    // that puts it on the wire.
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

/// Ask the compositor for the screen and the work area.
pub(super) fn query_layout(mgr: &mut ChannelTransport) -> Option<MgrLayout> {
    let mut reply = [0u8; 64];
    let n = mgr.request(OP_MGR_QUERY_LAYOUT, &[], None, &mut reply).ok()??;
    MgrLayout::read(&reply[..n])
}

/// What to assume when the compositor cannot be asked.
///
/// **A shell with no manager channel draws bars and launches things** — see where the channel is
/// taken — so it still needs numbers, and these are the ones it used everywhere before there was
/// an op to ask with. Named rather than inlined so the fallback is visible as a fallback.
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

/// Ask the manager channel to put `window` at `(x, y)`.
pub(super) fn place_window(mgr: &mut ChannelTransport, window: u32, x: i32, y: i32) {
    use librsproto::surface::{MgrPlace, OP_MGR_PLACE};
    // **Sized from the type, not from the byte count the spec publishes.** `write` refuses a
    // short buffer by returning `None`, so a hand-written length that stops matching after a
    // field is added turns every request of that kind into a silent no-op — the defect
    // `send_mgr_event` documents at length (PR #217 finding 5, restated by #242 finding 5).
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

/// Send one `window`+`value` manager request.
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
/// **The same defect the bars had, one layer along.** The compositor stamps every new window
/// with the desktop that is current when it is created, and `visible_on` is the single predicate
/// behind compositing, focus *and* hit-testing — so an overview opened on desktop 1 is invisible
/// and unclickable on desktop 2, while the shell still holds it and still believes it is open.
/// A person sees a menu that "stays there" when they come back and does nothing while they are
/// away; the gate saw a press land on `win=none`.
///
/// Chrome belongs to the screen rather than to one desktop, and that is as true of a launcher
/// and an overview as it is of a bar. It is *load-bearing* for the overview:
/// `desktop-shell.md` §6 says you switch desktops from inside it, which is only meaningful if it
/// survives the switch.
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
/// **`Raise` *is* the focus change** — the compositor's focus candidate is the topmost
/// focusable window, so there is no second request to make and no second piece of state to
/// disagree with the stack about who has it.
pub(super) fn raise_window(mgr: &mut ChannelTransport, e: &mut WinEntry) -> bool {
    use librsproto::surface::{MgrWindowRef, OP_MGR_RAISE, OP_MGR_SET_MINIMIZED};
    if e.minimized {
        // Restore before raising: a minimized window is not a focus candidate, so raising it
        // first would reorder the stack and leave the keyboard where it was.
        if !window_value(mgr, OP_MGR_SET_MINIMIZED, e.id, 0) {
            return false;
        }
        e.minimized = false;
    }
    // **`MgrWindowRef`, which is what `dispatch` parses this op as.** `MgrWindowValue` is
    // byte-identical here only because the second field is zero, and a shape that is right by
    // coincidence stops being right the moment either side grows (PR #242 review, finding 8).
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

/// Minimize `window`.
pub(super) fn minimize_window(mgr: &mut ChannelTransport, e: &mut WinEntry) -> bool {
    use librsproto::surface::OP_MGR_SET_MINIMIZED;
    if window_value(mgr, OP_MGR_SET_MINIMIZED, e.id, 1) {
        e.minimized = true;
        e.focused = false;
        return true;
    }
    false
}

/// Drain the manager channel and place every window it announces.
///
/// **Placing is what releases a held window.** With a manager attached the compositor holds a
/// `normal` window's first `Configure` until the manager acts, so a shell that received
/// `WindowCreated` and did nothing would leave every launched application invisible — a
/// failure that looks like the application never started.
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
    // **Four bytes more than `MAX_TITLE`, which is what a title record actually is.** A
    // `WindowTitle` body is `4 + title.len()` and the compositor stores up to `MAX_TITLE`, so a
    // 256-byte buffer truncated the longest titles by four bytes — and a cut through a
    // multi-byte character makes `title::read` return `None`, so the arm did nothing and the
    // entry kept a stale label for the life of the window, since titles are re-sent only on
    // change (PR #242 review, finding 6).
    let mut buf = [0u8; 4 + librsproto::surface::MAX_TITLE];
    // Zero timeout: drain what is queued and return. The outer `sys_wait` is what blocks.
    while let Ok(Some((op, n))) = mgr.wait_event_timeout(&mut buf, 0) {
        // **The other three events, which this loop used to discard.** They are exactly the
        // facts a window list shows, and reading them here is what keeps the shell's copy from
        // being a second stack that can disagree with the compositor's.
        match op {
            OP_MGR_WINDOW_DESTROYED => {
                // **The restore rectangle goes with the window.** Ids are never reused, so a
                // stale entry can never be *mistaken* for another window — but it is never
                // collected either, and a client looping create → maximise → destroy would grow
                // this vector for the life of the session. The window cap bounds concurrent
                // windows, not the number that have ever existed (PR #249 review, finding 3).
                if let Some(r) = MgrWindowRef::read(&buf[..n]) {
                    restore.retain(|(id, _)| *id != r.window);
                    let before = entries.len();
                    entries.retain(|e| e.id != r.window);
                    dirty |= entries.len() != before;
                }
                continue;
            }
            OP_MGR_LAYOUT_CHANGED => {
                // A strut appeared, went away or moved. Nothing is re-laid out here: what this
                // changes is where the *next* maximise puts a window, and a shell that resized
                // every maximised window under the user would be making a decision the user did
                // not ask for.
                if let Some(l) = MgrLayout::read(&buf[..n]) {
                    *layout = l;
                    // **The zones are the work area**, so a strut appearing makes all eight
                    // wrong at once — and a shell that registered them once at startup would
                    // snap windows over its own bars for the rest of the session (M9 Part F).
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
                // **Collected, not acted on here**, the way a hotkey is: acting means sending
                // manager requests and remembering a rectangle to restore to, and both belong
                // where the shell's own state lives rather than inside a drain.
                if let Some(s) = librsproto::surface::WindowState::read(&buf[..n]) {
                    states.push(s);
                }
                continue;
            }
            OP_MGR_DRAG_ENDED => {
                // **The one event a whole gesture produces** (M9 Parts E and F). Two produce it
                // — a resize, and a move released in a snap zone — and they mean the same thing
                // here: a rectangle somebody asked for. The compositor ran the drag and drew
                // the outline; it changed no geometry, because that is the manager's — so this
                // is a request and the answer is the `Configure` this shell would have sent
                // anyway. Collected rather than answered here for the reason a state request
                // is: sending manager requests belongs where the shell's own state lives.
                if let Some(c) = librsproto::surface::ConfigureEvent::read(&buf[..n]) {
                    dropped.push(c);
                }
                continue;
            }
            OP_MGR_WINDOW_FOCUS => {
                if let Some(f) = FocusEvent::read(&buf[..n]) {
                    // Focus is exclusive, so a gain clears every other entry's flag rather
                    // than trusting a matching loss to arrive.
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
                // A window's rectangle changes for more than one reason — a manager `Place`,
                // and a client committing a buffer of a different size — and this event
                // promises to report it for any of them. The overview needs the current size
                // to clamp a capture that may not scale up.
                if let Some(g) = librsproto::surface::ConfigureEvent::read(&buf[..n])
                    && let Some(e) = entries.iter_mut().find(|e| e.id == g.window)
                {
                    e.size = (g.width, g.height);
                    e.origin = (g.x, g.y);
                    // **Where it ended up, said once per move and no more than sixteen times.**
                    // A user-dragged window reports exactly one geometry event for the whole
                    // gesture — the compositor does not log one per motion, because that queue
                    // does not coalesce — so this is one line per move rather than one per
                    // frame, and it is the only place a gate can read where a drag put a window.
                    //
                    // Bounded because the *event* is client-driven as well: a client committing
                    // buffers of changing size produces one each, and the compositor caps its
                    // own diagnostics for exactly that reason (PR #248 review, finding 5).
                    // SAFETY: single-threaded process; this counter is touched only here, the
                    // way the compositor's own diagnostic counters are.
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
                // **Collected here rather than in a second drain of the same channel**, which
                // is how the first version lost every chord: `take_hotkeys` ran after this
                // function and found nothing, because this loop had already read the event and
                // fallen through its `_ => continue`. One channel wants one reader — a second
                // does not see what the first consumed, and says nothing about it.
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
        // **A dialog is placed and not listed** (M12 Part A). It is *held* for a manager
        // exactly as a `normal` is — `rsproto-surface-ops.md` says so outright — so a shell
        // that ignored one would leave every dialog waiting out the compositor's 200 ms
        // deadline and then appearing wherever the client happened to ask, which for a client
        // that cannot know where it is means the top-left corner of the screen.
        //
        // **Centred on its parent**, which is the placement the spec says a manager can work
        // out for itself: `WindowCreated` carries the parent id and the requested size, and the
        // geometry stream has been telling this shell where every window is all along. A parent
        // it does not know — a dialog on one of the shell's own windows, or on a `popup` —
        // centres on the work area instead, which is the honest answer to "over what?".
        //
        // **And it does not join `entries`.** A taskbar slot for a dialog would offer to close
        // or minimise it on its own, and a question minimised behind its window is a window
        // that cannot be closed and will not say why. Its parent's slot is the one that stands
        // for both.
        if created.role == ROLE_DIALOG && !ours.contains(&created.window) {
            let base = entries
                .iter()
                .find(|e| e.id == created.aux32)
                .map(|e| (e.origin.0, e.origin.1, e.size.0, e.size.1))
                .unwrap_or((layout.work_x, layout.work_y, layout.work_w, layout.work_h));
            let (x, y) = centre_dialog(base, (created.width, created.height), layout);
            let place = MgrPlace { window: created.window, x, y };
            // Sized from the type, not from a literal: `compositor::send_mgr_event` states that
            // rule after a hand-written `[0u8; 16]` silently dropped every widened
            // `PointerEvent`, and copying the number from the placement below would be the same
            // shape waiting for the same widening (PR #267 review, optional 7).
            let mut body = [0u8; core::mem::size_of::<MgrPlace>()];
            if place.write(&mut body).is_none() {
                continue;
            }
            let mut reply = [0u8; 64];
            if mgr.request(OP_MGR_PLACE, &body, None, &mut reply).is_err() {
                Line::new().s(b"desktop-shell: Place refused for dialog ").u(created.window as u64).end();
                continue;
            }
            // **Where it went, because a gate has no other way to find it.** `check-login`
            // presses both of a confirmation's buttons, and it aims from this origin — the
            // alternative is re-deriving this centring in the harness, which would be a second
            // copy of a policy that is the shell's to change (M10 Part E's own rule).
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
        // **Only `normal` windows, and none of the shell's own.** A bar is a `panel` and a menu
        // is a `popup`, so the role filter covers those — but an application's own popup
        // is a `popup` too, and listing one would put a menu in the taskbar. The id check is
        // belt and braces for anything the shell creates that is `normal` later.
        if created.role != ROLE_NORMAL || ours.contains(&created.window) {
            // **And it is not placed either, which the first version of this got wrong.** Every
            // window created while a manager is attached is announced to it, the shell's own
            // included — so the cascade moved the bottom bar it had just placed at the foot of
            // the screen up to `0,24`, under the top bar. Only `normal` windows are held for a
            // manager and only they want placing: a `panel` positions itself, and a `popup` is
            // placed by its creator (M6 Part C1).
            continue;
        }
        entries.push(WinEntry {
            id: created.window,
            title: alloc::string::String::new(),
            focused: false,
            minimized: false,
            // Placed below, and reported back by the geometry event that placement produces.
            origin: (0, 0),
            size: (created.width, created.height),
            // The compositor creates a window onto *its* current desktop, and the shell is what
            // set that. **The same number only because `sync_current` runs before every drain** —
            // it was not, until the desktop refresh's Part C: the shell moved its current desktop
            // off a removed one without telling the compositor, and from then on this recorded the
            // shell's number for windows the compositor had put on another.
            desktop: current,
        });
        dirty = true;
        // **Inset from the left edge, not flush against it** (M11 Part E batch 4). The cascade
        // stepped down from the bar and started at x=0, so a first window sat with its frame on
        // the screen's border — which reads as a window that has been shoved rather than placed.
        // One step in, so the offset matches the one the cascade already uses downward.
        let (x, y) = (CASCADE_STEP, *next_origin);
        // Wrapped, or a window is placed below the screen and never seen.
        *next_origin += CASCADE_STEP;
        if *next_origin > screen.height as i32 - CASCADE_STEP {
            // Back to where the cascade starts, which is one step below the bar rather than
            // against it — the same inset the first window gets.
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
        // **`x`, not a literal zero.** This said `at 0,` because the cascade always started at
        // the left edge — a value hardcoded into the line that reports it, which is the shape
        // that stays right until the thing it describes changes. Insetting the cascade made the
        // shell log one origin and place another, and `check-login` parses this line to find the
        // window it is about to click (M11 Part E batch 4).
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
/// **What to put away, and whether the set still holds, is [`ShownDesktop`]'s** — host-tested,
/// including the desktop with more windows than buttons, whose windows past the bar's end are left
/// up and must not count as something coming back (PR #314 review, blocking 1). This is the half
/// that talks to the compositor.
///
/// **One set at a time.** Showing a second desktop forgets the first's, whose windows are still
/// on that desktop's buttons — which is where any minimised window comes back from.
pub(super) fn show_desktop(
    mgr: &mut ChannelTransport,
    entries: &mut [WinEntry],
    current: u32,
    capacity: usize,
    shown: &mut Option<ShownDesktop>,
) {
    // Taken only if it is this desktop's: another desktop's set is still that desktop's to undo.
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
    // **Nothing put away is nothing to bring back**, so the button stays unlit.
    if !plan.put_away.is_empty() {
        *shown = Some(plan);
    }
}
