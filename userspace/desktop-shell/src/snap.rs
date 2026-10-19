//! Snapping and maximising: the zones the compositor matches during a move, and what the shell
//! answers a finished gesture or a window's own request to be maximised or restored.

use librsproto::surface::{
    ConfigureEvent, DRAG_ENDED_MOVED, MgrDragEnded, MgrLayout, MgrSnapZone, WINDOW_STATE_MAXIMIZED,
    WINDOW_STATE_NORMAL, WindowState,
};

/// A rectangle as the shell keeps one: `x`, `y`, width, height.
pub type Rect4 = (i32, i32, u32, u32);

/// Each maximised window and the rectangle a restore gives back.
pub type RestoreList = alloc::vec::Vec<(u32, Rect4)>;

/// The width in pixels of the band along each work-area edge that triggers a snap.
pub const SNAP_BAND: u32 = 24;

/// The eight snap zones for `work`, corners first, as the compositor takes the first match: an edge
/// is half the work area, a corner a quarter, and the top edge all of it. The top zones reach up
/// over the top bar.
pub fn snap_zones(work: &MgrLayout) -> [MgrSnapZone; 8] {
    let (x, y, w, h) = (work.work_x, work.work_y, work.work_w, work.work_h);
    let (hw, hh) = (w / 2, h / 2);
    let band = SNAP_BAND.min(w.max(1)).min(h.max(1));
    let b = band as i32;
    let top_h = (y.max(0) as u32).saturating_add(band);
    let (right, bottom) = (x + w as i32 - b, y + h as i32 - b);
    let zones: [(u32, Rect4, Rect4); 8] = [
        (1, (x, 0, band, top_h), (x, y, hw, hh)),
        (2, (right, 0, band, top_h), (x + hw as i32, y, w - hw, hh)),
        (3, (x, bottom, band, band), (x, y + hh as i32, hw, h - hh)),
        (4, (right, bottom, band, band), (x + hw as i32, y + hh as i32, w - hw, h - hh)),
        (5, (x, y, band, h), (x, y, hw, h)),
        (6, (right, y, band, h), (x + hw as i32, y, w - hw, h)),
        (7, (x, 0, w, top_h), (x, y, w, h)),
        (8, (x, bottom, w, band), (x, y + hh as i32, w, h - hh)),
    ];
    zones.map(|(id, t, g)| MgrSnapZone {
        id,
        trigger_x: t.0,
        trigger_y: t.1,
        trigger_w: t.2,
        trigger_h: t.3,
        target_x: g.0,
        target_y: g.1,
        target_w: g.2,
        target_h: g.3,
    })
}

/// What to send a window: a `Configure` to this rectangle, and a state for its client.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Answer {
    /// The rectangle to configure the window to, if it is not there already.
    pub configure: Option<Rect4>,
    /// The state to tell the window's client, if it changed or the client asked.
    pub tell: Option<u32>,
}

fn rect(c: &ConfigureEvent) -> Rect4 {
    (c.x, c.y, c.width, c.height)
}

/// The answer to a finished gesture: a snap or resize asking for the whole work area maximises the
/// window, keeping the restore rectangle it had or else the one from before the drag; anything
/// else leaves it normal where it asked. Only what is not already on screen is configured.
pub fn drag_ended(d: &MgrDragEnded, work: Rect4, restore: &mut RestoreList) -> Answer {
    let (window, asked) = (d.asked.window, rect(&d.asked));
    let was_maximised = restore.iter().any(|(id, _)| *id == window);
    let moved = d.flags & DRAG_ENDED_MOVED != 0;
    let configure = (!moved).then_some(asked);
    if !moved && asked == work {
        if !was_maximised {
            restore.push((window, rect(&d.from)));
        }
        Answer { configure, tell: (!was_maximised).then_some(WINDOW_STATE_MAXIMIZED) }
    } else {
        restore.retain(|(id, _)| *id != window);
        Answer { configure, tell: was_maximised.then_some(WINDOW_STATE_NORMAL) }
    }
}

/// The answer to a window's own request to be maximised or restored, from `now`: told either way,
/// so a button out of step is put right.
pub fn state_requested(window: u32, state: u32, now: Rect4, work: Rect4, restore: &mut RestoreList) -> Answer {
    match state {
        WINDOW_STATE_MAXIMIZED => {
            if !restore.iter().any(|(id, _)| *id == window) {
                restore.push((window, now));
            }
            Answer { configure: Some(work), tell: Some(WINDOW_STATE_MAXIMIZED) }
        }
        WINDOW_STATE_NORMAL => {
            let back = restore.iter().position(|(id, _)| *id == window).map(|i| restore.remove(i).1);
            Answer { configure: back, tell: Some(WINDOW_STATE_NORMAL) }
        }
        _ => Answer::default(),
    }
}

/// The answer to a request the shell does not grant, or for a window it does not list: the state
/// the window is in. Silence would leave the client's next request for the same state dropped as
/// a repeat, and its button dead.
pub fn declined(window: u32, restore: &RestoreList) -> Answer {
    let maximised = restore.iter().any(|(id, _)| *id == window);
    Answer { configure: None, tell: Some(if maximised { WINDOW_STATE_MAXIMIZED } else { WINDOW_STATE_NORMAL }) }
}

