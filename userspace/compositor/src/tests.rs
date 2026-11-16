use super::*;
use libkern::abi::{
    KEY_CAPSLOCK, KEY_LEFTALT, KEY_LEFTCTRL, KEY_LEFTMETA, KEY_LEFTSHIFT, KEY_NUMLOCK, KEY_RIGHTALT, KEY_RIGHTCTRL,
    KEY_RIGHTMETA, KEY_RIGHTSHIFT,
};

/// Two ordinary (non-modifier) keycodes, `a` and `b` in the Linux table.
const KEY_A: u16 = 30;
const KEY_B: u16 = 48;

#[test]
fn the_cursors_hotspot_is_its_top_left() {
    let r = cursor_rect(Point::new(40, 30));
    assert_eq!(r.origin, Point::new(40, 30));
    assert_eq!(r.size, libdraw::geom::Size::new(CURSOR_W, CURSOR_H));
}

#[test]
fn the_sprite_matches_the_size_it_claims() {
    assert_eq!(CURSOR.len(), CURSOR_H as usize);
    for (i, line) in CURSOR.iter().enumerate() {
        assert!(
            line.chars().count() <= CURSOR_W as usize,
            "row {i} is {} wide, past the declared {CURSOR_W}",
            line.chars().count()
        );
    }
}

#[test]
fn a_cursor_at_the_screen_edge_is_clipped_rather_than_wrapped() {
    use libdraw::format::PixelFormat;
    use libdraw::framebuffer::{Geometry, MemFramebuffer};
    let mut fb = MemFramebuffer::new(Geometry::packed(40, 40, PixelFormat::XRGB8888));
    let clip = Rect::new(0, 0, 40, 40);
    draw_cursor(&mut fb, Point::new(36, 36), clip);
    draw_cursor(&mut fb, Point::new(-4, -4), clip);
    assert_eq!(fb.get_pixel(0, 39), Some(Rgb::new(0, 0, 0)));
}

#[test]
fn an_outlines_edges_are_four_strips_that_cover_its_border_and_nothing_inside() {
    let r = Rect::new(10, 20, 100, 60);
    let e = outline_edges(r);
    for strip in e {
        assert!(strip.intersect(&r).is_some(), "a strip outside the rectangle: {strip:?}");
    }
    for y in r.origin.y..r.bottom() as i32 {
        for x in r.origin.x..r.right() as i32 {
            let on_border = x < r.origin.x + OUTLINE_W as i32
                || x >= r.right() as i32 - OUTLINE_W as i32
                || y < r.origin.y + OUTLINE_W as i32
                || y >= r.bottom() as i32 - OUTLINE_W as i32;
            let covered = e.iter().any(|s| s.contains(x, y));
            assert_eq!(covered, on_border, "({x},{y}) border={on_border} covered={covered}");
        }
    }
}

#[test]
fn a_degenerate_outline_stays_inside_itself() {
    for r in [Rect::new(5, 5, 1, 40), Rect::new(5, 5, 40, 1), Rect::new(5, 5, 0, 0)] {
        for strip in outline_edges(r) {
            assert!(strip.is_empty() || strip.intersect(&r) == Some(strip), "{strip:?} leaves {r:?}");
        }
    }
}

#[test]
fn the_outline_draws_only_inside_its_clip() {
    use libdraw::format::PixelFormat;
    use libdraw::framebuffer::{Geometry, MemFramebuffer};
    let mut fb = MemFramebuffer::new(Geometry::packed(60, 60, PixelFormat::XRGB8888));
    let bg = Rgb::new(0x11, 0x22, 0x33);
    for y in 0..60 {
        for x in 0..60 {
            fb.put_pixel(x, y, bg);
        }
    }
    let clip = Rect::new(8, 8, 4, 4);
    draw_outline(&mut fb, Rect::new(8, 8, 40, 40), clip);
    let mut ink = 0;
    for y in 0..60u32 {
        for x in 0..60u32 {
            if fb.get_pixel(x, y) != Some(bg) {
                ink += 1;
                assert!(clip.contains(x as i32, y as i32), "ink at ({x},{y}) outside the clip");
            }
        }
    }
    assert!(ink > 0, "the clip covers the outline's corner, so something must be drawn");
}

#[test]
fn presenting_draws_the_outline_over_the_stack_and_into_every_damage_rectangle() {
    let s = WindowStack::new();
    let src = MapSource::default();
    let mut fb = big_screen();
    let outline = Rect::new(20, 20, 60, 40);
    let corners = [Rect::new(20, 20, 4, 4), Rect::new(76, 56, 4, 4)];
    s.present_into(&mut fb, Rgb::BLACK, &src, &corners, Point::new(200, 200), Some(outline));
    for c in corners {
        let painted = (c.origin.y..c.bottom() as i32)
            .flat_map(|y| (c.origin.x..c.right() as i32).map(move |x| (x, y)))
            .filter(|(x, y)| fb.get_pixel(*x as u32, *y as u32) == Some(OUTLINE_COLOUR))
            .count();
        assert!(painted > 0, "the outline was not drawn into {c:?}");
    }
}

#[test]
fn the_cursor_draws_only_inside_its_clip() {
    use libdraw::format::PixelFormat;
    use libdraw::framebuffer::{Geometry, MemFramebuffer};
    let mut fb = MemFramebuffer::new(Geometry::packed(60, 60, PixelFormat::XRGB8888));
    let bg = Rgb::new(0x11, 0x22, 0x33);
    for y in 0..60 {
        for x in 0..60 {
            fb.put_pixel(x, y, bg);
        }
    }
    draw_cursor(&mut fb, Point::new(10, 10), Rect::new(10, 10, 4, 4));
    for y in 0..60u32 {
        for x in 0..60u32 {
            if !Rect::new(10, 10, 4, 4).contains(x as i32, y as i32) {
                assert_eq!(fb.get_pixel(x, y), Some(bg), "ink at ({x},{y}) outside the clip");
            }
        }
    }
}

#[test]
fn a_repeat_waits_the_delay_and_then_runs_at_the_interval() {
    let mut r = Repeat::armed(30, 0, 0, 1, 1_000);
    assert!(!r.due(1_000), "not immediately");
    assert!(!r.due(1_000 + REPEAT_DELAY_NS - 1), "not a nanosecond early");
    assert!(r.due(1_000 + REPEAT_DELAY_NS), "and then it fires");
    assert!(!r.due(1_000 + REPEAT_DELAY_NS), "once per deadline");
    assert!(r.due(1_000 + REPEAT_DELAY_NS + REPEAT_INTERVAL_NS), "then at the interval");
}

#[test]
fn a_late_wakeup_does_not_slow_the_repeat_rate() {
    let start = 1_000;
    let mut r = Repeat::armed(30, 0, 0, 1, start);
    let first = start + REPEAT_DELAY_NS;
    assert!(r.due(first + REPEAT_INTERVAL_NS / 2), "fired half an interval late");
    assert_eq!(
        r.next_at,
        first + REPEAT_INTERVAL_NS,
        "the next one is still on the original cadence, not shifted by the lateness"
    );
}

#[test]
fn a_very_late_wakeup_does_not_fire_a_burst() {
    // A stalled compositor must not deliver every missed repeat.
    let mut r = Repeat::armed(30, 0, 0, 1, 0);
    let much_later = REPEAT_DELAY_NS + REPEAT_INTERVAL_NS * 1_000;
    assert!(r.due(much_later));
    assert!(!r.due(much_later), "and only one");
    assert_eq!(r.next_at, much_later + REPEAT_INTERVAL_NS);
}

#[test]
fn modifiers_are_frozen_at_the_press() {
    // A repeat is that press continuing: `A` must not become `a` if shift is released.
    let r = Repeat::armed(30, 0x0001, 0, 7, 0);
    assert_eq!(r.modifiers, 0x0001);
    assert_eq!(r.keycode, 30);
    assert_eq!(r.window, 7);
}

#[test]
fn focus_transition_is_silent_when_focus_did_not_move() {
    assert_eq!(focus_transition(Some(3), Some(3)), None);
    assert_eq!(focus_transition(None, None), None);
}

#[test]
fn focus_transition_names_both_halves() {
    assert_eq!(focus_transition(Some(1), Some(2)), Some((Some(1), Some(2))));
    assert_eq!(focus_transition(None, Some(2)), Some((None, Some(2))), "first window");
    assert_eq!(focus_transition(Some(1), None), Some((Some(1), None)), "last window went");
}
use alloc::collections::BTreeMap;
use alloc::vec;
use libdraw::framebuffer::MemFramebuffer;
use librsproto::surface::SURFACE_FORMAT_XRGB8888;

#[derive(Default)]
struct MapSource(BTreeMap<(u32, u32), Vec<u8>>);

impl MapSource {
    fn put(&mut self, window: u32, buffer: u32, g: Geometry, colour: Rgb) {
        let mut px = vec![0u8; g.byte_len()];
        let word = g.format.encode(colour).to_le_bytes();
        for y in 0..g.height {
            for x in 0..g.width {
                let off = g.offset_of(x, y).unwrap();
                px[off..off + 4].copy_from_slice(&word);
            }
        }
        self.0.insert((window, buffer), px);
    }
}

/// A distinct colour per source row, so a row-stride error is visible.
fn row_colour(y: u32) -> Rgb {
    Rgb::new(0x20 + y as u8, 0x40, 0x80)
}

