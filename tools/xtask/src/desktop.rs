use crate::*;

/// Read `<id> at <x>,<y> <w>x<h>` — the tail of `nxfiles`' menu-popup line.
pub(crate) fn parse_menu_popup(tail: &str) -> Option<(u32, i32, i32, u32, u32)> {
    let (id, rest) = tail.trim().split_once(" at ")?;
    let (origin, size) = rest.split_once(' ')?;
    let (x, y) = origin.split_once(',')?;
    let (w, h) = size.trim().split_once('x')?;
    Some((id.parse().ok()?, x.parse().ok()?, y.parse().ok()?, w.parse().ok()?, h.parse().ok()?))
}

/// Read `<id> of window <parent> at <x>,<y> <w>x<h>` — the tail of the shell's dialog placement.
///
/// A line of its own rather than the `placed window` one every other placement uses, because a
/// dialog is placed by a different rule: centred on its parent rather than stepped along the
/// cascade, so the line names the parent it was centred on.
pub(crate) fn parse_dialog_placement(tail: &str) -> Option<(u32, u32, i32, i32, u32, u32)> {
    let (id, rest) = tail.trim().split_once(" of window ")?;
    let (parent, rest) = rest.split_once(" at ")?;
    let (origin, size) = rest.split_once(' ')?;
    let (x, y) = origin.split_once(',')?;
    let (w, h) = size.trim().split_once('x')?;
    Some((id.parse().ok()?, parent.parse().ok()?, x.parse().ok()?, y.parse().ok()?, w.parse().ok()?, h.parse().ok()?))
}

/// Which slot of the taskbar `id` occupies, from the shell's own window-list line.
///
/// **Read rather than counted.** The bar shows the windows on the *current* desktop, in the
/// order the shell holds them, and a gate that kept its own tally would drift the first time a
/// window was closed or moved between desktops — which by this point in `check-login` has
/// happened several times. The line is `… [20:nxfiles] [21:notes.txt*]`, so the slot is the
/// position of the group whose id matches.
pub(crate) fn taskbar_slot(list: &str, id: u32) -> Option<usize> {
    list.split('[').skip(1).position(|group| group.split_once(':').is_some_and(|(n, _)| n.trim() == id.to_string()))
}

/// Type `text` into the Applications menu's filter, one character at a time, waiting for each.
///
/// **A receipt per character, because injection is relative and unacknowledged.** The menu's
/// filter had no line of its own until M12 Part A — the shell's source said the receipt was
/// "limited to renaming so the launcher's typing stays quiet" — so a burst of six keys followed
/// immediately by a click on a row was six chances to lose a keystroke and no way to tell.
/// The list would then be showing something else, the click would land on nothing, and the gate
/// would fail three steps later at whatever depended on the launch. It did, intermittently, in
/// CI and locally (PR #267).
///
/// The count is read and not asserted: what `/bin` holds is the image's business, and a gate
/// that pinned the number of matches would fail the day a program is added. What is asserted is
/// that the keystroke arrived at all.
pub(crate) fn type_into_menu(qmp: &mut Qmp, session: &mut Session, text: &str) -> R<()> {
    for c in text.chars() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        session.expect("desktop-shell: applications menu listing ")?;
    }
    Ok(())
}

/// Middle-click at `(x, y)`, having first walked the pointer there and checked it arrived.
///
/// **The position is verified with a left click before the middle one**, because the compositor
/// logs where a press landed and nothing else here can say where the pointer is. On a
/// window-list entry that left click is a *gesture in its own right* — it raises the window, or
/// minimises it if it already had the focus — so a caller must be able to live with either. The
/// close request that follows does not care which happened.
pub(crate) fn middle_click_at(qmp: &mut Qmp, session: &mut Session, x: i32, y: i32) -> R<()> {
    click_at(qmp, session, x, y)?;
    qmp.send_button("middle", true)?;
    qmp.send_button("middle", false)?;
    // As `click_at`: the press's receipt, then a flush, so the release cannot be left held.
    expect_after_pointer(qmp, session, &format!("compositor: press at x={x} y={y}"))?;
    qmp.flush()
}