/// Forget a destroyed window: its restore rectangle, and any gesture or request of its still to be
/// answered.
pub fn forget(
    window: u32,
    restore: &mut RestoreList,
    dropped: &mut alloc::vec::Vec<MgrDragEnded>,
    states: &mut alloc::vec::Vec<WindowState>,
) {
    restore.retain(|(id, _)| *id != window);
    dropped.retain(|d| d.asked.window != window);
    states.retain(|s| s.window != window);
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: Rect4 = (0, 30, 1360, 708);

    fn layout() -> MgrLayout {
        MgrLayout { screen_w: 1360, screen_h: 768, work_x: 0, work_y: 30, work_w: 1360, work_h: 708 }
    }

    /// The first zone whose trigger holds `(px, py)`, as the compositor matches.
    fn zone_at(px: i32, py: i32) -> Option<MgrSnapZone> {
        snap_zones(&layout()).into_iter().find(|z| {
            px >= z.trigger_x
                && py >= z.trigger_y
                && px < z.trigger_x + z.trigger_w as i32
                && py < z.trigger_y + z.trigger_h as i32
        })
    }

    fn target(z: MgrSnapZone) -> Rect4 {
        (z.target_x, z.target_y, z.target_w, z.target_h)
    }

    /// A snap or a resize: `asked` is not yet on screen.
    fn ended(window: u32, asked: Rect4, from: Rect4) -> MgrDragEnded {
        let c = |r: Rect4| ConfigureEvent { window, x: r.0, y: r.1, width: r.2, height: r.3 };
        MgrDragEnded { asked: c(asked), from: c(from), flags: 0 }
    }

    /// A plain move, already where `asked` says.
    fn moved(window: u32, asked: Rect4, from: Rect4) -> MgrDragEnded {
        MgrDragEnded { flags: DRAG_ENDED_MOVED, ..ended(window, asked, from) }
    }

    #[test]
    fn the_top_edge_maximises_from_the_top_bar_down_to_the_band() {
        for py in [0, 15, 29, 30, 53] {
            assert_eq!(zone_at(680, py).map(target), Some(WORK), "row {py}");
        }
        assert_eq!(zone_at(680, 54), None, "the row below the band");
    }

    #[test]
    fn the_top_corners_reach_the_bar_too_and_beat_the_edge() {
        assert_eq!(zone_at(0, 0).map(target), Some((0, 30, 680, 354)));
        assert_eq!(zone_at(1359, 10).map(target), Some((680, 30, 680, 354)));
        assert_eq!(zone_at(24, 10).map(target), Some(WORK), "a column past the corner is the edge");
    }

    #[test]
    fn the_other_edges_and_corners_keep_their_halves_and_quarters() {
        assert_eq!(zone_at(5, 400).map(target), Some((0, 30, 680, 708)));
        assert_eq!(zone_at(1355, 400).map(target), Some((680, 30, 680, 708)));
        assert_eq!(zone_at(680, 737).map(target), Some((0, 384, 1360, 354)));
        assert_eq!(zone_at(1355, 737).map(target), Some((680, 384, 680, 354)));
        assert_eq!(zone_at(680, 400), None);
    }

    #[test]
    fn a_drop_at_the_top_keeps_the_rectangle_from_before_the_drag() {
        let mut restore = RestoreList::new();
        let a = drag_ended(&ended(7, WORK, (24, 78, 596, 505)), WORK, &mut restore);
        assert_eq!(a, Answer { configure: Some(WORK), tell: Some(WINDOW_STATE_MAXIMIZED) });
        assert_eq!(restore, [(7, (24, 78, 596, 505))]);
    }

    #[test]
    fn a_drop_at_the_top_of_a_maximised_window_keeps_the_restore_rectangle_it_had() {
        let mut restore: RestoreList = alloc::vec![(7, (24, 78, 596, 505))];
        let a = drag_ended(&ended(7, WORK, (100, 40, 1360, 708)), WORK, &mut restore);
        assert_eq!(a, Answer { configure: Some(WORK), tell: None }, "it was maximised already");
        assert_eq!(restore, [(7, (24, 78, 596, 505))]);
    }

    #[test]
    fn a_maximised_window_moved_or_snapped_elsewhere_is_normal_again() {
        let half = (0, 30, 680, 708);
        let mut restore: RestoreList = alloc::vec![(7, (24, 78, 596, 505))];
        let a = drag_ended(&ended(7, half, WORK), WORK, &mut restore);
        assert_eq!(a, Answer { configure: Some(half), tell: Some(WINDOW_STATE_NORMAL) });
        assert!(restore.is_empty());

        let mut restore: RestoreList = alloc::vec![(7, (24, 78, 596, 505))];
        let to = (40, 300, 1360, 708);
        let a = drag_ended(&moved(7, to, WORK), WORK, &mut restore);
        assert_eq!(a, Answer { configure: None, tell: Some(WINDOW_STATE_NORMAL) }, "a plain move is not configured");
        assert!(restore.is_empty());
    }

    #[test]
    fn a_normal_window_moved_or_resized_is_told_nothing() {
        let mut restore = RestoreList::new();
        let to = (40, 300, 596, 505);
        assert_eq!(drag_ended(&moved(7, to, (24, 78, 596, 505)), WORK, &mut restore), Answer::default());
        let bigger = (24, 78, 700, 505);
        let a = drag_ended(&ended(7, bigger, (24, 78, 596, 505)), WORK, &mut restore);
        assert_eq!(a, Answer { configure: Some(bigger), tell: None });
        assert!(restore.is_empty());
    }

    #[test]
    fn a_maximise_then_a_restore_by_the_button_gives_the_rectangle_back() {
        let mut restore = RestoreList::new();
        let was = (24, 78, 596, 505);
        let a = state_requested(7, WINDOW_STATE_MAXIMIZED, was, WORK, &mut restore);
        assert_eq!(a, Answer { configure: Some(WORK), tell: Some(WINDOW_STATE_MAXIMIZED) });
        let again = state_requested(7, WINDOW_STATE_MAXIMIZED, WORK, WORK, &mut restore);
        assert_eq!(again.tell, Some(WINDOW_STATE_MAXIMIZED), "a button out of step is told again");
        let a = state_requested(7, WINDOW_STATE_NORMAL, WORK, WORK, &mut restore);
        assert_eq!(a, Answer { configure: Some(was), tell: Some(WINDOW_STATE_NORMAL) }, "the first rectangle");
        let a = state_requested(7, WINDOW_STATE_NORMAL, was, WORK, &mut restore);
        assert_eq!(a, Answer { configure: None, tell: Some(WINDOW_STATE_NORMAL) }, "nothing to restore");
    }

    #[test]
    fn a_snap_back_to_where_it_began_is_configured_whatever_the_shell_last_saw() {
        // Snapped to the left half, dragged off and dropped in the same zone: the window is where
        // the pointer let go, so the half is asked again, though it is also where it began.
        let half = (0, 30, 680, 708);
        let mut restore = RestoreList::new();
        assert_eq!(drag_ended(&ended(7, half, half), WORK, &mut restore).configure, Some(half));
        let mut restore: RestoreList = alloc::vec![(7, (24, 78, 596, 505))];
        let a = drag_ended(&ended(7, WORK, WORK), WORK, &mut restore);
        assert_eq!(a, Answer { configure: Some(WORK), tell: None }, "a maximised window dropped back at the top");
    }

    #[test]
    fn a_plain_move_is_never_configured_and_never_maximises() {
        let mut restore = RestoreList::new();
        let dialog = moved(9, (300, 200, 240, 120), (100, 100, 240, 120));
        assert_eq!(drag_ended(&dialog, WORK, &mut restore), Answer::default(), "a window the shell does not list");
        assert_eq!(drag_ended(&moved(7, WORK, (0, 40, 1360, 708)), WORK, &mut restore), Answer::default());
        assert!(restore.is_empty(), "moved onto the work area by hand is not maximised");
    }

    #[test]
    fn a_request_the_shell_declines_is_answered_with_the_state_the_window_is_in() {
        let restore: RestoreList = alloc::vec![(7, (24, 78, 596, 505))];
        assert_eq!(declined(7, &restore), Answer { configure: None, tell: Some(WINDOW_STATE_MAXIMIZED) });
        assert_eq!(declined(8, &restore), Answer { configure: None, tell: Some(WINDOW_STATE_NORMAL) });
    }

    #[test]
    fn a_destroyed_window_leaves_no_restore_rectangle_and_no_gesture_to_answer() {
        let mut restore: RestoreList = alloc::vec![(7, (24, 78, 596, 505)), (8, (0, 30, 10, 10))];
        let mut dropped = alloc::vec![ended(7, WORK, WORK), ended(8, WORK, WORK)];
        let mut states = alloc::vec![WindowState { window: 7, state: 2 }, WindowState { window: 8, state: 0 }];
        forget(7, &mut restore, &mut dropped, &mut states);
        assert_eq!(restore, [(8, (0, 30, 10, 10))]);
        assert_eq!(dropped, [ended(8, WORK, WORK)]);
        assert_eq!(states, [WindowState { window: 8, state: 0 }]);
    }

    #[test]
    fn the_zones_fit_a_small_odd_work_area_at_the_screen_top() {
        let tiny = MgrLayout { screen_w: 11, screen_h: 7, work_x: 0, work_y: 0, work_w: 11, work_h: 7 };
        let z = snap_zones(&tiny);
        assert!(z.iter().all(|z| z.trigger_w <= 11 && z.trigger_h <= 7), "the band is clamped to the area");
        assert_eq!((z[6].trigger_y, z[6].trigger_h), (0, 7), "no bar above, so the top band starts at 0");
        assert_eq!((z[7].target_y, z[7].target_h), (3, 4), "the lower half takes the odd row");
        let above = MgrLayout { work_y: -5, ..tiny };
        assert_eq!(snap_zones(&above)[6].trigger_h, 7, "a work area above the screen adds no rows");
    }
}