impl MapSource {
    fn put_striped(&mut self, window: u32, buffer: u32, g: Geometry) {
        let mut px = vec![0u8; g.byte_len()];
        for y in 0..g.height {
            let word = g.format.encode(row_colour(y)).to_le_bytes();
            for x in 0..g.width {
                let off = g.offset_of(x, y).unwrap();
                px[off..off + 4].copy_from_slice(&word);
            }
        }
        self.0.insert((window, buffer), px);
    }
}

impl BufferSource for MapSource {
    fn pixels(&self, window: u32, buffer: u32) -> Option<libdraw::pixels::Pixels<'_>> {
        self.0.get(&(window, buffer)).map(|v| libdraw::pixels::Pixels::Own(v.as_slice()))
    }
}

fn geom(w: u32, h: u32) -> Geometry {
    Geometry::packed(w, h, PixelFormat::XRGB8888)
}

fn attach(window: u32, buffer: u32, w: u32, h: u32) -> AttachBufferRequest {
    AttachBufferRequest { window, buffer, width: w, height: h, pitch: w * 4, format: SURFACE_FORMAT_XRGB8888 }
}

fn shown(s: &mut WindowStack, req: &CreateWindowRequest) -> u32 {
    let id = s.create(req).expect("create");
    s.mark_configured(id);
    id
}

fn commit(window: u32, buffer: u32) -> CommitRequest {
    CommitRequest { window, buffer, damage_x: 0, damage_y: 0, damage_w: 0, damage_h: 0 }
}

fn screen() -> MemFramebuffer {
    MemFramebuffer::new(Geometry::with_pitch(32, 16, 140, PixelFormat::XRGB8888).unwrap())
}

fn big_screen() -> MemFramebuffer {
    MemFramebuffer::new(Geometry::with_pitch(96, 96, 400, PixelFormat::XRGB8888).unwrap())
}

fn overlapping_pair() -> (WindowStack, MapSource, (u32, u32)) {
    let mut s = WindowStack::new();
    let mut src = MapSource::default();
    let bottom = shown(&mut s, &CreateWindowRequest::new(20, 10, Role::Normal));
    let top = shown(&mut s, &CreateWindowRequest::new(20, 10, Role::Normal));
    for (id, colour) in [(bottom, Rgb::new(0xFF, 0, 0)), (top, Rgb::new(0, 0xFF, 0))] {
        s.attach(&attach(id, 0, 20, 10)).unwrap();
        src.put(id, 0, geom(20, 10), colour);
        s.commit(&commit(id, 0)).unwrap();
    }
    let _ = s.place(bottom, Point::new(0, 0)).unwrap();
    let _ = s.place(top, Point::new(10, 3)).unwrap();
    (s, src, (bottom, top))
}

#[test]
fn the_region_a_raise_reports_is_the_whole_of_what_it_changed() {
    // Guards narrowing a restack's repaint to one rectangle: paint only the reported region
    // into one screen, recompose another whole, and compare every byte.
    let (mut s, src, (bottom, _top)) = overlapping_pair();
    let mut painted = screen();
    let full = Rect::new(0, 0, 32, 16);
    s.present_into(&mut painted, Rgb::BLACK, &src, &[full], Point::new(100, 100), None);

    let d = s.raise(bottom).expect("in the stack");
    s.present_into(&mut painted, Rgb::BLACK, &src, &[d.rect()], Point::new(100, 100), None);

    let mut reference = screen();
    s.present_into(&mut reference, Rgb::BLACK, &src, &[full], Point::new(100, 100), None);
    assert_eq!(
        painted.bytes(),
        reference.bytes(),
        "repainting only the raised window's rectangle must leave the screen identical to a \
         full recompose"
    );
}

#[test]
fn the_region_a_lower_reports_is_the_whole_of_what_it_changed() {
    let (mut s, src, (_bottom, top)) = overlapping_pair();
    let mut painted = screen();
    let full = Rect::new(0, 0, 32, 16);
    s.present_into(&mut painted, Rgb::BLACK, &src, &[full], Point::new(100, 100), None);

    let d = s.lower(top).expect("in the stack");
    s.present_into(&mut painted, Rgb::BLACK, &src, &[d.rect()], Point::new(100, 100), None);

    let mut reference = screen();
    s.present_into(&mut reference, Rgb::BLACK, &src, &[full], Point::new(100, 100), None);
    assert_eq!(painted.bytes(), reference.bytes(), "a lower's region must cover its own");
}

#[test]
fn a_restack_that_changes_no_order_reports_no_region() {
    let (mut s, _src, (bottom, top)) = overlapping_pair();
    assert!(s.raise(top).expect("in the stack").is_empty(), "already topmost");
    assert!(s.lower(bottom).expect("in the stack").is_empty(), "already bottom-most");
    assert!(s.raise_above(top, bottom).expect("both exist").is_empty(), "already above it");
    assert_eq!(
        s.windows().iter().map(|w| w.id).collect::<Vec<_>>(),
        vec![bottom, top],
        "and none of them reordered anything"
    );
}

#[test]
fn a_raise_that_does_reorder_reports_the_window_it_moved() {
    let (mut s, _src, (bottom, _top)) = overlapping_pair();
    let want = s.window(bottom).expect("in the stack").painted_bounds();
    assert!(want.size.w > 20, "the premise: the region includes the shadow");
    assert_eq!(s.raise(bottom).expect("in the stack").rect(), want);
}

#[test]
fn a_repeated_state_request_is_not_a_second_event() {
    // The one manager event a client's own rate drives: a client looping on one state would
    // push a `WindowCreated` off the manager's queue.
    let mut s = WindowStack::new();
    let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
    assert!(s.note_state_request(w, 2), "the first ask is news");
    assert!(!s.note_state_request(w, 2), "the same ask again is not");
    assert!(s.note_state_request(w, 0), "and a different one is");
    assert!(!s.note_state_request(w, 0));
    assert!(!s.note_state_request(w + 100, 1), "a window that does not exist has nobody to tell about it");
}

#[test]
fn the_work_area_shrinks_by_every_panels_strut_not_only_the_first() {
    let mut s = WindowStack::new();
    let screen = Rect::new(0, 0, 100, 100);
    assert_eq!(s.work_area(screen), screen, "no panels, no reservation");
    let _ = shown(&mut s, &CreateWindowRequest::new(100, 10, Role::Panel { dock: Edge::Top, reserve: 10 }));
    let _ = shown(&mut s, &CreateWindowRequest::new(100, 8, Role::Panel { dock: Edge::Bottom, reserve: 8 }));
    assert_eq!(s.work_area(screen), Rect::new(0, 10, 100, 82), "both struts, not just the first");
}

#[test]
fn a_manager_changing_a_windows_state_invalidates_what_it_last_asked_to_be() {
    // The dedup compares against a value the manager also changes: a minimise, a taskbar
    // restore, then the same minimise must not be dropped as a repeat.
    let mut s = WindowStack::new();
    let w = s.create(&CreateWindowRequest::new(80, 60, Role::Normal)).unwrap();
    assert!(s.note_state_request(w, 1), "the first minimise is news");

    s.set_minimized(w, true).unwrap();
    s.clear_state_request(w);
    s.set_minimized(w, false).unwrap();
    s.clear_state_request(w);

    assert!(s.note_state_request(w, 1), "and minimising again must reach the manager");
}

#[test]
fn a_created_window_gets_a_fresh_id_and_keeps_its_role() {
    let mut s = WindowStack::new();
    let a = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
    let b = s.create(&CreateWindowRequest::new(32, 4, Role::Panel { dock: Edge::Top, reserve: 4 })).unwrap();
    assert_ne!(a, b, "ids must be unique");
    assert_eq!(s.window(a).unwrap().role, Role::Normal);
    assert_eq!(s.window(b).unwrap().role, Role::Panel { dock: Edge::Top, reserve: 4 });
}

#[test]
fn a_popup_must_name_a_parent_that_exists() {
    let mut s = WindowStack::new();
    assert_eq!(s.create(&CreateWindowRequest::new(4, 4, Role::Popup { parent: 99 })), Err(StackError::NoSuchParent));
    let p = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
    assert!(s.create(&CreateWindowRequest::new(4, 4, Role::Popup { parent: p })).is_ok());
}

#[test]
fn destroying_a_parent_takes_its_popups_with_it() {
    let mut s = WindowStack::new();
    let p = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
    let menu = s.create(&CreateWindowRequest::new(4, 4, Role::Popup { parent: p })).unwrap();
    let other = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
    s.destroy(p).unwrap();
    assert!(s.window(menu).is_none(), "an orphaned popup has no defined stacking position");
    assert!(s.window(other).is_some(), "unrelated windows are untouched");
}

#[test]
fn commit_returns_the_previous_buffer_to_release_not_the_new_one() {
    let mut s = WindowStack::new();
    let w = s.create(&CreateWindowRequest::new(4, 4, Role::Normal)).unwrap();
    s.attach(&attach(w, 0, 4, 4)).unwrap();
    s.attach(&attach(w, 1, 4, 4)).unwrap();

    assert_eq!(s.commit(&commit(w, 0)).unwrap(), None, "nothing was on screen before");
    assert_eq!(s.commit(&commit(w, 1)).unwrap(), Some(0), "buffer 0 is now free");
    assert_eq!(s.commit(&commit(w, 0)).unwrap(), Some(1));
    assert_eq!(s.commit(&commit(w, 0)).unwrap(), None);
}