/// Every `lines N->M, E evicted` `nxterm` reported, one per resize it accepted.
pub(crate) fn transcript_reflows(transcript: &str) -> Vec<(u32, u32, u32)> {
    transcript
        .lines()
        .filter_map(|l| l.split(", lines ").nth(1))
        .filter_map(|tail| {
            let (counts, rest) = tail.trim().split_once(", ")?;
            let (a, b) = counts.split_once("->")?;
            let e: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            Some((a.parse().ok()?, b.parse().ok()?, e.parse().ok()?))
        })
        .collect()
}

/// The largest grid `nxterm` reported reaching, in cells, from `nxterm: resized to WxH, grid CxR`.
///
/// **The largest rather than the last**, because a terminal is resized more than once in a run
/// and the interesting one is the maximise. `None` if it never reported a resize at all, which
/// is what a client that declines every `Configure` looks like.
pub(crate) fn transcript_grid(transcript: &str) -> Option<(u32, u32)> {
    transcript
        .lines()
        .filter_map(|l| l.split(", grid ").nth(1))
        .filter_map(|tail| {
            let t: String = tail.trim().chars().take_while(|c| c.is_ascii_digit() || *c == 'x').collect();
            let (c, r) = t.split_once('x')?;
            Some((c.parse().ok()?, r.parse().ok()?))
        })
        .max()
}

