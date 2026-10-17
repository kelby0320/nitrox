//! What the desktop reports on serial, parsed, and the pointer and menu moves that act on it.

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
pub(crate) fn parse_dialog_placement(tail: &str) -> Option<(u32, u32, i32, i32, u32, u32)> {
    let (id, rest) = tail.trim().split_once(" of window ")?;
    let (parent, rest) = rest.split_once(" at ")?;
    let (origin, size) = rest.split_once(' ')?;
    let (x, y) = origin.split_once(',')?;
    let (w, h) = size.trim().split_once('x')?;
    Some((id.parse().ok()?, parent.parse().ok()?, x.parse().ok()?, y.parse().ok()?, w.parse().ok()?, h.parse().ok()?))
}

/// Which slot of the taskbar `id` occupies, **read from the shell's window-list line** `…
/// [20:nxfiles] [21:notes.txt*]` rather than counted, as a tally drifts when a window closes.
pub(crate) fn taskbar_slot(list: &str, id: u32) -> Option<usize> {
    list.split('[').skip(1).position(|group| group.split_once(':').is_some_and(|(n, _)| n.trim() == id.to_string()))
}

/// Type `text` into the Applications menu's filter, **a receipt per character**: injection is
/// unacknowledged. The match count is read, not asserted: what `/bin` holds is the image's.
pub(crate) fn type_into_menu(qmp: &mut Qmp, session: &mut Session, text: &str) -> R<()> {
    for c in text.chars() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        session.expect("desktop-shell: applications menu listing ")?;
    }
    Ok(())
}

/// Middle-click at `(x, y)`, having first walked there and checked with a left click, itself a
/// gesture (a raise or a minimise) that the close request following does not care about.
pub(crate) fn middle_click_at(qmp: &mut Qmp, session: &mut Session, x: i32, y: i32) -> R<()> {
    click_at(qmp, session, x, y)?;
    qmp.send_button("middle", true)?;
    qmp.send_button("middle", false)?;
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

/// The largest grid `nxterm` reported reaching, from `nxterm: resized to WxH, grid CxR` (the
/// maximise).
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
    let (x, y) = it.next()?.split_once(',')?;
    Some((id, x.parse().ok()?, y.parse().ok()?))
}

/// Read `<id> at <x>,<y> <w>x<h>` — the tail of `nxterm`'s menu-popup line.
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

/// Read the tail of `desktop-shell: overview open, … of <n> desktops`: `n`. **The count is the
/// guest's, the positions are the gate's.**
pub(crate) fn parse_overview_line(rest: &str) -> Option<i32> {
    let (_, tail) = rest.split_once(" of ")?;
    tail.split_whitespace().next()?.parse().ok()
}

/// Read `desktop-shell: window N geometry X,Y WxH`: asked of the shell, not recomputed.
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

/// Pin the pointer to the bottom-right corner, then walk it to `(x, y)`. **Relative injection, so
/// the pointer must be somewhere known first**: the steps are bounded (a PS/2 delta is 9-bit) and
/// the corner is reached by over-driving into the clamp. It pins only when the position is unknown,
/// as a pin before every click floods the guest's input ring, and records where the pointer ends.
pub(crate) fn move_pointer_to(qmp: &mut Qmp, x: i32, y: i32) -> R<()> {
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
            // Let the pin drain, then flush what QEMU held of it: its PS/2 queue is sixteen bytes
            // and a packet that will not fit is **held**, not dropped, and added to the next
            // injected event, which would cancel the walk's first step (`check-terminal` once
            // landed at (495, 351) aiming at (397, 295)). One flush sends at most four packets, 508
            // px an axis; `click_at`'s retry absorbs the rest.
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