#[test]
fn committing_an_unattached_buffer_is_refused() {
    let mut s = WindowStack::new();
    let w = s.create(&CreateWindowRequest::new(4, 4, Role::Normal)).unwrap();
    assert_eq!(s.commit(&commit(w, 7)), Err(StackError::NoSuchBuffer));
    assert_eq!(s.commit(&commit(99, 0)), Err(StackError::NoSuchWindow));
}

#[test]
fn re_attaching_a_free_buffer_replaces_it_and_the_committed_one_is_refused() {
    // A resize replaces buffer ids (the protocol has no detach); the committed buffer is
    // refused, as its pixels may be being read.
    let mut s = WindowStack::new();
    let w = s.create(&CreateWindowRequest::new(4, 4, Role::Normal)).unwrap();
    s.attach(&attach(w, 0, 4, 4)).unwrap();
    s.attach(&attach(w, 1, 4, 4)).unwrap();
    assert_eq!(s.windows()[0].buffers.len(), 2);

    s.attach(&attach(w, 0, 8, 8)).expect("a free buffer takes new memory under its own id");
    assert_eq!(s.windows()[0].buffers.len(), 2, "replaced, not added");
    let g = s.windows()[0].buffers.iter().find(|b| b.id == 0).unwrap().geometry;
    assert_eq!((g.width, g.height), (8, 8), "and the new geometry is what the compositor will read it at");

    s.commit(&commit(w, 0)).unwrap();
    assert_eq!(
        s.attach(&attach(w, 0, 16, 16)),
        Err(StackError::DuplicateBuffer),
        "the buffer on screen is not replaceable"
    );
    s.attach(&attach(w, 1, 16, 16)).expect("the other one still is");
}

#[test]
fn panels_reserve_space_and_normal_windows_do_not() {
    let mut s = WindowStack::new();
    let screen = Rect::new(0, 0, 100, 50);
    assert_eq!(s.work_area(screen), screen, "no panels, no reservation");

    s.create(&CreateWindowRequest::new(100, 8, Role::Panel { dock: Edge::Top, reserve: 8 })).unwrap();
    s.create(&CreateWindowRequest::new(100, 6, Role::Panel { dock: Edge::Bottom, reserve: 6 })).unwrap();
    s.create(&CreateWindowRequest::new(40, 40, Role::Normal)).unwrap();

    assert_eq!(s.work_area(screen), Rect::new(0, 8, 100, 36));
}

#[test]
fn every_edge_reserves_on_its_own_axis() {
    let mut s = WindowStack::new();
    for (edge, reserve) in [(Edge::Left, 3u32), (Edge::Right, 5), (Edge::Top, 2), (Edge::Bottom, 4)] {
        s.create(&CreateWindowRequest::new(1, 1, Role::Panel { dock: edge, reserve })).unwrap();
    }
    assert_eq!(s.work_area(Rect::new(0, 0, 100, 50)), Rect::new(3, 2, 92, 44));
}

#[test]
fn over_reservation_empties_the_work_area_rather_than_inverting_it() {
    let mut s = WindowStack::new();
    s.create(&CreateWindowRequest::new(1, 1, Role::Panel { dock: Edge::Top, reserve: 90 })).unwrap();
    let wa = s.work_area(Rect::new(0, 0, 100, 50));
    assert_eq!(wa.size.h, 0, "a negative height would be a clipping catastrophe");
    assert!(wa.is_empty());
}

#[test]
fn focus_skips_panels_and_follows_the_top_of_the_stack() {
    let mut s = WindowStack::new();
    let a = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));
    let b = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));
    s.create(&CreateWindowRequest::new(32, 4, Role::Panel { dock: Edge::Top, reserve: 4 })).unwrap();
    assert_eq!(s.focus_candidate(), Some(b));
    assert!(!s.raise(a).unwrap().rect().is_empty());
    assert_eq!(s.focus_candidate(), Some(a));
}

#[test]
fn a_stack_with_only_panels_has_nothing_to_focus() {
    let mut s = WindowStack::new();
    s.create(&CreateWindowRequest::new(32, 4, Role::Panel { dock: Edge::Top, reserve: 4 })).unwrap();
    assert_eq!(s.focus_candidate(), None);
}

fn order(s: &WindowStack) -> Vec<u32> {
    s.windows().iter().map(|w| w.id).collect()
}

#[test]
fn moving_a_window_dirties_where_it_was_and_where_it_is() {
    // A rectangle cannot say "old minus new", so the union is the answer, computed by `place`
    // itself from the pre-move state.
    let mut s = WindowStack::new();
    let mut src = MapSource::default();
    let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
    s.attach(&attach(w, 0, 8, 8)).unwrap();
    src.put(w, 0, geom(8, 8), Rgb::new(1, 2, 3));
    s.commit(&commit(w, 0)).unwrap();

    // The union of where it was and where it is, each with its shadow; it must strictly contain
    // both window rectangles.
    let shadow = window_shadow(Scheme::Light);
    let dirty = s.place(w, Point::new(20, 10)).unwrap();
    assert_eq!(
        dirty.rect(),
        union(shadow.around(Rect::new(0, 0, 8, 8)), shadow.around(Rect::new(20, 10, 8, 8))),
        "the union of (0,0,8,8) and (20,10,8,8), each with its shadow"
    );
    assert!(dirty.rect().size.w > 28 && dirty.rect().size.h > 18, "the shadow is included");
    assert_eq!(s.window(w).unwrap().bounds(), Rect::new(20, 10, 8, 8), "bounds must not grow");

    assert_eq!(s.place(w, Point::new(20, 10)).unwrap().rect(), shadow.around(Rect::new(20, 10, 8, 8)));
}

#[test]
fn placing_a_window_that_has_never_committed_dirties_nothing() {
    // Placing a window before its first commit is what a manager does; it is not on screen, so
    // nothing is repainted.
    let mut s = WindowStack::new();
    let w = s.create(&CreateWindowRequest::new(64, 64, Role::Normal)).unwrap();
    let dirty = s.place(w, Point::new(100, 100)).unwrap();
    assert!(dirty.is_empty(), "an unmapped window is not on screen");
    assert_eq!(s.window(w).unwrap().origin, Point::new(100, 100), "but it did move");
}

#[test]
fn a_zero_sized_damage_is_the_identity_when_folded_into_a_wider_region() {
    let empty = Rect::new(100, 100, 0, 0);
    let real = Rect::new(4, 4, 10, 10);
    assert_eq!(union(empty, real), real);
    assert_eq!(union(real, empty), real);
    assert!(!empty.contains(100, 100), "a zero rect contains no pixel");
}

#[test]
fn placing_a_window_that_does_not_exist_is_refused() {
    let mut s = WindowStack::new();
    assert_eq!(s.place(99, Point::new(1, 1)), Err(StackError::NoSuchWindow));
}

#[test]
fn lower_sends_a_window_under_everything_and_raise_brings_it_back() {
    let mut s = WindowStack::new();
    let ids: Vec<u32> = (0..3).map(|_| s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap()).collect();
    assert_eq!(order(&s), ids, "creation order is bottom-first");

    assert!(!s.lower(ids[2]).unwrap().rect().is_empty());
    assert_eq!(order(&s), [ids[2], ids[0], ids[1]]);
    assert!(!s.raise(ids[2]).unwrap().rect().is_empty());
    assert_eq!(order(&s), [ids[0], ids[1], ids[2]]);
    assert_eq!(s.lower(99), Err(StackError::NoSuchWindow));
}

#[test]
fn raise_above_moves_one_window_and_leaves_the_rest_in_order() {
    let mut s = WindowStack::new();
    let ids: Vec<u32> = (0..4).map(|_| s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap()).collect();

    assert!(!s.raise_above(ids[0], ids[1]).unwrap().rect().is_empty());
    assert_eq!(order(&s), [ids[1], ids[0], ids[2], ids[3]]);

    assert!(!s.raise_above(ids[3], ids[1]).unwrap().rect().is_empty());
    assert_eq!(order(&s), [ids[1], ids[3], ids[0], ids[2]]);

    let before = order(&s);
    assert!(s.raise_above(ids[3], ids[3]).unwrap().rect().is_empty());
    assert_eq!(order(&s), before);
    assert_eq!(s.raise_above(ids[0], 99), Err(StackError::NoSuchWindow));
    assert_eq!(s.raise_above(99, ids[0]), Err(StackError::NoSuchWindow));
}

#[test]
fn compositing_draws_committed_windows_bottom_first() {
    let mut s = WindowStack::new();
    let mut src = MapSource::default();
    let red = Rgb::new(0xC0, 0x10, 0x10);
    let blue = Rgb::new(0x10, 0x10, 0xC0);

    let a = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));
    s.attach(&attach(a, 0, 8, 8)).unwrap();
    src.put(a, 0, geom(8, 8), red);
    s.commit(&commit(a, 0)).unwrap();

    let b = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));
    s.attach(&attach(b, 0, 8, 8)).unwrap();
    src.put(b, 0, geom(8, 8), blue);
    s.commit(&commit(b, 0)).unwrap();
    let _ = s.place(b, Point::new(4, 4)).unwrap();

    let mut fb = screen();
    let full = fb.geometry().bounds();
    s.compose_into(&mut fb, Rgb::BLACK, &src, &[full]);

    // Two samples are darkened by `b`'s shadow, so the claim is stacking: the lower window's
    // red, darker than raw. Sampled a pixel in from each corner, as rounded corners show what
    // is below.
    let lower = fb.get_pixel(2, 2).unwrap();
    assert!(lower.r > lower.g && lower.r > lower.b, "the lower window: {lower:?}");
    assert!(lower.r < red.r, "the lower window is darkened by the upper's shadow: {lower:?}");
    assert_eq!(fb.get_pixel(6, 6), Some(blue), "the upper window wins the overlap");
    assert_eq!(fb.get_pixel(9, 9), Some(blue));
    let elsewhere = fb.get_pixel(20, 2).unwrap();
    assert_ne!(elsewhere, blue, "background elsewhere, not the window");
    assert_ne!(elsewhere, red, "background elsewhere, not the window");
}