/// Read `<x>,<y> <w>x<h>` — the tail of the shell's window-geometry line. Returns `(w, h)`.
pub(crate) fn parse_geometry(rest: &str) -> Option<(u32, u32)> {
    let mut it = rest.split_whitespace();
    let _origin = it.next()?;
    let (w, h) = it.next()?.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// Read `<x>,<y> <w>x<h> of <sw>x<sh>` — the tail of the shell's work-area line.
///
/// Returns `(x, y, w, h, screen_w, screen_h)`.
pub(crate) fn parse_work_area(rest: &str) -> Option<(i32, i32, u32, u32, u32, u32)> {
    let mut it = rest.split_whitespace();
    let (x, y) = it.next()?.split_once(',')?;
    let (w, h) = it.next()?.split_once('x')?;
    if it.next()? != "of" {
        return None;
    }
    let (sw, sh) = it.next()?.split_once('x')?;
    Some((x.parse().ok()?, y.parse().ok()?, w.parse().ok()?, h.parse().ok()?, sw.parse().ok()?, sh.parse().ok()?))
}

/// Read `<id> at 0,<y>` — the tail of the shell's placement line. Returns `(id, y)`.
pub(crate) fn parse_placement(rest: &str) -> Option<(u32, i32, i32)> {
    let mut it = rest.split_whitespace();
    let id = it.next()?.parse().ok()?;
    if it.next()? != "at" {
        return None;
    }
    // **`x` as well as `y`, since M11 Part E batch 4.** It was discarded because the cascade
    // always started at the left edge, so every button this gate aims at could be measured from
    // the window's width alone. Insetting the cascade moved every one of them, and the gate went
    // on clicking as though the window began at zero — landing on minimise where it meant
    // maximise. A gate that assumes an origin is a gate that stops working when something moves.
    let (x, y) = it.next()?.split_once(',')?;
    Some((id, x.parse().ok()?, y.parse().ok()?))
}

/// Read `<id> at <x>,<y> <w>x<h>` — the tail of `nxterm`'s menu-popup line.
///
/// Hand-parsed for the same reason the QMP reply is: xtask carries no regex dependency, and the
/// shape here is fixed by the one line that emits it.
pub(crate) fn parse_popup_line(rest: &str) -> Option<(u32, i32, i32, u32, u32)> {
    let mut it = rest.split_whitespace();
    let id = it.next()?.parse().ok()?;
    if it.next()? != "at" {
        return None;
    }
    let (x, y) = it.next()?.split_once(',')?;
    let (w, h) = it.next()?.split_once('x')?;
    Some((id, x.parse().ok()?, y.parse().ok()?, w.parse().ok()?, h.parse().ok()?))
}

/// Read the tail of `desktop-shell: overview open, window <id> showing <k> of <n> desktops`.
///
/// Returns `n`, the number of cards. **The count is the guest's, the positions are the gate's**
/// (desktop refresh, Part E): the block of cards is centred, so every card's place depends on how
/// many there are, and how many desktops exist is a fact about the session rather than about the
/// layout. A gate that also took the *positions* from the shell could agree with a shell that had
/// stopped drawing where it says, which is what M11 decision 2 is about.
pub(crate) fn parse_overview_line(rest: &str) -> Option<i32> {
    let (_, tail) = rest.split_once(" of ")?;
    tail.split_whitespace().next()?.parse().ok()
}

/// Read `desktop-shell: window N geometry X,Y WxH` — the shell's own report of where a window is.
///
/// **Asked rather than computed**, because the alternative is re-implementing the shell's
/// placement cascade in the harness and having it drift the first time the shell's policy
/// changes. The gate needs real coordinates to press on a row and to grab a title bar, and the
/// shell prints them for every window it places.
pub(crate) fn parse_geometry_line(rest: &str) -> Option<(u32, i32, i32, u32, u32)> {
    let mut it = rest.split_whitespace();
    let id = it.next()?.parse().ok()?;
    if it.next()? != "geometry" {
        return None;
    }
    let (x, y) = it.next()?.split_once(',')?;
    let (w, h) = it.next()?.split_once('x')?;
    Some((id, x.parse().ok()?, y.parse().ok()?, w.parse().ok()?, h.parse().ok()?))
}

/// Wait for the next window-geometry line and read it.
pub(crate) fn next_geometry(session: &mut Session) -> R<(u32, i32, i32, u32, u32)> {
    session.expect("desktop-shell: window ")?;
    let line = session.rest_of_line()?;
    parse_geometry_line(&line).ok_or_else(|| format!("could not read a window geometry from {line:?}").into())
}

/// Pin the pointer to the bottom-right corner, then walk it to `(x, y)`.
///
/// **Relative injection, so the pointer must be somewhere known first.** A PS/2 packet carries a
/// 9-bit signed delta, so one huge motion is a different movement rather than a big one — the
/// steps are bounded, and the corner is reached by over-driving into the clamp.
///
/// **It records where it left the pointer**, since the desktop refresh's Part C. Every caller but
/// two did that by hand on the next line, and the two that did not — both in `shot` — walked their
/// next move from a stale position into the bottom-right corner, which passed only while that
/// corner happened to be the desktop indicator's. A believed position, not a confirmed one:
/// `click_at` still forgets it when a press does not land where it was aimed.
pub(crate) fn move_pointer_to(qmp: &mut Qmp, x: i32, y: i32) -> R<()> {
    // **Pin only when the position is unknown.** The pin is twenty over-driven motions, and
    // repeating it before every click is what floods the guest's input ring.
    let from = match qmp.pointer {
        Some(p) => p,
        None => {
            let screen = qmp.screen.ok_or(
                "move_pointer_to: this gate never said what size its screen is, so the corner a \
                 pin lands in is unknown",
            )?;
            let corner = screen.corner();
            for _ in 0..screen.pin_motions() {
                qmp.send_motion(100, 100)?; // pin to the bottom-right corner
            }
            // **Let the pin drain before walking, then flush what QEMU held of it.** The pin is
            // over-driven — twenty motions to cross thirteen hundred pixels — so any of it that
            // arrives late changes nothing *while the pointer is still in the corner*. But QEMU's
            // PS/2 queue is sixteen bytes and a packet that will not fit is **held**, not dropped:
            // its motion stays in the device and is added to the next injected event
            // (`expect_after_pointer` has the source). Walked straight after, the pin's held
            // over-drive rides on the walk's first step and cancels it.
            //
            // Seen in CI on 2026-08-31: `check-terminal` aimed at (397, 295) and the press
            // landed at (495, 351), exactly one step of (-98, -56) short. The sleep lets the
            // guest empty the queue; the flush then sends the held remainder into the corner,
            // where it is clamped away. **One flush sends at most four packets** — the queue's
            // worth, 508 px an axis — so a larger remainder survives it, and that rare case is
            // what `click_at`'s retry absorbs. The same drain and flush, for the same reason, as
            // `burst_holds_its_position` takes after its own pin. (This comment
            // said the queue *drops* a whole packet until 2026-09-23, when QEMU's `ps2.c` was
            // read — the sleep was right and the explanation was not.)
            std::thread::sleep(std::time::Duration::from_millis(500));
            qmp.flush()?;
            corner
        }
    };
    let (mut dx, mut dy) = (x - from.0, y - from.1);
    while dx != 0 || dy != 0 {
        let sx = dx.clamp(-100, 100);
        let sy = dy.clamp(-100, 100);
        qmp.send_motion(sx, sy)?;
        dx -= sx;
        dy -= sy;
    }
    qmp.pointer = Some((x, y));
    Ok(())
}