#[test]
fn a_window_with_no_committed_buffer_shows_background_not_garbage() {
    let mut s = WindowStack::new();
    let mut src = MapSource::default();
    // Configured, or the visibility gate skips the window before the `committed` guard under
    // test.
    let w = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));
    s.attach(&attach(w, 0, 8, 8)).unwrap();
    src.put(w, 0, geom(8, 8), Rgb::new(0xC0, 0x10, 0x10));
    let mut fb = screen();
    let full = fb.geometry().bounds();
    s.compose_into(&mut fb, Rgb::new(1, 2, 3), &src, &[full]);
    assert_eq!(fb.get_pixel(0, 0), Some(Rgb::new(1, 2, 3)), "uncommitted pixels leaked");
}

#[test]
fn destroying_an_ancestor_reaches_grandchildren() {
    // A submenu is a popup parented to a popup; one pass over direct children would leave it
    // alive.
    let mut s = WindowStack::new();
    let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
    let menu = s.create(&CreateWindowRequest::new(4, 4, Role::Popup { parent: w })).unwrap();
    let sub = s.create(&CreateWindowRequest::new(2, 2, Role::Popup { parent: menu })).unwrap();
    let dlg = s.create(&CreateWindowRequest::new(2, 2, Role::Dialog { parent: sub })).unwrap();
    let other = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));

    s.destroy(w).unwrap();
    for gone in [menu, sub, dlg] {
        assert!(s.window(gone).is_none(), "window {gone} outlived its ancestry");
    }
    assert!(s.window(other).is_some(), "unrelated windows are untouched");
    assert_eq!(s.focus_candidate(), Some(other));
}

#[test]
fn strut_accumulation_saturates_rather_than_wrapping() {
    // `Role`'s fields are public, so the library must not rely on the parser bounding
    // `reserve`: a wrap returns the full screen. Two reserves near `u32::MAX` actually
    // overflow.
    let mut s = WindowStack::new();
    for _ in 0..2 {
        // Exactly 2^31 each: the sum wraps to zero, the case that silently returned the full
        // screen.
        s.create(&CreateWindowRequest::new(1, 1, Role::Panel { dock: Edge::Top, reserve: 0x8000_0000 })).unwrap();
    }
    let wa = s.work_area(Rect::new(0, 0, 100, 50));
    assert_eq!(wa.size.h, 0, "the work area must clamp to empty, not wrap open");
    assert!(wa.is_empty());
}

#[test]
fn a_realistic_pair_of_bars_still_leaves_a_work_area() {
    let mut s = WindowStack::new();
    s.create(&CreateWindowRequest::new(100, 32, Role::Panel { dock: Edge::Top, reserve: 32 })).unwrap();
    s.create(&CreateWindowRequest::new(100, 28, Role::Panel { dock: Edge::Bottom, reserve: 28 })).unwrap();
    assert_eq!(s.work_area(Rect::new(0, 0, 200, 100)), Rect::new(0, 32, 200, 40));
}

#[test]
fn default_and_new_share_one_id_space() {
    let mut d = WindowStack::default();
    let mut n = WindowStack::new();
    let req = CreateWindowRequest::new(1, 1, Role::Normal);
    assert_eq!(d.create(&req).unwrap(), n.create(&req).unwrap());
}

// ---- shadows and corners ----

#[test]
fn every_floating_role_is_rounded_and_a_panel_is_not() {
    // Through `compose_into`, covering the wiring: a rounded window's corner pixel shows the
    // ground. All four roles, so popups and dialogs cannot silently stop being rounded.
    for (make_role, rounded) in [
        ((|_| Role::Normal) as fn(u32) -> Role, true),
        ((|p| Role::Popup { parent: p }) as fn(u32) -> Role, true),
        ((|p| Role::Dialog { parent: p }) as fn(u32) -> Role, true),
        ((|_| Role::Panel { dock: Edge::Top, reserve: 0 }) as fn(u32) -> Role, false),
    ] {
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        let parent = shown(&mut s, &CreateWindowRequest::new(2, 2, Role::Normal));
        let _ = s.place(parent, Point::new(80, 80));
        let role = make_role(parent);
        let id = shown(&mut s, &CreateWindowRequest::new(24, 20, role));
        s.attach(&attach(id, 0, 24, 20)).unwrap();
        let ink = Rgb::new(200, 30, 30);
        src.put(id, 0, geom(24, 20), ink);
        s.commit(&commit(id, 0)).unwrap();
        let _ = s.place(id, Point::new(8, 6));
        let ground = Rgb::new(120, 120, 120);
        let mut fb = big_screen();
        let full = fb.geometry().bounds();
        s.compose_into(&mut fb, ground, &src, &[full]);
        let origin = s.window(id).unwrap().origin;
        let corner = fb.get_pixel(origin.x as u32, origin.y as u32).unwrap();
        let middle = fb.get_pixel(origin.x as u32 + 12, origin.y as u32 + 10).unwrap();
        assert_eq!(middle, ink, "{role:?}: the middle is the window's own");
        assert_eq!(corner != ink, rounded, "{role:?}: its corner pixel is {corner:?}");
    }
}

#[test]
fn a_window_that_covers_the_screen_is_not_rounded() {
    // The overview's shape: a popup the size of the screen, over a panel. Its corners are the
    // screen's; rounding them showed the panel through the dimmed overlay.
    let mut s = WindowStack::new();
    let mut src = MapSource::default();
    let screen = big_screen().geometry().bounds();
    let bar = shown(&mut s, &CreateWindowRequest::new(screen.size.w, 10, Role::Panel { dock: Edge::Top, reserve: 10 }));
    s.attach(&attach(bar, 0, screen.size.w, 10)).unwrap();
    let bar_ink = Rgb::new(215, 214, 212);
    src.put(bar, 0, geom(screen.size.w, 10), bar_ink);
    s.commit(&commit(bar, 0)).unwrap();
    let cover = shown(&mut s, &CreateWindowRequest::new(screen.size.w, screen.size.h, Role::Popup { parent: bar }));
    s.attach(&attach(cover, 0, screen.size.w, screen.size.h)).unwrap();
    let cover_ink = Rgb::new(42, 42, 41);
    src.put(cover, 0, geom(screen.size.w, screen.size.h), cover_ink);
    s.commit(&commit(cover, 0)).unwrap();
    let mut fb = big_screen();
    s.compose_into(&mut fb, Rgb::BLACK, &src, &[screen]);
    assert_eq!(s.window(cover).unwrap().origin, Point::new(0, 0), "precondition: at the origin");
    for (x, y) in [(0, 0), (screen.size.w - 1, 0), (0, screen.size.h - 1)] {
        assert_eq!(fb.get_pixel(x, y), Some(cover_ink), "({x},{y}) shows what is beneath the cover");
    }
    assert_eq!(corner_of(Role::Popup { parent: bar }, screen, screen), 0);
    let short = Rect::new(0, 0, screen.size.w - 1, screen.size.h);
    assert_eq!(corner_of(Role::Popup { parent: bar }, short, screen), libdraw::corner::WINDOW_RADIUS);
}

#[test]
fn a_scheme_change_re_derives_every_windows_shadow_and_darkens_the_picture() {
    let mut s = WindowStack::new();
    let mut src = MapSource::default();
    let normal = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));
    let panel = shown(&mut s, &CreateWindowRequest::new(8, 2, Role::Panel { dock: Edge::Top, reserve: 0 }));
    s.attach(&attach(normal, 0, 8, 8)).unwrap();
    src.put(normal, 0, geom(8, 8), Rgb::new(200, 30, 30));
    s.commit(&commit(normal, 0)).unwrap();
    let _ = s.place(normal, Point::new(12, 4));

    let ground = Rgb::new(120, 120, 120);
    let beside = |s: &WindowStack| {
        let mut fb = screen();
        let full = fb.geometry().bounds();
        s.compose_into(&mut fb, ground, &src, &[full]);
        fb.get_pixel(10, 8).unwrap()
    };
    let light = beside(&s);
    let light_bounds = s.window(normal).unwrap().painted_bounds();

    assert!(s.set_scheme(Scheme::Dark), "a change reports itself");
    assert!(!s.set_scheme(Scheme::Dark), "and a repeat does not");
    for w in s.windows() {
        assert_eq!(w.shadow, shadow_for(w.role, Scheme::Dark), "window {}", w.id);
    }
    assert_eq!(s.window(panel).unwrap().shadow, None, "a panel still casts none");
    assert!(s.window(normal).unwrap().painted_bounds().size.w > light_bounds.size.w);
    let dark = beside(&s);
    assert!(dark.r < light.r, "beside the window: dark {dark:?}, light {light:?}");

    let later = s.create(&CreateWindowRequest::new(4, 4, Role::Normal)).unwrap();
    assert_eq!(s.window(later).unwrap().shadow, shadow_for(Role::Normal, Scheme::Dark));
}

#[test]
fn a_window_casts_a_shadow_and_a_panel_does_not() {
    // Which roles float: a bar or wallpaper with a shadow would show a dark band at the screen
    // edge.
    for (make_role, shadowed) in [
        ((|_| Role::Normal) as fn(u32) -> Role, true),
        ((|p| Role::Popup { parent: p }) as fn(u32) -> Role, true),
        ((|p| Role::Dialog { parent: p }) as fn(u32) -> Role, true),
        ((|_| Role::Panel { dock: Edge::Top, reserve: 0 }) as fn(u32) -> Role, false),
    ] {
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        let parent = shown(&mut s, &CreateWindowRequest::new(2, 2, Role::Normal));
        let _ = s.place(parent, Point::new(60, 40));
        let role = make_role(parent);
        let id = shown(&mut s, &CreateWindowRequest::new(8, 8, role));
        s.attach(&attach(id, 0, 8, 8)).unwrap();
        src.put(id, 0, geom(8, 8), Rgb::new(200, 30, 30));
        s.commit(&commit(id, 0)).unwrap();
        let _ = s.place(id, Point::new(12, 4));

        let ground = Rgb::new(120, 120, 120);
        let mut fb = screen();
        let full = fb.geometry().bounds();
        s.compose_into(&mut fb, ground, &src, &[full]);

        let beside = fb.get_pixel(10, 6).unwrap();
        assert_eq!(
            beside != ground,
            shadowed,
            "{role:?} should {} cast a shadow, got {beside:?} against {ground:?}",
            if shadowed { "" } else { "not" }
        );
        assert_eq!(fb.get_pixel(14, 6), Some(Rgb::new(200, 30, 30)), "{role:?}");
    }
}

#[test]
fn a_windows_damage_grows_by_its_shadow_and_its_reported_geometry_does_not() {
    // A shadow must be neither clickable nor told to a client: `info` and `painted_bounds`
    // differ.
    let mut s = WindowStack::new();
    let id = shown(&mut s, &CreateWindowRequest::new(40, 30, Role::Normal));
    s.attach(&attach(id, 0, 40, 30)).unwrap();
    s.commit(&commit(id, 0)).unwrap();
    let _ = s.place(id, Point::new(100, 60));

    let w = s.window(id).expect("in the stack");
    assert_eq!(w.bounds(), Rect::new(100, 60, 40, 30));
    assert_eq!(w.painted_bounds(), window_shadow(Scheme::Light).around(Rect::new(100, 60, 40, 30)));

    let info = s.info(id).expect("a window reports its geometry");
    assert_eq!((info.width, info.height), (40, 30), "a client is told its own size");
    assert_eq!((info.x, info.y), (100, 60), "and its own origin");
}

// ---- alpha ----

#[test]
fn a_window_attached_in_argb_is_composited_translucent() {
    let mut stack = WindowStack::new();
    let mut src = MapSource::default();
    let id = shown(&mut stack, &CreateWindowRequest::new(8, 8, Role::Normal));
    stack
        .attach(&AttachBufferRequest {
            window: id,
            buffer: 0,
            width: 8,
            height: 8,
            pitch: 32,
            format: SURFACE_FORMAT_ARGB8888,
        })
        .unwrap();
    stack.commit(&commit(id, 0)).unwrap();

    let ink = Rgb::new(200, 40, 40);
    let g = Geometry::with_pitch(8, 8, 32, PixelFormat::ARGB8888).unwrap();
    let word = g.format.encode_alpha(ink, 128).to_le_bytes();
    let mut px = vec![0u8; g.pitch * 8];
    for p in px.as_chunks_mut::<4>().0 {
        p.copy_from_slice(&word);
    }
    src.0.insert((id, 0), px);

    let bg = Rgb::new(9, 9, 9);
    let mut fb = screen();
    let full = fb.geometry().bounds();
    stack.compose_into(&mut fb, bg, &src, &[full]);

    assert_eq!(fb.get_pixel(4, 4), Some(ink.blend(bg, 128)));
    assert_ne!(fb.get_pixel(4, 4), Some(ink), "the tag was ignored and it drew opaque");
}

// ---- the shadow buffer ----

fn one_window(s: &mut WindowStack, src: &mut MapSource, w: u32, h: u32) -> u32 {
    let id = shown(s, &CreateWindowRequest::new(w, h, Role::Normal));
    s.attach(&attach(id, 0, w, h)).unwrap();
    s.commit(&commit(id, 0)).unwrap();
    src.put_striped(id, 0, Geometry::packed(w, h, PixelFormat::XRGB8888));
    id
}

#[test]
fn composing_through_the_shadow_draws_what_composing_directly_draws() {
    // The equivalence the change rests on: buffered and direct composition show the same
    // pixels.
    let mut stack = WindowStack::new();
    let mut src = MapSource::default();
    one_window(&mut stack, &mut src, 8, 6);

    let bg = Rgb::new(9, 9, 9);
    let full = screen().geometry().bounds();
    let pointer = Point::new(3, 2);

    let mut buffered = Screen::new(screen());
    assert!(buffered.is_buffered(), "a 32x16 shadow should fit");
    buffered.present(&stack, bg, &src, &[full], pointer, None);

    let mut direct = Screen::unbuffered(screen());
    direct.present(&stack, bg, &src, &[full], pointer, None);

    assert_eq!(buffered.display().bytes(), direct.display().bytes());
    assert_ne!(buffered.display().bytes(), screen().bytes(), "nothing was drawn");
}

#[test]
fn only_the_damage_is_copied_to_the_display() {
    // Asserted as a byte count because the picture cannot tell: a copy of the whole shadow
    // draws the identical image.
    let mut shadow = MemFramebuffer::new(screen().geometry());
    shadow.clear(Rgb::new(7, 7, 7));
    let mut display = screen();

    let corner = Rect::new(0, 0, 4, 4);
    assert_eq!(copy_damage(&mut display, &shadow, &[corner]), 4 * 4 * 4);
    assert_eq!(display.get_pixel(0, 0), Some(Rgb::new(7, 7, 7)));
    assert_eq!(display.get_pixel(5, 0), Some(Rgb::new(0, 0, 0)));

    let over = Rect::new(30, 14, 40, 40);
    assert_eq!(copy_damage(&mut display, &shadow, &[over]), 2 * 2 * 4);
    assert_eq!(copy_damage(&mut display, &shadow, &[Rect::new(90, 90, 4, 4)]), 0);
}

#[test]
fn a_later_small_frame_leaves_the_rest_of_the_display_alone() {
    // Checks placement end to end: a small-damage `present` puts the right pixels at the right
    // coordinates. It does not bound the work (the byte-count test does), and the shadow is a
    // staging buffer, not a cache, so its contents need not persist between presents.
    let mut stack = WindowStack::new();
    let mut src = MapSource::default();
    one_window(&mut stack, &mut src, 8, 6);

    let bg = Rgb::new(9, 9, 9);
    let mut sc = Screen::new(screen());
    let full = screen().geometry().bounds();
    sc.present(&stack, bg, &src, &[full], Point::new(20, 12), None);
    let after_full = sc.display().bytes().to_vec();

    sc.present(&stack, bg, &src, &[Rect::new(0, 0, 2, 2)], Point::new(20, 12), None);
    assert_eq!(sc.display().bytes(), &after_full[..], "the small frame disturbed the screen");
    assert_ne!(after_full, screen().bytes(), "the first frame drew nothing");
}

#[test]
fn a_screen_with_no_shadow_still_paints() {
    let mut stack = WindowStack::new();
    let mut src = MapSource::default();
    one_window(&mut stack, &mut src, 8, 6);

    let mut sc = Screen::unbuffered(screen());
    assert!(!sc.is_buffered());
    let full = screen().geometry().bounds();
    sc.present(&stack, Rgb::new(9, 9, 9), &src, &[full], Point::new(20, 12), None);
    assert_eq!(sc.display().get_pixel(4, 3), Some(row_colour(3)));
}

#[test]
fn a_buffer_the_source_cannot_resolve_is_skipped_not_drawn() {
    let mut s = WindowStack::new();
    let src = MapSource::default(); // deliberately empty
    let w = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));
    s.attach(&attach(w, 0, 8, 8)).unwrap();
    s.commit(&commit(w, 0)).unwrap();

    let mut fb = screen();
    let full = fb.geometry().bounds();
    s.compose_into(&mut fb, Rgb::new(9, 9, 9), &src, &[full]);
    assert_eq!(fb.get_pixel(0, 0), Some(Rgb::new(9, 9, 9)));
}

#[test]
fn a_short_buffer_is_skipped_rather_than_read_past_its_end() {
    struct Short;
    impl BufferSource for Short {
        fn pixels(&self, _w: u32, _b: u32) -> Option<libdraw::pixels::Pixels<'_>> {
            Some(libdraw::pixels::Pixels::Own(&[0u8; 4])) // far shorter than an 8x8 buffer
        }
    }
    let mut s = WindowStack::new();
    let w = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));
    s.attach(&attach(w, 0, 8, 8)).unwrap();
    s.commit(&commit(w, 0)).unwrap();

    let mut fb = screen();
    let full = fb.geometry().bounds();
    s.compose_into(&mut fb, Rgb::new(4, 5, 6), &Short, &[full]);
    assert_eq!(fb.get_pixel(0, 0), Some(Rgb::new(4, 5, 6)));
}

#[test]
fn raising_a_window_changes_what_wins_the_overlap() {
    let mut s = WindowStack::new();
    let mut src = MapSource::default();
    let red = Rgb::new(0xC0, 0x10, 0x10);
    let blue = Rgb::new(0x10, 0x10, 0xC0);
    for (id, colour) in [(1u32, red), (2u32, blue)] {
        let w = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));
        assert_eq!(w, id);
        s.attach(&attach(w, 0, 8, 8)).unwrap();
        src.put(w, 0, geom(8, 8), colour);
        s.commit(&commit(w, 0)).unwrap();
    }
    let full = screen().geometry().bounds();

    let mut fb = screen();
    s.compose_into(&mut fb, Rgb::BLACK, &src, &[full]);
    assert_eq!(fb.get_pixel(4, 4), Some(blue), "window 2 is on top");

    assert!(!s.raise(1).unwrap().rect().is_empty());
    let mut fb = screen();
    s.compose_into(&mut fb, Rgb::BLACK, &src, &[full]);
    assert_eq!(fb.get_pixel(4, 4), Some(red), "window 1 now is");
}

#[test]
fn a_modifier_press_neither_repeats_nor_hijacks_a_repeat_in_flight() {
    // Modifiers arrive as ordinary key transitions: arming on them made holding Ctrl repeat,
    // and pressing Shift mid-run replace the run.
    let now = 1_000;

    assert_eq!(Repeat::after_key(None, KEY_LEFTCTRL, true, 0, 0, Some(7), now), None, "a modifier armed a repeat");

    let run = Repeat::armed(KEY_A, 0, 0, 7, now);
    let after_shift_down = Repeat::after_key(Some(run), KEY_LEFTSHIFT, true, 0, 0, Some(7), now + 50);
    assert_eq!(after_shift_down, Some(run), "a modifier press hijacked the repeat");

    let after_shift_up = Repeat::after_key(after_shift_down, KEY_LEFTSHIFT, false, 0, 0, Some(7), now + 90);
    assert_eq!(after_shift_up, Some(run), "a modifier release stopped the repeat");

    assert_eq!(
        Repeat::after_key(after_shift_up, KEY_A, false, 0, 0, Some(7), now + 120),
        None,
        "the repeating key's release must disarm"
    );
}

#[test]
fn every_modifier_is_excluded_both_sides() {
    for code in [
        KEY_LEFTSHIFT,
        KEY_RIGHTSHIFT,
        KEY_LEFTCTRL,
        KEY_RIGHTCTRL,
        KEY_LEFTALT,
        KEY_RIGHTALT,
        KEY_LEFTMETA,
        KEY_RIGHTMETA,
    ] {
        assert_eq!(Repeat::after_key(None, code, true, 0, 0, Some(1), 0), None, "keycode {code} armed a repeat");
    }
    assert!(Repeat::after_key(None, KEY_A, true, 0, 0, Some(1), 0).is_some(), "an ordinary key stopped repeating");
}

#[test]
fn a_lock_key_neither_repeats_nor_takes_a_run() {
    for lock in [KEY_CAPSLOCK, KEY_NUMLOCK] {
        assert_eq!(Repeat::after_key(None, lock, true, 0, 0, Some(7), 0), None, "{lock} armed a repeat");
        let run = Repeat::armed(KEY_A, 0, 0, 7, 0);
        assert_eq!(Repeat::after_key(Some(run), lock, true, 0, 0, Some(7), 50), Some(run), "{lock} took the run");
        assert_eq!(Repeat::after_key(Some(run), lock, false, 0, 0, Some(7), 90), Some(run), "{lock}'s release");
    }
}

#[test]
fn a_repeat_carries_the_locks_it_went_down_with() {
    use librsproto::surface::{KEY_REPEAT, LOCK_CAPS, LOCK_NUM, MOD_SHIFT};
    let run = Repeat::after_key(None, KEY_A, true, MOD_SHIFT, LOCK_CAPS | LOCK_NUM, Some(7), 0).expect("armed");
    assert_eq!(run.locks, LOCK_CAPS | LOCK_NUM, "armed with the press's locks");
    let e = run.event();
    assert_eq!((e.window, e.keycode, e.pressed), (7, KEY_A, KEY_REPEAT));
    assert_eq!((e.modifiers, e.locks), (MOD_SHIFT, LOCK_CAPS | LOCK_NUM), "the press's, sent with each repeat");
    let after = Repeat::after_key(Some(run), KEY_CAPSLOCK, true, MOD_SHIFT, LOCK_NUM, Some(7), 50);
    assert_eq!(after.map(|r| r.event().locks), Some(LOCK_CAPS | LOCK_NUM));
}

#[test]
fn a_press_with_no_focus_candidate_disarms_rather_than_leaving_a_stale_run() {
    let run = Repeat::armed(KEY_A, 0, 0, 7, 0);
    assert_eq!(Repeat::after_key(Some(run), KEY_B, true, 0, 0, None, 100), None);
}

#[test]
fn presenting_leaves_the_pointer_on_screen_over_a_window() {
    // The cursor is drawn over the composed stack, so every path that recomposes must redraw
    // it; the binary's screen-update path cannot be host-tested, so pairing them here makes it
    // testable.
    let mut s = WindowStack::new();
    let mut src = MapSource::default();
    let w = shown(&mut s, &CreateWindowRequest::new(64, 64, Role::Normal));
    s.attach(&attach(w, 0, 64, 64)).unwrap();
    let window_colour = Rgb::new(0x10, 0x80, 0x20);
    src.put(w, 0, geom(64, 64), window_colour);
    s.commit(&commit(w, 0)).unwrap();

    let pointer = Point::new(20, 20);
    let mut fb = big_screen();
    let full = fb.geometry().bounds();
    s.present_into(&mut fb, Rgb::BLACK, &src, &[full], pointer, None);

    assert!(body_pixels(&fb, pointer) > 0, "the pointer is not on screen over the window it is above");
    assert_eq!(fb.get_pixel(10, 10), Some(window_colour), "the stack did not compose");
}

fn body_pixels(fb: &MemFramebuffer, at: Point) -> usize {
    let r = cursor_rect(at);
    let mut n = 0;
    for y in r.origin.y..r.bottom() as i32 {
        for x in r.origin.x..r.right() as i32 {
            if fb.get_pixel(x as u32, y as u32) == Some(CURSOR_BODY) {
                n += 1;
            }
        }
    }
    n
}

#[test]
fn presenting_draws_the_pointer_into_every_damage_rectangle() {
    // `serve_input` passes the cursor's old and new rectangles in one damage list; drawing into
    // only the first would erase the pointer at its destination.
    let s = WindowStack::new();
    let src = MapSource::default();
    let pointer = Point::new(40, 40);
    let mut fb = big_screen();
    let elsewhere = Rect::new(0, 0, 8, 8);
    s.present_into(&mut fb, Rgb::BLACK, &src, &[elsewhere, cursor_rect(pointer)], pointer, None);
    assert!(body_pixels(&fb, pointer) > 0, "the pointer was not drawn into the second damage rectangle");
}

#[test]
fn the_guest_configuration_composites_a_client_surface() {
    // The shape `ui-testclient` produces in the guest: a 1280x800 screen at pitch 5120, a 64x32
    // surface at pitch 268 (not 64*4): a stride computed from the width skews every row after
    // the first. Keep in step with `libdraw::scene::SCREEN_PITCH`.
    let mut fb = MemFramebuffer::new(Geometry::with_pitch(1280, 800, 5120, PixelFormat::XRGB8888).unwrap());
    let mut s = WindowStack::new();
    let mut src = MapSource::default();
    let w = shown(&mut s, &CreateWindowRequest::new(64, 32, Role::Normal));
    s.attach(&AttachBufferRequest {
        window: w,
        buffer: 0,
        width: 64,
        height: 32,
        pitch: 268,
        format: SURFACE_FORMAT_XRGB8888,
    })
    .unwrap();
    let g = Geometry::with_pitch(64, 32, 268, PixelFormat::XRGB8888).unwrap();
    src.put_striped(w, 0, g);
    s.commit(&commit(w, 0)).unwrap();

    let bounds = fb.geometry().bounds();
    s.compose_into(&mut fb, Rgb::new(0x0E, 0x14, 0x1B), &src, &[bounds]);

    assert_eq!(fb.get_pixel(6, 5), Some(row_colour(5)), "the client's surface must reach the screen, row for row");
    // The first and last pixels are outside the rounded corners, so the ends of the copy are
    // checked where the surface is still square.
    assert_eq!(fb.get_pixel(55, 31), Some(row_colour(31)), "the last row, to its curve");
    assert_eq!(fb.get_pixel(63, 23), Some(row_colour(23)), "the last column, to its curve");
    assert_eq!(fb.get_pixel(8, 0), Some(row_colour(0)), "and its first row");
    assert_eq!(fb.get_pixel(0, 8), Some(row_colour(8)), "and its first column");
    // Past the right edge the surface stops but its shadow does not: not the surface's colour,
    // and no brighter than the ground.
    let past = fb.get_pixel(64, 0).unwrap();
    let ground = Rgb::new(0x0E, 0x14, 0x1B);
    assert_ne!(past, row_colour(0), "the surface must stop at its edge");
    assert!(
        past.r <= ground.r && past.g <= ground.g && past.b <= ground.b,
        "past the edge is ground, shadowed at most: {past:?}"
    );
}

#[test]
fn info_reports_the_committed_size_not_the_requested_one() {
    let mut s = WindowStack::new();
    let mut src = MapSource::default();
    let w = s.create(&CreateWindowRequest::new(100, 50, Role::Normal)).unwrap();
    let info = s.info(w).unwrap();
    assert_eq!((info.width, info.height), (100, 50), "requested size before any commit");

    s.attach(&attach(w, 0, 8, 8)).unwrap();
    src.put(w, 0, geom(8, 8), Rgb::BLACK);
    s.commit(&commit(w, 0)).unwrap();
    let _ = s.place(w, Point::new(-3, 12)).unwrap();

    let info = s.info(w).unwrap();
    assert_eq!((info.width, info.height), (8, 8), "committed size once there is one");
    assert_eq!((info.x, info.y), (-3, 12));
    assert_eq!(info.id, w);
}

/// A geometry change reports the rectangle that is on screen, for a commit as well as a place.
#[test]
fn a_geometry_change_reports_committed_bounds_and_fires_for_a_commit_too() {
    let mut s = WindowStack::new();
    let mut src = MapSource::default();
    let w = s.create(&CreateWindowRequest::new(100, 50, Role::Normal)).unwrap();
    assert!(s.take_geometry_changes().is_empty(), "create is not a geometry change");

    s.attach(&attach(w, 0, 8, 8)).unwrap();
    src.put(w, 0, geom(8, 8), Rgb::BLACK);
    s.commit(&commit(w, 0)).unwrap();
    assert_eq!(s.take_geometry_changes(), vec![w], "a commit that resizes is reported");

    let _ = s.place(w, Point::new(-3, 12)).unwrap();
    assert_eq!(s.take_geometry_changes(), vec![w], "and so is a place");

    let b = s.window(w).unwrap().bounds();
    let info = s.info(w).unwrap();
    assert_eq!(
        (b.origin.x, b.origin.y, b.size.w, b.size.h),
        (info.x, info.y, info.width, info.height),
        "the event and /dev/draw/<id>/info must describe one window the same way"
    );
    assert_eq!((b.size.w, b.size.h), (8, 8), "committed, not the 100x50 requested");

    s.commit(&commit(w, 0)).unwrap();
    assert!(s.take_geometry_changes().is_empty(), "a commit that does not resize is silent");
}

#[test]
fn geometry_changes_are_deduplicated() {
    let mut s = WindowStack::new();
    let mut src = MapSource::default();
    let w = s.create(&CreateWindowRequest::new(100, 50, Role::Normal)).unwrap();
    s.attach(&attach(w, 0, 8, 8)).unwrap();
    src.put(w, 0, geom(8, 8), Rgb::BLACK);
    s.commit(&commit(w, 0)).unwrap();
    let _ = s.place(w, Point::new(5, 5)).unwrap();
    assert_eq!(s.take_geometry_changes(), vec![w], "one id, however many times it moved");
}

/// A window that has committed but has not been configured stays off the screen.
#[test]
fn an_unconfigured_window_is_not_composited_however_much_it_commits() {
    let screen = Geometry::packed(16, 16, PixelFormat::XRGB8888);
    let mut fb = MemFramebuffer::new(screen);
    let mut s = WindowStack::new();
    let mut src = MapSource::default();

    let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
    s.attach(&attach(w, 0, 8, 8)).unwrap();
    src.put(w, 0, geom(8, 8), Rgb::new(0xFF, 0xFF, 0xFF));
    s.commit(&commit(w, 0)).unwrap();

    s.compose_into(&mut fb, Rgb::new(0, 0, 0), &src, &[screen.bounds()]);
    assert_eq!(
        fb.get_pixel(4, 4),
        Some(Rgb::new(0, 0, 0)),
        "committed, but never configured: the client jumped the handshake and is not on screen"
    );

    assert!(s.mark_configured(w), "this is the transition");
    s.compose_into(&mut fb, Rgb::new(0, 0, 0), &src, &[screen.bounds()]);
    assert_eq!(fb.get_pixel(4, 4), Some(Rgb::new(0xFF, 0xFF, 0xFF)), "configured: now it composites");

    assert!(!s.mark_configured(w), "marking twice is not a second transition");
}

fn drawable(s: &mut WindowStack, src: &mut MapSource) -> u32 {
    let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
    s.attach(&attach(w, 0, 8, 8)).unwrap();
    src.put(w, 0, geom(8, 8), Rgb::new(0xFF, 0xFF, 0xFF));
    s.commit(&commit(w, 0)).unwrap();
    s.mark_configured(w);
    w
}

#[test]
fn a_window_on_another_desktop_is_not_composited_and_comes_back_when_you_switch_to_it() {
    let screen = Geometry::packed(16, 16, PixelFormat::XRGB8888);
    let mut fb = MemFramebuffer::new(screen);
    let mut s = WindowStack::new();
    let mut src = MapSource::default();
    drawable(&mut s, &mut src);
    let black = Rgb::new(0, 0, 0);
    let white = Rgb::new(0xFF, 0xFF, 0xFF);

    s.compose_into(&mut fb, black, &src, &[screen.bounds()]);
    assert_eq!(fb.get_pixel(4, 4), Some(white), "precondition: on the current desktop");

    assert!(s.set_current_desktop(2).unwrap(), "the switch changed something");
    s.compose_into(&mut fb, black, &src, &[screen.bounds()]);
    assert_eq!(fb.get_pixel(4, 4), Some(black), "desktop 2 does not show desktop 1's window");

    s.set_current_desktop(1).unwrap();
    s.compose_into(&mut fb, black, &src, &[screen.bounds()]);
    assert_eq!(fb.get_pixel(4, 4), Some(white), "and back again, with no further commit");
}

#[test]
fn a_sticky_window_composites_on_every_desktop() {
    let screen = Geometry::packed(16, 16, PixelFormat::XRGB8888);
    let mut fb = MemFramebuffer::new(screen);
    let mut s = WindowStack::new();
    let mut src = MapSource::default();
    let w = drawable(&mut s, &mut src);
    s.set_window_desktop(w, STICKY_DESKTOP).unwrap();

    for d in [1u32, 2, 99] {
        s.set_current_desktop(d).unwrap();
        s.compose_into(&mut fb, Rgb::new(0, 0, 0), &src, &[screen.bounds()]);
        assert_eq!(fb.get_pixel(4, 4), Some(Rgb::new(0xFF, 0xFF, 0xFF)), "a sticky window is on desktop {d} too");
    }
}

#[test]
fn a_minimized_window_is_not_composited_though_it_stays_on_its_desktop() {
    let screen = Geometry::packed(16, 16, PixelFormat::XRGB8888);
    let mut fb = MemFramebuffer::new(screen);
    let mut s = WindowStack::new();
    let mut src = MapSource::default();
    let w = drawable(&mut s, &mut src);

    assert!(s.set_minimized(w, true).unwrap(), "visibility changed");
    s.compose_into(&mut fb, Rgb::new(0, 0, 0), &src, &[screen.bounds()]);
    assert_eq!(fb.get_pixel(4, 4), Some(Rgb::new(0, 0, 0)), "minimized: off screen");

    assert_eq!(s.window(w).unwrap().desktop, 1, "minimizing did not move it");
    assert!(s.set_minimized(w, false).unwrap());
    s.compose_into(&mut fb, Rgb::new(0, 0, 0), &src, &[screen.bounds()]);
    assert_eq!(fb.get_pixel(4, 4), Some(Rgb::new(0xFF, 0xFF, 0xFF)), "restored");
}

#[test]
fn focus_does_not_land_on_a_window_that_is_not_on_screen() {
    // Focus follows the stack, so a hidden window must stop being a candidate or the keyboard
    // points at something invisible.
    let mut s = WindowStack::new();
    let lower = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
    s.mark_configured(lower);
    let upper = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
    s.mark_configured(upper);
    assert_eq!(s.focus_candidate(), Some(upper));

    s.set_minimized(upper, true).unwrap();
    assert_eq!(s.focus_candidate(), Some(lower), "minimized: not a candidate");

    s.set_minimized(upper, false).unwrap();
    s.set_window_desktop(upper, 2).unwrap();
    assert_eq!(s.focus_candidate(), Some(lower), "on another desktop: not a candidate");

    s.set_window_desktop(lower, 2).unwrap();
    assert_eq!(s.focus_candidate(), None, "nothing on this desktop takes focus");
}

#[test]
fn a_window_is_created_onto_the_current_desktop() {
    let mut s = WindowStack::new();
    let first = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
    assert_eq!(s.window(first).unwrap().desktop, 1);

    s.set_current_desktop(4).unwrap();
    let second = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
    assert_eq!(s.window(second).unwrap().desktop, 4, "created onto the current desktop");
    assert_eq!(s.window(first).unwrap().desktop, 1, "and the earlier one did not move");
}

#[test]
fn the_sticky_value_is_refused_as_a_current_desktop() {
    // A current desktop of 0 would composite only sticky windows and make every later window
    // sticky; `desktop switch N` takes N off a command line.
    let mut s = WindowStack::new();
    assert_eq!(s.set_current_desktop(STICKY_DESKTOP), Err(StackError::StickyIsNotADesktop));
    assert_eq!(s.current_desktop(), 1, "and the current desktop is unchanged");

    let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
    assert!(s.set_window_desktop(w, STICKY_DESKTOP).is_ok());
}

#[test]
fn moving_a_window_between_two_hidden_desktops_reports_no_visible_change() {
    let mut s = WindowStack::new();
    let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
    s.mark_configured(w);

    assert!(s.set_window_desktop(w, 2).unwrap(), "leaving the current desktop is visible");
    assert!(!s.set_window_desktop(w, 3).unwrap(), "2 -> 3 changes nothing on screen");
    assert!(!s.set_minimized(w, true).unwrap(), "already hidden");
    assert!(!s.set_window_desktop(w, 1).unwrap(), "still minimized: no change");
    assert!(s.set_minimized(w, false).unwrap(), "back on desktop 1 and un-minimized");
}

#[test]
fn a_popup_is_created_at_its_offset_from_the_parent() {
    let mut s = WindowStack::new();
    let parent = shown(&mut s, &CreateWindowRequest::new(200, 100, Role::Normal));
    let _ = s.place(parent, Point::new(30, 40)).unwrap();

    let menu = s.create(&CreateWindowRequest::at(60, 80, Role::Popup { parent }, 5, 24)).unwrap();
    assert_eq!(
        s.window(menu).unwrap().origin,
        Point::new(35, 64),
        "the parent's origin plus the offset, not the offset alone and not the origin"
    );

    let _ = s.place(parent, Point::new(100, 100)).unwrap();
    assert_eq!(
        s.window(menu).unwrap().origin,
        Point::new(35, 64),
        "resolved at creation: the popup does not track the parent"
    );
}

/// Only a `popup` is offset from its parent; the others ask the screen, and a panel does not
/// ask.
#[test]
fn a_popup_is_offset_from_its_parent_and_the_rest_from_the_screen() {
    let mut s = WindowStack::new();
    let parent = shown(&mut s, &CreateWindowRequest::new(50, 50, Role::Normal));
    let _ = s.place(parent, Point::new(60, 70)).unwrap();

    let plain = s.create(&CreateWindowRequest::at(10, 10, Role::Normal, 77, 88)).unwrap();
    assert_eq!(s.window(plain).unwrap().origin, Point::new(77, 88), "the request was dropped");

    let dlg = s.create(&CreateWindowRequest::at(10, 10, Role::Dialog { parent }, 77, 88)).unwrap();
    assert_eq!(
        s.window(dlg).unwrap().origin,
        Point::new(77, 88),
        "a dialog was offset from its parent, which is the popup rule leaking"
    );

    let bar = s.create(&CreateWindowRequest::at(10, 10, Role::Panel { dock: Edge::Top, reserve: 10 }, 77, 88)).unwrap();
    assert_eq!(s.window(bar).unwrap().origin, Point::new(0, 0), "a panel took an offset");

    let plain0 = s.create(&CreateWindowRequest::new(10, 10, Role::Normal)).unwrap();
    assert_eq!(s.window(plain0).unwrap().origin, Point::new(0, 0));

    let menu = s.create(&CreateWindowRequest::at(10, 10, Role::Popup { parent }, 5, 6)).unwrap();
    assert_eq!(s.window(menu).unwrap().origin, Point::new(65, 76), "the popup is");
}

/// A popup is not clipped to its parent — the whole reason popups are windows.
#[test]
fn a_popup_is_drawn_outside_its_parents_bounds() {
    let screen = Geometry::packed(64, 64, PixelFormat::XRGB8888);
    let mut fb = MemFramebuffer::new(screen);
    let mut s = WindowStack::new();
    let mut src = MapSource::default();

    let parent = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));
    let _ = s.place(parent, Point::new(28, 28)).unwrap();
    s.attach(&attach(parent, 0, 8, 8)).unwrap();
    src.put(parent, 0, geom(8, 8), Rgb::new(0x20, 0x20, 0x20));
    s.commit(&commit(parent, 0)).unwrap();

    let menu = s.create(&CreateWindowRequest::at(32, 32, Role::Popup { parent }, -12, -12)).unwrap();
    s.mark_configured(menu);
    s.attach(&attach(menu, 0, 32, 32)).unwrap();
    src.put(menu, 0, geom(32, 32), Rgb::new(0xFF, 0xFF, 0xFF));
    s.commit(&commit(menu, 0)).unwrap();

    s.compose_into(&mut fb, Rgb::new(0, 0, 0), &src, &[screen.bounds()]);
    let white = Some(Rgb::new(0xFF, 0xFF, 0xFF));
    assert_eq!(fb.get_pixel(20, 32), white, "left of the parent");
    assert_eq!(fb.get_pixel(44, 32), white, "right of it");
    assert_eq!(fb.get_pixel(32, 20), white, "above it");
    assert_eq!(fb.get_pixel(32, 44), white, "below it");
}

/// A popup crossing a screen edge is clipped to the screen, including at a negative origin.
#[test]
fn a_popup_crossing_a_screen_edge_is_clipped_not_wrapped() {
    let screen = Geometry::packed(32, 32, PixelFormat::XRGB8888);
    let mut fb = MemFramebuffer::new(screen);
    let mut s = WindowStack::new();
    let mut src = MapSource::default();

    let parent = shown(&mut s, &CreateWindowRequest::new(4, 4, Role::Normal));
    let menu = s.create(&CreateWindowRequest::at(32, 32, Role::Popup { parent }, -8, -8)).unwrap();
    assert_eq!(s.window(menu).unwrap().origin, Point::new(-8, -8));
    s.mark_configured(menu);
    s.attach(&attach(menu, 0, 32, 32)).unwrap();
    src.put_striped(menu, 0, geom(32, 32));
    s.commit(&commit(menu, 0)).unwrap();

    s.compose_into(&mut fb, Rgb::new(0, 0, 0), &src, &[screen.bounds()]);

    assert_eq!(
        fb.get_pixel(0, 0),
        Some(row_colour(8)),
        "screen (0,0) shows the popup's row 8 — the clip moved the source, not just the size"
    );
    assert_eq!(fb.get_pixel(0, 23), Some(row_colour(31)), "and the last row it has");
    assert_eq!(fb.get_pixel(25, 25), Some(Rgb::new(0, 0, 0)), "nothing beyond it");
}

#[test]
fn info_publishes_the_desktop_and_the_minimized_bit() {
    let mut s = WindowStack::new();
    let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
    assert_eq!(s.info(w).unwrap().desktop, 1, "created onto the current desktop");
    assert_eq!(s.info(w).unwrap().flags & WINDOW_FLAG_MINIMIZED, 0);

    s.set_window_desktop(w, 4).unwrap();
    s.set_minimized(w, true).unwrap();
    let info = s.info(w).unwrap();
    assert_eq!(info.desktop, 4, "the attribute a manager set is what `info` publishes");
    assert_ne!(info.flags & WINDOW_FLAG_MINIMIZED, 0, "and the minimized bit is set");

    s.set_minimized(w, false).unwrap();
    assert_eq!(s.info(w).unwrap().flags & WINDOW_FLAG_MINIMIZED, 0, "and cleared again");
}

#[test]
fn info_carries_the_role_and_its_extra_fields() {
    let mut s = WindowStack::new();
    let p = s.create(&CreateWindowRequest::new(100, 24, Role::Panel { dock: Edge::Bottom, reserve: 24 })).unwrap();
    let info = s.info(p).unwrap();
    assert_eq!(info.role, librsproto::surface::ROLE_PANEL);
    assert_eq!(info.reserve, 24);

    let child = s.create(&CreateWindowRequest::new(4, 4, Role::Popup { parent: p })).unwrap();
    assert_eq!(s.info(child).unwrap().parent, p);
    assert!(s.info(9999).is_none(), "a window that does not exist has no info");
}

#[test]
fn a_bad_pitch_is_refused_at_attach() {
    let mut s = WindowStack::new();
    let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
    let mut req = attach(w, 0, 8, 8);
    req.pitch = 8 * 4 - 1; // cannot hold a row
    assert_eq!(s.attach(&req), Err(StackError::BadGeometry));
}

/// `take_removed` reports every window that went, parent first, and then drains.
#[test]
fn take_removed_reports_the_whole_subtree_parent_first_then_drains() {
    let mut s = WindowStack::new();
    let root = s.create(&CreateWindowRequest::new(100, 80, Role::Normal)).unwrap();
    let menu = s.create(&CreateWindowRequest::new(40, 20, Role::Popup { parent: root })).unwrap();
    let submenu = s.create(&CreateWindowRequest::new(30, 15, Role::Popup { parent: menu })).unwrap();
    let other = s.create(&CreateWindowRequest::new(10, 10, Role::Normal)).unwrap();

    assert!(s.take_removed().is_empty(), "nothing has been destroyed yet");

    s.destroy(root).unwrap();
    let gone = s.take_removed();

    assert_eq!(gone[0], root, "the window asked for is reported first");
    assert!(gone.contains(&menu), "a popup goes with its parent");
    assert!(gone.contains(&submenu), "and so does a popup of that popup");
    assert!(!gone.contains(&other), "an unrelated window is not reported");
    assert_eq!(gone.len(), 3, "exactly the subtree, no duplicates");
    assert!(s.window(other).is_some(), "the unrelated window survives");
    assert!(s.take_removed().is_empty(), "the log drained: a second read must not re-announce the same subtree");
}
